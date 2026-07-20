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
    LegacyAdopt,
    Fresh,
}

impl ResolutionReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Bettercoding => "bettercoding",
            Self::LegacyAdopt => "legacy-adopt",
            Self::Fresh => "fresh",
        }
    }
}

#[derive(Debug)]
pub(crate) struct Resolution {
    pub(crate) path: PathBuf,
    pub(crate) reason: ResolutionReason,
}

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

/// Returns the cached, normalised `BC_WORKTREE_BASE` override, if one is valid.
pub fn worktree_base_env_override() -> Option<&'static Path> {
    WORKTREE_BASE_OVERRIDE
        .get_or_init(|| crate::env::env_path_override("BC_WORKTREE_BASE"))
        .as_deref()
}

// TODO(bc-legacy-cleanup): only the public function name remains frozen for callers;
// it is not a persisted on-disk identity.
/// Returns the worktree base directory, resolving and caching it once per process.
///
/// Resolution order is: `BC_WORKTREE_BASE`; a BetterCoding candidate containing
/// a directory; a legacy candidate containing a directory; an unknowable
/// BetterCoding candidate (adopted with a warning); an unknowable legacy
/// candidate (adopted with a warning); then a fresh BetterCoding candidate,
/// created best-effort. Plain files such as the boot port file do not count as
/// state. Without an override, debug builds keep their `-dev` directory names.
/// Unit tests exercise `resolve_worktree_base_dir` directly and never mutate env.
///
/// In-repo downgrade caveat: a fresh install stores state under the BetterCoding
/// home, which pre-dual-home binaries never probe. Downgrading therefore boots an
/// empty database at the legacy path; older binaries also ignore `BC_DATA_DIR`
/// and `BC_WORKTREE_BASE`.
pub fn get_vibe_kanban_temp_dir() -> PathBuf {
    WORKTREE_BASE_DIR
        .get_or_init(|| {
            let override_dir = worktree_base_env_override().map(Path::to_path_buf);
            let resolution = if let Some(override_dir) = override_dir {
                resolve_worktree_base_dir(Some(override_dir), PathBuf::new(), PathBuf::new())
            } else {
                // TODO(bc-legacy-cleanup): remove the legacy names from this tuple.
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
                    None,
                    base_dir.join(bettercoding_dir_name),
                    base_dir.join(legacy_dir_name),
                )
            };

            tracing::info!(
                path = %resolution.path.display(),
                reason = resolution.reason.as_str(),
                "Resolved worktree base directory"
            );
            resolution.path
        })
        .clone()
}

#[derive(Debug)]
enum DirectoryProbe {
    NonEmpty,
    EmptyOrAbsent,
    Unknown(std::io::Error),
}

fn resolve_worktree_base_dir(
    override_dir: Option<PathBuf>,
    bettercoding_candidate: PathBuf,
    legacy_candidate: PathBuf,
) -> Resolution {
    if let Some(override_dir) = override_dir {
        return Resolution {
            path: override_dir,
            reason: ResolutionReason::Override,
        };
    }

    let candidates = [
        (&bettercoding_candidate, ResolutionReason::Bettercoding),
        // TODO(bc-legacy-cleanup): remove when no vibe-kanban installs remain.
        (&legacy_candidate, ResolutionReason::LegacyAdopt),
    ];
    let mut unknowns = Vec::new();

    for (path, reason) in candidates {
        match is_non_empty_dir(path) {
            DirectoryProbe::NonEmpty => {
                return Resolution {
                    path: path.to_path_buf(),
                    reason,
                };
            }
            DirectoryProbe::Unknown(error) => unknowns.push((path, reason, error)),
            DirectoryProbe::EmptyOrAbsent => {}
        }
    }

    // Worktree-base access is deferrable: consumers can surface later I/O errors,
    // so preserve an unknowable candidate with a warning instead of guessing. Known
    // non-empty state above wins over an unknowable candidate in the preferred home.
    if let Some((path, reason, error)) = unknowns.into_iter().next() {
        tracing::warn!(
            path = %path.display(),
            error = %error,
            "Could not inspect worktree base candidate; adopting it to avoid skipping existing state"
        );
        return Resolution {
            path: path.to_path_buf(),
            reason,
        };
    }

    if let Err(error) = std::fs::create_dir_all(&bettercoding_candidate) {
        tracing::warn!(
            path = %bettercoding_candidate.display(),
            error = %error,
            "Failed to create fresh worktree base directory; continuing with the selected path"
        );
    }
    Resolution {
        path: bettercoding_candidate,
        reason: ResolutionReason::Fresh,
    }
}

fn is_non_empty_dir(path: &Path) -> DirectoryProbe {
    match std::fs::read_dir(path) {
        Ok(entries) => {
            // Only directories represent app state here (`worktrees/` and `qa-repos/`).
            // The port file is written beside them on every boot, and other plain junk
            // files must not make an otherwise fresh install adopt the legacy home.
            let mut contains_directory = false;
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => return DirectoryProbe::Unknown(error),
                };
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(error) => return DirectoryProbe::Unknown(error),
                };
                if file_type.is_dir() {
                    contains_directory = true;
                }
            }
            if contains_directory {
                DirectoryProbe::NonEmpty
            } else {
                DirectoryProbe::EmptyOrAbsent
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DirectoryProbe::EmptyOrAbsent,
        Err(error) => DirectoryProbe::Unknown(error),
    }
}

