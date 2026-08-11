# Rust build output: per-worktree disk footprint

## Why

Every agent task gets its own git worktree, and cargo builds **per worktree**.
With 33 crates and 1139 resolved dependencies, a full build materialises ~5G of
`target/` that belongs to exactly one task and is thrown away when that task
ends. Run four Rust tasks at once and that is ~20G of transient build output.

On ded02 (2026-08-11) this is the single largest consumer of the host disk, and
disk — not RAM, not CPU — is the binding constraint on that box: `/` sat at 91%
on 2026-08-07 and the memory-pressure incident that started this investigation
was survivable only because there was still disk to swap onto.

The point of this plan is **not** that the build output is garbage. It is live
work, it frees itself when the task finishes, and the box recovered on its own
(91% → 86% over four days). The point is that the cost is (a) larger than it
needs to be, (b) invisible to the host's automated cleanup, and (c) scales
linearly with agent concurrency, which is the thing we keep increasing.

## Today (verified 2026-08-11, ded02 / team-miguel)

### Where the space is

```
5.02G  /var/tmp/vk-deploy-build/target          (100% release, 0 debug)
4.83G  /var/tmp/chief-deploy/main-deploy/target
3.06G  /var/tmp/chief-worktrees/bc-seamless-p2/target
────
12.9G  total across 3 live build dirs
```

Breakdown of the largest, `vk-deploy-build/target/release` (5.02G):

```
4.08G  deps/          854 .rlib (2.79G) + 854 .rmeta (1.00G) + 59 .so (0.15G)
0.38G  build/         build-script output
0.02G  .fingerprint/
0.00G  incremental/   (release does not use incremental by default)
```

So ~81% of a worktree's build output is `deps/` — compiled dependencies, which
are **identical across worktrees** for any two tasks on the same lockfile.

### What is already optimised — do not "fix" these again

- **`[profile.release]` is already tuned**: `debug = 1`,
  `split-debuginfo = "packed"`, `strip = true`. The final binary is stripped and
  debug info is split out. Any plan that opens with "add `strip = true`" has not
  read the manifest.
- **The registry cache is already shared.** `~/.cargo/registry` is 1.70G and
  lives in the bind-mounted home, so it is downloaded once and reused by every
  worktree. Source packages are not duplicated per worktree — only *compiled
  output* is.

### The gap: the host sweep does not cover this

The devpod host runs `safe-cleanup.sh`, which sweeps cold cargo output from
worktree roots. Its safety contract says, verbatim:

> Sweeps Rust `target/debug` build output inside worktree roots, but ONLY when
> the worktree is BOTH idle and cold. Source, git metadata, and `target/release`
> are never touched.

and it globs `-path '*/target/debug'`.

**100% of the space measured above is in `target/release`.** There is no
`target/debug` on this box at all. The automated cleanup therefore reclaims
nothing from Rust builds — which is exactly what was observed: a forced
`safe-cleanup --if-above 0` run on 2026-08-06 recovered 3G against a disk at
93%, and logged no cargo sweeps at all.

This is the highest-value fix and it lives in the **devpod** repo, not this one.

## Options

Ordered by value. Each says what it actually saves, because two of them save
something other than what you would guess.

### 1. Extend the host sweep to `target/release` (devpod repo)

**Saves:** disk, retroactively, with no build-side change. Directly addresses
the 12.9G measured above once those worktrees go cold.

**Costs:** a cold worktree's next build is a full rebuild. That is already the
accepted trade for `target/debug`; nothing about `release` makes it different,
and the existing idle+cold gates (`TARGET_KEEP_DAYS=3`, no live process
executing from the worktree) apply unchanged.

**Why it was excluded originally** is worth checking before changing it — the
comment reads like a deliberate safety decision (a release binary is more likely
to be something you are *running*). The `live_exe` gate already covers that
case, which is probably enough, but confirm rather than assume.

### 2. `sccache` — shared compilation cache

