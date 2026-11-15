//! Import (technical document, section 7.2): the inbox, each file's state
//! in the index, its status, the lookups, and filing through autotag.
//!
//! iprune reads the inbox, keeps what autotag answered and what the user
//! chose, checks for duplicates against the index, and after filing scans
//! and adds the new songs to a playlist. autotag looks songs up and files
//! them; iprune has no network code.

pub mod autotag;

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

pub use autotag::{Applied, Autotag, By, Candidate, Fields, Hello, Lookup, Query};

use crate::adopt::normalize;
use crate::config::Config;
use crate::error::IoContext;
use crate::scan::{self, AudioKey, ScanReport};
use crate::store::{self, Store, TrackRow};
use crate::tags::{self, FileTags, ReadError, Tags};
use crate::{Error, Result, now_unix, paths, playlists};

/// How far a filed song's duration may be from its inbox copy's: a tag
/// write never changes the audio, but a reader may count the frames of an
/// MP3 without a header slightly differently once its tag changed size.
const DURATION_SLACK_MS: u32 = 50;

/// A file in the inbox, as recorded in `import_items`.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: i64,
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
    pub hash: [u8; 32],
    /// autotag's lookup answer, as it came.
    pub lookup: Option<String>,
    /// The artist and title typed in Search again.
    pub search: Option<Query>,
    /// The candidate the user chose; none takes the first.
    pub choice: Option<usize>,
    /// Tags typed in Type tags, which replace the candidate's.
    pub tags: Option<Fields>,
    /// A possible duplicate the user accepted.
    pub confirmed: bool,
    /// The last lookup or filing error.
    pub error: Option<String>,
    pub added_at: i64,
}

/// A file found in the inbox. Files that cannot be imported have no row.
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub item: Option<Item>,
    pub file: std::result::Result<FileTags, ReadError>,
}

/// The inbox, read.
#[derive(Debug, Clone)]
pub struct Inbox {
    pub dir: PathBuf,
    /// In path order, which is the order songs are filed in.
    pub entries: Vec<Entry>,
    /// Paths that could not be read, with why.
    pub unreadable: Vec<(PathBuf, String)>,
}

// -- State ------------------------------------------------------------------

const ITEM_COLUMNS: &str =
    "id, path, size, mtime_ns, hash, lookup, search, choice, tags, confirmed, error, added_at";

fn json<T: for<'de> Deserialize<'de>>(r: &Row, idx: usize) -> rusqlite::Result<Option<T>> {
    match r.get::<_, Option<String>>(idx)? {
        None => Ok(None),
        Some(s) => serde_json::from_str(&s).map(Some).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, e.into())
        }),
    }
}

fn item_from_row(r: &Row) -> rusqlite::Result<Item> {
    let hash: Vec<u8> = r.get(4)?;
    Ok(Item {
        id: r.get(0)?,
        path: PathBuf::from(r.get::<_, String>(1)?),
        size: r.get::<_, i64>(2)? as u64,
        mtime_ns: r.get(3)?,
        hash: hash.try_into().map_err(|_| {
            rusqlite::Error::InvalidColumnType(4, "hash".into(), rusqlite::types::Type::Blob)
        })?,
        lookup: r.get(5)?,
        search: json(r, 6)?,
        choice: r.get::<_, Option<i64>>(7)?.map(|c| c as usize),
        tags: json(r, 8)?,
        confirmed: r.get(9)?,
        error: r.get(10)?,
        added_at: r.get(11)?,
    })
}

