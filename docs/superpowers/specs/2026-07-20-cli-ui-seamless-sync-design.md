# CLI ↔ UI Seamless Sync — Design Spike

- **Date:** 2026-07-20
- **Branch:** `chief/bc-cli-ui-design` (chief run `bc-lanes-2026-07-19`, lane D)
- **Status:** DRAFT v2 (council-hardened) — awaiting owner approval; nothing
  here is implemented. Council round: Codex seats (realtime-systems,
  data-integrity) + Claude seat (product/UX); disposition in §11.
- **Scope:** local deployments, claude executor only (see Non-goals)

## 1. Goal

Owner's ask (verbatim): *"Make the change between console and uix seamless, like
some kind of autoimport where you can switch back to uix side where needed, and
even collaborative, like you can have me on my same account using uix and
another one using cli"* — and: *"even in realtime? super important."*

Concretely, after this design ships:

1. **CLI → UI**: every turn typed into the CLI (tmux) pane appears in the UI
   conversation view in near-realtime (≤ 1 s after the turn lands on disk),
   including turns made while the UI was closed (backfill). No manual import.
2. **UI → CLI**: already works (verified, §3): CLI mode resumes the selected
   chat via `claude --resume <sid>` from the mirrored cwd.
3. **Collaborative**: two people on the same account — one in the UI, one in
   the CLI — can work the same conversation without silently losing turns.

## 2. Background — what exists (all file:line refs at `b6e856e6`)

### 2.1 The two transcript stores

| Store | Writer | Path | Reader today |
| --- | --- | --- | --- |
| **claude store** | every claude process (headless or TUI) | `~/.claude/projects/<cwd-slug>/<claude-session-id>.jsonl` | only claude itself (`--resume`/`--continue`) |
| **app store** | app, capturing headless executor stdout | `<data-root>/sessions/<uuid-2-hex>/<session-uuid>/processes/<execution_process_id>.jsonl` (`crates/utils/src/execution_logs.rs:8-21,94-102`) — lines are serialized `LogMsg` wrappers (`{"Stdout":"…"}`) with **arbitrary chunk boundaries**, not one claude event per line (`crates/services/src/services/execution_process.rs:256-349`) | UI conversation history (normalized on read, `crates/services/src/services/container.rs:857-1000`) |

The UI renders **only** the app store. CLI-added turns exist on disk in the
claude store but are invisible to the UI. That asymmetry is the entire problem.

### 2.2 UI → CLI handover (works today)

- CLI mode runs claude exactly where the executor ran: workspace root +
  `agent_working_dir` (`crates/server/src/routes/terminal.rs:240-255`) —
  because claude keys its store by cwd.
- The claude session id is resolved from the latest coding-agent turn
  (`terminal.rs:263-280`; `CodingAgentTurn::find_latest_session_info`,
  `crates/db/src/models/coding_agent_turn.rs:36-53`).
- **One-way guard**: while a headless executor is RUNNING the session, the
  terminal route withholds the sid (`terminal.rs:263-280`). Notes: fail-open on
  DB error (`unwrap_or(false)`), and it only nulls the resume id — the attach
  itself proceeds and the bootstrap falls through to `--continue || fresh`
  (`crates/local-deployment/src/pty.rs:107-133`). The frontend separately
  avoids mounting the CLI pane while an executor runs
  (`packages/web-core/src/pages/workspaces/CliMainPane.tsx:95`) — client-side
  only, not server enforcement.
- Bootstrap launch precedence (`pty.rs:107-205`): valid-UUID resume →
  staged-prompt file → bare TUI for deferred paste → `--continue || fresh`.
- Interactive CLI mode launches the **user's installed** `claude`
  (`CliLaunchSpec::new("claude", …)`,
  `crates/executors/src/executors/claude.rs:749-760`), while the headless
  executor pins `npx -y @anthropic-ai/claude-code@2.1.154` (`claude.rs:147`).
  The two writers of the shared transcript run **different CLI versions**
  (2.1.215 vs 2.1.154 at the time of the spike).

### 2.3 How the app learns claude session ids — and the CLI-first hole

Headless only: stream-json stdout → `ClaudeLogProcessor` extracts the first
eligible session id (`claude.rs:1181,1231,1310`) → `MsgStore::push_session_id`
→ `coding_agent_turns.agent_session_id`
(`crates/services/src/services/execution_process.rs:309`). Tmux PTY output goes
straight to the terminal WebSocket (`pty.rs:1941`, `terminal.rs:629`) and never
enters this pipeline. **Consequence:** a CLI-first conversation's claude sid is
unknown to the app; a UI "follow-up" on such a session dispatches a
`CodingAgentInitialRequest` (`crates/server/src/routes/sessions/mod.rs:191-208`)
— a brand-new claude session — silently splitting the conversation.

### 2.4 The missing UI-side guard (P0)

