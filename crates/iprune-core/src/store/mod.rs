//! The SQLite index. It is a cache of the files: deleting it loses nothing but
//! play statistics, which are also exported.

use std::path::{Path, PathBuf};

pub mod devices;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::error::IoContext;
use crate::tags::{AudioInfo, FileKind, Tags};
use crate::{Error, Result};

/// Numbered migrations, applied in order and tracked with `user_version`.
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/0001_init.sql"),
    include_str!("migrations/0002_tag_state.sql"),
    include_str!("migrations/0003_playlist_tombstones.sql"),
    include_str!("migrations/0004_pending_pulls.sql"),
    include_str!("migrations/0005_stable_ids.sql"),
    include_str!("migrations/0006_synced_rating.sql"),
    include_str!("migrations/0007_tags_version.sql"),
    include_str!("migrations/0008_device_scope.sql"),
];

/// First line of a migration that rebuilds tables: it runs with foreign
/// keys off, which a transaction cannot change, and is checked before it is
/// committed.
const FOREIGN_KEYS_OFF: &str = "-- foreign_keys off";

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).at(dir)?;
        }
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", true)?;
        // The window and its sync worker each hold a connection: wait for
        // the other's write instead of failing at once.
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        let version =
            conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? as usize;
        if version > MIGRATIONS.len() {
            return Err(Error::Db(rusqlite::Error::InvalidParameterName(format!(
                "database schema version {version} is newer than this iprune ({})",
                MIGRATIONS.len()
            ))));
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            let rebuild = sql.starts_with(FOREIGN_KEYS_OFF);
            if rebuild {
                conn.pragma_update(None, "foreign_keys", false)?;
            }
            let done = (|| -> Result<()> {
                let tx = conn.transaction()?;
                tx.execute_batch(sql)?;
                if rebuild {
                    let broken: i64 =
                        tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                            r.get(0)
                        })?;
                    if broken > 0 {
                        return Err(Error::Migration(format!(
                            "migration {} would leave {broken} broken references; the index was \
                             not changed",
                            i + 1
                        )));
                    }
                }
                tx.pragma_update(None, "user_version", (i + 1) as i64)?;
                tx.commit()?;
                Ok(())
            })();
            if rebuild {
                conn.pragma_update(None, "foreign_keys", true)?;
            }
            done?;
        }
        Ok(Store { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn transaction(&mut self) -> Result<rusqlite::Transaction<'_>> {
        Ok(self.conn.transaction()?)
    }
}

