//! Preparing a sync: gather what the planner needs from the index and the
//! iPod, run it, and describe the plan. Writing to the iPod comes later (M4).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use rusqlite::Connection;

use crate::config::{self, Config};
use crate::device;
use crate::error::IoContext;
use crate::ipod::{self, Contents};
use crate::plan::{
    self, EntryReason, Field, Member, Plan, PlanInput, PlaylistDeleteReason, PlaylistOp,
    PlaylistSkipReason, RemoveReason, Scope, SkipReason, Space,
};
use crate::store::{self, TrackRow, devices};
use crate::{Error, Result};

pub struct Prepared {
    pub plan: Plan,
    pub ipod: Contents,
    pub guid: String,
    /// The device's mappings the plan was made from.
    pub tracks_map: Vec<devices::TrackMapping>,
    pub playlists_map: Vec<devices::PlaylistMapping>,
    /// Every indexed track, by id, for describing the plan.
    pub library: HashMap<i64, TrackRow>,
    /// Mappings taken back from tracks a crashed sync left on the iPod.
    pub recovered: usize,
    /// What the iPod was when it was read, checked again before a write;
    /// `None` for a database read from a file.
    pub identity: Option<Identity>,
}

/// What an iPod is at one moment: its database and `Play Counts` files and
/// the GUID in its device files. A write refuses an iPod that is no longer
/// the one read, such as one swapped or played while a prompt waited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    itunesdb: [u8; 32],
    play_counts: Option<[u8; 32]>,
    guid: Option<String>,
}

/// The [`Identity`] of the iPod at `mountpoint` now.
pub fn identity(mountpoint: &Path) -> Result<Identity> {
    let db = device::itunesdb(mountpoint);
    let bytes = std::fs::read(&db).at(&db)?;
    Ok(Identity {
        itunesdb: *blake3::hash(&bytes).as_bytes(),
        play_counts: crate::stats::play_counts(mountpoint)?.live,
        guid: device::firewire_guid(mountpoint),
    })
}

/// Refuses when the iPod at `mountpoint` is no longer `then`.
pub fn check_identity(mountpoint: &Path, then: &Identity) -> Result<()> {
    if identity(mountpoint)? != *then {
        return Err(Error::IpodChanged);
    }
    Ok(())
}

/// Size and free space of the filesystem holding `path`.
pub fn space(path: &Path) -> Result<Space> {
    let s = rustix::fs::statvfs(path)
        .map_err(std::io::Error::from)
        .at(path)?;
    Ok(Space {
        capacity: s.f_blocks * s.f_frsize,
        free: s.f_bavail * s.f_frsize,
    })
}

/// Plans a sync of the iPod in `db`, which must have been adopted.
pub fn prepare(conn: &Connection, cfg: &Config, db: &gpod::Database) -> Result<Prepared> {
    let ipod = ipod::read(db);
    let guid = ipod.firewire_guid.clone().ok_or(Error::NoFirewireGuid)?;
    let device = match devices::device_id(conn, &guid)? {
        // An empty iPod, freshly restored, has nothing to copy twice.
        Some(id) if ipod.tracks.is_empty() || devices::mapped_track_count(conn, id)? > 0 => id,
        // Without a mapping every track would be copied a second time.
        Some(_) => {
            return Err(Error::NothingMatched {
                guid,
                tracks: ipod.tracks.len(),
            });
        }
        None => return Err(Error::NotAdopted(guid)),
    };
    if let Some(stamp) = devices::restoring(conn, device)? {
        return Err(Error::RestoreUnfinished(stamp));
    }
    let library = store::all_tracks(conn)?;
    let playlists = devices::library_playlists(conn)?;
    let removed = store::removed_playlists(conn)?;
    let mut tracks_map = devices::track_mappings(conn, device)?;
    // Tracks a crashed sync copied but did not record are taken back.
    let lost = crate::transfer::lost_mappings(&guid, &library, &ipod, &tracks_map);
    let recovered = lost.len();
    if !lost.is_empty() {
        crate::store::savepoint(conn, || {
            devices::apply_track_changes(conn, device, &lost, &[])
        })?;
        tracks_map = devices::track_mappings(conn, device)?;
    }
    let playlists_map = devices::playlist_mappings(conn, device)?;
    let identity = db.mountpoint().map(|m| identity(&m)).transpose()?;
    let space = db.mountpoint().map(|m| space(&m)).transpose()?;
    // Entries whose file is gone, such as those a restore brought back after
    // a sync deleted their files: mapped ones in scope are copied again.
    let files_missing: std::collections::HashSet<u64> = match db.mountpoint() {
        Some(mp) => ipod
            .tracks
            .iter()
            .filter(|t| {
                t.ipod_path
                    .as_deref()
                    .and_then(|p| ipod::file_path(&mp, p))
                    .is_none_or(|f| !f.is_file())
            })
            .map(|t| t.adopt.dbid)
            .collect(),
        None => Default::default(),
    };
    let (selection, own) = device_scope(conn, cfg, device)?;
    if let Some(why) = scope_problem(&selection, own, &library, &playlists) {
        return Err(Error::BadScope(if own {
            format!("{why}; change it in the window or with `iprune scope`")
        } else {
            why
        }));
    }
    let scope = selection.as_plan();
    let plan = plan::plan(&PlanInput {
        library: &library,
        playlists: &playlists,
        removed_playlists: &removed,
        device: &ipod,
        tracks_map: &tracks_map,
        playlists_map: &playlists_map,
        scope,
        remove_unmanaged: cfg.sync.remove_unmanaged,
        space,
        files_missing: &files_missing,
    });
    Ok(Prepared {
        plan,
        ipod,
        guid,
        tracks_map,
        playlists_map,
        recovered,
        library: library.into_iter().map(|t| (t.id, t)).collect(),
        identity,
    })
}

