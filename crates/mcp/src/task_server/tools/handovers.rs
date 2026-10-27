use api_types::handover::{
    AdoptWorkspaceSessionRequest, AdoptWorkspaceSessionResponse, ConfirmHandoverResponse,
    HandoverResponse, HandoverWorkspaceSessionRequest,
};
use rmcp::{
    ErrorData, handler::server::wrapper::Parameters, model::CallToolResult, schemars, tool,
    tool_router,
};
use serde::Deserialize;
use uuid::Uuid;

use super::McpServer;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct OperationRequest {
    operation_id: Uuid,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetHandoverRequest {
    #[schemars(
        description = "Operation ID; omit in the launched successor to read its inherited operation"
    )]
    operation_id: Option<Uuid>,
}

#[tool_router(router = handover_tools_router, vis = "pub")]
impl McpServer {
    #[tool(
        description = "Adopt an already running Claude session without changing the terminal or process. Requires the expected old binding/app/native IDs, exact pane and PID/start identity, matching workspace/cwd, and an open session-specific transcript or task directory. The server refuses ambiguous identity and foreign transcript ownership. This changes session association only."
    )]
    async fn adopt_workspace_session(
        &self,
        Parameters(request): Parameters<AdoptWorkspaceSessionRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let url = self.url("/api/sessions/adopt");
        match self
            .send_json::<AdoptWorkspaceSessionResponse>(self.client.post(url).json(&request))
            .await
        {
            Ok(response) => Self::success(&response),
            Err(error) => Ok(Self::tool_error(error)),
        }
    }

    #[tool(
        description = "Replace the expected Claude pane with a fresh successor and initial prompt. The server resumes the exact fallback Claude session if the successor exits or misses confirmation. Session IDs name BetterCoding sessions; fallback_session_id names the predecessor native Claude UUID. request_id makes retries idempotent."
    )]
    async fn handover_workspace_session(
        &self,
        Parameters(request): Parameters<HandoverWorkspaceSessionRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let url = self.url("/api/handovers");
        match self
            .send_json::<HandoverResponse>(self.client.post(url).json(&request))
            .await
        {
            Ok(response) => Self::success(&response),
            Err(error) => Ok(Self::tool_error(error)),
        }
    }

    #[tool(
        description = "Read a durable handover receipt, including its phase, deadline and fallback failure. The launched successor may omit operation_id to use its inherited operation."
    )]
    async fn get_handover(
        &self,
        Parameters(request): Parameters<GetHandoverRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let id = request
            .operation_id
            .or_else(|| std::env::var("BC_HANDOVER_OPERATION_ID").ok()?.parse().ok());
        let Some(id) = id else {
            return Self::err(
                "operation_id is required outside a launched successor",
                None,
            );
        };
        let url = self.url(&format!("/api/handovers/{id}"));
        match self
            .send_json::<HandoverResponse>(self.client.get(url))
            .await
        {
            Ok(response) => Self::success(&response),
            Err(error) => Ok(Self::tool_error(error)),
        }
    }

    #[tool(
        description = "Confirm a handover from its launched successor. The MCP process forwards its inherited launch capability; callers cannot select a confirming session. Persist the receipt before accepting CoC ownership. Cleanup requires confirmed handover and accepted ownership."
    )]
    async fn confirm_handover(
        &self,
        Parameters(request): Parameters<OperationRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let Ok(capability) = std::env::var("BC_HANDOVER_CAPABILITY") else {
            return Self::err("This MCP process has no launched successor identity", None);
        };
        if capability.is_empty() {
            return Self::err("This MCP process has no launched successor identity", None);
        }
        let url = self.url(&format!("/api/handovers/{}/confirm", request.operation_id));
        match self
            .send_json::<ConfirmHandoverResponse>(
                self.client
                    .post(url)
                    .header("x-bc-handover-capability", capability),
            )
            .await
        {
            Ok(response) => Self::success(&response),
            Err(error) => Ok(Self::tool_error(error)),
        }
    }
}
