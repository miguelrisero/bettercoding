//! Workspace-owned short temporary directories. The root is fixed in production.
use std::{
    io,
    path::{Path, PathBuf},
};

use uuid::Uuid;

pub fn path(workspace_id: Uuid) -> PathBuf {
    path_in(Path::new("/tmp"), workspace_id)
}

fn path_in(root: &Path, workspace_id: Uuid) -> PathBuf {
    root.join(format!("bc-{}", workspace_id.simple()))
}

pub fn ensure(workspace_id: Uuid) -> io::Result<PathBuf> {
    ensure_in(Path::new("/tmp"), workspace_id)
}

pub fn validate(workspace_id: Uuid) -> io::Result<PathBuf> {
    validate_in(Path::new("/tmp"), workspace_id)
}

pub fn remove(workspace_id: Uuid) -> io::Result<()> {
    remove_in(Path::new("/tmp"), workspace_id)
}

#[cfg(unix)]
fn open_private(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    use nix::libc;

    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    validate_metadata(&directory.metadata()?, nix::unistd::geteuid().as_raw())?;
    Ok(directory)
}

#[cfg(unix)]
fn validate_metadata(metadata: &std::fs::Metadata, owner: u32) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o7777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "short temporary directory must be owned by the current user with mode 0700",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_in(root: &Path, workspace_id: Uuid) -> io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    let path = path_in(root, workspace_id);
    match std::fs::DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    validate_in(root, workspace_id)
}

/// Validate under an explicit root so callers can use an isolated fixture.
#[cfg(unix)]
pub fn validate_in(root: &Path, workspace_id: Uuid) -> io::Result<PathBuf> {
    let path = path_in(root, workspace_id);
    open_private(&path)?;
    Ok(path)
}

#[cfg(unix)]
fn remove_in(root: &Path, workspace_id: Uuid) -> io::Result<()> {
    use std::os::{fd::AsRawFd, unix::fs::MetadataExt};

    let path = path_in(root, workspace_id);
    let directory = match open_private(&path) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    remove_contents(directory.as_raw_fd(), directory.metadata()?.dev())?;
    let current = std::fs::symlink_metadata(&path)?;
    let opened = directory.metadata()?;
    if current.dev() != opened.dev() || current.ino() != opened.ino() {
        return Err(io::Error::other(
            "short temporary directory changed during cleanup",
        ));
    }
    std::fs::remove_dir(path)
}

