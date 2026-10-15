//! Applying a plan to the iPod. For now only the playlist operations are
//! applied; copies, removals and tag updates come with M5.
//!
//! Order: pull statistics, back up, change the database in memory, check
//! it, write it, read it back from the iPod and compare, then record the
//! new mapping. A failure before the write leaves the iPod untouched.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

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
use crate::transfer::{self, TrackWork};
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
    playlist_work_with(plan, mappings, ipod, &HashMap::new())
}

/// [`playlist_work`] once the copies are planned: a member the plan copies
/// takes the dbid `new_dbids` gives its track.
pub fn playlist_work_with(
    plan: &Plan,
    mappings: &[TrackMapping],
    ipod: &Contents,
    new_dbids: &HashMap<i64, u64>,
) -> Work {
    let on_ipod: std::collections::HashSet<u64> =
        ipod.tracks.iter().map(|t| t.adopt.dbid).collect();
    let mut by_track: HashMap<i64, u64> = mappings
        .iter()
        .filter(|m| on_ipod.contains(&m.dbid))
        .map(|m| (m.track_id, m.dbid))
        .collect();
    by_track.extend(new_dbids.iter().map(|(k, v)| (*k, *v)));
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
    /// Allow removing more than a tenth of the managed playlists or tracks
    /// at once.
    pub allow_mass_removal: bool,
}

/// Where a sync is, for a progress display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Removing {
        done: usize,
        total: usize,
    },
    Copying {
        done: usize,
        total: usize,
        bytes: u64,
        bytes_total: u64,
        name: String,
    },
    /// Writing the database, at a checkpoint or at the end.
    Writing,
    Verifying,
}

/// Everything a sync applies: the track part, then the playlists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FullWork {
    pub tracks: TrackWork,
    pub playlists: Work,
    /// Leave the device's playlist mappings as they are after the write (a
    /// repair that changes no playlist), instead of recording them from
    /// `playlists.managed`.
    pub keep_playlist_mappings: bool,
}

impl FullWork {
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty() && self.playlists.ops.is_empty()
    }
}

/// The whole of `prepared`'s plan as work. Pure.
pub fn full_work(prepared: &Prepared) -> FullWork {
    let tracks = transfer::track_work(
        &prepared.plan,
        &prepared.library,
        &prepared.ipod,
        &prepared.guid,
    );
    let playlists = playlist_work_with(
        &prepared.plan,
        &prepared.tracks_map,
        &prepared.ipod,
        &tracks.new_dbids(),
    );
    FullWork {
        tracks,
        playlists,
        keep_playlist_mappings: false,
    }
}

#[derive(Debug)]
pub struct Outcome {
    /// Playlist operations applied.
    pub applied: usize,
    pub copied: usize,
    pub recopied: usize,
    pub removed: usize,
    pub retagged: usize,
    pub bytes: u64,
    pub left_out: usize,
    pub pulled: Pulled,
    /// Tracks whose pull from an interrupted earlier sync was undone.
    pub recovered: usize,
    pub backup: Backup,
    pub pruned: usize,
    /// Set when the sync stopped early (interrupted, or a copy failed): what
    /// was done up to then is written and verified; the rest, playlists
    /// included, is left for the next sync.
    pub stopped: Option<String>,
    /// `(file, why)` of library files not copied because they were gone,
    /// unreadable or changed since the scan; a later sync copies them.
    pub skipped: Vec<(PathBuf, String)>,
    /// Steps after the verified write that failed: the sync itself stands.
    pub warnings: Vec<String>,
}

/// Refuses removing many managed playlists or tracks at once, which more
/// likely means an unmounted or emptied folder than a decision.
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

/// The same for tracks, against the number of tracks mapped.
pub fn check_track_removals(work: &TrackWork, mapped: usize, opts: Options) -> Result<()> {
    let removes = work.removes.iter().filter(|r| r.track_id.is_some()).count();
    if !opts.allow_mass_removal && removes > 5 && removes * 10 > mapped {
        return Err(Error::MassTrackRemoval {
            removes,
            managed: mapped,
        });
    }
    Ok(())
}

