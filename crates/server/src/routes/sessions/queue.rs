use axum::{
    Extension, Json, Router, extract::State, http::StatusCode, middleware::from_fn_with_state,
    response::Json as ResponseJson, routing::get,
};
use db::models::{
    scratch::DraftFollowUpData, session::Session, session_queued_message::QueuedMessageSource,
};
use deployment::Deployment;
use executors::profile::ExecutorConfig;
use serde::Deserialize;
use services::services::queued_message::{QueueMutation, QueueStatus};
use ts_rs::TS;
use utils::response::ApiResponse;

use crate::{DeploymentImpl, error::ApiError, middleware::load_session_middleware};

#[derive(Debug, Deserialize, TS)]
pub struct QueueMessageRequest {
    pub message: String,
    pub executor_config: ExecutorConfig,
    #[serde(default)]
    pub replace: bool,
}

type QueueResponse = (
    StatusCode,
    ResponseJson<ApiResponse<QueueStatus, QueueStatus>>,
);

fn mutation_response(result: QueueMutation) -> QueueResponse {
    match result {
        QueueMutation::Stored(status) => {
            (StatusCode::OK, ResponseJson(ApiResponse::success(status)))
        }
        QueueMutation::Conflict(status) => (
            StatusCode::CONFLICT,
            ResponseJson(ApiResponse::error_with_data(status)),
        ),
    }
}

async fn queue_message(
    Extension(session): Extension<Session>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<QueueMessageRequest>,
) -> Result<QueueResponse, ApiError> {
    let data = DraftFollowUpData {
        message: payload.message,
        executor_config: payload.executor_config,
    };
    let result = deployment
        .queued_message_service()
        .queue_message(session.id, data, QueuedMessageSource::Ui, payload.replace)
        .await?;

    if matches!(result, QueueMutation::Stored(_)) {
        deployment
            .track_if_analytics_allowed(
                "follow_up_queued",
                serde_json::json!({
                    "session_id": session.id.to_string(),
                    "workspace_id": session.workspace_id.to_string(),
                }),
            )
            .await;
    }
    Ok(mutation_response(result))
}

async fn cancel_queued_message(
    Extension(session): Extension<Session>,
    State(deployment): State<DeploymentImpl>,
) -> Result<QueueResponse, ApiError> {
    let result = deployment
        .queued_message_service()
        .cancel_queued(session.id)
        .await?;
    if matches!(result, QueueMutation::Stored(_)) {
        deployment
            .track_if_analytics_allowed(
                "follow_up_queue_cancelled",
                serde_json::json!({
                    "session_id": session.id.to_string(),
                    "workspace_id": session.workspace_id.to_string(),
                }),
            )
            .await;
    }
    Ok(mutation_response(result))
}

async fn get_queue_status(
    Extension(session): Extension<Session>,
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<QueueStatus>>, ApiError> {
    let status = deployment
        .queued_message_service()
        .get_status(session.id)
        .await?;
    Ok(ResponseJson(ApiResponse::success(status)))
}

pub(super) fn router(deployment: &DeploymentImpl) -> Router<DeploymentImpl> {
    Router::new()
        .route(
            "/",
            get(get_queue_status)
                .post(queue_message)
                .delete(cancel_queued_message),
        )
        .layer(from_fn_with_state(
            deployment.clone(),
            load_session_middleware,
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_conflict_returns_409_with_slot_status_body() {
        let (status, ResponseJson(body)) =
            mutation_response(QueueMutation::Conflict(QueueStatus::Empty));

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            serde_json::to_value(body).unwrap(),
            serde_json::json!({
                "success": false,
                "data": null,
                "error_data": { "status": "empty" },
                "message": null
            })
        );
    }
}
