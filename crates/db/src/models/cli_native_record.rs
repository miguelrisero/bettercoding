use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
use uuid::Uuid;

use super::{
    cli_ingest_outbox::CliIngestOutbox,
    cli_native_file::{CliNativeFile, RegisterCliNativeFile},
};

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
    pub disposition: String,
    pub bound_coding_agent_turn_id: Option<Uuid>,
    pub bound_queued_message_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliNativeRecordDisposition {
    Renderable,
    Bookkeeping,
    Sidechain,
    Unknown,
}

impl CliNativeRecordDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Renderable => "renderable",
            Self::Bookkeeping => "bookkeeping",
            Self::Sidechain => "sidechain",
            Self::Unknown => "unknown",
        }
    }
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
    pub disposition: CliNativeRecordDisposition,
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

/// Fresh, deployment-owned evidence captured immediately before an import.
/// `false` is the fail-closed default: an unreadable pane must never be
/// classified as a foreign writer.
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeImportContext {
    pub app_pane_absent: bool,
}

#[derive(Debug, Clone)]
pub struct ReplacedGenerationImport {
    pub file: CliNativeFile,
    pub imported: ImportBatchResult,
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
    pub disposition: String,
    pub linked_execution_process_id: Option<Uuid>,
    pub bound_turn_execution_process_id: Option<Uuid>,
    pub bound_queued_message_id: Option<Uuid>,
    pub seq: i64,
    pub dir_path: String,
    pub file_name: String,
    pub generation: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct NativeBranchPathRecord {
    pub claude_session_id: String,
    pub uuid: Option<String>,
    pub kind: String,
    pub raw: String,
    pub depth: i64,
}

impl CliNativeRecord {
    pub async fn dropped_branch_path(
        pool: &SqlitePool,
        session_id: Uuid,
        fork_parent_uuid: &str,
        branch_leaf_uuid: &str,
    ) -> Result<Option<Vec<NativeBranchPathRecord>>, sqlx::Error> {
        let rows = sqlx::query_as::<_, NativeBranchPathRecord>(
            r#"WITH RECURSIVE path(
                    file_id, claude_session_id, uuid, parent_uuid,
                    kind, raw, depth
                ) AS (
                    SELECT r.file_id, r.claude_session_id, r.uuid,
                           r.parent_uuid, r.kind, r.raw, 0
                    FROM cli_native_records r
                    JOIN cli_native_files f ON f.id = r.file_id
                    JOIN claude_session_links l
                      ON l.claude_session_id = r.claude_session_id
                    WHERE l.session_id = ?
                      AND r.uuid = ?
                      AND f.generation = (
                          SELECT MAX(newer.generation)
                          FROM cli_native_files newer
                          WHERE newer.dir_path = f.dir_path
                            AND newer.file_name = f.file_name
                      )
                    UNION ALL
                    SELECT parent.file_id, parent.claude_session_id,
                           parent.uuid, parent.parent_uuid,
                           parent.kind, parent.raw, child.depth + 1
                    FROM cli_native_records parent
                    JOIN path child
                      ON parent.file_id = child.file_id
                     AND parent.uuid = child.parent_uuid
                    WHERE child.uuid != ? AND child.depth < 4096
                )
                SELECT claude_session_id, uuid, kind, raw, depth
                FROM path
                ORDER BY depth DESC"#,
        )
        .bind(session_id)
        .bind(branch_leaf_uuid)
        .bind(fork_parent_uuid)
        .fetch_all(pool)
        .await?;
        if !rows
            .iter()
            .any(|record| record.uuid.as_deref() == Some(fork_parent_uuid))
        {
            return Ok(None);
        }
        Ok(Some(rows))
    }

