use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::Duration,
};

use anyhow::Result as AnyhowResult;
use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use db::{
    DBService,
    models::{
        claude_session_link::ClaudeSessionLink,
        cli_pane_binding::CliPaneBinding,
        coding_agent_turn::CodingAgentTurn,
        execution_process::{ExecutionProcess, ExecutionProcessRunReason, ExecutionProcessStatus},
        session::Session,
        session_queued_message::{
            QueuedMessageSource, QueuedMessageState, SessionQueuedMessage, StoreQueuedMessageResult,
        },
        workspace::Workspace,
        workspace_spawn_reservation::{SpawnReservationHolder, WorkspaceSpawnReservation},
    },
};
use executors::profile::ExecutorConfig;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;
use ts_rs::TS;
use uuid::Uuid;

use super::{
    claude_transcript_ingest::ClaudeTranscriptIngest,
    queued_message::{QueueMutation, QueueStatus, QueuedMessage, QueuedMessageError},
};

const DRAIN_INTERVAL: Duration = Duration::from_secs(1);
const RESUME_SIGNAL_ATTEMPTS: u32 = 3;
const RESUME_SIGNAL_RETRY_DELAY: Duration = Duration::from_millis(100);
const PASTING_STARTUP_GRACE: ChronoDuration = ChronoDuration::seconds(5);
const PASTE_ACK_HARD_CAP: ChronoDuration = ChronoDuration::minutes(15);
const ABNORMAL_EXECUTOR_QUEUE_HOLD: &str =
    "previous agent failed or was killed; confirm Send again to run this message";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidEvidence {
    ConfirmedResume(String),
    NoResumeArg,
    Ambiguous,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    pub pane_session_exists: bool,
    pub agent_running: Option<bool>,
    pub sid_evidence: SidEvidence,
    /// True when any authoritative tmux/process/DB component of the probe was
    /// unreadable. Lease derivation treats this as busy.
    pub probe_failed: bool,
    /// Used by cli-fresh discovery: `Some(true)` means exactly one live Claude
    /// process has `effective_dir` as its cwd.
    pub only_active_claude_in_cwd: Option<bool>,
}

impl ProbeReport {
    pub fn failed() -> Self {
        Self {
            pane_session_exists: false,
            agent_running: None,
            sid_evidence: SidEvidence::Unknown,
            probe_failed: true,
            only_active_claude_in_cwd: None,
        }
    }
}

#[async_trait]
pub trait CliWriterProbe: Send + Sync {
    async fn probe(
        &self,
        workspace_id: Uuid,
        effective_dir: &Path,
        expected_sid: Option<&str>,
        binding: Option<&CliPaneBinding>,
        check_cwd_uniqueness: bool,
    ) -> ProbeReport;
}

#[async_trait]
pub trait CliPasteTransport: Send + Sync {
    async fn paste_and_submit(&self, workspace_id: Uuid, text: &str) -> bool;
    async fn pane_alive(&self, workspace_id: Uuid) -> AnyhowResult<bool>;
    async fn agent_running(&self, workspace_id: Uuid) -> Option<bool>;
    async fn signal_resume_ready(&self, workspace_id: Uuid, sid: &str) -> AnyhowResult<()>;
}

