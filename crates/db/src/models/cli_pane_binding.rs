use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool, Type};
use ts_rs::TS;
use uuid::Uuid;

use super::{
    claude_session_link::{ClaudeSessionLink, ClaudeSessionLinkMutation},
    cli_ingest_outbox::CliIngestOutbox,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type, TS)]
#[sqlx(type_name = "TEXT", rename_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum CliPaneBoundVia {
    CliResume,
    CliFresh,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize, TS)]
pub struct CliPaneBinding {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub session_id: Uuid,
    pub claude_session_id: Option<String>,
    pub bound_via: CliPaneBoundVia,
    pub created_at: DateTime<Utc>,
    pub released_at: Option<DateTime<Utc>>,
}

impl CliPaneBinding {
    const SELECT_FIELDS: &'static str = r#"
        id, workspace_id, session_id, claude_session_id, bound_via,
        created_at, released_at
    "#;

    pub async fn record_launch(
        pool: &SqlitePool,
        workspace_id: Uuid,
        session_id: Uuid,
        claude_session_id: Option<&str>,
        bound_via: CliPaneBoundVia,
    ) -> Result<Self, sqlx::Error> {
        let mut tx = pool.begin().await?;
        sqlx::query!(
            r#"UPDATE cli_pane_bindings SET released_at = datetime('now', 'subsec')
               WHERE workspace_id = $1 AND released_at IS NULL"#,
            workspace_id
        )
        .execute(&mut *tx)
        .await?;
        let id = Uuid::new_v4();
        sqlx::query!(
            r#"INSERT INTO cli_pane_bindings
                   (id, workspace_id, session_id, claude_session_id, bound_via)
               VALUES ($1, $2, $3, $4, $5)"#,
            id,
            workspace_id,
            session_id,
            claude_session_id,
            bound_via
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Self::find_by_id(pool, id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
    }

    pub async fn find_by_id(pool: &SqlitePool, id: Uuid) -> Result<Option<Self>, sqlx::Error> {
        let sql = format!(
            "SELECT {} FROM cli_pane_bindings WHERE id = ?",
            Self::SELECT_FIELDS
        );
        sqlx::query_as::<_, Self>(&sql)
            .bind(id)
            .fetch_optional(pool)
            .await
    }

    pub async fn find_active_for_workspace(
        pool: &SqlitePool,
        workspace_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        let sql = format!(
            "SELECT {} FROM cli_pane_bindings \
             WHERE workspace_id = ? AND released_at IS NULL LIMIT 1",
            Self::SELECT_FIELDS
        );
        sqlx::query_as::<_, Self>(&sql)
            .bind(workspace_id)
            .fetch_optional(pool)
            .await
    }

    pub async fn find_active_for_session(
        pool: &SqlitePool,
        session_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        let sql = format!(
            "SELECT {} FROM cli_pane_bindings \
             WHERE session_id = ? AND released_at IS NULL LIMIT 1",
            Self::SELECT_FIELDS
        );
        sqlx::query_as::<_, Self>(&sql)
            .bind(session_id)
            .fetch_optional(pool)
            .await
    }

    pub async fn list_active(pool: &SqlitePool) -> Result<Vec<Self>, sqlx::Error> {
        let sql = format!(
            "SELECT {} FROM cli_pane_bindings \
             WHERE released_at IS NULL ORDER BY created_at ASC",
            Self::SELECT_FIELDS
        );
        sqlx::query_as::<_, Self>(&sql).fetch_all(pool).await
    }

    /// Every native session id a CLI pane in `workspace_id` was ever bound
    /// to, released bindings included, and every session a pane of the
    /// workspace ran before it switched to another inside the TUI.
    pub async fn bound_session_ids_for_workspace(
        pool: &SqlitePool,
        workspace_id: Uuid,
    ) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT claude_session_id FROM cli_pane_bindings \
             WHERE workspace_id = ?1 AND claude_session_id IS NOT NULL \
             UNION \
             SELECT claude_session_id FROM claude_session_links \
             WHERE workspace_id = ?1 AND bound_via IN ('cli-resume', 'cli-fresh')",
        )
        .bind(workspace_id)
        .fetch_all(pool)
        .await
    }

