use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

static WORKTREE_BASE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Directory name for storing attachments in worktrees
// TODO(bc-legacy-cleanup): rename deferred - stored free-text prompts and messages embed
// .vibe-attachments/... links (scratch messages, session history), and the prefix is also
// synthesized in the workspace create flow which is out of scope for this change; renaming
// would break historical references.
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

// TODO(bc-legacy-cleanup): keep the persisted public function name until callers can migrate.
/// Returns the worktree base directory, resolving and caching it once per process.
///
/// `BC_WORKTREE_BASE` is a hard override intended for test and development use.
/// Unit tests exercise [`resolve_worktree_base_dir`] directly and never mutate
/// process env.
pub fn get_vibe_kanban_temp_dir() -> PathBuf {
    WORKTREE_BASE_DIR
        .get_or_init(|| {
            let (bettercoding_dir_name, legacy_dir_name) = if cfg!(debug_assertions) {
                ("bettercoding-dev", "vibe-kanban-dev")
            } else {
                ("bettercoding", "vibe-kanban")
            };

            let base_dir = if cfg!(target_os = "macos") {
                // macOS already uses /var/folders/... which is persistent storage
                std::env::temp_dir()
            } else if cfg!(target_os = "linux") {
                // Linux: use /var/tmp instead of /tmp to avoid RAM usage
                PathBuf::from("/var/tmp")
            } else {
                // Windows and other platforms: use temp dir with a product subdirectory
                std::env::temp_dir()
            };

            resolve_worktree_base_dir(
                std::env::var_os("BC_WORKTREE_BASE").map(PathBuf::from),
                base_dir.join(bettercoding_dir_name),
                base_dir.join(legacy_dir_name),
            )
        })
        .clone()
}

fn resolve_worktree_base_dir(
    override_dir: Option<PathBuf>,
    bettercoding_candidate: PathBuf,
    legacy_candidate: PathBuf,
) -> PathBuf {
    if let Some(override_dir) = override_dir {
        return override_dir;
    }

    if is_non_empty_dir(&bettercoding_candidate) {
        return bettercoding_candidate;
    }

    if is_non_empty_dir(&legacy_candidate) {
        // TODO(bc-legacy-cleanup): remove when no vibe-kanban installs remain.
        return legacy_candidate;
    }

    let _ = std::fs::create_dir_all(&bettercoding_candidate);
    bettercoding_candidate
}

fn is_non_empty_dir(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_some())
}

/// Expand leading ~ to user's home directory.
pub fn expand_tilde(path_str: &str) -> std::path::PathBuf {
    shellexpand::tilde(path_str).as_ref().into()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn temp_dir_candidates(root: &TempDir) -> (PathBuf, PathBuf) {
        (
            root.path().join("bettercoding"),
            root.path().join("vibe-kanban"),
        )
    }

    fn seed_dir(dir: &Path) {
        fs::create_dir_all(dir).expect("create scratch worktree directory");
        fs::write(dir.join("entry"), b"scratch worktree").expect("seed scratch worktree");
    }

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

    #[test]
    fn uses_legacy_dir_when_only_legacy_is_non_empty() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = temp_dir_candidates(&root);
        seed_dir(&legacy_dir);

        assert_eq!(
            resolve_worktree_base_dir(None, bettercoding_dir, legacy_dir.clone()),
            legacy_dir
        );
    }

    #[test]
    fn prefers_bettercoding_dir_when_both_are_non_empty() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = temp_dir_candidates(&root);
        seed_dir(&bettercoding_dir);
        seed_dir(&legacy_dir);

        assert_eq!(
            resolve_worktree_base_dir(None, bettercoding_dir.clone(), legacy_dir),
            bettercoding_dir
        );
    }

    #[test]
    fn uses_and_creates_bettercoding_dir_when_both_are_absent() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = temp_dir_candidates(&root);

        assert_eq!(
            resolve_worktree_base_dir(None, bettercoding_dir.clone(), legacy_dir),
            bettercoding_dir
        );
        assert!(bettercoding_dir.is_dir());
    }

    #[test]
    fn uses_legacy_dir_when_bettercoding_dir_is_empty() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = temp_dir_candidates(&root);
        fs::create_dir_all(&bettercoding_dir).expect("create empty BetterCoding directory");
        seed_dir(&legacy_dir);

        assert_eq!(
            resolve_worktree_base_dir(None, bettercoding_dir, legacy_dir.clone()),
            legacy_dir
        );
    }

    #[test]
    fn override_wins_when_both_are_non_empty() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = temp_dir_candidates(&root);
        let override_dir = root.path().join("override");
        seed_dir(&bettercoding_dir);
        seed_dir(&legacy_dir);

        assert_eq!(
            resolve_worktree_base_dir(Some(override_dir.clone()), bettercoding_dir, legacy_dir,),
            override_dir
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_make_path_relative_macos_private_alias() {
        // Simulate a worktree under /var with a path reported under /private/var
        let worktree = "/var/folders/zz/abc123/T/vibe-kanban-dev/worktrees/vk-test";
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
