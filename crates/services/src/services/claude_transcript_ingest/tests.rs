use std::{collections::HashSet, fs, path::Path, sync::Arc, time::Duration};

use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
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
    ClaudeTranscriptIngest, DirectoryContext, NativeFeedOrigin, NativeFeedUpdate,
    claude_project_slug, effective_cwd,
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

async fn create_session(db: &DBService, workspace_id: Uuid) -> Session {
    Session::create(
        &db.pool,
        &CreateSession {
            executor: Some("CLAUDE_CODE".to_string()),
            name: None,
        },
        Uuid::new_v4(),
        workspace_id,
    )
    .await
    .unwrap()
}

async fn create_coding_turn(db: &DBService, session_id: Uuid, prompt: &str) -> Uuid {
    let process_id = Uuid::new_v4();
    let action = ExecutorAction::new(
        ExecutorActionType::CodingAgentInitialRequest(CodingAgentInitialRequest {
            prompt: prompt.to_string(),
            executor_config: ExecutorConfig::new(BaseCodingAgent::ClaudeCode),
            working_dir: None,
        }),
        None,
    );
    ExecutionProcess::create(
        &db.pool,
        &CreateExecutionProcess {
            session_id,
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
            prompt: Some(prompt.to_string()),
        },
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    process_id
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
async fn large_backfill_imports_across_bounded_line_batches() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let sid = "10101010-1010-4010-8010-101010101010";
    let content = (0..300)
        .map(|index| {
            native_user_record(
                sid,
                &format!("batched-{index}"),
                &format!("line {index}"),
                "2026-07-20T20:00:00Z",
            )
        })
        .collect::<String>();
    fs::write(native_dir.join(format!("{sid}.jsonl")), content).unwrap();
    ClaudeSessionLink::assign_manual(&db.pool, sid, session.id, &cwd.to_string_lossy())
        .await
        .unwrap()
        .unwrap();

    let service = Arc::new(ClaudeTranscriptIngest::new(db.clone(), projects_dir));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();

    let file = CliNativeFile::list_latest_by_sid(&db.pool, sid)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        CliNativeRecord::count_for_file(&db.pool, file.id)
            .await
            .unwrap(),
        300
    );
    assert_eq!(
        CliIngestOutbox::latest_seq(&db.pool, session.id)
            .await
            .unwrap(),
        300
    );

    let mut updates = service.subscribe();
    let shutdown = CancellationToken::new();
    let publisher = tokio::spawn(service.clone().run_publisher(shutdown.child_token()));
    match tokio::time::timeout(Duration::from_secs(5), updates.recv())
        .await
        .expect("publisher did not drain batched backfill")
        .unwrap()
    {
        NativeFeedUpdate::RecordsAppended {
            session_id,
            seq,
            revision,
        } => {
            assert_eq!(session_id, session.id);
            assert_eq!(seq, 300);
            assert_eq!(revision, 0);
        }
        NativeFeedUpdate::RevisionInvalidated { .. } => {
            panic!("ordinary backfill should not invalidate its revision")
        }
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), updates.recv())
            .await
            .is_err(),
        "one drain cycle must publish one coalesced append"
    );
    shutdown.cancel();
    publisher.await.unwrap();
}

