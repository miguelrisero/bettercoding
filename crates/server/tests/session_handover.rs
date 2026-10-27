#![cfg(target_os = "linux")]

use std::{fs, os::unix::fs::PermissionsExt, process::Command};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use chrono::{Duration, Utc};
use db::models::{
    cli_pane_binding::{CliPaneBinding, CliPaneBoundVia},
    session::{CreateSession, Session},
    workspace::{CreateWorkspace, Workspace},
};
use deployment::Deployment;
use local_deployment::LocalDeployment;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

type App = axum::Router;

async fn request(
    app: &App,
    method: &str,
    path: &str,
    body: Value,
    capability: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = capability {
        builder = builder.header("x-bc-handover-capability", token);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

struct Socket(String);
impl Drop for Socket {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.0, "kill-server"])
            .status();
    }
}

#[tokio::test]
async fn durable_handover_success_retries_crash_deadline_restart_and_binding_fences() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let socket = Socket(format!("bc-handover-{}", Uuid::new_v4().simple()));
    for dir in ["home", "data", "worktrees", "bin"] {
        fs::create_dir(root.join(dir)).unwrap();
    }
    let fake = root.join("bin/claude");
    fs::write(&fake, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$PWD/args\"\nprintf '%s' \"$BC_HANDOVER_CAPABILITY\" > \"$PWD/token\"\nprintf '%s' \"$$\" > \"$PWD/agent-pid\"\nwhile :; do /bin/sleep 1; done\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    unsafe {
        std::env::set_var("HOME", root.join("home"));
        std::env::set_var("SHELL", "/bin/sh");
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                root.join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        );
        std::env::set_var("BC_DATA_DIR", root.join("data"));
        std::env::set_var("BC_WORKTREE_BASE", root.join("worktrees"));
        std::env::set_var("BC_CLI_TMUX_SOCKET", &socket.0);
        std::env::remove_var("BC_SHARED_API_BASE");
        std::env::remove_var("BC_SHARED_RELAY_API_BASE");
    }
    let shutdown = CancellationToken::new();
    let deployment = LocalDeployment::new(shutdown.child_token()).await.unwrap();
    let pool = &deployment.db().pool;
    let app = server::routes::router(deployment.clone())
        .oneshot(())
        .await
        .unwrap();
    adoption_contract(&deployment, &app, root, &socket.0).await;
    for scenario in [
        "success",
        "crash",
        "deadline",
        "restart",
        "newer",
        "fallback_failure",
    ] {
        let workspace = Workspace::create(
            pool,
            &CreateWorkspace {
                branch: scenario.into(),
                name: None,
            },
            Uuid::new_v4(),
        )
        .await
        .unwrap();
        let cwd = root.join("worktrees").join(scenario);
        fs::create_dir(&cwd).unwrap();
        Workspace::update_container_ref(pool, workspace.id, cwd.to_str().unwrap())
            .await
            .unwrap();
        let mut sessions = Vec::new();
        for _ in 0..3 {
            sessions.push(
                Session::create(
                    pool,
                    &CreateSession {
                        executor: Some("CLAUDE_CODE".into()),
                        name: None,
                    },
                    Uuid::new_v4(),
                    workspace.id,
                )
                .await
                .unwrap(),
            );
        }
        let native = Uuid::new_v4().to_string();
        let binding = CliPaneBinding::record_launch(
            pool,
            workspace.id,
            sessions[0].id,
            Some(&native),
            CliPaneBoundVia::CliResume,
        )
        .await
        .unwrap();
        let name = local_deployment::pty::cli_tmux_session_name(workspace.id);
        assert!(
            Command::new("tmux")
                .args(["-L", &socket.0, "new-session", "-d", "-s", &name, "-c"])
                .arg(&cwd)
                .arg("sleep 600")
                .status()
                .unwrap()
                .success()
        );
        let payload = json!({ "workspace_id": workspace.id, "expected_binding_id": binding.id, "successor_session_id": sessions[1].id,
            "fallback_session_id": native, "initial_prompt": "/goal Read context.txt", "request_id": scenario, "confirm_deadline_secs": 900 });
        let mut stale = payload.clone();
        stale["expected_binding_id"] = json!(Uuid::new_v4());
        let (status, _) = request(&app, "POST", "/api/handovers", stale, None).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            CliPaneBinding::find_active_for_workspace(pool, workspace.id)
                .await
                .unwrap()
                .unwrap()
                .id,
            binding.id
        );
        let (status, response) =
            request(&app, "POST", "/api/handovers", payload.clone(), None).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        let op = &response["data"];
        assert_eq!(op["phase"], "awaiting_confirmation", "{response}");
        let id = op["operation_id"].as_str().unwrap();
        let operation_id = Uuid::parse_str(id).unwrap();
        let successor_binding = op["binding_id"].clone();
        let args = fs::read_to_string(cwd.join("args")).unwrap();
        assert!(args.contains("--session-id\n"));
        assert!(args.contains(op["successor_native_session_id"].as_str().unwrap()));
        assert!(args.contains("/goal Read context.txt"));
        let token = fs::read_to_string(cwd.join("token")).unwrap();
        assert!(!token.is_empty());
        let (_, duplicate) = request(&app, "POST", "/api/handovers", payload.clone(), None).await;
        assert_eq!(duplicate["data"]["operation_id"], op["operation_id"]);
        assert_eq!(duplicate["data"]["binding_id"], successor_binding);
        let mut different = payload;
        different["initial_prompt"] = json!("different");
        assert_eq!(
            request(&app, "POST", "/api/handovers", different, None)
                .await
                .0,
            StatusCode::CONFLICT
        );
        let confirm_url = format!("/api/handovers/{id}/confirm");
        assert_eq!(
            request(&app, "POST", &confirm_url, json!({}), Some("wrong-session"))
                .await
                .0,
            StatusCode::CONFLICT
        );
        if scenario == "success" {
            let (status, confirmed) =
                request(&app, "POST", &confirm_url, json!({}), Some(&token)).await;
            assert_eq!(status, StatusCode::OK, "{confirmed}");
            assert_eq!(confirmed["data"]["phase"], "confirmed");
            let (_, repeated) = request(&app, "POST", &confirm_url, json!({}), Some(&token)).await;
            assert_eq!(
                repeated["data"]["confirmed_at"],
                confirmed["data"]["confirmed_at"]
            );
            server::routes::session_handover::reconcile(
                &deployment,
                Utc::now() + Duration::hours(1),
            )
            .await
            .unwrap();
            assert_eq!(
                db::models::session_handover::SessionHandover::find(pool, operation_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .phase,
                "confirmed"
            );
            continue;
        }
        let mut newer_id = None;
        if scenario == "newer" {
            let before_pid = fs::read_to_string(cwd.join("agent-pid")).unwrap();
            let target = local_deployment::pty::locate_cli_tmux_target(workspace.id)
                .await
                .unwrap();
            let newer_pid = tokio::process::Command::new("tmux")
                .args([
                    "-L",
                    &socket.0,
                    "respawn-pane",
                    "-k",
                    "-t",
                    &local_deployment::pty::cli_workspace_pane_id(workspace.id)
                        .await
                        .unwrap(),
                    "-c",
                ])
                .arg(&cwd)
                .arg("exec claude --resume newer-pane")
                .status()
                .await
                .unwrap();
            assert!(newer_pid.success());
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while fs::read_to_string(cwd.join("agent-pid")).ok().as_deref()
                    == Some(before_pid.as_str())
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(local_deployment::pty::cli_tmux_target_exists(&target).await);
            newer_id = Some(
                CliPaneBinding::record_launch(
                    pool,
                    workspace.id,
                    sessions[2].id,
                    Some(&Uuid::new_v4().to_string()),
                    CliPaneBoundVia::CliResume,
                )
                .await
                .unwrap()
                .id,
            );
            fs::write(cwd.join("args"), "newer-pane-marker").unwrap();
        }
        if scenario == "restart" {
            let restarted_deployment = LocalDeployment::new(shutdown.child_token()).await.unwrap();
            server::routes::session_handover::reconcile(
                &restarted_deployment,
                Utc::now() + Duration::hours(1),
            )
            .await
            .unwrap();
            let restarted = server::routes::router(restarted_deployment)
                .oneshot(())
                .await
                .unwrap();
            assert_eq!(
                request(
                    &restarted,
                    "GET",
                    &format!("/api/handovers/{id}"),
                    json!({}),
                    None
                )
                .await
                .1["data"]["binding_id"],
                successor_binding
            );
        }
        if scenario == "crash" {
            let saved = db::models::session_handover::SessionHandover::find(pool, operation_id)
                .await
                .unwrap()
                .unwrap();
            let launch: Value = serde_json::from_str(&saved.launch_json).unwrap();
            let pid = fs::read_to_string(cwd.join("agent-pid")).unwrap();
            assert!(
                Command::new("kill")
                    .args(["-KILL", &pid])
                    .status()
                    .unwrap()
                    .success()
            );
            let exit = std::path::Path::new(launch["successor_exit"].as_str().unwrap());
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !exit.exists() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        if scenario == "fallback_failure" {
            fs::write(&fake, "#!/bin/sh\nexit 23\n").unwrap();
        }
        if scenario == "deadline" {
            sqlx::query("UPDATE session_handovers SET deadline_at = ? WHERE operation_id = ?")
                .bind(Utc::now() - Duration::seconds(1))
                .bind(operation_id)
                .execute(pool)
                .await
                .unwrap();
            assert_eq!(
                request(&app, "POST", &confirm_url, json!({}), Some(&token))
                    .await
                    .0,
                StatusCode::CONFLICT
            );
        }
        let now = if scenario == "crash" {
            Utc::now()
        } else {
            Utc::now() + Duration::hours(1)
        };
        server::routes::session_handover::reconcile(&deployment, now)
            .await
            .unwrap();
        let saved = db::models::session_handover::SessionHandover::find(pool, operation_id)
            .await
            .unwrap()
            .unwrap();
        if scenario == "newer" {
            assert_eq!(saved.phase, "failed");
            assert_eq!(
                CliPaneBinding::find_active_for_workspace(pool, workspace.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .id,
                newer_id.unwrap()
            );
            assert_eq!(
                fs::read_to_string(cwd.join("args")).unwrap(),
                "newer-pane-marker"
            );
        } else if scenario == "fallback_failure" {
            assert_eq!(saved.phase, "failed");
            assert!(saved.failure.unwrap().contains("fallback failed"));
        } else {
            assert_eq!(saved.phase, "rolled_back", "{:?}", saved.failure);
            let args = fs::read_to_string(cwd.join("args")).unwrap();
            assert!(args.contains(&format!("--resume\n{native}\n")), "{args}");
            let active = CliPaneBinding::find_active_for_workspace(pool, workspace.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(active.session_id, sessions[0].id);
            assert_eq!(active.claude_session_id.as_deref(), Some(native.as_str()));
        }
        assert_eq!(
            request(&app, "POST", &confirm_url, json!({}), Some(&token))
                .await
                .0,
            StatusCode::CONFLICT
        );
    }
    shutdown.cancel();
}

async fn adoption_contract(
    deployment: &LocalDeployment,
    app: &App,
    root: &std::path::Path,
    socket: &str,
) {
    use std::os::unix::fs::MetadataExt;

    use db::models::{
        claude_session_link::{ClaudeSessionBoundVia, ClaudeSessionLink},
        cli_native_file::{CliNativeFile, RegisterCliNativeFile},
    };
    let pool = &deployment.db().pool;
    let workspace = Workspace::create(
        pool,
        &CreateWorkspace {
            branch: "adoption".into(),
            name: None,
        },
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let cwd = root.join("worktrees/adoption");
    fs::create_dir(&cwd).unwrap();
    Workspace::update_container_ref(pool, workspace.id, cwd.to_str().unwrap())
        .await
        .unwrap();
    let mut sessions = Vec::new();
    for executor in ["CODEX", "CLAUDE_CODE", "CLAUDE_CODE"] {
        sessions.push(
            Session::create(
                pool,
                &CreateSession {
                    executor: Some(executor.into()),
                    name: None,
                },
                Uuid::new_v4(),
                workspace.id,
            )
            .await
            .unwrap(),
        );
    }
    let native = Uuid::new_v4().to_string();
    let old_native = Uuid::new_v4().to_string();
    let transcript_dir = root.join("home/.claude/projects/adoption");
    fs::create_dir_all(&transcript_dir).unwrap();
    let transcript = transcript_dir.join(format!("{native}.jsonl"));
    fs::write(&transcript, "{}").unwrap();
    let metadata = fs::metadata(&transcript).unwrap();
    CliNativeFile::register(
        pool,
        &RegisterCliNativeFile {
            claude_session_id: &native,
            dir_path: transcript_dir.to_str().unwrap(),
            file_name: transcript.file_name().unwrap().to_str().unwrap(),
            discovered_workspace_id: Some(workspace.id),
            dev: metadata.dev() as i64,
            inode: metadata.ino() as i64,
            observed_size: 2,
            observed_mtime_ms: None,
        },
    )
    .await
    .unwrap();
    let task_dir = root.join("home/.claude/tasks").join(&native);
    fs::create_dir_all(&task_dir).unwrap();
    let binding = CliPaneBinding::record_launch(
        pool,
        workspace.id,
        sessions[0].id,
        Some(&old_native),
        CliPaneBoundVia::CliFresh,
    )
    .await
    .unwrap();
    let name = local_deployment::pty::cli_tmux_session_name(workspace.id);
    let command = format!(
        "exec 9<'{}'; exec claude --dangerously-skip-permissions",
        task_dir.display()
    );
    assert!(
        Command::new("tmux")
            .args(["-L", socket, "new-session", "-d", "-s", &name, "-c"])
            .arg(&cwd)
            .arg(command)
            .status()
            .unwrap()
            .success()
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !cwd.join("agent-pid").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let pid: u32 = fs::read_to_string(cwd.join("agent-pid"))
        .unwrap()
        .parse()
        .unwrap();
    let start = || {
        fs::read_to_string(format!("/proc/{pid}/stat"))
            .unwrap()
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse::<u64>()
            .unwrap()
    };
    let start_time = start();
    let pane = local_deployment::pty::cli_workspace_pane_id(workspace.id)
        .await
        .unwrap();
    let payload = json!({ "workspace_id": workspace.id, "expected_binding_id": binding.id, "expected_session_id": sessions[0].id, "expected_native_session_id": old_native, "session_id": sessions[1].id, "native_session_id": native, "expected_pane_id": pane, "expected_pid": pid, "expected_start_time": start_time });
    for (field, value) in [
        ("expected_binding_id", json!(Uuid::new_v4())),
        ("expected_session_id", json!(Uuid::new_v4())),
        ("expected_native_session_id", json!(Uuid::new_v4())),
        ("expected_pane_id", json!("%999999")),
        ("expected_start_time", json!(start_time + 1)),
    ] {
        let mut invalid = payload.clone();
        invalid[field] = value;
        assert_eq!(
            request(app, "POST", "/api/sessions/adopt", invalid, None)
                .await
                .0,
            StatusCode::CONFLICT,
            "{field}"
        );
        assert_eq!(start(), start_time);
        assert_eq!(
            CliPaneBinding::find_active_for_workspace(pool, workspace.id)
                .await
                .unwrap()
                .unwrap()
                .session_id,
            sessions[0].id
        );
        assert!(
            ClaudeSessionLink::find(pool, &native)
                .await
                .unwrap()
                .is_none()
        );
    }
    let foreign = Workspace::create(
        pool,
        &CreateWorkspace {
            branch: "foreign".into(),
            name: None,
        },
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE cli_native_files SET discovered_workspace_id = ? WHERE claude_session_id = ?",
    )
    .bind(foreign.id)
    .bind(&native)
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(
        request(app, "POST", "/api/sessions/adopt", payload.clone(), None)
            .await
            .0,
        StatusCode::CONFLICT
    );
    sqlx::query(
        "UPDATE cli_native_files SET discovered_workspace_id = ? WHERE claude_session_id = ?",
    )
    .bind(workspace.id)
    .bind(&native)
    .execute(pool)
    .await
    .unwrap();
    ClaudeSessionLink::assign_cli(
        pool,
        &native,
        sessions[2].id,
        workspace.id,
        cwd.to_str().unwrap(),
        ClaudeSessionBoundVia::CliFresh,
    )
    .await
    .unwrap();
    assert_eq!(
        request(app, "POST", "/api/sessions/adopt", payload.clone(), None)
            .await
            .0,
        StatusCode::CONFLICT
    );
    sqlx::query("DELETE FROM claude_session_links WHERE claude_session_id = ?")
        .bind(&native)
        .execute(pool)
        .await
        .unwrap();
    let (status, result) = request(app, "POST", "/api/sessions/adopt", payload, None).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(start(), start_time);
    assert_eq!(
        local_deployment::pty::cli_workspace_pane_id(workspace.id)
            .await
            .unwrap(),
        pane
    );
    let active = CliPaneBinding::find_active_for_workspace(pool, workspace.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active.session_id, sessions[1].id);
    assert_ne!(active.id, binding.id);
    let released = CliPaneBinding::find_by_id(pool, binding.id)
        .await
        .unwrap()
        .unwrap();
    assert!(released.released_at.is_some());
    assert_eq!(released.session_id, sessions[0].id);
    assert_eq!(
        released.claude_session_id.as_deref(),
        Some(old_native.as_str())
    );
    assert_eq!(active.claude_session_id.as_deref(), Some(native.as_str()));
    assert_eq!(
        ClaudeSessionLink::find(pool, &native)
            .await
            .unwrap()
            .unwrap()
            .session_id,
        sessions[1].id
    );
}
