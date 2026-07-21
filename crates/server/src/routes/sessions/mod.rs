pub mod queue;
pub mod review;

use std::str::FromStr;

use axum::{
    Extension, Json, Router,
    extract::{Query, State},
    http::StatusCode,
    middleware::from_fn_with_state,
    response::Json as ResponseJson,
    routing::{get, post},
};
use db::models::{
    cli_native_record::CliNativeRecord,
    coding_agent_turn::CodingAgentTurn,
    execution_process::{ExecutionProcess, ExecutionProcessRunReason},
    requests::UpdateSession,
    scratch::{Scratch, ScratchType},
    session::{CreateSession, Session, SessionError},
    session_queued_message::QueuedMessageSource,
    workspace::{Workspace, WorkspaceError},
    workspace_repo::WorkspaceRepo,
};
use deployment::Deployment;
use executors::{
    actions::ExecutorActionType,
    executors::{BaseCodingAgent, claude::native::adapt_native_claude_line},
    profile::ExecutorConfig,
};
use serde::Deserialize;
use services::services::{
    cli_collab::{DispatchOutcome, RetryDispatchContext},
    container::ContainerService,
};
use ts_rs::TS;
use utils::response::ApiResponse;
use uuid::Uuid;

use crate::{
    DeploymentImpl, error::ApiError, middleware::load_session_middleware,
    routes::workspaces::execution::RunScriptError,
};

#[derive(Debug, Deserialize)]
pub struct SessionQuery {
    pub workspace_id: Uuid,
}

#[derive(Debug, Deserialize, TS)]
pub struct CreateSessionRequest {
    pub workspace_id: Uuid,
    pub executor: Option<String>,
    pub name: Option<String>,
}

pub async fn get_sessions(
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<SessionQuery>,
) -> Result<ResponseJson<ApiResponse<Vec<Session>>>, ApiError> {
    let pool = &deployment.db().pool;
    let sessions = Session::find_by_workspace_id(pool, query.workspace_id).await?;
    Ok(ResponseJson(ApiResponse::success(sessions)))
}

pub async fn get_session(
    Extension(session): Extension<Session>,
) -> Result<ResponseJson<ApiResponse<Session>>, ApiError> {
    Ok(ResponseJson(ApiResponse::success(session)))
}

pub async fn create_session(
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<CreateSessionRequest>,
) -> Result<ResponseJson<ApiResponse<Session>>, ApiError> {
    let pool = &deployment.db().pool;

    // Verify workspace exists
    let _workspace = Workspace::find_by_id(pool, payload.workspace_id)
        .await?
        .ok_or(ApiError::Workspace(WorkspaceError::ValidationError(
            "Workspace not found".to_string(),
        )))?;

    let session = Session::create(
        pool,
        &CreateSession {
            executor: payload.executor,
            name: payload.name,
        },
        Uuid::new_v4(),
        payload.workspace_id,
    )
    .await?;

    Ok(ResponseJson(ApiResponse::success(session)))
}

pub async fn update_session(
    Extension(session): Extension<Session>,
    State(deployment): State<DeploymentImpl>,
    Json(request): Json<UpdateSession>,
) -> Result<ResponseJson<ApiResponse<Session>>, ApiError> {
    let pool = &deployment.db().pool;

    Session::update(pool, session.id, request.name.as_deref()).await?;

    let updated = Session::find_by_id(pool, session.id)
        .await?
        .ok_or(ApiError::Session(SessionError::NotFound))?;

    Ok(ResponseJson(ApiResponse::success(updated)))
}

#[derive(Debug, Deserialize, TS)]
pub struct CreateFollowUpAttempt {
    pub prompt: String,
    pub executor_config: ExecutorConfig,
    pub retry_process_id: Option<Uuid>,
    pub force_when_dirty: Option<bool>,
    pub perform_git_reset: Option<bool>,
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Deserialize, TS)]
pub struct ResetProcessRequest {
    pub process_id: Uuid,
    pub force_when_dirty: Option<bool>,
    pub perform_git_reset: Option<bool>,
}

type DispatchResponse = (
    StatusCode,
    ResponseJson<ApiResponse<DispatchOutcome, DispatchOutcome>>,
);

fn dispatch_response(outcome: DispatchOutcome) -> DispatchResponse {
    if matches!(&outcome, DispatchOutcome::Conflict { .. }) {
        (
            StatusCode::CONFLICT,
            ResponseJson(ApiResponse::error_with_data(outcome)),
        )
    } else {
        (StatusCode::OK, ResponseJson(ApiResponse::success(outcome)))
    }
}

