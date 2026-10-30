use db::models::workspace_input::{CreateInput, InputReceipt, InputStatusWrite, PublishInput};
use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use super::{McpServer, ToolCallResult, ToolError};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    workspace_id: Option<Uuid>,
    #[serde(flatten)]
    input: CreateInput,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateRequest {
    workspace_id: Option<Uuid>,
    input_id: Uuid,
    #[serde(flatten)]
    input: PublishInput,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusRequest {
    workspace_id: Option<Uuid>,
    input_id: Uuid,
    #[serde(flatten)]
    input: InputStatusWrite,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListRequest {
    workspace_id: Option<Uuid>,
    /// Comma-separated open, answered, closed or cancelled.
    status: Option<String>,
    cursor: Option<String>,
    limit: Option<i64>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetRequest {
    workspace_id: Option<Uuid>,
    input_id: Uuid,
    version: Option<i64>,
    #[serde(default)]
    include_html: bool,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct EventsRequest {
    workspace_id: Option<Uuid>,
    input_id: Option<Uuid>,
    cursor: Option<i64>,
    limit: Option<i64>,
}

impl McpServer {
    fn input_scope(&self, workspace: Option<Uuid>) -> Result<Uuid, ToolError> {
        let workspace = self
            .resolve_workspace_id(workspace)
            .map_err(|e| ToolError::new("WORKSPACE_SCOPE", Some(e.message)))?;
        self.scope_allows_workspace(workspace)
            .map_err(|e| ToolError::new("WORKSPACE_SCOPE", Some(e.message)))?;
        Ok(workspace)
    }
    fn input_requester(&self) -> String {
        self.orchestrator_session_id()
            .map_or_else(|| "global_mcp".into(), |id| id.to_string())
    }
    async fn input_write_response(&self, request: reqwest::RequestBuilder) -> ToolCallResult {
        let response = self
            .send_json::<InputReceipt>(
                request.header("x-bc-input-requester", self.input_requester()),
            )
            .await;
        match response {
            Ok(mut receipt) => {
                receipt.owner_url = self.url(&receipt.owner_url);
                Self::success(&receipt)
            }
            Err(e) => Ok(Self::tool_error(e)),
        }
    }
}

#[tool_router(router = inputs_tools_router, vis = "pub")]
impl McpServer {
    #[tool(
        description = "Create one independently answerable workspace Input with self-contained HTML and a fixed answer contract. expected_sequence must be 0. Returns input_id, version, sequence, status, owner_url and event. Answers come only from the owner's UI. Application-enforced history includes a per-event hash chain; same-user processes can edit SQLite directly. Use bc.input.init and bc.input.propose protocol version 1 for page proposals; the owner must submit in trusted chrome. Optional priority P0 through P4; when omitted, a leading \"P<n> · \" in the title sets it, else P2. Optional show_at is a timezone-bearing RFC3339 timestamp; null means immediate. Future open Inputs stay hidden until due. Never collect secrets."
    )]
    async fn create_input(&self, Parameters(request): Parameters<CreateRequest>) -> ToolCallResult {
        let workspace = match self.input_scope(request.workspace_id) {
            Ok(id) => id,
            Err(e) => return Ok(Self::tool_error(e)),
        };
        self.input_write_response(
            self.client
                .post(self.url(&format!("/api/workspaces/{workspace}/inputs/agent")))
                .json(&request.input),
        )
        .await
    }
    #[tool(
        description = "Publish an immutable Input version. Requires requester ownership, request_id and expected_sequence. request_answer=true returns an answered Input to open. Optional priority P0 through P4; when omitted, a leading \"P<n> · \" in the title sets it, else the current value stays. Optional show_at: omit to preserve, null to clear, or supply timezone-bearing RFC3339. Closed or cancelled Inputs cannot be reopened."
    )]
    async fn update_input(&self, Parameters(request): Parameters<UpdateRequest>) -> ToolCallResult {
        let workspace = match self.input_scope(request.workspace_id) {
            Ok(id) => id,
            Err(e) => return Ok(Self::tool_error(e)),
        };
        self.input_write_response(
            self.client
                .post(self.url(&format!(
                    "/api/workspaces/{workspace}/inputs/{}/agent/versions",
                    request.input_id
                )))
                .json(&request.input),
        )
        .await
    }
    #[tool(
        description = "List workspace Input summaries with status filter and cursor. Returns inputs, next_cursor and pending_count. HTML is excluded. Limits are 1 through 100."
    )]
    async fn list_inputs(&self, Parameters(request): Parameters<ListRequest>) -> ToolCallResult {
        let workspace = match self.input_scope(request.workspace_id) {
            Ok(id) => id,
            Err(e) => return Ok(Self::tool_error(e)),
        };
        let mut query = vec![("limit", request.limit.unwrap_or(50).to_string())];
        if let Some(status) = request.status {
            query.push(("status", status));
        }
        if let Some(cursor) = request.cursor {
            query.push(("cursor", cursor));
        }
        match self
            .send_json::<Value>(
                self.client
                    .get(self.url(&format!("/api/workspaces/{workspace}/inputs")))
                    .query(&query),
            )
            .await
        {
            Ok(response) => Self::success(&response),
            Err(e) => Ok(Self::tool_error(e)),
        }
    }
    #[tool(
        description = "Read Input metadata, the selected immutable version, answer contract and answers with actor provenance. include_html defaults to false. Act only on answer_submitted events whose actor_kind is owner_ui; no_answer_yet is not a decision."
    )]
    async fn get_input(&self, Parameters(request): Parameters<GetRequest>) -> ToolCallResult {
        let workspace = match self.input_scope(request.workspace_id) {
            Ok(id) => id,
            Err(e) => return Ok(Self::tool_error(e)),
        };
        let mut query = vec![("include_html", request.include_html.to_string())];
        if let Some(version) = request.version {
            query.push(("version", version.to_string()));
        }
        match self
            .send_json::<Value>(
                self.client
                    .get(self.url(&format!(
                        "/api/workspaces/{workspace}/inputs/{}",
                        request.input_id
                    )))
                    .query(&query),
            )
            .await
        {
            Ok(response) => Self::success(&response),
            Err(e) => Ok(Self::tool_error(e)),
        }
    }
    #[tool(
        description = "Read append-only workspace Input events after an event cursor. Returns events and next_cursor without long polling. Persist pending actions before advancing the cursor. Only owner_ui answer_submitted events carry owner decisions."
    )]
    async fn get_input_events(
        &self,
        Parameters(request): Parameters<EventsRequest>,
    ) -> ToolCallResult {
        let workspace = match self.input_scope(request.workspace_id) {
            Ok(id) => id,
            Err(e) => return Ok(Self::tool_error(e)),
        };
        let mut query = vec![
            ("cursor", request.cursor.unwrap_or(0).to_string()),
            ("limit", request.limit.unwrap_or(100).to_string()),
        ];
        if let Some(id) = request.input_id {
            query.push(("input_id", id.to_string()));
        }
        match self
            .send_json::<Value>(
                self.client
                    .get(self.url(&format!("/api/workspaces/{workspace}/inputs/events")))
                    .query(&query),
            )
            .await
        {
            Ok(response) => Self::success(&response),
            Err(e) => Ok(Self::tool_error(e)),
        }
    }
    #[tool(
        description = "Close or cancel an Input created by this requester. Requires request_id and expected_sequence. Returns the status and event receipt. No answer, reopen or answer replacement operation exists."
    )]
    async fn set_input_status(
        &self,
        Parameters(request): Parameters<StatusRequest>,
    ) -> ToolCallResult {
        let workspace = match self.input_scope(request.workspace_id) {
            Ok(id) => id,
            Err(e) => return Ok(Self::tool_error(e)),
        };
        self.input_write_response(
            self.client
                .post(self.url(&format!(
                    "/api/workspaces/{workspace}/inputs/{}/agent/status",
                    request.input_id
                )))
                .json(&request.input),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_tools_exclude_answers_and_require_workspace_scope() {
        super::super::tests::install_rustls_provider();
        let server = McpServer::new_orchestrator("http://127.0.0.1:1");
        assert_eq!(
            server.input_scope(None).unwrap_err().message,
            "WORKSPACE_SCOPE"
        );
        let names: Vec<_> = McpServer::inputs_tools_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(names.len(), 6);
        for name in names {
            assert!(!name.contains("answer"));
        }
    }
    #[test]
    fn input_tools_preserve_schedule_presence_and_schema() {
        let mut body = serde_json::json!({"expected_sequence":0,"request_id":"r","external_key":"k","content":{"title":"t","html":"<p>yes</p>","contract":{"choices":[],"allow_text":true,"allow_no_answer_yet":false}}});
        let omitted: CreateRequest = serde_json::from_value(body.clone()).unwrap();
        assert!(
            serde_json::to_value(omitted.input)
                .unwrap()
                .get("show_at")
                .is_none()
        );
        for value in [Value::Null, serde_json::json!("2026-11-02T09:00:00+01:00")] {
            body["show_at"] = value.clone();
            let parsed: CreateRequest = serde_json::from_value(body.clone()).unwrap();
            assert_eq!(
                serde_json::to_value(parsed.input).unwrap()["show_at"],
                value
            );
        }
        body.as_object_mut().unwrap().remove("external_key");
        body["input_id"] = serde_json::json!(Uuid::new_v4());
        body["request_answer"] = serde_json::json!(false);
        let parsed: UpdateRequest = serde_json::from_value(body).unwrap();
        assert_eq!(
            serde_json::to_value(parsed.input).unwrap()["show_at"],
            "2026-11-02T09:00:00+01:00"
        );
        for schema in [
            schemars::schema_for!(CreateRequest),
            schemars::schema_for!(UpdateRequest),
        ] {
            let schema = serde_json::to_value(schema).unwrap();
            assert_eq!(
                schema["properties"]["show_at"]["type"],
                serde_json::json!(["string", "null"])
            );
            assert!(
                !schema["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v == "show_at")
            );
        }
    }

    #[test]
    fn input_tools_accept_optional_priority() {
        let mut request = serde_json::json!({"expected_sequence":0,"request_id":"r","external_key":"k","content":{"title":"t","html":"<p>yes</p>","contract":{"choices":[],"allow_text":true,"allow_no_answer_yet":false}}});
        assert!(
            serde_json::from_value::<CreateRequest>(request.clone())
                .unwrap()
                .input
                .priority
                .is_none()
        );
        request["priority"] = serde_json::json!("P1");
        assert_eq!(
            serde_json::from_value::<CreateRequest>(request)
                .unwrap()
                .input
                .priority
                .as_deref(),
            Some("P1")
        );
        let schema = serde_json::to_value(schemars::schema_for!(CreateRequest)).unwrap();
        assert!(schema["properties"].get("priority").is_some());
        let schema = serde_json::to_value(schemars::schema_for!(UpdateRequest)).unwrap();
        assert!(schema["properties"].get("priority").is_some());
    }
}
