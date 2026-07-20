use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Instant,
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
    /// Current `bc_` sessions use `tmux new-session -A`; live legacy `vk_`
    /// sessions are attach-only. Either survives WebSocket disconnects and
    /// server restarts, so reconnects reattach instead of respawning.
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
        /// Browser visibility captured in the WebSocket URL. When supported,
        /// the tmux client starts with `ignore-size` before it can affect the
        /// shared grid; later presence messages keep the flag synchronized.
        connect_hidden: bool,
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
        // file into `bc_p`, then delete it (the delete doubles as the delivery
        // acknowledgement — see [`cli_prompt_file_exists`]).
        let read_rm = format!(r#"bc_p="$(cat {qfile})"; rm -f -- {qfile};"#);
        match &spec.prompt_arg {
            // Trailing positional arg. The leading-dash guard and any trailing
            // whitespace handling are baked into the file's contents
            // ([`cli_prompt_file_content`]); command substitution strips a
            // trailing newline, which is harmless.
            CliPromptArg::Positional => {
                format!(r#"{read_rm} {base} "$bc_p""#)
            }
            // Prompt as a flag value (e.g. gemini/copilot `-i "<prompt>"`); a
            // leading '-' is harmless after the flag. The flag is one of our
            // own spec constants, but quote it anyway (like the program and
            // base args) so it can never be more than a single command word.
            CliPromptArg::Flag(flag) => {
                let qflag = shell_single_quote(flag);
                format!(r#"{read_rm} {base} {qflag} "$bc_p""#)
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

fn stageable_cli_prompt_content(
    resume_session_id: Option<&str>,
    initial_prompt: Option<&str>,
    prompt_arg: &CliPromptArg,
) -> Option<String> {
    if active_resume_id(resume_session_id).is_some() {
        None
    } else {
        initial_prompt.and_then(|prompt| cli_prompt_file_content(prompt_arg, prompt))
    }
}

fn legacy_attach_requires_prompt_staging(
    resume_session_id: Option<&str>,
    initial_prompt: Option<&str>,
    prompt_arg: &CliPromptArg,
) -> bool {
    stageable_cli_prompt_content(resume_session_id, initial_prompt, prompt_arg).is_some()
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
/// - `window-size smallest` (tmux >= 3.2 only): when several browser clients
///   share a workspace, the pane grid fits every client that participates in
///   sizing. Older tmux versions lack the client flags needed to exclude a
///   hidden/stale client, so they keep tmux's default sizing behavior.
/// - `default-shell /bin/sh`: window command strings (our launch bootstrap)
///   are run via `default-shell -c`, and the bootstrap is POSIX sh
///   (`bc_p="$(cat …)"`, `{ …; } | …`) — a fish/csh login shell would fail on
///   it outright. The pane still ends in the USER'S shell: the bootstrap's
///   final `exec "${SHELL:-/bin/sh}"` honors `$SHELL`.
const CLI_TMUX_CONF: &str = "\
# BetterCoding embedded terminal tmux server (socket: bettercoding).
# Written by the backend before each CLI terminal attach - edits are overwritten.
set -g mouse on
set -s set-clipboard on
set -as terminal-features ',xterm*:clipboard'
set -g default-shell /bin/sh
unbind-key -n MouseDown3Pane
";

fn cli_tmux_conf(client_flags_supported: bool) -> String {
    let mut conf = CLI_TMUX_CONF.to_string();
    if client_flags_supported {
        conf.push_str("set -g window-size smallest\n");
    }
    conf
}

/// Write the embedded server config (idempotent) and return its path.
fn cli_tmux_conf_path() -> Option<PathBuf> {
    let dir = utils::assets::asset_dir();
    let path = dir.join("cli-tmux.conf");
    let desired = cli_tmux_conf(tmux_client_flags_supported());
    let current = std::fs::read_to_string(&path).ok();
    if current.as_deref() != Some(desired.as_str()) {
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(&path, desired).ok()?;
    }
    Some(path)
}

/// Apply the embedded-server options to an ALREADY-RUNNING tmux server (the
/// `-f` config only applies to fresh server starts). Probes `set-clipboard`
/// first so the append-style options aren't re-applied on every attach.
/// Best-effort: no server running is the common case and simply a no-op.
fn ensure_cli_tmux_server_options_on(socket: &str, client_flags_supported: bool) {
    let tmux = |args: &[&str]| {
        std::process::Command::new("tmux")
            .args(["-L", socket])
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
    };

    // default-shell governs how each NEW window command string (our POSIX-sh
    // launch bootstrap) is parsed; apply unconditionally (idempotent, no-op
    // without a running server) so servers started before this option joined
    // the conf don't hand the bootstrap to a fish/csh login shell.
    let migration = if client_flags_supported {
        // This migration MUST stay above the clipboard probe: a production
        // server commonly already has `set-clipboard on`, which makes the
        // probe return early, but may have started before window-size joined
        // the config.
        tmux(&[
            "set-option",
            "-g",
            "default-shell",
            "/bin/sh",
            ";",
            "set-option",
            "-g",
            "window-size",
            "smallest",
        ])
    } else {
        // A server can outlive this backend process. Actively release a
        // `smallest` clamp left by an earlier supported build/probe result.
        tmux(&[
            "set-option",
            "-g",
            "default-shell",
            "/bin/sh",
            ";",
            "set-option",
            "-gu",
            "window-size",
        ])
    };
    match migration {
        Ok(output) if output.status.success() => {}
        Ok(output) => tracing::debug!(
            socket,
            status = %output.status,
            "CLI tmux server option migration exited unsuccessfully"
        ),
        Err(error) => tracing::debug!(
            socket,
            error = %error,
            "Failed to spawn CLI tmux server option migration"
        ),
    }

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

/// Dedicated BetterCoding tmux socket. Isolation keeps app-owned sessions out
/// of the user's default tmux server and gives them an unambiguous owner.
const DEFAULT_CLI_TMUX_SOCKET: &str = "bettercoding";
// TODO(bc-legacy-cleanup): remove when no vk_ sessions remain.
const DEFAULT_LEGACY_CLI_TMUX_SOCKET: &str = "vibe-kanban";

#[derive(Debug, PartialEq, Eq)]
struct CliTmuxSockets {
    current: String,
    legacy: String,
    legacy_home_enabled: bool,
}

fn resolve_cli_tmux_sockets(
    current_override: Option<String>,
    legacy_override: Option<String>,
    current_default: &str,
    legacy_default: &str,
) -> CliTmuxSockets {
    let current = current_override.unwrap_or_else(|| current_default.to_string());
    let legacy = legacy_override.unwrap_or_else(|| {
        if current == current_default {
            legacy_default.to_string()
        } else {
            format!("{current}-legacy")
        }
    });
    let legacy_home_enabled = current != legacy;
    CliTmuxSockets {
        current,
        legacy,
        legacy_home_enabled,
    }
}

/// Dedicated socket for newly-created CLI sessions. A value that differs from
/// the compiled default also moves an otherwise-unset legacy socket to
/// `<current>-legacy`, keeping test/dev stacks away from production. Both
/// environment overrides are cached on their first respective lookup; unit
/// tests exercise [`resolve_cli_tmux_sockets`] without mutating process-global
/// environment variables.
pub(crate) fn cli_tmux_socket() -> &'static str {
    static SOCKET: OnceLock<String> = OnceLock::new();
    SOCKET
        .get_or_init(|| {
            std::env::var("BC_CLI_TMUX_SOCKET")
                .unwrap_or_else(|_| DEFAULT_CLI_TMUX_SOCKET.to_string())
        })
        .as_str()
}

/// Legacy socket lookup seam for live sessions created before the identity
/// migration. An explicit `BC_CLI_TMUX_LEGACY_SOCKET` always wins; otherwise a
/// non-default current socket derives `<current>-legacy`, while the compiled
/// current socket keeps the production legacy default. The legacy override is
/// read once, and the current value comes from [`cli_tmux_socket`]'s read-once
/// cache.
// TODO(bc-legacy-cleanup): remove when no vk_ sessions remain.
pub(crate) fn legacy_cli_tmux_socket() -> &'static str {
    resolved_cli_tmux_sockets().legacy.as_str()
}

fn resolved_cli_tmux_sockets() -> &'static CliTmuxSockets {
    static SOCKETS: OnceLock<CliTmuxSockets> = OnceLock::new();
    SOCKETS.get_or_init(|| {
        let sockets = resolve_cli_tmux_sockets(
            Some(cli_tmux_socket().to_string()),
            std::env::var("BC_CLI_TMUX_LEGACY_SOCKET").ok(),
            DEFAULT_CLI_TMUX_SOCKET,
            DEFAULT_LEGACY_CLI_TMUX_SOCKET,
        );
        if !sockets.legacy_home_enabled {
            tracing::error!(
                current_socket = sockets.current,
                legacy_socket = sockets.legacy,
                "Resolved current and legacy CLI tmux sockets are equal; disabling the legacy home"
            );
        }
        sockets
    })
}

fn is_legacy_home_enabled() -> bool {
    resolved_cli_tmux_sockets().legacy_home_enabled
}

/// Bound CLI tmux control commands so a wedged current or legacy server cannot
/// retain a request task indefinitely.
const CLI_TMUX_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

const CLI_TMUX_SESSION_PREFIX: &str = "bc_";
// TODO(bc-legacy-cleanup): remove when no vk_ sessions remain.
const LEGACY_CLI_TMUX_SESSION_PREFIX: &str = "vk_";

/// tmux session name for a workspace's CLI-mode terminal. `simple()` (32 hex
/// chars, no hyphens) avoids tmux-special characters.
pub fn cli_tmux_session_name(workspace_id: Uuid) -> String {
    format!("{CLI_TMUX_SESSION_PREFIX}{}", workspace_id.simple())
}

// TODO(bc-legacy-cleanup): remove when no vk_ sessions remain.
fn legacy_cli_tmux_session_name(workspace_id: Uuid) -> String {
    format!("{LEGACY_CLI_TMUX_SESSION_PREFIX}{}", workspace_id.simple())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliTmuxHome {
    Current,
    // TODO(bc-legacy-cleanup): remove when no vk_ sessions remain.
    Legacy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliTmuxSessionProbe {
    Present,
    Absent,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CliTmuxLocation {
    home: CliTmuxHome,
    present: bool,
}

/// A workspace's tmux home resolved to one immutable delivery target. Keeping
/// the socket and session name together prevents a later locator pass from
/// redirecting a multi-step prompt delivery to the other home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliTmuxTarget {
    workspace_id: Uuid,
    socket: String,
    session_name: String,
}

fn cli_tmux_target_on<'a>(
    home: CliTmuxHome,
    workspace_id: Uuid,
    current_socket: &'a str,
    legacy_socket: &'a str,
) -> (&'a str, String) {
    match home {
        CliTmuxHome::Current => (current_socket, cli_tmux_session_name(workspace_id)),
        // TODO(bc-legacy-cleanup): remove when no vk_ sessions remain.
        CliTmuxHome::Legacy => (legacy_socket, legacy_cli_tmux_session_name(workspace_id)),
    }
}

fn cli_tmux_target(home: CliTmuxHome, workspace_id: Uuid) -> (&'static str, String) {
    cli_tmux_target_on(
        home,
        workspace_id,
        cli_tmux_socket(),
        legacy_cli_tmux_socket(),
    )
}

fn owned_cli_tmux_target_on(
    home: CliTmuxHome,
    workspace_id: Uuid,
    current_socket: &str,
    legacy_socket: &str,
) -> CliTmuxTarget {
    let (socket, session_name) =
        cli_tmux_target_on(home, workspace_id, current_socket, legacy_socket);
    CliTmuxTarget {
        workspace_id,
        socket: socket.to_string(),
        session_name,
    }
}

/// Build the tmux client argv for one already-resolved home. Current sessions
/// are attach-or-create and receive the app config/working directory/bootstrap;
/// legacy sessions are strictly attach-only and ignore all create-only inputs.
struct CliTmuxArgv<'a> {
    home: CliTmuxHome,
    workspace_id: Uuid,
    current_socket: &'a str,
    legacy_socket: &'a str,
    conf: Option<&'a Path>,
    connect_hidden: bool,
    working_dir: &'a Path,
    bootstrap: &'a str,
}

fn cli_tmux_argv_on(input: CliTmuxArgv<'_>) -> Vec<std::ffi::OsString> {
    let CliTmuxArgv {
        home,
        workspace_id,
        current_socket,
        legacy_socket,
        conf,
        connect_hidden,
        working_dir,
        bootstrap,
    } = input;
    let (socket, session_name) =
        cli_tmux_target_on(home, workspace_id, current_socket, legacy_socket);
    let mut args = vec!["-L".into(), socket.into()];
    match home {
        CliTmuxHome::Current => {
            if let Some(conf) = conf {
                args.push("-f".into());
                args.push(conf.as_os_str().to_owned());
            }
            args.push("new-session".into());
            if connect_hidden {
                args.push("-f".into());
                args.push("ignore-size".into());
            }
            args.extend([
                "-A".into(),
                "-s".into(),
                session_name.into(),
                "-c".into(),
                working_dir.as_os_str().to_owned(),
                bootstrap.into(),
            ]);
        }
        // TODO(bc-legacy-cleanup): remove when no vk_ sessions remain.
        CliTmuxHome::Legacy => {
            args.push("attach-session".into());
            if connect_hidden {
                args.push("-f".into());
                args.push("ignore-size".into());
            }
            args.extend(["-t".into(), format!("={session_name}").into()]);
        }
    }
    args
}

fn classify_cli_tmux_session_probe(success: bool, stderr: &str) -> CliTmuxSessionProbe {
    if success {
        CliTmuxSessionProbe::Present
    } else if stderr.contains("can't find session") || stderr.contains("no server running on") {
        CliTmuxSessionProbe::Absent
    } else {
        CliTmuxSessionProbe::Unknown
    }
}

async fn probe_tmux_session_on(socket: &str, session_name: &str) -> CliTmuxSessionProbe {
    match run_cli_tmux_output(&[
        "-L",
        socket,
        "has-session",
        "-t",
        &format!("={session_name}"),
    ])
    .await
    {
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let probe = classify_cli_tmux_session_probe(output.status.success(), &stderr);
            if probe == CliTmuxSessionProbe::Unknown {
                tracing::warn!(
                    socket,
                    session_name,
                    status = %output.status,
                    stderr = %stderr,
                    "CLI tmux session probe returned an unrecognized error; session state is unknown"
                );
            }
            probe
        }
        Err(error) => {
            tracing::warn!(
                socket,
                session_name,
                error,
                "CLI tmux session probe failed; session state is unknown"
            );
            CliTmuxSessionProbe::Unknown
        }
    }
}

fn resolve_cli_tmux_session_probes(
    workspace_id: Uuid,
    current: CliTmuxSessionProbe,
    legacy: CliTmuxSessionProbe,
) -> Result<CliTmuxLocation, PtyError> {
    if current == CliTmuxSessionProbe::Unknown || legacy == CliTmuxSessionProbe::Unknown {
        return Err(PtyError::CliTmuxStateUnknown(workspace_id));
    }

    Ok(match (current, legacy) {
        (CliTmuxSessionProbe::Present, _) => CliTmuxLocation {
            home: CliTmuxHome::Current,
            present: true,
        },
        (CliTmuxSessionProbe::Absent, CliTmuxSessionProbe::Present) => CliTmuxLocation {
            home: CliTmuxHome::Legacy,
            present: true,
        },
        (CliTmuxSessionProbe::Absent, CliTmuxSessionProbe::Absent) => CliTmuxLocation {
            home: CliTmuxHome::Current,
            present: false,
        },
        _ => unreachable!("unknown probes returned above"),
    })
}

async fn locate_cli_tmux_session_on(
    workspace_id: Uuid,
    current_socket: &str,
    legacy_socket: &str,
    legacy_home_enabled: bool,
) -> Result<CliTmuxLocation, PtyError> {
    // Probe in stable current-then-legacy order. Both results are required:
    // an unreadable home must never be mistaken for an empty one.
    let current_name = cli_tmux_session_name(workspace_id);
    let current = probe_tmux_session_on(current_socket, &current_name).await;

    // TODO(bc-legacy-cleanup): remove when no vk_ sessions remain.
    let legacy_name = legacy_cli_tmux_session_name(workspace_id);
    let legacy = if legacy_home_enabled {
        probe_tmux_session_on(legacy_socket, &legacy_name).await
    } else {
        CliTmuxSessionProbe::Absent
    };
    if current == CliTmuxSessionProbe::Present && legacy == CliTmuxSessionProbe::Present {
        tracing::info!(
            workspace_id = %workspace_id,
            current_socket,
            legacy_socket,
            "CLI tmux locator observed sessions in both current and legacy homes"
        );
    }
    resolve_cli_tmux_session_probes(workspace_id, current, legacy)
}

async fn locate_cli_tmux_session(workspace_id: Uuid) -> Result<CliTmuxLocation, PtyError> {
    if !tmux_available() {
        return Ok(CliTmuxLocation {
            home: CliTmuxHome::Current,
            present: false,
        });
    }
    locate_cli_tmux_session_on(
        workspace_id,
        cli_tmux_socket(),
        legacy_cli_tmux_socket(),
        is_legacy_home_enabled(),
    )
    .await
}

/// Locate and pin a workspace's tmux target for a multi-step operation. `None`
/// means tmux is unavailable; when neither session exists, the target is the
/// current home where a new session would be created.
pub async fn locate_cli_tmux_target(workspace_id: Uuid) -> Option<CliTmuxTarget> {
    if !tmux_available() {
        return None;
    }
    let home = locate_cli_tmux_session(workspace_id).await.ok()?.home;
    Some(owned_cli_tmux_target_on(
        home,
        workspace_id,
        cli_tmux_socket(),
        legacy_cli_tmux_socket(),
    ))
}

/// Whether the exact pinned tmux target still exists. Timeout and command
/// errors are treated as session-not-found.
pub async fn cli_tmux_target_exists(target: &CliTmuxTarget) -> bool {
    probe_tmux_session_on(&target.socket, &target.session_name).await
        == CliTmuxSessionProbe::Present
}

/// Resolve the name currently used by a workspace for diagnostic labels. When
/// neither home exists, this returns the current-home creation name.
pub async fn resolved_cli_tmux_session_name(workspace_id: Uuid) -> String {
    let home = locate_cli_tmux_session(workspace_id)
        .await
        .map(|location| location.home)
        .unwrap_or(CliTmuxHome::Current);
    cli_tmux_target(home, workspace_id).1
}

async fn cli_tmux_session_exists_on(
    workspace_id: Uuid,
    current_socket: &str,
    legacy_socket: &str,
    legacy_home_enabled: bool,
) -> bool {
    locate_cli_tmux_session_on(
        workspace_id,
        current_socket,
        legacy_socket,
        legacy_home_enabled,
    )
    .await
    .map(|location| location.present)
    // This bool only gates prompt parking. The create-time locator repeats
    // the probes and fails closed instead of creating on an unknown home.
    .unwrap_or(false)
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
    cli_tmux_session_exists_on(
        workspace_id,
        cli_tmux_socket(),
        legacy_cli_tmux_socket(),
        is_legacy_home_enabled(),
    )
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
    let target = locate_cli_tmux_target(workspace_id).await?;
    cli_pane_agent_running_at(&target, program).await
}

/// Check agent ownership at one already-located target without re-running the
/// dual-home locator. The tmux pane probe is bounded like the locator probes.
pub async fn cli_pane_agent_running_at(target: &CliTmuxTarget, program: &str) -> Option<bool> {
    let output = run_cli_tmux(&[
        "-L",
        &target.socket,
        "list-panes",
        "-t",
        &target.session_name,
        "-F",
        "#{pane_pid}",
    ])
    .await
    .ok()?;
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
        // `ps` right-justifies pid/ppid; split off exactly those two fields and
        // keep the WHOLE remainder as comm — a comm can contain spaces (macOS
        // `comm=` reports a full path, e.g. `/Applications/My App/codex`), and
        // splitting it at the first space would strand a space-containing agent
        // path (normalize_comm reduces it to the basename for the match).
        let rest = line.trim_start();
        let Some((pid, rest)) = rest.split_once(char::is_whitespace) else {
            continue;
        };
        let Some((ppid, comm)) = rest.trim_start().split_once(char::is_whitespace) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        comm_by_pid.insert(pid, comm.trim());
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

/// Recover the workspace id from a current `bc_` session name or a legacy
/// `vk_` session name. Other namespaces and malformed UUIDs are rejected.
pub(crate) fn workspace_id_from_cli_session_name(name: &str) -> Option<Uuid> {
    let hex = if let Some(hex) = name.strip_prefix(CLI_TMUX_SESSION_PREFIX) {
        hex
    // TODO(bc-legacy-cleanup): remove when no vk_ sessions remain.
    } else if let Some(hex) = name.strip_prefix(LEGACY_CLI_TMUX_SESSION_PREFIX) {
        hex
    } else {
        return None;
    };
    if hex.len() != 32 {
        return None;
    }
    Uuid::parse_str(hex).ok()
}

async fn kill_cli_tmux_sessions_on(
    workspace_id: Uuid,
    current_socket: &str,
    legacy_socket: &str,
    legacy_home_enabled: bool,
) {
    for home in [
        CliTmuxHome::Current,
        // TODO(bc-legacy-cleanup): remove when no vk_ sessions remain.
        CliTmuxHome::Legacy,
    ] {
        if home == CliTmuxHome::Legacy && !legacy_home_enabled {
            continue;
        }
        let (socket, session_name) =
            cli_tmux_target_on(home, workspace_id, current_socket, legacy_socket);
        match tokio::process::Command::new("tmux")
            .args([
                "-L",
                socket,
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
}

/// Best-effort kill of a workspace's current `bc_` and legacy `vk_` CLI tmux
/// sessions so neither can outlive its worktree. `=` forces exact-name
/// matching; tmux `-t` is otherwise a prefix match.
pub async fn kill_cli_tmux_session(workspace_id: Uuid) {
    if tmux_available() {
        kill_cli_tmux_sessions_on(
            workspace_id,
            cli_tmux_socket(),
            legacy_cli_tmux_socket(),
            is_legacy_home_enabled(),
        )
        .await;
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
    // Pane-targeting commands (capture-pane / send-keys) reject the `=exact`
    // session-target syntax; they take a pane target. A full 32-hex `bc_*` or
    // legacy `vk_*` name cannot prefix another valid session in its namespace,
    // so the bare name resolves unambiguously to this session's sole pane.
    let target = locate_cli_tmux_target(workspace_id).await?;
    let output = run_cli_tmux(&[
        "-L",
        &target.socket,
        "capture-pane",
        "-p",
        "-t",
        &target.session_name,
    ])
    .await
    .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Texts at/above this size go through tmux buffers (load-buffer + paste-buffer)
/// instead of `send-keys -l`, which — like every tmux client command — is
/// rejected once its argv exceeds ~16KB. The small-text `send-keys` path is the
/// already-live-verified one, so keep it for the common case.
const SEND_KEYS_PASTE_THRESHOLD: usize = 4096;

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
    let Some(target) = locate_cli_tmux_target(workspace_id).await else {
        return false;
    };
    send_cli_keys_to(&target, text).await
}

/// Send and submit text to one already-located target without allowing a
/// second locator pass to switch homes midway through the transaction.
pub async fn send_cli_keys_to(target: &CliTmuxTarget, text: &str) -> bool {
    // Bare name (not `=exact`): send-keys/paste-buffer take a pane target, for
    // which the `=` session-target syntax is rejected. Unambiguous given
    // full-hex names.
    let delivered = if needs_paste_transport(text) {
        paste_via_tmux_buffer(target, text).await
    } else {
        // `--` ends option parsing so text starting with `-` (e.g. a prompt
        // like "-rf ...") is treated as a literal key rather than an unknown
        // send-keys flag (verified: `send-keys -l "-x"` fails "unknown flag").
        run_cli_tmux(&[
            "-L",
            &target.socket,
            "send-keys",
            "-t",
            &target.session_name,
            "-l",
            "--",
            text,
        ])
        .await
        .is_ok()
    };
    if !delivered {
        return false;
    }

    // The text is already IN the pane; failing the whole send over a flaky
    // Enter would make the caller re-deliver the text on top of the residue
    // (a doubled prompt). Retry the Enter once, then accept: delivered (the
    // user can press Enter themselves), submission best-effort.
    for _ in 0..2 {
        if run_cli_tmux(&[
            "-L",
            &target.socket,
            "send-keys",
            "-t",
            &target.session_name,
            "Enter",
        ])
        .await
        .is_ok()
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    tracing::warn!(
        "CLI send to workspace {}: text delivered but Enter failed; \
         left unsubmitted in the pane",
        target.workspace_id
    );
    true
}

/// Stage `text` into a per-workspace tmux buffer via `load-buffer -` (text on
/// stdin, so no argv-length limit) and bracketed-paste it into the pane. The
/// buffer is namespaced (`bc_prompt_<wsid>`) and deleted on paste (`-d`) because
/// tmux buffers are server-global — otherwise concurrent workspaces could
/// cross-deliver. `-p` (bracketed paste) makes the TUI treat multi-line text as
/// one paste so embedded newlines don't submit early.
async fn load_cli_tmux_buffer(target: &CliTmuxTarget, buffer: &str, text: &str) -> bool {
    use tokio::io::AsyncWriteExt;

    tokio::time::timeout(CLI_TMUX_COMMAND_TIMEOUT, async {
        let mut child = match tokio::process::Command::new("tmux")
            .args(["-L", &target.socket, "load-buffer", "-b", buffer, "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return false,
        };

        let Some(mut stdin) = child.stdin.take() else {
            let _ = child.kill().await;
            return false;
        };
        if stdin.write_all(text.as_bytes()).await.is_err() {
            drop(stdin);
            let _ = child.kill().await;
            return false;
        }
        let _ = stdin.shutdown().await;
        drop(stdin);

        child.wait().await.map(|s| s.success()).unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

async fn paste_via_tmux_buffer(target: &CliTmuxTarget, text: &str) -> bool {
    // Per-send sequence number on top of the workspace namespace: two
    // concurrent sends to the SAME workspace (e.g. a loop wake-up racing a
    // deferred initial-prompt delivery) must not overwrite each other's buffer
    // between load and paste.
    static SEND_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEND_SEQ.fetch_add(1, Ordering::Relaxed);
    let buffer = cli_tmux_prompt_buffer_name(target.workspace_id, seq);

    if !load_cli_tmux_buffer(target, &buffer, text).await {
        let _ = run_cli_tmux(&["-L", &target.socket, "delete-buffer", "-b", &buffer]).await;
        return false;
    }

    let pasted = run_cli_tmux(&[
        "-L",
        &target.socket,
        "paste-buffer",
        "-d",
        "-p",
        "-b",
        &buffer,
        "-t",
        &target.session_name,
    ])
    .await
    .is_ok();
    if !pasted {
        // `-d` only fires on a successful paste; don't leave the staged prompt
        // readable in the server-global buffer list after a failed one.
        let _ = run_cli_tmux(&["-L", &target.socket, "delete-buffer", "-b", &buffer]).await;
    }
    pasted
}

fn cli_tmux_prompt_buffer_name(workspace_id: Uuid, sequence: u64) -> String {
    format!("bc_prompt_{}_{sequence}", workspace_id.simple())
}

/// Seconds since the Unix epoch (best-effort; 0 if the system clock is before
/// the epoch). Used to turn tmux's `session_activity` and `client_activity`
/// epochs into idle ages.
pub(crate) fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliTmuxSessionRow {
    session_name: String,
    workspace_id: Uuid,
    attached: bool,
    idle_secs: i64,
}

/// One socket's bounded `list-sessions` outcome. A normal no-server exit is
/// definitively empty; inability to obtain a trustworthy answer is failed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CliTmuxSocketSnapshot {
    Rows(Vec<CliTmuxSessionRow>),
    Empty,
    Failed(String),
}

/// List our CLI tmux sessions for the reaper: `(workspace_id, attached,
/// idle_secs)` for every current `bc_*` and legacy `vk_*` session across both
/// sockets, merged per workspace with the safest liveness values. An unreadable
/// socket fails the entire snapshot so the reaper cannot act on a partial view.
/// Other namespaces are ignored, so a user-created session is never reaped.
pub async fn list_cli_tmux_sessions() -> Result<Vec<(Uuid, bool, i64)>, String> {
    if !tmux_available() {
        return Ok(Vec::new());
    }
    let mut sockets = vec![cli_tmux_socket()];
    if is_legacy_home_enabled() {
        // TODO(bc-legacy-cleanup): remove when no vk_ sessions remain.
        sockets.push(legacy_cli_tmux_socket());
    }
    list_cli_tmux_sessions_on(&sockets).await
}

async fn list_cli_tmux_sessions_on(sockets: &[&str]) -> Result<Vec<(Uuid, bool, i64)>, String> {
    let now = now_unix_secs();
    let mut snapshots = Vec::with_capacity(sockets.len());
    for socket in sockets {
        snapshots.push(list_cli_tmux_socket_on(socket, now).await);
    }
    merge_cli_tmux_socket_snapshots(snapshots)
}

async fn list_cli_tmux_socket_on(socket: &str, now: i64) -> CliTmuxSocketSnapshot {
    let output = match run_cli_tmux_output(&[
        "-L",
        socket,
        "list-sessions",
        "-F",
        "#{session_name}\t#{session_attached}\t#{session_activity}",
    ])
    .await
    {
        Ok(output) => output,
        Err(error) => {
            return CliTmuxSocketSnapshot::Failed(format!("socket {socket}: {error}"));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if cli_tmux_list_exit_is_definitively_empty(&stderr) {
            return CliTmuxSocketSnapshot::Empty;
        }
        return CliTmuxSocketSnapshot::Failed(format!(
            "socket {socket}: tmux list-sessions exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }

    let rows: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| parse_cli_session_row(line, now))
        .collect();
    if rows.is_empty() {
        CliTmuxSocketSnapshot::Empty
    } else {
        CliTmuxSocketSnapshot::Rows(rows)
    }
}

fn cli_tmux_list_exit_is_definitively_empty(stderr: &str) -> bool {
    stderr.contains("no server running")
        || stderr.contains("No such file or directory")
        || stderr.contains("Connection refused")
}

fn merge_cli_tmux_socket_snapshots(
    snapshots: impl IntoIterator<Item = CliTmuxSocketSnapshot>,
) -> Result<Vec<(Uuid, bool, i64)>, String> {
    let mut sessions = Vec::new();
    let mut failures = Vec::new();
    for snapshot in snapshots {
        match snapshot {
            CliTmuxSocketSnapshot::Rows(rows) => {
                for row in rows {
                    merge_cli_tmux_session_liveness(
                        &mut sessions,
                        (row.workspace_id, row.attached, row.idle_secs),
                    );
                }
            }
            CliTmuxSocketSnapshot::Empty => {}
            CliTmuxSocketSnapshot::Failed(error) => failures.push(error),
        }
    }
    if failures.is_empty() {
        Ok(sessions)
    } else {
        Err(failures.join("; "))
    }
}

fn merge_cli_tmux_session_liveness(
    sessions: &mut Vec<(Uuid, bool, i64)>,
    candidate: (Uuid, bool, i64),
) {
    let (workspace_id, attached, idle_secs) = candidate;
    if let Some((_, existing_attached, existing_idle)) = sessions
        .iter_mut()
        .find(|(existing_id, _, _)| *existing_id == workspace_id)
    {
        // Double-homed workspaces are only safe to reap when BOTH copies are
        // detached and old enough; preserve the most protective combined view.
        *existing_attached |= attached;
        *existing_idle = (*existing_idle).min(idle_secs);
    } else {
        sessions.push((workspace_id, attached, idle_secs));
    }
}

/// Parse one `name\tattached\tactivity` tmux row into `(workspace_id, attached,
/// idle_secs)`, accepting current `bc_` and legacy `vk_` names only.
#[cfg(test)]
fn parse_cli_session_line(line: &str, now: i64) -> Option<(Uuid, bool, i64)> {
    let row = parse_cli_session_row(line, now)?;
    Some((row.workspace_id, row.attached, row.idle_secs))
}

fn parse_cli_session_row(line: &str, now: i64) -> Option<CliTmuxSessionRow> {
    let mut parts = line.split('\t');
    let session_name = parts.next()?.to_string();
    let workspace_id = workspace_id_from_cli_session_name(&session_name)?;
    let attached = parts.next()?.trim() != "0";
    let activity: i64 = parts.next()?.trim().parse().ok()?;
    Some(CliTmuxSessionRow {
        session_name,
        workspace_id,
        attached,
        idle_secs: (now - activity).max(0),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CliTmuxHomeLiveness {
    Present { attached: bool, idle_secs: i64 },
    Absent,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CliTmuxSessionLiveness {
    current: CliTmuxHomeLiveness,
    legacy: CliTmuxHomeLiveness,
}

fn cli_tmux_home_liveness(
    snapshot: CliTmuxSocketSnapshot,
    session_name: &str,
) -> CliTmuxHomeLiveness {
    match snapshot {
        CliTmuxSocketSnapshot::Rows(rows) => rows
            .into_iter()
            .find(|row| row.session_name == session_name)
            .map(|row| CliTmuxHomeLiveness::Present {
                attached: row.attached,
                idle_secs: row.idle_secs,
            })
            .unwrap_or(CliTmuxHomeLiveness::Absent),
        CliTmuxSocketSnapshot::Empty => CliTmuxHomeLiveness::Absent,
        CliTmuxSocketSnapshot::Failed(_) => CliTmuxHomeLiveness::Unknown,
    }
}

/// Fresh per-home liveness for one workspace. The reaper calls this immediately
/// before killing as a TOCTOU recheck. A failed home is `Unknown`, never absent,
/// and is therefore ineligible for a guarded kill.
///
/// Implemented by re-listing rather than `tmux display-message`: display-message
/// resolves formats in a client/pane context and returns EMPTY for the
/// session-scoped `#{session_attached}` / `#{session_activity}` there, which made
/// this recheck always parse to `None` and silently disabled the whole reaper.
/// `list-sessions` (used here) populates those fields correctly.
async fn cli_tmux_session_liveness_on(
    workspace_id: Uuid,
    current_socket: &str,
    legacy_socket: &str,
    legacy_home_enabled: bool,
) -> CliTmuxSessionLiveness {
    let now = now_unix_secs();
    let current = list_cli_tmux_socket_on(current_socket, now).await;
    let legacy = if legacy_home_enabled {
        cli_tmux_home_liveness(
            list_cli_tmux_socket_on(legacy_socket, now).await,
            &legacy_cli_tmux_session_name(workspace_id),
        )
    } else {
        CliTmuxHomeLiveness::Absent
    };
    CliTmuxSessionLiveness {
        current: cli_tmux_home_liveness(current, &cli_tmux_session_name(workspace_id)),
        legacy,
    }
}

pub(crate) async fn cli_tmux_session_liveness(workspace_id: Uuid) -> CliTmuxSessionLiveness {
    if !tmux_available() {
        return CliTmuxSessionLiveness {
            current: CliTmuxHomeLiveness::Absent,
            legacy: CliTmuxHomeLiveness::Absent,
        };
    }
    cli_tmux_session_liveness_on(
        workspace_id,
        cli_tmux_socket(),
        legacy_cli_tmux_socket(),
        is_legacy_home_enabled(),
    )
    .await
}

async fn kill_cli_tmux_home_on(
    home: CliTmuxHome,
    workspace_id: Uuid,
    current_socket: &str,
    legacy_socket: &str,
) -> bool {
    let (socket, session_name) =
        cli_tmux_target_on(home, workspace_id, current_socket, legacy_socket);
    run_cli_tmux(&[
        "-L",
        socket,
        "kill-session",
        "-t",
        &format!("={session_name}"),
    ])
    .await
    .is_ok()
}

async fn reap_cli_tmux_session_with_liveness_on(
    workspace_id: Uuid,
    minimum_idle_secs: i64,
    current_socket: &str,
    legacy_socket: &str,
    legacy_home_enabled: bool,
    liveness: CliTmuxSessionLiveness,
) -> usize {
    let mut killed = 0;
    for (home, state) in [
        (CliTmuxHome::Current, liveness.current),
        (CliTmuxHome::Legacy, liveness.legacy),
    ] {
        if home == CliTmuxHome::Legacy && !legacy_home_enabled {
            continue;
        }
        match state {
            CliTmuxHomeLiveness::Present {
                attached: false,
                idle_secs,
            } if idle_secs >= minimum_idle_secs => {
                if kill_cli_tmux_home_on(home, workspace_id, current_socket, legacy_socket).await {
                    killed += 1;
                }
            }
            CliTmuxHomeLiveness::Unknown => tracing::warn!(
                workspace_id = %workspace_id,
                home = ?home,
                "Reaper: CLI tmux home liveness is unknown; leaving it untouched"
            ),
            _ => {}
        }
    }
    killed
}

/// Guarded periodic-reaper kill. Unlike explicit workspace cleanup, this makes
/// an independent fresh decision for each home and never kills an attached,
/// fresh, or unknown home.
pub(crate) async fn reap_cli_tmux_session_if_inactive(
    workspace_id: Uuid,
    minimum_idle_secs: i64,
) -> usize {
    if !tmux_available() {
        return 0;
    }
    let liveness = cli_tmux_session_liveness(workspace_id).await;
    reap_cli_tmux_session_with_liveness_on(
        workspace_id,
        minimum_idle_secs,
        cli_tmux_socket(),
        legacy_cli_tmux_socket(),
        is_legacy_home_enabled(),
        liveness,
    )
    .await
}

#[derive(Debug, Clone, Copy)]
struct TmuxCapabilities {
    available: bool,
    client_flags: bool,
}

/// Parse tmux's stable `tmux -V` output and recognize the first release with
/// per-client flags. A version gate is preferable to probing `refresh-client`
/// because the latter needs a live client and would perturb it; tmux 3.2 added
/// the `-f` client-flag option used by the sizing feature.
fn tmux_version_supports_client_flags(output: &str) -> bool {
    let Some(version) = output.trim().strip_prefix("tmux ") else {
        return false;
    };
    let version = version.strip_prefix("next-").unwrap_or(version);
    let Some((major, minor_and_suffix)) = version.split_once('.') else {
        return false;
    };
    let minor: String = minor_and_suffix
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    let (Ok(major), Ok(minor)) = (major.parse::<u32>(), minor.parse::<u32>()) else {
        return false;
    };
    (major, minor) >= (3, 2)
}

fn tmux_capabilities() -> &'static TmuxCapabilities {
    static CAPABILITIES: OnceLock<TmuxCapabilities> = OnceLock::new();
    CAPABILITIES.get_or_init(|| {
        let output = std::process::Command::new("tmux")
            .arg("-V")
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success());
        let Some(output) = output else {
            tracing::warn!(
                "tmux not found on PATH; CLI mode terminals will degrade to ephemeral shells"
            );
            return TmuxCapabilities {
                available: false,
                client_flags: false,
            };
        };

        let version = String::from_utf8_lossy(&output.stdout);
        let client_flags = tmux_version_supports_client_flags(&version);
        if !client_flags {
            tracing::warn!(
                version = version.trim(),
                "tmux client-flag support could not be confirmed; shared CLI terminal sizing requires a recognized tmux >= 3.2"
            );
        }
        TmuxCapabilities {
            available: true,
            client_flags,
        }
    })
}

/// Whether tmux is on PATH. Checked once per process; when unavailable
/// (e.g. Windows, minimal containers) CLI mode degrades to a bare shell.
pub(crate) fn tmux_available() -> bool {
    tmux_capabilities().available
}

/// Whether tmux can exclude individual clients from `window-size smallest`.
/// Shares the cached `tmux -V` probe with [`tmux_available`].
pub(crate) fn tmux_client_flags_supported() -> bool {
    tmux_capabilities().client_flags
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
    #[error(
        "Could not determine whether the agent session for workspace {0} is already running; no session was created. Please retry"
    )]
    CliTmuxStateUnknown(Uuid),
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

/// Disarm the pty master's `VEOF` (Ctrl-D / end-of-transmission) control char so
/// that dropping `portable-pty`'s writer does NOT inject a keystroke into the
/// terminal.
///
/// `portable-pty` 0.8.1's `UnixMasterWriter::drop` reads the master's termios
/// and, if `c_cc[VEOF]` is non-zero, writes `[b'\n', VEOF]` — LF + Ctrl-D — into
/// the pty. That is the crate's documented "dropping the writer sends EOF to the
/// slave" behavior (registry source `unix.rs:351-363`), and nothing in this
/// crate's own source reveals it — hence this comment. It bites us because our
/// pty master drives a *tmux client's* tty: on teardown the tmux server reads
/// those 2 bytes as client keyboard input and forwards them to the active pane.
/// In the Claude TUI composer the LF inserts a stray newline (never submits) and
/// the Ctrl-D is a no-op; in a bare shell the Enter+Ctrl-D EXITS the shell and
/// kills the persistent `bc_` (or legacy `vk_`) session. Reproduced live 4-6/6
/// teardowns.
///
/// This is a TEARDOWN-ONLY fix: a live shell-mode terminal legitimately needs
/// Ctrl-D/VEOF, so we clear it only right before the writer drops. The writer
/// holds a `dup(2)` of the master fd (`take_writer` → `self.fd.try_clone()`) and
/// termios is a property of the pty device shared across dup'd fds, so clearing
/// `VEOF` here — on the master fd — is seen by the writer's own `tcgetattr`,
/// making its `if eot != 0` guard skip the write.
#[cfg(unix)]
fn disarm_master_eof(master: &dyn portable_pty::MasterPty) {
    let Some(fd) = master.as_raw_fd() else {
        // Runs inside Drop, so never panic — but with no fd there is no
        // disarm, and the writer's Drop may inject; leave a trace so the
        // attach-window tripwire (server side) can be correlated.
        tracing::debug!("pty master has no raw fd; VEOF disarm skipped");
        return;
    };
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut termios) == 0 {
            termios.c_cc[libc::VEOF] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &termios) != 0 {
                tracing::debug!(
                    "tcsetattr failed disarming pty VEOF; writer teardown may \
                     inject \\n+EOT"
                );
            }
        } else {
            tracing::debug!(
                "tcgetattr failed disarming pty VEOF; writer teardown may \
                 inject \\n+EOT"
            );
        }
    }
}

#[cfg(not(unix))]
fn disarm_master_eof(_master: &dyn portable_pty::MasterPty) {}

struct PtySession {
    /// Per-session writer behind its own lock so a blocking PTY write never
    /// holds up the global session registry (see `write`).
    ///
    /// Drop safety lives on `Drop for PtySession` below; the
    /// `writer`-before-`master` declaration order here is only
    /// defense-in-depth, not the load-bearing guarantee.
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
    /// `Drop` checks this before signalling so it never targets a PID that
    /// was already reaped (and possibly recycled) on the natural-exit path.
    child_reaped: Arc<AtomicBool>,
    /// Per-client tmux sizing state. Shell sessions (including CLI fallback
    /// when tmux is absent) have none; PID and presence always exist together.
    tmux_client: Option<CliTmuxClient>,
    _output_handle: thread::JoinHandle<()>,
}

struct CliTmuxClient {
    /// PID of the `tmux new-session/attach` client process. This is the stable
    /// bridge from a web PTY session to tmux's per-client `ignore-size` flag.
    pid: u32,
    /// Kept beside the session so teardown removes the presence record
    /// atomically with the PTY client it describes.
    presence: CliClientPresence,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CliClientPresence {
    pub(crate) visible: bool,
    pub(crate) last_visible_at: Instant,
    /// When `visible` last changed value. Same-value heartbeats must not move
    /// this timestamp: the sizing sweep compares it with tmux's last input to
    /// decide whether input happened after a delayed hidden transition.
    pub(crate) last_changed_at: Instant,
}

impl CliClientPresence {
    fn new(visible: bool) -> Self {
        let now = Instant::now();
        Self {
            visible,
            last_visible_at: now,
            last_changed_at: now,
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Disarm the master's VEOF BEFORE any field is dropped: a custom
        // `Drop::drop` runs ahead of field drops, and the disarm writes
        // persistent pty-device state, so every later writer Drop — the field
        // drop here or an outliving `Arc` clone — reads `VEOF == 0` and skips
        // its `\n` + Ctrl-D injection. See `disarm_master_eof` for why the
        // injection happens at all.
        disarm_master_eof(self.master.as_ref());
        // Kill the PTY child so a read-parked reader thread sees EOF, exits,
        // and reaps it. Without this the reader blocks on its cloned reader
        // forever (dropping `master` doesn't close that clone), leaking a
        // thread + an unreaped child per disconnect. For CLI mode this
        // detaches the tmux CLIENT — the persistent `bc_` or legacy `vk_`
        // server session survives. The `child_reaped` gate skips the signal
        // once the reader has reaped on the natural-exit path; the residual
        // load-then-kill window (reader completes `wait()` between our load
        // and the kill) is a few instructions wide and was accepted in the original
        // close_session design — signaling requires the OS to recycle the
        // PID inside that window.
        //
        // The session is constructed inside `create_session`'s blocking task,
        // so this Drop is the single teardown point EVERY path funnels
        // through: `close_session` (map remove), service shutdown (the
        // sessions `HashMap` dropping), creation failures after spawn, and
        // the caller's future being cancelled at the `.await` (the runtime
        // drops the returned session).
        if !self.child_reaped.load(Ordering::Acquire) {
            let _ = self.child_killer.kill();
        }
    }
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
        let tmux_sockets = (
            cli_tmux_socket().to_string(),
            legacy_cli_tmux_socket().to_string(),
        );
        let tmux_probe_result = match &command {
            PtyCommand::TmuxCli { workspace_id, .. } if tmux_available() => Some(
                locate_cli_tmux_session_on(
                    *workspace_id,
                    &tmux_sockets.0,
                    &tmux_sockets.1,
                    is_legacy_home_enabled(),
                )
                .await,
            ),
            _ => None,
        };
        self.create_session_with_probe_result(
            working_dir,
            cols,
            rows,
            command,
            tmux_sockets,
            tmux_probe_result,
        )
        .await
    }

    /// Test seam for the create-time locator outcome. Production always passes
    /// the real bounded probes; tests can inject `Unknown` and verify that no
    /// tmux client is ever spawned on the supplied scratch sockets.
    async fn create_session_with_probe_result(
        &self,
        working_dir: PathBuf,
        cols: u16,
        rows: u16,
        command: PtyCommand,
        tmux_sockets: (String, String),
        tmux_probe_result: Option<Result<CliTmuxLocation, PtyError>>,
    ) -> Result<(Uuid, mpsc::Receiver<Vec<u8>>), PtyError> {
        let tmux_home = tmux_probe_result.transpose()?.map(|location| location.home);
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
            let (
                tmux_workspace,
                tmux_resume_id,
                tmux_initial_prompt,
                tmux_deferred,
                tmux_connect_hidden,
                tmux_spec,
            ): (
                Option<Uuid>,
                Option<String>,
                Option<String>,
                bool,
                bool,
                Option<CliLaunchSpec>,
            ) = match &command {
                PtyCommand::TmuxCli {
                    workspace_id,
                    resume_session_id,
                    initial_prompt,
                    deferred_prompt_pending,
                    connect_hidden,
                    spec,
                } if tmux_available() => (
                    Some(*workspace_id),
                    resume_session_id.clone(),
                    initial_prompt.clone(),
                    *deferred_prompt_pending,
                    *connect_hidden,
                    Some(spec.clone()),
                ),
                _ => (None, None, None, false, false, None),
            };
            // Client flags are the release valve that makes smallest sizing
            // safe. The cached client-flags capability gates the entire
            // feature; without it smallest sizing would clamp with no release
            // valve.
            let tmux_connect_hidden = tmux_connect_hidden && tmux_client_flags_supported();

            // Never silently break the persistence promise: if CLI mode was
            // requested but tmux is absent, say so in the pane itself.
            if matches!(&command, PtyCommand::TmuxCli { .. }) {
                match tmux_workspace {
                    Some(workspace_id) => {
                        let (_, session_name) = cli_tmux_target_on(
                            tmux_home.unwrap_or(CliTmuxHome::Current),
                            workspace_id,
                            &tmux_sockets.0,
                            &tmux_sockets.1,
                        );
                        tracing::info!(
                            "CLI terminal attaching tmux session {session_name} at {cols}x{rows} in {}",
                            working_dir.display()
                        );
                    }
                    None => {
                        let _ = output_tx.blocking_send(TMUX_MISSING_NOTICE.to_vec());
                    }
                }
            }

            let (mut cmd, shell_name) = if let (Some(workspace_id), Some(spec)) =
                (tmux_workspace, &tmux_spec)
            {
                let home = tmux_home.unwrap_or(CliTmuxHome::Current);
                if home == CliTmuxHome::Legacy
                    && legacy_attach_requires_prompt_staging(
                        tmux_resume_id.as_deref(),
                        tmux_initial_prompt.as_deref(),
                        &spec.prompt_arg,
                    )
                {
                    // The route-time existence probe and this create-time
                    // locator are independent. If the route probe flakes, it
                    // can supply a baked prompt even though locate resolves a
                    // live legacy session. That attach-only arm cannot stage
                    // the file, so fail closed and leave the DB prompt parked
                    // for the next attach's follow-up paste delivery.
                    tracing::warn!(
                        "Refusing legacy CLI attach for workspace {workspace_id}: \
                         initial prompt requires staging; leaving prompt parked"
                    );
                    return Err(PtyError::PromptStageFailed);
                }
                let (socket, session_name) = cli_tmux_target_on(
                    home,
                    workspace_id,
                    &tmux_sockets.0,
                    &tmux_sockets.1,
                );
                // Bring an already-running server in line with our config
                // (options are server-wide; `-f` below only affects a fresh
                // server start).
                ensure_cli_tmux_server_options_on(socket, tmux_client_flags_supported());

                let (conf, bootstrap) = match home {
                    CliTmuxHome::Current => {
                        // Our own config instead of the user's ~/.tmux.conf —
                        // the embedded terminal needs deterministic mouse /
                        // clipboard behavior (see `cli_tmux_conf`).
                        let conf = cli_tmux_conf_path();

                        // Pre-accept the agent's per-directory folder-trust /
                        // first-run dialog before a new pane can launch.
                        maybe_seed_cli_trust(&spec.program, &working_dir);

                        // Materialize the initial prompt to a private file so
                        // the bootstrap reads it instead of carrying it inline.
                        let prompt_file: Option<PathBuf> = match stageable_cli_prompt_content(
                            tmux_resume_id.as_deref(),
                            tmux_initial_prompt.as_deref(),
                            &spec.prompt_arg,
                        ) {
                            Some(content) => Some(
                                write_cli_prompt_file(workspace_id, &content).map_err(|e| {
                                    tracing::error!(
                                        "Failed to write CLI prompt file for {session_name}: \
                                         {e}; leaving prompt parked"
                                    );
                                    remove_cli_prompt_file(workspace_id);
                                    PtyError::PromptStageFailed
                                })?,
                            ),
                            None => None,
                        };
                        let bootstrap = cli_bootstrap(
                            spec,
                            tmux_resume_id.as_deref(),
                            prompt_file.as_deref(),
                            tmux_deferred,
                        );
                        (conf, bootstrap)
                    }
                    // Attach only: if the legacy session vanished after the
                    // locator ran, this fails instead of recreating it.
                    // TODO(bc-legacy-cleanup): remove when no vk_ sessions remain.
                    CliTmuxHome::Legacy => (None, String::new()),
                };

                let argv = cli_tmux_argv_on(CliTmuxArgv {
                    home,
                    workspace_id,
                    current_socket: &tmux_sockets.0,
                    legacy_socket: &tmux_sockets.1,
                    conf: conf.as_deref(),
                    connect_hidden: tmux_connect_hidden,
                    working_dir: &working_dir,
                    bootstrap: &bootstrap,
                });
                let mut cmd = CommandBuilder::new("tmux");
                for arg in argv {
                    cmd.arg(arg);
                }
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

            // `Child` moves into the reader/reaper thread below, so capture its
            // tmux client PID now. Shell-mode PIDs must never enter the presence
            // registry: they have no tmux client whose size flag can be changed.
            let tmux_client_pid = if tmux_workspace.is_some() {
                child.process_id()
            } else {
                None
            };

            // Independent kill handle so teardown can unblock the reader.
            let mut child_killer = child.clone_killer();
            let child_reaped = Arc::new(AtomicBool::new(false));
            let child_reaped_reader = child_reaped.clone();

            // Reader + reaper thread BEFORE the writer exists: from here on,
            // every failure path can kill the child and rely on this thread to
            // reap it, and no failure can strand a VEOF-armed writer.
            let mut reader = match pty_pair.master.try_clone_reader() {
                Ok(reader) => reader,
                Err(e) => {
                    // No reaper thread yet, but the child is still owned here
                    // and this is a blocking task: kill and reap inline so
                    // repeated create failures can't accumulate zombies.
                    let mut child = child;
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(PtyError::CreateFailed(e.to_string()));
                }
            };

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
                // Mark reaped so teardown won't signal a freed/recycled PID.
                child_reaped_reader.store(true, Ordering::Release);
            });

            let mut writer = match pty_pair.master.take_writer() {
                Ok(writer) => writer,
                Err(e) => {
                    // The reaper thread owns the child; kill so it unblocks
                    // and reaps. No writer exists, so nothing can inject.
                    let _ = child_killer.kill();
                    return Err(PtyError::CreateFailed(e.to_string()));
                }
            };

            if shell_name == "zsh" {
                let _ = writer.write_all(b" PROMPT='$ '; RPROMPT=''\n");
                let _ = writer.flush();
                let _ = writer.write_all(b"\x0c");
                let _ = writer.flush();
            }

            // Construct the session INSIDE the blocking task, with no fallible
            // step between `take_writer` above and this point: the writer is
            // never alive outside a `PtySession`, so every teardown — early
            // return, panic-unwind, cancellation of the caller at the `.await`
            // (the runtime then drops this returned session), or normal
            // close — funnels through `Drop for PtySession`, which disarms
            // VEOF and kills the child.
            Ok::<_, PtyError>(PtySession {
                writer: Arc::new(Mutex::new(writer)),
                master: pty_pair.master,
                child_killer,
                child_reaped,
                tmux_client: tmux_client_pid.map(|pid| CliTmuxClient {
                    pid,
                    presence: CliClientPresence::new(!tmux_connect_hidden),
                }),
                _output_handle: output_handle,
            })
        })
        .await
        .map_err(|e| PtyError::CreateFailed(e.to_string()))??;

        self.sessions
            .lock()
            .map_err(|e| PtyError::CreateFailed(e.to_string()))?
            .insert(session_id, result);

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

    /// Record browser visibility for a CLI tmux client and schedule an update
    /// to whether it participates in `window-size smallest`. Shell sessions
    /// and same-state heartbeats are synchronous no-ops outside the registry;
    /// transition reconciliation is bounded, fire-and-forget work because the
    /// periodic activity sweep is the repair path.
    pub fn set_cli_presence(&self, session_id: Uuid, visible: bool) {
        if !tmux_client_flags_supported() {
            return;
        }

        let client_pid = {
            let mut sessions = match self.sessions.lock() {
                Ok(sessions) => sessions,
                Err(e) => {
                    tracing::debug!("Failed to lock CLI presence registry: {e}");
                    return;
                }
            };
            let Some(session) = sessions.get_mut(&session_id) else {
                tracing::debug!("CLI presence session {session_id} no longer exists");
                return;
            };
            let Some(tmux_client) = session.tmux_client.as_mut() else {
                return;
            };
            let presence = &mut tmux_client.presence;
            let now = Instant::now();
            if presence.visible == visible {
                if visible {
                    presence.last_visible_at = now;
                }
                return;
            }

            presence.visible = visible;
            presence.last_changed_at = now;
            if visible {
                presence.last_visible_at = now;
            }
            tmux_client.pid
        };

        // Two rapid transitions can finish out of order; the periodic sweep
        // repairs any resulting divergence within one sweep period.
        tokio::spawn(async move {
            let (socket, client_name) = match cli_tmux_client_name(client_pid).await {
                Ok(Some(location)) => location,
                Ok(None) => {
                    tracing::debug!(
                        "tmux client pid {client_pid} not found for CLI presence update"
                    );
                    return;
                }
                Err(e) => {
                    tracing::debug!("Failed to resolve tmux client pid {client_pid}: {e}");
                    return;
                }
            };
            if let Err(e) =
                refresh_cli_tmux_client_ignore_size(socket, &client_name, !visible).await
            {
                tracing::debug!(
                    "Failed to update ignore-size for tmux client {client_name} on {socket} (pid {client_pid}): {e}"
                );
            }
        });
    }

    /// Snapshot web presence by the tmux client PID used in `list-clients`.
    /// A poisoned registry yields an empty view; the sweep then falls back to
    /// tmux activity instead of letting a monitoring failure affect terminals.
    pub(crate) fn cli_presence_snapshot(&self) -> HashMap<u32, CliClientPresence> {
        let sessions = match self.sessions.lock() {
            Ok(sessions) => sessions,
            Err(e) => {
                tracing::debug!("Failed to snapshot CLI presence registry: {e}");
                return HashMap::new();
            }
        };
        sessions
            .values()
            .filter_map(|session| {
                session
                    .tmux_client
                    .as_ref()
                    .map(|client| (client.pid, client.presence))
            })
            .collect()
    }

    pub async fn close_session(&self, session_id: Uuid) -> Result<(), PtyError> {
        // Dropping the removed session runs the full teardown — VEOF disarm
        // then child kill — in `Drop for PtySession`. Bound OUTSIDE the lock
        // scope so the teardown syscalls never run under the global registry
        // lock. (A send-parked reader is additionally released when the
        // caller drops the output receiver after this returns.)
        let session = self
            .sessions
            .lock()
            .map_err(|_| PtyError::SessionClosed)?
            .remove(&session_id);
        drop(session);
        Ok(())
    }
}

async fn cli_tmux_client_name_on(socket: &str, client_pid: u32) -> Result<Option<String>, String> {
    let output = run_cli_tmux(&[
        "-L",
        socket,
        "list-clients",
        "-F",
        "#{client_pid}\t#{client_name}",
    ])
    .await?;

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let (pid, name) = line.split_once('\t')?;
            (pid.trim().parse::<u32>().ok()? == client_pid).then(|| name.to_string())
        }))
}

pub(crate) async fn cli_tmux_client_name(
    client_pid: u32,
) -> Result<Option<(&'static str, String)>, String> {
    let mut sockets = vec![cli_tmux_socket()];
    if is_legacy_home_enabled() {
        // TODO(bc-legacy-cleanup): remove when no vk_ sessions remain.
        sockets.push(legacy_cli_tmux_socket());
    }
    let mut errors = Vec::new();
    for &socket in &sockets {
        match cli_tmux_client_name_on(socket, client_pid).await {
            Ok(Some(client_name)) => return Ok(Some((socket, client_name))),
            Ok(None) => {}
            Err(error) => errors.push(format!("{socket}: {error}")),
        }
    }
    if errors.len() == sockets.len() {
        Err(errors.join("; "))
    } else {
        Ok(None)
    }
}

pub(crate) async fn refresh_cli_tmux_client_ignore_size(
    socket: &str,
    client_name: &str,
    ignore_size: bool,
) -> Result<(), String> {
    let flag = if ignore_size {
        "ignore-size"
    } else {
        "!ignore-size"
    };
    run_cli_tmux(&[
        "-L",
        socket,
        "refresh-client",
        "-t",
        client_name,
        "-f",
        flag,
    ])
    .await
    .map(|_| ())
}

/// Run a bounded tmux command and preserve its exit status. Only failures to
/// spawn or finish within the deadline are returned as errors; callers that
/// need to distinguish a normal nonzero exit from an unknown outcome use this
/// lower-level helper.
pub(crate) async fn run_cli_tmux_output(args: &[&str]) -> Result<std::process::Output, String> {
    tokio::time::timeout(
        CLI_TMUX_COMMAND_TIMEOUT,
        tokio::process::Command::new("tmux")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| format!("tmux command timed out after {CLI_TMUX_COMMAND_TIMEOUT:?}"))?
    .map_err(|e| e.to_string())
}

pub(crate) async fn run_cli_tmux(args: &[&str]) -> Result<std::process::Output, String> {
    let output = run_cli_tmux_output(args).await?;

    if output.status.success() {
        return Ok(output);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    Err(if stderr.is_empty() {
        format!("tmux command exited with {}", output.status)
    } else {
        format!("tmux command exited with {}: {stderr}", output.status)
    })
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
    fn cli_tmux_socket_pair_uses_both_compiled_defaults() {
        assert_eq!(
            resolve_cli_tmux_sockets(
                None,
                None,
                DEFAULT_CLI_TMUX_SOCKET,
                DEFAULT_LEGACY_CLI_TMUX_SOCKET,
            ),
            CliTmuxSockets {
                current: "bettercoding".to_string(),
                legacy: "vibe-kanban".to_string(),
                legacy_home_enabled: true,
            }
        );
    }

    #[test]
    fn cli_tmux_socket_pair_derives_legacy_from_current_override() {
        assert_eq!(
            resolve_cli_tmux_sockets(
                Some("dev-stack".to_string()),
                None,
                DEFAULT_CLI_TMUX_SOCKET,
                DEFAULT_LEGACY_CLI_TMUX_SOCKET,
            ),
            CliTmuxSockets {
                current: "dev-stack".to_string(),
                legacy: "dev-stack-legacy".to_string(),
                legacy_home_enabled: true,
            }
        );
    }

    #[test]
    fn cli_tmux_socket_pair_respects_both_explicit_overrides() {
        assert_eq!(
            resolve_cli_tmux_sockets(
                Some("dev-stack".to_string()),
                Some("old-dev-stack".to_string()),
                DEFAULT_CLI_TMUX_SOCKET,
                DEFAULT_LEGACY_CLI_TMUX_SOCKET,
            ),
            CliTmuxSockets {
                current: "dev-stack".to_string(),
                legacy: "old-dev-stack".to_string(),
                legacy_home_enabled: true,
            }
        );
    }

    #[test]
    fn cli_tmux_socket_pair_respects_explicit_legacy_with_default_current() {
        assert_eq!(
            resolve_cli_tmux_sockets(
                None,
                Some("legacy-override".to_string()),
                DEFAULT_CLI_TMUX_SOCKET,
                DEFAULT_LEGACY_CLI_TMUX_SOCKET,
            ),
            CliTmuxSockets {
                current: "bettercoding".to_string(),
                legacy: "legacy-override".to_string(),
                legacy_home_enabled: true,
            }
        );
    }

    #[test]
    fn cli_tmux_socket_pair_disables_equal_overrides() {
        assert_eq!(
            resolve_cli_tmux_sockets(
                Some("shared-socket".to_string()),
                Some("shared-socket".to_string()),
                DEFAULT_CLI_TMUX_SOCKET,
                DEFAULT_LEGACY_CLI_TMUX_SOCKET,
            ),
            CliTmuxSockets {
                current: "shared-socket".to_string(),
                legacy: "shared-socket".to_string(),
                legacy_home_enabled: false,
            }
        );
    }

    #[test]
    fn cli_session_names_are_namespaced_and_tmux_safe() {
        let id = Uuid::parse_str("bccad5cc-3bd4-4f80-b75d-35db5f087ac0").unwrap();
        let name = cli_tmux_session_name(id);
        assert_eq!(name, "bc_bccad5cc3bd44f80b75d35db5f087ac0");
        // tmux treats `.` and `:` specially in targets; the name must stay
        // strictly alphanumeric + underscore.
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert!(name.starts_with("bc_"));
    }

    #[test]
    fn current_and_legacy_cli_session_names_round_trip_strictly() {
        let id = Uuid::parse_str("bccad5cc-3bd4-4f80-b75d-35db5f087ac0").unwrap();
        let current = cli_tmux_session_name(id);
        let legacy = legacy_cli_tmux_session_name(id);

        assert_eq!(workspace_id_from_cli_session_name(&current), Some(id));
        assert_eq!(workspace_id_from_cli_session_name(&legacy), Some(id));
        assert_eq!(workspace_id_from_cli_session_name("bc_short"), None);
        assert_eq!(workspace_id_from_cli_session_name("vk_short"), None);
        assert_eq!(
            workspace_id_from_cli_session_name("bc_0000000000000000000000000000000"),
            None
        );
        assert_eq!(
            workspace_id_from_cli_session_name("vk_000000000000000000000000000000000"),
            None
        );
        assert_eq!(workspace_id_from_cli_session_name("other_session"), None);
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

    fn argv_strings(args: Vec<std::ffi::OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn current_home_argv_uses_current_socket_conf_and_attach_or_create() {
        let id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let args = argv_strings(cli_tmux_argv_on(CliTmuxArgv {
            home: CliTmuxHome::Current,
            workspace_id: id,
            current_socket: "bc-b2-test-current",
            legacy_socket: "bc-b2-test-legacy",
            conf: Some(Path::new("/tmp/bc-cli.conf")),
            connect_hidden: true,
            working_dir: Path::new("/tmp/worktree"),
            bootstrap: "agent-bootstrap",
        }));

        assert_eq!(
            args,
            vec![
                "-L",
                "bc-b2-test-current",
                "-f",
                "/tmp/bc-cli.conf",
                "new-session",
                "-f",
                "ignore-size",
                "-A",
                "-s",
                "bc_00000000000000000000000000000001",
                "-c",
                "/tmp/worktree",
                "agent-bootstrap",
            ]
        );
    }

    #[test]
    fn legacy_home_argv_is_attach_only_on_legacy_socket() {
        let id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let args = argv_strings(cli_tmux_argv_on(CliTmuxArgv {
            home: CliTmuxHome::Legacy,
            workspace_id: id,
            current_socket: "bc-b2-test-current",
            legacy_socket: "bc-b2-test-legacy",
            conf: Some(Path::new("/tmp/must-not-be-used.conf")),
            connect_hidden: true,
            working_dir: Path::new("/tmp/must-not-be-used"),
            bootstrap: "must-not-be-used-bootstrap",
        }));

        assert_eq!(
            args,
            vec![
                "-L",
                "bc-b2-test-legacy",
                "attach-session",
                "-f",
                "ignore-size",
                "-t",
                "=vk_00000000000000000000000000000001",
            ]
        );
        assert!(!args.iter().any(|arg| arg == "new-session"));
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
        assert!(!b.contains("bc_p="));
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
        assert!(b.contains("bc_p=\"$(cat '/tmp/vk/cli-prompts/abc.txt')\""));
        assert!(b.contains("rm -f -- '/tmp/vk/cli-prompts/abc.txt'"));
        assert!(
            b.contains("'--dangerously-skip-permissions' \"$bc_p\""),
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
        assert!(b.contains("rm -f -- '/tmp/vk/p.txt'; 'gemini' '-i' \"$bc_p\""));

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
        assert!(!b.contains("bc_p="));
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
    fn tmux_prompt_buffers_use_the_bettercoding_namespace() {
        let id = Uuid::parse_str("bccad5cc-3bd4-4f80-b75d-35db5f087ac0").unwrap();
        assert_eq!(
            cli_tmux_prompt_buffer_name(id, 7),
            "bc_prompt_bccad5cc3bd44f80b75d35db5f087ac0_7"
        );
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
    fn legacy_attach_prompt_guard_matches_current_staging_condition() {
        let resume_id = "bccad5cc-3bd4-4f80-b75d-35db5f087ac0";

        assert!(legacy_attach_requires_prompt_staging(
            None,
            Some("ship it"),
            &CliPromptArg::Positional,
        ));
        assert!(!legacy_attach_requires_prompt_staging(
            Some(resume_id),
            Some("ship it"),
            &CliPromptArg::Positional,
        ));
        assert!(legacy_attach_requires_prompt_staging(
            Some("not-an-active-resume-id"),
            Some("ship it"),
            &CliPromptArg::Positional,
        ));
        assert!(!legacy_attach_requires_prompt_staging(
            None,
            None,
            &CliPromptArg::Positional,
        ));
        assert!(!legacy_attach_requires_prompt_staging(
            None,
            Some("  \n\t"),
            &CliPromptArg::Positional,
        ));
        assert!(!legacy_attach_requires_prompt_staging(
            None,
            Some("ship it"),
            &CliPromptArg::Unsupported,
        ));
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

        // macOS `ps -o comm=` reports a full path that can contain spaces; the
        // whole remainder is the comm and normalize_comm reduces it to the
        // basename, so a space in the path must not truncate the match.
        let ps_macos = "  100     1 sh\n  200   100 /Applications/My App/codex\n";
        assert!(pane_subtree_has_program(ps_macos, 100, "codex"));

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
        for conf in [cli_tmux_conf(false), cli_tmux_conf(true)] {
            assert!(conf.contains("set -g mouse on"));
            assert!(conf.contains("set -s set-clipboard on"));
            assert!(conf.contains("set -as terminal-features ',xterm*:clipboard'"));
            assert!(conf.contains("unbind-key -n MouseDown3Pane"));
            // Window command strings (the POSIX-sh launch bootstrap) are
            // parsed by default-shell; fish/csh would reject them outright.
            assert!(conf.contains("set -g default-shell /bin/sh"));
        }
        assert!(cli_tmux_conf(true).contains("set -g window-size smallest"));
        assert!(!cli_tmux_conf(false).contains("window-size smallest"));
    }

    #[test]
    fn tmux_client_flag_version_gate_handles_release_formats() {
        assert!(tmux_version_supports_client_flags("tmux 3.2a\n"));
        assert!(tmux_version_supports_client_flags("tmux 3.4"));
        assert!(tmux_version_supports_client_flags("tmux next-3.6"));
        assert!(tmux_version_supports_client_flags("tmux 4.0"));
        assert!(!tmux_version_supports_client_flags("tmux 3.1c"));
        assert!(!tmux_version_supports_client_flags("tmux 2.9"));
        assert!(!tmux_version_supports_client_flags("tmux master"));
    }

    #[test]
    fn session_probe_classifies_command_outcomes() {
        assert_eq!(
            classify_cli_tmux_session_probe(true, ""),
            CliTmuxSessionProbe::Present
        );
        assert_eq!(
            classify_cli_tmux_session_probe(false, "can't find session: foo"),
            CliTmuxSessionProbe::Absent
        );
        assert_eq!(
            classify_cli_tmux_session_probe(false, "no server running on /tmp/x"),
            CliTmuxSessionProbe::Absent
        );
        assert_eq!(
            classify_cli_tmux_session_probe(
                false,
                "error connecting to /tmp/x (Permission denied)"
            ),
            CliTmuxSessionProbe::Unknown
        );
        assert_eq!(
            classify_cli_tmux_session_probe(false, ""),
            CliTmuxSessionProbe::Unknown
        );
        assert_eq!(
            classify_cli_tmux_session_probe(false, "some unrecognized message"),
            CliTmuxSessionProbe::Unknown
        );
    }

    struct ScratchTmuxServer {
        socket: String,
    }

    impl ScratchTmuxServer {
        fn start_session(&self, session_name: &str) {
            let output = std::process::Command::new("tmux")
                .args(["-L", &self.socket, "new-session", "-d", "-s", session_name])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .output()
                .expect("start scratch tmux session");
            assert!(
                output.status.success(),
                "scratch tmux session failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    struct ScratchTmuxPair {
        current: ScratchTmuxServer,
        legacy: ScratchTmuxServer,
    }

    fn scratch_tmux_pair() -> ScratchTmuxPair {
        static SOCKET_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SOCKET_SEQ.fetch_add(1, Ordering::Relaxed);
        let base = format!("bc-b2-test-{}-{seq}", std::process::id());
        let pair = ScratchTmuxPair {
            current: ScratchTmuxServer {
                socket: format!("{base}-current"),
            },
            legacy: ScratchTmuxServer {
                socket: format!("{base}-legacy"),
            },
        };
        // Keep both fixture servers alive with an out-of-namespace session so
        // an absent target exercises tmux's definitive "can't find session"
        // response rather than a version-specific missing-socket diagnostic.
        pair.current.start_session("scratch-fixture-sentinel");
        pair.legacy.start_session("scratch-fixture-sentinel");
        pair
    }

    impl Drop for ScratchTmuxServer {
        fn drop(&mut self) {
            let _ = std::process::Command::new("tmux")
                .args(["-L", &self.socket, "kill-server"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }

    #[tokio::test]
    async fn locator_finds_legacy_only_session_on_scratch_pair() {
        if !tmux_available() {
            return;
        }
        let pair = scratch_tmux_pair();
        let workspace_id = Uuid::new_v4();
        pair.legacy
            .start_session(&legacy_cli_tmux_session_name(workspace_id));

        assert_eq!(
            locate_cli_tmux_session_on(
                workspace_id,
                &pair.current.socket,
                &pair.legacy.socket,
                true,
            )
            .await
            .expect("scratch probes should be definitive"),
            CliTmuxLocation {
                home: CliTmuxHome::Legacy,
                present: true,
            }
        );
    }

    #[tokio::test]
    async fn located_target_stays_pinned_when_current_home_appears() {
        if !tmux_available() {
            return;
        }
        let pair = scratch_tmux_pair();
        let workspace_id = Uuid::new_v4();
        pair.legacy
            .start_session(&legacy_cli_tmux_session_name(workspace_id));

        let location = locate_cli_tmux_session_on(
            workspace_id,
            &pair.current.socket,
            &pair.legacy.socket,
            true,
        )
        .await
        .expect("scratch probes should be definitive");
        assert_eq!(location.home, CliTmuxHome::Legacy);
        let target = owned_cli_tmux_target_on(
            location.home,
            workspace_id,
            &pair.current.socket,
            &pair.legacy.socket,
        );

        pair.current
            .start_session(&cli_tmux_session_name(workspace_id));
        assert_eq!(
            locate_cli_tmux_session_on(
                workspace_id,
                &pair.current.socket,
                &pair.legacy.socket,
                true,
            )
            .await
            .expect("scratch probes should be definitive")
            .home,
            CliTmuxHome::Current,
        );
        assert_eq!(target.socket, pair.legacy.socket);
        assert_eq!(
            target.session_name,
            legacy_cli_tmux_session_name(workspace_id)
        );
        assert!(cli_tmux_target_exists(&target).await);
    }

    #[tokio::test]
    async fn locator_defaults_to_current_on_empty_scratch_pair() {
        if !tmux_available() {
            return;
        }
        let pair = scratch_tmux_pair();
        assert_eq!(
            locate_cli_tmux_session_on(
                Uuid::new_v4(),
                &pair.current.socket,
                &pair.legacy.socket,
                true,
            )
            .await
            .expect("empty scratch probes should be definitive"),
            CliTmuxLocation {
                home: CliTmuxHome::Current,
                present: false,
            }
        );
    }

    #[tokio::test]
    async fn locator_prefers_current_when_both_scratch_homes_exist() {
        if !tmux_available() {
            return;
        }
        let pair = scratch_tmux_pair();
        let workspace_id = Uuid::new_v4();
        pair.current
            .start_session(&cli_tmux_session_name(workspace_id));
        pair.legacy
            .start_session(&legacy_cli_tmux_session_name(workspace_id));

        assert_eq!(
            locate_cli_tmux_session_on(
                workspace_id,
                &pair.current.socket,
                &pair.legacy.socket,
                true,
            )
            .await
            .expect("scratch probes should be definitive"),
            CliTmuxLocation {
                home: CliTmuxHome::Current,
                present: true,
            }
        );
    }

    #[tokio::test]
    async fn unknown_probe_makes_create_fail_without_starting_a_scratch_session() {
        if !tmux_available() {
            return;
        }
        let pair = scratch_tmux_pair();
        let workspace_id = Uuid::new_v4();
        let probe_result = resolve_cli_tmux_session_probes(
            workspace_id,
            CliTmuxSessionProbe::Absent,
            CliTmuxSessionProbe::Unknown,
        );
        let command = PtyCommand::TmuxCli {
            workspace_id,
            resume_session_id: None,
            initial_prompt: None,
            deferred_prompt_pending: false,
            connect_hidden: false,
            spec: claude_spec(&[]),
        };
        let working_dir = tempfile::tempdir().expect("scratch working directory");

        let error = PtyService::new()
            .create_session_with_probe_result(
                working_dir.path().to_path_buf(),
                80,
                24,
                command,
                (pair.current.socket.clone(), pair.legacy.socket.clone()),
                Some(probe_result),
            )
            .await
            .expect_err("unknown state must fail before spawning tmux");

        assert!(matches!(error, PtyError::CliTmuxStateUnknown(id) if id == workspace_id));
        assert_eq!(
            probe_tmux_session_on(&pair.current.socket, &cli_tmux_session_name(workspace_id)).await,
            CliTmuxSessionProbe::Absent
        );
        assert_eq!(
            probe_tmux_session_on(
                &pair.legacy.socket,
                &legacy_cli_tmux_session_name(workspace_id)
            )
            .await,
            CliTmuxSessionProbe::Absent
        );
    }

    #[tokio::test]
    async fn session_exists_finds_legacy_only_session_on_scratch_pair() {
        if !tmux_available() {
            return;
        }
        let pair = scratch_tmux_pair();
        let workspace_id = Uuid::new_v4();
        pair.legacy
            .start_session(&legacy_cli_tmux_session_name(workspace_id));

        assert!(
            cli_tmux_session_exists_on(
                workspace_id,
                &pair.current.socket,
                &pair.legacy.socket,
                true,
            )
            .await
        );
    }

    #[tokio::test]
    async fn session_list_sweeps_current_and_legacy_scratch_sockets() {
        if !tmux_available() {
            return;
        }
        let pair = scratch_tmux_pair();
        let current_id = Uuid::new_v4();
        let legacy_id = Uuid::new_v4();
        pair.current
            .start_session(&cli_tmux_session_name(current_id));
        pair.legacy
            .start_session(&legacy_cli_tmux_session_name(legacy_id));

        let sessions = list_cli_tmux_sessions_on(&[&pair.current.socket, &pair.legacy.socket])
            .await
            .expect("both scratch socket snapshots should succeed");
        assert!(sessions.iter().any(|(id, _, _)| *id == current_id));
        assert!(sessions.iter().any(|(id, _, _)| *id == legacy_id));
    }

    #[tokio::test]
    async fn failed_socket_snapshot_aborts_reaper_round_before_any_kill() {
        if !tmux_available() {
            return;
        }
        let pair = scratch_tmux_pair();
        let workspace_id = Uuid::new_v4();
        let current_name = cli_tmux_session_name(workspace_id);
        let legacy_name = legacy_cli_tmux_session_name(workspace_id);
        pair.current.start_session(&current_name);
        pair.legacy.start_session(&legacy_name);

        // The failed current-socket snapshot may be hiding an attached copy.
        // Even though the visible legacy row looks reapable, the periodic
        // round gets no candidates and therefore cannot kill either home.
        let round = merge_cli_tmux_socket_snapshots([
            CliTmuxSocketSnapshot::Failed("injected current query failure".to_string()),
            CliTmuxSocketSnapshot::Rows(vec![CliTmuxSessionRow {
                session_name: legacy_name.clone(),
                workspace_id,
                attached: false,
                idle_secs: 10_000,
            }]),
        ]);
        assert!(round.is_err(), "a partial snapshot must abort the round");

        assert_eq!(
            probe_tmux_session_on(&pair.current.socket, &current_name).await,
            CliTmuxSessionProbe::Present
        );
        assert_eq!(
            probe_tmux_session_on(&pair.legacy.socket, &legacy_name).await,
            CliTmuxSessionProbe::Present
        );
    }

    #[tokio::test]
    async fn guarded_reaper_kills_idle_legacy_but_keeps_fresh_current_home() {
        if !tmux_available() {
            return;
        }
        let pair = scratch_tmux_pair();
        let workspace_id = Uuid::new_v4();
        let current_name = cli_tmux_session_name(workspace_id);
        let legacy_name = legacy_cli_tmux_session_name(workspace_id);
        pair.current.start_session(&current_name);
        pair.legacy.start_session(&legacy_name);

        let killed = reap_cli_tmux_session_with_liveness_on(
            workspace_id,
            60,
            &pair.current.socket,
            &pair.legacy.socket,
            true,
            CliTmuxSessionLiveness {
                current: CliTmuxHomeLiveness::Present {
                    attached: false,
                    idle_secs: 5,
                },
                legacy: CliTmuxHomeLiveness::Present {
                    attached: false,
                    idle_secs: 120,
                },
            },
        )
        .await;

        assert_eq!(killed, 1);
        assert_eq!(
            probe_tmux_session_on(&pair.current.socket, &current_name).await,
            CliTmuxSessionProbe::Present
        );
        assert_eq!(
            probe_tmux_session_on(&pair.legacy.socket, &legacy_name).await,
            CliTmuxSessionProbe::Absent
        );
    }

    #[tokio::test]
    async fn guarded_reaper_never_kills_an_unknown_home() {
        if !tmux_available() {
            return;
        }
        let pair = scratch_tmux_pair();
        let workspace_id = Uuid::new_v4();
        let current_name = cli_tmux_session_name(workspace_id);
        pair.current.start_session(&current_name);

        let killed = reap_cli_tmux_session_with_liveness_on(
            workspace_id,
            0,
            &pair.current.socket,
            &pair.legacy.socket,
            true,
            CliTmuxSessionLiveness {
                current: CliTmuxHomeLiveness::Unknown,
                legacy: CliTmuxHomeLiveness::Absent,
            },
        )
        .await;

        assert_eq!(killed, 0);
        assert_eq!(
            probe_tmux_session_on(&pair.current.socket, &current_name).await,
            CliTmuxSessionProbe::Present
        );
    }

    #[test]
    fn double_homed_liveness_keeps_attached_and_most_recent_state() {
        let workspace_id = Uuid::new_v4();
        let mut sessions = Vec::new();
        merge_cli_tmux_session_liveness(&mut sessions, (workspace_id, false, 900));
        merge_cli_tmux_session_liveness(&mut sessions, (workspace_id, true, 30));

        assert_eq!(sessions, vec![(workspace_id, true, 30)]);
    }

    #[tokio::test]
    async fn kill_removes_legacy_session_on_scratch_socket() {
        if !tmux_available() {
            return;
        }
        let pair = scratch_tmux_pair();
        let workspace_id = Uuid::new_v4();
        let legacy_name = legacy_cli_tmux_session_name(workspace_id);
        pair.legacy.start_session(&legacy_name);
        assert_eq!(
            probe_tmux_session_on(&pair.legacy.socket, &legacy_name).await,
            CliTmuxSessionProbe::Present
        );

        kill_cli_tmux_sessions_on(
            workspace_id,
            &pair.current.socket,
            &pair.legacy.socket,
            true,
        )
        .await;

        assert_eq!(
            probe_tmux_session_on(&pair.legacy.socket, &legacy_name).await,
            CliTmuxSessionProbe::Absent
        );
    }

    #[test]
    fn ensure_reconciles_window_size_in_both_capability_directions() {
        if !tmux_client_flags_supported() {
            return;
        }

        static SOCKET_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SOCKET_SEQ.fetch_add(1, Ordering::Relaxed);
        let socket = format!("bc-smallest-test-{}-{seq}", std::process::id());
        let server = ScratchTmuxServer {
            socket: socket.clone(),
        };

        let dir = tempfile::tempdir().expect("scratch tmux config dir");
        let conf = dir.path().join("old-cli-tmux.conf");
        std::fs::write(
            &conf,
            "set -s set-clipboard on\nset -g default-shell /bin/bash\n",
        )
        .expect("write old-style tmux config");
        let started = std::process::Command::new("tmux")
            .args(["-L", &socket, "-f"])
            .arg(&conf)
            .args(["new-session", "-d", "-s", "old-server"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .expect("start scratch tmux server");
        assert!(
            started.status.success(),
            "scratch tmux server failed: {}",
            String::from_utf8_lossy(&started.stderr)
        );

        ensure_cli_tmux_server_options_on(&socket, true);

        let shown = std::process::Command::new("tmux")
            .args(["-L", &socket, "show-options", "-gv", "window-size"])
            .output()
            .expect("show scratch window-size");
        assert!(shown.status.success(), "show window-size must succeed");
        assert_eq!(String::from_utf8_lossy(&shown.stdout).trim(), "smallest");

        let shown = std::process::Command::new("tmux")
            .args(["-L", &socket, "show-options", "-gv", "default-shell"])
            .output()
            .expect("show scratch default-shell");
        assert!(shown.status.success(), "show default-shell must succeed");
        assert_eq!(String::from_utf8_lossy(&shown.stdout).trim(), "/bin/sh");

        ensure_cli_tmux_server_options_on(&socket, false);

        let shown = std::process::Command::new("tmux")
            .args(["-L", &socket, "show-options", "-gv", "window-size"])
            .output()
            .expect("show downgraded scratch window-size");
        assert!(shown.status.success(), "show window-size must succeed");
        assert_eq!(String::from_utf8_lossy(&shown.stdout).trim(), "latest");

        drop(server);
    }

    #[test]
    fn parse_cli_session_line_reads_attached_and_idle() {
        for id in [
            "bc_00000000000000000000000000000001",
            "vk_00000000000000000000000000000001",
        ] {
            // activity 900, now 1000 -> idle 100; attached "0" -> false
            let (_, attached, idle) =
                parse_cli_session_line(&format!("{id}\t0\t900"), 1000).expect("valid line parses");
            assert!(!attached);
            assert_eq!(idle, 100);
            // attached count > 0 -> true
            let (_, attached, _) = parse_cli_session_line(&format!("{id}\t1\t900"), 1000).unwrap();
            assert!(attached);
        }
    }

    #[test]
    fn parse_cli_session_line_rejects_malformed() {
        for id in [
            "bc_00000000000000000000000000000001",
            "vk_00000000000000000000000000000001",
        ] {
            // Empty fields — the `tmux display-message` failure mode that silently
            // disabled the reaper — must NOT parse to a bogus liveness value.
            assert!(parse_cli_session_line(&format!("{id}\t\t"), 1000).is_none());
            // Missing columns.
            assert!(parse_cli_session_line(id, 1000).is_none());
        }
        // Non-vk session names are ignored entirely.
        assert!(parse_cli_session_line("misc\t0\t900", 1000).is_none());
    }

    /// Read the master fd's `VEOF` control char, or `None` if it has no fd /
    /// `tcgetattr` fails. Test-only mirror of the read side of
    /// `disarm_master_eof`.
    #[cfg(unix)]
    fn master_veof(master: &dyn portable_pty::MasterPty) -> Option<libc::cc_t> {
        let fd = master.as_raw_fd()?;
        unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut termios) == 0 {
                Some(termios.c_cc[libc::VEOF])
            } else {
                None
            }
        }
    }

    /// Same `openpty` path as `create_session`.
    #[cfg(unix)]
    fn open_test_pty() -> portable_pty::PtyPair {
        NativePtySystem::default()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty")
    }

    /// The core of the stray-newline/EOT fix: after `disarm_master_eof`,
    /// `portable-pty`'s writer-Drop reads `VEOF == 0` and skips its `\n` +
    /// Ctrl-D injection.
    #[cfg(unix)]
    #[test]
    fn disarm_master_eof_zeroes_veof() {
        let pair = open_test_pty();

        // Guard against the helper silently no-oping: a fresh pty must have
        // VEOF armed (Ctrl-D == 4 by default) for the disarm to be meaningful.
        let before = master_veof(pair.master.as_ref()).expect("tcgetattr before");
        assert_ne!(before, 0, "a fresh pty master should have VEOF armed");

        disarm_master_eof(pair.master.as_ref());

        let after = master_veof(pair.master.as_ref()).expect("tcgetattr after");
        assert_eq!(after, 0, "VEOF must be disarmed after disarm_master_eof");
    }

    /// `Drop for PtySession` is the single teardown point every path funnels
    /// through (close_session, shutdown, creation failure, cancelled create):
    /// dropping a session must disarm VEOF on the pty device (so the writer's
    /// own Drop injects nothing) AND kill the child so the reader thread
    /// reaps it.
    #[cfg(unix)]
    #[test]
    fn pty_session_drop_disarms_veof_and_reaps_child() {
        use std::os::fd::{AsRawFd as _, BorrowedFd};

        let portable_pty::PtyPair { master, slave } = open_test_pty();

        // Independent dup of the master so the pty device (and its termios)
        // stays observable after the session — and its master fd — drops.
        let probe = {
            let raw = master.as_raw_fd().expect("master raw fd");
            let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
            borrowed.try_clone_to_owned().expect("dup master fd")
        };

        let mut cmd = CommandBuilder::new("sleep");
        cmd.arg("30");
        let child = slave.spawn_command(cmd).expect("spawn child");
        // Mirror create_session: the slave fd is not held beyond spawn, so
        // the child's exit closes the last slave and EOFs the reader.
        drop(slave);

        let child_killer = child.clone_killer();
        let child_reaped = Arc::new(AtomicBool::new(false));
        let child_reaped_reader = child_reaped.clone();
        let mut reader = master.try_clone_reader().expect("clone reader");
        let output_handle = thread::spawn(move || {
            let mut child = child;
            let mut buf = [0u8; 256];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            let _ = child.wait();
            child_reaped_reader.store(true, Ordering::Release);
        });
        let writer = master.take_writer().expect("take writer");

        let session = PtySession {
            writer: Arc::new(Mutex::new(writer)),
            master,
            child_killer,
            child_reaped: child_reaped.clone(),
            tmux_client: None,
            _output_handle: output_handle,
        };
        drop(session);

        // VEOF disarmed on the device (visible through the probe dup).
        let veof = unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            assert_eq!(
                libc::tcgetattr(probe.as_raw_fd(), &mut termios),
                0,
                "probe tcgetattr"
            );
            termios.c_cc[libc::VEOF]
        };
        assert_eq!(veof, 0, "session drop must disarm VEOF");

        // Child killed -> reader EOFs -> thread reaps. Bounded wait; the
        // `sleep 30` bounds a kill failure to a test failure, not a hang.
        for _ in 0..200 {
            if child_reaped.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(
            child_reaped.load(Ordering::Acquire),
            "session drop must kill the child so the reader thread reaps it"
        );
    }
}
