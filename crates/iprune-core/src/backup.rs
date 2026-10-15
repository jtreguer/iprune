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

/// The iPod's artwork folder. libgpod rewrites its `F*_*.ithmb` thumbnail
/// files in place on a write (it moves thumbnails into the slots of removed
/// ones and truncates), and `ArtworkDB` holds offsets into them: the two
/// are kept, and put back, together.
const ARTWORK: &str = "iPod_Control/Artwork";

/// The `.ithmb` files of every backup of one iPod, stored once per content
/// under `<root>/<guid>/.ithmb/<blake3>`: they are large and rarely change.
const STORE: &str = ".ithmb";

/// The list of `.ithmb` files a backup relies on, in its folder.
const ARTWORK_LIST: &str = "artwork.json";

/// One `.ithmb` file of a backup.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Ithmb {
    pub name: String,
    pub size: u64,
    pub mtime_ns: i64,
    /// blake3, in hex: its name in the store.
    pub hash: String,
}

/// The `.ithmb` files in the iPod's artwork folder, sorted.
pub fn ithmb_files(mountpoint: &Path) -> Result<Vec<PathBuf>> {
    let dir = mountpoint.join(ARTWORK);
    let entries = match fs::read_dir(&dir) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::Io { path: dir, source }),
    };
    let mut out = Vec::new();
    for e in entries {
        let e = e.at(&dir)?;
        let name = e.file_name().to_string_lossy().to_lowercase();
        // FAT ignores case; a temporary file of ours starts with a dot.
        if name.ends_with(".ithmb") && !name.starts_with('.') && e.path().is_file() {
            out.push(e.path());
        }
    }
    out.sort();
    Ok(out)
}

/// Copies `from` to `to` while hashing it, syncs `to`, then hashes `to`
/// again from disk: the copy is good only if both hashes agree. Returns the
/// hash in hex.
fn hash_copy(from: &Path, to: &Path) -> Result<String> {
    use std::io::Read as _;
    let mut src = fs::File::open(from).at(from)?;
    let mut dst = fs::File::create(to).at(to)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = src.read(&mut buf).at(from)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        dst.write_all(&buf[..n]).at(to)?;
    }
    dst.sync_all().at(to)?;
    let hash = hasher.finalize().to_hex().to_string();
    if hash_file(to)? != hash {
        return Err(Error::BackupMismatch(to.to_owned()));
    }
    Ok(hash)
}

fn hash_file(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_reader(fs::File::open(path).at(path)?)
        .at(path)?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// The `.ithmb` files backup `dir` relies on; `None` for a backup taken
/// before they were kept, or whose list cannot be read.
pub fn artwork_list(dir: &Path) -> Option<Vec<Ithmb>> {
    artwork_list_checked(dir).ok().flatten()
}

/// Like [`artwork_list`], telling an unreadable list (an error) from none.
pub fn artwork_list_checked(dir: &Path) -> Result<Option<Vec<Ithmb>>> {
    let path = dir.join(ARTWORK_LIST);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(Error::Io { path, source }),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| Error::Io {
            path,
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        })
}

/// Puts each `.ithmb` file of the iPod in the store of `base` (an iPod's
/// backup folder) unless it is there already, and returns the list. Each
/// file is hashed first and copied only when the store lacks its content.
/// A file with the name, size and time it had in `previous` is taken to be
/// the same one, and not read again (libgpod touches every one on a write,
/// so this holds only when nothing was written in between).
fn save_artwork(base: &Path, mountpoint: &Path, previous: &[Ithmb]) -> Result<Vec<Ithmb>> {
    let store = base.join(STORE);
    let files = ithmb_files(mountpoint)?;
    if !files.is_empty() {
        fs::create_dir_all(&store).at(&store)?;
    }
    let mut out = Vec::new();
    for path in files {
        let name = path
            .file_name()
            .expect("listed file")
            .to_string_lossy()
            .into_owned();
        let size = fs::metadata(&path).at(&path)?.len();
        let mtime_ns = crate::playlist::mtime_ns(&path)?;
        let known = previous
            .iter()
            .find(|p| p.name == name && p.size == size && p.mtime_ns == mtime_ns)
            .filter(|p| store.join(&p.hash).is_file());
        let hash = match known {
            Some(p) => p.hash.clone(),
            None => match hash_file(&path)? {
                h if store.join(&h).is_file() => h,
                _ => {
                    let tmp = store.join(format!(".partial-{name}"));
                    let hash = hash_copy(&path, &tmp)?;
                    let dest = store.join(&hash);
                    if dest.is_file() {
                        fs::remove_file(&tmp).at(&tmp)?;
                    } else {
                        fs::rename(&tmp, &dest).at(&dest)?;
                    }
                    hash
                }
            },
        };
        out.push(Ithmb {
            name,
            size,
            mtime_ns,
            hash,
        });
    }
    Ok(out)
}