/// Runs `f` inside a savepoint: everything it writes is kept or rolled back
/// together. Works inside or outside an open transaction.
pub fn savepoint<T>(conn: &Connection, f: impl FnOnce() -> Result<T>) -> Result<T> {
    conn.execute_batch("SAVEPOINT iprune")?;
    match f() {
        Ok(v) => {
            conn.execute_batch("RELEASE iprune")?;
            Ok(v)
        }
        Err(e) => {
            // If SQLite already rolled back the whole transaction (disk full,
            // I/O error) the savepoint is gone; report the original error.
            let _ = conn.execute_batch("ROLLBACK TO iprune; RELEASE iprune");
            Err(e)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackRow {
    pub id: i64,
    pub path: PathBuf,
    pub path_key: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub hash: [u8; 32],
    pub info: AudioInfo,
    pub tags: Tags,
    pub tags_hash: [u8; 32],
    pub added_at: i64,
    pub missing_since: Option<i64>,
    /// Set when the tags could not be parsed; `tags` is then empty.
    pub tag_error: Option<String>,
    /// FairPlay protected.
    pub drm: bool,
}

impl TrackRow {
    /// The title, or the file name without extension when the tag is empty.
    pub fn display_title(&self) -> std::borrow::Cow<'_, str> {
        match &self.tags.title {
            Some(t) => t.into(),
            None => self
                .path
                .file_stem()
                .map_or("".into(), |s| s.to_string_lossy()),
        }
    }
}

/// What a scan learned about one file.
#[derive(Debug, Clone)]
pub struct FileRecord {
    pub path: PathBuf,
    pub path_key: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub hash: [u8; 32],
    pub info: AudioInfo,
    pub tags: Tags,
    pub tag_error: Option<String>,
    pub drm: bool,
}

const TRACK_COLUMNS: &str = "id, path, path_key, size, mtime_ns, hash, duration_ms, filetype, bitrate, samplerate, \
     title, artist, album, album_artist, composer, genre, year, track_no, track_total, disc_no, disc_total, \
     compilation, tags_hash, added_at, missing_since, tag_error, drm";

fn blob32(row: &Row, idx: usize) -> rusqlite::Result<[u8; 32]> {
    let v: Vec<u8> = row.get(idx)?;
    v.try_into().map_err(|_| {
        rusqlite::Error::InvalidColumnType(idx, "hash".into(), rusqlite::types::Type::Blob)
    })
}

fn track_from_row(r: &Row) -> rusqlite::Result<TrackRow> {
    let filetype: String = r.get(7)?;
    let kind = FileKind::parse(&filetype).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(7, "filetype".into(), rusqlite::types::Type::Text)
    })?;
    Ok(TrackRow {
        id: r.get(0)?,
        path: PathBuf::from(r.get::<_, String>(1)?),
        path_key: r.get(2)?,
        size: r.get::<_, i64>(3)? as u64,
        mtime_ns: r.get(4)?,
        hash: blob32(r, 5)?,
        info: AudioInfo {
            kind,
            duration_ms: r.get(6)?,
            bitrate: r.get(8)?,
            samplerate: r.get(9)?,
        },
        tags: Tags {
            title: r.get(10)?,
            artist: r.get(11)?,
            album: r.get(12)?,
            album_artist: r.get(13)?,
            composer: r.get(14)?,
            genre: r.get(15)?,
            year: r.get(16)?,
            track_no: r.get(17)?,
            track_total: r.get(18)?,
            disc_no: r.get(19)?,
            disc_total: r.get(20)?,
            compilation: r.get(21)?,
        },
        tags_hash: blob32(r, 22)?,
        added_at: r.get(23)?,
        missing_since: r.get(24)?,
        tag_error: r.get(25)?,
        drm: r.get(26)?,
    })
}

fn path_str(p: &Path) -> &str {
    p.to_str().expect("indexed paths are UTF-8")
}

