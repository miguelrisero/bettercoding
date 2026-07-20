use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use thiserror::Error;
use ts_rs::TS;
use uuid::Uuid;

use super::workspace_repo::WorkspaceRepo;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("Session not found")]
    NotFound,
    #[error("Workspace not found")]
    WorkspaceNotFound,
    #[error("Executor mismatch: session uses {expected} but request specified {actual}")]
    ExecutorMismatch { expected: String, actual: String },
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize, TS)]
pub struct Session {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: Option<String>,
    pub executor: Option<String>,
    pub agent_working_dir: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, TS)]
pub struct CreateSession {
    pub executor: Option<String>,
    pub name: Option<String>,
}

impl Session {
    /// Resolve the directory used by the coding agent within a local
    /// workspace. A missing configured subdirectory falls back to the
    /// workspace root, matching CLI handover behavior.
    pub fn effective_working_dir(&self, container_ref: &Path) -> Option<PathBuf> {
        if container_ref.as_os_str().is_empty() {
            return None;
        }
        if let Some(relative) = self
            .agent_working_dir
            .as_deref()
            .filter(|dir| !dir.is_empty())
        {
            let joined = container_ref.join(relative);
            if joined.exists() {
                return Some(joined);
            }
        }
        Some(container_ref.to_path_buf())
    }

