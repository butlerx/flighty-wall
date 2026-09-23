//! An exclusive advisory lock so two mutating processes never share one state database.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Why the host lock could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another flighty-wall process holds {0}")]
    Busy(PathBuf),
    #[error("cannot open lock file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// An exclusive advisory lock, released on drop.
#[derive(Debug)]
pub struct HostLock {
    _file: fs::File,
    path: PathBuf,
}

impl HostLock {
    /// Take the lock now or fail with [`LockError::Busy`]; never wait.
    ///
    /// # Errors
    ///
    /// [`LockError`] when another process holds it or the file cannot be created.
    pub fn acquire(path: &Path) -> Result<Self, LockError> {
        let io = |source: std::io::Error| LockError::Io {
            path: path.to_owned(),
            source,
        };
        if let Some(parent) = path.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(io)?;
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(io)?;
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Self {
                _file: file,
                path: path.to_owned(),
            }),
            Err(rustix::io::Errno::WOULDBLOCK) => Err(LockError::Busy(path.to_owned())),
            Err(errno) => Err(io(errno.into())),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The lock sits beside the state database, in the same private directory.
#[must_use]
pub fn lock_path_for(state_path: &Path) -> PathBuf {
    let name = state_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    state_path.with_file_name(format!("{name}.lock"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn host_lock_is_exclusive_and_released() {
        let dir = TempDir::new().unwrap();
        let lock = lock_path_for(&dir.path().join("state").join("state.sqlite3"));

        let held = HostLock::acquire(&lock).unwrap();
        assert!(matches!(HostLock::acquire(&lock), Err(LockError::Busy(_))));
        drop(held);

        HostLock::acquire(&lock).expect("released cleanly");
    }

    #[test]
    fn lock_file_is_private() {
        let dir = TempDir::new().unwrap();
        let lock = lock_path_for(&dir.path().join("state").join("state.sqlite3"));
        let _held = HostLock::acquire(&lock).unwrap();

        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&lock), 0o600);
        assert_eq!(mode(lock.parent().unwrap()), 0o700);
        assert_eq!(lock.file_name().unwrap(), "state.sqlite3.lock");
    }
}
