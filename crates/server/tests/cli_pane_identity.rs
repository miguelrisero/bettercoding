#![cfg(target_os = "linux")]

use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    process::Command,
};

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
async fn pane_identity_survives_side_tasks_respawn_recreation_and_explicit_switch() {
    let temp = tempfile::tempdir().unwrap();
    let socket = ScratchSocket(format!("bc-identity-test-{}", std::process::id()));
    let claude = temp.path().join("claude");
    symlink("/bin/sleep", &claude).unwrap();
    let data = temp.path().join("data");
    fs::create_dir(&data).unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let fake_bin = temp.path().join("bin");
    fs::create_dir(&fake_bin).unwrap();
    for program in ["claude", "codex"] {
        let path = fake_bin.join(program);
        fs::write(&path, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\nwhile [ ! -f \"$0.exit\" ]; do /bin/sleep 0.05; done\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    // This integration binary owns the environment before deployment caches initialise.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("SHELL", "/bin/sh");
        std::env::set_var(
            "PATH",
            format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap()),
        );
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
    let app = server::routes::router(deployment.clone())
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
        .clone()
        .oneshot(
            Request::post(format!("{prefix}/restart"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    local_deployment::pty::kill_cli_tmux_session(workspace.id, "test:replace_sleep_fixture").await;

    for (executor, program, native) in [
        ("CLAUDE_CODE", "claude", native_id.clone()),
        (
            "CODEX",
            "codex",
            "019a2171-5b00-7000-8000-000000000001".into(),
        ),
    ] {
        let chosen = if executor == "CLAUDE_CODE" {
            session.clone()
        } else {
            let chosen = Session::create(
                pool,
                &CreateSession {
                    executor: Some(executor.into()),
                    name: None,
                },
                Uuid::new_v4(),
                workspace.id,
            )
            .await
            .unwrap();
            db::models::claude_session_link::ClaudeSessionLink::assign_cli(
                pool,
                &native,
                chosen.id,
                workspace.id,
                temp.path().to_str().unwrap(),
                db::models::claude_session_link::ClaudeSessionBoundVia::CliResume,
            )
            .await
            .unwrap();
            assert_eq!(
                Session::find_for_cli(pool, workspace.id, Some(chosen.id))
                    .await
                    .unwrap()
                    .unwrap()
                    .id,
                session.id
            );
            chosen
        };
        let url = format!("{prefix}/session/{}", chosen.id);
        let response = app
            .clone()
            .oneshot(Request::post(url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !local_deployment::pty::cli_tmux_session_exists_checked(workspace.id)
                .await
                .unwrap()
        );
        assert_eq!(
            Session::find_for_cli(pool, workspace.id, Some(side_task.id))
                .await
                .unwrap()
                .unwrap()
                .id,
            chosen.id
        );
        let args_file = fake_bin.join(format!("{program}.args"));
        let exit_file = fake_bin.join(format!("{program}.exit"));
        for operation in ["launch", "restart", "launch"] {
            if operation == "restart" {
                fs::write(&exit_file, "exit").unwrap();
                for _ in 0..100 {
                    if local_deployment::cli_activity::probe_workspace_agent(workspace.id, program)
                        .await
                        == local_deployment::cli_activity::AgentPresence::Absent
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                fs::remove_file(&exit_file).unwrap();
                fs::remove_file(&args_file).unwrap();
            } else if args_file.exists() {
                local_deployment::pty::kill_cli_tmux_session(
                    workspace.id,
                    "test:session_recreation",
                )
                .await;
                fs::remove_file(&args_file).unwrap();
            }
            let response = app
                .clone()
                .oneshot(
                    Request::post(format!("{prefix}/{operation}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            wait_for_file(&args_file).await;
            let args = fs::read_to_string(&args_file).unwrap();
            let args: Vec<_> = args.lines().collect();
            let expected = if program == "claude" {
                "--resume"
            } else {
                "resume"
            };
            assert!(
                args.windows(2)
                    .any(|pair| pair == [expected, native.as_str()]),
                "{operation}: {args:?}"
            );
        }
        if program == "codex" {
            local_deployment::pty::kill_cli_tmux_session(workspace.id, "test:case_cleanup").await;
        }
    }
    let foreign_workspace = Workspace::create(
        pool,
        &CreateWorkspace {
            branch: "foreign".into(),
            name: None,
        },
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let foreign_session = Session::create(
        pool,
        &CreateSession {
            executor: Some("CODEX".into()),
            name: None,
        },
        Uuid::new_v4(),
        foreign_workspace.id,
    )
    .await
    .unwrap();
    let before = Session::find_for_cli(pool, workspace.id, None)
        .await
        .unwrap()
        .unwrap()
        .id;
    let response = app
        .clone()
        .oneshot(
            Request::post(format!("{prefix}/session/{}", foreign_session.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        Session::find_for_cli(pool, workspace.id, None)
            .await
            .unwrap()
            .unwrap()
            .id,
        before
    );
    let fresh = Session::create(
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
    fs::remove_file(fake_bin.join("claude.args")).unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::post(format!("{prefix}/session/{}", fresh.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .oneshot(
            Request::post(format!("{prefix}/launch"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    wait_for_file(&fake_bin.join("claude.args")).await;
    let args = fs::read_to_string(fake_bin.join("claude.args")).unwrap();
    assert!(
        !args
            .lines()
            .any(|arg| matches!(arg, "--continue" | "--resume")),
        "{args}"
    );
    local_deployment::pty::kill_cli_tmux_session(workspace.id, "test:fresh_cleanup").await;
    shutdown.cancel();
}

async fn wait_for_file(path: &Path) {
    for _ in 0..100 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("fake agent did not record arguments at {}", path.display());
}
