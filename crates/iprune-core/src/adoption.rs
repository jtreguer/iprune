//! Running adoption: read the iPod and the index, match them, write the
//! report and, when asked, save the mapping. The iPod is only read.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use rusqlite::Connection;

use crate::adopt::{self, Adoption, PlaylistAdoption, Proposal, Reason};
use crate::ipod::{self, Contents};
use crate::store::devices::{self, TrackMapping};
use crate::store::{self, TrackRow};
use crate::{Error, Result};

pub struct Run {
    pub ipod: Contents,
    pub adoption: Adoption,
    /// Final `(dbid, library id)` pairs after resolving proposals.
    pub mapping: Vec<(u64, i64)>,
    /// Proposals not resolved; their iPod tracks stay unmanaged.
    pub left: Vec<Proposal>,
    pub playlists: PlaylistAdoption,
    /// Present library tracks, by id.
    pub library: HashMap<i64, TrackRow>,
    /// iPod files read to compare them with the library.
    pub compared: usize,
    /// `(dbid, why)` for iPod files that could not be read for the comparison.
    pub unreadable: Vec<(u64, String)>,
}

/// Opens an iPod file for reading without updating its access time, so
/// reading leaves the device untouched. `O_NOATIME` needs the caller to own
/// the file (on vfat, the `uid=` mount option); otherwise a plain open.
fn open_untouched(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOATIME)
        .open(path)
    {
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => std::fs::File::open(path),
        r => r,
    }
}

/// Hashes the iPod files of `dbids`, one at a time: the iPod is a single
/// disk. Reports `(done, total)` after each file.
fn hash_ipod_files(
    mountpoint: &Path,
    tracks: &mut [adopt::IpodTrack],
    ipod: &Contents,
    dbids: &[u64],
    progress: &dyn Fn(usize, usize),
) -> Vec<(u64, String)> {
    let paths: HashMap<u64, Option<&str>> = ipod
        .tracks
        .iter()
        .map(|t| (t.adopt.dbid, t.ipod_path.as_deref()))
        .collect();
    let wanted: std::collections::HashSet<u64> = dbids.iter().copied().collect();
    let mut unreadable = Vec::new();
    let mut done = 0;
    for t in tracks.iter_mut().filter(|t| wanted.contains(&t.dbid)) {
        let file = paths[&t.dbid].and_then(|p| ipod::file_path(mountpoint, p));
        match file.map(|f| open_untouched(&f).and_then(crate::scan::hash_reader)) {
            Some(Ok(h)) => t.hash = Some(h),
            Some(Err(e)) => unreadable.push((t.dbid, e.to_string())),
            None => unreadable.push((t.dbid, "no usable path in the iPod database".into())),
        }
        done += 1;
        progress(done, dbids.len());
    }
    unreadable
}

/// Matches the iPod in `db` with the index. When the database comes from a
/// mounted iPod, the files the tags cannot settle are read and compared with
/// the library; `progress` reports `(done, total)` files. The iPod is only
/// read.
pub fn run(
    conn: &Connection,
    db: &gpod::Database,
    accept_proposals: bool,
    progress: &dyn Fn(usize, usize),
) -> Result<Run> {
    let library: HashMap<i64, TrackRow> = store::all_tracks(conn)?
        .into_iter()
        .filter(|t| t.missing_since.is_none())
        .map(|t| (t.id, t))
        .collect();
    if library.is_empty() {
        return Err(Error::EmptyIndex);
    }
    let mut lib: Vec<adopt::LibTrack> = library
        .values()
        .map(|t| adopt::LibTrack {
            id: t.id,
            size: t.size,
            duration_ms: t.info.duration_ms,
            // Without a title tag, iTunes and other players use the file name,
            // and that is what the iPod got.
            title: Some(t.display_title().into_owned()),
            artist: t.tags.artist.clone(),
            album: t.tags.album.clone(),
            hash: t.hash,
        })
        .collect();
    lib.sort_by_key(|t| t.id);
    let ipod = ipod::read(db);
    let mut tracks: Vec<adopt::IpodTrack> = ipod.tracks.iter().map(|t| t.adopt.clone()).collect();
    let mut adoption = adopt::adopt(&tracks, &lib);
    let (mut compared, mut unreadable) = (0, Vec::new());
    if let Some(mountpoint) = db.mountpoint() {
        let dbids = adopt::to_compare(&adoption, &tracks, &lib);
        if !dbids.is_empty() {
            unreadable = hash_ipod_files(&mountpoint, &mut tracks, &ipod, &dbids, progress);
            compared = dbids.len() - unreadable.len();
            adoption = adopt::adopt(&tracks, &lib);
        }
    }
    let (mapping, left) = adopt::resolve(&adoption, accept_proposals);
    let left = left.into_iter().cloned().collect();
    let playlists = adopt::match_playlists(
        &ipod.playlists,
        &devices::library_playlists(conn)?,
        &mapping,
    );
    Ok(Run {
        ipod,
        adoption,
        mapping,
        left,
        playlists,
        library,
        compared,
        unreadable,
    })
}