    /// The Codex thread the latest CLI pane of `session_id` ran, unless a
    /// coding-agent run of the session started after that pane launched.
    ///
    /// A pane that switches threads inside the Codex TUI (`/new`, `/resume`,
    /// `/fork`) records the thread it runs now, so this names the
    /// conversation the user last saw in the pane, which can be newer than
    /// the executor's latest thread. Only Codex thread ids (UUIDv7) are
    /// returned; a Claude pane keeps the executor-first resume order.
    pub async fn latest_codex_pane_thread(
        pool: &SqlitePool,
        session_id: Uuid,
    ) -> Result<Option<String>, sqlx::Error> {
        let sid: Option<String> = sqlx::query_scalar(
            "SELECT b.claude_session_id FROM cli_pane_bindings b \
             WHERE b.session_id = ?1 AND b.claude_session_id IS NOT NULL \
               AND NOT EXISTS ( \
                   SELECT 1 FROM execution_processes ep \
                   WHERE ep.session_id = ?1 AND ep.run_reason = 'codingagent' \
                     AND ep.dropped = FALSE \
                     AND julianday(ep.created_at) > julianday(b.created_at)) \
             ORDER BY b.created_at DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(pool)
        .await?;
        Ok(sid.filter(|sid| Uuid::parse_str(sid).is_ok_and(|id| id.get_version_num() == 7)))
    }

    /// Link the native session a pane runs to the pane's app session and
    /// record it on the pane, in one transaction, publishing the session's
    /// already imported records.
    ///
    /// `current` is the session the pane is recorded to run: `None` for a
    /// fresh `cli-fresh` pane that has not been bound yet, or the session it
    /// ran before switching inside the TUI. Nothing is written, and `None` is
    /// returned, when another session owns `claude_session_id` (by link or by
    /// a coding-agent run), or the binding is no longer active and still
    /// recorded as running `current`.
    pub async fn assign_discovered_session(
        pool: &SqlitePool,
        binding_id: Uuid,
        current: Option<&str>,
        claude_session_id: &str,
        session_id: Uuid,
        workspace_id: Uuid,
        cwd: &str,
    ) -> Result<Option<ClaudeSessionLinkMutation>, sqlx::Error> {
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let previous_session_id: Option<Uuid> = sqlx::query_scalar(
            "SELECT session_id FROM claude_session_links WHERE claude_session_id = ?",
        )
        .bind(claude_session_id)
        .fetch_optional(&mut *tx)
        .await?;
        if previous_session_id.is_some_and(|owner| owner != session_id) {
            return Ok(None);
        }
        let run_by_another_session: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM coding_agent_turns cat \
                 JOIN execution_processes ep ON ep.id = cat.execution_process_id \
                 WHERE cat.agent_session_id = ? AND ep.session_id != ?)",
        )
        .bind(claude_session_id)
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;
        if run_by_another_session {
            return Ok(None);
        }
        let bound = sqlx::query(
            "UPDATE cli_pane_bindings SET claude_session_id = ?1 \
             WHERE id = ?2 AND released_at IS NULL \
               AND claude_session_id IS ?3 \
               AND (?3 IS NOT NULL OR bound_via = 'cli-fresh')",
        )
        .bind(claude_session_id)
        .bind(binding_id)
        .bind(current)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if bound == 0 {
            return Ok(None);
        }
        sqlx::query(
            "INSERT INTO claude_session_links \
                 (claude_session_id, session_id, workspace_id, cwd, bound_via) \
             VALUES (?, ?, ?, ?, 'cli-fresh') \
             ON CONFLICT(claude_session_id) DO UPDATE SET \
                 session_id = excluded.session_id, \
                 workspace_id = excluded.workspace_id, \
                 cwd = excluded.cwd, \
                 bound_via = excluded.bound_via",
        )
        .bind(claude_session_id)
        .bind(session_id)
        .bind(workspace_id)
        .bind(cwd)
        .execute(&mut *tx)
        .await?;
        // Records imported before the link (none for a thread only just
        // bound, unless an earlier pass stopped here) join the feed now, the
        // same publication `ClaudeSessionLink::assign_cli` performs.
        let next_seq = CliIngestOutbox::next_seq_in_transaction(&mut tx, session_id).await?;
        let republished_outbox = sqlx::query(
            "INSERT OR IGNORE INTO cli_ingest_outbox (session_id, seq, file_id, line_seq) \
             SELECT ?1, ?2 + ROW_NUMBER() OVER ( \
                        ORDER BY f.created_at, f.generation, r.line_seq) - 1, \
                    r.file_id, r.line_seq \
             FROM cli_native_records r \
             JOIN cli_native_files f ON f.id = r.file_id \
             WHERE r.claude_session_id = ?3 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM cli_ingest_outbox published \
                   WHERE published.session_id = ?1 \
                     AND published.file_id = r.file_id \
                     AND published.line_seq = r.line_seq) \
             ORDER BY f.created_at, f.generation, r.line_seq",
        )
        .bind(session_id)
        .bind(next_seq)
        .bind(claude_session_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        let link = ClaudeSessionLink::find(pool, claude_session_id)
            .await?
            .expect("assigned session link exists");
        Ok(Some(ClaudeSessionLinkMutation {
            link,
            previous_session_id,
            republished_outbox,
        }))
    }

    pub async fn bind_discovered_sid(
        pool: &SqlitePool,
        id: Uuid,
        claude_session_id: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query!(
            r#"UPDATE cli_pane_bindings SET claude_session_id = $1
               WHERE id = $2 AND released_at IS NULL
                 AND bound_via = 'cli-fresh'
                 AND claude_session_id IS NULL"#,
            claude_session_id,
            id
        )
        .execute(pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn release(pool: &SqlitePool, id: Uuid) -> Result<bool, sqlx::Error> {
        let result = sqlx::query!(
            r#"UPDATE cli_pane_bindings SET released_at = datetime('now', 'subsec')
               WHERE id = $1 AND released_at IS NULL"#,
            id
        )
        .execute(pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}
