<h1 align="center">
  <code>Better<span>Coding</span></code>
</h1>

<p align="center"><strong>A local-first workspace manager for AI coding agents.</strong></p>
<p align="center">Plan work, run agents in isolated git worktrees, review diffs, and ship pull requests — from one interface.</p>

---

BetterCoding is a hard fork of [Vibe Kanban](https://github.com/BloopAI/vibe-kanban) (sunset by Bloop AI), rebuilt around a single idea: **everything that matters runs locally**. The cloud kanban, project management, and export features that depended on the discontinued hosted service have been removed. What remains is a focused tool for working with coding agents day to day.

## Features

- **Workspaces** — each task gets an isolated git worktree with its own branch, agent session, terminal, and dev server. Work on many things in parallel without stepping on yourself.
- **CLI mode** — turn the main pane into a persistent, tmux-backed interactive `claude` session. Survives page reloads, container restarts, and WebSocket drops; resumes the exact conversation from the chat UI and hands it back (`claude --resume` under the hood). Interactive terminal use stays covered by your subscription, unlike headless/API usage.
- **Chat-driven agent sessions** — drive Claude Code, Codex, Gemini CLI, GitHub Copilot, Amp, Cursor, OpenCode, Droid, CCR, or Qwen Code through a structured chat with full tool-call visibility.
- **Diff review** — side-by-side or inline diffs with inline comments that go straight back to the agent.
- **Preview browser** — built-in preview with devtools, inspect mode, and device emulation for testing what the agent built.
- **Git integration** — create branches, rebase, resolve conflicts, open PRs with generated descriptions, and merge without leaving the app.
- **Remote hosts (optional)** — pair other machines over a relay and drive their workspaces from one UI.

## Quick start

Authenticate with your coding agent of choice first (e.g. `claude` for Claude Code). Then:

```bash
pnpm i
pnpm run dev
```

This starts the backend and the web app with auto-assigned ports; a blank database is seeded from `dev_assets_seed/`.

### Production build

```bash
pnpm run build:npx     # build the server binary with the embedded frontend
cd npx-cli && pnpm pack
```

The packed CLI extracts and runs the server binary; point your browser at the printed URL.

## Architecture

| Layer | Tech | Where |
|-------|------|-------|
| Backend | Rust (axum, sqlx/SQLite, ts-rs) | `crates/` — `server`, `db`, `executors`, `services`, `local-deployment`, `git`, `utils` |
| Web app | React + TypeScript (Vite, Tailwind, TanStack Router/Query) | `packages/local-web` (entrypoint), `packages/web-core` (shared library), `packages/ui` (component library) |
| Shared types | Generated from Rust via ts-rs | `shared/types.ts` (run `pnpm run generate-types`, never edit by hand) |
| Agent executors | Per-agent protocol adapters | `crates/executors` |
| Remote (optional) | Postgres + ElectricSQL | `crates/remote`, `packages/remote-web` — self-hostable relay/host infrastructure |

## Development

### Prerequisites

- [Rust](https://rustup.rs/) (latest stable)
- [Node.js](https://nodejs.org/) (>= 20)
- [pnpm](https://pnpm.io/) (>= 8)

```bash
cargo install cargo-watch sqlx-cli
pnpm i
```

### Common commands

```bash
pnpm run dev                  # backend + web app with hot reload
pnpm run check                # typecheck all frontend packages + Rust workspaces
pnpm run lint                 # ESLint + clippy
pnpm run format               # Prettier + rustfmt
cargo test --workspace        # Rust tests
pnpm run generate-types       # regenerate shared/types.ts from Rust
pnpm run prepare-db           # refresh SQLx offline metadata
```

### Environment variables

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `POSTHOG_API_KEY` | Build-time | Empty | Analytics key (analytics disabled when empty) |
| `POSTHOG_API_ENDPOINT` | Build-time | Empty | Analytics endpoint (analytics disabled when empty) |
| `PORT` | Runtime | Auto-assign | **Production**: server port. **Dev**: frontend port (backend uses PORT+1) |
| `BACKEND_PORT` | Runtime | `0` (auto) | Backend port (dev mode, overrides PORT+1) |
| `FRONTEND_PORT` | Runtime | `3000` | Frontend dev server port |
| `HOST` | Runtime | `127.0.0.1` | Backend host |
| `MCP_HOST` | Runtime | `HOST` | MCP server connection host |
| `MCP_PORT` | Runtime | `BACKEND_PORT` | MCP server connection port |
| `DISABLE_WORKTREE_CLEANUP` | Runtime | Not set | Disable git worktree cleanup (debugging) |
| `VK_ALLOWED_ORIGINS` | Runtime | Not set | Comma-separated origins allowed to call the backend API |
| `VK_SHARED_API_BASE` | Runtime | Not set | Base URL of a self-hosted remote API (optional) |
| `VK_SHARED_RELAY_API_BASE` | Runtime | Not set | Base URL of the relay API for tunnel-mode connections |
| `VK_TUNNEL` | Runtime | Not set | Enable relay tunnel mode |
| `BC_MSG_HISTORY_BYTES` | Runtime | `16777216` (16 MiB) | In-memory log scrollback retained per store. Charged per concurrent execution — lower it under memory pressure with many agents, raise it if long runs lose early scrollback |
| `BC_MSG_BROADCAST_CAPACITY` | Runtime | `32768` | Live log ring size per store, in messages (tokio rounds up to a power of two). Raise it if the logs show `MsgStore broadcast lagged` |

### Reverse proxy / custom domain

Behind a reverse proxy (nginx, Caddy, Traefik) or on a custom domain, set `VK_ALLOWED_ORIGINS` to the full origin(s) the frontend is served from, otherwise API requests are rejected with 403:

```bash
VK_ALLOWED_ORIGINS=https://coding.example.com
```

### Remote server + local editor

When the app runs on a remote machine, configure Settings → Editor Integration with your SSH host/user; "Open in VSCode" buttons then emit `vscode://vscode-remote/ssh-remote+user@host/path` URLs that open your local editor against the remote checkout. Requires passwordless SSH and the VSCode Remote-SSH extension.

## What was removed from upstream

- The cloud kanban (projects, issues, statuses, tags) — upstream retired it to a read-only "export your data" tombstone; this fork removes the dead surface entirely.
- The export wizard, cloud-shutdown banners, and the far-left app rail.
- The `list_projects` MCP tool and the local→cloud project proxy routes.
- Discord/marketing links and all `vibekanban.com` documentation links.

The optional remote infrastructure (`crates/remote`, relay tunneling, host pairing) is still in the tree and self-hostable.

## License

[Apache-2.0](LICENSE). Forked from [BloopAI/vibe-kanban](https://github.com/BloopAI/vibe-kanban) — thanks to the Bloop team for the original work.
