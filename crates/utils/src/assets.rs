use std::{
    io,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use directories::ProjectDirs;
use rust_embed::RustEmbed;

use crate::{
    env::env_path_override,
    path::{Resolution, ResolutionReason},
};

const PROJECT_ROOT: &str = env!("CARGO_MANIFEST_DIR");
pub const DB_FILE_NAME: &str = "db.v2.sqlite";
/// Pre-v2 database name; its presence alone still marks a data home.
pub const LEGACY_DB_FILE_NAME: &str = "db.sqlite";

static PROD_ASSET_DIR: OnceLock<Result<PathBuf, String>> = OnceLock::new();
static DATA_DIR_OVERRIDE: OnceLock<Option<PathBuf>> = OnceLock::new();

fn data_dir_env_override() -> Option<&'static Path> {
    DATA_DIR_OVERRIDE
        .get_or_init(|| env_path_override("BC_DATA_DIR"))
        .as_deref()
}

pub fn asset_dir() -> std::path::PathBuf {
    let path = if cfg!(debug_assertions) {
        match data_dir_env_override() {
            Some(_) => prod_asset_dir_path(),
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
}

/// Returns the production data directory, resolving and caching it once per process.
///
/// Resolution order is: `BC_DATA_DIR`; a BetterCoding home containing
/// `db.v2.sqlite`; then a fresh BetterCoding home. A pre-rename home (the
/// `vibe-kanban` data dir) that holds a database is moved into the BetterCoding
/// home once, before anything opens it; see [`migrate_pre_rename_home`]. An
/// unknowable database probe or a failed move returns an error instead of
/// guessing. Without an override, [`asset_dir`] keeps using `dev_assets` in
/// debug builds. Unit tests exercise `resolve_data_dir` directly and never
/// mutate process env.
/// Startup-critical consumers deliberately panic if resolution is unknowable;
/// best-effort consumers should use [`try_prod_asset_dir_path`] instead.
pub fn prod_asset_dir_path() -> PathBuf {
    try_prod_asset_dir_path().unwrap_or_else(|error| {
        panic!("Cannot safely resolve the startup-critical data directory: {error}")
    })
}

/// Tries to return the cached production data directory without panicking.
///
/// Both successful resolution and failure are cached so best-effort consumers
/// do not repeat an unknowable filesystem probe.
pub fn try_prod_asset_dir_path() -> Result<PathBuf, String> {
    cached_prod_asset_dir_path(data_dir_env_override().map(Path::to_path_buf))
}

fn cached_prod_asset_dir_path(override_dir: Option<PathBuf>) -> Result<PathBuf, String> {
    PROD_ASSET_DIR
        .get_or_init(|| {
            // Ambiguity is returned and cached here. Panicking is the policy of the
            // startup-critical wrapper; `try_prod_asset_dir_path` lets best-effort
            // consumers skip work that depends on the production directory.
            let resolution = if let Some(override_dir) = override_dir {
                resolve_data_dir(Some(override_dir), PathBuf::new(), PathBuf::new())
                    .map_err(|error| error.to_string())
            } else {
                let bettercoding_dir = ProjectDirs::from("ai", "bloop", "bettercoding")
                    .ok_or_else(|| "OS didn't give us a home directory".to_string())?
                    .data_dir()
                    .to_path_buf();
                let pre_rename_dir = ProjectDirs::from("ai", "bloop", "vibe-kanban")
                    .ok_or_else(|| "OS didn't give us a home directory".to_string())?
                    .data_dir()
                    .to_path_buf();

                resolve_data_dir(None, bettercoding_dir, pre_rename_dir)
                    .map_err(|error| error.to_string())
            }?;

            tracing::info!(
                path = %resolution.path.display(),
                reason = resolution.reason.as_str(),
                "Resolved data directory"
            );
            Ok(resolution.path)
        })
        .clone()
}

#[derive(Debug, thiserror::Error)]
#[error(
    "failed to resolve the data directory at `{path}`: {source}; refusing to guess the data directory to protect existing data"
)]
struct DataDirResolveError {
    path: PathBuf,
    #[source]
    source: io::Error,
}

#[derive(Debug)]
enum FileProbe {
    Present,
    Absent,
    Unknown(DataDirResolveError),
}