/// Every item, in path order.
pub fn items(conn: &Connection) -> Result<Vec<Item>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ITEM_COLUMNS} FROM import_items ORDER BY path"
    ))?;
    let rows = stmt.query_map([], item_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn item(conn: &Connection, id: i64) -> Result<Option<Item>> {
    Ok(conn
        .query_row(
            &format!("SELECT {ITEM_COLUMNS} FROM import_items WHERE id = ?1"),
            [id],
            item_from_row,
        )
        .optional()?)
}

fn to_json<T: Serialize>(v: Option<&T>) -> Option<String> {
    v.map(|v| serde_json::to_string(v).expect("plain data always serializes"))
}

/// Stores autotag's lookup answer; a successful lookup clears the error.
pub fn set_lookup(conn: &Connection, id: i64, lookup: &str) -> Result<()> {
    conn.execute(
        "UPDATE import_items SET lookup = ?2, error = NULL WHERE id = ?1",
        params![id, lookup],
    )?;
    Ok(())
}

pub fn set_error(conn: &Connection, id: i64, error: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE import_items SET error = ?2 WHERE id = ?1",
        params![id, error],
    )?;
    Ok(())
}

/// The candidate chosen; none goes back to the first.
pub fn set_choice(conn: &Connection, id: i64, choice: Option<usize>) -> Result<()> {
    conn.execute(
        "UPDATE import_items SET choice = ?2 WHERE id = ?1",
        params![id, choice.map(|c| c as i64)],
    )?;
    Ok(())
}

/// Tags typed in Type tags; none goes back to the candidate's.
pub fn set_tags(conn: &Connection, id: i64, tags: Option<&Fields>) -> Result<()> {
    conn.execute(
        "UPDATE import_items SET tags = ?2 WHERE id = ?1",
        params![id, to_json(tags)],
    )?;
    Ok(())
}

pub fn set_confirmed(conn: &Connection, id: i64, confirmed: bool) -> Result<()> {
    conn.execute(
        "UPDATE import_items SET confirmed = ?2 WHERE id = ?1",
        params![id, confirmed],
    )?;
    Ok(())
}

/// Search again, or Look up again with none: the file starts over. Its
/// lookup, choice, typed tags, accepted duplicate and error go, and it
/// waits to be looked up.
pub fn look_up_again(conn: &Connection, id: i64, search: Option<&Query>) -> Result<()> {
    conn.execute(
        "UPDATE import_items SET search = ?2, lookup = NULL, choice = NULL, tags = NULL, \
         confirmed = 0, error = NULL WHERE id = ?1",
        params![id, to_json(search)],
    )?;
    Ok(())
}

/// Puts a candidate's full tags, from autotag's `tags`, in the stored
/// lookup: what Use this match does for a candidate whose genre is not
/// known yet.
pub fn set_candidate_tags(
    conn: &Connection,
    id: i64,
    candidate: usize,
    tags: &Fields,
) -> Result<()> {
    let Some(item) = item(conn, id)? else {
        return Ok(());
    };
    let Some(text) = item.lookup else {
        return Ok(());
    };
    let mut value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| Error::Autotag(format!("the stored lookup cannot be read ({e})")))?;
    let Some(c) = value["candidates"].get_mut(candidate) else {
        return Ok(());
    };
    c["tags"] = serde_json::to_value(tags).expect("plain data always serializes");
    set_lookup(conn, id, &value.to_string())
}

// -- The inbox --------------------------------------------------------------

