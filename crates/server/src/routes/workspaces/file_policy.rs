//! Centralized path-safety + denylist + size policy for the workspace Files
//! feature (`/api/workspaces/{id}/files`).
//!
//! Every list/download/zip/upload handler routes filesystem access through these
//! helpers so the rules live in exactly one auditable place. The core guarantee:
//! a caller-supplied relative path can never escape the workspace worktree root,
//! reach git/control directories, or follow a symlink out of bounds.
//!
//! These helpers return `Result<_, ApiError>`; `ApiError` is a large enum, so
//! `clippy::result_large_err` is allowed module-wide (matching `error.rs`'s
//! per-fn allows) rather than boxing it through every call site.
#![allow(clippy::result_large_err)]

use std::path::{Component, Path, PathBuf};

use services::services::file::FileError;
use utils::path::ALWAYS_SKIP_DIRS;

use crate::error::ApiError;

/// Git-ignored directory that uploads land in by default.
pub const UPLOADS_DIR: &str = ".bettercoding-uploads";
/// Legacy default upload directory retained for migrate-on-access compatibility.
///
/// Both directories may intentionally coexist after rollback and re-upgrade;
/// when both are present, neither is merged or deleted. An older binary rolled
/// back after migration hides `.bettercoding-uploads` in its Files panel because
/// its `is_hidden` exempts only `.vibe-uploads`. The files are not lost and become
/// visible again after re-upgrading. Older binaries also ignore `BC_`-prefixed
/// environment configuration, so keep matching `VK_` variables set throughout
/// a rollback window.
// TODO(bc-legacy-cleanup): remove legacy uploads-dir support when no .vibe-uploads dirs remain in the wild.
pub const LEGACY_UPLOADS_DIR: &str = ".vibe-uploads";

// --- Caps (tunable). Concrete limits, never `DefaultBodyLimit::disable()`. ---

/// Max directory entries returned by a single `list` call before truncation.
pub const MAX_LIST_ENTRIES: usize = 2000;
/// Max bytes accepted for a single uploaded file (streaming abort past this).
pub const MAX_UPLOAD_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Max total multipart request body (axum `DefaultBodyLimit`).
pub const MAX_UPLOAD_REQUEST_BYTES: usize = 5 * 1024 * 1024 * 1024;
/// Max number of files accepted in a single upload request.
pub const MAX_UPLOAD_FILES: usize = 50;
/// Max length (bytes) of an uploaded file's basename.
pub const MAX_FILENAME_LEN: usize = 255;
/// Max entries included in a generated zip before the archive fails loudly.
pub const MAX_ZIP_ENTRIES: usize = 10_000;
/// Max total uncompressed bytes in a generated zip before it fails loudly.
pub const MAX_ZIP_UNCOMPRESSED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// True if a path component must never be traversed/listed/written
/// (`.git`, `node_modules`, …). Reuses the shared worktree denylist.
///
/// Matched case-insensitively: on macOS/Windows default (case-insensitive)
/// filesystems `.Git` resolves to `.git`, so an exact match would be bypassable.
pub fn is_denied_component(name: &str) -> bool {
    ALWAYS_SKIP_DIRS
        .iter()
        .any(|denied| denied.eq_ignore_ascii_case(name))
}

/// True for dotfiles/dot-dirs, which are hidden from listings by default. The
/// current and legacy upload drop folders are exempt so users can browse what
/// they uploaded during the migration.
pub fn is_hidden(name: &str) -> bool {
    name.starts_with('.') && name != UPLOADS_DIR && name != LEGACY_UPLOADS_DIR
}