/// Whether libgpod changed the iPod's `.ithmb` files, compared with backup
/// `dir`'s list, but left its `ArtworkDB` as the backup holds it: the
/// thumbnails moved while the offsets pointing at them did not. libgpod
/// does not report a failed `ArtworkDB` write. A listed file still there
/// with another size (a rearrangement truncates) or a file the list does
/// not have counts as a change; a listed file gone does not, since libgpod
/// deletes a file no thumbnail uses without changing `ArtworkDB`. False for
/// a backup taken before the list was kept.
pub fn artwork_left_behind(dir: &Path, mountpoint: &Path) -> bool {
    let Some(list) = artwork_list(dir) else {
        return false;
    };
    let Ok(now) = ithmb_files(mountpoint) else {
        return false;
    };
    let named = |p: &PathBuf| p.file_name().map(|n| n.to_string_lossy().into_owned());
    let added = now
        .iter()
        .any(|p| !list.iter().any(|i| Some(&i.name) == named(p).as_ref()));
    let resized = list.iter().any(|i| {
        fs::metadata(mountpoint.join(ARTWORK).join(&i.name)).is_ok_and(|m| m.len() != i.size)
    });
    let moved = added || resized;
    let artwork_db = |p: &Path| fs::read(p).ok();
    moved
        && artwork_db(&dir.join("ArtworkDB"))
            == artwork_db(&mountpoint.join(ARTWORK).join("ArtworkDB"))
}

/// Whether the iPod's `.ithmb` files are still the ones backup `dir` lists:
/// the same names, each with the same content. True for a backup taken
/// before they were kept, false when its list cannot be read.
pub fn artwork_unchanged(dir: &Path, mountpoint: &Path) -> bool {
    let list = match artwork_list_checked(dir) {
        Ok(Some(l)) => l,
        Ok(None) => return true,
        Err(_) => return false,
    };
    let Ok(now) = ithmb_files(mountpoint) else {
        return false;
    };
    now.len() == list.len()
        && list
            .iter()
            .all(|i| hash_file(&mountpoint.join(ARTWORK).join(&i.name)).is_ok_and(|h| h == i.hash))
}

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
    create_with(root, guid, mountpoint, now, extra, true)
}

/// Like [`create`], for the copy a restore takes of what it replaces: the
/// iPod may have no `iTunesDB` left, and what is there is still kept. Such a
/// backup is listed only when it holds an `iTunesDB`.
pub fn create_before_restore(
    root: &Path,
    guid: &str,
    mountpoint: &Path,
    now: i64,
    extra: &[(&str, &[u8])],
) -> Result<Backup> {
    create_with(root, guid, mountpoint, now, extra, false)
}

fn create_with(
    root: &Path,
    guid: &str,
    mountpoint: &Path,
    now: i64,
    extra: &[(&str, &[u8])],
    require_db: bool,
) -> Result<Backup> {
    let base = folder(root, guid);
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
            if *required && require_db {
                return Err(Error::Io {
                    path: from,
                    source: std::io::ErrorKind::NotFound.into(),
                });
            }
            continue;
        }
        copy_synced(&from, &partial.join(name))?;
    }
    let previous = list(root, guid)?
        .last()
        .and_then(|b| artwork_list(&b.dir))
        .unwrap_or_default();
    let artwork = save_artwork(&base, mountpoint, &previous)?;
    let path = partial.join(ARTWORK_LIST);
    let json = serde_json::to_vec_pretty(&artwork).expect("plain data serializes");
    fs::write(&path, json).at(&path)?;
    fs::File::open(&path).and_then(|f| f.sync_all()).at(&path)?;
    for (name, contents) in extra {
        let path = partial.join(name);
        let mut f = fs::File::create(&path).at(&path)?;
        f.write_all(contents).at(&path)?;
        f.sync_all().at(&path)?;
    }
    fs::rename(&partial, &dir).at(&dir)?;
    Ok(Backup { dir, stamp })
}

