//! Read-only ingestion of Claude Code's native transcript store.
//!
//! Every filesystem operation below the configured projects root is a read:
//! `read_dir`, `metadata`, or `File::open`. App-owned persistence goes only to
//! SQLite through `db` model functions.

#[cfg(test)]
mod codex_tests;
mod forks;
mod projection;
mod tail;
#[cfg(test)]
mod tests;

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, Metadata},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use chrono::{DateTime, Utc};
use db::{
    DBService,
    models::{
        claude_session_link::{ClaudeSessionLink, ClaudeSessionLinkMutation},
        cli_ingest_outbox::CliIngestOutbox,
        cli_native_file::{CliNativeFile, RegisterCliNativeFile},
        cli_native_record::{
            CliNativeRecord, CliNativeRecordDisposition, ImportedCursor, NativeImportContext,
            NewCliNativeRecord,
        },
        cli_pane_binding::{CliPaneBinding, CliPaneBoundVia},
        session::Session,
        workspace::{Workspace, WorkspaceError},
        workspace_cli_activity::WorkspaceCliActivity,
    },
};
use executors::executors::{
    claude::native::{NativeClaudeDisposition, NativeClaudeSkipReason, adapt_native_claude_line},
    codex::rollout::{CodexRolloutDisposition, adapt_codex_rollout_line, rollout_thread_id},
};
pub use forks::{NativeForkBranch, NativeForkView};
use futures::StreamExt;
pub use projection::{
    NativeBranchMetadata, NativeFeedCursor, NativeFeedEntry, NativeFeedFork, NativeFeedOrigin,
    NativeFeedSnapshot, NativeFileImportHealth, NativeIngestHealth,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    sync::{Notify, RwLock, Semaphore, broadcast},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use ts_rs::TS;
use uuid::Uuid;

use self::{
    projection::NativeProjection,
    tail::{
        ObservedFileState, StoredTailState, hash_bytes, read_complete_line_batch, rescan_reason,
    },
};
use crate::services::{
    cli_collab::{CliWriterProbe, SidEvidence},
    filesystem_watcher,
};

const REGISTRY_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const OUTBOX_SAFETY_POLL_INTERVAL: Duration = Duration::from_secs(30);
const IMPORT_BATCH_LINE_LIMIT: usize = 256;
/// Import batches committed at once across every watched directory. SQLite
/// has one writer, so more permits only queue on its lock and starve API
/// writes; one keeps a start-up backfill from monopolising it while live
/// appends still interleave batch by batch (the semaphore is FIFO).
const CONCURRENT_IMPORT_BATCHES: usize = 1;
/// Sessions whose projection is kept for incremental feed updates. Past the
/// cap the least recently used session is evicted, preferring ones without a
/// connected feed; a session is also dropped when its last feed disconnects.
const PROJECTION_CACHE_CAPACITY: usize = 32;
/// Codex day directories a CLI-first fallback reads when no hook reported the
/// fresh pane's thread: the pane's launch day, its neighbours (the directory
/// name is the local date) and today, newest first.
const CODEX_FALLBACK_DAY_DIRS: usize = 4;
/// Rollouts written this long before a pane launch still count as its own.
const CODEX_FALLBACK_LAUNCH_SKEW: chrono::Duration = chrono::Duration::seconds(60);
/// Bytes read from a rollout's first line (`session_meta`) when matching a
/// fresh pane by working directory.
const CODEX_SESSION_META_READ_LIMIT: u64 = 1024 * 1024;

/// Days an unreachable transcript is kept before retention removes it. `0`
/// disables retention and lets the store grow without bound.
const RETENTION_ENV: &str = "VIBE_KANBAN_CLI_TRANSCRIPT_RETENTION_DAYS";
const DEFAULT_RETENTION_DAYS: u32 = 14;
const RETENTION_SWEEP_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Retention competes for the same write lock as ingest, so the first sweep
/// waits for start-up scanning to settle.
const RETENTION_STARTUP_DELAY: Duration = Duration::from_secs(5 * 60);
/// Files per sweep. Each is one transaction, so this bounds how long retention
/// can hold the write lock at a time; a backlog drains over several sweeps.
const RETENTION_FILES_PER_SWEEP: i64 = 25;

fn retention_days() -> Option<u32> {
    match std::env::var(RETENTION_ENV) {
        Err(_) => Some(DEFAULT_RETENTION_DAYS),
        Ok(raw) => match raw.trim().parse::<u32>() {
            Ok(0) => None,
            Ok(days) => Some(days),
            Err(_) => {
                tracing::warn!(
                    "{RETENTION_ENV}={raw:?} is not a whole number of days; \
                     using {DEFAULT_RETENTION_DAYS}"
                );
                Some(DEFAULT_RETENTION_DAYS)
            }
        },
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NativeLinkPersisted {
    pub execution_process_id: Uuid,
    pub native_uuid: String,
}

static NATIVE_LINK_EVENTS: OnceLock<broadcast::Sender<NativeLinkPersisted>> = OnceLock::new();

fn native_link_events() -> &'static broadcast::Sender<NativeLinkPersisted> {
    NATIVE_LINK_EVENTS.get_or_init(|| broadcast::channel(4096).0)
}

pub(crate) fn notify_native_link_persisted(event: NativeLinkPersisted) {
    let _ = native_link_events().send(event);
}

#[derive(Debug, Error)]
pub enum ClaudeTranscriptIngestError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Workspace(#[from] db::models::workspace::WorkspaceError),
    #[error("session {0} was not found")]
    SessionNotFound(Uuid),
    #[error("workspace for session {0} has no local container path")]
    WorkspacePathMissing(Uuid),
    #[error("Claude session {0} is not quarantined for this workspace")]
    NotQuarantined(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CliSessionKind {
    /// A real, interactive human CLI session (entrypoint "cli", or anything
    /// we cannot positively identify as a background subagent).
    Main,
    /// A programmatically spawned background agent (entrypoint "sdk-cli" / "sdk-py").
    Subagent,
}

impl CliSessionKind {
    /// Classify a session from its transcript `entrypoint`.
    ///
    /// FAIL OPEN TO MAIN: we hide a session from the default view ONLY when it
    /// is positively identified as a background subagent. Every other case —
    /// an unrecognized entrypoint, a value added by a future Claude version, a
    /// missing field, or a transcript we failed to read — classifies as Main so
    /// a real conversation is never hidden behind the agents toggle. Do NOT
    /// rewrite this as a closed match on the known values; the default arm is
    /// load-bearing.
    pub fn from_entrypoint(entrypoint: Option<&str>) -> Self {
        match entrypoint {
            Some("sdk-cli") | Some("sdk-py") => Self::Subagent,
            // Anything else (including None / unknown) -> visible Main.
            _ => Self::Main,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct UnassignedCliSession {
    pub claude_session_id: String,
    pub cwd: String,
    pub dir_path: String,
    pub file_name: String,
    pub mtime_ms: Option<i64>,
    pub first_prompt_snippet: Option<String>,
    pub kind: CliSessionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeFeedUpdate {
    RecordsAppended {
        session_id: Uuid,
        seq: i64,
        revision: u64,
    },
    RevisionInvalidated {
        session_id: Uuid,
        revision: u64,
    },
}

#[derive(Debug, Clone)]
struct DirectoryContext {
    workspace_id: Uuid,
    cwd: PathBuf,
}

/// A session's cached projection plus the links it was built under.
struct SessionProjection {
    projection: NativeProjection,
    linked_sids: Vec<String>,
}

type ProjectionCell = Arc<tokio::sync::Mutex<Option<SessionProjection>>>;

struct CachedProjection {
    cell: ProjectionCell,
    last_used: u64,
}

#[derive(Default)]
struct ProjectionCache {
    sessions: HashMap<Uuid, CachedProjection>,
    /// Live feed claims per session. Kept apart from `sessions` so evicting a
    /// projection never loses a claim.
    subscribers: HashMap<Uuid, usize>,
    clock: u64,
}

impl ProjectionCache {
    fn cell(&mut self, session_id: Uuid) -> ProjectionCell {
        self.clock += 1;
        let clock = self.clock;
        let cached = self
            .sessions
            .entry(session_id)
            .or_insert_with(|| CachedProjection {
                cell: Arc::default(),
                last_used: clock,
            });
        cached.last_used = clock;
        let cell = cached.cell.clone();
        if self.sessions.len() > PROJECTION_CACHE_CAPACITY
            && let Some(evict) = self
                .sessions
                .iter()
                .filter(|(id, _)| **id != session_id)
                // Unsubscribed sessions go first, least recently used first.
                .min_by_key(|(id, cached)| (self.subscribers.contains_key(*id), cached.last_used))
                .map(|(id, _)| *id)
        {
            // An evicted session that still has subscribers rebuilds on its
            // next update.
            self.sessions.remove(&evict);
        }
        cell
    }
}

/// One feed consumer's claim on a session's cached projection. The cache
/// drops the projection when the last claim is released.
pub struct NativeFeedSubscription {
    ingest: Arc<ClaudeTranscriptIngest>,
    session_id: Uuid,
}

impl Drop for NativeFeedSubscription {
    fn drop(&mut self) {
        let mut cache = self.ingest.projections.lock().unwrap();
        let Some(claims) = cache.subscribers.get_mut(&self.session_id) else {
            return;
        };
        *claims -= 1;
        if *claims == 0 {
            cache.subscribers.remove(&self.session_id);
            cache.sessions.remove(&self.session_id);
        }
    }
}

/// A feed update for one consumer: the whole projection, or only what changed
/// since the consumer's cursor.
#[derive(Debug)]
pub enum NativeFeedChange {
    Full(NativeFeedSnapshot),
    Delta {
        revision: u64,
        seq: i64,
        appended_from: usize,
        appended: Vec<NativeFeedEntry>,
        replaced: Vec<(usize, NativeFeedEntry)>,
        forks: Option<Vec<NativeFeedFork>>,
        health: NativeIngestHealth,
    },
}

#[derive(Debug, Default, Clone, Copy)]
struct ScanCounts {
    directories: u64,
    files: u64,
    failed_files: u64,
    records: u64,
}

#[derive(Debug, Default)]
struct ImportPathState {
    pending: bool,
    force_rescan: bool,
}

#[cfg(test)]
struct TestNoPaneWriterProbe;

#[cfg(test)]
#[async_trait::async_trait]
impl CliWriterProbe for TestNoPaneWriterProbe {
    async fn probe(
        &self,
        _workspace_id: Uuid,
        _effective_dir: &Path,
        _expected_sid: Option<&str>,
        _binding: Option<&CliPaneBinding>,
        _check_cwd_uniqueness: bool,
    ) -> crate::services::cli_collab::ProbeReport {
        crate::services::cli_collab::ProbeReport {
            pane_session_exists: false,
            agent_running: Some(false),
            sid_evidence: SidEvidence::Unknown,
            probe_failed: false,
            only_active_claude_in_cwd: Some(false),
        }
    }
}

pub struct ClaudeTranscriptIngest {
    db: DBService,
    projects_dir: PathBuf,
    /// `$CODEX_HOME/sessions`; `None` disables Codex rollout import.
    codex_sessions_dir: Option<PathBuf>,
    /// Codex rollouts to import, by day directory and path. Codex keeps every
    /// thread of every project in shared day directories, so only these files
    /// are read there.
    codex_files: RwLock<HashMap<PathBuf, HashMap<PathBuf, DirectoryContext>>>,
    /// Size and mtime at the last import of each Codex rollout, so events for
    /// other threads in a shared day directory cost one `stat` per file.
    /// Codex only appends to a rollout; a rewrite that keeps both is caught by
    /// a forced rescan, not by this skip.
    codex_seen: Mutex<HashMap<PathBuf, (i64, Option<i64>)>>,
    codex_path_cache: RwLock<HashMap<String, PathBuf>>,
    /// Threads a hook asked to import since the last registry pass; each asks
    /// once per pass, so a thread that cannot be bound does not turn every
    /// hook event into a pass.
    codex_nudged: Mutex<HashSet<String>>,
    registry_nudge: Notify,
    writer_probe: Arc<dyn CliWriterProbe>,
    directories: RwLock<HashMap<PathBuf, DirectoryContext>>,
    watchers: tokio::sync::Mutex<HashMap<PathBuf, JoinHandle<()>>>,
    importing_paths: tokio::sync::Mutex<HashMap<PathBuf, ImportPathState>>,
    sid_dir_cache: RwLock<HashMap<String, PathBuf>>,
    quarantined_paths: Mutex<HashSet<PathBuf>>,
    unknown_kinds: AtomicU64,
    rescans: AtomicU64,
    degraded_watchers: RwLock<HashSet<PathBuf>>,
    revisions: RwLock<HashMap<Uuid, u64>>,
    feed_updates: broadcast::Sender<NativeFeedUpdate>,
    publisher_notify: Notify,
    projections: Mutex<ProjectionCache>,
    import_permits: Semaphore,
    #[cfg(test)]
    snapshot_watermark_barrier: tokio::sync::Mutex<Option<Arc<tokio::sync::Barrier>>>,
    #[cfg(test)]
    path_import_barrier: tokio::sync::Mutex<Option<Arc<tokio::sync::Barrier>>>,
}

impl ClaudeTranscriptIngest {
    /// Check the feature gate exactly once at startup.
    ///
    /// The ingest is on by default. `DISABLE_CLI_TRANSCRIPT_INGEST` makes this
    /// return `None` — no watcher, no publisher, no registry reconcile.
    ///
    /// Retention is the deliberate exception and is spawned either way. It is
    /// the pruner for `cli_native_record`/`cli_native_file`; gating it behind
    /// the feature flag would strand every row an earlier enabled run wrote,
    /// growing the database forever and making every write-lock hold slower.
    /// Turning the feature off must let the existing data drain, not freeze it.
    pub fn spawn(
        db: DBService,
        writer_probe: Arc<dyn CliWriterProbe>,
        shutdown: CancellationToken,
    ) -> Option<Arc<Self>> {
        let projects_dir = dirs::home_dir()?.join(".claude").join("projects");
        let mut service = Self::new_with_probe(db, projects_dir, writer_probe);
        service.codex_sessions_dir =
            executors::executors::codex::codex_home().map(|home| home.join("sessions"));
        let service = Arc::new(service);

        if !utils::feature_flags::cli_transcript_ingest_enabled() {
            tracing::info!(
                flag = utils::feature_flags::CLI_TRANSCRIPT_INGEST_DISABLE_ENV,
                "CLI transcript ingest is disabled; running retention only"
            );
            tokio::spawn(service.run_retention(shutdown.child_token()));
            return None;
        }

        tokio::spawn(service.clone().run_publisher(shutdown.child_token()));
        let native_link_updates = native_link_events().subscribe();
        tokio::spawn(
            service
                .clone()
                .run_native_link_invalidation(native_link_updates, shutdown.child_token()),
        );
        tokio::spawn(service.clone().run_registry(shutdown.child_token(), true));
        tokio::spawn(service.clone().run_retention(shutdown.child_token()));
        Some(service)
    }

    /// Periodically reclaims transcripts no session can reach. Without this the
    /// raw-record table grows for the lifetime of the install, and a larger
    /// database makes every commit — and so every write-lock hold — slower.
    async fn run_retention(self: Arc<Self>, shutdown: CancellationToken) {
        let Some(days) = retention_days() else {
            tracing::info!("CLI transcript retention disabled by {RETENTION_ENV}");
            return;
        };
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + RETENTION_STARTUP_DELAY,
            RETENTION_SWEEP_INTERVAL,
        );
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = interval.tick() => {}
            }
            match CliNativeFile::prune_unreachable(&self.db.pool, days, RETENTION_FILES_PER_SWEEP)
                .await
            {
                Ok(pruned) if pruned.is_empty() => {}
                Ok(pruned) => tracing::info!(
                    files = pruned.files,
                    records = pruned.records,
                    retention_days = days,
                    "pruned unreachable CLI transcripts"
                ),
                Err(err) => {
                    tracing::warn!(%err, "CLI transcript retention sweep failed")
                }
            }
        }
    }

    fn new_with_probe(
        db: DBService,
        projects_dir: PathBuf,
        writer_probe: Arc<dyn CliWriterProbe>,
    ) -> Self {
        let (feed_updates, _) = broadcast::channel(4096);
        Self {
            db,
            projects_dir,
            codex_sessions_dir: None,
            codex_files: RwLock::new(HashMap::new()),
            codex_seen: Mutex::new(HashMap::new()),
            codex_path_cache: RwLock::new(HashMap::new()),
            codex_nudged: Mutex::new(HashSet::new()),
            registry_nudge: Notify::new(),
            writer_probe,
            directories: RwLock::new(HashMap::new()),
            watchers: tokio::sync::Mutex::new(HashMap::new()),
            importing_paths: tokio::sync::Mutex::new(HashMap::new()),
            sid_dir_cache: RwLock::new(HashMap::new()),
            quarantined_paths: Mutex::new(HashSet::new()),
            unknown_kinds: AtomicU64::new(0),
            rescans: AtomicU64::new(0),
            degraded_watchers: RwLock::new(HashSet::new()),
            revisions: RwLock::new(HashMap::new()),
            feed_updates,
            publisher_notify: Notify::new(),
            projections: Mutex::new(ProjectionCache::default()),
            import_permits: Semaphore::new(CONCURRENT_IMPORT_BATCHES),
            #[cfg(test)]
            snapshot_watermark_barrier: tokio::sync::Mutex::new(None),
            #[cfg(test)]
            path_import_barrier: tokio::sync::Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn new(db: DBService, projects_dir: PathBuf) -> Self {
        Self::new_with_probe(db, projects_dir, Arc::new(TestNoPaneWriterProbe))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<NativeFeedUpdate> {
        self.feed_updates.subscribe()
    }

    /// Keep `session_id`'s projection cached while the returned claim lives.
    pub fn subscribe_feed(self: &Arc<Self>, session_id: Uuid) -> NativeFeedSubscription {
        let mut cache = self.projections.lock().unwrap();
        *cache.subscribers.entry(session_id).or_default() += 1;
        cache.cell(session_id);
        NativeFeedSubscription {
            ingest: self.clone(),
            session_id,
        }
    }

    #[cfg(test)]
    async fn snapshot(
        &self,
        session_id: Uuid,
    ) -> Result<NativeFeedSnapshot, ClaudeTranscriptIngestError> {
        match self.feed_since(session_id, None).await?.0 {
            NativeFeedChange::Full(snapshot) => Ok(snapshot),
            NativeFeedChange::Delta { .. } => unreachable!("no cursor always yields a snapshot"),
        }
    }

    /// Bring the session's cached projection up to date and describe it
    /// relative to `since`.
    ///
    /// With an unchanged revision and link set, only rows above the cached
    /// `seq` are read and adapted. Anything an append cannot express — a new
    /// revision, a removed link, a replaced file generation — rebuilds the
    /// projection, which also invalidates every earlier cursor.
    pub async fn feed_since(
        &self,
        session_id: Uuid,
        since: Option<NativeFeedCursor>,
    ) -> Result<(NativeFeedChange, NativeFeedCursor), ClaudeTranscriptIngestError> {
        let session = Session::find_by_id(&self.db.pool, session_id)
            .await?
            .ok_or(ClaudeTranscriptIngestError::SessionNotFound(session_id))?;
        let cell = self.projections.lock().unwrap().cell(session_id);
        let mut cached = cell.lock().await;

        // Capture both live-stream watermarks before reading projection rows.
        // With subscribe-before-snapshot, a later import/reset is queued for
        // the subscriber, while anything included in `seq` is guaranteed to
        // be visible to the subsequent rows query after its atomic commit.
        let revision = self.revision(session_id).await;
        let seq = CliIngestOutbox::latest_seq(&self.db.pool, session_id).await?;
        #[cfg(test)]
        self.wait_at_snapshot_watermark().await;
        let linked_sids =
            ClaudeSessionLink::claude_session_ids_for_session(&self.db.pool, session_id).await?;

        // Any link change rebuilds. A removed link drops rows, and a restored
        // one brings back outbox rows whose seq can sit below the cached one.
        // Link mutations outside this service (the CLI launch path) do not
        // bump the revision, so the link set is checked on every update.
        let mut extended = false;
        if let Some(current) = cached.as_mut()
            && current.projection.revision() == revision
            && current.linked_sids == linked_sids
        {
            let rows = CliNativeRecord::list_for_session_after(
                &self.db.pool,
                session_id,
                current.projection.last_row_seq(),
            )
            .await?;
            if current.projection.can_extend(&rows) {
                current.projection.extend(&rows);
                extended = true;
            }
        }
        if !extended {
            let rows = CliNativeRecord::list_for_session(&self.db.pool, session_id).await?;
            *cached = Some(SessionProjection {
                projection: NativeProjection::build(&rows, revision),
                linked_sids,
            });
        }
        let projection = &cached
            .as_ref()
            .expect("projection was just built")
            .projection;
        let mut health = self.ingest_health(&session).await?;
        let cursor = projection.cursor();
        let delta = since.and_then(|since| projection.delta_since(since));
        let change = match delta {
            None => NativeFeedChange::Full(projection.snapshot(seq, health)),
            Some(delta) => {
                health.files = projection.files().to_vec();
                NativeFeedChange::Delta {
                    revision: projection.revision(),
                    seq,
                    appended_from: delta.appended_from,
                    appended: delta.appended.to_vec(),
                    replaced: delta
                        .replaced
                        .into_iter()
                        .map(|(index, entry)| (index, entry.clone()))
                        .collect(),
                    forks: delta.forks_changed.then(|| projection.forks().to_vec()),
                    health,
                }
            }
        };
        Ok((change, cursor))
    }

    /// Ingest health for `session`, without the per-file list the projection
    /// supplies.
    async fn ingest_health(
        &self,
        session: &Session,
    ) -> Result<NativeIngestHealth, ClaudeTranscriptIngestError> {
        let degraded_paths = self.degraded_watchers.read().await.clone();
        let directories = self.directories.read().await;
        let codex_files = self.codex_files.read().await;
        let watch_degraded = degraded_paths.iter().any(|path| {
            directories
                .get(path)
                .is_some_and(|context| context.workspace_id == session.workspace_id)
                || codex_files.get(path).is_some_and(|files| {
                    files
                        .values()
                        .any(|context| context.workspace_id == session.workspace_id)
                })
        });
        drop(directories);
        drop(codex_files);
        let quarantined_files = self.quarantined_paths.lock().unwrap().len() as u64;
        let foreign_writer_seen_at =
            ClaudeSessionLink::latest_foreign_writer_seen_for_session(&self.db.pool, session.id)
                .await?
                .map(|time| time.to_rfc3339());
        Ok(NativeIngestHealth {
            unknown_kinds: self.unknown_kinds.load(Ordering::Relaxed),
            rescans: self.rescans.load(Ordering::Relaxed),
            quarantined_files,
            watch_degraded,
            foreign_writer_seen_at,
            files: Vec::new(),
        })
    }

    #[cfg(test)]
    async fn wait_at_snapshot_watermark(&self) {
        let barrier = self.snapshot_watermark_barrier.lock().await.clone();
        if let Some(barrier) = barrier {
            barrier.wait().await;
            barrier.wait().await;
        }
    }

    pub async fn list_unassigned(
        &self,
        workspace_id: Uuid,
    ) -> Result<Vec<UnassignedCliSession>, ClaudeTranscriptIngestError> {
        let workspace = Workspace::find_by_id(&self.db.pool, workspace_id)
            .await?
            .ok_or(WorkspaceError::WorkspaceNotFound)?;
        let session = Session::find_latest_by_workspace_id(&self.db.pool, workspace_id).await?;
        let cwd = session
            .as_ref()
            .and_then(|session| {
                workspace
                    .container_ref
                    .as_deref()
                    .and_then(|container_ref| {
                        session.effective_working_dir(Path::new(container_ref))
                    })
            })
            .or_else(|| {
                workspace
                    .container_ref
                    .as_deref()
                    .filter(|container_ref| !container_ref.is_empty())
                    .map(PathBuf::from)
            })
            .ok_or(ClaudeTranscriptIngestError::WorkspacePathMissing(
                workspace_id,
            ))?;

        let files =
            CliNativeFile::list_unassigned_for_workspace(&self.db.pool, workspace_id).await?;
        Ok(files
            .into_iter()
            .map(|file| {
                let path = Path::new(&file.dir_path).join(&file.file_name);
                let preview = read_session_preview(&path, &file.claude_session_id);
                UnassignedCliSession {
                    claude_session_id: file.claude_session_id.clone(),
                    cwd: cwd.to_string_lossy().into_owned(),
                    dir_path: file.dir_path,
                    file_name: file.file_name,
                    mtime_ms: file.observed_mtime_ms,
                    first_prompt_snippet: preview.first_prompt_snippet,
                    kind: preview.kind,
                }
            })
            .collect())
    }

    pub async fn assign_manual(
        &self,
        claude_session_id: &str,
        session_id: Uuid,
    ) -> Result<(), ClaudeTranscriptIngestError> {
        let session = Session::find_by_id(&self.db.pool, session_id)
            .await?
            .ok_or(ClaudeTranscriptIngestError::SessionNotFound(session_id))?;
        let workspace = Workspace::find_by_id(&self.db.pool, session.workspace_id)
            .await?
            .ok_or(WorkspaceError::WorkspaceNotFound)?;
        let cwd = workspace
            .container_ref
            .as_deref()
            .and_then(|container_ref| session.effective_working_dir(Path::new(container_ref)))
            .ok_or(ClaudeTranscriptIngestError::WorkspacePathMissing(
                session.workspace_id,
            ))?;
        let files = CliNativeFile::list_latest_by_sid(&self.db.pool, claude_session_id).await?;
        if files.is_empty()
            || !files
                .iter()
                .any(|file| file.discovered_workspace_id == Some(session.workspace_id))
        {
            return Err(ClaudeTranscriptIngestError::NotQuarantined(
                claude_session_id.to_string(),
            ));
        }
        if ClaudeSessionLink::find(&self.db.pool, claude_session_id)
            .await?
            .is_some()
        {
            return Err(ClaudeTranscriptIngestError::NotQuarantined(
                claude_session_id.to_string(),
            ));
        }

        let mutation = ClaudeSessionLink::assign_manual(
            &self.db.pool,
            claude_session_id,
            session_id,
            &cwd.to_string_lossy(),
        )
        .await?
        .ok_or(ClaudeTranscriptIngestError::SessionNotFound(session_id))?;
        self.apply_link_mutation(&mutation).await;

        for file in files {
            let path = Path::new(&file.dir_path).join(&file.file_name);
            self.quarantined_paths.lock().unwrap().remove(&path);
            let context = DirectoryContext {
                workspace_id: session.workspace_id,
                cwd: cwd.clone(),
            };
            self.process_native_path(&path, &context, false).await?;
        }
        self.publisher_notify.notify_one();
        Ok(())
    }

    async fn run_registry(self: Arc<Self>, shutdown: CancellationToken, start_watchers: bool) {
        let started = std::time::Instant::now();
        match self
            .reconcile_registry(start_watchers, shutdown.child_token())
            .await
        {
            Ok(counts) => tracing::info!(
                directories = counts.directories,
                files = counts.files,
                failed_files = counts.failed_files,
                records = counts.records,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "initial CLI transcript backfill finished"
            ),
            Err(error) => tracing::warn!(?error, "initial CLI transcript reconciliation failed"),
        }
        let mut interval = tokio::time::interval(REGISTRY_RECONCILE_INTERVAL);
        interval.tick().await;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {}
                _ = self.registry_nudge.notified() => {}
            }
            if let Err(error) = self
                .reconcile_registry(start_watchers, shutdown.child_token())
                .await
            {
                tracing::warn!(?error, "CLI transcript reconciliation failed");
            }
        }
    }

    async fn reconcile_registry(
        self: &Arc<Self>,
        start_watchers: bool,
        shutdown: CancellationToken,
    ) -> Result<ScanCounts, ClaudeTranscriptIngestError> {
        let mut desired = HashMap::new();
        let mut codex_desired = HashMap::new();
        let claude_store = self.projects_dir.is_dir();
        let codex_root = self.codex_sessions_dir.clone().filter(|root| root.is_dir());
        if claude_store || codex_root.is_some() {
            for workspace in Workspace::fetch_all(&self.db.pool).await? {
                if workspace.archived || workspace.worktree_deleted {
                    continue;
                }
                let Some(session) =
                    Session::find_latest_by_workspace_id(&self.db.pool, workspace.id).await?
                else {
                    continue;
                };
                let Some(cwd) = workspace
                    .container_ref
                    .as_deref()
                    .and_then(|container_ref| {
                        session.effective_working_dir(Path::new(container_ref))
                    })
                else {
                    continue;
                };
                let context = DirectoryContext {
                    workspace_id: workspace.id,
                    cwd: cwd.clone(),
                };
                if let Some(root) = &codex_root {
                    for path in self.codex_cli_rollouts(root, &workspace).await? {
                        codex_desired.insert(path, context.clone());
                    }
                }
                if !claude_store {
                    continue;
                }
                let computed_dir = self.projects_dir.join(claude_project_slug(&cwd));
                if computed_dir.is_dir() {
                    let dir = fs::canonicalize(&computed_dir).unwrap_or(computed_dir.clone());
                    desired.insert(dir, context.clone());
                }

                for sid in
                    ClaudeSessionLink::known_session_ids_for_workspace(&self.db.pool, workspace.id)
                        .await?
                {
                    // Codex thread ids (UUIDv7) are handled above; Claude's
                    // are v4, so a v7 id is never in the Claude store.
                    if is_codex_thread_id(&sid) {
                        continue;
                    }
                    if computed_dir.join(format!("{sid}.jsonl")).is_file() {
                        continue;
                    }
                    if let Some(found_dir) = self.locate_sid(&sid).await? {
                        let dir = fs::canonicalize(&found_dir).unwrap_or(found_dir);
                        desired.insert(dir, context.clone());
                    }
                }
            }
        }
        self.reconcile_directories(desired, codex_desired, start_watchers, shutdown)
            .await
    }

    async fn reconcile_directories(
        self: &Arc<Self>,
        desired: HashMap<PathBuf, DirectoryContext>,
        codex_desired: HashMap<PathBuf, DirectoryContext>,
        start_watchers: bool,
        shutdown: CancellationToken,
    ) -> Result<ScanCounts, ClaudeTranscriptIngestError> {
        let scan_dirs = desired
            .keys()
            .cloned()
            .chain(
                codex_desired
                    .keys()
                    .filter_map(|path| path.parent().map(Path::to_path_buf)),
            )
            .collect::<HashSet<_>>();
        let watched_paths = if start_watchers {
            scan_dirs.clone()
        } else {
            HashSet::new()
        };
        *self.directories.write().await = desired;
        self.codex_seen
            .lock()
            .unwrap()
            .retain(|path, _| codex_desired.contains_key(path));
        self.codex_path_cache
            .write()
            .await
            .retain(|_, path| codex_desired.contains_key(path));
        // This pass served every earlier request; a thread that still cannot
        // be bound may ask again, at most once per pass.
        self.codex_nudged.lock().unwrap().clear();
        let mut codex_by_dir = HashMap::<PathBuf, HashMap<PathBuf, DirectoryContext>>::new();
        for (path, context) in codex_desired {
            if let Some(dir) = path.parent() {
                codex_by_dir
                    .entry(dir.to_path_buf())
                    .or_default()
                    .insert(path, context);
            }
        }
        *self.codex_files.write().await = codex_by_dir;

        let mut removed = Vec::new();
        {
            let mut watchers = self.watchers.lock().await;
            let stale_or_finished = watchers
                .iter()
                .filter_map(|(path, handle)| {
                    (!watched_paths.contains(path) || handle.is_finished()).then_some(path.clone())
                })
                .collect::<Vec<_>>();
            for path in stale_or_finished {
                if let Some(handle) = watchers.remove(&path) {
                    if !handle.is_finished() {
                        handle.abort();
                    }
                    removed.push(path);
                }
            }
        }
        if !removed.is_empty() {
            let mut degraded = self.degraded_watchers.write().await;
            for path in removed {
                degraded.remove(&path);
            }
        }
        self.degraded_watchers
            .write()
            .await
            .retain(|path| watched_paths.contains(path));

        if start_watchers {
            for dir in &watched_paths {
                self.ensure_watcher(dir.clone(), shutdown.child_token())
                    .await;
            }
        }
        let mut counts = ScanCounts::default();
        for dir in scan_dirs {
            let scanned = self.scan_directory(&dir, false).await?;
            counts.directories += 1;
            counts.files += scanned.files;
            counts.failed_files += scanned.failed_files;
            counts.records += scanned.records;
        }
        Ok(counts)
    }

    async fn ensure_watcher(self: &Arc<Self>, dir: PathBuf, shutdown: CancellationToken) {
        let mut watchers = self.watchers.lock().await;
        if let Some(handle) = watchers.get(&dir) {
            if !handle.is_finished() {
                return;
            }
            watchers.remove(&dir);
        }
        let service = self.clone();
        let watched_dir = dir.clone();
        let handle = tokio::spawn(async move {
            let Ok((fs_guard, mut receiver, _)) =
                filesystem_watcher::async_watcher(watched_dir.clone())
            else {
                service
                    .degraded_watchers
                    .write()
                    .await
                    .insert(watched_dir.clone());
                tracing::warn!(path = %watched_dir.display(), "native transcript watcher unavailable; using reconcile polling");
                return;
            };
            service.degraded_watchers.write().await.remove(&watched_dir);
            let _fs_guard = fs_guard;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    event = receiver.next() => match event {
                        Some(Ok(_)) => {
                            service.degraded_watchers.write().await.remove(&watched_dir);
                            if let Err(error) = service.scan_directory(&watched_dir, false).await {
                                tracing::warn!(?error, path = %watched_dir.display(), "native transcript watch scan failed");
                            }
                        }
                        Some(Err(error)) => {
                            service
                                .degraded_watchers
                                .write()
                                .await
                                .insert(watched_dir.clone());
                            tracing::warn!(?error, path = %watched_dir.display(), "native transcript watcher error; forcing rescan");
                            if let Err(error) = service.scan_directory(&watched_dir, true).await {
                                tracing::warn!(?error, path = %watched_dir.display(), "native transcript forced rescan failed");
                            }
                            break;
                        }
                        None => {
                            service
                                .degraded_watchers
                                .write()
                                .await
                                .insert(watched_dir.clone());
                            break;
                        },
                    }
                }
            }
        });
        watchers.insert(dir, handle);
    }

    async fn scan_directory(
        &self,
        dir: &Path,
        force_rescan: bool,
    ) -> Result<ScanCounts, ClaudeTranscriptIngestError> {
        let mut counts = self.scan_codex_files(dir, force_rescan).await;
        let Some(context) = self.directories.read().await.get(dir).cloned() else {
            return Ok(counts);
        };
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl")
                || file_name.starts_with("agent-")
            {
                continue;
            }
            counts.files += 1;
            match self
                .process_native_path(&path, &context, force_rescan)
                .await
            {
                Ok(records) => counts.records += records,
                Err(error) => {
                    counts.failed_files += 1;
                    tracing::warn!(?error, path = %path.display(), "native transcript import failed")
                }
            }
        }
        Ok(counts)
    }

    /// Import new lines of one transcript; returns the records inserted.
    async fn process_native_path(
        &self,
        path: &Path,
        context: &DirectoryContext,
        force_rescan: bool,
    ) -> Result<u64, ClaudeTranscriptIngestError> {
        let path = path.to_path_buf();
        {
            let mut importing = self.importing_paths.lock().await;
            if let Some(state) = importing.get_mut(&path) {
                state.pending = true;
                state.force_rescan |= force_rescan;
                return Ok(0);
            }
            importing.insert(path.clone(), ImportPathState::default());
        }

        let mut next_force_rescan = force_rescan;
        let mut inserted = 0;
        loop {
            let result = self
                .process_native_path_inner(&path, context, next_force_rescan)
                .await;
            if let Ok(records) = &result {
                inserted += records;
            }
            #[cfg(test)]
            if let Some(barrier) = self.path_import_barrier.lock().await.take() {
                barrier.wait().await;
                barrier.wait().await;
            }

            let mut importing = self.importing_paths.lock().await;
            let state = importing
                .get_mut(&path)
                .expect("active import path state exists");
            if state.pending {
                next_force_rescan = state.force_rescan;
                state.pending = false;
                state.force_rescan = false;
                drop(importing);
                if let Err(error) = result {
                    tracing::warn!(?error, path = %path.display(), "retrying native transcript path after pending event");
                }
                continue;
            }
            importing.remove(&path);
            return result.map(|_| inserted);
        }
    }

    async fn process_native_path_inner(
        &self,
        path: &Path,
        context: &DirectoryContext,
        force_rescan: bool,
    ) -> Result<u64, ClaudeTranscriptIngestError> {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let codex_thread_id = rollout_thread_id(file_name);
        let Some(claude_session_id) =
            codex_thread_id.or_else(|| path.file_stem().and_then(|stem| stem.to_str()))
        else {
            return Ok(0);
        };
        let is_codex = codex_thread_id.is_some();
        // Cleared until this import completes: a failed import, or a scan
        // coalesced into this one, must not leave the file looking current.
        if is_codex {
            self.codex_seen.lock().unwrap().remove(path);
        }
        let dir_path = path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_string_lossy()
            .into_owned();
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Ok(0);
        }
        let (dev, inode) = file_identity(&metadata);
        let observed_size = file_size(&metadata);
        let observed_mtime_ms = modified_ms(&metadata);
        let registration = RegisterCliNativeFile {
            claude_session_id,
            dir_path: &dir_path,
            file_name,
            discovered_workspace_id: Some(context.workspace_id),
            dev,
            inode,
            observed_size,
            observed_mtime_ms,
        };
        let mut native_file = CliNativeFile::register(&self.db.pool, &registration).await?;

        // A Codex rollout is only tracked once a CLI pane's binding linked it,
        // and the executor never writes to a thread another session owns (a
        // chat follow-up forks a new thread), so its link is read, not
        // re-derived with a write on every append.
        let mut link = if is_codex {
            ClaudeSessionLink::find(&self.db.pool, claude_session_id)
                .await?
                .map(|link| ClaudeSessionLinkMutation {
                    previous_session_id: Some(link.session_id),
                    link,
                    republished_outbox: 0,
                })
        } else {
            ClaudeSessionLink::resolve_or_bind_executor(
                &self.db.pool,
                claude_session_id,
                &context.cwd.to_string_lossy(),
            )
            .await?
        };
        // Fresh Codex panes are bound during registry reconciliation; this
        // probe only recognises a Claude process.
        if link.is_none() && !is_codex {
            link = self
                .try_auto_bind_cli_fresh(claude_session_id, context)
                .await?;
        }
        if let Some(mutation) = &link {
            self.apply_link_mutation(mutation).await;
        } else {
            self.quarantined_paths
                .lock()
                .unwrap()
                .insert(path.to_path_buf());
        }
        if link.is_some() {
            self.quarantined_paths.lock().unwrap().remove(path);
        }

        // The writer probe only recognises a Claude process, so a live Codex
        // pane would read as absent; Codex keeps the fail-closed default and
        // is never flagged as a foreign writer.
        let import_context = if link.is_some() && !is_codex {
            let report = self
                .writer_probe
                .probe(
                    context.workspace_id,
                    &context.cwd,
                    Some(claude_session_id),
                    None,
                    false,
                )
                .await;
            NativeImportContext {
                app_pane_absent: !(report.probe_failed
                    || report.pane_session_exists && report.agent_running == Some(true)),
            }
        } else {
            NativeImportContext::default()
        };

        let verified_hash = if observed_size >= native_file.cursor_offset {
            verify_last_line_hash(&mut file, &native_file)?
        } else {
            None
        };
        let reason = rescan_reason(
            StoredTailState {
                dev: native_file.dev,
                inode: native_file.inode,
                cursor_offset: native_file.cursor_offset,
                last_line_hash: native_file.last_line_hash.as_deref(),
            },
            ObservedFileState {
                dev,
                inode,
                size: observed_size,
                verified_last_line_hash: verified_hash.as_deref(),
            },
            force_rescan,
        );
        let mut activate_replacement = reason.is_some();
        let (mut cursor_offset, mut next_line_seq, mut last_line_offset, mut last_line_hash) =
            if activate_replacement {
                (0, 0, 0, None)
            } else {
                (
                    native_file.cursor_offset,
                    native_file.next_line_seq,
                    native_file.last_line_offset,
                    native_file.last_line_hash.clone(),
                )
            };
        if activate_replacement {
            file.seek(SeekFrom::Start(0))?;
        } else {
            file.seek(SeekFrom::Start(native_file.cursor_offset as u64))?;
        }
        let mut reader = BufReader::new(file);
        let mut inserted = 0;

        loop {
            let tail = read_complete_line_batch(
                &mut reader,
                cursor_offset,
                next_line_seq,
                IMPORT_BATCH_LINE_LIMIT,
            )?;
            if tail.lines.is_empty() {
                break;
            }

            let mut records = Vec::new();
            let codex_cwd = context.cwd.to_string_lossy();
            for complete in &tail.lines {
                if is_codex {
                    let (record, unknown) = codex_native_record(
                        complete.line_seq,
                        claude_session_id,
                        &complete.raw,
                        &codex_cwd,
                    );
                    if unknown {
                        self.unknown_kinds.fetch_add(1, Ordering::Relaxed);
                    }
                    records.extend(record);
                    continue;
                }
                match adapt_native_claude_line(&complete.raw, claude_session_id) {
                    Ok(line) => {
                        let disposition = match line.disposition() {
                            NativeClaudeDisposition::Renderable => {
                                CliNativeRecordDisposition::Renderable
                            }
                            NativeClaudeDisposition::Skip(NativeClaudeSkipReason::Bookkeeping) => {
                                CliNativeRecordDisposition::Bookkeeping
                            }
                            NativeClaudeDisposition::Skip(NativeClaudeSkipReason::Sidechain) => {
                                CliNativeRecordDisposition::Sidechain
                            }
                            NativeClaudeDisposition::Unknown => CliNativeRecordDisposition::Unknown,
                        };
                        if line.is_unknown() {
                            self.unknown_kinds.fetch_add(1, Ordering::Relaxed);
                        }
                        let envelope = line.metadata();
                        records.push(NewCliNativeRecord {
                            line_seq: complete.line_seq,
                            claude_session_id: claude_session_id.to_string(),
                            uuid: envelope.uuid.clone(),
                            parent_uuid: envelope.parent_uuid.clone(),
                            kind: envelope.kind.clone(),
                            ts: envelope.timestamp.clone(),
                            raw: complete.raw.clone(),
                            disposition,
                            user_prompt: line.plain_user_text(),
                            recorded_at: envelope
                                .timestamp
                                .as_deref()
                                .and_then(parse_native_timestamp),
                        });
                    }
                    Err(_) => {
                        self.unknown_kinds.fetch_add(1, Ordering::Relaxed);
                        records.push(NewCliNativeRecord {
                            line_seq: complete.line_seq,
                            claude_session_id: claude_session_id.to_string(),
                            uuid: None,
                            parent_uuid: None,
                            kind: "unknown".to_string(),
                            ts: None,
                            raw: complete.raw.clone(),
                            disposition: CliNativeRecordDisposition::Unknown,
                            user_prompt: None,
                            recorded_at: None,
                        });
                    }
                }
            }
            let batch_last_line_offset = tail.last_line_offset.unwrap_or(last_line_offset);
            let batch_last_line_hash = tail.last_line_hash.as_deref().or(last_line_hash.as_deref());
            let cursor = ImportedCursor {
                cursor_offset: tail.cursor_offset,
                next_line_seq: tail.next_line_seq,
                last_line_offset: batch_last_line_offset,
                last_line_hash: batch_last_line_hash,
                observed_size,
                observed_mtime_ms,
            };
            let permit = self
                .import_permits
                .acquire()
                .await
                .expect("the import semaphore is never closed");
            let imported = if activate_replacement {
                let replacement =
                    CliNativeRecord::replace_generation_and_import_batch_with_context(
                        &self.db.pool,
                        &registration,
                        &records,
                        &cursor,
                        import_context,
                    )
                    .await?;
                native_file = replacement.file;
                activate_replacement = false;
                self.rescans.fetch_add(1, Ordering::Relaxed);
                if let Some(link) = &link {
                    // The replacement is committed and visible before its
                    // revision can prompt a connected feed to resnapshot.
                    self.invalidate_revision(link.link.session_id).await;
                }
                tracing::info!(?reason, path = %path.display(), generation = native_file.generation, "rescanned native transcript generation");
                replacement.imported
            } else {
                CliNativeRecord::import_batch_with_context(
                    &self.db.pool,
                    native_file.id,
                    &records,
                    &cursor,
                    import_context,
                )
                .await?
            };
            drop(permit);
            inserted += imported.inserted_records;
            if imported.appended_outbox > 0 {
                self.publisher_notify.notify_one();
            }

            cursor_offset = tail.cursor_offset;
            next_line_seq = tail.next_line_seq;
            last_line_offset = batch_last_line_offset;
            last_line_hash = tail.last_line_hash.or(last_line_hash);
            if tail.trailing_bytes > 0 {
                break;
            }
        }
        if is_codex {
            self.codex_seen
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), (observed_size, observed_mtime_ms));
        }
        Ok(inserted)
    }

    /// Import every tracked Codex rollout in `dir` that changed since its last
    /// import.
    async fn scan_codex_files(&self, dir: &Path, force_rescan: bool) -> ScanCounts {
        let mut counts = ScanCounts::default();
        let files = self
            .codex_files
            .read()
            .await
            .get(dir)
            .map(|files| {
                files
                    .iter()
                    .map(|(path, context)| (path.clone(), context.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (path, context) in files {
            counts.files += 1;
            let Ok(metadata) = fs::metadata(&path) else {
                counts.failed_files += 1;
                continue;
            };
            let stamp = (file_size(&metadata), modified_ms(&metadata));
            if !force_rescan && self.codex_seen.lock().unwrap().get(&path) == Some(&stamp) {
                continue;
            }
            match self
                .process_native_path(&path, &context, force_rescan)
                .await
            {
                Ok(records) => counts.records += records,
                Err(error) => {
                    counts.failed_files += 1;
                    tracing::warn!(?error, path = %path.display(), "Codex rollout import failed");
                }
            }
        }
        counts
    }

    /// Ask for an early registry pass when a CLI-mode Codex reports a thread
    /// not imported yet, so its first turns do not wait for the next tick.
    pub async fn request_codex_import(&self, codex_thread_id: &str) {
        let tracked = self
            .codex_files
            .read()
            .await
            .values()
            .flat_map(HashMap::keys)
            .any(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(rollout_thread_id)
                    == Some(codex_thread_id)
            });
        if !tracked
            && self
                .codex_nudged
                .lock()
                .unwrap()
                .insert(codex_thread_id.to_string())
        {
            self.registry_nudge.notify_one();
        }
    }

    /// Rollouts of the Codex threads this workspace's CLI panes ran.
    ///
    /// Only threads a pane was bound to are imported. The executor forks a new
    /// thread for every chat follow-up, each replaying the history before it,
    /// and chat already renders those turns from the executor's own logs.
    async fn codex_cli_rollouts(
        &self,
        root: &Path,
        workspace: &Workspace,
    ) -> Result<Vec<PathBuf>, ClaudeTranscriptIngestError> {
        self.bind_fresh_codex_cli(root, workspace).await?;
        let mut paths = Vec::new();
        for sid in
            CliPaneBinding::bound_session_ids_for_workspace(&self.db.pool, workspace.id).await?
        {
            if let Some(path) = self.locate_codex_rollout(root, &sid).await {
                paths.push(path);
            }
        }
        Ok(paths)
    }

    /// Bind the thread a fresh CLI-mode Codex pane started.
    ///
    /// The thread id comes from the pane's own activity hook when Codex runs
    /// with hooks. Otherwise the pane's working directory and launch time
    /// select interactive rollouts no other session owns; only a single match
    /// binds, and several are quarantined rather than guessed between. Either
    /// way the thread must have started after the pane launched, so a report
    /// or rollout from an earlier pane never binds.
    ///
    /// The ownership check, the session link and the pane binding commit in
    /// one transaction, so a concurrent owner or a released pane leaves
    /// nothing half-written.
    async fn bind_fresh_codex_cli(
        &self,
        root: &Path,
        workspace: &Workspace,
    ) -> Result<(), ClaudeTranscriptIngestError> {
        let pool = &self.db.pool;
        let Some(binding) = CliPaneBinding::find_active_for_workspace(pool, workspace.id).await?
        else {
            return Ok(());
        };
        if binding.bound_via != CliPaneBoundVia::CliFresh || binding.claude_session_id.is_some() {
            return Ok(());
        }
        let Some(session) = Session::find_by_id(pool, binding.session_id).await? else {
            return Ok(());
        };
        let is_codex = session.executor.as_deref().is_some_and(|executor| {
            matches!(
                executor.parse(),
                Ok(executors::executors::BaseCodingAgent::Codex)
            )
        });
        // The pane runs in its own session's directory, which can differ
        // from the workspace's latest session.
        let Some(cwd) = workspace
            .container_ref
            .as_deref()
            .and_then(|container_ref| session.effective_working_dir(Path::new(container_ref)))
        else {
            return Ok(());
        };
        if !is_codex {
            return Ok(());
        }
        let launched_at = binding.created_at - CODEX_FALLBACK_LAUNCH_SKEW;
        let hook_sid = WorkspaceCliActivity::find_by_workspace_id(pool, workspace.id)
            .await?
            .filter(|activity| activity.hook_at.is_some_and(|at| at >= binding.created_at))
            .and_then(|activity| activity.hook)
            .map(|hook| hook.agent_session_id)
            .filter(|sid| codex_thread_created_at(sid).is_some_and(|at| at >= launched_at));
        // The hook names the thread, but its rollout must still be an
        // interactive session this pane started, as the fallback requires.
        let hooked = match &hook_sid {
            Some(sid) => self.locate_codex_rollout(root, sid).await,
            None => None,
        };
        let root = root.to_path_buf();
        let scan_cwd = cwd.clone();
        let candidates = tokio::task::spawn_blocking(move || match (hook_sid, hooked) {
            (Some(sid), Some(path)) if codex_rollout_started_in(&path, &scan_cwd, launched_at) => {
                vec![(sid, path)]
            }
            _ => codex_rollouts_started_in(&root, &scan_cwd, launched_at),
        })
        .await
        .unwrap_or_default();
        let mut unowned = Vec::new();
        for (sid, path) in candidates {
            if ClaudeSessionLink::find(pool, &sid)
                .await?
                .is_none_or(|link| link.session_id == binding.session_id)
            {
                unowned.push((sid, path));
            }
        }
        match unowned.as_slice() {
            [] => {}
            [(sid, path)] => {
                if let Some(mutation) = CliPaneBinding::assign_discovered_session(
                    pool,
                    binding.id,
                    sid,
                    binding.session_id,
                    workspace.id,
                    &cwd.to_string_lossy(),
                )
                .await?
                {
                    self.apply_link_mutation(&mutation).await;
                    self.quarantined_paths.lock().unwrap().remove(path);
                }
            }
            ambiguous => {
                tracing::info!(
                    workspace_id = %workspace.id,
                    candidates = ambiguous.len(),
                    "several Codex rollouts match a fresh CLI pane; none bound"
                );
                self.quarantined_paths
                    .lock()
                    .unwrap()
                    .extend(ambiguous.iter().map(|(_, path)| path.clone()));
            }
        }
        Ok(())
    }

    /// The rollout of Codex thread `sid`. Thread ids are UUIDv7, so their
    /// creation time names the day directory; only the neighbouring days are
    /// read (the directory name is the local date).
    async fn locate_codex_rollout(&self, root: &Path, sid: &str) -> Option<PathBuf> {
        if !is_codex_thread_id(sid) {
            return None;
        }
        if let Some(cached) = self.codex_path_cache.read().await.get(sid).cloned()
            && cached.is_file()
        {
            return Some(cached);
        }
        let created = codex_thread_created_at(sid)?;
        let suffix = format!("-{sid}.jsonl");
        let found = (-1..=1)
            .filter_map(|days| created.checked_add_signed(chrono::Duration::days(days)))
            .map(|day| codex_day_dir(root, day))
            .filter_map(|dir| fs::read_dir(dir).ok())
            .flat_map(|entries| entries.flatten())
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&suffix))
            })?;
        self.codex_path_cache
            .write()
            .await
            .insert(sid.to_string(), found.clone());
        Some(found)
    }

    async fn try_auto_bind_cli_fresh(
        &self,
        claude_session_id: &str,
        context: &DirectoryContext,
    ) -> Result<Option<ClaudeSessionLinkMutation>, ClaudeTranscriptIngestError> {
        let Some(binding) =
            CliPaneBinding::find_active_for_workspace(&self.db.pool, context.workspace_id).await?
        else {
            return Ok(None);
        };
        if binding.bound_via != CliPaneBoundVia::CliFresh
            || binding
                .claude_session_id
                .as_deref()
                .is_some_and(|sid| sid != claude_session_id)
        {
            return Ok(None);
        }
        let report = self
            .writer_probe
            .probe(
                context.workspace_id,
                &context.cwd,
                None,
                Some(&binding),
                true,
            )
            .await;
        if report.probe_failed
            || !report.pane_session_exists
            || report.agent_running != Some(true)
            || report.sid_evidence != SidEvidence::NoResumeArg
            || report.only_active_claude_in_cwd != Some(true)
        {
            return Ok(None);
        }
        if binding.claude_session_id.is_none()
            && !CliPaneBinding::bind_discovered_sid(&self.db.pool, binding.id, claude_session_id)
                .await?
        {
            return Ok(None);
        }
        let mutation = ClaudeSessionLink::assign_cli(
            &self.db.pool,
            claude_session_id,
            binding.session_id,
            binding.workspace_id,
            &context.cwd.to_string_lossy(),
            db::models::claude_session_link::ClaudeSessionBoundVia::CliFresh,
        )
        .await?;
        Ok(Some(mutation))
    }

    async fn apply_link_mutation(&self, mutation: &ClaudeSessionLinkMutation) {
        if mutation.republished_outbox > 0 {
            self.publisher_notify.notify_one();
        }
        if !mutation.session_changed() {
            return;
        }
        if let Some(previous_session_id) = mutation.previous_session_id {
            self.invalidate_revision(previous_session_id).await;
        }
        self.invalidate_revision(mutation.link.session_id).await;
    }

    async fn locate_sid(&self, sid: &str) -> Result<Option<PathBuf>, std::io::Error> {
        if let Some(cached) = self.sid_dir_cache.read().await.get(sid).cloned()
            && cached.join(format!("{sid}.jsonl")).is_file()
        {
            return Ok(Some(cached));
        }
        for entry in fs::read_dir(&self.projects_dir)? {
            let entry = entry?;
            let dir = entry.path();
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if dir.join(format!("{sid}.jsonl")).is_file() {
                self.sid_dir_cache
                    .write()
                    .await
                    .insert(sid.to_string(), dir.clone());
                return Ok(Some(dir));
            }
        }
        Ok(None)
    }

    async fn revision(&self, session_id: Uuid) -> u64 {
        self.revisions
            .read()
            .await
            .get(&session_id)
            .copied()
            .unwrap_or(0)
    }

    async fn invalidate_revision(&self, session_id: Uuid) {
        let revision = {
            let mut revisions = self.revisions.write().await;
            let revision = revisions.entry(session_id).or_insert(0);
            *revision += 1;
            *revision
        };
        let _ = self
            .feed_updates
            .send(NativeFeedUpdate::RevisionInvalidated {
                session_id,
                revision,
            });
    }

    async fn run_native_link_invalidation(
        self: Arc<Self>,
        mut updates: broadcast::Receiver<NativeLinkPersisted>,
        shutdown: CancellationToken,
    ) {
        loop {
            let event = tokio::select! {
                _ = shutdown.cancelled() => break,
                event = updates.recv() => match event {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "native-link invalidation listener lagged");
                        // A missed link can change any cached projection's
                        // origins, and cached projections only re-read rows on
                        // a new revision.
                        let cached = self
                            .projections
                            .lock()
                            .unwrap()
                            .sessions
                            .keys()
                            .copied()
                            .collect::<Vec<_>>();
                        for session_id in cached {
                            self.invalidate_revision(session_id).await;
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            };
            match CliNativeRecord::session_ids_for_uuid(&self.db.pool, &event.native_uuid).await {
                Ok(session_ids) => {
                    for session_id in session_ids {
                        self.invalidate_revision(session_id).await;
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        execution_process_id = %event.execution_process_id,
                        native_uuid = %event.native_uuid,
                        "failed to invalidate feed after native UUID persistence"
                    );
                }
            }
        }
    }

    /// Bump the revision of every cached session whose link set changed.
    ///
    /// Link mutations made outside this service (the CLI launch path) neither
    /// bump a revision nor notify the publisher, so a connected feed whose
    /// links moved away would otherwise show the moved turns until its next
    /// update. Runs on the publisher's safety tick.
    async fn invalidate_moved_links(&self) {
        let cells = self
            .projections
            .lock()
            .unwrap()
            .sessions
            .iter()
            .map(|(session_id, cached)| (*session_id, cached.cell.clone()))
            .collect::<Vec<_>>();
        for (session_id, cell) in cells {
            // A feed update holding the cell re-checks links itself.
            let Ok(cached) = cell.try_lock() else {
                continue;
            };
            let Some(cached_sids) = cached.as_ref().map(|cached| cached.linked_sids.clone()) else {
                continue;
            };
            drop(cached);
            match ClaudeSessionLink::claude_session_ids_for_session(&self.db.pool, session_id).await
            {
                Ok(linked_sids) if linked_sids != cached_sids => {
                    self.invalidate_revision(session_id).await;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(?error, %session_id, "failed to check native feed links")
                }
            }
        }
    }

    async fn run_publisher(self: Arc<Self>, shutdown: CancellationToken) {
        // Notifications drive ordinary delivery. The slow poll recovers a
        // notification lost to a crash; persisted watermarks keep that safety
        // pass from redraining already published history after restart.
        let mut safety_interval = tokio::time::interval(OUTBOX_SAFETY_POLL_INTERVAL);
        safety_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = safety_interval.tick() => self.invalidate_moved_links().await,
                _ = self.publisher_notify.notified() => {},
            }
            let maxima = match CliIngestOutbox::session_maxima(&self.db.pool).await {
                Ok(maxima) => maxima,
                Err(error) => {
                    tracing::warn!(?error, "failed to poll native transcript outbox");
                    continue;
                }
            };
            for maximum in maxima {
                let revision = self.revision(maximum.session_id).await;
                let _ = self.feed_updates.send(NativeFeedUpdate::RecordsAppended {
                    session_id: maximum.session_id,
                    seq: maximum.max_seq,
                    revision,
                });
                if let Err(error) = CliIngestOutbox::mark_published(
                    &self.db.pool,
                    maximum.session_id,
                    maximum.max_seq,
                )
                .await
                {
                    tracing::warn!(
                        ?error,
                        session_id = %maximum.session_id,
                        seq = maximum.max_seq,
                        "failed to persist native transcript publisher watermark"
                    );
                }
            }
            if let Err(error) = CliIngestOutbox::prune_superseded(&self.db.pool).await {
                tracing::warn!(
                    ?error,
                    "failed to prune superseded native transcript outbox rows"
                );
            }
        }
    }
}

