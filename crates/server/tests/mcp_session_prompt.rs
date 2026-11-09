#![cfg(all(target_os = "linux", not(feature = "qa-mode")))]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    panic::AssertUnwindSafe,
    process::Stdio,
    time::Duration,
};

use db::models::{
    execution_process::{ExecutionProcess, ExecutionProcessStatus},
    repo::Repo,
    session::Session,
    workspace::{CreateWorkspace, Workspace},
    workspace_repo::{CreateWorkspaceRepo, WorkspaceRepo},
};
use deployment::Deployment;
use futures_util::FutureExt;
use local_deployment::LocalDeployment;
use serde_json::{Value, json};
use services::services::{config::Config, container::ContainerService};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::{ChildStdin, ChildStdout},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workspace_manager::WorkspaceManager;

async fn rpc(input: &mut ChildStdin, output: &mut BufReader<ChildStdout>, message: Value) -> Value {
    let id = message["id"].clone();
    input
        .write_all(format!("{message}\n").as_bytes())
        .await
        .unwrap();
    input.flush().await.unwrap();
    loop {
        let mut line = String::new();
        assert_ne!(
            output.read_line(&mut line).await.unwrap(),
            0,
            "MCP closed stdout"
        );
        let response: Value = serde_json::from_str(&line).unwrap();
        if response["id"] == id {
            assert!(response.get("error").is_none(), "{response}");
            return response["result"].clone();
        }
    }
}