fn mtime_ns(m: &std::fs::Metadata) -> i64 {
    m.mtime() * 1_000_000_000 + m.mtime_nsec()
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// The inbox folder, created when missing.
pub fn inbox_dir(cfg: &Config) -> Result<PathBuf> {
    let dir = cfg.inbox().ok_or_else(|| {
        Error::NotWritable("there is no home folder for the default inbox; set import.inbox".into())
    })?;
    if cfg.inbox_overlaps(&dir) {
        return Err(Error::NotWritable(format!(
            "the inbox {} is inside a library root or holds one; set import.inbox to a \
             folder outside the library",
            dir.display()
        )));
    }
    std::fs::create_dir_all(&dir).at(&dir)?;
    Ok(dir)
}

/// Reads the inbox: makes a row for each new audio file, starts over for a
/// file that changed, drops the rows of files gone, and reads every file's
/// tags.
pub fn read_inbox(conn: &Connection, cfg: &Config) -> Result<Inbox> {
    let dir = inbox_dir(cfg)?;
    let mut entries = Vec::new();
    let mut unreadable = Vec::new();
    let walker = walkdir::WalkDir::new(&dir)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !e.file_name().as_encoded_bytes().starts_with(b"."));
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                let path = e.path().unwrap_or(&dir).to_path_buf();
                unreadable.push((path, e.to_string()));
                continue;
            }
        };
        if entry.depth() > 0 && entry.path_is_symlink() {
            unreadable.push((entry.into_path(), "symbolic link, not followed".into()));
            continue;
        }
        if entry.file_type().is_file() {
            entries.push(entry.into_path());
        }
    }
    let known: HashMap<PathBuf, Item> = items(conn)?
        .into_iter()
        .map(|i| (i.path.clone(), i))
        .collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in entries {
        let ext = extension(&path);
        if !tags::AUDIO_EXTENSIONS.contains(&ext.as_str()) {
            let why = match ext.as_str() {
                "" => "not an audio file".to_owned(),
                e => format!("{e} files cannot be imported"),
            };
            out.push(Entry {
                path,
                item: None,
                file: Err(ReadError::Unsupported(why)),
            });
            continue;
        }
        let Some(text) = path.to_str() else {
            out.push(Entry {
                path,
                item: None,
                file: Err(ReadError::Unsupported("its name is not UTF-8".into())),
            });
            continue;
        };
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                unreadable.push((path, e.to_string()));
                continue;
            }
        };
        let (size, mtime) = (meta.size(), mtime_ns(&meta));
        let item = match known.get(&path) {
            Some(i) if i.size == size && i.mtime_ns == mtime => i.clone(),
            old => {
                let hash = match std::fs::File::open(&path).and_then(scan::hash_reader) {
                    Ok(h) => h,
                    Err(e) => {
                        unreadable.push((path, e.to_string()));
                        continue;
                    }
                };
                let id = match old {
                    // Changed: what was looked up and chosen is for another file.
                    Some(i) => {
                        conn.execute(
                            "UPDATE import_items SET size = ?2, mtime_ns = ?3, hash = ?4, \
                             lookup = NULL, search = NULL, choice = NULL, tags = NULL, \
                             confirmed = 0, error = NULL WHERE id = ?1",
                            params![i.id, size as i64, mtime, &hash[..]],
                        )?;
                        i.id
                    }
                    None => {
                        conn.execute(
                            "INSERT INTO import_items (path, size, mtime_ns, hash, added_at) \
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![text, size as i64, mtime, &hash[..], now_unix()],
                        )?;
                        conn.last_insert_rowid()
                    }
                };
                item(conn, id)?.expect("just written")
            }
        };
        seen.insert(path.clone());
        let file = scan::without_panics(|| tags::read(&path));
        out.push(Entry {
            path,
            item: Some(item),
            file,
        });
    }
    // A file under a folder that could not be read may still be there.
    for (path, i) in &known {
        if !seen.contains(path) && !unreadable.iter().any(|(u, _)| path.starts_with(u)) {
            conn.execute("DELETE FROM import_items WHERE id = ?1", [i.id])?;
        }
    }
    Ok(Inbox {
        dir,
        entries: out,
        unreadable,
    })
}

