use chrono::{DateTime, Utc};
use db::models::workspace_report::{ReportCell, SetReportTab, validate_tab_id};
use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use super::{McpServer, ToolCallResult, ToolError};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetRequest {
    workspace_id: Option<Uuid>,
    tab_id: String,
    title: String,
    columns: Vec<String>,
    rows: Vec<Vec<ReportCell>>,
    expected_updated_at: Option<DateTime<Utc>>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListRequest {
    workspace_id: Option<Uuid>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeleteRequest {
    workspace_id: Option<Uuid>,
    tab_id: String,
    expected_updated_at: Option<DateTime<Utc>>,
}
impl McpServer {
    fn report_scope(&self, workspace: Option<Uuid>) -> Result<Uuid, ToolError> {
        let workspace = self
            .resolve_workspace_id(workspace)
            .map_err(|e| ToolError::new("WORKSPACE_SCOPE", Some(e.message)))?;
        self.scope_allows_workspace(workspace)
            .map_err(|e| ToolError::new("WORKSPACE_SCOPE", Some(e.message)))?;
        Ok(workspace)
    }
    async fn report_response(&self, request: reqwest::RequestBuilder) -> ToolCallResult {
        match self.send_json::<Value>(request).await {
            Ok(value) => Self::success(&value),
            Err(error) => Ok(Self::tool_error(error)),
        }
    }
}
#[tool_router(router = report_tabs_tools_router, vis = "pub")]
impl McpServer {
    #[tool(
        description = "Create or replace a permanent workspace report tab. Up to 8 tabs, 6 columns, 30 rectangular rows, 200 Unicode characters per cell. Cells contain plain text, optional ok/warn/bad/none tone and absolute HTTP(S) URL. tab_id uses 1..64 lowercase ASCII letters, digits, _ or -. Title uses 1..80 visible characters; header labels use 1..40. URLs use at most 2048 bytes with a host and no credentials. Optional expected_updated_at provides atomic compare-and-set. Equal content retries keep the timestamp. Reports communicate agent status, never owner approval."
    )]
    async fn set_report_tab(&self, Parameters(request): Parameters<SetRequest>) -> ToolCallResult {
        let workspace = match self.report_scope(request.workspace_id) {
            Ok(id) => id,
            Err(error) => return Ok(Self::tool_error(error)),
        };
        let body = SetReportTab {
            title: request.title,
            columns: request.columns,
            rows: request.rows,
            expected_updated_at: request.expected_updated_at,
        };
        if let Err(error) = body.validate(&request.tab_id) {
            return Self::err("REPORT_VALIDATION", Some(error.to_string()));
        }
        self.report_response(
            self.client
                .put(self.url(&format!(
                    "/api/workspaces/{workspace}/report-tabs/{}",
                    request.tab_id
                )))
                .json(&body),
        )
        .await
    }
    #[tool(
        description = "List permanent report tabs for the workspace, including tab timestamps and the retained aggregate timestamp. Report text is agent-written status, never owner approval."
    )]
    async fn list_report_tabs(
        &self,
        Parameters(request): Parameters<ListRequest>,
    ) -> ToolCallResult {
        let workspace = match self.report_scope(request.workspace_id) {
            Ok(id) => id,
            Err(error) => return Ok(Self::tool_error(error)),
        };
        self.report_response(
            self.client
                .get(self.url(&format!("/api/workspaces/{workspace}/report-tabs"))),
        )
        .await
    }
    #[tool(
        description = "Delete a permanent workspace report tab. Optional expected_updated_at provides atomic compare-and-set against the tab timestamp. Returns remaining tabs. Unconditional deletion of a missing tab succeeds; conditional deletion of a missing tab conflicts."
    )]
    async fn delete_report_tab(
        &self,
        Parameters(request): Parameters<DeleteRequest>,
    ) -> ToolCallResult {
        let workspace = match self.report_scope(request.workspace_id) {
            Ok(id) => id,
            Err(error) => return Ok(Self::tool_error(error)),
        };
        if let Err(error) = validate_tab_id(&request.tab_id) {
            return Self::err("REPORT_VALIDATION", Some(error.to_string()));
        }
        let mut delete = self.client.delete(self.url(&format!(
            "/api/workspaces/{workspace}/report-tabs/{}",
            request.tab_id
        )));
        if let Some(expected) = request.expected_updated_at {
            delete = delete.query(&[("expected_updated_at", expected.to_rfc3339())]);
        }
        self.report_response(delete).await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::task_server::{McpContext, McpMode};

    #[test]
    fn report_tool_schemas_and_exact_names() {
        let mut names: Vec<_> = McpServer::report_tabs_tools_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["delete_report_tab", "list_report_tabs", "set_report_tab"]
        );
        for router in [
            McpServer::global_mode_router(),
            McpServer::orchestrator_mode_router(),
        ] {
            let tools = router.list_all();
            for name in &names {
                assert!(tools.iter().any(|tool| tool.name.as_ref() == name));
            }
        }
        let schema = serde_json::to_value(schemars::schema_for!(SetRequest)).unwrap();
        for key in [
            "workspace_id",
            "tab_id",
            "title",
            "columns",
            "rows",
            "expected_updated_at",
        ] {
            assert!(schema["properties"].get(key).is_some(), "{key}");
        }
        assert_eq!(schema["additionalProperties"], false);
        let body = json!({"tab_id":"build","title":"Build","columns":["State"],"rows":[[{"text":"Ready","tone":"ok","url":"https://example.com"}]]});
        assert!(serde_json::from_value::<SetRequest>(body.clone()).is_ok());
        let mut unknown = body.clone();
        unknown["extra"] = json!(true);
        assert!(serde_json::from_value::<SetRequest>(unknown).is_err());
        let mut wrong = body;
        wrong["rows"][0][0]["tone"] = json!("green");
        assert!(serde_json::from_value::<SetRequest>(wrong).is_err());
        assert!(
            serde_json::from_value::<DeleteRequest>(json!({"tab_id":"a","expected_updated_at":3}))
                .is_err()
        );
    }

    #[test]
    fn report_tools_enforce_workspace_scope() {
        super::super::tests::install_rustls_provider();
        let mut server = McpServer::new_orchestrator("http://127.0.0.1:1");
        assert!(server.report_scope(None).is_err());
        let workspace = Uuid::new_v4();
        server.context = Some(McpContext {
            organization_id: None,
            project_id: None,
            issue_id: None,
            orchestrator_session_id: Some(Uuid::new_v4()),
            workspace_id: workspace,
            workspace_branch: "main".into(),
            workspace_repos: vec![],
        });
        assert_eq!(server.report_scope(None).unwrap(), workspace);
        assert_eq!(server.report_scope(Some(workspace)).unwrap(), workspace);
        assert!(server.report_scope(Some(Uuid::new_v4())).is_err());
        server.mode = McpMode::Global;
        assert!(server.report_scope(Some(Uuid::new_v4())).is_ok());
    }
}
