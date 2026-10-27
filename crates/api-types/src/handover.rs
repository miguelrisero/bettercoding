use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
pub struct AdoptWorkspaceSessionRequest {
    pub workspace_id: Uuid,
    pub expected_binding_id: Uuid,
    pub expected_session_id: Uuid,
    pub expected_native_session_id: Option<String>,
    pub session_id: Uuid,
    pub native_session_id: String,
    pub expected_pane_id: String,
    pub expected_pid: u32,
    pub expected_start_time: u64,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema, TS)]
pub struct AdoptWorkspaceSessionResponse {
    pub workspace_id: Uuid,
    pub binding_id: Uuid,
    pub session_id: Uuid,
    pub native_session_id: String,
    pub pane_id: String,
    pub pid: u32,
    pub start_time: u64,
}

pub fn default_confirm_deadline_secs() -> u32 {
    900
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS, PartialEq, Eq)]
pub struct HandoverWorkspaceSessionRequest {
    pub workspace_id: Uuid,
    pub expected_binding_id: Uuid,
    pub successor_session_id: Uuid,
    pub initial_prompt: String,
    pub fallback_session_id: String,
    #[serde(default = "default_confirm_deadline_secs")]
    pub confirm_deadline_secs: u32,
    pub request_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, TS, sqlx::Type, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
pub enum HandoverPhase {
    Starting,
    AwaitingConfirmation,
    Confirmed,
    RolledBack,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
pub struct HandoverResponse {
    pub operation_id: Uuid,
    pub workspace_id: Uuid,
    pub predecessor_session_id: Uuid,
    pub successor_session_id: Uuid,
    pub successor_native_session_id: String,
    pub phase: HandoverPhase,
    pub deadline_at: DateTime<Utc>,
    pub binding_id: Uuid,
    pub fallback_session_id: String,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub failure: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema, TS)]
pub struct ConfirmHandoverResponse {
    pub operation_id: Uuid,
    pub phase: HandoverPhase,
    pub confirmed_at: DateTime<Utc>,
    pub successor_session_id: Uuid,
    pub successor_native_session_id: String,
    pub binding_id: Uuid,
}