/// Refuses copying again many managed tracks because their files seem
/// missing on the iPod, which more likely means its music folder reads badly
/// than files lost.
pub fn check_missing_files(plan: &Plan, mapped: usize, opts: Options) -> Result<()> {
    let missing = plan.replace.iter().filter(|r| r.file_missing).count();
    if !opts.allow_mass_removal && missing > 5 && missing * 10 > mapped {
        return Err(Error::MassMissingFiles {
            missing,
            managed: mapped,
        });
    }
    Ok(())
}

/// `work.json` in a backup: what the sync was about to apply.
#[derive(Serialize)]
struct WorkFile<'a> {
    tracks: &'a TrackWork,
    playlist_ops: &'a [Op],
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

/// A checkpoint or the final write between a sync's changes and its record.
struct Writer<'a> {
    conn: &'a Connection,
    mountpoint: &'a Path,
    device: i64,
    backup: &'a Backup,
    before: &'a Contents,
    pulled: &'a Pulled,
    /// Set once a write reached the iPod: the pull stands from then on.
    written: bool,
    /// Files of removed or recopied tracks, deleted once a write is verified.
    doomed: Vec<PathBuf>,
    warnings: Vec<String>,
}

impl Writer<'_> {
    fn undo(&self, e: Error) -> Error {
        if self.written {
            return e;
        }
        match stats::undo(self.conn, self.device, self.pulled) {
            Ok(()) => e,
            Err(u) => Error::UndoFailed {
                cause: Box::new(e),
                undo: Box::new(u),
            },
        }
    }

    /// Checks the database against `expect`, writes it, reads it back and
    /// compares, then deletes the doomed files.
    fn write(
        &mut self,
        db: &mut gpod::Database,
        expect: &verify::Expect,
        progress: &dyn Fn(Progress),
    ) -> Result<()> {
        let after = ipod::read(db);
        let problems = verify::check(self.before, &after, expect, Some(self.mountpoint));
        if !problems.is_empty() {
            return Err(match self.written {
                false => self.undo(Error::PreWriteCheck(problems)),
                // An earlier checkpoint is on the iPod and verified: this
                // write is not made, and what the checkpoint holds stands.
                true => Error::CheckAfterCheckpoint(problems),
            });
        }
        let expected = snapshot(db).map_err(|e| self.undo(e))?;
        progress(Progress::Writing);
        if let Err(e) = db.write() {
            // libgpod writes several files; a failure part way leaves some
            // of them changed. Only an iPod still equal to the backup keeps
            // its `Play Counts`, and only then is the pull undone; otherwise
            // the next sync's recovery decides by the files.
            return Err(
                if !self.written && untouched(self.backup, self.mountpoint) {
                    self.undo(e.into())
                } else {
                    Error::WriteIncomplete {
                        cause: e.to_string(),
                        stamp: self.backup.stamp.clone(),
                    }
                },
            );
        }
        if !self.written {
            self.written = true;
            // libgpod has folded the recent counts into the database and
            // renamed `Play Counts`. If recording this fails, the next
            // sync's recovery settles it from `Play Counts.bak`.
            if let Err(e) = stats::settle(self.conn, self.device) {
                self.warnings
                    .push(format!("recording the statistics pull as done: {e}"));
            }
        }
        progress(Progress::Verifying);
        let failed = |problems: Vec<String>| Error::Verification {
            problems,
            stamp: self.backup.stamp.clone(),
        };
        let reread = gpod::Database::open(self.mountpoint)
            .map_err(|e| failed(vec![format!("the written database cannot be read: {e}")]))?;
        let diffs = verify::differences(&expected, &snapshot(&reread)?, 20);
        drop(reread);
        if !diffs.is_empty() {
            return Err(failed(diffs));
        }
        for f in self.doomed.drain(..) {
            if let Err(e) = std::fs::remove_file(&f)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                self.warnings.push(format!("deleting {}: {e}", f.display()));
            }
        }
        Ok(())
    }
}

/// A dbid's mapping after a sync, from what was copied or retagged.
fn copied_mapping(c: &transfer::Copy) -> TrackMapping {
    TrackMapping {
        track_id: c.track_id,
        dbid: c.dbid,
        synced_tags_hash: c.tags_hash,
        synced_size: c.audio.size,
        synced_duration_ms: c.duration_ms,
    }
}

