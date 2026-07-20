//! Read-only ingestion of Claude Code's native transcript store.
//!
//! Every filesystem operation below the configured projects root is a read:
//! `read_dir`, `metadata`, or `File::open`. App-owned persistence goes only to
//! SQLite through `db` model functions.

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
            CliNativeRecord, CliNativeRecordDisposition, ImportedCursor, NewCliNativeRecord,
        },
        session::Session,
        workspace::{Workspace, WorkspaceError},
    },
};
use executors::executors::claude::native::{
    NativeClaudeDisposition, NativeClaudeSkipReason, adapt_native_claude_line,
};
pub use forks::{NativeForkBranch, NativeForkView};
use futures::StreamExt;
pub use projection::{
    NativeBranchMetadata, NativeFeedEntry, NativeFeedFork, NativeFeedOrigin, NativeFeedSnapshot,
    NativeFileImportHealth, NativeIngestHealth,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    sync::{Notify, RwLock, broadcast},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use ts_rs::TS;
use uuid::Uuid;

use self::{
    projection::build_projection,
    tail::{
        ObservedFileState, StoredTailState, hash_bytes, read_complete_line_batch, rescan_reason,
    },
};
use crate::services::filesystem_watcher;

const REGISTRY_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const OUTBOX_SAFETY_POLL_INTERVAL: Duration = Duration::from_secs(30);
const IMPORT_BATCH_LINE_LIMIT: usize = 256;

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

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct UnassignedCliSession {
    pub claude_session_id: String,
    pub cwd: String,
    pub dir_path: String,
    pub file_name: String,
    pub mtime_ms: Option<i64>,
    pub first_prompt_snippet: Option<String>,
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

#[derive(Debug, Default)]
struct ImportPathState {
    pending: bool,
    force_rescan: bool,
}

pub struct ClaudeTranscriptIngest {
    db: DBService,
    projects_dir: PathBuf,
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
    #[cfg(test)]
    snapshot_watermark_barrier: tokio::sync::Mutex<Option<Arc<tokio::sync::Barrier>>>,
    #[cfg(test)]
    path_import_barrier: tokio::sync::Mutex<Option<Arc<tokio::sync::Barrier>>>,
}

impl ClaudeTranscriptIngest {
    /// Check the kill switch exactly once. A set value of any kind disables
    /// the entire service and no Claude-store watcher is created.
    pub fn spawn(db: DBService, shutdown: CancellationToken) -> Option<Arc<Self>> {
        if std::env::var_os("DISABLE_CLI_TRANSCRIPT_INGEST").is_some() {
            tracing::info!("CLI transcript ingest disabled by environment");
            return None;
        }
        let projects_dir = dirs::home_dir()?.join(".claude").join("projects");
        let service = Arc::new(Self::new(db, projects_dir));

        tokio::spawn(service.clone().run_publisher(shutdown.child_token()));
        let native_link_updates = native_link_events().subscribe();
        tokio::spawn(
            service
                .clone()
                .run_native_link_invalidation(native_link_updates, shutdown.child_token()),
        );
        tokio::spawn(service.clone().run_registry(shutdown.child_token(), true));
        Some(service)
    }

    fn new(db: DBService, projects_dir: PathBuf) -> Self {
        let (feed_updates, _) = broadcast::channel(4096);
        Self {
            db,
            projects_dir,
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
            #[cfg(test)]
            snapshot_watermark_barrier: tokio::sync::Mutex::new(None),
            #[cfg(test)]
            path_import_barrier: tokio::sync::Mutex::new(None),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<NativeFeedUpdate> {
        self.feed_updates.subscribe()
    }

    pub async fn snapshot(
        &self,
        session_id: Uuid,
    ) -> Result<NativeFeedSnapshot, ClaudeTranscriptIngestError> {
        let session = Session::find_by_id(&self.db.pool, session_id)
            .await?
            .ok_or(ClaudeTranscriptIngestError::SessionNotFound(session_id))?;

        // Capture both live-stream watermarks before reading projection rows.
        // With subscribe-before-snapshot, a later import/reset is queued for
        // the subscriber, while anything included in `seq` is guaranteed to
        // be visible to the subsequent rows query after its atomic commit.
        let revision = self.revision(session_id).await;
        let seq = CliIngestOutbox::latest_seq(&self.db.pool, session_id).await?;
        #[cfg(test)]
        self.wait_at_snapshot_watermark().await;
        let rows = CliNativeRecord::list_for_session(&self.db.pool, session_id).await?;

        let mut seen_files = HashSet::new();
        let mut files = Vec::new();
        for row in &rows {
            if !seen_files.insert(row.file_id) {
                continue;
            }
            if let Some(file) = CliNativeFile::find_by_id(&self.db.pool, row.file_id).await? {
                files.push(NativeFileImportHealth {
                    claude_session_id: file.claude_session_id,
                    file_name: file.file_name,
                    generation: file.generation,
                    last_import_at: file.last_import_at.map(|time| time.to_rfc3339()),
                });
            }
        }
        let degraded_paths = self.degraded_watchers.read().await.clone();
        let directories = self.directories.read().await;
        let watch_degraded = degraded_paths.iter().any(|path| {
            directories
                .get(path)
                .is_some_and(|context| context.workspace_id == session.workspace_id)
        });
        drop(directories);
        let health = NativeIngestHealth {
            unknown_kinds: self.unknown_kinds.load(Ordering::Relaxed),
            rescans: self.rescans.load(Ordering::Relaxed),
            quarantined_files: self.quarantined_paths.lock().unwrap().len() as u64,
            watch_degraded,
            files,
        };
        Ok(build_projection(&rows, revision, seq, health))
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
                UnassignedCliSession {
                    claude_session_id: file.claude_session_id.clone(),
                    cwd: cwd.to_string_lossy().into_owned(),
                    dir_path: file.dir_path,
                    file_name: file.file_name,
                    mtime_ms: file.observed_mtime_ms,
                    first_prompt_snippet: first_prompt_snippet(&path, &file.claude_session_id),
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
        if let Err(error) = self
            .reconcile_registry(start_watchers, shutdown.child_token())
            .await
        {
            tracing::warn!(?error, "initial CLI transcript reconciliation failed");
        }
        let mut interval = tokio::time::interval(REGISTRY_RECONCILE_INTERVAL);
        interval.tick().await;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(error) = self
                        .reconcile_registry(start_watchers, shutdown.child_token())
                        .await
                    {
                        tracing::warn!(?error, "CLI transcript reconciliation failed");
                    }
                }
            }
        }
    }

    async fn reconcile_registry(
        self: &Arc<Self>,
        start_watchers: bool,
        shutdown: CancellationToken,
    ) -> Result<(), ClaudeTranscriptIngestError> {
        let mut desired = HashMap::new();
        if self.projects_dir.is_dir() {
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
                let computed_dir = self.projects_dir.join(claude_project_slug(&cwd));
                if computed_dir.is_dir() {
                    let dir = fs::canonicalize(&computed_dir).unwrap_or(computed_dir.clone());
                    desired.insert(dir, context.clone());
                }

                for sid in
                    ClaudeSessionLink::known_session_ids_for_workspace(&self.db.pool, workspace.id)
                        .await?
                {
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
        self.reconcile_directories(desired, start_watchers, shutdown)
            .await
    }

    async fn reconcile_directories(
        self: &Arc<Self>,
        desired: HashMap<PathBuf, DirectoryContext>,
        start_watchers: bool,
        shutdown: CancellationToken,
    ) -> Result<(), ClaudeTranscriptIngestError> {
        let watched_paths = if start_watchers {
            desired.keys().cloned().collect::<HashSet<_>>()
        } else {
            HashSet::new()
        };
        *self.directories.write().await = desired;

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
        let directories = self
            .directories
            .read()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for dir in directories {
            self.scan_directory(&dir, false).await?;
        }
        Ok(())
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
    ) -> Result<(), ClaudeTranscriptIngestError> {
        let Some(context) = self.directories.read().await.get(dir).cloned() else {
            return Ok(());
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
            if let Err(error) = self
                .process_native_path(&path, &context, force_rescan)
                .await
            {
                tracing::warn!(?error, path = %path.display(), "native transcript import failed");
            }
        }
        Ok(())
    }

    async fn process_native_path(
        &self,
        path: &Path,
        context: &DirectoryContext,
        force_rescan: bool,
    ) -> Result<(), ClaudeTranscriptIngestError> {
        let path = path.to_path_buf();
        {
            let mut importing = self.importing_paths.lock().await;
            if let Some(state) = importing.get_mut(&path) {
                state.pending = true;
                state.force_rescan |= force_rescan;
                return Ok(());
            }
            importing.insert(path.clone(), ImportPathState::default());
        }

        let mut next_force_rescan = force_rescan;
        loop {
            let result = self
                .process_native_path_inner(&path, context, next_force_rescan)
                .await;
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
            return result;
        }
    }

    async fn process_native_path_inner(
        &self,
        path: &Path,
        context: &DirectoryContext,
        force_rescan: bool,
    ) -> Result<(), ClaudeTranscriptIngestError> {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let Some(claude_session_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            return Ok(());
        };
        let dir_path = path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_string_lossy()
            .into_owned();
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Ok(());
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

        let link = ClaudeSessionLink::resolve_or_bind_executor(
            &self.db.pool,
            claude_session_id,
            &context.cwd.to_string_lossy(),
        )
        .await?;
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
            for complete in &tail.lines {
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
            let imported = if activate_replacement {
                let replacement = CliNativeRecord::replace_generation_and_import_batch(
                    &self.db.pool,
                    &registration,
                    &records,
                    &cursor,
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
                CliNativeRecord::import_batch(&self.db.pool, native_file.id, &records, &cursor)
                    .await?
            };
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
        Ok(())
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

    async fn run_publisher(self: Arc<Self>, shutdown: CancellationToken) {
        // Notifications drive ordinary delivery. The slow poll recovers a
        // notification lost to a crash; persisted watermarks keep that safety
        // pass from redraining already published history after restart.
        let mut safety_interval = tokio::time::interval(OUTBOX_SAFETY_POLL_INTERVAL);
        safety_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = safety_interval.tick() => {},
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

fn first_prompt_snippet(path: &Path, file_session_id: &str) -> Option<String> {
    let file = File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(50) {
        let Ok(line) = line else {
            continue;
        };
        let Ok(adapted) = adapt_native_claude_line(&line, file_session_id) else {
            continue;
        };
        if let Some(prompt) = adapted.plain_user_text() {
            let mut snippet = prompt.chars().take(160).collect::<String>();
            if prompt.chars().count() > 160 {
                snippet.push('…');
            }
            return Some(snippet);
        }
    }
    None
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
    // Windows file ids are not exposed through std. Truncate and last-line
    // verification still detect replacements; generation remains portable.
    (0, 0)
}

fn parse_native_timestamp(timestamp: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}
