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
    ClaudeTranscriptIngest, NativeFeedSnapshot, NativeFeedUpdate, UnassignedCliSession,
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
    ApiError::FeatureDisabled("CLI transcript ingest is disabled".to_string())
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
                for message in disabled_feed_bootstrap().unwrap_or_default() {
                    if socket
                        .send(message.to_ws_message_unchecked())
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                // Close with an EXPLICIT 1000. A bare `close()` sends a close
                // frame with no status code, which browsers surface as
                // `CloseEvent.code == 1005`; the client's reconnect guard only
                // treats `code === 1000 && wasClean` as terminal, so an empty
                // code read as an unexpected drop and the tab reconnected
                // forever on the 8s backoff cap against a feature that is off.
                let _ = socket
                    .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: axum::extract::ws::close_code::NORMAL,
                        reason: "cli transcript ingest disabled".into(),
                    })))
                    .await;
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
    socket
        .send(snapshot_message(snapshot)?.to_ws_message_unchecked())
        .await?;
    Ok(())
}

fn snapshot_message(snapshot: &NativeFeedSnapshot) -> anyhow::Result<LogMsg> {
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
    Ok(LogMsg::JsonPatch(patch))
}

fn disabled_feed_bootstrap() -> anyhow::Result<Vec<LogMsg>> {
    let snapshot = NativeFeedSnapshot {
        revision: 0,
        seq: 0,
        entries: Vec::new(),
        forks: Vec::new(),
        health: Default::default(),
    };
    Ok(vec![snapshot_message(&snapshot)?, LogMsg::Ready])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResnapshotReason {
    sequence_gap: bool,
    revision_changed: bool,
}

fn resnapshot_reason(
    update: NativeFeedUpdate,
    session_id: Uuid,
    last_seq: i64,
    revision: u64,
) -> Option<ResnapshotReason> {
    match update {
        NativeFeedUpdate::RecordsAppended {
            session_id: update_session_id,
            seq,
            revision: update_revision,
        } => {
            if update_session_id != session_id
                || update_revision < revision
                || (update_revision == revision && seq <= last_seq)
            {
                return None;
            }
            Some(ResnapshotReason {
                sequence_gap: seq != last_seq + 1,
                revision_changed: update_revision > revision,
            })
        }
        NativeFeedUpdate::RevisionInvalidated {
            session_id: update_session_id,
            revision: update_revision,
        } => (update_session_id == session_id && update_revision > revision).then_some(
            ResnapshotReason {
                sequence_gap: false,
                revision_changed: true,
            },
        ),
    }
}

fn update_session_id(update: NativeFeedUpdate) -> Uuid {
    match update {
        NativeFeedUpdate::RecordsAppended { session_id, .. }
        | NativeFeedUpdate::RevisionInvalidated { session_id, .. } => session_id,
    }
}

fn drain_latest_update(
    first: NativeFeedUpdate,
    updates: &mut tokio::sync::broadcast::Receiver<NativeFeedUpdate>,
    session_id: Uuid,
) -> (Option<NativeFeedUpdate>, bool) {
    let mut latest = (update_session_id(first) == session_id).then_some(first);
    let mut lagged = false;
    loop {
        match updates.try_recv() {
            Ok(update) => {
                if update_session_id(update) == session_id {
                    latest = Some(update);
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                lagged = true;
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            | Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
        }
    }
    (latest, lagged)
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
                    Ok(update) => {
                        let (latest, lagged) =
                            drain_latest_update(update, &mut updates, session_id);
                        let reason = latest.and_then(|latest| {
                            resnapshot_reason(latest, session_id, last_seq, revision)
                        });
                        if !lagged && reason.is_none() {
                            continue;
                        }
                        if lagged
                            || reason.is_some_and(|reason| {
                                reason.sequence_gap || reason.revision_changed
                            })
                        {
                            tracing::debug!(
                                %session_id,
                                lagged,
                                sequence_gap = reason.is_some_and(|reason| reason.sequence_gap),
                                revision_changed = reason.is_some_and(|reason| reason.revision_changed),
                                "resnapshotting native transcript feed"
                            );
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
    let Some(ingest) = service(&deployment) else {
        return Ok(ResponseJson(ApiResponse::success(Vec::new())));
    };
    let sessions = ingest.list_unassigned(workspace_id).await?;
    Ok(ResponseJson(ApiResponse::success(sessions)))
}

async fn assign_unassigned(
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<AssignNativeCliSessionRequest>,
) -> Result<ResponseJson<ApiResponse<()>>, ApiError> {
    service(&deployment)
        .ok_or_else(disabled_error)?
        .assign_manual(&payload.claude_session_id, payload.session_id)
        .await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_feed_bootstraps_empty_snapshot_then_ready() {
        let messages = disabled_feed_bootstrap().unwrap();
        assert_eq!(messages.len(), 2);
        let LogMsg::JsonPatch(patch) = &messages[0] else {
            panic!("disabled feed must start with a snapshot patch");
        };
        assert!(matches!(messages[1], LogMsg::Ready));
        assert_eq!(
            serde_json::to_value(patch).unwrap(),
            serde_json::json!([
                { "op": "replace", "path": "/revision", "value": 0 },
                { "op": "replace", "path": "/seq", "value": 0 },
                { "op": "replace", "path": "/entries", "value": [] },
                { "op": "replace", "path": "/forks", "value": [] },
                {
                    "op": "replace",
                    "path": "/health",
                    "value": {
                        "unknown_kinds": 0,
                        "rescans": 0,
                        "quarantined_files": 0,
                        "watch_degraded": false,
                        "foreign_writer_seen_at": null,
                        "files": []
                    }
                }
            ])
        );
    }

    #[test]
    fn revision_invalidation_forces_resnapshot_without_sequence_advance() {
        let session_id = Uuid::new_v4();
        let reason = resnapshot_reason(
            NativeFeedUpdate::RevisionInvalidated {
                session_id,
                revision: 8,
            },
            session_id,
            42,
            7,
        )
        .expect("newer revision must invalidate a snapshot at the same sequence");

        assert!(!reason.sequence_gap);
        assert!(reason.revision_changed);
        assert!(
            resnapshot_reason(
                NativeFeedUpdate::RevisionInvalidated {
                    session_id,
                    revision: 8,
                },
                session_id,
                42,
                8,
            )
            .is_none()
        );
    }

    #[test]
    fn websocket_update_drain_keeps_only_latest_session_update() {
        let session_id = Uuid::new_v4();
        let other_session_id = Uuid::new_v4();
        let (sender, mut receiver) = tokio::sync::broadcast::channel(8);
        sender
            .send(NativeFeedUpdate::RecordsAppended {
                session_id,
                seq: 2,
                revision: 0,
            })
            .unwrap();
        sender
            .send(NativeFeedUpdate::RecordsAppended {
                session_id: other_session_id,
                seq: 99,
                revision: 0,
            })
            .unwrap();
        sender
            .send(NativeFeedUpdate::RecordsAppended {
                session_id,
                seq: 3,
                revision: 0,
            })
            .unwrap();

        let first = receiver.try_recv().unwrap();
        let (latest, lagged) = drain_latest_update(first, &mut receiver, session_id);
        assert!(!lagged);
        assert_eq!(
            latest,
            Some(NativeFeedUpdate::RecordsAppended {
                session_id,
                seq: 3,
                revision: 0,
            })
        );
        assert!(receiver.try_recv().is_err());
    }
}
