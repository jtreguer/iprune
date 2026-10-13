//! Applying a plan to the iPod. For now only the playlist operations are
//! applied; copies, removals and tag updates come with M5.
//!
//! Order: pull statistics, back up, change the database in memory, check
//! it, write it, read it back from the iPod and compare, then record the
//! new mapping. A failure before the write leaves the iPod untouched.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::Connection;
use serde::Serialize;

use crate::backup::{self, Backup};
use crate::config::{Config, Paths};
use crate::device;
use crate::ipod::{self, Contents};
use crate::plan::{Member, Plan, PlaylistOp};
use crate::playlist::{WriteMode, mtime_ns, write_atomic};
use crate::stats::{self, Pulled};
use crate::store::devices::{self, PlaylistMapping, TrackMapping};
use crate::sync::Prepared;
use crate::verify::{self, ExpectedPlaylist};
use crate::{Error, Result, now_unix};

/// One change made to the iPod database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Delete {
        ipod_id: u64,
        name: String,
    },
    Rename {
        ipod_id: u64,
        to: String,
    },
    Create {
        playlist_id: i64,
        name: String,
        members: Vec<u64>,
    },
    SetMembers {
        ipod_id: u64,
        name: String,
        members: Vec<u64>,
    },
}

/// A managed playlist after the sync. `ipod_id` is `None` until created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub playlist_id: i64,
    pub ipod_id: Option<u64>,
    pub name: String,
    pub members: Vec<u64>,
}

/// What this milestone applies of a plan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Work {
    pub ops: Vec<Op>,
    pub managed: Vec<Target>,
    pub deleted: Vec<u64>,
    /// Playlist entries for tracks not on the iPod yet, left out until
    /// tracks are copied.
    pub left_out: usize,
}

/// The playlist part of `plan`, with members as dbids. A member the plan
/// copies (a new track) is left out; one it replaces keeps its current dbid,
/// since the replace is not applied either. A member update that leaves the
/// iPod list as it is is dropped. Pure.
pub fn playlist_work(plan: &Plan, mappings: &[TrackMapping], ipod: &Contents) -> Work {
    let on_ipod: std::collections::HashSet<u64> =
        ipod.tracks.iter().map(|t| t.adopt.dbid).collect();
    let by_track: HashMap<i64, u64> = mappings
        .iter()
        .filter(|m| on_ipod.contains(&m.dbid))
        .map(|m| (m.track_id, m.dbid))
        .collect();
    let current: HashMap<u64, &Vec<u64>> =
        ipod.playlists.iter().map(|p| (p.id, &p.members)).collect();
    // Members as dbids, and how many entries had to be left out.
    let resolve = |members: &[Member]| -> (Vec<u64>, usize) {
        let dbids: Vec<u64> = members
            .iter()
            .filter_map(|m| match m {
                Member::Track(d) => Some(*d),
                Member::New(id) => by_track.get(id).copied(),
            })
            .collect();
        let left_out = members.len() - dbids.len();
        (dbids, left_out)
    };
    let mut work = Work::default();
    for op in &plan.playlists {
        match op {
            PlaylistOp::Delete { ipod_id, name, .. } => {
                work.deleted.push(*ipod_id);
                work.ops.push(Op::Delete {
                    ipod_id: *ipod_id,
                    name: name.clone(),
                });
            }
            PlaylistOp::Rename { ipod_id, to, .. } => work.ops.push(Op::Rename {
                ipod_id: *ipod_id,
                to: to.clone(),
            }),
            PlaylistOp::Create {
                playlist_id,
                name,
                members,
            } => work.ops.push(Op::Create {
                playlist_id: *playlist_id,
                name: name.clone(),
                members: resolve(members).0,
            }),
            PlaylistOp::SetMembers {
                ipod_id,
                name,
                members,
                ..
            } => {
                let (members, _) = resolve(members);
                if current.get(ipod_id).is_none_or(|c| **c != members) {
                    work.ops.push(Op::SetMembers {
                        ipod_id: *ipod_id,
                        name: name.clone(),
                        members,
                    });
                }
            }
        }
    }
    for e in &plan.expected.playlists {
        let (members, left_out) = resolve(&e.members);
        work.left_out += left_out;
        work.managed.push(Target {
            playlist_id: e.playlist_id,
            ipod_id: e.ipod_id,
            name: e.name.clone(),
            members,
        });
    }
    work
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Allow deleting more than a tenth of the managed playlists at once.
    pub allow_mass_removal: bool,
}

#[derive(Debug)]
pub struct Outcome {
    pub applied: usize,
    pub left_out: usize,
    pub pulled: Pulled,
    /// Tracks whose pull from an interrupted earlier sync was undone.
    pub recovered: usize,
    pub backup: Backup,
    pub pruned: usize,
    /// Steps after the verified write that failed: the sync itself stands.
    pub warnings: Vec<String>,
}

