use std::collections::HashMap;

use axum::{Json, extract::State, response::Json as ResponseJson};
use db::models::{
    coding_agent_turn::CodingAgentTurn,
    execution_process::{ExecutionProcess, ExecutionProcessStatus},
    merge::MergeStatus,
    pull_request::PullRequest,
    workspace::Workspace,
    workspace_cli_activity::{CliManual, CliPhase, WorkspaceCliActivity},
    workspace_repo::WorkspaceRepo,
};
use deployment::Deployment;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utils::response::ApiResponse;
use uuid::Uuid;

use crate::{DeploymentImpl, error::ApiError};

/// Request for fetching workspace summaries
#[derive(Debug, Deserialize, Serialize, TS)]
pub struct WorkspaceSummaryRequest {
    pub archived: bool,
}

/// Summary info for a single workspace
#[derive(Debug, Serialize, TS)]
pub struct WorkspaceSummary {
    pub workspace_id: Uuid,
    /// Number of repositories/worktrees owned by this workspace.
    pub repo_count: usize,
    /// Session ID of the latest execution process
    pub latest_session_id: Option<Uuid>,
    /// Is a tool approval currently pending?
    pub has_pending_approval: bool,
    /// When the latest execution process completed
    #[ts(optional)]
    pub latest_process_completed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Status of the latest execution process
    pub latest_process_status: Option<ExecutionProcessStatus>,
    /// Is a dev server currently running?
    pub has_running_dev_server: bool,
    /// Does this workspace have unseen coding agent turns?
    pub has_unseen_turns: bool,
    /// Did this workspace's CLI-mode claude session finish while no terminal
    /// was attached? (Cleared when the user opens the pane again.)
    pub cli_attention: bool,
    /// What the CLI-mode agent last reported it is doing (see `reduce_hook`),
    /// however old: the UI ages it with `cli_phase_at`. `None` for an agent
    /// that has never reported.
    pub cli_phase: Option<CliPhase>,
    /// When `cli_phase` was reported.
    #[ts(optional)]
    pub cli_phase_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Background tasks / scheduled jobs armed at the last turn end. `None`
    /// means unknown, never zero.
    pub cli_tasks: Option<i64>,
    pub cli_crons: Option<i64>,
    /// When the CLI session last changed state (hook report or poller).
    #[ts(optional)]
    pub cli_activity_at: Option<chrono::DateTime<chrono::Utc>>,
    /// A state the user set by hand (locked / seen).
    pub cli_manual: Option<CliManual>,
    /// PR status for this workspace (if any PR exists)
    pub pr_status: Option<MergeStatus>,
    /// PR number for this workspace (if any PR exists)
    pub pr_number: Option<i64>,
    /// PR URL for this workspace (if any PR exists)
    pub pr_url: Option<String>,
}

/// Response containing summaries for requested workspaces
#[derive(Debug, Serialize, TS)]
pub struct WorkspaceSummaryResponse {
    pub summaries: Vec<WorkspaceSummary>,
}

/// Diff totals for one workspace, as the workspace view displays them.
#[derive(Debug, Clone, Default, Serialize, TS)]
pub struct DiffStats {
    pub files_changed: usize,
    pub lines_added: usize,
    pub lines_removed: usize,
}

