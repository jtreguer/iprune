//! Checks around a write: on the changed database before it is written, and
//! on the database read back from the iPod after.

use std::collections::{HashMap, HashSet};

use crate::ipod::{self, Contents};

/// A managed playlist as it must be after the write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedPlaylist {
    pub ipod_id: u64,
    pub name: String,
    pub members: Vec<u64>,
}

/// What a write is allowed to change. Everything else must be as it was.
#[derive(Debug, Clone, Default)]
pub struct Expect {
    /// Tracks removed from the database.
    pub removed: HashSet<u64>,
    /// Tracks added, in the order they were added.
    pub added: Vec<u64>,
    /// Tracks whose file was copied again (new audio): file, size and
    /// audio facts may change, and their tags are set too.
    pub recopied: HashSet<u64>,
    /// Tracks whose tags were set, with the values they must now have.
    pub retagged: HashMap<u64, crate::tags::Tags>,
    /// Every playlist iprune manages after the write. An empty list means no
    /// playlist was changed on purpose: each must be as before, less the
    /// removed tracks.
    pub managed: Vec<ExpectedPlaylist>,
    /// iPod playlists deleted.
    pub deleted: Vec<u64>,
    /// Playlists created.
    pub created: usize,
    /// The library file's size, for each track copied or copied again: its
    /// file on the iPod must have it (a USB error can leave it short).
    pub sizes: HashMap<u64, u64>,
}

/// Problems found in the changed database `after` before it is written.
/// `before` is the iPod as read at the start. With `files`, the iPod's
/// mountpoint, the files of added and recopied tracks must exist. Empty when
/// everything holds.
pub fn check(
    before: &Contents,
    after: &Contents,
    expect: &Expect,
    files: Option<&std::path::Path>,
) -> Vec<String> {
    let mut problems = Vec::new();
    // Each track as stored, without the count of playlists it is in, which
    // playlist changes are meant to change.
    let stored = |t: &ipod::Track| -> ipod::Track {
        let mut t = t.clone();
        t.adopt.playlists = 0;
        t
    };
    let was: HashMap<u64, &ipod::Track> = before.tracks.iter().map(|t| (t.adopt.dbid, t)).collect();
    let now_tracks: HashMap<u64, &ipod::Track> =
        after.tracks.iter().map(|t| (t.adopt.dbid, t)).collect();
    let added: HashSet<u64> = expect.added.iter().copied().collect();
    let mut wanted: HashSet<u64> = was
        .keys()
        .copied()
        .filter(|d| !expect.removed.contains(d))
        .collect();
    wanted.extend(added.iter().copied());
    let have: HashSet<u64> = now_tracks.keys().copied().collect();
    if have != wanted || now_tracks.len() != after.tracks.len() {
        problems.push(format!(
            "the tracks changed ({} before, {} now); {} expected, {} missing, {} unexpected",
            before.tracks.len(),
            after.tracks.len(),
            wanted.len(),
            wanted.difference(&have).count(),
            have.difference(&wanted).count()
        ));
    }
    for (dbid, old) in &was {
        let Some(new) = now_tracks.get(dbid) else {
            continue;
        };
        if expect.recopied.contains(dbid) {
            // New audio goes to a new file; the old one is deleted after.
            if new.ipod_path.is_some() && new.ipod_path == old.ipod_path {
                problems.push(format!(
                    "recopied track {dbid:016x} still uses its old file"
                ));
            }
        } else if let Some(tags) = expect.retagged.get(dbid) {
            if new.tags != *tags {
                problems.push(format!("track {dbid:016x} does not have the tags set"));
            }
            if (new.adopt.size, &new.ipod_path) != (old.adopt.size, &old.ipod_path) {
                problems.push(format!("track {dbid:016x} changed more than its tags"));
            }
        } else if stored(new) != stored(old) {
            problems.push(format!("the tracks changed: track {dbid:016x} differs"));
        }
    }
    for dbid in expect.added.iter().chain(expect.recopied.iter()) {
        let Some(t) = now_tracks.get(dbid) else {
            continue;
        };
        match (&t.ipod_path, files) {
            (None, _) => problems.push(format!("track {dbid:016x} has no file")),
            (Some(p), Some(root)) => match ipod::file_path(root, p).map(std::fs::metadata) {
                None | Some(Err(_)) => {
                    problems.push(format!("the file of track {dbid:016x} is missing"))
                }
                Some(Ok(m)) if !m.is_file() => {
                    problems.push(format!("the file of track {dbid:016x} is missing"))
                }
                Some(Ok(m)) => {
                    let len = m.len();
                    if let Some(&want) = expect.sizes.get(dbid)
                        && len != want
                    {
                        problems.push(format!(
                            "the file of track {dbid:016x} has {len} bytes, the library file {want}"
                        ));
                    }
                }
            },
            (Some(_), None) => {}
        }
    }
    let master: Vec<u64> = before
        .master_members
        .iter()
        .copied()
        .filter(|d| !expect.removed.contains(d))
        .chain(expect.added.iter().copied())
        .collect();
    if after.master_members != master {
        problems.push("the master playlist's members are not as expected".into());
    }
    let all_tracks: HashSet<u64> = after.tracks.iter().map(|t| t.adopt.dbid).collect();
    let now: HashMap<u64, &crate::adopt::IpodPlaylist> =
        after.playlists.iter().map(|p| (p.id, p)).collect();
    let managed_ids: HashSet<u64> = expect.managed.iter().map(|m| m.ipod_id).collect();
    let before_ids: HashSet<u64> = before.playlists.iter().map(|p| p.id).collect();
    let expected_count =
        (before.playlists.len() + expect.created).saturating_sub(expect.deleted.len());
    if after.playlists.len() != expected_count || now.len() != after.playlists.len() {
        problems.push(format!(
            "{} regular playlists, expected {expected_count}",
            after.playlists.len()
        ));
    }
    for p in &after.playlists {
        if !before_ids.contains(&p.id) && !managed_ids.contains(&p.id) {
            problems.push(format!("playlist {:?} appeared from nowhere", p.name));
        }
    }
    for m in &expect.managed {
        match now.get(&m.ipod_id) {
            None => problems.push(format!("playlist {:?} is missing", m.name)),
            Some(p) => {
                if p.name != m.name {
                    problems.push(format!("playlist {:?} is named {:?}", m.name, p.name));
                }
                if p.members != m.members {
                    problems.push(format!(
                        "playlist {:?} has {} members, not the {} expected or not in order",
                        m.name,
                        p.members.len(),
                        m.members.len()
                    ));
                }
            }
        }
    }
    for p in &after.playlists {
        if let Some(d) = p.members.iter().find(|d| !all_tracks.contains(d)) {
            problems.push(format!(
                "playlist {:?} holds {d:016x}, which is not in the database",
                p.name
            ));
        }
    }
    // Everything iprune does not manage is as it was, less removed tracks.
    for p in before
        .playlists
        .iter()
        .filter(|p| !managed_ids.contains(&p.id) && !expect.deleted.contains(&p.id))
    {
        let mut kept = p.clone();
        kept.members.retain(|d| !expect.removed.contains(d));
        if now.get(&p.id).is_none_or(|q| **q != kept) {
            problems.push(format!("unmanaged playlist {:?} changed", p.name));
        }
    }
    for id in &expect.deleted {
        if now.contains_key(id) {
            problems.push(format!("playlist {id:016x} was not deleted"));
        }
    }
    if after.smart_playlists != before.smart_playlists {
        problems.push("the smart playlists changed".into());
    }
    if after.name != before.name {
        problems.push("the iPod's name changed".into());
    }
    problems
}

