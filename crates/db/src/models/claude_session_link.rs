use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool, Type};
use ts_rs::TS;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type, TS)]
#[sqlx(type_name = "TEXT", rename_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum ClaudeSessionBoundVia {
    Executor,
    CliResume,
    CliFresh,
    Manual,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize, TS)]
pub struct ClaudeSessionLink {
    pub claude_session_id: String,
    pub session_id: Uuid,
    pub workspace_id: Uuid,
    pub cwd: String,
    pub bound_via: ClaudeSessionBoundVia,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
struct ExecutorSessionOwner {
    session_id: Uuid,
    workspace_id: Uuid,
}

impl ClaudeSessionLink {
    pub async fn find(
        pool: &SqlitePool,
        claude_session_id: &str,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as!(
            ClaudeSessionLink,
            r#"SELECT claude_session_id AS "claude_session_id!",
                      session_id AS "session_id!: Uuid",
                      workspace_id AS "workspace_id!: Uuid",
                      cwd AS "cwd!",
                      bound_via AS "bound_via!: ClaudeSessionBoundVia",
                      created_at AS "created_at!: DateTime<Utc>"
               FROM claude_session_links
               WHERE claude_session_id = $1"#,
            claude_session_id
        )
        .fetch_optional(pool)
        .await
    }

    /// Executor evidence has precedence over a stale/manual association.
    /// When no executor has reported this sid, an existing link is returned;
    /// otherwise the caller must quarantine the file.
    pub async fn resolve_or_bind_executor(
        pool: &SqlitePool,
        claude_session_id: &str,
        cwd: &str,
    ) -> Result<Option<Self>, sqlx::Error> {
        let owner = sqlx::query_as!(
            ExecutorSessionOwner,
            r#"SELECT ep.session_id AS "session_id!: Uuid",
                      s.workspace_id AS "workspace_id!: Uuid"
               FROM coding_agent_turns cat
               JOIN execution_processes ep ON ep.id = cat.execution_process_id
               JOIN sessions s ON s.id = ep.session_id
               WHERE cat.agent_session_id = $1 AND ep.dropped = FALSE
               ORDER BY ep.created_at DESC
               LIMIT 1"#,
            claude_session_id
        )
        .fetch_optional(pool)
        .await?;

        if let Some(owner) = owner {
            Self::upsert(
                pool,
                claude_session_id,
                owner.session_id,
                owner.workspace_id,
                cwd,
                ClaudeSessionBoundVia::Executor,
            )
            .await?;
        }

        Self::find(pool, claude_session_id).await
    }

    pub async fn assign_manual(
        pool: &SqlitePool,
        claude_session_id: &str,
        session_id: Uuid,
        cwd: &str,
    ) -> Result<Option<Self>, sqlx::Error> {
        let workspace_id = sqlx::query_scalar!(
            r#"SELECT workspace_id AS "workspace_id!: Uuid"
               FROM sessions WHERE id = $1"#,
            session_id
        )
        .fetch_optional(pool)
        .await?;

        let Some(workspace_id) = workspace_id else {
            return Ok(None);
        };

        Self::upsert(
            pool,
            claude_session_id,
            session_id,
            workspace_id,
            cwd,
            ClaudeSessionBoundVia::Manual,
        )
        .await?;
        Self::find(pool, claude_session_id).await
    }

    async fn upsert(
        pool: &SqlitePool,
        claude_session_id: &str,
        session_id: Uuid,
        workspace_id: Uuid,
        cwd: &str,
        bound_via: ClaudeSessionBoundVia,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"INSERT INTO claude_session_links
                   (claude_session_id, session_id, workspace_id, cwd, bound_via)
               VALUES ($1, $2, $3, $4, $5)
               ON CONFLICT(claude_session_id) DO UPDATE SET
                   session_id = excluded.session_id,
                   workspace_id = excluded.workspace_id,
                   cwd = excluded.cwd,
                   bound_via = excluded.bound_via"#,
            claude_session_id,
            session_id,
            workspace_id,
            cwd,
            bound_via
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn known_session_ids_for_workspace(
        pool: &SqlitePool,
        workspace_id: Uuid,
    ) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar!(
            r#"SELECT claude_session_id AS "claude_session_id!"
               FROM claude_session_links
               WHERE workspace_id = $1
               UNION
               SELECT cat.agent_session_id AS "claude_session_id!"
               FROM coding_agent_turns cat
               JOIN execution_processes ep ON ep.id = cat.execution_process_id
               JOIN sessions s ON s.id = ep.session_id
               WHERE s.workspace_id = $1 AND cat.agent_session_id IS NOT NULL"#,
            workspace_id
        )
        .fetch_all(pool)
        .await
    }
}
