//! Playlist operations. The `.m3u8` file is the source of truth: every
//! operation reads it, changes it, writes it back atomically and then
//! refreshes the copy in the index.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::config::{Config, PathStyle};
use crate::error::IoContext;
use crate::playlist::{self, Entry, WriteMode, mtime_ns};
use crate::store::{self, TrackRow};
use crate::{Error, Result, paths};

const EXT: &str = ".m3u8";

/// One playlist entry with the track it resolves to, if any.
#[derive(Debug, Clone)]
pub struct Item {
    pub entry: Entry,
    pub track: Option<TrackRow>,
}

#[derive(Debug, Default)]
pub struct LoadReport {
    /// Playlists read from disk in this pass.
    pub loaded: usize,
    /// `(playlist, entry)` for every entry that matched no library track.
    pub unresolved: Vec<(String, String)>,
    /// Playlists whose file is gone, dropped from the index.
    pub removed: Vec<String>,
    /// Files that could not be read, with the reason.
    pub skipped: Vec<(PathBuf, String)>,
}

fn validate_name(name: &str) -> Result<()> {
    let bad = |why| Err(Error::InvalidName(name.to_owned(), why));
    if name.trim().is_empty() {
        return bad("empty");
    }
    if name.contains(['/', '\0', '\n', '\r']) {
        return bad("contains '/', a NUL or a line break");
    }
    if name.starts_with('.') {
        return bad("starts with '.'");
    }
    Ok(())
}

pub fn file_for(cfg: &Config, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(cfg.playlists.dir.join(format!("{name}{EXT}")))
}

fn existing_file(cfg: &Config, name: &str) -> Result<PathBuf> {
    let file = file_for(cfg, name)?;
    if !file.is_file() {
        return Err(Error::NoSuchPlaylist(name.to_owned()));
    }
    Ok(file)
}

/// `(name, path)` of every playlist file in `dir`, sorted by name. Hidden files
/// (including iprune's own temporary files) and non-UTF-8 names are ignored.
fn playlist_files(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .at(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let stem = name
                .strip_suffix(EXT)
                .filter(|s| !s.starts_with('.') && !s.is_empty())?;
            Some((stem.to_owned(), e.path()))
        })
        .filter(|(_, p)| p.is_file())
        .collect();
    out.sort();
    Ok(out)
}

fn resolve(conn: &Connection, cfg: &Config, entries: Vec<Entry>) -> Result<Vec<Item>> {
    entries
        .into_iter()
        .map(|entry| {
            let abs = paths::absolute(Path::new(&entry.location), &cfg.playlists.dir);
            let track = match paths::key(&abs) {
                Some(k) => store::present_track_by_key(conn, &k)?,
                None => None,
            };
            Ok(Item { entry, track })
        })
        .collect()
}

/// The location line for a track, in the configured style.
fn location(cfg: &Config, t: &TrackRow) -> String {
    let p = match cfg.playlists.path_style {
        PathStyle::Absolute => t.path.clone(),
        PathStyle::Relative => playlist::relative_to(&t.path, &cfg.playlists.dir),
    };
    p.to_str().expect("indexed paths are UTF-8").to_owned()
}

fn render_item(cfg: &Config, item: &Item) -> Entry {
    match &item.track {
        None => item.entry.clone(),
        Some(t) => Entry {
            location: location(cfg, t),
            extinf: Some(playlist::extinf(
                t.info.duration_ms,
                t.tags.artist.as_deref(),
                &t.display_title(),
            )),
        },
    }
}

fn store_items(conn: &Connection, name: &str, file: &Path, items: &[Item]) -> Result<()> {
    let rows: Vec<_> = items
        .iter()
        .map(|i| (i.track.as_ref().map(|t| t.id), i.entry.location.clone()))
        .collect();
    store::put_playlist(conn, name, file, mtime_ns(file)?, &rows)?;
    Ok(())
}