/// Validate a caller-supplied relative path string. Rejects absolute paths,
/// `..`/root/prefix components, non-UTF-8 segments, and denied components
/// (`.git`, `node_modules`). `.`/empty segments are ignored. Returns the cleaned
/// relative `PathBuf` (no filesystem access).
fn validate_relative(rel: &str) -> Result<PathBuf, ApiError> {
    let mut out = PathBuf::new();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(seg) => {
                let s = seg
                    .to_str()
                    .ok_or_else(|| ApiError::BadRequest("Invalid path encoding".to_string()))?;
                if is_denied_component(s) {
                    return Err(ApiError::Forbidden(format!(
                        "Access to '{s}' is not allowed"
                    )));
                }
                out.push(seg);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ApiError::BadRequest(
                    "Path traversal is not allowed".to_string(),
                ));
            }
        }
    }
    Ok(out)
}

/// Resolve `rel` under `base` for a READ (list/download/zip): the target must
/// exist. Canonicalizes both sides and enforces containment. Any canonicalize
/// error is a hard reject (never falls back to the raw join). Returns the
/// canonical absolute path.
///
/// A residual check-then-open symlink TOCTOU remains (a worktree process could
/// swap a component to an escaping symlink after this returns). This is accepted
/// for the local single-user threat model — the user already owns the machine
/// and worktree, so escaping it grants no access they don't already have.
pub fn resolve_existing_path(base: &Path, rel: &str) -> Result<PathBuf, ApiError> {
    let cleaned = validate_relative(rel)?;
    let joined = base.join(cleaned);

    let canonical =
        std::fs::canonicalize(&joined).map_err(|_| ApiError::File(FileError::NotFound))?;
    let canonical_base =
        std::fs::canonicalize(base).map_err(|_| ApiError::File(FileError::NotFound))?;

    if !canonical.starts_with(&canonical_base) {
        return Err(ApiError::File(FileError::NotFound));
    }

    // Re-check the *canonical* (symlink-resolved) components against the
    // denylist: a symlink under the worktree could resolve to `.git`/`node_modules`
    // while staying under canonical_base, bypassing the validate_relative check.
    let canonical_rel = canonical
        .strip_prefix(&canonical_base)
        .map_err(|_| ApiError::File(FileError::NotFound))?;
    for comp in canonical_rel.components() {
        if let Component::Normal(seg) = comp {
            let s = seg
                .to_str()
                .ok_or_else(|| ApiError::BadRequest("Invalid path encoding".to_string()))?;
            if is_denied_component(s) {
                return Err(ApiError::Forbidden(format!(
                    "Access to '{s}' is not allowed"
                )));
            }
        }
    }

    Ok(canonical)
}

/// Resolve `rel` under `base` and require it to be an existing directory.
/// Used to validate an upload target directory.
pub fn resolve_existing_dir(base: &Path, rel: &str) -> Result<PathBuf, ApiError> {
    let path = resolve_existing_path(base, rel)?;
    if !path.is_dir() {
        return Err(ApiError::BadRequest(
            "Upload target is not a directory".to_string(),
        ));
    }
    Ok(path)
}