/// Copies files and folders into the inbox, through a hidden temporary file
/// each, never replacing one: a name taken gets " (2)", " (3)" before its
/// extension. A folder keeps its name and brings its audio files, hidden
/// ones left out. Files already in the inbox stay where they are. Returns
/// the inbox paths.
pub fn add_to_inbox(cfg: &Config, sources: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let dir = inbox_dir(cfg)?;
    let real_dir = std::fs::canonicalize(&dir).at(&dir)?;
    let mut out = Vec::new();
    for src in sources {
        let real = std::fs::canonicalize(src).at(src)?;
        if real.starts_with(&real_dir) {
            out.push(src.clone());
            continue;
        }
        let name = src.file_name().ok_or_else(|| Error::Io {
            path: src.clone(),
            source: std::io::Error::other("no file name"),
        })?;
        if real.is_dir() {
            for entry in walkdir::WalkDir::new(&real)
                .sort_by_file_name()
                .into_iter()
                // A folder that holds the inbox does not bring the inbox
                // back into itself.
                .filter_entry(|e| {
                    e.depth() == 0
                        || !(e.file_name().as_encoded_bytes().starts_with(b".")
                            || e.path().starts_with(&real_dir))
                })
            {
                let entry = entry.map_err(|e| Error::Io {
                    path: e.path().unwrap_or(&real).to_path_buf(),
                    source: e.into(),
                })?;
                let ext = extension(entry.path());
                let audio = tags::AUDIO_EXTENSIONS.contains(&ext.as_str())
                    || tags::UNSUPPORTED_EXTENSIONS.contains(&ext.as_str());
                if !entry.file_type().is_file() || !audio {
                    continue;
                }
                let rel = entry
                    .path()
                    .strip_prefix(&real)
                    .expect("under the folder walked");
                out.push(copy_new(entry.path(), &dir.join(name).join(rel))?);
            }
        } else {
            out.push(copy_new(&real, &dir.join(name))?);
        }
    }
    Ok(out)
}

/// `dest` with " (n)" before its extension.
fn numbered(dest: &Path, n: u32) -> PathBuf {
    let stem = dest.file_stem().unwrap_or_default().to_string_lossy();
    let name = match dest.extension() {
        Some(ext) => format!("{stem} ({n}).{}", ext.to_string_lossy()),
        None => format!("{stem} ({n})"),
    };
    dest.with_file_name(name)
}

fn copy_new(src: &Path, dest: &Path) -> Result<PathBuf> {
    let parent = dest.parent().expect("inside the inbox");
    std::fs::create_dir_all(parent).at(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".iprune-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .at(parent)?;
    let mut from = std::fs::File::open(src).at(src)?;
    std::io::copy(&mut from, tmp.as_file_mut()).at(src)?;
    tmp.as_file().sync_all().at(tmp.path())?;
    let mut n = 1;
    loop {
        let candidate = if n == 1 {
            dest.to_path_buf()
        } else {
            numbered(dest, n)
        };
        match tmp.persist_noclobber(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                tmp = e.file;
                n += 1;
            }
            Err(e) => return Err(e.error).at(&candidate),
        }
    }
}

// -- Status -----------------------------------------------------------------

/// The present library tracks, keyed the ways an inbox file can match one.
#[derive(Debug, Default)]
pub struct Index {
    by_hash: HashMap<[u8; 32], i64>,
    by_audio: HashMap<AudioKey, Vec<i64>>,
    by_song: HashMap<(String, String), Vec<i64>>,
    paths: HashMap<i64, PathBuf>,
}

/// The title and album artist a duplicate is looked for by: album artist,
/// else artist, as autotag files a song.
fn song_key(t: &Tags) -> Option<(String, String)> {
    let title = normalize(t.title.as_deref());
    if title.is_empty() {
        return None;
    }
    let artist = normalize(t.album_artist.as_deref().or(t.artist.as_deref()));
    Some((title, artist))
}

impl Index {
    pub fn new(tracks: &[TrackRow]) -> Index {
        let mut index = Index::default();
        for t in tracks.iter().filter(|t| t.missing_since.is_none()) {
            index.by_hash.insert(t.hash, t.id);
            if let Some(k) = scan::audio_key(&t.info) {
                index.by_audio.entry(k).or_default().push(t.id);
            }
            if let Some(k) = song_key(&t.tags) {
                index.by_song.entry(k).or_default().push(t.id);
            }
            index.paths.insert(t.id, t.path.clone());
        }
        index
    }

