use std::{
    io,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use directories::ProjectDirs;
use rust_embed::RustEmbed;

use crate::env::env_path_override;

const PROJECT_ROOT: &str = env!("CARGO_MANIFEST_DIR");
const DATABASE_FILE_NAME: &str = "db.v2.sqlite";
const LEGACY_DATABASE_FILE_NAME: &str = "db.sqlite";

static PROD_ASSET_DIR: OnceLock<PathBuf> = OnceLock::new();
static DATA_DIR_OVERRIDE: OnceLock<Option<PathBuf>> = OnceLock::new();

fn data_dir_env_override() -> Option<&'static Path> {
    DATA_DIR_OVERRIDE
        .get_or_init(|| env_path_override("BC_DATA_DIR"))
        .as_deref()
}

pub fn asset_dir() -> std::path::PathBuf {
    let path = if cfg!(debug_assertions) {
        match data_dir_env_override() {
            Some(override_dir) => cached_prod_asset_dir_path(Some(override_dir.to_path_buf())),
            None => std::path::PathBuf::from(PROJECT_ROOT).join("../../dev_assets"),
        }
    } else {
        prod_asset_dir_path()
    };

    // Ensure the directory exists
    if !path.exists() {
        std::fs::create_dir_all(&path).expect("Failed to create asset directory");
    }

    path
    // ✔ macOS → ~/Library/Application Support/MyApp
    // ✔ Linux → ~/.local/share/myapp   (respects XDG_DATA_HOME)
    // ✔ Windows → %APPDATA%\Example\MyApp
}

/// Returns the production data directory, resolving and caching it once per process.
///
/// `BC_DATA_DIR` is read before platform home discovery and is a hard override in
/// both debug and release builds. An empty value is ignored with a warning. A
/// relative value is made absolute, or retained with a warning if that fails.
/// Without an override, [`asset_dir`] keeps using `dev_assets` in debug builds.
/// Unit tests exercise [`resolve_data_dir`] directly and never mutate process env.
pub fn prod_asset_dir_path() -> PathBuf {
    cached_prod_asset_dir_path(data_dir_env_override().map(Path::to_path_buf))
}

fn cached_prod_asset_dir_path(override_dir: Option<PathBuf>) -> PathBuf {
    PROD_ASSET_DIR
        .get_or_init(|| {
            let resolution = if let Some(override_dir) = override_dir {
                resolve_data_dir(Some(override_dir), PathBuf::new(), PathBuf::new()).map(|path| {
                    DataDirResolution {
                        path,
                        reason: DataDirReason::Override,
                    }
                })
            } else {
                let bettercoding_dir = ProjectDirs::from("ai", "bloop", "bettercoding")
                    .expect("OS didn't give us a home directory")
                    .data_dir()
                    .to_path_buf();
                let legacy_dir = ProjectDirs::from("ai", "bloop", "vibe-kanban")
                    .expect("OS didn't give us a home directory")
                    .data_dir()
                    .to_path_buf();

                resolve_data_dir_with_reason(None, bettercoding_dir, legacy_dir)
            }
            // The data directory is a startup prerequisite. Unlike the deferrable
            // worktree base, guessing here can create an empty DB and destroy state,
            // so an unknowable candidate must fail loudly.
            .unwrap_or_else(|error| {
                panic!("Cannot safely resolve the startup-critical data directory: {error}")
            });

            tracing::info!(
                path = %resolution.path.display(),
                reason = resolution.reason.as_str(),
                "Resolved data directory"
            );
            resolution.path
        })
        .clone()
}

#[derive(Debug, thiserror::Error)]
#[error(
    "failed to inspect data file candidate `{path}`: {source}; refusing to guess the data directory to protect existing data"
)]
struct DataDirResolveError {
    path: PathBuf,
    #[source]
    source: io::Error,
}

enum FileProbe {
    Present,
    Absent,
    Unknown(DataDirResolveError),
}

#[derive(Clone, Copy)]
enum DataDirReason {
    Override,
    Bettercoding,
    LegacyAdopt,
    Fresh,
}

impl DataDirReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Bettercoding => "bettercoding",
            Self::LegacyAdopt => "legacy-adopt",
            Self::Fresh => "fresh",
        }
    }
}