/// Copies one file: a new track, or new audio for an existing one.
fn copy_one(
    db: &mut gpod::Database,
    c: &transfer::Copy,
    now: i64,
    doomed: &mut Vec<PathBuf>,
) -> Result<()> {
    let audio = gpod::AudioFields {
        size: u32::try_from(c.audio.size).unwrap_or(u32::MAX),
        tracklen: i32::try_from(c.audio.tracklen_ms).unwrap_or(i32::MAX),
        bitrate: i32::try_from(c.audio.bitrate).unwrap_or(i32::MAX),
        samplerate: u16::try_from(c.audio.samplerate).unwrap_or(u16::MAX),
        filetype: c.audio.filetype.to_owned(),
        time_added: now,
        time_modified: c.audio.modified,
    };
    let fields = c.fields.to_gpod();
    // Before copying, so nothing fails once the track has its new file.
    fields.check()?;
    if c.replace {
        let old = db.copy_track_file(c.dbid, &c.path)?;
        doomed.extend(old);
        // The track keeps the date it was first added.
        let audio = gpod::AudioFields {
            time_added: 0,
            ..audio
        };
        db.set_track_audio(c.dbid, &audio)?;
        db.set_track_fields(c.dbid, &fields)?;
        return Ok(());
    }
    db.add_track(c.dbid, &fields, &audio)?;
    if let Err(e) = db.copy_track_file(c.dbid, &c.path) {
        // The half-added track goes; a partly copied file is an orphan for
        // `iprune doctor`.
        let _ = db.remove_track(c.dbid);
        return Err(e.into());
    }
    Ok(())
}

/// The checkpoint interval: this many copies, or this much time.
const CHECKPOINT_COPIES: usize = 500;
const CHECKPOINT_SECS: u64 = 600;