/// Best-effort Claude project key. It is never trusted as an ownership signal;
/// known sid files are verified and located by an exhaustive one-level scan.
fn claude_project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .collect()
}

struct SessionPreview {
    first_prompt_snippet: Option<String>,
    kind: CliSessionKind,
}

fn read_session_preview(path: &Path, file_session_id: &str) -> SessionPreview {
    let mut snippet: Option<String> = None;
    let mut entrypoint: Option<String> = None;
    if let Ok(file) = File::open(path) {
        for line in BufReader::new(file).lines().take(50) {
            let Ok(line) = line else {
                continue;
            };
            let Ok(adapted) = adapt_native_claude_line(&line, file_session_id) else {
                continue;
            };
            if entrypoint.is_none()
                && let Some(ep) = adapted.metadata().entrypoint.as_deref()
            {
                entrypoint = Some(ep.to_string());
            }
            if snippet.is_none()
                && let Some(prompt) = adapted.plain_user_text()
            {
                let mut s = prompt.chars().take(160).collect::<String>();
                if prompt.chars().count() > 160 {
                    s.push('…');
                }
                snippet = Some(s);
            }
            if snippet.is_some() && entrypoint.is_some() {
                break;
            }
        }
    }
    SessionPreview {
        first_prompt_snippet: snippet,
        kind: CliSessionKind::from_entrypoint(entrypoint.as_deref()),
    }
}

