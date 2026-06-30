# CLI-mode agents — install + end-to-end plan (awaiting approval)

Claude and **Codex** are done and live-verified. The other seven agents are
**fully coded and unit-tested** (their `interactive_cli_spec` is implemented and
wired through the same path Codex uses) but are **not installed locally**, so I
can't run the live TUI check the way I did for Codex. This doc is the per-agent
**install command + auth/onboarding requirements + the code already written +
the e2e checklist** — nothing here installs anything; it waits for your
approval.

## ✅ Verification update (2026-06-30) — all 7 installed + launch-verified

All seven were installed (npm: gemini/qwen/opencode/amp/copilot; curl:
cursor-agent/droid) and **each was launched in a real tmux pane with the exact
flags `interactive_cli_spec` generates**. Every binary resolved, **every coded
flag was accepted (no flag errors)**, and each reached its own UI — the expected
stopping point without an API key:

| Agent | Version | Launch result |
|---|---|---|
| gemini | 0.49.0 | TUI rendered, prompt seeded (`--approval-mode yolo -i`) |
| qwen | 0.19.3 | provider-connect menu |
| opencode | 1.17.11 | **answered the prompt** on its free built-in model |
| cursor-agent | 2026.06.26 | "Press any key to log in" |
| droid | 0.161.0 | full TUI (`--auto high`) |
| copilot | 1.0.65 | TUI + folder-trust dialog |
| amp | 0.0.178x | login flow (stdin-piped prompt delivered) |

**Flag drift fixed on the installed versions** (the specs in this branch reflect these):
- **droid 0.161** dropped `--model` / `--reasoning-effort` / `--skip-permissions-unsafe`
  from interactive launch → spec now emits only `--auto low|medium|high`
  (SkipPermissionsUnsafe → `--auto high`); model/effort are set in-TUI (`/model`).
- **copilot 1.0.65** has no `--no-banner` (banner is off by default) → removed.
- **opencode** → confirmed long-form `--session` / `--continue`.