/// Applies `work` to the iPod at `mountpoint`, whose database `db` was read
/// for `prepared`: removals, copies (with checkpoints), tag updates, then
/// playlists, checked, written and verified. `cancel` stops the copies after
/// the current file; what was done is written as a checkpoint. `lock` is
/// the iPod lock, taken before `db` was read.
#[allow(clippy::too_many_arguments)]
pub fn sync(
    conn: &Connection,
    cfg: &Config,
    paths: &Paths,
    lock: &crate::lock::IpodLock,
    mountpoint: &Path,
    mut db: gpod::Database,
    prepared: &Prepared,
    work: &FullWork,
    opts: Options,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<Outcome> {
    // Held by the caller since before it read the iPod for `prepared`.
    lock.check(paths)?;
    device::check_writable(mountpoint)?;
    let device = devices::device_id(conn, &prepared.guid)?
        .ok_or_else(|| Error::NotAdopted(prepared.guid.clone()))?;
    check_removals(&work.playlists, prepared.playlists_map.len(), opts)?;
    check_track_removals(&work.tracks, prepared.tracks_map.len(), opts)?;
    check_missing_files(&prepared.plan, prepared.tracks_map.len(), opts)?;
    if !work.tracks.copies.is_empty() && !prepared.plan.space.fits {
        return Err(Error::NoSpace);
    }

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
    let backups = paths.data_dir.join("backups");
    let undo_now = |e: Error| -> Error {
        match stats::undo(conn, device, &pulled) {
            Ok(()) => e,
            Err(u) => Error::UndoFailed {
                cause: Box::new(e),
                undo: Box::new(u),
            },
        }
    };
    let before_text = snapshot(&db).map_err(undo_now)?;
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
            (
                "work.json",
                &json(&WorkFile {
                    tracks: &work.tracks,
                    playlist_ops: &work.playlists.ops,
                }),
            ),
            ("snapshot.txt", before_text.as_bytes()),
            ("mappings.json", &mappings),
        ],
    )
    .map_err(undo_now)?;

    let mut w = Writer {
        conn,
        mountpoint,
        device,
        backup: &backup,
        before: &prepared.ipod,
        pulled: &pulled,
        written: false,
        doomed: Vec::new(),
        warnings: recovery.dropped.into_iter().collect(),
    };
    let mut expect = verify::Expect::default();
    let mut skipped: Vec<(PathBuf, String)> = Vec::new();
    // Mappings recorded at each verified write.
    let mut upserts: Vec<TrackMapping> = Vec::new();
    let mut removed_dbids: Vec<u64> = Vec::new();
    let record = |upserts: &mut Vec<TrackMapping>, removed: &mut Vec<u64>| -> Result<()> {
        crate::store::savepoint(conn, || {
            devices::apply_track_changes(conn, device, upserts, removed)
        })?;
        upserts.clear();
        removed.clear();
        Ok(())
    };

    db.start_sync();
    let result = (|| -> Result<(Option<String>, usize)> {
        let t = &work.tracks;
        for (i, r) in t.removes.iter().enumerate() {
            let file = db.remove_track(r.dbid).map_err(|e| w.undo(e.into()))?;
            w.doomed.extend(file);
            expect.removed.insert(r.dbid);
            removed_dbids.push(r.dbid);
            progress(Progress::Removing {
                done: i + 1,
                total: t.removes.len(),
            });
        }
        // The space check counts removed files as freed: make it true before
        // copying, by writing the removals and deleting their files.
        if !t.removes.is_empty() && !t.copies.is_empty() {
            w.write(&mut db, &expect, progress)?;
            record(&mut upserts, &mut removed_dbids)?;
        }
        let now = now_unix();
        let bytes_total = t.copy_bytes();
        let (mut bytes, mut since, mut stopped) = (0u64, 0usize, None);
        let mut skipped_dbids: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut last = std::time::Instant::now();
        for (i, c) in t.copies.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                stopped = Some("interrupted".to_owned());
                break;
            }
            progress(Progress::Copying {
                done: i,
                total: t.copies.len(),
                bytes,
                bytes_total,
                name: c
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            });
            // A file gone or changed since the scan is skipped and copied by
            // a later sync; a copy that fails with a readable file is the
            // iPod's problem (full, disconnected), and stops the sync.
            let skip = match std::fs::metadata(&c.path).and_then(|m| {
                std::fs::File::open(&c.path)?;
                Ok(m.len())
            }) {
                Ok(len) if len == c.audio.size => None,
                Ok(_) => Some("changed since the scan".to_owned()),
                Err(e) => Some(e.to_string()),
            };
            if let Some(why) = skip {
                skipped.push((c.path.clone(), why));
                // A track already on the iPod keeps its old audio and its
                // places in the playlists.
                if !c.replace {
                    skipped_dbids.insert(c.dbid);
                }
                continue;
            }
            if let Err(e) = copy_one(&mut db, c, now, &mut w.doomed) {
                stopped = Some(e.to_string());
                break;
            }
            if c.replace {
                expect.recopied.insert(c.dbid);
            } else {
                expect.added.push(c.dbid);
            }
            upserts.push(copied_mapping(c));
            bytes += c.audio.size;
            since += 1;
            let more = i + 1 < t.copies.len();
            if more && (since >= CHECKPOINT_COPIES || last.elapsed().as_secs() >= CHECKPOINT_SECS) {
                w.write(&mut db, &expect, progress)?;
                record(&mut upserts, &mut removed_dbids)?;
                since = 0;
                last = std::time::Instant::now();
            }
        }
        progress(Progress::Copying {
            done: expect.added.len() + expect.recopied.len(),
            total: t.copies.len(),
            bytes,
            bytes_total,
            name: String::new(),
        });
        if stopped.is_some() {
            // What was done is kept: written, verified and recorded.
            w.write(&mut db, &expect, progress)?;
            record(&mut upserts, &mut removed_dbids)?;
            return Ok((stopped, 0));
        }

        for r in &t.retags {
            db.set_track_fields(r.dbid, &r.fields.to_gpod())
                .map_err(|e| w.undo(e.into()))?;
            let old = prepared
                .ipod
                .tracks
                .iter()
                .find(|x| x.adopt.dbid == r.dbid)
                .map(|x| x.tags.clone())
                .unwrap_or_default();
            expect.retagged.insert(r.dbid, r.fields.applied_to(&old));
            let m = prepared.tracks_map.iter().find(|m| m.dbid == r.dbid);
            if let Some(m) = m {
                upserts.push(TrackMapping {
                    synced_tags_hash: r.tags_hash,
                    ..m.clone()
                });
            }
        }

        // New tracks skipped above are left out of the playlists for now.
        let keep = |m: &Vec<u64>| -> Vec<u64> {
            m.iter()
                .copied()
                .filter(|d| !skipped_dbids.contains(d))
                .collect()
        };
        let ops: Vec<Op> = work
            .playlists
            .ops
            .iter()
            .map(|op| match op {
                Op::Create {
                    playlist_id,
                    name,
                    members,
                } => Op::Create {
                    playlist_id: *playlist_id,
                    name: name.clone(),
                    members: keep(members),
                },
                Op::SetMembers {
                    ipod_id,
                    name,
                    members,
                } => Op::SetMembers {
                    ipod_id: *ipod_id,
                    name: name.clone(),
                    members: keep(members),
                },
                other => other.clone(),
            })
            .collect();
        let created = apply(&mut db, &ops).map_err(|e| w.undo(e))?;
        expect.managed = work
            .playlists
            .managed
            .iter()
            .map(|t| ExpectedPlaylist {
                ipod_id: t
                    .ipod_id
                    .or_else(|| created.get(&t.playlist_id).copied())
                    .unwrap_or(0),
                name: t.name.clone(),
                members: keep(&t.members),
            })
            .collect();
        expect.deleted = work.playlists.deleted.clone();
        expect.created = created.len();
        w.write(&mut db, &expect, progress)?;
        record(&mut upserts, &mut removed_dbids)?;
        let new_mappings: Vec<PlaylistMapping> = expect
            .managed
            .iter()
            .zip(&work.playlists.managed)
            .map(|(m, t)| PlaylistMapping {
                playlist_id: t.playlist_id,
                ipod_id: Some(m.ipod_id),
                ipod_name: m.name.clone(),
            })
            .collect();
        crate::store::savepoint(conn, || {
            if !work.keep_playlist_mappings {
                devices::replace_playlist_mappings(conn, device, &new_mappings)?;
            }
            devices::set_last_sync(conn, device, now_unix())
        })?;
        Ok((None, work.playlists.ops.len()))
    })();
    db.stop_sync();
    drop(db);
    let (stopped, applied) = result?;

    // The sync is done; what follows only reports a warning on failure.
    let mut warnings = std::mem::take(&mut w.warnings);
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
        applied,
        copied: expect.added.len(),
        recopied: expect.recopied.len(),
        removed: expect.removed.len(),
        retagged: expect.retagged.len(),
        bytes: work
            .tracks
            .copies
            .iter()
            .filter(|c| expect.added.contains(&c.dbid) || expect.recopied.contains(&c.dbid))
            .map(|c| c.audio.size)
            .sum(),
        left_out: work.playlists.left_out,
        pulled,
        recovered: recovery.undone,
        backup,
        pruned,
        stopped,
        skipped,
        warnings,
    })
}

