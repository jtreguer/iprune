//! Backups of the iPod database, taken before every write, and restoring
//! one. Each backup is a folder `<root>/<guid>/<stamp>/` holding copies of
//! the iPod files plus what iprune knew at the time.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::error::IoContext;
use crate::{Error, Result};

/// iPod files copied, relative to the mountpoint, and whether each is
/// required. `Play Counts` is kept for inspection but never restored: its
/// plays were pulled already and would count twice.
const FILES: &[(&str, &str, bool)] = &[
    ("iTunesDB", "iPod_Control/iTunes/iTunesDB", true),
    ("ArtworkDB", "iPod_Control/Artwork/ArtworkDB", false),
    ("Play Counts", "iPod_Control/iTunes/Play Counts", false),
    // On-The-Go playlists made on the iPod; libgpod removes it on write.
    (
        "OTGPlaylistInfo",
        "iPod_Control/iTunes/OTGPlaylistInfo",
        false,
    ),
];

/// The files `restore` puts back.
const RESTORED: &[&str] = &["iTunesDB", "ArtworkDB"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backup {
    pub dir: PathBuf,
    /// `YYYYMMDD-HHMMSS` in UTC, possibly with a `-N` suffix.
    pub stamp: String,
}

/// UTC date and time of a Unix timestamp as `YYYYMMDD-HHMMSS`.
pub fn stamp(unix: i64) -> String {
    let (days, secs) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    // Days to civil date, from Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

fn copy_synced(from: &Path, to: &Path) -> Result<()> {
    fs::copy(from, to).at(from)?;
    fs::File::open(to).and_then(|f| f.sync_all()).at(to)?;
    if fs::read(from).at(from)? != fs::read(to).at(to)? {
        return Err(Error::BackupMismatch(to.to_owned()));
    }
    Ok(())
}

/// Copies the iPod database files and writes `extra` files (name, contents)
/// into a new backup folder.
pub fn create(
    root: &Path,
    guid: &str,
    mountpoint: &Path,
    now: i64,
    extra: &[(&str, &[u8])],
) -> Result<Backup> {
    let base = root.join(guid);
    fs::create_dir_all(&base).at(&base)?;
    // Folders left by a backup interrupted part way.
    for e in fs::read_dir(&base).at(&base)?.filter_map(|e| e.ok()) {
        if e.file_name().as_encoded_bytes().starts_with(b".partial-") {
            fs::remove_dir_all(e.path()).at(e.path())?;
        }
    }
    let first = stamp(now);
    // After the highest backup already taken in this second, never in a gap
    // a prune left: the new backup must sort last.
    let taken = fs::read_dir(&base)
        .at(&base)?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|n| match n.strip_prefix(&first)? {
            "" => Some(1),
            rest => rest.strip_prefix('-')?.parse::<u32>().ok(),
        })
        .max();
    let stamp = match taken {
        None => first.clone(),
        Some(n) => format!("{first}-{}", n + 1),
    };
    let dir = base.join(&stamp);
    // Filled under a hidden name and renamed when complete, so a backup
    // interrupted part way is never listed or restored.
    let partial = base.join(format!(".partial-{stamp}"));
    fs::create_dir(&partial).at(&partial)?;
    for (name, rel, required) in FILES {
        let from = mountpoint.join(rel);
        if !from.is_file() {
            if *required {
                return Err(Error::Io {
                    path: from,
                    source: std::io::ErrorKind::NotFound.into(),
                });
            }
            continue;
        }
        copy_synced(&from, &partial.join(name))?;
    }
    for (name, contents) in extra {
        let path = partial.join(name);
        let mut f = fs::File::create(&path).at(&path)?;
        f.write_all(contents).at(&path)?;
        f.sync_all().at(&path)?;
    }
    fs::rename(&partial, &dir).at(&dir)?;
    Ok(Backup { dir, stamp })
}

/// The backups of one iPod, oldest first.
pub fn list(root: &Path, guid: &str) -> Result<Vec<Backup>> {
    let base = root.join(guid);
    if !base.is_dir() {
        return Ok(Vec::new());
    }
    let mut out: Vec<Backup> = fs::read_dir(&base)
        .at(&base)?
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_name().as_encoded_bytes().starts_with(b"."))
        .filter(|e| e.path().join("iTunesDB").is_file())
        .filter_map(|e| {
            Some(Backup {
                stamp: e.file_name().into_string().ok()?,
                dir: e.path(),
            })
        })
        .collect();
    // By time, then by the `-N` suffix taken as a number (`-10` after `-2`).
    let key = |s: &str| {
        let (base, n) = match s.get(15..).and_then(|r| r.strip_prefix('-')) {
            Some(n) => (&s[..15], n.parse::<u32>().unwrap_or(u32::MAX)),
            None => (s, 1),
        };
        (base.to_owned(), n)
    };
    out.sort_by_key(|b| key(&b.stamp));
    Ok(out)
}

/// Deletes the oldest backups so at most `keep` remain. Returns the removed
/// folders.
pub fn prune(root: &Path, guid: &str, keep: usize) -> Result<Vec<PathBuf>> {
    let all = list(root, guid)?;
    let excess = all.len().saturating_sub(keep);
    let mut removed = Vec::new();
    for b in all.into_iter().take(excess) {
        fs::remove_dir_all(&b.dir).at(&b.dir)?;
        removed.push(b.dir);
    }
    Ok(removed)
}

/// Puts the backed up `iTunesDB` and `ArtworkDB` back on the iPod, each
/// through a temporary file and a rename. Returns the files restored.
pub fn restore_files(backup: &Backup, mountpoint: &Path) -> Result<Vec<&'static str>> {
    let mut done = Vec::new();
    for (name, rel, _) in FILES.iter().filter(|(n, _, _)| RESTORED.contains(n)) {
        let from = backup.dir.join(name);
        if !from.is_file() {
            continue;
        }
        let to = mountpoint.join(rel);
        let dir = to.parent().expect("an iPod path has a parent");
        let tmp = dir.join(format!(".iprune-restore-{name}"));
        copy_synced(&from, &tmp)?;
        fs::rename(&tmp, &to).at(&to)?;
        done.push(*name);
    }
    Ok(done)
}
