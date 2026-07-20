use std::{collections::HashSet, fs, path::Path, sync::Arc, time::Duration};

use db::{
    DBService,
    models::{
        claude_session_link::{ClaudeSessionBoundVia, ClaudeSessionLink},
        cli_ingest_outbox::CliIngestOutbox,
        cli_native_file::CliNativeFile,
        cli_native_record::CliNativeRecord,
        coding_agent_turn::{CodingAgentTurn, CreateCodingAgentTurn},
        execution_native_link::ExecutionNativeLink,
        execution_process::{CreateExecutionProcess, ExecutionProcess, ExecutionProcessRunReason},
        session::{CreateSession, Session},
        workspace::{CreateWorkspace, Workspace},
    },
};
use executors::{
    actions::{
        ExecutorAction, ExecutorActionType, coding_agent_initial::CodingAgentInitialRequest,
    },
    executors::BaseCodingAgent,
    profile::ExecutorConfig,
};
use sqlx::sqlite::SqlitePoolOptions;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    ClaudeTranscriptIngest, NativeFeedOrigin, NativeFeedUpdate, claude_project_slug, effective_cwd,
};

const FIXTURE_SID: &str = "06a7eacd-664b-4d9c-83f3-d4774a6216a8";

async fn test_db() -> DBService {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    db::run_migrations_for_tests(&pool).await.unwrap();
    DBService { pool }
}

async fn create_workspace_and_session(
    db: &DBService,
    workspace_root: &Path,
) -> (Workspace, Session) {
    let workspace_id = Uuid::new_v4();
    Workspace::create(
        &db.pool,
        &CreateWorkspace {
            branch: "main".to_string(),
            name: Some("ingest test".to_string()),
        },
        workspace_id,
    )
    .await
    .unwrap();
    Workspace::update_container_ref(&db.pool, workspace_id, &workspace_root.to_string_lossy())
        .await
        .unwrap();
    let workspace = Workspace::find_by_id(&db.pool, workspace_id)
        .await
        .unwrap()
        .unwrap();
    let session = Session::create(
        &db.pool,
        &CreateSession {
            executor: Some("CLAUDE_CODE".to_string()),
            name: None,
        },
        Uuid::new_v4(),
        workspace_id,
    )
    .await
    .unwrap();
    (workspace, session)
}

fn fixture_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(
        "../../docs/superpowers/specs/evidence/2026-07-20-cli-ui-seam/evidence-transcript.redacted.jsonl",
    )
}

fn store_dir(projects_dir: &Path, cwd: &Path) -> std::path::PathBuf {
    projects_dir.join(claude_project_slug(cwd))
}

fn native_user_record(sid: &str, uuid: &str, text: &str, timestamp: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "type": "user",
            "sessionId": sid,
            "uuid": uuid,
            "timestamp": timestamp,
            "message": { "role": "user", "content": text }
        })
    )
}

#[cfg(unix)]
#[tokio::test]
async fn read_only_fixture_backfill_is_complete_and_idempotent() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    fs::create_dir_all(&projects_dir).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let native_file = native_dir.join(format!("{FIXTURE_SID}.jsonl"));
    fs::copy(fixture_path(), &native_file).unwrap();
    ClaudeSessionLink::assign_manual(&db.pool, FIXTURE_SID, session.id, &cwd.to_string_lossy())
        .await
        .unwrap()
        .unwrap();

    fs::set_permissions(&native_file, fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&native_dir, fs::Permissions::from_mode(0o555)).unwrap();
    let dir_mtime = fs::metadata(&native_dir).unwrap().modified().unwrap();
    let file_mtime = fs::metadata(&native_file).unwrap().modified().unwrap();
    let names_before = fs::read_dir(&native_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<HashSet<_>>();

    let service = Arc::new(ClaudeTranscriptIngest::new(db.clone(), projects_dir));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();
    let files = CliNativeFile::list_latest_by_sid(&db.pool, FIXTURE_SID)
        .await
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(
        CliNativeRecord::count_for_file(&db.pool, files[0].id)
            .await
            .unwrap(),
        94
    );
    assert_eq!(
        CliIngestOutbox::latest_seq(&db.pool, session.id)
            .await
            .unwrap(),
        94
    );

    // The same full discovery/backfill path is idempotent.
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        CliNativeRecord::count_for_file(&db.pool, files[0].id)
            .await
            .unwrap(),
        94
    );
    assert_eq!(
        CliIngestOutbox::latest_seq(&db.pool, session.id)
            .await
            .unwrap(),
        94
    );
    let snapshot = service.snapshot(session.id).await.unwrap();
    assert_eq!(snapshot.forks.len(), 1);
    assert!(snapshot.entries.iter().any(|entry| {
        matches!(entry.origin, NativeFeedOrigin::Cli)
            && matches!(
                entry.normalized_entry.entry_type,
                executors::logs::NormalizedEntryType::UserMessage
            )
    }));

    let names_after = fs::read_dir(&native_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<HashSet<_>>();
    assert_eq!(names_after, names_before);
    assert_eq!(
        fs::metadata(&native_dir).unwrap().modified().unwrap(),
        dir_mtime
    );
    assert_eq!(
        fs::metadata(&native_file).unwrap().modified().unwrap(),
        file_mtime
    );

    // Restore permissions only after all zero-write assertions so TempDir can
    // clean itself up on platforms that require directory write permission.
    fs::set_permissions(&native_dir, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&native_file, fs::Permissions::from_mode(0o644)).unwrap();
}