/// Applies `work`, playlist changes only (M4), to the iPod at
/// `mountpoint`. See [`sync`].
#[allow(clippy::too_many_arguments)]
pub fn sync_playlists(
    conn: &Connection,
    cfg: &Config,
    paths: &Paths,
    lock: &crate::lock::IpodLock,
    mountpoint: &Path,
    db: gpod::Database,
    prepared: &Prepared,
    work: &Work,
    opts: Options,
) -> Result<Outcome> {
    let full = FullWork {
        tracks: TrackWork::default(),
        playlists: work.clone(),
        keep_playlist_mappings: false,
    };
    sync(
        conn,
        cfg,
        paths,
        lock,
        mountpoint,
        db,
        prepared,
        &full,
        opts,
        &|_| {},
        &AtomicBool::new(false),
    )
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
    /// Track mappings dropped because their library track is gone.
    pub dropped_tracks: usize,
    /// iPod playlists the backup brings back whose library playlist is gone:
    /// the next sync deletes them again.
    pub playlists_to_delete: usize,
    /// The backup of what the restore replaced.
    pub saved: Backup,
    /// Statistics pulled from the replaced database first.
    pub pulled: Pulled,
    /// Files taken off the iPod once saved, see [`backup::SET_ASIDE`].
    pub set_aside: Vec<&'static str>,
}

