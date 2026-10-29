use db::models::{execution_process::ExecutionProcess, scratch::Scratch, workspace::Workspace};
use futures::StreamExt;
use serde_json::json;
use tokio_stream::wrappers::BroadcastStream;
use utils::log_msg::LogMsg;
use uuid::Uuid;

use super::{
    EventService,
    patches::execution_process_patch,
    types::{EventPatch, RecordTypes},
};

impl EventService {
    /// Stream execution processes for a specific session with initial snapshot (raw LogMsg format for WebSocket)
    pub async fn stream_execution_processes_for_session_raw(
        &self,
        session_id: Uuid,
        show_soft_deleted: bool,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<LogMsg, std::io::Error>>,
        super::types::EventError,
    > {
        // Get execution processes for this session
        let processes =
            ExecutionProcess::find_by_session_id(&self.db.pool, session_id, show_soft_deleted)
                .await?;

        // Convert processes array to object keyed by process ID
        let processes_map: serde_json::Map<String, serde_json::Value> = processes
            .into_iter()
            .map(|process| {
                (
                    process.id.to_string(),
                    serde_json::to_value(process).unwrap(),
                )
            })
            .collect();

        let initial_patch = json!([{
            "op": "replace",
            "path": "/execution_processes",
            "value": processes_map
        }]);
        let initial_msg = LogMsg::JsonPatch(serde_json::from_value(initial_patch).unwrap());

        // Get filtered event stream
        let filtered_stream =
            BroadcastStream::new(self.msg_store.get_receiver()).filter_map(move |msg_result| {
                async move {
                    match msg_result {
                        Ok(LogMsg::JsonPatch(patch)) => {
                            // Filter events based on session_id
                            if let Some(patch_op) = patch.0.first() {
                                // Check if this is a modern execution process patch
                                if patch_op.path().starts_with("/execution_processes/") {
                                    match patch_op {
                                        json_patch::PatchOperation::Add(op) => {
                                            // Parse execution process data directly from value
                                            if let Ok(process) =
                                                serde_json::from_value::<ExecutionProcess>(
                                                    op.value.clone(),
                                                )
                                                && process.session_id == session_id
                                            {
                                                if !show_soft_deleted && process.dropped {
                                                    let remove_patch =
                                                        execution_process_patch::remove(process.id);
                                                    return Some(Ok(LogMsg::JsonPatch(
                                                        remove_patch,
                                                    )));
                                                }
                                                return Some(Ok(LogMsg::JsonPatch(patch)));
                                            }
                                        }
                                        json_patch::PatchOperation::Replace(op) => {
                                            // Parse execution process data directly from value
                                            if let Ok(process) =
                                                serde_json::from_value::<ExecutionProcess>(
                                                    op.value.clone(),
                                                )
                                                && process.session_id == session_id
                                            {
                                                if !show_soft_deleted && process.dropped {
                                                    let remove_patch =
                                                        execution_process_patch::remove(process.id);
                                                    return Some(Ok(LogMsg::JsonPatch(
                                                        remove_patch,
                                                    )));
                                                }
                                                return Some(Ok(LogMsg::JsonPatch(patch)));
                                            }
                                        }
                                        json_patch::PatchOperation::Remove(_) => {
                                            // For remove operations, we can't verify session_id
                                            // so we allow all removals and let the client handle filtering
                                            return Some(Ok(LogMsg::JsonPatch(patch)));
                                        }
                                        _ => {}
                                    }
                                }
                                // Fallback to legacy EventPatch format for backward compatibility
                                else if let Ok(event_patch_value) = serde_json::to_value(patch_op)
                                    && let Ok(event_patch) =
                                        serde_json::from_value::<EventPatch>(event_patch_value)
                                {
                                    match &event_patch.value.record {
                                        RecordTypes::ExecutionProcess(process) => {
                                            if process.session_id == session_id {
                                                if !show_soft_deleted && process.dropped {
                                                    let remove_patch =
                                                        execution_process_patch::remove(process.id);
                                                    return Some(Ok(LogMsg::JsonPatch(
                                                        remove_patch,
                                                    )));
                                                }
                                                return Some(Ok(LogMsg::JsonPatch(patch)));
                                            }
                                        }
                                        RecordTypes::DeletedExecutionProcess {
                                            session_id: Some(deleted_session_id),
                                            ..
                                        } => {
                                            if *deleted_session_id == session_id {
                                                return Some(Ok(LogMsg::JsonPatch(patch)));
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            None
                        }
                        Ok(other) => Some(Ok(other)), // Pass through non-patch messages
                        Err(_) => None,               // Filter out broadcast errors
                    }
                }
            });

        // Start with initial snapshot, Ready signal, then live updates
        let initial_stream = futures::stream::iter(vec![Ok(initial_msg), Ok(LogMsg::Ready)]);
        let combined_stream = initial_stream.chain(filtered_stream).boxed();

        Ok(combined_stream)
    }

    /// Stream a single scratch item with initial snapshot (raw LogMsg format for WebSocket)
    pub async fn stream_scratch_raw(
        &self,
        scratch_id: Uuid,
        scratch_type: &db::models::scratch::ScratchType,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<LogMsg, std::io::Error>>,
        super::types::EventError,
    > {
        // Subscribe before loading the snapshot so a concurrent report write is retained.
        let receiver = self.msg_store.get_receiver();
        let reports = *scratch_type == db::models::scratch::ScratchType::WorkspaceReports;
        // Treat errors (e.g., corrupted/malformed data) the same as "scratch not found"
        // This prevents the websocket from closing and retrying indefinitely
        let scratch = match Scratch::find_by_id(&self.db.pool, scratch_id, scratch_type).await {
            Ok(scratch) => scratch,
            Err(e) => {
                tracing::warn!(
                    scratch_id = %scratch_id,
                    scratch_type = %scratch_type,
                    error = %e,
                    "Failed to load scratch, treating as empty"
                );
                None
            }
        };

        let initial_patch = json!([{
            "op": "replace",
            "path": "/scratch",
            "value": scratch
        }]);
        let initial_msg = LogMsg::JsonPatch(serde_json::from_value(initial_patch).unwrap());

        let type_str = scratch_type.to_string();
        let mut report_timestamp = scratch.as_ref().and_then(|scratch| {
            if let db::models::scratch::ScratchPayload::WorkspaceReports(state) = &scratch.payload {
                state.updated_at
            } else {
                None
            }
        });

        // Filter to only this scratch's events by matching id and payload.type in the patch value.
        let filtered_stream = BroadcastStream::new(receiver).filter_map(move |msg_result| {
            let id_str = scratch_id.to_string();
            let type_str = type_str.clone();
            if reports && let Ok(LogMsg::JsonPatch(patch)) = &msg_result {
                let value = patch.0.first().and_then(|op| match op {
                    json_patch::PatchOperation::Add(op) => Some(&op.value),
                    json_patch::PatchOperation::Replace(op) => Some(&op.value),
                    _ => None,
                });
                if let Some(value) = value.filter(|value| {
                    value["id"].as_str() == Some(&id_str)
                        && value["payload"]["type"].as_str() == Some(&type_str)
                }) {
                    let timestamp = value["payload"]["data"]["updated_at"]
                        .as_str()
                        .and_then(|stamp| stamp.parse::<chrono::DateTime<chrono::Utc>>().ok());
                    if timestamp <= report_timestamp {
                        return futures::future::ready(None);
                    }
                    report_timestamp = timestamp;
                }
            }
            futures::future::ready(match msg_result {
                Ok(LogMsg::JsonPatch(patch)) => {
                    if let Some(op) = patch.0.first()
                        && op.path() == "/scratch"
                    {
                        // Extract id and payload.type from the patch value
                        let value = match op {
                            json_patch::PatchOperation::Add(a) => Some(&a.value),
                            json_patch::PatchOperation::Replace(r) => Some(&r.value),
                            json_patch::PatchOperation::Remove(_) => None,
                            _ => None,
                        };

                        let matches = value.is_some_and(|v| {
                            let id_matches = v.get("id").and_then(|v| v.as_str()) == Some(&id_str);
                            let type_matches = v
                                .get("payload")
                                .and_then(|p| p.get("type"))
                                .and_then(|t| t.as_str())
                                == Some(&type_str);
                            id_matches && type_matches
                        });

                        if matches {
                            return futures::future::ready(Some(Ok(LogMsg::JsonPatch(patch))));
                        }
                    }
                    None
                }
                Ok(other) => Some(Ok(other)),
                // Report clients reconnect for a complete snapshot after lag.
                Err(error) if reports => Some(Err(std::io::Error::other(error))),
                Err(_) => None,
            })
        });

        let initial_stream = futures::stream::iter(vec![Ok(initial_msg), Ok(LogMsg::Ready)]);
        let combined_stream = initial_stream.chain(filtered_stream).boxed();
        Ok(combined_stream)
    }

    pub async fn stream_workspaces_raw(
        &self,
        archived: Option<bool>,
        limit: Option<i64>,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<LogMsg, std::io::Error>>,
        super::types::EventError,
    > {
        let workspaces = Workspace::find_all_with_status(&self.db.pool, archived, limit).await?;
        let workspaces_map: serde_json::Map<String, serde_json::Value> = workspaces
            .into_iter()
            .map(|ws| (ws.id.to_string(), serde_json::to_value(ws).unwrap()))
            .collect();

        let initial_patch = json!([{
            "op": "replace",
            "path": "/workspaces",
            "value": workspaces_map
        }]);
        let initial_msg = LogMsg::JsonPatch(serde_json::from_value(initial_patch).unwrap());

        let filtered_stream = BroadcastStream::new(self.msg_store.get_receiver()).filter_map(
            move |msg_result| async move {
                match msg_result {
                    Ok(LogMsg::JsonPatch(patch)) => {
                        if let Some(op) = patch.0.first()
                            && op.path().starts_with("/workspaces")
                        {
                            // If archived filter is set, handle state transitions
                            if let Some(archived_filter) = archived {
                                // Extract workspace data from Add/Replace operations
                                let value = match op {
                                    json_patch::PatchOperation::Add(a) => Some(&a.value),
                                    json_patch::PatchOperation::Replace(r) => Some(&r.value),
                                    json_patch::PatchOperation::Remove(_) => {
                                        // Allow remove operations through - client will handle
                                        return Some(Ok(LogMsg::JsonPatch(patch)));
                                    }
                                    _ => None,
                                };

                                if let Some(v) = value
                                    && let Some(ws_archived) =
                                        v.get("archived").and_then(|a| a.as_bool())
                                {
                                    if ws_archived == archived_filter {
                                        // Workspace matches this filter
                                        // Convert Replace to Add since workspace may be new to this filtered stream
                                        if let json_patch::PatchOperation::Replace(r) = op {
                                            let add_patch = json_patch::Patch(vec![
                                                json_patch::PatchOperation::Add(
                                                    json_patch::AddOperation {
                                                        path: r.path.clone(),
                                                        value: r.value.clone(),
                                                    },
                                                ),
                                            ]);
                                            return Some(Ok(LogMsg::JsonPatch(add_patch)));
                                        }
                                        return Some(Ok(LogMsg::JsonPatch(patch)));
                                    } else {
                                        // Workspace no longer matches this filter - send remove
                                        let remove_patch = json_patch::Patch(vec![
                                            json_patch::PatchOperation::Remove(
                                                json_patch::RemoveOperation {
                                                    path: op
                                                        .path()
                                                        .to_string()
                                                        .try_into()
                                                        .expect("Workspace path should be valid"),
                                                },
                                            ),
                                        ]);
                                        return Some(Ok(LogMsg::JsonPatch(remove_patch)));
                                    }
                                }
                            }
                            return Some(Ok(LogMsg::JsonPatch(patch)));
                        }
                        None
                    }
                    Ok(other) => Some(Ok(other)),
                    Err(_) => None,
                }
            },
        );

        let initial_stream = futures::stream::iter(vec![Ok(initial_msg), Ok(LogMsg::Ready)]);
        Ok(initial_stream.chain(filtered_stream).boxed())
    }
}

#[cfg(test)]
mod report_tests {
    use std::sync::Arc;

    use db::{
        DBService,
        models::{
            scratch::{Scratch, ScratchPayload, ScratchType},
            workspace_report::{SetReportTab, WorkspaceReport},
        },
    };
    use futures::StreamExt;
    use tokio::sync::RwLock;
    use utils::{log_msg::LogMsg, msg_store::MsgStore};
    use uuid::Uuid;

    use crate::services::events::{EventService, scratch_patch};

    #[tokio::test]
    async fn committed_report_publication_survives_precommit_wal_hook_read() {
        use db::models::workspace_report::ReportTabs;
        use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
        let scratch = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let directory = tempfile::tempdir_in(scratch).unwrap();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(3)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(directory.path().join("reports.sqlite"))
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal),
            )
            .await
            .unwrap();
        sqlx::raw_sql(include_str!(
            "../../../../db/migrations/20251120000001_refactor_to_scratch.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let workspace = Uuid::new_v4();
        WorkspaceReport::set(
            &pool,
            workspace,
            "build",
            SetReportTab {
                title: "Before".into(),
                columns: vec!["State".into()],
                rows: vec![],
                expected_updated_at: None,
            },
        )
        .await
        .unwrap();
        let first = Scratch::find_by_id(&pool, workspace, &ScratchType::WorkspaceReports)
            .await
            .unwrap()
            .unwrap();
        let ScratchPayload::WorkspaceReports(mut state) = first.payload.clone() else {
            panic!("expected reports")
        };
        let store = Arc::new(MsgStore::new());
        let db = DBService { pool: pool.clone() };
        let count = Arc::new(RwLock::new(0));
        let events = EventService::new(db.clone(), store.clone(), count.clone());
        let mut stream = events
            .stream_scratch_raw(workspace, &ScratchType::WorkspaceReports)
            .await
            .unwrap();
        stream.next().await.unwrap().unwrap();
        stream.next().await.unwrap().unwrap();
        let mut hook_events = store.get_receiver();
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        EventService::create_hook(store.clone(), count, db)(&mut tx)
            .await
            .unwrap();
        let stamp = first.updated_at + chrono::Duration::seconds(1);
        state.tabs[0].title = "Committed".into();
        state.tabs[0].updated_at = stamp;
        state.updated_at = Some(stamp);
        sqlx::query("UPDATE scratch SET payload=?,updated_at=? WHERE id=? AND scratch_type='WORKSPACE_REPORTS'")
            .bind(serde_json::to_string(&ScratchPayload::WorkspaceReports(state.clone())).unwrap()).bind(stamp).bind(workspace).execute(&mut *tx).await.unwrap();
        // Force the real update hook's separate WAL reader to finish before COMMIT.
        let hook = tokio::time::timeout(std::time::Duration::from_secs(2), hook_events.recv())
            .await
            .unwrap()
            .unwrap();
        let LogMsg::JsonPatch(patch) = hook else {
            panic!("expected hook patch")
        };
        let json_patch::PatchOperation::Replace(op) = &patch.0[0] else {
            panic!("expected replacement")
        };
        assert_eq!(op.value["payload"]["data"]["tabs"][0]["title"], "Before");
        tx.commit().await.unwrap();
        events.publish_workspace_reports(workspace).await.unwrap();
        let LogMsg::JsonPatch(patch) =
            tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
        else {
            panic!("expected committed patch")
        };
        let json_patch::PatchOperation::Replace(op) = &patch.0[0] else {
            panic!("expected replacement")
        };
        let committed: ReportTabs =
            serde_json::from_value(op.value["payload"]["data"].clone()).unwrap();
        assert_eq!(committed, state);
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        EventService::create_hook(
            store.clone(),
            Arc::new(RwLock::new(0)),
            DBService { pool: pool.clone() },
        )(&mut tx)
        .await
        .unwrap();
        state.tabs[0].title = "Rolled back".into();
        state.tabs[0].updated_at = stamp + chrono::Duration::seconds(1);
        state.updated_at = Some(state.tabs[0].updated_at);
        sqlx::query("UPDATE scratch SET payload=? WHERE id=? AND scratch_type='WORKSPACE_REPORTS'")
            .bind(serde_json::to_string(&ScratchPayload::WorkspaceReports(state)).unwrap())
            .bind(workspace)
            .execute(&mut *tx)
            .await
            .unwrap();
        // The rolled-back update hook can publish only the committed snapshot.
        hook_events.recv().await.unwrap(); // Drain the explicit committed publication.
        let hook = tokio::time::timeout(std::time::Duration::from_secs(2), hook_events.recv())
            .await
            .unwrap()
            .unwrap();
        let LogMsg::JsonPatch(patch) = hook else {
            panic!("expected hook patch")
        };
        let json_patch::PatchOperation::Replace(op) = &patch.0[0] else {
            panic!("expected replacement")
        };
        assert_eq!(op.value["payload"]["data"]["tabs"][0]["title"], "Committed");
        tx.rollback().await.unwrap();
        assert_eq!(
            WorkspaceReport::list(&pool, workspace).await.unwrap(),
            committed
        );
        // Ready is a marker after all queued hooks: stale payloads must be filtered.
        store.push(LogMsg::Ready);
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            LogMsg::Ready
        ));
        pool.close().await;
    }

    #[tokio::test]
    async fn report_stream_snapshot_filters_scope_and_stale_events() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!(
            "../../../../db/migrations/20251120000001_refactor_to_scratch.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let workspace = Uuid::new_v4();
        let body = SetReportTab {
            title: "First".into(),
            columns: vec!["State".into()],
            rows: vec![],
            expected_updated_at: None,
        };
        WorkspaceReport::set(&pool, workspace, "build", body.clone())
            .await
            .unwrap();
        let first = Scratch::find_by_id(&pool, workspace, &ScratchType::WorkspaceReports)
            .await
            .unwrap()
            .unwrap();
        let store = Arc::new(MsgStore::new());
        let events = EventService::new(
            DBService { pool: pool.clone() },
            store.clone(),
            Arc::new(RwLock::new(0)),
        );
        let mut stream = events
            .stream_scratch_raw(workspace, &ScratchType::WorkspaceReports)
            .await
            .unwrap();
        assert!(matches!(
            stream.next().await.unwrap().unwrap(),
            LogMsg::JsonPatch(_)
        ));
        assert!(matches!(
            stream.next().await.unwrap().unwrap(),
            LogMsg::Ready
        ));
        WorkspaceReport::set(
            &pool,
            workspace,
            "build",
            SetReportTab {
                title: "Second".into(),
                ..body
            },
        )
        .await
        .unwrap();
        let latest = Scratch::find_by_id(&pool, workspace, &ScratchType::WorkspaceReports)
            .await
            .unwrap()
            .unwrap();
        let mut foreign = latest.clone();
        foreign.id = Uuid::new_v4();
        store.push_patch(scratch_patch::replace(&foreign));
        store.push_patch(scratch_patch::replace(&first));
        store.push_patch(scratch_patch::replace(&latest));
        let LogMsg::JsonPatch(patch) =
            tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
        else {
            panic!("expected report patch")
        };
        let json_patch::PatchOperation::Replace(operation) = &patch.0[0] else {
            panic!("expected replacement")
        };
        assert_eq!(operation.path, "/scratch");
        let decoded: Scratch = serde_json::from_value(operation.value.clone()).unwrap();
        let ScratchPayload::WorkspaceReports(state) = decoded.payload else {
            panic!("expected reports")
        };
        assert_eq!(state.tabs[0].title, "Second");
        let mut reconnected = events
            .stream_scratch_raw(workspace, &ScratchType::WorkspaceReports)
            .await
            .unwrap();
        let LogMsg::JsonPatch(patch) = reconnected.next().await.unwrap().unwrap() else {
            panic!("expected snapshot")
        };
        let json_patch::PatchOperation::Replace(operation) = &patch.0[0] else {
            panic!("expected replacement")
        };
        assert_eq!(
            operation.value["payload"]["data"]["tabs"][0]["title"],
            "Second"
        );
    }
}
