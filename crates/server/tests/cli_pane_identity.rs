#![cfg(target_os = "linux")]

use std::{fs, os::unix::fs::symlink, process::Command};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use db::models::{
    cli_pane_binding::{CliPaneBinding, CliPaneBoundVia},
    session::{CreateSession, Session},
    workspace::{CreateWorkspace, Workspace},
};
use deployment::Deployment;
use local_deployment::LocalDeployment;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

struct ScratchSocket(String);

impl Drop for ScratchSocket {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.0, "kill-server"])
            .status();
    }
}

#[tokio::test]
async fn live_claude_remains_alive_and_cannot_be_restarted_after_codex_side_task() {
    let temp = tempfile::tempdir().unwrap();
    let socket = ScratchSocket(format!("bc-identity-test-{}", std::process::id()));
    let claude = temp.path().join("claude");
    symlink("/bin/sleep", &claude).unwrap();
    let data = temp.path().join("data");
    fs::create_dir(&data).unwrap();
    // This integration binary owns the environment before deployment caches initialise.
    unsafe {
        std::env::set_var("BC_DATA_DIR", &data);
        std::env::set_var("BC_WORKTREE_BASE", temp.path().join("worktrees"));
        std::env::set_var("BC_CLI_TMUX_SOCKET", &socket.0);
        std::env::remove_var("BC_SHARED_API_BASE");
        std::env::remove_var("BC_SHARED_RELAY_API_BASE");
    }
    let shutdown = CancellationToken::new();
    let deployment = LocalDeployment::new(shutdown.child_token()).await.unwrap();
    let pool = &deployment.db().pool;
    let workspace = Workspace::create(
        pool,
        &CreateWorkspace {
            branch: "identity-test".into(),
            name: None,
        },
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    Workspace::update_container_ref(pool, workspace.id, temp.path().to_str().unwrap())
        .await
        .unwrap();
    let session = Session::create(
        pool,
        &CreateSession {
            executor: Some("CLAUDE_CODE".into()),
            name: None,
        },
        Uuid::new_v4(),
        workspace.id,
    )
    .await
    .unwrap();
    let native_id = Uuid::new_v4().to_string();
    CliPaneBinding::record_launch(
        pool,
        workspace.id,
        session.id,
        Some(&native_id),
        CliPaneBoundVia::CliResume,
    )
    .await
    .unwrap();
    let side_task = Session::create(
        pool,
        &CreateSession {
            executor: Some("CODEX".into()),
            name: None,
        },
        Uuid::new_v4(),
        workspace.id,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE sessions SET created_at = '2099-01-01 00:00:00' WHERE id = ?")
        .bind(side_task.id)
        .execute(pool)
        .await
        .unwrap();
    let session_name = local_deployment::pty::cli_tmux_session_name(workspace.id);
    assert!(
        Command::new("tmux")
            .args(["-L", &socket.0, "new-session", "-d", "-s", &session_name])
            .arg(&claude)
            .arg("600")
            .status()
            .unwrap()
            .success()
    );
    let app = server::routes::router(deployment)
        .oneshot(())
        .await
        .unwrap();
    let prefix = format!("/api/workspaces/{}/cli", workspace.id);
    let response = app
        .clone()
        .oneshot(
            Request::get(format!("{prefix}/agent-status"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 16384).await.unwrap()).unwrap();
    assert_eq!(body["data"]["program"], "claude");
    assert_eq!(body["data"]["agent_alive"], true);
    assert_eq!(body["data"]["restartable"], false);
    let response = app
        .oneshot(
            Request::post(format!("{prefix}/restart"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    shutdown.cancel();
}
