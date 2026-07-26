use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct CliNativeFile {
    pub id: Uuid,
    pub claude_session_id: String,
    pub dir_path: String,
    pub file_name: String,
    pub discovered_workspace_id: Option<Uuid>,
    pub dev: i64,
    pub inode: i64,
    pub generation: i64,
    pub cursor_offset: i64,
    pub next_line_seq: i64,
    pub last_line_offset: i64,
    pub last_line_hash: Option<String>,
    pub observed_size: i64,
    pub observed_mtime_ms: Option<i64>,
    pub last_import_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct RegisterCliNativeFile<'a> {
    pub claude_session_id: &'a str,
    pub dir_path: &'a str,
    pub file_name: &'a str,
    pub discovered_workspace_id: Option<Uuid>,
    pub dev: i64,
    pub inode: i64,
    pub observed_size: i64,
    pub observed_mtime_ms: Option<i64>,
}

impl CliNativeFile {
    const SELECT_FIELDS: &'static str = r#"
        f.id, f.claude_session_id, f.dir_path, f.file_name,
        f.discovered_workspace_id, f.dev, f.inode, f.generation,
        f.cursor_offset, f.next_line_seq, f.last_line_offset,
        f.last_line_hash, f.observed_size, f.observed_mtime_ms,
        f.last_import_at, f.created_at, f.updated_at
    "#;

    pub async fn find_latest_by_path(
        pool: &SqlitePool,
        dir_path: &str,
        file_name: &str,
    ) -> Result<Option<Self>, sqlx::Error> {
        // Dynamic query keeps the shared projection list in one place; SQL
        // remains encapsulated in the db model as required by the service API.
        let sql = format!(
            "SELECT {} FROM cli_native_files f \
             WHERE dir_path = ? AND file_name = ? \
             ORDER BY generation DESC LIMIT 1",
            Self::SELECT_FIELDS
        );
        sqlx::query_as::<_, CliNativeFile>(&sql)
            .bind(dir_path)
            .bind(file_name)
            .fetch_optional(pool)
            .await
    }

    pub async fn register(
        pool: &SqlitePool,
        data: &RegisterCliNativeFile<'_>,
    ) -> Result<Self, sqlx::Error> {
        if let Some(mut existing) =
            Self::find_latest_by_path(pool, data.dir_path, data.file_name).await?
        {
            // Registration runs on every scan of every tracked file, so writing
            // unconditionally would take the database's single write lock once
            // per file per poll just to restamp `updated_at` — which nothing
            // reads, and which a real import restamps anyway. Only the observed
            // stat fields and a first-time workspace attribution are worth a
            // write; `COALESCE` in the statement below means a `None` discovery
            // never clears an existing one, so it is not a change either.
            let discovery = data
                .discovered_workspace_id
                .filter(|id| Some(*id) != existing.discovered_workspace_id);
            let changed = discovery.is_some()
                || existing.observed_size != data.observed_size
                || existing.observed_mtime_ms != data.observed_mtime_ms;
            if !changed {
                return Ok(existing);
            }

            sqlx::query!(
                r#"UPDATE cli_native_files SET
                       discovered_workspace_id = COALESCE($1, discovered_workspace_id),
                       observed_size = $2,
                       observed_mtime_ms = $3,
                       updated_at = datetime('now', 'subsec')
                   WHERE id = $4"#,
                data.discovered_workspace_id,
                data.observed_size,
                data.observed_mtime_ms,
                existing.id
            )
            .execute(pool)
            .await?;
            // Re-reading only to observe writes we just made is another lock
            // acquisition on the hot path; apply them to the row we already hold.
            existing.discovered_workspace_id = discovery.or(existing.discovered_workspace_id);
            existing.observed_size = data.observed_size;
            existing.observed_mtime_ms = data.observed_mtime_ms;
            existing.updated_at = Utc::now();
            return Ok(existing);
        }

        Self::insert_generation(pool, data, 0).await
    }

