//! Codex rollout import: identity, dedupe against the executor, and
//! incremental delivery through the shared native feed.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::{DateTime, SecondsFormat, Utc};
use db::{
    DBService,
    models::{
        claude_session_link::{ClaudeSessionBoundVia, ClaudeSessionLink},
        cli_native_record::CliNativeRecord,
        cli_pane_binding::{CliPaneBinding, CliPaneBoundVia},
        coding_agent_turn::{CodingAgentTurn, CreateCodingAgentTurn},
        execution_process::{CreateExecutionProcess, ExecutionProcess, ExecutionProcessRunReason},
        session::{CreateSession, Session},
        workspace::{CreateWorkspace, Workspace},
        workspace_cli_activity::{CliHookState, CliPhase, WorkspaceCliActivity},
    },
};
use executors::{
    actions::{
        ExecutorAction, ExecutorActionType, coding_agent_initial::CodingAgentInitialRequest,
    },
    executors::BaseCodingAgent,
    logs::NormalizedEntryType,
    profile::ExecutorConfig,
};
use serde_json::json;
use sqlx::sqlite::SqlitePoolOptions;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{ClaudeTranscriptIngest, NativeFeedChange, NativeFeedEntry, NativeFeedOrigin};

/// The installed Codex TUI's rollout (0.157.1), redacted; see the adapter
/// tests in the executors crate.
const FIXTURE: &str = include_str!(
    "../../../../executors/src/executors/codex/testdata/rollout-0.157.1.redacted.jsonl"
);
const FIXTURE_THREAD: &str = "01a0e7f5-9f42-73e0-9b4d-2dfa51b6f868";
const FIXTURE_ENTRIES: usize = 10;

struct Case {
    temp: TempDir,
    db: DBService,
    workspace: Workspace,
    session: Session,
    cwd: PathBuf,
    root: PathBuf,
    service: Arc<ClaudeTranscriptIngest>,
}

async fn codex_case() -> Case {
    let temp = TempDir::new().unwrap();
    let cwd = temp.path().join("worktree");
    fs::create_dir_all(&cwd).unwrap();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    db::run_migrations_for_tests(&pool).await.unwrap();
    let db = DBService { pool };
    let workspace_id = Uuid::new_v4();
    Workspace::create(
        &db.pool,
        &CreateWorkspace {
            branch: "main".to_string(),
            name: Some("codex ingest".to_string()),
        },
        workspace_id,
    )
    .await
    .unwrap();
    Workspace::update_container_ref(&db.pool, workspace_id, &cwd.to_string_lossy())
        .await
        .unwrap();
    let workspace = Workspace::find_by_id(&db.pool, workspace_id)
        .await
        .unwrap()
        .unwrap();
    let session = Session::create(
        &db.pool,
        &CreateSession {
            executor: Some("CODEX".to_string()),
            name: None,
        },
        Uuid::new_v4(),
        workspace_id,
    )
    .await
    .unwrap();
    let root = temp.path().join("codex").join("sessions");
    fs::create_dir_all(&root).unwrap();
    let mut service = ClaudeTranscriptIngest::new(db.clone(), temp.path().join("projects"));
    service.codex_sessions_dir = Some(root.clone());
    Case {
        temp,
        db,
        workspace,
        session,
        cwd,
        root,
        service: Arc::new(service),
    }
}

/// A UUIDv7 thread id created at `at`, distinguished by `n`.
fn thread_id_at(at: DateTime<Utc>, n: u16) -> String {
    let ms = at.timestamp_millis() as u64;
    format!(
        "{:08x}-{:04x}-7{:03x}-8{:03x}-{:012x}",
        ms >> 16,
        ms & 0xffff,
        n & 0xfff,
        n & 0xfff,
        n
    )
}

fn rollout_path(root: &Path, thread: &str, at: DateTime<Utc>) -> PathBuf {
    root.join(at.format("%Y/%m/%d").to_string()).join(format!(
        "rollout-{}-{thread}.jsonl",
        at.format("%Y-%m-%dT%H-%M-%S")
    ))
}

