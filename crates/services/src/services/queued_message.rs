use chrono::{DateTime, Utc};
use db::{
    DBService,
    models::{
        scratch::DraftFollowUpData,
        session_queued_message::{QueuedMessageSource, QueuedMessageState, SessionQueuedMessage},
    },
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ts_rs::TS;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum QueuedMessageError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("queued message {0} has no executor configuration")]
    MissingExecutorConfig(Uuid),
    #[error("queued message {id} has invalid executor configuration: {source}")]
    InvalidExecutorConfig {
        id: Uuid,
        #[source]
        source: serde_json::Error,
    },
    #[error("queued message {0} lost its executor claim before consumption")]
    ExecutorClaimLost(Uuid),
}

/// Durable frontend-facing representation of the session's collaboration
/// slot. The `data` field preserves the existing queue API shape while source
/// and delivery state expose the explicit Phase 2 contract.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct QueuedMessage {
    pub id: Uuid,
    pub session_id: Uuid,
    pub data: DraftFollowUpData,
    pub source: QueuedMessageSource,
    pub state: QueuedMessageState,
    pub failure_reason: Option<String>,
    pub claude_session_id: Option<String>,
    pub pasted_at: Option<DateTime<Utc>>,
    pub acked_at: Option<DateTime<Utc>>,
    pub queued_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TryFrom<SessionQueuedMessage> for QueuedMessage {
    type Error = QueuedMessageError;

    fn try_from(row: SessionQueuedMessage) -> Result<Self, Self::Error> {
        let Some(raw_config) = row.executor_config.as_deref() else {
            return Err(QueuedMessageError::MissingExecutorConfig(row.id));
        };
        let executor_config = serde_json::from_str(raw_config)
            .map_err(|source| QueuedMessageError::InvalidExecutorConfig { id: row.id, source })?;
        Ok(Self {
            id: row.id,
            session_id: row.session_id,
            data: DraftFollowUpData {
                message: row.prompt,
                executor_config,
            },
            source: row.source,
            state: row.state,
            failure_reason: row.failure_reason,
            claude_session_id: row.claude_session_id,
            pasted_at: row.pasted_at,
            acked_at: row.acked_at,
            queued_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum QueueStatus {
    Empty,
    Queued { message: QueuedMessage },
    Pasting { message: QueuedMessage },
    Pasted { message: QueuedMessage },
}

impl QueueStatus {
    pub fn from_message(message: QueuedMessage) -> Self {
        match message.state {
            QueuedMessageState::Queued => Self::Queued { message },
            QueuedMessageState::Pasting => Self::Pasting { message },
            QueuedMessageState::Pasted => Self::Pasted { message },
            QueuedMessageState::Imported
            | QueuedMessageState::Failed
            | QueuedMessageState::Consumed
            | QueuedMessageState::Cancelled => Self::Empty,
        }
    }

    #[cfg(test)]
    pub fn message(&self) -> Option<&QueuedMessage> {
        match self {
            Self::Empty => None,
            Self::Queued { message } | Self::Pasting { message } | Self::Pasted { message } => {
                Some(message)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum QueueMutation {
    Stored(QueueStatus),
    Conflict(QueueStatus),
}

/// DB-backed single-slot queue. There is deliberately no process-local copy:
/// restarts, paste acknowledgement, routes, and executor finalization all read
/// the same SQLite state machine.
#[derive(Clone)]
pub struct QueuedMessageService {
    db: DBService,
}

impl QueuedMessageService {
    pub fn new(db: DBService) -> Self {
        Self { db }
    }

    pub async fn get_queued(
        &self,
        session_id: Uuid,
    ) -> Result<Option<QueuedMessage>, QueuedMessageError> {
        SessionQueuedMessage::find_active(&self.db.pool, session_id)
            .await?
            .map(TryInto::try_into)
            .transpose()
    }

    /// Transitional consumer for callers not yet moved to `CliCollabService`.
    /// It atomically claims and consumes the durable row before returning it;
    /// Phase 2 finalization uses `on_executor_finished` instead.
    pub async fn take_queued(
        &self,
        session_id: Uuid,
    ) -> Result<Option<QueuedMessage>, QueuedMessageError> {
        let Some(row) = SessionQueuedMessage::find_active(&self.db.pool, session_id).await? else {
            return Ok(None);
        };
        if row.state != QueuedMessageState::Queued
            || SessionQueuedMessage::claim_for_executor(
                &self.db.pool,
                row.id,
                "queued-message-service",
            )
            .await?
            .is_none()
        {
            return Ok(None);
        }
        if !SessionQueuedMessage::mark_consumed(&self.db.pool, row.id, "queued-message-service")
            .await?
        {
            return Err(QueuedMessageError::ExecutorClaimLost(row.id));
        }
        Ok(Some(row.try_into()?))
    }

    pub async fn has_queued(&self, session_id: Uuid) -> Result<bool, QueuedMessageError> {
        Ok(SessionQueuedMessage::find_active(&self.db.pool, session_id)
            .await?
            .is_some())
    }

    pub async fn get_status(&self, session_id: Uuid) -> Result<QueueStatus, QueuedMessageError> {
        Ok(match self.get_queued(session_id).await? {
            Some(message) => QueueStatus::from_message(message),
            None => QueueStatus::Empty,
        })
    }
}