    pub async fn bump_generation(
        pool: &SqlitePool,
        data: &RegisterCliNativeFile<'_>,
    ) -> Result<Self, sqlx::Error> {
        let next_generation = sqlx::query_scalar!(
            r#"SELECT COALESCE(MAX(generation), -1) + 1 AS "generation!: i64"
               FROM cli_native_files
               WHERE dir_path = $1 AND file_name = $2"#,
            data.dir_path,
            data.file_name
        )
        .fetch_one(pool)
        .await?;
        Self::insert_generation(pool, data, next_generation).await
    }

    async fn insert_generation(
        pool: &SqlitePool,
        data: &RegisterCliNativeFile<'_>,
        generation: i64,
    ) -> Result<Self, sqlx::Error> {
        let id = Uuid::new_v4();
        sqlx::query!(
            r#"INSERT INTO cli_native_files
                   (id, claude_session_id, dir_path, file_name,
                    discovered_workspace_id, dev, inode, generation,
                    observed_size, observed_mtime_ms)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"#,
            id,
            data.claude_session_id,
            data.dir_path,
            data.file_name,
            data.discovered_workspace_id,
            data.dev,
            data.inode,
            generation,
            data.observed_size,
            data.observed_mtime_ms
        )
        .execute(pool)
        .await?;
        Ok(Self::find_by_id(pool, id)
            .await?
            .expect("inserted row exists"))
    }

    pub async fn find_by_id(pool: &SqlitePool, id: Uuid) -> Result<Option<Self>, sqlx::Error> {
        let sql = format!(
            "SELECT {} FROM cli_native_files f WHERE f.id = ?",
            Self::SELECT_FIELDS
        );
        sqlx::query_as::<_, CliNativeFile>(&sql)
            .bind(id)
            .fetch_optional(pool)
            .await
    }

    pub async fn list_latest_by_sid(
        pool: &SqlitePool,
        claude_session_id: &str,
    ) -> Result<Vec<Self>, sqlx::Error> {
        let sql = format!(
            "SELECT {} FROM cli_native_files f \
             WHERE f.claude_session_id = ? \
               AND f.generation = (SELECT MAX(generation) FROM cli_native_files newer \
                                   WHERE newer.dir_path = f.dir_path \
                                     AND newer.file_name = f.file_name) \
             ORDER BY f.dir_path, f.file_name",
            Self::SELECT_FIELDS
        );
        sqlx::query_as::<_, CliNativeFile>(&sql)
            .bind(claude_session_id)
            .fetch_all(pool)
            .await
    }

    pub async fn list_unassigned_for_workspace(
        pool: &SqlitePool,
        workspace_id: Uuid,
    ) -> Result<Vec<Self>, sqlx::Error> {
        let sql = format!(
            "SELECT {} FROM cli_native_files f \
             LEFT JOIN claude_session_links l \
               ON l.claude_session_id = f.claude_session_id \
             WHERE f.discovered_workspace_id = ? AND l.claude_session_id IS NULL \
               AND f.generation = (SELECT MAX(generation) FROM cli_native_files newer \
                                   WHERE newer.dir_path = f.dir_path \
                                     AND newer.file_name = f.file_name) \
             ORDER BY f.observed_mtime_ms DESC, f.file_name",
            Self::SELECT_FIELDS
        );
        sqlx::query_as::<_, CliNativeFile>(&sql)
            .bind(workspace_id)
            .fetch_all(pool)
            .await
    }

