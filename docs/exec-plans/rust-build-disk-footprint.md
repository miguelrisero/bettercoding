# Rust build output: per-worktree disk footprint

## Why

`bettercoding` is a Rust workspace — 33 crates, 1139 resolved dependencies — so
anything that builds it materialises a multi-gigabyte `target/`. On ded02
(2026-08-12) three build directories hold **12.9G** between them.

The framing that prompted this plan was wrong in a way worth recording, because
it changes the fix:

> "Each agent task gets its own worktree, cargo builds per worktree, and the
> output frees itself when the task ends."

That describes VK task worktrees under `.vk-worktrees/`, which are ephemeral and
**contain no Rust at all** (they hold `patricia-monorepo`, `runflow-docs`,
`mr_docs`). The Rust build output lives somewhere else entirely: **long-lived
`git worktree` checkouts of this repo**, registered against
`~/projects/bettercoding/.git` and parked in `/var/tmp`. They are created by the
chief tooling for parallel branch work, plus two detached-HEAD build dirs.

They do not clean themselves up, and the measured ones are **cold, not live**:

```
5.02G  vk-deploy-build/target            last built 19 days ago  (detached HEAD)
4.83G  chief-deploy/main-deploy/target   last built  8 days ago  (detached HEAD)
3.06G  chief-worktrees/bc-seamless-p2    last built  8 days ago  (chief/bc-seamless-p2, UNMERGED)
```

No process is parked in any of the three. So this is not the running cost of
concurrent agent work — it is stale build output from branch work that finished
or paused, sitting on a host where disk is the binding constraint (`/` hit 91%
on 2026-08-07).

## Today (verified 2026-08-11, ded02 / team-miguel)

### Where the space is

```
5.02G  /var/tmp/vk-deploy-build/target          (100% release, 0 debug)
4.83G  /var/tmp/chief-deploy/main-deploy/target
3.06G  /var/tmp/chief-worktrees/bc-seamless-p2/target
────
12.9G  total across 3 live build dirs
```

All nine worktrees are registered and none are prunable (`git worktree prune
--dry-run` reports nothing), so `git worktree` has no opinion about them — they
are live checkouts whose *build output* is stale, not orphaned directories.

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

**Do not cap the number of worktrees.** Worktrees are the parallelism mechanism:
one per branch is what lets several agents work bettercoding simultaneously.
Capping them caps concurrent agent work — which is the capability the whole
setup exists to provide — in exchange for disk, which is the cheapest resource
to buy. It also targets the wrong thing. Measured per worktree (2026-08-12):

```
worktree            TOTAL   target/  node_modules   SOURCE
vk-deploy-build     6.11G     5.02G         0.95G    0.15G
main-deploy         5.92G     4.83G         0.94G    0.15G
bc-seamless-p2      4.27G     3.06G         1.03G    0.18G
bc-archive-buckets  1.26G     0.00G         1.12G    0.15G
bc-subagent-tabs    1.20G     0.00G         1.12G    0.08G
```

**The source checkout is 80-180 MB. Everything else is regenerable.** Generated
output is ~97% of a Rust worktree and ~93% of a frontend-only one. Nine
worktrees of source is roughly 1G in total — genuinely a rounding error against
a 876G disk. Nine stale `target/` dirs is 12.9G.

The rule this plan asserts: **retention belongs on generated artifacts, keyed on
age, never on the count of the thing that generates them.** Sweep a cold
`target/` and the worktree stays usable — the next build just takes longer.
Delete a worktree and you have deleted someone's branch checkout.

This is the same error as "reduce `RUNNER_CPUS` to lower load average" on the
devpod side: throttling the mechanism instead of fixing the artifact it leaves
behind. Both look like capacity management and are really just capacity removal.

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
4. **Delete the three cold `target/` dirs now** — 12.9G, regenerable, nothing
   using them. This is a one-off; options 1-3 are what stop it recurring. Cost
   is one full rebuild (33 crates / 1139 deps) the next time each branch is
   touched, so check with whoever owns `chief/bc-seamless-p2` first: it is
   **unmerged**, so that worktree is real work-in-progress even though its build
   is cold.
5. **Put a retention policy on generated artifacts — NOT on the worktree count.**
   See "What NOT to do" below. Sweep `target/` and `node_modules/` on age, and
   let worktrees accumulate freely.

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
- What is the right age threshold for sweeping a cold `target/`? The host's
  existing cargo sweep uses `TARGET_KEEP_DAYS=3`; the dirs measured here were
  cold for 8-19 days, so 3 would have caught all of them comfortably. Reusing
  that number keeps one policy instead of two.
- The six `seo-*` worktrees carry ~20G of `node_modules` with no `target/` at
  all. Same missing retention, different ecosystem — worth covering in the same
  sweep rather than as a separate effort.
