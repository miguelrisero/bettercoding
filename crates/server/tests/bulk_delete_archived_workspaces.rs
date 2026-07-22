use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use chrono::{Duration, Utc};
use db::models::{
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

async fn create_archived_workspace(
    deployment: &LocalDeployment,
    name: &str,
    age: Duration,
) -> Workspace {
    let workspace = Workspace::create(
        &deployment.db().pool,
        &CreateWorkspace {
            branch: format!("test-{name}"),
            name: Some(name.to_string()),
        },
        Uuid::new_v4(),
    )
    .await
    .unwrap();
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

fn result_for<'a>(results: &'a [Value], workspace_id: Uuid) -> &'a Value {
    results
        .iter()
        .find(|result| result["workspace_id"] == workspace_id.to_string())
        .unwrap_or_else(|| panic!("missing result for workspace {workspace_id}"))
}

#[tokio::test]
async fn bulk_delete_resolves_bucket_and_reports_each_item_outcome() {
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
    let deletable = create_archived_workspace(&deployment, "deletable", Duration::days(4)).await;
    let running = create_archived_workspace(&deployment, "running", Duration::days(5)).await;
    let today = create_archived_workspace(&deployment, "today", Duration::hours(2)).await;
    let older = create_archived_workspace(&deployment, "older", Duration::days(20)).await;
    add_running_process(&deployment, running.id).await;

    let app = Router::new()
        .merge(server::routes::workspaces::router(&deployment))
        .with_state(deployment.clone());
    let response = app
        .oneshot(
            Request::post("/workspaces/archived/bulk-delete")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "bucket": "three_to_seven_days",
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
    let results = payload["data"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(
        result_for(results, deletable.id)["outcome"]["status"],
        "deleted"
    );
    assert_eq!(
        result_for(results, running.id)["outcome"]["status"],
        "skipped"
    );
    assert!(
        result_for(results, running.id)["outcome"]["reason"]
            .as_str()
            .unwrap()
            .contains("processes are running")
    );

    assert!(
        Workspace::find_by_id(&deployment.db().pool, deletable.id)
            .await
            .unwrap()
            .is_none()
    );
    for untouched_id in [running.id, today.id, older.id] {
        assert!(
            Workspace::find_by_id(&deployment.db().pool, untouched_id)
                .await
                .unwrap()
                .is_some(),
            "workspace {untouched_id} must remain"
        );
    }

    shutdown.cancel();
}
