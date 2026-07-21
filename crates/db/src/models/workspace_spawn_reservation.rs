use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool, Type};
use ts_rs::TS;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type, TS)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum SpawnReservationHolder {
    Executor,
    Cli,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize, TS)]
pub struct WorkspaceSpawnReservation {
    pub workspace_id: Uuid,
    pub holder: SpawnReservationHolder,
    pub fence: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl WorkspaceSpawnReservation {
    pub const DEFAULT_TTL: Duration = Duration::seconds(15);

    pub async fn acquire(
        pool: &SqlitePool,
        workspace_id: Uuid,
        holder: SpawnReservationHolder,
    ) -> Result<Option<Self>, sqlx::Error> {
        Self::acquire_at(pool, workspace_id, holder, Utc::now(), Self::DEFAULT_TTL).await
    }

    pub async fn acquire_at(
        pool: &SqlitePool,
        workspace_id: Uuid,
        holder: SpawnReservationHolder,
        now: DateTime<Utc>,
        ttl: Duration,
    ) -> Result<Option<Self>, sqlx::Error> {
        let mut tx = pool.begin().await?;
        sqlx::query!(
            r#"DELETE FROM workspace_spawn_reservations
               WHERE workspace_id = $1 AND expires_at <= $2"#,
            workspace_id,
            now
        )
        .execute(&mut *tx)
        .await?;

        let fence = Uuid::new_v4().to_string();
        let expires_at = now + ttl;
        let inserted = sqlx::query!(
            r#"INSERT OR IGNORE INTO workspace_spawn_reservations
                   (workspace_id, holder, fence, created_at, expires_at)
               VALUES ($1, $2, $3, $4, $5)"#,
            workspace_id,
            holder,
            fence,
            now,
            expires_at
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;

        if inserted == 0 {
            return Ok(None);
        }
        Ok(Some(Self {
            workspace_id,
            holder,
            fence,
            created_at: now,
            expires_at,
        }))
    }

    pub async fn release(
        pool: &SqlitePool,
        workspace_id: Uuid,
        fence: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query!(
            r#"DELETE FROM workspace_spawn_reservations
               WHERE workspace_id = $1 AND fence = $2"#,
            workspace_id,
            fence
        )
        .execute(pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn find(pool: &SqlitePool, workspace_id: Uuid) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as!(
            Self,
            r#"SELECT workspace_id AS "workspace_id!: Uuid",
                      holder AS "holder!: SpawnReservationHolder",
                      fence,
                      created_at AS "created_at!: DateTime<Utc>",
                      expires_at AS "expires_at!: DateTime<Utc>"
               FROM workspace_spawn_reservations
               WHERE workspace_id = $1"#,
            workspace_id
        )
        .fetch_optional(pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;
    use crate::models::workspace::{CreateWorkspace, Workspace};

    #[tokio::test]
    async fn reservation_fence_and_ttl_prevent_overlapping_spawns() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::run_migrations_for_tests(&pool).await.unwrap();
        let workspace_id = Uuid::new_v4();
        Workspace::create(
            &pool,
            &CreateWorkspace {
                branch: "main".to_string(),
                name: Some("reservation fixture".to_string()),
            },
            workspace_id,
        )
        .await
        .unwrap();
        let now = Utc::now();
        let first = WorkspaceSpawnReservation::acquire_at(
            &pool,
            workspace_id,
            SpawnReservationHolder::Executor,
            now,
            Duration::seconds(15),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            WorkspaceSpawnReservation::acquire_at(
                &pool,
                workspace_id,
                SpawnReservationHolder::Cli,
                now + Duration::seconds(14),
                Duration::seconds(15),
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            !WorkspaceSpawnReservation::release(&pool, workspace_id, "wrong-fence")
                .await
                .unwrap()
        );

        let second = WorkspaceSpawnReservation::acquire_at(
            &pool,
            workspace_id,
            SpawnReservationHolder::Cli,
            now + Duration::seconds(16),
            Duration::seconds(15),
        )
        .await
        .unwrap()
        .expect("expired reservation must be replaced");
        assert_ne!(first.fence, second.fence);
        assert_eq!(second.holder, SpawnReservationHolder::Cli);
        assert!(
            !WorkspaceSpawnReservation::release(&pool, workspace_id, &first.fence)
                .await
                .unwrap()
        );
        assert!(
            WorkspaceSpawnReservation::release(&pool, workspace_id, &second.fence)
                .await
                .unwrap()
        );
    }
}