/// The backup folder name for the iPod whose GUID is `guid` (as read by
/// [`device::firewire_guid`]), matched without regard to case or a `0x`
/// prefix. Without a GUID the iPod cannot be told apart from another one,
/// and it is refused: the caller names the folder instead.
pub fn backup_guid(paths: &Paths, guid: Option<&str>) -> Result<String> {
    let Some(guid) = guid else {
        return Err(Error::RestoreNeedsGuid);
    };
    let root = paths.data_dir.join("backups");
    let norm = device::normalize_guid;
    let names: Vec<String> = match std::fs::read_dir(&root) {
        Ok(d) => d
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| !n.starts_with('.'))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(source) => return Err(Error::Io { path: root, source }),
    };
    let found: Vec<&String> = names.iter().filter(|n| norm(n) == norm(guid)).collect();
    match found[..] {
        [one] => Ok(one.clone()),
        [] => Err(Error::NoSuchBackup(format!("any, for the iPod {guid}"))),
        _ => Err(Error::NoSuchBackup(format!(
            "folder for {guid}: several match"
        ))),
    }
}

fn bad_backup(stamp: &str, why: impl std::fmt::Display) -> Error {
    Error::BadBackup {
        stamp: stamp.to_owned(),
        why: why.to_string(),
    }
}

