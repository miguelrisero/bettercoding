use chrono::{DateTime, Duration, Utc};
use executors::profile::ExecutorConfig;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool, Type};
use ts_rs::TS;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type, TS)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum QueuedMessageSource {
    Ui,
    Recovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type, TS)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum QueuedMessageState {
    Queued,
    Pasting,
    Pasted,
    Imported,
    Failed,
    Consumed,
    Cancelled,
}

impl QueuedMessageState {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Pasting | Self::Pasted)
    }
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize, TS)]
pub struct SessionQueuedMessage {
    pub id: Uuid,
    pub session_id: Uuid,
    pub prompt: String,
    /// Serialized [`ExecutorConfig`]. Paste delivery does not need it, but a
    /// queued prompt must retain it across a restart before executor dispatch.
    #[ts(type = "ExecutorConfig | null")]
    pub executor_config: Option<String>,
    pub source: QueuedMessageSource,
    pub state: QueuedMessageState,
    pub failure_reason: Option<String>,
    pub claude_session_id: Option<String>,
    pub pasted_at: Option<DateTime<Utc>>,
    pub acked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub enum StoreQueuedMessageResult {
    Stored(SessionQueuedMessage),
    Conflict(SessionQueuedMessage),
}

#[derive(Debug, Clone)]
pub enum CancelQueuedMessageResult {
    Empty,
    Cancelled(SessionQueuedMessage),
    Conflict(SessionQueuedMessage),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueReconciliation {
    pub imported: u64,
    pub requeued_pasting: u64,
    pub requeued_pasted: u64,
}

impl SessionQueuedMessage {
    const SELECT_FIELDS: &'static str = r#"
        id, session_id, prompt, executor_config, source, state,
        failure_reason, claude_session_id, pasted_at, acked_at,
        created_at, updated_at
    "#;

    pub fn parsed_executor_config(&self) -> Result<Option<ExecutorConfig>, serde_json::Error> {
        self.executor_config
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
    }

    pub async fn find_by_id(pool: &SqlitePool, id: Uuid) -> Result<Option<Self>, sqlx::Error> {
        let sql = format!(
            "SELECT {} FROM session_queued_messages WHERE id = ?",
            Self::SELECT_FIELDS
        );
        sqlx::query_as::<_, Self>(&sql)
            .bind(id)
            .fetch_optional(pool)
            .await
    }

    pub async fn find_active(
        pool: &SqlitePool,
        session_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        let sql = format!(
            "SELECT {} FROM session_queued_messages \
             WHERE session_id = ? AND state IN ('queued', 'pasting', 'pasted') \
             ORDER BY created_at DESC LIMIT 1",
            Self::SELECT_FIELDS
        );
        sqlx::query_as::<_, Self>(&sql)
            .bind(session_id)
            .fetch_optional(pool)
            .await
    }

    pub async fn list_active(pool: &SqlitePool) -> Result<Vec<Self>, sqlx::Error> {
        let sql = format!(
            "SELECT {} FROM session_queued_messages \
             WHERE state IN ('queued', 'pasting', 'pasted') \
             ORDER BY created_at ASC",
            Self::SELECT_FIELDS
        );
        sqlx::query_as::<_, Self>(&sql).fetch_all(pool).await
    }

    /// Store the session's active slot. Replacement is deliberately limited
    /// to `queued`; paste-in-flight rows always conflict.
    pub async fn store(
        pool: &SqlitePool,
        session_id: Uuid,
        prompt: &str,
        executor_config: Option<&str>,
        source: QueuedMessageSource,
        replace: bool,
    ) -> Result<StoreQueuedMessageResult, sqlx::Error> {
        let mut tx = pool.begin().await?;
        let sql = format!(
            "SELECT {} FROM session_queued_messages \
             WHERE session_id = ? AND state IN ('queued', 'pasting', 'pasted') \
             ORDER BY created_at DESC LIMIT 1",
            Self::SELECT_FIELDS
        );
        let existing = sqlx::query_as::<_, Self>(&sql)
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await?;

        let id = if let Some(existing) = existing {
            if !replace || existing.state != QueuedMessageState::Queued {
                tx.rollback().await?;
                return Ok(StoreQueuedMessageResult::Conflict(existing));
            }
            sqlx::query!(
                r#"UPDATE session_queued_messages SET
                       prompt = $1,
                       executor_config = $2,
                       source = $3,
                       failure_reason = NULL,
                       claude_session_id = NULL,
                       pasted_at = NULL,
                       acked_at = NULL,
                       updated_at = datetime('now', 'subsec')
                   WHERE id = $4 AND state = 'queued'"#,
                prompt,
                executor_config,
                source,
                existing.id
            )
            .execute(&mut *tx)
            .await?;
            existing.id
        } else {
            let id = Uuid::new_v4();
            sqlx::query!(
                r#"INSERT INTO session_queued_messages
                       (id, session_id, prompt, executor_config, source, state)
                   VALUES ($1, $2, $3, $4, $5, 'queued')"#,
                id,
                session_id,
                prompt,
                executor_config,
                source
            )
            .execute(&mut *tx)
            .await?;
            id
        };
        tx.commit().await?;
        Ok(StoreQueuedMessageResult::Stored(
            Self::find_by_id(pool, id)
                .await?
                .expect("stored queue row exists"),
        ))
    }

