use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::Utc;
use db::models::workspace_cli_activity::{WorkspaceCliActivity, reduce_hook};
use deployment::Deployment;
use serde::Deserialize;
use uuid::Uuid;

use crate::DeploymentImpl;

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

    // Two hooks firing at once both reduce against the row as they found it,
    // so the later write wins. `upsert_hook`'s seq guard bounds that to "a
    // newer report wins", which is the property that matters.
    let previous = match WorkspaceCliActivity::find_by_workspace_id(pool, workspace_id).await {
        Ok(row) => row,
        Err(error) => {
            tracing::debug!(?error, %workspace_id, "failed to read CLI activity for a hook report");
            return StatusCode::NO_CONTENT;
        }
    };

    let Some(next) = reduce_hook(
        previous.as_ref().and_then(|row| row.hook.as_ref()),
        &payload,
        seq,
    ) else {
        return StatusCode::NO_CONTENT;
    };

    // FK failures are expected when the workspace was deleted while its tmux
    // session lingered; nothing here is worth failing the agent's hook over.
    if let Err(error) = WorkspaceCliActivity::upsert_hook(pool, workspace_id, &next, now).await {
        tracing::debug!(?error, %workspace_id, "failed to record a CLI hook report");
    }

    StatusCode::NO_CONTENT
}
