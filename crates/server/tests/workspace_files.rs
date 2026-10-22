use std::{fs, process::Command};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use db::models::{
    repo::Repo,
    workspace::{CreateWorkspace, Workspace},
    workspace_repo::{CreateWorkspaceRepo, WorkspaceRepo},
};
use deployment::Deployment;
use local_deployment::LocalDeployment;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use tower_http::compression::CompressionLayer;
use uuid::Uuid;

#[tokio::test]
async fn workspace_files_routes_preserve_paths_conditionals_and_input_limits() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let data = root.join("data");
    let worktrees = root.join("worktrees");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&worktrees).unwrap();
    // This integration target owns its environment before deployment caches initialise.
    unsafe {
        std::env::set_var("BC_DATA_DIR", &data);
        std::env::set_var("BC_WORKTREE_BASE", &worktrees);
        std::env::set_var("BC_ALLOWED_ORIGINS", "https://preview.example");
        std::env::remove_var("BC_SHARED_API_BASE");
        std::env::remove_var("BC_SHARED_RELAY_API_BASE");
    }
    let repo_path = root.join("repo");
    fs::create_dir(&repo_path).unwrap();
    for args in [
        vec!["init", "-b", "main"],
        vec![
            "-c",
            "user.name=Preview Test",
            "-c",
            "user.email=preview@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    ] {
        let output = Command::new("git")
            .current_dir(&repo_path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let shutdown = CancellationToken::new();
    let deployment = LocalDeployment::new(shutdown.child_token()).await.unwrap();
    let workspace = Workspace::create(
        &deployment.db().pool,
        &CreateWorkspace {
            branch: "preview-test".into(),
            name: Some("preview".into()),
        },
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let repo = Repo::find_or_create(&deployment.db().pool, &repo_path, "repo")
        .await
        .unwrap();
    WorkspaceRepo::create_many(
        &deployment.db().pool,
        workspace.id,
        &[CreateWorkspaceRepo {
            repo_id: repo.id,
            target_branch: "main".into(),
        }],
    )
    .await
    .unwrap();
    let base = worktrees.join("preview");
    fs::create_dir(&base).unwrap();
    Workspace::update_container_ref(&deployment.db().pool, workspace.id, base.to_str().unwrap())
        .await
        .unwrap();
    fs::create_dir(base.join("report")).unwrap();
    fs::write(base.join("report/final.html"), b"nested file").unwrap();
    fs::write(base.join("page.html"), b"preview document").unwrap();
    #[cfg(unix)]
    fs::write(base.join("report\\final.html"), b"literal backslash file").unwrap();
    let app = Router::new()
        .nest("/api", server::routes::workspaces::router(&deployment))
        .with_state(deployment.clone())
        .layer(CompressionLayer::new());
    let prefix = format!("/api/workspaces/{}/files", workspace.id);
    let response = app
        .clone()
        .oneshot(
            Request::get(format!("{prefix}/preview/page.html"))
                .header(header::HOST, "preview.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let modified = response.headers()[header::LAST_MODIFIED].clone();
    assert_eq!(
        to_bytes(response.into_body(), 1024).await.unwrap(),
        "preview document"
    );
    let response = app
        .clone()
        .oneshot(
            Request::get(format!("{prefix}/preview/page.html"))
                .header(header::IF_MODIFIED_SINCE, modified)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert!(!response.headers().contains_key(header::LOCATION));
    assert!(to_bytes(response.into_body(), 0).await.unwrap().is_empty());

    #[cfg(unix)]
    {
        let response = app
            .clone()
            .oneshot(
                Request::get(format!("{prefix}/list"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let listing: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert!(
            listing["data"]["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["path"] == "report\\final.html")
        );
        for (path, expected) in [
            ("report%5Cfinal.html", "literal backslash file"),
            ("report%2Ffinal.html", "nested file"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::get(format!("{prefix}/link?path={path}"))
                        .header(header::HOST, "preview.example")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let link: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap())
                    .unwrap();
            let url = url::Url::parse(link["data"]["url"].as_str().unwrap()).unwrap();
            assert_eq!(
                url.origin().ascii_serialization(),
                "https://preview.example"
            );
            if path.contains("%5C") {
                assert_eq!(link["data"]["path"], "report\\final.html");
                assert!(url.path().ends_with("report%5Cfinal.html"));
            }
            let response = app
                .clone()
                .oneshot(Request::get(url.path()).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                to_bytes(response.into_body(), 1024).await.unwrap(),
                expected
            );
        }
    }

    let body = format!("\"{}\"", "a".repeat(256 * 1024 - 2));
    let response = app
        .clone()
        .oneshot(
            Request::put(format!("{prefix}/inputs?name=answers"))
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        fs::read(base.join(".bettercoding-uploads/inputs/answers.json")).unwrap(),
        body.as_bytes()
    );
    let response = app
        .oneshot(
            Request::put(format!("{prefix}/inputs?name=answers"))
                .body(Body::from(format!("{body} ")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        fs::read(base.join(".bettercoding-uploads/inputs/answers.json")).unwrap(),
        body.as_bytes()
    );
    shutdown.cancel();
}
