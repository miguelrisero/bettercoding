//! Detect and recover a CLI tmux session whose agent has died.
//!
//! `tmux new-session -A` — how every CLI attach reaches its session — runs its
//! bootstrap ONLY when it creates the session. Attaching to an existing one
//! silently discards it. So once a session's agent exits (crash, `kill -9`, OOM)
//! the pane keeps the fallback shell the bootstrap dropped it into, and no
//! amount of reloading the workspace will bring the agent back: every reload
//! just reattaches to the same dead pane.
//!
//! The recovery is `respawn-pane -k`, which replaces the pane's process with a
//! fresh bootstrap on a fresh pty. It is exposed as an explicit user action
//! rather than something that fires on load, because "no agent in this pane" is
//! ALSO what a user quitting their agent to use the shell looks like, and those
//! two states are indistinguishable from the outside. Restarting automatically
//! would yank a shell out from under someone mid-command.

use axum::{
    Router,
    extract::{Path, State},
    response::Json as ResponseJson,
    routing::{get, post},
};
use db::models::{
    claude_session_link::ClaudeSessionLink, coding_agent_turn::CodingAgentTurn,
    execution_process::ExecutionProcess, session::Session, workspace::Workspace,
};
use deployment::Deployment;
use local_deployment::{
    cli_activity::{AgentPresence, probe_workspace_agent},
    pty::{
        CliRespawnClaim, cli_restart_bootstrap, cli_tmux_available, cli_tmux_session_is_legacy,
        respawn_cli_tmux_pane,
    },
};
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utils::response::ApiResponse;
use uuid::Uuid;

use crate::{DeploymentImpl, error::ApiError, routes::terminal};

/// Why a workspace's CLI pane cannot be restarted right now.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum CliRestartBlocker {
    /// No CLI tmux session exists. A normal attach creates one and runs its
    /// bootstrap, so there is nothing to repair.
    NoSession,
    /// A headless executor still owns this conversation. Respawning would put a
    /// second agent on the same transcript and fork it.
    ExecutorRunning,
    /// A legacy `vk_` session. These are attach-only by design and carry no
    /// bootstrap we could re-run; the user must reopen the workspace instead.
    LegacySession,
    /// tmux or /proc could not be read, so liveness is unknown. Never offer a
    /// restart on evidence this weak.
    Unknown,
}

#[derive(Debug, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CliAgentStatus {
    /// `Some(false)` is the only state that warrants offering a restart.
    /// `None` means liveness could not be established and the UI must stay
    /// quiet rather than guess.
    pub agent_alive: Option<bool>,
    /// Present so the UI can distinguish "no session yet" from "session with a
    /// dead agent" without inferring it from `agent_alive`.
    pub session_present: bool,
    /// Whether a restart would be accepted right now.
    pub restartable: bool,
    /// Why not, when `restartable` is false and the agent is not simply alive.
    pub blocker: Option<CliRestartBlocker>,
    /// The agent binary the liveness probe looked for, for diagnostics.
    pub program: String,
}

#[derive(Debug, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CliRestartResponse {
    /// Always true on success; a failure is reported as an API error instead.
    pub restarted: bool,
    /// Whether the restart resumed a known conversation. When false the agent
    /// falls back to its own continue/fresh behaviour.
    pub resumed: bool,
}

/// Everything the two handlers need about a workspace's CLI launch.
struct CliContext {
    program: String,
    working_dir: std::path::PathBuf,
    resume_session_id: Option<String>,
    executor_active: bool,
    spec: executors::executors::cli::CliLaunchSpec,
}

/// Resolve a workspace's CLI launch parameters the same way the terminal attach
/// does, so a restart lands on the same agent, model, effort and directory as
/// the launch it is replacing.
async fn cli_context(
    deployment: &DeploymentImpl,
    workspace_id: Uuid,
) -> Result<CliContext, ApiError> {
    let pool = &deployment.db().pool;
    let workspace = Workspace::find_by_id(pool, workspace_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("Workspace not found".to_string()))?;
    let container_ref = workspace
        .container_ref
        .ok_or_else(|| ApiError::BadRequest("Workspace has no directory".to_string()))?;
    let base_dir = std::path::PathBuf::from(&container_ref);
    if !base_dir.exists() {
        return Err(ApiError::BadRequest(
            "Workspace directory does not exist".to_string(),
        ));
    }

    let session = Session::find_latest_by_workspace_id(pool, workspace_id).await?;

    // Mirrors the attach path: the agent must run where the coding agent runs,
    // because agents key conversation storage by cwd and a resume started from
    // the wrong directory finds no transcript.
    let mut working_dir = base_dir.clone();
    if let Some(rel) = session
        .as_ref()
        .and_then(|s| s.agent_working_dir.as_deref())
        .filter(|d| !d.is_empty())
    {
        let candidate = base_dir.join(rel);
        if candidate.exists() {
            working_dir = candidate;
        }
    }

    // Fail CLOSED on the executor probe: an unreadable answer must read as
    // "an executor may be running", never as permission to respawn.
    let (executor_active, known_session_id) = match &session {
        Some(s) => {
            let active = ExecutionProcess::has_running_coding_agent_for_session(pool, s.id)
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!(?error, session_id = %s.id, "restart writer guard failed closed");
                    true
                });
            let sid = CodingAgentTurn::find_latest_session_info(pool, s.id)
                .await?
                .map(|info| info.session_id)
                .or(ClaudeSessionLink::find_latest_for_session(pool, s.id)
                    .await?
                    .map(|link| link.claude_session_id));
            (active, sid)
        }
        None => (false, None),
    };

    let (model_id, reasoning_id) = terminal::resolve_cli_model_effort(pool, session.as_ref()).await;
    let spec =
        terminal::resolve_cli_launch_spec(session.as_ref(), model_id, reasoning_id, &working_dir);

    Ok(CliContext {
        program: spec.program.clone(),
        working_dir,
        resume_session_id: (!executor_active).then_some(known_session_id).flatten(),
        executor_active,
        spec,
    })
}