    pub fn load(conn: &Connection) -> Result<Index> {
        Ok(Index::new(&store::all_tracks(conn)?))
    }

    /// A present track's path.
    pub fn path(&self, track: i64) -> Option<&Path> {
        self.paths.get(&track).map(PathBuf::as_path)
    }

    /// Whether a file identical to this one is in the library.
    pub fn holds(&self, hash: &[u8; 32]) -> bool {
        self.by_hash.contains_key(hash)
    }

    /// Present tracks with the same audio, or the same title under the same
    /// album artist, sorted.
    fn similar(&self, file: &FileTags, tags: &Tags) -> Vec<i64> {
        let mut out: Vec<i64> = scan::audio_key(&file.info)
            .and_then(|k| self.by_audio.get(&k))
            .into_iter()
            .chain(song_key(tags).and_then(|k| self.by_song.get(&k)))
            .flatten()
            .copied()
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Not an audio file iprune can import, or a protected one.
    NotSupported(String),
    /// The file cannot be read, or its lookup failed.
    Error(String),
    /// Not looked up yet.
    LookingUp,
    /// No candidate and no typed tags.
    NoMatch,
    /// Waits for the user to choose a candidate or type the tags.
    Choose,
    Ready,
    /// A file identical to it is in the library; never filed.
    AlreadyInLibrary(i64),
    /// It may be a song the library holds; filed once the user accepts it.
    PossibleDuplicate,
}

/// An inbox file as the window and `iprune import` show it.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub status: Status,
    pub lookup: Option<Lookup>,
    /// The candidate it takes; none for typed tags or without a match.
    pub candidate: Option<usize>,
    /// What iprune sends autotag: the typed tags, else the candidate's.
    pub fields: Option<Fields>,
    /// The tags it would have once filed.
    pub tags: Option<Tags>,
    /// Relative to the library root, as autotag computed it for the
    /// candidate; none for typed tags.
    pub destination: Option<String>,
    /// Present library tracks it may duplicate.
    pub duplicates: Vec<i64>,
    /// Files of the same title autotag found under the artist's folder.
    pub same_title: Vec<String>,
    /// The last lookup or filing error.
    pub error: Option<String>,
}

impl View {
    fn only(status: Status) -> View {
        View {
            status,
            lookup: None,
            candidate: None,
            fields: None,
            tags: None,
            destination: None,
            duplicates: Vec::new(),
            same_title: Vec::new(),
            error: None,
        }
    }

    /// The recording and release written with the tags: the candidate's.
    pub fn ids(&self) -> Option<(&str, &str)> {
        let c = &self.lookup.as_ref()?.candidates[self.candidate?];
        Some((&c.recording, &c.release))
    }
}