fn resolve_data_dir(
    override_dir: Option<PathBuf>,
    bettercoding_dir: PathBuf,
    pre_rename_dir: PathBuf,
) -> Result<Resolution, DataDirResolveError> {
    if let Some(override_dir) = override_dir {
        return Ok(Resolution {
            path: override_dir,
            reason: ResolutionReason::Override,
        });
    }

    match probe_file(&bettercoding_dir.join(DB_FILE_NAME)) {
        FileProbe::Present => {
            return Ok(Resolution {
                path: bettercoding_dir,
                reason: ResolutionReason::Bettercoding,
            });
        }
        FileProbe::Unknown(error) => return Err(error),
        FileProbe::Absent => {}
    }

    match probe_home_database(&pre_rename_dir) {
        FileProbe::Present => {
            migrate_pre_rename_home(&pre_rename_dir, &bettercoding_dir)?;
            return Ok(Resolution {
                path: bettercoding_dir,
                reason: ResolutionReason::Migrated,
            });
        }
        FileProbe::Unknown(error) => return Err(error),
        FileProbe::Absent => {}
    }

    Ok(Resolution {
        path: bettercoding_dir,
        reason: ResolutionReason::Fresh,
    })
}

/// Probes the v2 database first. An unknown v2 result short-circuits instead of
/// probing the pre-v2 name, matching the parent resolver's refuse-to-guess policy.
fn probe_home_database(dir: &Path) -> FileProbe {
    match probe_file(&dir.join(DB_FILE_NAME)) {
        FileProbe::Absent => probe_file(&dir.join(LEGACY_DB_FILE_NAME)),
        result => result,
    }
}

