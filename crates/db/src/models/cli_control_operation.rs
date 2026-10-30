//! Inert control custody. Native dispatch, confirmation provisioning and hook registration are unavailable.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, SqliteConnection, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchIdentity {
    pub launch_id: Uuid,
    pub native_session_id: String,
    pub socket: String,
    pub tmux_session: String,
    pub window: String,
    pub pane: String,
    pub pid: u32,
    pub process_start: String,
    pub executor: String,
    pub cwd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlRequest {
    pub request_id: String,
    pub workspace_id: Uuid,
    pub session_id: Uuid,
    pub binding_id: Uuid,
    pub launch: LaunchIdentity,
    pub expected_model: String,
    pub target_model: String,
    pub requester: String,
    pub confirmation_principal: String,
    /// Digest of a separate per-launch hook-report capability; never a request or confirmation credential.
    pub hook_capability_digest: String,
    pub deadline_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelEvent {
    Pre,
    Post,
}

/// Untrusted input. Deserialization alone grants zero receipt authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawReceipt {
    pub receipt_id: String,
    pub operation_id: Uuid,
    pub request_digest: String,
    pub binding_id: Uuid,
    pub launch: LaunchIdentity,
    pub event: ModelEvent,
    pub source: String,
    pub from_model: String,
    pub to_model: String,
}

/// Constructed only after capability and independently observed process identity validation.
#[derive(Debug)]
pub struct AuthenticatedReceipt {
    raw: RawReceipt,
    observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct CliControlOperation {
    pub operation_id: Uuid,
    pub workspace_id: Uuid,
    pub request_id: String,
    pub request_digest: String,
    request_json: String,
    pub binding_id: Uuid,
    pub phase: String,
    pub pre_receipt_id: Option<String>,
    pub confirmation_receipt_id: Option<String>,
    pub confirmation_principal: Option<String>,
    pub post_receipt_id: Option<String>,
}

/// Shared cooperating-writer mutex, including service-owned destructive lifecycle operations.
/// ponytail: process-local exclusion; cross-process custody requires native acceptance.
pub fn cli_writer_lock(workspace_id: Uuid) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    locks.entry(workspace_id).or_default().clone()
}

pub fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn invalid(message: &str) -> sqlx::Error {
    sqlx::Error::Protocol(message.into())
}
fn json_error(error: serde_json::Error) -> sqlx::Error {
    sqlx::Error::Decode(Box::new(error))
}

impl CliControlOperation {
    pub fn request(&self) -> Result<ControlRequest, sqlx::Error> {
        serde_json::from_str(&self.request_json).map_err(json_error)
    }

    pub async fn find(pool: &SqlitePool, id: Uuid) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as("SELECT * FROM cli_control_operations WHERE operation_id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await
    }

    pub async fn ensure_unfenced(pool: &SqlitePool, workspace_id: Uuid) -> Result<(), sqlx::Error> {
        let mut connection = pool.acquire().await?;
        Self::ensure_unfenced_on(&mut connection, workspace_id).await
    }