/// Expand leading ~ to user's home directory.
pub fn expand_tilde(path_str: &str) -> std::path::PathBuf {
    shellexpand::tilde(path_str).as_ref().into()
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;

    fn temp_dir_candidates(root: &TempDir) -> (PathBuf, PathBuf) {
        (
            root.path().join("bettercoding"),
            root.path().join("vibe-kanban"),
        )
    }

    fn seed_dir(dir: &Path) {
        fs::create_dir_all(dir.join("worktrees")).expect("seed scratch worktree directory");
    }

    fn assert_resolution(
        resolution: Resolution,
        expected_path: &Path,
        expected_reason: ResolutionReason,
    ) {
        assert_eq!(resolution.path, expected_path);
        assert_eq!(resolution.reason, expected_reason);
    }

    #[cfg(unix)]
    fn running_as_root() -> bool {
        nix::unistd::geteuid().is_root()
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
    fn uses_legacy_dir_when_it_contains_one_subdirectory() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = temp_dir_candidates(&root);
        seed_dir(&legacy_dir);

        assert_resolution(
            resolve_worktree_base_dir(None, bettercoding_dir, legacy_dir.clone()),
            &legacy_dir,
            ResolutionReason::LegacyAdopt,
        );
    }

    #[test]
    fn prefers_bettercoding_dir_when_both_are_non_empty() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = temp_dir_candidates(&root);
        seed_dir(&bettercoding_dir);
        seed_dir(&legacy_dir);

        assert_resolution(
            resolve_worktree_base_dir(None, bettercoding_dir.clone(), legacy_dir),
            &bettercoding_dir,
            ResolutionReason::Bettercoding,
        );
    }

    #[test]
    fn uses_and_creates_bettercoding_dir_when_both_are_absent() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = temp_dir_candidates(&root);

        assert_resolution(
            resolve_worktree_base_dir(None, bettercoding_dir.clone(), legacy_dir),
            &bettercoding_dir,
            ResolutionReason::Fresh,
        );
        assert!(bettercoding_dir.is_dir());
    }

    #[test]
    fn uses_legacy_dir_when_bettercoding_dir_is_empty() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = temp_dir_candidates(&root);
        fs::create_dir_all(&bettercoding_dir).expect("create empty BetterCoding directory");
        seed_dir(&legacy_dir);

        assert_resolution(
            resolve_worktree_base_dir(None, bettercoding_dir, legacy_dir.clone()),
            &legacy_dir,
            ResolutionReason::LegacyAdopt,
        );
    }

    #[test]
    fn ignores_plain_files_in_legacy_candidate() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = temp_dir_candidates(&root);
        fs::create_dir_all(&legacy_dir).expect("create legacy directory");
        fs::write(legacy_dir.join("vibe-kanban.port"), b"3000").expect("seed legacy port file");

        assert_resolution(
            resolve_worktree_base_dir(None, bettercoding_dir.clone(), legacy_dir),
            &bettercoding_dir,
            ResolutionReason::Fresh,
        );
        assert!(bettercoding_dir.is_dir());
    }

    #[test]
    fn override_wins_when_both_are_non_empty() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = temp_dir_candidates(&root);
        let override_dir = root.path().join("override");
        seed_dir(&bettercoding_dir);
        seed_dir(&legacy_dir);

        assert_resolution(
            resolve_worktree_base_dir(Some(override_dir.clone()), bettercoding_dir, legacy_dir),
            &override_dir,
            ResolutionReason::Override,
        );
    }

    #[cfg(unix)]
    #[test]
    fn known_legacy_state_beats_unknown_bettercoding_then_unknown_is_adopted_if_legacy_absent() {
        if running_as_root() {
            return;
        }

        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = temp_dir_candidates(&root);
        fs::create_dir(&bettercoding_dir).expect("create BetterCoding directory");
        seed_dir(&legacy_dir);
        fs::set_permissions(&bettercoding_dir, fs::Permissions::from_mode(0o000))
            .expect("block candidate probe");

        let with_legacy =
            resolve_worktree_base_dir(None, bettercoding_dir.clone(), legacy_dir.clone());
        assert_resolution(with_legacy, &legacy_dir, ResolutionReason::LegacyAdopt);

        fs::remove_dir_all(&legacy_dir).expect("remove legacy state");
        let without_legacy = resolve_worktree_base_dir(None, bettercoding_dir.clone(), legacy_dir);

        fs::set_permissions(&bettercoding_dir, fs::Permissions::from_mode(0o700))
            .expect("restore BetterCoding directory permissions");
        assert_resolution(
            without_legacy,
            &bettercoding_dir,
            ResolutionReason::Bettercoding,
        );
    }

    #[cfg(unix)]
    #[test]
    fn adopts_legacy_candidate_when_probe_is_unknown() {
        if running_as_root() {
            return;
        }

        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = temp_dir_candidates(&root);
        fs::create_dir(&legacy_dir).expect("create legacy directory");
        fs::set_permissions(&legacy_dir, fs::Permissions::from_mode(0o000))
            .expect("block candidate probe");

        let resolution = resolve_worktree_base_dir(None, bettercoding_dir, legacy_dir.clone());

        fs::set_permissions(&legacy_dir, fs::Permissions::from_mode(0o700))
            .expect("restore legacy directory permissions");
        assert_resolution(resolution, &legacy_dir, ResolutionReason::LegacyAdopt);
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