**Saves: build time, NOT disk.** This is worth stating plainly because it is the
natural assumption and it is wrong. sccache stores compiled artifacts in a
shared cache and hands them back on a hit — but cargo still materialises the
full `target/` in each worktree. Net disk usage goes **up** (per-worktree
`target/` plus a shared cache), while cold-build wall-clock goes down a lot,
because 854 dependency rlibs that are identical across worktrees compile once
instead of once per worktree.

**Costs:** a daemon, cache disk (bounded by `SCCACHE_CACHE_SIZE`), and it does
not cache everything — proc macros and build scripts with side effects still
run. Rust support is mature but not universal.

**Recommendation:** worth doing for developer/agent iteration speed, and it
should be evaluated on that basis. It is not a disk fix.

### 3. `debug = 0` for agent/CI builds

The `.rlib` files carry debug info — `libserver-*.rlib` alone is 401M and
`readelf` reports 256 debug sections in it. `strip = true` removes symbols from
the *final binary*; it does nothing for the 2.79G of intermediate rlibs.

**Saves:** disk, at build time, proportional to the debug-info share of `deps/`.

**Honest caveat: I did not measure that share.** `objdump`/`readelf` did not
yield a clean byte total in the container, so the saving is unquantified —
somewhere between "modest" and "most of 2.79G". **Measure before committing to
this.** A one-line experiment settles it: build one worktree with
`CARGO_PROFILE_RELEASE_DEBUG=0` and diff `du -s target`.

**Costs:** panics lose line numbers. That is a real debuggability regression for
agent tasks, which is exactly when you want good backtraces. Prefer a dedicated
profile over changing `release`:

```toml
[profile.agent]
inherits = "release"
debug = 0
```

so deploy builds keep their line tables and only throwaway worktrees go bare.

### 4. Periodic `cargo sweep` inside long-lived worktrees

For worktrees that live for weeks (`chief-worktrees/*`), `cargo-sweep -t 7`
removes artifacts from older toolchain/feature combinations without wiping the
whole cache. Complements option 1 rather than replacing it.

## What NOT to do

**Do not set a shared `CARGO_TARGET_DIR` across worktrees.** It looks like the
obvious fix — one `target/` instead of N — and it would genuinely eliminate the
duplication. But cargo takes an **exclusive file lock** on the target directory
for the duration of a build. Two agents building at once would serialise, and
the entire point of the worktree-per-task design is that they do not. This trades
a disk problem for a concurrency problem, on the axis we care about most.

## Recommended sequence

1. **Extend the host sweep to `target/release`** (devpod). Biggest win, no
   build-side risk, addresses the measured 12.9G. Confirm the original exclusion
   was not load-bearing first.
2. **Measure the `debug = 0` saving** with the one-line experiment above. If it
   is large, add a `[profile.agent]` and point worktree builds at it. If it is
   small, drop it.
3. **Evaluate `sccache` on build-time grounds**, separately, and do not count it
   as disk relief.
4. **Revisit only if disk climbs again.** As of 2026-08-11 `/` is at 86% with
   119G free and trending down on its own.

## How to verify any of this

```bash
# per-worktree build output, largest first
for d in /var/tmp/chief-worktrees/*/ /var/tmp/vk-deploy-build /var/tmp/chief-deploy/*/; do
  [ -d "$d/target" ] && printf '%8.2fG  %s\n' \
    "$(du -xs "$d/target" | awk '{print $1/1048576}')" "$d"
done | sort -rn

# profile split — release vs debug
du -x --max-depth=1 /var/tmp/vk-deploy-build/target

# deps composition
find /var/tmp/vk-deploy-build/target/release/deps -name '*.rlib' -printf '%s\n' \
  | awk '{t+=$1} END{printf "rlib total: %.2fG\n", t/1073741824}'
```

## Open questions

- Was `target/release` excluded from the host sweep deliberately, or by
  omission? The comment reads deliberate; the `live_exe` gate suggests it may no
  longer be necessary.
- What fraction of `deps/` is debug info? Unmeasured — gates option 3.
- How many Rust worktrees run concurrently at peak? The footprint is
  `N × ~5G`, and nobody has bounded N. That number, not the per-worktree size,
  is what determines whether this recurs.
