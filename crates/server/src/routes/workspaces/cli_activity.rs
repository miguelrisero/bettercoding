use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use chrono::Utc;
use db::models::workspace_cli_activity::{
    CliManual, CliManualKind, WorkspaceCliActivity, normalize_manual_note,
};
use deployment::Deployment;
use executors::executors::codex::rollout::rollout_thread_id;
use serde::Deserialize;
use ts_rs::TS;
use utils::response::ApiResponse;
use uuid::Uuid;

use crate::{DeploymentImpl, error::ApiError};

#[derive(Debug, Deserialize)]
pub struct HookReportQuery {
    /// Nanosecond stamp minted by the reporting hook (`date +%s%N`), so two
    /// events that overtake each other in flight are still ordered by when
    /// they FIRED. Kept as a string: a `date` without `%N` yields something
    /// unparseable rather than nothing, and the server's receipt time orders
    /// the report instead.
    seq: Option<String>,
}

/// Receive one raw Claude Code hook payload for a CLI-mode workspace.
///
/// Registered outside `load_workspace_middleware`: this runs on Claude's own
/// critical path (up to 15 times a turn), so it does the least work that can
/// possibly answer — reduce, upsert, done. The existing SQLite update hook
/// broadcasts the workspace patch from the write.
///
/// It always answers `204`, including for a payload it drops or a database it
/// cannot reach. A display signal must never become a reason the agent stalls.
pub async fn report_cli_activity(
    State(deployment): State<DeploymentImpl>,
    Path(workspace_id): Path<Uuid>,
    Query(query): Query<HookReportQuery>,
    headers: HeaderMap,
    Json(payload): Json<serde_json::Value>,
) -> StatusCode {
    let pool = &deployment.db().pool;
    let now = Utc::now();
    let seq = query
        .seq
        .as_deref()
        .map(str::trim)
        .and_then(|s| s.parse::<i64>().ok())
        .or_else(|| now.timestamp_nanos_opt())
        .unwrap_or_else(|| now.timestamp_millis());

    let (next, switched) = match WorkspaceCliActivity::record_hook(
        pool,
        workspace_id,
        &payload,
        seq,
        now,
    )
    .await
    {
        Ok(Some(report)) => report,
        Ok(None) => return StatusCode::NO_CONTENT,
        Err(error) => {
            tracing::debug!(?error, %workspace_id, "failed to read CLI activity for a hook report");
            return StatusCode::NO_CONTENT;
        }
    };

    if payload.get("hook_event_name").and_then(|v| v.as_str()) == Some("SessionStart")
        && let Some(capability) = headers
            .get("x-bc-handover-capability")
            .and_then(|value| value.to_str().ok())
        && let Err(error) = crate::routes::session_handover::fallback_ready(
            &deployment,
            workspace_id,
            &next.agent_session_id,
            capability,
        )
        .await
    {
        tracing::warn!(?error, %workspace_id, "failed to persist fallback initialization receipt");
    }

    // A CLI-mode Codex names its rollout; import a thread the feed does not
    // track yet, or rebind a pane that switched threads inside the TUI,
    // without waiting for the next registry pass.
    let reports_rollout = next
        .transcript_path
        .as_deref()
        .and_then(|path| std::path::Path::new(path).file_name()?.to_str())
        .and_then(rollout_thread_id)
        .is_some();
    if reports_rollout && let Some(ingest) = deployment.claude_transcript_ingest() {
        ingest
            .request_codex_import(&next.agent_session_id, switched)
            .await;
    }

    StatusCode::NO_CONTENT
}

/// Body of `PUT /workspaces/{id}/cli-activity/manual`. `kind: null` clears.
#[derive(Debug, Deserialize, TS)]
pub struct SetCliManualRequest {
    pub kind: Option<CliManualKind>,
    pub note: Option<String>,
}

/// Lock, mark seen, or clear a workspace's manual state from the UI.
pub async fn set_cli_manual(
    State(deployment): State<DeploymentImpl>,
    Path(workspace_id): Path<Uuid>,
    Json(request): Json<SetCliManualRequest>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let manual = request.kind.map(|kind| CliManual {
        kind,
        // Only a lock carries a note.
        note: match kind {
            CliManualKind::Locked => request.note.as_deref().and_then(normalize_manual_note),
            CliManualKind::Seen => None,
        },
    });
    WorkspaceCliActivity::set_manual(&deployment.db().pool, workspace_id, manual.as_ref()).await?;
    Ok(Json(ApiResponse::success(())))
}
