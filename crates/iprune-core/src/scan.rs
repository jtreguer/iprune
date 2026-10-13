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
use crate::tags::{self, ReadError};
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
        let walker = walkdir::WalkDir::new(root).into_iter().filter_entry(|e| {
            e.depth() == 0 || !e.file_name().as_encoded_bytes().starts_with(b".")
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
    }
    seen
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
    // Nothing under a path that could not be read is declared missing.
    let walk_errors: Vec<PathBuf> = report.unreadable.iter().map(|(p, _)| p.clone()).collect();
    let vanished = |t: &TrackRow| {
        t.missing_since.is_none()
            && !seen_keys.contains(t.path_key.as_str())
            && !walk_errors.iter().any(|e| t.path.starts_with(e))
    };

    let mut to_read: Vec<(&Seen, Option<&TrackRow>)> = Vec::new();
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
                report.unchanged += 1;
            }
            t => to_read.push((s, t.copied())),
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
            .map(|&(s, t)| {
                let r = read_file(s);
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
                    _ => {
                        store::insert_track(&tx, &rec, now)?;
                        report.added += 1;
                    }
                }
            }
        }
    }
    for (t, newly) in gone.into_values().flatten() {
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
}
