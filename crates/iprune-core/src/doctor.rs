//! `iprune doctor`: what a crash, an interrupted sync or another program can
//! leave behind, found and, when asked, repaired.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

use crate::error::IoContext;
use crate::ipod::{self, Contents};
use crate::store::{self, devices};
use crate::{Error, Result};

/// A library track missing for longer than this, and on no device, is
/// dropped from the index by `--fix`.
pub const FORGET_AFTER_SECS: i64 = 30 * 24 * 3600;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Files under `iPod_Control/Music` that no database entry uses.
    pub orphan_files: Vec<PathBuf>,
    /// `(dbid, "artist - title")` of database entries whose file is gone.
    pub missing_files: Vec<(u64, String)>,
    /// `(playlist, managed)` for regular iPod playlists holding a track more
    /// than once when their file (for a managed one) does not.
    pub repeated: Vec<(String, bool)>,
    /// `(track id, dbid)` of mappings whose iPod track is gone.
    pub stale_mappings: Vec<(i64, u64)>,
    /// `(track id, path)` of library tracks missing for over 30 days and on
    /// no device.
    pub forgotten: Vec<(i64, PathBuf)>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.orphan_files.is_empty()
            && self.missing_files.is_empty()
            && self.repeated.is_empty()
            && self.stale_mappings.is_empty()
            && self.forgotten.is_empty()
    }
}

/// The files of the iPod's tracks, lower-cased and relative to the
/// mountpoint: FAT ignores case.
fn referenced(contents: &Contents) -> HashSet<String> {
    contents
        .tracks
        .iter()
        .filter_map(|t| t.ipod_path.as_deref())
        .filter_map(|p| p.strip_prefix(':'))
        .map(|p| p.replace(':', "/").to_lowercase())
        .collect()
}

/// Regular files in `iPod_Control/Music/*/`, where libgpod puts tracks.
fn music_files(mountpoint: &Path) -> Result<Vec<PathBuf>> {
    let music = mountpoint.join("iPod_Control/Music");
    let mut out = Vec::new();
    if !music.is_dir() {
        return Ok(out);
    }
    for dir in std::fs::read_dir(&music).at(&music)? {
        let dir = dir.at(&music)?;
        if !dir.file_type().at(dir.path())?.is_dir() {
            continue;
        }
        for f in std::fs::read_dir(dir.path()).at(dir.path())? {
            let f = f.at(dir.path())?;
            if f.file_type().at(f.path())?.is_file() {
                out.push(f.path());
            }
        }
    }
    out.sort();
    Ok(out)
}

fn repeats(members: &[u64]) -> usize {
    members.len() - members.iter().collect::<HashSet<_>>().len()
}