/// The status of an inbox entry, the tags it would get and where. Pure:
/// the same entry and index give the same view.
pub fn view(entry: &Entry, index: &Index) -> View {
    let file = match &entry.file {
        Ok(f) => f,
        Err(ReadError::Unsupported(why)) => return View::only(Status::NotSupported(why.clone())),
        Err(ReadError::Unreadable(why)) => return View::only(Status::Error(why.clone())),
    };
    let Some(item) = &entry.item else {
        return View::only(Status::NotSupported("cannot be imported".into()));
    };
    if file.drm {
        return View::only(Status::NotSupported("FairPlay protected".into()));
    }
    if let Some(&track) = index.by_hash.get(&item.hash) {
        let mut v = View::only(Status::AlreadyInLibrary(track));
        v.duplicates = vec![track];
        return v;
    }
    let Some(text) = &item.lookup else {
        return View::only(match &item.error {
            Some(e) => Status::Error(e.clone()),
            None => Status::LookingUp,
        });
    };
    let lookup = match Lookup::parse(text) {
        Ok(l) => l,
        Err(e) => {
            return View::only(Status::Error(format!(
                "the stored lookup cannot be read ({e}); look it up again"
            )));
        }
    };
    let chosen = item.choice.filter(|&c| c < lookup.candidates.len());
    let candidate = match &item.tags {
        Some(_) => None,
        None => chosen.or((!lookup.candidates.is_empty()).then_some(0)),
    };
    let mut v = View::only(Status::NoMatch);
    v.error = item.error.clone();
    let fields = match (&item.tags, candidate) {
        (Some(t), _) => t.clone(),
        (None, Some(c)) => lookup.candidates[c].tags.clone(),
        (None, None) => {
            v.lookup = Some(lookup);
            return v;
        }
    };
    let tags = fields.over(&file.tags);
    if let Some(c) = candidate {
        v.destination = lookup.candidates[c].destination.clone();
        v.same_title = lookup.candidates[c].same_title.clone();
    }
    v.duplicates = index.similar(file, &tags);
    let ready = item.tags.is_some()
        || chosen.is_some()
        || candidate.is_some_and(|c| lookup.candidates[c].certain);
    v.status = if (!v.duplicates.is_empty() || !v.same_title.is_empty()) && !item.confirmed {
        Status::PossibleDuplicate
    } else if ready {
        Status::Ready
    } else {
        Status::Choose
    };
    v.candidate = candidate;
    v.fields = Some(fields);
    v.tags = Some(tags);
    v.lookup = Some(lookup);
    v
}

// -- Lookups ----------------------------------------------------------------

/// The entries waiting for a lookup: readable audio not in the library,
/// with no lookup yet, or a stored one that no longer reads; with `retry`,
/// those whose lookup failed too.
pub fn pending<'a>(inbox: &'a Inbox, index: &Index, retry: bool) -> Vec<&'a Entry> {
    inbox
        .entries
        .iter()
        .filter(|e| {
            let (Some(item), Ok(file)) = (&e.item, &e.file) else {
                return false;
            };
            let unread = item
                .lookup
                .as_deref()
                .is_none_or(|text| Lookup::parse(text).is_err());
            unread && (retry || item.error.is_none()) && !file.drm && !index.holds(&item.hash)
        })
        .collect()
}

/// Looks each entry up, in order, and stores the answer or the error.
/// Stops between files when `stop` is set. An error is autotag failing;
/// what was looked up until then is kept.
pub fn look_up(
    conn: &Connection,
    entries: &[&Entry],
    autotag: &mut Autotag,
    progress: &mut dyn FnMut(usize, usize, &Path),
    stop: &AtomicBool,
) -> Result<usize> {
    let mut done = 0;
    for (n, entry) in entries.iter().enumerate() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Some(item) = &entry.item else { continue };
        progress(n, entries.len(), &entry.path);
        match autotag.lookup(&entry.path, item.search.as_ref())? {
            Ok((_, text)) => set_lookup(conn, item.id, &text)?,
            Err(e) => set_error(conn, item.id, Some(&e))?,
        }
        done += 1;
    }
    Ok(done)
}

// -- Filing -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Filed {
    /// Where it was in the inbox.
    pub from: PathBuf,
    /// Where it is now in the library.
    pub to: PathBuf,
    /// Its track, once the scan indexed it.
    pub track: Option<i64>,
}