pub async fn follow_up(
    Extension(session): Extension<Session>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<CreateFollowUpAttempt>,
) -> Result<DispatchResponse, ApiError> {
    let pool = &deployment.db().pool;

    // Load workspace from session
    let workspace = Workspace::find_by_id(pool, session.workspace_id)
        .await?
        .ok_or(ApiError::Workspace(WorkspaceError::ValidationError(
            "Workspace not found".to_string(),
        )))?;

    tracing::info!("{:?}", workspace);

    deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;

    let executor_profile_id = payload.executor_config.profile_id();

    // Validate executor matches session if session has prior executions
    let expected_executor: Option<String> =
        ExecutionProcess::latest_executor_profile_for_session(pool, session.id)
            .await?
            .map(|profile| profile.executor.to_string())
            .or_else(|| session.executor.clone());

    if let Some(expected) = expected_executor {
        let actual = executor_profile_id.executor.to_string();
        if expected != actual {
            return Err(ApiError::Session(SessionError::ExecutorMismatch {
                expected,
                actual,
            }));
        }
    }

    if session.executor.is_none() {
        Session::update_executor(pool, session.id, &executor_profile_id.executor.to_string())
            .await?;
    }

    let retry = if let Some(process_id) = payload.retry_process_id {
        let reset_to_message_id = CodingAgentTurn::find_latest_session_info(pool, session.id)
            .await?
            .and_then(|info| info.message_id);
        Some(RetryDispatchContext {
            process_id,
            force_when_dirty: payload.force_when_dirty.unwrap_or(false),
            perform_git_reset: payload.perform_git_reset.unwrap_or(true),
            reset_to_message_id,
        })
    } else {
        None
    };

    let dispatch = if let Some(retry) = retry {
        deployment
            .cli_collab()
            .dispatch_retry(
                &session,
                payload.prompt,
                payload.executor_config,
                QueuedMessageSource::Ui,
                payload.replace,
                retry,
            )
            .await
    } else {
        deployment
            .cli_collab()
            .dispatch_gate(
                &session,
                payload.prompt,
                payload.executor_config,
                QueuedMessageSource::Ui,
                payload.replace,
            )
            .await
    };
    let outcome = match dispatch {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(?error, session_id = %session.id, "follow-up dispatch gate failed closed");
            return Err(ApiError::Conflict(
                "Writer state could not be verified; the message was not dispatched".to_string(),
            ));
        }
    };
    let conflict = matches!(&outcome, DispatchOutcome::Conflict { .. });

    // Clear the draft follow-up scratch on successful spawn
    // This ensures the scratch is wiped even if the user navigates away quickly
    if !conflict
        && let Err(e) = Scratch::delete(pool, session.id, &ScratchType::DraftFollowUp).await
    {
        // Log but don't fail the request - scratch deletion is best-effort
        tracing::debug!(
            "Failed to delete draft follow-up scratch for session {}: {}",
            session.id,
            e
        );
    }

    Ok(dispatch_response(outcome))
}

#[derive(Debug, Deserialize, TS)]
pub struct ForkRecoveryRequest {
    pub fork_parent_uuid: String,
    pub branch_leaf_uuid: String,
}

async fn latest_executor_config(
    pool: &sqlx::SqlitePool,
    session: &Session,
) -> Result<ExecutorConfig, ApiError> {
    for process in ExecutionProcess::find_by_session_id(pool, session.id, false)
        .await?
        .iter()
        .rev()
    {
        let Ok(action) = process.executor_action() else {
            continue;
        };
        match action.typ() {
            ExecutorActionType::CodingAgentInitialRequest(request) => {
                return Ok(request.executor_config.clone());
            }
            ExecutorActionType::CodingAgentFollowUpRequest(request) => {
                return Ok(request.executor_config.clone());
            }
            _ => {}
        }
    }
    let executor = session
        .executor
        .as_deref()
        .and_then(|executor| BaseCodingAgent::from_str(executor).ok())
        .ok_or_else(|| {
            ApiError::BadRequest("No executor configuration exists for this session".to_string())
        })?;
    Ok(ExecutorConfig::new(executor))
}