`POST /sessions/{id}/follow-up` (`sessions/mod.rs:124-234`) checks executor
type match and container existence, then dispatches
`CodingAgentFollowUpRequest { session_id: <latest agent sid> }`
(`:178, :191-199`) → headless `--resume <same-sid>` (`claude.rs:780`). It
checks **nothing** about the CLI side: not `cli_tmux_session_exists`, not
`workspace_cli_activity`, not pane state (verified; also
`crates/services/src/services/container.rs:526`: *"the interactive session is
not an `execution_process`, so the running-process guards miss it"*). The
composer's queue-vs-send decision keys off streamed execution processes only
(`packages/web-core/src/shared/hooks/useExecutionProcesses.ts:75`,
`packages/ui/src/components/SessionChatBox.tsx:335`), so tmux claude can be
mid-turn while the UI happily fires a concurrent `--resume`. §3 T4 shows
exactly what that does.

### 2.5 CLI activity poller (not transcript import)

`crates/local-deployment/src/cli_activity.rs`: 2 s cadence (`:42`), observes
four tmux metadata fields + `/proc` walk (`:606,:164-170`), classifies
running/attention/idle (`:563-599`), upserts `workspace_cli_activity` on
transitions (`:372`). It reads no transcripts and cannot see turns or session
ids. Its update-hook already re-emits a workspace SSE patch
(`crates/services/src/services/events.rs:128`).

### 2.6 Infra available for reuse

- **Events/SSE**: SQLite update hook → `MsgStore` JSON patches → `/events/`
  SSE (`events.rs`, `crates/server/src/routes/events.rs:15`).
- **Filesystem watching**: `notify` + `notify_debouncer_full` already deps and
  patterns in `crates/services/src/services/filesystem_watcher.rs`.
- **Log normalization**: `ClaudeLogProcessor` parses `ClaudeJson` lines into
  `NormalizedEntry` patches (`claude.rs:1181-1308`;
  `crates/executors/src/logs/mod.rs:70-121`) transported as
  `PatchType::NormalizedEntry` over `/normalized-logs/ws`
  (`crates/server/src/routes/execution_processes.rs:138-185`) — the same
  message shapes the claude store wraps in its envelope (§3 T6).
- **Native-store discovery precedent**: `crates/review/src/claude_session.rs`
  already locates `~/.claude/projects`, discovers non-`agent-*` jsonl files
  and reads records with a deliberately loose camelCase serde model
  (`claude_session.rs:33-46,48-50,144-194`).
- **Prompt paste plumbing**: deferred prompts are pasted into a live TUI pane
  with delivery confirmation and CAS clear (`terminal.rs:300-425,:567,:739`;
  `pty.rs:1211-1278`; single-line <4096 B via `send-keys`, else tmux buffer +
  bracketed paste).
- **Queued messages**: single in-memory slot per session that **silently
  replaces** on enqueue (`crates/services/src/services/queued_message.rs:45-53`;
  route adds no occupied-slot check,
  `crates/server/src/routes/sessions/queue.rs:33,47`).

### 2.7 Landing-soon shape this design assumes

- **PR #36** (tmux identity): socket `bettercoding` (env
  `BC_CLI_TMUX_SOCKET`), session prefix `bc_`, with `vk_`/`vibe-kanban` legacy
  fallback via `CliTmuxHome::{Current,Legacy}`. (`CliClientPresence` itself
  already exists on main — in-memory, per-web-client visibility only,
  `pty.rs:1620-1653`; #36 carries it forward.)
- **PR #37** (dual-home paths): data root resolves `bettercoding` vs legacy
  `vibe-kanban` by `db.v2.sqlite` presence (`resolve_data_dir`). This design
  never hardcodes a home; it uses the path helpers.

## 3. Empirical verification of the shared-transcript seam

**The design stands on this section.** All probes ran 2026-07-20 on the dev
box, in a scratch cwd (`…/scratchpad/seamlab/cwd`), with a throwaway tmux
socket (`tmux -L bc-seamlab`, killed afterwards). No production data dirs, no
app tmux sockets, no running app processes were touched. Executor twin =
pinned `npx -y @anthropic-ai/claude-code@2.1.154`; CLI-mode twin = installed
`claude` 2.1.215. Evidence artifacts (committed under
`docs/superpowers/specs/evidence/2026-07-20-cli-ui-seam/`): the full shared
transcript `evidence-transcript.jsonl` plus per-probe CLI outputs
`t1.json … t4c.json`, `t6.stream.jsonl`.

| # | Probe | Result |
| --- | --- | --- |
| T1 | Fresh headless write, pinned 2.1.154 | Creates `~/.claude/projects/<cwd-slug>/<sid>.jsonl`. Line types seen: `user`, `assistant`, `attachment`, `queue-operation`, `last-prompt`. Content lines carry `sessionId`, `uuid`, `parentUuid`, `cwd`, `version`, `gitBranch`, `isSidechain`, `timestamp`, `message` (an API Message object). |
| T2 | Headless `-p --resume <sid>`, same version | **Same session id, same file appended** (90,439 → 113,747 B; one file in dir). Recalled T1 state ("CODEWORD=PLUM"). No fork. |
| T3 | Interactive 2.1.215 `--resume <sid>` of the 2.1.154 transcript (tmux) | TUI rendered the full prior history; new turn appended to the **same file, same sessionId**, contiguous `parentUuid` chain, lines tagged `"version":"2.1.215"`. New record types appear (`mode`, `bridge-session`, `permission-mode`, `system`); some bookkeeping lines have **no** `sessionId`. |
| T4a | Headless resume **while the TUI stayed attached** | Succeeds; same sid; same file; the headless run **saw the TUI's turns** (answered the TUI-set codeword). Disk is the shared source of truth at resume time. No file-level corruption; every line across 75+ dual-writer lines parsed cleanly. |
| T4b | TUI sends again after T4a | **DAG fork**: one parent uuid (`e647b0e3…`) now has two children — the headless branch and the TUI branch. Two leaves in one file. |
| T4c | Fresh headless resume after the fork | Followed **one** branch (the one referenced by the final `last-prompt.leafUuid`) and listed only that branch's history — the other branch was **silently dropped from model context**. |
| T6 | Executor-style `--output-format stream-json` vs store file | **Assistant event `uuid`s match the store line `uuid`s exactly** (2/2). The `-p` stream carries no user-turn event uuid. |
| T7 | Intra-turn flush cadence during a 5-step tool-using run (~16 s) | The store file grew in **five distinct appends spread across the run** (t≈4.4/10.2/13.4/16.0/16.4 s), not one write at turn end: claude flushes **per step, mid-turn**. A watching UI can stream a CLI turn's tool activity as it happens. |

Conclusions the design builds on:

- **C1 (seam is real):** one jsonl per claude session id, keyed by cwd slug;
  every writer (any version, headless or TUI) appends turns to the same file
  under the same sid. CLI→UI import = tail this file. Terminal capture is
  unnecessary and inferior.
- **C2 (cross-version safe today, not contractual):** 2.1.154 ↔ 2.1.215
  read/write interop verified both directions; but the store format is a
  private, unversioned contract that grew new record types between those
  versions. Ingestion must be tolerant (skip unknown types, per-line
  `version` available for gating).
- **C3 (concurrency danger is context loss, not file corruption):**
  dual-writer appends do not tear the file; the damage is a forked uuid-DAG
  where subsequent resumes follow one leaf and silently drop the other branch.
  This is the empirical justification for the §6 guards.
- **C4 (dedup is tractable):** executor-originated assistant turns can be
  deduped **exactly** by line `uuid` (T6); user turns the app itself submitted
  are known to it (prompt text + run window).
- **C5 (granularity):** the store gains whole records per content block, not
  tokens — and it gains them **mid-turn, per step** (T7). CLI→UI realtime is
  **step/block-level** (tool calls and intermediate assistant blocks stream in
  as they land), not keystroke-level; the running/idle cue comes from
  `cli_activity`.
- Incidental: first attach in a fresh scratch cwd hits claude's folder-trust
  prompt before the TUI (T3) — CLI-mode bootstrap in brand-new worktrees has a
  first-run friction step that "seamless" messaging should acknowledge (F8).

## 4. Design A — Transcript unification

**Recommendation: the claude store is the canonical source for conversation
content; the app store remains canonical for run/process metadata. The UI
renders a merged, deduplicated view keyed by claude session id.**

- **Id mapping — bindings, not guesses (council-hardened).**
  `coding_agent_turns.agent_session_id` already registers executor-created
  sids per turn (`coding_agent_turn.rs:11,36-53`). For CLI-created sids, the
  binding is captured **at launch time**, not inferred later: the app knows
  the resume sid it hands each bootstrap (`terminal.rs:263-280`), and when a
  *fresh* CLI launch is spawned for a workspace pane, the next new
  `<sid>.jsonl` appearing in that pane's cwd slug while that pane is the only
  active claude there binds to that pane's app session. A
  `claude_session_links` table records
  `(claude_session_id PK, session_id, workspace_id, cwd, bound_via
  ('executor'|'cli-resume'|'cli-fresh'|'manual'), created_at)`. Discovered
  files that match **no** binding are **quarantined** into a visible
  "unassigned CLI conversations" list for manual assignment — there is no
  latest-session fallback (a wrong auto-bind pollutes a conversation and its
  future resumes; council DI-6). Re-attribution moves the link row only —
  the link is the sole ownership relation; imported rows do not duplicate
  `session_id` (council DI-7). This also closes the §2.3 CLI-first hole:
  once a CLI-first sid is bound, `follow_up` can resume it instead of
  silently starting a new conversation.
- **Dedup — import everything, reconcile by identity (council-hardened).**
  The ingester imports **every** native record; nothing is suppressed by
  timestamp (a CLI turn landing during an executor run window is real data —
  council RTS-1/DI-1). Reconciliation against executor-rendered entries uses
  durable identity only: extend the existing stream-json parse
  (`claude.rs:1181-1240`) to persist every observed event `uuid` per
  execution into an `execution_native_links` bridge; the merged view renders
  native records as canonical and treats executor-stream entries as a
  provisional live overlay that is replaced when the matching native record
  (same uuid) arrives. App-authored user turns — which have no stream uuid
  (T6) — are bound explicitly at dispatch: the ingester matches the first
  native user record with the exact dispatched prompt after the dispatch
  point and records that binding durably. Logical-turn duplicates from
  claude-side retries (fresh uuids, same content) are a known P2: keep raw
  records, add a deterministic render-side reducer later if observed in
  practice (council DI-3).
- **Fork representation (council-hardened).** A fork is *observed*, not
  inferred: >1 child for one `parentUuid` within a file (T4b). With
  concurrent writers a single file-wide "mainline" is **not well-defined**
  (each writer keeps its own head; `last-prompt` can be stale — council
  DI-5): the UI renders the **common prefix** inline, then each divergent
  leaf as a labeled branch, using the newest valid `last-prompt.leafUuid`
  only as the *"claude will resume this branch"* hint (T4c) to mark the
  default-expanded branch. Copy is plain-language ("some messages went down
  a path the agent is no longer following"), and the dropped branch offers a
  one-click **"bring these back"** that re-sends its user turns onto the
  resume branch (council UX-6). All branches' turns are always persisted.

## 5. Design B — CLI→UI realtime ingestion

**Recommendation: a read-only `claude_transcript_ingest` service in
`crates/services`, notify-watch + offset-tail + normalize + persist + SSE.**

- **Watch mechanism.** Per open/CLI-active workspace, resolve the claude
  project dir from the effective dir exactly as `terminal.rs:240-255` does
  (root + `agent_working_dir` → cwd → slug), then verify by presence of the
  expected `<sid>.jsonl` rather than trusting slug math (the slug encoding is
  claude-internal; filename discovery is the robust anchor —
  `crates/review/src/claude_session.rs:144-194` is the in-repo precedent).
  Watch **only that slug directory** with `notify` + `notify_debouncer_full`
  (`filesystem_watcher::async_watcher` proves the mechanics at 200 ms
  debounce, `filesystem_watcher.rs:455-475`; on Linux watches are
  per-directory non-recursive, `:54-59,294-365` — another reason not to watch
  all of `~/.claude/projects`). The transcript watcher gets its own
  deployment-owned lifecycle guard — `DiffStreamHandle` ownership and the
  gitignore-aware worktree traversal are not reused. Exclude `agent-*` files
  (subagent sidechains) in v1. Fallback when notify is unavailable: piggyback
  a `stat()` on the existing 2 s `cli_activity` tick. **Latency SLO, split
  honestly (council RTS-8):** watch path p95 ≤ 1 s from line-on-disk to UI
  patch (debounce 200-300 ms + read + patch); poll-fallback path is a
  declared degraded mode (2-4 s worst case) surfaced in the ingest health
  indicator, plus a periodic reconcile scan so a missed notify event can
  delay a turn but never lose it. A debounce window can coalesce many
  appended lines — each flush processes every complete line found, not one.
- **Tail protocol (council-hardened).** Per file: open `O_RDONLY`, seek to
  cursor, read to EOF, split lines, buffer a trailing partial line until its
  newline arrives (T4 saw no torn lines, but tailing must not depend on
  that). **Crash consistency rule:** imported rows and the new
  newline-aligned cursor commit in **one SQLite transaction** — never advance
  past a partial line, never commit the cursor separately from its rows
  (council RTS-7/DI-8). File identity is tracked as `(dev, inode)` + a
  generation counter; a changed identity, a `size < cursor`, or any watcher
  error triggers a full rescan reconciled through the idempotent import path
  (equal-size rewrites are caught by re-verifying the last imported line's
  bytes at its recorded offset — council DI-9). New files in the dir ⇒
  discovery/quarantine (§4). **Read-only posture (claim downgraded per
  council DI-11):** the ingester's file API never opens for write, never
  renames, never creates files under `~/.claude`; enforced by code review
  plus a **release-mode integration test** that runs the ingester against a
  deny-write (read-only bind/permission) copy of a store dir and asserts
  zero write attempts — debug assertions alone don't survive release builds.
- **Normalization.** Store lines wrap the same API `message` objects the
  stream-json pipeline parses (T6): reuse `ClaudeLogProcessor`'s entry state
  machine through a thin **native adapter** — do not feed native lines in
  unchanged. Known deltas the adapter owns: the native envelope is camelCase
  (`sessionId`) while `ClaudeJson` expects `session_id`
  (`claude.rs:2711-2764`); the `Default` history strategy deliberately drops
  ordinary user turns and reclassifies string-form user content as system
  messages (`claude.rs:1882-1949`) because the UI reconstructs user rows from
  the execution turn's stored prompt
  (`deriveConversationSemanticTimeline.ts:88-100`) — CLI-imported user turns
  have no execution turn, so a `NativeClaude` history strategy must emit them
  as real `user_message` entries and the timeline must keep them for
  `origin='cli'`; native `timestamp`/`gitBranch`/`parentUuid` are not part of
  `ClaudeJson` and are carried by the ingest row instead. Render
  `user`/`assistant` (incl. tool_use/tool_result blocks); skip bookkeeping
  (`attachment`, `queue-operation`, `last-prompt`, `mode`, `bridge-session`,
  `permission-mode`); tolerate unknown types with a counter metric (C2);
  tolerate missing `sessionId` lines (attribute to the file's sid).
- **Persistence (council-hardened schema).** Raw first, render second. Raw
  table `cli_native_records (file_id, line_seq, claude_session_id, uuid
  NULLABLE, parent_uuid, kind, ts NULLABLE, raw JSON, PRIMARY KEY (file_id,
  line_seq))` — keyed by file identity + line sequence because uuid-less
  records exist (`last-prompt`, `mode`, …) and the DAG/fork model needs
  `parent_uuid` and order preserved (council RTS-12/DI-4); `last-prompt`
  checkpoints are stored even though never rendered (they carry the resume
  hint). A derived, rebuildable render table/view holds the normalized
  entries the UI consumes. Unknown kinds are persisted raw, so a future
  parser upgrade can re-render history that older code skipped (council
  DI-10).
- **Transport (council-hardened).** SQLite update hooks fire **before**
  commit and re-query asynchronously (`events.rs:119-279`) — good enough for
  status badges, not for conversation ordering (council RTS-9/10/11). Ingest
  publication therefore uses a small **transactional outbox**: the same
  transaction that inserts rows appends `(session_id, seq)` outbox entries
  with a per-session monotonic sequence; a post-commit publisher drains the
  outbox into a **session-keyed revisioned snapshot-plus-live stream**
  (subscribe-before-snapshot, and any observed sequence gap or broadcast lag
  forces a resnapshot). The chat UI consumes ONE merged session feed in
  which native records are canonical and the executor's live
  `NormalizedEntry` stream is a provisional overlay reconciled by uuid (§4),
  merged into the semantic timeline
  (`useConversationHistory.ts:96-121,437-463`). History survives restarts
  and renders offline (backfill = full-file scan through the same idempotent
  path on first registration or app start).
- **Failure modes.** Partial trailing line (buffer); unknown record type
  (skip + metric); cwd mismatch / worktree moved (re-resolve via sid-file
  discovery); store file absent (no-op); fork (represent, §4); inotify watch
  limit (poll fallback); version skew (C2 posture + a canary fixture test
  pinned to a committed sample transcript so a store-format change fails CI
  loudly, not silently).

## 6. Design C — Concurrency & collaboration semantics

**Recommendation: a derived single-writer lease per claude sid, with UI sends
routed INTO the live CLI pane (paste) when the CLI owns the lease — plus the
missing server-side guards and an explicit queue-replace contract.**

- **Lease (derived + re-probed; council-hardened):** writer = the running
  headless executor (`ExecutionProcess::has_running_coding_agent_for_session`)
  if any; else the CLI, per the **pane binding** — `(workspace, pane, agent
  pid, claude_sid, app session)` recorded at launch (§4), not the bare
  `workspace_cli_activity` row, which carries no sid (council RTS-2); else
  free. Two TOCTOU closures (council RTS-3/4): (1) dispatch decisions
  **re-probe tmux/process/executor state synchronously** inside the per-sid
  critical section — the 2 s poller cache is a hint, never the authority;
  ambiguity resolves to 409/queue, and DB errors fail **closed** (guard
  today is fail-open, §2.2). (2) Spawning either kind of claude first takes
  a short fenced per-workspace reservation held until the new process is
  durably registered (executor row / pane binding), so a fresh CLI that has
  no sid yet is never invisible to the lease.
- **Scope boundary (council UX-2), stated plainly:** these guards bind the
  app's own surfaces. A human running `claude --resume` in a raw terminal
  bypasses every app-side lease. The ingester doubles as the detector: a
  native record arriving with no app-known origin (no executor stream match,
  no bound pane active) raises a "this conversation is being edited outside
  the app" banner. That detection-not-prevention limit is written into F.
- **UI send while CLI holds the lease:** do NOT spawn a concurrent headless
  `--resume` (T4 proves silent branch loss). Route the prompt through the
  existing paste plumbing into the live TUI (`terminal.rs:739`,
  `pty.rs:1211-1278`; claude's TUI natively queues input typed mid-turn) —
  upgraded to a **durable delivery state machine** (council RTS-5):
  `pasted → submitted → imported`, where *submitted* is acknowledged only
  when the ingester observes the matching native user record in the
  transcript (pane-survival heuristics like `terminal.rs:739`'s are
  necessary but not sufficient — the Enter keystroke can fail with the text
  left sitting in the composer, `pty.rs:1278`, and §3's T3 hit exactly that
  bracketed-paste edge). The sender's UI reflects true state ("queued in
  CLI — not yet submitted" until imported; council UX-5). On timeout: the
  message returns to a **persisted** queue slot + a notice — never dropped.
  Alternatives rejected: hard 409 always (hostile to the two-person flow);
  allow the fork (empirically loses turns, T4c).
- **CLI attach while executor runs:** keep the existing sid-withholding guard
  but fix its UX: today the bootstrap silently starts a *different*
  conversation (`--continue || fresh`, §2.2). Phase 2: when the guard bites,
  the bootstrap prints "agent busy in UI — this pane will resume the
  conversation when it finishes" and polls, instead of forking user attention.
  Also harden the guard fail-closed (DB error ⇒ withhold the sid, not hand it
  out) and add the same check server-side where the frontend currently
  self-gates (§2.4).
- **Queue fix (council-hardened):** two changes. (1) The slot becomes
  **persistent** (DB-backed) — the current DashMap slot is memory-only and
  consumed solely by executor finalization, which loses queued sends on
  restart and never fires for a CLI-held lease (council RTS-6); a small
  consumer runs on lease release. (2) Silent replace becomes explicit: the
  route gains `replace: bool`; enqueueing over an occupied slot without it
  returns 409 + current status, and the composer's confirm shows the queued
  message's **text and source** ("replace the CLI's queued message: '…'?" —
  council UX-8). (Evidence of today's silent replace:
  `queued_message.rs:45-53`.)
- **Conflict matrix (defined behavior):**

| Case | Behavior |
| --- | --- |
| UI sends, executor running (same session) | queue (existing), now with explicit replace contract |
| UI sends, CLI attached & idle | paste into TUI (delivered-to-CLI notice) |
| UI sends, CLI attached & mid-turn | paste into TUI (claude queues it natively); fallback 409+queue |
| CLI attaches, executor running | attach proceeds, sid withheld, busy-notice + wait (no silent fork) |
| Both send within the same window | per-sid critical section picks one writer; loser follows its rule above |
| CLI types while UI streams executor output | app-side collision blocked by the lease; TUI keystrokes affect only the TUI. (Raw-terminal writers are outside the lease — detected and bannered, not prevented.) |

Every "loser" path in this matrix has a defined, visible outcome for the
person whose send didn't go through directly — a toast naming what happened
and where the message went ("held — the CLI is mid-send; your message is
queued and will go next") plus a persistent queue-state chip. Silent
rerouting is treated as a bug (council UX-3).

- **Presence:** what exists today is thinner than it looks: per-web-client
  visibility presence lives in memory only (`CliClientPresence`,
  `pty.rs:1620-1653`; heartbeat `TerminalProvider.tsx:249-304`), the
  persisted row is just `workspace_id + state + updated_at`
  (`workspace_cli_activity.rs:40-45`), nothing identifies *which* client is
  attached, and the `attention` badge lags up to the 15 s summary poll
  (`useWorkspaces.ts:170-195`). Phase 3 therefore adds a real presence
  surface: extend `workspace_cli_activity` (or a sibling table on the same
  update-hook path) with attach counts/kinds fed by the poller's
  `session_attached` plus the web-client registry. Header chips: "CLI
  attached", "UI viewing (n)". Imported turns are labeled "via CLI" (from
  `origin`, §5).

## 7. Design D — Mode-switch UX ("seamless", concretely)

- **CLI → UI:** open the workspace; the conversation is already complete
  (backfill) and stays live (≤ 1 s per landed step, C5). While the CLI agent
  is mid-turn the UI **streams its steps** — tool calls and intermediate
  assistant blocks appear as claude flushes them (verified intra-turn, T7) —
  on top of the running badge. Only keystroke/token-level mirroring is out
  (C5).
- **UI → CLI:** unchanged and verified working (§2.2, §3 T3): attach resumes
  the exact conversation with full history. The §6 busy-notice removes the one
  case where it silently doesn't.
- **In-flight runs on switch:** they continue; both surfaces observe. An
  executor run keeps streaming to the UI while a CLI pane waits politely; a
  CLI run keeps writing the store while the UI imports each landed block.
- **Sessions vs panes:** switching never kills anything — the tmux session
  survives detach (existing behavior), the executor survives UI navigation.

## 8. Design E — Phasing

**Phase 1 — read-only realtime import (M, risk: low — read-only by
construction; highest owner value).**
Ingest service + discovery/quarantine + reconciliation + raw persistence +
outbox stream + backfill + fork rendering; feature-flagged. Includes two
small presence crumbs pulled forward from Phase 3 so imported turns don't
read as a glitch (council UX-7): an inline "via CLI" marker on imported
turns and a coarse "CLI session active" hint sourced from the existing
`cli_activity` state. **Verification (live e2e):** converse in a CLI pane on
a dev workspace → steps appear in the UI ≤ 1 s each (including mid-turn tool
steps, T7); kill the app mid-CLI-run, reopen → backfill imports what was
missed; run an executor turn → no duplicates in the merged view; replay the
committed T4 evidence transcript → common prefix + branches render, nothing
crashes; release-mode deny-write test proves the ingester never writes to
`~/.claude`.

**Phase 2 — concurrency guards + collaboration semantics (M-L, risk: medium —
touches dispatch paths).**
Pane-binding lease with synchronous re-probe + fenced spawn reservation;
follow-up paste-routing with the ingest-acknowledged delivery state machine;
durable queue + replace contract; CLI-attach busy-notice; fail-closed guard;
server-side mirror of the frontend gate; foreign-writer banner.
**Verification:** two-browser + one-tmux live test of every row in the §6
matrix including each loser-path toast; kill the server between paste and
ack → message recovered from the persisted queue; specifically prove the T4
fork can no longer be produced through app surfaces (inspect the store DAG
stays single-leaf).

**Phase 3 — presence & polish (S, risk: low).**
Presence chips, "via CLI" attribution, dropped-branch viewer, latency metrics,
ingest health surface. **Verification:** presence chips track attach/detach
within 2 s; attribution correct across a mixed CLI/UI conversation.

**Explicitly OUT:** terminal scrollback capture; multi-account/auth semantics;
remote/cloud deployments; token-level CLI streaming; non-claude CLI import
(codex/gemini stores differ — future spike); claude-store compaction/summary
record semantics beyond skip-and-count.

## 9. Design F — Risks & decisions for the owner

Each with recommendation; nothing proceeds until you approve.

1. **UI-send-while-CLI-attached policy.** Options: paste-into-TUI (rec — keeps
   one writer, both humans see the same pane text), queue-until-detach,
   reject. Rec: **paste-into-TUI** with ingest-acknowledged delivery and
   persisted-queue fallback (§6). Sub-decision: the pasted text appears in
   the CLI person's composer unattributed and auto-submits — acceptable for
   a same-account pair? (Alternative: require the CLI person to press Enter;
   costs the "it just went" feel. Rec: **auto-submit**, with the CLI pane
   bell/flash on injection.)
2. **Fork display.** Common prefix + labeled branches + resume-hint default
   (rec, §4) vs hiding dropped branches vs full DAG view. Rec: **prefix +
   branches**, plain-language copy, one-click "bring these back" recovery.
3. **CLI attach during an executor run.** Busy-notice + wait (rec) vs today's
   silent fresh session vs blocking attach entirely. Rec: **busy-notice**.
4. **Retention of imported turns.** Keep forever in app DB (rec — it's text,
   the claude store already persists it anyway) vs TTL. Rec: **keep**; revisit
   only if DB size ever matters.
5. **Version-skew posture.** Tolerate skew with canary fixture test (rec —
   empirically fine today, C2) vs pinning CLI mode to the executor's npx pin
   (adds npx startup latency to every CLI attach, and users expect their own
   `claude`). Rec: **tolerate + canary**.
6. **Queue contract.** Explicit `replace` flag + 409 + persisted slot (rec)
   vs keeping the silent in-memory replace. Rec: **explicit replace +
   durable slot** — the confirm always shows whose message and what text is
   being replaced.
7. **Discovered-sid attribution.** Auto-attribute with re-attribution UI (rec)
   vs asking every time. Rec: **auto + re-attribute**.
8. **First-run trust prompt.** Accept as-is (rec for now) vs pre-trusting
   workspace dirs via claude config (touches the user's global claude
   settings — out of comfort zone without explicit approval). Rec: **as-is**,
   document it.
9. **Privacy note.** Ingestion copies conversation content that already lives
   on the same machine into the app DB (same user, same disk). Any future
   remote sync of that table is a separate decision — flagged now so it never
   happens implicitly.
10. **Who-said-what identity (council UX-1).** Same-account collaboration
    means the app cannot distinguish the two humans; "via CLI" labels the
    surface, not the person. Options: per-client editable nickname stamped on
    turns and presence chips (rec) vs accepting surface-only attribution.
    Rec: **lightweight client nickname** (defaults: "UI" / hostname of the
    CLI attach), Phase 3.
11. **Raw-terminal writers (council UX-2).** The lease cannot bind a claude
    run the app didn't spawn. Options: detect + banner (rec — the ingester
    sees foreign turns for free) vs ignore. Rec: **detect + banner**;
    prevention is out of scope.
12. **Idle-surface notification (council UX-9).** Should the non-active
    surface get a nudge ("partner replied via CLI") when imported turns
    arrive? Rec: **yes, reuse the existing attention/notification plumbing**,
    Phase 3.
13. **Cross-surface composing awareness (council UX-10).** A coarse "the
    other side is typing/about to send" indicator would prevent most
    simultaneous-send collisions before the lease has to resolve them. Rec:
    **defer to Phase 3+** (nice-to-have; the lease already makes collisions
    safe, only occasionally surprising).

## 10. Open questions (technical, non-blocking for approval)

- Reuse `ClaudeLogProcessor` internals directly vs a thin adapter crate-side:
  decided at implementation time by whichever avoids exposing executor
  internals to `services` (candidate: move the entry mapper into a shared
  module of `crates/executors`).
- Presence storage shape for Phase 3: extra columns on
  `workspace_cli_activity` vs a sibling table (both ride the same update-hook
  path; pick whichever keeps the upsert single-row).
- Exact debounce constant (200-300 ms starting point; tune against the e2e
  latency target).

## 11. Council round — disposition

One design-review round, three seats: realtime-systems (Codex/gpt-5.6-sol),
data-integrity (Codex/gpt-5.6-sol), product/UX (Claude Opus). 33 findings
total; every P0/P1 was either folded into v2 above or recorded as an explicit
owner decision. Highlights of what changed from v1:

| Finding (seat) | Disposition |
| --- | --- |
| Executor-window dedup silently drops real CLI turns (RTS-1, DI-1, P0) | **Folded §4**: import-all + identity-based reconciliation; no timestamp suppression |
| Sid auto-attribution can bind wrong session (DI-6, P0); lease can't name the pane's sid (RTS-2, P0) | **Folded §4/§6**: launch-time pane bindings; quarantine + manual assign; no latest-session fallback |
| Lease TOCTOU windows; 2 s cache staleness (RTS-3/4) | **Folded §6**: synchronous re-probe in the critical section; fenced spawn reservation; fail-closed |
| PK breaks on uuid-less records; DAG not reconstructable (RTS-12, DI-4) | **Folded §5**: raw table keyed (file_id, line_seq), parent_uuid + raw payload persisted |
| File-wide "mainline" ill-defined across writers (DI-5) | **Folded §4**: common prefix + labeled branches; last-prompt = resume hint only |
| Cursor crash-consistency; equal-size rewrites (RTS-7, DI-8/9) | **Folded §5**: rows + newline-aligned cursor in one transaction; file identity/generation; last-line re-verify |
| SQLite hooks fire pre-commit; reconnect gaps; no joint ordering (RTS-9/10/11) | **Folded §5**: transactional outbox + per-session sequence; revisioned snapshot stream; single merged feed |
| Paste "confirmation" doesn't prove submission (RTS-5); sender lied to (UX-5) | **Folded §6**: delivery state machine acked by the ingester observing the native user record |
| In-memory queue loses sends (RTS-6); silent replace opaque (UX-8) | **Folded §6**: persisted slot + explicit replace showing text/source |
| Read-only claim unenforceable via debug asserts (DI-11) | **Folded §5**: claim downgraded; release-mode deny-write test |
| Notify reliability vs 1 s SLO (RTS-8) | **Folded §5**: split SLO + reconcile scan + health surface |
| Same-account identity unknowable (UX-1, P0) | **Owner decision F10** |
| Raw-terminal writers bypass the lease (UX-2, P0) | **Folded §6** (detect + banner) + **owner decision F11** |
| Loser-path visibility (UX-3); presence before Phase 3 (UX-7) | **Folded §6 matrix note / §8 Phase 1** |
| Intra-turn flush cadence unverified (UX-4) | **Closed empirically**: new probe T7 (§3) — per-step flushes confirmed |
| Idle-surface notification / composing awareness (UX-9/10) | **Owner decisions F12/F13** |
| Logical-turn retry duplicates (DI-3, P2) | **Recorded §4** as P2 with render-side reducer fallback |

## Appendix — Evidence

`docs/superpowers/specs/evidence/2026-07-20-cli-ui-seam/`:

- `evidence-transcript.redacted.jsonl` — the full T1→T4 shared transcript
  (sid `06a7eacd-664b-4d9c-83f3-d4774a6216a8`), including the cross-version
  appends and the T4 fork (parent `e647b0e3…`, leaves `975a2278…`/`13cd5918…`).
  **Redacted for the public repo:** every line keeps the full envelope the doc
  cites (`type`, `uuid`, `parentUuid`, `sessionId`, `version`, `timestamp`,
  `gitBranch`) and the probe-authored user/assistant text; attachment and
  bookkeeping payloads (which embed local environment details) are replaced
  with `{"redacted": true}` stubs that keep the record `type`.
- `t1.json`, `t2.json`, `t4.json`, `t4c.json` — headless CLI result envelopes
  (session ids, results; trimmed to the cited fields).
- `t6.stream.redacted.jsonl` — executor-style stream-json run whose assistant
  uuids match the store lines (same redaction rules).

Probe environment: Linux, `npx -y @anthropic-ai/claude-code@2.1.154` vs
installed `claude` 2.1.215, scratch cwd, throwaway tmux socket `bc-seamlab`
(destroyed after T4). No production paths, sockets, or processes involved.
