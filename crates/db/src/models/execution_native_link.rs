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
}