fn write_rollout(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn append(path: &Path, content: &str) {
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn session_meta(thread: &str, cwd: &Path, at: DateTime<Utc>, source: &str) -> String {
    format!(
        "{}\n",
        json!({
            "timestamp": stamp(at), "type": "session_meta",
            "payload": { "id": thread, "session_id": thread, "timestamp": stamp(at),
                         "cwd": cwd, "originator": "codex-tui", "cli_version": "0.157.1",
                         "source": source },
        })
    )
}

fn item(thread: &str, turn: &str, at: DateTime<Utc>, item: serde_json::Value) -> String {
    format!(
        "{}\n",
        json!({
            "timestamp": stamp(at), "type": "event_msg",
            "payload": { "type": "item_completed", "thread_id": thread, "turn_id": turn, "item": item },
        })
    )
}

fn user_turn(thread: &str, turn: &str, at: DateTime<Utc>, text: &str) -> String {
    [
        format!(
            "{}\n",
            json!({ "timestamp": stamp(at), "type": "event_msg",
                    "payload": { "type": "task_started", "turn_id": turn } })
        ),
        item(
            thread,
            turn,
            at,
            json!({ "type": "UserMessage", "id": format!("{turn}-user"),
                    "content": [{ "type": "text", "text": text, "text_elements": [] }] }),
        ),
        item(
            thread,
            turn,
            at,
            json!({ "type": "AgentMessage", "id": format!("{turn}-agent"),
                    "content": [{ "type": "Text", "text": format!("answer to {text}") }],
                    "phase": "final_answer" }),
        ),
        format!(
            "{}\n",
            json!({ "timestamp": stamp(at), "type": "event_msg",
                    "payload": { "type": "token_count", "info": null } })
        ),
    ]
    .concat()
}

fn fixture_created_at() -> DateTime<Utc> {
    "2026-09-28T12:20:29.122Z".parse().unwrap()
}

async fn bind_resumed(case: &Case, thread: &str) {
    CliPaneBinding::record_launch(
        &case.db.pool,
        case.workspace.id,
        case.session.id,
        Some(thread),
        CliPaneBoundVia::CliResume,
    )
    .await
    .unwrap();
    ClaudeSessionLink::assign_cli(
        &case.db.pool,
        thread,
        case.session.id,
        case.workspace.id,
        &case.cwd.to_string_lossy(),
        ClaudeSessionBoundVia::CliResume,
    )
    .await
    .unwrap();
}

async fn reconcile(case: &Case) {
    case.service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();
}

async fn entries(case: &Case) -> Vec<NativeFeedEntry> {
    case.service
        .snapshot(case.session.id)
        .await
        .unwrap()
        .entries
}

fn texts(entries: &[NativeFeedEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| entry.normalized_entry.content.clone())
        .collect()
}

async fn native_file_count(db: &DBService) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM cli_native_files")
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_resumed_codex_thread_renders_its_turns_and_stores_only_what_renders() {
    let case = codex_case().await;
    let created = fixture_created_at();
    let path = rollout_path(&case.root, FIXTURE_THREAD, created);
    write_rollout(&path, FIXTURE);
    // Another project's thread in the same day directory.
    let other = thread_id_at(created, 9);
    write_rollout(
        &rollout_path(&case.root, &other, created),
        &user_turn(&other, "other-turn", created, "someone else's prompt"),
    );
    bind_resumed(&case, FIXTURE_THREAD).await;

    reconcile(&case).await;

    let entries = entries(&case).await;
    assert_eq!(
        texts(&entries),
        [
            "Reply with OK only.",
            "OK",
            "List the files, then add a greeting to README.md.",
            "OK",
            "ls",
            "search",
            "markdown greeting",
            // Outside this test's worktree, so not made relative.
            "/workspace/demo/README.md",
            "Context compacted",
            "Added a greeting to README.md.",
        ]
    );
    assert!(entries.iter().all(|entry| {
        entry.origin == NativeFeedOrigin::Cli
            && entry.linked_execution_process_id.is_none()
            && entry.claude_session_id == FIXTURE_THREAD
            && entry.branch.is_none()
    }));
    assert!(matches!(
        entries[0].normalized_entry.entry_type,
        NormalizedEntryType::UserMessage
    ));
    let snapshot = case.service.snapshot(case.session.id).await.unwrap();
    assert!(snapshot.forks.is_empty());
    // Renderable lines plus the two unknown ones; bookkeeping is not stored.
    let file_id = CliNativeRecord::list_for_session(&case.db.pool, case.session.id)
        .await
        .unwrap()[0]
        .file_id;
    assert_eq!(
        CliNativeRecord::count_for_file(&case.db.pool, file_id)
            .await
            .unwrap(),
        12
    );
    assert_eq!(snapshot.health.unknown_kinds, 2);
    assert_eq!(
        native_file_count(&case.db).await,
        1,
        "only the bound thread is read"
    );
}

#[tokio::test]
async fn a_codex_append_reaches_the_feed_as_a_delta() {
    let case = codex_case().await;
    let created = fixture_created_at();
    let path = rollout_path(&case.root, FIXTURE_THREAD, created);
    write_rollout(&path, FIXTURE);
    bind_resumed(&case, FIXTURE_THREAD).await;
    reconcile(&case).await;
    let (_, cursor) = case
        .service
        .feed_since(case.session.id, None)
        .await
        .unwrap();

    let dir = path.parent().unwrap();
    // Nothing changed: the tracked file is skipped on its stat alone.
    assert_eq!(
        case.service
            .scan_directory(dir, false)
            .await
            .unwrap()
            .records,
        0
    );
    let later = created + chrono::Duration::minutes(5);
    append(
        &path,
        &user_turn(FIXTURE_THREAD, "turn-cli", later, "from the TUI"),
    );
    let scanned = case.service.scan_directory(dir, false).await.unwrap();
    assert_eq!(scanned.records, 2, "the token count is not stored");

    let (change, _) = case
        .service
        .feed_since(case.session.id, Some(cursor))
        .await
        .unwrap();
    let NativeFeedChange::Delta {
        appended_from,
        appended,
        replaced,
        forks,
        ..
    } = change
    else {
        panic!("an append must not resend the projection");
    };
    assert_eq!(appended_from, FIXTURE_ENTRIES);
    assert_eq!(texts(&appended), ["from the TUI", "answer to from the TUI"]);
    assert!(replaced.is_empty());
    assert!(forks.is_none());
}

#[tokio::test]
async fn turns_written_while_the_executor_ran_are_attributed_to_it() {
    let case = codex_case().await;
    let created = fixture_created_at();
    write_rollout(&rollout_path(&case.root, FIXTURE_THREAD, created), FIXTURE);
    bind_resumed(&case, FIXTURE_THREAD).await;
    // The executor ran the first turn on this thread (12:20:49.3 – 12:20:52.4).
    let on_thread = executor_run(
        &case,
        FIXTURE_THREAD,
        "2026-09-28 12:20:49.000+00:00",
        Some("2026-09-28 12:20:53.000+00:00"),
    )
    .await;
    // A chat follow-up forked another thread and is still running while the
    // pane's second turn (12:21:14) is written.
    let forked = executor_run(
        &case,
        &thread_id_at(created, 77),
        "2026-09-28 12:21:00.000+00:00",
        None,
    )
    .await;

    reconcile(&case).await;

    let entries = entries(&case).await;
    let (executor, cli): (Vec<_>, Vec<_>) = entries
        .iter()
        .partition(|entry| entry.linked_execution_process_id == Some(on_thread));
    assert_eq!(
        texts(&executor.into_iter().cloned().collect::<Vec<_>>()),
        ["Reply with OK only.", "OK"]
    );
    assert_eq!(cli.len(), FIXTURE_ENTRIES - 2);
    assert!(cli.iter().all(|entry| entry.origin == NativeFeedOrigin::Cli
        && entry.linked_execution_process_id.is_none()));
    assert!(
        entries
            .iter()
            .all(|entry| entry.linked_execution_process_id != Some(forked)),
        "a run on another thread never claims the pane's turns"
    );

    // A reset drops the run; its turns stay out of chat with it.
    sqlx::query("UPDATE execution_processes SET dropped = TRUE WHERE id = ?")
        .bind(on_thread)
        .execute(&case.db.pool)
        .await
        .unwrap();
    let still_claimed = CliNativeRecord::list_for_session(&case.db.pool, case.session.id)
        .await
        .unwrap()
        .into_iter()
        .filter(|row| row.linked_execution_process_id == Some(on_thread))
        .count();
    assert_eq!(still_claimed, 2);
}

/// A coding-agent run of this session on Codex `thread`, between the given
/// SQLite timestamps (`None`: still running).
async fn executor_run(
    case: &Case,
    thread: &str,
    started_at: &str,
    completed_at: Option<&str>,
) -> Uuid {
    let process_id = Uuid::new_v4();
    ExecutionProcess::create(
        &case.db.pool,
        &CreateExecutionProcess {
            session_id: case.session.id,
            executor_action: ExecutorAction::new(
                ExecutorActionType::CodingAgentInitialRequest(CodingAgentInitialRequest {
                    prompt: "chat prompt".to_string(),
                    executor_config: ExecutorConfig::new(BaseCodingAgent::Codex),
                    working_dir: None,
                }),
                None,
            ),
            run_reason: ExecutionProcessRunReason::CodingAgent,
        },
        process_id,
        &[],
    )
    .await
    .unwrap();
    CodingAgentTurn::create(
        &case.db.pool,
        &CreateCodingAgentTurn {
            execution_process_id: process_id,
            prompt: Some("chat prompt".to_string()),
        },
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    CodingAgentTurn::update_agent_session_id(&case.db.pool, process_id, thread)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE execution_processes SET status = ?, started_at = ?, completed_at = ? \
         WHERE id = ?",
    )
    .bind(if completed_at.is_some() {
        "completed"
    } else {
        "running"
    })
    .bind(started_at)
    .bind(completed_at)
    .bind(process_id)
    .execute(&case.db.pool)
    .await
    .unwrap();
    process_id
}

#[tokio::test]
async fn a_hook_naming_a_non_interactive_rollout_never_binds() {
    let pane = fresh_codex_pane().await;
    let exec = started_rollout(&pane, 11, chrono::Duration::seconds(2), "exec");
    report_hook(&pane.case, &exec).await;

    reconcile(&pane.case).await;

    assert!(
        ClaudeSessionLink::find(&pane.case.db.pool, &exec)
            .await
            .unwrap()
            .is_none()
    );
    assert!(entries(&pane.case).await.is_empty());
}

struct FreshPane {
    case: Case,
    binding: CliPaneBinding,
    launched: DateTime<Utc>,
}

async fn fresh_codex_pane() -> FreshPane {
    let case = codex_case().await;
    let binding = CliPaneBinding::record_launch(
        &case.db.pool,
        case.workspace.id,
        case.session.id,
        None,
        CliPaneBoundVia::CliFresh,
    )
    .await
    .unwrap();
    let launched = binding.created_at;
    FreshPane {
        case,
        binding,
        launched,
    }
}

/// A rollout the fresh pane could have started, `after` its launch.
fn started_rollout(pane: &FreshPane, n: u16, after: chrono::Duration, source: &str) -> String {
    let at = pane.launched + after;
    let thread = thread_id_at(at, n);
    write_rollout(
        &rollout_path(&pane.case.root, &thread, at),
        &[
            session_meta(&thread, &pane.case.cwd, at, source),
            user_turn(&thread, &format!("turn-{n}"), at, &format!("prompt {n}")),
        ]
        .concat(),
    );
    thread
}

async fn quarantined(case: &Case) -> u64 {
    case.service
        .snapshot(case.session.id)
        .await
        .unwrap()
        .health
        .quarantined_files
}

#[tokio::test]
async fn a_fresh_codex_pane_binds_its_only_matching_rollout() {
    let pane = fresh_codex_pane().await;
    // Not candidates: a non-interactive run in the same directory, and a
    // thread that started before the pane.
    started_rollout(&pane, 1, chrono::Duration::seconds(2), "exec");
    started_rollout(&pane, 2, chrono::Duration::minutes(-10), "cli");
    let thread = started_rollout(&pane, 3, chrono::Duration::seconds(3), "cli");

    reconcile(&pane.case).await;

    let link = ClaudeSessionLink::find(&pane.case.db.pool, &thread)
        .await
        .unwrap()
        .expect("the pane's thread is bound");
    assert_eq!(link.session_id, pane.case.session.id);
    assert_eq!(link.bound_via, ClaudeSessionBoundVia::CliFresh);
    let binding = CliPaneBinding::find_by_id(&pane.case.db.pool, pane.binding.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(binding.claude_session_id.as_deref(), Some(thread.as_str()));
    assert_eq!(
        texts(&entries(&pane.case).await),
        ["prompt 3", "answer to prompt 3"]
    );
    assert_eq!(quarantined(&pane.case).await, 0);
}

#[tokio::test]
async fn ambiguous_fresh_codex_rollouts_are_quarantined_until_the_hook_names_one() {
    let pane = fresh_codex_pane().await;
    let first = started_rollout(&pane, 4, chrono::Duration::seconds(2), "cli");
    let second = started_rollout(&pane, 5, chrono::Duration::seconds(4), "cli");

    reconcile(&pane.case).await;

    for thread in [&first, &second] {
        assert!(
            ClaudeSessionLink::find(&pane.case.db.pool, thread)
                .await
                .unwrap()
                .is_none(),
            "an ambiguous match must not bind"
        );
    }
    assert!(entries(&pane.case).await.is_empty());
    assert_eq!(quarantined(&pane.case).await, 2);
    assert_eq!(native_file_count(&pane.case.db).await, 0);

    // The pane's own hook reports which thread it runs.
    let second_path = rollout_path(
        &pane.case.root,
        &second,
        pane.launched + chrono::Duration::seconds(4),
    );
    WorkspaceCliActivity::upsert_hook(
        &pane.case.db.pool,
        pane.case.workspace.id,
        &CliHookState {
            agent_session_id: second.clone(),
            transcript_path: Some(second_path.to_string_lossy().into_owned()),
            phase: CliPhase::Working,
            tasks: None,
            crons: None,
            seq: 1,
        },
        false,
        Utc::now(),
    )
    .await
    .unwrap();
    reconcile(&pane.case).await;

    assert!(
        ClaudeSessionLink::find(&pane.case.db.pool, &second)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        ClaudeSessionLink::find(&pane.case.db.pool, &first)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        texts(&entries(&pane.case).await),
        ["prompt 5", "answer to prompt 5"]
    );
    assert_eq!(quarantined(&pane.case).await, 1);
    drop(pane.case.temp);
}

async fn report_hook(case: &Case, thread: &str) {
    WorkspaceCliActivity::upsert_hook(
        &case.db.pool,
        case.workspace.id,
        &CliHookState {
            agent_session_id: thread.to_string(),
            transcript_path: None,
            phase: CliPhase::Working,
            tasks: None,
            crons: None,
            seq: Utc::now().timestamp_nanos_opt().unwrap(),
        },
        false,
        Utc::now(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_hook_naming_a_thread_older_than_the_pane_never_binds() {
    let pane = fresh_codex_pane().await;
    // An earlier pane's thread, reported after this pane launched.
    let earlier = started_rollout(&pane, 6, chrono::Duration::minutes(-10), "cli");
    report_hook(&pane.case, &earlier).await;
    let own = started_rollout(&pane, 7, chrono::Duration::seconds(2), "cli");

    reconcile(&pane.case).await;

    assert!(
        ClaudeSessionLink::find(&pane.case.db.pool, &earlier)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        ClaudeSessionLink::find(&pane.case.db.pool, &own)
            .await
            .unwrap()
            .map(|link| link.session_id),
        Some(pane.case.session.id)
    );
}

#[tokio::test]
async fn a_fresh_pane_is_matched_in_its_own_sessions_directory() {
    let pane = fresh_codex_pane().await;
    // A newer session in the workspace runs in a subdirectory; the pane's
    // own (older) session runs at the worktree root.
    fs::create_dir_all(pane.case.cwd.join("sub")).unwrap();
    let newer = Session::create(
        &pane.case.db.pool,
        &CreateSession {
            executor: Some("CODEX".to_string()),
            name: None,
        },
        Uuid::new_v4(),
        pane.case.workspace.id,
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE sessions SET agent_working_dir = 'sub', \
         created_at = datetime('now', '+1 minute') WHERE id = ?",
    )
    .bind(newer.id)
    .execute(&pane.case.db.pool)
    .await
    .unwrap();
    let own = started_rollout(&pane, 8, chrono::Duration::seconds(2), "cli");

    reconcile(&pane.case).await;

    let link = ClaudeSessionLink::find(&pane.case.db.pool, &own)
        .await
        .unwrap()
        .expect("the pane's thread is found in the pane's directory");
    assert_eq!(link.session_id, pane.case.session.id);
    assert_eq!(link.cwd, pane.case.cwd.to_string_lossy());
}

#[tokio::test]
async fn a_thread_already_linked_to_the_pane_session_binds() {
    let pane = fresh_codex_pane().await;
    let own = started_rollout(&pane, 10, chrono::Duration::seconds(2), "cli");
    // Linked earlier (a CLI launch, a manual assignment) but not yet bound.
    ClaudeSessionLink::assign_cli(
        &pane.case.db.pool,
        &own,
        pane.case.session.id,
        pane.case.workspace.id,
        &pane.case.cwd.to_string_lossy(),
        ClaudeSessionBoundVia::CliFresh,
    )
    .await
    .unwrap();

    reconcile(&pane.case).await;

    let binding = CliPaneBinding::find_by_id(&pane.case.db.pool, pane.binding.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(binding.claude_session_id.as_deref(), Some(own.as_str()));
    assert_eq!(
        texts(&entries(&pane.case).await),
        ["prompt 10", "answer to prompt 10"]
    );
}

#[tokio::test]
async fn fresh_pane_assignment_writes_nothing_for_a_taken_thread_or_released_pane() {
    let pane = fresh_codex_pane().await;
    let pool = &pane.case.db.pool;
    let other = Session::create(
        pool,
        &CreateSession {
            executor: Some("CODEX".to_string()),
            name: None,
        },
        Uuid::new_v4(),
        pane.case.workspace.id,
    )
    .await
    .unwrap();
    let taken = thread_id_at(pane.launched, 12);
    let cwd = pane.case.cwd.to_string_lossy().into_owned();
    ClaudeSessionLink::assign_cli(
        pool,
        &taken,
        other.id,
        pane.case.workspace.id,
        &cwd,
        ClaudeSessionBoundVia::CliFresh,
    )
    .await
    .unwrap();

    // Another session took the thread after the candidate was chosen.
    let assigned = CliPaneBinding::assign_discovered_session(
        pool,
        pane.binding.id,
        &taken,
        pane.case.session.id,
        pane.case.workspace.id,
        &cwd,
    )
    .await
    .unwrap();
    assert!(assigned.is_none());
    let link = ClaudeSessionLink::find(pool, &taken)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link.session_id, other.id, "the existing owner is kept");
    let binding = CliPaneBinding::find_by_id(pool, pane.binding.id)
        .await
        .unwrap()
        .unwrap();
    assert!(binding.claude_session_id.is_none());

    // The pane was released after the candidate was chosen.
    CliPaneBinding::release(pool, pane.binding.id)
        .await
        .unwrap();
    let free = thread_id_at(pane.launched, 13);
    let assigned = CliPaneBinding::assign_discovered_session(
        pool,
        pane.binding.id,
        &free,
        pane.case.session.id,
        pane.case.workspace.id,
        &cwd,
    )
    .await
    .unwrap();
    assert!(assigned.is_none());
    assert!(
        ClaudeSessionLink::find(pool, &free)
            .await
            .unwrap()
            .is_none()
    );
}

/// Measures a Codex rollout backfill and per-append cost against a copy of a
/// real database while an API-shaped load runs. `CODEX_MEASURE_DB` is a COPY
/// (the run writes to it), `CODEX_MEASURE_ROLLOUT` a real rollout (only
/// read), and `CODEX_MEASURE_ROOT` a scratch sessions root the rollout is
/// replayed into: all but its last ten lines, then one line per append.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual measurement against a copied database"]
async fn measure_codex_on_copied_database() {
    use std::{io::BufRead, time::Duration as StdDuration};

    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};

    let db_path = std::env::var("CODEX_MEASURE_DB").expect("CODEX_MEASURE_DB");
    let source = PathBuf::from(std::env::var("CODEX_MEASURE_ROLLOUT").expect("rollout"));
    let root = PathBuf::from(std::env::var("CODEX_MEASURE_ROOT").expect("root"));
    let options = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(StdDuration::from_secs(30))
        .synchronous(SqliteSynchronous::Normal);
    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .unwrap();
    db::run_migrations_for_tests(&pool).await.unwrap();
    let db = DBService { pool };

    let file_name = source.file_name().unwrap().to_str().unwrap().to_string();
    let thread = super::rollout_thread_id(&file_name).unwrap().to_string();
    let created = super::codex_thread_created_at(&thread).unwrap();
    let target = super::codex_day_dir(&root, created).join(&file_name);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    let lines = std::io::BufReader::new(fs::File::open(&source).unwrap())
        .lines()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    let (history, tail) = lines.split_at(lines.len() - 10);
    fs::write(&target, history.join("\n") + "\n").unwrap();

    // Bind the thread to the live workspace with the most sessions.
    let (workspace_id, session_id): (Uuid, Uuid) = sqlx::query_as(
        "SELECT w.id, (SELECT s.id FROM sessions s WHERE s.workspace_id = w.id
                        ORDER BY s.created_at DESC LIMIT 1)
         FROM workspaces w WHERE w.archived = 0 AND w.container_ref IS NOT NULL
           AND EXISTS (SELECT 1 FROM sessions s WHERE s.workspace_id = w.id)
         ORDER BY (SELECT COUNT(*) FROM sessions s WHERE s.workspace_id = w.id) DESC LIMIT 1",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    CliPaneBinding::record_launch(
        &db.pool,
        workspace_id,
        session_id,
        Some(&thread),
        CliPaneBoundVia::CliResume,
    )
    .await
    .unwrap();
    ClaudeSessionLink::assign_cli(
        &db.pool,
        &thread,
        session_id,
        workspace_id,
        "/measure",
        ClaudeSessionBoundVia::CliResume,
    )
    .await
    .unwrap();

    let done = CancellationToken::new();
    let load_db = db.clone();
    let load_done = done.clone();
    let load = tokio::spawn(async move {
        let mut latencies = Vec::new();
        let mut busy = 0u64;
        while !load_done.is_cancelled() {
            let started = std::time::Instant::now();
            let read = sqlx::query("SELECT * FROM workspaces ORDER BY updated_at DESC")
                .fetch_all(&load_db.pool)
                .await
                .map(|_| ());
            let write = sqlx::query("UPDATE workspaces SET updated_at = updated_at WHERE id = ?")
                .bind(workspace_id)
                .execute(&load_db.pool)
                .await
                .map(|_| ());
            latencies.push(started.elapsed());
            for result in [read, write] {
                if let Err(sqlx::Error::Database(error)) = result
                    && matches!(error.code().as_deref(), Some("5") | Some("517"))
                {
                    busy += 1;
                }
            }
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }
        (latencies, busy)
    });

    let empty_projects = root.join("no-claude-projects");
    let mut service = ClaudeTranscriptIngest::new(db.clone(), empty_projects);
    service.codex_sessions_dir = Some(root.clone());
    let service = Arc::new(service);
    let started = std::time::Instant::now();
    let counts = service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();
    let backfill = started.elapsed();
    let started = std::time::Instant::now();
    service
        .reconcile_registry(false, CancellationToken::new())
        .await
        .unwrap();
    let idle_reconcile = started.elapsed();
    done.cancel();
    let (mut latencies, busy) = load.await.unwrap();
    latencies.sort();
    let percentile = |p: usize| latencies[(latencies.len() * p / 100).min(latencies.len() - 1)];
    println!(
        "codex backfill {backfill:?}: {} history lines, {} files, {} records stored; \
         idle reconcile {idle_reconcile:?}; API-shaped requests {} (p50 {:?}, p95 {:?}, max {:?}); \
         SQLITE_BUSY {busy}",
        history.len(),
        counts.files,
        counts.records,
        latencies.len(),
        percentile(50),
        percentile(95),
        latencies.last().unwrap(),
    );

    let _subscription = service.subscribe_feed(session_id);
    let started = std::time::Instant::now();
    let (change, mut cursor) = service.feed_since(session_id, None).await.unwrap();
    let full = started.elapsed();
    let NativeFeedChange::Full(snapshot) = change else {
        panic!("first update is a snapshot");
    };
    println!(
        "feed: {} entries ({} cli), {} snapshot bytes, full build {full:?}",
        snapshot.entries.len(),
        snapshot
            .entries
            .iter()
            .filter(|entry| entry.origin == NativeFeedOrigin::Cli)
            .count(),
        serde_json::to_vec(&snapshot).unwrap().len(),
    );
    let dir = target.parent().unwrap().to_path_buf();
    let started = std::time::Instant::now();
    service.scan_directory(&dir, false).await.unwrap();
    println!("unchanged scan (watcher noise) {:?}", started.elapsed());
    for line in tail {
        append(&target, &format!("{line}\n"));
        let started = std::time::Instant::now();
        let scanned = service.scan_directory(&dir, false).await.unwrap();
        let import = started.elapsed();
        let started = std::time::Instant::now();
        let (change, next) = service.feed_since(session_id, Some(cursor)).await.unwrap();
        let update = started.elapsed();
        cursor = next;
        let (kind, bytes) = match &change {
            NativeFeedChange::Delta { appended, .. } => {
                ("delta", serde_json::to_vec(appended).unwrap().len())
            }
            NativeFeedChange::Full(snapshot) => {
                ("FULL", serde_json::to_vec(snapshot).unwrap().len())
            }
        };
        println!(
            "  append {} B line: import {import:?} ({} stored), feed update {update:?} ({kind}, {bytes} B)",
            line.len(),
            scanned.records,
        );
    }
}

struct CodexPaneProbe {
    agent_running: bool,
}

#[async_trait::async_trait]
impl crate::services::cli_collab::CliWriterProbe for CodexPaneProbe {
    async fn probe(
        &self,
        _workspace_id: Uuid,
        _effective_dir: &Path,
        _expected_sid: Option<&str>,
        _binding: Option<&CliPaneBinding>,
        _check_cwd_uniqueness: bool,
    ) -> crate::services::cli_collab::ProbeReport {
        crate::services::cli_collab::ProbeReport {
            pane_session_exists: true,
            agent_running: Some(self.agent_running),
            agent_program: self.agent_running.then(|| "codex".to_string()),
            sid_evidence: crate::services::cli_collab::SidEvidence::NoResumeArg,
            probe_failed: false,
            only_active_claude_in_cwd: None,
        }
    }
}

/// A user turn in a linked Codex thread while no app pane agent runs and no
/// executor runs came from a writer outside the app.
async fn codex_foreign_writer_flagged(pane_agent_running: bool) -> bool {
    let mut case = codex_case().await;
    let mut service = ClaudeTranscriptIngest::new_with_probe(
        case.db.clone(),
        case.temp.path().join("projects"),
        Arc::new(CodexPaneProbe {
            agent_running: pane_agent_running,
        }),
    );
    service.codex_sessions_dir = Some(case.root.clone());
    case.service = Arc::new(service);
    let created = fixture_created_at();
    let thread = thread_id_at(created, 7);
    write_rollout(
        &rollout_path(&case.root, &thread, created),
        &[
            session_meta(&thread, &case.cwd, created, "cli"),
            user_turn(
                &thread,
                "foreign-turn",
                created,
                "typed in another terminal",
            ),
        ]
        .concat(),
    );
    bind_resumed(&case, &thread).await;

    reconcile(&case).await;

    assert_eq!(
        texts(&entries(&case).await),
        [
            "typed in another terminal",
            "answer to typed in another terminal"
        ]
    );
    ClaudeSessionLink::find(&case.db.pool, &thread)
        .await
        .unwrap()
        .unwrap()
        .foreign_writer_seen_at
        .is_some()
}

#[tokio::test]
async fn a_codex_turn_written_outside_the_app_pane_marks_a_foreign_writer() {
    assert!(codex_foreign_writer_flagged(false).await);
    assert!(!codex_foreign_writer_flagged(true).await);
}