#[tokio::test]
async fn unmatched_sid_is_quarantined_until_manual_assignment() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let sid = "11111111-1111-4111-8111-111111111111";
    let native_file = native_dir.join(format!("{sid}.jsonl"));
    fs::write(
        &native_file,
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"{sid}\",\"uuid\":\"cli-user\",\"timestamp\":\"2026-07-20T20:00:00Z\",\"message\":{{\"role\":\"user\",\"content\":\"from cli\"}}}}\n"
        ),
    )
    .unwrap();

    let service = Arc::new(ClaudeTranscriptIngest::new(db.clone(), projects_dir));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();
    assert!(
        ClaudeSessionLink::find(&db.pool, sid)
            .await
            .unwrap()
            .is_none()
    );
    let files = CliNativeFile::list_latest_by_sid(&db.pool, sid)
        .await
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(
        CliNativeRecord::count_for_file(&db.pool, files[0].id)
            .await
            .unwrap(),
        0
    );
    let unassigned = service.list_unassigned(workspace.id).await.unwrap();
    assert_eq!(unassigned.len(), 1);
    assert_eq!(unassigned[0].claude_session_id, sid);
    assert_eq!(
        unassigned[0].first_prompt_snippet.as_deref(),
        Some("from cli")
    );

    service.assign_manual(sid, session.id).await.unwrap();
    let link = ClaudeSessionLink::find(&db.pool, sid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link.bound_via, ClaudeSessionBoundVia::Manual);
    assert_eq!(
        CliNativeRecord::count_for_file(&db.pool, files[0].id)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn executor_sid_links_and_origins_use_durable_identity() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;

    let process_id = Uuid::new_v4();
    let action = ExecutorAction::new(
        ExecutorActionType::CodingAgentInitialRequest(CodingAgentInitialRequest {
            prompt: "app prompt".to_string(),
            executor_config: ExecutorConfig::new(BaseCodingAgent::ClaudeCode),
            working_dir: None,
        }),
        None,
    );
    ExecutionProcess::create(
        &db.pool,
        &CreateExecutionProcess {
            session_id: session.id,
            executor_action: action,
            run_reason: ExecutionProcessRunReason::CodingAgent,
        },
        process_id,
        &[],
    )
    .await
    .unwrap();
    CodingAgentTurn::create(
        &db.pool,
        &CreateCodingAgentTurn {
            execution_process_id: process_id,
            prompt: Some("app prompt".to_string()),
        },
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let sid = "22222222-2222-4222-8222-222222222222";
    CodingAgentTurn::update_agent_session_id(&db.pool, process_id, sid)
        .await
        .unwrap();
    ExecutionNativeLink::insert(&db.pool, process_id, "executor-assistant")
        .await
        .unwrap();

    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let native_file = native_dir.join(format!("{sid}.jsonl"));
    let user = serde_json::json!({
        "type": "user",
        "sessionId": sid,
        "uuid": "app-user",
        "timestamp": "2099-07-20T20:00:00Z",
        "message": { "role": "user", "content": "app prompt" }
    });
    let assistant = serde_json::json!({
        "type": "assistant",
        "sessionId": sid,
        "uuid": "executor-assistant",
        "parentUuid": "app-user",
        "timestamp": "2099-07-20T20:00:01Z",
        "message": { "role": "assistant", "content": [{"type": "text", "text": "done"}] }
    });
    fs::write(&native_file, format!("{user}\n{assistant}\n")).unwrap();

    let service = Arc::new(ClaudeTranscriptIngest::new(db.clone(), projects_dir));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();
    let link = ClaudeSessionLink::find(&db.pool, sid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link.bound_via, ClaudeSessionBoundVia::Executor);
    assert_eq!(link.session_id, session.id);

    let rows = CliNativeRecord::list_for_session(&db.pool, session.id)
        .await
        .unwrap();
    let user_row = rows
        .iter()
        .find(|row| row.uuid.as_deref() == Some("app-user"))
        .unwrap();
    assert!(user_row.bound_coding_agent_turn_id.is_some());
    let assistant_row = rows
        .iter()
        .find(|row| row.uuid.as_deref() == Some("executor-assistant"))
        .unwrap();
    assert_eq!(assistant_row.linked_execution_process_id, Some(process_id));

    let snapshot = service.snapshot(session.id).await.unwrap();
    assert!(snapshot.entries.iter().any(|entry| {
        entry.uuid.as_deref() == Some("app-user") && entry.origin == NativeFeedOrigin::App
    }));
    assert!(snapshot.entries.iter().any(|entry| {
        entry.uuid.as_deref() == Some("executor-assistant")
            && entry.origin == NativeFeedOrigin::Executor
            && entry.linked_execution_process_id == Some(process_id)
    }));
}

#[tokio::test]
async fn truncation_without_complete_line_invalidates_connected_subscriber() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let sid = "33333333-3333-4333-8333-333333333333";
    let native_file = native_dir.join(format!("{sid}.jsonl"));
    fs::write(
        &native_file,
        native_user_record(sid, "before-truncate", "stale", "2026-07-20T20:00:00Z"),
    )
    .unwrap();
    ClaudeSessionLink::assign_manual(&db.pool, sid, session.id, &cwd.to_string_lossy())
        .await
        .unwrap()
        .unwrap();

    let service = Arc::new(ClaudeTranscriptIngest::new(db.clone(), projects_dir));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();
    let before = service.snapshot(session.id).await.unwrap();
    assert_eq!(before.entries.len(), 1);

    let mut updates = service.subscribe();
    fs::write(&native_file, br#"{"type":"user"#).unwrap();
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();

    let update = tokio::time::timeout(Duration::from_secs(5), updates.recv())
        .await
        .expect("connected subscriber was not invalidated")
        .unwrap();
    let invalidated_revision = match update {
        NativeFeedUpdate::RevisionInvalidated {
            session_id,
            revision,
        } => {
            assert_eq!(session_id, session.id);
            revision
        }
        NativeFeedUpdate::RecordsAppended { .. } => {
            panic!("truncation without a complete line must not require an outbox row")
        }
    };

    let resnapshot = service.snapshot(session.id).await.unwrap();
    assert_eq!(resnapshot.revision, invalidated_revision);
    assert!(resnapshot.revision > before.revision);
    assert_eq!(resnapshot.seq, before.seq);
    assert!(resnapshot.entries.is_empty());
    let files = CliNativeFile::list_latest_by_sid(&db.pool, sid)
        .await
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].generation, 1);
}