    pub async fn ensure_unfenced_on(
        connection: &mut SqliteConnection,
        workspace_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        let busy: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM cli_control_operations WHERE workspace_id = ? AND phase NOT IN ('applied', 'cancelled'))").bind(workspace_id).fetch_one(connection).await?;
        if busy {
            return Err(invalid(
                "CLI control busy; reconciliation or safe cancellation required",
            ));
        }
        Ok(())
    }

    /// The local-deployment boundary holds writer admission through this transaction.
    /// This model method performs no native action and grants no dispatch authority.
    pub async fn reserve(pool: &SqlitePool, request: &ControlRequest) -> Result<Self, sqlx::Error> {
        let request_json = serde_json::to_string(request).map_err(json_error)?;
        let request_digest = digest(&request_json);
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing: Option<Self> = sqlx::query_as(
            "SELECT * FROM cli_control_operations WHERE workspace_id = ? AND request_id = ?",
        )
        .bind(request.workspace_id)
        .bind(&request.request_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(existing) = existing {
            return if existing.request_digest == request_digest
                && existing.request_json == request_json
            {
                Ok(existing)
            } else {
                Err(invalid(
                    "request_id already names a different control request",
                ))
            };
        }
        let launch = &request.launch;
        if request.request_id.is_empty()
            || request.requester.is_empty()
            || request.confirmation_principal.is_empty()
            || request.expected_model.is_empty()
            || request.target_model.is_empty()
            || request.expected_model == request.target_model
            || request.deadline_at <= Utc::now()
            || launch.launch_id.is_nil()
            || launch.pid == 0
            || launch.process_start.is_empty()
            || launch.executor != "CLAUDE_CODE"
            || !std::path::Path::new(&launch.cwd).is_absolute()
            || Uuid::parse_str(&launch.native_session_id).is_err()
            || launch.socket.is_empty()
            || launch.tmux_session.is_empty()
            || !launch.window.starts_with('@')
            || !launch.pane.starts_with('%')
            || request.hook_capability_digest.len() != 64
            || !request
                .hook_capability_digest
                .bytes()
                .all(|c| c.is_ascii_hexdigit())
        {
            return Err(invalid("Incomplete control request identity or authority"));
        }
        Self::ensure_unfenced_on(&mut tx, request.workspace_id).await?;
        let executor_active: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM execution_processes ep JOIN sessions s ON s.id = ep.session_id WHERE s.workspace_id = ? AND ep.run_reason = 'codingagent' AND ep.status = 'running' AND ep.dropped = FALSE)")
            .bind(request.workspace_id).fetch_one(&mut *tx).await?;
        if executor_active {
            return Err(invalid("A headless executor owns this workspace"));
        }
        let eligible: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM cli_pane_bindings b JOIN sessions s ON s.id = b.session_id WHERE b.id = ? AND b.workspace_id = ? AND b.session_id = ? AND b.claude_session_id = ? AND b.released_at IS NULL AND s.workspace_id = b.workspace_id AND s.executor = 'CLAUDE_CODE' AND NOT EXISTS (SELECT 1 FROM session_handovers h WHERE h.workspace_id = b.workspace_id AND h.phase IN ('starting', 'awaiting_confirmation')) AND NOT EXISTS (SELECT 1 FROM workspace_spawn_reservations r WHERE r.workspace_id = b.workspace_id AND julianday(r.expires_at) > julianday('now')))")
            .bind(request.binding_id).bind(request.workspace_id).bind(request.session_id).bind(&launch.native_session_id).fetch_one(&mut *tx).await?;
        if !eligible {
            return Err(invalid(
                "Control binding changed or lifecycle custody is busy",
            ));
        }
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO cli_control_operations (operation_id, workspace_id, request_id, request_digest, request_json, binding_id, phase) VALUES (?, ?, ?, ?, ?, ?, 'reserved')")
            .bind(id).bind(request.workspace_id).bind(&request.request_id).bind(request_digest).bind(request_json).bind(request.binding_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Self::find(pool, id).await?.ok_or(sqlx::Error::RowNotFound)
    }

    /// `observed_launch` must come from independent launch/process verification, never hook JSON.
    /// No production caller provisions these credentials or invokes this inert reducer.
    pub fn authenticate(
        &self,
        raw: RawReceipt,
        capability: &str,
        observed_launch: &LaunchIdentity,
    ) -> Result<AuthenticatedReceipt, sqlx::Error> {
        let request = self.request()?;
        let actual_digest = digest(capability);
        let expected = request.hook_capability_digest.as_bytes();
        let difference = actual_digest
            .as_bytes()
            .iter()
            .zip(expected)
            .fold(0u8, |diff, (a, b)| diff | (a ^ b));
        if capability.is_empty()
            || expected.len() != actual_digest.len()
            || difference != 0
            || raw.receipt_id.is_empty()
            || raw.operation_id != self.operation_id
            || raw.request_digest != self.request_digest
            || raw.binding_id != request.binding_id
            || raw.launch != request.launch
            || observed_launch != &request.launch
            || raw.source != "command"
            || raw.from_model != request.expected_model
            || raw.to_model != request.target_model
        {
            return Err(invalid("Unauthenticated or uncorrelated native receipt"));
        }
        Ok(AuthenticatedReceipt {
            raw,
            observed_at: Utc::now(),
        })
    }

    pub async fn record_receipt(
        pool: &SqlitePool,
        receipt: AuthenticatedReceipt,
    ) -> Result<Self, sqlx::Error> {
        let raw = receipt.raw;
        let json = serde_json::to_string(&raw).map_err(json_error)?;
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let op: Self =
            sqlx::query_as("SELECT * FROM cli_control_operations WHERE operation_id = ?")
                .bind(raw.operation_id)
                .fetch_one(&mut *tx)
                .await?;
        let duplicate: Option<String> = sqlx::query_scalar(
            "SELECT receipt_json FROM cli_control_receipts WHERE receipt_id = ?",
        )
        .bind(&raw.receipt_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(duplicate) = duplicate {
            return if duplicate == json {
                Ok(op)
            } else {
                Err(invalid(
                    "Receipt identifier reuse with conflicting evidence",
                ))
            };
        }
        let request = op.request()?;
        if raw.request_digest != op.request_digest
            || raw.binding_id != op.binding_id
            || raw.launch != request.launch
            || raw.source != "command"
            || raw.from_model != request.expected_model
            || raw.to_model != request.target_model
        {
            return Err(invalid("Receipt does not match the durable operation"));
        }
        let binding_matches: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM cli_pane_bindings WHERE id = ? AND workspace_id = ? AND session_id = ? AND claude_session_id = ? AND released_at IS NULL)").bind(request.binding_id).bind(request.workspace_id).bind(request.session_id).bind(&request.launch.native_session_id).fetch_one(&mut *tx).await?;
        let fresh = receipt.observed_at < request.deadline_at
            && Utc::now() < request.deadline_at
            && binding_matches;
        let phase = match raw.event {
            ModelEvent::Pre
                if fresh && op.phase == "command_sent" && op.pre_receipt_id.is_none() =>
            {
                "awaiting_confirmation"
            }
            ModelEvent::Post
                if fresh
                    && op.phase == "awaiting_applied"
                    && op.pre_receipt_id.is_some()
                    && op.post_receipt_id.is_none()
                    && op.confirmation_receipt_id.is_some()
                    && op.confirmation_principal.as_deref()
                        == Some(request.confirmation_principal.as_str()) =>
            {
                "applied"
            }
            _ => "reconcile_required",
        };
        sqlx::query("INSERT INTO cli_control_receipts (receipt_id, operation_id, receipt_json) VALUES (?, ?, ?)").bind(&raw.receipt_id).bind(op.operation_id).bind(json).execute(&mut *tx).await?;
        sqlx::query("UPDATE cli_control_operations SET phase = ?, pre_receipt_id = CASE WHEN ? = 'pre' AND pre_receipt_id IS NULL THEN ? ELSE pre_receipt_id END, post_receipt_id = CASE WHEN ? = 'post' AND post_receipt_id IS NULL THEN ? ELSE post_receipt_id END WHERE operation_id = ?")
            .bind(phase).bind(if raw.event == ModelEvent::Pre { "pre" } else { "post" }).bind(&raw.receipt_id).bind(if raw.event == ModelEvent::Post { "post" } else { "pre" }).bind(&raw.receipt_id).bind(op.operation_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Self::find(pool, op.operation_id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
    }

    /// Evidence-backed safe cancellation: durable reserved state and zero native receipts.
    /// Ambiguous, dispatched, or restarted attempts retain custody for receipt-backed reconciliation.
    pub async fn cancel_undispatched(pool: &SqlitePool, id: Uuid) -> Result<(), sqlx::Error> {
        let changed = sqlx::query("UPDATE cli_control_operations SET phase = 'cancelled' WHERE operation_id = ? AND phase = 'reserved' AND NOT EXISTS (SELECT 1 FROM cli_control_receipts r WHERE r.operation_id = cli_control_operations.operation_id)").bind(id).execute(pool).await?.rows_affected();
        if changed != 1 {
            return Err(invalid(
                "Control outcome requires receipt-backed reconciliation",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "cli_control_operation_tests.rs"]
mod tests;
