//! REST API for agentic-loop automation (part 1): the per-workspace "keep
//! going" policy plus manual wake-ups ("ping at 05:00 UTC"). The supervisor
//! (`local_deployment::loop_supervisor`) consumes what these write.

use axum::{
    Json, Router,
    extract::{Path, State},
    response::Json as ResponseJson,
    routing::{delete, get, post},
};
use chrono::{DateTime, Utc};
use db::models::loop_automation::{LoopAutomation, ScheduledWakeup, WakeupKind};
use deployment::Deployment;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utils::response::ApiResponse;
use uuid::Uuid;

use crate::{DeploymentImpl, error::ApiError};

/// Combined policy + pending wake-ups for a workspace (one round-trip for the UI).
#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct LoopAutomationStatus {
    /// `None` when the workspace has never been configured (treated as disabled).
    pub policy: Option<LoopAutomation>,
    pub pending_wakeups: Vec<ScheduledWakeup>,
}

#[derive(Debug, Deserialize, TS)]
#[ts(export)]
pub struct UpsertLoopAutomationRequest {
    pub enabled: bool,
    #[serde(default)]
    pub retry_interval_secs: Option<i64>,
    #[serde(default)]
    pub continuation_prompt: Option<String>,
    #[serde(default)]
    pub max_attempts: Option<i64>,
}

#[derive(Debug, Deserialize, TS)]
#[ts(export)]
pub struct CreateWakeupRequest {
    /// When to fire (UTC). The supervisor delivers the prompt into the CLI pane.
    #[ts(type = "string")]
    pub fire_at: DateTime<Utc>,
    /// Message to deliver; falls back to the policy's continuation prompt.
    #[serde(default)]
    pub prompt: Option<String>,
}

/// Lower bound on the retry interval so a misconfiguration can't hammer the pane.
const MIN_RETRY_INTERVAL_SECS: i64 = 60;
const DEFAULT_RETRY_INTERVAL_SECS: i64 = 600;
const DEFAULT_MAX_ATTEMPTS: i64 = 50;
const DEFAULT_CONTINUATION: &str = "Continue.";

async fn status_for(
    deployment: &DeploymentImpl,
    workspace_id: Uuid,
) -> Result<LoopAutomationStatus, ApiError> {
    let pool = &deployment.db().pool;
    let policy = LoopAutomation::get(pool, workspace_id).await?;
    let pending_wakeups = ScheduledWakeup::list_for_workspace(pool, workspace_id, false).await?;
    Ok(LoopAutomationStatus {
        policy,
        pending_wakeups,
    })
}

pub async fn get_status(
    State(deployment): State<DeploymentImpl>,
    Path(workspace_id): Path<Uuid>,
) -> Result<ResponseJson<ApiResponse<LoopAutomationStatus>>, ApiError> {
    Ok(ResponseJson(ApiResponse::success(
        status_for(&deployment, workspace_id).await?,
    )))
}

pub async fn upsert_policy(
    State(deployment): State<DeploymentImpl>,
    Path(workspace_id): Path<Uuid>,
    Json(payload): Json<UpsertLoopAutomationRequest>,
) -> Result<ResponseJson<ApiResponse<LoopAutomationStatus>>, ApiError> {
    let pool = &deployment.db().pool;

    let retry_interval_secs = payload
        .retry_interval_secs
        .unwrap_or(DEFAULT_RETRY_INTERVAL_SECS)
        .max(MIN_RETRY_INTERVAL_SECS);
    let continuation_prompt = payload
        .continuation_prompt
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CONTINUATION.to_string());
    let max_attempts = payload.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS).max(0);

    LoopAutomation::upsert(
        pool,
        workspace_id,
        payload.enabled,
        retry_interval_secs,
        &continuation_prompt,
        max_attempts,
    )
    .await?;

    // Disabling the loop clears any pending auto-retries so a stale wake-up
    // doesn't poke the agent after the user turned it off.
    if !payload.enabled {
        ScheduledWakeup::delete_pending_for_workspace(pool, workspace_id).await?;
    }

    Ok(ResponseJson(ApiResponse::success(
        status_for(&deployment, workspace_id).await?,
    )))
}

pub async fn create_wakeup(
    State(deployment): State<DeploymentImpl>,
    Path(workspace_id): Path<Uuid>,
    Json(payload): Json<CreateWakeupRequest>,
) -> Result<ResponseJson<ApiResponse<ScheduledWakeup>>, ApiError> {
    let pool = &deployment.db().pool;
    let prompt = payload.prompt.filter(|p| !p.trim().is_empty());
    let wakeup = ScheduledWakeup::create(
        pool,
        workspace_id,
        payload.fire_at,
        WakeupKind::Manual,
        prompt.as_deref(),
        1,
    )
    .await?;
    Ok(ResponseJson(ApiResponse::success(wakeup)))
}

pub async fn delete_wakeup(
    State(deployment): State<DeploymentImpl>,
    Path(wakeup_id): Path<Uuid>,
) -> Result<ResponseJson<ApiResponse<()>>, ApiError> {
    ScheduledWakeup::delete(&deployment.db().pool, wakeup_id).await?;
    Ok(ResponseJson(ApiResponse::success(())))
}

pub fn router() -> Router<DeploymentImpl> {
    Router::new()
        .route(
            "/workspaces/{workspace_id}/loop-automation",
            get(get_status).put(upsert_policy),
        )
        .route(
            "/workspaces/{workspace_id}/loop-automation/wakeups",
            post(create_wakeup),
        )
        .route(
            "/loop-automation/wakeups/{wakeup_id}",
            delete(delete_wakeup),
        )
}