/// Deployment-owned executor bridge. The trait lives in services so
/// `CliCollabService` never imports local-deployment; the local implementation
/// reuses `ContainerService::start_execution` and its normal bookkeeping.
#[async_trait]
pub trait CliExecutorDispatcher: Send + Sync {
    async fn dispatch(
        &self,
        session: &Session,
        prompt: &str,
        executor_config: &ExecutorConfig,
        retry: Option<&RetryDispatchContext>,
    ) -> AnyhowResult<ExecutionProcess>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryDispatchContext {
    pub process_id: Uuid,
    pub force_when_dirty: bool,
    pub perform_git_reset: bool,
    pub reset_to_message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriterLease {
    Executor,
    Cli { claude_session_id: Option<String> },
    CliAmbiguous,
    Free,
    Busy,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DispatchOutcome {
    Started { execution_process: ExecutionProcess },
    Queued { status: QueueStatus },
    RoutedToCli { delivery: QueueStatus },
    Conflict { status: QueueStatus },
}

#[derive(Debug, Error)]
pub enum CliCollabError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Queue(#[from] QueuedMessageError),
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
    #[error("session {0} has no workspace")]
    WorkspaceMissing(Uuid),
    #[error("queued message {0} lost its executor dispatch claim")]
    ExecutorClaimLost(Uuid),
    #[error("queued message {0} lost its CLI paste claim")]
    PasteClaimLost(Uuid),
    #[error("CLI transport failed for workspace {workspace_id}: {source}")]
    Transport {
        workspace_id: Uuid,
        #[source]
        source: anyhow::Error,
    },
}

pub struct CliCollabService {
    db: DBService,
    probe: Arc<dyn CliWriterProbe>,
    transport: Arc<dyn CliPasteTransport>,
    dispatcher: Arc<dyn CliExecutorDispatcher>,
    ingest: Option<Arc<ClaudeTranscriptIngest>>,
    session_locks: Mutex<HashMap<Uuid, Weak<Mutex<()>>>>,
    resume_signaled_bindings: Mutex<HashSet<Uuid>>,
    executor_claim_owner: String,
    paste_claim_owner: String,
    notify: Notify,
    routing_disabled: bool,
    shutdown: CancellationToken,
}

impl CliCollabService {
    pub fn spawn(
        db: DBService,
        probe: Arc<dyn CliWriterProbe>,
        transport: Arc<dyn CliPasteTransport>,
        dispatcher: Arc<dyn CliExecutorDispatcher>,
        ingest: Option<Arc<ClaudeTranscriptIngest>>,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        let service = Arc::new(Self {
            db,
            probe,
            transport,
            dispatcher,
            ingest,
            session_locks: Mutex::new(HashMap::new()),
            resume_signaled_bindings: Mutex::new(HashSet::new()),
            executor_claim_owner: Uuid::new_v4().to_string(),
            paste_claim_owner: Uuid::new_v4().to_string(),
            notify: Notify::new(),
            routing_disabled: std::env::var_os("DISABLE_CLI_COLLAB_ROUTING").is_some(),
            shutdown,
        });
        tokio::spawn(service.clone().run_drain());
        if let Some(ingest) = &service.ingest {
            let updates = ingest.subscribe();
            tokio::spawn(service.clone().run_ingest_wakeup(updates));
        }
        service
    }

    async fn session_lock(&self, session_id: Uuid) -> Arc<Mutex<()>> {
        let mut locks = self.session_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&session_id).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(session_id, Arc::downgrade(&lock));
        lock
    }

    #[cfg(test)]
    pub async fn derive_lease(&self, session: &Session) -> WriterLease {
        let lock = self.session_lock(session.id).await;
        let _guard = lock.lock().await;
        self.derive_lease_locked(session).await
    }

    async fn derive_lease_locked(&self, session: &Session) -> WriterLease {
        match ExecutionProcess::has_running_coding_agent_for_session(&self.db.pool, session.id)
            .await
        {
            Ok(true) => return WriterLease::Executor,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(?error, session_id = %session.id, "executor lease probe failed closed");
                return WriterLease::Busy;
            }
        }

        let binding = match CliPaneBinding::find_active_for_workspace(
            &self.db.pool,
            session.workspace_id,
        )
        .await
        {
            Ok(binding) => binding,
            Err(error) => {
                tracing::warn!(?error, session_id = %session.id, "pane binding probe failed closed");
                return WriterLease::Busy;
            }
        };
        let workspace = match Workspace::find_by_id(&self.db.pool, session.workspace_id).await {
            Ok(Some(workspace)) => workspace,
            Ok(None) => return WriterLease::Busy,
            Err(error) => {
                tracing::warn!(?error, session_id = %session.id, "workspace lease probe failed closed");
                return WriterLease::Busy;
            }
        };
        let Some(effective_dir) = workspace
            .container_ref
            .as_deref()
            .and_then(|root| session.effective_working_dir(Path::new(root)))
        else {
            return WriterLease::Busy;
        };
        let expected_sid = match self.expected_sid(session.id).await {
            Ok(sid) => sid,
            Err(error) => {
                tracing::warn!(?error, session_id = %session.id, "sid lease probe failed closed");
                return WriterLease::Busy;
            }
        };
        let report = self
            .probe
            .probe(
                session.workspace_id,
                &effective_dir,
                expected_sid.as_deref(),
                binding.as_ref(),
                false,
            )
            .await;
        if report.probe_failed {
            return WriterLease::Busy;
        }
        if !report.pane_session_exists {
            return WriterLease::Free;
        }
        match report.agent_running {
            None => return WriterLease::Busy,
            Some(false) => {
                let handoff_pending = match binding.as_ref() {
                    Some(binding) => self
                        .resume_signaled_bindings
                        .lock()
                        .await
                        .contains(&binding.id),
                    None => false,
                };
                return if handoff_pending {
                    WriterLease::CliAmbiguous
                } else {
                    WriterLease::Free
                };
            }
            Some(true) => {
                if let Some(binding) = binding.as_ref() {
                    self.resume_signaled_bindings
                        .lock()
                        .await
                        .remove(&binding.id);
                }
            }
        }

        let Some(binding) = binding.filter(|binding| binding.session_id == session.id) else {
            return WriterLease::CliAmbiguous;
        };
        match (expected_sid.as_deref(), &report.sid_evidence) {
            (Some(expected), SidEvidence::ConfirmedResume(observed)) if expected == observed => {
                WriterLease::Cli {
                    claude_session_id: Some(expected.to_string()),
                }
            }
            (None, SidEvidence::NoResumeArg) if binding.claude_session_id.is_none() => {
                WriterLease::Cli {
                    claude_session_id: None,
                }
            }
            _ => WriterLease::CliAmbiguous,
        }
    }

    async fn expected_sid(&self, session_id: Uuid) -> Result<Option<String>, sqlx::Error> {
        if let Some(info) =
            CodingAgentTurn::find_latest_session_info(&self.db.pool, session_id).await?
        {
            return Ok(Some(info.session_id));
        }
        Ok(
            ClaudeSessionLink::find_latest_for_session(&self.db.pool, session_id)
                .await?
                .map(|link| link.claude_session_id),
        )
    }

    pub async fn dispatch_gate(
        &self,
        session: &Session,
        prompt: String,
        executor_config: ExecutorConfig,
        source: QueuedMessageSource,
        replace: bool,
    ) -> Result<DispatchOutcome, CliCollabError> {
        self.dispatch_gate_with_context(session, prompt, executor_config, source, replace, None)
            .await
    }

    pub async fn dispatch_retry(
        &self,
        session: &Session,
        prompt: String,
        executor_config: ExecutorConfig,
        source: QueuedMessageSource,
        replace: bool,
        retry: RetryDispatchContext,
    ) -> Result<DispatchOutcome, CliCollabError> {
        self.dispatch_gate_with_context(
            session,
            prompt,
            executor_config,
            source,
            replace,
            Some(retry),
        )
        .await
    }

    async fn dispatch_gate_with_context(
        &self,
        session: &Session,
        prompt: String,
        executor_config: ExecutorConfig,
        source: QueuedMessageSource,
        replace: bool,
        dispatch_context: Option<RetryDispatchContext>,
    ) -> Result<DispatchOutcome, CliCollabError> {
        let lock = self.session_lock(session.id).await;
        let _guard = lock.lock().await;
        let existing = self
            .prepare_existing_slot(
                session.id,
                &prompt,
                &executor_config,
                dispatch_context.as_ref(),
                source,
                replace,
            )
            .await?;
        let existing = match existing {
            PreparedSlot::None => None,
            PreparedSlot::Stored(row) => Some(row),
            PreparedSlot::Conflict(status) => {
                return Ok(DispatchOutcome::Conflict { status });
            }
        };

        let lease = self.derive_lease_locked(session).await;
        let outcome = match lease {
            WriterLease::Executor | WriterLease::CliAmbiguous | WriterLease::Busy => {
                self.queue_or_status(
                    session.id,
                    &prompt,
                    &executor_config,
                    dispatch_context.as_ref(),
                    source,
                    existing,
                )
                .await?
            }
            WriterLease::Cli { .. } if dispatch_context.is_some() => {
                self.queue_or_status(
                    session.id,
                    &prompt,
                    &executor_config,
                    dispatch_context.as_ref(),
                    source,
                    existing,
                )
                .await?
            }
            WriterLease::Cli { claude_session_id } if !self.routing_disabled => {
                let row = match existing {
                    Some(row) => row,
                    None => {
                        self.store_slot(session.id, &prompt, &executor_config, None, source, false)
                            .await?
                    }
                };
                self.paste_slot(session, row, claude_session_id.as_deref())
                    .await?
            }
            WriterLease::Cli { .. } => {
                self.queue_or_status(
                    session.id,
                    &prompt,
                    &executor_config,
                    dispatch_context.as_ref(),
                    source,
                    existing,
                )
                .await?
            }
            WriterLease::Free => {
                self.dispatch_executor_locked(
                    session,
                    &prompt,
                    &executor_config,
                    dispatch_context.as_ref(),
                    source,
                    existing,
                )
                .await?
            }
        };
        if matches!(outcome, DispatchOutcome::Queued { .. }) {
            self.notify.notify_one();
        }
        Ok(outcome)
    }

    pub async fn queue_message(
        &self,
        session: &Session,
        prompt: String,
        executor_config: ExecutorConfig,
        source: QueuedMessageSource,
        replace: bool,
    ) -> Result<QueueMutation, CliCollabError> {
        let lock = self.session_lock(session.id).await;
        let _guard = lock.lock().await;
        let result = self
            .store_slot_result(session.id, &prompt, &executor_config, None, source, replace)
            .await?;
        let outcome = match result {
            StoreQueuedMessageResult::Stored(row) => {
                QueueMutation::Stored(Self::status_from_row(row)?)
            }
            StoreQueuedMessageResult::Conflict(row) => {
                QueueMutation::Conflict(Self::status_from_row(row)?)
            }
        };
        if matches!(outcome, QueueMutation::Stored(_)) {
            self.notify.notify_one();
        }
        Ok(outcome)
    }

    pub async fn cancel_queued(&self, session_id: Uuid) -> Result<QueueMutation, CliCollabError> {
        let lock = self.session_lock(session_id).await;
        let _guard = lock.lock().await;
        match db::models::session_queued_message::SessionQueuedMessage::cancel_queued(
            &self.db.pool,
            session_id,
        )
        .await?
        {
            db::models::session_queued_message::CancelQueuedMessageResult::Empty
            | db::models::session_queued_message::CancelQueuedMessageResult::Cancelled(_) => {
                Ok(QueueMutation::Stored(QueueStatus::Empty))
            }
            db::models::session_queued_message::CancelQueuedMessageResult::Conflict(row) => {
                Ok(QueueMutation::Conflict(Self::status_from_row(row)?))
            }
        }
    }

    async fn status(&self, session_id: Uuid) -> Result<QueueStatus, CliCollabError> {
        match SessionQueuedMessage::find_active(&self.db.pool, session_id).await? {
            Some(row) => Self::status_from_row(row),
            None => Ok(QueueStatus::Empty),
        }
    }

    async fn prepare_existing_slot(
        &self,
        session_id: Uuid,
        prompt: &str,
        executor_config: &ExecutorConfig,
        dispatch_context: Option<&RetryDispatchContext>,
        source: QueuedMessageSource,
        replace: bool,
    ) -> Result<PreparedSlot, CliCollabError> {
        let Some(active) = SessionQueuedMessage::find_active(&self.db.pool, session_id).await?
        else {
            return Ok(PreparedSlot::None);
        };
        if !replace || active.state != QueuedMessageState::Queued {
            return Ok(PreparedSlot::Conflict(Self::status_from_row(active)?));
        }
        Ok(PreparedSlot::Stored(
            self.store_slot(
                session_id,
                prompt,
                executor_config,
                dispatch_context,
                source,
                true,
            )
            .await?,
        ))
    }

    async fn queue_or_status(
        &self,
        session_id: Uuid,
        prompt: &str,
        executor_config: &ExecutorConfig,
        dispatch_context: Option<&RetryDispatchContext>,
        source: QueuedMessageSource,
        existing: Option<SessionQueuedMessage>,
    ) -> Result<DispatchOutcome, CliCollabError> {
        let row = match existing {
            Some(row) => row,
            None => {
                self.store_slot(
                    session_id,
                    prompt,
                    executor_config,
                    dispatch_context,
                    source,
                    false,
                )
                .await?
            }
        };
        Ok(DispatchOutcome::Queued {
            status: Self::status_from_row(row)?,
        })
    }

    async fn store_slot(
        &self,
        session_id: Uuid,
        prompt: &str,
        executor_config: &ExecutorConfig,
        dispatch_context: Option<&RetryDispatchContext>,
        source: QueuedMessageSource,
        replace: bool,
    ) -> Result<SessionQueuedMessage, CliCollabError> {
        match self
            .store_slot_result(
                session_id,
                prompt,
                executor_config,
                dispatch_context,
                source,
                replace,
            )
            .await?
        {
            StoreQueuedMessageResult::Stored(row) | StoreQueuedMessageResult::Conflict(row) => {
                Ok(row)
            }
        }
    }

    async fn store_slot_result(
        &self,
        session_id: Uuid,
        prompt: &str,
        executor_config: &ExecutorConfig,
        dispatch_context: Option<&RetryDispatchContext>,
        source: QueuedMessageSource,
        replace: bool,
    ) -> Result<StoreQueuedMessageResult, CliCollabError> {
        let config = serde_json::to_string(executor_config)?;
        let dispatch_context = dispatch_context.map(serde_json::to_string).transpose()?;
        Ok(SessionQueuedMessage::store_with_context(
            &self.db.pool,
            session_id,
            prompt,
            Some(&config),
            dispatch_context.as_deref(),
            source,
            replace,
        )
        .await?)
    }

    async fn paste_slot(
        &self,
        session: &Session,
        row: SessionQueuedMessage,
        claude_session_id: Option<&str>,
    ) -> Result<DispatchOutcome, CliCollabError> {
        let Some(claimed) = SessionQueuedMessage::claim_for_paste(
            &self.db.pool,
            row.id,
            claude_session_id,
            &self.paste_claim_owner,
        )
        .await?
        else {
            return Ok(DispatchOutcome::Conflict {
                status: self.status(session.id).await?,
            });
        };
        if self
            .transport
            .paste_and_submit(session.workspace_id, &claimed.prompt)
            .await
        {
            if !SessionQueuedMessage::mark_pasted(
                &self.db.pool,
                claimed.id,
                &self.paste_claim_owner,
            )
            .await?
            {
                return Err(CliCollabError::PasteClaimLost(claimed.id));
            }
            let status = self.status(session.id).await?;
            Ok(DispatchOutcome::RoutedToCli { delivery: status })
        } else {
            SessionQueuedMessage::requeue(
                &self.db.pool,
                claimed.id,
                Some("CLI paste failed; queued for retry"),
            )
            .await?;
            Ok(DispatchOutcome::Queued {
                status: self.status(session.id).await?,
            })
        }
    }

    async fn dispatch_executor_locked(
        &self,
        session: &Session,
        prompt: &str,
        executor_config: &ExecutorConfig,
        dispatch_context: Option<&RetryDispatchContext>,
        source: QueuedMessageSource,
        existing: Option<SessionQueuedMessage>,
    ) -> Result<DispatchOutcome, CliCollabError> {
        let reservation = match WorkspaceSpawnReservation::acquire(
            &self.db.pool,
            session.workspace_id,
            SpawnReservationHolder::Executor,
        )
        .await
        {
            Ok(Some(reservation)) => reservation,
            Ok(None) => {
                return self
                    .queue_or_status(
                        session.id,
                        prompt,
                        executor_config,
                        dispatch_context,
                        source,
                        existing,
                    )
                    .await;
            }
            Err(error) => {
                tracing::warn!(?error, session_id = %session.id, "spawn reservation failed closed");
                return self
                    .queue_or_status(
                        session.id,
                        prompt,
                        executor_config,
                        dispatch_context,
                        source,
                        existing,
                    )
                    .await;
            }
        };

        let claimed = if let Some(row) = existing {
            match SessionQueuedMessage::claim_for_executor(
                &self.db.pool,
                row.id,
                &self.executor_claim_owner,
            )
            .await?
            {
                Some(row) => Some(row),
                None => {
                    let _ = WorkspaceSpawnReservation::release(
                        &self.db.pool,
                        session.workspace_id,
                        &reservation.fence,
                    )
                    .await;
                    return Ok(DispatchOutcome::Conflict {
                        status: self.status(session.id).await?,
                    });
                }
            }
        } else {
            None
        };

        let dispatched = self
            .dispatcher
            .dispatch(session, prompt, executor_config, dispatch_context)
            .await;
        if let Err(error) = WorkspaceSpawnReservation::release(
            &self.db.pool,
            session.workspace_id,
            &reservation.fence,
        )
        .await
        {
            tracing::warn!(?error, workspace_id = %session.workspace_id, "failed to release executor spawn reservation");
        }

        match dispatched {
            Ok(execution_process) => {
                if let Some(row) = claimed
                    && !SessionQueuedMessage::mark_consumed(
                        &self.db.pool,
                        row.id,
                        &self.executor_claim_owner,
                    )
                    .await?
                {
                    return Err(CliCollabError::ExecutorClaimLost(row.id));
                }
                Ok(DispatchOutcome::Started { execution_process })
            }
            Err(error) => {
                tracing::warn!(?error, session_id = %session.id, "executor dispatch failed; preserving prompt in queue");
                if let Some(row) = claimed {
                    SessionQueuedMessage::requeue(
                        &self.db.pool,
                        row.id,
                        Some("executor start failed; queued for retry"),
                    )
                    .await?;
                    Ok(DispatchOutcome::Queued {
                        status: self.status(session.id).await?,
                    })
                } else {
                    self.queue_or_status(
                        session.id,
                        prompt,
                        executor_config,
                        dispatch_context,
                        source,
                        None,
                    )
                    .await
                }
            }
        }
    }

    fn status_from_row(row: SessionQueuedMessage) -> Result<QueueStatus, CliCollabError> {
        Ok(QueueStatus::from_message(QueuedMessage::try_from(row)?))
    }

    pub async fn on_executor_finished(
        &self,
        session_id: Uuid,
        finished_status: ExecutionProcessStatus,
        releases_collaboration_writer: bool,
    ) -> Result<bool, CliCollabError> {
        let lock = self.session_lock(session_id).await;
        let guard = lock.lock().await;
        if !releases_collaboration_writer {
            return Ok(false);
        }
        if matches!(
            finished_status,
            ExecutionProcessStatus::Failed | ExecutionProcessStatus::Killed
        ) {
            if let Some(row) = SessionQueuedMessage::find_active(&self.db.pool, session_id).await?
                && row.state == QueuedMessageState::Queued
            {
                self.hold_after_abnormal_writer(&row).await?;
            }
            self.notify.notify_one();
            return Ok(false);
        }
        let session = Session::find_by_id(&self.db.pool, session_id)
            .await?
            .ok_or(CliCollabError::WorkspaceMissing(session_id))?;
        match ExecutionProcess::has_running_coding_agent_for_session(&self.db.pool, session_id)
            .await
        {
            Ok(true) => {
                tracing::debug!(
                    %session_id,
                    "CLI resume handoff deferred while another executor is running"
                );
                return Ok(false);
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(?error, %session_id, "finish hook executor probe failed closed");
                return Err(error.into());
            }
        }
        if let Some(binding) =
            CliPaneBinding::find_active_for_session(&self.db.pool, session_id).await?
            && self
                .hold_or_resume_bound_pane_locked(&session, binding)
                .await?
        {
            // Give the 1 s bootstrap poll a chance to exec the resumed TUI;
            // the durable queue remains untouched during this transition.
            self.notify.notify_one();
            return Ok(false);
        }
        drop(guard);
        let started = self.drain_session(session_id).await?;
        self.notify.notify_one();
        Ok(started)
    }

    async fn hold_after_abnormal_writer(
        &self,
        row: &SessionQueuedMessage,
    ) -> Result<bool, CliCollabError> {
        if row.failure_reason.as_deref() == Some(ABNORMAL_EXECUTOR_QUEUE_HOLD) {
            return Ok(true);
        }
        if !ExecutionProcess::has_abnormal_coding_agent_completion_after(
            &self.db.pool,
            row.session_id,
            row.updated_at,
        )
        .await?
        {
            return Ok(false);
        }
        Ok(SessionQueuedMessage::set_failure_reason(
            &self.db.pool,
            row.id,
            ABNORMAL_EXECUTOR_QUEUE_HOLD,
        )
        .await?)
    }

    async fn hold_or_resume_bound_pane_locked(
        &self,
        session: &Session,
        binding: CliPaneBinding,
    ) -> Result<bool, CliCollabError> {
        let pane_alive = self
            .transport
            .pane_alive(session.workspace_id)
            .await
            .map_err(|source| CliCollabError::Transport {
                workspace_id: session.workspace_id,
                source,
            })?;
        if !pane_alive {
            CliPaneBinding::release(&self.db.pool, binding.id).await?;
            self.resume_signaled_bindings
                .lock()
                .await
                .remove(&binding.id);
            return Ok(false);
        }
        match self.transport.agent_running(session.workspace_id).await {
            Some(true) => {
                self.resume_signaled_bindings
                    .lock()
                    .await
                    .remove(&binding.id);
                return Ok(true);
            }
            None => {
                tracing::warn!(
                    session_id = %session.id,
                    workspace_id = %session.workspace_id,
                    "busy CLI pane agent probe failed closed; resume handoff will retry"
                );
                return Ok(true);
            }
            Some(false) => {}
        }
        if self
            .resume_signaled_bindings
            .lock()
            .await
            .contains(&binding.id)
        {
            return Ok(true);
        }

        let sid = match binding.claude_session_id.as_ref() {
            Some(sid) => Some(sid.clone()),
            None => self.expected_sid(session.id).await?,
        };
        let Some(sid) = sid else {
            tracing::warn!(
                session_id = %session.id,
                workspace_id = %session.workspace_id,
                "busy CLI pane has no resume sid yet; resume handoff will retry"
            );
            return Ok(true);
        };
        if binding.bound_via == db::models::cli_pane_binding::CliPaneBoundVia::CliFresh
            && binding.claude_session_id.is_none()
            && !CliPaneBinding::bind_discovered_sid(&self.db.pool, binding.id, &sid).await?
        {
            tracing::warn!(
                session_id = %session.id,
                binding_id = %binding.id,
                "busy CLI pane sid binding changed before resume handoff; retrying from fresh state"
            );
            return Ok(true);
        }

        self.signal_resume_ready_with_retry(session.workspace_id, &sid)
            .await?;
        self.resume_signaled_bindings
            .lock()
            .await
            .insert(binding.id);
        tracing::info!(
            session_id = %session.id,
            workspace_id = %session.workspace_id,
            binding_id = %binding.id,
            claude_session_id = %sid,
            "signaled busy CLI pane to resume"
        );
        Ok(true)
    }

    async fn signal_resume_ready_with_retry(
        &self,
        workspace_id: Uuid,
        sid: &str,
    ) -> Result<(), CliCollabError> {
        let mut last_error = None;
        for attempt in 1..=RESUME_SIGNAL_ATTEMPTS {
            match self.transport.signal_resume_ready(workspace_id, sid).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        %workspace_id,
                        attempt,
                        max_attempts = RESUME_SIGNAL_ATTEMPTS,
                        "failed to write CLI resume-ready signal"
                    );
                    last_error = Some(error);
                    if attempt < RESUME_SIGNAL_ATTEMPTS {
                        tokio::time::sleep(RESUME_SIGNAL_RETRY_DELAY).await;
                    }
                }
            }
        }
        let source = last_error.expect("resume signaling always makes at least one attempt");
        tracing::error!(
            ?source,
            %workspace_id,
            attempts = RESUME_SIGNAL_ATTEMPTS,
            "CLI resume-ready signal exhausted retries; busy pane remains blocked"
        );
        Err(CliCollabError::Transport {
            workspace_id,
            source,
        })
    }

    async fn drain_session(&self, session_id: Uuid) -> Result<bool, CliCollabError> {
        if self.routing_disabled {
            return Ok(false);
        }
        let lock = self.session_lock(session_id).await;
        let _guard = lock.lock().await;
        let Some(row) = SessionQueuedMessage::find_active(&self.db.pool, session_id).await? else {
            return Ok(false);
        };
        if row.state != QueuedMessageState::Queued {
            return Ok(false);
        }
        if self.hold_after_abnormal_writer(&row).await? {
            return Ok(false);
        }
        let session = Session::find_by_id(&self.db.pool, session_id)
            .await?
            .ok_or(CliCollabError::WorkspaceMissing(session_id))?;
        let executor_config = match row.parsed_executor_config() {
            Ok(Some(config)) => config,
            Ok(None) => {
                SessionQueuedMessage::set_failure_reason(
                    &self.db.pool,
                    row.id,
                    "queued message has no executor configuration",
                )
                .await?;
                return Ok(false);
            }
            Err(error) => {
                SessionQueuedMessage::set_failure_reason(
                    &self.db.pool,
                    row.id,
                    "queued message has invalid executor configuration",
                )
                .await?;
                tracing::warn!(?error, queue_id = %row.id, "invalid durable queue executor config");
                return Ok(false);
            }
        };
        let dispatch_context = match row
            .dispatch_context
            .as_deref()
            .map(serde_json::from_str::<RetryDispatchContext>)
            .transpose()
        {
            Ok(context) => context,
            Err(error) => {
                SessionQueuedMessage::set_failure_reason(
                    &self.db.pool,
                    row.id,
                    "queued message has invalid dispatch context",
                )
                .await?;
                tracing::warn!(?error, queue_id = %row.id, "invalid durable queue dispatch context");
                return Ok(false);
            }
        };
        match self.derive_lease_locked(&session).await {
            WriterLease::Executor | WriterLease::CliAmbiguous | WriterLease::Busy => Ok(false),
            WriterLease::Cli { .. } if dispatch_context.is_some() => Ok(false),
            WriterLease::Cli { claude_session_id } => {
                if self.original_paste_binding_is_active(&row).await? {
                    return Ok(false);
                }
                let routed = self
                    .paste_slot(&session, row, claude_session_id.as_deref())
                    .await?;
                Ok(matches!(routed, DispatchOutcome::RoutedToCli { .. }))
            }
            WriterLease::Free => {
                let prompt = row.prompt.clone();
                let source = row.source;
                let outcome = self
                    .dispatch_executor_locked(
                        &session,
                        &prompt,
                        &executor_config,
                        dispatch_context.as_ref(),
                        source,
                        Some(row),
                    )
                    .await?;
                Ok(matches!(outcome, DispatchOutcome::Started { .. }))
            }
        }
    }

    async fn original_paste_binding_is_active(
        &self,
        row: &SessionQueuedMessage,
    ) -> Result<bool, CliCollabError> {
        if !row.was_requeued_from_pasted() {
            return Ok(false);
        }
        let Some(pasted_at) = row.pasted_at else {
            return Ok(false);
        };
        Ok(
            CliPaneBinding::find_active_for_session(&self.db.pool, row.session_id)
                .await?
                .is_some_and(|binding| binding.created_at <= pasted_at),
        )
    }

    async fn reconcile_pasted_delivery(
        &self,
        row: SessionQueuedMessage,
    ) -> Result<bool, CliCollabError> {
        let lock = self.session_lock(row.session_id).await;
        let _guard = lock.lock().await;
        let Some(row) = SessionQueuedMessage::find_by_id(&self.db.pool, row.id).await? else {
            return Ok(false);
        };
        if row.state != QueuedMessageState::Pasted {
            return Ok(false);
        }
        let session = Session::find_by_id(&self.db.pool, row.session_id)
            .await?
            .ok_or(CliCollabError::WorkspaceMissing(row.session_id))?;
        let binding =
            CliPaneBinding::find_active_for_session(&self.db.pool, row.session_id).await?;
        let Some(binding) = binding else {
            return Ok(SessionQueuedMessage::requeue_pasted(&self.db.pool, row.id).await?);
        };
        if row
            .pasted_at
            .is_some_and(|pasted_at| binding.created_at > pasted_at)
        {
            return Ok(SessionQueuedMessage::requeue_pasted(&self.db.pool, row.id).await?);
        }
        let workspace = Workspace::find_by_id(&self.db.pool, session.workspace_id)
            .await?
            .ok_or(CliCollabError::WorkspaceMissing(row.session_id))?;
        let Some(effective_dir) = workspace
            .container_ref
            .as_deref()
            .and_then(|root| session.effective_working_dir(Path::new(root)))
        else {
            return Ok(false);
        };
        let report = self
            .probe
            .probe(
                session.workspace_id,
                &effective_dir,
                row.claude_session_id.as_deref(),
                Some(&binding),
                false,
            )
            .await;
        if report.probe_failed || report.agent_running.is_none() {
            return Ok(false);
        }
        if !report.pane_session_exists {
            CliPaneBinding::release(&self.db.pool, binding.id).await?;
        }
        if !report.pane_session_exists || report.agent_running == Some(false) {
            return Ok(SessionQueuedMessage::requeue_pasted(&self.db.pool, row.id).await?);
        }
        Ok(false)
    }

    async fn reconcile_delivery_state(&self, pasted_rows: Vec<SessionQueuedMessage>) {
        let reconciled = match SessionQueuedMessage::reconcile(
            &self.db.pool,
            Utc::now(),
            PASTING_STARTUP_GRACE,
            PASTE_ACK_HARD_CAP,
            Some(&self.executor_claim_owner),
            Some(&self.paste_claim_owner),
        )
        .await
        {
            Ok(reconciled) => reconciled,
            Err(error) => {
                tracing::warn!(?error, "failed to reconcile CLI delivery state");
                return;
            }
        };
        let mut requeued_lost_evidence = 0_u64;
        for row in pasted_rows {
            let queue_id = row.id;
            let session_id = row.session_id;
            match self.reconcile_pasted_delivery(row).await {
                Ok(true) => requeued_lost_evidence += 1,
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    ?error,
                    %queue_id,
                    %session_id,
                    "failed closed while reconciling CLI delivery evidence"
                ),
            }
        }
        if reconciled.imported > 0
            || reconciled.requeued_pasting > 0
            || reconciled.requeued_pasted > 0
            || requeued_lost_evidence > 0
        {
            tracing::info!(
                imported = reconciled.imported,
                requeued_pasting = reconciled.requeued_pasting,
                requeued_pasted = reconciled.requeued_pasted,
                requeued_lost_evidence,
                "reconciled durable CLI delivery state"
            );
        }
    }

    #[cfg(test)]
    async fn reconcile_delivery_state_from_db(&self) {
        let pasted_rows = SessionQueuedMessage::list_active(&self.db.pool)
            .await
            .unwrap()
            .into_iter()
            .filter(|row| row.state == QueuedMessageState::Pasted)
            .collect();
        self.reconcile_delivery_state(pasted_rows).await;
    }

    async fn reconcile_waiting_panes(&self, bindings: Vec<CliPaneBinding>) {
        let active_ids: HashSet<_> = bindings.iter().map(|binding| binding.id).collect();
        self.resume_signaled_bindings
            .lock()
            .await
            .retain(|id| active_ids.contains(id));

        for observed_binding in bindings {
            let session_id = observed_binding.session_id;
            let lock = self.session_lock(session_id).await;
            let _guard = lock.lock().await;
            let binding =
                match CliPaneBinding::find_active_for_session(&self.db.pool, session_id).await {
                    Ok(Some(binding)) if binding.id == observed_binding.id => binding,
                    Ok(_) => continue,
                    Err(error) => {
                        tracing::warn!(
                            ?error,
                            %session_id,
                            "failed to refresh CLI pane binding during resume reconciliation"
                        );
                        continue;
                    }
                };
            match ExecutionProcess::has_running_coding_agent_for_session(&self.db.pool, session_id)
                .await
            {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        %session_id,
                        "resume reconciliation executor probe failed closed"
                    );
                    continue;
                }
            }
            let processes =
                match ExecutionProcess::find_by_session_id(&self.db.pool, session_id, true).await {
                    Ok(processes) => processes,
                    Err(error) => {
                        tracing::warn!(
                            ?error,
                            %session_id,
                            "failed to load executor release evidence for CLI resume"
                        );
                        continue;
                    }
                };
            let writer_released_after_binding = processes
                .iter()
                .any(|process| process.writer_released_after(binding.created_at));
            if !writer_released_after_binding {
                continue;
            }
            let session = match Session::find_by_id(&self.db.pool, session_id).await {
                Ok(Some(session)) => session,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        %session_id,
                        "failed to load session during CLI resume reconciliation"
                    );
                    continue;
                }
            };
            if let Err(error) = self
                .hold_or_resume_bound_pane_locked(&session, binding)
                .await
            {
                tracing::error!(
                    ?error,
                    %session_id,
                    "failed to reconcile busy CLI resume handoff; will retry"
                );
            }
        }
    }

    async fn drain_all(&self) {
        let bindings = match CliPaneBinding::list_active(&self.db.pool).await {
            Ok(bindings) => bindings,
            Err(error) => {
                tracing::warn!(?error, "failed to scan active CLI pane bindings");
                return;
            }
        };
        let active_rows = match SessionQueuedMessage::list_active(&self.db.pool).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(?error, "durable CLI queue scan failed closed");
                return;
            }
        };
        if bindings.is_empty() && active_rows.is_empty() {
            self.resume_signaled_bindings.lock().await.clear();
            return;
        }

        let mut pasted_rows = Vec::new();
        let mut queued_session_ids = Vec::new();
        for row in active_rows {
            match row.state {
                QueuedMessageState::Pasted => pasted_rows.push(row),
                QueuedMessageState::Queued => queued_session_ids.push(row.session_id),
                QueuedMessageState::Pasting => {}
                QueuedMessageState::Imported
                | QueuedMessageState::Failed
                | QueuedMessageState::Consumed
                | QueuedMessageState::Cancelled => {
                    debug_assert!(false, "active queue scan returned a terminal row");
                }
            }
        }

        self.reconcile_waiting_panes(bindings).await;
        self.reconcile_delivery_state(pasted_rows).await;
        if self.routing_disabled {
            return;
        }
        for session_id in queued_session_ids {
            if let Err(error) = self.drain_session(session_id).await {
                tracing::warn!(?error, %session_id, "durable CLI queue drain failed");
            }
        }
    }

    async fn run_drain(self: Arc<Self>) {
        self.drain_all().await;
        let mut interval = tokio::time::interval(DRAIN_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = interval.tick() => self.drain_all().await,
                _ = self.notify.notified() => self.drain_all().await,
            }
        }
    }

    async fn run_ingest_wakeup(
        self: Arc<Self>,
        mut updates: tokio::sync::broadcast::Receiver<
            super::claude_transcript_ingest::NativeFeedUpdate,
        >,
    ) {
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                update = updates.recv() => match update {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        self.notify.notify_one();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

enum PreparedSlot {
    None,
    Stored(SessionQueuedMessage),
    Conflict(QueueStatus),
}

/// Build the executor's effective cwd without importing deployment code.
pub async fn effective_dir_for_session(
    db: &DBService,
    session: &Session,
) -> Result<Option<PathBuf>, sqlx::Error> {
    let Some(workspace) = Workspace::find_by_id(&db.pool, session.workspace_id).await? else {
        return Ok(None);
    };
    Ok(workspace
        .container_ref
        .as_deref()
        .and_then(|root| session.effective_working_dir(Path::new(root))))
}

/// Kept here so local dispatcher implementations and server validation use the
/// same run reason without restating its storage spelling.
pub const COLLAB_RUN_REASON: ExecutionProcessRunReason = ExecutionProcessRunReason::CodingAgent;

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    };

    use db::models::{
        claude_session_link::ClaudeSessionLink,
        cli_native_file::{CliNativeFile, RegisterCliNativeFile},
        cli_pane_binding::{CliPaneBinding, CliPaneBoundVia},
        execution_process::{
            CreateExecutionProcess, ExecutionProcessRunReason, ExecutionProcessStatus,
        },
        session::CreateSession,
        workspace::CreateWorkspace,
    };
    use executors::{
        actions::{
            ExecutorAction, ExecutorActionType, coding_agent_initial::CodingAgentInitialRequest,
        },
        executors::BaseCodingAgent,
    };
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;

    #[derive(Clone)]
    struct FakeProbe(Arc<StdMutex<ProbeReport>>);

    #[async_trait]
    impl CliWriterProbe for FakeProbe {
        async fn probe(
            &self,
            _workspace_id: Uuid,
            _effective_dir: &Path,
            _expected_sid: Option<&str>,
            _binding: Option<&CliPaneBinding>,
            _check_cwd_uniqueness: bool,
        ) -> ProbeReport {
            self.0.lock().unwrap().clone()
        }
    }

    struct FakeTransport;

    #[async_trait]
    impl CliPasteTransport for FakeTransport {
        async fn paste_and_submit(&self, _workspace_id: Uuid, _text: &str) -> bool {
            true
        }

        async fn pane_alive(&self, _workspace_id: Uuid) -> AnyhowResult<bool> {
            Ok(true)
        }

        async fn agent_running(&self, _workspace_id: Uuid) -> Option<bool> {
            Some(true)
        }

        async fn signal_resume_ready(&self, _workspace_id: Uuid, _sid: &str) -> AnyhowResult<()> {
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct TransportState {
        pasted_prompts: Vec<String>,
        observed_states: Vec<QueuedMessageState>,
    }

    struct RecordingTransport {
        db: DBService,
        session_id: Uuid,
        state: Arc<StdMutex<TransportState>>,
        paste_succeeds: bool,
    }

    #[async_trait]
    impl CliPasteTransport for RecordingTransport {
        async fn paste_and_submit(&self, _workspace_id: Uuid, text: &str) -> bool {
            let state = SessionQueuedMessage::find_active(&self.db.pool, self.session_id)
                .await
                .unwrap()
                .map(|row| row.state)
                .expect("paste transport must observe an active delivery row");
            let mut observations = self.state.lock().unwrap();
            observations.pasted_prompts.push(text.to_string());
            observations.observed_states.push(state);
            self.paste_succeeds
        }

        async fn pane_alive(&self, _workspace_id: Uuid) -> AnyhowResult<bool> {
            Ok(true)
        }

        async fn agent_running(&self, _workspace_id: Uuid) -> Option<bool> {
            Some(true)
        }

        async fn signal_resume_ready(&self, _workspace_id: Uuid, _sid: &str) -> AnyhowResult<()> {
            Ok(())
        }
    }

    struct SlowPasteTransport {
        calls: Arc<AtomicUsize>,
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl CliPasteTransport for SlowPasteTransport {
        async fn paste_and_submit(&self, _workspace_id: Uuid, _text: &str) -> bool {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                self.started.notify_one();
                self.release.notified().await;
            }
            true
        }

        async fn pane_alive(&self, _workspace_id: Uuid) -> AnyhowResult<bool> {
            Ok(true)
        }

        async fn agent_running(&self, _workspace_id: Uuid) -> Option<bool> {
            Some(true)
        }

        async fn signal_resume_ready(&self, _workspace_id: Uuid, _sid: &str) -> AnyhowResult<()> {
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct ResumeTransportState {
        signals: Vec<(Uuid, String)>,
        failures_remaining: u32,
    }

    struct ResumeRecordingTransport {
        state: Arc<StdMutex<ResumeTransportState>>,
    }

    #[async_trait]
    impl CliPasteTransport for ResumeRecordingTransport {
        async fn paste_and_submit(&self, _workspace_id: Uuid, _text: &str) -> bool {
            true
        }

        async fn pane_alive(&self, _workspace_id: Uuid) -> AnyhowResult<bool> {
            Ok(true)
        }

        async fn agent_running(&self, _workspace_id: Uuid) -> Option<bool> {
            Some(false)
        }

        async fn signal_resume_ready(&self, workspace_id: Uuid, sid: &str) -> AnyhowResult<()> {
            let mut state = self.state.lock().unwrap();
            state.signals.push((workspace_id, sid.to_string()));
            if state.failures_remaining > 0 {
                state.failures_remaining -= 1;
                anyhow::bail!("intentional resume signal failure");
            }
            Ok(())
        }
    }

    struct NeverDispatcher;

    #[async_trait]
    impl CliExecutorDispatcher for NeverDispatcher {
        async fn dispatch(
            &self,
            _session: &Session,
            _prompt: &str,
            _executor_config: &ExecutorConfig,
            _retry: Option<&RetryDispatchContext>,
        ) -> AnyhowResult<ExecutionProcess> {
            panic!("lease-only tests must not dispatch")
        }
    }

    #[derive(Debug, Default)]
    struct DispatcherState {
        prompts: Vec<String>,
        retry_contexts: Vec<Option<RetryDispatchContext>>,
        reservation_seen: bool,
    }

    struct RecordingDispatcher {
        db: DBService,
        state: Arc<StdMutex<DispatcherState>>,
    }

    #[async_trait]
    impl CliExecutorDispatcher for RecordingDispatcher {
        async fn dispatch(
            &self,
            session: &Session,
            prompt: &str,
            _executor_config: &ExecutorConfig,
            retry: Option<&RetryDispatchContext>,
        ) -> AnyhowResult<ExecutionProcess> {
            let reservation_seen =
                WorkspaceSpawnReservation::find(&self.db.pool, session.workspace_id)
                    .await?
                    .is_some();
            {
                let mut state = self.state.lock().unwrap();
                state.prompts.push(prompt.to_string());
                state.retry_contexts.push(retry.cloned());
                state.reservation_seen = reservation_seen;
            }
            Ok(create_running_executor(&self.db, session.id, prompt).await)
        }
    }

    struct FailingDispatcher {
        db: DBService,
        state: Arc<StdMutex<DispatcherState>>,
    }

    struct SlowDispatcher {
        db: DBService,
        calls: Arc<AtomicUsize>,
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl CliExecutorDispatcher for SlowDispatcher {
        async fn dispatch(
            &self,
            session: &Session,
            prompt: &str,
            _executor_config: &ExecutorConfig,
            _retry: Option<&RetryDispatchContext>,
        ) -> AnyhowResult<ExecutionProcess> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            self.release.notified().await;
            Ok(create_running_executor(&self.db, session.id, prompt).await)
        }
    }

    #[async_trait]
    impl CliExecutorDispatcher for FailingDispatcher {
        async fn dispatch(
            &self,
            session: &Session,
            prompt: &str,
            _executor_config: &ExecutorConfig,
            _retry: Option<&RetryDispatchContext>,
        ) -> AnyhowResult<ExecutionProcess> {
            let reservation_seen =
                WorkspaceSpawnReservation::find(&self.db.pool, session.workspace_id)
                    .await?
                    .is_some();
            let mut state = self.state.lock().unwrap();
            state.prompts.push(prompt.to_string());
            state.reservation_seen = reservation_seen;
            Err(anyhow::anyhow!("intentional dispatcher failure"))
        }
    }

    async fn fixture() -> (DBService, Workspace, Session) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        db::run_migrations_for_tests(&pool).await.unwrap();
        let db = DBService { pool };
        let workspace_id = Uuid::new_v4();
        Workspace::create(
            &db.pool,
            &CreateWorkspace {
                branch: "main".to_string(),
                name: Some("collaboration fixture".to_string()),
            },
            workspace_id,
        )
        .await
        .unwrap();
        Workspace::update_container_ref(&db.pool, workspace_id, "/tmp/collab-fixture")
            .await
            .unwrap();
        let workspace = Workspace::find_by_id(&db.pool, workspace_id)
            .await
            .unwrap()
            .unwrap();
        let session = Session::create(
            &db.pool,
            &CreateSession {
                executor: Some("CLAUDE_CODE".to_string()),
                name: None,
            },
            Uuid::new_v4(),
            workspace_id,
        )
        .await
        .unwrap();
        (db, workspace, session)
    }

    fn service_with_components(
        db: DBService,
        report: ProbeReport,
        transport: Arc<dyn CliPasteTransport>,
        dispatcher: Arc<dyn CliExecutorDispatcher>,
    ) -> (CliCollabService, FakeProbe) {
        let probe = FakeProbe(Arc::new(StdMutex::new(report)));
        (
            CliCollabService {
                db,
                probe: Arc::new(probe.clone()),
                transport,
                dispatcher,
                ingest: None,
                session_locks: Mutex::new(HashMap::new()),
                resume_signaled_bindings: Mutex::new(HashSet::new()),
                executor_claim_owner: Uuid::new_v4().to_string(),
                paste_claim_owner: Uuid::new_v4().to_string(),
                notify: Notify::new(),
                routing_disabled: false,
                shutdown: CancellationToken::new(),
            },
            probe,
        )
    }

    fn service(db: DBService, report: ProbeReport) -> (CliCollabService, FakeProbe) {
        service_with_components(
            db,
            report,
            Arc::new(FakeTransport),
            Arc::new(NeverDispatcher),
        )
    }

    fn report(
        pane_session_exists: bool,
        agent_running: Option<bool>,
        sid_evidence: SidEvidence,
    ) -> ProbeReport {
        ProbeReport {
            pane_session_exists,
            agent_running,
            sid_evidence,
            probe_failed: false,
            only_active_claude_in_cwd: None,
        }
    }

    fn executor_config() -> ExecutorConfig {
        ExecutorConfig::new(BaseCodingAgent::ClaudeCode)
    }

    async fn create_running_executor(
        db: &DBService,
        session_id: Uuid,
        prompt: &str,
    ) -> ExecutionProcess {
        let action = ExecutorAction::new(
            ExecutorActionType::CodingAgentInitialRequest(CodingAgentInitialRequest {
                prompt: prompt.to_string(),
                executor_config: executor_config(),
                working_dir: None,
            }),
            None,
        );
        ExecutionProcess::create(
            &db.pool,
            &CreateExecutionProcess {
                session_id,
                executor_action: action,
                run_reason: ExecutionProcessRunReason::CodingAgent,
            },
            Uuid::new_v4(),
            &[],
        )
        .await
        .unwrap()
    }

    async fn store_message(
        db: &DBService,
        session_id: Uuid,
        prompt: &str,
        source: QueuedMessageSource,
    ) -> SessionQueuedMessage {
        let serialized = serde_json::to_string(&executor_config()).unwrap();
        match SessionQueuedMessage::store(
            &db.pool,
            session_id,
            prompt,
            Some(&serialized),
            source,
            false,
        )
        .await
        .unwrap()
        {
            StoreQueuedMessageResult::Stored(row) => row,
            StoreQueuedMessageResult::Conflict(_) => panic!("fixture slot must be empty"),
        }
    }

    async fn bind_confirmed_cli(
        db: &DBService,
        workspace: &Workspace,
        session: &Session,
        sid: &str,
    ) -> CliPaneBinding {
        let binding = CliPaneBinding::record_launch(
            &db.pool,
            workspace.id,
            session.id,
            Some(sid),
            CliPaneBoundVia::CliResume,
        )
        .await
        .unwrap();
        ClaudeSessionLink::assign_manual(&db.pool, sid, session.id, "/tmp/collab-fixture")
            .await
            .unwrap()
            .unwrap();
        binding
    }

    #[tokio::test]
    async fn lease_derivation_covers_executor_confirmed_cli_ambiguous_and_free() {
        let (db, workspace, session) = fixture().await;
        let (service, probe) =
            service(db.clone(), report(false, Some(false), SidEvidence::Unknown));
        assert_eq!(service.derive_lease(&session).await, WriterLease::Free);

        CliPaneBinding::record_launch(
            &db.pool,
            workspace.id,
            session.id,
            Some("11111111-1111-4111-8111-111111111111"),
            CliPaneBoundVia::CliResume,
        )
        .await
        .unwrap();
        ClaudeSessionLink::assign_manual(
            &db.pool,
            "11111111-1111-4111-8111-111111111111",
            session.id,
            "/tmp/collab-fixture",
        )
        .await
        .unwrap()
        .unwrap();
        *probe.0.lock().unwrap() = report(
            true,
            Some(true),
            SidEvidence::ConfirmedResume("11111111-1111-4111-8111-111111111111".to_string()),
        );
        assert_eq!(
            service.derive_lease(&session).await,
            WriterLease::Cli {
                claude_session_id: Some("11111111-1111-4111-8111-111111111111".to_string())
            }
        );

        *probe.0.lock().unwrap() = report(true, Some(true), SidEvidence::Ambiguous);
        assert_eq!(
            service.derive_lease(&session).await,
            WriterLease::CliAmbiguous
        );

        create_running_executor(&db, session.id, "running").await;
        assert_eq!(service.derive_lease(&session).await, WriterLease::Executor);
    }

    #[tokio::test]
    async fn stale_database_sid_never_overrides_live_resume_evidence() {
        let (db, workspace, session) = fixture().await;
        let expected = "11111111-1111-4111-8111-111111111111";
        bind_confirmed_cli(&db, &workspace, &session, expected).await;
        let (service, probe) = service(
            db,
            report(
                true,
                Some(true),
                SidEvidence::ConfirmedResume("22222222-2222-4222-8222-222222222222".to_string()),
            ),
        );

        assert_eq!(
            service.derive_lease(&session).await,
            WriterLease::CliAmbiguous
        );

        *probe.0.lock().unwrap() = report(true, Some(true), SidEvidence::NoResumeArg);
        assert_eq!(
            service.derive_lease(&session).await,
            WriterLease::CliAmbiguous
        );
    }

    #[tokio::test]
    async fn lease_derivation_fails_closed_on_database_and_probe_errors() {
        let (db, _workspace, session) = fixture().await;
        let (service, probe) =
            service(db.clone(), report(false, Some(false), SidEvidence::Unknown));

        *probe.0.lock().unwrap() = ProbeReport::failed();
        assert_eq!(service.derive_lease(&session).await, WriterLease::Busy);

        db.pool.close().await;
        assert_eq!(service.derive_lease(&session).await, WriterLease::Busy);
    }

    #[tokio::test]
    async fn dispatch_gate_queues_while_executor_is_running() {
        let (db, _workspace, session) = fixture().await;
        create_running_executor(&db, session.id, "already running").await;
        let transport_state = Arc::new(StdMutex::new(TransportState::default()));
        let transport = Arc::new(RecordingTransport {
            db: db.clone(),
            session_id: session.id,
            state: transport_state.clone(),
            paste_succeeds: true,
        });
        let (service, _) = service_with_components(
            db.clone(),
            report(false, Some(false), SidEvidence::Unknown),
            transport,
            Arc::new(NeverDispatcher),
        );

        let outcome = service
            .dispatch_gate(
                &session,
                "wait for executor".to_string(),
                executor_config(),
                QueuedMessageSource::Ui,
                false,
            )
            .await
            .unwrap();
        let DispatchOutcome::Queued { status } = outcome else {
            panic!("running executor must queue the prompt");
        };
        let message = status.message().unwrap();
        assert_eq!(message.data.message, "wait for executor");
        assert_eq!(message.state, QueuedMessageState::Queued);
        assert!(transport_state.lock().unwrap().pasted_prompts.is_empty());
    }

    #[tokio::test]
    async fn finish_hook_waits_for_auto_drained_executor_before_resuming_busy_pane() {
        let (db, workspace, session) = fixture().await;
        let sid = "24242424-2424-4242-8242-242424242424";
        let first = create_running_executor(&db, session.id, "blocking writer").await;
        bind_confirmed_cli(&db, &workspace, &session, sid).await;
        ExecutionProcess::update_completion(
            &db.pool,
            first.id,
            ExecutionProcessStatus::Completed,
            Some(0),
        )
        .await
        .unwrap();
        let second = create_running_executor(&db, session.id, "auto-drained follow-up").await;
        let transport_state = Arc::new(StdMutex::new(ResumeTransportState::default()));
        let transport = Arc::new(ResumeRecordingTransport {
            state: transport_state.clone(),
        });
        let (service, probe) = service_with_components(
            db.clone(),
            report(false, Some(false), SidEvidence::Unknown),
            transport,
            Arc::new(NeverDispatcher),
        );

        assert!(
            !service
                .on_executor_finished(session.id, ExecutionProcessStatus::Completed, true)
                .await
                .unwrap()
        );
        assert!(
            transport_state.lock().unwrap().signals.is_empty(),
            "a prior finish hook must not resume the pane over a newer executor"
        );

        ExecutionProcess::update_completion(
            &db.pool,
            second.id,
            ExecutionProcessStatus::Completed,
            Some(0),
        )
        .await
        .unwrap();
        assert!(
            !service
                .on_executor_finished(session.id, ExecutionProcessStatus::Completed, true)
                .await
                .unwrap()
        );
        assert_eq!(
            transport_state.lock().unwrap().signals,
            [(workspace.id, sid.to_string())]
        );

        *probe.0.lock().unwrap() = report(
            true,
            Some(true),
            SidEvidence::ConfirmedResume(sid.to_string()),
        );
        assert_eq!(
            service.derive_lease(&session).await,
            WriterLease::Cli {
                claude_session_id: Some(sid.to_string())
            }
        );
        *probe.0.lock().unwrap() = report(true, Some(false), SidEvidence::Unknown);
        assert_eq!(service.derive_lease(&session).await, WriterLease::Free);
    }

    #[tokio::test]
    async fn killed_executor_holds_queued_follow_up_until_explicit_retry() {
        let (db, _workspace, session) = fixture().await;
        let execution = create_running_executor(&db, session.id, "running writer").await;
        let row = store_message(
            &db,
            session.id,
            "do not run after kill",
            QueuedMessageSource::Ui,
        )
        .await;
        ExecutionProcess::update_completion(
            &db.pool,
            execution.id,
            ExecutionProcessStatus::Killed,
            None,
        )
        .await
        .unwrap();
        let dispatcher_state = Arc::new(StdMutex::new(DispatcherState::default()));
        let (service, _) = service_with_components(
            db.clone(),
            report(false, Some(false), SidEvidence::Unknown),
            Arc::new(FakeTransport),
            Arc::new(RecordingDispatcher {
                db: db.clone(),
                state: dispatcher_state.clone(),
            }),
        );

        assert!(
            !service
                .on_executor_finished(session.id, ExecutionProcessStatus::Killed, true)
                .await
                .unwrap()
        );
        assert!(!service.drain_session(session.id).await.unwrap());
        assert!(dispatcher_state.lock().unwrap().prompts.is_empty());
        let held = SessionQueuedMessage::find_by_id(&db.pool, row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(held.state, QueuedMessageState::Queued);
        assert_eq!(
            held.failure_reason.as_deref(),
            Some(ABNORMAL_EXECUTOR_QUEUE_HOLD)
        );
    }

    #[tokio::test]
    async fn finish_hook_self_guards_a_non_releasing_chained_action() {
        let (db, _workspace, session) = fixture().await;
        let row = store_message(
            &db,
            session.id,
            "wait for the chained action",
            QueuedMessageSource::Ui,
        )
        .await;
        let dispatcher_state = Arc::new(StdMutex::new(DispatcherState::default()));
        let (service, _) = service_with_components(
            db.clone(),
            report(false, Some(false), SidEvidence::Unknown),
            Arc::new(FakeTransport),
            Arc::new(RecordingDispatcher {
                db: db.clone(),
                state: dispatcher_state.clone(),
            }),
        );

        assert!(
            !service
                .on_executor_finished(session.id, ExecutionProcessStatus::Completed, false)
                .await
                .unwrap()
        );
        assert!(dispatcher_state.lock().unwrap().prompts.is_empty());
        assert_eq!(
            SessionQueuedMessage::find_by_id(&db.pool, row.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            QueuedMessageState::Queued
        );
    }

    #[tokio::test]
    async fn drain_recovers_busy_resume_pane_when_finish_hook_was_missed() {
        let (db, workspace, session) = fixture().await;
        let sid = "25252525-2525-4252-8252-252525252525";
        let execution = create_running_executor(&db, session.id, "blocking writer").await;
        bind_confirmed_cli(&db, &workspace, &session, sid).await;
        ExecutionProcess::update_completion(
            &db.pool,
            execution.id,
            ExecutionProcessStatus::Completed,
            Some(0),
        )
        .await
        .unwrap();
        let transport_state = Arc::new(StdMutex::new(ResumeTransportState::default()));
        let transport = Arc::new(ResumeRecordingTransport {
            state: transport_state.clone(),
        });
        let (service, _) = service_with_components(
            db,
            report(false, Some(false), SidEvidence::Unknown),
            transport,
            Arc::new(NeverDispatcher),
        );

        service.drain_all().await;

        assert_eq!(
            transport_state.lock().unwrap().signals,
            [(workspace.id, sid.to_string())]
        );
    }

    #[tokio::test]
    async fn resume_ready_signal_retries_transient_failures() {
        let (db, workspace, session) = fixture().await;
        let sid = "26262626-2626-4262-8262-262626262626";
        let execution = create_running_executor(&db, session.id, "blocking writer").await;
        bind_confirmed_cli(&db, &workspace, &session, sid).await;
        ExecutionProcess::update_completion(
            &db.pool,
            execution.id,
            ExecutionProcessStatus::Completed,
            Some(0),
        )
        .await
        .unwrap();
        let transport_state = Arc::new(StdMutex::new(ResumeTransportState {
            signals: Vec::new(),
            failures_remaining: 2,
        }));
        let transport = Arc::new(ResumeRecordingTransport {
            state: transport_state.clone(),
        });
        let (service, _) = service_with_components(
            db,
            report(false, Some(false), SidEvidence::Unknown),
            transport,
            Arc::new(NeverDispatcher),
        );

        assert!(
            !service
                .on_executor_finished(session.id, ExecutionProcessStatus::Completed, true)
                .await
                .unwrap()
        );

        assert_eq!(
            transport_state.lock().unwrap().signals,
            vec![(workspace.id, sid.to_string()); 3]
        );
    }

    #[tokio::test]
    async fn resume_ready_signal_exhaustion_is_returned_to_finish_hook() {
        let (db, workspace, session) = fixture().await;
        let sid = "27272727-2727-4272-8272-272727272727";
        let execution = create_running_executor(&db, session.id, "blocking writer").await;
        bind_confirmed_cli(&db, &workspace, &session, sid).await;
        ExecutionProcess::update_completion(
            &db.pool,
            execution.id,
            ExecutionProcessStatus::Completed,
            Some(0),
        )
        .await
        .unwrap();
        let transport_state = Arc::new(StdMutex::new(ResumeTransportState {
            signals: Vec::new(),
            failures_remaining: RESUME_SIGNAL_ATTEMPTS,
        }));
        let transport = Arc::new(ResumeRecordingTransport {
            state: transport_state.clone(),
        });
        let (service, _) = service_with_components(
            db,
            report(false, Some(false), SidEvidence::Unknown),
            transport,
            Arc::new(NeverDispatcher),
        );

        let error = service
            .on_executor_finished(session.id, ExecutionProcessStatus::Completed, true)
            .await
            .unwrap_err();

        assert!(matches!(error, CliCollabError::Transport { .. }));
        assert_eq!(
            transport_state.lock().unwrap().signals,
            vec![(workspace.id, sid.to_string()); RESUME_SIGNAL_ATTEMPTS as usize]
        );
    }

    #[tokio::test]
    async fn dispatch_gate_routes_confirmed_cli_through_pasting_to_pasted() {
        let (db, workspace, session) = fixture().await;
        let sid = "22222222-2222-4222-8222-222222222222";
        bind_confirmed_cli(&db, &workspace, &session, sid).await;
        let transport_state = Arc::new(StdMutex::new(TransportState::default()));
        let transport = Arc::new(RecordingTransport {
            db: db.clone(),
            session_id: session.id,
            state: transport_state.clone(),
            paste_succeeds: true,
        });
        let (service, _) = service_with_components(
            db.clone(),
            report(
                true,
                Some(true),
                SidEvidence::ConfirmedResume(sid.to_string()),
            ),
            transport,
            Arc::new(NeverDispatcher),
        );

        let outcome = service
            .dispatch_gate(
                &session,
                "route to CLI".to_string(),
                executor_config(),
                QueuedMessageSource::Ui,
                false,
            )
            .await
            .unwrap();
        let DispatchOutcome::RoutedToCli { delivery } = outcome else {
            panic!("confirmed CLI must receive the prompt");
        };
        let message = delivery.message().unwrap();
        assert_eq!(message.data.message, "route to CLI");
        assert_eq!(message.state, QueuedMessageState::Pasted);
        assert_eq!(message.claude_session_id.as_deref(), Some(sid));
        let observations = transport_state.lock().unwrap();
        assert_eq!(observations.pasted_prompts, ["route to CLI"]);
        assert_eq!(observations.observed_states, [QueuedMessageState::Pasting]);
    }

    #[tokio::test]
    async fn dispatch_gate_queues_ambiguous_sid_without_pasting() {
        let (db, workspace, session) = fixture().await;
        let sid = "23232323-2323-4232-8232-232323232323";
        bind_confirmed_cli(&db, &workspace, &session, sid).await;
        let transport_state = Arc::new(StdMutex::new(TransportState::default()));
        let transport = Arc::new(RecordingTransport {
            db: db.clone(),
            session_id: session.id,
            state: transport_state.clone(),
            paste_succeeds: true,
        });
        let (service, _) = service_with_components(
            db,
            report(true, Some(true), SidEvidence::Ambiguous),
            transport,
            Arc::new(NeverDispatcher),
        );

        let outcome = service
            .dispatch_gate(
                &session,
                "do not paste ambiguously".to_string(),
                executor_config(),
                QueuedMessageSource::Ui,
                false,
            )
            .await
            .unwrap();
        let DispatchOutcome::Queued { status } = outcome else {
            panic!("ambiguous CLI sid must queue the prompt");
        };
        assert_eq!(
            status.message().unwrap().data.message,
            "do not paste ambiguously"
        );
        assert!(transport_state.lock().unwrap().pasted_prompts.is_empty());
    }

    #[tokio::test]
    async fn dispatch_gate_starts_free_executor_with_scoped_reservation() {
        let (db, _workspace, session) = fixture().await;
        let dispatcher_state = Arc::new(StdMutex::new(DispatcherState::default()));
        let dispatcher = Arc::new(RecordingDispatcher {
            db: db.clone(),
            state: dispatcher_state.clone(),
        });
        let (service, _) = service_with_components(
            db.clone(),
            report(false, Some(false), SidEvidence::Unknown),
            Arc::new(FakeTransport),
            dispatcher,
        );

        let outcome = service
            .dispatch_gate(
                &session,
                "start immediately".to_string(),
                executor_config(),
                QueuedMessageSource::Ui,
                false,
            )
            .await
            .unwrap();
        let DispatchOutcome::Started { execution_process } = outcome else {
            panic!("free lease must start the executor");
        };
        assert_eq!(execution_process.session_id, session.id);
        let (prompts, reservation_seen) = {
            let state = dispatcher_state.lock().unwrap();
            (state.prompts.clone(), state.reservation_seen)
        };
        assert_eq!(prompts, ["start immediately"]);
        assert!(reservation_seen);
        assert!(
            WorkspaceSpawnReservation::find(&db.pool, session.workspace_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            SessionQueuedMessage::find_active(&db.pool, session.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn dispatcher_error_releases_reservation_and_requeues_prompt() {
        let (db, _workspace, session) = fixture().await;
        let dispatcher_state = Arc::new(StdMutex::new(DispatcherState::default()));
        let dispatcher = Arc::new(FailingDispatcher {
            db: db.clone(),
            state: dispatcher_state.clone(),
        });
        let (service, _) = service_with_components(
            db.clone(),
            report(false, Some(false), SidEvidence::Unknown),
            Arc::new(FakeTransport),
            dispatcher,
        );

        let outcome = service
            .dispatch_gate(
                &session,
                "preserve failed dispatch".to_string(),
                executor_config(),
                QueuedMessageSource::Ui,
                false,
            )
            .await
            .unwrap();

        let DispatchOutcome::Queued { status } = outcome else {
            panic!("dispatcher failure must return the prompt to the queue");
        };
        assert_eq!(
            status.message().unwrap().data.message,
            "preserve failed dispatch"
        );
        let (prompts, reservation_seen) = {
            let state = dispatcher_state.lock().unwrap();
            (state.prompts.clone(), state.reservation_seen)
        };
        assert_eq!(prompts, ["preserve failed dispatch"]);
        assert!(reservation_seen);
        assert!(
            WorkspaceSpawnReservation::find(&db.pool, session.workspace_id)
                .await
                .unwrap()
                .is_none()
        );
        let queued = SessionQueuedMessage::find_active(&db.pool, session.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(queued.state, QueuedMessageState::Queued);
        assert_eq!(queued.prompt, "preserve failed dispatch");
    }

    #[tokio::test]
    async fn retry_conflict_never_invokes_destructive_dispatcher() {
        let (db, _workspace, session) = fixture().await;
        store_message(&db, session.id, "already queued", QueuedMessageSource::Ui).await;
        let dispatcher_state = Arc::new(StdMutex::new(DispatcherState::default()));
        let (service, _) = service_with_components(
            db.clone(),
            report(false, Some(false), SidEvidence::Unknown),
            Arc::new(FakeTransport),
            Arc::new(RecordingDispatcher {
                db,
                state: dispatcher_state.clone(),
            }),
        );
        let retry = RetryDispatchContext {
            process_id: Uuid::new_v4(),
            force_when_dirty: false,
            perform_git_reset: true,
            reset_to_message_id: Some("assistant-message".to_string()),
        };

        let outcome = service
            .dispatch_retry(
                &session,
                "retry prompt".to_string(),
                executor_config(),
                QueuedMessageSource::Ui,
                false,
                retry,
            )
            .await
            .unwrap();

        assert!(matches!(outcome, DispatchOutcome::Conflict { .. }));
        assert!(dispatcher_state.lock().unwrap().prompts.is_empty());
    }

    #[tokio::test]
    async fn queued_retry_preserves_reset_and_transcript_context_until_dispatch() {
        let (db, _workspace, session) = fixture().await;
        let blocker = create_running_executor(&db, session.id, "block retry").await;
        let dispatcher_state = Arc::new(StdMutex::new(DispatcherState::default()));
        let (service, _) = service_with_components(
            db.clone(),
            report(false, Some(false), SidEvidence::Unknown),
            Arc::new(FakeTransport),
            Arc::new(RecordingDispatcher {
                db,
                state: dispatcher_state.clone(),
            }),
        );
        let retry = RetryDispatchContext {
            process_id: Uuid::new_v4(),
            force_when_dirty: true,
            perform_git_reset: true,
            reset_to_message_id: Some("assistant-message".to_string()),
        };

        let outcome = service
            .dispatch_retry(
                &session,
                "retry prompt".to_string(),
                executor_config(),
                QueuedMessageSource::Ui,
                false,
                retry.clone(),
            )
            .await
            .unwrap();

        assert!(matches!(outcome, DispatchOutcome::Queued { .. }));
        assert!(dispatcher_state.lock().unwrap().retry_contexts.is_empty());

        ExecutionProcess::update_completion(
            &service.db.pool,
            blocker.id,
            ExecutionProcessStatus::Completed,
            Some(0),
        )
        .await
        .unwrap();
        assert!(service.drain_session(session.id).await.unwrap());
        assert_eq!(
            dispatcher_state.lock().unwrap().retry_contexts,
            [Some(retry)]
        );
    }

    #[tokio::test]
    async fn slow_executor_dispatch_is_not_reconciled_or_run_twice() {
        let (db, _workspace, session) = fixture().await;
        let row = store_message(&db, session.id, "run exactly once", QueuedMessageSource::Ui).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (service, _) = service_with_components(
            db.clone(),
            report(false, Some(false), SidEvidence::Unknown),
            Arc::new(FakeTransport),
            Arc::new(SlowDispatcher {
                db: db.clone(),
                calls: calls.clone(),
                started: started.clone(),
                release: release.clone(),
            }),
        );
        let service = Arc::new(service);
        let draining = {
            let service = service.clone();
            tokio::spawn(async move { service.drain_session(session.id).await })
        };

        started.notified().await;
        let older_than_paste_grace =
            Utc::now() - PASTING_STARTUP_GRACE - ChronoDuration::seconds(1);
        sqlx::query("UPDATE session_queued_messages SET updated_at = ? WHERE id = ?")
            .bind(older_than_paste_grace)
            .bind(row.id)
            .execute(&db.pool)
            .await
            .unwrap();
        let reconciled = SessionQueuedMessage::reconcile(
            &db.pool,
            Utc::now(),
            PASTING_STARTUP_GRACE,
            PASTE_ACK_HARD_CAP,
            Some(&service.executor_claim_owner),
            Some(&service.paste_claim_owner),
        )
        .await
        .unwrap();
        assert_eq!(reconciled.requeued_pasting, 0);
        assert_eq!(
            SessionQueuedMessage::find_by_id(&db.pool, row.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            QueuedMessageState::Pasting
        );

        release.notify_one();
        assert!(draining.await.unwrap().unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            SessionQueuedMessage::find_by_id(&db.pool, row.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            QueuedMessageState::Consumed
        );

        let execution = ExecutionProcess::find_running(&db.pool)
            .await
            .unwrap()
            .pop()
            .unwrap();
        ExecutionProcess::update_completion(
            &db.pool,
            execution.id,
            ExecutionProcessStatus::Completed,
            Some(0),
        )
        .await
        .unwrap();
        assert!(
            !service
                .on_executor_finished(session.id, ExecutionProcessStatus::Completed, true)
                .await
                .unwrap()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn slow_paste_is_not_reconciled_or_submitted_twice() {
        let (db, workspace, session) = fixture().await;
        let sid = "41414141-4141-4141-8141-414141414141";
        bind_confirmed_cli(&db, &workspace, &session, sid).await;
        let row = store_message(
            &db,
            session.id,
            "paste exactly once",
            QueuedMessageSource::Ui,
        )
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (service, _) = service_with_components(
            db.clone(),
            report(
                true,
                Some(true),
                SidEvidence::ConfirmedResume(sid.to_string()),
            ),
            Arc::new(SlowPasteTransport {
                calls: calls.clone(),
                started: started.clone(),
                release: release.clone(),
            }),
            Arc::new(NeverDispatcher),
        );
        let service = Arc::new(service);
        let draining = {
            let service = service.clone();
            tokio::spawn(async move { service.drain_session(session.id).await })
        };

        started.notified().await;
        let older_than_paste_grace =
            Utc::now() - PASTING_STARTUP_GRACE - ChronoDuration::seconds(1);
        sqlx::query("UPDATE session_queued_messages SET updated_at = ? WHERE id = ?")
            .bind(older_than_paste_grace)
            .bind(row.id)
            .execute(&db.pool)
            .await
            .unwrap();
        service.reconcile_delivery_state_from_db().await;

        release.notify_one();
        assert!(draining.await.unwrap().unwrap());
        let _ = service.drain_session(session.id).await.unwrap();

        let native_file = CliNativeFile::register(
            &db.pool,
            &RegisterCliNativeFile {
                claude_session_id: sid,
                dir_path: "/tmp/slow-paste-native",
                file_name: "slow-paste.jsonl",
                discovered_workspace_id: Some(workspace.id),
                dev: 1,
                inode: 41,
                observed_size: 1,
                observed_mtime_ms: None,
            },
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO cli_native_records \
                 (file_id, line_seq, claude_session_id, uuid, kind, raw, \
                  disposition, bound_queued_message_id) \
             VALUES (?, 0, ?, ?, 'user', '{}', 'renderable', ?)",
        )
        .bind(native_file.id)
        .bind(sid)
        .bind("slow-paste-user")
        .bind(row.id)
        .execute(&db.pool)
        .await
        .unwrap();
        service.reconcile_delivery_state_from_db().await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let delivered = SessionQueuedMessage::find_by_id(&db.pool, row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivered.state, QueuedMessageState::Imported);
        assert!(delivered.acked_at.is_some());
    }

    #[tokio::test]
    async fn paste_slot_does_not_report_delivery_after_losing_its_claim() {
        let (db, workspace, session) = fixture().await;
        let sid = "42424242-4242-4242-8242-424242424242";
        bind_confirmed_cli(&db, &workspace, &session, sid).await;
        let row = store_message(&db, session.id, "lose paste claim", QueuedMessageSource::Ui).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (service, _) = service_with_components(
            db.clone(),
            report(
                true,
                Some(true),
                SidEvidence::ConfirmedResume(sid.to_string()),
            ),
            Arc::new(SlowPasteTransport {
                calls: calls.clone(),
                started: started.clone(),
                release: release.clone(),
            }),
            Arc::new(NeverDispatcher),
        );
        let service = Arc::new(service);
        let draining = {
            let service = service.clone();
            tokio::spawn(async move { service.drain_session(session.id).await })
        };

        started.notified().await;
        assert!(
            SessionQueuedMessage::requeue(&db.pool, row.id, Some("claim cancelled"))
                .await
                .unwrap()
        );
        release.notify_one();

        let result = draining.await.unwrap();
        assert!(matches!(
            result,
            Err(CliCollabError::PasteClaimLost(id)) if id == row.id
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            SessionQueuedMessage::find_by_id(&db.pool, row.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            QueuedMessageState::Queued
        );
    }

    #[tokio::test]
    async fn replace_contract_preserves_conflict_details_and_rejects_in_flight_replace() {
        let (db, _workspace, session) = fixture().await;
        let (service, _) = service(db.clone(), report(false, Some(false), SidEvidence::Unknown));

        let first = service
            .queue_message(
                &session,
                "queued from recovery".to_string(),
                executor_config(),
                QueuedMessageSource::Recovery,
                false,
            )
            .await
            .unwrap();
        let QueueMutation::Stored(status) = first else {
            panic!("empty slot must accept the first prompt");
        };
        let first_id = status.message().unwrap().id;

        let conflict = service
            .queue_message(
                &session,
                "new UI prompt".to_string(),
                executor_config(),
                QueuedMessageSource::Ui,
                false,
            )
            .await
            .unwrap();
        let QueueMutation::Conflict(status) = conflict else {
            panic!("queued replacement requires explicit confirmation");
        };
        let conflict_message = status.message().unwrap();
        assert_eq!(conflict_message.id, first_id);
        assert_eq!(conflict_message.data.message, "queued from recovery");
        assert_eq!(conflict_message.source, QueuedMessageSource::Recovery);

        let replaced = service
            .queue_message(
                &session,
                "new UI prompt".to_string(),
                executor_config(),
                QueuedMessageSource::Ui,
                true,
            )
            .await
            .unwrap();
        let QueueMutation::Stored(status) = replaced else {
            panic!("explicit queued replacement must succeed");
        };
        let replaced_message = status.message().unwrap();
        assert_eq!(replaced_message.id, first_id);
        assert_eq!(replaced_message.data.message, "new UI prompt");
        assert_eq!(replaced_message.source, QueuedMessageSource::Ui);

        SessionQueuedMessage::claim_for_paste(&db.pool, first_id, Some("sid"), "test-paste-owner")
            .await
            .unwrap()
            .unwrap();
        assert!(
            SessionQueuedMessage::mark_pasted(&db.pool, first_id, "test-paste-owner")
                .await
                .unwrap()
        );
        let in_flight = service
            .queue_message(
                &session,
                "replace pasted".to_string(),
                executor_config(),
                QueuedMessageSource::Recovery,
                true,
            )
            .await
            .unwrap();
        let QueueMutation::Conflict(status) = in_flight else {
            panic!("pasted delivery must never be replaced");
        };
        let in_flight_message = status.message().unwrap();
        assert_eq!(in_flight_message.id, first_id);
        assert_eq!(in_flight_message.data.message, "new UI prompt");
        assert_eq!(in_flight_message.state, QueuedMessageState::Pasted);
    }

    #[tokio::test]
    async fn delivery_reconciliation_keeps_long_live_paste_without_repasting() {
        let (db, workspace, session) = fixture().await;
        let sid = "31313131-3131-4131-8131-313131313131";
        let binding = bind_confirmed_cli(&db, &workspace, &session, sid).await;
        let row = store_message(
            &db,
            session.id,
            "wait through long CLI turn",
            QueuedMessageSource::Ui,
        )
        .await;
        SessionQueuedMessage::claim_for_paste(&db.pool, row.id, Some(sid), "test-paste-owner")
            .await
            .unwrap()
            .unwrap();
        SessionQueuedMessage::mark_pasted(&db.pool, row.id, "test-paste-owner")
            .await
            .unwrap();
        sqlx::query(
            "UPDATE cli_pane_bindings \
             SET created_at = datetime('now', '-32 seconds') WHERE id = ?",
        )
        .bind(binding.id)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE session_queued_messages \
             SET pasted_at = datetime('now', '-31 seconds') WHERE id = ?",
        )
        .bind(row.id)
        .execute(&db.pool)
        .await
        .unwrap();
        let transport_state = Arc::new(StdMutex::new(TransportState::default()));
        let transport = Arc::new(RecordingTransport {
            db: db.clone(),
            session_id: session.id,
            state: transport_state.clone(),
            paste_succeeds: true,
        });
        let (service, _) = service_with_components(
            db.clone(),
            report(
                true,
                Some(true),
                SidEvidence::ConfirmedResume(sid.to_string()),
            ),
            transport,
            Arc::new(NeverDispatcher),
        );

        service.reconcile_delivery_state_from_db().await;

        let row = SessionQueuedMessage::find_by_id(&db.pool, row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, QueuedMessageState::Pasted);
        assert_eq!(row.prompt, "wait through long CLI turn");
        assert!(row.failure_reason.is_none());
        assert!(!service.drain_session(session.id).await.unwrap());
        assert!(transport_state.lock().unwrap().pasted_prompts.is_empty());
    }

    #[tokio::test]
    async fn delivery_reconciliation_requeues_dead_pane_with_notice() {
        let (db, workspace, session) = fixture().await;
        let sid = "32323232-3232-4232-8232-323232323232";
        let binding = bind_confirmed_cli(&db, &workspace, &session, sid).await;
        let row = store_message(
            &db,
            session.id,
            "recover after pane death",
            QueuedMessageSource::Recovery,
        )
        .await;
        SessionQueuedMessage::claim_for_paste(&db.pool, row.id, Some(sid), "test-paste-owner")
            .await
            .unwrap()
            .unwrap();
        SessionQueuedMessage::mark_pasted(&db.pool, row.id, "test-paste-owner")
            .await
            .unwrap();
        let pasted_at = SessionQueuedMessage::find_by_id(&db.pool, row.id)
            .await
            .unwrap()
            .unwrap()
            .pasted_at;
        let (service, _) = service(db.clone(), report(false, Some(false), SidEvidence::Unknown));

        service.reconcile_delivery_state_from_db().await;

        let row = SessionQueuedMessage::find_by_id(&db.pool, row.id)
            .await
            .unwrap()
            .unwrap();
        assert!(row.was_requeued_from_pasted());
        assert_eq!(row.pasted_at, pasted_at);
        assert_eq!(
            row.failure_reason.as_deref(),
            Some(db::models::session_queued_message::PASTED_REQUEUE_FAILURE_REASON)
        );
        assert!(
            CliPaneBinding::find_by_id(&db.pool, binding.id)
                .await
                .unwrap()
                .unwrap()
                .released_at
                .is_some()
        );
    }

    #[tokio::test]
    async fn delivery_reconciliation_hard_cap_requeues_with_notice() {
        let (db, workspace, session) = fixture().await;
        let sid = "33333333-3333-4333-8333-333333333333";
        let binding = bind_confirmed_cli(&db, &workspace, &session, sid).await;
        let row = store_message(
            &db,
            session.id,
            "hard capped CLI delivery",
            QueuedMessageSource::Ui,
        )
        .await;
        SessionQueuedMessage::claim_for_paste(&db.pool, row.id, Some(sid), "test-paste-owner")
            .await
            .unwrap()
            .unwrap();
        SessionQueuedMessage::mark_pasted(&db.pool, row.id, "test-paste-owner")
            .await
            .unwrap();
        sqlx::query(
            "UPDATE cli_pane_bindings \
             SET created_at = datetime('now', '-17 minutes') WHERE id = ?",
        )
        .bind(binding.id)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE session_queued_messages \
             SET pasted_at = datetime('now', '-16 minutes') WHERE id = ?",
        )
        .bind(row.id)
        .execute(&db.pool)
        .await
        .unwrap();
        let transport_state = Arc::new(StdMutex::new(TransportState::default()));
        let transport = Arc::new(RecordingTransport {
            db: db.clone(),
            session_id: session.id,
            state: transport_state.clone(),
            paste_succeeds: true,
        });
        let (service, _) = service_with_components(
            db.clone(),
            report(
                true,
                Some(true),
                SidEvidence::ConfirmedResume(sid.to_string()),
            ),
            transport,
            Arc::new(NeverDispatcher),
        );

        service.reconcile_delivery_state_from_db().await;

        let row = SessionQueuedMessage::find_by_id(&db.pool, row.id)
            .await
            .unwrap()
            .unwrap();
        assert!(row.was_requeued_from_pasted());
        assert_eq!(
            row.failure_reason.as_deref(),
            Some(db::models::session_queued_message::PASTED_REQUEUE_FAILURE_REASON)
        );
        assert!(!service.drain_session(session.id).await.unwrap());
        assert!(transport_state.lock().unwrap().pasted_prompts.is_empty());
    }

    #[tokio::test]
    async fn requeued_paste_waits_for_original_binding_then_drains_after_release() {
        let (db, workspace, session) = fixture().await;
        let sid = "34343434-3434-4434-8434-343434343434";
        let binding = bind_confirmed_cli(&db, &workspace, &session, sid).await;
        let row = store_message(
            &db,
            session.id,
            "dispatch only after pane release",
            QueuedMessageSource::Ui,
        )
        .await;
        SessionQueuedMessage::claim_for_paste(&db.pool, row.id, Some(sid), "test-paste-owner")
            .await
            .unwrap()
            .unwrap();
        SessionQueuedMessage::mark_pasted(&db.pool, row.id, "test-paste-owner")
            .await
            .unwrap();
        SessionQueuedMessage::requeue_pasted(&db.pool, row.id)
            .await
            .unwrap();
        let transport_state = Arc::new(StdMutex::new(TransportState::default()));
        let transport = Arc::new(RecordingTransport {
            db: db.clone(),
            session_id: session.id,
            state: transport_state.clone(),
            paste_succeeds: true,
        });
        let dispatcher_state = Arc::new(StdMutex::new(DispatcherState::default()));
        let dispatcher = Arc::new(RecordingDispatcher {
            db: db.clone(),
            state: dispatcher_state.clone(),
        });
        let (service, probe) = service_with_components(
            db.clone(),
            report(
                true,
                Some(true),
                SidEvidence::ConfirmedResume(sid.to_string()),
            ),
            transport,
            dispatcher,
        );

        assert!(!service.drain_session(session.id).await.unwrap());
        assert!(transport_state.lock().unwrap().pasted_prompts.is_empty());
        assert!(dispatcher_state.lock().unwrap().prompts.is_empty());

        CliPaneBinding::release(&db.pool, binding.id).await.unwrap();
        *probe.0.lock().unwrap() = report(false, Some(false), SidEvidence::Unknown);
        assert!(service.drain_session(session.id).await.unwrap());

        assert!(transport_state.lock().unwrap().pasted_prompts.is_empty());
        assert_eq!(
            dispatcher_state.lock().unwrap().prompts,
            ["dispatch only after pane release"]
        );
        assert_eq!(
            SessionQueuedMessage::find_by_id(&db.pool, row.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            QueuedMessageState::Consumed
        );
    }

    #[tokio::test]
    async fn startup_reconciliation_recovers_pasting_and_honors_ack_without_repaste() {
        let (db, workspace, interrupted_session) = fixture().await;
        let acknowledged_session = Session::create(
            &db.pool,
            &CreateSession {
                executor: Some("CLAUDE_CODE".to_string()),
                name: None,
            },
            Uuid::new_v4(),
            workspace.id,
        )
        .await
        .unwrap();
        let interrupted = store_message(
            &db,
            interrupted_session.id,
            "recover interrupted paste",
            QueuedMessageSource::Recovery,
        )
        .await;
        SessionQueuedMessage::claim_for_paste(
            &db.pool,
            interrupted.id,
            Some("interrupted-sid"),
            "crashed-paste-owner",
        )
        .await
        .unwrap()
        .unwrap();
        sqlx::query(
            "UPDATE session_queued_messages \
             SET updated_at = datetime('now', '-6 seconds') WHERE id = ?",
        )
        .bind(interrupted.id)
        .execute(&db.pool)
        .await
        .unwrap();

        let acknowledged = store_message(
            &db,
            acknowledged_session.id,
            "already imported",
            QueuedMessageSource::Ui,
        )
        .await;
        SessionQueuedMessage::claim_for_paste(
            &db.pool,
            acknowledged.id,
            Some("acknowledged-sid"),
            "test-paste-owner",
        )
        .await
        .unwrap()
        .unwrap();
        SessionQueuedMessage::mark_pasted(&db.pool, acknowledged.id, "test-paste-owner")
            .await
            .unwrap();
        let native_file = CliNativeFile::register(
            &db.pool,
            &RegisterCliNativeFile {
                claude_session_id: "acknowledged-sid",
                dir_path: "/tmp/collab-native",
                file_name: "acknowledged-sid.jsonl",
                discovered_workspace_id: Some(workspace.id),
                dev: 1,
                inode: 1,
                observed_size: 1,
                observed_mtime_ms: None,
            },
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO cli_native_records \
                 (file_id, line_seq, claude_session_id, uuid, kind, raw, \
                  disposition, bound_queued_message_id) \
             VALUES (?, 0, ?, ?, 'user', '{}', 'renderable', ?)",
        )
        .bind(native_file.id)
        .bind("acknowledged-sid")
        .bind("acknowledged-user")
        .bind(acknowledged.id)
        .execute(&db.pool)
        .await
        .unwrap();

        let transport_state = Arc::new(StdMutex::new(TransportState::default()));
        let transport = Arc::new(RecordingTransport {
            db: db.clone(),
            session_id: acknowledged_session.id,
            state: transport_state.clone(),
            paste_succeeds: true,
        });
        let (service, _) = service_with_components(
            db.clone(),
            report(false, Some(false), SidEvidence::Unknown),
            transport,
            Arc::new(NeverDispatcher),
        );
        service.reconcile_delivery_state_from_db().await;

        let interrupted = SessionQueuedMessage::find_by_id(&db.pool, interrupted.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(interrupted.state, QueuedMessageState::Queued);
        assert_eq!(interrupted.prompt, "recover interrupted paste");
        let acknowledged = SessionQueuedMessage::find_by_id(&db.pool, acknowledged.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(acknowledged.state, QueuedMessageState::Imported);
        assert!(acknowledged.acked_at.is_some());
        assert!(
            !service
                .drain_session(acknowledged_session.id)
                .await
                .unwrap()
        );
        assert!(transport_state.lock().unwrap().pasted_prompts.is_empty());
    }
}
