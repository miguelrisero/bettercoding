use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool, Type};
use ts_rs::TS;
use uuid::Uuid;

use super::cli_ingest_outbox::CliIngestOutbox;

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
    pub foreign_writer_seen_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct ClaudeSessionLinkMutation {
    pub link: ClaudeSessionLink,
    pub previous_session_id: Option<Uuid>,
    pub republished_outbox: u64,
}

impl ClaudeSessionLinkMutation {
    pub fn session_changed(&self) -> bool {
        self.previous_session_id != Some(self.link.session_id)
    }
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
                      created_at AS "created_at!: DateTime<Utc>",
                      foreign_writer_seen_at AS "foreign_writer_seen_at: DateTime<Utc>"
               FROM claude_session_links
               WHERE claude_session_id = $1"#,
            claude_session_id
        )
        .fetch_optional(pool)
        .await
    }

    pub async fn find_latest_for_session(
        pool: &SqlitePool,
        session_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as!(
            ClaudeSessionLink,
            r#"SELECT claude_session_id AS "claude_session_id!",
                      session_id AS "session_id!: Uuid",
                      workspace_id AS "workspace_id!: Uuid",
                      cwd AS "cwd!",
                      bound_via AS "bound_via!: ClaudeSessionBoundVia",
                      created_at AS "created_at!: DateTime<Utc>",
                      foreign_writer_seen_at AS "foreign_writer_seen_at: DateTime<Utc>"
               FROM claude_session_links
               WHERE session_id = $1
               ORDER BY created_at DESC
               LIMIT 1"#,
            session_id
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
    ) -> Result<Option<ClaudeSessionLinkMutation>, sqlx::Error> {
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
            return Self::upsert(
                pool,
                claude_session_id,
                owner.session_id,
                owner.workspace_id,
                cwd,
                ClaudeSessionBoundVia::Executor,
            )
            .await
            .map(Some);
        }

        Ok(Self::find(pool, claude_session_id)
            .await?
            .map(|link| ClaudeSessionLinkMutation {
                previous_session_id: Some(link.session_id),
                link,
                republished_outbox: 0,
            }))
    }

    pub async fn assign_manual(
        pool: &SqlitePool,
        claude_session_id: &str,
        session_id: Uuid,
        cwd: &str,
    ) -> Result<Option<ClaudeSessionLinkMutation>, sqlx::Error> {
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

        let mutation = Self::upsert(
            pool,
            claude_session_id,
            session_id,
            workspace_id,
            cwd,
            ClaudeSessionBoundVia::Manual,
        )
        .await?;
        Ok(Some(mutation))
    }

    pub async fn assign_cli(
        pool: &SqlitePool,
        claude_session_id: &str,
        session_id: Uuid,
        workspace_id: Uuid,
        cwd: &str,
        bound_via: ClaudeSessionBoundVia,
    ) -> Result<ClaudeSessionLinkMutation, sqlx::Error> {
        debug_assert!(matches!(
            bound_via,
            ClaudeSessionBoundVia::CliResume | ClaudeSessionBoundVia::CliFresh
        ));
        Self::upsert(
            pool,
            claude_session_id,
            session_id,
            workspace_id,
            cwd,
            bound_via,
        )
        .await
    }

    pub async fn latest_foreign_writer_seen_for_session(
        pool: &SqlitePool,
        session_id: Uuid,
    ) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
        sqlx::query_scalar!(
            r#"SELECT MAX(foreign_writer_seen_at) AS "seen_at: DateTime<Utc>"
               FROM claude_session_links
               WHERE session_id = $1"#,
            session_id
        )
        .fetch_one(pool)
        .await
    }

    async fn upsert(
        pool: &SqlitePool,
        claude_session_id: &str,
        session_id: Uuid,
        workspace_id: Uuid,
        cwd: &str,
        bound_via: ClaudeSessionBoundVia,
    ) -> Result<ClaudeSessionLinkMutation, sqlx::Error> {
        let mut tx = pool.begin().await?;
        let previous_session_id = sqlx::query_scalar!(
            r#"SELECT session_id AS "session_id!: Uuid"
               FROM claude_session_links
               WHERE claude_session_id = $1"#,
            claude_session_id
        )
        .fetch_optional(&mut *tx)
        .await?;

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
        .execute(&mut *tx)
        .await?;

        // Raw records can predate a binding or survive a session cascade.
        // Publish every missing record to the new owner in this same
        // transaction. INSERT OR IGNORE is intentional here: assigning or
        // resolving the same sid repeatedly is an idempotent replay.
        let next_seq = CliIngestOutbox::next_seq_in_transaction(&mut tx, session_id).await?;
        let republished_outbox = sqlx::query!(
            r#"INSERT OR IGNORE INTO cli_ingest_outbox
                   (session_id, seq, file_id, line_seq)
               SELECT $1,
                      $2 + ROW_NUMBER() OVER (
                          ORDER BY f.created_at, f.generation, r.line_seq
                      ) - 1,
                      r.file_id,
                      r.line_seq
               FROM cli_native_records r
               JOIN cli_native_files f ON f.id = r.file_id
               WHERE r.claude_session_id = $3
                 AND NOT EXISTS (
                     SELECT 1 FROM cli_ingest_outbox published
                     WHERE published.session_id = $1
                       AND published.file_id = r.file_id
                       AND published.line_seq = r.line_seq
                 )
               ORDER BY f.created_at, f.generation, r.line_seq"#,
            session_id,
            next_seq,
            claude_session_id
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();

        tx.commit().await?;
        let link = Self::find(pool, claude_session_id)
            .await?
            .expect("upserted Claude session link exists");
        Ok(ClaudeSessionLinkMutation {
            link,
            previous_session_id,
            republished_outbox,
        })
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
