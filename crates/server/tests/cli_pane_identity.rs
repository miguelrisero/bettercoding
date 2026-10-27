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
    let switch_url = format!("{prefix}/session/{}", side_task.id);
    let response = app
        .clone()
        .oneshot(Request::post(&switch_url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(response.status().is_client_error());
    for (acknowledged, binding, expected) in [
        (
            false,
            CliPaneBinding::find_active_for_workspace(pool, workspace.id)
                .await
                .unwrap()
                .unwrap()
                .id,
            StatusCode::BAD_REQUEST,
        ),
        (true, Uuid::new_v4(), StatusCode::CONFLICT),
    ] {
        let response = app.clone().oneshot(Request::post(&switch_url).header("content-type", "application/json")
            .body(Body::from(serde_json::json!({"acknowledge_agent_shutdown": acknowledged, "expected_binding_id": binding}).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), expected);
        assert_eq!(
            local_deployment::cli_activity::probe_workspace_agent(workspace.id, "claude").await,
            local_deployment::cli_activity::AgentPresence::Alive
        );
    }
    local_deployment::pty::kill_cli_tmux_session(workspace.id, "test:replace_sleep_fixture").await;

    // Recreate and respawn the incident pane without any handover request.
    let stale_ack = acknowledge(pool, workspace.id).await;
    passive_launch_and_resume(&app, &prefix, &fake_bin, workspace.id, &native_id).await;
    let response = app
        .clone()
        .oneshot(
            Request::post(&switch_url)
                .header("content-type", "application/json")
                .body(Body::from(stale_ack))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        local_deployment::cli_activity::probe_workspace_agent(workspace.id, "claude").await,
        local_deployment::cli_activity::AgentPresence::Alive
    );
    local_deployment::pty::kill_cli_tmux_session(workspace.id, "test:passive_cleanup").await;

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
        let delayed = if program == "codex" {
            fs::remove_file(fake_bin.join("claude.args")).unwrap();
            Some(delay_existing_attach(app.clone(), workspace.id, session.id).await)
        } else {
            None
        };
        let url = format!("{prefix}/session/{}", chosen.id);
        let acknowledgement = acknowledge(pool, workspace.id).await;
        let response = app
            .clone()
            .oneshot(
                Request::post(url)
                    .header("content-type", "application/json")
                    .body(Body::from(acknowledgement))
                    .unwrap(),
            )
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
        if let Some((release, listener_task)) = delayed {
            release.send(()).unwrap();
            listener_task.await.unwrap();
            wait_for_file(&fake_bin.join("codex.args")).await;
            assert!(
                !fake_bin.join("claude.args").exists(),
                "stale attach relaunched Claude across the Codex handover"
            );
            let args = fs::read_to_string(fake_bin.join("codex.args")).unwrap();
            assert!(
                args.lines()
                    .collect::<Vec<_>>()
                    .windows(2)
                    .any(|pair| pair == ["resume", native.as_str()]),
                "{args}"
            );
        }
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
                .header("content-type", "application/json")
                .body(Body::from(acknowledge(pool, workspace.id).await))
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
    let _ = fs::remove_file(fake_bin.join("claude.args"));
    let acknowledgement = acknowledge(pool, workspace.id).await;
    let response = app
        .clone()
        .oneshot(
            Request::post(format!("{prefix}/session/{}", fresh.id))
                .header("content-type", "application/json")
                .body(Body::from(acknowledgement))
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

async fn acknowledge(pool: &sqlx::SqlitePool, workspace_id: Uuid) -> String {
    serde_json::json!({"acknowledge_agent_shutdown": true,
        "expected_binding_id": CliPaneBinding::find_active_for_workspace(pool, workspace_id).await.unwrap().map(|b| b.id)}).to_string()
}

async fn passive_launch_and_resume(
    app: &axum::Router,
    prefix: &str,
    bin: &Path,
    workspace: Uuid,
    native: &str,
) {
    let args_file = bin.join("claude.args");
    for operation in ["launch", "restart"] {
        if operation == "restart" {
            fs::write(bin.join("claude.exit"), "exit").unwrap();
            for _ in 0..100 {
                if local_deployment::cli_activity::probe_workspace_agent(workspace, "claude").await
                    == local_deployment::cli_activity::AgentPresence::Absent
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            fs::remove_file(bin.join("claude.exit")).unwrap();
            fs::remove_file(&args_file).unwrap();
        }
        if operation == "launch" {
            let (release, attached) =
                delay_existing_attach(app.clone(), workspace, Uuid::new_v4()).await;
            release.send(()).unwrap();
            attached.await.unwrap();
        } else {
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
        }
        wait_for_file(&args_file).await;
        let args = fs::read_to_string(&args_file).unwrap();
        assert!(
            args.lines()
                .collect::<Vec<_>>()
                .windows(2)
                .any(|pair| pair == ["--resume", native]),
            "{args}"
        );
    }
}

async fn delay_existing_attach(
    app: axum::Router,
    workspace: Uuid,
    stale_session: Uuid,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };

    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
    struct HeldHandshake {
        stream: tokio::net::TcpStream,
        ready: Option<tokio::sync::oneshot::Sender<()>>,
        release: tokio::sync::oneshot::Receiver<()>,
        released: bool,
    }
    impl AsyncRead for HeldHandshake {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.stream).poll_read(cx, buf)
        }
    }
    impl AsyncWrite for HeldHandshake {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if !self.released {
                if let Some(ready) = self.ready.take() {
                    let _ = ready.send(());
                }
                if Pin::new(&mut self.release).poll(cx).is_pending() {
                    return Poll::Pending;
                }
                self.released = true;
            }
            Pin::new(&mut self.stream).poll_write(cx, buf)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.stream).poll_flush(cx)
        }
        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.stream).poll_shutdown(cx)
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let service = hyper_util::service::TowerToHyperService::new(app);
        let stream = HeldHandshake {
            stream,
            ready: Some(ready_tx),
            release: release_rx,
            released: false,
        };
        hyper::server::conn::http1::Builder::new()
            .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
            .with_upgrades()
            .await
            .unwrap();
    });
    let mut tcp = tokio::net::TcpStream::connect(address).await.unwrap();
    tcp.write_all(format!("GET /api/terminal/ws?workspace_id={workspace}&session_id={stale_session}&mode=cli&cols=100&rows=30 HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").as_bytes()).await.unwrap();
    ready_rx.await.unwrap();
    let keeper = tokio::spawn(async move {
        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            response.push(tcp.read_u8().await.unwrap());
        }
        assert!(String::from_utf8_lossy(&response).contains("101 Switching Protocols"));
        task.await.unwrap();
    });
    (release_tx, keeper)
}