#[tokio::test]
async fn event_arriving_during_import_triggers_immediate_path_rescan() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let sid = "11101010-1010-4010-8010-101010101010";
    let native_file = native_dir.join(format!("{sid}.jsonl"));
    let first = native_user_record(sid, "before-pending", "before", "2026-07-20T20:00:00Z");
    fs::write(&native_file, &first).unwrap();
    ClaudeSessionLink::assign_manual(&db.pool, sid, session.id, &cwd.to_string_lossy())
        .await
        .unwrap()
        .unwrap();

    let service = Arc::new(ClaudeTranscriptIngest::new(db.clone(), projects_dir));
    let context = DirectoryContext {
        workspace_id: workspace.id,
        cwd,
    };
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    *service.path_import_barrier.lock().await = Some(barrier.clone());
    let import_service = service.clone();
    let import_path = native_file.clone();
    let import_context = context.clone();
    let first_import = tokio::spawn(async move {
        import_service
            .process_native_path(&import_path, &import_context, false)
            .await
    });

    // The first pass has completed its database transaction but still owns
    // the path. Append and deliver another event in that exact window.
    barrier.wait().await;
    let second = native_user_record(sid, "after-pending", "after", "2026-07-20T20:00:01Z");
    fs::write(&native_file, format!("{first}{second}")).unwrap();
    service
        .process_native_path(&native_file, &context, false)
        .await
        .unwrap();
    barrier.wait().await;
    first_import.await.unwrap().unwrap();

    let file = CliNativeFile::list_latest_by_sid(&db.pool, sid)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        CliNativeRecord::count_for_file(&db.pool, file.id)
            .await
            .unwrap(),
        2
    );
    let snapshot = service.snapshot(session.id).await.unwrap();
    assert!(
        snapshot
            .entries
            .iter()
            .any(|entry| entry.uuid.as_deref() == Some("after-pending"))
    );
}

#[tokio::test]
async fn unmatched_sid_is_imported_raw_before_manual_assignment() {
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
        1
    );
    assert_eq!(
        CliIngestOutbox::latest_seq(&db.pool, session.id)
            .await
            .unwrap(),
        0
    );
    assert!(
        service
            .snapshot(session.id)
            .await
            .unwrap()
            .entries
            .is_empty()
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
    assert_eq!(
        CliIngestOutbox::latest_seq(&db.pool, session.id)
            .await
            .unwrap(),
        1
    );
    assert_eq!(service.snapshot(session.id).await.unwrap().entries.len(), 1);
}

