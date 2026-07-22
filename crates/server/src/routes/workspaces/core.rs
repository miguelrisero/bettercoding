use axum::{
    Extension, Json,
    extract::{Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
};
use chrono::Utc;
use db::models::{
    archive_bucket::ArchiveBucket,
    coding_agent_turn::CodingAgentTurn,
    execution_process::{ExecutionProcess, ExecutionProcessStatus},
    workspace::{Workspace, WorkspaceError},
};
use deployment::Deployment;
use serde::{Deserialize, Serialize};
use services::services::{container::ContainerService, diff_stream, remote_sync};
use sqlx::Error as SqlxError;
use ts_rs::TS;
use utils::response::ApiResponse;
use uuid::Uuid;
use workspace_manager::WorkspaceManager;

use crate::{DeploymentImpl, error::ApiError};

#[derive(Debug, Deserialize)]
pub struct DeleteWorkspaceQuery {
    #[serde(default)]
    pub delete_remote: bool,
    #[serde(default)]
    pub delete_branches: bool,
}

const RUNNING_PROCESSES_DELETE_MESSAGE: &str =
    "Cannot delete workspace while processes are running. Stop all processes first.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteWorkspaceOutcome {
    Deleted,
    SkippedRunningProcesses,
}

#[derive(Debug, Deserialize, TS)]
pub struct BulkDeleteArchivedWorkspacesRequest {
    pub bucket: ArchiveBucket,
    pub delete_branches: bool,
}

#[derive(Debug, Serialize, TS)]
#[serde(tag = "status", rename_all = "snake_case")]
#[ts(tag = "status", rename_all = "snake_case")]
pub enum BulkDeleteItemOutcome {
    Deleted,
    Skipped { reason: String },
    Failed { reason: String },
}

#[derive(Debug, Serialize, TS)]
pub struct BulkDeleteItemResult {
    pub workspace_id: Uuid,
    pub workspace_name: Option<String>,
    pub outcome: BulkDeleteItemOutcome,
}

#[derive(Debug, Serialize, TS)]
pub struct BulkDeleteArchivedWorkspacesResponse {
    pub results: Vec<BulkDeleteItemResult>,
}