/// What the iPod `device` syncs: the selection set for it, else `[sync]`
/// in the config. The flag is true for one set for the iPod.
pub fn device_scope(
    conn: &Connection,
    cfg: &Config,
    device: i64,
) -> Result<(devices::DeviceScope, bool)> {
    Ok(match devices::scope(conn, device)? {
        Some(s) => (s, true),
        None => (
            devices::DeviceScope {
                kind: cfg.sync.scope,
                playlists: cfg.sync.playlists.clone(),
            },
            false,
        ),
    })
}

impl devices::DeviceScope {
    /// The planner's form of this selection.
    pub fn as_plan(&self) -> Scope<'_> {
        match self.kind {
            config::Scope::All => Scope::All,
            config::Scope::Playlists => Scope::Playlists(&self.playlists),
            config::Scope::Except => Scope::Except(&self.playlists),
        }
    }

    /// The selection in words: "the whole library", "only 2 playlists: A,
    /// B", "the whole library except 1 playlist: C".
    pub fn describe(&self) -> String {
        let list = |n: &[String]| {
            let noun = if n.len() == 1 {
                "playlist"
            } else {
                "playlists"
            };
            format!("{} {noun}: {}", n.len(), n.join(", "))
        };
        match self.kind {
            config::Scope::All => "the whole library".into(),
            config::Scope::Playlists => format!("only {}", list(&self.playlists)),
            config::Scope::Except if self.playlists.is_empty() => "the whole library".into(),
            config::Scope::Except => format!("the whole library except {}", list(&self.playlists)),
        }
    }

    /// How many present tracks this selection sends, and their size.
    pub fn size(
        &self,
        library: &[TrackRow],
        playlists: &[crate::adopt::LibPlaylist],
    ) -> (usize, u64) {
        let ids = plan::in_scope(library, playlists, self.as_plan());
        let bytes = library
            .iter()
            .filter(|t| ids.contains(&t.id))
            .map(|t| t.size)
            .sum();
        (ids.len(), bytes)
    }
}

/// Why a selection cannot be synced, if it cannot: it names a playlist
/// that does not exist, or selects no present track, which would plan the
/// removal of every managed track. `own` is true for
/// a selection set for the iPod, false for the config's.
pub fn scope_problem(
    s: &devices::DeviceScope,
    own: bool,
    library: &[TrackRow],
    playlists: &[crate::adopt::LibPlaylist],
) -> Option<String> {
    if s.kind == config::Scope::All {
        return None;
    }
    if s.kind == config::Scope::Playlists && s.playlists.is_empty() {
        return Some(if own {
            "this iPod's selection keeps only some playlists but names none".into()
        } else {
            "sync.scope is \"playlists\" but sync.playlists is empty".into()
        });
    }
    let unknown: Vec<&str> = s
        .playlists
        .iter()
        .filter(|n| !playlists.iter().any(|p| &p.name == *n))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        let unknown = unknown.join(", ");
        return Some(if own {
            format!("this iPod's selection names playlists that do not exist: {unknown}")
        } else {
            format!("sync.playlists names playlists that do not exist: {unknown}")
        });
    }
    if !plan::in_scope(library, playlists, s.as_plan()).is_empty() {
        return None;
    }
    Some(match (s.kind, own) {
        (config::Scope::Except, true) => {
            "this iPod's selection leaves out every library track".into()
        }
        (config::Scope::Except, false) => {
            "the playlists in sync.playlists hold every library track, which \"except\" leaves out"
                .into()
        }
        (_, true) => "the playlists this iPod keeps hold no library track".into(),
        (_, false) => "the playlists in sync.playlists hold no library track".into(),
    })
}

