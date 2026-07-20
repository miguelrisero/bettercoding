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
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use chrono::{DateTime, Utc};
use db::{
    DBService,
    models::{
        claude_session_link::ClaudeSessionLink,
        cli_ingest_outbox::CliIngestOutbox,
        cli_native_file::{CliNativeFile, RegisterCliNativeFile},
        cli_native_record::{CliNativeRecord, ImportedCursor, NewCliNativeRecord},
        session::Session,
        workspace::Workspace,
        workspace_cli_activity::{CliActivityState, WorkspaceCliActivity},
    },
};
use executors::executors::claude::native::adapt_native_claude_line;
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
    tail::{ObservedFileState, StoredTailState, hash_bytes, rescan_reason, split_complete_lines},
};
use crate::services::filesystem_watcher;

const REGISTRY_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const OUTBOX_POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_PROJECT_DIR_SCAN: usize = 512;

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

#[derive(Debug, Clone, Copy)]
pub struct NativeFeedUpdate {
    pub session_id: Uuid,
    pub seq: i64,
    pub revision: u64,
}

#[derive(Debug, Clone)]
struct DirectoryContext {
    workspace_id: Uuid,
    cwd: PathBuf,
}

pub struct ClaudeTranscriptIngest {
    db: DBService,
    projects_dir: PathBuf,
    directories: RwLock<HashMap<PathBuf, DirectoryContext>>,
    watchers: tokio::sync::Mutex<HashMap<PathBuf, JoinHandle<()>>>,
    importing_paths: tokio::sync::Mutex<HashSet<PathBuf>>,
    sid_dir_cache: RwLock<HashMap<String, PathBuf>>,
    quarantined_paths: Mutex<HashSet<PathBuf>>,
    unknown_kinds: AtomicU64,
    rescans: AtomicU64,
    watch_degraded: AtomicBool,
    revisions: RwLock<HashMap<Uuid, u64>>,
    feed_updates: broadcast::Sender<NativeFeedUpdate>,
    publisher_notify: Notify,
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
            importing_paths: tokio::sync::Mutex::new(HashSet::new()),
            sid_dir_cache: RwLock::new(HashMap::new()),
            quarantined_paths: Mutex::new(HashSet::new()),
            unknown_kinds: AtomicU64::new(0),
            rescans: AtomicU64::new(0),
            watch_degraded: AtomicBool::new(false),
            revisions: RwLock::new(HashMap::new()),
            feed_updates,
            publisher_notify: Notify::new(),
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
        let rows = CliNativeRecord::list_for_session(&self.db.pool, session_id).await?;
        let seq = CliIngestOutbox::latest_seq(&self.db.pool, session_id).await?;
        let revision = self.revision(session_id).await;
        let cli_session_active = WorkspaceCliActivity::find_all(&self.db.pool)
            .await?
            .into_iter()
            .find(|activity| activity.workspace_id == session.workspace_id)
            .is_some_and(|activity| activity.state != CliActivityState::Idle);

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
        let health = NativeIngestHealth {
            unknown_kinds: self.unknown_kinds.load(Ordering::Relaxed),
            rescans: self.rescans.load(Ordering::Relaxed),
            quarantined_files: self.quarantined_paths.lock().unwrap().len() as u64,
            watch_degraded: self.watch_degraded.load(Ordering::Relaxed),
            files,
        };
        Ok(build_projection(
            &rows,
            revision,
            seq,
            health,
            cli_session_active,
        ))
    }

    pub async fn list_unassigned(
        &self,
        workspace_id: Uuid,
    ) -> Result<Vec<UnassignedCliSession>, ClaudeTranscriptIngestError> {
        let workspace = Workspace::find_by_id(&self.db.pool, workspace_id)
            .await?
            .ok_or(ClaudeTranscriptIngestError::WorkspacePathMissing(
                workspace_id,
            ))?;
        let session = Session::find_latest_by_workspace_id(&self.db.pool, workspace_id).await?;
        let cwd = session
            .as_ref()
            .and_then(|session| effective_cwd(&workspace, session))
            .or_else(|| workspace.container_ref.as_ref().map(PathBuf::from))
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
            .ok_or(ClaudeTranscriptIngestError::WorkspacePathMissing(
                session.workspace_id,
            ))?;
        let cwd = effective_cwd(&workspace, &session).ok_or(
            ClaudeTranscriptIngestError::WorkspacePathMissing(session.workspace_id),
        )?;
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

        ClaudeSessionLink::assign_manual(
            &self.db.pool,
            claude_session_id,
            session_id,
            &cwd.to_string_lossy(),
        )
        .await?
        .ok_or(ClaudeTranscriptIngestError::SessionNotFound(session_id))?;

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
        if !self.projects_dir.is_dir() {
            return Ok(());
        }
        for workspace in Workspace::fetch_all(&self.db.pool).await? {
            if workspace.archived || workspace.worktree_deleted {
                continue;
            }
            let Some(session) =
                Session::find_latest_by_workspace_id(&self.db.pool, workspace.id).await?
            else {
                continue;
            };
            let Some(cwd) = effective_cwd(&workspace, &session) else {
                continue;
            };
            let context = DirectoryContext {
                workspace_id: workspace.id,
                cwd: cwd.clone(),
            };
            let computed_dir = self.projects_dir.join(claude_project_slug(&cwd));
            if computed_dir.is_dir() {
                self.register_directory(
                    computed_dir.clone(),
                    context.clone(),
                    start_watchers,
                    shutdown.child_token(),
                )
                .await?;
            }

            for sid in
                ClaudeSessionLink::known_session_ids_for_workspace(&self.db.pool, workspace.id)
                    .await?
            {
                if computed_dir.join(format!("{sid}.jsonl")).is_file() {
                    continue;
                }
                if let Some(found_dir) = self.locate_sid(&sid).await? {
                    self.register_directory(
                        found_dir,
                        context.clone(),
                        start_watchers,
                        shutdown.child_token(),
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    async fn register_directory(
        self: &Arc<Self>,
        dir: PathBuf,
        context: DirectoryContext,
        start_watcher: bool,
        shutdown: CancellationToken,
    ) -> Result<(), ClaudeTranscriptIngestError> {
        let dir = fs::canonicalize(&dir).unwrap_or(dir);
        self.directories.write().await.insert(dir.clone(), context);
        self.scan_directory(&dir, false).await?;
        if start_watcher {
            self.ensure_watcher(dir, shutdown).await;
        }
        Ok(())
    }

    async fn ensure_watcher(self: &Arc<Self>, dir: PathBuf, shutdown: CancellationToken) {
        let mut watchers = self.watchers.lock().await;
        if watchers.contains_key(&dir) {
            return;
        }
        let service = self.clone();
        let watched_dir = dir.clone();
        let handle = tokio::spawn(async move {
            let Ok((fs_guard, mut receiver, _)) =
                filesystem_watcher::async_watcher(watched_dir.clone())
            else {
                service.watch_degraded.store(true, Ordering::Relaxed);
                tracing::warn!(path = %watched_dir.display(), "native transcript watcher unavailable; using reconcile polling");
                return;
            };
            let _fs_guard = fs_guard;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    event = receiver.next() => match event {
                        Some(Ok(_)) => {
                            if let Err(error) = service.scan_directory(&watched_dir, false).await {
                                tracing::warn!(?error, path = %watched_dir.display(), "native transcript watch scan failed");
                            }
                        }
                        Some(Err(error)) => {
                            service.watch_degraded.store(true, Ordering::Relaxed);
                            tracing::warn!(?error, path = %watched_dir.display(), "native transcript watcher error; forcing rescan");
                            if let Err(error) = service.scan_directory(&watched_dir, true).await {
                                tracing::warn!(?error, path = %watched_dir.display(), "native transcript forced rescan failed");
                            }
                        }
                        None => break,
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
        {
            let mut importing = self.importing_paths.lock().await;
            if !importing.insert(path.to_path_buf()) {
                return Ok(());
            }
        }
        let result = self
            .process_native_path_inner(path, context, force_rescan)
            .await;
        self.importing_paths.lock().await.remove(path);
        result
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
        let metadata = fs::metadata(path)?;
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
        let Some(link) = link else {
            self.quarantined_paths
                .lock()
                .unwrap()
                .insert(path.to_path_buf());
            return Ok(());
        };
        self.quarantined_paths.lock().unwrap().remove(path);

        let mut file = File::open(path)?;
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
        if let Some(reason) = reason {
            native_file = CliNativeFile::bump_generation(&self.db.pool, &registration).await?;
            self.rescans.fetch_add(1, Ordering::Relaxed);
            self.bump_revision(link.session_id).await;
            tracing::info!(?reason, path = %path.display(), generation = native_file.generation, "rescanning native transcript generation");
            file.seek(SeekFrom::Start(0))?;
        } else {
            file.seek(SeekFrom::Start(native_file.cursor_offset as u64))?;
        }

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let tail =
            split_complete_lines(&bytes, native_file.cursor_offset, native_file.next_line_seq);
        if tail.lines.is_empty() {
            return Ok(());
        }

        let mut records = Vec::new();
        for complete in &tail.lines {
            match adapt_native_claude_line(&complete.raw, claude_session_id) {
                Ok(line) if line.is_sidechain() => continue,
                Ok(line) => {
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
                        user_prompt: None,
                        recorded_at: None,
                    });
                }
            }
        }
        let cursor = ImportedCursor {
            cursor_offset: tail.cursor_offset,
            next_line_seq: tail.next_line_seq,
            last_line_offset: tail
                .last_line_offset
                .unwrap_or(native_file.last_line_offset),
            last_line_hash: tail
                .last_line_hash
                .as_deref()
                .or(native_file.last_line_hash.as_deref()),
            observed_size,
            observed_mtime_ms,
        };
        let imported =
            CliNativeRecord::import_batch(&self.db.pool, native_file.id, &records, &cursor).await?;
        if imported.appended_outbox > 0 {
            self.publisher_notify.notify_one();
        }
        Ok(())
    }

    async fn locate_sid(&self, sid: &str) -> Result<Option<PathBuf>, std::io::Error> {
        if let Some(cached) = self.sid_dir_cache.read().await.get(sid).cloned()
            && cached.join(format!("{sid}.jsonl")).is_file()
        {
            return Ok(Some(cached));
        }
        for entry in fs::read_dir(&self.projects_dir)?.take(MAX_PROJECT_DIR_SCAN) {
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

    async fn bump_revision(&self, session_id: Uuid) {
        let mut revisions = self.revisions.write().await;
        *revisions.entry(session_id).or_insert(0) += 1;
    }

    async fn run_publisher(self: Arc<Self>, shutdown: CancellationToken) {
        // Redrain the durable outbox after restart. New subscribers still use
        // a snapshot, while this closes the startup race for a subscriber that
        // connects during initial backfill.
        let mut published = HashMap::new();
        let mut interval = tokio::time::interval(OUTBOX_POLL_INTERVAL);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {},
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
                let cursor = published.entry(maximum.session_id).or_insert(0);
                while *cursor < maximum.max_seq {
                    let rows = match CliIngestOutbox::find_after(
                        &self.db.pool,
                        maximum.session_id,
                        *cursor,
                        256,
                    )
                    .await
                    {
                        Ok(rows) => rows,
                        Err(error) => {
                            tracing::warn!(?error, session_id = %maximum.session_id, "failed to drain native transcript outbox");
                            break;
                        }
                    };
                    if rows.is_empty() {
                        break;
                    }
                    let revision = self.revision(maximum.session_id).await;
                    for row in rows {
                        *cursor = row.seq;
                        let _ = self.feed_updates.send(NativeFeedUpdate {
                            session_id: maximum.session_id,
                            seq: row.seq,
                            revision,
                        });
                    }
                }
            }
        }
    }
}

/// Replicates the terminal route's effective-cwd rule without coupling the
/// read-only service to that write/attach route.
fn effective_cwd(workspace: &Workspace, session: &Session) -> Option<PathBuf> {
    let base = PathBuf::from(workspace.container_ref.as_ref()?);
    if let Some(relative) = session.agent_working_dir.as_deref() {
        let joined = base.join(relative);
        if joined.exists() {
            return Some(joined);
        }
    }
    Some(base)
}

/// Best-effort Claude project key. It is never trusted as an ownership signal;
/// known sid files are verified and located by bounded filename scan.
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
        let line = line.ok()?;
        let adapted = adapt_native_claude_line(&line, file_session_id).ok()?;
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