/// Saves the mapping for the device with `guid`. Refuses to replace an
/// earlier adoption unless `force` is set.
pub fn save(conn: &Connection, r: &Run, guid: &str, force: bool) -> Result<()> {
    let by_dbid: HashMap<u64, &ipod::Track> =
        r.ipod.tracks.iter().map(|t| (t.adopt.dbid, t)).collect();
    store::savepoint(conn, || {
        if let Some(id) = devices::device_id(conn, guid)?
            && !force
            && devices::mapped_track_count(conn, id)? > 0
        {
            return Err(Error::AlreadyAdopted(guid.to_owned()));
        }
        let device =
            devices::upsert_device(conn, guid, r.ipod.name.as_deref(), r.ipod.model.as_deref())?;
        let tracks: Vec<TrackMapping> = r
            .mapping
            .iter()
            .map(|&(dbid, track_id)| {
                let t = by_dbid[&dbid];
                TrackMapping {
                    track_id,
                    dbid,
                    // The iPod's values, so the first sync updates only the
                    // tags that really differ from the file.
                    synced_tags_hash: t.tags.hash(),
                    synced_size: t.adopt.size,
                }
            })
            .collect();
        let playlists: Vec<(i64, String)> = r
            .playlists
            .matched
            .iter()
            .map(|m| (m.playlist_id, m.ipod_name.clone()))
            .collect();
        devices::replace_mappings(conn, device, &tracks, &playlists)
    })
}

fn reason(r: Reason) -> &'static str {
    match r {
        Reason::WeakTags => "same size and duration, title or artist differ",
        Reason::SeveralCandidates => "several identical library files",
        Reason::SharedWithAnotherIpodTrack => {
            "another iPod track matched the same file (duplicate on the iPod)"
        }
        Reason::TagsOnly => "same artist, title and album, different size",
    }
}

