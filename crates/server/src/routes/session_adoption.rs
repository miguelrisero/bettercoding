use api_types::handover::{AdoptWorkspaceSessionRequest, AdoptWorkspaceSessionResponse};
use axum::{Json, Router, extract::State, routing::post};
use db::models::{
    cli_native_file::CliNativeFile, cli_pane_binding::CliPaneBinding, session::Session,
    workspace::Workspace,
};
use deployment::Deployment;
use local_deployment::pty::{CLI_AGENT_PROGRAMS, cli_pane_agent_processes, cli_workspace_pane_id};
use utils::response::ApiResponse;

use super::cli_agent::cli_lifecycle_lock;
use crate::{DeploymentImpl, error::ApiError};

fn conflict() -> ApiError {
    ApiError::Conflict("Adoption identity, binding or transcript evidence does not match".into())
}

#[cfg(target_os = "linux")]
fn process_start(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(target_os = "linux")]
async fn adopt(
    State(deployment): State<DeploymentImpl>,
    Json(request): Json<AdoptWorkspaceSessionRequest>,
) -> Result<Json<ApiResponse<AdoptWorkspaceSessionResponse>>, ApiError> {
    let _guard = cli_lifecycle_lock(request.workspace_id).lock_owned().await;
    let pool = &deployment.db().pool;
    let reservation = db::models::workspace_spawn_reservation::WorkspaceSpawnReservation::acquire(
        pool,
        request.workspace_id,
        db::models::workspace_spawn_reservation::SpawnReservationHolder::Cli,
    )
    .await?
    .ok_or_else(conflict)?;
    let result = adopt_checked(&deployment, &request).await;
    db::models::workspace_spawn_reservation::WorkspaceSpawnReservation::release(
        pool,
        request.workspace_id,
        &reservation.fence,
    )
    .await?;
    result
}

#[cfg(target_os = "linux")]
async fn adopt_checked(
    deployment: &DeploymentImpl,
    request: &AdoptWorkspaceSessionRequest,
) -> Result<Json<ApiResponse<AdoptWorkspaceSessionResponse>>, ApiError> {
    use std::os::unix::fs::MetadataExt;
    let pool = &deployment.db().pool;
    let native = uuid::Uuid::parse_str(&request.native_session_id).map_err(|_| conflict())?;
    if native.get_version_num() != 4
        || process_start(request.expected_pid) != Some(request.expected_start_time)
    {
        return Err(conflict());
    }
    let workspace = Workspace::find_by_id(pool, request.workspace_id)
        .await?
        .ok_or_else(conflict)?;
    let session = Session::find_by_id(pool, request.session_id)
        .await?
        .filter(|s| s.workspace_id == workspace.id && s.executor.as_deref() == Some("CLAUDE_CODE"))
        .ok_or_else(conflict)?;
    let cwd = session
        .effective_working_dir(std::path::Path::new(
            workspace.container_ref.as_deref().ok_or_else(conflict)?,
        ))
        .ok_or_else(conflict)?
        .canonicalize()?;
    if std::fs::read_link(format!("/proc/{}/cwd", request.expected_pid))?.canonicalize()? != cwd {
        return Err(conflict());
    }
    if cli_workspace_pane_id(workspace.id).await? != request.expected_pane_id {
        return Err(conflict());
    }
    let (program, processes) = cli_pane_agent_processes(workspace.id, CLI_AGENT_PROGRAMS)
        .await
        .ok_or_else(conflict)?;
    if program != Some("claude") || processes.len() != 1 || processes[0].pid != request.expected_pid
    {
        return Err(conflict());
    }
    let files = CliNativeFile::list_latest_by_sid(pool, &request.native_session_id).await?;
    let local: Vec<_> = files
        .iter()
        .filter(|f| f.discovered_workspace_id == Some(workspace.id))
        .collect();
    if local.is_empty()
        || files.iter().any(|f| {
            f.discovered_workspace_id
                .is_some_and(|id| id != workspace.id)
        })
    {
        return Err(conflict());
    }
    let transcript_paths: Vec<_> = local
        .iter()
        .filter_map(|file| {
            let path = std::path::Path::new(&file.dir_path).join(&file.file_name);
            let metadata = std::fs::metadata(&path).ok()?;
            (metadata.dev() as i64 == file.dev
                && metadata.ino() as i64 == file.inode
                && file.file_name == format!("{}.jsonl", request.native_session_id))
            .then_some(path)
        })
        .collect();
    if transcript_paths.is_empty() {
        return Err(conflict());
    }
    let tasks = dirs::home_dir()
        .ok_or_else(conflict)?
        .join(".claude/tasks")
        .join(&request.native_session_id);
    let mut native_proof = false;
    for fd in std::fs::read_dir(format!("/proc/{}/fd", request.expected_pid))? {
        if let Ok(path) = std::fs::read_link(fd?.path()) {
            if transcript_paths.contains(&path) || path == tasks {
                native_proof = true;
            }
        }
    }
    // An open session-specific transcript or task directory ties a fresh CLI process to its native UUID.
    let live = cli_pane_agent_processes(workspace.id, CLI_AGENT_PROGRAMS).await;
    if !native_proof
        || process_start(request.expected_pid) != Some(request.expected_start_time)
        || cli_workspace_pane_id(workspace.id).await? != request.expected_pane_id
        || !live.is_some_and(|(program, processes)| {
            program == Some("claude")
                && processes.len() == 1
                && processes[0].pid == request.expected_pid
        })
        || std::fs::read_link(format!("/proc/{}/cwd", request.expected_pid))?.canonicalize()? != cwd
    {
        return Err(conflict());
    }
    let adopted = CliPaneBinding::adopt_session(
        pool,
        request.expected_binding_id,
        request.expected_session_id,
        request.expected_native_session_id.as_deref(),
        &request.native_session_id,
        request.session_id,
        request.workspace_id,
        &cwd.to_string_lossy(),
    )
    .await?;
    if adopted.is_none() {
        return Err(conflict());
    }
    let binding = CliPaneBinding::find_active_for_workspace(pool, workspace.id)
        .await?
        .ok_or_else(conflict)?;
    Ok(Json(ApiResponse::success(AdoptWorkspaceSessionResponse {
        workspace_id: workspace.id,
        binding_id: binding.id,
        session_id: request.session_id,
        native_session_id: request.native_session_id.clone(),
        pane_id: request.expected_pane_id.clone(),
        pid: request.expected_pid,
        start_time: request.expected_start_time,
    })))
}

#[cfg(not(target_os = "linux"))]
async fn adopt() -> Result<(), ApiError> {
    Err(ApiError::BadRequest(
        "Session adoption requires Linux process identity evidence".into(),
    ))
}

pub(super) fn router() -> Router<DeploymentImpl> {
    Router::new().route("/sessions/adopt", post(adopt))
}