#[derive(Debug, Default)]
pub struct FileReport {
    pub filed: Vec<Filed>,
    /// Ready files not filed, with why: refused, failed, or changed.
    pub failed: Vec<(PathBuf, String)>,
    /// Filed songs whose tags or audio read back otherwise than sent.
    pub mismatches: Vec<(PathBuf, Vec<&'static str>)>,
    /// autotag stopped part way; the files after it were not tried.
    pub error: Option<String>,
    /// The scan after filing, or why it failed.
    pub scan: Option<std::result::Result<ScanReport, String>>,
    /// Filed songs the scan did not index.
    pub not_indexed: Vec<PathBuf>,
    /// The playlist the songs were added to, or why they were not.
    pub playlist: Option<std::result::Result<String, String>>,
    /// Stopped between files.
    pub stopped: bool,
}

/// The library must be all there: a scan refuses a missing root, and songs
/// filed without one would not be indexed.
pub fn check_roots(cfg: &Config) -> Result<PathBuf> {
    for r in &cfg.library.roots {
        if !r.is_dir() {
            return Err(Error::RootMissing(r.clone()));
        }
    }
    let root = cfg
        .import_root()
        .ok_or_else(|| Error::NotWritable("no library root to file into".into()))?;
    if !std::fs::read_dir(root).is_ok_and(|mut d| d.next().is_some()) {
        return Err(Error::RootMissing(root.to_owned()));
    }
    Ok(root.to_owned())
}

/// What a filed file must read back as: the fields sent, and the same kind
/// and duration as its inbox copy.
fn read_back(sent: &Fields, before: &FileTags, after: &FileTags) -> Vec<&'static str> {
    let mut out = sent.differences(&after.tags);
    if after.info.kind != before.info.kind {
        out.push("kind");
    }
    if let (Some(a), Some(b)) = (after.info.duration_ms, before.info.duration_ms)
        && a.abs_diff(b) > DURATION_SLACK_MS
    {
        out.push("duration");
    }
    out
}

/// Files the Ready songs of the inbox, in order, then scans and adds them
/// to `playlist`. Stops between files when `stop` is set.
pub fn file(
    store: &mut Store,
    cfg: &Config,
    autotag: &mut Autotag,
    playlist: Option<&str>,
    progress: &mut dyn FnMut(usize, usize, &Path),
    stop: &AtomicBool,
) -> Result<FileReport> {
    let root = check_roots(cfg)?;
    if let Some(name) = playlist {
        playlists::show(store.conn(), cfg, name)?;
    }
    let inbox = read_inbox(store.conn(), cfg)?;
    let index = Index::load(store.conn())?;
    let ready: Vec<(&Entry, View)> = inbox
        .entries
        .iter()
        .map(|e| (e, view(e, &index)))
        .filter(|(_, v)| v.status == Status::Ready)
        .collect();
    let mut report = FileReport::default();
    for (n, (entry, v)) in ready.iter().enumerate() {
        if stop.load(Ordering::SeqCst) {
            report.stopped = true;
            break;
        }
        let (Some(item), Ok(before), Some(fields)) = (&entry.item, &entry.file, &v.fields) else {
            continue;
        };
        progress(n, ready.len(), &entry.path);
        let unchanged = std::fs::metadata(&entry.path)
            .is_ok_and(|m| m.size() == item.size && mtime_ns(&m) == item.mtime_ns);
        if !unchanged {
            let why = Error::InboxChanged(entry.path.clone()).to_string();
            report.failed.push((entry.path.clone(), why));
            continue;
        }
        let applied = match autotag.apply(&entry.path, fields, v.ids(), false, item.confirmed) {
            Ok(a) => a,
            Err(e) => {
                // autotag is gone: the inbox file is as it was.
                report.error = Some(e.to_string());
                break;
            }
        };
        match applied {
            Ok(Applied::Filed { destination }) => {
                let to = root.join(&destination);
                conn_delete(store.conn(), item.id)?;
                match scan::without_panics(|| tags::read(&to)) {
                    Ok(after) => {
                        let diff = read_back(fields, before, &after);
                        if !diff.is_empty() {
                            report.mismatches.push((to.clone(), diff));
                        }
                    }
                    Err(_) => report.mismatches.push((to.clone(), vec!["unreadable"])),
                }
                report.filed.push(Filed {
                    from: entry.path.clone(),
                    to,
                    track: None,
                });
            }
            Ok(Applied::Refused { reason, .. }) => {
                set_error(store.conn(), item.id, Some(&reason))?;
                report.failed.push((entry.path.clone(), reason));
            }
            Ok(Applied::WouldFile { .. }) => {
                let why = "autotag did not file it";
                set_error(store.conn(), item.id, Some(why))?;
                report.failed.push((entry.path.clone(), why.into()));
            }
            Err(e) => {
                set_error(store.conn(), item.id, Some(&e))?;
                report.failed.push((entry.path.clone(), e));
            }
        }
    }
    // autotag may have filed a song and died before answering: scan then
    // too, so it is indexed.
    if report.filed.is_empty() && report.error.is_none() {
        return Ok(report);
    }
    report.scan = Some(scan::scan(store, cfg, &|_| {}).map_err(|e| e.to_string()));
    let mut added = Vec::new();
    for f in &mut report.filed {
        f.track = match paths::key(&f.to) {
            Some(k) => store::present_track_by_key(store.conn(), &k)?.map(|t| t.id),
            None => None,
        };
        match f.track {
            Some(_) => added.push(f.to.clone()),
            None => report.not_indexed.push(f.to.clone()),
        }
    }
    if let Some(name) = playlist
        && !added.is_empty()
    {
        report.playlist = Some(
            playlists::add(store.conn(), cfg, name, &added, None)
                .map(|()| name.to_owned())
                .map_err(|e| e.to_string()),
        );
    }
    Ok(report)
}