#[tokio::test]
async fn subscribe_during_import_preserves_update_after_snapshot_watermark() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let sid = "44444444-4444-4444-8444-444444444444";
    let native_file = native_dir.join(format!("{sid}.jsonl"));
    let initial = native_user_record(sid, "before-snapshot", "before", "2026-07-20T20:00:00Z");
    fs::write(&native_file, &initial).unwrap();
    ClaudeSessionLink::assign_manual(&db.pool, sid, session.id, &cwd.to_string_lossy())
        .await
        .unwrap()
        .unwrap();

    let service = Arc::new(ClaudeTranscriptIngest::new(db, projects_dir));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();

    let mut updates = service.subscribe();
    let shutdown = CancellationToken::new();
    let publisher = tokio::spawn(service.clone().run_publisher(shutdown.child_token()));
    let baseline_update = tokio::time::timeout(Duration::from_secs(5), updates.recv())
        .await
        .expect("publisher did not drain the initial outbox row")
        .unwrap();
    let baseline_seq = match baseline_update {
        NativeFeedUpdate::RecordsAppended {
            session_id,
            seq,
            revision,
        } => {
            assert_eq!(session_id, session.id);
            assert_eq!(revision, 0);
            seq
        }
        NativeFeedUpdate::RevisionInvalidated { .. } => {
            panic!("initial append unexpectedly invalidated the revision")
        }
    };

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    *service.snapshot_watermark_barrier.lock().await = Some(barrier.clone());
    let snapshot_service = service.clone();
    let snapshot = tokio::spawn(async move { snapshot_service.snapshot(session.id).await });

    // The receiver is already subscribed, and the snapshot has captured its
    // revision/sequence but not read projection rows. Import in that window.
    barrier.wait().await;
    let concurrent = native_user_record(sid, "during-snapshot", "during", "2026-07-20T20:00:01Z");
    fs::write(&native_file, format!("{initial}{concurrent}")).unwrap();
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();
    barrier.wait().await;

    let snapshot = snapshot.await.unwrap().unwrap();
    *service.snapshot_watermark_barrier.lock().await = None;
    assert_eq!(snapshot.seq, baseline_seq);
    assert!(
        snapshot
            .entries
            .iter()
            .any(|entry| entry.uuid.as_deref() == Some("during-snapshot"))
    );

    let queued_update = tokio::time::timeout(Duration::from_secs(5), updates.recv())
        .await
        .expect("concurrent import was not queued for the subscriber")
        .unwrap();
    match queued_update {
        NativeFeedUpdate::RecordsAppended {
            session_id,
            seq,
            revision,
        } => {
            assert_eq!(session_id, session.id);
            assert_eq!(seq, baseline_seq + 1);
            assert_eq!(revision, snapshot.revision);
            assert!(seq > snapshot.seq);
        }
        NativeFeedUpdate::RevisionInvalidated { .. } => {
            panic!("append unexpectedly invalidated the revision")
        }
    }

    shutdown.cancel();
    publisher.await.unwrap();
}
