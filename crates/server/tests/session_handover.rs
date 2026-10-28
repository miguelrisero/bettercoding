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
    (
        status,
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        },
    )
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
    let original_fake = fs::read(&fake).unwrap();
    unsafe {
        std::env::set_var("HOME", root.join("home"));
        std::env::set_var("SHELL", "/bin/sh");
        std::env::set_var("ENABLE_CLI_HANDOVER", "1");
        std::env::remove_var("DISABLE_CLI_COLLAB_ROUTING");
        std::env::remove_var("DISABLE_CLI_TRANSCRIPT_INGEST");
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
        "fallback_intent_restart",
        "fallback_launch_restart",
        "newer",
        "fallback_deadline",
        "stale_fallback_identity",
        "late_fallback_receipt",
        "delayed_fallback_failure",
        "fallback_failure",
    ] {
        fs::write(&fake, &original_fake).unwrap();
        let workspace = Workspace::create(
            pool,
            &CreateWorkspace {
                branch: scenario.into(),
                name: Some("Named lifecycle workspace".into()),
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
        if matches!(scenario, "success" | "crash") {
            let repo_path = root.join(format!("fixture-repo-{scenario}"));
            fs::create_dir(&repo_path).unwrap();
            assert!(
                Command::new("git")
                    .args(["-C"])
                    .arg(&repo_path)
                    .args(["init", "-b", "main"])
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&repo_path)
                    .args([
                        "-c",
                        "user.name=Fixture",
                        "-c",
                        "user.email=fixture@example.invalid",
                        "commit",
                        "--allow-empty",
                        "-m",
                        "fixture"
                    ])
                    .status()
                    .unwrap()
                    .success()
            );
            let repo = db::models::repo::Repo::find_or_create(pool, &repo_path, "fixture")
                .await
                .unwrap();
            db::models::workspace_repo::WorkspaceRepo::create_many(
                pool,
                workspace.id,
                &[db::models::workspace_repo::CreateWorkspaceRepo {
                    repo_id: repo.id,
                    target_branch: "main".into(),
                }],
            )
            .await
            .unwrap();
        }
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
            let (status, delivered) = request(
                &app,
                "POST",
                &format!("/api/sessions/{}/follow-up", sessions[1].id),
                json!({"prompt":"dashboard message", "executor_config":{"executor":"CLAUDE_CODE"}}),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{delivered}");
            assert_eq!(delivered["data"]["outcome"], "routed_to_cli", "{delivered}");
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
            let (status, deleted) = request(
                &app,
                "DELETE",
                &format!("/api/workspaces/{}", workspace.id),
                json!({}),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::ACCEPTED, "{deleted}");
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while Workspace::find_by_id(pool, workspace.id)
                    .await
                    .unwrap()
                    .is_some()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(
                db::models::session_handover::SessionHandover::find(pool, operation_id)
                    .await
                    .unwrap()
                    .is_none()
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
        if scenario == "delayed_fallback_failure" {
            fs::write(&fake, "#!/bin/sh\nprintf '%s' \"$$\" > \"$PWD/agent-pid\"\nwhile :; do /bin/sleep 1; done\n").unwrap();
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
        if scenario == "fallback_intent_restart" {
            sqlx::query("UPDATE session_handovers SET rollback_started = 1, fallback_started_at = ? WHERE operation_id = ?").bind(Utc::now()).bind(operation_id).execute(pool).await.unwrap();
        }
        let now = if matches!(scenario, "crash" | "restart") {
            Utc::now()
        } else {
            Utc::now() + Duration::hours(1)
        };
        server::routes::session_handover::reconcile(&deployment, now)
            .await
            .unwrap();
        if !matches!(scenario, "success" | "newer" | "fallback_failure") {
            let pending = db::models::session_handover::SessionHandover::find(pool, operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_ne!(pending.phase, "rolled_back");
            if matches!(
                scenario,
                "fallback_intent_restart" | "fallback_launch_restart"
            ) {
                if scenario == "fallback_launch_restart" {
                    sqlx::query("UPDATE cli_pane_bindings SET session_id = ?, claude_session_id = ?, bound_via = 'cli-fresh' WHERE id = ?")
                        .bind(sessions[1].id).bind(&pending.successor_native_session_id).bind(pending.binding_id).execute(pool).await.unwrap();
                    sqlx::query("UPDATE session_handovers SET fallback_pid = NULL, fallback_start_time = NULL WHERE operation_id = ?").bind(operation_id).execute(pool).await.unwrap();
                }
                let pid = fs::read_to_string(cwd.join("agent-pid")).unwrap();
                let recovered = LocalDeployment::new(shutdown.child_token()).await.unwrap();
                server::routes::session_handover::reconcile(&recovered, Utc::now())
                    .await
                    .unwrap();
                assert_eq!(fs::read_to_string(cwd.join("agent-pid")).unwrap(), pid);
            }

            if scenario == "delayed_fallback_failure" {
                let pid = fs::read_to_string(cwd.join("agent-pid")).unwrap();
                assert!(
                    Command::new("kill")
                        .args(["-KILL", &pid])
                        .status()
                        .unwrap()
                        .success()
                );
                let launch: Value = serde_json::from_str(&pending.launch_json).unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while !std::path::Path::new(launch["fallback_exit"].as_str().unwrap()).exists()
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            } else if scenario == "fallback_deadline" {
                let pid = fs::read_to_string(cwd.join("agent-pid")).unwrap();
                server::routes::session_handover::reconcile(
                    &deployment,
                    pending.fallback_started_at.unwrap() + Duration::seconds(901),
                )
                .await
                .unwrap();
                assert!(std::path::Path::new(&format!("/proc/{}", pid.trim())).exists());
            } else {
                let (status, _) = request(
                    &app,
                    "POST",
                    &format!("/api/workspaces/{}/cli-activity", workspace.id),
                    json!({"hook_event_name":"SessionStart","session_id":native}),
                    Some("stale-operation"),
                )
                .await;
                assert_eq!(status, StatusCode::NO_CONTENT);
                assert!(
                    db::models::session_handover::SessionHandover::find(pool, operation_id)
                        .await
                        .unwrap()
                        .unwrap()
                        .fallback_ready_at
                        .is_none()
                );
                let (status, _) = request(
                    &app,
                    "POST",
                    &format!("/api/workspaces/{}/cli-activity", workspace.id),
                    json!({"hook_event_name":"SessionStart", "session_id":native}),
                    Some(&format!("{token}-fallback")),
                )
                .await;
                assert_eq!(status, StatusCode::NO_CONTENT);
            }
            if scenario == "late_fallback_receipt" {
                sqlx::query("UPDATE session_handovers SET fallback_started_at = ?, fallback_ready_at = ? WHERE operation_id = ?")
                    .bind(Utc::now()-Duration::seconds(901)).bind(Utc::now()).bind(operation_id).execute(pool).await.unwrap();
            }
            if scenario == "stale_fallback_identity" {
                sqlx::query("UPDATE session_handovers SET fallback_start_time = 'stale-process' WHERE operation_id = ?").bind(operation_id).execute(pool).await.unwrap();
                server::routes::session_handover::reconcile(&deployment, Utc::now())
                    .await
                    .unwrap();
                assert_ne!(
                    db::models::session_handover::SessionHandover::find(pool, operation_id)
                        .await
                        .unwrap()
                        .unwrap()
                        .phase,
                    "rolled_back"
                );
                server::routes::session_handover::reconcile(
                    &deployment,
                    pending.fallback_started_at.unwrap() + Duration::seconds(901),
                )
                .await
                .unwrap();
            } else {
                server::routes::session_handover::reconcile(&deployment, Utc::now())
                    .await
                    .unwrap();
            }
        }
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
        } else if matches!(
            scenario,
            "fallback_deadline" | "stale_fallback_identity" | "late_fallback_receipt"
        ) {
            assert_eq!(saved.phase, "failed");
            assert!(saved.failure.unwrap().contains("needs recovery"));
        } else if matches!(scenario, "fallback_failure" | "delayed_fallback_failure") {
            assert_eq!(saved.phase, "failed");
            assert!(
                saved
                    .failure
                    .unwrap()
                    .to_lowercase()
                    .contains("fallback failed")
            );
        } else {
            assert_eq!(saved.phase, "rolled_back", "{:?}", saved.failure);
            let args = fs::read_to_string(cwd.join("args")).unwrap();
            if scenario != "delayed_fallback_failure" {
                assert!(args.contains(&format!("--resume\n{native}\n")), "{args}");
            }
            let active = CliPaneBinding::find_active_for_workspace(pool, workspace.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(active.session_id, sessions[0].id);
            assert_eq!(active.claude_session_id.as_deref(), Some(native.as_str()));
            if scenario == "crash" {
                let (status,delivered)=request(&app,"POST",&format!("/api/sessions/{}/follow-up",sessions[0].id),json!({"prompt":"named fallback dashboard message","executor_config":{"executor":"CLAUDE_CODE"}}),None).await;
                assert_eq!(status, StatusCode::OK, "{delivered}");
                assert_eq!(delivered["data"]["outcome"], "routed_to_cli", "{delivered}");
            }
        }
        assert_eq!(
            request(&app, "POST", &confirm_url, json!({}), Some(&token))
                .await
                .0,
            StatusCode::CONFLICT
        );
        fs::write(&fake, &original_fake).unwrap();
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
