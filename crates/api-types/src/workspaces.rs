use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

#[derive(Debug, Deserialize, Serialize)]
pub struct DeleteWorkspaceRequest {
    pub local_workspace_id: Uuid,
}

#[derive(Debug, Serialize)]
pub struct CreateWorkspaceRequest {
    pub project_id: Uuid,
    pub local_workspace_id: Uuid,
    pub issue_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_changed: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_added: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_removed: Option<i32>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct UpdateWorkspaceRequest {
    pub local_workspace_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_changed: Option<Option<i32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_added: Option<Option<i32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_removed: Option<Option<i32>>,
}

/// Body of `POST /workspaces/{id}/cli/send`: composer text for the
/// workspace's live CLI agent pane.
#[derive(Debug, Deserialize, Serialize, TS)]
pub struct SendCliTextRequest {
    pub text: String,
}

/// `submitted: false` means the text reached the agent's input box but Enter
/// did not go through; the turn is not submitted.
#[derive(Debug, Deserialize, Serialize, TS)]
pub struct SendCliTextResponse {
    pub submitted: bool,
}

/// Result of `POST /workspaces/{id}/cli/launch`. `launched: false` means the
/// agent's pane was already up; a parked prompt was still delivered to it.
#[derive(Debug, Deserialize, Serialize, TS)]
pub struct LaunchCliAgentResponse {
    pub launched: bool,
}

/// Result of `GET /workspaces/{id}/cli/screen`: the visible text of the
/// workspace's CLI pane, or `None` when it has no pane.
#[derive(Debug, Deserialize, Serialize, TS)]
pub struct CliScreenResponse {
    pub screen: Option<String>,
}
