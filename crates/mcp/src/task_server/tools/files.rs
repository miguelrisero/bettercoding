use std::path::{Path, PathBuf};

use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{McpServer, ToolCallResult};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FileLinkRequest {
    #[schemars(
        description = "Path relative to the agent's current directory for the current workspace, or workspace-root relative when selecting a different workspace explicitly. Absolute paths inside the selected workspace are also accepted."
    )]
    path: String,
    #[schemars(description = "Workspace ID. Defaults to the current MCP workspace context.")]
    workspace_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FileLinkResponse {
    url: String,
    path: String,
}

#[tool_router(router = files_tools_router, vis = "pub")]
impl McpServer {
    #[tool(
        description = "Return a browser link that opens a workspace file (HTML, PDF, image, video, audio, text) in preview on the owner's computer or phone. Share this link rather than asking the owner to download. HTML pages open sandboxed: keep them self-contained with inline scripts and styles and data: image URLs. Link other files in the same tab with their own file_link URLs. Relative paths use the agent's current directory for the current workspace, and the workspace root for an explicitly selected different workspace."
    )]
    async fn file_link(
        &self,
        Parameters(FileLinkRequest { path, workspace_id }): Parameters<FileLinkRequest>,
    ) -> ToolCallResult {
        let workspace_id = match self.resolve_workspace_id(workspace_id) {
            Ok(id) => id,
            Err(error) => return Ok(Self::tool_error(error)),
        };
        if let Err(error) = self.scope_allows_workspace(workspace_id) {
            return Ok(Self::tool_error(error));
        }

        let cwd =
            if self.scoped_workspace_id() == Some(workspace_id) && Path::new(&path).is_relative() {
                match std::env::current_dir() {
                    Ok(cwd) => cwd,
                    Err(error) => {
                        return Self::err(
                            "Cannot resolve the current directory".to_string(),
                            Some(error.to_string()),
                        );
                    }
                }
            } else {
                PathBuf::new()
            };
        let path = file_link_path(&path, workspace_id, self.scoped_workspace_id(), &cwd);
        let Some(path) = path.to_str() else {
            return Self::err("Invalid path encoding", None::<&str>);
        };
        let url = self.url(&format!("/api/workspaces/{workspace_id}/files/link"));
        match self
            .send_json::<FileLinkResponse>(self.client.get(&url).query(&[("path", path)]))
            .await
        {
            Ok(response) => Self::success(&response),
            Err(error) => Ok(Self::tool_error(error)),
        }
    }
}

fn file_link_path(
    path: &str,
    workspace_id: Uuid,
    current_workspace: Option<Uuid>,
    cwd: &Path,
) -> PathBuf {
    if current_workspace == Some(workspace_id) {
        cwd.join(path)
    } else {
        PathBuf::from(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_server::McpContext;

    #[test]
    fn file_link_paths_use_cwd_only_for_the_current_workspace() {
        let current = Uuid::new_v4();
        let foreign = Uuid::new_v4();
        let root = std::env::current_dir().unwrap();
        let cwd = root.join("repo/subdir");
        assert_eq!(
            file_link_path("report.html", current, Some(current), &cwd),
            cwd.join("report.html")
        );
        assert_ne!(
            file_link_path("report.html", current, Some(current), &cwd),
            root.join("report.html")
        );
        let absolute = root.join("report.html");
        assert_eq!(
            file_link_path(absolute.to_str().unwrap(), current, Some(current), &cwd),
            absolute
        );
        assert_eq!(
            file_link_path("report.html", foreign, Some(current), &cwd),
            PathBuf::from("report.html")
        );
        assert_eq!(
            file_link_path("report.html", foreign, None, &cwd),
            PathBuf::from("report.html")
        );
        assert_eq!(
            file_link_path(absolute.to_str().unwrap(), foreign, Some(current), &cwd),
            absolute
        );
    }

    #[tokio::test]
    async fn file_link_requires_a_workspace_and_respects_orchestrator_scope() {
        super::super::tests::install_rustls_provider();
        let mut server = McpServer::new_orchestrator("http://127.0.0.1:1");
        let request = || FileLinkRequest {
            path: "report.html".to_string(),
            workspace_id: None,
        };
        let result = server.file_link(Parameters(request())).await.unwrap();
        assert_eq!(result.is_error, Some(true));
        let error = result.content[0].as_text().unwrap();
        assert!(error.text.contains("workspace_id is required"));

        server.context = Some(McpContext {
            organization_id: None,
            project_id: None,
            issue_id: None,
            orchestrator_session_id: None,
            workspace_id: Uuid::new_v4(),
            workspace_branch: "main".to_string(),
            workspace_repos: vec![],
        });
        let result = server
            .file_link(Parameters(FileLinkRequest {
                workspace_id: Some(Uuid::new_v4()),
                ..request()
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        let error = result.content[0].as_text().unwrap();
        assert!(
            error
                .text
                .contains("outside the configured workspace scope")
        );
    }
}