#[tokio::test]
async fn manual_assignment_republishes_raw_history_after_session_cascade() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, deleted_session) = create_workspace_and_session(&db, &workspace_root).await;
    let cwd = effective_cwd(&workspace, &deleted_session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let sid = "12121212-1212-4212-8212-121212121212";
    let native_file = native_dir.join(format!("{sid}.jsonl"));
    fs::write(
        &native_file,
        native_user_record(sid, "surviving-record", "survives", "2026-07-20T20:00:00Z"),
    )
    .unwrap();
    ClaudeSessionLink::assign_manual(&db.pool, sid, deleted_session.id, &cwd.to_string_lossy())
        .await
        .unwrap()
        .unwrap();

    let service = Arc::new(ClaudeTranscriptIngest::new(db.clone(), projects_dir));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();
    let file = CliNativeFile::list_latest_by_sid(&db.pool, sid)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        CliNativeRecord::count_for_file(&db.pool, file.id)
            .await
            .unwrap(),
        1
    );

    sqlx::query("DELETE FROM sessions WHERE id = ?")
        .bind(deleted_session.id)
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        ClaudeSessionLink::find(&db.pool, sid)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        CliNativeRecord::count_for_file(&db.pool, file.id)
            .await
            .unwrap(),
        1
    );

    let replacement_session = create_session(&db, workspace.id).await;
    service
        .assign_manual(sid, replacement_session.id)
        .await
        .unwrap();

    let snapshot = service.snapshot(replacement_session.id).await.unwrap();
    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(
        snapshot.entries[0].uuid.as_deref(),
        Some("surviving-record")
    );
    assert_eq!(
        CliIngestOutbox::latest_seq(&db.pool, replacement_session.id)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn executor_precedence_repoints_history_and_invalidates_both_feeds() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, original_session) = create_workspace_and_session(&db, &workspace_root).await;
    let cwd = effective_cwd(&workspace, &original_session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let sid = "13131313-1313-4313-8313-131313131313";
    fs::write(
        native_dir.join(format!("{sid}.jsonl")),
        native_user_record(sid, "repointed-record", "move me", "2026-07-20T20:00:00Z"),
    )
    .unwrap();
    ClaudeSessionLink::assign_manual(&db.pool, sid, original_session.id, &cwd.to_string_lossy())
        .await
        .unwrap()
        .unwrap();

    let service = Arc::new(ClaudeTranscriptIngest::new(db.clone(), projects_dir));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        service
            .snapshot(original_session.id)
            .await
            .unwrap()
            .entries
            .len(),
        1
    );

    let executor_session = create_session(&db, workspace.id).await;
    let process_id = create_coding_turn(&db, executor_session.id, "move me").await;
    CodingAgentTurn::update_agent_session_id(&db.pool, process_id, sid)
        .await
        .unwrap();
    let mut updates = service.subscribe();

    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();

    let mut invalidated = HashSet::new();
    for _ in 0..2 {
        match tokio::time::timeout(Duration::from_secs(5), updates.recv())
            .await
            .expect("both feeds should be invalidated")
            .unwrap()
        {
            NativeFeedUpdate::RevisionInvalidated { session_id, .. } => {
                invalidated.insert(session_id);
            }
            NativeFeedUpdate::RecordsAppended { .. } => {
                panic!("publisher is not running in this test")
            }
        }
    }
    assert_eq!(
        invalidated,
        HashSet::from([original_session.id, executor_session.id])
    );
    assert!(
        service
            .snapshot(original_session.id)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    let moved = service.snapshot(executor_session.id).await.unwrap();
    assert_eq!(moved.entries.len(), 1);
    assert_eq!(moved.entries[0].uuid.as_deref(), Some("repointed-record"));
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
    let recorded_at =
        (Utc::now() + ChronoDuration::seconds(1)).to_rfc3339_opts(SecondsFormat::Millis, true);
    let user = serde_json::json!({
        "type": "user",
        "sessionId": sid,
        "uuid": "app-user",
        "timestamp": recorded_at,
        "message": { "role": "user", "content": "app prompt" }
    });
    let assistant = serde_json::json!({
        "type": "assistant",
        "sessionId": sid,
        "uuid": "executor-assistant",
        "parentUuid": "app-user",
        "timestamp": (Utc::now() + ChronoDuration::seconds(2))
            .to_rfc3339_opts(SecondsFormat::Millis, true),
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
async fn same_prompt_hours_after_dispatch_remains_cli_origin() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let sid = "23232323-2323-4323-8323-232323232323";
    let process_id = create_coding_turn(&db, session.id, "continue").await;
    CodingAgentTurn::update_agent_session_id(&db.pool, process_id, sid)
        .await
        .unwrap();
    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let much_later =
        (Utc::now() + ChronoDuration::hours(2)).to_rfc3339_opts(SecondsFormat::Millis, true);
    fs::write(
        native_dir.join(format!("{sid}.jsonl")),
        native_user_record(sid, "later-cli", "continue", &much_later),
    )
    .unwrap();

    let service = Arc::new(ClaudeTranscriptIngest::new(db, projects_dir));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();

    let entry = service
        .snapshot(session.id)
        .await
        .unwrap()
        .entries
        .into_iter()
        .find(|entry| entry.uuid.as_deref() == Some("later-cli"))
        .unwrap();
    assert_eq!(entry.origin, NativeFeedOrigin::Cli);
}

#[tokio::test]
async fn native_link_after_import_reclassifies_origin_and_invalidates_feed() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let sid = "28282828-2828-4828-8828-282828282828";
    let assistant = serde_json::json!({
        "type": "assistant",
        "sessionId": sid,
        "uuid": "linked-after-import",
        "timestamp": "2026-07-20T20:00:00Z",
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": "late identity"}]
        }
    });
    fs::write(
        native_dir.join(format!("{sid}.jsonl")),
        format!("{assistant}\n"),
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
    assert_eq!(
        service.snapshot(session.id).await.unwrap().entries[0].origin,
        NativeFeedOrigin::Cli
    );

    let native_link_updates = super::native_link_events().subscribe();
    let shutdown = CancellationToken::new();
    let listener = tokio::spawn(
        service
            .clone()
            .run_native_link_invalidation(native_link_updates, shutdown.child_token()),
    );
    let mut feed_updates = service.subscribe();
    let process_id = create_coding_turn(&db, session.id, "executor output").await;
    assert!(
        crate::services::execution_process::persist_native_link_with_retry(
            &db,
            process_id,
            "linked-after-import",
        )
        .await
    );

    match tokio::time::timeout(Duration::from_secs(5), feed_updates.recv())
        .await
        .expect("late link did not invalidate the feed")
        .unwrap()
    {
        NativeFeedUpdate::RevisionInvalidated { session_id, .. } => {
            assert_eq!(session_id, session.id);
        }
        NativeFeedUpdate::RecordsAppended { .. } => panic!("publisher is not running"),
    }
    let snapshot = service.snapshot(session.id).await.unwrap();
    assert_eq!(snapshot.entries[0].origin, NativeFeedOrigin::Executor);
    assert_eq!(
        snapshot.entries[0].linked_execution_process_id,
        Some(process_id)
    );

    shutdown.cancel();
    listener.await.unwrap();
}

#[tokio::test]
async fn recent_app_dispatch_prefers_the_newest_matching_turn() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let sid = "24242424-2424-4424-8424-242424242424";
    let older_process = create_coding_turn(&db, session.id, "same prompt").await;
    let newer_process = create_coding_turn(&db, session.id, "same prompt").await;
    CodingAgentTurn::update_agent_session_id(&db.pool, newer_process, sid)
        .await
        .unwrap();
    let reference = Utc::now();
    sqlx::query("UPDATE coding_agent_turns SET created_at = ? WHERE execution_process_id = ?")
        .bind(reference - ChronoDuration::minutes(5))
        .bind(older_process)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE coding_agent_turns SET created_at = ? WHERE execution_process_id = ?")
        .bind(reference - ChronoDuration::minutes(1))
        .bind(newer_process)
        .execute(&db.pool)
        .await
        .unwrap();
    let newer_turn = CodingAgentTurn::find_by_execution_process_id(&db.pool, newer_process)
        .await
        .unwrap()
        .unwrap();
    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    fs::write(
        native_dir.join(format!("{sid}.jsonl")),
        native_user_record(
            sid,
            "recent-app-user",
            "same prompt",
            &reference.to_rfc3339_opts(SecondsFormat::Millis, true),
        ),
    )
    .unwrap();

    let service = Arc::new(ClaudeTranscriptIngest::new(db.clone(), projects_dir));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();

    let row = CliNativeRecord::list_for_session(&db.pool, session.id)
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.uuid.as_deref() == Some("recent-app-user"))
        .unwrap();
    assert_eq!(row.bound_coding_agent_turn_id, Some(newer_turn.id));
    assert_eq!(
        service.snapshot(session.id).await.unwrap().entries[0].origin,
        NativeFeedOrigin::App
    );
}