/// Puts backup `stamp` of the iPod `guid` (a backup folder name, see
/// [`backup_guid`]) back on the iPod at `mountpoint` and returns the
/// device's mappings to what they were when it was taken. In order:
/// - the backup is read and its database parsed; nothing changes if either
///   fails;
/// - what the iPod holds now is backed up, with the current mappings;
/// - when the current database can be read, its statistics are pulled as a
///   sync would; when it cannot and a `Play Counts` file is there, the
///   restore is refused unless `discard_play_counts`;
/// - `Play Counts` and `OTGPlaylistInfo`, which index the current database
///   by position, are taken off the iPod (they are in that backup);
/// - the files are put back, read back and compared with the backup's
///   database; the mappings are replaced last. From the first change to
///   the iPod until then, the device is marked as being restored and a sync
///   refuses to run, so a restore stopped part way is run again.
///
/// Mappings of library tracks gone since the backup are dropped. A
/// playlist mapping whose library playlist is gone becomes a tombstone, so
/// the next sync deletes the iPod playlist again. Files copied to the iPod
/// after the backup stay as orphans for `iprune doctor`.
pub fn restore(
    conn: &Connection,
    paths: &Paths,
    mountpoint: &Path,
    guid: &str,
    stamp: &str,
    discard_play_counts: bool,
) -> Result<Restored> {
    let _lock = crate::lock::ipod(paths)?;
    let backups = paths.data_dir.join("backups");
    let backup = backup::list(&backups, guid)?
        .into_iter()
        .find(|b| b.stamp == stamp)
        .ok_or_else(|| Error::NoSuchBackup(stamp.to_owned()))?;
    let path = backup.dir.join("mappings.json");
    let text = std::fs::read(&path).map_err(|e| bad_backup(stamp, e))?;
    let saved: SavedMappings = serde_json::from_slice(&text)
        .map_err(|e| bad_backup(stamp, format!("mappings.json: {e}")))?;
    // Parsed from a copy on its own: libgpod applies a `Play Counts` it
    // finds next to the file, and the backup may hold one.
    let expected = {
        let lone = tempfile::tempdir_in(&paths.data_dir).map_err(|source| Error::Io {
            path: paths.data_dir.clone(),
            source,
        })?;
        let copy = lone.path().join("iTunesDB");
        std::fs::copy(backup.dir.join("iTunesDB"), &copy).map_err(|e| bad_backup(stamp, e))?;
        gpod::Database::open_file(&copy)
            .map_err(|e| bad_backup(stamp, format!("iTunesDB: {e}")))
            .and_then(|db| snapshot(&db))?
    };
    device::check_writable(mountpoint)?;
    let device =
        devices::device_id(conn, guid)?.ok_or_else(|| Error::NotAdopted(guid.to_owned()))?;

    // What the iPod holds now, before anything changes.
    let current = gpod::Database::open(mountpoint).ok();
    let live_counts = mountpoint.join("iPod_Control/iTunes/Play Counts");
    if current.is_none()
        && !discard_play_counts
        && std::fs::metadata(&live_counts).is_ok_and(|m| m.len() > 0)
    {
        return Err(Error::PlayCountsUnread);
    }
    let current_tracks = devices::track_mappings(conn, device)?;
    let current_playlists = devices::playlist_mappings(conn, device)?;
    let mut extra: Vec<(&str, Vec<u8>)> = vec![(
        "mappings.json",
        json(&MappingsFile {
            tracks: &current_tracks,
            playlists: &current_playlists,
        }),
    )];
    if let Some(db) = &current {
        extra.push(("snapshot.txt", snapshot(db)?.into_bytes()));
    }
    let extra: Vec<(&str, &[u8])> = extra.iter().map(|(n, b)| (*n, &b[..])).collect();
    let kept = backup::create_before_restore(&backups, guid, mountpoint, now_unix(), &extra)?;

    // The plays on the iPod belong to the current database's tracks.
    let pulled = match current {
        Some(db) => {
            let play_counts = stats::play_counts(mountpoint)?;
            stats::recover(conn, device, play_counts)?;
            let contents = ipod::read(&db);
            drop(db);
            stats::pull(conn, device, &current_tracks, &contents, play_counts.live)?
        }
        None => Pulled::default(),
    };

    let track_ids: std::collections::HashSet<i64> = crate::store::all_tracks(conn)?
        .into_iter()
        .map(|t| t.id)
        .collect();
    let playlist_ids: std::collections::HashSet<i64> = conn
        .prepare("SELECT id FROM playlists")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let saved_tracks = saved.tracks.len();
    let tracks: Vec<TrackMapping> = saved
        .tracks
        .into_iter()
        .filter(|m| track_ids.contains(&m.track_id))
        .collect();
    let (playlists, gone): (Vec<PlaylistMapping>, Vec<PlaylistMapping>) = saved
        .playlists
        .into_iter()
        .partition(|m| playlist_ids.contains(&m.playlist_id));

    crate::store::savepoint(conn, || devices::set_restoring(conn, device, Some(stamp)))?;
    // The pull stands from here: its `Play Counts` is gone from the iPod.
    let set_aside = backup::set_aside(&kept, mountpoint)?;
    stats::settle(conn, device)?;
    let files = backup::restore_files(&backup, mountpoint)?;
    let reread = gpod::Database::open(mountpoint).map_err(|e| Error::RestoreVerification {
        problems: vec![format!("the restored database cannot be read: {e}")],
        stamp: stamp.to_owned(),
        saved: kept.stamp.clone(),
    })?;
    let diffs = verify::differences(
        &verify::without_artwork(&expected),
        &verify::without_artwork(&snapshot(&reread)?),
        20,
    );
    drop(reread);
    if !diffs.is_empty() {
        return Err(Error::RestoreVerification {
            problems: diffs,
            stamp: stamp.to_owned(),
            saved: kept.stamp.clone(),
        });
    }
    let playlists_to_delete = gone.len();
    crate::store::savepoint(conn, || {
        let mut all = playlists.clone();
        for m in &gone {
            all.push(PlaylistMapping {
                playlist_id: crate::store::insert_tombstone(conn, &m.ipod_name)?,
                ..m.clone()
            });
        }
        devices::replace_mappings(conn, device, &tracks, &all)?;
        devices::set_restoring(conn, device, None)
    })?;
    Ok(Restored {
        files,
        tracks: tracks.len(),
        playlists: playlists.len(),
        dropped_tracks: saved_tracks - tracks.len(),
        playlists_to_delete,
        saved: kept,
        pulled,
        set_aside,
    })
}
