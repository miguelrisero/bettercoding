use std::{
    path::{Path, PathBuf},
    str::FromStr,
};

use axum::{
    Router,
    extract::{Query, State, ws::Message},
    response::IntoResponse,
    routing::get,
};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use db::models::{
    coding_agent_turn::CodingAgentTurn, execution_process::ExecutionProcess, session::Session,
    workspace::Workspace, workspace_repo::WorkspaceRepo,
};
use deployment::Deployment;
use executors::{
    actions::ExecutorActionType,
    executors::{BaseCodingAgent, StandardCodingAgentExecutor, cli::CliLaunchSpec},
    profile::{ExecutorConfig, ExecutorConfigs},
};
use local_deployment::pty::{
    PtyCommand, cli_tmux_available, cli_tmux_session_exists, cli_tmux_session_name,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::{
    DeploymentImpl,
    error::ApiError,
    middleware::signed_ws::{MaybeSignedWebSocket, SignedWsUpgrade},
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TerminalMode {
    /// Plain interactive shell (default side terminal).
    #[default]
    Shell,
    /// Persistent tmux-backed session running the interactive `claude` CLI.
    Cli,
}

#[derive(Debug, Deserialize)]
struct TerminalQuery {
    pub workspace_id: Uuid,
    /// `None` (fell back to 80x24) is distinguishable from an explicit value
    /// so the stray-newline regression tripwire below can warn on true
    /// absence without false-positiving on a pane that genuinely measures
    /// 80x24.
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    #[serde(default)]
    mode: TerminalMode,
    /// VibeKanban session whose claude conversation CLI mode should resume,
    /// so the terminal joins the exact chat the UI is showing (handover).
    #[serde(default)]
    session_id: Option<Uuid>,
}

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TerminalCommand {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TerminalMessage {
    Output { data: String },
    Error { message: String },
}

/// Recover the model + reasoning effort the workspace's CLI session should
/// launch claude with. CLI-first creation persists the picked selection on the
/// session; chat/automation sessions fall back to the most recent coding-agent
/// run's `executor_config`. `(None, None)` means none was found and the launch
/// uses its own defaults (Opus at max effort).
async fn resolve_cli_model_effort(
    pool: &SqlitePool,
    session: Option<&Session>,
) -> (Option<String>, Option<String>) {
    let Some(session) = session else {
        return (None, None);
    };

    // CLI-first: the selection persisted at creation wins.
    if let Ok((model, reasoning)) = Session::get_cli_model_effort(pool, session.id).await
        && (model.is_some() || reasoning.is_some())
    {
        return (model, reasoning);
    }

    // Chat/automation sessions carry the selection in the execution process's
    // action instead (find_by_session_id is ordered created_at ASC, so the most
    // recent coding-agent run is the last match).
    if let Ok(processes) = ExecutionProcess::find_by_session_id(pool, session.id, false).await {
        for process in processes.iter().rev() {
            let Ok(action) = process.executor_action() else {
                continue;
            };
            let config = match action.typ() {
                ExecutorActionType::CodingAgentInitialRequest(request) => {
                    Some(&request.executor_config)
                }
                ExecutorActionType::CodingAgentFollowUpRequest(request) => {
                    Some(&request.executor_config)
                }
                _ => None,
            };
            if let Some(config) = config {
                return (config.model_id.clone(), config.reasoning_id.clone());
            }
        }
    }

    (None, None)
}

/// Resolve how to launch the workspace's selected agent in CLI mode. The agent
/// type comes from the session's `executor` (defaulting to claude); the picked
/// model/effort are folded in as overrides so the interactive launch honors the
/// same selection as headless mode. Agents without their own interactive CLI
/// support fall back to a default claude launch so CLI mode always works.
fn resolve_cli_launch_spec(
    session: Option<&Session>,
    model_id: Option<String>,
    reasoning_id: Option<String>,
    dir: &Path,
) -> CliLaunchSpec {
    let executor = session
        .and_then(|s| s.executor.as_deref())
        .and_then(|e| BaseCodingAgent::from_str(e).ok())
        .unwrap_or(BaseCodingAgent::ClaudeCode);

    let profiles = ExecutorConfigs::get_cached();

    let mut config = ExecutorConfig::new(executor);
    config.model_id = model_id;
    config.reasoning_id = reasoning_id;
    let mut agent = profiles.get_coding_agent_or_default(&config.profile_id());
    if config.has_overrides() {
        agent.apply_overrides(&config);
    }

    agent.interactive_cli_spec(dir).unwrap_or_else(|| {
        // The selected agent has no interactive CLI support (yet) — fall back to
        // a default claude launch so the CLI pane is never left without an agent.
        let claude = ExecutorConfig::new(BaseCodingAgent::ClaudeCode);
        profiles
            .get_coding_agent_or_default(&claude.profile_id())
            .interactive_cli_spec(dir)
            .expect("claude always provides an interactive CLI spec")
    })
}

async fn terminal_ws(
    ws: SignedWsUpgrade,
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<TerminalQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let attempt = Workspace::find_by_id(&deployment.db().pool, query.workspace_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("Attempt not found".to_string()))?;

    let container_ref = attempt
        .container_ref
        .ok_or_else(|| ApiError::BadRequest("Attempt has no workspace directory".to_string()))?;

    let base_dir = PathBuf::from(&container_ref);
    if !base_dir.exists() {
        return Err(ApiError::BadRequest(
            "Workspace directory does not exist".to_string(),
        ));
    }

    // Session whose parked CLI prompt should be cleared once the tmux session
    // is confirmed created (CLI-first first attach only). Set inside the Cli
    // arm; cleared post-spawn in handle_terminal_ws.
    let mut prompt_session_to_clear: Option<Uuid> = None;

    let (working_dir, command) = match query.mode {
        TerminalMode::Cli => {
            // Regression tripwire for the stray-newline bug: a CLI attach must
            // always carry the pane's real fitted grid. Absent cols/rows mean
            // the frontend connected without measuring, which makes claude
            // reflow and stack blank lines on the follow-up resize. After the
            // frontend fix (never connect at an unmeasured size, URL always
            // carries the fitted grid) this must never fire. An EXPLICIT
            // 80x24 is legitimate (a pane can really measure that); a
            // literal-default regression stays observable via the per-attach
            // cols x rows log in pty.rs.
            if query.cols.is_none() || query.rows.is_none() {
                tracing::warn!(
                    "CLI terminal attaching without cols/rows for workspace {} — \
                     frontend connected unmeasured (stray-newline regression)",
                    query.workspace_id
                );
            }

            let pool = &deployment.db().pool;

            // Resolve the uix session driving the handover. A mid-switch
            // frontend can briefly send the PREVIOUS workspace's session id;
            // honoring it would resume a foreign conversation (observed live
            // as "No conversation found with session ID …" and the pane
            // dropping to a bare shell), so any session that doesn't belong
            // to this workspace is discarded in favor of the workspace's
            // latest session.
            let mut session = match query.session_id {
                Some(session_id) => Session::find_by_id(pool, session_id)
                    .await
                    .ok()
                    .flatten()
                    .filter(|s| s.workspace_id == query.workspace_id),
                None => None,
            };
            if session.is_none() {
                session = Session::find_latest_by_workspace_id(pool, query.workspace_id)
                    .await
                    .ok()
                    .flatten();
            }

            // Run claude exactly where the coding agent runs: the workspace
            // root plus the session's relative agent_working_dir (mirrors
            // CodingAgentInitialRequest::effective_dir). claude keys
            // conversation storage by cwd, so --resume/--continue only find
            // the executor's transcript from that directory.
            let mut dir = base_dir.clone();
            if let Some(rel) = session
                .as_ref()
                .and_then(|s| s.agent_working_dir.as_deref())
                .filter(|d| !d.is_empty())
            {
                let candidate = base_dir.join(rel);
                if candidate.exists() {
                    dir = candidate;
                }
            }

            // Resolve claude's session id for the selected uix chat so CLI
            // mode resumes the exact conversation (handover). With no prior
            // turn the bootstrap starts a fresh TUI. While the headless
            // executor is actively RUNNING this session, never hand its id to
            // a second claude — resuming a session mid-write forks it and the
            // user ends up with chat and CLI doing the same work twice.
            let resume_session_id = match &session {
                Some(s) => {
                    let executor_active =
                        ExecutionProcess::has_running_coding_agent_for_session(pool, s.id)
                            .await
                            .unwrap_or(false);
                    if executor_active {
                        None
                    } else {
                        CodingAgentTurn::find_latest_session_info(pool, s.id)
                            .await
                            .ok()
                            .flatten()
                            .map(|info| info.session_id)
                    }
                }
                None => None,
            };

            // CLI-first creation parks the workspace's initial prompt on the
            // session; the tmux bootstrap runs claude with it directly. We
            // only PEEK it here (read, don't clear) and only on the genuine
            // FIRST attach (no tmux session yet) — reattaches and post-death
            // reconnects must not replay it. The clear is deferred until the
            // tmux session is confirmed created (see `prompt_session_to_clear`
            // below), so a failure between WS upgrade and PTY spawn can't
            // destroy the prompt, and two racing first-attaches that both peek
            // the same prompt are safe (whichever wins `new-session` carries
            // it; the loser's `-A` reattach ignores its bootstrap). An
            // existing resumable conversation always wins over a parked prompt.
            //
            // Gate on tmux availability: with tmux down, CLI mode degrades to
            // an ephemeral shell that can't run claude, so the bootstrap would
            // never deliver the prompt — peeking+clearing it would lose it.
            // Since availability is process-cached, `true` here means
            // `create_session` also takes the tmux branch, so a successful
            // spawn really did carry the prompt into a tmux session.
            let initial_prompt = match &session {
                Some(s)
                    if resume_session_id.is_none()
                        && cli_tmux_available()
                        && !cli_tmux_session_exists(query.workspace_id).await =>
                {
                    match Session::peek_pending_cli_prompt(pool, s.id).await {
                        Ok(prompt) => prompt,
                        Err(e) => {
                            // The prompt is not lost — it stays parked and the
                            // next attach re-peeks — but a transient DB error
                            // here delays delivery, so make it observable.
                            tracing::warn!(
                                "Failed to read pending CLI prompt for session {}: {}",
                                s.id,
                                e
                            );
                            None
                        }
                    }
                }
                _ => None,
            };
            // Remember which session's prompt to clear once the PTY/tmux
            // session is up (only when we actually carried a prompt).
            if initial_prompt.is_some() {
                prompt_session_to_clear = session.as_ref().map(|s| s.id);
            }

            // Honor the workspace's selected agent + model/effort at launch
            // (defaults to claude at Opus/max when nothing was selected).
            let (model_id, reasoning_id) = resolve_cli_model_effort(pool, session.as_ref()).await;
            let spec = resolve_cli_launch_spec(session.as_ref(), model_id, reasoning_id, &dir);

            (
                dir,
                PtyCommand::TmuxCli {
                    session_name: cli_tmux_session_name(query.workspace_id),
                    resume_session_id,
                    initial_prompt,
                    spec,
                },
            )
        }
        // Side terminals open in the repo for single-repo workspaces — the
        // most useful default for running project commands by hand.
        TerminalMode::Shell => {
            let mut dir = base_dir.clone();
            match WorkspaceRepo::find_repos_for_workspace(&deployment.db().pool, query.workspace_id)
                .await
            {
                Ok(repos) if repos.len() == 1 => {
                    let repo_dir = base_dir.join(&repos[0].name);
                    if repo_dir.exists() {
                        dir = repo_dir;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        "Failed to resolve repos for workspace {}: {}",
                        attempt.id,
                        e
                    );
                }
            }
            (dir, PtyCommand::Shell)
        }
    };

    Ok(ws.on_upgrade(move |socket| {
        handle_terminal_ws(
            socket,
            deployment,
            working_dir,
            query.cols.unwrap_or(DEFAULT_COLS),
            query.rows.unwrap_or(DEFAULT_ROWS),
            command,
            prompt_session_to_clear,
        )
    }))
}

async fn handle_terminal_ws(
    mut socket: MaybeSignedWebSocket,
    deployment: DeploymentImpl,
    working_dir: PathBuf,
    cols: u16,
    rows: u16,
    command: PtyCommand,
    prompt_session_to_clear: Option<Uuid>,
) {
    // FIX 4 tripwire label: the pty session name, captured before `command` is
    // moved into `create_session`. For CLI mode this is the tmux `vk_<uuid>`
    // name, so the logged bytes line up with tmux server logs.
    let tripwire_session = match &command {
        PtyCommand::TmuxCli { session_name, .. } => session_name.clone(),
        PtyCommand::Shell => "shell".to_string(),
    };

    let (session_id, mut output_rx) = match deployment
        .pty()
        .create_session(working_dir, cols, rows, command)
        .await
    {
        Ok(result) => result,
        Err(e) => {
            tracing::error!("Failed to create PTY session: {}", e);
            let _ = send_error(&mut socket, &e.to_string()).await;
            return;
        }
    };

    // FIX 4 input tripwire — bounds and rationale live on `AttachInputTripwire`.
    let mut tripwire = AttachInputTripwire::new(tripwire_session, session_id);

    // The tmux session is now created and its bootstrap (carrying the parked
    // CLI prompt) is running; only now is it safe to clear the prompt, so a
    // failure before this point leaves it parked for the next attach. If the
    // clear itself fails the prompt stays parked and a later attach (after a
    // tmux death) could replay it — narrow, but log so it's observable.
    if let Some(session_id) = prompt_session_to_clear
        && let Err(e) = Session::clear_pending_cli_prompt(&deployment.db().pool, session_id).await
    {
        tracing::warn!(
            "Failed to clear delivered CLI prompt for session {}: {}",
            session_id,
            e
        );
    }

    let pty_service = deployment.pty().clone();
    let session_id_for_input = session_id;

    loop {
        tokio::select! {
            maybe_output = output_rx.recv() => {
                let Some(data) = maybe_output else {
                    break;
                };

                let msg = TerminalMessage::Output {
                    data: BASE64.encode(&data),
                };
                let json = match serde_json::to_string(&msg) {
                    Ok(j) => j,
                    Err(_) => continue,
                };

                if socket.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
            inbound = socket.recv() => {
                match inbound {
                    Ok(Some(Message::Text(text))) => {
                        if let Ok(cmd) = serde_json::from_str::<TerminalCommand>(text.as_str()) {
                            match cmd {
                                TerminalCommand::Input { data } => {
                                    if let Ok(bytes) = BASE64.decode(&data) {
                                        tripwire.observe(&bytes);
                                        let _ = pty_service.write(session_id_for_input, &bytes).await;
                                    }
                                }
                                TerminalCommand::Resize { cols, rows } => {
                                    let _ = pty_service.resize(session_id_for_input, cols, rows).await;
                                }
                            }
                        }
                    }
                    Ok(Some(Message::Close(_))) => break,
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(error) => {
                        tracing::warn!("terminal WS receive error: {}", error);
                        break;
                    }
                }
            }
        }
    }

    let _ = deployment.pty().close_session(session_id).await;
}

/// FIX 4 input tripwire: for a strictly bounded window right after each
/// attach, hex-dump every client→pty input chunk. This is the production
/// canary for the stray-newline / EOT injection bug (portable-pty's
/// writer-Drop wrote `\n` + Ctrl-D into the tmux client's tty on teardown,
/// killing the pane) and for any FUTURE client-side injector: it makes the
/// exact bytes the browser sent — with an ms-since-attach stamp and the
/// per-attach id — visible in server logs, so a client-origin injection can
/// be told from a server-side one. Bounded to the first `Self::WINDOW` OR
/// `Self::MAX_BYTES` (whichever first); once spent, `observe` is a single
/// integer compare with no allocation, so it is permanently safe to leave
/// enabled.
struct AttachInputTripwire {
    /// The pty session name (tmux `vk_<uuid>` for CLI mode) so the logged
    /// bytes line up with tmux server logs.
    session: String,
    /// The per-attach PTY session id.
    attach_id: Uuid,
    started: std::time::Instant,
    bytes_logged: usize,
}

impl AttachInputTripwire {
    const WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
    const MAX_BYTES: usize = 128;

    fn new(session: String, attach_id: Uuid) -> Self {
        Self {
            session,
            attach_id,
            started: std::time::Instant::now(),
            bytes_logged: 0,
        }
    }

    /// Log (redacted — see `hex_dump`) input bytes while the attach window is
    /// open. Expiry latches: the monotonic clock never goes backwards, so
    /// marking the byte budget spent on the first out-of-window observation
    /// is behavior-identical and skips even the clock read afterwards.
    fn observe(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.bytes_logged >= Self::MAX_BYTES {
            return;
        }
        let elapsed = self.started.elapsed();
        if elapsed >= Self::WINDOW {
            self.bytes_logged = Self::MAX_BYTES;
            return;
        }
        let take = (Self::MAX_BYTES - self.bytes_logged).min(bytes.len());
        tracing::info!(
            session = %self.session,
            attach = %self.attach_id,
            ms_since_attach = elapsed.as_millis() as u64,
            bytes = %hex_dump(&bytes[..take]),
            "terminal input (attach-window tripwire)"
        );
        self.bytes_logged += take;
    }
}

/// Space-separated redacted hex of a byte slice for the attach-window input
/// tripwire: control bytes (< 0x20, or 0x7f) are shown verbatim (e.g. `0a 04`,
/// `1b`) because they are the injection/escape-sequence signature; every other
/// byte is masked as `..` so real keystrokes (passwords, pasted secrets) never
/// reach the logs — only their count and timing do.
/// The caller bounds the slice length, so this
/// never allocates more than a fixed maximum.
fn hex_dump(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 3);
    for byte in bytes {
        if *byte < 0x20 || *byte == 0x7f {
            let _ = write!(out, "{byte:02x} ");
        } else {
            out.push_str(".. ");
        }
    }
    out.pop(); // trailing space
    out
}

async fn send_error(socket: &mut MaybeSignedWebSocket, message: &str) -> anyhow::Result<()> {
    let msg = TerminalMessage::Error {
        message: message.to_string(),
    };
    let json = serde_json::to_string(&msg).unwrap_or_default();
    socket.send(Message::Text(json.into())).await?;
    socket.close().await?;
    Ok(())
}

pub(super) fn router() -> Router<DeploymentImpl> {
    Router::new().route("/terminal/ws", get(terminal_ws))
}

#[cfg(test)]
mod tripwire_tests {
    use uuid::Uuid;

    use super::{AttachInputTripwire, hex_dump};

    fn tripwire() -> AttachInputTripwire {
        AttachInputTripwire::new("vk_test".to_string(), Uuid::new_v4())
    }

    /// The byte budget: chunks are truncated to the remaining budget, and a
    /// spent budget stops all logging state changes.
    #[test]
    fn observe_caps_logged_bytes_at_budget() {
        let mut tw = tripwire();
        tw.observe(&[0x0a; 100]);
        assert_eq!(tw.bytes_logged, 100);
        // 100 + 100 crosses the 128 cap: only the remainder is taken.
        tw.observe(&[0x0a; 100]);
        assert_eq!(tw.bytes_logged, AttachInputTripwire::MAX_BYTES);
        // Spent: further input changes nothing.
        tw.observe(&[0x0a; 4]);
        assert_eq!(tw.bytes_logged, AttachInputTripwire::MAX_BYTES);
        // Empty chunks never count.
        let mut fresh = tripwire();
        fresh.observe(&[]);
        assert_eq!(fresh.bytes_logged, 0);
    }

    /// The time window: the first out-of-window observation latches the
    /// budget as spent (behavior-identical to checking the clock forever,
    /// but cheaper), and nothing is logged after expiry.
    #[test]
    fn observe_latches_after_window_expires() {
        let mut tw = tripwire();
        // Simulate an expired window without sleeping.
        tw.started = std::time::Instant::now() - (AttachInputTripwire::WINDOW * 2);
        tw.observe(&[0x0a]);
        assert_eq!(
            tw.bytes_logged,
            AttachInputTripwire::MAX_BYTES,
            "expiry must latch the budget as spent without logging"
        );
    }

    #[test]
    fn hex_dump_shows_control_bytes_and_masks_printables() {
        // The \n+EOT injection signature must stay fully visible…
        assert_eq!(hex_dump(&[0x0a, 0x04]), "0a 04");
        // …while printable payload (keystrokes, secrets) is masked to `..`,
        // keeping only count and position.
        assert_eq!(hex_dump(b"hi"), ".. ..");
        assert_eq!(hex_dump(&[0x1b, b'[', b'A']), "1b .. ..");
        assert_eq!(hex_dump(&[0x7f]), "7f");
        assert_eq!(hex_dump(&[]), "");
    }

    /// Locks the redaction invariant over the whole byte range: ONLY the C0
    /// controls (0x00-0x1f) and DEL (0x7f) may appear verbatim; every
    /// printable AND high/UTF-8 byte (0x20-0x7e, 0x80-0xff) must be masked so
    /// no keystroke content can ever reach the logs.
    #[test]
    fn hex_dump_masks_every_non_control_byte() {
        for byte in 0x00u8..=0xff {
            let dumped = hex_dump(&[byte]);
            if byte < 0x20 || byte == 0x7f {
                assert_eq!(dumped, format!("{byte:02x}"), "control byte {byte:#04x}");
            } else {
                assert_eq!(dumped, "..", "byte {byte:#04x} must be masked");
            }
        }
    }
}
