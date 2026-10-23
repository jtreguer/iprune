//! Library scan: walk the roots, read new or changed files, detect moves,
//! mark missing files, then reload the playlists.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::config::Config;
use crate::playlists::{self, LoadReport};
use crate::store::{self, FileRecord, Store, TrackRow};
use crate::tags::{self, AudioInfo, FileKind, ReadError};
use crate::{Error, Result, now_unix, paths};

/// Threads used to read and hash files. The library is on a USB hard disk,
/// where many parallel readers only add seeking.
const READ_THREADS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Walking { files: usize },
    Reading { done: usize, total: usize },
}

#[derive(Debug, Default)]
pub struct ScanReport {
    pub unchanged: usize,
    pub added: usize,
    pub updated: usize,
    /// Files that were missing and came back at the same path.
    pub returned: usize,
    /// `(old, new)` paths of files detected as moved or renamed.
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// Files that disappeared in this scan.
    pub missing: Vec<PathBuf>,
    pub unreadable: Vec<(PathBuf, String)>,
    pub unsupported: Vec<(PathBuf, String)>,
    /// Present files indexed without tags because their tags could not be
    /// parsed. Taken from the index: listed on every scan until fixed.
    pub untagged: Vec<(PathBuf, String)>,
    /// Present FairPlay protected files (iTunes Store purchases), indexed as
    /// AAC. Taken from the index.
    pub drm: Vec<PathBuf>,
    pub non_utf8: Vec<PathBuf>,
    /// Folders under a root that hold an iPod (`iPod_Control/iTunes/
    /// iTunesDB`): never indexed, or the iPod's own files would become
    /// library tracks.
    pub ipods: Vec<PathBuf>,
    /// Playlists rewritten to follow moved files.
    pub rewritten_playlists: Vec<String>,
    pub playlists: LoadReport,
}

struct Seen {
    path: PathBuf,
    key: String,
    size: u64,
    mtime_ns: i64,
}

fn walk(cfg: &Config, report: &mut ScanReport, progress: &(dyn Fn(Progress) + Sync)) -> Vec<Seen> {
    let mut seen = Vec::new();
    let mut keys = HashSet::new();
    for root in &cfg.library.roots {
        let ipods = std::cell::RefCell::new(Vec::new());
        let walker = walkdir::WalkDir::new(root).into_iter().filter_entry(|e| {
            if e.depth() > 0 && e.file_name().as_encoded_bytes().starts_with(b".") {
                return false;
            }
            let ipod =
                e.file_type().is_dir() && e.path().join("iPod_Control/iTunes/iTunesDB").is_file();
            if ipod {
                ipods.borrow_mut().push(e.path().to_owned());
            }
            !ipod
        });
        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    let path = e.path().unwrap_or(root).to_path_buf();
                    report.unreadable.push((path, e.to_string()));
                    continue;
                }
            };
            // Links are not followed: what is under one is kept as it was,
            // never declared missing, and the link is reported.
            if entry.depth() > 0 && entry.path_is_symlink() {
                report
                    .unreadable
                    .push((entry.into_path(), "symbolic link, not followed".into()));
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.into_path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if cfg.excluded(&ext) {
                continue;
            }
            if tags::UNSUPPORTED_EXTENSIONS.contains(&ext.as_str()) {
                report.unsupported.push((path, ext));
                continue;
            }
            if !tags::AUDIO_EXTENSIONS.contains(&ext.as_str()) {
                continue;
            }
            let Some(key) = paths::key(&path) else {
                report.non_utf8.push(path);
                continue;
            };
            if !keys.insert(key.clone()) {
                report.unreadable.push((
                    path,
                    "another file has the same name once normalized".into(),
                ));
                continue;
            }
            match std::fs::metadata(&path) {
                Ok(m) => seen.push(Seen {
                    path,
                    key,
                    size: m.size(),
                    mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
                }),
                Err(e) => report.unreadable.push((path, e.to_string())),
            }
            if seen.len().is_multiple_of(500) {
                progress(Progress::Walking { files: seen.len() });
            }
        }
        report.ipods.extend(ipods.into_inner());
    }
    seen
}

