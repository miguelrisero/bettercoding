use std::time::Duration as StdDuration;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use chrono::{Duration, Utc};
use db::models::{
    archive_bucket::ArchiveBucket,
    session::{CreateSession, Session},
    workspace::{CreateWorkspace, Workspace},
};
use deployment::Deployment;
use local_deployment::LocalDeployment;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

async fn create_workspace(deployment: &LocalDeployment, name: &str) -> Workspace {
    Workspace::create(
        &deployment.db().pool,
        &CreateWorkspace {
            branch: format!("test-{name}"),
            name: Some(name.to_string()),
        },
        Uuid::new_v4(),
    )
    .await
    .unwrap()
}

async fn create_archived_workspace(
    deployment: &LocalDeployment,
    name: &str,
    age: Duration,
) -> Workspace {
    let workspace = create_workspace(deployment, name).await;
    Workspace::set_archived(&deployment.db().pool, workspace.id, true)
        .await
        .unwrap();
    sqlx::query("UPDATE workspaces SET archived_at = ? WHERE id = ?")
        .bind(Utc::now() - age)
        .bind(workspace.id)
        .execute(&deployment.db().pool)
        .await
        .unwrap();
    Workspace::find_by_id(&deployment.db().pool, workspace.id)
        .await
        .unwrap()
        .unwrap()
}

async fn add_running_process(deployment: &LocalDeployment, workspace_id: Uuid) {
    let session = Session::create(
        &deployment.db().pool,
        &CreateSession {
            executor: None,
            name: Some("running process".to_string()),
        },
        Uuid::new_v4(),
        workspace_id,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO execution_processes (id, session_id, run_reason, executor_action, status) VALUES (?, ?, 'codingagent', '{}', 'running')",
    )
    .bind(Uuid::new_v4())
    .bind(session.id)
    .execute(&deployment.db().pool)
    .await
    .unwrap();
}

fn target(workspace_id: Uuid, archived_at: Value) -> Value {
    json!({
        "workspace_id": workspace_id,
        "archived_at": archived_at,
    })
}

fn workspace_target(workspace: &Workspace) -> Value {
    target(workspace.id, json!(workspace.archived_at))
}

async fn submit_bulk_delete(app: Router, targets: Vec<Value>) -> Vec<Value> {
    let response = app
        .oneshot(
            Request::post("/workspaces/archived/bulk-delete")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "targets": targets,
                        "delete_branches": true,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload["success"], true);
    payload["data"]["results"].as_array().unwrap().clone()
}

fn result_for(results: &[Value], workspace_id: Uuid) -> &Value {
    results
        .iter()
        .find(|result| result["workspace_id"] == workspace_id.to_string())
        .unwrap_or_else(|| panic!("missing result for workspace {workspace_id}"))
}

async fn assert_workspace_exists(deployment: &LocalDeployment, workspace_id: Uuid) {
    assert!(
        Workspace::find_by_id(&deployment.db().pool, workspace_id)
            .await
            .unwrap()
            .is_some(),
        "workspace {workspace_id} must remain"
    );
}

async fn assert_workspace_deleted(deployment: &LocalDeployment, workspace_id: Uuid) {
    assert!(
        Workspace::find_by_id(&deployment.db().pool, workspace_id)
            .await
            .unwrap()
            .is_none(),
        "workspace {workspace_id} must be deleted"
    );
}

