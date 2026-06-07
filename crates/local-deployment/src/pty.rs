use std::{
    collections::HashMap,
    io::{Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
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
    TmuxCli { session_name: String },
}

/// Initial window command for new CLI tmux sessions: run the interactive
/// `claude` TUI when installed, then drop to a shell instead of ending the
/// session (so a crashed/exited claude leaves a usable pane where
/// `claude --continue` resumes the conversation). Ignored by `-A` attaches.
///
/// TODO(profile-integration): this hardcoded invocation must converge on the
/// executor profile system (`ExecutorProfileId`) before growing any argument
/// (model, flags, alternate agent CLIs) — do not bolt options onto
/// `PtyCommand::TmuxCli` piecemeal.
const CLI_BOOTSTRAP: &str =
    r#"command -v claude >/dev/null 2>&1 && claude; exec "${SHELL:-/bin/sh}""#;

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
const CLI_TMUX_SOCKET: &str = "vibe-kanban";

/// tmux session name for a workspace's CLI-mode terminal. The `vk_` namespace
/// is ours: creation, attach, and cleanup only ever target these names.
/// `simple()` (32 hex chars, no hyphens) avoids tmux-special characters.
pub fn cli_tmux_session_name(workspace_id: Uuid) -> String {
    format!("vk_{}", workspace_id.simple())
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
fn tmux_available() -> bool {
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
    writer: Box<dyn Write + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
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
            let tmux_session = match &command {
                PtyCommand::TmuxCli { session_name } if tmux_available() => {
                    Some(session_name.clone())
                }
                _ => None,
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
                let mut cmd = CommandBuilder::new("tmux");
                // Dedicated socket isolates our sessions from the user's tmux.
                cmd.arg("-L");
                cmd.arg(CLI_TMUX_SOCKET);
                cmd.arg("new-session");
                // -A: attach if the session exists, else create.
                // -D: detach any other client first, so one workspace pane has
                //     a single owner — avoids two browser windows fighting the
                //     same TUI and the "smallest client wins" resize churn.
                cmd.arg("-A");
                cmd.arg("-D");
                cmd.arg("-s");
                cmd.arg(session_name);
                cmd.arg("-c");
                cmd.arg(&working_dir);
                cmd.arg(CLI_BOOTSTRAP);
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
            });

            Ok::<_, PtyError>((pty_pair.master, writer, output_handle))
        })
        .await
        .map_err(|e| PtyError::CreateFailed(e.to_string()))??;

        let (master, writer, output_handle) = result;

        let session = PtySession {
            writer,
            master,
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
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|e| PtyError::WriteFailed(e.to_string()))?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or(PtyError::SessionNotFound(session_id))?;

        if session.closed {
            return Err(PtyError::SessionClosed);
        }

        session
            .writer
            .write_all(data)
            .map_err(|e| PtyError::WriteFailed(e.to_string()))?;

        session
            .writer
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
            session.closed = true;
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
        assert!(CLI_BOOTSTRAP.contains("command -v claude"));
        assert!(
            CLI_BOOTSTRAP.ends_with(r#"exec "${SHELL:-/bin/sh}""#),
            "bootstrap must keep the pane alive after claude exits"
        );
    }
}
