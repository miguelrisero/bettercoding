use db::models::{
    cli_control_operation::{ControlRequest, LaunchIdentity, digest},
    cli_pane_binding::{CliPaneBinding, CliPaneBoundVia},
    session::{CreateSession, Session},
    workspace::{CreateWorkspace, Workspace},
};

use super::*;

async fn fixture() -> (sqlx::SqlitePool, ControlRequest) {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    db::run_migrations_for_tests(&pool).await.unwrap();
    let wid = Uuid::new_v4();
    Workspace::create(
        &pool,
        &CreateWorkspace {
            branch: "main".into(),
            name: None,
        },
        wid,
    )
    .await
    .unwrap();
    let session = Session::create(
        &pool,
        &CreateSession {
            executor: Some("CLAUDE_CODE".into()),
            name: None,
        },
        Uuid::new_v4(),
        wid,
    )
    .await
    .unwrap();
    let native = Uuid::new_v4().to_string();
    let binding = CliPaneBinding::record_launch(
        &pool,
        wid,
        session.id,
        Some(&native),
        CliPaneBoundVia::CliFresh,
    )
    .await
    .unwrap();
    let request = ControlRequest {
        request_id: "synthetic".into(),
        workspace_id: wid,
        session_id: session.id,
        binding_id: binding.id,
        launch: LaunchIdentity {
            launch_id: Uuid::new_v4(),
            native_session_id: native,
            socket: "synthetic".into(),
            tmux_session: "synthetic-session".into(),
            window: "@1".into(),
            pane: "%1".into(),
            pid: 42,
            process_start: "100".into(),
            executor: "CLAUDE_CODE".into(),
            cwd: "/synthetic".into(),
        },
        expected_model: "opus".into(),
        target_model: "sol".into(),
        requester: "owner".into(),
        confirmation_principal: "confirmation".into(),
        hook_capability_digest: digest("secret"),
        deadline_at: chrono::Utc::now() + chrono::Duration::minutes(1),
    };
    (pool, request)
}

#[tokio::test]
async fn admitted_write_finishes_before_reservation_and_all_later_writers_are_busy() {
    let (pool, request) = fixture().await;
    let guard = cli_writer_admission(&pool, request.workspace_id)
        .await
        .unwrap();
    let (started, ready) = tokio::sync::oneshot::channel();
    let task_pool = pool.clone();
    let task_request = request.clone();
    let reservation = tokio::spawn(async move {
        started.send(()).unwrap();
        reserve_cli_control(&task_pool, &task_request).await
    });
    ready.await.unwrap();
    tokio::task::yield_now().await;
    assert!(!reservation.is_finished());
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"manual terminal input");
    drop(guard);
    reservation.await.unwrap().unwrap();
    for _writer in [
        "attachment-1",
        "attachment-2",
        "composer",
        "MCP",
        "Input",
        "deferred",
        "supervisor",
        "rename",
        "lifecycle",
    ] {
        assert!(matches!(
            cli_writer_admission(&pool, request.workspace_id).await,
            Err(PtyError::ControlBusy(_))
        ));
    }
    assert_eq!(bytes, b"manual terminal input");
}

#[tokio::test]
async fn stale_binding_and_unavailable_admission_fail_closed() {
    let (pool, request) = fixture().await;
    let binding = CliPaneBinding::find_by_id(&pool, request.binding_id)
        .await
        .unwrap()
        .unwrap();
    CliPaneBinding::record_launch(
        &pool,
        request.workspace_id,
        request.session_id,
        Some(&request.launch.native_session_id),
        CliPaneBoundVia::CliResume,
    )
    .await
    .unwrap();
    assert!(matches!(
        validate_cli_binding(&pool, &binding).await,
        Err(PtyError::StaleBinding)
    ));
    pool.close().await;
    assert!(matches!(
        cli_writer_admission(&pool, request.workspace_id).await,
        Err(PtyError::AdmissionUnavailable(_))
    ));
}

#[tokio::test]
async fn application_sender_emits_zero_bytes_and_preserves_parked_prompt_when_busy() {
    let (pool, request) = fixture().await;
    Session::set_pending_cli_prompt_if_empty(&pool, request.session_id, "durable user prompt")
        .await
        .unwrap();
    reserve_cli_control(&pool, &request).await.unwrap();
    // The synthetic socket has no server. Busy must return before any native probe or write.
    let target = owned_cli_tmux_target_on(request.workspace_id, "synthetic-never-started");
    assert!(matches!(
        send_cli_keys_to_live_agent(&pool, &target, &["claude"], "message").await,
        Err(PtyError::ControlBusy(_))
    ));
    assert_eq!(
        Session::peek_pending_cli_prompt_for_workspace(&pool, request.workspace_id)
            .await
            .unwrap(),
        Some((request.session_id, "durable user prompt".into()))
    );
}

#[tokio::test]
async fn fresh_attachment_survives_first_native_identity_discovery() {
    let (pool, request) = fixture().await;
    let mut attached = CliPaneBinding::find_by_id(&pool, request.binding_id)
        .await
        .unwrap()
        .unwrap();
    attached.claude_session_id = None;
    validate_cli_binding(&pool, &attached).await.unwrap();
}

#[test]
fn attachment_identity_requires_exact_client_and_pane() {
    let listing = "42\t%1\n420\t%2\n";
    assert!(cli_client_matches_pane(listing, 42, "%1"));
    assert!(!cli_client_matches_pane(listing, 42, "%2"));
    assert!(!cli_client_matches_pane(listing, 4, "%1"));
}

#[tokio::test]
async fn reservation_denies_resume_launch_signal_before_filesystem_effects() {
    use services::services::cli_collab::CliPasteTransport;
    let (pool, request) = fixture().await;
    reserve_cli_control(&pool, &request).await.unwrap();
    let transport = crate::paste_transport::LocalCliPasteTransport { pool };
    assert!(
        transport
            .signal_resume_ready(request.workspace_id, &request.launch.native_session_id)
            .await
            .is_err()
    );
}