/// More indexed tracks than this under a folder now empty means a disk or
/// share not mounted there, not an album deleted file by file.
const EMPTY_FOLDER_TRACKS: usize = 50;

/// Whether `dir` is a mount target in `/etc/fstab` text.
fn mounted_by_fstab(fstab: &str, dir: &Path) -> bool {
    fstab
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_whitespace().nth(1))
        .any(|target| Path::new(&crate::device::unescape_mount(target)) == dir)
}

/// What a retag leaves unchanged in a file: its kind, sample rate, bitrate
/// and exact duration. `None` without a duration.
type AudioKey = (FileKind, Option<u32>, Option<u32>, u32);

/// Whether two tag sets name the same song well enough to pair files by
/// their audio properties: the same title, or the same artist and album,
/// ignoring case and surrounding spaces.
fn same_song(a: &tags::Tags, b: &tags::Tags) -> bool {
    let eq = |x: &Option<String>, y: &Option<String>| match (x, y) {
        (Some(x), Some(y)) => x.trim().to_lowercase() == y.trim().to_lowercase(),
        _ => false,
    };
    eq(&a.title, &b.title) || (eq(&a.artist, &b.artist) && eq(&a.album, &b.album))
}

fn audio_key(info: &AudioInfo) -> Option<AudioKey> {
    let ms = info.duration_ms.filter(|&ms| ms > 0)?;
    Some((info.kind, info.samplerate, info.bitrate, ms))
}

fn hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    hash_reader(File::open(path)?)
}

/// The content hash stored in the index.
pub(crate) fn hash_reader(r: impl std::io::Read) -> std::io::Result<[u8; 32]> {
    let mut h = blake3::Hasher::new();
    h.update_reader(r)?;
    Ok(*h.finalize().as_bytes())
}

/// Runs a tag reader, turning a panic into `Unreadable`: one malformed file
/// must not stop the whole scan.
fn without_panics<T>(
    read: impl FnOnce() -> Result<T, ReadError> + std::panic::UnwindSafe,
) -> Result<T, ReadError> {
    std::panic::catch_unwind(read).unwrap_or_else(|_| {
        Err(ReadError::Unreadable(
            "the tag reader crashed on this file".into(),
        ))
    })
}

/// A file's tags read again, its content hash `hash` kept.
fn read_tags(s: &Seen, hash: [u8; 32]) -> Result<FileRecord, ReadError> {
    let read = without_panics(|| tags::read(&s.path))?;
    Ok(FileRecord {
        path: s.path.clone(),
        path_key: s.key.clone(),
        size: s.size,
        mtime_ns: s.mtime_ns,
        hash,
        info: read.info,
        tags: read.tags,
        tag_error: read.tag_error,
        drm: read.drm,
    })
}

fn read_file(s: &Seen) -> Result<FileRecord, ReadError> {
    let read = without_panics(|| tags::read(&s.path))?;
    let hash = hash_file(&s.path).map_err(|e| ReadError::Unreadable(e.to_string()))?;
    Ok(FileRecord {
        path: s.path.clone(),
        path_key: s.key.clone(),
        size: s.size,
        mtime_ns: s.mtime_ns,
        hash,
        info: read.info,
        tags: read.tags,
        tag_error: read.tag_error,
        drm: read.drm,
    })
}