/// Refuses deleting many managed playlists at once, which more likely means
/// an unmounted or emptied playlists folder than a decision.
pub fn check_removals(work: &Work, mapped: usize, opts: Options) -> Result<()> {
    let deletes = work.deleted.len();
    if !opts.allow_mass_removal && deletes > 5 && deletes * 10 > mapped {
        return Err(Error::MassRemoval {
            deletes,
            managed: mapped,
        });
    }
    Ok(())
}

#[derive(Serialize)]
struct MappingsFile<'a> {
    tracks: &'a [TrackMapping],
    playlists: &'a [PlaylistMapping],
}

fn snapshot(db: &gpod::Database) -> Result<String> {
    let mut out = Vec::new();
    gpod::snapshot::write(db, &mut out).map_err(|source| Error::Io {
        path: "snapshot".into(),
        source,
    })?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn json<T: Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec_pretty(v).expect("plain data serializes")
}

/// Changes the in-memory database. Returns the ids of created playlists by
/// library playlist id.
fn apply(db: &mut gpod::Database, ops: &[Op]) -> Result<HashMap<i64, u64>> {
    let mut created = HashMap::new();
    for op in ops {
        match op {
            Op::Delete { ipod_id, .. } => db.remove_playlist(*ipod_id)?,
            Op::Rename { ipod_id, to } => db.rename_playlist(*ipod_id, to)?,
            Op::Create {
                playlist_id,
                name,
                members,
            } => {
                let id = db.create_playlist(name)?;
                db.set_playlist_members(id, members)?;
                created.insert(*playlist_id, id);
            }
            Op::SetMembers {
                ipod_id, members, ..
            } => db.set_playlist_members(*ipod_id, members)?,
        }
    }
    Ok(created)
}

/// Applies `work` to the iPod at `mountpoint`, whose database `db` was read
/// for `prepared`.
/// Whether the iPod's database files still equal the backup's copies.
fn untouched(backup: &Backup, mountpoint: &Path) -> bool {
    [
        ("iTunesDB", "iPod_Control/iTunes/iTunesDB"),
        ("ArtworkDB", "iPod_Control/Artwork/ArtworkDB"),
    ]
    .iter()
    .all(|(name, rel)| {
        let (saved, now) = (
            std::fs::read(backup.dir.join(name)),
            std::fs::read(mountpoint.join(rel)),
        );
        match (saved, now) {
            (Ok(a), Ok(b)) => a == b,
            // A file absent from both is unchanged.
            (Err(a), Err(b)) => {
                a.kind() == std::io::ErrorKind::NotFound && b.kind() == std::io::ErrorKind::NotFound
            }
            _ => false,
        }
    })
}