#[tokio::test]
async fn prompt_fallback_skips_turn_with_uuid_linked_native_record() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let sid = "24545454-2454-4454-8454-245454545454";
    let older_process = create_coding_turn(&db, session.id, "same prompt").await;
    let newer_process = create_coding_turn(&db, session.id, "same prompt").await;
    CodingAgentTurn::update_agent_session_id(&db.pool, newer_process, sid)
        .await
        .unwrap();
    let reference = Utc::now();
    sqlx::query("UPDATE coding_agent_turns SET created_at = ? WHERE execution_process_id = ?")
        .bind(reference - ChronoDuration::minutes(5))
        .bind(older_process)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE coding_agent_turns SET created_at = ? WHERE execution_process_id = ?")
        .bind(reference - ChronoDuration::minutes(1))
        .bind(newer_process)
        .execute(&db.pool)
        .await
        .unwrap();
    let older_turn = CodingAgentTurn::find_by_execution_process_id(&db.pool, older_process)
        .await
        .unwrap()
        .unwrap();
    ExecutionNativeLink::insert(&db.pool, newer_process, "newer-executor-native")
        .await
        .unwrap();

    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let assistant = serde_json::json!({
        "type": "assistant",
        "sessionId": sid,
        "uuid": "newer-executor-native",
        "timestamp": reference.to_rfc3339_opts(SecondsFormat::Millis, true),
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": "executor evidence"}]
        }
    });
    let user = native_user_record(
        sid,
        "fallback-user",
        "same prompt",
        &reference.to_rfc3339_opts(SecondsFormat::Millis, true),
    );
    fs::write(
        native_dir.join(format!("{sid}.jsonl")),
        format!("{assistant}\n{user}"),
    )
    .unwrap();

    let service = Arc::new(ClaudeTranscriptIngest::new(db.clone(), projects_dir));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();

    let user_row = CliNativeRecord::list_for_session(&db.pool, session.id)
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.uuid.as_deref() == Some("fallback-user"))
        .unwrap();
    assert_eq!(user_row.bound_coding_agent_turn_id, Some(older_turn.id));
}

