# Agent-Activity Legibility — Design Spike

- **Date:** 2026-07-23
- **Branch:** `chief/bc-agent-activity-design` (chief run, lane N)
- **Status:** Facet 2 (realtime subagent strip) **DECIDED & implemented for the executor / UI-driven path** — Lane P (`chief/bc-subagent-tabs`). Facets 1 & 3, and the native/CLI-sidechain path of facet 2, remain design-only. See **§0** for the owner decisions that override the recommendations below where they conflict.
- **Scope:** local deployments, claude executor + CLI/native sessions (see Non-goals). The *implemented* slice is **executor / UI-driven subagents only** — native/CLI sessions surface their own subagents in the terminal and are out of scope here (§0).
- **Builds on:** [`2026-07-20-cli-ui-seamless-sync-design.md`](2026-07-20-cli-ui-seamless-sync-design.md)
  (PR #38 / #39 / #41). This doc is essentially that spike's deferred
  **"Phase 3 — presence & polish"** (§8 there), expanded, plus the subagent
  surface it explicitly left out (§5 there: *"Exclude `agent-*` files (subagent
  sidechains) in v1"*).
- **All file:line refs at `origin/main` @ `0fbc0965`.**

## 0. Decision & as-built (Lane P, 2026-07-23)

Two owner decisions were made after this spike and **override the recommendations
below where they conflict**:

**Decision 1 — EXECUTOR / UI-DRIVEN AGENTS ONLY (not CLI).** Verbatim: *"tabs is
only for non-cli, so for uix. cli already handles these sessions on it's own no
worries."* A real `claude` in a terminal already surfaces its own subagents, so
the CLI/native-sidechain path of facet 2 (§4.1 un-hiding, §6.5 "Native (CLI)
subagents", §10 Q6) is **out of scope** for the implementation. This removes the
design's riskiest piece (reversing #38's `agent-*.jsonl` exclusion / un-hiding
sidechain records). The strip is a pure **read-only projection over the executor
`task_create` conversation rows** the inline `ChatSubagentEntry` already consumes
— no new ingestion, no schema, no new stream.

**Decision 2 — PER-TAB ACTIVITY.** Each active subagent tab carries its own
animated working indicator (reuses `RunningDots`), so the owner sees which
subagent is busy vs done at a glance — not one session-wide state (resolves §10
Q5a → yes).

Resolutions to §10 for the shipped facet-2 slice: **Q3** → (b) ~5 s linger
(`FINISHED_LINGER_MS = 5000`) then collapse to the drawer; **Q4** →
`MAX_ACTIVE_TABS = 4`, extras fold into the trailing "N more ▾" button; **Q5** →
(a) per-tab pulse YES, (b) header naming the working subagent deferred with facet
1; **Q8** → executor path implemented now, native/CLI deferred (Decision 1);
**Q9** → generic "Subagent" fallback when `subagent_type` is absent.

### As-built (Lane P)

- **Projection (pure, unit-tested):**
  `packages/web-core/src/features/workspace-chat/model/subagent-strip-model.ts`
  — `selectSubagents(entries)` maps `task_create` tool_use rows to descriptors
  (phase: active = `created`/`pending_approval`, done = `success`, error =
  `failed`/`denied`/`timed_out`); `partitionStrip(...)` does the active-first tab
  budget, ~5 s linger, and overflow. Tests in `subagent-strip-model.test.ts` and
  `subagent-strip-partition.test.ts`.
- **Hook:** `.../model/hooks/useSubagentStrip.ts` — tracks completion `doneAt`
  per key and schedules a single absolute-deadline timer for the linger; reads
  the live session-scoped `useEntries()` stream (no new subscription).
- **UI:** `.../ui/subagent-strip/{SubagentStrip,SubagentTab,SubagentDrawer}.tsx`
  — leading Main chip (`AgentIcon`), active tabs with per-tab `RunningDots`,
  done/error icons mirrored from `ChatSubagentEntry`, a Radix-`Popover` "N done ▾"
  / "N more ▾" drawer reusing `ChatSubagentEntry`, and a mobile summary line +
  expandable sheet (no horizontal tab scroll).
- **Mount:** above `<ConversationList>` in
  `.../pages/workspaces/WorkspacesMainContainer.tsx` and `VSCodeWorkspacePage.tsx`
  (inside the session-scoped `EntriesProvider`); the strip self-hides when there
  are no subagents. The global 7-tab nav (§8) is untouched.
- **i18n:** `conversation.subagentStrip.*` across all 7 locales.

### Still design-only after this PR (recommendations unchanged, not built)

- Facet 1 "streaming…" patch-cadence pill (§5).
- Facet 3 full `AgentRef` unification across turns / presence / quarantine (§7) —
  only the executor-slice identity (subagent_type ▸ icon, role main/subagent) is
  realized in the strip.
- Native/CLI subagent surfacing (§4.1, §6.5 "Native (CLI) subagents", §10 Q6).

## 1. Goal — one theme: the web UI does not make agent activity legible

The owner named three things. They are one theme — *when an agent (main or sub)
is doing something, the web UI should show it* — split across three facets:

1. **(NEW, most urgent) Processing indicator.** Verbatim: *"on UIX mode you
   don't know when the agent is doing/processing. that's not good."* There is no
   clear, in-view cue that the agent is actively working/thinking/streaming —
   specifically in CLI ("UIX") mode.
2. **Realtime subagent tabs.** Verbatim: *"tabs on cli tracking in realtime,
   subagents being spawned as tabs."* Surface spawned subagents live. Owner's own
   nuance, honored throughout: *"Claude Code spawns many subagents but then
   they're inactive, so maybe also a small button to check inactive subagents but
   not there as tabs by default."* → **ACTIVE subagents as tabs; INACTIVE ones
   behind a button, not tabs by default.**
3. **Main-vs-subagent identity.** Generalize the identity/presence model that the
   quarantine panel and session labeling already reach for (concurrent lanes L/M),
   rather than re-spec that panel.

After this design ships, in one sentence: **at any moment the person in the web
UI can tell (a) whether the agent is working right now, (b) which subagents are
running right now, and (c) who produced any given turn — main agent, a named
subagent, or the CLI vs the app** — on both desktop and mobile, without the tab
strip blowing up.

### 1.1 The coarse facet-1 fix is already a separate lane (Lane O)

The urgent, coarse half of facet 1 — *"is the agent working right now"* on the
in-view CLI surface — is **shipping separately and immediately as Lane O
(`chief/bc-cli-working-indicator`)**. This spike's investigation established that
the running signal already runs end-to-end (§4.4, §5.4), so no backend is needed;
the boss pulled it into its own fast lane on that finding. **Lane O's scope is
deliberately coarse:** animate the existing `is_running` signal on the in-view
CLI surface + `CliMainPane`, and distinguish working-vs-idle copy. Nothing more.

**This document does not re-spec Lane O.** It owns the *finer-grained remainder*
Lane O explicitly is not doing (§5): per-subagent activity indication,
per-message streaming granularity, an "actively emitting" signal derived from
patch cadence, and — the real composition question — **how the processing
indicator relates to the subagent tabs** (does a tab pulse while its subagent
works?). Facet 1 here is: *coarse fix in Lane O; here is the complete
activity-legibility model it is the first step of.*

Facets 2–3 are a genuine feature with real product decisions: what counts as
"active", whether finished-subagent tabs linger, how many tabs before overflow,
whether the indicator names the subagent, and how identity unifies. Those
decisions (§10) are the point of this document.

## 2. Relationship to the seamless-sync design (don't contradict — extend)

| Seamless-sync (#38) established | This doc extends it by |
| --- | --- |
| §2.5 `cli_activity` poller derives `running`/`attention`/`idle` from tmux pane behavior and rides the SQLite update-hook → workspace SSE patch | Facet 1: surface `running` as an **in-view animated** cue, not just a sidebar bucket + a static badge (the coarse version of this = **Lane O**, §1.1; this doc adds streaming + per-agent granularity) |
| §5 ingests the main transcript but **excludes `agent-*` sidechain files in v1**; sidechain records are skipped | Facet 2: this is exactly the deferred subagent seam — the doc specifies how to surface it without contradicting the v1 exclusion (it becomes v-next) |
| §5 transport: transactional outbox → session-keyed revisioned snapshot+live stream; one merged session feed | Facets 1–2 ride this same stream — no new transport is proposed |
| §6 presence: header chips "CLI attached" / "UI viewing (n)"; imported turns labeled "via CLI" from `origin` | Facet 3: generalize `origin` + the executor `AgentIcon` + `subagent_type` into one identity model that presence and tabs both read |
| §3 T7: claude flushes **per step, mid-turn** (5 appends over a ~16 s run) | Facets 1–2 realtime feasibility rests on this — subagent steps and "still working" are observable at step granularity, not just turn-end |

Nothing here overrides a seamless-sync decision. Where they touch, this doc
cites the section. The subagent work presumes the seamless-sync ingester exists
(it landed: PR #39 `claude_transcript_ingest`, PR #41 concurrency).

## 3. Background — the three legibility surfaces that exist today

### 3.1 Facet 1: processing indicators today (rich for executor, invisible for CLI)

**Executor-driven runs have good, animated cues.** Run state comes from
execution-process status:

- `packages/ui/src/components/SessionChatBox.tsx:319` — `isRunning = status ===
  'running' || status === 'queued'`; `:321-325` gates `showRunningAnimation`.
- `SessionChatBox.tsx:792-796` — renders `<SpinnerIcon … animate-spin />` plus
  the in-progress todo text (a "thinking" surrogate); passed down at `:758`
  `isRunning={showRunningAnimation}` → `packages/ui/src/components/ChatBoxBase.tsx:85`
  adds the `chat-box-running` class.
- `packages/ui/src/components/WorkspaceSummary.tsx:142-150` — animated
  `<RunningDots />` (three pulsing dots, `packages/ui/src/components/RunningDots.tsx:1-9`,
  `animate-running-dot-*`) when `isRunning`.
- `packages/ui/src/components/ProcessListItem.tsx:97,112-113` — `<RunningDots />`
  per running process.
- `packages/web-core/src/pages/workspaces/CliMainPane.tsx:95-108` — a
  `CircleNotchIcon animate-spin` **but gated on executor processes only**
  (`executorRunning ? … : codingAgentRunning`), not on tmux-native activity.

**CLI ("UIX") mode has the signal but renders it as static presence.** The only
dedicated CLI cue is a badge:

- `packages/web-core/src/features/workspace-chat/ui/ConversationListContainer.tsx:1038-1051`
  — a `<Badge role="status">` with a **static** `<CircleIcon weight="fill">` (no
  animation) and text `t('conversation.cliSessionActive')` =
  **"CLI session active"** (`packages/web-core/src/i18n/locales/en/common.json:77`).
  Presence wording, not "working".
- Truthiness: `packages/web-core/src/pages/workspaces/WorkspacesLayout.tsx:72-75`
  — `cliSessionActive = isRunning && !isExecutorRunning`, from
  `packages/web-core/src/shared/hooks/useWorkspaces.ts:75-76` (`ws.is_running`,
  `ws.is_executor_running`).
- **`is_running` genuinely includes live CLI output activity:**
  `crates/db/src/models/workspace.rs:586-592` — `… OR EXISTS (SELECT 1 FROM
  workspace_cli_activity ca WHERE ca.workspace_id = w.id AND ca.state =
  'running')`, commented *"CLI-mode tmux claude actively producing output."*

So the badge is wired to the real activity state — it just doesn't *look* like
activity. The same `is_running` drives the sidebar's animated `RunningDots`
(`packages/ui/src/components/WorkspacesSidebar.tsx:153,346,463`), so CLI activity
*does* pulse — but only in the peripheral sidebar bucket, not where a UIX-mode
user is looking (the terminal pane or the conversation view).

**The stream hook has no "actively emitting" signal.**
`packages/web-core/src/shared/hooks/useJsonPatchWsStream.ts` receives patch
frames (`:123-144`), a `Ready` frame (`:147-151`), and a terminal `{finished:true}`
frame (`:155-160`), but exposes only `isConnected` / `isInitialized` / `error`
(`:244-249`). The CLI-native chat feed
(`packages/web-core/src/features/workspace-chat/model/hooks/useSessionNativeFeed.ts:63,80-81`)
surfaces `isConnected` and `isLoading`. Nothing means "a patch just arrived / the
agent is emitting right now", even though the hook is the obvious place to derive
it (a debounced "last patch < N ms ago").

### 3.2 Facet 2: subagents today (inline call+result, no realtime, no tabs)

Subagents already have exactly one surface — an inline collapsible chat row for
the `Task` tool:

- `packages/ui/src/components/ChatSubagentEntry.tsx` — header shows the subagent
  type (`:92-96` `formattedType`, capitalized, uppercase) + description
  (`:149-159`); status icon is success `CheckCircle` / error `XCircle` / pending
  `CircleNotchIcon animate-spin` (`:67-77`); expands to show the subagent's final
  output (`:170-180`). Glyph is `CpuIcon`.
- Fed from the executor stream's `task_create` tool call:
  `packages/web-core/src/features/workspace-chat/ui/DisplayConversationEntry.tsx:225-229`
  passes `subagentType={action_type.subagent_type}`.

This is **call + result**, not realtime internals: one row that spins while
pending and shows a result when done. It has no notion of the subagent's live
steps, and it is a chat row, never a tab.

The CLI/native path shows **nothing** for subagents. Sidechain content is
suppressed at two layers (§4.1). So "subagents spawned in a CLI session" — facet
2's literal ask — is currently invisible end to end.

### 3.3 Facet 3: identity today (three disconnected axes)

Agent identity is expressed three unrelated ways, none of them a tab, none of
them a shared model:

1. **Subagent name** = `subagent_type` (nullable) on a `task_create` action —
   `shared/types.ts:854` (`ActionType` variant `{ action: "task_create",
   description, subagent_type, result }`); row family
   `packages/web-core/src/features/workspace-chat/model/conversation-row-model.ts:36,151`
   (`'subagent'`).
2. **Executor badge** = `AgentIcon` keyed on the *executor* (claude/codex/…),
   `packages/web-core/src/shared/components/AgentIcon.tsx`, rendered next to
   session titles (`SessionChatBox.tsx:766,898,935`;
   `SessionChatBoxContainer.tsx:1165-1166,1220-1221`). This encodes *which tool*,
   not main-vs-subagent.
3. **Turn source** = `NativeFeedOrigin = "cli" | "app" | "executor"` —
   `shared/types.ts:367`, carried on every `NativeFeedEntry.origin` (`:371`). The
   nearest existing axis to "who produced this", but it is transport/source, not
   agent hierarchy.

The quarantine panel — **already on `origin/main`** — is the concrete instance
this doc generalizes (not re-specs):
`UnassignedCliSession` (`shared/types.ts:385`) whose only human label is
`first_prompt_snippet` (no agent name), assigned via
`AssignNativeCliSessionRequest` (`:387`); UI
`packages/web-core/src/features/workspace-chat/ui/UnassignedCliSessions.tsx:47-118`;
hook `…/model/hooks/useUnassignedCliSessions.ts:16-37`; backend
`crates/db/src/models/claude_session_link.rs`, `…/cli_native_file.rs`, route
`crates/server/src/routes/native_transcripts.rs`. (`chief/bc-cli-session-labeling`
has zero commits beyond `origin/main` at time of writing — the panel is present,
so this doc treats it as the baseline to unify, not pending work to duplicate.)

**There is no `entrypoint` field and no main-vs-subagent boolean anywhere in the
domain model** (the only `entrypoint` hits are unrelated stack-trace fixtures in
`crates/executors/src/executors/claude.rs:3274-3373`). That absence is the core
facet-3 gap.

## 4. Signals available (grounded) — what the realtime model can key off

### 4.1 The native transcript envelope and the two sidechain suppressors

The ingester adapts each native JSONL line through
`crates/executors/src/executors/claude/native.rs`. The envelope it exposes
(`NativeClaudeEnvelopeMetadata`, `native.rs:26-37`) is:

```
claude_session_id, uuid, parent_uuid, timestamp, version,
git_branch, kind, leaf_uuid, is_sidechain
```

The wire struct (`native.rs:105-124`) additionally parses `is_synthetic`
(`:120-121`) and `is_replay` (`:122-123`). **There is no `agentId` and no
`entrypoint`.** A subagent is signalled only by `is_sidechain: true`
(`native.rs:114-115`), and a record so flagged is short-circuited to
`Skip(Sidechain)` with `event: None` (`native.rs:151-157`).

Sidechain (subagent) content is suppressed at **two** layers today:

1. **File level** — the directory scan skips `agent-*.jsonl` entirely:
   `crates/services/src/services/claude_transcript_ingest.rs:635-638`
   (`file_name.starts_with("agent-")` → `continue`). Separate per-subagent
   transcript files are never even read.
2. **Record level** — inline `isSidechain: true` records in the *main*
   transcript are ingested but marked non-renderable:
   `claude_transcript_ingest.rs:831-842` maps `Skip(Sidechain)` →
   `CliNativeRecordDisposition::Sidechain`; the row is still persisted raw
   (`:847-861`, a `NewCliNativeRecord { uuid, parent_uuid, kind, disposition, …
   }`). So the data is on disk in the DB — merely hidden.

**Consequence for the design:** the record-level sidechain data is already
captured (surfacing it is a render/projection change, not new ingestion). The
file-level `agent-*.jsonl` content is *not* captured — surfacing file-based
subagents requires the scan to stop excluding them (still read-only; §5.2 of #38
watches only the slug dir, which would need to include `agent-*`).

### 4.2 Per-subagent identity — the gap and the workable key

Because there is no `agentId` field, per-subagent identity must be **derived**:

- **File-based subagents:** identity = the `agent-<id>.jsonl` filename (the `<id>`
  is claude-internal but stable per subagent within a run).
- **Inline sidechain records:** identity = the **root `parent_uuid` of a
  sidechain sub-DAG** — the first `is_sidechain` record whose `parent_uuid` points
  at a non-sidechain (main-agent) record is a spawn; its `uuid` roots that
  subagent's chain; descendants share the lineage.
- **Executor path (richest):** the `task_create` action already carries
  `subagent_type` + `description` (`shared/types.ts:854`) — a real human name and
  task. Where an executor Task call and a native sidechain sub-DAG describe the
  same subagent, the Task call is the naming source and the sidechain records are
  its live internals.

This derivation is the heart of facet 3 (§7): a single `AgentRef` computed from
whichever signals are present.

### 4.3 Spawn / active / inactive — a concrete rule

- **Spawn detected** = a new `agent-<id>.jsonl` appears in the watched slug dir,
  **or** the first `is_sidechain` record with a new root `parent_uuid` is
  appended, **or** (executor path) a `task_create` tool_use is streamed.
- **ACTIVE** = the subagent has appended a record within a short recency window
  and has not terminated. Reuse the seamless-sync cadence intuition (T7:
  per-step, seconds apart) and the existing `ACTIVE_WINDOW_SECS = 5`
  (`crates/local-deployment/src/cli_activity.rs:62-65`) as the starting constant.
  `is_synthetic` / `is_replay` records (`native.rs:120-123`) **do not** count as
  activity (backfill/replay must not light up a tab).
- **INACTIVE (done)** = the subagent's terminal signal is observed. There is a
  path asymmetry to state honestly (confirmed: the native path has **no**
  `stop_reason` — that field exists only in the executor stream types,
  `crates/executors/src/executors/claude.rs:2876,2977`):
  - *Executor path:* the `task_create` action's `result` becomes non-null / its
    tool status flips to `success|failed` — exactly what `ChatSubagentEntry`
    already keys on (`ChatSubagentEntry.tsx:59-77`).
  - *Native path:* completion is **inferred** — the parent (main-agent, non-
    sidechain) transcript resumes appending after the subagent's last sidechain
    record (the Task returned), **or** the sub-DAG has been quiet past an
    idle window while the parent is still running.
- **Proposed unified rule:** a subagent is ACTIVE from its spawn record until the
  first of {Task `result` observed, parent resumes past its last sidechain
  record, quiet > `SUBAGENT_IDLE_SECS` (propose 10 s)}. Everything else is
  INACTIVE. The exact windows are an open question (§10 Q7).

### 4.4 Facet-1 signals — already end-to-end for the fix

- **Executor:** `status ∈ {running, queued}` (`SessionChatBox.tsx:319`) — already
  animated.
- **CLI/native (coarse):** `workspace_cli_activity.state = 'running'`
  (`crates/db/src/models/workspace_cli_activity.rs:6-38`; transition function
  `cli_activity.rs:614-652`; 2 s poll `:42`, 5 s window `:62-65`) already flows to
  the frontend via `is_running` (`workspace.rs:586-592`) and `cliSessionActive`
  (`WorkspacesLayout.tsx:72-75`). **The signal exists; only its presentation is
  missing.**
- **CLI/native (fine):** a derived "actively emitting" boolean from patch-arrival
  cadence on `useJsonPatchWsStream.ts` / `useSessionNativeFeed.ts` (§3.1) — new,
  but small and local to the hook.

## 5. Design — Facet 1: the processing indicator (Lane O coarse fix + the finer model)

**Split of ownership:**

- **Lane O (`chief/bc-cli-working-indicator`), shipping now — COARSE:** promote
  the existing CLI `running` signal from the static "session exists" badge
  (`ConversationListContainer.tsx:1038-1051`) into an in-view **animated
  "working" indicator**, and mirror it on `CliMainPane`; working-vs-idle copy.
  Backend-free (§5.4). This spike does not re-spec that work.
- **This doc — FINER (assume Lane O does none of the below):** (a) an "actively
  emitting / streaming" state derived from patch-arrival cadence in the stream
  hooks; (b) **per-agent** activity — the same signal computed for each subagent,
  not just the session as a whole; (c) the composition rule with facet 2 — a
  subagent tab pulses while that subagent is working, and the header indicator can
  name the working subagent. Unify executor + CLI + per-subagent into one
  `AgentActivity` view-model so every surface renders the same component.**

### 5.1 One view-model, three states

```
AgentActivity = Idle | Working | AwaitingInput
  Working:  executor status∈{running,queued}  OR  cli_activity.state='running'
            OR  a native/normalized patch arrived < STREAM_IDLE_MS ago
  AwaitingInput: cli_activity.state='attention'  (run went quiet unattended)
  Idle:     otherwise
```

The `cli_activity.state='running'` term is exactly Lane O's coarse signal; the
patch-cadence term and the per-agent computation are this doc's additions. The
`Working` state optionally carries a **subject** (§6 ties this to facet 2): "main
agent" vs a named subagent. The indicator distinguishes them only once facet 2
ships (§10 Q5/Q9); until then `Working` is unqualified — which is precisely what
Lane O ships.

### 5.2 Where it renders (in-view, not peripheral)

- **Lane O (coarse) owns these:** replace the static
  `ConversationListContainer.tsx:1038-1051` badge's non-animated `CircleIcon`
  with an animated "working" component + reworded copy; mirror the executor
  spinner (`CliMainPane.tsx:95-108`) for tmux-native `running` so the
  terminal-first surface finally shows non-executor activity; keep the sidebar
  `RunningDots` (already correct).
- **This doc adds on top:** a compact **streaming pill** in the conversation
  header driven by the patch-cadence term (so "working" sharpens to "streaming…"
  when content is actively landing), and the **per-agent** rendering — each
  subagent's tab carries its own activity dot (§6.4), and the header indicator can
  name the working subagent rather than showing one session-wide state.

### 5.3 ASCII mockups — facet 1

Today (CLI mode), the only cue is a static dot that reads as presence:

```
┌ Conversation ──────────────────────────────────────────────┐
│  ● CLI session active                        [via CLI]      │   ← static dot,
│                                                             │     "session
│  user: refactor the auth module                            │     exists" not
│  assistant: Sure — I'll start by …                         │     "working"
│                                                             │
│  ┌───────────────────────────────────────────────────────┐ │
│  │  Type a follow-up…                                      │ │   ← composer:
│  └───────────────────────────────────────────────────────┘ │     no running
└─────────────────────────────────────────────────────────────┘     state
```

Proposed — an animated, labeled working state, in view:

```
┌ Conversation ──────────────────────────────────────────────┐
│  ◍ Working · streaming…                       [via CLI]     │   ← animated;
│                                                             │     "streaming"
│  user: refactor the auth module                            │     when patches
│  assistant: Sure — I'll start by reading auth/…            │     arriving
│  ⋯ running tool: Grep("verifyToken")                       │   ← live step
│                                                             │     (T7 cadence)
│  ┌───────────────────────────────────────────────────────┐ │
│  │  Type a follow-up…                        ◍ agent busy  │ │   ← composer
│  └───────────────────────────────────────────────────────┘ │     mirrors state
└─────────────────────────────────────────────────────────────┘
```

Mobile: the same pill sits in the CLI pane's slim header (the PR #26 header,
`CliMainPane.tsx:66-93`), icon-only when space is tight (◍) with an `aria-label`.

### 5.4 Relationship to Lane O — what it ships, what this doc keeps

The reason facet 1's coarse half could be pulled into its own lane: the backend
signal, its SSE transport, and the `is_running`/`cliSessionActive` plumbing all
exist (§4.4). Lane O's fix is therefore purely frontend — (1) swap the static
badge for an animated "working" component and reword the string; (2) mirror it on
`CliMainPane`. No schema, no new backend service, no type changes.

What Lane O does **not** do, and this doc keeps (assume none of it lands in Lane
O — it may attempt the patch-cadence boolean only as a cheap stretch):

- The **"actively emitting / streaming"** boolean derived from patch-arrival
  cadence on `useJsonPatchWsStream.ts` / `useSessionNativeFeed.ts` (§3.1) — turns
  a binary working/idle into working → streaming → idle.
- **Per-subagent** activity (§6.4) — Lane O renders one session-wide state; the
  finer model computes activity per `AgentRef` so each tab pulses independently.
- The **composition rule** with facet 2 (§6.2, §10 Q9) — a tab pulses while its
  subagent works, and the header indicator names the working subagent.

So Lane O is Phase A's coarse first step; §9 Phase A below is scoped to only the
finer remainder.

## 6. Design — Facet 2: realtime subagent tabs + inactive drawer

**Recommendation: a session-scoped "agent activity strip" that lives inside the
conversation/CLI pane (never the global nav). ACTIVE subagents render as tabs;
INACTIVE ones collapse behind a single "N done ▾" button that opens a drawer.
The strip is bounded by construction and degrades to the existing inline
`ChatSubagentEntry` rows on mobile.**

### 6.1 Placement — a local strip, not the global tab bar

The only horizontal tab strip today is the **global mobile nav**
(`Navbar.tsx:97-114,251-294`), a static 7-tab set already at its width ceiling
(§8). Desktop has no tab strip at all (`Navbar.tsx:363-418` renders icon-only
toggles + `RightSidebar.tsx`). Subagent tabs therefore **cannot** join the global
nav — they get their own **session-scoped strip** rendered at the top of the
conversation pane (and mirrored in the CLI pane header), owned by the active
session, disappearing when no subagents are active.

This also matches the data's scope: subagents belong to one conversation, not to
the whole app.

### 6.2 Active-as-tabs, inactive-behind-a-button (the owner's nuance)

- The strip shows a tab **only** for subagents currently `ACTIVE` (§4.3).
  Typical count is 0–2; parallel fan-out can spike higher.
- A finished subagent's tab **collapses into a trailing "N done ▾" button**
  (whether it lingers briefly first is §10 Q3). The button opens a drawer listing
  all inactive subagents for the session, each expandable to its transcript /
  result (reusing `ChatSubagentEntry`'s expand).
- A leading "main" chip always anchors the strip (facet 3), so the user can
  always return focus to the main agent.

### 6.3 Bounding the active-tab count

Active subagents are few and short-lived, so the strip is self-bounding in
practice. For the pathological fan-out, overflow is explicit rather than
infinite scroll: show up to `MAX_ACTIVE_TABS` (propose 4) tabs; beyond that,
extra active subagents also fold into the "N done ▾" button relabeled "N more ▾"
(count of not-shown active + inactive). This reuses one overflow affordance for
both cases and never lets the strip grow unbounded — directly addressing the fact
that **no overflow menu exists today** (§8).

### 6.4 ASCII mockups — facet 2

Desktop, two subagents running under the main agent:

```
┌ Conversation ──────────────────────────────────────────────────────┐
│ ⌂ Main ·◍  │ ⌕ Explore ·◍ │ ⚙ code-review ·◍ │            2 done ▾ │  ← strip:
│─────────────────────────────────────────────────────────────────────│    active
│                                                                     │    = tabs,
│  [Explore] scanning packages/… (14 files)                           │    inactive
│  ⋯ running tool: Grep("MOBILE_TABS")                                │    = button
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
   ◍ = animated working dot per tab (facet 1 signal, per-agent)
```

The "2 done ▾" drawer:

```
        ┌ Inactive subagents (this session) ─────────────┐
        │  ✓ Explore      "map tab strip"      1m ago  ▸ │  ← expand → result
        │  ✓ general      "find PR #42"        3m ago  ▸ │
        │  ✗ code-review  "review diff" (failed) 4m ago ▸│
        └─────────────────────────────────────────────────┘
```

Mobile — the global 7-tab nav is untouched; the strip degrades to a single
collapsible summary line inside the conversation pane, and the *inline*
`ChatSubagentEntry` rows (which already exist) remain the primary reading
surface:

```
┌ Conversation (mobile) ───────────────────┐
│ ⌂ Main ·◍   2 active · 3 done  ▾          │  ← one line; tap to expand a sheet
│───────────────────────────────────────────│     listing active + inactive.
│  [Explore] scanning packages/…            │     No horizontal tab scroll on
│  ⋯ Grep("MOBILE_TABS")                    │     mobile — avoids the §8 crowding.
│                                           │
│  ▸ code-review (running)                  │  ← inline rows unchanged
└───────────────────────────────────────────┘
```

### 6.5 Data flow (read-only projection over existing rows)

- **[IMPLEMENTED — Lane P, §0] Executor subagents:** already present as
  `task_create` actions; the strip is a new *projection/aggregation* over the same
  conversation rows the inline entries use (`conversation-row-model.ts:151`, family
  `'subagent'`) — one descriptor per `task_create` row, active/done computed from
  its `ToolStatus`. No new ingestion. This is the shipped path.
- **[DEFERRED — out of scope, §0] Native (CLI) subagents:** would require un-hiding
  the record-level sidechain rows (already persisted,
  `claude_transcript_ingest.rs:847-861`) via a new render projection, and — for
  file-based subagents — letting the scan read `agent-*.jsonl` (§4.1). Both are
  read-only. Owner Decision 1 puts this out of scope: a terminal `claude` already
  shows its own subagents. Not built.
- **Transport:** the seamless-sync merged session feed (#38 §5) already carries
  these rows; the strip subscribes to the same stream. No new SSE channel.

## 7. Design — Facet 3: one generalized agent-identity model

**Recommendation: introduce a single derived `AgentRef` that unifies the three
disconnected axes (subagent_type, executor `AgentIcon`, `NativeFeedOrigin`) plus
a main-vs-subagent role, and render it consistently across turns, the subagent
strip/tabs, and presence chips. This is a view-model unification, not a schema
migration.**

### 7.1 The `AgentRef` shape (conceptual — no type change proposed here)

```
AgentRef {
  role:      'main' | 'subagent'          # derived: is_sidechain / task_create vs not
  name:      string                        # subagent_type ▸ agent-<id> ▸ "Main"
  executor:  Executor                      # existing AgentIcon key (claude/codex/…)
  origin:    'cli' | 'app' | 'executor'    # existing NativeFeedOrigin
  activity:  Idle | Working | AwaitingInput # facet 1 view-model, per agent
}
```

- `role` fills the current void — no `entrypoint`/main-vs-subagent flag exists
  today (§3.3); it is derived from `is_sidechain` (native) or `task_create`
  (executor).
- `name` prefers the human `subagent_type` (`shared/types.ts:854`), falls back to
  the `agent-<id>` filename, then "Main".
- `executor` reuses `AgentIcon` (`AgentIcon.tsx`) unchanged — identity of *tool*.
- `origin` reuses `NativeFeedOrigin` (`shared/types.ts:367`) unchanged — identity
  of *surface* ("via CLI" already planned in #38 §6).

### 7.2 Where `AgentRef` renders (one model, many surfaces)

- **Turn attribution:** each conversation entry shows `role`+`name` (generalizing
  today's ad-hoc `subagent_type` row and the "via CLI" label into one badge).
- **Subagent tabs (facet 2):** each tab *is* an `AgentRef` with `role:'subagent'`;
  the leading chip is the `role:'main'` ref.
- **Presence chips (#38 §6):** "CLI attached" / "UI viewing (n)" become
  `AgentRef`-shaped, so the same component renders human presence and agent
  presence consistently.
- **Quarantine panel (existing):** `UnassignedCliSession` gains a place to show a
  derived name instead of only `first_prompt_snippet` — **without re-specing the
  panel**; it simply consumes `AgentRef` when a binding exists. This is the
  "generalize, don't re-spec" boundary the brief drew (§3.3).

### 7.3 Explicit boundary vs lanes L/M

This doc **does not** design the quarantine/labeling panel — that is on
`origin/main` and owned by the concurrent lanes. Facet 3 only defines the shared
identity view-model those surfaces (and facets 1–2) can all read, so labels stop
being invented three different ways. If lane M lands a first-class role/label
field, `AgentRef.role`/`name` bind to it instead of deriving — the model is
forward-compatible with that.

## 8. The crowded-UI constraint (first-class)

This is a hard constraint, not a footnote. The mobile nav already fought for
horizontal room and won by **dropping labels**:

- 7 fixed tabs (`Navbar.tsx:97-104`: workspaces, chat, changes, logs, preview,
  git, files), mapped at `:265-294` in a `flex … overflow-x-auto` row (`:251`).
- **PR #42** (commit `8f153c7c`, "add Files as a 7th mobile tab, active-only tab
  labels") made only the active tab show its text label:
  `Navbar.tsx:289` — `<span className={isActive ? 'inline' : 'hidden'}>{tab.label}</span>`.
  This *replaced* the earlier width-driven `hidden min-[480px]:inline`; adding the
  7th tab overflowed that, so labels became active-state-driven. Accessible names
  survive via `aria-label`/`aria-current` (`:273-276`) and active state also shows
  in icon weight (`:287`).
- **There is no overflow menu, no "+N", no dropdown** — only horizontal scroll
  (`overflow-x-auto`) and the active-only-label shrink. The tab set is static
  (7 mobile / 6 remote — `NavbarContainer.tsx` filters `files` out on remote), so
  nothing today collapses a *dynamic* tab count. (The generic multi-terminal
  `TerminalPanel.tsx:14` likewise `tabs.map`s with no overflow handling.)
- Mobile is `max-width: 767` (`packages/web-core/src/shared/hooks/useIsMobile.ts:3`,
  `matchMedia`); Tailwind `md:` (≥768) is its intentional complement
  (documented `CliMainPane.tsx:68-69`).
- Footnote for implementers: `cn()` is bare `clsx` with **no** tailwind-merge
  (`packages/ui/src/lib/cn.ts:3-6`) — `cn("hidden", isActive && "inline")` emits
  both and `.hidden` wins; PR #42 hit exactly this. Any conditional
  show/hide in the new strip must use a ternary, not two conflicting utilities.

**How the design honors it:**

1. **Subagent tabs never enter the global nav.** They are session-scoped and live
   in the conversation/CLI pane (§6.1). The 7-tab ceiling is untouched.
2. **Bounded active count + one overflow affordance.** `MAX_ACTIVE_TABS` + the
   "N more ▾" button (§6.3) supplies the collapse behavior the codebase lacks,
   scoped to the new strip only.
3. **Mobile degrades to a single summary line + existing inline rows** (§6.4) — no
   horizontal tab scroll is introduced on mobile at all. Facet 1's indicator is a
   single pill that goes icon-only under pressure, exactly like the nav's
   active-only-label trick.

## 9. Phasing & risk

**Phase A₀ — coarse working indicator = Lane O (`chief/bc-cli-working-indicator`),
in progress, not owned here.** Animate + reword the CLI activity badge; mirror on
`CliMainPane`. Listed only to place the ordering.

**Phase A — Facet 1 finer remainder (S, risk: low).** The parts Lane O does not
do: patch-cadence "streaming" boolean in the stream hooks; per-agent activity
computation so the signal can attach to individual subagents (feeds Phase C).
No schema/type changes beyond an optional derived boolean.
*Verification:* in a CLI session the header sharpens from "working" to
"streaming…" while patches land and settles to idle within one debounce of the
last patch; executor runs unchanged; mobile shows the icon-only pill; the
per-agent signal lights the right subagent (checked once Phase C lands).

**Phase B — Facet 3 `AgentRef` view-model (S–M, risk: low).** Derive `AgentRef`;
render unified turn attribution + presence chips. No migration.
*Verification:* a mixed conversation (executor turn, CLI turn, a subagent) labels
each turn's role/name/origin consistently; quarantine panel shows a derived name
where a binding exists.

**Phase C — Facet 2 subagent strip (M, risk: medium — un-hides sidechain data).**
Render projection over `task_create` + un-hidden native sidechain rows; active
tabs + inactive drawer + overflow; wire per-tab activity to facet 1.
*Verification:* spawn 3 subagents in a CLI run → up to `MAX_ACTIVE_TABS` tabs
appear live (step cadence, T7), finished ones move to the drawer; `is_synthetic`
backfill never lights a tab; mobile shows the summary line, global nav unaffected;
replay a transcript with an `agent-*.jsonl` fan-out → strip renders, nothing
crashes.

Ordering rationale: A is the urgent, isolated win; B is the cheap unifier C
depends on for labels; C is the largest and rides on both.

## 10. Open questions — decisions for the owner (the point of this doc)

Each with a recommendation; nothing proceeds until you approve, item by item.

> **DECIDED (Lane P, see §0) for the shipped facet-2 slice:** **Q2** surface →
> session-scoped strip in the conversation pane (yes). **Q3** → (b) ~5 s linger.
> **Q4** → `MAX_ACTIVE_TABS = 4` + "N more ▾" overflow. **Q5** → (a) per-tab pulse
> yes; (b) header naming deferred with facet 1. **Q8** → executor path only for
> now (native/CLI out of scope per Decision 1). **Q9** → generic "Subagent"
> fallback. **Q6** (native `agent-*.jsonl` / sidechain un-hiding) is **not** in
> scope. Q1, Q7, Q10, Q11 remain open for the deferred facets.

1. **Facet-1 coarse fix — DECIDED (Lane O).** The coarse "is the agent working"
   indicator is already its own lane (`chief/bc-cli-working-indicator`), scoped to
   animating the existing `is_running` signal in view + on `CliMainPane`. The open
   remainder for you: **is the finer "streaming…" state (patch-cadence) worth
   building at all, or is coarse working/idle enough?** Rec: **build the finer
   state** — it is small (§5.4) and it is what makes mid-turn tool steps (T7) feel
   live rather than frozen; but it is genuinely optional and can wait behind
   facets 2–3.
2. **Where do subagent tabs live?** Rec: **session-scoped strip in the
   conversation/CLI pane, never the global nav** (§6.1). Approve the surface
   before C starts.
3. **Do finished-subagent tabs linger?** Options: (a) collapse to the drawer
   immediately on completion; (b) linger `N` seconds then collapse; (c) stay a tab
   until the whole turn ends. Rec: **(b), ~5 s linger** so a fast subagent is still
   noticed, then collapse — keeps the strip honest to "active".
4. **How many active tabs before overflow?** Rec: **`MAX_ACTIVE_TABS = 4`**, extras
   fold into the "N more ▾" button (§6.3). Tune to taste.
5. **How does the processing indicator compose with the subagent tabs?** Two
   linked sub-decisions: (a) **Does each tab pulse on its own** while that
   subagent is working? Rec: **yes** — per-agent activity (§5.4, §6.4) makes each
   tab carry its own dot, which is the whole point of "tracking in realtime". (b)
   **Does the header indicator name the working subagent** ("Explore working…")
   vs staying unqualified like Lane O's coarse state? Rec: **name it once facet 2
   ships** (§5.1, §6.2); unqualified until then. (Alternative for both: one
   session-wide state only — simpler, but loses the per-subagent legibility the
   owner asked for.)
6. **File-based subagents (`agent-*.jsonl`) in scope, or inline-sidechain only?**
   Un-hiding inline records is nearly free (already persisted); reading `agent-*`
   files reverses a deliberate #38 v1 exclusion and widens the watch set. Rec:
   **inline sidechain records first (Phase C), `agent-*` files as a follow-up** once
   the strip proves out.
7. **Active/idle windows for subagents.** Rec: reuse `ACTIVE_WINDOW_SECS = 5` for
   "still working" and propose `SUBAGENT_IDLE_SECS = 10` for "gone quiet"; confirm
   or tune against a live fan-out (§4.3).
8. **Should the indicator/tabs also cover executor (non-CLI) runs, or CLI only?**
   The owner framed it as "UIX mode", but the `AgentRef`/activity model is
   uniform. Rec: **uniform** — one component for both; costs nothing extra and
   keeps executor and CLI legibility consistent.
9. **Naming an unnamed subagent.** Native sidechains have no `subagent_type`;
   only `agent-<id>`. Rec: show a **generic "Subagent" + short id**, upgraded to the
   real name if an executor `task_create` correlates. (Alternative: hide unnamed
   ones from tabs, show only in the drawer.)
10. **Presence unification depth.** Rec: make #38's "CLI attached"/"UI viewing (n)"
    chips `AgentRef`-shaped in Phase B so human and agent presence share one
    component (§7.2). (Alternative: keep them separate; less consistent.)
11. **Mobile subagent affordance.** Rec: **summary line + expandable sheet + keep
    inline rows** (§6.4); explicitly **no** horizontal subagent-tab scroll on
    mobile (§8). Confirm you're happy trading tab fidelity for the crowding
    constraint on small screens.

## 11. Non-goals / deferred (scope honesty)

- **No implementation.** This lane ships only this document.
- **The coarse working indicator itself** — owned by **Lane O**
  (`chief/bc-cli-working-indicator`), not this doc. Facet 1 here is the finer
  remainder + the model it fits into (§1.1, §5).
- **Token/keystroke-level streaming.** Realtime is step/block-level (#38 C5); the
  indicator means "working", not a live token stream.
- **Re-specing the quarantine/session-labeling panel** (lanes L/M own it, already
  on main). Facet 3 only defines the shared `AgentRef` those surfaces can read.
- **A first-class `entrypoint`/role column.** The design *derives* role; adding a
  persisted field is a separate decision (forward-compatible, §7.3).
- **Non-claude subagents** (codex/gemini fan-out) — their stores differ; a future
  spike, mirroring #38's non-claude deferral.
- **Reversing #38's `agent-*.jsonl` v1 exclusion by default** — gated behind Q6;
  not assumed.
- **Desktop tab-strip parity for the global nav.** Desktop keeps its icon
  toggles + `RightSidebar`; the subagent strip is the only new tabbed surface, and
  it is session-scoped.
- **Notifications** ("a subagent finished" toast/badge) — reuse #38's
  attention plumbing later if wanted; not designed here.
