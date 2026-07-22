use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool, Type};
use ts_rs::TS;
use uuid::Uuid;

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