#[tokio::test]
async fn timestampless_user_record_uses_import_clock_for_prompt_binding() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let sid = "25252525-2525-4525-8525-252525252525";
    let process_id = create_coding_turn(&db, session.id, "no timestamp").await;
    CodingAgentTurn::update_agent_session_id(&db.pool, process_id, sid)
        .await
        .unwrap();
    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let record = serde_json::json!({
        "type": "user",
        "sessionId": sid,
        "uuid": "timestampless-user",
        "message": { "role": "user", "content": "no timestamp" }
    });
    fs::write(
        native_dir.join(format!("{sid}.jsonl")),
        format!("{record}\n"),
    )
    .unwrap();

    let service = Arc::new(ClaudeTranscriptIngest::new(db, projects_dir));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();

    let entry = service
        .snapshot(session.id)
        .await
        .unwrap()
        .entries
        .into_iter()
        .find(|entry| entry.uuid.as_deref() == Some("timestampless-user"))
        .unwrap();
    assert_eq!(entry.origin, NativeFeedOrigin::App);
}

#[tokio::test]
async fn ordinary_outbox_constraint_failure_rolls_back_for_scan_retry() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let first_sid = "26262626-2626-4626-8626-262626262626";
    fs::write(
        native_dir.join(format!("{first_sid}.jsonl")),
        native_user_record(first_sid, "first", "first", "2026-07-20T20:00:00Z"),
    )
    .unwrap();
    ClaudeSessionLink::assign_manual(&db.pool, first_sid, session.id, &cwd.to_string_lossy())
        .await
        .unwrap()
        .unwrap();
    let service = Arc::new(ClaudeTranscriptIngest::new(db.clone(), projects_dir));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();

    // This secondary constraint models any unexpected outbox invariant. A
    // strict append must abort the raw-record/cursor transaction rather than
    // silently accepting a partially published import.
    sqlx::query(
        "CREATE UNIQUE INDEX test_outbox_line_seq ON cli_ingest_outbox(session_id, line_seq)",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    let retry_sid = "27272727-2727-4727-8727-272727272727";
    fs::write(
        native_dir.join(format!("{retry_sid}.jsonl")),
        native_user_record(retry_sid, "retry", "retry", "2026-07-20T20:00:01Z"),
    )
    .unwrap();
    ClaudeSessionLink::assign_manual(&db.pool, retry_sid, session.id, &cwd.to_string_lossy())
        .await
        .unwrap()
        .unwrap();
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();
    let retry_file = CliNativeFile::list_latest_by_sid(&db.pool, retry_sid)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        CliNativeRecord::count_for_file(&db.pool, retry_file.id)
            .await
            .unwrap(),
        0
    );
    assert_eq!(retry_file.cursor_offset, 0);

    sqlx::query("DROP INDEX test_outbox_line_seq")
        .execute(&db.pool)
        .await
        .unwrap();
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        CliNativeRecord::count_for_file(&db.pool, retry_file.id)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        CliIngestOutbox::latest_seq(&db.pool, session.id)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn staged_truncation_keeps_old_generation_until_replacement_batch_commits() {
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
    let old_file = CliNativeFile::list_latest_by_sid(&db.pool, sid)
        .await
        .unwrap()
        .pop()
        .unwrap();

    let mut updates = service.subscribe();
    fs::write(&native_file, br#"{"type":"user"#).unwrap();
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();

    assert!(
        tokio::time::timeout(Duration::from_millis(100), updates.recv())
            .await
            .is_err(),
        "an uncommitted replacement must not invalidate the feed"
    );
    let staged = service.snapshot(session.id).await.unwrap();
    assert_eq!(staged.revision, before.revision);
    assert_eq!(staged.entries.len(), 1);
    assert_eq!(staged.entries[0].uuid.as_deref(), Some("before-truncate"));
    let staged_files = CliNativeFile::list_latest_by_sid(&db.pool, sid)
        .await
        .unwrap();
    assert_eq!(staged_files.len(), 1);
    assert_eq!(staged_files[0].generation, 0);

    fs::write(
        &native_file,
        native_user_record(sid, "after-truncate", "replacement", "2026-07-20T20:01:00Z"),
    )
    .unwrap();
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();
    let update = tokio::time::timeout(Duration::from_secs(5), updates.recv())
        .await
        .expect("committed replacement did not invalidate the feed")
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
            panic!("publisher is not running in this test")
        }
    };

    let resnapshot = service.snapshot(session.id).await.unwrap();
    assert_eq!(resnapshot.revision, invalidated_revision);
    assert!(resnapshot.revision > before.revision);
    assert!(resnapshot.seq > before.seq);
    assert_eq!(resnapshot.entries.len(), 1);
    assert_eq!(
        resnapshot.entries[0].uuid.as_deref(),
        Some("after-truncate")
    );
    let files = CliNativeFile::list_latest_by_sid(&db.pool, sid)
        .await
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].generation, 1);
    assert!(
        CliNativeFile::find_by_id(&db.pool, old_file.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        CliNativeRecord::count_for_file(&db.pool, old_file.id)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn rescan_purge_frees_prompt_binding_for_rewritten_record() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let sid = "34343434-3434-4434-8434-343434343434";
    let process_id = create_coding_turn(&db, session.id, "rewrite prompt").await;
    CodingAgentTurn::update_agent_session_id(&db.pool, process_id, sid)
        .await
        .unwrap();
    let turn = CodingAgentTurn::find_by_execution_process_id(&db.pool, process_id)
        .await
        .unwrap()
        .unwrap();
    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let native_file = native_dir.join(format!("{sid}.jsonl"));
    let recorded_at =
        (Utc::now() + ChronoDuration::seconds(1)).to_rfc3339_opts(SecondsFormat::Millis, true);
    fs::write(
        &native_file,
        native_user_record(sid, "before-rewrite", "rewrite prompt", &recorded_at),
    )
    .unwrap();

    let service = Arc::new(ClaudeTranscriptIngest::new(db.clone(), projects_dir));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();
    let old_file = CliNativeFile::list_latest_by_sid(&db.pool, sid)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let old_row = CliNativeRecord::list_for_session(&db.pool, session.id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(old_row.bound_coding_agent_turn_id, Some(turn.id));

    fs::write(
        &native_file,
        native_user_record(sid, "after-rewrite", "rewrite prompt", &recorded_at),
    )
    .unwrap();
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();

    assert!(
        CliNativeFile::find_by_id(&db.pool, old_file.id)
            .await
            .unwrap()
            .is_none()
    );
    let rows = CliNativeRecord::list_for_session(&db.pool, session.id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].uuid.as_deref(), Some("after-rewrite"));
    assert_eq!(rows[0].bound_coding_agent_turn_id, Some(turn.id));
    assert_eq!(
        service.snapshot(session.id).await.unwrap().entries[0].origin,
        NativeFeedOrigin::App
    );
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

#[tokio::test]
async fn reconcile_restarts_finished_native_transcript_watcher() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let canonical_dir = fs::canonicalize(&native_dir).unwrap();
    let service = Arc::new(ClaudeTranscriptIngest::new(db, projects_dir));
    let shutdown = CancellationToken::new();

    service
        .reconcile_registry(true, shutdown.child_token())
        .await
        .unwrap();
    let old_task_id = {
        let watchers = service.watchers.lock().await;
        watchers.get(&canonical_dir).unwrap().id()
    };
    {
        let watchers = service.watchers.lock().await;
        watchers.get(&canonical_dir).unwrap().abort();
    }
    for _ in 0..20 {
        if service
            .watchers
            .lock()
            .await
            .get(&canonical_dir)
            .unwrap()
            .is_finished()
        {
            break;
        }
        tokio::task::yield_now().await;
    }

    service
        .reconcile_registry(true, shutdown.child_token())
        .await
        .unwrap();
    let watchers = service.watchers.lock().await;
    let replacement = watchers.get(&canonical_dir).unwrap();
    assert_ne!(replacement.id(), old_task_id);
    assert!(!replacement.is_finished());
    drop(watchers);
    shutdown.cancel();
}

#[tokio::test]
async fn reconcile_removes_watcher_when_workspace_is_archived() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let canonical_dir = fs::canonicalize(&native_dir).unwrap();
    let service = Arc::new(ClaudeTranscriptIngest::new(db.clone(), projects_dir));
    let shutdown = CancellationToken::new();

    service
        .reconcile_registry(true, shutdown.child_token())
        .await
        .unwrap();
    assert!(service.watchers.lock().await.contains_key(&canonical_dir));
    assert!(
        service
            .directories
            .read()
            .await
            .contains_key(&canonical_dir)
    );

    Workspace::set_archived(&db.pool, workspace.id, true)
        .await
        .unwrap();
    service
        .reconcile_registry(true, shutdown.child_token())
        .await
        .unwrap();
    assert!(!service.watchers.lock().await.contains_key(&canonical_dir));
    assert!(
        !service
            .directories
            .read()
            .await
            .contains_key(&canonical_dir)
    );
    assert!(
        !service
            .degraded_watchers
            .read()
            .await
            .contains(&canonical_dir)
    );
    shutdown.cancel();
}