/// Looks at the iPod at `mountpoint`, read into `contents`, and at the
/// index. `device` is the iPod's row, if it was adopted. Reads only.
pub fn examine(
    conn: &Connection,
    mountpoint: &Path,
    contents: &Contents,
    device: Option<i64>,
    now: i64,
) -> Result<Report> {
    let mut report = Report::default();
    let used = referenced(contents);
    for f in music_files(mountpoint)? {
        let rel = f
            .strip_prefix(mountpoint)
            .map(|r| r.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !used.contains(&rel) {
            report.orphan_files.push(f);
        }
    }
    for t in &contents.tracks {
        let exists = t
            .ipod_path
            .as_deref()
            .and_then(|p| ipod::file_path(mountpoint, p))
            .is_some_and(|f| f.is_file());
        if !exists {
            report.missing_files.push((
                t.adopt.dbid,
                format!(
                    "{} - {}",
                    t.adopt.artist.as_deref().unwrap_or("?"),
                    t.adopt.title.as_deref().unwrap_or("?")
                ),
            ));
        }
    }

    let on_ipod: HashSet<u64> = contents.tracks.iter().map(|t| t.adopt.dbid).collect();
    let (tracks_map, playlists_map) = match device {
        Some(d) => (
            devices::track_mappings(conn, d)?,
            devices::playlist_mappings(conn, d)?,
        ),
        None => (Vec::new(), Vec::new()),
    };
    let dbid_of: HashMap<i64, u64> = tracks_map.iter().map(|m| (m.track_id, m.dbid)).collect();
    let files: HashMap<i64, Vec<Option<i64>>> = devices::library_playlists(conn)?
        .into_iter()
        .map(|p| (p.id, p.members))
        .collect();
    let file_of: HashMap<u64, i64> = playlists_map
        .iter()
        .filter_map(|m| Some((m.ipod_id?, m.playlist_id)))
        .collect();
    for p in &contents.playlists {
        let on = repeats(&p.members);
        if on == 0 {
            continue;
        }
        match file_of.get(&p.id).and_then(|id| files.get(id)) {
            // A managed playlist whose file repeats the same tracks is fine.
            Some(members) => {
                let in_file: Vec<u64> = members
                    .iter()
                    .flatten()
                    .filter_map(|id| dbid_of.get(id).copied())
                    .collect();
                if on > repeats(&in_file) {
                    report.repeated.push((p.name.clone(), true));
                }
            }
            None => report.repeated.push((p.name.clone(), false)),
        }
    }
    for m in &tracks_map {
        if !on_ipod.contains(&m.dbid) {
            report.stale_mappings.push((m.track_id, m.dbid));
        }
    }

    let mut stmt = conn.prepare(
        "SELECT id, path FROM tracks WHERE missing_since IS NOT NULL AND missing_since < ?1 \
         AND id NOT IN (SELECT track_id FROM device_tracks) ORDER BY path",
    )?;
    report.forgotten = stmt
        .query_map(params![now - FORGET_AFTER_SECS], |r| {
            Ok((r.get(0)?, PathBuf::from(r.get::<_, String>(1)?)))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(report)
}

/// Repairs the index: drops stale mappings, and forgets long-missing
/// tracks (their statistics go, and playlist entries pointing to them stay
/// as unresolved lines). One savepoint.
pub fn fix_index(conn: &Connection, device: Option<i64>, report: &Report) -> Result<()> {
    store::savepoint(conn, || {
        if let Some(d) = device {
            let stale: Vec<u64> = report
                .stale_mappings
                .iter()
                .map(|(_, dbid)| *dbid)
                .collect();
            devices::apply_track_changes(conn, d, &[], &stale)?;
        }
        for (id, _) in &report.forgotten {
            conn.execute("DELETE FROM stats WHERE track_id = ?1", [id])?;
            conn.execute(
                "UPDATE playlist_items SET track_id = NULL WHERE track_id = ?1",
                [id],
            )?;
            conn.execute(
                "DELETE FROM tracks WHERE id = ?1 AND id NOT IN (SELECT track_id FROM device_tracks)",
                [id],
            )?;
        }
        Ok(())
    })
}

/// Refuses orphan files more than a tenth of the iPod's tracks and over
/// five, unless `allow_mass_removal`: that many orphans more likely means a
/// damaged database (restore it first) than leftovers. Run it before any
/// repair, so a refusal changes nothing.
pub fn check_orphans(report: &Report, tracks: usize, allow_mass_removal: bool) -> Result<()> {
    let n = report.orphan_files.len();
    if too_many(n, tracks) && !allow_mass_removal {
        return Err(Error::MassOrphans { files: n, tracks });
    }
    Ok(())
}

/// Refuses removing the database entries whose file is gone when they are
/// more than a tenth of the iPod's tracks and over five, unless
/// `allow_mass_removal`: an unreadable music folder makes every file look
/// gone. Run it before any repair.
pub fn check_missing(report: &Report, tracks: usize, allow_mass_removal: bool) -> Result<()> {
    let n = report.missing_files.len();
    if too_many(n, tracks) && !allow_mass_removal {
        return Err(Error::MassMissing { entries: n, tracks });
    }
    Ok(())
}

fn too_many(n: usize, tracks: usize) -> bool {
    n > 5 && n * 10 > tracks
}

/// Deletes the orphan files, after [`check_orphans`]. Returns how many were
/// deleted.
pub fn delete_orphans(report: &Report, tracks: usize, allow_mass_removal: bool) -> Result<usize> {
    check_orphans(report, tracks, allow_mass_removal)?;
    let n = report.orphan_files.len();
    for f in &report.orphan_files {
        std::fs::remove_file(f).at(f)?;
    }
    Ok(n)
}

/// The work that removes the database entries whose file is gone and
/// changes nothing else: every managed playlist is expected as it is on the
/// iPod, less those tracks, and the playlist mappings are left alone
/// (tombstones and mappings without an iPod id included).
pub fn repair_work(prepared: &crate::sync::Prepared, report: &Report) -> crate::exec::FullWork {
    use crate::exec::{FullWork, Target, Work};
    use crate::transfer::{Remove, TrackWork};
    let gone: HashSet<u64> = report.missing_files.iter().map(|(d, _)| *d).collect();
    let track_of: HashMap<u64, i64> = prepared
        .tracks_map
        .iter()
        .map(|m| (m.dbid, m.track_id))
        .collect();
    let size_of: HashMap<u64, u64> = prepared
        .ipod
        .tracks
        .iter()
        .map(|t| (t.adopt.dbid, t.adopt.size))
        .collect();
    let removes = report
        .missing_files
        .iter()
        .map(|(dbid, _)| Remove {
            dbid: *dbid,
            track_id: track_of.get(dbid).copied(),
            size: size_of.get(dbid).copied().unwrap_or(0),
        })
        .collect();
    let on_ipod: HashMap<u64, &crate::adopt::IpodPlaylist> =
        prepared.ipod.playlists.iter().map(|p| (p.id, p)).collect();
    let managed = prepared
        .playlists_map
        .iter()
        .filter_map(|m| {
            let p = on_ipod.get(&m.ipod_id?)?;
            Some(Target {
                playlist_id: m.playlist_id,
                ipod_id: Some(p.id),
                name: p.name.clone(),
                members: p
                    .members
                    .iter()
                    .copied()
                    .filter(|d| !gone.contains(d))
                    .collect(),
            })
        })
        .collect();
    FullWork {
        tracks: TrackWork {
            removes,
            ..TrackWork::default()
        },
        playlists: Work {
            managed,
            ..Work::default()
        },
        keep_playlist_mappings: true,
    }
}
