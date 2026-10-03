use api_types::{
    CliScreenResponse, LaunchCliAgentResponse, SendCliTextRequest, SendCliTextResponse,
};
use db::models::workspace::Workspace;
use rmcp::{
    ErrorData, handler::server::wrapper::Parameters, model::CallToolResult, schemars, tool,
    tool_router,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::McpServer;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct McpWorkspaceRequest {
    #[schemars(description = "Workspace ID. Optional if running inside that workspace context.")]
    workspace_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct McpSendWorkspaceMessageRequest {
    #[schemars(description = "Workspace whose CLI agent receives the message")]
    workspace_id: Uuid,
    #[schemars(description = "Text to type into the agent and submit, as if the user sent it")]
    text: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct McpWorkspaceDetails {
    id: String,
    name: Option<String>,
    branch: String,
    archived: bool,
    pinned: bool,
    #[schemars(description = "Absolute path of the workspace directory")]
    path: Option<String>,
    created_at: String,
    updated_at: String,
    #[schemars(
        description = "Sidebar summary: cli_phase (what the agent last reported: working, approval, question, stopped, ready, ended, ...), cli_phase_at, cli_activity_at, cli_attention, pr_url, pr_status"
    )]
    status: Option<serde_json::Value>,
}

#[tool_router(router = workspace_agents_tools_router, vis = "pub")]
impl McpServer {
    #[tool(
        description = "Get one workspace: name, branch, directory, archived/pinned state, and the status of its CLI agent and PR."
    )]
    async fn get_workspace(
        &self,
        Parameters(McpWorkspaceRequest { workspace_id }): Parameters<McpWorkspaceRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let workspace_id = match self.resolve_workspace_id(workspace_id) {
            Ok(id) => id,
            Err(e) => return Ok(Self::tool_error(e)),
        };
        let url = self.url(&format!("/api/workspaces/{workspace_id}"));
        let workspace: Workspace = match self.send_json(self.client.get(&url)).await {
            Ok(ws) => ws,
            Err(e) => return Ok(Self::tool_error(e)),
        };

        let summaries_url = self.url("/api/workspaces/summaries");
        let status = self
            .send_json::<serde_json::Value>(
                self.client
                    .post(&summaries_url)
                    .json(&serde_json::json!({ "archived": workspace.archived })),
            )
            .await
            .ok()
            .and_then(|data| {
                data["summaries"].as_array()?.iter().find_map(|summary| {
                    (summary["workspace_id"].as_str() == Some(&workspace_id.to_string()))
                        .then(|| summary.clone())
                })
            });

        McpServer::success(&McpWorkspaceDetails {
            id: workspace.id.to_string(),
            name: workspace.name,
            branch: workspace.branch,
            archived: workspace.archived,
            pinned: workspace.pinned,
            path: workspace.container_ref,
            created_at: workspace.created_at.to_rfc3339(),
            updated_at: workspace.updated_at.to_rfc3339(),
            status,
        })
    }

    #[tool(
        description = "Start the workspace's CLI agent (Claude Code or Codex) in the background, resuming its conversation and delivering any queued prompt. Use after unarchiving a workspace, or when its agent is not running. `launched: false` means it was already running."
    )]
    async fn launch_workspace_agent(
        &self,
        Parameters(McpWorkspaceRequest { workspace_id }): Parameters<McpWorkspaceRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let workspace_id = match self.resolve_workspace_id(workspace_id) {
            Ok(id) => id,
            Err(e) => return Ok(Self::tool_error(e)),
        };
        match self.launch_cli_agent(workspace_id).await {
            Ok(response) => McpServer::success(&response),
            Err(e) => Ok(Self::tool_error(e)),
        }
    }

    #[tool(
        description = "Send a message to the live CLI agent of another workspace: the text is typed into its prompt and submitted. Fails when no agent is running there (use launch_workspace_agent first)."
    )]
    async fn send_workspace_message(
        &self,
        Parameters(McpSendWorkspaceMessageRequest { workspace_id, text }): Parameters<
            McpSendWorkspaceMessageRequest,
        >,
    ) -> Result<CallToolResult, ErrorData> {
        let url = self.url(&format!("/api/workspaces/{workspace_id}/cli/send"));
        let response: SendCliTextResponse = match self
            .send_json(self.client.post(&url).json(&SendCliTextRequest { text }))
            .await
        {
            Ok(response) => response,
            Err(e) => return Ok(Self::tool_error(e)),
        };
        McpServer::success(&response)
    }

    #[tool(
        description = "Read the visible screen of a workspace's CLI agent pane, to see what the agent is doing or asking. `screen` is null when the workspace has no running pane."
    )]
    async fn read_workspace_screen(
        &self,
        Parameters(McpWorkspaceRequest { workspace_id }): Parameters<McpWorkspaceRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let workspace_id = match self.resolve_workspace_id(workspace_id) {
            Ok(id) => id,
            Err(e) => return Ok(Self::tool_error(e)),
        };
        let url = self.url(&format!("/api/workspaces/{workspace_id}/cli/screen"));
        let response: CliScreenResponse = match self.send_json(self.client.get(&url)).await {
            Ok(response) => response,
            Err(e) => return Ok(Self::tool_error(e)),
        };
        McpServer::success(&response)
    }
}

impl McpServer {
    pub(super) async fn launch_cli_agent(
        &self,
        workspace_id: Uuid,
    ) -> Result<LaunchCliAgentResponse, super::ToolError> {
        let url = self.url(&format!("/api/workspaces/{workspace_id}/cli/launch"));
        self.send_json(self.client.post(&url)).await
    }
}
