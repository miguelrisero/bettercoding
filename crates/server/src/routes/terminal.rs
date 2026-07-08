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
    CLI_PROMPT_PARKED_NOTICE, CliPromptDelivery, CliPromptRouting, PtyCommand,
    cli_pane_agent_running, cli_prompt_file_exists, cli_tmux_available, cli_tmux_session_exists,
    kill_cli_tmux_session, remove_cli_prompt_file, route_initial_prompt, send_cli_keys,
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

    // Set inside the Cli arm when this attach carries the workspace's parked
    // prompt; consumed by handle_terminal_ws to confirm delivery and clear it.
    let mut prompt_delivery: Option<PromptDelivery> = None;

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

            // A parked prompt (CLI-first creation, or a re-parked loop wake-up)
            // is only ever PEEKED here (read, don't clear); the clear happens
            // after delivery is CONFIRMED (see handle_terminal_ws), so no
            // failure between WS upgrade and agent hand-off can destroy the
            // user's only copy. Racing attaches are serialized by the
            // CliPromptDelivery claim — the loser attaches without the prompt.
            //
            // Gate on tmux availability: with tmux down, CLI mode degrades to
            // an ephemeral shell that can't run claude, so the bootstrap would
            // never deliver the prompt — peeking+clearing it would lose it.
            // Since availability is process-cached, `true` here means
            // `create_session` also takes the tmux branch, so a successful
            // spawn really did carry the prompt into a tmux session.
            let carried: Option<(String, CliPromptDelivery, Uuid)> = match &session {
                Some(s) if cli_tmux_available() => {
                    match CliPromptDelivery::try_claim(query.workspace_id) {
                        Some(claim) => match Session::peek_pending_cli_prompt(pool, s.id).await {
                            Ok(Some(prompt)) => Some((prompt, claim, s.id)),
                            // Nothing parked: release the claim (drop).
                            Ok(None) => None,
                            Err(e) => {
                                // The prompt is not lost — it stays parked and
                                // the next attach re-peeks — but a transient DB
                                // error here delays delivery, so make it
                                // observable.
                                tracing::warn!(
                                    "Failed to read pending CLI prompt for session {}: {}",
                                    s.id,
                                    e
                                );
                                None
                            }
                        },
                        None => {
                            tracing::debug!(
                                "CLI prompt delivery for workspace {} already in flight; \
                                 attaching without the prompt",
                                query.workspace_id
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

            // How the parked prompt travels:
            // - Genuine first attach (no tmux session yet, nothing to resume):
            //   small prompts ride the bootstrap's temp-file transport (baked
            //   into the launch); prompts too large for one argv entry are
            //   pasted after the agent owns the pane.
            // - Otherwise (the session already exists — e.g. an earlier
            //   delivery went unconfirmed, or a loop wake-up was re-parked —
            //   or an existing conversation is being resumed, which always
            //   wins the launch itself): deliver by paste into the running
            //   agent, as a follow-up. Without this branch a parked prompt
            //   behind a live session or a resume would be stranded forever.
            // Either way the parked prompt is cleared only after delivery is
            // confirmed.
            let mut baked_prompt = None;
            if let Some((peeked, claim, clear_session_id)) = carried {
                let fresh_launch = resume_session_id.is_none()
                    && !cli_tmux_session_exists(query.workspace_id).await;
                let routed = if fresh_launch {
                    route_initial_prompt(Some(peeked.clone()), &spec.prompt_arg)
                } else {
                    let trimmed = peeked.trim();
                    if trimmed.is_empty() {
                        CliPromptRouting::None
                    } else {
                        CliPromptRouting::Deferred(trimmed.to_string())
                    }
                };
                match routed {
                    // Blank-after-trim: nothing will be delivered; the claim
                    // drops here rather than being held across the attach.
                    CliPromptRouting::None => {}
                    CliPromptRouting::Baked(prompt) => {
                        baked_prompt = Some(prompt);
                        prompt_delivery = Some(PromptDelivery {
                            workspace_id: query.workspace_id,
                            clear_session_id,
                            peeked,
                            deferred: None,
                            claim,
                        });
                    }
                    CliPromptRouting::Deferred(prompt) => {
                        prompt_delivery = Some(PromptDelivery {
                            workspace_id: query.workspace_id,
                            clear_session_id,
                            peeked,
                            deferred: Some(prompt),
                            claim,
                        });
                    }
                }
            }

            (
                dir,
                PtyCommand::TmuxCli {
                    workspace_id: query.workspace_id,
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
            prompt_delivery,
        )
    }))
}

/// Everything a prompt-carrying CLI attach needs to confirm delivery and clear
/// the parked prompt. Built only when this attach actually carries the prompt
/// (it holds the claim), so "claim without prompt" or "deferred text without a
/// session to clear" are unrepresentable.
struct PromptDelivery {
    workspace_id: Uuid,
    /// Session whose parked prompt to clear once delivery is confirmed.
    clear_session_id: Uuid,
    /// The exact parked value that was peeked — the clear is a compare-and-swap
    /// against it, so a NEWER prompt parked mid-confirmation (e.g. a loop
    /// wake-up re-parked while this delivery was in flight) is never destroyed
    /// by this delivery's clear.
    peeked: String,
    /// Prompt to paste once the agent owns the pane (`None` = baked into the
    /// launch's temp-file transport instead).
    deferred: Option<String>,
    /// Exclusive in-process claim; held until the parked copy is cleared or
    /// deliberately left parked, releasing on drop.
    claim: CliPromptDelivery,
}

async fn handle_terminal_ws(
    mut socket: MaybeSignedWebSocket,
    deployment: DeploymentImpl,
    working_dir: PathBuf,
    cols: u16,
    rows: u16,
    command: PtyCommand,
    prompt_delivery: Option<PromptDelivery>,
) {
    let (session_id, mut output_rx) = match deployment
        .pty()
        .create_session(working_dir, cols, rows, command)
        .await
    {
        Ok(result) => result,
        Err(e) => {
            tracing::error!("Failed to create PTY session: {}", e);
            // A prompt file staged before the spawn failure must not outlive
            // it (the DB copy stays parked for the next attach; the claim is
            // released on return).
            if let Some(delivery) = &prompt_delivery {
                remove_cli_prompt_file(delivery.workspace_id);
            }
            let _ = send_error(&mut socket, &e.to_string()).await;
            return;
        }
    };

    // create_session returning Ok only means the tmux *client* process spawned;
    // the tmux server can still reject the command moments later (historically:
    // an over-long prompt baked into `new-session`). So confirm the session
    // actually exists before doing anything with the parked prompt. A session
    // that never comes up leaves the prompt saved for the next attach and tells
    // the user so, instead of silently destroying it.
    if let Some(delivery) = prompt_delivery {
        let workspace_id = delivery.workspace_id;
        if wait_for_cli_session(workspace_id).await {
            // Confirm delivery and clear the parked prompt in the background so
            // the pane's output starts streaming immediately — the confirmation
            // polls below would otherwise hold the whole terminal blank.
            let pool = deployment.db().pool.clone();
            tokio::spawn(async move {
                // Hold the exclusive delivery claim for the whole
                // confirm-then-clear window.
                let _claim = delivery.claim;
                let delivered = match &delivery.deferred {
                    Some(text) => deliver_deferred_prompt(workspace_id, text).await,
                    None => confirm_baked_prompt_consumed(workspace_id).await,
                };
                if delivered {
                    match Session::clear_pending_cli_prompt(
                        &pool,
                        delivery.clear_session_id,
                        &delivery.peeked,
                    )
                    .await
                    {
                        // Superseded: a newer prompt was parked while this one
                        // was being confirmed; it stays parked for its own
                        // delivery on the next attach.
                        Ok(false) => tracing::info!(
                            "Parked CLI prompt for session {} changed during delivery; \
                             left for the next attach",
                            delivery.clear_session_id
                        ),
                        Ok(true) => {}
                        Err(e) => tracing::warn!(
                            "Failed to clear delivered CLI prompt for session {}: {}",
                            delivery.clear_session_id,
                            e
                        ),
                    }
                } else {
                    // Leave the prompt parked; the next attach retries (paste
                    // into the live pane, or a fresh launch after the session
                    // dies) — delivery is only ever confirmed, never assumed.
                    tracing::warn!(
                        "CLI prompt delivery for workspace {} unconfirmed; left parked",
                        workspace_id
                    );
                }
            });
        } else {
            // The session never appeared — tear down whatever half-state the
            // launch left behind and surface the failure. `kill_cli_tmux_session`
            // both removes the transient prompt file (the DB copy stays parked)
            // and kills a session that squeaked in after the poll window, so the
            // next attach deterministically re-peeks the parked prompt instead
            // of finding a stray session that blocks delivery. The frontend
            // renders the error in red and halts its reconnect loop.
            kill_cli_tmux_session(workspace_id).await;
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
/// owns the prompt file) is running. Bounded backoff (~10 × 250ms): the session
/// shows up near-instantly when tmux accepts the command, so the full window is
/// only spent when the launch is failing — and a window that's too short would
/// false-negative on a heavily loaded machine, stranding a prompt that WAS
/// delivered (the replay hazard the deferred clear exists to prevent).
async fn wait_for_cli_session(workspace_id: Uuid) -> bool {
    for attempt in 0..10 {
        if cli_tmux_session_exists(workspace_id).await {
            return true;
        }
        if attempt < 9 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
    false
}

/// Confirm a baked prompt was actually handed to the agent: the staged file is
/// gone (the bootstrap `rm`s it at hand-off — see [`cli_prompt_file_exists`])
/// AND a non-shell process owns the pane. The second condition catches the
/// consumed-but-never-executed window (`command -v` passed but the agent's
/// exec failed: bad shebang, loader error, permissions) where file-gone alone
/// would clear a prompt no agent ever received. An unconfirmed launch leaves
/// the parked DB prompt for the next attach's paste/fresh-launch retry.
async fn confirm_baked_prompt_consumed(workspace_id: Uuid) -> bool {
    for _ in 0..20 {
        if !cli_prompt_file_exists(workspace_id)
            && cli_pane_agent_running(workspace_id).await == Some(true)
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    false
}

/// Deliver an oversized (non-inlineable) prompt by paste once the AGENT owns
/// the pane. Pasting earlier would hand the prompt to the bootstrap/fallback
/// shell instead — which executes it as shell input (binary missing) or
/// truncates it at the tty's canonical-mode line limit (TUI not yet in raw
/// mode). Readiness = a non-shell process in the pane on two consecutive
/// polls (a single read could catch the short-lived first leg of the
/// `--continue || fresh` relaunch), plus a short grace for the TUI to enter
/// raw mode. Bounded at ~10s; an unready pane leaves the prompt parked.
async fn deliver_deferred_prompt(workspace_id: Uuid, text: &str) -> bool {
    let mut stable = 0u32;
    for _ in 0..40 {
        match cli_pane_agent_running(workspace_id).await {
            Some(true) => {
                stable += 1;
                if stable >= 2 {
                    tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                    return send_cli_keys(workspace_id, text).await;
                }
            }
            Some(false) => stable = 0,
            None => {
                // Pane unreadable: if the session is really gone, give up and
                // leave the prompt parked; a transient probe failure (ps/pgrep
                // hiccup) just resets the stability counter.
                if !cli_tmux_session_exists(workspace_id).await {
                    return false;
                }
                stable = 0;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
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
