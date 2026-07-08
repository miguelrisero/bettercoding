use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use executors::executors::cli::{CliContinue, CliLaunchSpec, CliPromptArg, CliResume};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use thiserror::Error;
use tokio::sync::mpsc;
use utils::shell::get_interactive_shell;
use uuid::Uuid;

/// What to run on the PTY.
// `TmuxCli` carries the resolved launch spec and is the common case (CLI mode is
// the default pane); the size gap vs. the unit `Shell` variant is expected and
// boxing the hot path would only add an allocation.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum PtyCommand {
    /// The user's interactive shell (default side-terminal behavior).
    Shell,
    /// Attach-or-create a namespaced tmux session running the CLI bootstrap.
    /// `tmux new-session -A` attaches when the session already exists, so the
    /// session (and whatever runs inside it) survives WebSocket disconnects
    /// and server restarts; reconnects reattach instead of respawning.
    TmuxCli {
        /// The workspace this pane belongs to; the tmux session name and the
        /// staged prompt-file path are both derived from it
        /// ([`cli_tmux_session_name`], [`write_cli_prompt_file`]).
        workspace_id: Uuid,
        /// claude's own session UUID to resume (the workspace's selected uix
        /// chat). When set, the bootstrap runs `claude --resume <id>` so CLI
        /// mode joins the *exact* conversation the chat UI is showing, and
        /// follow-ups from either side share one transcript.
        resume_session_id: Option<String>,
        /// The workspace's initial prompt (CLI-first creation): handed to the
        /// interactive agent as its argument so the run happens visibly in
        /// the terminal instead of a headless executor. Ignored when
        /// `resume_session_id` is set.
        initial_prompt: Option<String>,
        /// A paste delivery will follow this launch (oversized or
        /// launch-arg-less prompt). The bootstrap must start a FRESH TUI
        /// instead of the `--continue || fresh` fallback: the doomed
        /// `--continue` first leg of a brand-new workspace lives just long
        /// enough to swallow the paste and exit, silently discarding it.
        deferred_prompt_pending: bool,
        /// How to launch the selected agent's interactive CLI — binary, flags
        /// (model/effort/sandbox/approval pre-resolved from the session's
        /// ExecutorConfig), and the resume/prompt/continue forms. Claude is the
        /// default; codex and the other agents fill in their own spec.
        spec: CliLaunchSpec,
    },
}

/// Build the initial window command for a new CLI tmux session. Runs the
/// selected agent's interactive TUI when installed (`spec.program`), then drops
/// to a shell instead of ending the session (so a crashed/exited agent leaves a
/// usable pane). Ignored by `-A` attaches (only runs on first creation).
///
/// The launch form is chosen from the agent-supplied [`CliLaunchSpec`]:
/// - resume by id (`resume_session_id`, a validated UUID) joins the agent's
///   exact session — the same transcript the headless executor created/uses —
///   so the chat UI and CLI hand off the conversation. The shape is per-agent:
///   a flag (`claude --resume <id>`) or a subcommand (`codex resume <id>`).
/// - with an `initial_prompt` (CLI-first creation) the workspace prompt is
///   delivered as a positional arg or a flag value, per the spec.
/// - otherwise the agent's continue-fallback runs (claude `--continue`, codex
///   `resume --last`, or a fresh TUI), so a workspace whose tmux session died
///   rejoins its conversation where possible.
///
/// Agent flags (model / effort / sandbox / approval / autonomy) are pre-resolved
/// into `spec.base_args` from the session's selected `ExecutorConfig`, so CLI
/// mode honors the same selection as the headless executor. Per-folder trust /
/// onboarding dialogs the agent can't suppress are pre-accepted separately in
/// [`maybe_seed_cli_trust`] before this bootstrap runs.
fn cli_bootstrap(
    spec: &CliLaunchSpec,
    resume_session_id: Option<&str>,
    prompt_file: Option<&Path>,
    deferred_prompt_pending: bool,
) -> String {
    // The program is a bare binary name from our own code; quote it anyway so
    // it can never be anything but a single command word.
    let prog = shell_single_quote(&spec.program);

    // Agent flags (model/effort/sandbox/approval/autonomy) applied to every
    // launch form except resume-by-subcommand, shell-quoted so a value can
    // never break out of the command.
    let flags: String = spec
        .base_args
        .iter()
        .map(|arg| format!(" {}", shell_single_quote(arg)))
        .collect();
    let base = format!("{prog}{flags}");

    // Nothing explicit to run (a CLI-first workspace whose tmux session died):
    // continue the most recent conversation in this cwd if the agent can,
    // otherwise a fresh TUI.
    let continue_launch = || match &spec.continue_fallback {
        CliContinue::Flag(flag) => format!("{base} {flag} || {base}"),
        CliContinue::ResumeLast { subcommand } => {
            format!("{prog} {subcommand} --last || {prog}")
        }
        CliContinue::Fresh => base.clone(),
    };

    let launch = if let Some(id) = active_resume_id(resume_session_id) {
        match &spec.resume {
            // `<base> --resume <id>` — flags still apply (claude).
            CliResume::Flag(flag) => format!("{base} {flag} {id}"),
            // `<program> resume <id>` — a resume subcommand restores the
            // session's own settings, so the base flags are NOT replayed (codex).
            CliResume::Subcommand(sub) => format!("{prog} {sub} {id}"),
            CliResume::Unsupported => continue_launch(),
        }
    } else if let Some(file) = prompt_file {
        // CLI-first creation: the workspace prompt lives in a private file
        // ([`write_cli_prompt_file`]) and is read into the launch at pane-shell
        // time. This keeps the tmux `new-session` command O(1) in prompt size
        // (tmux rejects commands past ~16KB) and means the prompt never re-enters
        // shell quoting — the file PATH is single-quoted, and the content only
        // ever expands inside double quotes, so it can't be word-split or parsed
        // as shell. The file self-deletes (`rm`) once consumed.
        let qfile = shell_single_quote(&file.to_string_lossy());
        // Shared read-and-delete stage for the argv-passing arms: consume the
        // file into `vk_p`, then delete it (the delete doubles as the delivery
        // acknowledgement — see [`cli_prompt_file_exists`]).
        let read_rm = format!(r#"vk_p="$(cat {qfile})"; rm -f -- {qfile};"#);
        match &spec.prompt_arg {
            // Trailing positional arg. The leading-dash guard and any trailing
            // whitespace handling are baked into the file's contents
            // ([`cli_prompt_file_content`]); command substitution strips a
            // trailing newline, which is harmless.
            CliPromptArg::Positional => {
                format!(r#"{read_rm} {base} "$vk_p""#)
            }
            // Prompt as a flag value (e.g. gemini/copilot `-i "<prompt>"`); a
            // leading '-' is harmless after the flag. The flag is one of our
            // own spec constants, but quote it anyway (like the program and
            // base args) so it can never be more than a single command word.
            CliPromptArg::Flag(flag) => {
                let qflag = shell_single_quote(flag);
                format!(r#"{read_rm} {base} {qflag} "$vk_p""#)
            }
            // Prompt piped on stdin (e.g. amp); the TUI stays interactive
            // because the tmux pane keeps stdout a TTY. No argv-length ceiling
            // at all. The file already carries the trailing newline printf
            // added. The `rm` runs inside the pipeline's producer — right after
            // `cat` streams the file — so consumption is acknowledged (file
            // gone) as soon as the prompt is handed off, not when the agent
            // eventually exits.
            CliPromptArg::StdinPipe => {
                format!("{{ cat {qfile}; rm -f -- {qfile}; }} | {base}")
            }
            // No CLI way to seed the prompt — start the TUI and rely on a
            // post-launch keystroke delivery (loop automation / send-keys).
            CliPromptArg::Unsupported => continue_launch(),
        }
    } else if deferred_prompt_pending {
        // A paste delivery follows: start a FRESH TUI. The usual
        // `--continue || fresh` fallback would run a doomed `--continue`
        // first leg on a brand-new workspace — alive just long enough to
        // pass the readiness gate, swallow the pasted prompt, and exit.
        base.clone()
    } else {
        continue_launch()
    };

    // When the agent's binary isn't installed/on PATH, don't silently drop into
    // a bare shell (the user picked an agent and would see no reason why) — print
    // a clear, actionable notice first, then keep the pane usable as a shell so
    // they can install it right there or switch agents.
    let missing = match cli_install_hint(&spec.program) {
        Some(hint) => format!(
            "printf '\\n  [!] %s is not installed or not on PATH.\\n      Install: %s\\n      then reopen this terminal, or pick another agent.\\n\\n' {prog} {}",
            shell_single_quote(hint)
        ),
        None => format!("printf '\\n  [!] %s is not installed or not on PATH.\\n\\n' {prog}"),
    };

    format!(
        r#"if command -v {prog} >/dev/null 2>&1; then {launch}; else {missing}; fi; exec "${{SHELL:-/bin/sh}}""#
    )
}

/// Largest prompt (bytes) baked into the launch command via the temp-file
/// `$(cat)` transport for argv-passing agents. Positional/Flag agents hand the
/// prompt to the pane shell as a single argv entry, bounded by Linux
/// `MAX_ARG_STRLEN` (~131072 bytes); a conservative cap keeps clear of `E2BIG`
/// (and of macOS's smaller shared `ARG_MAX`). Larger prompts are delivered
/// post-launch by paste instead (see [`cli_prompt_fits_inline`]).
const MAX_INLINE_PROMPT_BYTES: usize = 100_000;

/// The resume id that will actually drive a resume launch: only a strict UUID
/// may ever be interpolated into the bootstrap shell string (agent session ids
/// are UUIDs, so this both validates intent and forecloses shell injection via
/// the id). The prompt-staging gate uses the SAME predicate, so a file is
/// staged exactly when the bootstrap will consume it — the two can't drift.
fn active_resume_id(resume_session_id: Option<&str>) -> Option<&str> {
    resume_session_id.filter(|id| is_uuid(id))
}

/// Whether an initial prompt of `byte_len` bytes can be baked into the launch
/// command for an agent with this `prompt_arg`, or must be delivered after the
/// TUI is up (via [`send_cli_keys`]). `StdinPipe` has no argv ceiling;
/// `Positional`/`Flag` pass the prompt as one argv entry and are capped;
/// `Unsupported` has no launch-time transport at all.
fn cli_prompt_fits_inline(prompt_arg: &CliPromptArg, byte_len: usize) -> bool {
    match prompt_arg {
        CliPromptArg::Positional | CliPromptArg::Flag(_) => byte_len <= MAX_INLINE_PROMPT_BYTES,
        CliPromptArg::StdinPipe => true,
        CliPromptArg::Unsupported => false,
    }
}

/// How a workspace's parked initial prompt should be delivered to the freshly
/// launched CLI agent — decided from the raw prompt and the agent's
/// `prompt_arg`.
#[derive(Debug, PartialEq, Eq)]
pub enum CliPromptRouting {
    /// Nothing to deliver: no prompt was carried, or it was blank after trim.
    None,
    /// Small enough to bake into the launch command's temp-file transport.
    Baked(String),
    /// Too large to pass as one argv entry, or an agent with no launch-time
    /// prompt arg: deliver by paste ([`send_cli_keys`]) after the pane is
    /// confirmed up.
    Deferred(String),
}

/// Decide how to deliver a CLI-first workspace's initial prompt. Pure so the
/// baked-vs-deferred routing that gates the deferred-clear recovery path is unit
/// testable without a live tmux/socket. Blank prompts route to `None` (the
/// caller carries no prompt and clears nothing); anything that fits inline is
/// `Baked`, everything else `Deferred`. Both variants carry the trimmed text
/// (the downstream [`cli_prompt_file_content`] trim is then a no-op).
pub fn route_initial_prompt(
    initial_prompt: Option<String>,
    prompt_arg: &CliPromptArg,
) -> CliPromptRouting {
    let Some(prompt) = initial_prompt else {
        return CliPromptRouting::None;
    };
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        CliPromptRouting::None
    } else if cli_prompt_fits_inline(prompt_arg, trimmed.len()) {
        CliPromptRouting::Baked(trimmed.to_string())
    } else {
        CliPromptRouting::Deferred(trimmed.to_string())
    }
}

/// Route a parked prompt that must arrive as a FOLLOW-UP — the tmux session
/// already exists (an earlier delivery went unconfirmed, or a loop wake-up
/// was re-parked) or a resume launch wins the boot: never baked, always
/// pasted into the running agent. Blank prompts route to `None`. Pure for the
/// same reason as [`route_initial_prompt`].
pub fn route_followup_prompt(prompt: &str) -> CliPromptRouting {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        CliPromptRouting::None
    } else {
        CliPromptRouting::Deferred(trimmed.to_string())
    }
}

/// The exact bytes to write to a workspace's CLI prompt file for `prompt_arg`,
/// or `None` when the (trimmed) prompt is blank — mirroring the old in-command
/// quoting semantics so small prompts behave identically: the leading-dash
/// guard for `Positional` (so a prompt like `-rf` can't parse as a flag) is a
/// literal leading space in the file; `StdinPipe` keeps the trailing newline the
/// old `printf '%s\n'` added. The content is stored verbatim (never
/// shell-escaped) — the bootstrap reads it back inside double quotes.
fn cli_prompt_file_content(prompt_arg: &CliPromptArg, prompt: &str) -> Option<String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return None;
    }
    Some(match prompt_arg {
        CliPromptArg::Positional => {
            if prompt.starts_with('-') {
                format!(" {prompt}")
            } else {
                prompt.to_string()
            }
        }
        CliPromptArg::Flag(_) => prompt.to_string(),
        CliPromptArg::StdinPipe => format!("{prompt}\n"),
        CliPromptArg::Unsupported => return None,
    })
}