/// What a backup holds, from its `snapshot.txt`: the iPod's name and its
/// track and playlist counts, as "iPod de Julien, 9669 tracks, 336
/// playlists". `None` when the backup has no readable snapshot.
pub fn summary(backup: &Backup) -> Option<String> {
    let text = fs::read_to_string(backup.dir.join("snapshot.txt")).ok()?;
    let mut lines = text.lines();
    let counts: Vec<&str> = lines.next()?.split(' ').collect();
    let ["tracks", tracks, "playlists", playlists] = counts[..] else {
        return None;
    };
    let name = lines
        .filter_map(|l| l.strip_prefix("P "))
        .find_map(|l| Some(l[..l.find(" [master]")?].to_owned()));
    Some(format!(
        "{}, {tracks} tracks, {playlists} playlists",
        name.as_deref().unwrap_or("unnamed iPod")
    ))
}

/// The folder of the iPod `guid`'s backups under `root`: the one already
/// there whose name is the same GUID however spelled (`0x` or not, either
/// case, as libgpod gives it from `SysInfo` or `SysInfoExtended`), or a new
/// one named `guid`.
fn folder(root: &Path, guid: &str) -> PathBuf {
    let exact = root.join(guid);
    if exact.is_dir() {
        return exact;
    }
    let want = crate::device::normalize_guid(guid);
    fs::read_dir(root)
        .ok()
        .and_then(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| !e.file_name().as_encoded_bytes().starts_with(b"."))
                .find(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| crate::device::normalize_guid(n) == want)
                })
                .map(|e| e.path())
        })
        .unwrap_or(exact)
}

/// The backups of one iPod, oldest first.
pub fn list(root: &Path, guid: &str) -> Result<Vec<Backup>> {
    let base = folder(root, guid);
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
    prune_store(&folder(root, guid))?;
    Ok(removed)
}

/// Deletes the stored `.ithmb` files no backup of the iPod in `base` lists
/// any more, listed or not (a backup taken before a restore of an iPod
/// with no `iTunesDB` is not listed, but keeps its files).
fn prune_store(base: &Path) -> Result<()> {
    let store = base.join(STORE);
    if !store.is_dir() {
        return Ok(());
    }
    let mut used = std::collections::HashSet::new();
    for e in fs::read_dir(base).at(base)?.filter_map(|e| e.ok()) {
        if e.file_name().as_encoded_bytes().starts_with(b".") {
            continue;
        }
        match artwork_list(&e.path()) {
            Some(l) => used.extend(l.into_iter().map(|i| i.hash)),
            // An unreadable list: keep everything.
            None if e.path().join(ARTWORK_LIST).exists() => return Ok(()),
            None => {}
        }
    }
    for e in fs::read_dir(&store).at(&store)?.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().into_owned();
        if !used.contains(&name) {
            fs::remove_file(e.path()).at(e.path())?;
        }
    }
    Ok(())
}

/// Puts the backed up `iTunesDB` and `ArtworkDB` back on the iPod, with the
/// `.ithmb` files the backup lists (those the iPod holds unchanged are left
/// as they are). Every file is first copied next to its target under a
/// temporary name and compared, then renamed over it: the `.ithmb` files,
/// then `ArtworkDB`, then `iTunesDB`. A failure while copying changes
/// nothing, and the free space is checked first. A failure part way through
/// the renames leaves the old `iTunesDB`, but possibly cover art files that
/// do not match the `ArtworkDB` beside them: the caller marks the restore
/// as unfinished until it is run again. An unreadable artwork list is
/// refused. Returns the files restored.
pub fn restore_files(backup: &Backup, mountpoint: &Path) -> Result<Vec<&'static str>> {
    let plan = plan_restore(backup, mountpoint)?;
    restore_planned(backup, mountpoint, plan)
}

/// What [`restore_files`] will copy: the `.ithmb` files the iPod does not
/// already hold with the listed content.
#[derive(Debug)]
pub struct RestorePlan {
    ithmb: Vec<Ithmb>,
}