#[tokio::test]
async fn bulk_delete_validates_each_submitted_snapshot_and_continues_after_failures() {
    let data_dir = TempDir::new().unwrap();
    // This integration test is its own process and owns the environment before
    // LocalDeployment initializes its process-wide data-directory cache.
    unsafe {
        std::env::set_var("BC_DATA_DIR", data_dir.path());
        std::env::remove_var("BC_SHARED_API_BASE");
        std::env::remove_var("VK_SHARED_API_BASE");
        std::env::remove_var("BC_SHARED_RELAY_API_BASE");
        std::env::remove_var("VK_SHARED_RELAY_API_BASE");
    }

    let shutdown = CancellationToken::new();
    let deployment = LocalDeployment::new(shutdown.child_token()).await.unwrap();
    let app = Router::new()
        .merge(server::routes::workspaces::router(&deployment))
        .with_state(deployment.clone());

    // Only submitted targets are considered. A same-bucket row that was not
    // reviewed remains untouched, and active or running targets are skipped.
    let submitted = create_archived_workspace(&deployment, "submitted", Duration::days(4)).await;
    let not_submitted =
        create_archived_workspace(&deployment, "not-submitted", Duration::days(4)).await;
    let running = create_archived_workspace(&deployment, "running", Duration::days(5)).await;
    let active = create_workspace(&deployment, "active").await;
    add_running_process(&deployment, running.id).await;

    let results = submit_bulk_delete(
        app.clone(),
        vec![
            workspace_target(&submitted),
            workspace_target(&running),
            workspace_target(&active),
        ],
    )
    .await;
    assert_eq!(results.len(), 3);
    assert_eq!(
        result_for(&results, submitted.id)["outcome"]["status"],
        "deleted"
    );
    assert_eq!(
        result_for(&results, running.id)["outcome"]["status"],
        "skipped"
    );
    assert!(
        result_for(&results, running.id)["outcome"]["reason"]
            .as_str()
            .unwrap()
            .contains("processes are running")
    );
    assert_eq!(
        result_for(&results, active.id)["outcome"]["reason"],
        "no longer archived"
    );
    assert!(
        results
            .iter()
            .all(|result| result["workspace_id"] != not_submitted.id.to_string()),
        "an unsubmitted workspace must not even receive an outcome"
    );
    assert_workspace_deleted(&deployment, submitted.id).await;
    for untouched_id in [not_submitted.id, running.id, active.id] {
        assert_workspace_exists(&deployment, untouched_id).await;
    }

    // Exact archived_at equality rejects an unarchive/re-archive transition,
    // but natural aging across a bucket boundary preserves the same timestamp.
    let changed = create_archived_workspace(&deployment, "changed", Duration::days(4)).await;
    let original_changed_timestamp = json!(changed.archived_at);
    Workspace::set_archived(&deployment.db().pool, changed.id, false)
        .await
        .unwrap();
    tokio::time::sleep(StdDuration::from_millis(10)).await;
    Workspace::set_archived(&deployment.db().pool, changed.id, true)
        .await
        .unwrap();

    let aging = create_archived_workspace(&deployment, "aging", Duration::days(2)).await;
    let review_now = Utc::now();
    let boundary_timestamp = review_now - Duration::days(3) + Duration::milliseconds(250);
    sqlx::query("UPDATE workspaces SET archived_at = ? WHERE id = ?")
        .bind(boundary_timestamp)
        .bind(aging.id)
        .execute(&deployment.db().pool)
        .await
        .unwrap();
    let aging = Workspace::find_by_id(&deployment.db().pool, aging.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ArchiveBucket::from_age(review_now.signed_duration_since(aging.archived_at.unwrap())),
        ArchiveBucket::OneToThreeDays
    );
    tokio::time::sleep(StdDuration::from_millis(300)).await;
    assert_eq!(
        ArchiveBucket::from_age(Utc::now().signed_duration_since(aging.archived_at.unwrap())),
        ArchiveBucket::ThreeToSevenDays
    );

    let null_timestamp =
        create_archived_workspace(&deployment, "null-timestamp", Duration::days(40)).await;
    sqlx::query("UPDATE workspaces SET archived_at = NULL WHERE id = ?")
        .bind(null_timestamp.id)
        .execute(&deployment.db().pool)
        .await
        .unwrap();
    let null_timestamp = Workspace::find_by_id(&deployment.db().pool, null_timestamp.id)
        .await
        .unwrap()
        .unwrap();
    assert!(null_timestamp.archived);
    assert_eq!(null_timestamp.archived_at, None);

    // This real HTTP request exercises JSON/serde timestamp precision for both
    // Some(timestamp) and None snapshots.
    let results = submit_bulk_delete(
        app.clone(),
        vec![
            target(changed.id, original_changed_timestamp),
            workspace_target(&aging),
            workspace_target(&null_timestamp),
        ],
    )
    .await;
    assert_eq!(
        result_for(&results, changed.id)["outcome"]["reason"],
        "archive state changed since review"
    );
    assert_eq!(
        result_for(&results, aging.id)["outcome"]["status"],
        "deleted"
    );
    assert_eq!(
        result_for(&results, null_timestamp.id)["outcome"]["status"],
        "deleted"
    );
    assert_workspace_exists(&deployment, changed.id).await;
    assert_workspace_deleted(&deployment, aging.id).await;
    assert_workspace_deleted(&deployment, null_timestamp.id).await;

    // A missing target is itemized without preventing another target from
    // being deleted.
    let already_deleted =
        create_archived_workspace(&deployment, "already-deleted", Duration::days(8)).await;
    let already_deleted_target = workspace_target(&already_deleted);
    Workspace::delete(&deployment.db().pool, already_deleted.id)
        .await
        .unwrap();
    let after_missing =
        create_archived_workspace(&deployment, "after-missing", Duration::days(8)).await;
    let results = submit_bulk_delete(
        app.clone(),
        vec![already_deleted_target, workspace_target(&after_missing)],
    )
    .await;
    assert_eq!(
        result_for(&results, already_deleted.id)["outcome"]["reason"],
        "already deleted"
    );
    assert_eq!(
        result_for(&results, after_missing.id)["outcome"]["status"],
        "deleted"
    );
    assert_workspace_deleted(&deployment, after_missing.id).await;

    // Force one DELETE statement to fail and prove the handler still processes
    // the following target.
    let forced_failure =
        create_archived_workspace(&deployment, "forced-failure", Duration::days(9)).await;
    let after_failure =
        create_archived_workspace(&deployment, "after-failure", Duration::days(9)).await;
    let failed_id_hex = forced_failure.id.simple().to_string().to_ascii_uppercase();
    sqlx::query(&format!(
        "CREATE TRIGGER fail_selected_workspace_delete
         BEFORE DELETE ON workspaces
         WHEN hex(OLD.id) = '{failed_id_hex}'
         BEGIN
             SELECT RAISE(FAIL, 'forced deletion failure');
         END"
    ))
    .execute(&deployment.db().pool)
    .await
    .unwrap();

    let results = submit_bulk_delete(
        app,
        vec![
            workspace_target(&forced_failure),
            workspace_target(&after_failure),
        ],
    )
    .await;
    assert_eq!(
        result_for(&results, forced_failure.id)["outcome"]["status"],
        "failed"
    );
    assert!(
        result_for(&results, forced_failure.id)["outcome"]["reason"]
            .as_str()
            .unwrap()
            .contains("forced deletion failure")
    );
    assert_eq!(
        result_for(&results, after_failure.id)["outcome"]["status"],
        "deleted"
    );
    assert_workspace_exists(&deployment, forced_failure.id).await;
    assert_workspace_deleted(&deployment, after_failure.id).await;

    shutdown.cancel();
}
