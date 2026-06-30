# CLI-mode multi-agent support + agentic-loop automation

Date: 2026-06-30 · Branch: `mr/1ac7-agent-retry-schedule-cli`

Two additive features for BetterCoding (the managed/UIX executors are **untouched**):

1. **Loop automation** — keep agentic loops running when a *chat stops for a non-completion
   reason* (usage limit / transient rate limit): detect the limit banner, schedule a
   wake-up at the reset time (or retry every N min), and re-prompt the agent.
2. **Multi-agent CLI mode** — the persistent tmux-backed CLI pane currently only runs
   `claude`. Generalize it so every supported agent (codex first, then gemini, opencode,
   cursor, droid, amp, qwen, copilot) can run interactively in the pane, reusing the same
   per-agent settings the managed UIX already exposes.

## Approved decisions

- Automation is **CLI-first** (tmux panes); headless/managed is a documented extension point.
- Detection is **auto**, but the loop is **opt-in per workspace, default OFF** (global default OFF).
- **All 8** agents via one generic abstraction; codex built + tested live now, the rest staged
  with code + tests + an exact install/e2e plan awaiting approval before any install.
- **Autonomous by default** in CLI (worktrees are app-created/isolated): claude
  `--dangerously-skip-permissions`, codex `--sandbox danger-full-access --ask-for-approval never`,
  etc. Supervised/Plan still map through so it can be dialed back per workspace.
- Policy lives at the **workspace** level (1:1 with the tmux session) + a global default;
  "per project" = enabling it across a project's workspaces (no `workspace→project` FK exists).

## Key code facts (from exploration)

- CLI launch is Claude-hardwired in 3 spots: `cli_bootstrap` literal `claude …`
  (`crates/local-deployment/src/pty.rs:84`), `start_workspace_cli` hardcodes
  `BaseCodingAgent::ClaudeCode` (`crates/services/src/services/container.rs:1097`), and
  `terminal.rs` only calls `claude::interactive_cli_args` (`crates/server/src/routes/terminal.rs:265`).
- `PtyCommand::TmuxCli.agent_args: Vec<String>` is already a generic seam (`pty.rs:27-44`);
  there's a `TODO(profile-integration)` at `pty.rs:69`.
- The create flow already produces an `ExecutorConfig` for ANY agent
  (`CreateChatBoxContainer.tsx`); it just isn't honored in the CLI pane.
- `StandardCodingAgentExecutor` trait + `#[enum_dispatch(CodingAgent)]`
  (`crates/executors/src/executors/mod.rs:220`). Each `CodingAgent` variant wraps that agent's
  config struct; `apply_overrides` already maps the unified `ExecutorConfig`
  (`model_id`/`reasoning_id`/`permission_policy`) onto agent fields.
- Codex managed mode runs `codex app-server` (JSON-RPC) — the TUI is greenfield. Real codex 0.140
  surface (captured live): `codex [PROMPT]`, `codex resume <SESSION_ID> [PROMPT] --last`,
  `-m/--model`, `-s/--sandbox <read-only|workspace-write|danger-full-access>`,
  `-a/--ask-for-approval <untrusted|on-failure|on-request|never>`, `-C/--cd <DIR>`,
  `-c model_reasoning_effort="high"`, `--no-alt-screen` (inline mode → clean tmux scrollback).
- No scheduler exists; background loops are `tokio::spawn`+`interval`. Templates: reaper
  (`crates/local-deployment/src/container.rs:409`, 30 min), PR monitor (`select!{interval, Notify}`,
  `crates/services/src/services/pr_monitor.rs:86`), cli_activity (2 s, `crates/local-deployment/src/cli_activity.rs`).
  Spawned from `LocalDeployment::new()` (`crates/local-deployment/src/lib.rs:265`).
- CLI pane output is **ephemeral** (PTY→WS, never persisted): detection needs a new
  `tmux capture-pane -p` poll; live re-prompt needs a new `tmux send-keys` helper. Neither exists.
- Headless extension hooks: Claude parses `ClaudeJson::RateLimitEvent` but **drops** it
  (`crates/executors/src/executors/claude.rs:2152`); `finalize_task`
  (`crates/services/src/services/container.rs:238`) is the completion hook; retries reuse
  `CodingAgentFollowUpRequest`. Codex has `account/rateLimits/read` (`codex/client.rs:293`).
- Session carries the agent + CLI selections: `Session.executor`, `pending_cli_prompt`,
  `cli_model_id`, `cli_reasoning_id` (`crates/db/src/models/session.rs`).
- Notifications exist: `NotificationService.notify(title, msg, workspace_id)`
  (`crates/services/src/services/notification.rs`).
- Types → TS: `crates/server/src/bin/generate_types.rs` → `shared/types.ts`; per-agent RJSF
  schemas via `virtual:executor-schemas`. Frontend API client: `packages/web-core/src/shared/lib/api.ts`.

## Part 2 — Multi-agent CLI mode