fn tool_payload(result: Value) -> Value {
    assert_ne!(result["isError"], true, "{result}");
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn stdio_session_prompt_returns_started_for_the_owned_executor() {
    // Regresses decoding the dispatch envelope as a bare ExecutionProcess.
    // This integration binary owns process-wide caches and environment.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for dir in ["home", "data", "worktrees", "bin", "tmp", "repo", "mcp"] {
        fs::create_dir(root.join(dir)).unwrap();
    }
    let marker = root.join("agent-input");
    let agent = root.join("bin/claude");
    fs::write(
        &agent,
        "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$$\" > \"$BC_TEST_MARKER.pid\"\nIFS= read -r initialize\nIFS= read -r permissions\nIFS= read -r prompt\nprintf '%s\\n' \"$prompt\" > \"$BC_TEST_MARKER\"\ni=0\nwhile [ \"$i\" -lt 300 ]; do /bin/sleep 0.1; i=$((i + 1)); done\n",
    )
    .unwrap();
    // Headless Claude uses npx. This fixed shim never invokes a package manager.
    fs::write(
        root.join("bin/npx"),
        "#!/bin/sh\nexec /usr/bin/timeout --foreground 30 \"$BC_TEST_FAKE_AGENT\" \"$@\"\n",
    )
    .unwrap();
    for path in [&agent, &root.join("bin/npx")] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let workspace_id = Uuid::new_v4();
    eprintln!(
        "MCP fixture owns workspace {workspace_id} and {}",
        root.display()
    );
    unsafe {
        for (name, path) in [
            ("HOME", root.join("home")),
            ("XDG_CONFIG_HOME", root.join("home/config")),
            ("XDG_DATA_HOME", root.join("home/data")),
            ("BC_DATA_DIR", root.join("data")),
            ("BC_WORKTREE_BASE", root.join("worktrees")),
            ("TMPDIR", root.join("tmp")),
            ("TMUX_TMPDIR", root.join("tmp")),
            ("BC_TEST_FAKE_AGENT", agent.clone()),
            ("BC_TEST_MARKER", marker.clone()),
        ] {
            std::env::set_var(name, path);
        }
        std::env::set_var(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        );
        std::env::set_var("SHELL", "/bin/sh");
        std::env::set_var("BC_CLI_TMUX_SOCKET", format!("bc-mcp-{workspace_id}"));
        std::env::set_var("DISABLE_CLI_SESSION_REAP", "1");
        std::env::set_var("DISABLE_CLI_TRANSCRIPT_INGEST", "1");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_TERMINAL_PROMPT", "0");
        // Override compile-time remote endpoints with an inert loopback endpoint.
        std::env::set_var("BC_SHARED_API_BASE", "http://127.0.0.1:1");
        std::env::set_var("BC_SHARED_RELAY_API_BASE", "http://127.0.0.1:1");
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "POSTHOG_API_KEY",
            "POSTHOG_API_ENDPOINT",
            "SENTRY_DSN",
            "SENTRY_DSN_REMOTE",
            "GIT_TEMPLATE_DIR",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
        ] {
            std::env::remove_var(name);
        }
    }
    let mut config = Config::default();
    config.onboarding_acknowledged = true;
    config.analytics_enabled = false;
    config.pr_auto_description_enabled = false;
    config.commit_reminder_enabled = false;
    config.notifications.sound_enabled = false;
    config.notifications.push_enabled = false;
    fs::write(
        root.join("data/config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let shutdown = CancellationToken::new();
    let deployment = LocalDeployment::new(shutdown.child_token()).await.unwrap();
    let workspace = Workspace::create(
        &deployment.db().pool,
        &CreateWorkspace {
            branch: "mcp-fixture".into(),
            name: None,
        },
        workspace_id,
    )
    .await
    .unwrap();

    let outcome = AssertUnwindSafe(async {
        let repo_path = root.join("repo");
        for args in [
            vec!["init", "-b", "main"],
            vec![
                "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                "commit", "--allow-empty", "-m", "fixture",
            ],
        ] {
            let mut git = tokio::process::Command::new("git");
            git.arg("-c")
                .arg("core.hooksPath=/dev/null")
                .arg("-C")
                .arg(&repo_path)
                .args(args)
                .kill_on_drop(true);
            assert!(
                tokio::time::timeout(Duration::from_secs(5), git.status())
                    .await.unwrap().unwrap().success()
            );
        }
        let pool = &deployment.db().pool;
        let repo = Repo::find_or_create(pool, &repo_path, "fixture").await.unwrap();
        WorkspaceRepo::create_many(
            pool,
            workspace_id,
            &[CreateWorkspaceRepo {
                repo_id: repo.id,
                target_branch: "main".into(),
            }],
        )
        .await
        .unwrap();
        let container = root.join("worktrees/fixture");
        Workspace::update_container_ref(pool, workspace_id, container.to_str().unwrap())
            .await
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        // The main server passes this same IntoMakeService to axum::serve.
        let app = server::routes::router(deployment.clone());
        let server_shutdown = shutdown.child_token();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(server_shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        let mut mcp = tokio::process::Command::new(env!("CARGO_BIN_EXE_server"))
            .args(["mcp", "--mode", "global"])
            .current_dir(root.join("mcp"))
            .env("BC_BACKEND_URL", base_url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut input = mcp.stdin.take().unwrap();
        let mut output = BufReader::new(mcp.stdout.take().unwrap());
        tokio::time::timeout(Duration::from_secs(20), async {
            rpc(&mut input, &mut output, json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-03-26", "capabilities": {},
                    "clientInfo": {"name": "fixture", "version": "1"}}
            })).await;
            input.write_all(
                b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n"
            ).await.unwrap();
            let created = tool_payload(rpc(&mut input, &mut output, json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "create_session", "arguments": {
                    "workspace_id": workspace_id, "executor": "CLAUDE_CODE"}}
            })).await);
            let session_id = Uuid::parse_str(created["session"]["id"].as_str().unwrap()).unwrap();
            let receipt = tool_payload(rpc(&mut input, &mut output, json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "run_session_prompt", "arguments": {
                    "session_id": session_id, "prompt": "fixture prompt"}}
            })).await);
            assert_eq!(receipt["outcome"], "started", "{receipt}");
            assert_eq!(receipt["session_id"], session_id.to_string());
            let execution_id = Uuid::parse_str(receipt["execution_id"].as_str().unwrap()).unwrap();
            let process = ExecutionProcess::find_by_id(pool, execution_id).await.unwrap().unwrap();
            assert_eq!(process.session_id, session_id);
            assert_eq!(process.status, ExecutionProcessStatus::Running);
            assert_eq!(receipt["execution"]["id"], execution_id.to_string());
            assert_eq!(receipt["execution"]["session_id"], session_id.to_string());
            let delivered: Value = loop {
                if let Ok(raw) = fs::read_to_string(&marker)
                    && let Ok(value) = serde_json::from_str(&raw)
                {
                    break value;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            assert_eq!(delivered["message"]["content"], "fixture prompt");
            let pid: u32 = fs::read_to_string(marker.with_extension("pid"))
                .unwrap().trim().parse().unwrap();
            assert_eq!(
                fs::read_link(format!("/proc/{pid}/cwd")).unwrap(),
                container.join(&repo.name)
            );
            let argv = fs::read(format!("/proc/{pid}/cmdline")).unwrap();
            assert!(argv.split(|byte| *byte == 0).any(|arg| arg == agent.as_os_str().as_encoded_bytes()));
        })
        .await
        .expect("MCP dispatch exceeds fixture deadline");
        drop(input);
        tokio::time::timeout(Duration::from_secs(3), mcp.wait())
            .await
            .unwrap()
            .unwrap();
    })
    .catch_unwind()
    .await;

    // Stop only this UUID's executor before deleting its managed short directory.
    let cleanup = tokio::time::timeout(Duration::from_secs(10), async {
        deployment.container().try_stop(&workspace, true).await;
        let pool = &deployment.db().pool;
        for session in Session::find_by_workspace_id(pool, workspace_id).await? {
            let processes = ExecutionProcess::find_by_session_id(pool, session.id, false).await?;
            if processes.iter().any(|process| process.status == ExecutionProcessStatus::Running) {
                return Err(sqlx::Error::Protocol("fixture executor remains running".into()));
            }
        }
        WorkspaceManager::delete_record_with_short_tmp(pool, workspace_id).await
    })
    .await;
    shutdown.cancel();
    if outcome.is_err() || !matches!(cleanup, Ok(Ok(_))) {
        let evidence = temp.keep();
        eprintln!(
            "MCP fixture failure evidence: {}; workspace {workspace_id}; cleanup {cleanup:?}",
            evidence.display()
        );
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
        panic!("MCP fixture cleanup exceeds its deadline or fails");
    }
}