/// Path of a workspace's transient CLI initial-prompt file. Kept next to the
/// other backend assets (same trust domain as the SQLite DB) under a dedicated
/// `cli-prompts/` subdir; named by the workspace id so racing first-attaches
/// write the same path idempotently.
fn cli_prompt_file_path(workspace_id: Uuid) -> PathBuf {
    utils::assets::asset_dir()
        .join("cli-prompts")
        .join(format!("{}.txt", workspace_id.simple()))
}

/// Write a workspace's CLI initial prompt to its private (0600) file for the
/// bootstrap to read. Returns the path on success. The file self-deletes once
/// the bootstrap consumes it; [`remove_cli_prompt_file`] and
/// [`kill_cli_tmux_session`] clean up the never-consumed case.
fn write_cli_prompt_file(workspace_id: Uuid, content: &str) -> std::io::Result<PathBuf> {
    let path = cli_prompt_file_path(workspace_id);
    if let Some(dir) = path.parent() {
        // Owner-only dir: the files inside are already 0600, but a 0700 dir
        // also keeps prompt-file names (workspace ids + staging times) from
        // being enumerable by other local users.
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(dir)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        // `.mode()` only applies when the file is CREATED; if a looser-perm file
        // pre-existed at this path (stale from an older build, or same-user
        // tampering) `create(true)` reuses it without re-chmod'ing. Force 0600
        // so the prompt is never readable at wider perms. (The 0700 dir already
        // blocks other users; this is defense in depth.)
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(content.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, content.as_bytes())?;
    }
    Ok(path)
}

/// Best-effort delete of a workspace's transient CLI prompt file. Called when a
/// session that never consumed it is torn down (spawn failure recovery, kill,
/// reaper) so a prompt is never left readable on disk after it's no longer
/// needed.
pub fn remove_cli_prompt_file(workspace_id: Uuid) {
    let _ = std::fs::remove_file(cli_prompt_file_path(workspace_id));
}

/// Whether a workspace's staged CLI prompt file is still on disk. The bootstrap
/// `rm`s the file the moment it consumes it, so "file gone" is the delivery
/// acknowledgement the terminal route polls before clearing the parked DB copy.
/// A launch that never consumed the prompt (agent binary missing → the
/// `command -v` guard skips the whole launch arm) leaves the file — and
/// therefore the DB copy — in place for the next fresh session.
pub fn cli_prompt_file_exists(workspace_id: Uuid) -> bool {
    cli_prompt_file_path(workspace_id).exists()
}

/// Workspaces with a CLI prompt delivery currently in flight. See
/// [`CliPromptDelivery`].
static CLI_PROMPT_DELIVERIES: OnceLock<Mutex<HashSet<Uuid>>> = OnceLock::new();

/// In-process claim that exactly ONE terminal attach delivers a workspace's
/// parked CLI prompt at a time. Two racing first-attaches can both pass the
/// "no tmux session yet" gate before either spawns; without the claim both
/// would stage the same prompt file (the loser's `truncate` can tear the
/// winner's in-flight `cat`) or, for an oversized prompt, both would paste it
/// (double delivery). The loser simply attaches without carrying the prompt —
/// `new-session -A` ignores its bootstrap anyway. Dropping the claim releases
/// it; delivery holders keep it until the parked prompt is cleared or
/// explicitly left parked.
#[derive(Debug)]
pub struct CliPromptDelivery(Uuid);

impl CliPromptDelivery {
    /// Claim the workspace's prompt delivery, or `None` if another attach in
    /// this process already holds it.
    pub fn try_claim(workspace_id: Uuid) -> Option<Self> {
        let set = CLI_PROMPT_DELIVERIES.get_or_init(Default::default);
        // Recover from poisoning rather than propagating it: the set is a
        // plain HashSet with no invariants a panicked holder could have
        // broken, and treating poison as "already claimed" would silently
        // disable prompt delivery for EVERY workspace until restart.
        //
        // Release the registry guard BEFORE constructing the claim: an eagerly
        // built `Self` on the not-inserted path (`then_some`) would be dropped
        // while the guard is still alive, and `Drop` re-locks the same
        // (non-reentrant) mutex — a self-deadlock.
        let inserted = set
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(workspace_id);
        inserted.then(|| Self(workspace_id))
    }
}

impl Drop for CliPromptDelivery {
    fn drop(&mut self) {
        if let Some(set) = CLI_PROMPT_DELIVERIES.get() {
            set.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.0);
        }
    }
}

/// How to install each interactive-CLI agent, shown in the pane when its binary
/// isn't on PATH. Kept here (next to the bootstrap that prints it) rather than on
/// the spec so the message stays a deployment concern.
fn cli_install_hint(program: &str) -> Option<&'static str> {
    Some(match program {
        "claude" => "npm i -g @anthropic-ai/claude-code",
        "codex" => "npm i -g @openai/codex",
        "gemini" => "npm i -g @google/gemini-cli",
        "qwen" => "npm i -g @qwen-code/qwen-code",
        "opencode" => "npm i -g opencode-ai",
        "copilot" => "npm i -g @github/copilot",
        "amp" => "npm i -g @sourcegraph/amp",
        "cursor-agent" => "curl https://cursor.com/install -fsS | bash",
        "droid" => "curl -fsSL https://app.factory.ai/cli | sh",
        _ => return None,
    })
}

/// Pre-accept per-folder trust / first-run dialogs the selected agent's CLI
/// would otherwise block on, for this app-created (trusted) worktree. Keyed by
/// the launch program so each agent's local-environment friction is handled in
/// one place. Best-effort: any failure just means the dialog shows as before.
fn maybe_seed_cli_trust(program: &str, dir: &Path) {
    match program {
        "claude" => ensure_claude_folder_trusted(dir),
        "codex" => {
            ensure_codex_folder_trusted(dir);
            ensure_codex_update_nag_dismissed();
        }
        // copilot / cursor / gemini / qwen onboarding seeding is added
        // alongside each agent's CLI support.
        _ => {}
    }
}

/// POSIX single-quote escaping: the only character that needs handling
/// inside single quotes is the single quote itself.
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Pre-accept claude's per-directory "Do you trust the files in this folder?"
/// dialog for an app-created workspace, so CLI mode never blocks the user on it.
///
/// claude exposes no flag/setting/env to suppress this — `--dangerously-skip-permissions`
/// only covers per-tool approval — and records trust per directory in the global
/// `~/.claude.json`. BetterCoding created this worktree, so the user implicitly
/// trusts it; we pre-seed the very key claude writes on "Yes". Best-effort and
/// per-process memoized: any failure just means the dialog shows as before.
fn ensure_claude_folder_trusted(dir: &Path) {
    static SEEDED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let seeded = SEEDED.get_or_init(|| Mutex::new(HashSet::new()));

    if seeded
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(dir)
    {
        return;
    }

    match seed_claude_trust(dir) {
        Ok(()) => {
            seeded
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(dir.to_path_buf());
        }
        Err(e) => {
            tracing::debug!(
                "Could not pre-trust {} in ~/.claude.json (folder-trust dialog may show): {e}",
                dir.display()
            );
        }
    }
}

/// Read `~/.claude.json`, mark `dir` trusted, and write it back atomically.
/// Serialized against itself so concurrent CLI attaches don't clobber each
/// other's additions. Bails (never clobbers) if the file exists but isn't valid
/// JSON. NOTE: this rewrites the whole file, so a claude instance writing it in
/// the same instant could lose that write — a narrow, single-user-tool trade we
/// accept; writes only happen once per worktree per process.
fn seed_claude_trust(dir: &Path) -> std::io::Result<()> {
    static WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());

    let Some(config_path) = dirs::home_dir().map(|home| home.join(".claude.json")) else {
        return Ok(());
    };

    let mut root: serde_json::Value = match std::fs::read_to_string(&config_path) {
        Ok(contents) => serde_json::from_str(&contents)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e),
    };

    // claude keys trust by the cwd it resolves (getcwd resolves symlinks), so
    // seed both the given path and its canonical form to be safe.
    let mut keys = vec![dir.to_string_lossy().into_owned()];
    if let Ok(canonical) = dir.canonicalize() {
        let canonical = canonical.to_string_lossy().into_owned();
        if !keys.contains(&canonical) {
            keys.push(canonical);
        }
    }

    if !apply_trust_to_config(&mut root, &keys) {
        return Ok(());
    }

    // Atomic replace: write a sibling temp file then rename over the original.
    let serialized = serde_json::to_vec_pretty(&root)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp_path = config_path.with_extension("json.vk-trust-tmp");
    std::fs::write(&tmp_path, &serialized)?;
    std::fs::rename(&tmp_path, &config_path)?;
    Ok(())
}