**Remaining per-agent friction (only matters once a key is added):** copilot's
folder-trust modal and gemini's "untrusted folder" notice still want a
`maybe_seed_cli_trust` arm (like codex's); qwen/cursor/droid/amp/copilot/gemini
need their auth key/login. None of this blocks the integration — it's the
key/login step below.

---

> **What "done" means per agent below:** the launch recipe
> (`interactive_cli_spec`) is written + unit-tested and the agent already works
> end-to-end via the generic path (`terminal.rs` → `cli_bootstrap`). What's left
> per agent is: (1) `cargo`-free **install**, (2) **auth** (an API key/token you
> own), (3) a small **onboarding/trust seeder** in `maybe_seed_cli_trust`
> (`crates/local-deployment/src/pty.rs`) where the agent blocks on a first-run
> dialog — same pattern as `ensure_codex_folder_trusted` /
> `ensure_codex_update_nag_dismissed` — and (4) the **live e2e** in a tmux pane.

## Already-shared code (all 7)

- `crates/executors/src/executors/<agent>.rs` — `interactive_cli_spec()` mapping
  the agent's existing managed config (model / reasoning / autonomy) to its
  interactive flags. Autonomous by default (your Q4), Supervised dials it back.
- `crates/executors/src/executors/cli.rs` — added `CliPromptArg::StdinPipe` for
  Amp.
- `crates/executors/src/executors/mod.rs` — `cli_spec_tests`: every agent has a
  spec with the right binary + autonomous-by-default flags (passing).
- No new wiring needed per agent: `terminal.rs` already resolves any
  `Session.executor` and `cli_bootstrap` already speaks the generic spec.

## Codex e2e recipe (the template every agent below follows)

1. Confirm installed + authed (`codex --version`, `~/.codex/auth.json`).
2. Seed first-run friction so the TUI doesn't block (trust + update modal).
3. Create a CLI workspace with the agent selected → the tmux pane launches the
   TUI with the picked model + autonomous permissions, runs the initial prompt.
4. Confirm: model shown, autonomy mode, `resume` rejoins, loop automation
   (enable "Keep going") re-prompts on a simulated limit.

---

## 1. Codex ✅ DONE (reference)
- Install: `npm i -g @openai/codex@latest` (verified on 0.140.0).
- Seeders coded: `ensure_codex_folder_trusted` (per-project `trust_level`),
  `ensure_codex_update_nag_dismissed` (version.json). **Live-verified**: YOLO
  autonomy + model + worktree in a real pane.

## 2. Gemini CLI
- **Install:** `npm i -g @google/gemini-cli@latest` (latest 0.49.0). Binary: `gemini`.
- **Auth:** `GEMINI_API_KEY` (Gemini API) or Google OAuth / Vertex envs.
- **First-run friction (blocks TUI):** theme-select + auth-method dialogs.
  Folder-trust is OFF by default. **Seeder to add** (`maybe_seed_cli_trust` →
  `"gemini"`): write `~/.gemini/settings.json` with `theme`,
  `security.auth.selectedType` (e.g. `gemini-api-key`), and
  `privacy.usageStatisticsEnabled=false`, only if absent. (Env alt:
  `GEMINI_CLI_TRUST_WORKSPACE=true` if folder-trust is enabled.)
- **Code (done):** `gemini.rs::interactive_cli_spec` → `gemini --model <m>
  --approval-mode yolo -i '<prompt>'`, resume `--resume <id>`, continue Fresh.
- **e2e checklist:** install → set `GEMINI_API_KEY` → add seeder → create CLI
  workspace w/ Gemini → verify TUI + model + "YOLO" + `--resume`.

## 3. Qwen Code
- **Install:** `npm i -g @qwen-code/qwen-code@latest` (latest 0.19.3). Binary: `qwen`.
- **Auth:** `OPENAI_API_KEY` (+ `OPENAI_BASE_URL`, `OPENAI_MODEL`) or DashScope.
  (Qwen OAuth free tier was discontinued.)
- **First-run friction:** `/auth` menu blocks. Trust OFF by default. **Seeder to
  add** (`"qwen"`): write `~/.qwen/settings.json` `modelProviders.openai` +
  `security.auth.selectedType="openai"`; set `QWEN_CODE_SUPPRESS_YOLO_WARNING=1`.
- **Code (done):** `qwen.rs` → `qwen --model <m> --approval-mode yolo -i
  '<prompt>'`, resume `--resume <id>`, continue `--continue`.
- **e2e checklist:** install → key → seeder → workspace → verify.

## 4. opencode
- **Install:** `npm i -g opencode-ai@latest` (latest 1.17.11). Binary: `opencode`.
- **Auth:** provider env (`ANTHROPIC_API_KEY` / `OPENAI_API_KEY`) or `opencode
  auth login` (`~/.config/opencode/auth.json`). **No trust prompt.**
- **Autonomy caveat:** the stable interactive command has **no autonomy flag**;
  set `"permission": "allow"` in `opencode.json`. **Seeder to add** (`"opencode"`):
  ensure the workspace/global `opencode.json` has `permission: "allow"` (mirrors
  the managed `auto_approve`).
- **Code (done):** `opencode.rs` → `opencode -m <provider/model> --prompt
  '<prompt>'`, resume `-s <id>`, continue `-c`.
- **e2e checklist:** install → provider key → seed `permission:"allow"` →
  workspace → verify (note: model id is `provider/model`).

## 5. Cursor Agent
- **Install:** `curl https://cursor.com/install -fsS | bash` (CalVer, auto-updates;
  no official npm). Binary: `cursor-agent`.
- **Auth:** `CURSOR_API_KEY` (or `cursor-agent login`; `NO_OPEN_BROWSER=1`).
- **First-run friction:** workspace-trust enforcement — an interactive launch in
  a fresh folder may show a trust prompt (`--trust` is documented headless-only).
  **Seeder/verify needed:** confirm whether `--force` clears it on the pinned
  build; if not, pre-trust the worktree (Cursor stores trust under `~/.cursor`).
- **Code (done):** `cursor.rs` → `cursor-agent --model <resolved> --force
  '<prompt>'` (reuses `resolve_cursor_model_name` for reasoning), resume
  `--resume <id>`, continue `--continue`.
- **e2e checklist:** install → `CURSOR_API_KEY` → workspace → **watch for the
  trust prompt** → add a trust seeder if it blocks → verify.

## 6. Droid (Factory)
- **Install:** `curl -fsSL https://app.factory.ai/cli | sh` (or `npm i -g
  @factory/cli`; v0.150.x). Binary: `droid`.
- **Auth:** `FACTORY_API_KEY`. **No folder-trust gate** (welcome/login screen
  only, removed by the key).
- **Code (done):** `droid.rs` → `droid --model <m> --reasoning-effort <e>
  --skip-permissions-unsafe '<prompt>'` (long-form effort because interactive
  `-r` = resume), resume `--resume <id>`, continue `--resume`.
- **e2e checklist:** install → `FACTORY_API_KEY` → workspace → verify autonomy
  (default `--skip-permissions-unsafe`); confirm `--auto`/`--skip-permissions-unsafe`
  apply at interactive launch on the pinned build (else set via `Ctrl+L`/settings).

## 7. Amp (Sourcegraph)
- **Install:** `npm i -g @sourcegraph/amp` (date-versioned; `@latest`). Binary: `amp`.
- **Auth:** `AMP_API_KEY`. **No trust prompt.** Threads are **cloud-stored** —
  resume needs network/auth.
- **Autonomy caveat:** no CLI flag (removed); set `"amp.dangerouslyAllowAll":
  true` in `~/.config/amp/settings.json`. **Seeder to add** (`"amp"`).
- **Code (done):** `amp.rs` → `printf '%s\n' '<prompt>' | amp --no-ide` (stdin
  seed — stays interactive on a TTY), resume `amp threads continue <id>`,
  continue `amp threads continue --last`.
- **e2e checklist:** install → `AMP_API_KEY` → seed `dangerouslyAllowAll` →
  workspace → verify the piped prompt runs interactively.

## 8. GitHub Copilot CLI
- **Install:** `npm i -g @github/copilot@latest` (latest 1.0.65; Node 22+).
  Binary: `copilot`. (Note: managed mode still pins `0.0.403`; CLI mode uses the
  PATH binary — bump recommended.)
- **Auth:** `COPILOT_GITHUB_TOKEN` → `GH_TOKEN` → `GITHUB_TOKEN` (fine-grained
  PAT with "Copilot Requests").
- **First-run friction:** folder-trust dialog (no flag — open issue #1121).
  **Seeder to add** (`"copilot"`): add the worktree to `trusted_folders` in
  `~/.copilot/config.json` (verify exact key casing against a real file). Banner
  handled by `--no-banner` (already in the spec).
- **Code (done):** `copilot.rs` → `copilot --model <m> --allow-all-tools
  [--allow-tool/--deny-tool/--add-dir] --no-banner -i '<prompt>'`, resume
  `--resume <id>`, continue `--continue`.
- **e2e checklist:** install → token → add `trusted_folders` seeder → workspace
  → verify.

---

## Suggested rollout order (after you approve installs)
codex ✅ → **gemini → opencode → cursor → droid → amp → qwen → copilot**, each:
install + auth, add the seeder arm (1 small fn in `pty.rs`, like Codex's),
live-verify in a pane, then move on. The launch code + tests are already in the
branch, so each agent is just install + auth + (maybe) a seeder + the live check.