/// Applies `work` to the iPod at `mountpoint`, whose database `db` was read
/// for `prepared`.
#[allow(clippy::too_many_arguments)]
pub fn sync_playlists(
    conn: &Connection,
    cfg: &Config,
    paths: &Paths,
    mountpoint: &Path,
    mut db: gpod::Database,
    prepared: &Prepared,
    work: &Work,
    opts: Options,
) -> Result<Outcome> {
    device::check_writable(mountpoint)?;
    let device = devices::device_id(conn, &prepared.guid)?
        .ok_or_else(|| Error::NotAdopted(prepared.guid.clone()))?;
    check_removals(work, prepared.playlists_map.len(), opts)?;

    // A pull left pending by an interrupted sync is settled first, against
    // the `Play Counts` files as they are now.
    let play_counts = stats::play_counts(mountpoint)?;
    let recovery = stats::recover(conn, device, play_counts)?;
    // Statistics before anything else: the write resets the recent counts.
    let pulled = stats::pull(
        conn,
        device,
        &prepared.tracks_map,
        &prepared.ipod,
        play_counts.live,
    )?;
    let undo = |e: Error| -> Error {
        match stats::undo(conn, device, &pulled) {
            Ok(()) => e,
            Err(u) => Error::UndoFailed {
                cause: Box::new(e),
                undo: Box::new(u),
            },
        }
    };
    let backups = paths.data_dir.join("backups");
    let before = snapshot(&db).map_err(undo)?;
    let mappings = json(&MappingsFile {
        tracks: &prepared.tracks_map,
        playlists: &prepared.playlists_map,
    });
    let backup = backup::create(
        &backups,
        &prepared.guid,
        mountpoint,
        now_unix(),
        &[
            ("plan.json", &json(&prepared.plan)),
            ("work.json", &json(&work.ops)),
            ("snapshot.txt", before.as_bytes()),
            ("mappings.json", &mappings),
        ],
    )
    .map_err(undo)?;

    db.start_sync();
    let created = apply(&mut db, &work.ops).map_err(undo)?;
    let managed: Vec<ExpectedPlaylist> = work
        .managed
        .iter()
        .map(|t| ExpectedPlaylist {
            ipod_id: t
                .ipod_id
                .or_else(|| created.get(&t.playlist_id).copied())
                .unwrap_or(0),
            name: t.name.clone(),
            members: t.members.clone(),
        })
        .collect();
    let after = ipod::read(&db);
    let problems = verify::pre_write(
        &prepared.ipod,
        &after,
        &managed,
        &work.deleted,
        created.len(),
    );
    if !problems.is_empty() {
        db.stop_sync();
        return Err(undo(Error::PreWriteCheck(problems)));
    }
    let expected = snapshot(&db).map_err(undo)?;
    if let Err(e) = db.write() {
        db.stop_sync();
        drop(db);
        // libgpod writes several files; a failure part way leaves some of
        // them changed. Only an iPod still equal to the backup keeps its
        // `Play Counts`, and only then is the pull undone.
        // Whether `Play Counts` was renamed is left to the next sync's
        // recovery, which decides by the files as they are then.
        return Err(if untouched(&backup, mountpoint) {
            undo(e.into())
        } else {
            Error::WriteIncomplete {
                cause: e.to_string(),
                stamp: backup.stamp.clone(),
            }
        });
    }
    db.stop_sync();
    drop(db);
    // The write happened: libgpod has folded the recent counts into the
    // database and renamed `Play Counts`. The pull stands.
    // If this fails the next sync's recovery settles it from the renamed
    // `Play Counts.bak`; the sync itself stands.
    let mut warnings: Vec<String> = recovery.dropped.into_iter().collect();
    if let Err(e) = stats::settle(conn, device) {
        warnings.push(format!("recording the statistics pull as done: {e}"));
    }

    let written = |problems: Vec<String>| Error::Verification {
        problems,
        stamp: backup.stamp.clone(),
    };
    let reread = gpod::Database::open(mountpoint)
        .map_err(|e| written(vec![format!("the written database cannot be read: {e}")]))?;
    let diffs = verify::differences(&expected, &snapshot(&reread)?, 20);
    drop(reread);
    if !diffs.is_empty() {
        return Err(written(diffs));
    }

    let new_mappings: Vec<PlaylistMapping> = work
        .managed
        .iter()
        .zip(&managed)
        .map(|(t, m)| PlaylistMapping {
            playlist_id: t.playlist_id,
            ipod_id: Some(m.ipod_id),
            ipod_name: m.name.clone(),
        })
        .collect();
    crate::store::savepoint(conn, || {
        devices::replace_playlist_mappings(conn, device, &new_mappings)?;
        devices::set_last_sync(conn, device, now_unix())
    })?;
    // The sync is done; what follows only reports a warning on failure.
    if let Err(e) = stats::export_csv(conn, &paths.data_dir.join("stats.csv")) {
        warnings.push(format!("writing stats.csv: {e}"));
    }
    if let Err(e) = write_manifest(conn, mountpoint, &prepared.guid, device) {
        warnings.push(format!("writing the manifest on the iPod: {e}"));
    }
    // Never delete the backup just taken.
    let keep = (cfg.sync.keep_backups as usize).max(1);
    let pruned = match backup::prune(&backups, &prepared.guid, keep) {
        Ok(p) => p.len(),
        Err(e) => {
            warnings.push(format!("deleting old backups: {e}"));
            0
        }
    };
    Ok(Outcome {
        applied: work.ops.len(),
        left_out: work.left_out,
        pulled,
        recovered: recovery.undone,
        backup,
        pruned,
        warnings,
    })
}

#[derive(Serialize)]
struct Manifest {
    firewire_guid: String,
    written_at: i64,
    tracks: Vec<ManifestTrack>,
    playlists: Vec<ManifestPlaylist>,
}

#[derive(Serialize)]
struct ManifestTrack {
    dbid: u64,
    path: String,
}

#[derive(Serialize)]
struct ManifestPlaylist {
    ipod_id: Option<u64>,
    name: String,
    file: String,
}