/// Pure merge: set the trust + onboarding keys for each project path, preserving
/// everything else. Returns whether anything changed. Split out so the merge is
/// unit-testable without touching the real `~/.claude.json`.
fn apply_trust_to_config(root: &mut serde_json::Value, project_keys: &[String]) -> bool {
    let Some(obj) = root.as_object_mut() else {
        return false;
    };
    let projects = obj
        .entry("projects")
        .or_insert_with(|| serde_json::json!({}));
    let Some(projects) = projects.as_object_mut() else {
        return false;
    };

    let mut changed = false;
    for key in project_keys {
        let entry = projects
            .entry(key.clone())
            .or_insert_with(|| serde_json::json!({}));
        let Some(entry) = entry.as_object_mut() else {
            continue;
        };
        for (field, value) in [
            ("hasTrustDialogAccepted", serde_json::Value::Bool(true)),
            (
                "hasCompletedProjectOnboarding",
                serde_json::Value::Bool(true),
            ),
        ] {
            if entry.get(field) != Some(&value) {
                entry.insert(field.to_string(), value);
                changed = true;
            }
        }
        let seen = entry
            .get("projectOnboardingSeenCount")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if seen < 1 {
            entry.insert(
                "projectOnboardingSeenCount".to_string(),
                serde_json::json!(1),
            );
            changed = true;
        }
    }
    changed
}

/// `$CODEX_HOME/config.toml`, else `~/.codex/config.toml` — where codex records
/// per-project trust.
fn codex_config_path() -> Option<PathBuf> {
    match std::env::var("CODEX_HOME") {
        Ok(home) if !home.trim().is_empty() => Some(PathBuf::from(home).join("config.toml")),
        _ => dirs::home_dir().map(|home| home.join(".codex").join("config.toml")),
    }
}

/// Pre-accept codex's per-directory trust prompt for this app-created worktree
/// by marking it `trusted` in `~/.codex/config.toml`, so the interactive launch
/// never blocks on "Do you want to allow Codex to work in this folder?".
/// Per-process memoized; best-effort (a failure just means the prompt shows).
fn ensure_codex_folder_trusted(dir: &Path) {
    static SEEDED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let seeded = SEEDED.get_or_init(|| Mutex::new(HashSet::new()));

    if seeded
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(dir)
    {
        return;
    }

    match seed_codex_trust(dir) {
        Ok(()) => {
            seeded
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(dir.to_path_buf());
        }
        Err(e) => {
            tracing::debug!(
                "Could not pre-trust {} in codex config.toml (trust prompt may show): {e}",
                dir.display()
            );
        }
    }
}

/// Append `[projects."<dir>"] trust_level = "trusted"` blocks for `dir` (and its
/// canonical form) to codex's `config.toml`, but only when absent. Non-destructive
/// by design: it never rewrites the user's existing settings — it appends — and
/// bails without writing if either the existing file or the result wouldn't parse
/// as TOML, so a malformed merge can never corrupt the user's codex config.
fn seed_codex_trust(dir: &Path) -> std::io::Result<()> {
    static WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());

    let Some(config_path) = codex_config_path() else {
        return Ok(());
    };

    let existing = match std::fs::read_to_string(&config_path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };

    // Never touch a config we can't parse — preserve the user's settings.
    if !existing.trim().is_empty() && toml::from_str::<toml::Table>(&existing).is_err() {
        return Ok(());
    }

    let additions = codex_trust_additions(&existing, dir);
    if additions.is_empty() {
        return Ok(());
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&additions);

    // Final guard: only write if the merged document is valid TOML, so a stray
    // duplicate table (e.g. codex stored the path in a different quoting) can
    // never corrupt the config — worst case the trust prompt shows once.
    if toml::from_str::<toml::Table>(&updated).is_err() {
        return Ok(());
    }

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = config_path.with_extension("toml.vk-trust-tmp");
    std::fs::write(&tmp_path, updated.as_bytes())?;
    std::fs::rename(&tmp_path, &config_path)?;
    Ok(())
}

/// Pure helper: the `[projects."<key>"]` blocks to append for `dir` (and its
/// canonical form) that aren't already present in `existing`. Split out so the
/// "only add when absent" logic is unit-testable without touching real files.
fn codex_trust_additions(existing: &str, dir: &Path) -> String {
    let mut keys = vec![dir.to_string_lossy().into_owned()];
    if let Ok(canonical) = dir.canonicalize() {
        let canonical = canonical.to_string_lossy().into_owned();
        if !keys.contains(&canonical) {
            keys.push(canonical);
        }
    }

    let mut additions = String::new();
    for key in keys {
        // codex writes the table as `[projects."<path>"]`; only append when that
        // exact header is absent so we never create a duplicate table.
        let header = format!("[projects.\"{key}\"]");
        if !existing.contains(&header) {
            additions.push_str(&format!(
                "\n[projects.\"{key}\"]\ntrust_level = \"trusted\"\n"
            ));
        }
    }
    additions
}

/// Far-future sentinel written to codex's `version.json` `last_checked_at`, so
/// codex's throttled update check treats the cached result as fresh and never
/// re-checks (a re-check would re-surface the blocking "Update available" modal
/// with a newer version).
const CODEX_UPDATE_CHECK_FROZEN_AT: &str = "2099-01-01T00:00:00Z";

/// `$CODEX_HOME/version.json`, else `~/.codex/version.json` — codex's cached
/// update-check state.
fn codex_version_json_path() -> Option<PathBuf> {
    match std::env::var("CODEX_HOME") {
        Ok(home) if !home.trim().is_empty() => Some(PathBuf::from(home).join("version.json")),
        _ => dirs::home_dir().map(|home| home.join(".codex").join("version.json")),
    }
}

/// Defuse codex's blocking "Update available — 1. Update now / 2. Skip / 3. Skip
/// until next version · Press enter to continue" startup modal, which otherwise
/// stalls an unattended launch whenever the user's codex is behind latest
/// (verified live). We replicate codex's own "skip until next version" state in
/// `version.json`: mark the cached latest as dismissed and freeze the check
/// timestamp so codex won't re-check and re-prompt. Codex still shows a passive
/// one-line banner, but the TUI proceeds straight to the prompt. Per-process
/// memoized; best-effort (a failure just means the modal may show).
fn ensure_codex_update_nag_dismissed() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        if let Err(e) = seed_codex_update_dismissal() {
            tracing::debug!("Could not pre-dismiss codex update modal: {e}");
        }
    });
}

