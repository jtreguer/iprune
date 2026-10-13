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

/// Problems found in the changed database `after` before it is written.
/// `before` is the iPod as read at the start, `managed` every playlist iprune
/// manages after the change, `deleted` the iPod playlists it deleted and
/// `created` how many it created. Tracks are not touched by playlist changes,
/// so they must be exactly as before. Empty when everything holds.
pub fn pre_write(
    before: &Contents,
    after: &Contents,
    managed: &[ExpectedPlaylist],
    deleted: &[u64],
    created: usize,
) -> Vec<String> {
    let mut problems = Vec::new();
    // Each track as stored, without the count of playlists it is in, which
    // playlist changes are meant to change.
    let stored = |c: &Contents| -> Vec<ipod::Track> {
        c.tracks
            .iter()
            .map(|t| {
                let mut t = t.clone();
                t.adopt.playlists = 0;
                t
            })
            .collect()
    };
    if stored(after) != stored(before) {
        problems.push(format!(
            "the tracks changed ({} before, {} now)",
            before.tracks.len(),
            after.tracks.len()
        ));
    }
    if after.master_members != before.master_members {
        problems.push("the master playlist's members changed".into());
    }
    let all_tracks: HashSet<u64> = after.tracks.iter().map(|t| t.adopt.dbid).collect();
    let now: HashMap<u64, &crate::adopt::IpodPlaylist> =
        after.playlists.iter().map(|p| (p.id, p)).collect();
    let managed_ids: HashSet<u64> = managed.iter().map(|m| m.ipod_id).collect();
    let before_ids: HashSet<u64> = before.playlists.iter().map(|p| p.id).collect();
    let expected_count = (before.playlists.len() + created).saturating_sub(deleted.len());
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
    for m in managed {
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
    // Everything iprune does not manage is exactly as it was.
    for p in before
        .playlists
        .iter()
        .filter(|p| !managed_ids.contains(&p.id) && !deleted.contains(&p.id))
    {
        if now.get(&p.id).is_none_or(|q| *q != p) {
            problems.push(format!("unmanaged playlist {:?} changed", p.name));
        }
    }
    for id in deleted {
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

/// A snapshot without the parts libgpod renumbers on every write: the
/// artwork link (`mhii`) of each track.
pub fn normalize(snapshot: &str) -> String {
    snapshot
        .lines()
        .map(|l| match l.find(" mhii ") {
            Some(i) => {
                let rest = &l[i + 6..];
                let end = rest.find(' ').unwrap_or(rest.len());
                format!("{}{}", &l[..i], &rest[end..])
            }
            None => l.to_owned(),
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
