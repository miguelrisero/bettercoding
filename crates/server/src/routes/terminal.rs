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
    remove_cli_prompt_file, resolved_cli_tmux_session_name, route_followup_prompt,
    route_initial_prompt, send_cli_keys,
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
    /// BetterCoding session whose claude conversation CLI mode should resume,
    /// so the terminal joins the exact chat the UI is showing (handover).
    #[serde(default)]
    session_id: Option<Uuid>,
    /// Connect-time browser visibility. A hidden tmux client must be excluded
    /// from shared sizing before its first WebSocket message can arrive.
    #[serde(default)]
    hidden: bool,
}

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TerminalCommand {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
    Presence { visible: bool },
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
            // Workspace-scoped peek: creation parks on the CLI-first session,
            // loop wake-ups re-park on the LATEST session, and this attach may
            // have resolved a third (frontend-selected) session — a
            // session-scoped peek would strand a prompt parked on a sibling
            // row.
            let carried: Option<(String, CliPromptDelivery, Uuid)> = if cli_tmux_available() {
                match CliPromptDelivery::try_claim(query.workspace_id) {
                    Some(claim) => {
                        match Session::peek_pending_cli_prompt_for_workspace(
                            pool,
                            query.workspace_id,
                        )
                        .await
                        {
                            Ok(Some((owning_session_id, prompt))) => {
                                Some((prompt, claim, owning_session_id))
                            }
                            // Nothing parked: release the claim (drop).
                            Ok(None) => None,
                            Err(e) => {
                                // The prompt is not lost — it stays parked and
                                // the next attach re-peeks — but a transient DB
                                // error here delays delivery, so make it
                                // observable.
                                tracing::warn!(
                                    "Failed to read pending CLI prompt for workspace {}: {}",
                                    query.workspace_id,
                                    e
                                );
                                None
                            }
                        }
                    }
                    None => {
                        tracing::debug!(
                            "CLI prompt delivery for workspace {} already in flight; \
                             attaching without the prompt",
                            query.workspace_id
                        );
                        None
                    }
                }
            } else {
                None
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
            let mut deferred_prompt_pending = false;
            if let Some((peeked, claim, clear_session_id)) = carried {
                // Match the bootstrap's resume predicate EXACTLY (it filters the
                // id through is_uuid before resuming): a non-UUID resume id is
                // NOT resumed by the launch, so it must count as a fresh launch
                // here too. Otherwise the prompt would route as a follow-up
                // paste into a continue_launch's doomed `--continue` leg (the
                // same loss hazard the racing-attach path avoids).
                let resume_will_apply = resume_session_id
                    .as_deref()
                    .is_some_and(|id| Uuid::parse_str(id).is_ok());
                let fresh_launch =
                    !resume_will_apply && !cli_tmux_session_exists(query.workspace_id).await;
                let routed = if fresh_launch {
                    route_initial_prompt(Some(peeked.clone()), &spec.prompt_arg)
                } else {
                    route_followup_prompt(&peeked)
                };
                match routed {
                    CliPromptRouting::None => {
                        // Blank-after-trim: nothing can ever be delivered, so
                        // clear the parked blank (CAS keeps a newer prompt
                        // safe) instead of re-claiming and re-probing it on
                        // every future attach. The claim drops here.
                        match Session::clear_pending_cli_prompt(pool, clear_session_id, &peeked)
                            .await
                        {
                            Ok(_) => {}
                            Err(e) => tracing::warn!(
                                "Failed to clear blank parked CLI prompt for session {}: {}",
                                clear_session_id,
                                e
                            ),
                        }
                    }
                    CliPromptRouting::Baked(prompt) => {
                        baked_prompt = Some(prompt);
                        prompt_delivery = Some(PromptDelivery {
                            workspace_id: query.workspace_id,
                            clear_session_id,
                            peeked,
                            deferred: None,
                            program: spec.program.clone(),
                            claim,
                        });
                    }
                    CliPromptRouting::Deferred(prompt) => {
                        // Request the bare-TUI bootstrap whenever a resume won't
                        // apply — not just when the session is currently absent.
                        // A live session can exit between this check and
                        // `new-session -A`; if it does, the freshly created pane
                        // must NOT run `continue_launch`'s doomed `--continue`
                        // leg (the deferred paste could land on it and be lost).
                        // When the session survives, the bootstrap is ignored by
                        // `-A`, so requesting it is harmless.
                        deferred_prompt_pending = !resume_will_apply;
                        prompt_delivery = Some(PromptDelivery {
                            workspace_id: query.workspace_id,
                            clear_session_id,
                            peeked,
                            deferred: Some(prompt),
                            program: spec.program.clone(),
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
                    deferred_prompt_pending,
                    connect_hidden: query.hidden,
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
    /// The agent binary this launch runs — delivery is only ever confirmed
    /// against THIS process owning the pane, so a fallback shell (or anything
    /// the user runs inside it) can never satisfy the gate.
    program: String,
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
    // FIX 4 tripwire label: the pty session name, captured before `command` is
    // moved into `create_session`. For CLI mode this is the actual current
    // `bc_<uuid>` or legacy `vk_<uuid>` name, so the bytes line up with tmux logs.
    let tripwire_session = match &command {
        // Resolve the workspace-derived name through both homes so legacy
        // attaches carry their real `vk_<uuid>` label rather than a `bc_` guess.
        PtyCommand::TmuxCli { workspace_id, .. } => {
            resolved_cli_tmux_session_name(*workspace_id).await
        }
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

    // FIX 4 input tripwire — bounds and rationale live on `AttachInputTripwire`.
    let mut tripwire = AttachInputTripwire::new(tripwire_session, session_id);

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
                    Some(text) => {
                        deliver_deferred_prompt(workspace_id, text, &delivery.program).await
                    }
                    None => confirm_baked_prompt_consumed(workspace_id, &delivery.program).await,
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
            // The session never appeared — remove the transient prompt file
            // (the DB copy stays parked) and surface the failure. Deliberately
            // NOT killing a session that squeaks in after the poll window: a
            // slow-but-successful launch may already be running the agent (or
            // hosting another attach's client), and murdering it loses real
            // work — while a parked prompt behind a live session is now
            // recoverable anyway (the next attach delivers it by paste). The
            // frontend renders the error in red and halts its reconnect loop.
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
                                        tripwire.observe(&bytes);
                                        let _ = pty_service.write(session_id_for_input, &bytes).await;
                                    }
                                }
                                TerminalCommand::Resize { cols, rows } => {
                                    let _ = pty_service.resize(session_id_for_input, cols, rows).await;
                                }
                                TerminalCommand::Presence { visible } => {
                                    pty_service.set_cli_presence(session_id_for_input, visible);
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
/// AND the expected agent process owns the pane. The second condition catches
/// the consumed-but-never-executed window (`command -v` passed but the agent's
/// exec failed: bad shebang, loader error, permissions) where file-gone alone
/// would clear a prompt no agent ever received. An unconfirmed launch leaves
/// the parked DB prompt for the next attach's paste/fresh-launch retry — and
/// drops the never-consumed file so a later launch can't half-consume it.
///
/// Note: a prompt stranded by a losing racing first-attach that won
/// `new-session -A` with a promptless bootstrap is deliberately NOT recovered
/// by pasting here — the loser's `--continue || fresh` leg and the eventual
/// fresh agent share a process name, so a paste could land on the dying
/// `--continue` leg yet be "confirmed" by the fresh one, silently losing the
/// prompt. Leaving it parked means the next attach delivers it as a follow-up
/// paste into the live agent: slower in that rare race, but never lost.
async fn confirm_baked_prompt_consumed(workspace_id: Uuid, program: &str) -> bool {
    for _ in 0..30 {
        if !cli_prompt_file_exists(workspace_id)
            && cli_pane_agent_running(workspace_id, program).await == Some(true)
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    remove_cli_prompt_file(workspace_id);
    false
}

/// Deliver a deferred prompt by paste once THE AGENT owns the pane. Pasting
/// earlier would hand the prompt to the bootstrap/fallback shell instead —
/// which executes it as shell input (binary missing) or truncates it at the
/// tty's canonical-mode line limit (TUI not yet in raw mode). Readiness = the
/// expected agent process in the pane on two consecutive polls, re-checked
/// right before the paste (the agent can die inside the grace window — e.g. a
/// failed resume attempt — and the pane fall back to a shell) and re-checked
/// after it (an agent that exited immediately after the paste discarded the
/// text with its tty; delivery must not be confirmed). Bounded at ~15s; an
/// unready pane leaves the prompt parked for the next attach's retry.
async fn deliver_deferred_prompt(workspace_id: Uuid, text: &str, program: &str) -> bool {
    let mut stable = 0u32;
    for _ in 0..60 {
        match cli_pane_agent_running(workspace_id, program).await {
            Some(true) => {
                stable += 1;
                if stable >= 2 {
                    // Grace for the TUI to enter raw mode, then re-verify the
                    // agent still owns the pane immediately before pasting.
                    tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                    if cli_pane_agent_running(workspace_id, program).await != Some(true) {
                        stable = 0;
                        continue;
                    }
                    if !send_cli_keys(workspace_id, text).await {
                        return false;
                    }
                    // Post-paste ack: the agent must have survived receiving
                    // it. A process that died right after the paste (doomed
                    // resume leg, instant crash) never processed the text.
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    return cli_pane_agent_running(workspace_id, program).await == Some(true);
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
    /// The pty session name (current `bc_<uuid>` or legacy `vk_<uuid>` in CLI
    /// mode) so the logged bytes line up with tmux server logs.
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
mod tests {
    use db::models::session::Session;
    use sqlx::SqlitePool;
    use uuid::Uuid;

    /// Minimal in-memory slice of the `sessions` table — just the columns the
    /// parked-prompt primitives touch — so the CAS/park semantics that guard
    /// the "never destroy the prompt" invariant are exercised against real
    /// SQLite. (Lives here rather than in `crates/db` because the db crate
    /// has no async test runtime; this route module is the consumer whose
    /// correctness depends on these semantics.)
    async fn pool_with_session(id: Uuid, workspace_id: Uuid) -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE sessions (
                 id BLOB PRIMARY KEY,
                 workspace_id BLOB NOT NULL,
                 pending_cli_prompt TEXT,
                 created_at TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO sessions (id, workspace_id) VALUES ($1, $2)")
            .bind(id)
            .bind(workspace_id)
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    #[tokio::test]
    async fn clear_pending_cli_prompt_is_compare_and_swap() {
        let sid = Uuid::new_v4();
        let pool = pool_with_session(sid, Uuid::new_v4()).await;

        // Park, then clear with the exact delivered value: clears.
        Session::set_pending_cli_prompt(&pool, sid, "deliver me")
            .await
            .unwrap();
        assert!(
            Session::clear_pending_cli_prompt(&pool, sid, "deliver me")
                .await
                .unwrap(),
            "matching clear must succeed"
        );
        assert_eq!(
            Session::peek_pending_cli_prompt(&pool, sid).await.unwrap(),
            None
        );

        // A NEWER prompt parked mid-confirmation survives the older
        // delivery's clear (the CAS misses) — the invariant this method
        // exists for.
        Session::set_pending_cli_prompt(&pool, sid, "newer prompt")
            .await
            .unwrap();
        assert!(
            !Session::clear_pending_cli_prompt(&pool, sid, "deliver me")
                .await
                .unwrap(),
            "stale clear must be superseded"
        );
        assert_eq!(
            Session::peek_pending_cli_prompt(&pool, sid).await.unwrap(),
            Some("newer prompt".to_string())
        );

        // Clearing an empty slot is a no-op, not an error.
        Session::clear_pending_cli_prompt(&pool, sid, "newer prompt")
            .await
            .unwrap();
        assert!(
            !Session::clear_pending_cli_prompt(&pool, sid, "newer prompt")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn repark_only_fills_an_empty_slot() {
        let sid = Uuid::new_v4();
        let pool = pool_with_session(sid, Uuid::new_v4()).await;

        // Empty slot: the wake-up parks.
        assert!(
            Session::set_pending_cli_prompt_if_empty(&pool, sid, "continue")
                .await
                .unwrap()
        );
        // Occupied slot: a wake-up must never overwrite a parked prompt.
        assert!(
            !Session::set_pending_cli_prompt_if_empty(&pool, sid, "boilerplate")
                .await
                .unwrap()
        );
        assert_eq!(
            Session::peek_pending_cli_prompt(&pool, sid).await.unwrap(),
            Some("continue".to_string())
        );
    }

    #[tokio::test]
    async fn workspace_peek_finds_prompt_on_any_session_row() {
        let workspace_id = Uuid::new_v4();
        let older = Uuid::new_v4();
        let pool = pool_with_session(older, workspace_id).await;
        // A second, newer session in the same workspace (distinct created_at
        // ordering via explicit timestamps).
        let newer = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO sessions (id, workspace_id, created_at)
             VALUES ($1, $2, datetime('now', '+1 hour'))",
        )
        .bind(newer)
        .bind(workspace_id)
        .execute(&pool)
        .await
        .unwrap();

        // Prompt parked on the OLDER (e.g. CLI-first) session is still found
        // when the attach resolved the newer session.
        Session::set_pending_cli_prompt(&pool, older, "parked on older")
            .await
            .unwrap();
        assert_eq!(
            Session::peek_pending_cli_prompt_for_workspace(&pool, workspace_id)
                .await
                .unwrap(),
            Some((older, "parked on older".to_string()))
        );

        // With prompts on both rows, the newest session's wins (loop re-parks
        // land on the latest session).
        Session::set_pending_cli_prompt(&pool, newer, "parked on newer")
            .await
            .unwrap();
        assert_eq!(
            Session::peek_pending_cli_prompt_for_workspace(&pool, workspace_id)
                .await
                .unwrap(),
            Some((newer, "parked on newer".to_string()))
        );

        // Foreign workspaces see nothing.
        assert_eq!(
            Session::peek_pending_cli_prompt_for_workspace(&pool, Uuid::new_v4())
                .await
                .unwrap(),
            None
        );
    }
}

#[cfg(test)]
mod tripwire_tests {
    use uuid::Uuid;

    use super::{AttachInputTripwire, hex_dump};

    fn tripwire() -> AttachInputTripwire {
        AttachInputTripwire::new("bc_test".to_string(), Uuid::new_v4())
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
