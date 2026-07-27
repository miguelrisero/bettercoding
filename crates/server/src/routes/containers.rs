use axum::{
    Router,
    extract::{Query, State},
    response::Json as ResponseJson,
    routing::get,
};
use db::models::{
    requests::ContainerQuery,
    workspace::{Workspace, WorkspaceContext, WorkspaceError},
};
use deployment::Deployment;
use serde::Serialize;
use utils::response::ApiResponse;
use uuid::Uuid;

use crate::{DeploymentImpl, error::ApiError};

#[derive(Debug, Serialize)]
struct ContainerInfo {
    pub attempt_id: Uuid,
}

/// Both endpoints answer "which workspace holds this directory?" for clients
/// that probe their own working directory, so being handed a path outside every
/// workspace is routine. Report it as a miss rather than a server fault.
///
/// Returns the error rather than taking a `Result`, because `ApiError` is large
/// enough that a `Result<_, ApiError>` helper trips `clippy::result_large_err`.
fn no_workspace_contains_path() -> ApiError {
    ApiError::Workspace(WorkspaceError::WorkspaceNotFound)
}

async fn get_container_info(
    Query(query): Query<ContainerQuery>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<ContainerInfo>>, ApiError> {
    let info =
        Workspace::resolve_container_ref_by_prefix(&deployment.db().pool, &query.container_ref)
            .await
            .map_err(ApiError::Database)?
            .ok_or_else(no_workspace_contains_path)?;

    Ok(ResponseJson(ApiResponse::success(ContainerInfo {
        attempt_id: info.workspace_id,
    })))
}

async fn get_context(
    State(deployment): State<DeploymentImpl>,
    Query(payload): Query<ContainerQuery>,
) -> Result<ResponseJson<ApiResponse<WorkspaceContext>>, ApiError> {
    let info =
        Workspace::resolve_container_ref_by_prefix(&deployment.db().pool, &payload.container_ref)
            .await
            .map_err(ApiError::Database)?
            .ok_or_else(no_workspace_contains_path)?;

    let ctx = Workspace::load_context(&deployment.db().pool, info.workspace_id).await?;
    Ok(ResponseJson(ApiResponse::success(ctx)))
}

pub(super) fn router(_deployment: &DeploymentImpl) -> Router<DeploymentImpl> {
    Router::new()
        // NOTE: /containers/info is required by the VSCode extension (vibe-kanban-vscode)
        // to auto-detect workspaces. It maps workspace_id to attempt_id for compatibility.
        // Do not remove this endpoint without updating the extension.
        .route("/containers/info", get(get_container_info))
        .route("/containers/attempt-context", get(get_context))
}

#[cfg(test)]
mod tests {
    use axum::{http::StatusCode, response::IntoResponse};

    use super::*;

    #[test]
    fn a_directory_outside_every_workspace_answers_not_found() {
        let status = no_workspace_contains_path().into_response().status();

        // Previously this arrived as ApiError::Database(RowNotFound) and mapped
        // to a 500 with "An internal error occurred.", which made routine probes
        // of a non-workspace directory look like server faults.
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(!status.is_server_error());
    }
}