/// Move the pre-rename data home into the BetterCoding home with one atomic
/// `rename`, so the database, its WAL and every sidecar move together.
///
/// It runs at most once: afterwards the BetterCoding home holds the database
/// and wins resolution. A BetterCoding home that already exists without a
/// database must be empty, or the move fails and nothing is touched. On Unix
/// the old path is left as a symlink to the new home, so absolute paths held
/// outside the app (running agents' hook settings files, a supervisor's log
/// path) keep resolving.
fn migrate_pre_rename_home(from: &Path, to: &Path) -> Result<(), DataDirResolveError> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|source| DataDirResolveError {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::rename(from, to).map_err(|source| DataDirResolveError {
        path: to.to_path_buf(),
        source,
    })?;
    tracing::info!(
        from = %from.display(),
        to = %to.display(),
        "Moved the pre-rename data directory into the BetterCoding data directory"
    );
    #[cfg(unix)]
    if let Err(error) = std::os::unix::fs::symlink(to, from) {
        tracing::warn!(
            path = %from.display(),
            error = %error,
            "Could not leave a symlink at the pre-rename data directory"
        );
    }
    Ok(())
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
        seed_database_named(dir, DB_FILE_NAME);
    }

    fn seed_database_named(dir: &Path, file_name: &str) {
        fs::create_dir_all(dir).expect("create scratch data directory");
        fs::write(dir.join(file_name), b"scratch database").expect("seed scratch database");
    }

    fn assert_resolution(
        result: Result<Resolution, DataDirResolveError>,
        expected_path: &Path,
        expected_reason: ResolutionReason,
    ) {
        let resolution = result.expect("resolve data directory");
        assert_eq!(resolution.path, expected_path);
        assert_eq!(resolution.reason, expected_reason);
    }

    #[cfg(unix)]
    fn running_as_root() -> bool {
        nix::unistd::geteuid().is_root()
    }

    #[test]
    fn moves_pre_rename_home_with_database_into_bettercoding_dir() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);
        seed_database(&legacy_dir);
        fs::create_dir_all(legacy_dir.join("cli-hooks")).expect("seed sidecar directory");
        fs::write(legacy_dir.join("db.v2.sqlite-wal"), b"wal").expect("seed WAL");

        assert_resolution(
            resolve_data_dir(None, bettercoding_dir.clone(), legacy_dir.clone()),
            &bettercoding_dir,
            ResolutionReason::Migrated,
        );
        assert_eq!(
            fs::read(bettercoding_dir.join(DB_FILE_NAME)).unwrap(),
            b"scratch database"
        );
        assert!(bettercoding_dir.join("db.v2.sqlite-wal").is_file());
        assert!(bettercoding_dir.join("cli-hooks").is_dir());

        // The old path still reaches the same files, and the next start
        // resolves to the BetterCoding home without moving anything.
        #[cfg(unix)]
        assert!(
            fs::symlink_metadata(&legacy_dir)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        #[cfg(unix)]
        assert!(legacy_dir.join(DB_FILE_NAME).is_file());
        assert_resolution(
            resolve_data_dir(None, bettercoding_dir.clone(), legacy_dir),
            &bettercoding_dir,
            ResolutionReason::Bettercoding,
        );
    }

    #[test]
    fn moves_pre_rename_home_into_an_empty_bettercoding_dir() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);
        fs::create_dir_all(&bettercoding_dir).expect("create empty BetterCoding directory");
        seed_database_named(&legacy_dir, LEGACY_DB_FILE_NAME);

        assert_resolution(
            resolve_data_dir(None, bettercoding_dir.clone(), legacy_dir),
            &bettercoding_dir,
            ResolutionReason::Migrated,
        );
        assert!(bettercoding_dir.join(LEGACY_DB_FILE_NAME).is_file());
    }

    #[test]
    fn refuses_to_move_into_a_non_empty_bettercoding_dir() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);
        seed_database_named(&bettercoding_dir, LEGACY_DB_FILE_NAME);
        seed_database(&legacy_dir);

        let error = resolve_data_dir(None, bettercoding_dir.clone(), legacy_dir.clone())
            .expect_err("a non-empty target must not be overwritten");
        assert_eq!(error.path, bettercoding_dir);
        assert!(legacy_dir.join(DB_FILE_NAME).is_file());
        assert!(bettercoding_dir.join(LEGACY_DB_FILE_NAME).is_file());
    }

    #[test]
    fn prefers_bettercoding_dir_when_both_have_database() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);
        seed_database(&bettercoding_dir);
        seed_database(&legacy_dir);

        assert_resolution(
            resolve_data_dir(None, bettercoding_dir.clone(), legacy_dir),
            &bettercoding_dir,
            ResolutionReason::Bettercoding,
        );
    }

    #[test]
    fn uses_bettercoding_dir_for_fresh_install() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);

        assert_resolution(
            resolve_data_dir(None, bettercoding_dir.clone(), legacy_dir),
            &bettercoding_dir,
            ResolutionReason::Fresh,
        );
    }

    #[test]
    fn override_wins_when_both_have_database() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);
        let override_dir = root.path().join("override");
        seed_database(&bettercoding_dir);
        seed_database(&legacy_dir);

        assert_resolution(
            resolve_data_dir(Some(override_dir.clone()), bettercoding_dir, legacy_dir),
            &override_dir,
            ResolutionReason::Override,
        );
    }

    #[test]
    fn ignores_legacy_dir_without_database() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);
        fs::create_dir_all(&legacy_dir).expect("create empty legacy data directory");

        assert_resolution(
            resolve_data_dir(None, bettercoding_dir.clone(), legacy_dir),
            &bettercoding_dir,
            ResolutionReason::Fresh,
        );
    }

    #[test]
    fn wrong_type_database_candidate_is_treated_as_absent() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);
        fs::create_dir_all(bettercoding_dir.join(DB_FILE_NAME))
            .expect("create wrong-type database candidate");

        assert_resolution(
            resolve_data_dir(None, bettercoding_dir.clone(), legacy_dir),
            &bettercoding_dir,
            ResolutionReason::Fresh,
        );
    }

    #[cfg(unix)]
    #[test]
    fn errors_when_bettercoding_database_probe_is_unknown() {
        if running_as_root() {
            return;
        }

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
        assert_eq!(error.path, bettercoding_dir.join(DB_FILE_NAME));
        assert_eq!(error.source.kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn errors_when_legacy_database_probe_is_unknown() {
        if running_as_root() {
            return;
        }

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
        assert_eq!(error.path, legacy_dir.join(DB_FILE_NAME));
        assert_eq!(error.source.kind(), io::ErrorKind::PermissionDenied);
    }
}
