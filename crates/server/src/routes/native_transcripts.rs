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
    ClaudeTranscriptIngest, NativeFeedChange, NativeFeedSnapshot, NativeFeedUpdate,
    UnassignedCliSession,
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

async fn send_change(
    socket: &mut MaybeSignedWebSocket,
    change: &NativeFeedChange,
) -> anyhow::Result<()> {
    socket
        .send(change_message(change)?.to_ws_message_unchecked())
        .await?;
    Ok(())
}

/// A full change replaces every top-level field. A delta adds appended
/// entries at their exact index and replaces changed ones in place, so a
/// client whose copy has drifted fails to apply it instead of silently
/// diverging; revision, seq and health are always replaced, and forks only
/// when they changed.
fn change_message(change: &NativeFeedChange) -> anyhow::Result<LogMsg> {
    let (revision, seq, appended_from, appended, replaced, forks, health) = match change {
        NativeFeedChange::Full(snapshot) => return snapshot_message(snapshot),
        NativeFeedChange::Delta {
            revision,
            seq,
            appended_from,
            appended,
            replaced,
            forks,
            health,
        } => (
            revision,
            seq,
            appended_from,
            appended,
            replaced,
            forks,
            health,
        ),
    };
    let mut ops = vec![
        serde_json::json!({ "op": "replace", "path": "/revision", "value": revision }),
        serde_json::json!({ "op": "replace", "path": "/seq", "value": seq }),
    ];
    for (index, entry) in replaced {
        ops.push(serde_json::json!({
            "op": "replace",
            "path": format!("/entries/{index}"),
            "value": entry,
        }));
    }
    for (offset, entry) in appended.iter().enumerate() {
        ops.push(serde_json::json!({
            "op": "add",
            "path": format!("/entries/{}", appended_from + offset),
            "value": entry,
        }));
    }
    if let Some(forks) = forks {
        ops.push(serde_json::json!({ "op": "replace", "path": "/forks", "value": forks }));
    }
    ops.push(serde_json::json!({ "op": "replace", "path": "/health", "value": health }));
    Ok(LogMsg::JsonPatch(serde_json::from_value(
        serde_json::Value::Array(ops),
    )?))
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
    // Keep the session's projection cached for incremental updates while
    // this socket lives.
    let _subscription = ingest.subscribe_feed(session_id);
    // Subscribe before taking the snapshot. Updates already represented by the
    // snapshot are skipped by seq; anything newer is queued in this receiver.
    let mut updates = ingest.subscribe();
    let (change, mut cursor) = ingest
        .feed_since(session_id, None)
        .await
        .map_err(anyhow::Error::from)?;
    let (mut last_seq, mut revision) = change_watermarks(&change);
    send_change(&mut socket, &change).await?;
    socket.send(LogMsg::Ready.to_ws_message_unchecked()).await?;

    loop {
        tokio::select! {
            update = updates.recv() => {
                let full = match update {
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
                        lagged || reason.is_some_and(|reason| reason.revision_changed)
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                // A native record can replace an earlier tool-use entry or
                // change fork membership. The projection reports those as
                // in-place replacements, and one message carries the whole
                // update, so it stays atomic at the WebSocket boundary.
                let (change, next_cursor) = ingest
                    .feed_since(session_id, (!full).then_some(cursor))
                    .await
                    .map_err(anyhow::Error::from)?;
                cursor = next_cursor;
                (last_seq, revision) = change_watermarks(&change);
                send_change(&mut socket, &change).await?;
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

fn change_watermarks(change: &NativeFeedChange) -> (i64, u64) {
    match change {
        NativeFeedChange::Full(snapshot) => (snapshot.seq, snapshot.revision),
        NativeFeedChange::Delta { seq, revision, .. } => (*seq, *revision),
    }
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
    use executors::logs::{NormalizedEntry, NormalizedEntryType};
    use services::services::claude_transcript_ingest::{
        NativeFeedEntry, NativeFeedFork, NativeFeedOrigin, NativeFileImportHealth, NativeForkView,
        NativeIngestHealth,
    };

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

    fn feed_entry(seq: i64, content: &str) -> NativeFeedEntry {
        NativeFeedEntry {
            normalized_entry: NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::AssistantMessage,
                content: content.to_string(),
                metadata: None,
            },
            claude_session_id: "fixture-sid".to_string(),
            uuid: Some(format!("uuid-{seq}")),
            parent_uuid: None,
            ts: None,
            origin: NativeFeedOrigin::Cli,
            linked_execution_process_id: None,
            git_branch: None,
            version: None,
            branch: None,
            seq,
        }
    }

    fn feed_document(snapshot: &NativeFeedSnapshot) -> serde_json::Value {
        serde_json::to_value(snapshot).unwrap()
    }

    /// Server-shaped messages and the document each must produce, shared with
    /// the web client's applicator test (`jsonPatch.test.ts`).
    #[test]
    fn feed_patches_match_the_web_client_fixture() {
        let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/web-core/src/shared/lib/__fixtures__/native-feed-patches.json");
        let health = |files| NativeIngestHealth {
            files,
            ..Default::default()
        };
        let file = NativeFileImportHealth {
            claude_session_id: "fixture-sid".to_string(),
            file_name: "fixture-sid.jsonl".to_string(),
            generation: 1,
            last_import_at: None,
        };
        let fork = NativeFeedFork {
            claude_session_id: "fixture-sid".to_string(),
            file_id: Uuid::nil(),
            fork: NativeForkView {
                fork_parent_uuid: "uuid-1".to_string(),
                prefix_uuids: vec!["uuid-1".to_string()],
                branches: Vec::new(),
                default_branch: None,
            },
        };

        let mut state = NativeFeedSnapshot {
            revision: 1,
            seq: 2,
            entries: vec![feed_entry(1, "tool: running"), feed_entry(2, "thinking")],
            forks: Vec::new(),
            health: health(vec![file.clone()]),
        };
        let mut steps = vec![(
            "snapshot",
            NativeFeedChange::Full(state.clone()),
            feed_document(&state),
        )];

        // A tool result replaces an entry mid-list while a new one appends.
        state.seq = 3;
        state.entries[0] = feed_entry(1, "tool: done");
        state.entries.push(feed_entry(3, "answer"));
        steps.push((
            "append with mid replace",
            NativeFeedChange::Delta {
                revision: 1,
                seq: 3,
                appended_from: 2,
                appended: vec![state.entries[2].clone()],
                replaced: vec![(0, state.entries[0].clone())],
                forks: None,
                health: state.health.clone(),
            },
            feed_document(&state),
        ));

        state.seq = 5;
        state.entries.push(feed_entry(4, "rewound prompt"));
        state.entries.push(feed_entry(5, "rewound answer"));
        state.forks = vec![fork];
        steps.push((
            "append with fork change",
            NativeFeedChange::Delta {
                revision: 1,
                seq: 5,
                appended_from: 3,
                appended: state.entries[3..].to_vec(),
                replaced: Vec::new(),
                forks: Some(state.forks.clone()),
                health: state.health.clone(),
            },
            feed_document(&state),
        ));

        let state = NativeFeedSnapshot {
            revision: 2,
            seq: 5,
            entries: vec![feed_entry(5, "rebuilt")],
            forks: Vec::new(),
            health: health(vec![file]),
        };
        steps.push((
            "revision reset",
            NativeFeedChange::Full(state.clone()),
            feed_document(&state),
        ));

        let fixture = serde_json::Value::Array(
            steps
                .into_iter()
                .map(|(name, change, expected)| {
                    let message = change_message(&change).unwrap().to_ws_message_unchecked();
                    let Message::Text(text) = message else {
                        panic!("feed messages are text frames");
                    };
                    serde_json::json!({
                        "name": name,
                        "message": serde_json::from_str::<serde_json::Value>(&text).unwrap(),
                        "expected": expected,
                    })
                })
                .collect(),
        );
        if std::env::var_os("UPDATE_NATIVE_FEED_FIXTURE").is_some() {
            let rendered = serde_json::to_string_pretty(&fixture).unwrap();
            std::fs::write(&fixture_path, format!("{rendered}\n")).unwrap();
        }
        let committed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&fixture_path).unwrap()).unwrap();
        assert_eq!(
            committed, fixture,
            "regenerate with UPDATE_NATIVE_FEED_FIXTURE=1"
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