fn seed_codex_update_dismissal() -> std::io::Result<()> {
    let Some(path) = codex_version_json_path() else {
        return Ok(());
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        // Nothing cached yet (codex never ran) — nothing to pre-dismiss.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return Ok(());
    };
    let Some(obj) = value.as_object_mut() else {
        return Ok(());
    };
    // No known latest -> there's no modal to pre-dismiss.
    let Some(latest) = obj
        .get("latest_version")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
    else {
        return Ok(());
    };

    let already_dismissed = obj.get("dismissed_version").and_then(|v| v.as_str()) == Some(&latest);
    let already_frozen =
        obj.get("last_checked_at").and_then(|v| v.as_str()) == Some(CODEX_UPDATE_CHECK_FROZEN_AT);
    if already_dismissed && already_frozen {
        return Ok(());
    }

    obj.insert(
        "dismissed_version".to_string(),
        serde_json::Value::String(latest),
    );
    obj.insert(
        "last_checked_at".to_string(),
        serde_json::Value::String(CODEX_UPDATE_CHECK_FROZEN_AT.to_string()),
    );

    let serialized = serde_json::to_vec(&value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp_path = path.with_extension("json.vk-upd-tmp");
    std::fs::write(&tmp_path, &serialized)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// Configuration for the embedded tmux server, written next to the app's
/// other runtime assets and passed via `-f` so a fresh server never loads the
/// user's personal `~/.tmux.conf` (prefix remaps, status styling — and most
/// importantly `mouse` surprises) into the web terminal.
///
/// The choices reconcile "Windows-style" copying with wheel scrolling:
/// - `mouse on`: the wheel scrolls pane history via tmux copy-mode, and a
///   plain drag selects text tmux-side.
/// - `set-clipboard on` + the `clipboard` terminal feature: releasing a drag
///   copies the selection and emits OSC 52, which the web terminal's
///   clipboard addon forwards to the system clipboard — select-to-copy with
///   no keystroke.
/// - right-click is unbound from tmux's context menu so the web terminal can
///   use it for paste.
/// - `default-shell /bin/sh`: window command strings (our launch bootstrap)
///   are run via `default-shell -c`, and the bootstrap is POSIX sh
///   (`vk_p="$(cat …)"`, `{ …; } | …`) — a fish/csh login shell would fail on
///   it outright. The pane still ends in the USER'S shell: the bootstrap's
///   final `exec "${SHELL:-/bin/sh}"` honors `$SHELL`.
const CLI_TMUX_CONF: &str = "\
# BetterCoding embedded terminal tmux server (socket: vibe-kanban).
# Written by the backend before each CLI terminal attach - edits are overwritten.
set -g mouse on
set -s set-clipboard on
set -as terminal-features ',xterm*:clipboard'
set -g default-shell /bin/sh
unbind-key -n MouseDown3Pane
";

/// Write the embedded server config (idempotent) and return its path.
fn cli_tmux_conf_path() -> Option<PathBuf> {
    let dir = utils::assets::asset_dir();
    let path = dir.join("cli-tmux.conf");
    let current = std::fs::read_to_string(&path).ok();
    if current.as_deref() != Some(CLI_TMUX_CONF) {
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(&path, CLI_TMUX_CONF).ok()?;
    }
    Some(path)
}

/// Apply the embedded-server options to an ALREADY-RUNNING tmux server (the
/// `-f` config only applies to fresh server starts). Probes `set-clipboard`
/// first so the append-style options aren't re-applied on every attach.
/// Best-effort: no server running is the common case and simply a no-op.
fn ensure_cli_tmux_server_options() {
    let tmux = |args: &[&str]| {
        std::process::Command::new("tmux")
            .args(["-L", CLI_TMUX_SOCKET])
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
    };

    // default-shell governs how each NEW window command string (our POSIX-sh
    // launch bootstrap) is parsed; apply unconditionally (idempotent, no-op
    // without a running server) so servers started before this option joined
    // the conf don't hand the bootstrap to a fish/csh login shell.
    let _ = tmux(&["set-option", "-g", "default-shell", "/bin/sh"]);

    let Ok(probe) = tmux(&["show-options", "-s", "set-clipboard"]) else {
        return;
    };
    if !probe.status.success()
        || String::from_utf8_lossy(&probe.stdout).contains("set-clipboard on")
    {
        return;
    }

    let _ = tmux(&["set-option", "-g", "mouse", "on"]);
    let _ = tmux(&["set-option", "-s", "set-clipboard", "on"]);
    let _ = tmux(&[
        "set-option",
        "-as",
        "terminal-features",
        ",xterm*:clipboard",
    ]);
    let _ = tmux(&["unbind-key", "-n", "MouseDown3Pane"]);
}

/// Strict UUID check (8-4-4-4-12 hex). Used to vet a session id before it is
/// interpolated into the bootstrap shell command.
fn is_uuid(s: &str) -> bool {
    Uuid::parse_str(s).is_ok()
}

/// Bound on queued PTY output (chunks of up to 4KB). Full-screen TUI redraws
/// are chatty; if the WebSocket consumer stalls (throttled background tab),
/// the blocking send pauses the PTY reader instead of growing memory —
/// natural terminal backpressure with a ~1MB worst case per session.
const OUTPUT_CHANNEL_CAPACITY: usize = 256;

/// Dedicated tmux socket for all VibeKanban CLI sessions. Isolating onto our
/// own server (rather than the user's default tmux) means: our sessions never
/// collide with or appear in the user's personal tmux; a single
/// `tmux -L <socket> kill-server` can reclaim everything; and the long-lived
/// server that inherits the backend environment is clearly ours, not shared.
pub(crate) const CLI_TMUX_SOCKET: &str = "vibe-kanban";

/// tmux session name for a workspace's CLI-mode terminal. The `vk_` namespace
/// is ours: creation, attach, and cleanup only ever target these names.
/// `simple()` (32 hex chars, no hyphens) avoids tmux-special characters.
pub fn cli_tmux_session_name(workspace_id: Uuid) -> String {
    format!("vk_{}", workspace_id.simple())
}

/// Whether a workspace's CLI tmux session already exists. Lets the terminal
/// route consume the parked initial prompt ONLY on the genuine first attach:
/// reattaches (switch away/back) and post-tmux-death reconnects see no parked
/// prompt and the bootstrap's `--continue` fallback takes over instead of
/// replaying the prompt. `=` forces exact-name matching.
pub async fn cli_tmux_session_exists(workspace_id: Uuid) -> bool {
    if !tmux_available() {
        return false;
    }
    let session_name = cli_tmux_session_name(workspace_id);
    tmux_ok(&[
        "-L",
        CLI_TMUX_SOCKET,
        "has-session",
        "-t",
        &format!("={session_name}"),
    ])
    .await
}

/// Whether CLI mode can actually run claude in a tmux session (vs. degrading
/// to an ephemeral shell). The terminal route uses this to decide whether to
/// deliver+clear a parked CLI-first prompt: with tmux down the bootstrap
/// can't hand the prompt to claude, so the prompt must stay parked rather
/// than be cleared into the void. Because the result is process-cached, a
/// `true` here guarantees `create_session` also takes the tmux branch.
pub fn cli_tmux_available() -> bool {
    tmux_available()
}

/// Whether THE EXPECTED AGENT (`program`, the spec's binary name) currently
/// runs anywhere in a workspace's CLI pane process tree. `None` when the pane
/// itself can't be read (session gone).
///
/// tmux runs our bootstrap string via `default-shell -c`, and every launch
/// stage shares that shell's process group — so `#{pane_current_command}`
/// reports the outer shell for the pane's whole life (verified empirically)
/// and can't distinguish "agent running" from "fallback shell". Instead this
/// walks the pane's process SUBTREE (from `#{pane_pid}` down) looking for a
/// process whose comm IS the expected agent.
///
/// The whole subtree, not just the pane root and its direct children, because
/// node-wrapped agents run the native binary as a GRANDCHILD: `codex` ships as
/// a `#!/usr/bin/env node` launcher that `spawn`s the native `codex` binary, so
/// the pane tree is `sh → node → codex`. A shebang exec sets comm to the
/// interpreter (`node`), NOT the script basename (verified on a scratch
/// socket), and we deliberately do NOT accept the intermediate `node` as an
/// agent — so a direct-children-only probe would never confirm a node-wrapped
/// agent's delivery, stranding its parked prompt for replay on the next fresh
/// launch. Matching the EXACT program name (not "any non-shell") still keeps a
/// user's vim/npm inside the missing-binary fallback shell from satisfying the
/// paste gate.
pub async fn cli_pane_agent_running(workspace_id: Uuid, program: &str) -> Option<bool> {
    if !tmux_available() {
        return None;
    }
    let target = cli_tmux_session_name(workspace_id);
    let output = tokio::process::Command::new("tmux")
        .args([
            "-L",
            CLI_TMUX_SOCKET,
            "list-panes",
            "-t",
            &target,
            "-F",
            "#{pane_pid}",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let pane_pid = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .trim()
        .parse::<u32>()
        .ok()?;

    // One process snapshot, walked in-process: a single `ps` instead of an
    // unbounded fan-out of `pgrep -P` calls, and a consistent view of the tree.
    let snapshot = tokio::process::Command::new("ps")
        .args(["-eo", "pid=,ppid=,comm="])
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !snapshot.status.success() {
        return None;
    }
    let listing = String::from_utf8_lossy(&snapshot.stdout);
    Some(pane_subtree_has_program(&listing, pane_pid, program))
}

/// Whether `program` runs anywhere in the process subtree rooted at `root_pid`
/// (inclusive), given a `ps -eo pid=,ppid=,comm=` snapshot. Pure so the tree
/// walk — the part that decides whether a node-wrapped agent grandchild counts
/// — is unit testable without a live process tree.
fn pane_subtree_has_program(ps_listing: &str, root_pid: u32, program: &str) -> bool {
    let mut comm_by_pid: HashMap<u32, &str> = HashMap::new();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for line in ps_listing.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(comm)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        comm_by_pid.insert(pid, comm);
        children.entry(ppid).or_default().push(pid);
    }
    // Depth-first from the pane root, inclusive. `visited` guards against a
    // malformed snapshot: a real ppid graph is a forest and can't cycle, but a
    // torn read must not spin.
    let mut stack = vec![root_pid];
    let mut visited = HashSet::new();
    while let Some(pid) = stack.pop() {
        if !visited.insert(pid) {
            continue;
        }
        if comm_by_pid
            .get(&pid)
            .is_some_and(|comm| comm_matches_program(comm, program))
        {
            return true;
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids);
        }
    }
    false
}

/// Whether a `ps`/`pgrep` process name is the expected agent binary. The
/// kernel truncates comm to 15 bytes (`TASK_COMM_LEN` - 1), so long program
/// names match on their truncated prefix; comparison happens on the
/// normalized basename so `/usr/local/bin/claude` and `claude` agree.
fn comm_matches_program(comm: &str, program: &str) -> bool {
    let comm = normalize_comm(comm);
    let program = normalize_comm(program);
    comm == program
        || (program.len() > 15 && program.get(..15).is_some_and(|prefix| comm == prefix))
}

/// Normalize a `ps` process name for comparison: login shells report as
/// `-zsh`, and macOS `ps -o comm=` can report a full path.
fn normalize_comm(comm: &str) -> &str {
    let comm = comm.trim().trim_start_matches('-');
    comm.rsplit('/').next().unwrap_or(comm)
}

/// Inverse of [`cli_tmux_session_name`]: recover the workspace id from one of
/// our tmux session names. Returns `None` for anything outside the `vk_`
/// namespace (e.g. a user-created session on the same socket).
pub(crate) fn workspace_id_from_cli_session_name(name: &str) -> Option<Uuid> {
    let hex = name.strip_prefix("vk_")?;
    if hex.len() != 32 {
        return None;
    }
    Uuid::parse_str(hex).ok()
}

/// Best-effort kill of a workspace's CLI tmux session (used on workspace
/// cleanup so sessions don't outlive their worktree). `=` forces exact-name
/// matching — tmux `-t` is otherwise a prefix match.
pub async fn kill_cli_tmux_session(workspace_id: Uuid) {
    if tmux_available() {
        let session_name = cli_tmux_session_name(workspace_id);
        match tokio::process::Command::new("tmux")
            .args([
                "-L",
                CLI_TMUX_SOCKET,
                "kill-session",
                "-t",
                &format!("={session_name}"),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
        {
            // Non-success simply means no such session — the common case.
            Ok(_) => {}
            Err(e) => tracing::debug!("Failed to run tmux kill-session for {session_name}: {e}"),
        }
    }
    // Drop any transient prompt file AFTER the session is dead (covers the
    // never-attached case where the bootstrap never ran to self-delete it).
    // Order matters: removing the file first could yank it from under a
    // bootstrap that is between `command -v` and `cat` — launching the agent
    // with an empty prompt while a concurrent delivery confirmation reads
    // "file gone + agent up" as delivered and clears the parked DB copy.
    remove_cli_prompt_file(workspace_id);
}

/// Capture the visible content of a workspace's CLI tmux pane (best-effort).
/// Returns `None` when tmux is down or the session doesn't exist. The loop
/// supervisor uses this to spot usage/rate-limit banners and to tell whether
/// the agent is idle — CLI pane output is otherwise ephemeral (streamed to the
/// browser, never persisted), so this is the only server-side view of it.
pub async fn capture_cli_pane(workspace_id: Uuid) -> Option<String> {
    if !tmux_available() {
        return None;
    }
    // Pane-targeting commands (capture-pane / send-keys) reject the `=exact`
    // session-target syntax; they take a pane target. The full 32-hex session
    // name can't be a prefix of any other `vk_*` session, so the bare name
    // resolves unambiguously to this session's (sole) pane.
    let session_name = cli_tmux_session_name(workspace_id);
    let output = tokio::process::Command::new("tmux")
        .args([
            "-L",
            CLI_TMUX_SOCKET,
            "capture-pane",
            "-p",
            "-t",
            &session_name,
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Texts at/above this size go through tmux buffers (load-buffer + paste-buffer)
/// instead of `send-keys -l`, which — like every tmux client command — is
/// rejected once its argv exceeds ~16KB. The small-text `send-keys` path is the
/// already-live-verified one, so keep it for the common case.
const SEND_KEYS_PASTE_THRESHOLD: usize = 4096;

/// Run a fire-and-forget tmux command on our socket, reporting only success.
async fn tmux_ok(args: &[&str]) -> bool {
    tokio::process::Command::new("tmux")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Whether `text` must ride the buffer/bracketed-paste transport instead of
/// plain `send-keys -l`: oversized texts hit tmux's ~16KB client-command
/// ceiling, and MULTI-LINE texts sent as literal keystrokes would submit at
/// the first newline (each `\n` acts as Enter to the TUI) — mangling the
/// message while still reporting success. Single-line small texts keep the
/// live-verified `send-keys -l` path.
fn needs_paste_transport(text: &str) -> bool {
    text.len() >= SEND_KEYS_PASTE_THRESHOLD || text.contains('\n')
}

/// Type `text` into a workspace's CLI tmux pane and submit it (Enter), as if the
/// user typed it. This is the only way to re-prompt a LIVE, detached agent: the
/// parked `pending_cli_prompt` path only fires when a fresh tmux session is
/// created, so an already-running pane needs keystroke injection. Small
/// single-line texts go via `send-keys -l` (literal, so never interpreted as
/// tmux key names); larger or multi-line texts are staged through a namespaced
/// tmux buffer and bracketed-pasted ([`needs_paste_transport`]). Enter is a
/// separate call so it submits rather than being typed verbatim. Best-effort.
pub async fn send_cli_keys(workspace_id: Uuid, text: &str) -> bool {
    if !tmux_available() {
        return false;
    }
    // Bare name (not `=exact`): send-keys/paste-buffer take a pane target, for
    // which the `=` session-target syntax is rejected. Unambiguous given
    // full-hex names.
    let target = cli_tmux_session_name(workspace_id);

    let delivered = if needs_paste_transport(text) {
        paste_via_tmux_buffer(workspace_id, &target, text).await
    } else {
        tmux_ok(&[
            "-L",
            CLI_TMUX_SOCKET,
            "send-keys",
            "-t",
            &target,
            "-l",
            text,
        ])
        .await
    };
    if !delivered {
        return false;
    }

    // The text is already IN the pane; failing the whole send over a flaky
    // Enter would make the caller re-deliver the text on top of the residue
    // (a doubled prompt). Retry the Enter once, then accept: delivered (the
    // user can press Enter themselves), submission best-effort.
    for _ in 0..2 {
        if tmux_ok(&["-L", CLI_TMUX_SOCKET, "send-keys", "-t", &target, "Enter"]).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    tracing::warn!(
        "CLI send to workspace {workspace_id}: text delivered but Enter failed; \
         left unsubmitted in the pane"
    );
    true
}

/// Stage `text` into a per-workspace tmux buffer via `load-buffer -` (text on
/// stdin, so no argv-length limit) and bracketed-paste it into the pane. The
/// buffer is namespaced (`vk_prompt_<wsid>`) and deleted on paste (`-d`) because
/// tmux buffers are server-global — otherwise concurrent workspaces could
/// cross-deliver. `-p` (bracketed paste) makes the TUI treat multi-line text as
/// one paste so embedded newlines don't submit early.
async fn paste_via_tmux_buffer(workspace_id: Uuid, target: &str, text: &str) -> bool {
    use tokio::io::AsyncWriteExt;

    // Per-send sequence number on top of the workspace namespace: two
    // concurrent sends to the SAME workspace (e.g. a loop wake-up racing a
    // deferred initial-prompt delivery) must not overwrite each other's buffer
    // between load and paste.
    static SEND_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEND_SEQ.fetch_add(1, Ordering::Relaxed);
    let buffer = format!("vk_prompt_{}_{seq}", workspace_id.simple());

    let mut child = match tokio::process::Command::new("tmux")
        .args(["-L", CLI_TMUX_SOCKET, "load-buffer", "-b", &buffer, "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };

    let Some(mut stdin) = child.stdin.take() else {
        // Piped stdin should always be there; a missing handle means the
        // buffer can't be loaded, so reap the child and report failure rather
        // than pasting an empty buffer.
        let _ = child.kill().await;
        return false;
    };
    if stdin.write_all(text.as_bytes()).await.is_err() {
        // Reap the load-buffer child before bailing: dropping the Child does
        // not wait() it, so a failed stdin write would otherwise leave a
        // lingering/zombie tmux process. Drop stdin first (EOF), then kill
        // (which also awaits the exit).
        drop(stdin);
        let _ = child.kill().await;
        return false;
    }
    // Drop stdin (EOF) so load-buffer completes.
    let _ = stdin.shutdown().await;
    drop(stdin);

    let loaded = child.wait().await.map(|s| s.success()).unwrap_or(false);
    if !loaded {
        return false;
    }

    let pasted = tmux_ok(&[
        "-L",
        CLI_TMUX_SOCKET,
        "paste-buffer",
        "-d",
        "-p",
        "-b",
        &buffer,
        "-t",
        target,
    ])
    .await;
    if !pasted {
        // `-d` only fires on a successful paste; don't leave the staged prompt
        // readable in the server-global buffer list after a failed one.
        tmux_ok(&["-L", CLI_TMUX_SOCKET, "delete-buffer", "-b", &buffer]).await;
    }
    pasted
}

/// Seconds since the Unix epoch (best-effort; 0 if the system clock is before
/// the epoch). Used to turn tmux's `session_activity` epoch into an idle age.
fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// List our CLI tmux sessions for the reaper: `(workspace_id, attached,
/// idle_secs)` for every `vk_*` session on our socket. Returns empty when tmux
/// is unavailable or no server is running (both mean "nothing to reap"). Idle is
/// derived from tmux `session_activity`; non-`vk_` sessions are ignored so we
/// never touch a user's own session on the same socket.
pub async fn list_cli_tmux_sessions() -> Vec<(Uuid, bool, i64)> {
    if !tmux_available() {
        return Vec::new();
    }
    let output = tokio::process::Command::new("tmux")
        .args([
            "-L",
            CLI_TMUX_SOCKET,
            "list-sessions",
            "-F",
            "#{session_name}\t#{session_attached}\t#{session_activity}",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await;
    let stdout = match output {
        Ok(o) if o.status.success() => o.stdout,
        // Non-zero exit (no server / no sessions) or spawn error => nothing to reap.
        _ => return Vec::new(),
    };
    let now = now_unix_secs();
    String::from_utf8_lossy(&stdout)
        .lines()
        .filter_map(|line| parse_cli_session_line(line, now))
        .collect()
}

/// Parse one `name\tattached\tactivity` tmux row into `(workspace_id, attached,
/// idle_secs)`, skipping anything outside the `vk_` namespace or malformed.
fn parse_cli_session_line(line: &str, now: i64) -> Option<(Uuid, bool, i64)> {
    let mut parts = line.split('\t');
    let workspace_id = workspace_id_from_cli_session_name(parts.next()?)?;
    let attached = parts.next()?.trim() != "0";
    let activity: i64 = parts.next()?.trim().parse().ok()?;
    Some((workspace_id, attached, (now - activity).max(0)))
}

/// Fresh `(attached, idle_secs)` for one CLI session, or `None` if it no longer
/// exists. The reaper calls this immediately before killing as a TOCTOU recheck,
/// so a session that was attached or became active since the list snapshot is
/// spared.
///
/// Implemented by re-listing rather than `tmux display-message`: display-message
/// resolves formats in a client/pane context and returns EMPTY for the
/// session-scoped `#{session_attached}` / `#{session_activity}` there, which made
/// this recheck always parse to `None` and silently disabled the whole reaper.
/// `list-sessions` (used here) populates those fields correctly, and already
/// no-ops cleanly when tmux is unavailable.
pub async fn cli_tmux_session_liveness(workspace_id: Uuid) -> Option<(bool, i64)> {
    list_cli_tmux_sessions()
        .await
        .into_iter()
        .find(|(id, _, _)| *id == workspace_id)
        .map(|(_, attached, idle)| (attached, idle))
}

/// Whether tmux is on PATH. Checked once per process; when unavailable
/// (e.g. Windows, minimal containers) CLI mode degrades to a bare shell.
pub(crate) fn tmux_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let available = std::process::Command::new("tmux")
            .arg("-V")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !available {
            tracing::warn!(
                "tmux not found on PATH; CLI mode terminals will degrade to ephemeral shells"
            );
        }
        available
    })
}

/// Visible notice written into a CLI pane when tmux is unavailable, so the
/// "persistent session" promise is never silently broken.
const TMUX_MISSING_NOTICE: &[u8] =
    b"\x1b[33m\xe2\x9a\xa0 tmux not found \xe2\x80\x94 running an ephemeral shell; this session will NOT survive disconnects.\x1b[0m\r\n";

/// User-facing copy shown (in red, halting the reconnect loop) whenever a CLI
/// attach fails in a way that leaves the parked prompt intact for the next
/// attach. Shared so the "prompt staging failed" and "session never came up"
/// recovery paths speak with one voice.
pub const CLI_PROMPT_PARKED_NOTICE: &str = "Failed to start the agent session — your prompt is saved and will be delivered on the next attach";

#[derive(Debug, Error)]
pub enum PtyError {
    #[error("Failed to create PTY: {0}")]
    CreateFailed(String),
    /// A parked initial prompt could not be staged for delivery (e.g. the
    /// prompt file write failed on a full/read-only FS). Distinct from
    /// `CreateFailed` so the message reaches the user verbatim — and so we
    /// never silently drop the prompt by falling through to an empty
    /// `continue_launch` TUI. The parked DB copy is left untouched for retry.
    /// A unit variant whose display IS the shared recovery notice, so every
    /// construction site speaks the same user-facing copy by construction.
    #[error("{}", CLI_PROMPT_PARKED_NOTICE)]
    PromptStageFailed,
    #[error("Session not found: {0}")]
    SessionNotFound(Uuid),
    #[error("Failed to write to PTY: {0}")]
    WriteFailed(String),
    #[error("Failed to resize PTY: {0}")]
    ResizeFailed(String),
    #[error("Session already closed")]
    SessionClosed,
}

struct PtySession {
    /// Per-session writer behind its own lock so a blocking PTY write never
    /// holds up the global session registry (see `write`).
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    /// Kills the PTY child (tmux client / shell) on teardown. Required because
    /// dropping the master does not close the reader thread's *cloned* reader,
    /// so without an explicit kill the reader blocks on `read()` forever and
    /// the child is never reaped on disconnect. For CLI mode this detaches the
    /// tmux client (the session persists on the server); for a bare shell it
    /// ends the ephemeral shell.
    child_killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    /// Set by the reader thread once it has reaped the child via `wait()`.
    /// `close_session` checks this before signalling so it never targets a PID
    /// that was already reaped (and possibly recycled) on the natural-exit path.
    child_reaped: Arc<AtomicBool>,
    _output_handle: thread::JoinHandle<()>,
    closed: bool,
}

#[derive(Clone)]
pub struct PtyService {
    sessions: Arc<Mutex<HashMap<Uuid, PtySession>>>,
}

impl PtyService {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn create_session(
        &self,
        working_dir: PathBuf,
        cols: u16,
        rows: u16,
        command: PtyCommand,
    ) -> Result<(Uuid, mpsc::Receiver<Vec<u8>>), PtyError> {
        let session_id = Uuid::new_v4();
        let (output_tx, output_rx) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
        let shell = get_interactive_shell().await;

        let result = tokio::task::spawn_blocking(move || {
            let pty_system = NativePtySystem::default();

            let pty_pair = pty_system
                .openpty(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| PtyError::CreateFailed(e.to_string()))?;

            // CLI mode rides tmux when present; otherwise (and for the
            // default side terminal) spawn the user's shell directly.
            let (tmux_workspace, tmux_resume_id, tmux_initial_prompt, tmux_deferred, tmux_spec): (
                Option<Uuid>,
                Option<String>,
                Option<String>,
                bool,
                Option<CliLaunchSpec>,
            ) = match &command {
                PtyCommand::TmuxCli {
                    workspace_id,
                    resume_session_id,
                    initial_prompt,
                    deferred_prompt_pending,
                    spec,
                } if tmux_available() => (
                    Some(*workspace_id),
                    resume_session_id.clone(),
                    initial_prompt.clone(),
                    *deferred_prompt_pending,
                    Some(spec.clone()),
                ),
                _ => (None, None, None, false, None),
            };

            // Never silently break the persistence promise: if CLI mode was
            // requested but tmux is absent, say so in the pane itself.
            if matches!(&command, PtyCommand::TmuxCli { .. }) {
                match tmux_workspace {
                    Some(workspace_id) => tracing::info!(
                        "CLI terminal attaching tmux session {} in {}",
                        cli_tmux_session_name(workspace_id),
                        working_dir.display()
                    ),
                    None => {
                        let _ = output_tx.blocking_send(TMUX_MISSING_NOTICE.to_vec());
                    }
                }
            }

            let (mut cmd, shell_name) = if let (Some(workspace_id), Some(spec)) =
                (tmux_workspace, &tmux_spec)
            {
                let session_name = cli_tmux_session_name(workspace_id);
                // Bring an already-running server in line with our config
                // (options are server-wide; `-f` below only affects a fresh
                // server start).
                ensure_cli_tmux_server_options();

                // Pre-accept the agent's per-directory folder-trust / first-run
                // dialog for this app-created worktree so the launch never
                // blocks on it.
                maybe_seed_cli_trust(&spec.program, &working_dir);

                let mut cmd = CommandBuilder::new("tmux");
                // Our own config instead of the user's ~/.tmux.conf — the
                // embedded terminal needs deterministic mouse/clipboard
                // behavior (see CLI_TMUX_CONF); the user's personal tmux on
                // the default socket is unaffected.
                if let Some(conf) = cli_tmux_conf_path() {
                    cmd.arg("-f");
                    cmd.arg(conf);
                }
                // Dedicated socket isolates our sessions from the user's tmux.
                cmd.arg("-L");
                cmd.arg(CLI_TMUX_SOCKET);
                cmd.arg("new-session");
                // -A: attach if the session exists, else create.
                //
                // We deliberately do NOT pass -D (detach other clients): a new
                // attach would detach the prior client, whose tmux process then
                // exits → its PTY hits EOF → the WebSocket closes → the frontend
                // reconnects → the new attach detaches it again, a self-
                // sustaining reconnect loop that also resets the session to the
                // attaching client's 80x24 default on every cycle. Without -D,
                // reconnects simply attach; the prior client is cleaned up by
                // close_session killing its PTY child. Two simultaneous browser
                // windows would mirror (tmux sizes to the smaller) — a rare,
                // benign trade vs. the loop.
                cmd.arg("-A");
                cmd.arg("-s");
                cmd.arg(&session_name);
                cmd.arg("-c");
                cmd.arg(&working_dir);
                // Materialize the initial prompt to a private file so the
                // bootstrap reads it back rather than carrying it inline
                // (tmux rejects `new-session` commands past ~16KB). Only when
                // an existing conversation won't take precedence (the same
                // predicate the bootstrap applies) and the prompt isn't blank;
                // the file self-deletes once consumed.
                let resume_active = active_resume_id(tmux_resume_id.as_deref()).is_some();
                let prompt_file: Option<PathBuf> = if resume_active {
                    None
                } else {
                    match tmux_initial_prompt
                        .as_deref()
                        .and_then(|p| cli_prompt_file_content(&spec.prompt_arg, p))
                    {
                        // There is a prompt to deliver: its file MUST be staged.
                        // If the write fails we CANNOT fall through to
                        // `continue_launch` — that yields a healthy-looking but
                        // empty TUI that could then be mistaken for delivery.
                        // Fail the spawn instead so the parked prompt survives
                        // for the next attach and the user sees the recovery
                        // notice (the "never destroy the prompt" invariant).
                        Some(content) => {
                            Some(write_cli_prompt_file(workspace_id, &content).map_err(|e| {
                                tracing::error!(
                                    "Failed to write CLI prompt file for {session_name}: \
                                     {e}; leaving prompt parked"
                                );
                                // Drop any partial file so a torn write can't
                                // leave a stale prompt readable on disk.
                                remove_cli_prompt_file(workspace_id);
                                PtyError::PromptStageFailed
                            })?)
                        }
                        None => None,
                    }
                };
                cmd.arg(cli_bootstrap(
                    spec,
                    tmux_resume_id.as_deref(),
                    prompt_file.as_deref(),
                    tmux_deferred,
                ));
                cmd.cwd(&working_dir);
                // No shell-specific prompt configuration for the tmux client.
                (cmd, String::new())
            } else {
                let mut cmd = CommandBuilder::new(&shell);
                cmd.cwd(&working_dir);

                // Configure shell-specific options
                let shell_name = shell
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();

                if shell_name == "powershell.exe" || shell_name == "pwsh.exe" {
                    // PowerShell: use -NoLogo for cleaner startup
                    cmd.arg("-NoLogo");
                } else if shell_name == "cmd.exe" {
                    // cmd.exe: no special args needed
                } else {
                    // Unix shells
                    cmd.env("VIBE_KANBAN_TERMINAL", "1");

                    if shell_name == "bash" {
                        cmd.env("PROMPT_COMMAND", r#"PS1='$ '; unset PROMPT_COMMAND"#);
                    } else if shell_name == "zsh" {
                        // PROMPT is set after spawning
                    } else {
                        cmd.env("PS1", "$ ");
                    }
                }
                (cmd, shell_name)
            };

            cmd.env("TERM", "xterm-256color");
            cmd.env("COLORTERM", "truecolor");

            let child = pty_pair
                .slave
                .spawn_command(cmd)
                .map_err(|e| PtyError::CreateFailed(e.to_string()))?;

            // Independent kill handle so close_session can unblock the reader.
            let child_killer = child.clone_killer();
            let child_reaped = Arc::new(AtomicBool::new(false));
            let child_reaped_reader = child_reaped.clone();

            let mut writer = pty_pair
                .master
                .take_writer()
                .map_err(|e| PtyError::CreateFailed(e.to_string()))?;

            if shell_name == "zsh" {
                let _ = writer.write_all(b" PROMPT='$ '; RPROMPT=''\n");
                let _ = writer.flush();
                let _ = writer.write_all(b"\x0c");
                let _ = writer.flush();
            }

            let mut reader = pty_pair
                .master
                .try_clone_reader()
                .map_err(|e| PtyError::CreateFailed(e.to_string()))?;

            let output_handle = thread::spawn(move || {
                let mut child = child;
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            // Blocking send: a stalled WebSocket consumer pauses
                            // this reader (PTY backpressure) instead of queueing
                            // unbounded TUI redraw output.
                            if output_tx.blocking_send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                // Reap the child: dropping a Child does NOT wait(), and an
                // unreaped PTY child leaves one zombie per disconnect until
                // the server exits (observed live as a defunct tmux client).
                let _ = child.wait();
                // Mark reaped so close_session won't signal a freed/recycled PID.
                child_reaped_reader.store(true, Ordering::Release);
            });

            Ok::<_, PtyError>((
                pty_pair.master,
                writer,
                child_killer,
                child_reaped,
                output_handle,
            ))
        })
        .await
        .map_err(|e| PtyError::CreateFailed(e.to_string()))??;

        let (master, writer, child_killer, child_reaped, output_handle) = result;

        let session = PtySession {
            writer: Arc::new(Mutex::new(writer)),
            master,
            child_killer,
            child_reaped,
            _output_handle: output_handle,
            closed: false,
        };

        self.sessions
            .lock()
            .map_err(|e| PtyError::CreateFailed(e.to_string()))?
            .insert(session_id, session);

        Ok((session_id, output_rx))
    }

    pub async fn write(&self, session_id: Uuid, data: &[u8]) -> Result<(), PtyError> {
        // Clone the per-session writer handle under the registry lock, then
        // do the PTY write OUTSIDE it. A PTY whose consumer stalls can block
        // `write_all` indefinitely; holding the global registry lock across
        // that would freeze every other terminal — including new attaches —
        // with no errors anywhere. With the per-session lock, a wedged PTY
        // only ever blocks its own session.
        let writer = {
            let sessions = self
                .sessions
                .lock()
                .map_err(|e| PtyError::WriteFailed(e.to_string()))?;
            let session = sessions
                .get(&session_id)
                .ok_or(PtyError::SessionNotFound(session_id))?;

            if session.closed {
                return Err(PtyError::SessionClosed);
            }

            session.writer.clone()
        };

        let mut writer = writer
            .lock()
            .map_err(|e| PtyError::WriteFailed(e.to_string()))?;

        writer
            .write_all(data)
            .map_err(|e| PtyError::WriteFailed(e.to_string()))?;

        writer
            .flush()
            .map_err(|e| PtyError::WriteFailed(e.to_string()))?;

        Ok(())
    }

    pub async fn resize(&self, session_id: Uuid, cols: u16, rows: u16) -> Result<(), PtyError> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| PtyError::ResizeFailed(e.to_string()))?;
        let session = sessions
            .get(&session_id)
            .ok_or(PtyError::SessionNotFound(session_id))?;

        if session.closed {
            return Err(PtyError::SessionClosed);
        }

        session
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::ResizeFailed(e.to_string()))?;

        Ok(())
    }

    pub async fn close_session(&self, session_id: Uuid) -> Result<(), PtyError> {
        if let Some(mut session) = self
            .sessions
            .lock()
            .map_err(|_| PtyError::SessionClosed)?
            .remove(&session_id)
        {
            // Kill the PTY child so a read-parked reader sees EOF, exits, and
            // reaps it. Without this the reader blocks on its cloned reader
            // forever (dropping the master here doesn't close that clone),
            // leaking a thread + an unreaped child per disconnect. (A
            // send-parked reader is instead released when the caller drops the
            // output receiver after this returns.) Skip the signal if the
            // reader already reaped on the natural-exit path, so we never
            // SIGHUP a freed/recycled PID.
            if !session.child_reaped.load(Ordering::Acquire) {
                let _ = session.child_killer.kill();
            }
        }
        Ok(())
    }
}

impl Default for PtyService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_session_names_are_namespaced_and_tmux_safe() {
        let id = Uuid::parse_str("bccad5cc-3bd4-4f80-b75d-35db5f087ac0").unwrap();
        let name = cli_tmux_session_name(id);
        assert_eq!(name, "vk_bccad5cc3bd44f80b75d35db5f087ac0");
        // tmux treats `.` and `:` specially in targets; the name must stay
        // strictly alphanumeric + underscore.
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert!(name.starts_with("vk_"));
    }

    /// A claude-shaped spec (flag resume, positional prompt, `--continue`
    /// fallback) — mirrors `ClaudeCode::interactive_cli_spec`.
    fn claude_spec(base_args: &[&str]) -> CliLaunchSpec {
        CliLaunchSpec::new("claude", base_args.iter().map(|s| s.to_string()).collect())
            .with_resume(CliResume::Flag("--resume".to_string()))
            .with_prompt_arg(CliPromptArg::Positional)
            .with_continue(CliContinue::Flag("--continue".to_string()))
    }

    /// A codex-shaped spec (subcommand resume, positional prompt, `resume
    /// --last` fallback) — mirrors `Codex::interactive_cli_spec`.
    fn codex_spec(base_args: &[&str]) -> CliLaunchSpec {
        CliLaunchSpec::new("codex", base_args.iter().map(|s| s.to_string()).collect())
            .with_resume(CliResume::Subcommand("resume".to_string()))
            .with_prompt_arg(CliPromptArg::Positional)
            .with_continue(CliContinue::ResumeLast {
                subcommand: "resume".to_string(),
            })
    }

    #[test]
    fn cli_bootstrap_runs_program_then_drops_to_shell() {
        let b = cli_bootstrap(&claude_spec(&[]), None, None, false);
        assert!(b.contains("command -v 'claude'"));
        assert!(
            b.ends_with(r#"exec "${SHELL:-/bin/sh}""#),
            "bootstrap must keep the pane alive after the agent exits"
        );
    }

    #[test]
    fn cli_bootstrap_warns_with_install_hint_when_agent_missing() {
        // A not-installed agent must explain itself instead of silently dropping
        // to a bare shell.
        let b = cli_bootstrap(&claude_spec(&[]), None, None, false);
        assert!(b.contains("if command -v 'claude'"));
        assert!(b.contains("is not installed or not on PATH"));
        assert!(
            b.contains("npm i -g @anthropic-ai/claude-code"),
            "missing-agent notice should carry the install hint: {b}"
        );
        // The pane is still left usable as a shell.
        assert!(b.ends_with(r#"exec "${SHELL:-/bin/sh}""#));
    }

    #[test]
    fn cli_bootstrap_resume_takes_precedence_and_rejects_non_uuids() {
        // A valid session UUID -> --resume <id>, even if a prompt is also
        // present (an existing conversation always wins).
        let id = "28b98f08-5f5f-4b1e-8c4e-41ae87c0c706";
        let b = cli_bootstrap(
            &claude_spec(&[]),
            Some(id),
            Some(Path::new("/tmp/vk/prompt.txt")),
            false,
        );
        assert!(b.contains(&format!("--resume {id}")));
        // The prompt file is ignored entirely when resuming.
        assert!(!b.contains("prompt.txt"));
        assert!(!b.contains("vk_p="));
        // Non-UUID (injection attempt) is rejected and never interpolated.
        let evil = "x; rm -rf ~";
        let b = cli_bootstrap(&claude_spec(&[]), Some(evil), None, false);
        assert!(!b.contains("rm -rf"));
        assert!(!b.contains("--resume"));
    }

    #[test]
    fn cli_bootstrap_codex_resume_is_a_subcommand_without_base_flags() {
        // codex resumes via a subcommand that restores the session's own
        // settings, so the model/sandbox/approval flags are NOT replayed.
        let id = "28b98f08-5f5f-4b1e-8c4e-41ae87c0c706";
        let spec = codex_spec(&["-m", "gpt-5.5", "-s", "danger-full-access"]);
        let b = cli_bootstrap(&spec, Some(id), None, false);
        assert!(b.contains(&format!("'codex' resume {id}")));
        assert!(
            !b.contains("-m"),
            "base flags must not ride the resume: {b}"
        );
        // Continue fallback uses `resume --last`, falling back to a fresh TUI.
        let cont = cli_bootstrap(&spec, None, None, false);
        assert!(cont.contains("'codex' resume --last || 'codex'"));
    }

    #[test]
    fn cli_bootstrap_reads_prompt_from_file_length_is_constant() {
        // The prompt is delivered via a temp file, so the generated command is
        // O(1) in prompt size — the whole point of the fix (tmux rejects
        // commands past ~16KB). The file PATH is single-quoted; the content is
        // only ever expanded inside double quotes, so it can never be
        // word-split or parsed as shell (injection-safe by construction).
        let spec = claude_spec(&["--dangerously-skip-permissions"]);
        let file = Path::new("/tmp/vk/cli-prompts/abc.txt");
        let b = cli_bootstrap(&spec, None, Some(file), false);
        assert!(
            b.len() < 2048,
            "bootstrap must stay small regardless of prompt size: {} bytes",
            b.len()
        );
        // Path single-quoted, expansion double-quoted, file self-deletes.
        assert!(b.contains("vk_p=\"$(cat '/tmp/vk/cli-prompts/abc.txt')\""));
        assert!(b.contains("rm -f -- '/tmp/vk/cli-prompts/abc.txt'"));
        assert!(
            b.contains("'--dangerously-skip-permissions' \"$vk_p\""),
            "positional prompt expands double-quoted after the flags: {b}"
        );

        // A prompt file whose path contains a quote can't break out of the
        // single-quoting (defense in depth; real paths are workspace hex): the
        // embedded quote is POSIX-escaped as `'\''`, so the dangerous run stays
        // inert data inside the quoting rather than terminating it.
        let evil = Path::new("/tmp/'; rm -rf ~; echo '.txt");
        let b = cli_bootstrap(&spec, None, Some(evil), false);
        assert!(
            b.contains(r"'\''; rm -rf ~; echo '\''"),
            "path quote must be escaped, not terminated: {b}"
        );
        // The raw, unescaped break-out (a bare `'` closing the cat quote right
        // before the command) must never appear.
        assert!(
            !b.contains("cat '/tmp/'; rm"),
            "quoting must not break out: {b}"
        );
    }

    #[test]
    fn cli_bootstrap_flag_and_stdin_prompt_forms_read_from_file() {
        let file = Path::new("/tmp/vk/p.txt");

        // Flag agents expand the file into the flag's value, double-quoted;
        // the flag itself is quoted like every other word we emit.
        let flag_spec = CliLaunchSpec::new("gemini", vec![])
            .with_prompt_arg(CliPromptArg::Flag("-i".to_string()));
        let b = cli_bootstrap(&flag_spec, None, Some(file), false);
        assert!(b.contains("rm -f -- '/tmp/vk/p.txt'; 'gemini' '-i' \"$vk_p\""));

        // StdinPipe agents pipe the file into the program — no argv ceiling.
        // The `rm` runs inside the producer group, right after `cat` streams
        // the file, so consumption is acknowledged (file gone) immediately —
        // not when the agent eventually exits.
        let pipe_spec = CliLaunchSpec::new("amp", vec![]).with_prompt_arg(CliPromptArg::StdinPipe);
        let b = cli_bootstrap(&pipe_spec, None, Some(file), false);
        assert!(b.contains("{ cat '/tmp/vk/p.txt'; rm -f -- '/tmp/vk/p.txt'; } | 'amp'"));
    }

    #[test]
    fn cli_bootstrap_no_prompt_file_falls_through_to_continue() {
        // No prompt file (blank prompt filtered out by the caller) -> the
        // no-prompt continue/fresh path, exactly as before.
        let spec = claude_spec(&["--dangerously-skip-permissions"]);
        let b = cli_bootstrap(&spec, None, None, false);
        assert!(b.contains("--continue || 'claude'"));
        assert!(!b.contains("vk_p="));
    }

    #[test]
    fn cli_bootstrap_deferred_prompt_launches_fresh_tui() {
        // A paste delivery follows this launch: NO `--continue` (its doomed
        // first leg on a brand-new workspace would live just long enough to
        // swallow the paste and exit), just the bare agent TUI.
        let spec = claude_spec(&["--dangerously-skip-permissions"]);
        let b = cli_bootstrap(&spec, None, None, true);
        assert!(!b.contains("--continue"), "no doomed continue leg: {b}");
        assert!(b.contains("'claude' '--dangerously-skip-permissions'"));
        // Resume still wins over a pending deferred paste at launch time.
        let id = "28b98f08-5f5f-4b1e-8c4e-41ae87c0c706";
        let b = cli_bootstrap(&spec, Some(id), None, true);
        assert!(b.contains(&format!("--resume {id}")));
    }

    #[test]
    fn route_followup_prompt_always_pastes_or_drops() {
        // Follow-up delivery (live session / post-resume) is never baked.
        assert_eq!(
            route_followup_prompt("  keep going  "),
            CliPromptRouting::Deferred("keep going".to_string())
        );
        assert_eq!(route_followup_prompt("   \n\t"), CliPromptRouting::None);
    }

    #[test]
    fn multiline_or_oversized_text_takes_the_paste_transport() {
        // `send-keys -l` types newlines as Enter keystrokes — a multi-line
        // text would submit at its first line — so anything with a newline
        // rides the bracketed-paste buffer path regardless of size.
        assert!(!needs_paste_transport("single line"));
        assert!(needs_paste_transport("two\nlines"));
        assert!(needs_paste_transport(&"x".repeat(4096)));
        assert!(!needs_paste_transport(&"x".repeat(4095)));
    }

    #[test]
    fn cli_prompt_file_content_matches_old_quoting_semantics() {
        // Blank (after trim) -> no file is written (falls through to continue).
        assert_eq!(
            cli_prompt_file_content(&CliPromptArg::Positional, "   "),
            None
        );

        // Positional: stored verbatim, byte-exact — quotes/metacharacters are
        // NOT escaped (the bootstrap reads it back inside double quotes).
        let evil = "'; rm -rf ~; echo '";
        assert_eq!(
            cli_prompt_file_content(&CliPromptArg::Positional, evil).as_deref(),
            Some(evil)
        );

        // Positional leading-dash guard becomes a literal leading space in the
        // file so the agent can't parse the prompt as a flag.
        assert_eq!(
            cli_prompt_file_content(&CliPromptArg::Positional, "-rf is a prompt").as_deref(),
            Some(" -rf is a prompt")
        );

        // Flag: no dash guard needed (the value follows a flag).
        assert_eq!(
            cli_prompt_file_content(&CliPromptArg::Flag("-i".to_string()), "-x").as_deref(),
            Some("-x")
        );

        // StdinPipe keeps the trailing newline the old `printf '%s\n'` added.
        assert_eq!(
            cli_prompt_file_content(&CliPromptArg::StdinPipe, "hello").as_deref(),
            Some("hello\n")
        );

        // Unsupported agents have no launch-time transport.
        assert_eq!(
            cli_prompt_file_content(&CliPromptArg::Unsupported, "hi"),
            None
        );
    }

    #[test]
    fn prompt_stage_failed_surfaces_recovery_notice_verbatim() {
        // When staging a parked prompt fails (e.g. a full/read-only FS), the
        // spawn is aborted with PromptStageFailed rather than silently dropping
        // the prompt. The error text must reach the user verbatim (terminal.rs
        // sends `e.to_string()` to the pane), and the unit variant makes it
        // structurally impossible to construct with different copy than the
        // "session never came up" branch — both speak the shared recovery
        // notice telling the user the prompt is saved for the next attach.
        let err = PtyError::PromptStageFailed;
        assert_eq!(err.to_string(), CLI_PROMPT_PARKED_NOTICE);
        assert!(err.to_string().contains("your prompt is saved"));
    }

    #[test]
    fn cli_prompt_fits_inline_caps_argv_agents_only() {
        // Positional/Flag are capped (single argv entry, Linux MAX_ARG_STRLEN).
        assert!(cli_prompt_fits_inline(&CliPromptArg::Positional, 100_000));
        assert!(!cli_prompt_fits_inline(&CliPromptArg::Positional, 100_001));
        assert!(cli_prompt_fits_inline(
            &CliPromptArg::Flag("-i".to_string()),
            100_000
        ));
        assert!(!cli_prompt_fits_inline(
            &CliPromptArg::Flag("-i".to_string()),
            200_000
        ));
        // StdinPipe has no argv ceiling.
        assert!(cli_prompt_fits_inline(&CliPromptArg::StdinPipe, 5_000_000));
        // Unsupported never bakes in.
        assert!(!cli_prompt_fits_inline(&CliPromptArg::Unsupported, 1));
    }

    #[test]
    fn route_initial_prompt_bakes_defers_or_drops() {
        // No prompt carried -> nothing to deliver, nothing to clear.
        assert_eq!(
            route_initial_prompt(None, &CliPromptArg::Positional),
            CliPromptRouting::None
        );
        // Blank-after-trim -> None (the empty-TUI case must clear nothing).
        assert_eq!(
            route_initial_prompt(Some("   \n\t".to_string()), &CliPromptArg::Positional),
            CliPromptRouting::None
        );

        // Small prompt that fits inline -> Baked, trimmed (the downstream
        // file-content trim is then a no-op; the dash guard still applies).
        assert_eq!(
            route_initial_prompt(Some("  hi there  ".to_string()), &CliPromptArg::Positional),
            CliPromptRouting::Baked("hi there".to_string())
        );

        // Oversized Positional prompt -> Deferred (paste path), carrying the
        // trimmed text that will actually be pasted.
        let big = "x".repeat(MAX_INLINE_PROMPT_BYTES + 1);
        assert_eq!(
            route_initial_prompt(Some(format!("  {big}  ")), &CliPromptArg::Positional),
            CliPromptRouting::Deferred(big.clone())
        );

        // StdinPipe has no argv ceiling, so even a huge prompt bakes in.
        assert_eq!(
            route_initial_prompt(Some(big.clone()), &CliPromptArg::StdinPipe),
            CliPromptRouting::Baked(big)
        );

        // Unsupported agents have no launch-time transport -> always Deferred
        // (delivered post-launch by paste), never dropped or baked.
        assert_eq!(
            route_initial_prompt(Some("hello".to_string()), &CliPromptArg::Unsupported),
            CliPromptRouting::Deferred("hello".to_string())
        );
    }

    #[test]
    fn cli_prompt_delivery_claim_is_exclusive_until_dropped() {
        // Fresh workspace id so parallel tests can't collide in the global set.
        let wid = Uuid::new_v4();
        let claim = CliPromptDelivery::try_claim(wid).expect("first claim succeeds");
        // A racing second attach must NOT also carry the prompt.
        assert!(
            CliPromptDelivery::try_claim(wid).is_none(),
            "second claim while held must fail"
        );
        // Another workspace's delivery is independent.
        let other = Uuid::new_v4();
        assert!(CliPromptDelivery::try_claim(other).is_some());
        // Releasing (drop) lets the next attach retry delivery.
        drop(claim);
        assert!(
            CliPromptDelivery::try_claim(wid).is_some(),
            "claim must be reusable after drop"
        );
    }

    #[test]
    fn comm_matching_gates_paste_on_the_expected_agent_only() {
        // The paste gate matches THE agent we launched, so a shell (bootstrap
        // still starting / missing-binary fallback) — or an unrelated program
        // the user ran in that fallback shell (vim, npm→node) — can never
        // receive a prompt meant for the agent.
        assert!(comm_matches_program("claude", "claude"));
        assert!(comm_matches_program("codex", "codex"));
        for not_agent in ["sh", "bash", "zsh", "vim", "node", "npm", "htop"] {
            assert!(
                !comm_matches_program(not_agent, "claude"),
                "{not_agent} must not satisfy the claude gate"
            );
        }
        // Full-path (macOS ps) and login-dash spellings normalize away.
        assert!(comm_matches_program("/usr/local/bin/claude", "claude"));
        assert!(comm_matches_program("-claude", "claude"));
        // The kernel truncates comm to 15 bytes; long program names match on
        // the truncated prefix.
        assert!(comm_matches_program(
            "verylongagentna",
            "verylongagentname-cli"
        ));
        assert!(!comm_matches_program(
            "verylongagentXX",
            "verylongagentname-cli"
        ));
    }

    #[test]
    fn pane_subtree_finds_node_wrapped_agent_grandchild() {
        // codex ships as `#!/usr/bin/env node` which spawns the native `codex`
        // as a GRANDCHILD, so the pane tree is `sh(pane) → node → codex`. The
        // gate must descend past the intermediate `node` (which is NOT an
        // agent) to confirm delivery — a direct-children-only probe would miss
        // it and strand the prompt for replay.
        let ps = "\
  100     1 sh
  200   100 node
  300   200 codex
  400     1 unrelated
  500   400 vim
";
        assert!(
            pane_subtree_has_program(ps, 100, "codex"),
            "grandchild agent under a node wrapper must be found"
        );
        // Native agent as a direct child (claude is an ELF binary).
        let ps_native = "  100     1 sh\n  200   100 claude\n";
        assert!(pane_subtree_has_program(ps_native, 100, "claude"));

        // A shell-only subtree (bootstrap still starting / missing-binary
        // fallback) must NOT satisfy the gate...
        assert!(!pane_subtree_has_program("  100     1 sh\n", 100, "codex"));
        // ...nor may an unrelated program the user ran in the fallback shell
        // (node from an `npm` invocation is exactly the intermediate we refuse
        // to accept as the agent).
        let ps_npm = "  100     1 sh\n  200   100 node\n  300   200 esbuild\n";
        assert!(!pane_subtree_has_program(ps_npm, 100, "codex"));
        // A sibling subtree's agent (different pane) is out of scope.
        assert!(!pane_subtree_has_program(ps, 400, "codex"));
    }

    #[test]
    fn cli_bootstrap_falls_back_to_continue_then_fresh() {
        // With nothing explicit to run: continue the cwd's latest conversation
        // when one exists (CLI-first workspace after tmux death), else a
        // fresh TUI — never a stranded "No conversation found" pane.
        let b = cli_bootstrap(
            &claude_spec(&["--dangerously-skip-permissions"]),
            None,
            None,
            false,
        );
        assert!(b.contains(
            "'claude' '--dangerously-skip-permissions' --continue || 'claude' '--dangerously-skip-permissions'"
        ));
    }

    #[test]
    fn cli_bootstrap_shell_quotes_agent_args_on_every_form() {
        // Glob/metacharacters in a model id stay inert (single-quoted)...
        let b = cli_bootstrap(&claude_spec(&["--model", "opus[1m]"]), None, None, false);
        assert!(b.contains("'--model' 'opus[1m]'"));
        // ...and the flags ride the continue/fresh fallback too.
        assert!(b.contains("'opus[1m]' --continue"));
    }

    #[test]
    fn apply_trust_seeds_keys_and_preserves_other_data() {
        let mut root = serde_json::json!({
            "projects": { "/existing": { "hasTrustDialogAccepted": true, "foo": 1 } },
            "other": "keep me"
        });
        assert!(apply_trust_to_config(&mut root, &["/new/dir".to_string()]));
        assert_eq!(
            root["projects"]["/new/dir"]["hasTrustDialogAccepted"],
            serde_json::json!(true)
        );
        assert_eq!(
            root["projects"]["/new/dir"]["hasCompletedProjectOnboarding"],
            serde_json::json!(true)
        );
        // Unrelated data is preserved.
        assert_eq!(root["other"], serde_json::json!("keep me"));
        assert_eq!(root["projects"]["/existing"]["foo"], serde_json::json!(1));
        // Idempotent: re-applying to an already-trusted key changes nothing.
        assert!(!apply_trust_to_config(&mut root, &["/new/dir".to_string()]));
    }

    #[test]
    fn codex_trust_additions_appends_only_when_absent() {
        let dir = std::path::Path::new("/var/tmp/wt/abc-fresh-xyz");
        // Empty config -> a trusted block for the dir is produced (the path
        // appears, marked trusted). Canonicalize fails for a non-existent path,
        // so only the given key is emitted.
        let add = codex_trust_additions("", dir);
        assert!(add.contains(r#"[projects."/var/tmp/wt/abc-fresh-xyz"]"#));
        assert!(add.contains(r#"trust_level = "trusted""#));
        // The merged result must be valid TOML.
        assert!(toml::from_str::<toml::Table>(&add).is_ok());

        // Already-present header -> no duplicate table is appended (which would
        // make codex's config invalid TOML).
        let existing = format!(
            "approval_policy = \"never\"\n\n[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
            dir.display()
        );
        assert!(codex_trust_additions(&existing, dir).is_empty());
    }

    #[test]
    fn shell_single_quote_escapes_quotes() {
        assert_eq!(shell_single_quote("plain"), "'plain'");
        // POSIX rule: close the quote, emit an escaped quote, reopen.
        assert_eq!(shell_single_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_single_quote("''"), r"''\'''\'''");
    }

    #[test]
    fn cli_tmux_conf_keeps_mouse_scroll_and_osc52_copy() {
        // The reconciliation contract: wheel scrolling stays (mouse on) AND
        // selections land in the system clipboard via OSC 52.
        assert!(CLI_TMUX_CONF.contains("set -g mouse on"));
        assert!(CLI_TMUX_CONF.contains("set -s set-clipboard on"));
        assert!(CLI_TMUX_CONF.contains("clipboard"));
        assert!(CLI_TMUX_CONF.contains("unbind-key -n MouseDown3Pane"));
        // Window command strings (the POSIX-sh launch bootstrap) are parsed by
        // default-shell; a fish/csh login shell would reject them outright.
        assert!(CLI_TMUX_CONF.contains("set -g default-shell /bin/sh"));
    }

    #[test]
    fn parse_cli_session_line_reads_attached_and_idle() {
        let id = "vk_00000000000000000000000000000001";
        // activity 900, now 1000 -> idle 100; attached "0" -> false
        let (_, attached, idle) =
            parse_cli_session_line(&format!("{id}\t0\t900"), 1000).expect("valid line parses");
        assert!(!attached);
        assert_eq!(idle, 100);
        // attached count > 0 -> true
        let (_, attached, _) = parse_cli_session_line(&format!("{id}\t1\t900"), 1000).unwrap();
        assert!(attached);
    }

    #[test]
    fn parse_cli_session_line_rejects_malformed() {
        let id = "vk_00000000000000000000000000000001";
        // Empty fields — the `tmux display-message` failure mode that silently
        // disabled the reaper — must NOT parse to a bogus liveness value.
        assert!(parse_cli_session_line(&format!("{id}\t\t"), 1000).is_none());
        // Missing columns.
        assert!(parse_cli_session_line(id, 1000).is_none());
        // Non-vk session names are ignored entirely.
        assert!(parse_cli_session_line("misc\t0\t900", 1000).is_none());
    }
}