/// Writes the device mapping to `iPod_Control/iprune/manifest.json`, so it
/// can be rebuilt if the local index is lost.
fn write_manifest(conn: &Connection, mountpoint: &Path, guid: &str, device: i64) -> Result<()> {
    let paths: HashMap<i64, String> = crate::store::all_tracks(conn)?
        .into_iter()
        .map(|t| (t.id, t.path.to_string_lossy().into_owned()))
        .collect();
    let files: HashMap<i64, String> = crate::store::playlists(conn)?
        .into_iter()
        .map(|p| (p.id, p.file.to_string_lossy().into_owned()))
        .collect();
    let manifest = Manifest {
        firewire_guid: guid.to_owned(),
        written_at: now_unix(),
        tracks: devices::track_mappings(conn, device)?
            .iter()
            .filter_map(|m| {
                Some(ManifestTrack {
                    dbid: m.dbid,
                    path: paths.get(&m.track_id)?.clone(),
                })
            })
            .collect(),
        playlists: devices::playlist_mappings(conn, device)?
            .into_iter()
            .filter_map(|m| {
                Some(ManifestPlaylist {
                    ipod_id: m.ipod_id,
                    name: m.ipod_name,
                    file: files.get(&m.playlist_id)?.clone(),
                })
            })
            .collect(),
    };
    let dir = mountpoint.join("iPod_Control/iprune");
    std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
        path: dir.clone(),
        source,
    })?;
    let path = dir.join("manifest.json");
    let mode = match mtime_ns(&path) {
        Ok(mtime_ns) => WriteMode::Replace { mtime_ns },
        Err(_) => WriteMode::CreateNew,
    };
    let text = String::from_utf8(json(&manifest)).expect("JSON is UTF-8");
    write_atomic(&path, &text, mode)
}

#[derive(serde::Deserialize)]
struct SavedMappings {
    tracks: Vec<TrackMapping>,
    playlists: Vec<PlaylistMapping>,
}

#[derive(Debug)]
pub struct Restored {
    pub files: Vec<&'static str>,
    pub tracks: usize,
    pub playlists: usize,
    /// Mappings dropped because their library track or playlist is gone.
    pub dropped_tracks: usize,
    pub dropped_playlists: usize,
}

/// The backup folder name for the iPod whose GUID is `guid` (as read by
/// [`device::firewire_guid`]), matched without regard to case or a `0x`
/// prefix. Without a GUID, the only iPod with backups, if there is one.
pub fn backup_guid(paths: &Paths, guid: Option<&str>) -> Result<String> {
    let root = paths.data_dir.join("backups");
    let norm = |s: &str| {
        let s = s.trim();
        s.strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .unwrap_or(s)
            .to_ascii_uppercase()
    };
    let names: Vec<String> = match std::fs::read_dir(&root) {
        Ok(d) => d
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| !n.starts_with('.'))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(source) => return Err(Error::Io { path: root, source }),
    };
    let found: Vec<&String> = match guid {
        Some(g) => names.iter().filter(|n| norm(n) == norm(g)).collect(),
        None => names.iter().collect(),
    };
    match found[..] {
        [one] => Ok(one.clone()),
        [] => Err(Error::NoSuchBackup("no backups for this iPod".to_owned())),
        _ => Err(Error::NoSuchBackup(
            "backups of several iPods, and this one has no readable GUID".to_owned(),
        )),
    }
}

/// Puts backup `stamp` of the iPod `guid` (a backup folder name, see
/// [`backup_guid`]) back on the iPod at `mountpoint` and returns the
/// device's mappings to what they were when it was taken. Files copied to the
/// iPod after the backup stay as orphans. The iPod's database is not read, so
/// this works when it is damaged.
pub fn restore(
    conn: &Connection,
    paths: &Paths,
    mountpoint: &Path,
    guid: &str,
    stamp: &str,
) -> Result<Restored> {
    let backups = paths.data_dir.join("backups");
    let backup = backup::list(&backups, guid)?
        .into_iter()
        .find(|b| b.stamp == stamp)
        .ok_or_else(|| Error::NoSuchBackup(stamp.to_owned()))?;
    let path = backup.dir.join("mappings.json");
    let text = std::fs::read(&path).map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;
    let saved: SavedMappings = serde_json::from_slice(&text).map_err(|e| Error::Io {
        path: path.clone(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
    })?;
    device::check_writable(mountpoint)?;
    let device =
        devices::device_id(conn, guid)?.ok_or_else(|| Error::NotAdopted(guid.to_owned()))?;
    let track_ids: std::collections::HashSet<i64> = crate::store::all_tracks(conn)?
        .into_iter()
        .map(|t| t.id)
        .collect();
    let playlist_ids: std::collections::HashSet<i64> = conn
        .prepare("SELECT id FROM playlists")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let saved_counts = (saved.tracks.len(), saved.playlists.len());
    let tracks: Vec<TrackMapping> = saved
        .tracks
        .into_iter()
        .filter(|m| track_ids.contains(&m.track_id))
        .collect();
    let playlists: Vec<PlaylistMapping> = saved
        .playlists
        .into_iter()
        .filter(|m| playlist_ids.contains(&m.playlist_id))
        .collect();
    let files = backup::restore_files(&backup, mountpoint)?;
    crate::store::savepoint(conn, || {
        devices::replace_mappings(conn, device, &tracks, &playlists)
    })?;
    Ok(Restored {
        files,
        tracks: tracks.len(),
        playlists: playlists.len(),
        dropped_tracks: saved_counts.0 - tracks.len(),
        dropped_playlists: saved_counts.1 - playlists.len(),
    })
}
