use std::{path::PathBuf, sync::OnceLock};

use directories::ProjectDirs;
use rust_embed::RustEmbed;

const PROJECT_ROOT: &str = env!("CARGO_MANIFEST_DIR");
const DATABASE_FILE_NAME: &str = "db.v2.sqlite";

static PROD_ASSET_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn asset_dir() -> std::path::PathBuf {
    let path = if cfg!(debug_assertions) {
        std::path::PathBuf::from(PROJECT_ROOT).join("../../dev_assets")
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
/// `BC_DATA_DIR` is a hard override intended for test and development use. Unit
/// tests exercise [`resolve_data_dir`] directly and never mutate process env.
pub fn prod_asset_dir_path() -> PathBuf {
    PROD_ASSET_DIR
        .get_or_init(|| {
            let bettercoding_dir = ProjectDirs::from("ai", "bloop", "bettercoding")
                .expect("OS didn't give us a home directory")
                .data_dir()
                .to_path_buf();
            let legacy_dir = ProjectDirs::from("ai", "bloop", "vibe-kanban")
                .expect("OS didn't give us a home directory")
                .data_dir()
                .to_path_buf();

            resolve_data_dir(
                std::env::var_os("BC_DATA_DIR").map(PathBuf::from),
                bettercoding_dir,
                legacy_dir,
            )
        })
        .clone()
}

fn resolve_data_dir(
    override_dir: Option<PathBuf>,
    bettercoding_dir: PathBuf,
    legacy_dir: PathBuf,
) -> PathBuf {
    if let Some(override_dir) = override_dir {
        return override_dir;
    }

    if bettercoding_dir.join(DATABASE_FILE_NAME).is_file() {
        return bettercoding_dir;
    }

    if legacy_dir.join(DATABASE_FILE_NAME).is_file() {
        // TODO(bc-legacy-cleanup): remove when no vibe-kanban installs remain.
        return legacy_dir;
    }

    bettercoding_dir
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
        fs::create_dir_all(dir).expect("create scratch data directory");
        fs::write(dir.join(DATABASE_FILE_NAME), b"scratch database")
            .expect("seed scratch database");
    }

    #[test]
    fn uses_legacy_dir_when_only_legacy_has_database() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);
        seed_database(&legacy_dir);

        assert_eq!(
            resolve_data_dir(None, bettercoding_dir, legacy_dir.clone()),
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
            resolve_data_dir(None, bettercoding_dir.clone(), legacy_dir),
            bettercoding_dir
        );
    }

    #[test]
    fn uses_bettercoding_dir_for_fresh_install() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);

        assert_eq!(
            resolve_data_dir(None, bettercoding_dir.clone(), legacy_dir),
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
            resolve_data_dir(Some(override_dir.clone()), bettercoding_dir, legacy_dir,),
            override_dir
        );
    }

    #[test]
    fn ignores_legacy_dir_without_database() {
        let root = TempDir::new().expect("create scratch directory");
        let (bettercoding_dir, legacy_dir) = candidates(&root);
        fs::create_dir_all(&legacy_dir).expect("create empty legacy data directory");

        assert_eq!(
            resolve_data_dir(None, bettercoding_dir.clone(), legacy_dir),
            bettercoding_dir
        );
    }
}