    /// Deletes ingested transcripts that nothing can reach any more.
    ///
    /// Raw transcript text is by far the largest thing this schema stores and
    /// nothing ever reclaimed it: `cli_native_files` has no foreign key to
    /// `sessions`, so deleting a session stranded its transcript, and the
    /// cascades that would have collected the rest never run because
    /// `PRAGMA foreign_keys` defaults to off on every connection.
    ///
    /// A file is prunable only once it is older than `retention_days` *and*
    /// unreachable: it has no [`claude_session_links`] row, so no session can
    /// render it and the unassigned-CLI adoption list has had the whole window
    /// to claim it, and it has no outbox row belonging to a session that still
    /// exists, so no live feed can be replaying it.
    ///
    /// Each file is deleted in its own transaction and `file_limit` bounds the
    /// sweep: holding the single write lock is the precise problem retention
    /// exists to relieve, so a backlog is worked off across sweeps.
    pub async fn prune_unreachable(
        pool: &SqlitePool,
        retention_days: u32,
        file_limit: i64,
    ) -> Result<PrunedTranscripts, sqlx::Error> {
        let cutoff = format!("-{retention_days} days");
        let candidates = sqlx::query_scalar!(
            r#"SELECT f.id AS "id!: Uuid"
               FROM cli_native_files f
               WHERE COALESCE(f.last_import_at, f.updated_at, f.created_at)
                     < datetime('now', $1)
                 AND NOT EXISTS (
                     SELECT 1 FROM claude_session_links l
                     WHERE l.claude_session_id = f.claude_session_id
                 )
                 AND NOT EXISTS (
                     SELECT 1 FROM cli_ingest_outbox o
                     JOIN sessions s ON s.id = o.session_id
                     WHERE o.file_id = f.id
                 )
               ORDER BY COALESCE(f.last_import_at, f.updated_at, f.created_at) ASC
               LIMIT $2"#,
            cutoff,
            file_limit
        )
        .fetch_all(pool)
        .await?;

        let mut pruned = PrunedTranscripts::default();
        for file_id in candidates {
            let mut tx = pool.begin().await?;
            // Children first and explicitly, since ON DELETE CASCADE is inert
            // while foreign keys are disabled.
            sqlx::query!("DELETE FROM cli_ingest_outbox WHERE file_id = $1", file_id)
                .execute(&mut *tx)
                .await?;
            let records =
                sqlx::query!("DELETE FROM cli_native_records WHERE file_id = $1", file_id)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            let files = sqlx::query!("DELETE FROM cli_native_files WHERE id = $1", file_id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            tx.commit().await?;
            pruned.records += records;
            pruned.files += files;
        }
        Ok(pruned)
    }
}

/// What one retention sweep reclaimed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrunedTranscripts {
    pub files: u64,
    pub records: u64,
}

impl PrunedTranscripts {
    pub fn is_empty(self) -> bool {
        self.files == 0 && self.records == 0
    }
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;
    use crate::models::{
        session::{CreateSession, Session},
        workspace::{CreateWorkspace, Workspace},
    };