/// The full adoption report, for `adopt-report.txt`.
pub fn report(r: &Run) -> String {
    let ipod: HashMap<u64, &ipod::Track> =
        r.ipod.tracks.iter().map(|t| (t.adopt.dbid, t)).collect();
    let ipod_line = |dbid: u64| {
        let t = ipod[&dbid];
        format!(
            "{dbid:016x} {} - {} [{}] {} bytes, {} ms, {}",
            t.adopt.artist.as_deref().unwrap_or("?"),
            t.adopt.title.as_deref().unwrap_or("?"),
            t.adopt.album.as_deref().unwrap_or("?"),
            t.adopt.size,
            t.adopt.duration_ms,
            t.ipod_path.as_deref().unwrap_or("?")
        )
    };
    let lib_line = |id: i64| {
        let t = &r.library[&id];
        format!(
            "{} - {} [{}] {} bytes, {} ms, {}",
            t.tags.artist.as_deref().unwrap_or("?"),
            t.display_title(),
            t.tags.album.as_deref().unwrap_or("?"),
            t.size,
            t.info.duration_ms.map_or("?".into(), |d| d.to_string()),
            t.path.display()
        )
    };
    let mapped: HashMap<u64, i64> = r.mapping.iter().copied().collect();
    let mut out = String::new();
    let w = &mut out;
    let _ = writeln!(w, "iPod: {}", r.ipod.name.as_deref().unwrap_or("?"));
    let _ = writeln!(w, "model: {}", r.ipod.model.as_deref().unwrap_or("unknown"));
    let _ = writeln!(
        w,
        "FireWire GUID: {}",
        r.ipod.firewire_guid.as_deref().unwrap_or("unknown")
    );
    let _ = writeln!(w);
    let _ = writeln!(w, "{}", summary(r));
    let _ = writeln!(w, "\n== Proposals ({})", r.adoption.proposals.len());
    for p in &r.adoption.proposals {
        let outcome = match mapped.get(&p.dbid) {
            Some(id) => format!("mapped to {}", r.library[id].path.display()),
            None => "left unmanaged".to_owned(),
        };
        let _ = writeln!(
            w,
            "\n{}\n  why: {}\n  outcome: {outcome}",
            ipod_line(p.dbid),
            reason(p.reason)
        );
        for id in &p.candidates {
            let mark = if p.default == Some(*id) { "*" } else { " " };
            let _ = writeln!(w, "  {mark} {}", lib_line(*id));
        }
    }
    if !r.unreadable.is_empty() {
        let _ = writeln!(
            w,
            "\n== iPod files that could not be read for the comparison ({})",
            r.unreadable.len()
        );
        for (dbid, why) in &r.unreadable {
            let _ = writeln!(w, "  {}\n    {why}", ipod_line(*dbid));
        }
    }
    let _ = writeln!(
        w,
        "\n== iPod tracks with no library file ({}), left on the iPod unmanaged",
        r.adoption.unmatched_ipod.len()
    );
    for d in &r.adoption.unmatched_ipod {
        let _ = writeln!(w, "  {}", ipod_line(*d));
    }
    let _ = writeln!(
        w,
        "\n== Library files not on the iPod ({}), copied by the next sync",
        r.adoption.unmatched_library.len()
    );
    for id in &r.adoption.unmatched_library {
        let _ = writeln!(w, "  {}", lib_line(*id));
    }
    let pl = &r.playlists;
    let differ: Vec<_> = pl.matched.iter().filter(|m| !m.same_contents).collect();
    let _ = writeln!(
        w,
        "\n== Playlists whose iPod contents differ from the file ({})",
        differ.len()
    );
    for m in differ {
        let _ = writeln!(w, "  {}", m.library_name);
    }
    let _ = writeln!(
        w,
        "\n== iPod playlists with no file ({}), kept on the iPod",
        pl.unmatched_ipod.len()
    );
    for n in &pl.unmatched_ipod {
        let _ = writeln!(w, "  {n}");
    }
    let _ = writeln!(
        w,
        "\n== Playlist files not on the iPod ({}), created by the next sync",
        pl.unmatched_library.len()
    );
    for n in &pl.unmatched_library {
        let _ = writeln!(w, "  {n}");
    }
    out
}

/// A few lines for the terminal.
pub fn summary(r: &Run) -> String {
    let a = &r.adoption;
    let mut by_reason: HashMap<Reason, usize> = HashMap::new();
    for p in &a.proposals {
        *by_reason.entry(p.reason).or_default() += 1;
    }
    let mut reasons: Vec<_> = by_reason.into_iter().collect();
    reasons.sort();
    let mut s = format!(
        "iPod tracks: {}\n  matched: {} ({} by identical file)\n  proposals: {}",
        r.ipod.tracks.len(),
        a.accepted.len(),
        a.identical.len(),
        a.proposals.len()
    );
    for (reason_, n) in reasons {
        let _ = write!(s, "\n    {n}: {}", reason(reason_));
    }
    let _ = write!(
        s,
        "\n  no library file: {}\nlibrary tracks not on the iPod: {}\nmapping after proposals: {} tracks, {} left unmanaged\nplaylists: {} matched ({} differ), {} only on the iPod, {} only in files, {} smart playlists untouched",
        a.unmatched_ipod.len(),
        a.unmatched_library.len(),
        r.mapping.len(),
        r.left.len() + a.unmatched_ipod.len(),
        r.playlists.matched.len(),
        r.playlists
            .matched
            .iter()
            .filter(|m| !m.same_contents)
            .count(),
        r.playlists.unmatched_ipod.len(),
        r.playlists.unmatched_library.len(),
        r.ipod.smart_playlists,
    );
    if r.compared + r.unreadable.len() > 0 {
        let _ = write!(
            s,
            "\niPod files compared with the library: {} ({} unreadable)",
            r.compared,
            r.unreadable.len()
        );
    }
    s
}
