//! One iprune at a time with the iPod. A sync, a repair, a restore and an
//! eject each hold an advisory lock on `ipod.lock` in the data folder, so
//! the CLI and the window, or two of either, never write or unmount the iPod
//! under each other. A sync takes it before reading the iPod and keeps it
//! until the write is verified, so it never writes back a database another
//! changed meanwhile. The kernel drops the lock when the process ends, crash
//! included.

use std::fs::{File, OpenOptions};
use std::path::PathBuf;

use rustix::fs::{FlockOperation, flock};

use crate::config::Paths;
use crate::error::IoContext;
use crate::{Error, Result};

/// Held while it lives.
#[derive(Debug)]
pub struct IpodLock {
    _file: File,
    path: PathBuf,
}

impl IpodLock {
    /// Refuses a lock taken for another data folder.
    pub fn check(&self, paths: &Paths) -> Result<()> {
        if self.path == lock_file(paths) {
            Ok(())
        } else {
            Err(Error::NotWritable(format!(
                "the iPod lock held is {}, not the one of {}",
                self.path.display(),
                paths.data_dir.display()
            )))
        }
    }
}

fn lock_file(paths: &Paths) -> PathBuf {
    paths.data_dir.join("ipod.lock")
}

/// Takes the lock without waiting; refused with [`Error::IpodBusy`] while
/// another iprune holds it.
pub fn ipod(paths: &Paths) -> Result<IpodLock> {
    std::fs::create_dir_all(&paths.data_dir).at(&paths.data_dir)?;
    let path = lock_file(paths);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .at(&path)?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(IpodLock { _file: file, path }),
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => Err(Error::IpodBusy),
        Err(e) => Err(Error::Io {
            path,
            source: e.into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(dir: &std::path::Path) -> Paths {
        Paths {
            config_file: dir.join("config.toml"),
            data_dir: dir.join("data"),
        }
    }

    #[test]
    fn a_second_lock_is_refused_while_the_first_lives_and_allowed_after_it_drops() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let first = ipod(&p).unwrap();
        // Another open file description, even in this process, is refused.
        assert!(matches!(ipod(&p), Err(Error::IpodBusy)));
        assert!(matches!(ipod(&p), Err(Error::IpodBusy)));
        drop(first);
        let again = ipod(&p).expect("free after the drop");
        assert!(matches!(ipod(&p), Err(Error::IpodBusy)));
        drop(again);
    }

    #[test]
    fn check_accepts_its_own_data_folder_and_refuses_another() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (pa, pb) = (paths(a.path()), paths(b.path()));
        let lock = ipod(&pa).unwrap();
        lock.check(&pa).unwrap();
        assert!(matches!(lock.check(&pb), Err(Error::NotWritable(_))));
    }

    #[test]
    fn the_data_folder_is_created_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths {
            config_file: dir.path().join("config.toml"),
            data_dir: dir.path().join("a/b/data"),
        };
        assert!(!p.data_dir.exists());
        let _lock = ipod(&p).unwrap();
        assert!(p.data_dir.join("ipod.lock").is_file());
    }

    #[test]
    fn a_lock_file_left_behind_does_not_hold_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        drop(ipod(&p).unwrap());
        assert!(p.data_dir.join("ipod.lock").is_file());
        ipod(&p).expect("the file alone is not a lock");
    }

    #[test]
    fn a_data_folder_that_cannot_be_created_is_an_io_error_not_busy() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, b"x").unwrap();
        let p = Paths {
            config_file: dir.path().join("config.toml"),
            data_dir: blocker.join("data"),
        };
        assert!(matches!(ipod(&p), Err(Error::Io { .. })));
    }
}
