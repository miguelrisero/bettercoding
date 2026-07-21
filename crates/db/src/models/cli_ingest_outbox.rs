use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
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
               FROM (
                   SELECT seq FROM cli_ingest_outbox WHERE session_id = $1
                   UNION ALL
                   SELECT published_seq AS seq
                   FROM cli_ingest_publisher_watermarks WHERE session_id = $1
               )"#,
            session_id
        )
        .fetch_one(pool)
        .await
    }

    pub async fn next_seq_in_transaction(
        tx: &mut Transaction<'_, Sqlite>,
        session_id: Uuid,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar!(
            r#"SELECT COALESCE(MAX(seq), 0) + 1 AS "seq!: i64"
               FROM (
                   SELECT seq FROM cli_ingest_outbox WHERE session_id = $1
                   UNION ALL
                   SELECT published_seq AS seq
                   FROM cli_ingest_publisher_watermarks WHERE session_id = $1
               )"#,
            session_id
        )
        .fetch_one(&mut **tx)
        .await
    }

    pub async fn session_maxima(
        pool: &SqlitePool,
    ) -> Result<Vec<CliIngestSessionMaximum>, sqlx::Error> {
        sqlx::query_as!(
            CliIngestSessionMaximum,
            r#"SELECT outbox.session_id AS "session_id!: Uuid",
                      MAX(outbox.seq) AS "max_seq!: i64"
               FROM cli_ingest_outbox outbox
               LEFT JOIN cli_ingest_publisher_watermarks watermark
                 ON watermark.session_id = outbox.session_id
               WHERE outbox.seq > COALESCE(watermark.published_seq, 0)
               GROUP BY outbox.session_id"#
        )
        .fetch_all(pool)
        .await
    }

    pub async fn mark_published(
        pool: &SqlitePool,
        session_id: Uuid,
        published_seq: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"INSERT INTO cli_ingest_publisher_watermarks
                   (session_id, published_seq)
               VALUES ($1, $2)
               ON CONFLICT(session_id) DO UPDATE SET
                   published_seq = MAX(published_seq, excluded.published_seq),
                   updated_at = datetime('now', 'subsec')"#,
            session_id,
            published_seq
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn published_seq(pool: &SqlitePool, session_id: Uuid) -> Result<i64, sqlx::Error> {
        Ok(sqlx::query_scalar!(
            r#"SELECT published_seq
               FROM cli_ingest_publisher_watermarks WHERE session_id = $1"#,
            session_id
        )
        .fetch_optional(pool)
        .await?
        .unwrap_or(0))
    }

    /// Remove delivery rows that can no longer participate in the current
    /// session projection. Active-generation rows stay as the stable ordering
    /// ledger; the persisted watermark prevents them from being redelivered.
    pub async fn prune_superseded(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"DELETE FROM cli_ingest_outbox
               WHERE NOT EXISTS (
                   SELECT 1
                   FROM cli_native_records record
                   JOIN cli_native_files file ON file.id = record.file_id
                   JOIN claude_session_links link
                     ON link.claude_session_id = record.claude_session_id
                   WHERE record.file_id = cli_ingest_outbox.file_id
                     AND record.line_seq = cli_ingest_outbox.line_seq
                     AND link.session_id = cli_ingest_outbox.session_id
                     AND file.generation = (
                         SELECT MAX(newer.generation)
                         FROM cli_native_files newer
                         WHERE newer.dir_path = file.dir_path
                           AND newer.file_name = file.file_name
                     )
               )"#
        )
        .execute(pool)
        .await?;
        Ok(result.rows_affected())
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