fn verify_last_line_hash(
    file: &mut File,
    native_file: &CliNativeFile,
) -> Result<Option<String>, std::io::Error> {
    if native_file.cursor_offset <= 0 || native_file.last_line_hash.is_none() {
        return Ok(None);
    }
    let length = native_file
        .cursor_offset
        .saturating_sub(native_file.last_line_offset) as usize;
    file.seek(SeekFrom::Start(native_file.last_line_offset as u64))?;
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes)?;
    Ok(Some(hash_bytes(&bytes)))
}

/// Codex thread ids are UUIDv7; Claude session ids are v4.
fn is_codex_thread_id(sid: &str) -> bool {
    Uuid::parse_str(sid).is_ok_and(|id| id.get_version_num() == 7)
}

fn codex_thread_created_at(sid: &str) -> Option<DateTime<Utc>> {
    let (seconds, nanos) = Uuid::parse_str(sid).ok()?.get_timestamp()?.to_unix();
    DateTime::from_timestamp(seconds as i64, nanos)
}

fn codex_day_dir(root: &Path, day: DateTime<Utc>) -> PathBuf {
    root.join(day.format("%Y/%m/%d").to_string())
}

/// Interactive (`source: "cli"`) rollouts whose `session_meta` names `cwd`
/// and which started at or after `since`, as `(thread id, path)`.
fn codex_rollouts_started_in(
    root: &Path,
    cwd: &Path,
    since: DateTime<Utc>,
) -> Vec<(String, PathBuf)> {
    let first = since.date_naive() - chrono::Duration::days(1);
    let since_ms = since.timestamp_millis();
    let last = (Utc::now() + chrono::Duration::days(1)).date_naive();
    std::iter::successors(Some(last), |day| day.pred_opt())
        .take_while(|day| *day >= first)
        .take(CODEX_FALLBACK_DAY_DIRS)
        .filter_map(|day| fs::read_dir(root.join(day.format("%Y/%m/%d").to_string())).ok())
        .flat_map(|entries| entries.flatten())
        .filter_map(|entry| {
            let path = entry.path();
            let sid = rollout_thread_id(path.file_name()?.to_str()?)?.to_string();
            let modified = modified_ms(&entry.metadata().ok()?)?;
            (modified >= since_ms && codex_rollout_started_in(&path, cwd, since))
                .then_some((sid, path))
        })
        .collect()
}

