use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct CliIngestOutbox {
    pub session_id: Uuid,
    pub seq: i64,
    pub file_id: Uuid,
    pub line_seq: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct CliIngestSessionMaximum {
    pub session_id: Uuid,
    pub max_seq: i64,
}

impl CliIngestOutbox {
    pub async fn latest_seq(pool: &SqlitePool, session_id: Uuid) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar!(
            r#"SELECT COALESCE(MAX(seq), 0) AS "seq!: i64"
               FROM cli_ingest_outbox WHERE session_id = $1"#,
            session_id
        )
        .fetch_one(pool)
        .await
    }

    pub async fn session_maxima(
        pool: &SqlitePool,
    ) -> Result<Vec<CliIngestSessionMaximum>, sqlx::Error> {
        sqlx::query_as!(
            CliIngestSessionMaximum,
            r#"SELECT session_id AS "session_id!: Uuid",
                      MAX(seq) AS "max_seq!: i64"
               FROM cli_ingest_outbox
               GROUP BY session_id"#
        )
        .fetch_all(pool)
        .await
    }

    pub async fn find_after(
        pool: &SqlitePool,
        session_id: Uuid,
        seq: i64,
        limit: i64,
    ) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as!(
            CliIngestOutbox,
            r#"SELECT session_id AS "session_id!: Uuid",
                      seq,
                      file_id AS "file_id!: Uuid",
                      line_seq,
                      created_at AS "created_at!: DateTime<Utc>"
               FROM cli_ingest_outbox
               WHERE session_id = $1 AND seq > $2
               ORDER BY seq ASC
               LIMIT $3"#,
            session_id,
            seq,
            limit
        )
        .fetch_all(pool)
        .await
    }
}
