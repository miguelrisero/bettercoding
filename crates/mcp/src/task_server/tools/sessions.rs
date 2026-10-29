use db::models::{
    execution_process::{ExecutionProcess, ExecutionProcessStatus},
    session::Session,
};
use rmcp::{
    ErrorData, handler::server::wrapper::Parameters, model::CallToolResult, schemars, tool,
    tool_router,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::McpServer;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CreateSessionRequest {
    #[schemars(
        description = "Workspace ID to create the session in. Optional when running inside a scoped orchestrator MCP."
    )]
    workspace_id: Option<Uuid>,
    #[schemars(description = "Optional executor to pin this session to")]
    executor: Option<String>,
    #[schemars(description = "Optional display name for the session")]
    name: Option<String>,
}

#[derive(Debug, Serialize)]
struct CreateSessionPayload {
    workspace_id: Uuid,
    executor: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct SessionSummary {
    #[schemars(description = "Session ID")]
    id: String,
    #[schemars(description = "Workspace ID")]
    workspace_id: String,
    #[schemars(description = "Session display name (if set)")]
    name: Option<String>,
    #[schemars(description = "Session executor (if set)")]
    executor: Option<String>,
    #[schemars(description = "Creation timestamp")]
    created_at: String,
    #[schemars(description = "Last update timestamp")]
    updated_at: String,
    #[schemars(description = "True if this is the orchestrator session for this MCP server")]
    is_orchestrator_session: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct CreateSessionResponse {
    session: SessionSummary,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListSessionsRequest {
    #[schemars(
        description = "Workspace ID to inspect. Optional when running inside a scoped orchestrator MCP."
    )]
    workspace_id: Option<Uuid>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ListSessionsResponse {
    #[schemars(description = "Workspace ID this result is scoped to")]
    workspace_id: String,
    total_count: usize,
    sessions: Vec<SessionSummary>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RunCodingAgentInSessionRequest {
    #[schemars(description = "Session ID to run the coding agent in")]
    session_id: Uuid,
    #[schemars(description = "Prompt for the coding agent")]
    prompt: String,
}

#[derive(Debug, Serialize)]
struct FollowUpPayload {
    prompt: String,
    executor_config: ExecutorConfigPayload,
    retry_process_id: Option<Uuid>,
    force_when_dirty: Option<bool>,
    perform_git_reset: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ExecutorConfigPayload {
    executor: String,
    variant: Option<String>,
    model_id: Option<String>,
    agent_id: Option<String>,
    reasoning_id: Option<String>,
    permission_policy: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum FollowUpOutcome {
    Started {
        execution_process: Box<ExecutionProcess>,
    },
    Queued {
        #[serde(deserialize_with = "deserialize_receipt")]
        status: serde_json::Value,
    },
    RoutedToCli {
        #[serde(deserialize_with = "deserialize_receipt")]
        delivery: serde_json::Value,
    },
    Conflict {
        #[serde(deserialize_with = "deserialize_receipt")]
        status: serde_json::Value,
    },
}

// Preserve the complete receipt while rejecting missing, null, or scalar payloads.
fn deserialize_receipt<'de, D>(deserializer: D) -> Result<serde_json::Value, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde_json::Map::deserialize(deserializer).map(serde_json::Value::Object)
}

#[derive(Debug, Deserialize)]
struct FollowUpResponse {
    success: bool,
    data: Option<FollowUpOutcome>,
    error_data: Option<FollowUpOutcome>,
    message: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct UpdateSessionRequest {
    #[schemars(description = "Session ID to update")]
    session_id: Uuid,
    #[schemars(description = "Set session display name (empty string clears it)")]
    name: Option<String>,
    #[schemars(
        description = "Set the model the session's next CLI launch uses, e.g. `claude-opus-5-5` (empty string clears it). A running agent keeps its model until it is relaunched."
    )]
    cli_model_id: Option<String>,
    #[schemars(
        description = "Set the reasoning effort the session's next CLI launch uses (empty string clears it)"
    )]
    cli_reasoning_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct UpdateSessionPayload {
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cli_model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cli_reasoning_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UpdatedSession {
    #[serde(flatten)]
    session: Session,
    cli_model_id: Option<String>,
    cli_reasoning_id: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct UpdateSessionResponse {
    success: bool,
    session_id: String,
    name: Option<String>,
    cli_model_id: Option<String>,
    cli_reasoning_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetExecutionRequest {
    #[schemars(description = "Execution ID to inspect")]
    execution_id: Uuid,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct GetExecutionResponse {
    execution_id: String,
    session_id: String,
    status: String,
    is_finished: bool,
    execution: serde_json::Value,
    #[schemars(description = "Final assistant message/summary when execution has finished")]
    final_message: Option<String>,
}

#[tool_router(router = session_tools_router, vis = "pub")]
impl McpServer {
    #[tool(description = "Create a new session in a workspace.")]
    async fn create_session(
        &self,
        Parameters(CreateSessionRequest {
            workspace_id,
            executor,
            name,
        }): Parameters<CreateSessionRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let workspace_id = match self.resolve_workspace_id(workspace_id) {
            Ok(id) => id,
            Err(error_result) => return Ok(Self::tool_error(error_result)),
        };
        if let Err(error_result) = self.scope_allows_workspace(workspace_id) {
            return Ok(Self::tool_error(error_result));
        }

        let payload = CreateSessionPayload {
            workspace_id,
            executor: executor.and_then(|value| {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            }),
            name: name.and_then(|value| {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            }),
        };

        let url = self.url("/api/sessions");
        let session: Session = match self.send_json(self.client.post(&url).json(&payload)).await {
            Ok(value) => value,
            Err(error_result) => return Ok(Self::tool_error(error_result)),
        };

        Self::success(&CreateSessionResponse {
            session: self.session_summary(session),
        })
    }

    #[tool(description = "List all sessions for a workspace.")]
    async fn list_sessions(
        &self,
        Parameters(ListSessionsRequest { workspace_id }): Parameters<ListSessionsRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let workspace_id = match self.resolve_workspace_id(workspace_id) {
            Ok(id) => id,
            Err(error_result) => return Ok(Self::tool_error(error_result)),
        };
        if let Err(error_result) = self.scope_allows_workspace(workspace_id) {
            return Ok(Self::tool_error(error_result));
        }

        let url = self.url(&format!("/api/sessions?workspace_id={workspace_id}"));
        let sessions: Vec<Session> = match self.send_json(self.client.get(&url)).await {
            Ok(value) => value,
            Err(error_result) => return Ok(Self::tool_error(error_result)),
        };

        let sessions = sessions
            .into_iter()
            .map(|session| self.session_summary(session))
            .collect::<Vec<_>>();

        Self::success(&ListSessionsResponse {
            workspace_id: workspace_id.to_string(),
            total_count: sessions.len(),
            sessions,
        })
    }

    #[tool(
        description = "Update a session's name and the model and reasoning effort of its next CLI launch. `session_id` is required. The response reads the stored launch values back."
    )]
    async fn update_session(
        &self,
        Parameters(UpdateSessionRequest {
            session_id,
            name,
            cli_model_id,
            cli_reasoning_id,
        }): Parameters<UpdateSessionRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        // Verify session exists and check scope
        let session_url = self.url(&format!("/api/sessions/{session_id}"));
        let session: Session = match self.send_json(self.client.get(&session_url)).await {
            Ok(value) => value,
            Err(error_result) => return Ok(Self::tool_error(error_result)),
        };
        if let Err(error_result) = self.scope_allows_workspace(session.workspace_id) {
            return Ok(Self::tool_error(error_result));
        }

        let payload = UpdateSessionPayload {
            name: name.map(|value| value.trim().to_string()),
            cli_model_id,
            cli_reasoning_id,
        };
        let url = self.url(&format!("/api/sessions/{session_id}"));
        let updated: UpdatedSession =
            match self.send_json(self.client.put(&url).json(&payload)).await {
                Ok(value) => value,
                Err(error_result) => return Ok(Self::tool_error(error_result)),
            };

        Self::success(&UpdateSessionResponse {
            success: true,
            session_id: updated.session.id.to_string(),
            name: updated.session.name,
            cli_model_id: updated.cli_model_id,
            cli_reasoning_id: updated.cli_reasoning_id,
        })
    }

    #[tool(
        description = "Dispatch a coding agent prompt in an existing session. Returns started execution details, a queued status, or a CLI delivery receipt. Poll get_execution only for started outcomes. A dispatch conflict returns a tool error with the current status."
    )]
    async fn run_session_prompt(
        &self,
        Parameters(RunCodingAgentInSessionRequest { session_id, prompt }): Parameters<
            RunCodingAgentInSessionRequest,
        >,
    ) -> Result<CallToolResult, ErrorData> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Self::err("prompt must not be empty", None);
        }

        let session_url = self.url(&format!("/api/sessions/{session_id}"));
        let session: Session = match self.send_json(self.client.get(&session_url)).await {
            Ok(value) => value,
            Err(error_result) => return Ok(Self::tool_error(error_result)),
        };
        if let Err(error_result) = self.scope_allows_workspace(session.workspace_id) {
            return Ok(Self::tool_error(error_result));
        }
        if self.orchestrator_session_id() == Some(session_id) {
            return Self::err(
                "Cannot run coding agent in the orchestrator session".to_string(),
                Some(
                    "Create or re-use a different session and run the coding agent there."
                        .to_string(),
                ),
            );
        }

        let executor_config = match Self::executor_config_payload_for_session(&session) {
            Ok(config) => config,
            Err(error_result) => return Ok(Self::tool_error(error_result)),
        };

        let payload = FollowUpPayload {
            prompt: prompt.to_string(),
            executor_config,
            retry_process_id: None,
            force_when_dirty: None,
            perform_git_reset: None,
        };

        let url = self.url(&format!("/api/sessions/{session_id}/follow-up"));
        // Send once: a parse failure can still follow a successful dispatch.
        let response = match self.client.post(&url).json(&payload).send().await {
            Ok(response) => response,
            Err(error) => {
                return Ok(Self::tool_error(super::ToolError::new(
                    "Failed to connect to BetterCoding API",
                    Some(error.to_string()),
                )));
            }
        };
        let http_status = response.status();
        if !http_status.is_success() && http_status != reqwest::StatusCode::CONFLICT {
            return Ok(Self::tool_error(Self::status_error(response).await));
        }
        let response = match response.json::<FollowUpResponse>().await {
            Ok(response) => response,
            Err(error) => {
                return Ok(Self::tool_error(super::ToolError::new(
                    "Failed to parse BetterCoding API response",
                    Some(error.to_string()),
                )));
            }
        };

        let outcome = match (
            http_status.is_success(),
            response.success,
            response.data,
            response.error_data,
        ) {
            (true, true, Some(outcome), None) => outcome,
            (false, false, None, Some(FollowUpOutcome::Conflict { status })) => {
                return Ok(CallToolResult::error(vec![rmcp::model::Content::text(
                    serde_json::json!({
                        "success": false,
                        "error": "Session dispatch conflict",
                        "session_id": session_id,
                        "outcome": "conflict",
                        "status": status,
                    })
                    .to_string(),
                )]));
            }
            _ if !http_status.is_success() => {
                return Ok(Self::tool_error(super::ToolError::new(
                    format!("BetterCoding API returned error status: {http_status}"),
                    response.message,
                )));
            }
            _ => {
                return Self::err(
                    "Invalid session dispatch response".to_string(),
                    response.message,
                );
            }
        };

        let mut result = serde_json::json!({"session_id": session_id});
        match outcome {
            FollowUpOutcome::Started { execution_process } => {
                let execution = match Self::serialize_execution_process(&execution_process) {
                    Ok(value) => value,
                    Err(error) => return Ok(Self::tool_error(error)),
                };
                result["outcome"] = serde_json::json!("started");
                result["execution_id"] = serde_json::json!(execution_process.id);
                result["execution"] = execution;
            }
            FollowUpOutcome::Queued { status } => {
                result["outcome"] = serde_json::json!("queued");
                result["status"] = status;
            }
            FollowUpOutcome::RoutedToCli { delivery } => {
                result["outcome"] = serde_json::json!("routed_to_cli");
                result["delivery"] = delivery;
            }
            FollowUpOutcome::Conflict { .. } => {
                return Self::err("Invalid session dispatch response", None);
            }
        }
        Self::success(&result)
    }