/// Fetch summary information for workspaces filtered by archived status.
/// This endpoint returns data that cannot be efficiently included in the streaming endpoint.
#[axum::debug_handler]
pub async fn get_workspace_summaries(
    State(deployment): State<DeploymentImpl>,
    Json(request): Json<WorkspaceSummaryRequest>,
) -> Result<ResponseJson<ApiResponse<WorkspaceSummaryResponse>>, ApiError> {
    let pool = &deployment.db().pool;
    let archived = request.archived;

    // 1. Fetch all workspaces with the given archived status
    let workspaces: Vec<Workspace> = Workspace::find_all_with_status(pool, Some(archived), None)
        .await?
        .into_iter()
        .map(|ws| ws.workspace)
        .collect();

    if workspaces.is_empty() {
        return Ok(ResponseJson(ApiResponse::success(
            WorkspaceSummaryResponse { summaries: vec![] },
        )));
    }

    // 2. Fetch latest process info for workspaces with this archived status
    let latest_processes = ExecutionProcess::find_latest_for_workspaces(pool, archived).await?;

    // 3. Check which workspaces have running dev servers
    let dev_server_workspaces =
        ExecutionProcess::find_workspaces_with_running_dev_servers(pool, archived).await?;

    // 4. Check pending approvals for running processes
    let running_ep_ids: Vec<_> = latest_processes
        .values()
        .filter(|info| info.status == ExecutionProcessStatus::Running)
        .map(|info| info.execution_process_id)
        .collect();
    let pending_approval_eps = deployment
        .approvals()
        .get_pending_execution_process_ids(&running_ep_ids);

    // 5. Check which workspaces have unseen coding agent turns
    let unseen_workspaces = CodingAgentTurn::find_workspaces_with_unseen(pool, archived).await?;

    // 5b. Check which workspaces' CLI tmux sessions finished unattended
    let cli_attention_workspaces =
        WorkspaceCliActivity::find_workspaces_needing_attention(pool, archived).await?;

    // 5c. What each CLI-mode agent last reported about itself
    let cli_rows: HashMap<Uuid, WorkspaceCliActivity> = WorkspaceCliActivity::find_all(pool)
        .await?
        .into_iter()
        .map(|row| (row.workspace_id, row))
        .collect();
    let mut cli_manuals = WorkspaceCliActivity::find_all_manual(pool).await?;

    // 6. Get PR status for each workspace
    let pr_statuses = PullRequest::get_latest_for_workspaces(pool, archived).await?;

    // 7. Count repositories/worktrees for each workspace (in parallel)
    let repo_count_futures = workspaces.iter().map(|workspace| async move {
        WorkspaceRepo::find_by_workspace_id(pool, workspace.id)
            .await
            .map(|repos| (workspace.id, repos.len()))
    });
    let repo_counts: HashMap<Uuid, usize> = futures_util::future::join_all(repo_count_futures)
        .await
        .into_iter()
        .collect::<Result<_, _>>()?;

    // 8. Assemble response
    let summaries: Vec<WorkspaceSummary> = workspaces
        .iter()
        .map(|ws| {
            let id = ws.id;
            let latest = latest_processes.get(&id);
            let has_pending = latest
                .map(|p| pending_approval_eps.contains(&p.execution_process_id))
                .unwrap_or(false);
            let cli = cli_rows.get(&id);
            let cli_hook = cli.and_then(|row| row.hook.as_ref());

            WorkspaceSummary {
                workspace_id: id,
                repo_count: repo_counts.get(&id).copied().unwrap_or_default(),
                latest_session_id: latest.map(|p| p.session_id),
                has_pending_approval: has_pending,
                latest_process_completed_at: latest.and_then(|p| p.completed_at),
                latest_process_status: latest.map(|p| p.status.clone()),
                has_running_dev_server: dev_server_workspaces.contains(&id),
                has_unseen_turns: unseen_workspaces.contains(&id),
                cli_attention: cli_attention_workspaces.contains(&id),
                cli_phase: cli.and_then(WorkspaceCliActivity::shown_phase),
                cli_phase_at: cli.and_then(|row| row.hook_at),
                cli_tasks: cli_hook.and_then(|hook| hook.tasks),
                cli_crons: cli_hook.and_then(|hook| hook.crons),
                cli_activity_at: cli.map(|row| row.updated_at),
                cli_manual: cli_manuals.remove(&id),
                pr_status: pr_statuses.get(&id).map(|pr| pr.pr_status.clone()),
                pr_number: pr_statuses.get(&id).map(|pr| pr.pr_number),
                pr_url: pr_statuses.get(&id).map(|pr| pr.pr_url.clone()),
            }
        })
        .collect();

    Ok(ResponseJson(ApiResponse::success(
        WorkspaceSummaryResponse { summaries },
    )))
}