#[cfg(unix)]
fn remove_contents(fd: std::os::fd::RawFd, device: u64) -> io::Result<()> {
    use std::{
        ffi::CStr,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::MetadataExt,
        },
    };

    use nix::libc;

    // fdopendir owns its duplicate. Relative operations pin traversal to the
    // opened directories; symlink replacements cannot redirect cleanup.
    let duplicate = unsafe { libc::dup(fd) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    let entries = unsafe { libc::fdopendir(duplicate) };
    if entries.is_null() {
        let error = io::Error::last_os_error();
        unsafe { libc::close(duplicate) };
        return Err(error);
    }
    let result = (|| {
        loop {
            nix::errno::Errno::clear();
            let entry = unsafe { libc::readdir(entries) };
            if entry.is_null() {
                let error = nix::errno::Errno::last_raw();
                return if error == 0 {
                    Ok(())
                } else {
                    Err(io::Error::from_raw_os_error(error))
                };
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            let child = unsafe {
                libc::openat(
                    fd,
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            let flags = if child >= 0 {
                let child = unsafe { std::fs::File::from_raw_fd(child) };
                if child.metadata()?.dev() != device {
                    return Err(io::Error::other("refusing to cross a mount during cleanup"));
                }
                remove_contents(child.as_raw_fd(), device)?;
                libc::AT_REMOVEDIR
            } else {
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(libc::ENOTDIR | libc::ELOOP) => 0,
                    Some(libc::ENOENT) => continue,
                    _ => return Err(error),
                }
            };
            if unsafe { libc::unlinkat(fd, name.as_ptr(), flags) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    })();
    unsafe { libc::closedir(entries) };
    result
}

#[cfg(not(unix))]
fn ensure_in(_root: &Path, _workspace_id: Uuid) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private short temporary directories require Unix",
    ))
}

#[cfg(not(unix))]
pub fn validate_in(root: &Path, workspace_id: Uuid) -> io::Result<PathBuf> {
    ensure_in(root, workspace_id)
}

#[cfg(not(unix))]
fn remove_in(_root: &Path, _workspace_id: Uuid) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn stable_short_path_uses_the_full_workspace_uuid() {
        let id = Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap();
        assert_eq!(
            path(id),
            PathBuf::from("/tmp/bc-0123456789abcdef0123456789abcdef")
        );
        assert_eq!(path(id).as_os_str().len(), 40);
    }

    #[test]
    fn private_directory_reuse_preserves_contents_and_cleanup_is_bounded() {
        let root = TempDir::new().unwrap();
        let id = Uuid::new_v4();
        let owned = ensure_in(root.path(), id).unwrap();
        assert_eq!(std::fs::metadata(&owned).unwrap().mode() & 0o7777, 0o700);
        std::fs::create_dir(owned.join("nested")).unwrap();
        std::fs::write(owned.join("nested/active"), "keep").unwrap();
        assert_eq!(ensure_in(root.path(), id).unwrap(), owned);
        assert_eq!(
            std::fs::read_to_string(owned.join("nested/active")).unwrap(),
            "keep"
        );
        let other = ensure_in(root.path(), Uuid::new_v4()).unwrap();
        remove_in(root.path(), id).unwrap();
        remove_in(root.path(), id).unwrap();
        assert!(!owned.exists());
        assert!(other.is_dir());
    }

    #[test]
    fn discovery_requires_an_existing_private_directory() {
        let root = TempDir::new().unwrap();
        let id = Uuid::new_v4();
        assert!(validate_in(root.path(), id).is_err());
        assert!(!path_in(root.path(), id).exists());
        let owned = ensure_in(root.path(), id).unwrap();
        assert_eq!(validate_in(root.path(), id).unwrap(), owned);
        for mode in [0o755, 0o770, 0o1700] {
            std::fs::set_permissions(&owned, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(validate_in(root.path(), id).is_err());
            assert!(ensure_in(root.path(), id).is_err());
            assert!(remove_in(root.path(), id).is_err());
            assert_eq!(std::fs::metadata(&owned).unwrap().mode() & 0o7777, mode);
        }
    }

    #[test]
    fn ownership_validation_refuses_a_different_owner() {
        let root = TempDir::new().unwrap();
        let id = Uuid::new_v4();
        let directory = ensure_in(root.path(), id).unwrap();
        let metadata = std::fs::metadata(directory).unwrap();
        assert!(validate_metadata(&metadata, metadata.uid().wrapping_add(1)).is_err());
        assert!(validate_metadata(&metadata, metadata.uid()).is_ok());
    }

    #[test]
    fn collisions_and_external_symlink_targets_remain_untouched() {
        let root = TempDir::new().unwrap();
        let id = Uuid::new_v4();
        let owned = path_in(root.path(), id);
        let external = root.path().join("external");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("keep"), "protected").unwrap();
        symlink(&external, &owned).unwrap();
        assert!(ensure_in(root.path(), id).is_err());
        assert!(validate_in(root.path(), id).is_err());
        assert!(remove_in(root.path(), id).is_err());
        assert_eq!(
            std::fs::read_to_string(external.join("keep")).unwrap(),
            "protected"
        );
        assert!(std::fs::symlink_metadata(&owned).unwrap().is_symlink());

        let file_id = Uuid::new_v4();
        let file = path_in(root.path(), file_id);
        std::fs::write(&file, "protected").unwrap();
        assert!(ensure_in(root.path(), file_id).is_err());
        assert!(remove_in(root.path(), file_id).is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "protected");

        let directory_id = Uuid::new_v4();
        let directory = ensure_in(root.path(), directory_id).unwrap();
        symlink(&external, directory.join("link")).unwrap();
        remove_in(root.path(), directory_id).unwrap();
        assert_eq!(
            std::fs::read_to_string(external.join("keep")).unwrap(),
            "protected"
        );
    }
}