    #[tool(description = "Get status for an execution.")]
    async fn get_execution(
        &self,
        Parameters(GetExecutionRequest { execution_id }): Parameters<GetExecutionRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let process_url = self.url(&format!("/api/execution-processes/{execution_id}"));
        let execution_process: ExecutionProcess =
            match self.send_json(self.client.get(&process_url)).await {
                Ok(value) => value,
                Err(error_result) => return Ok(Self::tool_error(error_result)),
            };

        let session_url = self.url(&format!("/api/sessions/{}", execution_process.session_id));
        let session: Session = match self.send_json(self.client.get(&session_url)).await {
            Ok(value) => value,
            Err(error_result) => return Ok(Self::tool_error(error_result)),
        };
        if let Err(error_result) = self.scope_allows_workspace(session.workspace_id) {
            return Ok(Self::tool_error(error_result));
        }

        let is_finished = execution_process.status != ExecutionProcessStatus::Running;

        let execution_process_value = match Self::serialize_execution_process(&execution_process) {
            Ok(value) => value,
            Err(error_result) => return Ok(Self::tool_error(error_result)),
        };

        Self::success(&GetExecutionResponse {
            execution_id: execution_process.id.to_string(),
            session_id: execution_process.session_id.to_string(),
            status: Self::execution_process_status_label(&execution_process.status).to_string(),
            is_finished,
            execution: execution_process_value,
            final_message: None,
        })
    }
}

