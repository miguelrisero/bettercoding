# CLI Mode: tmux-backed interactive `claude` as the main workspace pane

## Why

Programmatic Claude Code use (`claude -p` + stream-json — the current executor
path) moves off subscription limits onto a metered Agent SDK credit pool on
**2026-06-15**, then standard API rates. Interactive terminal sessions remain
covered by Pro/Max subscriptions. CLI mode gives each workspace a terminal-first
main pane running the interactive `claude` TUI inside a persistent tmux session,
keeping VibeKanban's orchestration value (kanban, worktrees, diffs, git, PRs)
while the agent surface becomes the native CLI.

CLI mode is **additive**: the chat/executor path is untouched and remains the
default. Diffs and git panels read from git state, and kanban status from
process lifecycle — none depend on the chat's normalized logs, so they keep
working alongside CLI mode.

## Architecture

### Today (verified)

- `crates/local-deployment/src/pty.rs` — `PtyService::create_session(working_dir, cols, rows)`
  spawns `get_interactive_shell()` on a fresh PTY per WebSocket connection;
  the session dies with the connection (`terminal.rs:165`). Sole caller:
  `crates/server/src/routes/terminal.rs:104`.
- `crates/server/src/routes/terminal.rs` — `GET /api/terminal/ws?workspace_id&cols&rows`
  resolves the worktree dir from `workspace.container_ref`
  (+ `/<repo_name>` when the workspace has exactly one repo).
- Frontend: `XTermInstance.tsx` builds the WS endpoint; `TerminalProvider.tsx`
  owns tabs/connections (reconnect w/ backoff); per-workspace panel state lives
  in `useUiPreferencesStore.ts` (`WorkspacePanelState`); the left-main pane is
  `WorkspacesMainContainer` inside `WorkspacesLayout.tsx`.

### Change

#### Backend

1. `PtyService::create_session` gains a `command: PtyCommand` parameter:
   - `PtyCommand::Shell` — existing behavior (default; side terminal unchanged).
   - `PtyCommand::TmuxCli { session_name }` (working dir stays a
     `create_session` parameter) — spawn
     `tmux new-session -A -s <session_name> -c <working_dir> <bootstrap>`
     on the PTY. `-A` = attach if the session exists, create otherwise —
     this is the whole reattach mechanism; reconnects attach, never respawn.
   - Bootstrap (initial window command, ignored by `-A` when attaching):
     `command -v claude >/dev/null 2>&1 && claude; exec "${SHELL:-/bin/sh}"`
     — runs the interactive `claude`; when it exits (or isn't installed) the
     pane drops to a shell instead of killing the tmux session.
2. tmux availability probe (`tmux -V`, checked once, cached): if tmux is
   missing (or on Windows), CLI mode silently falls back to `PtyCommand::Shell`
   in the same worktree — degraded but functional.
3. `terminal.rs`: query gains `mode` (`shell` default | `cli`). For `cli`,
   derive `session_name = format!("vk_{}", workspace_id.simple())`
   (simple = no hyphens; tmux names must avoid `.`/`:`).
4. Workspace deletion: best-effort `tmux kill-session -t vk_<id>` so sessions
   don't orphan past their workspace. Only ever target the `vk_` namespace.

#### Frontend

1. `WorkspacePanelState` gains `mainPaneMode: 'chat' | 'cli'` (default
   `'chat'`), with `setMainPaneMode` exposed via `useWorkspacePanelState`.
2. `WorkspacesLayout.tsx`: left-main panel renders `WorkspacesMainContainer`
   (chat) or a new `CliMainPane` (an `XTermInstance` with `mode="cli"`,
   header with mode-switch back to chat). Toggle button in the workspace nav.
3. `XTermInstance` accepts `mode?: 'shell' | 'cli'` and appends `&mode=cli`
   to the endpoint. CLI tab uses a stable per-workspace tab id
   (`cli-<workspaceId>`) so the provider reuses the connection across
   navigation.

### Lifecycle (the self-healing matrix)

| Event | tmux client (PTY child) | tmux session (server) | UX |
|---|---|---|---|
| WS disconnect / tab close / navigate away | killed via PTY teardown | survives (detached) | reattach on next open, scrollback intact |
| VibeKanban backend restart | killed | survives | reattach on next open |
| `claude` exits or crashes | n/a | survives | pane drops to `$SHELL`; `claude --continue` resumes the conversation (sessions are keyed by cwd = the worktree, so `--continue` is deterministic per workspace) |
| Machine reboot | gone | gone (tmux server dies) | fresh session on next open; `claude --continue` restores the conversation |
| Workspace deleted | n/a | `kill-session` best-effort | none |

Note: `--session-id` does NOT pin interactive session UUIDs (verified against
current docs) — cwd-keyed `--continue` is the resume mechanism.

### Environment & secrets (intentional trade-off)

The tmux server and the `claude` it runs inherit the backend process
environment — identical to the existing bare-shell terminal, which already
spawns an interactive shell with the same env in the same worktree. The new
factor is **longevity**: the CLI tmux session can outlive a backend restart,
so an inherited secret (e.g. `ANTHROPIC_API_KEY`) persists in a detached
process until the workspace is deleted (`kill_cli_tmux_session`) or the socket
server is killed. Mitigations applied: a **dedicated tmux socket**
(`tmux -L vibe-kanban`) isolates our server from the user's personal tmux and
makes the long-lived server unambiguously ours; cleanup is namespaced and
exact-matched. A curated env allowlist was deliberately **not** applied — it
risks breaking interactive subscription auth (which the feature depends on),
and the exposure is local-only and equivalent to the pre-existing terminal.
Per-secret scrubbing should ride the executor-profile integration (below).

### Out of scope (MVP)

- Auto-resuming `claude --continue` inside the bootstrap (ambiguous on first
  run and after intentional exits; manual resume is one short command).
- Remote/cloud deployments (local-deployment only).
- Multi-window tmux management, status-bar config, mouse-mode tuning.
- Replacing the executor path — chat mode remains default and fully working.
- **Startup orphan sweep** of `vk_` sessions whose workspace no longer exists
  (e.g. after a backend crash mid-delete, or manual worktree removal). Bounded
  today by per-delete cleanup + the dedicated socket (one `kill-server`
  reclaims all); a periodic/startup reconciliation is a clean follow-up.
- **Executor-profile integration** for the bootstrap (model/flags/alternate
  agent, and per-secret env scrubbing) — see the `CLI_BOOTSTRAP` TODO.

## Test plan

Live, against the dev stack (`pnpm run dev`):
(a) switch a workspace to CLI mode → interactive `claude` runs in the pane;
(b) reload the browser → the SAME tmux session reattaches with scrollback;
(c) restart the backend server → session survives, reattaches;
(d) kill `claude` → pane drops to shell; `claude --continue` resumes the
conversation. Plus gates: fmt, clippy `-D warnings`, `cargo test` (touched
crates), `pnpm run check`, `pnpm run lint`.
