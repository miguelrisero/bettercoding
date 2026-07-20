//! Workspace Files API: browse/download/zip/upload files in a workspace worktree.
//!
//! Local-only (a `reject_relay_requests` guard blocks relay/host-proxied access).
//! All filesystem access goes through [`super::file_policy`] so path-safety, the
//! `.git`/`node_modules` denylist, and size caps live in one place.

use std::{
    collections::HashMap,
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
    time::{Duration, Instant},
};

use axum::{
    Extension, Router,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Query, Request, State},
    http::header,
    middleware::{Next, from_fn, from_fn_with_state},
    response::{IntoResponse, Json as ResponseJson, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use db::models::workspace::Workspace;
use deployment::Deployment;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use services::services::{container::ContainerService, file::FileError};
use tokio::{
    io::AsyncWriteExt,
    sync::{OwnedRwLockReadGuard, RwLock},
};
use tokio_util::io::ReaderStream;
use ts_rs::TS;
use utils::response::ApiResponse;
use uuid::Uuid;
use walkdir::WalkDir;

use super::file_policy::{
    self, LEGACY_UPLOADS_DIR, MAX_LIST_ENTRIES, MAX_UPLOAD_FILE_BYTES, MAX_UPLOAD_FILES,
    MAX_UPLOAD_REQUEST_BYTES, MAX_ZIP_ENTRIES, MAX_ZIP_UNCOMPRESSED_BYTES, UPLOADS_DIR,
};
use crate::{DeploymentImpl, error::ApiError, middleware::load_workspace_middleware};

/// Wall-clock budget for generating a zip archive.
const ZIP_WALL_TIME: Duration = Duration::from_secs(120);

type WorkspaceFilesystemLock = Arc<RwLock<()>>;

/// Serializes upload-directory migration against Files requests within one
/// server process. Keys are canonical worktree roots so aliases of the same
/// workspace share a lock. The deployment model runs one local server per data
/// directory; cross-process concurrency is not fully serialized and is
/// mitigated by atomic rename, rename-failure re-probing, and default-upload
/// publish retry.
///
/// TODO: Add an OS-level lockfile if the deployment model ever permits multiple
/// local servers for one data directory.
static WORKSPACE_FILESYSTEM_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Weak<RwLock<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn workspace_filesystem_lock(canonical_base: &Path) -> WorkspaceFilesystemLock {
    let mut locks = WORKSPACE_FILESYSTEM_LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.retain(|_, filesystem_lock| filesystem_lock.strong_count() > 0);
    if let Some(filesystem_lock) = locks.get(canonical_base).and_then(Weak::upgrade) {
        return filesystem_lock;
    }

    let filesystem_lock = Arc::new(RwLock::new(()));
    locks.insert(
        canonical_base.to_path_buf(),
        Arc::downgrade(&filesystem_lock),
    );
    filesystem_lock
}

/// Stream a file while retaining the workspace read guard until the response
/// body is fully consumed or dropped.
fn guarded_file_body(file: tokio::fs::File, guard: OwnedRwLockReadGuard<()>) -> Body {
    let stream = futures_util::stream::unfold(
        (ReaderStream::new(file), guard),
        |(mut reader, guard)| async move { reader.next().await.map(|item| (item, (reader, guard))) },
    );
    Body::from_stream(stream)
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct WorkspaceFileEntry {
    /// File or directory name (no path separators).
    pub name: String,
    /// Path relative to the worktree root (forward slashes).
    pub path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size_bytes: i64,
    pub modified: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct WorkspaceDirListing {
    /// The directory that was listed, relative to the worktree root.
    pub path: String,
    pub entries: Vec<WorkspaceFileEntry>,
    /// True if the directory had more than `MAX_LIST_ENTRIES` entries.
    pub truncated: bool,
}

#[derive(Debug, Deserialize)]
pub struct PathQuery {
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    /// Target directory relative to the worktree root. Absent/empty → `.bettercoding-uploads/`.
    #[serde(default)]
    pub path: Option<String>,
    /// Allow replacing an existing file. Defaults to false (409 on conflict).
    #[serde(default)]
    pub overwrite: bool,
}

/// Resolve the worktree root (`container_ref`) for a workspace.
async fn workspace_root(
    deployment: &DeploymentImpl,
    workspace: &Workspace,
) -> Result<PathBuf, ApiError> {
    let container_ref = deployment
        .container()
        .ensure_container_exists(workspace)
        .await?;
    Ok(PathBuf::from(container_ref))
}

/// Every Files handler must resolve paths only while holding the read side of
/// this lock, keyed by the canonical worktree root.
async fn canonical_workspace(
    deployment: &DeploymentImpl,
    workspace: &Workspace,
) -> Result<(PathBuf, PathBuf, WorkspaceFilesystemLock), ApiError> {
    let base = workspace_root(deployment, workspace).await?;
    let canonical_base = tokio::fs::canonicalize(&base)
        .await
        .map_err(|_| ApiError::File(FileError::NotFound))?;
    let filesystem_lock = workspace_filesystem_lock(&canonical_base);
    Ok((base, canonical_base, filesystem_lock))
}

/// Render `full` as a worktree-relative, forward-slash path. Fails closed if
/// `full` is not under `base` rather than leaking an absolute server path.
#[allow(clippy::result_large_err)]
fn relative_to(base: &Path, full: &Path) -> Result<String, ApiError> {
    Ok(full
        .strip_prefix(base)
        .map_err(|_| ApiError::File(FileError::NotFound))?
        .to_string_lossy()
        .replace('\\', "/"))
}

// --- GET /list ---

pub async fn list_files(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<PathQuery>,
) -> Result<ResponseJson<ApiResponse<WorkspaceDirListing>>, ApiError> {
    let (base, canonical_base, filesystem_lock) =
        canonical_workspace(&deployment, &workspace).await?;
    let _read_guard = filesystem_lock.read_owned().await;
    let dir = file_policy::resolve_existing_path(&base, &query.path)?;
    if !dir.is_dir() {
        return Err(ApiError::BadRequest("Path is not a directory".to_string()));
    }

    let mut entries = Vec::new();
    let mut truncated = false;
    let mut read_dir = tokio::fs::read_dir(&dir)
        .await
        .map_err(|_| ApiError::File(FileError::NotFound))?;

    while let Some(entry) = read_dir.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if file_policy::is_hidden(&name) || file_policy::is_denied_component(&name) {
            continue;
        }
        if entries.len() >= MAX_LIST_ENTRIES {
            truncated = true;
            break;
        }
        // symlink_metadata: never follow symlinks for type/size decisions.
        let meta = tokio::fs::symlink_metadata(entry.path()).await.ok();
        let is_symlink = meta
            .as_ref()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        entries.push(WorkspaceFileEntry {
            path: relative_to(&canonical_base, &entry.path())?,
            name,
            is_dir: meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
            is_symlink,
            size_bytes: meta.as_ref().map(|m| m.len() as i64).unwrap_or(0),
            modified: meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .map(DateTime::<Utc>::from),
        });
    }

    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    Ok(ResponseJson(ApiResponse::success(WorkspaceDirListing {
        path: relative_to(&canonical_base, &dir)?,
        entries,
        truncated,
    })))
}

// --- GET /download ---

/// Build a header-injection-proof `Content-Disposition` with an RFC 5987 filename.
/// Percent-encoding every non-unreserved byte makes CRLF/quote injection impossible.
fn attachment_disposition(name: &str) -> String {
    let mut out = String::from("attachment; filename*=UTF-8''");
    for &b in name.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub async fn download_file(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<PathQuery>,
) -> Result<Response, ApiError> {
    let (base, _canonical_base, filesystem_lock) =
        canonical_workspace(&deployment, &workspace).await?;
    let read_guard = filesystem_lock.read_owned().await;
    let path = file_policy::resolve_existing_path(&base, &query.path)?;
    // Open once, then read metadata from the handle so the streamed bytes and
    // the Content-Length come from the same inode (no reopen/swap race).
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| ApiError::File(FileError::NotFound))?;
    let meta = file
        .metadata()
        .await
        .map_err(|_| ApiError::File(FileError::NotFound))?;
    if !meta.is_file() {
        return Err(ApiError::File(FileError::NotFound));
    }
    let body = guarded_file_body(file, read_guard);

    let filename = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());

    // Always force a download as octet-stream + nosniff so no worktree file is
    // ever rendered inline (mirrors the hardened attachment serving path).
    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, meta.len())
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            header::CONTENT_DISPOSITION,
            attachment_disposition(&filename),
        )
        .body(body)
        .map_err(|e| ApiError::File(FileError::ResponseBuildError(e.to_string())))
}

