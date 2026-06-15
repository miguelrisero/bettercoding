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

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use thiserror::Error;
use tokio::sync::mpsc;
use utils::shell::get_interactive_shell;
use uuid::Uuid;

/// What to run on the PTY.
#[derive(Debug, Clone)]
pub enum PtyCommand {
    /// The user's interactive shell (default side-terminal behavior).
    Shell,
    /// Attach-or-create a namespaced tmux session running the CLI bootstrap.
    /// `tmux new-session -A` attaches when the session already exists, so the
    /// session (and whatever runs inside it) survives WebSocket disconnects
    /// and server restarts; reconnects reattach instead of respawning.
    TmuxCli {
        session_name: String,
        /// claude's own session UUID to resume (the workspace's selected uix
        /// chat). When set, the bootstrap runs `claude --resume <id>` so CLI
        /// mode joins the *exact* conversation the chat UI is showing, and
        /// follow-ups from either side share one transcript.
        resume_session_id: Option<String>,
        /// The workspace's initial prompt (CLI-first creation): handed to
        /// interactive claude as its argument so the run happens visibly in
        /// the terminal instead of a headless executor. Ignored when
        /// `resume_session_id` is set.
        initial_prompt: Option<String>,
        /// Profile-derived flags for the interactive `claude` launch
        /// (currently `--model`/`--effort`), pre-resolved from the session's
        /// selected ExecutorConfig. Empty falls back to claude's own defaults;
        /// the resolver defaults a fresh CLI start to Opus at max effort.
        agent_args: Vec<String>,
    },
}