struct DataDirResolution {
    path: PathBuf,
    reason: DataDirReason,
}

fn resolve_data_dir(
    override_dir: Option<PathBuf>,
    bettercoding_dir: PathBuf,
    legacy_dir: PathBuf,
) -> Result<PathBuf, DataDirResolveError> {
    resolve_data_dir_with_reason(override_dir, bettercoding_dir, legacy_dir)
        .map(|resolution| resolution.path)
}

fn resolve_data_dir_with_reason(
    override_dir: Option<PathBuf>,
    bettercoding_dir: PathBuf,
    legacy_dir: PathBuf,
) -> Result<DataDirResolution, DataDirResolveError> {
    if let Some(override_dir) = override_dir {
        return Ok(DataDirResolution {
            path: override_dir,
            reason: DataDirReason::Override,
        });
    }

    match probe_file(&bettercoding_dir.join(DATABASE_FILE_NAME)) {
        FileProbe::Present => {
            return Ok(DataDirResolution {
                path: bettercoding_dir,
                reason: DataDirReason::Bettercoding,
            });
        }
        FileProbe::Unknown(error) => return Err(error),
        FileProbe::Absent => {}
    }

    match probe_legacy_database(&legacy_dir) {
        FileProbe::Present => {
            // TODO(bc-legacy-cleanup): remove when no vibe-kanban installs remain.
            return Ok(DataDirResolution {
                path: legacy_dir,
                reason: DataDirReason::LegacyAdopt,
            });
        }
        FileProbe::Unknown(error) => return Err(error),
        FileProbe::Absent => {}
    }

    Ok(DataDirResolution {
        path: bettercoding_dir,
        reason: DataDirReason::Fresh,
    })
}

fn probe_legacy_database(legacy_dir: &Path) -> FileProbe {
    let mut first_unknown = None;

    for file_name in [DATABASE_FILE_NAME, LEGACY_DATABASE_FILE_NAME] {
        match probe_file(&legacy_dir.join(file_name)) {
            FileProbe::Present => return FileProbe::Present,
            FileProbe::Absent => {}
            FileProbe::Unknown(error) => {
                first_unknown.get_or_insert(error);
            }
        }
    }

    first_unknown.map_or(FileProbe::Absent, FileProbe::Unknown)
}