#[tokio::test]
async fn watcher_degradation_is_scoped_to_its_workspace() {
    let temp = TempDir::new().unwrap();
    let first_root = temp.path().join("first-worktree");
    let second_root = temp.path().join("second-worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&first_root).unwrap();
    fs::create_dir_all(&second_root).unwrap();
    let db = test_db().await;
    let (first_workspace, first_session) = create_workspace_and_session(&db, &first_root).await;
    let (second_workspace, second_session) = create_workspace_and_session(&db, &second_root).await;
    let first_dir = store_dir(
        &projects_dir,
        &effective_cwd(&first_workspace, &first_session).unwrap(),
    );
    let second_dir = store_dir(
        &projects_dir,
        &effective_cwd(&second_workspace, &second_session).unwrap(),
    );
    fs::create_dir_all(&first_dir).unwrap();
    fs::create_dir_all(&second_dir).unwrap();
    let second_dir = fs::canonicalize(second_dir).unwrap();
    let service = Arc::new(ClaudeTranscriptIngest::new(db, projects_dir));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();
    service.degraded_watchers.write().await.insert(second_dir);

    assert!(
        !service
            .snapshot(first_session.id)
            .await
            .unwrap()
            .health
            .watch_degraded
    );
    assert!(
        service
            .snapshot(second_session.id)
            .await
            .unwrap()
            .health
            .watch_degraded
    );
}