    pub async fn find_by_id(pool: &SqlitePool, id: Uuid) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as!(
            Session,
            r#"SELECT id AS "id!: Uuid",
                      workspace_id AS "workspace_id!: Uuid",
                      name,
                      executor,
                      agent_working_dir,
                      created_at AS "created_at!: DateTime<Utc>",
                      updated_at AS "updated_at!: DateTime<Utc>"
               FROM sessions
               WHERE id = $1"#,
            id
        )
        .fetch_optional(pool)
        .await
    }

    /// Find all sessions for a workspace, ordered by most recently used.
    /// "Most recently used" is defined as the most recent non-dev server execution process.
    /// Sessions with no executions fall back to created_at for ordering.
    pub async fn find_by_workspace_id(
        pool: &SqlitePool,
        workspace_id: Uuid,
    ) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as!(
            Session,
            r#"SELECT s.id AS "id!: Uuid",
                      s.workspace_id AS "workspace_id!: Uuid",
                      s.name,
                      s.executor,
                      s.agent_working_dir,
                      s.created_at AS "created_at!: DateTime<Utc>",
                      s.updated_at AS "updated_at!: DateTime<Utc>"
               FROM sessions s
               LEFT JOIN (
                   SELECT ep.session_id, MAX(ep.created_at) as last_used
                   FROM execution_processes ep
                   WHERE ep.run_reason != 'devserver' AND ep.dropped = FALSE
                   GROUP BY ep.session_id
               ) latest_ep ON s.id = latest_ep.session_id
               WHERE s.workspace_id = $1
               ORDER BY COALESCE(latest_ep.last_used, s.created_at) DESC"#,
            workspace_id
        )
        .fetch_all(pool)
        .await
    }

    /// Find the most recently used session for a workspace.
    /// "Most recently used" is defined as the most recent non-dev server execution process.
    /// Sessions with no executions fall back to created_at for ordering.
    pub async fn find_latest_by_workspace_id(
        pool: &SqlitePool,
        workspace_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as!(
            Session,
            r#"SELECT s.id AS "id!: Uuid",
                      s.workspace_id AS "workspace_id!: Uuid",
                      s.name,
                      s.executor,
                      s.agent_working_dir,
                      s.created_at AS "created_at!: DateTime<Utc>",
                      s.updated_at AS "updated_at!: DateTime<Utc>"
               FROM sessions s
               LEFT JOIN (
                   SELECT ep.session_id, MAX(ep.created_at) as last_used
                   FROM execution_processes ep
                   WHERE ep.run_reason != 'devserver' AND ep.dropped = FALSE
                   GROUP BY ep.session_id
               ) latest_ep ON s.id = latest_ep.session_id
               WHERE s.workspace_id = $1
               ORDER BY COALESCE(latest_ep.last_used, s.created_at) DESC
               LIMIT 1"#,
            workspace_id
        )
        .fetch_optional(pool)
        .await
    }

    /// Find the first-created session for a workspace.
    /// This is a temporary policy for orchestrator MCP session discovery.
    pub async fn find_first_by_workspace_id(
        pool: &SqlitePool,
        workspace_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Session>(
            r#"SELECT id,
                      workspace_id,
                      name,
                      executor,
                      agent_working_dir,
                      created_at,
                      updated_at
               FROM sessions
               WHERE workspace_id = ?
               ORDER BY created_at ASC, id ASC
               LIMIT 1"#,
        )
        .bind(workspace_id)
        .fetch_optional(pool)
        .await
    }

    pub async fn create(
        pool: &SqlitePool,
        data: &CreateSession,
        id: Uuid,
        workspace_id: Uuid,
    ) -> Result<Self, SessionError> {
        let agent_working_dir = Self::resolve_agent_working_dir(pool, workspace_id).await?;
        let name = data.name.as_deref().filter(|s| !s.is_empty());

        Ok(sqlx::query_as!(
            Session,
            r#"INSERT INTO sessions (id, workspace_id, name, executor, agent_working_dir)
               VALUES ($1, $2, $3, $4, $5)
               RETURNING id AS "id!: Uuid",
                         workspace_id AS "workspace_id!: Uuid",
                         name,
                         executor,
                         agent_working_dir,
                         created_at AS "created_at!: DateTime<Utc>",
                         updated_at AS "updated_at!: DateTime<Utc>""#,
            id,
            workspace_id,
            name,
            data.executor,
            agent_working_dir
        )
        .fetch_one(pool)
        .await?)
    }

    async fn resolve_agent_working_dir(
        pool: &SqlitePool,
        workspace_id: Uuid,
    ) -> Result<Option<String>, sqlx::Error> {
        let repos = WorkspaceRepo::find_repos_for_workspace(pool, workspace_id).await?;
        if repos.len() != 1 {
            return Ok(None);
        }

        let repo = &repos[0];
        let path = match repo.default_working_dir.as_deref() {
            Some(subdir) if !subdir.is_empty() => std::path::PathBuf::from(&repo.name).join(subdir),
            _ => std::path::PathBuf::from(&repo.name),
        };

        Ok(Some(path.to_string_lossy().to_string()))
    }

    /// Park the workspace's initial prompt for the CLI terminal (CLI-first
    /// creation: the headless executor never runs it; the tmux bootstrap
    /// hands it to interactive claude instead). Deliberately NOT part of the
    /// `Session` struct — it's transport between creation and first terminal
    /// attach, not session state worth serializing to clients.
    pub async fn set_pending_cli_prompt(
        pool: &SqlitePool,
        id: Uuid,
        prompt: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"UPDATE sessions SET pending_cli_prompt = $1 WHERE id = $2"#,
            prompt,
            id
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Park `prompt` ONLY if nothing is parked yet. Loop wake-up re-parks use
    /// this so continuation boilerplate can never overwrite a parked-but-
    /// undelivered user prompt — the "never destroy the prompt" invariant has
    /// to hold on the write side too, not just the CAS-guarded clear. Returns
    /// whether the park happened; a skipped park is fine (the parked prompt
    /// is delivered first and the loop re-detects its limit banner).
    pub async fn set_pending_cli_prompt_if_empty(
        pool: &SqlitePool,
        id: Uuid,
        prompt: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query!(
            r#"UPDATE sessions
               SET pending_cli_prompt = $1
               WHERE id = $2 AND pending_cli_prompt IS NULL"#,
            prompt,
            id
        )
        .execute(pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The workspace's parked CLI prompt, wherever it lives: creation parks it
    /// on the CLI-first session, loop wake-ups re-park on the LATEST session,
    /// and an attach may resolve a third (frontend-selected) session — so the
    /// peek must be workspace-scoped or a prompt parked on a sibling session
    /// row would be stranded forever. Returns the owning session's id (for
    /// the eventual CAS clear) and the prompt.
    pub async fn peek_pending_cli_prompt_for_workspace(
        pool: &SqlitePool,
        workspace_id: Uuid,
    ) -> Result<Option<(Uuid, String)>, sqlx::Error> {
        let row = sqlx::query!(
            r#"SELECT id AS "id!: Uuid", pending_cli_prompt AS "prompt!: String"
               FROM sessions
               WHERE workspace_id = $1 AND pending_cli_prompt IS NOT NULL
               ORDER BY created_at DESC
               LIMIT 1"#,
            workspace_id
        )
        .fetch_optional(pool)
        .await?;
        Ok(row.map(|r| (r.id, r.prompt)))
    }

    /// Read the parked CLI prompt WITHOUT clearing it. The clear is deferred
    /// to [`clear_pending_cli_prompt`] until delivery is CONFIRMED — the
    /// launch bootstrap consumed the staged prompt file with the agent up, or
    /// the paste into the agent's pane succeeded — so neither a failure
    /// between attach and spawn nor tmux rejecting the launch command after
    /// spawn can destroy the user's only copy of the prompt. Racing
    /// first-attaches are serialized by an in-process delivery claim
    /// (`CliPromptDelivery` in local-deployment); the losing attach simply
    /// doesn't carry the prompt.
    pub async fn peek_pending_cli_prompt(
        pool: &SqlitePool,
        id: Uuid,
    ) -> Result<Option<String>, sqlx::Error> {
        Ok(sqlx::query_scalar!(
            r#"SELECT pending_cli_prompt FROM sessions WHERE id = $1"#,
            id
        )
        .fetch_optional(pool)
        .await?
        .flatten())
    }

    /// Clear the parked CLI prompt once THIS delivery's copy of it has been
    /// confirmed delivered. Compare-and-swap on the exact delivered value —
    /// a prompt parked mid-confirmation (e.g. a loop wake-up re-parked while
    /// an initial prompt's delivery was still being confirmed) is newer,
    /// undelivered, and must not be destroyed by the older delivery's clear.
    /// Returns whether the clear happened (`false` = superseded; the newer
    /// prompt stays parked for its own delivery). Idempotent and atomic (a
    /// single guarded UPDATE), so a double-call from racing attaches is a
    /// no-op.
    pub async fn clear_pending_cli_prompt(
        pool: &SqlitePool,
        id: Uuid,
        delivered: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query!(
            r#"UPDATE sessions
               SET pending_cli_prompt = NULL
               WHERE id = $1 AND pending_cli_prompt = $2"#,
            id,
            delivered
        )
        .execute(pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Persist the model + reasoning effort chosen at CLI-first creation so the
    /// workspace's CLI terminal launches interactive claude with the same
    /// selection. Like [`set_pending_cli_prompt`], this is launch transport
    /// rather than session state worth serializing to clients, so it stays off
    /// the `Session` struct. Either value may be `None` (then the launch falls
    /// back to its defaults).
    pub async fn set_cli_model_effort(
        pool: &SqlitePool,
        id: Uuid,
        model_id: Option<&str>,
        reasoning_id: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"UPDATE sessions SET cli_model_id = $1, cli_reasoning_id = $2 WHERE id = $3"#,
            model_id,
            reasoning_id,
            id
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Read the CLI launch model + reasoning effort persisted at creation.
    /// Returns `(model_id, reasoning_id)`; either may be `None`.
    pub async fn get_cli_model_effort(
        pool: &SqlitePool,
        id: Uuid,
    ) -> Result<(Option<String>, Option<String>), sqlx::Error> {
        let row = sqlx::query!(
            r#"SELECT cli_model_id, cli_reasoning_id FROM sessions WHERE id = $1"#,
            id
        )
        .fetch_optional(pool)
        .await?;
        Ok(row
            .map(|r| (r.cli_model_id, r.cli_reasoning_id))
            .unwrap_or((None, None)))
    }

    pub async fn update(
        pool: &SqlitePool,
        id: Uuid,
        name: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let name_value = name.filter(|s| !s.is_empty());
        let name_provided = name.is_some();

        sqlx::query!(
            r#"UPDATE sessions SET
                name = CASE WHEN $1 THEN $2 ELSE name END,
                updated_at = datetime('now', 'subsec')
            WHERE id = $3"#,
            name_provided,
            name_value,
            id
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn update_executor(
        pool: &SqlitePool,
        id: Uuid,
        executor: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"UPDATE sessions SET executor = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2"#,
            executor,
            id
        )
        .execute(pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(agent_working_dir: Option<&str>) -> Session {
        Session {
            id: Uuid::nil(),
            workspace_id: Uuid::nil(),
            name: None,
            executor: None,
            agent_working_dir: agent_working_dir.map(str::to_string),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn effective_working_dir_uses_existing_relative_path() {
        let base = std::env::temp_dir();
        assert_eq!(
            session(Some(".")).effective_working_dir(&base),
            Some(base.join("."))
        );
    }

    #[test]
    fn effective_working_dir_falls_back_for_missing_relative_path() {
        let base = std::env::temp_dir();
        let missing = format!("missing-session-dir-{}", Uuid::new_v4());
        assert_eq!(
            session(Some(&missing)).effective_working_dir(&base),
            Some(base)
        );
    }

    #[test]
    fn effective_working_dir_rejects_empty_container_path() {
        assert_eq!(session(None).effective_working_dir(Path::new("")), None);
    }
}