async fn get_agent_status(
    State(deployment): State<DeploymentImpl>,
    Path(workspace_id): Path<Uuid>,
) -> Result<ResponseJson<ApiResponse<CliAgentStatus>>, ApiError> {
    let context = cli_context(&deployment, workspace_id).await?;

    if !cli_tmux_available() {
        return Ok(ResponseJson(ApiResponse::success(CliAgentStatus {
            agent_alive: None,
            session_present: false,
            restartable: false,
            blocker: Some(CliRestartBlocker::Unknown),
            program: context.program,
        })));
    }

    let presence = probe_workspace_agent(workspace_id, &context.program).await;
    let (agent_alive, session_present, blocker) = match presence {
        AgentPresence::Alive => (Some(true), true, None),
        AgentPresence::Absent => (Some(false), true, None),
        AgentPresence::NoSession => (None, false, Some(CliRestartBlocker::NoSession)),
        AgentPresence::Unknown => (None, false, Some(CliRestartBlocker::Unknown)),
    };

    // Precedence matters: a dead agent under a running executor must report
    // ExecutorRunning, so the UI explains the wait instead of offering a button
    // that would only ever be refused.
    let blocker = if agent_alive == Some(false) {
        if context.executor_active {
            Some(CliRestartBlocker::ExecutorRunning)
        } else if cli_tmux_session_is_legacy(workspace_id).await {
            Some(CliRestartBlocker::LegacySession)
        } else {
            None
        }
    } else {
        blocker
    };

    Ok(ResponseJson(ApiResponse::success(CliAgentStatus {
        agent_alive,
        session_present,
        restartable: agent_alive == Some(false) && blocker.is_none(),
        blocker,
        program: context.program,
    })))
}

async fn restart_agent(
    State(deployment): State<DeploymentImpl>,
    Path(workspace_id): Path<Uuid>,
) -> Result<ResponseJson<ApiResponse<CliRestartResponse>>, ApiError> {
    let context = cli_context(&deployment, workspace_id).await?;

    if context.executor_active {
        return Err(ApiError::Conflict(
            "A headless run still owns this conversation; wait for it to finish before restarting"
                .to_string(),
        ));
    }
    if cli_tmux_session_is_legacy(workspace_id).await {
        return Err(ApiError::BadRequest(
            "This is a legacy session and cannot be restarted in place; reopen the workspace"
                .to_string(),
        ));
    }

    // Re-probe rather than trusting the status the client saw: the agent may
    // have recovered, or a user may have started something in the fallback
    // shell, between the banner rendering and the button being pressed.
    match probe_workspace_agent(workspace_id, &context.program).await {
        AgentPresence::Absent => {}
        AgentPresence::Alive => {
            return Err(ApiError::Conflict(
                "The agent is already running in this workspace".to_string(),
            ));
        }
        AgentPresence::NoSession => {
            return Err(ApiError::BadRequest(
                "No CLI session exists for this workspace; open the terminal to start one"
                    .to_string(),
            ));
        }
        AgentPresence::Unknown => {
            return Err(ApiError::Conflict(
                "Could not determine whether an agent is running; nothing was restarted. Please retry"
                    .to_string(),
            ));
        }
    }

    // Two tabs showing the same dead pane will both POST this. Without the
    // claim the second respawn would kill the agent the first just started.
    let Some(claim) = CliRespawnClaim::try_claim(workspace_id) else {
        return Err(ApiError::Conflict(
            "A restart is already in progress for this workspace".to_string(),
        ));
    };

    let bootstrap = cli_restart_bootstrap(&context.spec, context.resume_session_id.as_deref());
    respawn_cli_tmux_pane(workspace_id, &claim, &context.working_dir, &bootstrap).await?;

    Ok(ResponseJson(ApiResponse::success(CliRestartResponse {
        restarted: true,
        resumed: context.resume_session_id.is_some(),
    })))
}

pub(super) fn router() -> Router<DeploymentImpl> {
    Router::new()
        .route(
            "/workspaces/{workspace_id}/cli/agent-status",
            get(get_agent_status),
        )
        .route(
            "/workspaces/{workspace_id}/cli/restart",
            post(restart_agent),
        )
}
