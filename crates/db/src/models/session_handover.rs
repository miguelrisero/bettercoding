use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct SessionHandover {
    pub operation_id: Uuid,
    pub workspace_id: Uuid,
    pub request_id: String,
    pub request_json: String,
    pub predecessor_session_id: Uuid,
    pub successor_session_id: Uuid,
    pub predecessor_binding_id: Uuid,
    pub binding_id: Uuid,
    pub successor_native_session_id: String,
    pub fallback_session_id: String,
    pub capability: String,
    pub phase: String,
    pub deadline_at: DateTime<Utc>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub failure: Option<String>,
    pub rollback_started: bool,
    pub launch_json: String,
}

impl SessionHandover {
    pub async fn find(pool: &SqlitePool, id: Uuid) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as("SELECT * FROM session_handovers WHERE operation_id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await
    }

    pub async fn find_request(
        pool: &SqlitePool,
        workspace_id: Uuid,
        request_id: &str,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as("SELECT * FROM session_handovers WHERE workspace_id = ? AND request_id = ?")
            .bind(workspace_id)
            .bind(request_id)
            .fetch_optional(pool)
            .await
    }

    pub async fn unfinished(pool: &SqlitePool) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as(
            "SELECT * FROM session_handovers WHERE phase IN ('starting', 'awaiting_confirmation')",
        )
        .fetch_all(pool)
        .await
    }

    /// Persist the recovery record and the successor binding before touching tmux.
    pub async fn insert(&self, pool: &SqlitePool) -> Result<(), sqlx::Error> {
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let released = sqlx::query("UPDATE cli_pane_bindings SET released_at = datetime('now', 'subsec') WHERE id = ? AND workspace_id = ? AND released_at IS NULL")
            .bind(self.predecessor_binding_id).bind(self.workspace_id).execute(&mut *tx).await?.rows_affected();
        if released != 1 {
            return Err(sqlx::Error::RowNotFound);
        }
        sqlx::query("INSERT INTO cli_pane_bindings (id, workspace_id, session_id, claude_session_id, bound_via) VALUES (?, ?, ?, ?, 'cli-fresh')")
            .bind(self.binding_id).bind(self.workspace_id).bind(self.successor_session_id).bind(&self.successor_native_session_id).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO session_handovers (operation_id, workspace_id, request_id, request_json, predecessor_session_id, successor_session_id, predecessor_binding_id, binding_id, successor_native_session_id, fallback_session_id, capability, phase, deadline_at, launch_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'starting', ?, ?)")
            .bind(self.operation_id).bind(self.workspace_id).bind(&self.request_id).bind(&self.request_json)
            .bind(self.predecessor_session_id).bind(self.successor_session_id).bind(self.predecessor_binding_id).bind(self.binding_id)
            .bind(&self.successor_native_session_id).bind(&self.fallback_session_id).bind(&self.capability).bind(self.deadline_at).bind(&self.launch_json)
            .execute(&mut *tx).await?;
        tx.commit().await
    }

    pub async fn set_phase(
        &self,
        pool: &SqlitePool,
        phase: &str,
        failure: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE session_handovers SET phase = ?, failure = ? WHERE operation_id = ? AND phase IN ('starting', 'awaiting_confirmation')")
            .bind(phase).bind(failure).bind(self.operation_id).execute(pool).await?;
        Ok(())
    }
}