/// Checks, before anything changes, that backup `backup` can be put back
/// on the iPod at `mountpoint`: its artwork list reads, every listed
/// `.ithmb` content the iPod lacks is in the store with its size, and the
/// iPod has room for the temporary copies.
pub fn plan_restore(backup: &Backup, mountpoint: &Path) -> Result<RestorePlan> {
    let store = backup
        .dir
        .parent()
        .expect("a backup is in its iPod's folder")
        .join(STORE);
    let dir = mountpoint.join(ARTWORK);
    let ithmb: Vec<Ithmb> = artwork_list_checked(&backup.dir)?
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !hash_file(&dir.join(&i.name)).is_ok_and(|h| h == i.hash))
        .collect();
    let needed: u64 = ithmb.iter().map(|i| i.size).sum::<u64>()
        + RESTORED
            .iter()
            .filter_map(|n| fs::metadata(backup.dir.join(n)).ok())
            .map(|m| m.len())
            .sum::<u64>();
    if let Ok(s) = rustix::fs::statvfs(mountpoint) {
        let free = s.f_bavail * s.f_frsize;
        if free < needed {
            return Err(Error::NoRoomToRestore { needed, free });
        }
    }
    for i in &ithmb {
        let stored = store.join(&i.hash);
        match fs::metadata(&stored) {
            Ok(m) if m.is_file() && m.len() == i.size => {}
            // Truncated or otherwise damaged; its content is checked again
            // when it is copied.
            Ok(_) => return Err(Error::BackupMismatch(stored)),
            Err(source) => {
                return Err(Error::Io {
                    path: stored,
                    source,
                });
            }
        }
    }
    Ok(RestorePlan { ithmb })
}

/// [`restore_files`] with a plan from [`plan_restore`].
pub fn restore_planned(
    backup: &Backup,
    mountpoint: &Path,
    plan: RestorePlan,
) -> Result<Vec<&'static str>> {
    let mut staged: Vec<(&'static str, PathBuf, PathBuf)> = Vec::new();
    let unstage = |staged: &[(&'static str, PathBuf, PathBuf)]| {
        for (_, tmp, _) in staged {
            let _ = fs::remove_file(tmp);
        }
    };
    // The `.ithmb` files `ArtworkDB` points into, from the store, first.
    let store = backup
        .dir
        .parent()
        .expect("a backup is in its iPod's folder")
        .join(STORE);
    let dir = mountpoint.join(ARTWORK);
    for i in plan.ithmb {
        let tmp = dir.join(format!(".iprune-restore-{}", i.name));
        match hash_copy(&store.join(&i.hash), &tmp) {
            Ok(h) if h == i.hash => staged.push(("cover art", tmp, dir.join(&i.name))),
            Ok(_) => {
                unstage(&staged);
                let _ = fs::remove_file(&tmp);
                return Err(Error::BackupMismatch(store.join(&i.hash)));
            }
            Err(e) => {
                unstage(&staged);
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        }
    }
    // `RESTORED` in reverse: `ArtworkDB`, then `iTunesDB`.
    for name in RESTORED.iter().rev() {
        let from = backup.dir.join(name);
        if !from.is_file() {
            continue;
        }
        let rel = FILES
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, rel, _)| *rel)
            .expect("restored files are backed up");
        let to = mountpoint.join(rel);
        let dir = to.parent().expect("an iPod path has a parent");
        let tmp = dir.join(format!(".iprune-restore-{name}"));
        if let Err(e) = copy_synced(&from, &tmp) {
            unstage(&staged);
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        staged.push((*name, tmp, to));
    }
    let mut done = Vec::new();
    for (name, tmp, to) in staged {
        fs::rename(&tmp, &to).at(&to)?;
        if !done.contains(&name) {
            done.push(name);
        }
    }
    Ok(done)
}

/// Files a restore takes off the iPod once they are in the backup taken
/// before it: libgpod would read `Play Counts` and `OTGPlaylistInfo`, which
/// index tracks by their position, against the restored database.
pub const SET_ASIDE: &[&str] = &["Play Counts", "OTGPlaylistInfo"];

/// Deletes the [`SET_ASIDE`] files from the iPod, each only when `kept`, the
/// backup taken before the restore, holds the same bytes. Returns the files
/// deleted.
pub fn set_aside(kept: &Backup, mountpoint: &Path) -> Result<Vec<&'static str>> {
    let mut done = Vec::new();
    for (name, rel, _) in FILES.iter().filter(|(n, _, _)| SET_ASIDE.contains(n)) {
        let live = mountpoint.join(rel);
        let now = match fs::read(&live) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(Error::Io { path: live, source }),
        };
        let copy = kept.dir.join(name);
        if fs::read(&copy).ok().as_deref() != Some(&now[..]) {
            return Err(Error::BackupMismatch(copy));
        }
        fs::remove_file(&live).at(&live)?;
        done.push(*name);
    }
    Ok(done)
}