fn write(
    conn: &Connection,
    cfg: &Config,
    name: &str,
    file: &Path,
    items: &mut [Item],
    mode: WriteMode,
) -> Result<()> {
    for item in items.iter_mut() {
        item.entry = render_item(cfg, item);
    }
    let entries: Vec<Entry> = items.iter().map(|i| i.entry.clone()).collect();
    playlist::write_atomic(file, &playlist::render(&entries), mode)?;
    store_items(conn, name, file, items)
}

/// Reads the playlist, lets `f` change it, writes it back and updates the index.
fn edit(
    conn: &Connection,
    cfg: &Config,
    name: &str,
    f: impl FnOnce(&mut Vec<Item>) -> Result<()>,
) -> Result<()> {
    let file = existing_file(cfg, name)?;
    let mtime = mtime_ns(&file)?;
    let mut items = resolve(conn, cfg, playlist::read(&file)?)?;
    f(&mut items)?;
    write(
        conn,
        cfg,
        name,
        &file,
        &mut items,
        WriteMode::Replace { mtime_ns: mtime },
    )
}

/// Brings the index in line with the playlists folder. Files whose mtime
/// matches the index are skipped unless `force` is set.
pub fn sync_from_disk(conn: &Connection, cfg: &Config, force: bool) -> Result<LoadReport> {
    // One savepoint: all or nothing, and one commit instead of one per entry.
    store::savepoint(conn, || load_from_disk(conn, cfg, force))
}

fn load_from_disk(conn: &Connection, cfg: &Config, force: bool) -> Result<LoadReport> {
    let dir = &cfg.playlists.dir;
    let known: HashMap<String, i64> = store::playlists(conn)?
        .into_iter()
        .map(|p| (p.name, p.file_mtime_ns))
        .collect();
    let mut report = LoadReport::default();
    let mut seen = HashSet::new();
    for (name, file) in playlist_files(dir)? {
        let mtime = mtime_ns(&file)?;
        seen.insert(name.clone());
        if !force && known.get(&name) == Some(&mtime) {
            continue;
        }
        let entries = match playlist::read(&file) {
            Ok(e) => e,
            Err(e) => {
                report.skipped.push((file.clone(), e.to_string()));
                continue;
            }
        };
        let items = resolve(conn, cfg, entries)?;
        for i in items.iter().filter(|i| i.track.is_none()) {
            report
                .unresolved
                .push((name.clone(), i.entry.location.clone()));
        }
        store_items(conn, &name, &file, &items)?;
        report.loaded += 1;
    }
    for name in known.keys().filter(|n| !seen.contains(*n)) {
        store::delete_playlist(conn, name)?;
        report.removed.push(name.clone());
    }
    report.removed.sort();
    Ok(report)
}

/// Rewrites every playlist that refers to a moved file so it points at the
/// file's new location. `moves` maps the old path's lookup key to the track,
/// already updated in `conn`. Returns the names of the playlists rewritten.
pub fn rewrite_moved(
    conn: &Connection,
    cfg: &Config,
    moves: &HashMap<String, i64>,
) -> Result<Vec<String>> {
    if moves.is_empty() {
        return Ok(Vec::new());
    }
    let mut rewritten = Vec::new();
    let dir = &cfg.playlists.dir;
    for (name, file) in playlist_files(dir)? {
        let mtime = mtime_ns(&file)?;
        // An unreadable playlist is reported by the reload that follows.
        let Ok(entries) = playlist::read(&file) else {
            continue;
        };
        let mut changed = false;
        let mut items = resolve(conn, cfg, entries)?;
        for item in items.iter_mut().filter(|i| i.track.is_none()) {
            let abs = paths::absolute(Path::new(&item.entry.location), dir);
            if let Some(&id) = paths::key(&abs).as_ref().and_then(|k| moves.get(k)) {
                item.track = store::track(conn, id)?;
                changed = true;
            }
        }
        if changed {
            write(
                conn,
                cfg,
                &name,
                &file,
                &mut items,
                WriteMode::Replace { mtime_ns: mtime },
            )?;
            rewritten.push(name);
        }
    }
    Ok(rewritten)
}