impl McpServer {
    fn executor_config_payload_for_session(
        session: &Session,
    ) -> Result<ExecutorConfigPayload, super::ToolError> {
        Ok(ExecutorConfigPayload {
            executor: Self::normalize_executor_name(session.executor.as_deref())?,
            variant: None,
            model_id: None,
            agent_id: None,
            reasoning_id: None,
            permission_policy: None,
        })
    }

    fn session_summary(&self, session: Session) -> SessionSummary {
        let is_orchestrator_session = self.orchestrator_session_id() == Some(session.id);
        SessionSummary {
            id: session.id.to_string(),
            workspace_id: session.workspace_id.to_string(),
            name: session.name,
            executor: session.executor,
            created_at: session.created_at.to_rfc3339(),
            updated_at: session.updated_at.to_rfc3339(),
            is_orchestrator_session,
        }
    }

    fn serialize_execution_process(
        execution_process: &ExecutionProcess,
    ) -> Result<serde_json::Value, super::ToolError> {
        serde_json::to_value(execution_process).map_err(|error| {
            super::ToolError::new(
                "Failed to serialize execution process response",
                Some(error.to_string()),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use rmcp::model::CallToolResult;
    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };

    use super::*;
    use crate::task_server::McpContext;

    struct MockApi {
        server: McpServer,
        requests: Arc<Mutex<Vec<(String, Value)>>>,
        task: JoinHandle<()>,
    }

    impl Drop for MockApi {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl MockApi {
        async fn new(session: Value, status: u16, body: String) -> Self {
            super::super::tests::install_rustls_provider();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut server =
                McpServer::new_orchestrator(&format!("http://{}", listener.local_addr().unwrap()));
            server.client = reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = requests.clone();
            let task = tokio::spawn(async move {
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    let header_end = loop {
                        let mut chunk = [0; 1024];
                        let size = socket.read(&mut chunk).await.unwrap();
                        assert_ne!(size, 0);
                        bytes.extend_from_slice(&chunk[..size]);
                        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            break end + 4;
                        }
                    };
                    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    while bytes.len() < header_end + content_length {
                        let mut chunk = [0; 1024];
                        let size = socket.read(&mut chunk).await.unwrap();
                        assert_ne!(size, 0);
                        bytes.extend_from_slice(&chunk[..size]);
                    }
                    let request_line = headers.lines().next().unwrap().to_string();
                    let payload = if content_length == 0 {
                        Value::Null
                    } else {
                        serde_json::from_slice(&bytes[header_end..header_end + content_length])
                            .unwrap()
                    };
                    recorded
                        .lock()
                        .unwrap()
                        .push((request_line.clone(), payload));
                    let (status, body) = if request_line.starts_with("GET ") {
                        (200, json!({"success": true, "data": session}).to_string())
                    } else {
                        (status, body.clone())
                    };
                    socket.write_all(format!(
                        "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    ).as_bytes()).await.unwrap();
                }
            });
            Self {
                server,
                requests,
                task,
            }
        }

        async fn run(&self, session_id: Uuid, prompt: &str) -> CallToolResult {
            self.server
                .run_session_prompt(Parameters(RunCodingAgentInSessionRequest {
                    session_id,
                    prompt: prompt.into(),
                }))
                .await
                .unwrap()
        }

        fn assert_one_follow_up(&self, session_id: Uuid) {
            let requests = self.requests.lock().unwrap();
            assert_eq!(requests.len(), 2, "exactly one GET and one POST");
            assert_eq!(
                requests[0].0,
                format!("GET /api/sessions/{session_id} HTTP/1.1")
            );
            assert_eq!(
                requests[1].0,
                format!("POST /api/sessions/{session_id}/follow-up HTTP/1.1")
            );
            assert_eq!(
                requests[1].1,
                json!({
                    "prompt": "Continue",
                    "executor_config": {
                        "executor": "CLAUDE_CODE", "variant": null, "model_id": null,
                        "agent_id": null, "reasoning_id": null, "permission_policy": null
                    },
                    "retry_process_id": null, "force_when_dirty": null, "perform_git_reset": null
                })
            );
        }
    }

    fn session() -> Value {
        json!({
            "id": Uuid::new_v4(), "workspace_id": Uuid::new_v4(),
            "executor": "claude-code", "name": null, "agent_working_dir": null,
            "created_at": "2026-10-29T00:00:00Z", "updated_at": "2026-10-29T00:00:00Z"
        })
    }

    fn execution(session_id: Uuid) -> Value {
        json!({
            "id": Uuid::new_v4(), "session_id": session_id,
            "run_reason": "codingagent", "executor_action": {}, "status": "running",
            "exit_code": null, "dropped": false, "completed_at": null,
            "started_at": "2026-10-29T00:00:00Z", "created_at": "2026-10-29T00:00:00Z",
            "updated_at": "2026-10-29T00:00:00Z"
        })
    }

    fn result_json(result: &CallToolResult) -> Value {
        serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap()
    }

    #[test]
    fn follow_up_wire_requires_tag_and_object_receipts() {
        for (outcome, key) in [
            ("queued", "status"),
            ("routed_to_cli", "delivery"),
            ("conflict", "status"),
        ] {
            let receipt = json!({"status": "empty", "extra": {"nested": [true, 1]}});
            let mut wire = json!({"outcome": outcome});
            assert!(serde_json::from_value::<FollowUpOutcome>(wire.clone()).is_err());
            for invalid in [Value::Null, json!("queued"), json!([])] {
                wire[key] = invalid;
                assert!(serde_json::from_value::<FollowUpOutcome>(wire.clone()).is_err());
            }
            wire[key] = receipt.clone();
            let decoded = serde_json::from_value::<FollowUpOutcome>(wire).unwrap();
            let actual = match decoded {
                FollowUpOutcome::Queued { status } | FollowUpOutcome::Conflict { status } => status,
                FollowUpOutcome::RoutedToCli { delivery } => delivery,
                FollowUpOutcome::Started { .. } => panic!("unexpected started outcome"),
            };
            assert_eq!(actual, receipt);
        }
        for wire in [json!({}), json!({"outcome": "unknown"})] {
            assert!(serde_json::from_value::<FollowUpOutcome>(wire).is_err());
        }
    }

    #[tokio::test]
    async fn follow_up_maps_started_and_preserves_execution_fields() {
        let session = session();
        let session_id = serde_json::from_value(session["id"].clone()).unwrap();
        let execution = execution(session_id);
        let api = MockApi::new(
            session,
            200,
            json!({
                "success": true,
                "data": {"outcome": "started", "execution_process": execution}
            })
            .to_string(),
        )
        .await;
        let result = api.run(session_id, "  Continue  ").await;
        assert_ne!(result.is_error, Some(true));
        assert_eq!(
            result_json(&result),
            json!({
                "session_id": session_id, "outcome": "started",
                "execution_id": execution["id"], "execution": execution
            })
        );
        api.assert_one_follow_up(session_id);
    }

    #[tokio::test]
    async fn follow_up_maps_receipts_without_losing_extra_fields() {
        let receipt = json!({
            "status": "queued", "message": {
                "id": Uuid::new_v4(), "data": {"message": "Continue"},
                "failure_reason": null, "future_field": {"nested": [1, true]}
            }, "future_receipt_field": "preserved"
        });
        for (outcome, key, status) in [
            ("queued", "status", 200),
            ("routed_to_cli", "delivery", 200),
            ("conflict", "status", 409),
        ] {
            let session = session();
            let session_id = serde_json::from_value(session["id"].clone()).unwrap();
            let mut dispatch = json!({"outcome": outcome});
            dispatch[key] = receipt.clone();
            let body = if status == 409 {
                json!({"success": false, "error_data": dispatch, "data": null, "message": null})
            } else {
                json!({"success": true, "data": dispatch})
            };
            let api = MockApi::new(session, status, body.to_string()).await;
            let result = api.run(session_id, "Continue").await;
            let mut expected = json!({"session_id": session_id, "outcome": outcome});
            expected[key] = receipt.clone();
            if status == 409 {
                expected["success"] = json!(false);
                expected["error"] = json!("Session dispatch conflict");
                assert_eq!(result.is_error, Some(true));
            } else {
                assert_ne!(result.is_error, Some(true));
            }
            assert_eq!(result_json(&result), expected);
            api.assert_one_follow_up(session_id);
        }
    }

    #[tokio::test]
    async fn follow_up_preserves_message_only_conflict() {
        let session = session();
        let session_id = serde_json::from_value(session["id"].clone()).unwrap();
        let api = MockApi::new(
            session,
            409,
            json!({
                "success": false, "message": "Executor mismatch: session uses CODEX"
            })
            .to_string(),
        )
        .await;
        let result = api.run(session_id, "Continue").await;
        assert_eq!(result.is_error, Some(true));
        let body = result_json(&result);
        assert!(body["error"].as_str().unwrap().contains("409"));
        assert_eq!(body["details"], "Executor mismatch: session uses CODEX");
        assert!(body.get("outcome").is_none());
        api.assert_one_follow_up(session_id);
    }

    #[tokio::test]
    async fn follow_up_fails_closed_without_retry() {
        for (status, body) in [
            (200, "invalid JSON"),
            (200, r#"{"success":true}"#),
            (200, r#"{"success":true,"data":{"outcome":"unknown"}}"#),
            (200, r#"{"success":true,"data":{}}"#),
            (
                200,
                r#"{"success":true,"data":{"outcome":"started","execution_process":{}}}"#,
            ),
            (200, r#"{"success":true,"data":{"outcome":"queued"}}"#),
            (
                200,
                r#"{"success":true,"data":{"outcome":"queued","status":null}}"#,
            ),
            (
                200,
                r#"{"success":true,"data":{"outcome":"routed_to_cli","delivery":"bad"}}"#,
            ),
            (
                200,
                r#"{"success":true,"data":{"outcome":"conflict","status":{}}}"#,
            ),
            (
                200,
                r#"{"success":false,"data":{"outcome":"queued","status":{}}}"#,
            ),
            (
                409,
                r#"{"success":false,"error_data":{"outcome":"conflict"}}"#,
            ),
            (
                409,
                r#"{"success":false,"error_data":{"outcome":"unknown","status":{}}}"#,
            ),
            (
                409,
                r#"{"success":true,"error_data":{"outcome":"conflict","status":{}}}"#,
            ),
        ] {
            let session = session();
            let session_id = serde_json::from_value(session["id"].clone()).unwrap();
            let api = MockApi::new(session, status, body.into()).await;
            let result = api.run(session_id, "Continue").await;
            assert_eq!(result.is_error, Some(true), "{status}: {body}");
            api.assert_one_follow_up(session_id);
        }
    }

    #[tokio::test]
    async fn follow_up_guards_send_zero_posts() {
        for guard in ["prompt", "scope", "orchestrator", "executor"] {
            let mut session = session();
            if guard == "executor" {
                session["executor"] = json!("invalid-executor");
            }
            let session_id = serde_json::from_value(session["id"].clone()).unwrap();
            let workspace_id = serde_json::from_value(session["workspace_id"].clone()).unwrap();
            let mut api = MockApi::new(session, 200, "{}".into()).await;
            api.server.context = Some(McpContext {
                organization_id: None,
                project_id: None,
                issue_id: None,
                orchestrator_session_id: (guard == "orchestrator").then_some(session_id),
                workspace_id: if guard == "scope" {
                    Uuid::new_v4()
                } else {
                    workspace_id
                },
                workspace_branch: "main".into(),
                workspace_repos: vec![],
            });
            let result = api
                .run(
                    session_id,
                    if guard == "prompt" {
                        " \n "
                    } else {
                        "Continue"
                    },
                )
                .await;
            assert_eq!(result.is_error, Some(true), "{guard}");
            let requests = api.requests.lock().unwrap();
            assert!(
                requests.iter().all(|(line, _)| line.starts_with("GET ")),
                "{guard}"
            );
            assert_eq!(requests.len(), usize::from(guard != "prompt"), "{guard}");
        }
    }
}