pub fn scan(
    store: &mut Store,
    cfg: &Config,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<ScanReport> {
    let mut report = ScanReport::default();
    let existing = store::all_tracks(store.conn())?;
    let by_key: HashMap<&str, &TrackRow> =
        existing.iter().map(|t| (t.path_key.as_str(), t)).collect();

    // An unmounted disk must not look like an empty library.
    for root in &cfg.library.roots {
        if !root.is_dir() {
            return Err(Error::RootMissing(root.clone()));
        }
    }
    let seen = walk(cfg, &mut report, progress);
    // Same for a root that is an empty mountpoint, checked root by root.
    for root in &cfg.library.roots {
        let indexed = existing
            .iter()
            .filter(|t| t.missing_since.is_none() && t.path.starts_with(root))
            .count();
        if indexed > 0 && !seen.iter().any(|s| s.path.starts_with(root)) {
            return Err(Error::EmptyRoot {
                root: root.clone(),
                tracks: indexed,
            });
        }
    }
    let seen_keys: HashSet<&str> = seen.iter().map(|s| s.key.as_str()).collect();
    // Same for a folder below a root that is now empty and is where a disk
    // or share is mounted (in /etc/fstab), or held more tracks than an
    // album: most likely what is mounted there is not.
    let fstab = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
    let mut empty: HashMap<PathBuf, usize> = HashMap::new();
    for t in &existing {
        if t.missing_since.is_some() || seen_keys.contains(t.path_key.as_str()) {
            continue;
        }
        let Some(root) = cfg.library.roots.iter().find(|r| t.path.starts_with(r)) else {
            continue;
        };
        let Some(dir) = t.path.ancestors().skip(1).find(|d| d.is_dir()) else {
            continue;
        };
        if dir != root.as_path() && dir.starts_with(root) {
            *empty.entry(dir.to_owned()).or_default() += 1;
        }
    }
    for (dir, held) in empty {
        if (held > EMPTY_FOLDER_TRACKS || mounted_by_fstab(&fstab, &dir))
            && std::fs::read_dir(&dir).is_ok_and(|mut d| d.next().is_none())
        {
            report.unreadable.push((
                dir,
                "empty folder that held indexed tracks: is a disk mounted there? \
             If the music was deleted on purpose, remove the folder"
                    .into(),
            ));
        }
    }
    // Nothing under a path that could not be read is declared missing.
    // An iPod inside a root is skipped, and rows an older scan indexed from
    // it are left as they were.
    let walk_errors: Vec<PathBuf> = report
        .unreadable
        .iter()
        .map(|(p, _)| p.clone())
        .chain(report.ipods.iter().cloned())
        .collect();
    let vanished = |t: &TrackRow| {
        t.missing_since.is_none()
            && !seen_keys.contains(t.path_key.as_str())
            && !walk_errors.iter().any(|e| t.path.starts_with(e))
    };

    // Rows whose tags an older reader read: their tags are read again, not
    // their audio, whose hash is kept.
    let stale = store::stale_tags(store.conn())?;
    let mut to_read: Vec<(&Seen, Option<&TrackRow>, bool)> = Vec::new();
    for s in &seen {
        match by_key.get(s.key.as_str()) {
            Some(t)
                if t.missing_since.is_none()
                    && t.size == s.size
                    && t.mtime_ns == s.mtime_ns
                    && t.path == s.path
                    // Files whose tags failed are retried on every scan.
                    && t.tag_error.is_none() =>
            {
                if stale.contains(&t.id) {
                    to_read.push((s, Some(t), true));
                } else {
                    report.unchanged += 1;
                }
            }
            t => to_read.push((s, t.copied(), false)),
        }
    }

    let done = AtomicUsize::new(0);
    let total = to_read.len();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(READ_THREADS)
        .build()
        .expect("thread pool");
    let results: Vec<_> = pool.install(|| {
        to_read
            .par_iter()
            .map(|&(s, t, tags_only)| {
                let r = match (tags_only, t) {
                    (true, Some(t)) => read_tags(s, t.hash),
                    _ => read_file(s),
                };
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_multiple_of(25) || n == total {
                    progress(Progress::Reading { done: n, total });
                }
                (s, t, r)
            })
            .collect()
    });

    // Move candidates by content hash: tracks whose file vanished in this
    // scan, and tracks already missing (a file moved out of the library and
    // later back in keeps its id). The flag is true for the first kind.
    let mut gone: HashMap<[u8; 32], Vec<(&TrackRow, bool)>> = HashMap::new();
    for t in &existing {
        if vanished(t) {
            gone.entry(t.hash).or_default().push((t, true));
        } else if t.missing_since.is_some() && !seen_keys.contains(t.path_key.as_str()) {
            gone.entry(t.hash).or_default().push((t, false));
        }
    }
    let mut new_hash_count: HashMap<[u8; 32], usize> = HashMap::new();
    for (_, t, r) in &results {
        if let (None, Ok(rec)) = (t, r) {
            *new_hash_count.entry(rec.hash).or_default() += 1;
        }
    }

    let now = now_unix();
    let tx = store.transaction()?;
    let mut moves: HashMap<String, i64> = HashMap::new();
    // New files the content hash did not pair with a vanished track.
    let mut unpaired: Vec<FileRecord> = Vec::new();
    for (s, t, r) in results {
        let rec = match r {
            Ok(rec) => rec,
            Err(ReadError::Unsupported(why)) => {
                report.unsupported.push((s.path.clone(), why));
                continue;
            }
            Err(ReadError::Unreadable(why)) => {
                report.unreadable.push((s.path.clone(), why));
                continue;
            }
        };
        match t {
            Some(t) => {
                store::update_track(&tx, t.id, &rec)?;
                if t.missing_since.is_some() {
                    report.returned += 1
                } else {
                    report.updated += 1
                }
            }
            None => {
                // A new file is a moved track when it is the only new file with
                // that content and exactly one vanished or missing track had it.
                let candidates = gone.get_mut(&rec.hash);
                match candidates {
                    Some(c) if c.len() == 1 && new_hash_count[&rec.hash] == 1 => {
                        let (old, _) = c.pop().expect("one candidate");
                        store::update_track(&tx, old.id, &rec)?;
                        moves.insert(old.path_key.clone(), old.id);
                        report.moved.push((old.path.clone(), rec.path.clone()));
                    }
                    _ => unpaired.push(rec),
                }
            }
        }
    }
    // A file moved and retagged in one go (Picard, beets) has another hash,
    // but the same audio: the same kind, sample rate, bitrate and duration
    // to the millisecond, which tags do not change. Paired only when exactly
    // one vanished or missing track and exactly one new file share them.
    let left: Vec<(&TrackRow, bool)> = gone.values().flatten().copied().collect();
    let mut by_audio: HashMap<AudioKey, Vec<usize>> = HashMap::new();
    for (i, (t, _)) in left.iter().enumerate() {
        if let Some(k) = audio_key(&t.info) {
            by_audio.entry(k).or_default().push(i);
        }
    }
    let mut new_by_audio: HashMap<AudioKey, usize> = HashMap::new();
    for rec in &unpaired {
        if let Some(k) = audio_key(&rec.info) {
            *new_by_audio.entry(k).or_default() += 1;
        }
    }
    let mut taken: HashSet<i64> = HashSet::new();
    for rec in unpaired {
        let pair = audio_key(&rec.info)
            .filter(|k| new_by_audio[k] == 1)
            .and_then(|k| match by_audio.get(&k).map(Vec::as_slice) {
                Some(&[i]) => Some(left[i].0),
                _ => None,
            })
            .filter(|old| same_song(&old.tags, &rec.tags));
        match pair {
            Some(old) => {
                store::update_track(&tx, old.id, &rec)?;
                // Its audio is not proven the same: every iPod copy of it is
                // copied again, keeping the iPod entry and its history.
                store::devices::forget_synced_audio(&tx, old.id)?;
                taken.insert(old.id);
                moves.insert(old.path_key.clone(), old.id);
                report.moved.push((old.path.clone(), rec.path.clone()));
            }
            None => {
                store::insert_track(&tx, &rec, now)?;
                report.added += 1;
            }
        }
    }
    for (t, newly) in gone.into_values().flatten() {
        if taken.contains(&t.id) {
            continue;
        }
        if newly {
            store::set_missing(&tx, t.id, Some(now))?;
            report.missing.push(t.path.clone());
        }
    }
    report.missing.sort();
    report.moved.sort();
    // Rewrite playlists before committing: if this fails, the next scan sees
    // the same moves again and finishes the job.
    report.rewritten_playlists = playlists::rewrite_moved(&tx, cfg, &moves)?;
    tx.commit()?;

    // Reported from the index, so they show up on every scan, not just the
    // one that first read the file.
    let flagged = store::flagged_tracks(store.conn())?;
    (report.untagged, report.drm) = (flagged.untagged, flagged.drm);
    report.playlists = playlists::sync_from_disk(store.conn(), cfg, true)?;
    Ok(report)
}

/// What a scan found, as `iprune scan` prints it and the window shows it:
/// the counts, then each non-empty list, one item per indented line.
pub fn describe(r: &ScanReport) -> String {
    fn list<T>(out: &mut String, title: &str, items: &[T], line: impl Fn(&T) -> String) {
        if !items.is_empty() {
            out.push_str(&format!("{title}: {}\n", items.len()));
            for i in items {
                out.push_str(&format!("  {}\n", line(i)));
            }
        }
    }
    let shown = |p: &PathBuf| p.display().to_string();
    let with_why = |(p, why): &(PathBuf, String)| format!("{}: {why}", p.display());
    let mut out = format!(
        "library: {} unchanged, {} added, {} updated, {} back from missing, {} moved, {} missing\n",
        r.unchanged,
        r.added,
        r.updated,
        r.returned,
        r.moved.len(),
        r.missing.len()
    );
    list(&mut out, "moved", &r.moved, |(a, b)| {
        format!("{} -> {}", a.display(), b.display())
    });
    list(&mut out, "missing", &r.missing, shown);
    list(&mut out, "unreadable", &r.unreadable, with_why);
    list(
        &mut out,
        "iPods inside the library folders, not indexed",
        &r.ipods,
        shown,
    );
    list(&mut out, "unsupported", &r.unsupported, with_why);
    list(
        &mut out,
        "indexed without tags (tags could not be parsed)",
        &r.untagged,
        with_why,
    );
    list(
        &mut out,
        "FairPlay protected, indexed as AAC",
        &r.drm,
        shown,
    );
    list(&mut out, "names not in UTF-8", &r.non_utf8, shown);
    list(
        &mut out,
        "playlists rewritten for moved files",
        &r.rewritten_playlists,
        |n| n.clone(),
    );
    let pl = &r.playlists;
    out.push_str(&format!(
        "playlists: {} loaded, {} unresolved entries\n",
        pl.loaded,
        pl.unresolved.len()
    ));
    list(&mut out, "unresolved", &pl.unresolved, |(n, e)| {
        format!("{n}: {e}")
    });
    list(
        &mut out,
        "playlists removed from the index",
        &pl.removed,
        |n| n.clone(),
    );
    list(&mut out, "playlist files skipped", &pl.skipped, with_why);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panicking_reader_becomes_unreadable() {
        let r: Result<u32, ReadError> = without_panics(|| panic!("lofty blew up"));
        assert!(matches!(r, Err(ReadError::Unreadable(_))), "{r:?}");
    }

    #[test]
    fn results_pass_through_unchanged() {
        assert_eq!(without_panics(|| Ok(7)), Ok(7));
        let unsupported = || Err::<u32, _>(ReadError::Unsupported("MP4 codec X".into()));
        assert_eq!(
            without_panics(unsupported),
            Err(ReadError::Unsupported("MP4 codec X".into()))
        );
        let unreadable = || Err::<u32, _>(ReadError::Unreadable("bad".into()));
        assert_eq!(
            without_panics(unreadable),
            Err(ReadError::Unreadable("bad".into()))
        );
    }

    const FSTAB: &str = "\
# <file system> <mount point> <type> <options> <dump> <pass>
UUID=1111 / ext4 rw 0 1
   # /mnt/commented ext4 defaults 0 0
//nas/share\t/mnt/nas\tcifs\tuser=x 0 0
/dev/sdb1   /mnt/My\\040Disk   ext4   defaults  0 2

/dev/sdc1 /mnt/seagate ext4 defaults 0 2 # /mnt/after-comment
/mnt/device-field /mnt/other ext4 defaults 0 0
";

    #[test]
    fn a_mount_target_in_fstab_is_found_whatever_the_spacing_and_escapes() {
        for dir in [
            "/",
            "/mnt/nas",
            "/mnt/My Disk",
            "/mnt/seagate",
            "/mnt/other",
        ] {
            assert!(mounted_by_fstab(FSTAB, Path::new(dir)), "{dir}");
        }
    }

    #[test]
    fn comments_other_fields_and_near_misses_are_not_mount_targets() {
        for dir in [
            "/mnt/commented",
            "/mnt/after-comment",
            // the device field, not the mount point
            "/mnt/device-field",
            "//nas/share",
            "/dev/sdb1",
            // prefixes, parents and the unescaped text
            "/mnt",
            "/mnt/na",
            "/mnt/nas/sub",
            "/mnt/seagate2",
            "/mnt/My\\040Disk",
            "/mnt/My",
        ] {
            assert!(!mounted_by_fstab(FSTAB, Path::new(dir)), "{dir}");
        }
        assert!(!mounted_by_fstab("", Path::new("/mnt/nas")));
        assert!(!mounted_by_fstab(
            "# only a comment\n\n",
            Path::new("/mnt/nas")
        ));
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    const COUNTS_ZERO: &str = "library: 0 unchanged, 0 added, 0 updated, 0 back from missing, 0 moved, 0 missing\n\
         playlists: 0 loaded, 0 unresolved entries\n";

    #[test]
    fn describe_of_an_empty_report_is_the_two_count_lines() {
        assert_eq!(describe(&ScanReport::default()), COUNTS_ZERO);
    }

    #[test]
    fn describe_prints_the_counts_in_their_fixed_order() {
        let r = ScanReport {
            unchanged: 1,
            added: 2,
            updated: 3,
            returned: 4,
            moved: vec![(p("/a"), p("/b")); 5],
            missing: vec![p("/m"); 6],
            playlists: LoadReport {
                loaded: 7,
                unresolved: vec![("pl".into(), "x".into()); 8],
                ..LoadReport::default()
            },
            ..ScanReport::default()
        };
        let text = describe(&r);
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            Some(
                "library: 1 unchanged, 2 added, 3 updated, 4 back from missing, 5 moved, 6 missing"
            )
        );
        assert!(
            text.contains("\nplaylists: 7 loaded, 8 unresolved entries\n"),
            "{text}"
        );
    }

    // One list at a time: its heading with the count, then the items indented
    // by two spaces, and nothing else when the list is empty.
    #[test]
    fn describe_each_list_alone_has_its_heading_count_and_item_lines() {
        type Fill = fn(&mut ScanReport);
        let cases: Vec<(&str, Fill, &str)> = vec![
            (
                "moved",
                |r| r.moved = vec![(p("/old.mp3"), p("/new.mp3")), (p("/o2"), p("/n2"))],
                "moved: 2\n  /old.mp3 -> /new.mp3\n  /o2 -> /n2\n",
            ),
            (
                "missing",
                |r| r.missing = vec![p("/gone.mp3")],
                "missing: 1\n  /gone.mp3\n",
            ),
            (
                "unreadable",
                |r| r.unreadable = vec![(p("/bad.mp3"), "truncated".into())],
                "unreadable: 1\n  /bad.mp3: truncated\n",
            ),
            (
                "ipods",
                |r| r.ipods = vec![p("/music/ipod")],
                "iPods inside the library folders, not indexed: 1\n  /music/ipod\n",
            ),
            (
                "unsupported",
                |r| r.unsupported = vec![(p("/c.flac"), "FLAC".into())],
                "unsupported: 1\n  /c.flac: FLAC\n",
            ),
            (
                "untagged",
                |r| r.untagged = vec![(p("/u.mp3"), "bad frame".into())],
                "indexed without tags (tags could not be parsed): 1\n  /u.mp3: bad frame\n",
            ),
            (
                "drm",
                |r| r.drm = vec![p("/buy.m4a")],
                "FairPlay protected, indexed as AAC: 1\n  /buy.m4a\n",
            ),
            (
                "non_utf8",
                |r| r.non_utf8 = vec![p("/odd")],
                "names not in UTF-8: 1\n  /odd\n",
            ),
            (
                "rewritten",
                |r| r.rewritten_playlists = vec!["mix".into(), "road".into()],
                "playlists rewritten for moved files: 2\n  mix\n  road\n",
            ),
        ];
        for (name, fill, section) in cases {
            let mut r = ScanReport::default();
            fill(&mut r);
            let text = describe(&r);
            // Counts line first; the section sits before the playlists line.
            let (head, rest) = text.split_once('\n').unwrap();
            assert!(head.starts_with("library: "), "{name}: {text}");
            let tail = "playlists: 0 loaded, 0 unresolved entries\n";
            assert_eq!(rest, format!("{section}{tail}"), "{name}");
        }
    }

    #[test]
    fn describe_playlist_lists_follow_the_playlists_line() {
        let r = ScanReport {
            playlists: LoadReport {
                loaded: 3,
                unresolved: vec![("mix".into(), "lost.mp3".into())],
                removed: vec!["old".into()],
                skipped: vec![(p("/l/x.m3u8"), "not UTF-8".into())],
            },
            ..ScanReport::default()
        };
        let text = describe(&r);
        let want = format!(
            "{}playlists: 3 loaded, 1 unresolved entries\n\
             unresolved: 1\n  mix: lost.mp3\n\
             playlists removed from the index: 1\n  old\n\
             playlist files skipped: 1\n  /l/x.m3u8: not UTF-8\n",
            COUNTS_ZERO.lines().next().unwrap().to_owned() + "\n"
        );
        assert_eq!(text, want);
    }

    // The order of sections is part of the output people read and script.
    #[test]
    fn describe_sections_come_in_the_documented_order() {
        let r = ScanReport {
            moved: vec![(p("/a"), p("/b"))],
            missing: vec![p("/c")],
            unreadable: vec![(p("/d"), "w".into())],
            ipods: vec![p("/e")],
            unsupported: vec![(p("/f"), "w".into())],
            untagged: vec![(p("/g"), "w".into())],
            drm: vec![p("/h")],
            non_utf8: vec![p("/i")],
            rewritten_playlists: vec!["j".into()],
            playlists: LoadReport {
                loaded: 1,
                unresolved: vec![("k".into(), "l".into())],
                removed: vec!["m".into()],
                skipped: vec![(p("/n"), "w".into())],
            },
            ..ScanReport::default()
        };
        let text = describe(&r);
        let headings = [
            "library: ",
            "moved: ",
            "missing: ",
            "unreadable: ",
            "iPods inside the library folders",
            "unsupported: ",
            "indexed without tags",
            "FairPlay protected",
            "names not in UTF-8: ",
            "playlists rewritten for moved files: ",
            "playlists: 1 loaded",
            "unresolved: ",
            "playlists removed from the index: ",
            "playlist files skipped: ",
        ];
        let at: Vec<usize> = headings
            .iter()
            .map(|h| {
                text.find(&format!("\n{h}"))
                    .or_else(|| text.starts_with(h).then_some(0))
                    .unwrap_or_else(|| panic!("no {h:?} in\n{text}"))
            })
            .collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "{at:?}\n{text}");
        assert!(text.ends_with('\n'));
    }
}
