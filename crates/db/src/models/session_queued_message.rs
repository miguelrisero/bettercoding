use chrono::{DateTime, Duration, Utc};
use executors::profile::ExecutorConfig;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool, Type};
use ts_rs::TS;
use uuid::Uuid;

pub const PASTED_REQUEUE_FAILURE_REASON: &str =
    "CLI submission was not acknowledged; queued for retry";

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
    #[serde(skip)]
    #[ts(skip)]
    pub executor_claim_owner: Option<String>,
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
        failure_reason, claude_session_id, executor_claim_owner, pasted_at, acked_at,
        created_at, updated_at
    "#;

    pub fn parsed_executor_config(&self) -> Result<Option<ExecutorConfig>, serde_json::Error> {
        self.executor_config
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
    }

    pub fn was_requeued_from_pasted(&self) -> bool {
        self.state == QueuedMessageState::Queued
            && self.pasted_at.is_some()
            && self.failure_reason.as_deref() == Some(PASTED_REQUEUE_FAILURE_REASON)
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
        if replace {
            let sql = format!(
                r#"UPDATE session_queued_messages SET
                       prompt = $1,
                       executor_config = $2,
                       source = $3,
                       failure_reason = NULL,
                       claude_session_id = NULL,
                       executor_claim_owner = NULL,
                       pasted_at = NULL,
                       acked_at = NULL,
                       updated_at = datetime('now', 'subsec')
                   WHERE session_id = $4 AND state = 'queued'
                   RETURNING {}"#,
                Self::SELECT_FIELDS
            );
            if let Some(row) = sqlx::query_as::<_, Self>(&sql)
                .bind(prompt)
                .bind(executor_config)
                .bind(source)
                .bind(session_id)
                .fetch_optional(pool)
                .await?
            {
                return Ok(StoreQueuedMessageResult::Stored(row));
            }
        }

        if let Some(existing) = Self::find_active(pool, session_id).await? {
            return Ok(StoreQueuedMessageResult::Conflict(existing));
        }

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
        .execute(pool)
        .await?;
        Self::find_by_id(pool, id)
            .await?
            .map(StoreQueuedMessageResult::Stored)
            .ok_or(sqlx::Error::RowNotFound)
    }

    async fn claim(
        pool: &SqlitePool,
        id: Uuid,
        claude_session_id: Option<&str>,
        executor_claim_owner: Option<&str>,
    ) -> Result<Option<Self>, sqlx::Error> {
        let result = sqlx::query!(
            r#"UPDATE session_queued_messages SET
                   state = 'pasting',
                   claude_session_id = $1,
                   executor_claim_owner = $2,
                   pasted_at = CASE WHEN $2 IS NULL
                                    THEN datetime('now', 'subsec')
                                    ELSE NULL END,
                   failure_reason = NULL,
                   updated_at = datetime('now', 'subsec')
               WHERE id = $3 AND state = 'queued'"#,
            claude_session_id,
            executor_claim_owner,
            id
        )
        .execute(pool)
        .await?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        Self::find_by_id(pool, id).await
    }

    /// Claim a queued row for terminal paste delivery.
    pub async fn claim_for_paste(
        pool: &SqlitePool,
        id: Uuid,
        claude_session_id: Option<&str>,
    ) -> Result<Option<Self>, sqlx::Error> {
        Self::claim(pool, id, claude_session_id, None).await
    }

    /// Claim a queued row for executor delivery. The process-scoped owner is
    /// retained until consumption so reconciliation cannot steal a live claim.
    pub async fn claim_for_executor(
        pool: &SqlitePool,
        id: Uuid,
        owner: &str,
    ) -> Result<Option<Self>, sqlx::Error> {
        Self::claim(pool, id, None, Some(owner)).await
    }

    pub async fn mark_pasted(pool: &SqlitePool, id: Uuid) -> Result<bool, sqlx::Error> {
        let result = sqlx::query!(
            r#"UPDATE session_queued_messages SET
                   state = 'pasted',
                   updated_at = datetime('now', 'subsec')
               WHERE id = $1 AND state = 'pasting'
                 AND executor_claim_owner IS NULL"#,
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
                   executor_claim_owner = NULL,
                   claude_session_id = CASE
                       WHEN state = 'pasted' THEN claude_session_id
                       ELSE NULL
                   END,
                   pasted_at = CASE
                       WHEN state = 'pasted' THEN pasted_at
                       ELSE NULL
                   END,
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

    /// Return a paste with missing delivery evidence to the persisted slot.
    /// The original timestamp and sid stay attached so a delayed native user
    /// record can still acknowledge this exact delivery.
    pub async fn requeue_pasted(pool: &SqlitePool, id: Uuid) -> Result<bool, sqlx::Error> {
        Self::requeue(pool, id, Some(PASTED_REQUEUE_FAILURE_REASON)).await
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

    pub async fn mark_consumed(
        pool: &SqlitePool,
        id: Uuid,
        owner: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query!(
            r#"UPDATE session_queued_messages SET
                   state = 'consumed',
                   updated_at = datetime('now', 'subsec')
               WHERE id = $1 AND state = 'pasting'
                 AND executor_claim_owner = $2"#,
            id,
            owner
        )
        .execute(pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn cancel_queued(
        pool: &SqlitePool,
        session_id: Uuid,
    ) -> Result<CancelQueuedMessageResult, sqlx::Error> {
        let sql = format!(
            r#"UPDATE session_queued_messages SET
                   state = 'cancelled',
                   updated_at = datetime('now', 'subsec')
               WHERE session_id = $1 AND state = 'queued'
               RETURNING {}"#,
            Self::SELECT_FIELDS
        );
        if let Some(cancelled) = sqlx::query_as::<_, Self>(&sql)
            .bind(session_id)
            .fetch_optional(pool)
            .await?
        {
            return Ok(CancelQueuedMessageResult::Cancelled(cancelled));
        }
        Ok(match Self::find_active(pool, session_id).await? {
            Some(active) => CancelQueuedMessageResult::Conflict(active),
            None => CancelQueuedMessageResult::Empty,
        })
    }

    /// Reconcile delivery rows after startup or a periodic drain. Ack evidence
    /// wins before age-based recovery, so a crash after the import transaction
    /// never causes a blind re-paste.
    pub async fn reconcile(
        pool: &SqlitePool,
        now: DateTime<Utc>,
        pasting_grace: Duration,
        paste_ack_timeout: Duration,
        active_executor_claim_owner: Option<&str>,
    ) -> Result<QueueReconciliation, sqlx::Error> {
        let imported = sqlx::query!(
            r#"UPDATE session_queued_messages SET
                   state = 'imported',
                   acked_at = COALESCE(acked_at, datetime('now', 'subsec')),
                   updated_at = datetime('now', 'subsec')
               WHERE (
                     state IN ('pasting', 'pasted')
                     OR (
                         state = 'queued'
                         AND failure_reason = $1
                         AND pasted_at IS NOT NULL
                     )
                 )
                 AND EXISTS (
                     SELECT 1 FROM cli_native_records record
                     WHERE record.bound_queued_message_id = session_queued_messages.id
                 )"#,
            PASTED_REQUEUE_FAILURE_REASON
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
                   executor_claim_owner = NULL,
                   pasted_at = NULL,
                   updated_at = datetime('now', 'subsec')
               WHERE state = 'pasting'
                 AND (
                     (executor_claim_owner IS NULL
                      AND julianday(updated_at) <= julianday($1))
                     OR (executor_claim_owner IS NOT NULL
                         AND ($2 IS NULL OR executor_claim_owner != $2))
                 )"#,
            pasting_cutoff,
            active_executor_claim_owner
        )
        .execute(pool)
        .await?
        .rows_affected();

        let pasted_cutoff = now - paste_ack_timeout;
        let requeued_pasted = sqlx::query!(
            r#"UPDATE session_queued_messages SET
                   state = 'queued',
                   failure_reason = $2,
                   updated_at = datetime('now', 'subsec')
               WHERE state = 'pasted'
                 AND julianday(pasted_at) <= julianday($1)"#,
            pasted_cutoff,
            PASTED_REQUEUE_FAILURE_REASON
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

        SessionQueuedMessage::claim_for_paste(&pool, first.id, Some("sid"))
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
    async fn replace_losing_a_queued_slot_reports_conflict_instead_of_success() {
        let (pool, session_id) = session_fixture().await;
        SessionQueuedMessage::store(
            &pool,
            session_id,
            "original",
            None,
            QueuedMessageSource::Ui,
            false,
        )
        .await
        .unwrap();
        sqlx::query(
            r#"CREATE TRIGGER simulate_replace_claim_race
               BEFORE UPDATE OF prompt ON session_queued_messages
               WHEN OLD.state = 'queued'
               BEGIN
                   UPDATE session_queued_messages SET state = 'pasting'
                   WHERE id = OLD.id;
                   SELECT RAISE(IGNORE);
               END"#,
        )
        .execute(&pool)
        .await
        .unwrap();

        let result = SessionQueuedMessage::store(
            &pool,
            session_id,
            "replacement",
            None,
            QueuedMessageSource::Recovery,
            true,
        )
        .await
        .unwrap();
        assert!(matches!(
            result,
            StoreQueuedMessageResult::Conflict(ref row)
                if row.prompt == "original" && row.state == QueuedMessageState::Pasting
        ));
    }

    #[tokio::test]
    async fn cancel_losing_a_claim_race_does_not_report_false_success() {
        let (pool, session_id) = session_fixture().await;
        SessionQueuedMessage::store(
            &pool,
            session_id,
            "claim wins",
            None,
            QueuedMessageSource::Ui,
            false,
        )
        .await
        .unwrap();
        sqlx::query(
            r#"CREATE TRIGGER simulate_cancel_claim_race
               BEFORE UPDATE OF state ON session_queued_messages
               WHEN OLD.state = 'queued' AND NEW.state = 'cancelled'
               BEGIN
                   UPDATE session_queued_messages SET state = 'pasting'
                   WHERE id = OLD.id;
                   SELECT RAISE(IGNORE);
               END"#,
        )
        .execute(&pool)
        .await
        .unwrap();

        let result = SessionQueuedMessage::cancel_queued(&pool, session_id)
            .await
            .unwrap();
        assert!(matches!(
            result,
            CancelQueuedMessageResult::Conflict(ref row)
                if row.state == QueuedMessageState::Pasting
        ));
    }

    #[tokio::test]
    async fn delivery_reconciliation_recovers_pasting_but_only_hard_caps_pasted_rows() {
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
        SessionQueuedMessage::claim_for_paste(&pool, row.id, Some("sid"))
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
            None,
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

        SessionQueuedMessage::claim_for_paste(&pool, row.id, Some("sid"))
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
            Duration::minutes(15),
            None,
        )
        .await
        .unwrap();
        assert_eq!(recovered.requeued_pasted, 0);
        assert_eq!(
            SessionQueuedMessage::find_by_id(&pool, row.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            QueuedMessageState::Pasted
        );

        let hard_capped_at = Utc::now() - Duration::minutes(16);
        sqlx::query("UPDATE session_queued_messages SET pasted_at = ? WHERE id = ?")
            .bind(hard_capped_at)
            .bind(row.id)
            .execute(&pool)
            .await
            .unwrap();
        let recovered = SessionQueuedMessage::reconcile(
            &pool,
            Utc::now(),
            Duration::seconds(5),
            Duration::minutes(15),
            None,
        )
        .await
        .unwrap();
        assert_eq!(recovered.requeued_pasted, 1);
        let active = SessionQueuedMessage::find_active(&pool, session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(active.state, QueuedMessageState::Queued);
        assert!(active.was_requeued_from_pasted());
        assert_eq!(active.pasted_at, Some(hard_capped_at));
        assert_eq!(active.claude_session_id.as_deref(), Some("sid"));
        assert_eq!(
            active.failure_reason.as_deref(),
            Some(PASTED_REQUEUE_FAILURE_REASON)
        );
    }

    #[tokio::test]
    async fn reconciliation_preserves_live_executor_claim_and_recovers_crashed_owner() {
        let (pool, session_id) = session_fixture().await;
        let row = match SessionQueuedMessage::store(
            &pool,
            session_id,
            "executor claim",
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
        SessionQueuedMessage::claim_for_executor(&pool, row.id, "runtime-a")
            .await
            .unwrap()
            .unwrap();
        let old = Utc::now() - Duration::seconds(10);
        sqlx::query("UPDATE session_queued_messages SET updated_at = ? WHERE id = ?")
            .bind(old)
            .bind(row.id)
            .execute(&pool)
            .await
            .unwrap();

        let live = SessionQueuedMessage::reconcile(
            &pool,
            Utc::now(),
            Duration::seconds(5),
            Duration::minutes(15),
            Some("runtime-a"),
        )
        .await
        .unwrap();
        assert_eq!(live.requeued_pasting, 0);

        let crashed = SessionQueuedMessage::reconcile(
            &pool,
            Utc::now(),
            Duration::seconds(5),
            Duration::minutes(15),
            Some("runtime-b"),
        )
        .await
        .unwrap();
        assert_eq!(crashed.requeued_pasting, 1);
        assert_eq!(
            SessionQueuedMessage::find_by_id(&pool, row.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            QueuedMessageState::Queued
        );
    }
}