/// The checks of a write that changes playlists only (M4): no track may
/// change. See [`check`].
pub fn pre_write(
    before: &Contents,
    after: &Contents,
    managed: &[ExpectedPlaylist],
    deleted: &[u64],
    created: usize,
) -> Vec<String> {
    let expect = Expect {
        managed: managed.to_vec(),
        deleted: deleted.to_vec(),
        created,
        ..Expect::default()
    };
    check(before, after, &expect, None)
}

/// A snapshot without the parts libgpod renumbers on every write: the
/// artwork link (`mhii`) of each track.
pub fn normalize(snapshot: &str) -> String {
    snapshot
        .lines()
        // From the end of a track line: a tag may contain " mhii ".
        .map(
            |l| match l.starts_with("T ").then(|| l.rfind(" mhii ")).flatten() {
                // Only the real field: a number, then the rating.
                Some(i) => {
                    let rest = &l[i + 6..];
                    let end = rest.find(' ').unwrap_or(rest.len());
                    let number =
                        !rest[..end].is_empty() && rest[..end].bytes().all(|b| b.is_ascii_digit());
                    if number && rest[end..].starts_with(" | rating ") {
                        format!("{}{}", &l[..i], &rest[end..])
                    } else {
                        l.to_owned()
                    }
                }
                None => l.to_owned(),
            },
        )
        .collect::<Vec<_>>()
        .join("\n")
}

/// `snapshot` with the members of the playlists at the positions in
/// `unordered` sorted, counting its `P` lines from 0 (the database's
/// playlist order). libgpod writes the Podcasts playlist grouped by show
/// through a hash table, so its order read back after a write is not the
/// order in memory; its members, as a multiset, must still match. Taken by
/// position, a regular playlist of the same name keeps its order checked.
pub fn sort_members(snapshot: &str, unordered: &[usize]) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut lines = snapshot.lines().peekable();
    let mut playlist = 0usize;
    while let Some(l) = lines.next() {
        out.push(l);
        if l.starts_with("P ") {
            let sorted = unordered.contains(&playlist);
            playlist += 1;
            if sorted {
                let mut members = Vec::new();
                while let Some(m) = lines.next_if(|m| m.starts_with("  ")) {
                    members.push(m);
                }
                members.sort_unstable();
                out.extend(members);
            }
        }
    }
    out.join("\n")
}

/// `snapshot` without each track's artwork fields (`art N mhii M`). A
/// database read from a file alone has no artwork, the same one read on the
/// iPod has its `ArtworkDB`.
pub fn without_artwork(snapshot: &str) -> String {
    snapshot
        .lines()
        // From the end: a tag may contain " | art ".
        .map(|l| match (l.rfind(" | art "), l.rfind(" | rating ")) {
            (Some(a), Some(r)) if l.starts_with("T ") && a < r => format!("{}{}", &l[..a], &l[r..]),
            _ => l.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Lines that differ between two normalized snapshots, at most `limit`, as
/// `-expected` / `+actual` pairs with their line number.
pub fn differences(expected: &str, actual: &str, limit: usize) -> Vec<String> {
    let (e, a) = (normalize(expected), normalize(actual));
    let (e, a): (Vec<&str>, Vec<&str>) = (e.lines().collect(), a.lines().collect());
    let mut out = Vec::new();
    for i in 0..e.len().max(a.len()) {
        let (x, y) = (e.get(i), a.get(i));
        if x != y {
            out.push(format!(
                "line {}: -{}\n         +{}",
                i + 1,
                x.unwrap_or(&""),
                y.unwrap_or(&"")
            ));
            if out.len() == limit {
                break;
            }
        }
    }
    out
}