    pub async fn session_ids_for_uuid(
        pool: &SqlitePool,
        native_uuid: &str,
    ) -> Result<Vec<Uuid>, sqlx::Error> {
        sqlx::query_scalar!(
            r#"SELECT DISTINCT l.session_id AS "session_id!: Uuid"
               FROM cli_native_records r
               JOIN claude_session_links l
                 ON l.claude_session_id = r.claude_session_id
               WHERE r.uuid = $1"#,
            native_uuid
        )
        .fetch_all(pool)
        .await
    }

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
        let result = Self::import_batch_in_transaction(
            &mut tx,
            file_id,
            records,
            cursor,
            None,
            NativeImportContext::default(),
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn import_batch_with_context(
        pool: &SqlitePool,
        file_id: Uuid,
        records: &[NewCliNativeRecord],
        cursor: &ImportedCursor<'_>,
        context: NativeImportContext,
    ) -> Result<ImportBatchResult, sqlx::Error> {
        let mut tx = pool.begin().await?;
        let result =
            Self::import_batch_in_transaction(&mut tx, file_id, records, cursor, None, context)
                .await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Activate a rewritten file only once its first complete-line batch is
    /// ready. New generation insertion, old generation removal (including
    /// cascading records/outbox), first-batch import, and cursor advancement
    /// are one transaction, so readers see either the old generation or a
    /// populated new one.
    pub async fn replace_generation_and_import_batch(
        pool: &SqlitePool,
        registration: &RegisterCliNativeFile<'_>,
        records: &[NewCliNativeRecord],
        cursor: &ImportedCursor<'_>,
    ) -> Result<ReplacedGenerationImport, sqlx::Error> {
        let mut tx = pool.begin().await?;
        let next_generation = sqlx::query_scalar!(
            r#"SELECT COALESCE(MAX(generation), -1) + 1 AS "generation!: i64"
               FROM cli_native_files
               WHERE dir_path = $1 AND file_name = $2"#,
            registration.dir_path,
            registration.file_name
        )
        .fetch_one(&mut *tx)
        .await?;

        // Capture the pre-purge watermark so replacement rows never reuse a
        // sequence already observed by a connected publisher.
        let session_id = sqlx::query_scalar!(
            r#"SELECT session_id AS "session_id!: Uuid"
               FROM claude_session_links
               WHERE claude_session_id = $1"#,
            registration.claude_session_id
        )
        .fetch_optional(&mut *tx)
        .await?;
        let next_outbox_seq = if let Some(session_id) = session_id {
            Some(CliIngestOutbox::next_seq_in_transaction(&mut tx, session_id).await?)
        } else {
            None
        };

        let file_id = Uuid::new_v4();
        sqlx::query!(
            r#"INSERT INTO cli_native_files
                   (id, claude_session_id, dir_path, file_name,
                    discovered_workspace_id, dev, inode, generation,
                    observed_size, observed_mtime_ms)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"#,
            file_id,
            registration.claude_session_id,
            registration.dir_path,
            registration.file_name,
            registration.discovered_workspace_id,
            registration.dev,
            registration.inode,
            next_generation,
            registration.observed_size,
            registration.observed_mtime_ms
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query!(
            r#"DELETE FROM cli_native_files
               WHERE dir_path = $1 AND file_name = $2 AND id != $3"#,
            registration.dir_path,
            registration.file_name,
            file_id
        )
        .execute(&mut *tx)
        .await?;

        let imported = Self::import_batch_in_transaction(
            &mut tx,
            file_id,
            records,
            cursor,
            next_outbox_seq,
            NativeImportContext::default(),
        )
        .await?;
        tx.commit().await?;

        let file = CliNativeFile::find_by_id(pool, file_id)
            .await?
            .expect("activated native generation exists");
        Ok(ReplacedGenerationImport { file, imported })
    }

    pub async fn replace_generation_and_import_batch_with_context(
        pool: &SqlitePool,
        registration: &RegisterCliNativeFile<'_>,
        records: &[NewCliNativeRecord],
        cursor: &ImportedCursor<'_>,
        context: NativeImportContext,
    ) -> Result<ReplacedGenerationImport, sqlx::Error> {
        let mut tx = pool.begin().await?;
        let next_generation = sqlx::query_scalar!(
            r#"SELECT COALESCE(MAX(generation), -1) + 1 AS "generation!: i64"
               FROM cli_native_files
               WHERE dir_path = $1 AND file_name = $2"#,
            registration.dir_path,
            registration.file_name
        )
        .fetch_one(&mut *tx)
        .await?;
        let session_id = sqlx::query_scalar!(
            r#"SELECT session_id AS "session_id!: Uuid"
               FROM claude_session_links
               WHERE claude_session_id = $1"#,
            registration.claude_session_id
        )
        .fetch_optional(&mut *tx)
        .await?;
        let next_outbox_seq = if let Some(session_id) = session_id {
            Some(CliIngestOutbox::next_seq_in_transaction(&mut tx, session_id).await?)
        } else {
            None
        };
        let file_id = Uuid::new_v4();
        sqlx::query!(
            r#"INSERT INTO cli_native_files
                   (id, claude_session_id, dir_path, file_name,
                    discovered_workspace_id, dev, inode, generation,
                    observed_size, observed_mtime_ms)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"#,
            file_id,
            registration.claude_session_id,
            registration.dir_path,
            registration.file_name,
            registration.discovered_workspace_id,
            registration.dev,
            registration.inode,
            next_generation,
            registration.observed_size,
            registration.observed_mtime_ms
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            r#"DELETE FROM cli_native_files
               WHERE dir_path = $1 AND file_name = $2 AND id != $3"#,
            registration.dir_path,
            registration.file_name,
            file_id
        )
        .execute(&mut *tx)
        .await?;
        let imported = Self::import_batch_in_transaction(
            &mut tx,
            file_id,
            records,
            cursor,
            next_outbox_seq,
            context,
        )
        .await?;
        tx.commit().await?;
        let file = CliNativeFile::find_by_id(pool, file_id)
            .await?
            .expect("activated native generation exists");
        Ok(ReplacedGenerationImport { file, imported })
    }

    async fn import_batch_in_transaction(
        tx: &mut Transaction<'_, Sqlite>,
        file_id: Uuid,
        records: &[NewCliNativeRecord],
        cursor: &ImportedCursor<'_>,
        minimum_next_outbox_seq: Option<i64>,
        context: NativeImportContext,
    ) -> Result<ImportBatchResult, sqlx::Error> {
        let imported_at = Utc::now();

        let session_id = sqlx::query_scalar!(
            r#"SELECT l.session_id AS "session_id!: Uuid"
               FROM cli_native_files f
               JOIN claude_session_links l
                 ON l.claude_session_id = f.claude_session_id
               WHERE f.id = $1"#,
            file_id
        )
        .fetch_optional(&mut **tx)
        .await?;

        let mut next_outbox_seq = if let Some(session_id) = session_id {
            CliIngestOutbox::next_seq_in_transaction(tx, session_id)
                .await?
                .max(minimum_next_outbox_seq.unwrap_or(1))
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
                .fetch_one(&mut **tx)
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
                        .fetch_optional(&mut **tx)
                        .await?
                    }
                    _ => None,
                }
            } else {
                None
            };

            let bound_queued_message_id =
                if record.kind == "user" && !linked_to_execution && bound_turn_id.is_none() {
                    match (record.user_prompt.as_deref(), session_id) {
                        (Some(prompt), Some(session_id)) => {
                            let reference_time = record.recorded_at.unwrap_or(imported_at);
                            let earliest_paste = reference_time - chrono::Duration::minutes(15);
                            let latest_paste = reference_time + chrono::Duration::seconds(5);
                            sqlx::query_scalar!(
                                r#"SELECT id AS "id!: Uuid"
                               FROM session_queued_messages
                               WHERE session_id = $1
                                 AND state IN ('pasting', 'pasted')
                                 AND prompt = $2
                                 AND (claude_session_id IS NULL OR claude_session_id = $3)
                                 AND pasted_at IS NOT NULL
                                 AND julianday(pasted_at) >= julianday($4)
                                 AND julianday(pasted_at) <= julianday($5)
                               ORDER BY pasted_at DESC
                               LIMIT 1"#,
                                session_id,
                                prompt,
                                record.claude_session_id,
                                earliest_paste,
                                latest_paste
                            )
                            .fetch_optional(&mut **tx)
                            .await?
                        }
                        _ => None,
                    }
                } else {
                    None
                };

            let disposition = record.disposition.as_str();
            let inserted = sqlx::query!(
                r#"INSERT OR IGNORE INTO cli_native_records
                       (file_id, line_seq, claude_session_id, uuid,
                        parent_uuid, kind, ts, raw,
                        disposition, bound_coding_agent_turn_id,
                        bound_queued_message_id)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"#,
                file_id,
                record.line_seq,
                record.claude_session_id,
                record.uuid,
                record.parent_uuid,
                record.kind,
                record.ts,
                record.raw,
                disposition,
                bound_turn_id,
                bound_queued_message_id
            )
            .execute(&mut **tx)
            .await?;

            if inserted.rows_affected() == 0 {
                continue;
            }

            if let Some(queue_id) = bound_queued_message_id {
                sqlx::query!(
                    r#"UPDATE session_queued_messages SET
                           state = 'imported',
                           claude_session_id = COALESCE(claude_session_id, $1),
                           acked_at = $2,
                           updated_at = $2
                       WHERE id = $3 AND state IN ('pasting', 'pasted')"#,
                    record.claude_session_id,
                    imported_at,
                    queue_id
                )
                .execute(&mut **tx)
                .await?;
            } else if context.app_pane_absent
                && record.kind == "user"
                && !linked_to_execution
                && bound_turn_id.is_none()
                && let Some(session_id) = session_id
            {
                let executor_running = sqlx::query_scalar!(
                    r#"SELECT EXISTS(
                           SELECT 1 FROM execution_processes
                           WHERE session_id = $1
                             AND status = 'running'
                             AND run_reason = 'codingagent'
                       ) AS "running!: bool""#,
                    session_id
                )
                .fetch_one(&mut **tx)
                .await?;
                if !executor_running {
                    sqlx::query!(
                        r#"UPDATE claude_session_links
                           SET foreign_writer_seen_at = $1
                           WHERE claude_session_id = $2 AND session_id = $3"#,
                        imported_at,
                        record.claude_session_id,
                        session_id
                    )
                    .execute(&mut **tx)
                    .await?;
                }
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
            .execute(&mut **tx)
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
        .execute(&mut **tx)
        .await?;
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
                      r.disposition,
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
                      r.bound_queued_message_id AS "bound_queued_message_id: Uuid",
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