pub async fn get_workspaces(
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<Vec<Workspace>>>, ApiError> {
    let pool = &deployment.db().pool;
    let workspaces = Workspace::fetch_all(pool).await?;
    Ok(ResponseJson(ApiResponse::success(workspaces)))
}

pub async fn get_workspace(
    Extension(workspace): Extension<Workspace>,
) -> Result<ResponseJson<ApiResponse<Workspace>>, ApiError> {
    Ok(ResponseJson(ApiResponse::success(workspace)))
}

pub async fn update_workspace(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Json(request): Json<db::models::requests::UpdateWorkspace>,
) -> Result<ResponseJson<ApiResponse<Workspace>>, ApiError> {
    let pool = &deployment.db().pool;
    let is_archiving = request.archived == Some(true) && !workspace.archived;

    Workspace::update(
        pool,
        workspace.id,
        request.archived,
        request.pinned,
        request.name.as_deref(),
    )
    .await?;
    let updated = Workspace::find_by_id(pool, workspace.id)
        .await?
        .ok_or(WorkspaceError::WorkspaceNotFound)?;

    if (request.archived.is_some() || request.name.is_some())
        && let Ok(client) = deployment.remote_client()
    {
        let ws = updated.clone();
        let name = request.name.clone();
        let archived = request.archived;
        let stats =
            diff_stream::compute_diff_stats(&deployment.db().pool, deployment.git(), &ws).await;
        tokio::spawn(async move {
            remote_sync::sync_workspace_to_remote(
                &client,
                ws.id,
                name.map(Some),
                archived,
                stats.as_ref(),
            )
            .await;
        });
    }

    if is_archiving && let Err(e) = deployment.container().archive_workspace(workspace.id).await {
        tracing::error!("Failed to archive workspace {}: {}", workspace.id, e);
    }

    Ok(ResponseJson(ApiResponse::success(updated)))
}

pub async fn get_first_user_message(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<Option<String>>>, ApiError> {
    let pool = &deployment.db().pool;
    let message = Workspace::get_first_user_message(pool, workspace.id).await?;
    Ok(ResponseJson(ApiResponse::success(message)))
}

pub async fn bulk_delete_archived_workspaces(
    State(deployment): State<DeploymentImpl>,
    Json(request): Json<BulkDeleteArchivedWorkspacesRequest>,
) -> Result<ResponseJson<ApiResponse<BulkDeleteArchivedWorkspacesResponse>>, ApiError> {
    let pool = &deployment.db().pool;
    let now = Utc::now();
    let targets = Workspace::fetch_all(pool)
        .await?
        .into_iter()
        .filter(|workspace| workspace.archived)
        .filter(|workspace| {
            let bucket = workspace.archived_at.map_or(
                // An archived row without a timestamp has unknown age. Keep it
                // in the oldest bucket so it never appears deceptively fresh.
                ArchiveBucket::OlderThanThirtyDays,
                |archived_at| ArchiveBucket::from_age(now.signed_duration_since(archived_at)),
            );
            bucket == request.bucket
        })
        .map(|workspace| (workspace.id, workspace.name))
        .collect::<Vec<_>>();

    let mut results = Vec::with_capacity(targets.len());
    for (workspace_id, resolved_name) in targets {
        let fresh_workspace = match Workspace::find_by_id(pool, workspace_id).await {
            Ok(Some(workspace)) if !workspace.archived => {
                results.push(BulkDeleteItemResult {
                    workspace_id,
                    workspace_name: workspace.name,
                    outcome: BulkDeleteItemOutcome::Skipped {
                        reason: "no longer archived".to_string(),
                    },
                });
                continue;
            }
            Ok(Some(workspace)) => workspace,
            Ok(None) => {
                results.push(BulkDeleteItemResult {
                    workspace_id,
                    workspace_name: resolved_name,
                    outcome: BulkDeleteItemOutcome::Skipped {
                        reason: "already deleted".to_string(),
                    },
                });
                continue;
            }
            Err(error) => {
                results.push(BulkDeleteItemResult {
                    workspace_id,
                    workspace_name: resolved_name,
                    outcome: BulkDeleteItemOutcome::Failed {
                        reason: error.to_string(),
                    },
                });
                continue;
            }
        };
        let workspace_name = fresh_workspace.name.clone();

        let outcome = match delete_workspace_core(
            &deployment,
            fresh_workspace,
            false,
            request.delete_branches,
        )
        .await
        {
            Ok(DeleteWorkspaceOutcome::Deleted) => BulkDeleteItemOutcome::Deleted,
            Ok(DeleteWorkspaceOutcome::SkippedRunningProcesses) => BulkDeleteItemOutcome::Skipped {
                reason: RUNNING_PROCESSES_DELETE_MESSAGE.to_string(),
            },
            Err(error) => BulkDeleteItemOutcome::Failed {
                reason: error.to_string(),
            },
        };
        results.push(BulkDeleteItemResult {
            workspace_id,
            workspace_name,
            outcome,
        });
    }

    Ok(ResponseJson(ApiResponse::success(
        BulkDeleteArchivedWorkspacesResponse { results },
    )))
}

pub async fn delete_workspace(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<DeleteWorkspaceQuery>,
) -> Result<(StatusCode, ResponseJson<ApiResponse<()>>), ApiError> {
    match delete_workspace_core(
        &deployment,
        workspace,
        query.delete_remote,
        query.delete_branches,
    )
    .await?
    {
        DeleteWorkspaceOutcome::Deleted => {
            Ok((StatusCode::ACCEPTED, ResponseJson(ApiResponse::success(()))))
        }
        DeleteWorkspaceOutcome::SkippedRunningProcesses => Err(ApiError::Conflict(
            RUNNING_PROCESSES_DELETE_MESSAGE.to_string(),
        )),
    }
}

pub async fn delete_workspace_core(
    deployment: &DeploymentImpl,
    workspace: Workspace,
    delete_remote: bool,
    delete_branches: bool,
) -> Result<DeleteWorkspaceOutcome, ApiError> {
    let pool = &deployment.db().pool;
    let workspace_manager = deployment.workspace_manager();
    let workspace_id = workspace.id;

    if ExecutionProcess::has_running_non_dev_server_processes_for_workspace(pool, workspace_id)
        .await?
    {
        return Ok(DeleteWorkspaceOutcome::SkippedRunningProcesses);
    }

    let dev_servers =
        ExecutionProcess::find_running_dev_servers_by_workspace(pool, workspace_id).await?;

    for dev_server in dev_servers {
        tracing::info!(
            "Stopping dev server {} before deleting workspace {}",
            dev_server.id,
            workspace_id
        );

        if let Err(e) = deployment
            .container()
            .stop_execution(&dev_server, ExecutionProcessStatus::Killed)
            .await
        {
            tracing::error!(
                "Failed to stop dev server {} for workspace {}: {}",
                dev_server.id,
                workspace_id,
                e
            );
        }
    }

    // Reap the CLI tmux session BEFORE deleting the DB row, so a detached
    // `claude` session can't outlive its workspace. The interactive session is
    // not an execution_process, so the running-process guard above misses it;
    // without this, deletion can orphan a current `bc_<id>` or legacy
    // `vk_<id>` tmux session.
    // Best-effort + idempotent (no-op if already gone or tmux is absent).
    deployment.container().kill_cli_session(workspace_id).await;

    let managed_workspace = workspace_manager.load_managed_workspace(workspace).await?;
    let deletion_context = managed_workspace.prepare_deletion_context().await?;
    let rows_affected = managed_workspace.delete_record().await?;

    if rows_affected == 0 {
        return Err(ApiError::Database(SqlxError::RowNotFound));
    }

    deployment
        .track_if_analytics_allowed(
            "workspace_deleted",
            serde_json::json!({
                "workspace_id": workspace_id.to_string(),
            }),
        )
        .await;

    if delete_remote {
        if let Ok(client) = deployment.remote_client() {
            match client.delete_workspace(workspace_id).await {
                Ok(()) => {
                    tracing::info!("Deleted remote workspace for {}", workspace_id);
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to delete remote workspace for {}: {}",
                        workspace_id,
                        e
                    );
                }
            }
        } else {
            tracing::debug!(
                "Remote client not available, skipping remote deletion for {}",
                workspace_id
            );
        }
    }

    WorkspaceManager::spawn_workspace_deletion_cleanup(deletion_context, delete_branches);

    Ok(DeleteWorkspaceOutcome::Deleted)
}

#[axum::debug_handler]
pub async fn mark_seen(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<()>>, ApiError> {
    let pool = &deployment.db().pool;
    CodingAgentTurn::mark_seen_by_workspace_id(pool, workspace.id).await?;
    Ok(ResponseJson(ApiResponse::success(())))
}
