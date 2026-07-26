use std::{sync::Arc, time::Duration};

use sqlx::{
    ConnectOptions, Error, Pool, Sqlite,
    migrate::MigrateError,
    sqlite::{
        SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqlitePoolOptions,
        SqliteSynchronous,
    },
};
use utils::assets::{DB_FILE_NAME, asset_dir};

pub mod models;

/// Overrides the journal mode, accepting `wal` or `delete`.
///
/// We default to WAL: in rollback-journal mode a writer holds an exclusive lock
/// that blocks every reader, so a continuously-writing background service (CLI
/// transcript ingest, activity monitors, PR monitor) starves ordinary requests
/// until they exhaust the busy timeout and surface as 500s. Upstream pinned
/// `delete` in #1882 after reverting #1806, but recorded no reason for the
/// revert; the likely one is that WAL needs an mmap-able `-shm` sidecar and so
/// fails on network mounts. [`connect_pool`] detects that and falls back, and
/// this variable forces the old behaviour outright.
const JOURNAL_MODE_ENV: &str = "VIBE_KANBAN_SQLITE_JOURNAL_MODE";

/// SQLite serialises writers, so contention is normal and waiting is correct.
/// sqlx defaults to 5s, which a slow commit can exceed — and losing that race
/// fails the request rather than delaying it.
const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

fn preferred_journal_mode() -> SqliteJournalMode {
    match std::env::var(JOURNAL_MODE_ENV) {
        Err(_) => SqliteJournalMode::Wal,
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "wal" => SqliteJournalMode::Wal,
            "delete" => SqliteJournalMode::Delete,
            other => {
                tracing::warn!(
                    "{JOURNAL_MODE_ENV}={other:?} is not a supported journal mode \
                     (expected `wal` or `delete`); using `wal`"
                );
                SqliteJournalMode::Wal
            }
        },
    }
}

/// Applies the concurrency settings to any connection, separately from which
/// file it points at, so the settings can be exercised against a scratch
/// database in tests.
fn tune(options: SqliteConnectOptions, journal_mode: SqliteJournalMode) -> SqliteConnectOptions {
    options
        .journal_mode(journal_mode)
        .busy_timeout(BUSY_TIMEOUT)
        // WAL already fsyncs the log before a checkpoint can discard it, so
        // NORMAL only risks the most recent commits on host power loss — never
        // on process crash — and removes an fsync from every commit.
        .synchronous(match journal_mode {
            SqliteJournalMode::Wal => SqliteSynchronous::Normal,
            _ => SqliteSynchronous::Full,
        })
}

fn connect_options(journal_mode: SqliteJournalMode) -> SqliteConnectOptions {
    tune(
        SqliteConnectOptions::new()
            .filename(asset_dir().join(DB_FILE_NAME))
            .create_if_missing(true),
        journal_mode,
    )
}

/// Connects with the preferred journal mode, retrying once in `delete` mode so
/// a filesystem that cannot host WAL's shared-memory file still opens.
async fn connect_pool(
    pool_options: SqlitePoolOptions,
    disable_statement_logging: bool,
) -> Result<Pool<Sqlite>, Error> {
    let build = |journal_mode| {
        let options = connect_options(journal_mode);
        if disable_statement_logging {
            options.disable_statement_logging()
        } else {
            options
        }
    };

    let preferred = preferred_journal_mode();
    match pool_options.clone().connect_with(build(preferred)).await {
        Ok(pool) => Ok(pool),
        Err(err) if preferred == SqliteJournalMode::Wal => {
            tracing::warn!(
                %err,
                "could not open the database in WAL mode (the filesystem may not support \
                 shared memory); falling back to `delete`. Set {JOURNAL_MODE_ENV}=delete \
                 to select it explicitly."
            );
            pool_options
                .connect_with(build(SqliteJournalMode::Delete))
                .await
        }
        Err(err) => Err(err),
    }
}