/// Build the initial window command for a new CLI tmux session. Runs the
/// interactive `claude` TUI when installed, then drops to a shell instead of
/// ending the session (so a crashed/exited claude leaves a usable pane).
/// Ignored by `-A` attaches (only runs when the session is first created).
///
/// - `--resume <id>` (when `resume_session_id` is a valid UUID) joins claude's
///   exact session — the same transcript the headless executor created/uses —
///   so the chat UI and CLI hand off the conversation in both directions.
/// - With nothing to resume, claude starts a FRESH TUI. Never `--continue`:
///   on a brand-new workspace it printed "No conversation found to continue"
///   and dumped the pane into a bare shell (the resume target is resolved
///   workspace-wide server-side, so a missing id really means there is no
///   conversation yet).
/// - `--dangerously-skip-permissions` skips per-tool approval prompts for this
///   trusted worktree. claude's one-time folder-trust dialog is separate and
///   has no flag/setting to suppress; we pre-accept it for the worktree in
///   `ensure_claude_folder_trusted` (the workspace is app-created, so trusted)
///   before this bootstrap runs.
/// - `agent_args` carries profile-derived flags (`--model`/`--effort`) resolved
///   from the session's selected ExecutorConfig, so CLI mode honors the same
///   model + reasoning effort as the headless executor.
///
/// TODO(profile-integration): model/effort now flow through `agent_args`, but a
/// full convergence on the executor profile system (`ExecutorProfileId`,
/// alternate agent CLIs) is still future work — keep new options flowing as
/// pre-resolved `agent_args` rather than bolting fields onto `PtyCommand`.
fn cli_bootstrap(
    resume_session_id: Option<&str>,
    initial_prompt: Option<&str>,
    agent_args: &[String],
) -> String {
    // Profile-derived flags (model/effort) applied to EVERY launch form below,
    // shell-quoted so a model id can never break out of the command.
    let flags: String = agent_args
        .iter()
        .map(|arg| format!(" {}", shell_single_quote(arg)))
        .collect();
    let base = format!("claude{flags} --dangerously-skip-permissions");

    // Only a strict UUID may be interpolated into the shell string. claude
    // session ids are UUIDs, so this both validates intent and forecloses
    // shell injection via the id.
    let launch = if let Some(id) = resume_session_id.filter(|id| is_uuid(id)) {
        format!("{base} --resume {id}")
    } else if let Some(prompt) = initial_prompt.map(str::trim).filter(|p| !p.is_empty()) {
        // CLI-first creation: the workspace prompt becomes claude's argument
        // (single-quote escaped — the prompt is arbitrary user text). A
        // leading space neutralizes prompts that start with '-' so they can
        // never parse as flags.
        let guarded = if prompt.starts_with('-') {
            format!(" {prompt}")
        } else {
            prompt.to_string()
        };
        format!("{base} {}", shell_single_quote(&guarded))
    } else {
        // Nothing explicit to run: resume the most recent conversation in
        // this cwd if one exists (a CLI-first workspace whose tmux session
        // died), otherwise fall through to a fresh TUI — `--continue` exits
        // non-zero when there is no conversation to continue.
        format!("{base} --continue || {base}")
    };

    format!(r#"command -v claude >/dev/null 2>&1 && {launch}; exec "${{SHELL:-/bin/sh}}""#)
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
const CLI_TMUX_CONF: &str = "\
# BetterCoding embedded terminal tmux server (socket: vibe-kanban).
# Written by the backend before each CLI terminal attach - edits are overwritten.
set -g mouse on
set -s set-clipboard on
set -as terminal-features ',xterm*:clipboard'
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
    tokio::process::Command::new("tmux")
        .args([
            "-L",
            CLI_TMUX_SOCKET,
            "has-session",
            "-t",
            &format!("={session_name}"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
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
    if !tmux_available() {
        return;
    }
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

#[derive(Debug, Error)]
pub enum PtyError {
    #[error("Failed to create PTY: {0}")]
    CreateFailed(String),
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
            let (tmux_session, tmux_resume_id, tmux_initial_prompt, tmux_agent_args) =
                match &command {
                    PtyCommand::TmuxCli {
                        session_name,
                        resume_session_id,
                        initial_prompt,
                        agent_args,
                    } if tmux_available() => (
                        Some(session_name.clone()),
                        resume_session_id.clone(),
                        initial_prompt.clone(),
                        agent_args.clone(),
                    ),
                    _ => (None, None, None, Vec::new()),
                };

            // Never silently break the persistence promise: if CLI mode was
            // requested but tmux is absent, say so in the pane itself.
            if matches!(&command, PtyCommand::TmuxCli { .. }) {
                match &tmux_session {
                    Some(session_name) => tracing::info!(
                        "CLI terminal attaching tmux session {session_name} in {}",
                        working_dir.display()
                    ),
                    None => {
                        let _ = output_tx.blocking_send(TMUX_MISSING_NOTICE.to_vec());
                    }
                }
            }

            let (mut cmd, shell_name) = if let Some(session_name) = &tmux_session {
                // Bring an already-running server in line with our config
                // (options are server-wide; `-f` below only affects a fresh
                // server start).
                ensure_cli_tmux_server_options();

                // Pre-accept claude's per-directory folder-trust dialog for
                // this app-created worktree so the launch never blocks on it.
                ensure_claude_folder_trusted(&working_dir);

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
                cmd.arg(session_name);
                cmd.arg("-c");
                cmd.arg(&working_dir);
                cmd.arg(cli_bootstrap(
                    tmux_resume_id.as_deref(),
                    tmux_initial_prompt.as_deref(),
                    &tmux_agent_args,
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

    #[test]
    fn cli_bootstrap_runs_claude_then_drops_to_shell() {
        let b = cli_bootstrap(None, None, &[]);
        assert!(b.contains("command -v claude"));
        assert!(
            b.ends_with(r#"exec "${SHELL:-/bin/sh}""#),
            "bootstrap must keep the pane alive after claude exits"
        );
    }

    #[test]
    fn cli_bootstrap_resume_takes_precedence_and_rejects_non_uuids() {
        // A valid claude session UUID -> --resume <id>, even if a prompt is
        // also present (an existing conversation always wins).
        let id = "28b98f08-5f5f-4b1e-8c4e-41ae87c0c706";
        let b = cli_bootstrap(Some(id), Some("do things"), &[]);
        assert!(b.contains(&format!("--resume {id}")));
        assert!(!b.contains("do things"));
        // Non-UUID (injection attempt) is rejected and never interpolated.
        let evil = "x; rm -rf ~";
        let b = cli_bootstrap(Some(evil), None, &[]);
        assert!(!b.contains("rm -rf"));
        assert!(!b.contains("--resume"));
    }

    #[test]
    fn cli_bootstrap_passes_initial_prompt_injection_safe() {
        let b = cli_bootstrap(None, Some("Fix the login bug"), &[]);
        assert!(b.contains("claude --dangerously-skip-permissions 'Fix the login bug'"));

        // Quotes and shell metacharacters stay inert inside the quoting.
        let evil = "'; rm -rf ~; echo '";
        let b = cli_bootstrap(None, Some(evil), &[]);
        // The single quotes in the prompt are escaped as '\'' — the raw
        // sequence `'; rm` can therefore never terminate the quoting.
        assert!(b.contains(r"'"), "quotes must be escaped: {b}");
        assert!(!b.contains("&& rm"), "injection must not escape: {b}");

        // A prompt starting with '-' is space-guarded so claude can't parse
        // it as a flag.
        let dashy = cli_bootstrap(None, Some("-rf is a flag-looking prompt"), &[]);
        assert!(dashy.contains("' -rf is a flag-looking prompt'"));

        // Blank prompts fall through to the no-prompt path.
        let blank = cli_bootstrap(None, Some("   "), &[]);
        assert!(blank.contains("--continue || claude"));
    }

    #[test]
    fn cli_bootstrap_falls_back_to_continue_then_fresh() {
        // With nothing explicit to run: resume the cwd's latest conversation
        // when one exists (CLI-first workspace after tmux death), else a
        // fresh TUI — never a stranded "No conversation found" pane.
        let b = cli_bootstrap(None, None, &[]);
        assert!(b.contains(
            "claude --dangerously-skip-permissions --continue || claude --dangerously-skip-permissions"
        ));
    }

    #[test]
    fn cli_bootstrap_applies_model_and_effort_flags() {
        let args = ["--model", "opus", "--effort", "max"].map(String::from);
        let b = cli_bootstrap(None, None, &args);
        assert!(
            b.contains("claude '--model' 'opus' '--effort' 'max' --dangerously-skip-permissions")
        );
    }

    #[test]
    fn cli_bootstrap_shell_quotes_agent_args_on_every_form() {
        // Glob/metacharacters in a model id stay inert (single-quoted)...
        let args = ["--model".to_string(), "opus[1m]".to_string()];
        let b = cli_bootstrap(None, None, &args);
        assert!(b.contains("'--model' 'opus[1m]'"));
        // ...and the flags ride the continue/fresh fallback too.
        assert!(b.contains("--dangerously-skip-permissions --continue"));
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
    }
}
