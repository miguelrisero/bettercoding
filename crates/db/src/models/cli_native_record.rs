use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct CliNativeRecord {
    pub file_id: Uuid,
    pub line_seq: i64,
    pub claude_session_id: String,
    pub uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub kind: String,
    pub ts: Option<String>,
    pub raw: String,
    pub bound_coding_agent_turn_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct NewCliNativeRecord {
    pub line_seq: i64,
    pub claude_session_id: String,
    pub uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub kind: String,
    pub ts: Option<String>,
    pub raw: String,
    /// Plain user text used only for durable app-turn reconciliation.
    pub user_prompt: Option<String>,
    pub recorded_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct ImportedCursor<'a> {
    pub cursor_offset: i64,
    pub next_line_seq: i64,
    pub last_line_offset: i64,
    pub last_line_hash: Option<&'a str>,
    pub observed_size: i64,
    pub observed_mtime_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportBatchResult {
    pub inserted_records: u64,
    pub appended_outbox: u64,
    pub last_seq: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct SessionNativeRecord {
    pub file_id: Uuid,
    pub line_seq: i64,
    pub claude_session_id: String,
    pub uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub kind: String,
    pub ts: Option<String>,
    pub raw: String,
    pub bound_coding_agent_turn_id: Option<Uuid>,
    pub linked_execution_process_id: Option<Uuid>,
    pub bound_turn_execution_process_id: Option<Uuid>,
    pub seq: i64,
    pub dir_path: String,
    pub file_name: String,
    pub generation: i64,
}

impl CliNativeRecord {
    /// Insert raw records, append per-session outbox rows, and advance the
    /// newline-aligned cursor atomically. A replay of an existing line is a
    /// no-op for both the raw table and outbox.
    pub async fn import_batch(
        pool: &SqlitePool,
        file_id: Uuid,
        records: &[NewCliNativeRecord],
        cursor: &ImportedCursor<'_>,
    ) -> Result<ImportBatchResult, sqlx::Error> {
        let mut tx = pool.begin().await?;
        let imported_at = Utc::now();

        let session_id = sqlx::query_scalar!(
            r#"SELECT l.session_id AS "session_id!: Uuid"
               FROM cli_native_files f
               JOIN claude_session_links l
                 ON l.claude_session_id = f.claude_session_id
               WHERE f.id = $1"#,
            file_id
        )
        .fetch_optional(&mut *tx)
        .await?;

        let mut next_outbox_seq = if let Some(session_id) = session_id {
            sqlx::query_scalar!(
                r#"SELECT COALESCE(MAX(seq), 0) + 1 AS "seq!: i64"
                   FROM cli_ingest_outbox WHERE session_id = $1"#,
                session_id
            )
            .fetch_one(&mut *tx)
            .await?
        } else {
            0
        };

        let mut result = ImportBatchResult::default();

        for record in records {
            let linked_to_execution = if let Some(native_uuid) = record.uuid.as_deref() {
                sqlx::query_scalar!(
                    r#"SELECT EXISTS(
                           SELECT 1 FROM execution_native_links
                           WHERE native_uuid = $1
                       ) AS "linked!: bool""#,
                    native_uuid
                )
                .fetch_one(&mut *tx)
                .await?
            } else {
                false
            };

            let bound_turn_id = if record.kind == "user" && !linked_to_execution {
                match (record.user_prompt.as_deref(), session_id) {
                    (Some(prompt), Some(session_id)) => {
                        // Prompt equality is only a backfill bridge for an app
                        // dispatch close to this native record. Native records
                        // without a timestamp use this import's clock so they
                        // can still reconcile without matching old turns.
                        let reference_time = record.recorded_at.unwrap_or(imported_at);
                        let window_start = reference_time - chrono::Duration::minutes(15);
                        sqlx::query_scalar!(
                            r#"SELECT cat.id AS "id!: Uuid"
                               FROM coding_agent_turns cat
                               JOIN execution_processes ep
                                 ON ep.id = cat.execution_process_id
                               WHERE ep.session_id = $1
                                 AND ep.run_reason = 'codingagent'
                                 AND ep.dropped = FALSE
                                 AND cat.prompt = $2
                                 AND cat.created_at >= $3
                                 AND cat.created_at <= $4
                                 AND NOT EXISTS (
                                     SELECT 1 FROM cli_native_records existing
                                     WHERE existing.bound_coding_agent_turn_id = cat.id
                                 )
                                 AND NOT EXISTS (
                                     SELECT 1
                                     FROM cli_native_records native_record
                                     JOIN execution_native_links enl
                                       ON enl.native_uuid = native_record.uuid
                                     WHERE enl.execution_process_id = cat.execution_process_id
                                 )
                               ORDER BY cat.created_at DESC
                               LIMIT 1"#,
                            session_id,
                            prompt,
                            window_start,
                            reference_time
                        )
                        .fetch_optional(&mut *tx)
                        .await?
                    }
                    _ => None,
                }
            } else {
                None
            };

            let inserted = sqlx::query!(
                r#"INSERT OR IGNORE INTO cli_native_records
                       (file_id, line_seq, claude_session_id, uuid,
                        parent_uuid, kind, ts, raw,
                        bound_coding_agent_turn_id)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"#,
                file_id,
                record.line_seq,
                record.claude_session_id,
                record.uuid,
                record.parent_uuid,
                record.kind,
                record.ts,
                record.raw,
                bound_turn_id
            )
            .execute(&mut *tx)
            .await?;

            if inserted.rows_affected() == 0 {
                continue;
            }

            result.inserted_records += 1;
            let Some(session_id) = session_id else {
                continue;
            };
            let outbox_inserted = sqlx::query!(
                r#"INSERT INTO cli_ingest_outbox
                       (session_id, seq, file_id, line_seq)
                   VALUES ($1, $2, $3, $4)"#,
                session_id,
                next_outbox_seq,
                file_id,
                record.line_seq
            )
            .execute(&mut *tx)
            .await?;

            if outbox_inserted.rows_affected() > 0 {
                result.appended_outbox += 1;
                result.last_seq = next_outbox_seq;
                next_outbox_seq += 1;
            }
        }

        sqlx::query!(
            r#"UPDATE cli_native_files SET
                   cursor_offset = $1,
                   next_line_seq = $2,
                   last_line_offset = $3,
                   last_line_hash = $4,
                   observed_size = $5,
                   observed_mtime_ms = $6,
                   last_import_at = datetime('now', 'subsec'),
                   updated_at = datetime('now', 'subsec')
               WHERE id = $7"#,
            cursor.cursor_offset,
            cursor.next_line_seq,
            cursor.last_line_offset,
            cursor.last_line_hash,
            cursor.observed_size,
            cursor.observed_mtime_ms,
            file_id
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(result)
    }

    pub async fn list_for_session(
        pool: &SqlitePool,
        session_id: Uuid,
    ) -> Result<Vec<SessionNativeRecord>, sqlx::Error> {
        sqlx::query_as!(
            SessionNativeRecord,
            r#"SELECT r.file_id AS "file_id!: Uuid",
                      r.line_seq,
                      r.claude_session_id,
                      r.uuid,
                      r.parent_uuid,
                      r.kind,
                      r.ts,
                      r.raw,
                      r.bound_coding_agent_turn_id AS "bound_coding_agent_turn_id: Uuid",
                      (
                          SELECT enl.execution_process_id
                          FROM execution_native_links enl
                          JOIN execution_processes linked_ep
                            ON linked_ep.id = enl.execution_process_id
                          WHERE enl.native_uuid = r.uuid
                          ORDER BY linked_ep.created_at ASC
                          LIMIT 1
                      ) AS "linked_execution_process_id: Uuid",
                      cat.execution_process_id AS "bound_turn_execution_process_id: Uuid",
                      outbox.seq,
                      f.dir_path,
                      f.file_name,
                      f.generation
               FROM cli_native_records r
               JOIN cli_native_files f ON f.id = r.file_id
               JOIN claude_session_links l
                 ON l.claude_session_id = r.claude_session_id
               JOIN cli_ingest_outbox outbox
                 ON outbox.file_id = r.file_id
                AND outbox.line_seq = r.line_seq
                AND outbox.session_id = l.session_id
               LEFT JOIN coding_agent_turns cat
                 ON cat.id = r.bound_coding_agent_turn_id
               WHERE l.session_id = $1
                 AND f.generation = (
                     SELECT MAX(newer.generation)
                     FROM cli_native_files newer
                     WHERE newer.dir_path = f.dir_path
                       AND newer.file_name = f.file_name
                 )
               ORDER BY outbox.seq ASC"#,
            session_id
        )
        .fetch_all(pool)
        .await
    }

    pub async fn count_for_file(pool: &SqlitePool, file_id: Uuid) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64"
               FROM cli_native_records WHERE file_id = $1"#,
            file_id
        )
        .fetch_one(pool)
        .await
    }
}
