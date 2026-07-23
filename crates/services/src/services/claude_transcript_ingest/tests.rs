use std::{
    collections::HashSet,
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
use db::{
    DBService,
    models::{
        claude_session_link::{ClaudeSessionBoundVia, ClaudeSessionLink},
        cli_ingest_outbox::CliIngestOutbox,
        cli_native_file::{CliNativeFile, RegisterCliNativeFile},
        cli_native_record::{
            CliNativeRecord, CliNativeRecordDisposition, ImportedCursor, NativeImportContext,
            NewCliNativeRecord,
        },
        cli_pane_binding::{CliPaneBinding, CliPaneBoundVia},
        coding_agent_turn::{CodingAgentTurn, CreateCodingAgentTurn},
        execution_native_link::ExecutionNativeLink,
        execution_process::{CreateExecutionProcess, ExecutionProcess, ExecutionProcessRunReason},
        session::{CreateSession, Session},
        session_queued_message::{
            QueuedMessageSource, QueuedMessageState, SessionQueuedMessage, StoreQueuedMessageResult,
        },
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
    ClaudeTranscriptIngest, ClaudeTranscriptIngestError, CliSessionKind, DirectoryContext,
    NativeFeedOrigin, NativeFeedUpdate, claude_project_slug, read_session_preview,
};
use crate::services::cli_collab::{CliWriterProbe, ProbeReport, SidEvidence};

#[derive(Clone)]
struct StaticWriterProbe(ProbeReport);

#[async_trait]
impl CliWriterProbe for StaticWriterProbe {
    async fn probe(
        &self,
        _workspace_id: Uuid,
        _effective_dir: &Path,
        _expected_sid: Option<&str>,
        _binding: Option<&CliPaneBinding>,
        _check_cwd_uniqueness: bool,
    ) -> ProbeReport {
        self.0.clone()
    }
}

#[derive(Clone)]
struct RecordingWriterProbe {
    report: ProbeReport,
    uniqueness_checks: Arc<Mutex<Vec<bool>>>,
}

#[async_trait]
impl CliWriterProbe for RecordingWriterProbe {
    async fn probe(
        &self,
        _workspace_id: Uuid,
        _effective_dir: &Path,
        _expected_sid: Option<&str>,
        _binding: Option<&CliPaneBinding>,
        check_cwd_uniqueness: bool,
    ) -> ProbeReport {
        self.uniqueness_checks
            .lock()
            .unwrap()
            .push(check_cwd_uniqueness);
        self.report.clone()
    }
}

fn effective_cwd(workspace: &Workspace, session: &Session) -> Option<std::path::PathBuf> {
    workspace
        .container_ref
        .as_deref()
        .and_then(|container_ref| session.effective_working_dir(Path::new(container_ref)))
}

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

#[tokio::test]
async fn nonexistent_workspace_is_not_reported_as_a_missing_path() {
    let db = test_db().await;
    let service = ClaudeTranscriptIngest::new(db, std::env::temp_dir());

    assert!(matches!(
        service.list_unassigned(Uuid::new_v4()).await,
        Err(ClaudeTranscriptIngestError::Workspace(
            db::models::workspace::WorkspaceError::WorkspaceNotFound
        ))
    ));
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

async fn register_native_file(db: &DBService, workspace_id: Uuid, sid: &str) -> CliNativeFile {
    CliNativeFile::register(
        &db.pool,
        &RegisterCliNativeFile {
            claude_session_id: sid,
            dir_path: "/tmp/native-import-test",
            file_name: &format!("{sid}.jsonl"),
            discovered_workspace_id: Some(workspace_id),
            dev: 1,
            inode: sid.bytes().map(i64::from).sum(),
            observed_size: 1,
            observed_mtime_ms: None,
        },
    )
    .await
    .unwrap()
}

async fn pasted_slot(
    db: &DBService,
    session_id: Uuid,
    sid: &str,
    prompt: &str,
) -> SessionQueuedMessage {
    let config = serde_json::to_string(&ExecutorConfig::new(BaseCodingAgent::ClaudeCode)).unwrap();
    let row = match SessionQueuedMessage::store(
        &db.pool,
        session_id,
        prompt,
        Some(&config),
        QueuedMessageSource::Ui,
        false,
    )
    .await
    .unwrap()
    {
        StoreQueuedMessageResult::Stored(row) => row,
        StoreQueuedMessageResult::Conflict(_) => panic!("fixture slot must be empty"),
    };
    SessionQueuedMessage::claim_for_paste(&db.pool, row.id, Some(sid), "test-paste-owner")
        .await
        .unwrap()
        .unwrap();
    SessionQueuedMessage::mark_pasted(&db.pool, row.id, "test-paste-owner")
        .await
        .unwrap();
    SessionQueuedMessage::find_by_id(&db.pool, row.id)
        .await
        .unwrap()
        .unwrap()
}

fn import_record(
    sid: &str,
    line_seq: i64,
    uuid: &str,
    prompt: &str,
    at: chrono::DateTime<Utc>,
) -> NewCliNativeRecord {
    NewCliNativeRecord {
        line_seq,
        claude_session_id: sid.to_string(),
        uuid: Some(uuid.to_string()),
        parent_uuid: None,
        kind: "user".to_string(),
        ts: Some(at.to_rfc3339()),
        raw: native_user_record(sid, uuid, prompt, &at.to_rfc3339())
            .trim()
            .to_string(),
        disposition: CliNativeRecordDisposition::Renderable,
        user_prompt: Some(prompt.to_string()),
        recorded_at: Some(at),
    }
}

fn cursor(next_line_seq: i64) -> ImportedCursor<'static> {
    ImportedCursor {
        cursor_offset: next_line_seq + 1,
        next_line_seq,
        last_line_offset: next_line_seq,
        last_line_hash: None,
        observed_size: next_line_seq + 1,
        observed_mtime_ms: None,
    }
}

fn writer_report(
    pane_session_exists: bool,
    agent_running: Option<bool>,
    sid_evidence: SidEvidence,
    only_active_claude_in_cwd: Option<bool>,
) -> ProbeReport {
    ProbeReport {
        pane_session_exists,
        agent_running,
        sid_evidence,
        probe_failed: false,
        only_active_claude_in_cwd,
    }
}

async fn import_foreign_writer_case(report: ProbeReport, executor_running: bool) -> (bool, bool) {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let sid = "45454545-4545-4545-8545-454545454545";
    ClaudeSessionLink::assign_manual(&db.pool, sid, session.id, &cwd.to_string_lossy())
        .await
        .unwrap()
        .unwrap();
    if executor_running {
        create_coding_turn(&db, session.id, "unrelated running prompt").await;
    }
    fs::write(
        native_dir.join(format!("{sid}.jsonl")),
        native_user_record(
            sid,
            "foreign-user",
            "written elsewhere",
            "2026-07-21T12:00:00Z",
        ),
    )
    .unwrap();

    let service = Arc::new(ClaudeTranscriptIngest::new_with_probe(
        db.clone(),
        projects_dir,
        Arc::new(StaticWriterProbe(report)),
    ));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();

    let rows = CliNativeRecord::list_for_session(&db.pool, session.id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].linked_execution_process_id, None);
    assert_eq!(rows[0].bound_turn_execution_process_id, None);
    assert_eq!(rows[0].bound_queued_message_id, None);
    let link = ClaudeSessionLink::find(&db.pool, sid)
        .await
        .unwrap()
        .unwrap();
    let health = service.snapshot(session.id).await.unwrap().health;
    (
        link.foreign_writer_seen_at.is_some(),
        health.foreign_writer_seen_at.is_some(),
    )
}

async fn import_cli_fresh_case(
    release_binding: bool,
    report: ProbeReport,
) -> (Option<ClaudeSessionLink>, CliPaneBinding, u64, Vec<bool>) {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let sid = "46464646-4646-4646-8646-464646464646";
    let binding = CliPaneBinding::record_launch(
        &db.pool,
        workspace.id,
        session.id,
        None,
        CliPaneBoundVia::CliFresh,
    )
    .await
    .unwrap();
    if release_binding {
        assert!(CliPaneBinding::release(&db.pool, binding.id).await.unwrap());
    }
    fs::write(
        native_dir.join(format!("{sid}.jsonl")),
        native_user_record(
            sid,
            "cli-fresh-user",
            "new CLI conversation",
            "2026-07-21T12:00:00Z",
        ),
    )
    .unwrap();

    let uniqueness_checks = Arc::new(Mutex::new(Vec::new()));
    let service = Arc::new(ClaudeTranscriptIngest::new_with_probe(
        db.clone(),
        projects_dir,
        Arc::new(RecordingWriterProbe {
            report,
            uniqueness_checks: uniqueness_checks.clone(),
        }),
    ));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();

    let link = ClaudeSessionLink::find(&db.pool, sid).await.unwrap();
    let binding = CliPaneBinding::find_by_id(&db.pool, binding.id)
        .await
        .unwrap()
        .unwrap();
    let quarantined_files = service
        .snapshot(session.id)
        .await
        .unwrap()
        .health
        .quarantined_files;
    let uniqueness_checks = uniqueness_checks.lock().unwrap().clone();
    (link, binding, quarantined_files, uniqueness_checks)
}

#[tokio::test]
async fn foreign_writer_is_recorded_and_surfaced_when_no_app_writer_is_live() {
    let observed = import_foreign_writer_case(
        writer_report(false, Some(false), SidEvidence::Unknown, Some(false)),
        false,
    )
    .await;
    assert_eq!(observed, (true, true));
}

#[tokio::test]
async fn foreign_writer_classifier_fails_safe_for_executor_pane_and_probe_failure() {
    let executor_running = import_foreign_writer_case(
        writer_report(false, Some(false), SidEvidence::Unknown, Some(false)),
        true,
    )
    .await;
    assert_eq!(executor_running, (false, false));

    let live_pane = import_foreign_writer_case(
        writer_report(true, Some(true), SidEvidence::Unknown, Some(true)),
        false,
    )
    .await;
    assert_eq!(live_pane, (false, false));

    let probe_failure = import_foreign_writer_case(ProbeReport::failed(), false).await;
    assert_eq!(probe_failure, (false, false));
}

#[tokio::test]
async fn cli_fresh_file_auto_binds_to_the_only_live_unreleased_pane() {
    let (link, binding, quarantined_files, _) = import_cli_fresh_case(
        false,
        writer_report(true, Some(true), SidEvidence::NoResumeArg, Some(true)),
    )
    .await;
    let link = link.unwrap();
    assert_eq!(link.session_id, binding.session_id);
    assert_eq!(link.bound_via, ClaudeSessionBoundVia::CliFresh);
    assert_eq!(
        binding.claude_session_id.as_deref(),
        Some(link.claude_session_id.as_str())
    );
    assert!(binding.released_at.is_none());
    assert_eq!(quarantined_files, 0);
}

#[tokio::test]
async fn cwd_uniqueness_is_requested_only_for_cli_fresh_auto_binding() {
    let (_, _, _, uniqueness_checks) = import_cli_fresh_case(
        false,
        writer_report(true, Some(true), SidEvidence::NoResumeArg, Some(true)),
    )
    .await;

    assert_eq!(uniqueness_checks, vec![true, false]);
}

#[tokio::test]
async fn cli_fresh_file_quarantines_for_released_dead_or_nonexclusive_panes() {
    let healthy_report = || writer_report(true, Some(true), SidEvidence::NoResumeArg, Some(true));
    let cases = [
        (true, healthy_report()),
        (
            false,
            writer_report(false, Some(false), SidEvidence::NoResumeArg, Some(true)),
        ),
        (
            false,
            writer_report(true, Some(true), SidEvidence::NoResumeArg, Some(false)),
        ),
    ];

    for (release_binding, report) in cases {
        let (link, binding, quarantined_files, _) =
            import_cli_fresh_case(release_binding, report).await;
        assert!(link.is_none());
        assert!(binding.claude_session_id.is_none());
        assert_eq!(quarantined_files, 1);
    }
}

#[tokio::test]
async fn paste_ack_binding_and_slot_import_are_atomic_and_project_as_app_origin() {
    let temp = TempDir::new().unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, temp.path()).await;
    let sid = "41414141-4141-4141-8141-414141414141";
    let cwd = effective_cwd(&workspace, &session).unwrap();
    ClaudeSessionLink::assign_manual(&db.pool, sid, session.id, &cwd.to_string_lossy())
        .await
        .unwrap()
        .unwrap();
    let native_file = register_native_file(&db, workspace.id, sid).await;
    let slot = pasted_slot(&db, session.id, sid, "deliver through CLI").await;
    let recorded_at = slot.pasted_at.unwrap() + ChronoDuration::seconds(1);
    let record = import_record(
        sid,
        0,
        "delivery-bound-user",
        "deliver through CLI",
        recorded_at,
    );

    sqlx::query(
        "CREATE TRIGGER fail_native_cursor BEFORE UPDATE ON cli_native_files \
         BEGIN SELECT RAISE(ABORT, 'cursor failure'); END",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(
        CliNativeRecord::import_batch_with_context(
            &db.pool,
            native_file.id,
            std::slice::from_ref(&record),
            &cursor(1),
            NativeImportContext::default(),
        )
        .await
        .is_err()
    );
    assert_eq!(
        CliNativeRecord::count_for_file(&db.pool, native_file.id)
            .await
            .unwrap(),
        0
    );
    let rolled_back_slot = SessionQueuedMessage::find_by_id(&db.pool, slot.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rolled_back_slot.state, QueuedMessageState::Pasted);
    assert!(rolled_back_slot.acked_at.is_none());

    sqlx::query("DROP TRIGGER fail_native_cursor")
        .execute(&db.pool)
        .await
        .unwrap();
    CliNativeRecord::import_batch_with_context(
        &db.pool,
        native_file.id,
        &[record],
        &cursor(1),
        NativeImportContext::default(),
    )
    .await
    .unwrap();

    let rows = CliNativeRecord::list_for_session(&db.pool, session.id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].bound_queued_message_id, Some(slot.id));
    let imported_slot = SessionQueuedMessage::find_by_id(&db.pool, slot.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(imported_slot.state, QueuedMessageState::Imported);
    assert!(imported_slot.acked_at.is_some());
    assert_eq!(imported_slot.prompt, "deliver through CLI");

    let service = ClaudeTranscriptIngest::new(db, temp.path().join("projects"));
    let entry = service
        .snapshot(session.id)
        .await
        .unwrap()
        .entries
        .into_iter()
        .find(|entry| entry.uuid.as_deref() == Some("delivery-bound-user"))
        .unwrap();
    assert_eq!(entry.origin, NativeFeedOrigin::App);
}

#[tokio::test]
async fn late_paste_ack_imports_requeued_slot_and_blocks_duplicate_claim() {
    let temp = TempDir::new().unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, temp.path()).await;
    let sid = "45454545-4545-4545-8545-454545454545";
    let cwd = effective_cwd(&workspace, &session).unwrap();
    ClaudeSessionLink::assign_manual(&db.pool, sid, session.id, &cwd.to_string_lossy())
        .await
        .unwrap()
        .unwrap();
    let native_file = register_native_file(&db, workspace.id, sid).await;
    let slot = pasted_slot(&db, session.id, sid, "late CLI submission").await;
    let pasted_at = slot.pasted_at.unwrap();

    assert!(
        SessionQueuedMessage::requeue_pasted(&db.pool, slot.id)
            .await
            .unwrap()
    );
    let requeued = SessionQueuedMessage::find_by_id(&db.pool, slot.id)
        .await
        .unwrap()
        .unwrap();
    assert!(requeued.was_requeued_from_pasted());
    assert_eq!(requeued.pasted_at, Some(pasted_at));

    CliNativeRecord::import_batch(
        &db.pool,
        native_file.id,
        &[import_record(
            sid,
            0,
            "late-delivery-bound-user",
            "late CLI submission",
            pasted_at + ChronoDuration::seconds(31),
        )],
        &cursor(1),
    )
    .await
    .unwrap();

    let rows = CliNativeRecord::list_for_session(&db.pool, session.id)
        .await
        .unwrap();
    assert_eq!(rows[0].bound_queued_message_id, Some(slot.id));
    let imported = SessionQueuedMessage::find_by_id(&db.pool, slot.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(imported.state, QueuedMessageState::Imported);
    assert!(imported.acked_at.is_some());
    assert!(
        SessionQueuedMessage::claim_for_executor(&db.pool, slot.id, "test-owner")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        SessionQueuedMessage::find_active(&db.pool, session.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn paste_ack_matcher_enforces_paste_window_bounds() {
    let temp = TempDir::new().unwrap();
    let db = test_db().await;
    let (workspace, early_session) = create_workspace_and_session(&db, temp.path()).await;
    let cwd = effective_cwd(&workspace, &early_session).unwrap();

    let early_sid = "42424242-4242-4242-8242-424242424242";
    ClaudeSessionLink::assign_manual(
        &db.pool,
        early_sid,
        early_session.id,
        &cwd.to_string_lossy(),
    )
    .await
    .unwrap()
    .unwrap();
    let early_file = register_native_file(&db, workspace.id, early_sid).await;
    let early_slot = pasted_slot(&db, early_session.id, early_sid, "same prompt").await;
    let before_skew = early_slot.pasted_at.unwrap() - ChronoDuration::seconds(6);
    CliNativeRecord::import_batch(
        &db.pool,
        early_file.id,
        &[import_record(
            early_sid,
            0,
            "before-paste-skew",
            "same prompt",
            before_skew,
        )],
        &cursor(1),
    )
    .await
    .unwrap();
    let early_rows = CliNativeRecord::list_for_session(&db.pool, early_session.id)
        .await
        .unwrap();
    assert_eq!(early_rows[0].bound_queued_message_id, None);
    assert_eq!(
        SessionQueuedMessage::find_by_id(&db.pool, early_slot.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        QueuedMessageState::Pasted
    );

    let stale_session = create_session(&db, workspace.id).await;
    let stale_sid = "43434343-4343-4343-8343-434343434343";
    ClaudeSessionLink::assign_manual(
        &db.pool,
        stale_sid,
        stale_session.id,
        &cwd.to_string_lossy(),
    )
    .await
    .unwrap()
    .unwrap();
    let stale_file = register_native_file(&db, workspace.id, stale_sid).await;
    let stale_slot = pasted_slot(&db, stale_session.id, stale_sid, "same prompt").await;
    let after_timeout =
        stale_slot.pasted_at.unwrap() + ChronoDuration::minutes(15) + ChronoDuration::seconds(1);
    CliNativeRecord::import_batch(
        &db.pool,
        stale_file.id,
        &[import_record(
            stale_sid,
            0,
            "after-paste-window",
            "same prompt",
            after_timeout,
        )],
        &cursor(1),
    )
    .await
    .unwrap();
    let stale_rows = CliNativeRecord::list_for_session(&db.pool, stale_session.id)
        .await
        .unwrap();
    assert_eq!(stale_rows[0].bound_queued_message_id, None);
    assert_eq!(
        SessionQueuedMessage::find_by_id(&db.pool, stale_slot.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        QueuedMessageState::Pasted
    );
}

#[tokio::test]
async fn paste_ack_matcher_excludes_executor_linked_native_records() {
    let temp = TempDir::new().unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, temp.path()).await;
    let sid = "44444444-4444-4444-8444-444444444444";
    let cwd = effective_cwd(&workspace, &session).unwrap();
    ClaudeSessionLink::assign_manual(&db.pool, sid, session.id, &cwd.to_string_lossy())
        .await
        .unwrap()
        .unwrap();
    let native_file = register_native_file(&db, workspace.id, sid).await;
    let slot = pasted_slot(&db, session.id, sid, "linked prompt").await;
    let process_id = create_coding_turn(&db, session.id, "linked prompt").await;
    ExecutionNativeLink::insert(&db.pool, process_id, "executor-linked-user")
        .await
        .unwrap();
    let recorded_at = slot.pasted_at.unwrap() + ChronoDuration::seconds(1);

    CliNativeRecord::import_batch(
        &db.pool,
        native_file.id,
        &[import_record(
            sid,
            0,
            "executor-linked-user",
            "linked prompt",
            recorded_at,
        )],
        &cursor(1),
    )
    .await
    .unwrap();

    let rows = CliNativeRecord::list_for_session(&db.pool, session.id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].linked_execution_process_id, Some(process_id));
    assert_eq!(rows[0].bound_queued_message_id, None);
    let unacked_slot = SessionQueuedMessage::find_by_id(&db.pool, slot.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unacked_slot.state, QueuedMessageState::Pasted);
    assert!(unacked_slot.acked_at.is_none());
}

#[test]
fn cli_session_kind_classification_fails_open_to_main() {
    assert_eq!(
        CliSessionKind::from_entrypoint(Some("cli")),
        CliSessionKind::Main
    );
    assert_eq!(CliSessionKind::from_entrypoint(None), CliSessionKind::Main);
    assert_eq!(
        CliSessionKind::from_entrypoint(Some("totally-new-thing")),
        CliSessionKind::Main
    );
    assert_eq!(
        CliSessionKind::from_entrypoint(Some("sdk-cli")),
        CliSessionKind::Subagent
    );
    assert_eq!(
        CliSessionKind::from_entrypoint(Some("sdk-py")),
        CliSessionKind::Subagent
    );
}

#[test]
fn session_preview_skips_malformed_lines_within_its_scan_bound() {
    let temp = TempDir::new().unwrap();
    let sid = "91919191-9191-4919-8919-919191919191";
    let path = temp.path().join(format!("{sid}.jsonl"));
    fs::write(
        &path,
        format!(
            "not-json\n{}",
            native_user_record(
                sid,
                "preview-user",
                "usable preview",
                "2026-07-20T20:00:00Z"
            )
        ),
    )
    .unwrap();

    let preview = read_session_preview(&path, sid);
    assert_eq!(
        preview.first_prompt_snippet.as_deref(),
        Some("usable preview")
    );
    assert_eq!(preview.kind, CliSessionKind::Main);
}

#[test]
fn session_preview_reads_entrypoint_from_bookkeeping_before_prompt() {
    let temp = TempDir::new().unwrap();
    let sid = "92929292-9292-4929-8929-929292929292";
    let path = temp.path().join(format!("{sid}.jsonl"));
    let attachment = serde_json::json!({
        "type": "attachment",
        "sessionId": sid,
        "entrypoint": "sdk-py"
    });
    fs::write(
        &path,
        format!(
            "{attachment}\n{}",
            native_user_record(
                sid,
                "preview-user",
                "usable preview",
                "2026-07-20T20:00:00Z"
            )
        ),
    )
    .unwrap();

    let preview = read_session_preview(&path, sid);
    assert_eq!(preview.kind, CliSessionKind::Subagent);
    assert_eq!(
        preview.first_prompt_snippet.as_deref(),
        Some("usable preview")
    );
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

    let service = Arc::new(ClaudeTranscriptIngest::new(
        db.clone(),
        projects_dir.clone(),
    ));
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
    assert_eq!(
        CliIngestOutbox::published_seq(&db.pool, session.id)
            .await
            .unwrap(),
        300
    );

    let restarted = Arc::new(ClaudeTranscriptIngest::new(db, projects_dir));
    let mut restarted_updates = restarted.subscribe();
    let restarted_shutdown = CancellationToken::new();
    let restarted_publisher = tokio::spawn(
        restarted
            .clone()
            .run_publisher(restarted_shutdown.child_token()),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), restarted_updates.recv())
            .await
            .is_err(),
        "persisted publisher watermark must suppress restart history drain"
    );
    restarted_shutdown.cancel();
    restarted_publisher.await.unwrap();
}

#[tokio::test]
async fn publisher_prunes_outbox_rows_for_a_previous_session_owner() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, previous_session) = create_workspace_and_session(&db, &workspace_root).await;
    let current_session = create_session(&db, workspace.id).await;
    let cwd = effective_cwd(&workspace, &previous_session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let sid = "14141414-1414-4414-8414-141414141414";
    fs::write(
        native_dir.join(format!("{sid}.jsonl")),
        native_user_record(sid, "owner-change", "move me", "2026-07-20T20:00:00Z"),
    )
    .unwrap();
    ClaudeSessionLink::assign_manual(&db.pool, sid, previous_session.id, &cwd.to_string_lossy())
        .await
        .unwrap()
        .unwrap();
    let service = Arc::new(ClaudeTranscriptIngest::new(db.clone(), projects_dir));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();

    ClaudeSessionLink::assign_manual(&db.pool, sid, current_session.id, &cwd.to_string_lossy())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        CliIngestOutbox::prune_superseded(&db.pool).await.unwrap(),
        1
    );

    let previous_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM cli_ingest_outbox WHERE session_id = ?")
            .bind(previous_session.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let current_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM cli_ingest_outbox WHERE session_id = ?")
            .bind(current_session.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(previous_count, 0);
    assert_eq!(current_count, 1);
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
async fn sidechain_line_in_tracked_file_is_persisted_but_not_rendered() {
    let temp = TempDir::new().unwrap();
    let workspace_root = temp.path().join("worktree");
    let projects_dir = temp.path().join("projects");
    fs::create_dir_all(&workspace_root).unwrap();
    let db = test_db().await;
    let (workspace, session) = create_workspace_and_session(&db, &workspace_root).await;
    let cwd = effective_cwd(&workspace, &session).unwrap();
    let native_dir = store_dir(&projects_dir, &cwd);
    fs::create_dir_all(&native_dir).unwrap();
    let sid = "13131313-1313-4313-8313-131313131313";
    let raw = format!(
        "{}\n",
        serde_json::json!({
            "type": "user",
            "sessionId": sid,
            "uuid": "sidechain-user",
            "isSidechain": true,
            "timestamp": "2026-07-20T20:00:00Z",
            "message": { "role": "user", "content": "hidden agent work" }
        })
    );
    fs::write(native_dir.join(format!("{sid}.jsonl")), &raw).unwrap();
    ClaudeSessionLink::assign_manual(&db.pool, sid, session.id, &cwd.to_string_lossy())
        .await
        .unwrap()
        .unwrap();

    let service = Arc::new(ClaudeTranscriptIngest::new(db.clone(), projects_dir));
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();

    let rows = CliNativeRecord::list_for_session(&db.pool, session.id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].disposition,
        CliNativeRecordDisposition::Sidechain.as_str()
    );
    assert_eq!(rows[0].raw, raw.trim_end());
    assert!(
        service
            .snapshot(session.id)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
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
    assert!(user_row.bound_turn_execution_process_id.is_some());
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
    assert_eq!(row.bound_turn_execution_process_id, Some(newer_process));
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
    assert_eq!(
        user_row.bound_turn_execution_process_id,
        Some(older_process)
    );
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
    assert_eq!(old_row.bound_turn_execution_process_id, Some(process_id));

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
    assert_eq!(rows[0].bound_turn_execution_process_id, Some(process_id));
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