pub async fn fork_recovery(
    Extension(session): Extension<Session>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<ForkRecoveryRequest>,
) -> Result<DispatchResponse, ApiError> {
    let pool = &deployment.db().pool;
    let workspace = Workspace::find_by_id(pool, session.workspace_id)
        .await?
        .ok_or(ApiError::Workspace(WorkspaceError::ValidationError(
            "Workspace not found".to_string(),
        )))?;
    deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;
    let path = CliNativeRecord::dropped_branch_path(
        pool,
        session.id,
        &payload.fork_parent_uuid,
        &payload.branch_leaf_uuid,
    )
    .await?
    .ok_or_else(|| ApiError::BadRequest("The requested fork branch was not found".to_string()))?;
    let mut prompts = Vec::new();
    for record in path {
        if record.uuid.as_deref() == Some(payload.fork_parent_uuid.as_str())
            || record.kind != "user"
        {
            continue;
        }
        if let Ok(line) = adapt_native_claude_line(&record.raw, &record.claude_session_id)
            && let Some(prompt) = line.plain_user_text()
        {
            prompts.push(prompt);
        }
    }
    if prompts.is_empty() {
        return Err(ApiError::BadRequest(
            "The requested fork branch has no recoverable user messages".to_string(),
        ));
    }
    let prompt = if prompts.len() == 1 {
        prompts.pop().expect("one prompt exists")
    } else {
        format!(
            "Recovered messages from an alternate CLI branch:\n\n{}",
            prompts.join("\n\n--- recovered message ---\n\n")
        )
    };
    let executor_config = latest_executor_config(pool, &session).await?;
    let outcome = match deployment
        .cli_collab()
        .dispatch_gate(
            &session,
            prompt,
            executor_config,
            QueuedMessageSource::Recovery,
            false,
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(?error, session_id = %session.id, "fork recovery dispatch failed closed");
            return Err(ApiError::Conflict(
                "Writer state could not be verified; recovery was not dispatched".to_string(),
            ));
        }
    };
    Ok(dispatch_response(outcome))
}

pub async fn reset_process(
    Extension(session): Extension<Session>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<ResetProcessRequest>,
) -> Result<ResponseJson<ApiResponse<()>>, ApiError> {
    let force_when_dirty = payload.force_when_dirty.unwrap_or(false);
    let perform_git_reset = payload.perform_git_reset.unwrap_or(true);

    deployment
        .container()
        .reset_session_to_process(
            session.id,
            payload.process_id,
            perform_git_reset,
            force_when_dirty,
        )
        .await?;

    Ok(ResponseJson(ApiResponse::success(())))
}

pub async fn run_setup_script(
    Extension(session): Extension<Session>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<ExecutionProcess, RunScriptError>>, ApiError> {
    let pool = &deployment.db().pool;

    let workspace = Workspace::find_by_id(pool, session.workspace_id)
        .await?
        .ok_or(ApiError::Workspace(WorkspaceError::ValidationError(
            "Workspace not found".to_string(),
        )))?;

    if ExecutionProcess::has_running_non_dev_server_processes_for_workspace(pool, workspace.id)
        .await?
    {
        return Ok(ResponseJson(ApiResponse::error_with_data(
            RunScriptError::ProcessAlreadyRunning,
        )));
    }

    deployment
        .container()
        .ensure_container_exists(&workspace)
        .await?;

    let repos = WorkspaceRepo::find_repos_for_workspace(pool, workspace.id).await?;
    let executor_action = match deployment.container().setup_actions_for_repos(&repos) {
        Some(action) => action,
        None => {
            return Ok(ResponseJson(ApiResponse::error_with_data(
                RunScriptError::NoScriptConfigured,
            )));
        }
    };

    let execution_process = deployment
        .container()
        .start_execution(
            &workspace,
            &session,
            &executor_action,
            &ExecutionProcessRunReason::SetupScript,
        )
        .await?;

    deployment
        .track_if_analytics_allowed(
            "setup_script_executed",
            serde_json::json!({
                "workspace_id": workspace.id.to_string(),
            }),
        )
        .await;

    Ok(ResponseJson(ApiResponse::success(execution_process)))
}

pub fn router(deployment: &DeploymentImpl) -> Router<DeploymentImpl> {
    let session_id_router = Router::new()
        .route("/", get(get_session).put(update_session))
        .route("/follow-up", post(follow_up))
        .route("/fork-recovery", post(fork_recovery))
        .route("/reset", post(reset_process))
        .route("/setup", post(run_setup_script))
        .route("/review", post(review::start_review))
        .layer(from_fn_with_state(
            deployment.clone(),
            load_session_middleware,
        ));

    let sessions_router = Router::new()
        .route("/", get(get_sessions).post(create_session))
        .nest("/{session_id}", session_id_router)
        .nest("/{session_id}/queue", queue::router(deployment));

    Router::new().nest("/sessions", sessions_router)
}