fn probe_file(path: &Path) -> FileProbe {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => FileProbe::Present,
        Ok(_) => FileProbe::Absent,
        Err(error) if error.kind() == io::ErrorKind::NotFound => FileProbe::Absent,
        Err(source) => FileProbe::Unknown(DataDirResolveError {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub fn config_path() -> std::path::PathBuf {
    asset_dir().join("config.json")
}

pub fn profiles_path() -> std::path::PathBuf {
    asset_dir().join("profiles.json")
}

pub fn credentials_path() -> std::path::PathBuf {
    asset_dir().join("credentials.json")
}

pub fn trusted_keys_path() -> std::path::PathBuf {
    asset_dir().join("trusted_ed25519_public_keys.json")
}

pub fn server_signing_key_path() -> std::path::PathBuf {
    asset_dir().join("server_ed25519_signing_key")
}

pub fn relay_host_credentials_path() -> std::path::PathBuf {
    asset_dir().join("relay_host_credentials.json")
}

#[derive(RustEmbed)]
#[folder = "../../assets/sounds"]
pub struct SoundAssets;

#[derive(RustEmbed)]
#[folder = "../../assets/scripts"]
pub struct ScriptAssets;

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::{fs, path::Path};

    use tempfile::TempDir;

    use super::*;

    fn candidates(root: &TempDir) -> (PathBuf, PathBuf) {
        (
            root.path().join("bettercoding"),
            root.path().join("vibe-kanban"),
        )
    }

    fn seed_database(dir: &Path) {
        seed_database_named(dir, DATABASE_FILE_NAME);
    }

    fn seed_database_named(dir: &Path, file_name: &str) {
        fs::create_dir_all(dir).expect("create scratch data directory");
        fs::write(dir.join(file_name), b"scratch database").expect("seed scratch database");
    }

    #[test]
    fn uses_legacy_dir_when_only_legacy_has_database() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);
        seed_database(&legacy_dir);

        assert_eq!(
            resolve_data_dir(None, bettercoding_dir, legacy_dir.clone())
                .expect("resolve data directory"),
            legacy_dir
        );
    }

    #[test]
    fn uses_legacy_dir_when_it_only_has_pre_v2_database() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);
        seed_database_named(&legacy_dir, LEGACY_DATABASE_FILE_NAME);

        assert_eq!(
            resolve_data_dir(None, bettercoding_dir, legacy_dir.clone())
                .expect("resolve data directory"),
            legacy_dir
        );
    }

    #[test]
    fn ignores_pre_v2_database_in_bettercoding_dir() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);
        seed_database_named(&bettercoding_dir, LEGACY_DATABASE_FILE_NAME);
        seed_database(&legacy_dir);

        assert_eq!(
            resolve_data_dir(None, bettercoding_dir, legacy_dir.clone())
                .expect("resolve data directory"),
            legacy_dir
        );
    }

    #[test]
    fn prefers_bettercoding_dir_when_both_have_database() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);
        seed_database(&bettercoding_dir);
        seed_database(&legacy_dir);

        assert_eq!(
            resolve_data_dir(None, bettercoding_dir.clone(), legacy_dir)
                .expect("resolve data directory"),
            bettercoding_dir
        );
    }

    #[test]
    fn uses_bettercoding_dir_for_fresh_install() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);

        assert_eq!(
            resolve_data_dir(None, bettercoding_dir.clone(), legacy_dir)
                .expect("resolve data directory"),
            bettercoding_dir
        );
    }

    #[test]
    fn override_wins_when_both_have_database() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);
        let override_dir = root.path().join("override");
        seed_database(&bettercoding_dir);
        seed_database(&legacy_dir);

        assert_eq!(
            resolve_data_dir(Some(override_dir.clone()), bettercoding_dir, legacy_dir)
                .expect("resolve data directory"),
            override_dir
        );
    }

    #[test]
    fn ignores_legacy_dir_without_database() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);
        fs::create_dir_all(&legacy_dir).expect("create empty legacy data directory");

        assert_eq!(
            resolve_data_dir(None, bettercoding_dir.clone(), legacy_dir)
                .expect("resolve data directory"),
            bettercoding_dir
        );
    }

    #[cfg(unix)]
    #[test]
    fn errors_when_bettercoding_database_probe_is_unknown() {
        let root = TempDir::new().expect("create scratch directory");
        let blocked_parent = root.path().join("blocked");
        let bettercoding_dir = blocked_parent.join("bettercoding");
        let legacy_dir = root.path().join("vibe-kanban");
        fs::create_dir(&blocked_parent).expect("create blocked parent");
        seed_database(&legacy_dir);
        fs::set_permissions(&blocked_parent, fs::Permissions::from_mode(0o000))
            .expect("block candidate probe");

        let result = resolve_data_dir(None, bettercoding_dir.clone(), legacy_dir);

        fs::set_permissions(&blocked_parent, fs::Permissions::from_mode(0o700))
            .expect("restore blocked parent permissions");
        let error = result.expect_err("unknown preferred candidate must fail resolution");
        assert_eq!(error.path, bettercoding_dir.join(DATABASE_FILE_NAME));
        assert_eq!(error.source.kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn errors_when_legacy_database_probe_is_unknown() {
        let root = TempDir::new().expect("create scratch directory");
        let bettercoding_dir = root.path().join("bettercoding");
        let blocked_parent = root.path().join("blocked");
        let legacy_dir = blocked_parent.join("vibe-kanban");
        fs::create_dir(&blocked_parent).expect("create blocked parent");
        fs::set_permissions(&blocked_parent, fs::Permissions::from_mode(0o000))
            .expect("block candidate probe");

        let result = resolve_data_dir(None, bettercoding_dir, legacy_dir.clone());

        fs::set_permissions(&blocked_parent, fs::Permissions::from_mode(0o700))
            .expect("restore blocked parent permissions");
        let error = result.expect_err("unknown legacy candidate must fail resolution");
        assert_eq!(error.path, legacy_dir.join(DATABASE_FILE_NAME));
        assert_eq!(error.source.kind(), io::ErrorKind::PermissionDenied);
    }
}
