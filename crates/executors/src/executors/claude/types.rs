//! Type definitions for Claude Code control protocol
//!
//! Similar to: https://github.com/ZhangHanDong/claude-code-api-rs/blob/main/claude-code-sdk-rs/src/types.rs

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Top-level message types from CLI stdout
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CLIMessage {
    ControlRequest {
        request_id: String,
        request: ControlRequestType,
    },
    ControlResponse {
        response: ControlResponseType,
    },
    ControlCancelRequest {
        request_id: String,
    },
    Result(serde_json::Value),
    #[serde(untagged)]
    Other(serde_json::Value),
}

/// Control request from SDK to CLI (outgoing)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SDKControlRequest {
    #[serde(rename = "type")]
    message_type: String, // Always "control_request"
    pub request_id: String,
    pub request: SDKControlRequestType,
}

impl SDKControlRequest {
    pub fn new(request: SDKControlRequestType) -> Self {
        use uuid::Uuid;
        Self {
            message_type: "control_request".to_string(),
            request_id: Uuid::new_v4().to_string(),
            request,
        }
    }
}

/// Control response from SDK to CLI (outgoing)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponseMessage {
    #[serde(rename = "type")]
    message_type: String, // Always "control_response"
    pub response: ControlResponseType,
}

impl ControlResponseMessage {
    pub fn new(response: ControlResponseType) -> Self {
        Self {
            message_type: "control_response".to_string(),
            response,
        }
    }
}

/// Types of control requests
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum ControlRequestType {
    CanUseTool {
        tool_name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        permission_suggestions: Option<Vec<PermissionUpdate>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        blocked_paths: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
    },
    HookCallback {
        #[serde(rename = "callback_id")]
        callback_id: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
    },
    /// A control-request subtype we don't model yet (e.g. one added by a newer
    /// Claude CLI). Captured so the message still parses; the read loop replies
    /// with an error response so the CLI doesn't hang waiting for one.
    #[serde(other)]
    Unknown,
}

/// Result of permission check
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "behavior", rename_all = "camelCase")]
pub enum PermissionResult {
    Allow {
        #[serde(rename = "updatedInput")]
        updated_input: Value,
        #[serde(skip_serializing_if = "Option::is_none", rename = "updatedPermissions")]
        updated_permissions: Option<Vec<PermissionUpdate>>,
    },
    Deny {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        interrupt: Option<bool>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionUpdateType {
    SetMode,
    AddRules,
    RemoveRules,
    ClearRules,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionUpdateDestination {
    Session,
    UserSettings,
    ProjectSettings,
    LocalSettings,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRuleValue {
    pub tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_content: Option<String>,
}

/// Permission update operation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PermissionUpdate {
    #[serde(rename = "type")]
    pub update_type: PermissionUpdateType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<PermissionMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<PermissionUpdateDestination>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<PermissionRuleValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behavior: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directories: Option<Vec<String>>,
}

/// Control response from SDK to CLI
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum ControlResponseType {
    Success {
        request_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        response: Option<Value>,
    },
    Error {
        request_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    User { message: ClaudeUserMessage },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeUserMessage {
    role: String,
    content: String,
}

impl Message {
    pub fn new_user(content: String) -> Self {
        Self::User {
            message: ClaudeUserMessage {
                role: "user".to_string(),
                content,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum SDKControlRequestType {
    SetPermissionMode {
        mode: PermissionMode,
    },
    Initialize {
        #[serde(skip_serializing_if = "Option::is_none")]
        hooks: Option<Value>,
    },
    Interrupt {},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    BypassPermissions,
}

impl PermissionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_control_cancel_request() {
        let msg: CLIMessage =
            serde_json::from_str(r#"{"type":"control_cancel_request","request_id":"abc"}"#)
                .unwrap();
        assert!(matches!(
            msg,
            CLIMessage::ControlCancelRequest { request_id } if request_id == "abc"
        ));
    }

    #[test]
    fn unknown_message_type_falls_back_to_other() {
        // Unknown top-level message types must parse into `Other` so the read
        // loop ignores them rather than erroring.
        let msg: CLIMessage =
            serde_json::from_str(r#"{"type":"brand_new_event","foo":1}"#).unwrap();
        assert!(matches!(msg, CLIMessage::Other(_)));
    }

    #[test]
    fn parses_hook_callback_control_request() {
        let msg: CLIMessage = serde_json::from_str(
            r#"{"type":"control_request","request_id":"r1","request":{"subtype":"hook_callback","callback_id":"STOP_GIT_CHECK_CALLBACK_ID","input":{}}}"#,
        )
        .unwrap();
        match msg {
            CLIMessage::ControlRequest {
                request_id,
                request,
            } => {
                assert_eq!(request_id, "r1");
                assert!(matches!(request, ControlRequestType::HookCallback { .. }));
            }
            other => panic!("expected control_request, got {other:?}"),
        }
    }

    #[test]
    fn unknown_control_request_subtype_parses_to_unknown() {
        // A control_request whose subtype we don't model must still parse (into
        // ControlRequestType::Unknown) so the read loop can reply with an error
        // instead of the message failing to parse and the CLI hanging.
        let msg: CLIMessage = serde_json::from_str(
            r#"{"type":"control_request","request_id":"r2","request":{"subtype":"brand_new_subtype","foo":1}}"#,
        )
        .unwrap();
        match msg {
            CLIMessage::ControlRequest { request, .. } => {
                assert!(matches!(request, ControlRequestType::Unknown));
            }
            other => panic!("expected control_request, got {other:?}"),
        }
    }
}