pub fn show(conn: &Connection, cfg: &Config, name: &str) -> Result<Vec<Item>> {
    let file = existing_file(cfg, name)?;
    resolve(conn, cfg, playlist::read(&file)?)
}

pub fn create(conn: &Connection, cfg: &Config, name: &str) -> Result<()> {
    let file = file_for(cfg, name)?;
    match write(conn, cfg, name, &file, &mut [], WriteMode::CreateNew) {
        Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(Error::PlaylistExists(name.to_owned()))
        }
        r => r,
    }
}

pub fn delete(conn: &Connection, cfg: &Config, name: &str) -> Result<()> {
    let file = existing_file(cfg, name)?;
    std::fs::remove_file(&file).at(&file)?;
    store::delete_playlist(conn, name)
}

pub fn rename(conn: &Connection, cfg: &Config, old: &str, new: &str) -> Result<()> {
    let from = existing_file(cfg, old)?;
    let to = file_for(cfg, new)?;
    // A hard link fails if `to` exists, where a rename would replace it.
    match std::fs::hard_link(&from, &to) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Error::PlaylistExists(new.to_owned()));
        }
        r => r.at(&to)?,
    }
    std::fs::remove_file(&from).at(&from)?;
    // Keep the row id so the iPod playlist it maps to is renamed, not replaced.
    store::rename_playlist(conn, old, new, &to)?;
    let items = resolve(conn, cfg, playlist::read(&to)?)?;
    store_items(conn, new, &to, &items)
}

fn check_position(position: usize, len: usize) -> Result<usize> {
    if position == 0 || position > len {
        return Err(Error::BadPosition { position, len });
    }
    Ok(position - 1)
}

/// Adds library files to a playlist, at 1-based `at` or at the end. Every
/// file must already be in the index; nothing is written otherwise.
pub fn add(
    conn: &Connection,
    cfg: &Config,
    name: &str,
    files: &[PathBuf],
    at: Option<usize>,
) -> Result<()> {
    let cwd = std::env::current_dir().at(".")?;
    let mut tracks = Vec::with_capacity(files.len());
    for f in files {
        let abs = paths::absolute(f, &cwd);
        let track = match paths::key(&abs) {
            Some(k) => store::present_track_by_key(conn, &k)?,
            None => None,
        };
        tracks.push(track.ok_or_else(|| Error::NotIndexed(abs.clone()))?);
    }
    edit(conn, cfg, name, |items| {
        let at = match at {
            None => items.len(),
            Some(p) => check_position(p, items.len() + 1)?,
        };
        let new = tracks.into_iter().map(|t| Item {
            entry: Entry {
                location: String::new(),
                extinf: None,
            },
            track: Some(t),
        });
        items.splice(at..at, new);
        Ok(())
    })
}

/// Removes the entries at the given 1-based positions.
pub fn remove(conn: &Connection, cfg: &Config, name: &str, positions: &[usize]) -> Result<()> {
    edit(conn, cfg, name, |items| {
        let mut idx = positions
            .iter()
            .map(|&p| check_position(p, items.len()))
            .collect::<Result<Vec<_>>>()?;
        idx.sort_unstable();
        idx.dedup();
        for i in idx.into_iter().rev() {
            items.remove(i);
        }
        Ok(())
    })
}

/// Moves the entry at 1-based `from` so it ends up at 1-based `to`.
pub fn move_entry(
    conn: &Connection,
    cfg: &Config,
    name: &str,
    from: usize,
    to: usize,
) -> Result<()> {
    edit(conn, cfg, name, |items| {
        let (f, t) = (
            check_position(from, items.len())?,
            check_position(to, items.len())?,
        );
        let item = items.remove(f);
        items.insert(t, item);
        Ok(())
    })
}