// --- GET /download-zip ---

fn zip_io_error(msg: impl std::fmt::Display) -> ApiError {
    ApiError::Io(std::io::Error::other(msg.to_string()))
}

/// Build a safe, forward-slash zip entry name for `path` relative to `root`.
/// Only `Normal` components are accepted, and none may contain a path separator,
/// so a Unix filename containing `\` can't become a `../` zip-slip entry.
#[allow(clippy::result_large_err)]
fn zip_entry_name(root: &Path, path: &Path) -> Result<String, ApiError> {
    let rel = path.strip_prefix(root).map_err(zip_io_error)?;
    let mut parts = Vec::new();
    for component in rel.components() {
        let Component::Normal(part) = component else {
            return Err(ApiError::BadRequest("Unsafe archive path".to_string()));
        };
        let part = part.to_string_lossy();
        if part == "." || part == ".." || part.contains('/') || part.contains('\\') {
            return Err(ApiError::BadRequest("Unsafe archive path".to_string()));
        }
        parts.push(part.into_owned());
    }
    Ok(parts.join("/"))
}

/// Build a bounded zip of `root` into an anonymous temp file (auto-deleted when the
/// returned handle drops). Skips `.git`/`node_modules`/hidden/symlinks. Fails loud
/// when any cap is exceeded rather than emitting a silent partial archive.
#[allow(clippy::result_large_err)]
fn build_zip(root: &Path) -> Result<std::fs::File, ApiError> {
    let tmp = tempfile::tempfile()?;
    let mut zip = zip::ZipWriter::new(tmp);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let start = Instant::now();
    let mut entry_count: usize = 0;
    let mut total_bytes: u64 = 0;

    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true; // never skip the chosen root itself
            }
            let name = e.file_name().to_string_lossy();
            !(file_policy::is_denied_component(&name) || file_policy::is_hidden(&name))
        });

    for entry in walker {
        if start.elapsed() > ZIP_WALL_TIME {
            return Err(ApiError::BadRequest(
                "Archive generation timed out; select a smaller folder".to_string(),
            ));
        }
        let entry = entry.map_err(zip_io_error)?;
        let file_type = entry.file_type();
        if file_type.is_symlink() || !file_type.is_file() {
            continue;
        }
        let name = zip_entry_name(root, entry.path())?;
        if name.is_empty() {
            continue;
        }

        entry_count += 1;
        if entry_count > MAX_ZIP_ENTRIES {
            return Err(ApiError::BadRequest(
                "Folder has too many files to archive; select a smaller folder".to_string(),
            ));
        }

        // Bound the copy so a single oversized file can't blow the uncompressed
        // cap (or the temp disk) before a post-copy check would fire.
        let remaining = MAX_ZIP_UNCOMPRESSED_BYTES.saturating_sub(total_bytes);
        if remaining == 0 {
            return Err(ApiError::PayloadTooLarge);
        }
        zip.start_file(name, options).map_err(zip_io_error)?;
        let mut src = std::fs::File::open(entry.path())?;
        let written = std::io::copy(&mut src.by_ref().take(remaining + 1), &mut zip)?;
        if written > remaining {
            return Err(ApiError::PayloadTooLarge);
        }
        total_bytes += written;
    }

    let mut file = zip.finish().map_err(zip_io_error)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

pub async fn download_zip(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<PathQuery>,
) -> Result<Response, ApiError> {
    let (base, _canonical_base, filesystem_lock) =
        canonical_workspace(&deployment, &workspace).await?;
    let read_guard = filesystem_lock.read_owned().await;
    let root = file_policy::resolve_existing_path(&base, &query.path)?;
    if !root.is_dir() {
        return Err(ApiError::BadRequest(
            "Zip target is not a directory".to_string(),
        ));
    }

    let archive_name = root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());

    let (std_file, read_guard) =
        tokio::task::spawn_blocking(move || build_zip(&root).map(|file| (file, read_guard)))
            .await
            .map_err(zip_io_error)??;
    let file = tokio::fs::File::from_std(std_file);
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let body = guarded_file_body(file, read_guard);

    Response::builder()
        .header(header::CONTENT_TYPE, "application/zip")
        .header(header::CONTENT_LENGTH, len)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            header::CONTENT_DISPOSITION,
            attachment_disposition(&format!("{archive_name}.zip")),
        )
        .body(body)
        .map_err(|e| ApiError::File(FileError::ResponseBuildError(e.to_string())))
}