/// Reduce a multipart-supplied filename to a safe basename: strips any directory
/// components, rejects empty/`..`/separators/NUL, over-long names, and denied
/// names (e.g. a file literally named `.git`).
pub fn safe_basename(raw: &str) -> Result<String, ApiError> {
    let trimmed = raw.trim();
    let base = Path::new(trimmed)
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| ApiError::BadRequest("Invalid file name".to_string()))?;

    if base.is_empty()
        || base == "."
        || base == ".."
        || base.len() > MAX_FILENAME_LEN
        || base.contains('/')
        || base.contains('\\')
        || base.contains('\0')
    {
        return Err(ApiError::BadRequest("Invalid file name".to_string()));
    }
    if is_denied_component(base) {
        return Err(ApiError::Forbidden(format!(
            "A file named '{base}' is not allowed"
        )));
    }
    Ok(base.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn is_bad_request(err: &ApiError) -> bool {
        matches!(err, ApiError::BadRequest(_))
    }
    fn is_forbidden(err: &ApiError) -> bool {
        matches!(err, ApiError::Forbidden(_))
    }
    fn is_not_found(err: &ApiError) -> bool {
        matches!(err, ApiError::File(FileError::NotFound))
    }

    #[test]
    fn upload_dirs_are_not_hidden_during_migration() {
        assert!(!is_hidden(UPLOADS_DIR));
        assert!(!is_hidden(LEGACY_UPLOADS_DIR));
        assert!(is_hidden(".gitignore"));
        assert!(is_hidden(".other-dotfile"));
        assert!(!is_hidden("visible.txt"));
    }

    #[test]
    fn accepts_valid_nested_path() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        fs::create_dir_all(base.join("a/b")).unwrap();
        fs::write(base.join("a/b/c.txt"), b"hi").unwrap();

        let resolved = resolve_existing_path(base, "a/b/c.txt").unwrap();
        assert!(resolved.ends_with("c.txt"));
        // root resolves to the (canonical) base
        let root = resolve_existing_path(base, "").unwrap();
        assert_eq!(root, fs::canonicalize(base).unwrap());
    }

    #[test]
    fn rejects_parent_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_existing_path(dir.path(), "../etc/passwd").unwrap_err();
        assert!(is_bad_request(&err));
    }

    #[test]
    fn rejects_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_existing_path(dir.path(), "/etc/passwd").unwrap_err();
        assert!(is_bad_request(&err));
    }

    #[test]
    fn rejects_git_and_node_modules_components() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        fs::create_dir_all(base.join(".git/hooks")).unwrap();
        fs::write(base.join(".git/hooks/post-checkout"), b"x").unwrap();
        fs::create_dir_all(base.join("node_modules/pkg")).unwrap();

        let g = resolve_existing_path(base, ".git/hooks/post-checkout").unwrap_err();
        assert!(is_forbidden(&g));
        let n = resolve_existing_path(base, "node_modules/pkg").unwrap_err();
        assert!(is_forbidden(&n));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, base.join("link")).unwrap();

        // A symlink that escapes base must be rejected (canonicalize + starts_with).
        let err = resolve_existing_path(&base, "link/secret.txt").unwrap_err();
        assert!(is_not_found(&err));
    }

    #[test]
    fn safe_basename_strips_dirs_and_rejects_traversal() {
        assert_eq!(safe_basename("report.txt").unwrap(), "report.txt");
        assert_eq!(safe_basename("a/b/c.txt").unwrap(), "c.txt");
        assert_eq!(safe_basename("  spaced.bin  ").unwrap(), "spaced.bin");
        // Traversal components are stripped to a safe basename (lands in the
        // target dir, never escapes it), not rejected.
        assert_eq!(safe_basename("../escape").unwrap(), "escape");
        assert_eq!(safe_basename("../../etc/passwd").unwrap(), "passwd");
        // A path that reduces to nothing usable IS rejected.
        assert!(safe_basename("..").is_err());
        assert!(safe_basename("a/b/..").is_err());
        assert!(safe_basename("").is_err());
        assert!(safe_basename("a\\b.txt").is_err());
        assert!(safe_basename(".git").is_err());
        assert!(safe_basename(&"x".repeat(MAX_FILENAME_LEN + 1)).is_err());
    }

    #[test]
    fn resolve_existing_dir_rejects_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), b"x").unwrap();
        assert!(resolve_existing_dir(dir.path(), "f.txt").is_err());
        assert!(resolve_existing_dir(dir.path(), "").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_to_denied_dir() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        fs::create_dir_all(base.join(".git")).unwrap();
        fs::write(base.join(".git/config"), b"x").unwrap();
        // A symlink under the worktree pointing at .git must not become a read
        // path just because its canonical target stays under the base.
        std::os::unix::fs::symlink(base.join(".git"), base.join("link")).unwrap();
        let err = resolve_existing_path(base, "link/config").unwrap_err();
        assert!(is_forbidden(&err));
    }
}