    /// Claim a queued row for either paste or executor delivery. `pasting` is
    /// the crash-recoverable in-flight state for both; a NULL sid distinguishes
    /// an executor claim from a terminal paste claim.
    pub async fn claim(
        pool: &SqlitePool,
        id: Uuid,
        claude_session_id: Option<&str>,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query!(
            r#"UPDATE session_queued_messages SET
                   state = 'pasting',
                   claude_session_id = $1,
                   pasted_at = datetime('now', 'subsec'),
                   failure_reason = NULL,
                   updated_at = datetime('now', 'subsec')
               WHERE id = $2 AND state = 'queued'"#,
            claude_session_id,
            id
        )
        .execute(pool)
        .await?;
        let row = Self::find_by_id(pool, id).await?;
        Ok(row.filter(|row| row.state == QueuedMessageState::Pasting))
    }

    pub async fn mark_pasted(pool: &SqlitePool, id: Uuid) -> Result<bool, sqlx::Error> {
        let result = sqlx::query!(
            r#"UPDATE session_queued_messages SET
                   state = 'pasted',
                   updated_at = datetime('now', 'subsec')
               WHERE id = $1 AND state = 'pasting'"#,
            id
        )
        .execute(pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn requeue(
        pool: &SqlitePool,
        id: Uuid,
        failure_reason: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query!(
            r#"UPDATE session_queued_messages SET
                   state = 'queued',
                   failure_reason = $1,
                   claude_session_id = NULL,
                   pasted_at = NULL,
                   acked_at = NULL,
                   updated_at = datetime('now', 'subsec')
               WHERE id = $2 AND state IN ('pasting', 'pasted')"#,
            failure_reason,
            id
        )
        .execute(pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn set_failure_reason(
        pool: &SqlitePool,
        id: Uuid,
        failure_reason: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query!(
            r#"UPDATE session_queued_messages SET
                   failure_reason = $1,
                   updated_at = datetime('now', 'subsec')
               WHERE id = $2 AND state = 'queued'"#,
            failure_reason,
            id
        )
        .execute(pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn mark_consumed(pool: &SqlitePool, id: Uuid) -> Result<bool, sqlx::Error> {
        let result = sqlx::query!(
            r#"UPDATE session_queued_messages SET
                   state = 'consumed',
                   updated_at = datetime('now', 'subsec')
               WHERE id = $1 AND state = 'pasting'"#,
            id
        )
        .execute(pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn cancel_queued(
        pool: &SqlitePool,
        session_id: Uuid,
    ) -> Result<CancelQueuedMessageResult, sqlx::Error> {
        let Some(active) = Self::find_active(pool, session_id).await? else {
            return Ok(CancelQueuedMessageResult::Empty);
        };
        if active.state != QueuedMessageState::Queued {
            return Ok(CancelQueuedMessageResult::Conflict(active));
        }
        sqlx::query!(
            r#"UPDATE session_queued_messages SET
                   state = 'cancelled',
                   updated_at = datetime('now', 'subsec')
               WHERE id = $1 AND state = 'queued'"#,
            active.id
        )
        .execute(pool)
        .await?;
        Ok(CancelQueuedMessageResult::Cancelled(
            Self::find_by_id(pool, active.id)
                .await?
                .expect("cancelled queue row exists"),
        ))
    }

    /// Reconcile delivery rows after startup or a periodic drain. Ack evidence
    /// wins before age-based recovery, so a crash after the import transaction
    /// never causes a blind re-paste.
    pub async fn reconcile(
        pool: &SqlitePool,
        now: DateTime<Utc>,
        pasting_grace: Duration,
        paste_ack_timeout: Duration,
    ) -> Result<QueueReconciliation, sqlx::Error> {
        let imported = sqlx::query!(
            r#"UPDATE session_queued_messages SET
                   state = 'imported',
                   acked_at = COALESCE(acked_at, datetime('now', 'subsec')),
                   updated_at = datetime('now', 'subsec')
               WHERE state IN ('pasting', 'pasted')
                 AND EXISTS (
                     SELECT 1 FROM cli_native_records record
                     WHERE record.bound_queued_message_id = session_queued_messages.id
                 )"#
        )
        .execute(pool)
        .await?
        .rows_affected();

        let pasting_cutoff = now - pasting_grace;
        let requeued_pasting = sqlx::query!(
            r#"UPDATE session_queued_messages SET
                   state = 'queued',
                   failure_reason = 'delivery interrupted; queued for retry',
                   claude_session_id = NULL,
                   pasted_at = NULL,
                   updated_at = datetime('now', 'subsec')
               WHERE state = 'pasting' AND updated_at <= $1"#,
            pasting_cutoff
        )
        .execute(pool)
        .await?
        .rows_affected();

        let pasted_cutoff = now - paste_ack_timeout;
        let requeued_pasted = sqlx::query!(
            r#"UPDATE session_queued_messages SET
                   state = 'queued',
                   failure_reason = 'CLI submission was not acknowledged; queued for retry',
                   claude_session_id = NULL,
                   pasted_at = NULL,
                   updated_at = datetime('now', 'subsec')
               WHERE state = 'pasted' AND pasted_at <= $1"#,
            pasted_cutoff
        )
        .execute(pool)
        .await?
        .rows_affected();

        Ok(QueueReconciliation {
            imported,
            requeued_pasting,
            requeued_pasted,
        })
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

    async fn session_fixture() -> (SqlitePool, Uuid) {
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
                name: Some("queue fixture".to_string()),
            },
            workspace_id,
        )
        .await
        .unwrap();
        let session_id = Uuid::new_v4();
        Session::create(
            &pool,
            &CreateSession {
                executor: Some("CLAUDE_CODE".to_string()),
                name: None,
            },
            session_id,
            workspace_id,
        )
        .await
        .unwrap();
        (pool, session_id)
    }

    #[tokio::test]
    async fn active_slot_requires_explicit_queued_only_replacement() {
        let (pool, session_id) = session_fixture().await;
        let first = match SessionQueuedMessage::store(
            &pool,
            session_id,
            "first",
            None,
            QueuedMessageSource::Ui,
            false,
        )
        .await
        .unwrap()
        {
            StoreQueuedMessageResult::Stored(row) => row,
            StoreQueuedMessageResult::Conflict(_) => panic!("empty slot must store"),
        };

        let conflict = SessionQueuedMessage::store(
            &pool,
            session_id,
            "second",
            None,
            QueuedMessageSource::Recovery,
            false,
        )
        .await
        .unwrap();
        assert!(matches!(
            conflict,
            StoreQueuedMessageResult::Conflict(ref row) if row.prompt == "first"
        ));

        let replaced = SessionQueuedMessage::store(
            &pool,
            session_id,
            "second",
            None,
            QueuedMessageSource::Recovery,
            true,
        )
        .await
        .unwrap();
        assert!(matches!(
            replaced,
            StoreQueuedMessageResult::Stored(ref row)
                if row.id == first.id
                    && row.prompt == "second"
                    && row.source == QueuedMessageSource::Recovery
        ));

        SessionQueuedMessage::claim(&pool, first.id, Some("sid"))
            .await
            .unwrap()
            .unwrap();
        let in_flight = SessionQueuedMessage::store(
            &pool,
            session_id,
            "third",
            None,
            QueuedMessageSource::Ui,
            true,
        )
        .await
        .unwrap();
        assert!(matches!(
            in_flight,
            StoreQueuedMessageResult::Conflict(ref row)
                if row.state == QueuedMessageState::Pasting
        ));
    }

    #[tokio::test]
    async fn delivery_reconciliation_requeues_interrupted_and_unacked_rows() {
        let (pool, session_id) = session_fixture().await;
        let row = match SessionQueuedMessage::store(
            &pool,
            session_id,
            "recover me",
            None,
            QueuedMessageSource::Ui,
            false,
        )
        .await
        .unwrap()
        {
            StoreQueuedMessageResult::Stored(row) => row,
            StoreQueuedMessageResult::Conflict(_) => unreachable!(),
        };
        SessionQueuedMessage::claim(&pool, row.id, Some("sid"))
            .await
            .unwrap();
        let old = Utc::now() - Duration::seconds(10);
        sqlx::query("UPDATE session_queued_messages SET updated_at = ? WHERE id = ?")
            .bind(old)
            .bind(row.id)
            .execute(&pool)
            .await
            .unwrap();
        let recovered = SessionQueuedMessage::reconcile(
            &pool,
            Utc::now(),
            Duration::seconds(5),
            Duration::seconds(30),
        )
        .await
        .unwrap();
        assert_eq!(recovered.requeued_pasting, 1);
        assert_eq!(
            SessionQueuedMessage::find_active(&pool, session_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            QueuedMessageState::Queued
        );

        SessionQueuedMessage::claim(&pool, row.id, Some("sid"))
            .await
            .unwrap();
        SessionQueuedMessage::mark_pasted(&pool, row.id)
            .await
            .unwrap();
        let old = Utc::now() - Duration::seconds(31);
        sqlx::query("UPDATE session_queued_messages SET pasted_at = ? WHERE id = ?")
            .bind(old)
            .bind(row.id)
            .execute(&pool)
            .await
            .unwrap();
        let recovered = SessionQueuedMessage::reconcile(
            &pool,
            Utc::now(),
            Duration::seconds(5),
            Duration::seconds(30),
        )
        .await
        .unwrap();
        assert_eq!(recovered.requeued_pasted, 1);
        let active = SessionQueuedMessage::find_active(&pool, session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(active.state, QueuedMessageState::Queued);
        assert!(active.failure_reason.unwrap().contains("not acknowledged"));
    }
}
