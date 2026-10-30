use super::*;
use crate::models::{
    cli_pane_binding::{CliPaneBinding, CliPaneBoundVia},
    session::{CreateSession, Session},
    workspace::{CreateWorkspace, Workspace},
    workspace_spawn_reservation::{SpawnReservationHolder, WorkspaceSpawnReservation},
};

async fn fixture(pool: &SqlitePool) -> ControlRequest {
    crate::run_migrations_for_tests(pool).await.unwrap();
    let workspace_id = Uuid::new_v4();
    Workspace::create(
        pool,
        &CreateWorkspace {
            branch: "main".into(),
            name: None,
        },
        workspace_id,
    )
    .await
    .unwrap();
    let session = Session::create(
        pool,
        &CreateSession {
            executor: Some("CLAUDE_CODE".into()),
            name: None,
        },
        Uuid::new_v4(),
        workspace_id,
    )
    .await
    .unwrap();
    let native_session_id = Uuid::new_v4().to_string();
    let binding = CliPaneBinding::record_launch(
        pool,
        workspace_id,
        session.id,
        Some(&native_session_id),
        CliPaneBoundVia::CliFresh,
    )
    .await
    .unwrap();
    ControlRequest {
        request_id: "request-1".into(),
        workspace_id,
        session_id: session.id,
        binding_id: binding.id,
        launch: LaunchIdentity {
            launch_id: Uuid::new_v4(),
            native_session_id,
            socket: "synthetic".into(),
            tmux_session: "synthetic-session".into(),
            window: "@1".into(),
            pane: "%1".into(),
            pid: 42,
            process_start: "100".into(),
            executor: "CLAUDE_CODE".into(),
            cwd: "/synthetic".into(),
        },
        expected_model: "opus-canonical".into(),
        target_model: "sol-canonical".into(),
        requester: "owner".into(),
        confirmation_principal: "owner-confirmation".into(),
        hook_capability_digest: digest("launch-secret"),
        deadline_at: Utc::now() + chrono::Duration::minutes(1),
    }
}

async fn pool() -> SqlitePool {
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap()
}

fn raw(op: &CliControlOperation, event: ModelEvent) -> RawReceipt {
    RawReceipt {
        receipt_id: "receipt-1".into(),
        operation_id: op.operation_id,
        request_digest: op.request_digest.clone(),
        binding_id: op.request().unwrap().binding_id,
        launch: op.request().unwrap().launch,
        event,
        source: "command".into(),
        from_model: "opus-canonical".into(),
        to_model: "sol-canonical".into(),
    }
}