/// Bytes in decimal units, as the iPod and file managers show them.
pub fn bytes(n: u64) -> String {
    match n {
        n if n >= 1_000_000_000 => format!("{:.1} GB", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.1} MB", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1} kB", n as f64 / 1e3),
        n => format!("{n} B"),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn field(f: Field) -> &'static str {
    match f {
        Field::Title => "title",
        Field::Artist => "artist",
        Field::Album => "album",
        Field::AlbumArtist => "album artist",
        Field::Composer => "composer",
        Field::Genre => "genre",
        Field::Year => "year",
        Field::TrackNumber => "track number",
        Field::TrackCount => "track count",
        Field::DiscNumber => "disc number",
        Field::DiscCount => "disc count",
        Field::Compilation => "compilation",
    }
}

/// The plan as text: a summary, and with `verbose` every change.
pub fn describe(p: &Prepared, verbose: bool) -> String {
    let plan = &p.plan;
    let on_ipod: HashMap<u64, &ipod::Track> =
        p.ipod.tracks.iter().map(|t| (t.adopt.dbid, t)).collect();
    let ipod_track = |dbid: u64| match on_ipod.get(&dbid) {
        Some(t) => format!(
            "{} - {}",
            t.adopt.artist.as_deref().unwrap_or("?"),
            t.adopt.title.as_deref().unwrap_or("?")
        ),
        None => format!("{dbid:016x}"),
    };
    let path = |id: i64| match p.library.get(&id) {
        Some(t) => t.path.display().to_string(),
        None => format!("track {id}"),
    };
    let mut s = String::new();
    let w = &mut s;
    let _ = writeln!(
        w,
        "iPod {:?} (FireWire GUID {})",
        p.ipod.name.as_deref().unwrap_or("?"),
        p.guid
    );
    let copy_bytes: u64 = plan.copy.iter().map(|c| c.size).sum();
    let _ = writeln!(
        w,
        "  + {} to copy ({})",
        plural(plan.copy.len(), "track", "tracks"),
        bytes(copy_bytes)
    );
    if !plan.replace.is_empty() {
        let _ = writeln!(
            w,
            "  * {} with new audio, or no file on the iPod, to copy again",
            plural(plan.replace.len(), "track", "tracks")
        );
    }
    let _ = writeln!(
        w,
        "  - {} to remove",
        plural(plan.remove.len(), "track", "tracks")
    );
    let _ = writeln!(
        w,
        "  ~ {} with changed tags",
        plural(plan.update_tags.len(), "track", "tracks")
    );
    let count = |f: fn(&PlaylistOp) -> bool| plan.playlists.iter().filter(|o| f(o)).count();
    let created = count(|o| matches!(o, PlaylistOp::Create { .. }));
    let changed = count(|o| matches!(o, PlaylistOp::SetMembers { .. }));
    let renamed = count(|o| matches!(o, PlaylistOp::Rename { .. }));
    let deleted = count(|o| matches!(o, PlaylistOp::Delete { .. }));
    let _ = writeln!(
        w,
        "  playlists: {created} new, {changed} changed, {renamed} renamed, {deleted} removed"
    );
    for t in &plan.takeovers {
        let _ = writeln!(
            w,
            "  takes over iPod playlist {:?}, not managed until now: {} of its {} taken out",
            t.name,
            plural(t.dropped, "track", "tracks"),
            t.before
        );
    }
    let _ = writeln!(
        w,
        "  unmanaged on the iPod, kept: {}, {}",
        plural(plan.unmanaged.tracks.len(), "track", "tracks"),
        plural(plan.unmanaged.playlists.len(), "playlist", "playlists")
    );
    if !plan.skipped.is_empty()
        || !plan.entries_skipped.is_empty()
        || !plan.playlists_skipped.is_empty()
    {
        let _ = writeln!(
            w,
            "  skipped: {}, {}, {}",
            plural(plan.skipped.len(), "track", "tracks"),
            plural(plan.playlists_skipped.len(), "playlist", "playlists"),
            plural(
                plan.entries_skipped.len(),
                "playlist entry",
                "playlist entries"
            )
        );
    }
    if !plan.stale_mappings.is_empty() {
        let _ = writeln!(
            w,
            "  mapped tracks no longer on the iPod: {}",
            plan.stale_mappings.len()
        );
    }
    let _ = writeln!(
        w,
        "  play counts to pull: {}",
        plural(plan.stats_to_pull, "track", "tracks")
    );
    // The peak is what must fit; the net is what is left once it is done.
    let signed = |n: i64| match n {
        n if n < 0 => format!("frees {}", bytes(n.unsigned_abs())),
        n => format!("needs {}", bytes(n as u64)),
    };
    let need = match (plan.space.peak, plan.space.needed) {
        (peak, net) if peak == net => signed(net),
        (peak, net) => format!("{} at the peak, {} once done", signed(peak), signed(net)),
    };
    let _ = match plan.space.device {
        Some(d) => writeln!(w, "  space: {need}, {} free", bytes(d.free)),
        None => writeln!(w, "  space: {need}"),
    };
    if !plan.space.fits {
        let _ = writeln!(w, "  THE PLAN DOES NOT FIT ON THE IPOD");
    }
    if !verbose {
        return s;
    }

    let section = |w: &mut String, title: &str, lines: Vec<String>| {
        if !lines.is_empty() {
            let _ = writeln!(w, "\n== {title} ({})", lines.len());
            for l in lines {
                let _ = writeln!(w, "  {l}");
            }
        }
    };
    section(
        w,
        "Copy",
        plan.copy.iter().map(|c| path(c.track_id)).collect(),
    );
    section(
        w,
        "Copy again (audio changed, or no file on the iPod)",
        plan.replace.iter().map(|r| path(r.track_id)).collect(),
    );
    section(
        w,
        "Remove",
        plan.remove
            .iter()
            .map(|r| {
                let why = match r.reason {
                    RemoveReason::MissingFromLibrary => "file gone from the library",
                    RemoveReason::OutOfScope => "no longer in the sync scope",
                    RemoveReason::Unmanaged => "not managed by iprune",
                };
                format!("{} ({why})", ipod_track(r.dbid))
            })
            .collect(),
    );
    section(
        w,
        "Tag changes",
        plan.update_tags
            .iter()
            .map(|t| {
                let changes: Vec<String> = t
                    .changes
                    .iter()
                    .map(|c| {
                        format!(
                            "{}: {:?} -> {:?}",
                            field(c.field),
                            c.ipod.as_deref().unwrap_or(""),
                            c.library
                        )
                    })
                    .collect();
                format!("{}\n    {}", path(t.track_id), changes.join("\n    "))
            })
            .collect(),
    );
    let members = |m: &[Member]| plural(m.len(), "track", "tracks");
    section(
        w,
        "Playlists",
        plan.playlists
            .iter()
            .map(|o| match o {
                PlaylistOp::Create {
                    name, members: m, ..
                } => {
                    format!("new {name:?}, {}", members(m))
                }
                PlaylistOp::SetMembers {
                    name, members: m, ..
                } => {
                    format!("set {name:?} to {}", members(m))
                }
                PlaylistOp::Rename { from, to, .. } => format!("rename {from:?} to {to:?}"),
                PlaylistOp::Delete { name, reason, .. } => {
                    let why = match reason {
                        PlaylistDeleteReason::FileDeleted => "file deleted",
                        PlaylistDeleteReason::OutOfScope => "no longer in the sync scope",
                        PlaylistDeleteReason::Unmanaged => "not managed by iprune",
                    };
                    format!("remove {name:?} ({why})")
                }
            })
            .collect(),
    );
    section(
        w,
        "Unmanaged tracks kept on the iPod",
        plan.unmanaged
            .tracks
            .iter()
            .map(|&d| ipod_track(d))
            .collect(),
    );
    section(
        w,
        "Unmanaged playlists kept on the iPod",
        plan.unmanaged.playlists.clone(),
    );
    section(
        w,
        "Skipped tracks",
        plan.skipped
            .iter()
            .map(|t| match t.reason {
                SkipReason::FairPlay => format!("{} (FairPlay protected)", t.path.display()),
                SkipReason::SampleRate => format!(
                    "{} (sampled above 48 kHz, which the iPod cannot play)",
                    t.path.display()
                ),
                SkipReason::TooLarge => format!(
                    "{} (4 GiB or more, which a FAT32 iPod cannot hold)",
                    t.path.display()
                ),
            })
            .collect(),
    );
    section(
        w,
        "Playlist files not synced",
        plan.playlists_skipped
            .iter()
            .map(|p| match p.reason {
                PlaylistSkipReason::SmartOnIpod => {
                    format!("{} (named like a smart playlist on the iPod)", p.name)
                }
            })
            .collect(),
    );
    section(
        w,
        "Skipped playlist entries",
        plan.entries_skipped
            .iter()
            .map(|e| {
                let why = match e.reason {
                    EntryReason::Unresolved => "matches no library file",
                    EntryReason::Missing => "file missing",
                    EntryReason::TrackSkipped => "track skipped",
                };
                format!("{} #{} ({why})", e.playlist, e.position)
            })
            .collect(),
    );
    section(
        w,
        "Mapped tracks no longer on the iPod",
        plan.stale_mappings
            .iter()
            .map(|m| path(m.track_id))
            .collect(),
    );
    s
}
