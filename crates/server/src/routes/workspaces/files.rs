//! Workspace Files API: browse/download/zip/upload files in a workspace worktree.
//!
//! Local-only (a `reject_relay_requests` guard blocks relay/host-proxied access).
//! All filesystem access goes through [`super::file_policy`] so path-safety, the
//! `.git`/`node_modules` denylist, and size caps live in one place.

use std::{
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
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
use serde::{Deserialize, Serialize};
use services::services::{container::ContainerService, file::FileError};
use tokio::io::AsyncWriteExt;
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
    let base = workspace_root(&deployment, &workspace).await?;
    let canonical_base = tokio::fs::canonicalize(&base)
        .await
        .map_err(|_| ApiError::File(FileError::NotFound))?;
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
    let base = workspace_root(&deployment, &workspace).await?;
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
    let body = Body::from_stream(ReaderStream::new(file));

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
    let base = workspace_root(&deployment, &workspace).await?;
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

    let std_file = tokio::task::spawn_blocking(move || build_zip(&root))
        .await
        .map_err(zip_io_error)??;
    let file = tokio::fs::File::from_std(std_file);
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let body = Body::from_stream(ReaderStream::new(file));

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

async fn is_directory(path: &Path) -> Result<bool, ApiError> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

async fn ensure_uploads_gitignore(uploads_dir: &Path) -> Result<(), ApiError> {
    let gitignore = uploads_dir.join(".gitignore");
    if !tokio::fs::try_exists(&gitignore).await.unwrap_or(false) {
        // Fail loudly: a missing .gitignore would let uploads leak into git.
        tokio::fs::write(&gitignore, "*\n").await?;
    }
    Ok(())
}

/// Resolve the default upload directory, migrating the legacy directory when
/// possible while preserving it as a lossless fallback on any rename failure.
async fn resolve_uploads_dir(canonical_base: &Path) -> Result<PathBuf, ApiError> {
    let uploads_dir = canonical_base.join(UPLOADS_DIR);
    let legacy_uploads_dir = canonical_base.join(LEGACY_UPLOADS_DIR);

    let uploads_exists = is_directory(&uploads_dir).await?;
    let legacy_exists = is_directory(&legacy_uploads_dir).await?;

    let resolved_dir = match (uploads_exists, legacy_exists) {
        (true, _) => uploads_dir,
        (false, true) => match tokio::fs::rename(&legacy_uploads_dir, &uploads_dir).await {
            Ok(()) => uploads_dir,
            Err(_) => {
                // A concurrent resolver may have won the rename race. Any other
                // rename failure keeps the legacy directory as the safe fallback.
                if tokio::fs::metadata(&uploads_dir)
                    .await
                    .is_ok_and(|metadata| metadata.is_dir())
                {
                    uploads_dir
                } else {
                    legacy_uploads_dir
                }
            }
        },
        (false, false) => {
            tokio::fs::create_dir_all(&uploads_dir).await?;
            uploads_dir
        }
    };

    ensure_uploads_gitignore(&resolved_dir).await?;
    Ok(resolved_dir)
}

pub async fn upload_files(
    Extension(workspace): Extension<Workspace>,
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<UploadQuery>,
    mut multipart: Multipart,
) -> Result<ResponseJson<ApiResponse<Vec<WorkspaceFileEntry>>>, ApiError> {
    let base = workspace_root(&deployment, &workspace).await?;
    let canonical_base = tokio::fs::canonicalize(&base)
        .await
        .map_err(|_| ApiError::File(FileError::NotFound))?;

    let target_dir = match query.path.as_deref() {
        Some(p) if !p.trim().is_empty() => file_policy::resolve_existing_dir(&base, p)?,
        _ => resolve_uploads_dir(&canonical_base).await?,
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
        let final_path = target_dir.join(&name);

        if !query.overwrite && tokio::fs::try_exists(&final_path).await.unwrap_or(false) {
            return Err(ApiError::Conflict(format!("File '{name}' already exists")));
        }

        // Stream to a temp file in the target dir (same filesystem → atomic rename).
        let tmp_path = target_dir.join(format!(".bc-upload-{}.tmp", Uuid::new_v4()));
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
        // already exists, closing the try_exists -> rename race (TOCTOU).
        if query.overwrite {
            if let Err(e) = tokio::fs::rename(&tmp_path, &final_path).await {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(e.into());
            }
        } else {
            match tokio::fs::hard_link(&tmp_path, &final_path).await {
                Ok(()) => {
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    return Err(ApiError::Conflict(format!("File '{name}' already exists")));
                }
                Err(e) => {
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    return Err(e.into());
                }
            }
        }

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
        assert_eq!(
            fs::read_to_string(path.join(".gitignore")).unwrap(),
            GITIGNORE_CONTENTS
        );
    }

    #[tokio::test]
    async fn uploads_dir_resolver_creates_new_dir_when_neither_exists() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = tempdir.path();
        let expected = base.join(UPLOADS_DIR);

        let resolved = resolve_uploads_dir(base).await.unwrap();

        assert_eq!(resolved, expected);
        assert!(expected.is_dir());
        assert!(!base.join(LEGACY_UPLOADS_DIR).exists());
        assert_gitignored(&expected);
    }

    #[tokio::test]
    async fn uploads_dir_resolver_migrates_legacy_only_dir() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = tempdir.path();
        let legacy = seed_upload_dir(base, LEGACY_UPLOADS_DIR);
        fs::create_dir_all(legacy.join("nested")).unwrap();
        fs::write(legacy.join("report.txt"), b"legacy report").unwrap();
        fs::write(legacy.join("nested/data.bin"), b"legacy data").unwrap();
        let expected = base.join(UPLOADS_DIR);

        let resolved = resolve_uploads_dir(base).await.unwrap();

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
    async fn uploads_dir_resolver_returns_new_only_dir_untouched() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = tempdir.path();
        let expected = seed_upload_dir(base, UPLOADS_DIR);
        fs::write(expected.join("new.txt"), b"new data").unwrap();
        fs::remove_file(expected.join(".gitignore")).unwrap();

        let resolved = resolve_uploads_dir(base).await.unwrap();

        assert_eq!(resolved, expected);
        assert!(expected.is_dir());
        assert!(!base.join(LEGACY_UPLOADS_DIR).exists());
        assert_gitignored(&expected);
        assert_eq!(fs::read(expected.join("new.txt")).unwrap(), b"new data");
    }

    #[tokio::test]
    async fn uploads_dir_resolver_prefers_new_dir_when_both_exist() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = tempdir.path();
        let expected = seed_upload_dir(base, UPLOADS_DIR);
        let legacy = seed_upload_dir(base, LEGACY_UPLOADS_DIR);
        fs::write(expected.join("new.txt"), b"new data").unwrap();
        fs::write(legacy.join("legacy.txt"), b"legacy data").unwrap();
        fs::remove_file(expected.join(".gitignore")).unwrap();
        fs::remove_file(legacy.join(".gitignore")).unwrap();

        let resolved = resolve_uploads_dir(base).await.unwrap();

        assert_eq!(resolved, expected);
        assert!(expected.is_dir());
        assert!(legacy.is_dir());
        assert_gitignored(&expected);
        assert!(!legacy.join(".gitignore").exists());
        assert_eq!(fs::read(expected.join("new.txt")).unwrap(), b"new data");
        assert_eq!(fs::read(legacy.join("legacy.txt")).unwrap(), b"legacy data");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_uploads_dir_resolvers_preserve_legacy_files() {
        let tempdir = tempfile::tempdir().unwrap();
        let base = tempdir.path().to_path_buf();
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
        let base = tempdir.path();
        let legacy = seed_upload_dir(base, LEGACY_UPLOADS_DIR);
        fs::write(legacy.join("keep.txt"), b"do not lose me").unwrap();
        fs::remove_file(legacy.join(".gitignore")).unwrap();
        let blocked_new_path = base.join(UPLOADS_DIR);
        fs::write(&blocked_new_path, b"not a directory").unwrap();

        let resolved = resolve_uploads_dir(base).await.unwrap();

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