### Abstraction (executors crate)
New types + trait methods (default = no CLI support, so agents opt in):
```rust
pub struct CliLaunchSpec {
    pub program: String,            // binary gated on `command -v` and exec'd
    pub base_args: Vec<String>,     // model/effort/sandbox/approval/cwd/autonomy flags
    pub resume: CliResume,          // Flag("--resume") | Subcommand("resume") | Unsupported
    pub prompt_arg: CliPromptArg,   // Positional | PositionalAfterResume | Unsupported
    pub extra_env: Vec<(String,String)>, // e.g. skip-onboarding env
}
enum CliResume { Flag(String), Subcommand(String), Unsupported }
enum CliPromptArg { Positional, Unsupported }

// on StandardCodingAgentExecutor:
fn interactive_cli_spec(&self, cwd: &Path) -> Option<CliLaunchSpec> { None }
fn pre_cli_launch(&self, _cwd: &Path) {}   // trust-seeding etc.
```
- `ClaudeCode`: refactor existing `interactive_cli_args` + `ensure_claude_folder_trusted` behind
  these (no behavior change; keep `--model/--effort`, `--dangerously-skip-permissions`,
  `--resume <uuid>`, prompt positional).
- `Codex`: build from `Codex` config — `-m <model>` (strip `-fast`), `-c model_reasoning_effort=<e>`,
  `-s <sandbox>` (default danger-full-access), `-a <approval>` (Auto→never), `-C <cwd>`,
  `--no-alt-screen`; resume = `Subcommand("resume")` (`codex resume <id> [prompt]`).

### Plumbing
- `pty.rs`: `cli_bootstrap` takes a resolved spec (program + flags + resume/prompt form) instead of
  the `claude` literal; keep shell-quoting, `command -v <program>` gate, `exec $SHELL` fallback.
  `PtyCommand::TmuxCli` carries `program` + `resume_kind` + `prompt_kind` (+ existing `agent_args`).
- `services/container.rs:start_workspace_cli`: use the workspace's selected executor, not `ClaudeCode`.
- `server/routes/terminal.rs`: resolve `ExecutorConfig` from `Session.executor` + CLI model/effort,
  instantiate the `CodingAgent`, `apply_overrides`, call `interactive_cli_spec`; fall back to claude
  default if the agent has no spec.
- Frontend: CLI pane header shows the running agent (label from `Session.executor`).

### Tests
- Unit: `interactive_cli_spec` argv for representative configs per agent (mirror claude's
  `cli_launch_tests`).
- Codex live e2e (installed 0.140): create CLI workspace w/ codex, verify tmux launch, model/effort/
  sandbox honored, resume, no approval stall.
- Other 7: unit tests now; e2e checklist gated on install approval.

## Part 1 — Loop automation

### Data model (new migration)
- `loop_automation`: `workspace_id` PK, `enabled` (def false), `retry_interval_secs` (def 600),
  `continuation_prompt` (def "Continue."), `max_attempts` (def e.g. 50), `attempts_used`, timestamps.
- `scheduled_wakeup`: `id`, `workspace_id`, `fire_at`, `kind` (RateLimitRetry|UsageLimitWake|Manual),
  `prompt`, `recurring_cron` NULL, `fired_at` NULL, `attempt`, `created_at`.
- Global defaults in `Config`.

### Loop supervisor (new periodic task, local-deployment)
PR-monitor shape (`select!{ interval.tick(), notify.notified() }`), spawned from `LocalDeployment::new()`.
Each tick:
1. **Detect** — for enabled workspaces, `tmux capture-pane -p -t vk_<id>` (new helper) and scan the
   tail: transient rate-limit banner → schedule `RateLimitRetry` at now+interval (dedupe);
   usage-limit banner → parse reset time → schedule `UsageLimitWake` at that instant (next-window /
   next-day fallback). Reuse cli_activity Running/Idle so a busy agent is never interrupted.
2. **Fire** — for due wakeups with an idle agent: `tmux send-keys -t vk_<id> -l '<prompt>'` then
   `Enter` (new helper). If the session died, re-park via `set_pending_cli_prompt`. Mark `fired_at`,
   bump attempts, respect cap, fire a notification.

Banner patterns (case-insensitive), centralized + unit-tested:
- rate-limit: `temporarily limiting requests`, `· Rate limited`, `Rate limited`.
- usage-limit: Claude `usage limit reached` / `resets at <time>` / `5-hour`; parse the timestamp.

### API + UI
- Endpoints: get/set `loop_automation` per workspace; create/list/delete `scheduled_wakeup`.
- UI: per-workspace "Keep going" toggle + interval + next-wakeup status in `CliMainPane` header and
  chat `ContextBarContainer`. Default off. Shows "retrying at HH:MM" / "waking at 05:00 UTC".

### Safety
Default OFF · max-attempt cap · idle-only re-prompt · notification per auto-resume ·
`DISABLE_LOOP_AUTOMATION` kill-switch env (matches `DISABLE_CLI_SESSION_REAP`).

## Phase order
1. Part-2 core abstraction + Claude refactor (no behavior change).
2. Codex CLI mode + live e2e.
3. Part-1 backend (migration → helpers → supervisor → detection → delivery → API → tests).
4. Frontend (types → CLI agent label → loop controls → API hooks).
5. Remaining 6 agents staged + per-agent install/e2e plan.
6. format / lint / check / `cargo test --workspace` / PR vs main.

## Out of scope (v1)
- Headless/managed auto-retry (hooks identified, deferred).
- First-class per-project policy object (derive from per-workspace).
- Agent-specific niche flags beyond model/effort/permission/sandbox/MCP.