/// Whether the rollout at `path` is an interactive (`source: "cli"`) session
/// that started in `cwd` at or after `since`, per its `session_meta`.
fn codex_rollout_started_in(path: &Path, cwd: &Path, since: DateTime<Utc>) -> bool {
    let matches = || -> Option<bool> {
        let mut first_line = String::new();
        BufReader::new(File::open(path).ok()?.take(CODEX_SESSION_META_READ_LIMIT))
            .read_line(&mut first_line)
            .ok()?;
        let meta: serde_json::Value = serde_json::from_str(&first_line).ok()?;
        let payload = meta.get("payload")?;
        let started = payload
            .get("timestamp")
            .and_then(|value| value.as_str())
            .and_then(parse_native_timestamp)?;
        Some(
            meta.get("type")?.as_str()? == "session_meta"
                && payload.get("source")?.as_str()? == "cli"
                && Path::new(payload.get("cwd")?.as_str()?) == cwd
                && started >= since,
        )
    };
    matches().unwrap_or(false)
}

/// The stored form of one Codex rollout line, and whether it was unknown.
/// Bookkeeping lines are not stored: they never render and a rollout repeats
/// every turn several times over.
fn codex_native_record(
    line_seq: i64,
    thread_id: &str,
    raw: &str,
    cwd: &str,
) -> (Option<NewCliNativeRecord>, bool) {
    let unknown = |ts: Option<String>| NewCliNativeRecord {
        line_seq,
        claude_session_id: thread_id.to_string(),
        uuid: None,
        parent_uuid: None,
        kind: "unknown".to_string(),
        ts,
        raw: raw.to_string(),
        disposition: CliNativeRecordDisposition::Unknown,
        user_prompt: None,
        recorded_at: None,
    };
    let Ok(line) = adapt_codex_rollout_line(raw, cwd) else {
        return (Some(unknown(None)), true);
    };
    match line.disposition {
        CodexRolloutDisposition::Bookkeeping => (None, false),
        CodexRolloutDisposition::Unknown => (Some(unknown(line.timestamp)), true),
        CodexRolloutDisposition::Renderable => (
            Some(NewCliNativeRecord {
                line_seq,
                claude_session_id: thread_id.to_string(),
                uuid: line.item_id,
                parent_uuid: line.turn_id,
                kind: line.kind,
                recorded_at: line.timestamp.as_deref().and_then(parse_native_timestamp),
                ts: line.timestamp,
                raw: raw.to_string(),
                disposition: CliNativeRecordDisposition::Renderable,
                // No prompt-equality reconciliation: it would hide a pane turn
                // that repeats a recent chat prompt, and executor turns in a
                // rollout are attributed by their run window instead.
                user_prompt: None,
            }),
            false,
        ),
    }
}

fn file_size(metadata: &Metadata) -> i64 {
    metadata.len().min(i64::MAX as u64) as i64
}

fn modified_ms(metadata: &Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

#[cfg(unix)]
fn file_identity(metadata: &Metadata) -> (i64, i64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev() as i64, metadata.ino() as i64)
}

#[cfg(not(unix))]
fn file_identity(_metadata: &Metadata) -> (i64, i64) {
    // Windows std metadata exposes no stable dev/inode identity. The (0, 0)
    // sentinel means replacements rely only on truncation and last-line hash.
    (0, 0)
}

fn parse_native_timestamp(timestamp: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}
