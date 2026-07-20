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
        if let Some(existing) =
            Self::find_latest_by_path(pool, data.dir_path, data.file_name).await?
        {
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
            return Ok(Self::find_by_id(pool, existing.id)
                .await?
                .expect("row exists"));
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
}