#[tokio::test]
async fn identical_retry_and_conflicting_digest_keep_one_operation() {
    let pool = pool().await;
    let mut request = fixture(&pool).await;
    let op = CliControlOperation::reserve(&pool, &request).await.unwrap();
    assert_eq!(
        CliControlOperation::reserve(&pool, &request)
            .await
            .unwrap()
            .operation_id,
        op.operation_id
    );
    request.target_model = "other".into();
    assert!(CliControlOperation::reserve(&pool, &request).await.is_err());
    request.request_id = "other-request".into();
    assert!(CliControlOperation::reserve(&pool, &request).await.is_err());
    assert!(
        CliControlOperation::ensure_unfenced(&pool, request.workspace_id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn concurrent_reservations_have_one_winner() {
    let pool = pool().await;
    let first = fixture(&pool).await;
    let mut second = first.clone();
    second.request_id = "request-2".into();
    let (a, b) = tokio::join!(
        CliControlOperation::reserve(&pool, &first),
        CliControlOperation::reserve(&pool, &second)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
}

#[tokio::test]
async fn fence_survives_reopen_and_spawn_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.path().join("control.sqlite"))
        .create_if_missing(true);
    let pool = sqlx::SqlitePool::connect_with(options.clone())
        .await
        .unwrap();
    let request = fixture(&pool).await;
    let op = CliControlOperation::reserve(&pool, &request).await.unwrap();
    pool.close().await;
    let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
    assert!(
        CliControlOperation::ensure_unfenced(&pool, request.workspace_id)
            .await
            .is_err()
    );
    assert!(
        WorkspaceSpawnReservation::acquire(
            &pool,
            request.workspace_id,
            SpawnReservationHolder::Cli
        )
        .await
        .is_err()
    );
    let mut stale = request.clone();
    stale.deadline_at = Utc::now() - chrono::Duration::days(1);
    assert!(CliControlOperation::reserve(&pool, &stale).await.is_err());
    assert_eq!(
        CliControlOperation::find(&pool, op.operation_id)
            .await
            .unwrap()
            .unwrap()
            .phase,
        "reserved"
    );
}

#[tokio::test]
async fn binding_mutations_fail_but_exact_discovery_is_allowed() {
    let pool = pool().await;
    let request = fixture(&pool).await;
    CliControlOperation::reserve(&pool, &request).await.unwrap();
    assert!(
        CliPaneBinding::release(&pool, request.binding_id)
            .await
            .is_err()
    );
    assert!(
        CliPaneBinding::record_launch(
            &pool,
            request.workspace_id,
            request.session_id,
            Some(&request.launch.native_session_id),
            CliPaneBoundVia::CliResume
        )
        .await
        .is_err()
    );
    assert!(
        sqlx::query("UPDATE cli_pane_bindings SET claude_session_id = ? WHERE id = ?")
            .bind("different")
            .bind(request.binding_id)
            .execute(&pool)
            .await
            .is_err()
    );
    sqlx::query("UPDATE cli_pane_bindings SET claude_session_id = ? WHERE id = ?")
        .bind(&request.launch.native_session_id)
        .bind(request.binding_id)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn authentication_binds_capability_launch_pid_start_and_model_edge() {
    let pool = pool().await;
    let request = fixture(&pool).await;
    let op = CliControlOperation::reserve(&pool, &request).await.unwrap();
    let receipt = raw(&op, ModelEvent::Pre);
    assert!(
        op.authenticate(receipt.clone(), "forged", &request.launch)
            .is_err()
    );
    let mut wrong = request.launch.clone();
    wrong.process_start = "101".into();
    assert!(
        op.authenticate(receipt.clone(), "launch-secret", &wrong)
            .is_err()
    );
    wrong = request.launch.clone();
    wrong.launch_id = Uuid::new_v4();
    assert!(
        op.authenticate(receipt.clone(), "launch-secret", &wrong)
            .is_err()
    );
    let mut wrong = receipt.clone();
    wrong.to_model = "alias-or-wrong-model".into();
    assert!(
        op.authenticate(wrong, "launch-secret", &request.launch)
            .is_err()
    );
    let mut wrong = receipt.clone();
    wrong.source = "auto".into();
    assert!(
        op.authenticate(wrong, "launch-secret", &request.launch)
            .is_err()
    );
    assert!(
        op.authenticate(receipt, "launch-secret", &request.launch)
            .is_ok()
    );
}

#[tokio::test]
async fn reordered_delayed_duplicate_and_unconfirmed_receipts_cannot_apply() {
    for event in [ModelEvent::Pre, ModelEvent::Post] {
        let pool = pool().await;
        let request = fixture(&pool).await;
        let op = CliControlOperation::reserve(&pool, &request).await.unwrap();
        let receipt = op
            .authenticate(raw(&op, event), "launch-secret", &request.launch)
            .unwrap();
        let reduced = CliControlOperation::record_receipt(&pool, receipt)
            .await
            .unwrap();
        assert_eq!(reduced.phase, "reconcile_required");
        let duplicate = op
            .authenticate(raw(&op, event), "launch-secret", &request.launch)
            .unwrap();
        assert_eq!(
            CliControlOperation::record_receipt(&pool, duplicate)
                .await
                .unwrap()
                .phase,
            "reconcile_required"
        );
        let mut conflict = raw(&op, ModelEvent::Post);
        conflict.event = if event == ModelEvent::Pre {
            ModelEvent::Post
        } else {
            ModelEvent::Pre
        };
        let conflict = op
            .authenticate(conflict, "launch-secret", &request.launch)
            .unwrap();
        assert!(
            CliControlOperation::record_receipt(&pool, conflict)
                .await
                .is_err()
        );
        assert!(
            CliControlOperation::cancel_undispatched(&pool, op.operation_id)
                .await
                .is_err()
        );
        assert!(
            CliControlOperation::ensure_unfenced(&pool, request.workspace_id)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn synchronous_pre_requires_dispatch_and_post_requires_confirmation() {
    let pool = pool().await;
    let request = fixture(&pool).await;
    let op = CliControlOperation::reserve(&pool, &request).await.unwrap();
    // Synthetic evidence only: production exposes no dispatch or confirmation setter.
    sqlx::query("UPDATE cli_control_operations SET phase = 'command_sent' WHERE operation_id = ?")
        .bind(op.operation_id)
        .execute(&pool)
        .await
        .unwrap();
    let pre = op
        .authenticate(raw(&op, ModelEvent::Pre), "launch-secret", &request.launch)
        .unwrap();
    assert_eq!(
        CliControlOperation::record_receipt(&pool, pre)
            .await
            .unwrap()
            .phase,
        "awaiting_confirmation"
    );
    let mut post = raw(&op, ModelEvent::Post);
    post.receipt_id = "post".into();
    let post = op
        .authenticate(post, "launch-secret", &request.launch)
        .unwrap();
    assert_eq!(
        CliControlOperation::record_receipt(&pool, post)
            .await
            .unwrap()
            .phase,
        "reconcile_required"
    );
}

#[tokio::test]
async fn only_undispatched_operation_with_zero_receipts_can_cancel() {
    let pool = pool().await;
    let request = fixture(&pool).await;
    let op = CliControlOperation::reserve(&pool, &request).await.unwrap();
    CliControlOperation::cancel_undispatched(&pool, op.operation_id)
        .await
        .unwrap();
    CliControlOperation::ensure_unfenced(&pool, request.workspace_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn unfinished_handover_denies_reservation() {
    let pool = pool().await;
    let request = fixture(&pool).await;
    sqlx::query("INSERT INTO session_handovers (operation_id, workspace_id, request_id, request_json, predecessor_session_id, successor_session_id, predecessor_binding_id, binding_id, successor_native_session_id, fallback_session_id, capability, phase, deadline_at, launch_json) VALUES (?, ?, 'handover', '{}', ?, ?, ?, ?, 'successor', 'fallback', 'separate-capability', 'starting', ?, '{}')")
        .bind(Uuid::new_v4()).bind(request.workspace_id).bind(request.session_id).bind(request.session_id).bind(request.binding_id).bind(request.binding_id).bind(request.deadline_at).execute(&pool).await.unwrap();
    assert!(CliControlOperation::reserve(&pool, &request).await.is_err());
}

#[tokio::test]
async fn expired_spawn_reservation_never_releases_control_custody() {
    let pool = pool().await;
    let request = fixture(&pool).await;
    let expired = WorkspaceSpawnReservation::acquire_at(
        &pool,
        request.workspace_id,
        SpawnReservationHolder::Cli,
        Utc::now() - chrono::Duration::minutes(1),
        WorkspaceSpawnReservation::DEFAULT_TTL,
    )
    .await
    .unwrap()
    .unwrap();
    CliControlOperation::reserve(&pool, &request).await.unwrap();
    assert!(
        WorkspaceSpawnReservation::acquire(
            &pool,
            request.workspace_id,
            SpawnReservationHolder::Cli
        )
        .await
        .is_err()
    );
    WorkspaceSpawnReservation::release(&pool, request.workspace_id, &expired.fence)
        .await
        .unwrap();
    assert!(
        CliControlOperation::ensure_unfenced(&pool, request.workspace_id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn delayed_post_and_wrong_confirmation_keep_custody() {
    for expired in [false, true] {
        let pool = pool().await;
        let request = fixture(&pool).await;
        let op = CliControlOperation::reserve(&pool, &request).await.unwrap();
        sqlx::query("UPDATE cli_control_operations SET phase = 'awaiting_applied', pre_receipt_id = 'pre', confirmation_receipt_id = 'confirmation', confirmation_principal = ? WHERE operation_id = ?")
            .bind(if expired { "owner-confirmation" } else { "hook-reporter" }).bind(op.operation_id).execute(&pool).await.unwrap();
        let mut receipt = op
            .authenticate(raw(&op, ModelEvent::Post), "launch-secret", &request.launch)
            .unwrap();
        if expired {
            receipt.observed_at = request.deadline_at;
        }
        assert_eq!(
            CliControlOperation::record_receipt(&pool, receipt)
                .await
                .unwrap()
                .phase,
            "reconcile_required"
        );
        assert!(
            CliControlOperation::ensure_unfenced(&pool, request.workspace_id)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn correlated_post_reduces_only_with_separate_confirmation_evidence() {
    let pool = pool().await;
    let request = fixture(&pool).await;
    let op = CliControlOperation::reserve(&pool, &request).await.unwrap();
    // CI seeds unavailable dispatch/confirmation evidence. Production has zero setters.
    sqlx::query("UPDATE cli_control_operations SET phase = 'awaiting_applied', pre_receipt_id = 'pre', confirmation_receipt_id = 'owner-confirmation', confirmation_principal = ? WHERE operation_id = ?")
        .bind(&request.confirmation_principal).bind(op.operation_id).execute(&pool).await.unwrap();
    let receipt = op
        .authenticate(raw(&op, ModelEvent::Post), "launch-secret", &request.launch)
        .unwrap();
    assert_eq!(
        CliControlOperation::record_receipt(&pool, receipt)
            .await
            .unwrap()
            .phase,
        "applied"
    );
    CliControlOperation::ensure_unfenced(&pool, request.workspace_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn immutable_operation_identity_cannot_be_replaced() {
    let pool = pool().await;
    let request = fixture(&pool).await;
    let op = CliControlOperation::reserve(&pool, &request).await.unwrap();
    for statement in [
        "UPDATE cli_control_operations SET request_digest = 'forged' WHERE operation_id = ?",
        "UPDATE cli_control_operations SET request_json = '{}' WHERE operation_id = ?",
        "UPDATE cli_control_operations SET binding_id = randomblob(16) WHERE operation_id = ?",
    ] {
        assert!(
            sqlx::query(statement)
                .bind(op.operation_id)
                .execute(&pool)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn running_executor_denies_control_reservation() {
    let pool = pool().await;
    let request = fixture(&pool).await;
    sqlx::query("INSERT INTO execution_processes (id, session_id, run_reason, executor_action, status, started_at) VALUES (?, ?, 'codingagent', '{}', 'running', ?)")
        .bind(Uuid::new_v4()).bind(request.session_id).bind(Utc::now()).execute(&pool).await.unwrap();
    assert!(CliControlOperation::reserve(&pool, &request).await.is_err());
}
