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
    CLI_PROMPT_PARKED_NOTICE, CliPromptRouting, PtyCommand, cli_tmux_available,
    cli_tmux_session_exists, cli_tmux_session_name, remove_cli_prompt_file, route_initial_prompt,
    send_cli_keys,
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
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    #[serde(default)]
    mode: TerminalMode,
    /// VibeKanban session whose claude conversation CLI mode should resume,
    /// so the terminal joins the exact chat the UI is showing (handover).
    #[serde(default)]
    session_id: Option<Uuid>,
}

fn default_cols() -> u16 {
    80
}

fn default_rows() -> u16 {
    24
}

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
    // A large (or otherwise non-inlineable) initial prompt that must be
    // delivered by paste AFTER the pane is up, rather than baked into the launch
    // command. Set inside the Cli arm; delivered in handle_terminal_ws.
    let mut deferred_prompt: Option<String> = None;

    let (working_dir, command) = match query.mode {
        TerminalMode::Cli => {
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
            // Honor the workspace's selected agent + model/effort at launch
            // (defaults to claude at Opus/max when nothing was selected).
            let (model_id, reasoning_id) = resolve_cli_model_effort(pool, session.as_ref()).await;
            let spec = resolve_cli_launch_spec(session.as_ref(), model_id, reasoning_id, &dir);

            // Small prompts ride the bootstrap's temp-file transport (baked into
            // the launch). Prompts too large to pass as one argv entry — or
            // agents with no launch-time prompt arg — are delivered by paste
            // after the pane is confirmed up (see handle_terminal_ws), so they're
            // never truncated and never silently lost. Either way the parked
            // prompt is cleared only after delivery is confirmed.
            let baked_prompt = match route_initial_prompt(initial_prompt, &spec.prompt_arg) {
                CliPromptRouting::None => None,
                CliPromptRouting::Baked(prompt) => Some(prompt),
                CliPromptRouting::Deferred(prompt) => {
                    deferred_prompt = Some(prompt);
                    None
                }
            };
            // Remember which session's prompt to clear once delivery is
            // confirmed (only when we actually carried a prompt).
            if baked_prompt.is_some() || deferred_prompt.is_some() {
                prompt_session_to_clear = session.as_ref().map(|s| s.id);
            }

            (
                dir,
                PtyCommand::TmuxCli {
                    session_name: cli_tmux_session_name(query.workspace_id),
                    resume_session_id,
                    initial_prompt: baked_prompt,
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
            query.cols,
            query.rows,
            command,
            query.workspace_id,
            prompt_session_to_clear,
            deferred_prompt,
        )
    }))
}

#[allow(clippy::too_many_arguments)]
async fn handle_terminal_ws(
    mut socket: MaybeSignedWebSocket,
    deployment: DeploymentImpl,
    working_dir: PathBuf,
    cols: u16,
    rows: u16,
    command: PtyCommand,
    workspace_id: Uuid,
    prompt_session_to_clear: Option<Uuid>,
    deferred_prompt: Option<String>,
) {
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

    // create_session returning Ok only means the tmux *client* process spawned;
    // the tmux server can still reject the command moments later (historically:
    // an over-long prompt baked into `new-session`). So confirm the session
    // actually exists before clearing the parked prompt — and, for a deferred
    // large prompt, before pasting it in. A session that never comes up leaves
    // the prompt saved for the next attach and tells the user so, instead of
    // silently destroying it.
    if let Some(clear_session_id) = prompt_session_to_clear {
        if wait_for_cli_session(workspace_id).await {
            let delivered = match &deferred_prompt {
                Some(text) => {
                    // Give the freshly-launched TUI a moment to be ready to
                    // accept a paste, then deliver the oversized prompt via the
                    // buffer path (no argv-length ceiling).
                    tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                    send_cli_keys(workspace_id, text).await
                }
                None => true,
            };
            if delivered {
                if let Err(e) =
                    Session::clear_pending_cli_prompt(&deployment.db().pool, clear_session_id).await
                {
                    tracing::warn!(
                        "Failed to clear delivered CLI prompt for session {}: {}",
                        clear_session_id,
                        e
                    );
                }
            } else {
                // Leave the prompt parked; the next attach retries delivery.
                tracing::warn!(
                    "Failed to deliver large CLI prompt to workspace {}; left parked",
                    workspace_id
                );
            }
        } else {
            // The session never appeared — remove the transient prompt file (the
            // DB copy stays parked) and surface the failure. The frontend renders
            // this in red and halts its reconnect loop.
            remove_cli_prompt_file(workspace_id);
            tracing::error!(
                "CLI tmux session for workspace {} never came up; prompt left parked",
                workspace_id
            );
            let _ = send_error(&mut socket, CLI_PROMPT_PARKED_NOTICE).await;
            // Tear down the PtyService entry for the (already-dead) tmux client
            // before bailing; otherwise this early return leaks the session map
            // entry — one per failed attach — that the normal exit path below
            // would have reaped via close_session.
            let _ = deployment.pty().close_session(session_id).await;
            return;
        }
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

/// Poll for the workspace's CLI tmux session to appear after a spawn. The
/// session existing proves tmux accepted `new-session` and the bootstrap (which
/// owns the prompt file) is running; only then is it safe to clear the parked
/// prompt. Short backoff (~5 × 200ms) — the session shows up immediately when
/// tmux accepts the command, so this only spins when it's failing.
async fn wait_for_cli_session(workspace_id: Uuid) -> bool {
    for attempt in 0..5 {
        if cli_tmux_session_exists(workspace_id).await {
            return true;
        }
        if attempt < 4 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
    false
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
