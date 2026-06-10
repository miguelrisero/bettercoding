use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

/// Live state of a workspace's CLI-mode tmux claude session.
///
/// Maintained by the local deployment's tmux poller (`CliActivityMonitor`):
/// `running` while the pane is producing output, `attention` once a run went
/// quiet while no client was attached (claude finished while the user was on
/// another workspace), `idle` otherwise. Rows are upserted only on state
/// transitions so the SQLite update hook broadcasts a workspace patch exactly
/// when the sidebar bucket should move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliActivityState {
    Idle,
    Running,
    Attention,
}

impl CliActivityState {
    pub fn as_str(&self) -> &'static str {
        match self {
            CliActivityState::Idle => "idle",
            CliActivityState::Running => "running",
            CliActivityState::Attention => "attention",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "running" => CliActivityState::Running,
            "attention" => CliActivityState::Attention,
            _ => CliActivityState::Idle,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceCliActivity {
    pub workspace_id: Uuid,
    pub state: CliActivityState,
    pub updated_at: DateTime<Utc>,
}

impl WorkspaceCliActivity {
    /// Set a workspace's CLI activity state. No-op write avoidance is the
    /// caller's job (the monitor only calls this on transitions).
    pub async fn upsert(
        pool: &SqlitePool,
        workspace_id: Uuid,
        state: CliActivityState,
    ) -> Result<(), sqlx::Error> {
        let state = state.as_str();
        sqlx::query!(
            r#"INSERT INTO workspace_cli_activity (workspace_id, state, updated_at)
               VALUES ($1, $2, datetime('now', 'subsec'))
               ON CONFLICT(workspace_id) DO UPDATE SET
                 state = excluded.state,
                 updated_at = excluded.updated_at"#,
            workspace_id,
            state
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Current states for all workspaces that have a row.
    pub async fn find_all(pool: &SqlitePool) -> Result<Vec<Self>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT
                 workspace_id as "workspace_id!: Uuid",
                 state,
                 updated_at as "updated_at!: DateTime<Utc>"
               FROM workspace_cli_activity"#
        )
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| Self {
                workspace_id: r.workspace_id,
                state: CliActivityState::parse(&r.state),
                updated_at: r.updated_at,
            })
            .collect())
    }

    /// Workspaces whose CLI session finished while unattended (for the
    /// "Needs Attention" sidebar bucket), filtered by archived status.
    pub async fn find_workspaces_needing_attention(
        pool: &SqlitePool,
        archived: bool,
    ) -> Result<std::collections::HashSet<Uuid>, sqlx::Error> {
        let result: Vec<Uuid> = sqlx::query_scalar!(
            r#"SELECT ca.workspace_id as "workspace_id!: Uuid"
               FROM workspace_cli_activity ca
               JOIN workspaces w ON ca.workspace_id = w.id
               WHERE ca.state = 'attention' AND w.archived = $1"#,
            archived
        )
        .fetch_all(pool)
        .await?;

        Ok(result.into_iter().collect())
    }

    /// Row lookup for the SQLite update hook (hooks only get a rowid).
    pub async fn find_by_rowid(pool: &SqlitePool, rowid: i64) -> Result<Option<Self>, sqlx::Error> {
        let row = sqlx::query!(
            r#"SELECT
                 workspace_id as "workspace_id!: Uuid",
                 state,
                 updated_at as "updated_at!: DateTime<Utc>"
               FROM workspace_cli_activity
               WHERE rowid = $1"#,
            rowid
        )
        .fetch_optional(pool)
        .await?;

        Ok(row.map(|r| Self {
            workspace_id: r.workspace_id,
            state: CliActivityState::parse(&r.state),
            updated_at: r.updated_at,
        }))
    }
}