    const SID: &str = "06a7eacd-664b-4d9c-83f3-d4774a6216a8";
    /// Distinguishable from any timestamp a write would produce.
    const SENTINEL: &str = "2000-01-01 00:00:00.000";

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::run_migrations_for_tests(&pool).await.unwrap();
        pool
    }

    fn registration(
        observed_size: i64,
        observed_mtime_ms: Option<i64>,
    ) -> RegisterCliNativeFile<'static> {
        RegisterCliNativeFile {
            claude_session_id: SID,
            dir_path: "/home/dev/project",
            file_name: "session.jsonl",
            discovered_workspace_id: None,
            dev: 1,
            inode: 2,
            observed_size,
            observed_mtime_ms,
        }
    }

    /// Backdates the bookkeeping timestamps so a later write is unmistakable
    /// without depending on clock resolution.
    async fn backdate(pool: &SqlitePool, id: Uuid) {
        sqlx::query(
            "UPDATE cli_native_files
             SET updated_at = ?1, last_import_at = ?1, created_at = ?1
             WHERE id = ?2",
        )
        .bind(SENTINEL)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn updated_at(pool: &SqlitePool, id: Uuid) -> String {
        sqlx::query_scalar::<_, String>("SELECT updated_at FROM cli_native_files WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn add_records(pool: &SqlitePool, file_id: Uuid, count: i64) {
        for line_seq in 0..count {
            sqlx::query(
                "INSERT INTO cli_native_records
                     (file_id, line_seq, claude_session_id, kind, raw, disposition)
                 VALUES (?, ?, ?, 'user', '{}', 'renderable')",
            )
            .bind(file_id)
            .bind(line_seq)
            .bind(SID)
            .execute(pool)
            .await
            .unwrap();
        }
    }

    async fn remaining_records(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM cli_native_records")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn rescan_with_unchanged_stats_does_not_write() {
        let pool = pool().await;
        let file = CliNativeFile::register(&pool, &registration(120, Some(9)))
            .await
            .unwrap();
        backdate(&pool, file.id).await;

        let again = CliNativeFile::register(&pool, &registration(120, Some(9)))
            .await
            .unwrap();

        assert_eq!(again.id, file.id);
        assert_eq!(
            updated_at(&pool, file.id).await,
            SENTINEL,
            "an unchanged rescan must not take the write lock"
        );
    }

    #[tokio::test]
    async fn rescan_with_new_stats_writes_them() {
        let pool = pool().await;
        let file = CliNativeFile::register(&pool, &registration(120, Some(9)))
            .await
            .unwrap();
        backdate(&pool, file.id).await;

        let grown = CliNativeFile::register(&pool, &registration(310, Some(11)))
            .await
            .unwrap();

        assert_eq!(grown.observed_size, 310);
        assert_eq!(grown.observed_mtime_ms, Some(11));
        assert_ne!(updated_at(&pool, file.id).await, SENTINEL);
        // The returned row must match what a fresh read would see.
        let reread = CliNativeFile::find_by_id(&pool, file.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reread.observed_size, 310);
        assert_eq!(reread.observed_mtime_ms, Some(11));
    }

    #[tokio::test]
    async fn first_workspace_attribution_is_written_then_never_rewritten() {
        let pool = pool().await;
        let workspace_id = Uuid::new_v4();
        Workspace::create(
            &pool,
            &CreateWorkspace {
                branch: "main".to_string(),
                name: Some("retention test".to_string()),
            },
            workspace_id,
        )
        .await
        .unwrap();

        let file = CliNativeFile::register(&pool, &registration(120, Some(9)))
            .await
            .unwrap();
        assert_eq!(file.discovered_workspace_id, None);
        backdate(&pool, file.id).await;

        let attributed = CliNativeFile::register(
            &pool,
            &RegisterCliNativeFile {
                discovered_workspace_id: Some(workspace_id),
                ..registration(120, Some(9))
            },
        )
        .await
        .unwrap();
        assert_eq!(attributed.discovered_workspace_id, Some(workspace_id));
        assert_ne!(updated_at(&pool, file.id).await, SENTINEL);

        // Re-attributing the same workspace is not a change.
        backdate(&pool, file.id).await;
        CliNativeFile::register(
            &pool,
            &RegisterCliNativeFile {
                discovered_workspace_id: Some(workspace_id),
                ..registration(120, Some(9))
            },
        )
        .await
        .unwrap();
        assert_eq!(updated_at(&pool, file.id).await, SENTINEL);
    }

    #[tokio::test]
    async fn prune_removes_unreachable_transcripts_past_retention() {
        let pool = pool().await;
        let file = CliNativeFile::register(&pool, &registration(120, Some(9)))
            .await
            .unwrap();
        add_records(&pool, file.id, 3).await;
        backdate(&pool, file.id).await;

        let pruned = CliNativeFile::prune_unreachable(&pool, 14, 25)
            .await
            .unwrap();

        assert_eq!(
            pruned,
            PrunedTranscripts {
                files: 1,
                records: 3
            }
        );
        assert_eq!(remaining_records(&pool).await, 0);
        assert!(
            CliNativeFile::find_by_id(&pool, file.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn prune_keeps_transcripts_inside_the_retention_window() {
        let pool = pool().await;
        let file = CliNativeFile::register(&pool, &registration(120, Some(9)))
            .await
            .unwrap();
        add_records(&pool, file.id, 3).await;

        let pruned = CliNativeFile::prune_unreachable(&pool, 14, 25)
            .await
            .unwrap();

        assert!(pruned.is_empty());
        assert_eq!(remaining_records(&pool).await, 3);
    }

    #[tokio::test]
    async fn prune_keeps_transcripts_a_session_still_owns() {
        let pool = pool().await;
        let file = CliNativeFile::register(&pool, &registration(120, Some(9)))
            .await
            .unwrap();
        add_records(&pool, file.id, 3).await;
        backdate(&pool, file.id).await;

        let workspace_id = Uuid::new_v4();
        Workspace::create(
            &pool,
            &CreateWorkspace {
                branch: "main".to_string(),
                name: Some("owner".to_string()),
            },
            workspace_id,
        )
        .await
        .unwrap();
        let session = Session::create(
            &pool,
            &CreateSession {
                executor: Some("CLAUDE_CODE".to_string()),
                name: None,
            },
            Uuid::new_v4(),
            workspace_id,
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO claude_session_links
                 (claude_session_id, session_id, workspace_id, cwd, bound_via)
             VALUES (?, ?, ?, '/home/dev/project', 'executor')",
        )
        .bind(SID)
        .bind(session.id)
        .bind(workspace_id)
        .execute(&pool)
        .await
        .unwrap();

        let pruned = CliNativeFile::prune_unreachable(&pool, 14, 25)
            .await
            .unwrap();

        assert!(pruned.is_empty(), "a linked transcript is still renderable");
        assert_eq!(remaining_records(&pool).await, 3);
    }

    #[tokio::test]
    async fn prune_keeps_unlinked_transcripts_a_live_session_can_replay() {
        let pool = pool().await;
        let file = CliNativeFile::register(&pool, &registration(120, Some(9)))
            .await
            .unwrap();
        add_records(&pool, file.id, 2).await;
        backdate(&pool, file.id).await;

        let workspace_id = Uuid::new_v4();
        Workspace::create(
            &pool,
            &CreateWorkspace {
                branch: "main".to_string(),
                name: Some("replay".to_string()),
            },
            workspace_id,
        )
        .await
        .unwrap();
        let session = Session::create(
            &pool,
            &CreateSession {
                executor: Some("CLAUDE_CODE".to_string()),
                name: None,
            },
            Uuid::new_v4(),
            workspace_id,
        )
        .await
        .unwrap();
        // The ownership link is gone but the publication log still points here.
        sqlx::query(
            "INSERT INTO cli_ingest_outbox (session_id, seq, file_id, line_seq)
             VALUES (?, 1, ?, 0)",
        )
        .bind(session.id)
        .bind(file.id)
        .execute(&pool)
        .await
        .unwrap();

        let pruned = CliNativeFile::prune_unreachable(&pool, 14, 25)
            .await
            .unwrap();

        assert!(
            pruned.is_empty(),
            "a live session's feed can still replay this"
        );
        assert_eq!(remaining_records(&pool).await, 2);
    }

    #[tokio::test]
    async fn prune_bounds_each_sweep_to_the_file_limit() {
        let pool = pool().await;
        for index in 0..3 {
            let file = CliNativeFile::register(
                &pool,
                &RegisterCliNativeFile {
                    file_name: match index {
                        0 => "a.jsonl",
                        1 => "b.jsonl",
                        _ => "c.jsonl",
                    },
                    ..registration(120, Some(9))
                },
            )
            .await
            .unwrap();
            add_records(&pool, file.id, 1).await;
            backdate(&pool, file.id).await;
        }

        let first = CliNativeFile::prune_unreachable(&pool, 14, 2)
            .await
            .unwrap();
        assert_eq!(first.files, 2);

        let second = CliNativeFile::prune_unreachable(&pool, 14, 2)
            .await
            .unwrap();
        assert_eq!(second.files, 1);

        assert!(
            CliNativeFile::prune_unreachable(&pool, 14, 2)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(remaining_records(&pool).await, 0);
    }
}
