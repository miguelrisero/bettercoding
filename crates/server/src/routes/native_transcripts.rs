use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State, ws::Message},
    response::{IntoResponse, Json as ResponseJson},
    routing::{get, post},
};
use deployment::Deployment;
use serde::Deserialize;
use services::services::claude_transcript_ingest::{
    ClaudeTranscriptIngest, ClaudeTranscriptIngestError, NativeFeedSnapshot, UnassignedCliSession,
};
use ts_rs::TS;
use utils::{log_msg::LogMsg, response::ApiResponse};
use uuid::Uuid;

use crate::{
    DeploymentImpl,
    error::ApiError,
    middleware::signed_ws::{MaybeSignedWebSocket, SignedWsUpgrade},
};

#[derive(Debug, Deserialize, TS)]
pub struct AssignNativeCliSessionRequest {
    pub claude_session_id: String,
    pub session_id: Uuid,
}

fn service(deployment: &DeploymentImpl) -> Option<Arc<ClaudeTranscriptIngest>> {
    deployment.claude_transcript_ingest().cloned()
}

fn disabled_error() -> ApiError {
    ApiError::BadRequest("CLI transcript ingest is disabled".to_string())
}

fn map_ingest_error(error: ClaudeTranscriptIngestError) -> ApiError {
    match error {
        ClaudeTranscriptIngestError::Database(error) => ApiError::Database(error),
        ClaudeTranscriptIngestError::Io(error) => ApiError::Io(error),
        ClaudeTranscriptIngestError::Workspace(error) => ApiError::Workspace(error),
        ClaudeTranscriptIngestError::SessionNotFound(_) => {
            ApiError::Session(db::models::session::SessionError::NotFound)
        }
        ClaudeTranscriptIngestError::WorkspacePathMissing(workspace_id) => ApiError::BadRequest(
            format!("workspace {workspace_id} has no local path to map to Claude"),
        ),
        ClaudeTranscriptIngestError::NotQuarantined(claude_session_id) => ApiError::Conflict(
            format!("Claude session {claude_session_id} is not unassigned in this workspace"),
        ),
    }
}

async fn stream_native_feed_ws(
    ws: SignedWsUpgrade,
    State(deployment): State<DeploymentImpl>,
    Path(session_id): Path<Uuid>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let ingest = match service(&deployment) {
            Some(ingest) => ingest,
            None => {
                let mut socket = socket;
                let _ = socket
                    .send(LogMsg::Stderr(disabled_error().to_string()).to_ws_message_unchecked())
                    .await;
                let _ = socket.close().await;
                return;
            }
        };
        if let Err(error) = handle_native_feed_ws(socket, ingest, session_id).await {
            tracing::warn!(?error, %session_id, "native transcript feed WS closed");
        }
    })
}

async fn send_snapshot(
    socket: &mut MaybeSignedWebSocket,
    snapshot: &NativeFeedSnapshot,
) -> anyhow::Result<()> {
    // Replace each top-level field instead of the JSON document root. The
    // existing web hook applies patches to an Immer draft in place, so root
    // replacement would discard the replacement value returned by RFC 6902.
    let patch = serde_json::from_value(serde_json::json!([
        { "op": "replace", "path": "/revision", "value": snapshot.revision },
        { "op": "replace", "path": "/seq", "value": snapshot.seq },
        { "op": "replace", "path": "/entries", "value": snapshot.entries },
        { "op": "replace", "path": "/forks", "value": snapshot.forks },
        { "op": "replace", "path": "/health", "value": snapshot.health },
    ]))?;
    socket
        .send(LogMsg::JsonPatch(patch).to_ws_message_unchecked())
        .await?;
    Ok(())
}

async fn handle_native_feed_ws(
    mut socket: MaybeSignedWebSocket,
    ingest: Arc<ClaudeTranscriptIngest>,
    session_id: Uuid,
) -> anyhow::Result<()> {
    // Subscribe before taking the snapshot. Updates already represented by the
    // snapshot are skipped by seq; anything newer is queued in this receiver.
    let mut updates = ingest.subscribe();
    let mut snapshot = ingest
        .snapshot(session_id)
        .await
        .map_err(anyhow::Error::from)?;
    let mut last_seq = snapshot.seq;
    let mut revision = snapshot.revision;
    send_snapshot(&mut socket, &snapshot).await?;
    socket.send(LogMsg::Ready.to_ws_message_unchecked()).await?;

    loop {
        tokio::select! {
            update = updates.recv() => {
                match update {
                    Ok(update) if update.session_id != session_id => continue,
                    Ok(update) if update.seq <= last_seq => continue,
                    Ok(update) => {
                        let gap = update.seq != last_seq + 1;
                        let generation_changed = update.revision != revision;
                        if gap || generation_changed {
                            tracing::debug!(%session_id, gap, generation_changed, "resnapshotting native transcript feed");
                        }
                        // A native record can replace an earlier tool-use entry
                        // or change fork membership. Rebuilding the canonical
                        // projection and replacing one snapshot keeps that
                        // update atomic at the WebSocket-message boundary.
                        snapshot = ingest
                            .snapshot(session_id)
                            .await
                            .map_err(anyhow::Error::from)?;
                        last_seq = snapshot.seq;
                        revision = snapshot.revision;
                        send_snapshot(&mut socket, &snapshot).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        snapshot = ingest
                            .snapshot(session_id)
                            .await
                            .map_err(anyhow::Error::from)?;
                        last_seq = snapshot.seq;
                        revision = snapshot.revision;
                        send_snapshot(&mut socket, &snapshot).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            inbound = socket.recv() => {
                match inbound {
                    Ok(Some(Message::Close(_))) | Ok(None) | Err(_) => break,
                    Ok(Some(_)) => {}
                }
            }
        }
    }
    let _ = socket.close().await;
    Ok(())
}

async fn get_unassigned(
    State(deployment): State<DeploymentImpl>,
    Path(workspace_id): Path<Uuid>,
) -> Result<ResponseJson<ApiResponse<Vec<UnassignedCliSession>>>, ApiError> {
    let sessions = service(&deployment)
        .ok_or_else(disabled_error)?
        .list_unassigned(workspace_id)
        .await
        .map_err(map_ingest_error)?;
    Ok(ResponseJson(ApiResponse::success(sessions)))
}

async fn assign_unassigned(
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<AssignNativeCliSessionRequest>,
) -> Result<ResponseJson<ApiResponse<()>>, ApiError> {
    service(&deployment)
        .ok_or_else(disabled_error)?
        .assign_manual(&payload.claude_session_id, payload.session_id)
        .await
        .map_err(map_ingest_error)?;
    Ok(ResponseJson(ApiResponse::success(())))
}

pub fn router() -> Router<DeploymentImpl> {
    Router::new()
        .route(
            "/sessions/{session_id}/native-feed/ws",
            get(stream_native_feed_ws),
        )
        .route(
            "/workspaces/{workspace_id}/native-cli-sessions/unassigned",
            get(get_unassigned),
        )
        .route("/native-cli-sessions/assign", post(assign_unassigned))
}