fn conn_delete(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM import_items WHERE id = ?1", [id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags::{AudioInfo, FileKind};

    fn file(kind: FileKind, ms: Option<u32>, tags: Tags) -> FileTags {
        FileTags {
            info: AudioInfo {
                kind,
                duration_ms: ms,
                bitrate: None,
                samplerate: None,
            },
            tags,
            tag_error: None,
            drm: false,
        }
    }

    fn sent() -> Fields {
        Fields {
            title: Some("T".into()),
            year: Some(1999),
            ..Fields::default()
        }
    }

    fn read(title: &str, year: u32) -> Tags {
        Tags {
            title: Some(title.into()),
            year: Some(year),
            ..Tags::default()
        }
    }

    #[test]
    fn a_song_that_reads_back_as_sent_with_the_same_kind_and_duration_has_no_difference() {
        let before = file(FileKind::Mp3, Some(200_000), Tags::default());
        let after = file(FileKind::Mp3, Some(200_000), read("T", 1999));
        assert!(read_back(&sent(), &before, &after).is_empty());
    }

    #[test]
    fn read_back_names_fields_then_kind_then_duration() {
        let before = file(FileKind::Mp3, Some(200_000), Tags::default());
        let after = file(FileKind::Aac, Some(300_000), read("Other", 1999));
        assert_eq!(
            read_back(&sent(), &before, &after),
            vec!["title", "kind", "duration"]
        );
    }

    #[test]
    fn the_duration_may_move_by_fifty_milliseconds_and_no_more() {
        let before = file(FileKind::Mp3, Some(200_000), Tags::default());
        for (ms, differs) in [
            (200_050, false),
            (199_950, false),
            (200_051, true),
            (199_949, true),
        ] {
            let after = file(FileKind::Mp3, Some(ms), read("T", 1999));
            assert_eq!(
                read_back(&sent(), &before, &after),
                if differs { vec!["duration"] } else { vec![] },
                "{ms}"
            );
        }
    }

    #[test]
    fn an_unknown_duration_on_either_side_is_not_a_difference() {
        let known = file(FileKind::Mp3, Some(200_000), Tags::default());
        let unknown = file(FileKind::Mp3, None, read("T", 1999));
        assert!(read_back(&sent(), &known, &unknown).is_empty());
        let after = file(FileKind::Mp3, Some(9), read("T", 1999));
        assert!(read_back(&sent(), &unknown, &after).is_empty());
    }
}