async fn run_migrations(pool: &Pool<Sqlite>) -> Result<(), Error> {
    use std::collections::HashSet;

    let migrator = sqlx::migrate!("./migrations");
    let mut processed_versions: HashSet<i64> = HashSet::new();

    loop {
        match migrator.run(pool).await {
            Ok(()) => return Ok(()),
            Err(MigrateError::VersionMismatch(version)) => {
                if cfg!(debug_assertions) {
                    // return the error in debug mode to catch migration issues early
                    return Err(sqlx::Error::Migrate(Box::new(
                        MigrateError::VersionMismatch(version),
                    )));
                }

                if !cfg!(windows) {
                    // On non-Windows platforms, we do not attempt to auto-fix checksum mismatches
                    return Err(sqlx::Error::Migrate(Box::new(
                        MigrateError::VersionMismatch(version),
                    )));
                }

                // Guard against infinite loop
                if !processed_versions.insert(version) {
                    return Err(sqlx::Error::Migrate(Box::new(
                        MigrateError::VersionMismatch(version),
                    )));
                }

                // On Windows, there can be checksum mismatches due to line ending differences
                // or other platform-specific issues. Update the stored checksum and retry.
                tracing::warn!(
                    "Migration version {} has checksum mismatch, updating stored checksum (likely platform-specific difference)",
                    version
                );

                // Find the migration with the mismatched version and get its current checksum
                if let Some(migration) = migrator.iter().find(|m| m.version == version) {
                    // Update the checksum in _sqlx_migrations to match the current file
                    sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = ?")
                        .bind(&*migration.checksum)
                        .bind(version)
                        .execute(pool)
                        .await?;
                } else {
                    // Migration not found in current set, can't fix
                    return Err(sqlx::Error::Migrate(Box::new(
                        MigrateError::VersionMismatch(version),
                    )));
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
}

#[doc(hidden)]
pub async fn run_migrations_for_tests(pool: &Pool<Sqlite>) -> Result<(), Error> {
    run_migrations(pool).await
}

#[derive(Clone)]
pub struct DBService {
    pub pool: Pool<Sqlite>,
}

impl DBService {
    pub async fn new() -> Result<DBService, Error> {
        let pool = connect_pool(SqlitePoolOptions::new(), false).await?;
        run_migrations(&pool).await?;
        Ok(DBService { pool })
    }

    pub async fn new_migration_pool() -> Result<Pool<Sqlite>, Error> {
        connect_pool(SqlitePoolOptions::new().max_connections(64), true).await
    }

    pub async fn new_with_after_connect<F>(after_connect: F) -> Result<DBService, Error>
    where
        F: for<'a> Fn(
                &'a mut SqliteConnection,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<(), Error>> + Send + 'a>,
            > + Send
            + Sync
            + 'static,
    {
        let pool = Self::create_pool(Some(Arc::new(after_connect))).await?;
        Ok(DBService { pool })
    }

    async fn create_pool<F>(after_connect: Option<Arc<F>>) -> Result<Pool<Sqlite>, Error>
    where
        F: for<'a> Fn(
                &'a mut SqliteConnection,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<(), Error>> + Send + 'a>,
            > + Send
            + Sync
            + 'static,
    {
        let pool_options = match after_connect {
            Some(hook) => SqlitePoolOptions::new().after_connect(move |conn, _meta| {
                let hook = hook.clone();
                Box::pin(async move {
                    hook(conn).await?;
                    Ok(())
                })
            }),
            None => SqlitePoolOptions::new(),
        };
        let pool = connect_pool(pool_options, false).await?;

        run_migrations(&pool).await?;
        Ok(pool)
    }
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;
    use uuid::Uuid;

    use super::*;

    /// Opens a scratch database with the real tuning and reports the pragmas
    /// SQLite actually settled on — `journal_mode` is a property of the file, so
    /// asking for WAL is not the same as getting it.
    async fn effective_pragmas(journal_mode: SqliteJournalMode) -> (String, i64) {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(tune(
                SqliteConnectOptions::new()
                    .filename(dir.path().join("pragmas.sqlite"))
                    .create_if_missing(true),
                journal_mode,
            ))
            .await
            .unwrap();
        let mode = sqlx::query_scalar::<_, String>("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .unwrap();
        let timeout = sqlx::query_scalar::<_, i64>("PRAGMA busy_timeout")
            .fetch_one(&pool)
            .await
            .unwrap();
        pool.close().await;
        (mode, timeout)
    }

    #[tokio::test]
    async fn wal_and_a_generous_busy_timeout_are_actually_applied() {
        let (mode, busy_timeout) = effective_pragmas(SqliteJournalMode::Wal).await;
        assert_eq!(mode, "wal");
        assert_eq!(busy_timeout, BUSY_TIMEOUT.as_millis() as i64);
    }

    #[tokio::test]
    async fn the_delete_fallback_remains_available() {
        let (mode, busy_timeout) = effective_pragmas(SqliteJournalMode::Delete).await;
        assert_eq!(mode, "delete");
        assert_eq!(busy_timeout, BUSY_TIMEOUT.as_millis() as i64);
    }

    #[tokio::test]
    async fn a_reader_does_not_block_a_writer_under_wal() {
        // The behaviour the whole change exists for: in `delete` mode an open
        // read transaction holds a shared lock that fails the writer, which is
        // what surfaced as 500s.
        let dir = tempfile::tempdir().unwrap();
        let options = tune(
            SqliteConnectOptions::new()
                .filename(dir.path().join("concurrent.sqlite"))
                .create_if_missing(true),
            SqliteJournalMode::Wal,
        )
        // Without waiting, any blocking would surface immediately as an error.
        .busy_timeout(Duration::ZERO);
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE t (v INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO t (v) VALUES (1)")
            .execute(&pool)
            .await
            .unwrap();

        let mut reader = pool.begin().await.unwrap();
        let seen = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM t")
            .fetch_one(&mut *reader)
            .await
            .unwrap();
        assert_eq!(seen, 1);

        sqlx::query("INSERT INTO t (v) VALUES (2)")
            .execute(&pool)
            .await
            .expect("an open read transaction must not block a writer under WAL");

        // The reader keeps its original snapshot.
        let still_seen = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM t")
            .fetch_one(&mut *reader)
            .await
            .unwrap();
        assert_eq!(still_seen, 1);
        drop(reader);
        pool.close().await;
    }

    #[tokio::test]
    async fn archived_at_migration_backfills_preexisting_archived_rows() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE workspaces (
                id BLOB PRIMARY KEY,
                updated_at TEXT NOT NULL,
                archived INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        let archived_id = Uuid::new_v4();
        let active_id = Uuid::new_v4();
        let archived_updated_at = "2026-07-20 12:34:56.789";
        sqlx::query("INSERT INTO workspaces (id, updated_at, archived) VALUES (?, ?, TRUE)")
            .bind(archived_id)
            .bind(archived_updated_at)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO workspaces (id, updated_at, archived) VALUES (?, ?, FALSE)")
            .bind(active_id)
            .bind("2026-07-21 01:02:03.456")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::raw_sql(include_str!(
            "../migrations/20260721103000_add_workspace_archived_at.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();

        let archived_at: Option<String> =
            sqlx::query_scalar("SELECT archived_at FROM workspaces WHERE id = ?")
                .bind(archived_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let active_archived_at: Option<String> =
            sqlx::query_scalar("SELECT archived_at FROM workspaces WHERE id = ?")
                .bind(active_id)
                .fetch_one(&pool)
                .await
                .unwrap();

        assert_eq!(archived_at.as_deref(), Some(archived_updated_at));
        assert_eq!(active_archived_at, None);
    }
}