pub fn all_tracks(conn: &Connection) -> Result<Vec<TrackRow>> {
    let mut stmt = conn.prepare(&format!("SELECT {TRACK_COLUMNS} FROM tracks ORDER BY id"))?;
    let rows = stmt
        .query_map([], track_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn track(conn: &Connection, id: i64) -> Result<Option<TrackRow>> {
    let sql = format!("SELECT {TRACK_COLUMNS} FROM tracks WHERE id = ?1");
    Ok(conn.query_row(&sql, [id], track_from_row).optional()?)
}

/// A present (not missing) track by lookup key.
pub fn present_track_by_key(conn: &Connection, key: &str) -> Result<Option<TrackRow>> {
    let sql =
        format!("SELECT {TRACK_COLUMNS} FROM tracks WHERE path_key = ?1 AND missing_since IS NULL");
    Ok(conn.query_row(&sql, [key], track_from_row).optional()?)
}

pub fn insert_track(conn: &Connection, f: &FileRecord, added_at: i64) -> Result<i64> {
    conn.execute(
        "INSERT INTO tracks (path, path_key, size, mtime_ns, hash, duration_ms, filetype, bitrate, samplerate, \
         title, artist, album, album_artist, composer, genre, year, track_no, track_total, disc_no, disc_total, \
         compilation, tags_hash, added_at, tag_error, drm, tags_version) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, \
         ?24, ?25, ?26)",
        params![
            path_str(&f.path),
            f.path_key,
            f.size as i64,
            f.mtime_ns,
            &f.hash[..],
            f.info.duration_ms,
            f.info.kind.as_str(),
            f.info.bitrate,
            f.info.samplerate,
            f.tags.title,
            f.tags.artist,
            f.tags.album,
            f.tags.album_artist,
            f.tags.composer,
            f.tags.genre,
            f.tags.year,
            f.tags.track_no,
            f.tags.track_total,
            f.tags.disc_no,
            f.tags.disc_total,
            f.tags.compilation,
            &f.tags.hash()[..],
            added_at,
            f.tag_error,
            f.drm,
            crate::tags::READER_VERSION,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Replaces everything known about track `id` with `f` and marks it present.
/// The id, and so every mapping to it, is kept.
pub fn update_track(conn: &Connection, id: i64, f: &FileRecord) -> Result<()> {
    conn.execute(
        "UPDATE tracks SET path = ?2, path_key = ?3, size = ?4, mtime_ns = ?5, hash = ?6, duration_ms = ?7, \
         filetype = ?8, bitrate = ?9, samplerate = ?10, title = ?11, artist = ?12, album = ?13, \
         album_artist = ?14, composer = ?15, genre = ?16, year = ?17, track_no = ?18, track_total = ?19, \
         disc_no = ?20, disc_total = ?21, compilation = ?22, tags_hash = ?23, tag_error = ?24, drm = ?25, missing_since = NULL, \
         tags_version = ?26 WHERE id = ?1",
        params![
            id,
            path_str(&f.path),
            f.path_key,
            f.size as i64,
            f.mtime_ns,
            &f.hash[..],
            f.info.duration_ms,
            f.info.kind.as_str(),
            f.info.bitrate,
            f.info.samplerate,
            f.tags.title,
            f.tags.artist,
            f.tags.album,
            f.tags.album_artist,
            f.tags.composer,
            f.tags.genre,
            f.tags.year,
            f.tags.track_no,
            f.tags.track_total,
            f.tags.disc_no,
            f.tags.disc_total,
            f.tags.compilation,
            &f.tags.hash()[..],
            f.tag_error,
            f.drm,
            crate::tags::READER_VERSION,
        ],
    )?;
    Ok(())
}

pub fn set_missing(conn: &Connection, id: i64, since: Option<i64>) -> Result<()> {
    conn.execute(
        "UPDATE tracks SET missing_since = ?2 WHERE id = ?1",
        params![id, since],
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistRow {
    pub id: i64,
    pub name: String,
    pub file: PathBuf,
    pub file_mtime_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistSummary {
    pub name: String,
    pub entries: usize,
    pub unresolved: usize,
}

pub fn playlists(conn: &Connection) -> Result<Vec<PlaylistRow>> {
    let mut stmt =
        conn.prepare("SELECT id, name, file, file_mtime_ns FROM playlists WHERE removed_at IS NULL ORDER BY name")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PlaylistRow {
                id: r.get(0)?,
                name: r.get(1)?,
                file: PathBuf::from(r.get::<_, String>(2)?),
                file_mtime_ns: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Names of the playlists holding the track `track_id`, each once, in the
/// order of [`playlists`].
pub fn playlists_holding(conn: &Connection, track_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT p.name FROM playlists p WHERE p.removed_at IS NULL AND EXISTS \
         (SELECT 1 FROM playlist_items i WHERE i.playlist_id = p.id AND i.track_id = ?1) \
         ORDER BY p.name",
    )?;
    let names = stmt
        .query_map([track_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(names)
}

pub fn playlist_summaries(conn: &Connection) -> Result<Vec<PlaylistSummary>> {
    let mut stmt = conn.prepare(
        "SELECT p.name, COUNT(i.position), COUNT(i.position) - COUNT(i.track_id) \
         FROM playlists p LEFT JOIN playlist_items i ON i.playlist_id = p.id \
         WHERE p.removed_at IS NULL GROUP BY p.id ORDER BY p.name",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PlaylistSummary {
                name: r.get(0)?,
                entries: r.get::<_, i64>(1)? as usize,
                unresolved: r.get::<_, i64>(2)? as usize,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Present tracks the scan reports on every run.
#[derive(Debug, Default)]
pub struct Flagged {
    /// Tags could not be parsed, with the reason.
    pub untagged: Vec<(PathBuf, String)>,
    /// FairPlay protected.
    pub drm: Vec<PathBuf>,
}

pub fn flagged_tracks(conn: &Connection) -> Result<Flagged> {
    let mut f = Flagged::default();
    let mut stmt = conn.prepare(
        "SELECT path, tag_error, drm FROM tracks \
         WHERE missing_since IS NULL AND (tag_error IS NOT NULL OR drm) ORDER BY path",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let path = PathBuf::from(r.get::<_, String>(0)?);
        if let Some(e) = r.get::<_, Option<String>>(1)? {
            f.untagged.push((path.clone(), e));
        }
        if r.get::<_, bool>(2)? {
            f.drm.push(path);
        }
    }
    Ok(f)
}

/// Stores a playlist and its items, keeping the row id if the name exists.
pub fn put_playlist(
    conn: &Connection,
    name: &str,
    file: &Path,
    mtime_ns: i64,
    items: &[(Option<i64>, String)],
) -> Result<i64> {
    savepoint(conn, || {
        put_playlist_rows(conn, name, file, mtime_ns, items)
    })
}

fn put_playlist_rows(
    conn: &Connection,
    name: &str,
    file: &Path,
    mtime_ns: i64,
    items: &[(Option<i64>, String)],
) -> Result<i64> {
    let id: i64 = conn.query_row(
        "INSERT INTO playlists (name, file, file_mtime_ns) VALUES (?1, ?2, ?3) \
         ON CONFLICT(name) DO UPDATE SET file = excluded.file, file_mtime_ns = excluded.file_mtime_ns, \
         removed_at = NULL RETURNING id",
        params![name, path_str(file), mtime_ns],
        |r| r.get(0),
    )?;
    conn.execute("DELETE FROM playlist_items WHERE playlist_id = ?1", [id])?;
    let mut stmt =
        conn.prepare("INSERT INTO playlist_items (playlist_id, position, track_id, raw_entry) VALUES (?1, ?2, ?3, ?4)")?;
    for (pos, (track_id, raw)) in items.iter().enumerate() {
        stmt.execute(params![id, pos as i64, track_id, raw])?;
    }
    Ok(id)
}

/// Renames a playlist, keeping its row id so the iPod playlist it maps to is
/// renamed rather than replaced. A tombstone under the new name is moved
/// aside, keeping its mapping, so the next sync still deletes its iPod
/// playlist.
pub fn rename_playlist(conn: &Connection, old: &str, new: &str, file: &Path) -> Result<()> {
    savepoint(conn, || {
        set_tombstone_aside(conn, new)?;
        conn.execute(
            "UPDATE playlists SET name = ?2, file = ?3 WHERE name = ?1",
            params![old, new, path_str(file)],
        )?;
        // An iPod's selection names the playlist; it follows the rename.
        conn.execute(
            "UPDATE OR REPLACE device_scope_playlists SET name = ?2 WHERE name = ?1",
            params![old, new],
        )?;
        Ok(())
    })
}

/// Gives a tombstone named `name` an internal name and file starting with
/// `.`, which no playlist file can have, freeing `name`.
fn set_tombstone_aside(conn: &Connection, name: &str) -> Result<()> {
    conn.execute(
        "UPDATE playlists SET name = '.removed-' || id, file = '.removed-' || id \
         WHERE name = ?1 AND removed_at IS NOT NULL",
        [name],
    )?;
    Ok(())
}

/// Forgets a playlist whose file is gone. One mapped to an iPod playlist
/// becomes a tombstone, so the next sync can delete the iPod playlist.
pub fn delete_playlist(conn: &Connection, name: &str) -> Result<()> {
    savepoint(conn, || {
        let mapped: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM device_playlists d JOIN playlists p ON p.id = d.playlist_id \
             WHERE p.name = ?1)",
            [name],
            |r| r.get(0),
        )?;
        if mapped {
            conn.execute(
                "DELETE FROM playlist_items WHERE playlist_id = (SELECT id FROM playlists WHERE name = ?1)",
                [name],
            )?;
            conn.execute(
                "UPDATE playlists SET removed_at = ?2 WHERE name = ?1 AND removed_at IS NULL",
                params![name, crate::now_unix()],
            )?;
        } else {
            conn.execute("DELETE FROM playlists WHERE name = ?1", [name])?;
        }
        Ok(())
    })
}

/// Adds a tombstone for a playlist a restore brought back on the iPod whose
/// library playlist is gone, so the next sync deletes it again. It gets
/// `name` when no playlist has it, an internal name otherwise. Returns its id.
pub fn insert_tombstone(conn: &Connection, name: &str) -> Result<i64> {
    let taken: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM playlists WHERE name = ?1)",
        [name],
        |r| r.get(0),
    )?;
    // Inserted under a placeholder, then named after its id.
    let id: i64 = conn.query_row(
        "INSERT INTO playlists (name, file, file_mtime_ns, removed_at) \
         VALUES ('.restoring', '.restoring', 0, ?1) RETURNING id",
        [crate::now_unix()],
        |r| r.get(0),
    )?;
    let (name, file) = match taken {
        true => (format!(".removed-{id}"), format!(".removed-{id}")),
        false => (name.to_owned(), format!(".removed-{id}")),
    };
    conn.execute(
        "UPDATE playlists SET name = ?2, file = ?3 WHERE id = ?1",
        params![id, name, file],
    )?;
    Ok(id)
}

/// Ids of present tracks whose tags an older tag reader read.
pub fn stale_tags(conn: &Connection) -> Result<std::collections::HashSet<i64>> {
    let mut stmt =
        conn.prepare("SELECT id FROM tracks WHERE missing_since IS NULL AND tags_version < ?1")?;
    let ids = stmt
        .query_map([crate::tags::READER_VERSION], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// `(id, name)` of the tombstones: deleted playlists still mapped to an iPod.
pub fn removed_playlists(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt =
        conn.prepare("SELECT id, name FROM playlists WHERE removed_at IS NOT NULL ORDER BY name")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &str) -> FileRecord {
        FileRecord {
            path: path.into(),
            path_key: path.into(),
            size: 10,
            mtime_ns: 5,
            hash: [7; 32],
            info: AudioInfo {
                kind: FileKind::Alac,
                duration_ms: Some(1000),
                bitrate: None,
                samplerate: Some(44100),
            },
            tags: Tags {
                title: Some("T".into()),
                year: Some(1984),
                compilation: true,
                ..Tags::default()
            },
            tag_error: None,
            drm: false,
        }
    }

    #[test]
    fn migrations_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b.sqlite");
        drop(Store::open(&path).unwrap());
        let s = Store::open(&path).unwrap();
        let v: i64 = s
            .conn()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, MIGRATIONS.len() as i64);
    }

    #[test]
    fn track_round_trip_and_update_keeps_id() {
        let s = Store::open_in_memory().unwrap();
        let id = insert_track(s.conn(), &record("/m/a.m4a"), 99).unwrap();
        let row = track(s.conn(), id).unwrap().unwrap();
        assert_eq!(row.info.kind, FileKind::Alac);
        assert_eq!(row.tags.year, Some(1984));
        assert!(row.tags.compilation);
        assert_eq!(row.tags_hash, record("/m/a.m4a").tags.hash());

        set_missing(s.conn(), id, Some(5)).unwrap();
        assert!(
            present_track_by_key(s.conn(), "/m/a.m4a")
                .unwrap()
                .is_none()
        );
        update_track(s.conn(), id, &record("/m/b.m4a")).unwrap();
        let row = present_track_by_key(s.conn(), "/m/b.m4a").unwrap().unwrap();
        assert_eq!((row.id, row.added_at, row.missing_since), (id, 99, None));
    }

    #[test]
    fn put_playlist_keeps_id_and_replaces_items() {
        let s = Store::open_in_memory().unwrap();
        let t = insert_track(s.conn(), &record("/m/a.m4a"), 0).unwrap();
        let id = put_playlist(
            s.conn(),
            "80s",
            Path::new("/p/80s.m3u8"),
            1,
            &[(Some(t), "a".into()), (None, "x".into())],
        )
        .unwrap();
        let id2 = put_playlist(
            s.conn(),
            "80s",
            Path::new("/p/80s.m3u8"),
            2,
            &[(None, "y".into())],
        )
        .unwrap();
        assert_eq!(id, id2);
        let sums = playlist_summaries(s.conn()).unwrap();
        assert_eq!(
            sums,
            vec![PlaylistSummary {
                name: "80s".into(),
                entries: 1,
                unresolved: 1
            }]
        );
        rename_playlist(s.conn(), "80s", "1980s", Path::new("/p/1980s.m3u8")).unwrap();
        assert_eq!(playlists(s.conn()).unwrap()[0].id, id);
        delete_playlist(s.conn(), "1980s").unwrap();
        assert!(playlists(s.conn()).unwrap().is_empty());
    }
}