// --- POST /upload ---

async fn is_real_directory(path: &Path) -> Result<bool, ApiError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => Ok(metadata.file_type().is_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

async fn is_symlink(path: &Path) -> Result<bool, ApiError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => Ok(metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

async fn move_aside_non_regular_gitignore(
    uploads_dir: &Path,
    gitignore: &Path,
) -> Result<(), ApiError> {
    let backup = uploads_dir.join(format!(".gitignore.bak.{}", Uuid::new_v4()));
    tokio::fs::rename(gitignore, &backup).await?;
    tracing::warn!(
        original_path = %gitignore.display(),
        backup_path = %backup.display(),
        "Moved non-regular uploads .gitignore aside before repair"
    );
    Ok(())
}

async fn ensure_uploads_gitignore(uploads_dir: &Path) -> Result<(), ApiError> {
    let gitignore = uploads_dir.join(".gitignore");
    loop {
        match tokio::fs::symlink_metadata(&gitignore).await {
            Ok(metadata) if metadata.file_type().is_file() => {
                // Deliberately do not parse, validate, or rewrite an existing
                // regular .gitignore: it is user-owned data. The app guarantees
                // only that one exists (app-seeded files contain "*"); users own
                // any edits they make.
                return Ok(());
            }
            Ok(_) => {
                // Preserve directories, symlinks, sockets, and every other
                // non-regular entry. If the move fails, fail closed rather than
                // deleting user data or continuing without an ignore file.
                move_aside_non_regular_gitignore(uploads_dir, &gitignore).await?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&gitignore)
                    .await
                {
                    Ok(mut file) => {
                        // Fail loudly: a missing .gitignore would let uploads leak
                        // into git. create_new ensures a raced-in symlink is never
                        // followed outside the worktree.
                        let write_result = async {
                            file.write_all(b"*\n").await?;
                            file.flush().await
                        }
                        .await;
                        if let Err(error) = write_result {
                            drop(file);
                            let _ = tokio::fs::remove_file(&gitignore).await;
                            return Err(error.into());
                        }
                        return Ok(());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

async fn existing_canonical_uploads(
    canonical_base: &Path,
    directory: &Path,
) -> Result<Option<PathBuf>, ApiError> {
    let metadata = match tokio::fs::symlink_metadata(directory).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() {
        return Err(ApiError::BadRequest(
            "Upload directory must be a real directory".to_string(),
        ));
    }

    let canonical = tokio::fs::canonicalize(directory).await?;
    if !canonical.starts_with(canonical_base) {
        return Err(ApiError::BadRequest(
            "Upload directory escapes the workspace".to_string(),
        ));
    }
    Ok(Some(canonical))
}

async fn create_real_uploads_directory(uploads_dir: &Path) -> Result<(), ApiError> {
    tokio::fs::create_dir_all(uploads_dir).await?;
    if !is_real_directory(uploads_dir).await? {
        return Err(ApiError::BadRequest(
            "Upload directory must be a real directory".to_string(),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ResolvedUploadsDirectory {
    Current,
    Legacy,
}

async fn probe_resolved_state(
    uploads_dir: &Path,
    legacy_uploads_dir: &Path,
) -> Result<ResolvedUploadsDirectory, ApiError> {
    if is_real_directory(uploads_dir).await? {
        Ok(ResolvedUploadsDirectory::Current)
    } else if is_real_directory(legacy_uploads_dir).await? {
        Ok(ResolvedUploadsDirectory::Legacy)
    } else {
        // Both paths may vanish between earlier probes and this re-check.
        // Re-create the current directory instead of returning a dead path.
        create_real_uploads_directory(uploads_dir).await?;
        Ok(ResolvedUploadsDirectory::Current)
    }
}

/// Resolve the default upload directory, migrating the legacy directory when
/// possible while preserving it as a lossless fallback on any rename failure.
/// The migration takes the per-workspace write lock only with
/// `try_write_owned`, so a request already holding a read lock is never blocked
/// or renamed underneath.
async fn resolve_uploads_dir_impl<F, R>(
    canonical_base: &Path,
    after_state_probes: F,
    before_rename: R,
) -> Result<PathBuf, ApiError>
where
    F: FnOnce(&Path, &Path),
    R: FnOnce() + Send + 'static,
{
    let uploads_dir = canonical_base.join(UPLOADS_DIR);
    let legacy_uploads_dir = canonical_base.join(LEGACY_UPLOADS_DIR);

    let uploads_exists = is_real_directory(&uploads_dir).await?;
    let legacy_exists = is_real_directory(&legacy_uploads_dir).await?;

    // Test seam for deterministic cross-process-style create/rename races.
    after_state_probes(&uploads_dir, &legacy_uploads_dir);

    // A legacy symlink is ignored so a fresh current directory can be created,
    // but the current name is authoritative: never fall back around a symlink
    // there, even when a real legacy directory is also present.
    if is_symlink(&uploads_dir).await? {
        return Err(ApiError::BadRequest(
            "Upload directory must be a real directory".to_string(),
        ));
    }

    let mut _contention_read_guard = None;
    let resolved_dir = match (uploads_exists, legacy_exists) {
        (true, _) => ResolvedUploadsDirectory::Current,
        (false, true) => {
            let filesystem_lock = workspace_filesystem_lock(canonical_base);
            match filesystem_lock.clone().try_write_owned() {
                Ok(write_guard) => {
                    let rename_old_path = legacy_uploads_dir.clone();
                    let rename_new_path = uploads_dir.clone();
                    let rename_result = tokio::task::spawn_blocking(move || {
                        // Keep the owned guard inside the blocking task. Dropping
                        // the awaiting resolver detaches this task, so the guard
                        // must live here until the real filesystem call returns.
                        let _write_guard = write_guard;
                        before_rename();
                        std::fs::rename(rename_old_path, rename_new_path)
                    })
                    .await;
                    let rename_result = match rename_result {
                        Ok(result) => result,
                        Err(error) => Err(std::io::Error::other(format!(
                            "Uploads directory migration task failed: {error}"
                        ))),
                    };

                    match rename_result {
                        Ok(()) => {
                            tracing::info!(
                                old_path = %legacy_uploads_dir.display(),
                                new_path = %uploads_dir.display(),
                                "Migrated workspace uploads directory"
                            );
                            ResolvedUploadsDirectory::Current
                        }
                        Err(error) => {
                            tracing::warn!(
                                old_path = %legacy_uploads_dir.display(),
                                new_path = %uploads_dir.display(),
                                error = %error,
                                "Uploads directory migration rename failed; resolving current state"
                            );
                            // Keep this re-check even with the in-process lock: an
                            // old binary or another process can still win the race.
                            probe_resolved_state(&uploads_dir, &legacy_uploads_dir).await?
                        }
                    }
                }
                Err(_) => {
                    tracing::debug!(
                        old_path = %legacy_uploads_dir.display(),
                        new_path = %uploads_dir.display(),
                        "Skipping uploads directory migration because the workspace filesystem lock is contended"
                    );

                    // Usually contention is an in-flight reader, so this takes
                    // another read guard immediately (including when the caller
                    // already holds one). If a rename writer is active, wait only
                    // for that already-started atomic operation; every handler
                    // would need the same read side before resolving a path.
                    _contention_read_guard = Some(match filesystem_lock.clone().try_read_owned() {
                        Ok(guard) => guard,
                        Err(_) => filesystem_lock.clone().read_owned().await,
                    });

                    probe_resolved_state(&uploads_dir, &legacy_uploads_dir).await?
                }
            }
        }
        (false, false) => {
            create_real_uploads_directory(&uploads_dir).await?;
            ResolvedUploadsDirectory::Current
        }
    };

    // Both-present is intentional. Validate containment before writing either
    // .gitignore, then assert the ignore file on every real upload directory.
    let mut current = if uploads_exists || matches!(resolved_dir, ResolvedUploadsDirectory::Current)
    {
        existing_canonical_uploads(canonical_base, &uploads_dir).await?
    } else {
        None
    };
    let mut legacy = if legacy_exists || matches!(resolved_dir, ResolvedUploadsDirectory::Legacy) {
        existing_canonical_uploads(canonical_base, &legacy_uploads_dir).await?
    } else {
        None
    };

    if current.is_none() && legacy.is_none() {
        // The two probes above can straddle another process's atomic legacy ->
        // current rename (current absent just before it, legacy absent just
        // after it). A final current-path probe closes that false-neither edge.
        current = existing_canonical_uploads(canonical_base, &uploads_dir).await?;
        if current.is_none() {
            legacy = existing_canonical_uploads(canonical_base, &legacy_uploads_dir).await?;
        }
    }

    if let Some(directory) = &current {
        ensure_uploads_gitignore(directory).await?;
    }
    if let Some(directory) = &legacy {
        ensure_uploads_gitignore(directory).await?;
    }

    match resolved_dir {
        ResolvedUploadsDirectory::Current => current
            .or(legacy)
            .ok_or_else(|| ApiError::BadRequest("Upload directory no longer exists".to_string())),
        // A concurrent in-process writer may complete its rename after this
        // resolver skips migration. Prefer the still-present legacy directory,
        // but converge on current if legacy moved before the final check.
        ResolvedUploadsDirectory::Legacy => legacy.or(current).ok_or_else(|| {
            ApiError::BadRequest("Legacy upload directory no longer exists".to_string())
        }),
    }
}

/// Resolve the default upload directory; see [`LEGACY_UPLOADS_DIR`] for the
/// rollback caveat.
async fn resolve_uploads_dir(canonical_base: &Path) -> Result<PathBuf, ApiError> {
    resolve_uploads_dir_impl(canonical_base, |_, _| {}, || {}).await
}

async fn publish_upload_once(
    tmp_path: &Path,
    final_path: &Path,
    overwrite: bool,
) -> std::io::Result<()> {
    if overwrite {
        tokio::fs::rename(tmp_path, final_path).await
    } else {
        tokio::fs::hard_link(tmp_path, final_path).await
    }
}

async fn cleanup_upload_temp(path: &Path) {
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "Failed to clean up temporary upload file"
            );
        }
    }
}

async fn cleanup_upload_temp_paths(original: &Path, relocated: Option<&Path>) {
    cleanup_upload_temp(original).await;
    if let Some(relocated) = relocated.filter(|path| *path != original) {
        cleanup_upload_temp(relocated).await;
    }
}

async fn upload_target_is_missing(target_dir: &Path) -> bool {
    matches!(
        tokio::fs::symlink_metadata(target_dir).await,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

fn map_upload_publish_error(error: std::io::Error, name: &str, overwrite: bool) -> ApiError {
    if !overwrite && error.kind() == std::io::ErrorKind::AlreadyExists {
        ApiError::Conflict(format!("File '{name}' already exists"))
    } else {
        error.into()
    }
}

async fn publish_streamed_upload(
    canonical_base: &Path,
    target_dir: &Path,
    tmp_name: &str,
    name: &str,
    overwrite: bool,
    retry_default_publish: bool,
) -> Result<(PathBuf, PathBuf), ApiError> {
    let original_tmp_path = target_dir.join(tmp_name);
    let original_final_path = target_dir.join(name);
    let initial_error =
        match publish_upload_once(&original_tmp_path, &original_final_path, overwrite).await {
            Ok(()) => {
                cleanup_upload_temp_paths(&original_tmp_path, None).await;
                return Ok((target_dir.to_path_buf(), original_final_path));
            }
            Err(error) => error,
        };

    let retry_after_move = retry_default_publish
        && initial_error.kind() == std::io::ErrorKind::NotFound
        && upload_target_is_missing(target_dir).await;
    if !retry_after_move {
        cleanup_upload_temp_paths(&original_tmp_path, None).await;
        return Err(map_upload_publish_error(initial_error, name, overwrite));
    }

    tracing::warn!(
        original_target_dir = %target_dir.display(),
        error = %initial_error,
        "Default uploads directory moved during publish; re-resolving and retrying once"
    );

    let retry_target_dir = match resolve_uploads_dir(canonical_base).await {
        Ok(directory) => directory,
        Err(error) => {
            // If resolution itself fails, the atomic migration can only have
            // moved this UUID-named temp file between the two default dirs.
            let current_tmp = canonical_base.join(UPLOADS_DIR).join(tmp_name);
            let legacy_tmp = canonical_base.join(LEGACY_UPLOADS_DIR).join(tmp_name);
            cleanup_upload_temp_paths(&original_tmp_path, Some(&current_tmp)).await;
            cleanup_upload_temp_paths(&original_tmp_path, Some(&legacy_tmp)).await;
            return Err(error);
        }
    };
    let retry_tmp_path = retry_target_dir.join(tmp_name);
    let retry_final_path = retry_target_dir.join(name);
    let retry_result = publish_upload_once(&retry_tmp_path, &retry_final_path, overwrite).await;

    // Whether the retry succeeds or fails, the hard-link publication path and
    // every error path get best-effort cleanup at both possible locations.
    cleanup_upload_temp_paths(&original_tmp_path, Some(&retry_tmp_path)).await;

    match retry_result {
        Ok(()) => Ok((retry_target_dir, retry_final_path)),
        Err(error) => Err(map_upload_publish_error(error, name, overwrite)),
    }
}

pub async fn upload_files(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<UploadQuery>,
    mut multipart: Multipart,
) -> Result<ResponseJson<ApiResponse<Vec<WorkspaceFileEntry>>>, ApiError> {
    let (base, canonical_base, filesystem_lock) =
        canonical_workspace(&deployment, &workspace).await?;
    let custom_path = query.path.as_deref().filter(|path| !path.trim().is_empty());

    if custom_path.is_none() {
        // Give migration one non-blocking, best-effort chance before taking this
        // request's read guard. The result is deliberately discarded: after
        // acquiring the read side we resolve again, so no other in-process
        // migration can move the selected directory during multipart streaming
        // and publication. That guarded resolve remains authoritative.
        if let Err(error) = resolve_uploads_dir(&canonical_base).await {
            tracing::warn!(
                error = %error,
                "Best-effort uploads directory resolve failed before acquiring the workspace filesystem lock"
            );
        }
    }
    let _read_guard = filesystem_lock.read_owned().await;

    let mut target_dir = match custom_path {
        Some(path) => file_policy::resolve_existing_dir(&base, path)?,
        None => resolve_uploads_dir(&canonical_base).await?,
    };

    let mut uploaded = Vec::new();
    while let Some(mut field) = multipart.next_field().await? {
        let Some(raw_name) = field.file_name().map(|s| s.to_string()) else {
            continue; // skip non-file form fields
        };
        if uploaded.len() >= MAX_UPLOAD_FILES {
            return Err(ApiError::BadRequest(format!(
                "Too many files (max {MAX_UPLOAD_FILES})"
            )));
        }
        let name = file_policy::safe_basename(&raw_name)?;
        let prospective_final_path = target_dir.join(&name);

        if !query.overwrite
            && tokio::fs::try_exists(&prospective_final_path)
                .await
                .unwrap_or(false)
        {
            return Err(ApiError::Conflict(format!("File '{name}' already exists")));
        }

        // Stream to a temp file in the target dir (same filesystem → atomic rename).
        let tmp_name = format!(".bc-upload-{}.tmp", Uuid::new_v4());
        let tmp_path = target_dir.join(&tmp_name);
        let mut out = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .await?;

        let mut written: u64 = 0;
        let mut over_limit = false;
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    written += chunk.len() as u64;
                    if written > MAX_UPLOAD_FILE_BYTES {
                        over_limit = true;
                        break;
                    }
                    if let Err(e) = out.write_all(&chunk).await {
                        let _ = tokio::fs::remove_file(&tmp_path).await;
                        return Err(e.into());
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    return Err(e.into());
                }
            }
        }
        if over_limit {
            let _ = out.shutdown().await;
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(ApiError::PayloadTooLarge);
        }
        out.flush().await?;
        drop(out);

        // Publish atomically. For no-overwrite, hard_link fails if the target
        // already exists, closing the try_exists -> rename race (TOCTOU). A
        // default-folder publish gets one recovery attempt if another process
        // atomically migrates the directory after streaming began.
        let (published_target_dir, final_path) = publish_streamed_upload(
            &canonical_base,
            &target_dir,
            &tmp_name,
            &name,
            query.overwrite,
            custom_path.is_none(),
        )
        .await?;
        target_dir = published_target_dir;

        let meta = tokio::fs::symlink_metadata(&final_path).await.ok();
        uploaded.push(WorkspaceFileEntry {
            path: relative_to(&canonical_base, &final_path)?,
            name,
            is_dir: false,
            is_symlink: false,
            size_bytes: meta
                .as_ref()
                .map(|m| m.len() as i64)
                .unwrap_or(written as i64),
            modified: meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .map(DateTime::<Utc>::from),
        });
    }

    if uploaded.is_empty() {
        return Err(ApiError::BadRequest("No files in upload".to_string()));
    }
    Ok(ResponseJson(ApiResponse::success(uploaded)))
}

// --- local-only guard ---

/// Reject relay/host-proxied requests so the Files API is local-direct only.
async fn reject_relay_requests(request: Request, next: Next) -> Result<Response, ApiError> {
    let is_relay = request
        .headers()
        .get(relay_client::RELAY_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim() == "1");
    if is_relay {
        return Err(ApiError::Forbidden(
            "The Files browser is only available on local deployments".to_string(),
        ));
    }
    Ok(next.run(request).await.into_response())
}

pub fn router(deployment: &DeploymentImpl) -> Router<DeploymentImpl> {
    Router::new()
        .route("/list", get(list_files))
        .route("/download", get(download_file))
        .route("/download-zip", get(download_zip))
        .route(
            "/upload",
            post(upload_files).layer(DefaultBodyLimit::max(MAX_UPLOAD_REQUEST_BYTES)),
        )
        .layer(from_fn_with_state(
            deployment.clone(),
            load_workspace_middleware,
        ))
        .layer(from_fn(reject_relay_requests))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;

    const GITIGNORE_CONTENTS: &str = "*\n";

    fn seed_upload_dir(base: &Path, name: &str) -> PathBuf {
        let path = base.join(name);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join(".gitignore"), GITIGNORE_CONTENTS).unwrap();
        path
    }

    fn assert_gitignored(path: &Path) {
        let metadata = fs::symlink_metadata(path.join(".gitignore")).unwrap();
        assert!(metadata.file_type().is_file());
        assert_eq!(
            fs::read_to_string(path.join(".gitignore")).unwrap(),
            GITIGNORE_CONTENTS
        );
    }

    fn gitignore_backups(path: &Path) -> Vec<PathBuf> {
        let mut backups = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|entry| {
                entry
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".gitignore.bak."))
            })
            .collect::<Vec<_>>();
        backups.sort();
        backups
    }

    #[tokio::test]
    async fn uploads_dir_resolver_creates_new_dir_when_neither_exists() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let expected = base.join(UPLOADS_DIR);

        let resolved = resolve_uploads_dir(&base).await.unwrap();

        assert_eq!(resolved, expected);
        assert!(expected.is_dir());
        assert!(!base.join(LEGACY_UPLOADS_DIR).exists());
        assert_gitignored(&expected);
    }

    #[tokio::test]
    async fn uploads_dir_resolver_migrates_legacy_only_dir() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let legacy = seed_upload_dir(&base, LEGACY_UPLOADS_DIR);
        fs::create_dir_all(legacy.join("nested")).unwrap();
        fs::write(legacy.join("report.txt"), b"legacy report").unwrap();
        fs::write(legacy.join("nested/data.bin"), b"legacy data").unwrap();
        let expected = base.join(UPLOADS_DIR);

        let resolved = resolve_uploads_dir(&base).await.unwrap();

        assert_eq!(resolved, expected);
        assert!(expected.is_dir());
        assert!(!legacy.exists());
        assert_gitignored(&expected);
        assert_eq!(
            fs::read(expected.join("report.txt")).unwrap(),
            b"legacy report"
        );
        assert_eq!(
            fs::read(expected.join("nested/data.bin")).unwrap(),
            b"legacy data"
        );
    }

    #[tokio::test]
    async fn uploads_dir_migration_skips_in_flight_request_then_converges() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let legacy = seed_upload_dir(&base, LEGACY_UPLOADS_DIR);
        let temporary = legacy.join(".bc-upload-in-flight.tmp");
        let published = legacy.join("report.txt");
        fs::write(&temporary, b"streamed upload").unwrap();
        let expected = base.join(UPLOADS_DIR);

        let filesystem_lock = workspace_filesystem_lock(&base);
        let read_guard = filesystem_lock.clone().read_owned().await;

        let resolved_while_streaming = resolve_uploads_dir(&base).await.unwrap();

        assert_eq!(resolved_while_streaming, legacy);
        assert!(legacy.is_dir());
        assert!(!expected.exists());
        fs::rename(&temporary, &published).unwrap();
        drop(read_guard);

        let resolved_after_publish = resolve_uploads_dir(&base).await.unwrap();

        assert_eq!(resolved_after_publish, expected);
        assert!(!legacy.exists());
        assert!(!expected.join(".bc-upload-in-flight.tmp").exists());
        assert_eq!(
            fs::read(expected.join("report.txt")).unwrap(),
            b"streamed upload"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uploads_dir_migration_guard_outlives_resolver_cancellation() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let legacy = seed_upload_dir(&base, LEGACY_UPLOADS_DIR);
        fs::write(legacy.join("report.txt"), b"legacy report").unwrap();
        let expected = base.join(UPLOADS_DIR);
        let filesystem_lock = workspace_filesystem_lock(&base);
        let (rename_started_tx, rename_started_rx) = tokio::sync::oneshot::channel();
        let (release_rename_tx, release_rename_rx) = tokio::sync::oneshot::channel();

        let resolver_base = base.clone();
        let resolver = tokio::spawn(async move {
            resolve_uploads_dir_impl(
                &resolver_base,
                |_, _| {},
                move || {
                    let _ = rename_started_tx.send(());
                    let _ = release_rename_rx.blocking_recv();
                },
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(5), rename_started_rx)
            .await
            .expect("blocking rename did not start")
            .unwrap();
        resolver.abort();
        assert!(resolver.await.unwrap_err().is_cancelled());

        // The blocking task has not renamed yet, but it must still own the
        // write guard after its awaiting resolver has been cancelled.
        assert!(legacy.is_dir());
        assert!(!expected.exists());
        assert_eq!(
            fs::read(legacy.join("report.txt")).unwrap(),
            b"legacy report"
        );
        assert!(filesystem_lock.clone().try_read_owned().is_err());

        release_rename_tx.send(()).unwrap();
        let completed_guard =
            tokio::time::timeout(Duration::from_secs(5), filesystem_lock.clone().read_owned())
                .await
                .expect("blocking rename did not release the write guard");
        drop(completed_guard);

        assert!(expected.is_dir());
        assert!(!legacy.exists());
        assert_eq!(
            fs::read(expected.join("report.txt")).unwrap(),
            b"legacy report"
        );
        assert_eq!(resolve_uploads_dir(&base).await.unwrap(), expected);
    }

    #[tokio::test]
    async fn uploads_dir_resolver_rechecks_after_losing_rename_race() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let legacy = seed_upload_dir(&base, LEGACY_UPLOADS_DIR);
        fs::write(legacy.join("report.txt"), b"legacy report").unwrap();
        let expected = base.join(UPLOADS_DIR);

        let resolved = resolve_uploads_dir_impl(
            &base,
            |current, legacy| {
                fs::rename(legacy, current).unwrap();
            },
            || {},
        )
        .await
        .unwrap();

        assert_eq!(resolved, expected);
        assert!(!legacy.exists());
        assert_eq!(
            fs::read(expected.join("report.txt")).unwrap(),
            b"legacy report"
        );
        assert_gitignored(&expected);
    }

    #[tokio::test]
    async fn uploads_dir_resolver_converges_when_create_races_rename() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let legacy = seed_upload_dir(&base, LEGACY_UPLOADS_DIR);
        fs::write(legacy.join("legacy.txt"), b"legacy data").unwrap();
        let expected = base.join(UPLOADS_DIR);

        let resolved = resolve_uploads_dir_impl(
            &base,
            |current, _legacy| {
                fs::create_dir(current).unwrap();
                fs::write(current.join("new.txt"), b"new data").unwrap();
            },
            || {},
        )
        .await
        .unwrap();

        assert_eq!(resolved, expected);
        assert!(legacy.is_dir());
        assert_eq!(fs::read(expected.join("new.txt")).unwrap(), b"new data");
        assert_eq!(fs::read(legacy.join("legacy.txt")).unwrap(), b"legacy data");
        assert_gitignored(&expected);
        assert_gitignored(&legacy);
    }

    #[tokio::test]
    async fn uploads_dir_resolver_creates_current_when_legacy_vanishes() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let legacy = seed_upload_dir(&base, LEGACY_UPLOADS_DIR);
        let expected = base.join(UPLOADS_DIR);

        let resolved = resolve_uploads_dir_impl(
            &base,
            |_current, legacy| {
                fs::remove_dir_all(legacy).unwrap();
            },
            || {},
        )
        .await
        .unwrap();

        assert_eq!(resolved, expected);
        assert!(!legacy.exists());
        assert!(expected.is_dir());
        assert_gitignored(&expected);
    }

    #[tokio::test]
    async fn uploads_dir_resolver_returns_new_only_dir_untouched() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let expected = seed_upload_dir(&base, UPLOADS_DIR);
        fs::write(expected.join("new.txt"), b"new data").unwrap();
        fs::remove_file(expected.join(".gitignore")).unwrap();

        let resolved = resolve_uploads_dir(&base).await.unwrap();

        assert_eq!(resolved, expected);
        assert!(expected.is_dir());
        assert!(!base.join(LEGACY_UPLOADS_DIR).exists());
        assert_gitignored(&expected);
        assert_eq!(fs::read(expected.join("new.txt")).unwrap(), b"new data");
    }

    #[tokio::test]
    async fn uploads_dir_resolver_preserves_regular_gitignore_contents() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let expected = seed_upload_dir(&base, UPLOADS_DIR);
        fs::write(expected.join(".gitignore"), b"*.secret\n").unwrap();

        let resolved = resolve_uploads_dir(&base).await.unwrap();

        assert_eq!(resolved, expected);
        assert_eq!(
            fs::read_to_string(expected.join(".gitignore")).unwrap(),
            "*.secret\n"
        );
    }

    #[tokio::test]
    async fn uploads_dir_resolver_preserves_gitignore_directory_as_backup() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let expected = seed_upload_dir(&base, UPLOADS_DIR);
        let gitignore = expected.join(".gitignore");
        fs::remove_file(&gitignore).unwrap();
        fs::create_dir(&gitignore).unwrap();
        fs::write(gitignore.join("nested"), b"not an ignore file").unwrap();

        let resolved = resolve_uploads_dir(&base).await.unwrap();

        assert_eq!(resolved, expected);
        let backups = gitignore_backups(&expected);
        assert_eq!(backups.len(), 1);
        assert_eq!(
            fs::read(backups[0].join("nested")).unwrap(),
            b"not an ignore file"
        );
        assert_gitignored(&expected);
    }

    #[tokio::test]
    async fn uploads_dir_resolver_preserves_existing_gitignore_backup_without_clobbering() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let expected = seed_upload_dir(&base, UPLOADS_DIR);
        let gitignore = expected.join(".gitignore");
        fs::remove_file(&gitignore).unwrap();
        fs::create_dir(&gitignore).unwrap();
        fs::write(gitignore.join("nested"), b"preserve me").unwrap();
        let existing_backup = expected.join(".gitignore.bak.00000000-0000-0000-0000-000000000000");
        fs::create_dir(&existing_backup).unwrap();
        fs::write(existing_backup.join("nested"), b"existing backup").unwrap();

        let resolved = resolve_uploads_dir(&base).await.unwrap();

        assert_eq!(resolved, expected);
        let backups = gitignore_backups(&expected);
        assert_eq!(backups.len(), 2);
        assert_eq!(
            fs::read(existing_backup.join("nested")).unwrap(),
            b"existing backup"
        );
        let new_backup = backups
            .iter()
            .find(|backup| **backup != existing_backup)
            .unwrap();
        assert_eq!(fs::read(new_backup.join("nested")).unwrap(), b"preserve me");
        assert_gitignored(&expected);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uploads_dir_resolver_preserves_gitignore_symlink_as_backup() {
        use std::os::unix::fs::symlink;

        let tempdir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let expected = seed_upload_dir(&base, UPLOADS_DIR);
        let gitignore = expected.join(".gitignore");
        let outside_file = outside.path().join("outside-ignore");
        fs::write(&outside_file, b"outside data").unwrap();
        fs::remove_file(&gitignore).unwrap();
        symlink(&outside_file, &gitignore).unwrap();

        let resolved = resolve_uploads_dir(&base).await.unwrap();

        assert_eq!(resolved, expected);
        assert_gitignored(&expected);
        let backups = gitignore_backups(&expected);
        assert_eq!(backups.len(), 1);
        let backup = &backups[0];
        assert!(
            fs::symlink_metadata(backup)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(backup).unwrap(), outside_file);
        assert_eq!(fs::read(outside_file).unwrap(), b"outside data");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uploads_dir_resolver_ignores_legacy_symlink_and_creates_real_current_dir() {
        use std::os::unix::fs::symlink;

        let tempdir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let legacy = base.join(LEGACY_UPLOADS_DIR);
        let expected = base.join(UPLOADS_DIR);
        symlink(outside.path(), &legacy).unwrap();

        let resolved = resolve_uploads_dir(&base).await.unwrap();

        assert_eq!(resolved, expected);
        assert!(
            fs::symlink_metadata(&legacy)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            fs::symlink_metadata(&expected)
                .unwrap()
                .file_type()
                .is_dir()
        );
        assert_gitignored(&expected);
        assert!(!outside.path().join(".gitignore").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uploads_dir_resolver_rejects_current_symlink_outside_workspace() {
        use std::os::unix::fs::symlink;

        let tempdir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let current = base.join(UPLOADS_DIR);
        let legacy = seed_upload_dir(&base, LEGACY_UPLOADS_DIR);
        symlink(outside.path(), &current).unwrap();

        let error = resolve_uploads_dir(&base).await.unwrap_err();

        assert!(matches!(error, ApiError::BadRequest(_)));
        assert!(
            fs::symlink_metadata(current)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(legacy.is_dir());
        assert!(!outside.path().join(".gitignore").exists());
    }

    #[tokio::test]
    async fn uploads_dir_resolver_prefers_new_dir_when_both_exist() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let expected = seed_upload_dir(&base, UPLOADS_DIR);
        let legacy = seed_upload_dir(&base, LEGACY_UPLOADS_DIR);
        fs::write(expected.join("new.txt"), b"new data").unwrap();
        fs::write(legacy.join("legacy.txt"), b"legacy data").unwrap();
        fs::remove_file(expected.join(".gitignore")).unwrap();
        fs::remove_file(legacy.join(".gitignore")).unwrap();

        let resolved = resolve_uploads_dir(&base).await.unwrap();

        assert_eq!(resolved, expected);
        assert!(expected.is_dir());
        assert!(legacy.is_dir());
        assert_gitignored(&expected);
        assert_gitignored(&legacy);
        assert_eq!(fs::read(expected.join("new.txt")).unwrap(), b"new data");
        assert_eq!(fs::read(legacy.join("legacy.txt")).unwrap(), b"legacy data");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_uploads_dir_resolvers_preserve_legacy_files() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let legacy = seed_upload_dir(&base, LEGACY_UPLOADS_DIR);
        fs::create_dir_all(legacy.join("nested")).unwrap();
        fs::write(legacy.join("first.txt"), b"first").unwrap();
        fs::write(legacy.join("nested/second.txt"), b"second").unwrap();
        let expected = base.join(UPLOADS_DIR);

        let mut calls = Vec::new();
        for _ in 0..4 {
            let base = base.clone();
            calls.push(tokio::spawn(
                async move { resolve_uploads_dir(&base).await },
            ));
        }
        for call in calls {
            assert_eq!(call.await.unwrap().unwrap(), expected);
        }

        assert!(expected.is_dir());
        assert!(!legacy.exists());
        assert_gitignored(&expected);
        assert_eq!(fs::read(expected.join("first.txt")).unwrap(), b"first");
        assert_eq!(
            fs::read(expected.join("nested/second.txt")).unwrap(),
            b"second"
        );
    }

    #[tokio::test]
    async fn uploads_dir_resolver_falls_back_to_legacy_on_rename_failure() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let legacy = seed_upload_dir(&base, LEGACY_UPLOADS_DIR);
        fs::write(legacy.join("keep.txt"), b"do not lose me").unwrap();
        fs::remove_file(legacy.join(".gitignore")).unwrap();
        let blocked_new_path = base.join(UPLOADS_DIR);
        fs::write(&blocked_new_path, b"not a directory").unwrap();

        let resolved = resolve_uploads_dir(&base).await.unwrap();

        assert_eq!(resolved, legacy);
        assert!(legacy.is_dir());
        assert_gitignored(&legacy);
        assert_eq!(
            fs::read(legacy.join("keep.txt")).unwrap(),
            b"do not lose me"
        );
        assert_eq!(fs::read(blocked_new_path).unwrap(), b"not a directory");
        // When both paths are directories, new-dir precedence is covered by
        // uploads_dir_resolver_prefers_new_dir_when_both_exist.
    }

    #[tokio::test]
    async fn default_upload_publish_retries_after_external_migration() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let legacy = seed_upload_dir(&base, LEGACY_UPLOADS_DIR);
        let expected = base.join(UPLOADS_DIR);
        let tmp_name = ".bc-upload-retry.tmp";
        fs::write(legacy.join(tmp_name), b"streamed upload").unwrap();
        fs::rename(&legacy, &expected).unwrap();

        let (published_dir, final_path) =
            publish_streamed_upload(&base, &legacy, tmp_name, "report.txt", false, true)
                .await
                .unwrap();

        assert_eq!(published_dir, expected);
        assert_eq!(final_path, expected.join("report.txt"));
        assert_eq!(fs::read(final_path).unwrap(), b"streamed upload");
        assert!(!legacy.join(tmp_name).exists());
        assert!(!expected.join(tmp_name).exists());
    }

    #[tokio::test]
    async fn default_upload_publish_retry_failure_cleans_both_temp_paths() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tempdir.path()).unwrap();
        let legacy = seed_upload_dir(&base, LEGACY_UPLOADS_DIR);
        let expected = base.join(UPLOADS_DIR);
        let tmp_name = ".bc-upload-conflict.tmp";
        fs::write(legacy.join(tmp_name), b"streamed upload").unwrap();
        fs::write(legacy.join("report.txt"), b"existing report").unwrap();
        fs::rename(&legacy, &expected).unwrap();

        let error = publish_streamed_upload(&base, &legacy, tmp_name, "report.txt", false, true)
            .await
            .unwrap_err();

        assert!(matches!(error, ApiError::Conflict(_)));
        assert_eq!(
            fs::read(expected.join("report.txt")).unwrap(),
            b"existing report"
        );
        assert!(!legacy.join(tmp_name).exists());
        assert!(!expected.join(tmp_name).exists());
    }

    #[test]
    fn workspace_filesystem_lock_prunes_dead_entries() {
        let first_tempdir = tempfile::tempdir().unwrap();
        let first_base = fs::canonicalize(first_tempdir.path()).unwrap();
        let first_lock = workspace_filesystem_lock(&first_base);
        drop(first_lock);

        let second_tempdir = tempfile::tempdir().unwrap();
        let second_base = fs::canonicalize(second_tempdir.path()).unwrap();
        let second_lock = workspace_filesystem_lock(&second_base);

        let locks = WORKSPACE_FILESYSTEM_LOCKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(!locks.contains_key(&first_base));
        assert!(locks.get(&second_base).and_then(Weak::upgrade).is_some());
        drop(locks);
        drop(second_lock);
    }

    #[test]
    fn attachment_disposition_is_injection_proof() {
        // CRLF + quote in a malicious filename must be percent-encoded away.
        let d = attachment_disposition("evil\r\nSet-Cookie: x=\"y\".txt");
        assert!(d.starts_with("attachment; filename*=UTF-8''"));
        assert!(!d.contains('\r'));
        assert!(!d.contains('\n'));
        assert!(!d.contains('"'));
        // unreserved chars survive; the ".txt" extension stays readable
        assert!(d.contains(".txt"));
        // a plain name round-trips its safe chars
        assert_eq!(
            attachment_disposition("report.csv"),
            "attachment; filename*=UTF-8''report.csv"
        );
    }

    #[test]
    fn build_zip_excludes_denylisted_hidden_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("keep.txt"), b"hello").unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/inner.txt"), b"world").unwrap();
        fs::create_dir_all(root.join(".git/hooks")).unwrap();
        fs::write(root.join(".git/hooks/post-checkout"), b"#!/bin/sh\n").unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), b"x").unwrap();
        fs::write(root.join(".secret"), b"shh").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("keep.txt"), root.join("link.txt")).unwrap();

        let file = build_zip(root).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();

        assert!(names.contains(&"keep.txt".to_string()));
        assert!(names.contains(&"sub/inner.txt".to_string()));
        // denylisted, hidden, and symlinked entries are never archived
        assert!(names.iter().all(|n| !n.contains(".git")));
        assert!(names.iter().all(|n| !n.contains("node_modules")));
        assert!(!names.contains(&".secret".to_string()));
        #[cfg(unix)]
        assert!(!names.contains(&"link.txt".to_string()));
    }

    #[test]
    fn zip_entry_name_rejects_backslash_and_traversal() {
        let root = Path::new("/base");
        assert_eq!(
            zip_entry_name(root, Path::new("/base/a/b.txt")).unwrap(),
            "a/b.txt"
        );
        // A Unix filename containing a backslash must be rejected, never
        // rewritten into a "/" separator (zip-slip on extraction).
        assert!(zip_entry_name(root, Path::new("/base/..\\evil.txt")).is_err());
    }
}
