use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

static WORKTREE_BASE_DIR: OnceLock<PathBuf> = OnceLock::new();
static WORKTREE_BASE_OVERRIDE: OnceLock<Option<PathBuf>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResolutionReason {
    Override,
    Bettercoding,
    Migrated,
    Fresh,
}

impl ResolutionReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Bettercoding => "bettercoding",
            Self::Migrated => "migrated",
            Self::Fresh => "fresh",
        }
    }
}

#[derive(Debug)]
pub(crate) struct Resolution {
    pub(crate) path: PathBuf,
    pub(crate) reason: ResolutionReason,
}

/// Directory name for storing attachments in worktrees. Stored prompts and
/// messages (scratch messages, session history) embed `.vibe-attachments/...`
/// links, so the name stays as it is.
pub const VIBE_ATTACHMENTS_DIR: &str = ".vibe-attachments";

/// Directories that should always be skipped regardless of gitignore.
/// .git is not in .gitignore but should never be watched.
pub const ALWAYS_SKIP_DIRS: &[&str] = &[".git", "node_modules"];

/// Convert absolute paths to relative paths based on worktree path
/// This is a robust implementation that handles symlinks and edge cases
pub fn make_path_relative(path: &str, worktree_path: &str) -> String {
    tracing::trace!("Making path relative: {} -> {}", path, worktree_path);

    let path_obj = normalize_macos_private_alias(Path::new(&path));
    let worktree_path_obj = normalize_macos_private_alias(Path::new(worktree_path));

    // If path is already relative, return as is
    if path_obj.is_relative() {
        return path.to_string();
    }

    if let Ok(relative_path) = path_obj.strip_prefix(&worktree_path_obj) {
        let result = relative_path.to_string_lossy().to_string();
        tracing::trace!("Successfully made relative: '{}' -> '{}'", path, result);
        if result.is_empty() {
            return ".".to_string();
        }
        return result;
    }

    if !path_obj.exists() || !worktree_path_obj.exists() {
        return path.to_string();
    }

    // canonicalize may fail if paths don't exist
    let canonical_path = std::fs::canonicalize(&path_obj);
    let canonical_worktree = std::fs::canonicalize(&worktree_path_obj);

    match (canonical_path, canonical_worktree) {
        (Ok(canon_path), Ok(canon_worktree)) => {
            tracing::debug!(
                "Trying canonical path resolution: '{}' -> '{}', '{}' -> '{}'",
                path,
                canon_path.display(),
                worktree_path,
                canon_worktree.display()
            );

            match canon_path.strip_prefix(&canon_worktree) {
                Ok(relative_path) => {
                    let result = relative_path.to_string_lossy().to_string();
                    tracing::debug!(
                        "Successfully made relative with canonical paths: '{}' -> '{}'",
                        path,
                        result
                    );
                    if result.is_empty() {
                        return ".".to_string();
                    }
                    result
                }
                Err(e) => {
                    tracing::debug!(
                        "Failed to make canonical path relative: '{}' relative to '{}', error: {}, returning original",
                        canon_path.display(),
                        canon_worktree.display(),
                        e
                    );
                    path.to_string()
                }
            }
        }
        _ => {
            tracing::debug!(
                "Could not canonicalize paths (paths may not exist): '{}', '{}', returning original",
                path,
                worktree_path
            );
            path.to_string()
        }
    }
}

/// Normalize macOS prefix /private/var/ and /private/tmp/ to their public aliases without resolving paths.
/// This allows prefix normalization to work when the full paths don't exist.
pub fn normalize_macos_private_alias<P: AsRef<Path>>(p: P) -> PathBuf {
    let p = p.as_ref();
    if cfg!(target_os = "macos")
        && let Some(s) = p.to_str()
    {
        if s == "/private/var" {
            return PathBuf::from("/var");
        }
        if let Some(rest) = s.strip_prefix("/private/var/") {
            return PathBuf::from(format!("/var/{rest}"));
        }
        if s == "/private/tmp" {
            return PathBuf::from("/tmp");
        }
        if let Some(rest) = s.strip_prefix("/private/tmp/") {
            return PathBuf::from(format!("/tmp/{rest}"));
        }
    }
    p.to_path_buf()
}

/// Returns the cached, normalised `BC_WORKTREE_BASE` override, if one is valid.
pub fn worktree_base_env_override() -> Option<&'static Path> {
    WORKTREE_BASE_OVERRIDE
        .get_or_init(|| crate::env::env_path_override("BC_WORKTREE_BASE"))
        .as_deref()
}

/// Returns the app temp directory (worktrees and QA repos), resolving and
/// caching it once per process.
///
/// `BC_WORKTREE_BASE` wins and is used as-given. Otherwise it is
/// `bettercoding` (`bettercoding-dev` in debug builds) under `/var/tmp` on
/// Linux, to keep worktrees out of RAM-backed `/tmp`, and under the OS temp
/// dir elsewhere (persistent `/var/folders/...` on macOS); it is created
/// best-effort. Workspaces created under an earlier root keep their absolute
/// paths, so nothing here needs to find them.
pub fn get_bettercoding_temp_dir() -> PathBuf {
    WORKTREE_BASE_DIR
        .get_or_init(|| {
            if let Some(override_dir) = worktree_base_env_override() {
                return override_dir.to_path_buf();
            }
            let name = if cfg!(debug_assertions) {
                "bettercoding-dev"
            } else {
                "bettercoding"
            };
            let base_dir = if cfg!(target_os = "linux") {
                PathBuf::from("/var/tmp")
            } else {
                std::env::temp_dir()
            };
            let dir = base_dir.join(name);
            if let Err(error) = std::fs::create_dir_all(&dir) {
                tracing::warn!(
                    path = %dir.display(),
                    error = %error,
                    "Failed to create the app temp directory; continuing with it"
                );
            }
            dir
        })
        .clone()
}

/// Expand leading ~ to user's home directory.
pub fn expand_tilde(path_str: &str) -> std::path::PathBuf {
    shellexpand::tilde(path_str).as_ref().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_make_path_relative() {
        // Test with relative path (should remain unchanged)
        assert_eq!(
            make_path_relative("src/main.rs", "/tmp/test-worktree"),
            "src/main.rs"
        );

        // Test with absolute path (should become relative if possible)
        let test_worktree = "/tmp/test-worktree";
        let absolute_path = format!("{test_worktree}/src/main.rs");
        let result = make_path_relative(&absolute_path, test_worktree);
        assert_eq!(result, "src/main.rs");

        // Test with path outside worktree (should return original)
        assert_eq!(
            make_path_relative("/other/path/file.js", "/tmp/test-worktree"),
            "/other/path/file.js"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_make_path_relative_macos_private_alias() {
        // Simulate a worktree under /var with a path reported under /private/var
        let worktree = "/var/folders/zz/abc123/T/bettercoding-dev/worktrees/bc-test";
        let path_under_private = format!(
            "/private/var{}/hello-world.txt",
            worktree.strip_prefix("/var").unwrap()
        );
        assert_eq!(
            make_path_relative(&path_under_private, worktree),
            "hello-world.txt"
        );

        // Also handle the inverse: worktree under /private and path under /var
        let worktree_private = format!("/private{worktree}");
        let path_under_var = format!("{worktree}/hello-world.txt");
        assert_eq!(
            make_path_relative(&path_under_var, &worktree_private),
            "hello-world.txt"
        );
    }
}
