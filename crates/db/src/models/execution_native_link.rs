use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ExecutionNativeLink {
    pub execution_process_id: Uuid,
    pub native_uuid: String,
}

impl ExecutionNativeLink {
    pub async fn insert(
        pool: &SqlitePool,
        execution_process_id: Uuid,
        native_uuid: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query!(
            r#"INSERT OR IGNORE INTO execution_native_links
               (execution_process_id, native_uuid)
               VALUES ($1, $2)"#,
            execution_process_id,
            native_uuid
        )
        .execute(pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn find_execution_id(
        pool: &SqlitePool,
        native_uuid: &str,
    ) -> Result<Option<Uuid>, sqlx::Error> {
        sqlx::query_scalar!(
            r#"SELECT enl.execution_process_id AS "execution_process_id!: Uuid"
               FROM execution_native_links enl
               JOIN execution_processes ep ON ep.id = enl.execution_process_id
               WHERE enl.native_uuid = $1
               ORDER BY ep.created_at ASC
               LIMIT 1"#,
            native_uuid
        )
        .fetch_optional(pool)
        .await
    }

    pub async fn contains_at(
        pool: &SqlitePool,
        native_uuid: &str,
        before: DateTime<Utc>,
    ) -> Result<bool, sqlx::Error> {
        let found = sqlx::query_scalar!(
            r#"SELECT EXISTS(
                   SELECT 1
                   FROM execution_native_links enl
                   JOIN execution_processes ep ON ep.id = enl.execution_process_id
                   WHERE enl.native_uuid = $1 AND ep.created_at <= $2
               ) AS "found!: bool""#,
            native_uuid,
            before
        )
        .fetch_one(pool)
        .await?;
        Ok(found)
    }
}
