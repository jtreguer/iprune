//! The sync planner: what to change on the iPod so it matches the library.
//!
//! Pure: plain data in, a [`Plan`] out, no I/O and no libgpod. The same input
//! gives the same plan in the same order.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

use crate::adopt::{self, IpodPlaylist, LibPlaylist, NAME_PASSES};
use crate::ipod::{self, Contents};
use crate::store::TrackRow;
use crate::store::devices::{PlaylistMapping, TrackMapping};
use crate::tags::Tags;

/// The audio of a mapped file counts as changed when its size differs from
/// the synced one and its duration differs from the file's duration at the
/// last sync by more than this. A tag edit changes the size but not the
/// duration. The iPod's own durations are not used: some are wrong.
const AUDIO_DURATION_MS: u32 = 1000;

/// Which library tracks and playlists go to the iPod.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    All,
    /// These playlists and the tracks in them.
    Playlists(&'a [String]),
}

/// The iPod filesystem's size and free space, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Space {
    pub capacity: u64,
    pub free: u64,
}

pub struct PlanInput<'a> {
    /// Every indexed track, missing ones included.
    pub library: &'a [TrackRow],
    /// Playlists with a file, members resolved to track ids.
    pub playlists: &'a [LibPlaylist],
    /// `(id, name)` of deleted playlists still mapped to the iPod.
    pub removed_playlists: &'a [(i64, String)],
    pub device: &'a Contents,
    pub tracks_map: &'a [TrackMapping],
    pub playlists_map: &'a [PlaylistMapping],
    pub scope: Scope<'a>,
    pub remove_unmanaged: bool,
    /// `None` when unknown; the plan then cannot be refused for space.
    pub space: Option<Space>,
    /// iPod tracks whose audio file is missing from the iPod, such as
    /// entries a restore brought back after a sync deleted their files.
    pub files_missing: &'a HashSet<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub copy: Vec<CopyOp>,
    pub replace: Vec<ReplaceOp>,
    pub update_tags: Vec<TagOp>,
    pub remove: Vec<RemoveOp>,
    pub playlists: Vec<PlaylistOp>,
    pub unmanaged: Unmanaged,
    pub skipped: Vec<Skipped>,
    pub entries_skipped: Vec<EntrySkipped>,
    /// Playlist files not synced, by name.
    pub playlists_skipped: Vec<PlaylistSkipped>,
    /// Mappings whose iPod track is gone (deleted on the device, or the
    /// database was restored). In scope, the track is copied again.
    pub stale_mappings: Vec<StaleMapping>,
    /// iPod playlists iprune did not manage that a library playlist of the
    /// same name takes over, with members it changes.
    pub takeovers: Vec<Takeover>,
    /// Mapped tracks with plays or skips since the last database write.
    pub stats_to_pull: usize,
    pub space: SpaceCheck,
    pub expected: Expected,
}

/// A library playlist taking over an iPod playlist iprune did not manage,
/// matched by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Takeover {
    pub playlist_id: i64,
    pub ipod_id: u64,
    /// The iPod playlist's name.
    pub name: String,
    /// Its tracks now.
    pub before: usize,
    /// Its tracks the library playlist does not hold, which the sync takes
    /// out of it.
    pub dropped: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CopyOp {
    pub track_id: i64,
    pub path: PathBuf,
    pub size: u64,
}

/// The file's audio changed: copy the new file, remove the old iPod track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplaceOp {
    pub track_id: i64,
    pub old_dbid: u64,
    pub path: PathBuf,
    pub size: u64,
    pub old_size: u64,
    /// Copied again because the iPod's file is missing, not because the
    /// audio changed.
    pub file_missing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TagOp {
    pub track_id: i64,
    pub dbid: u64,
    pub changes: Vec<TagChange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    Title,
    Artist,
    Album,
    AlbumArtist,
    Composer,
    Genre,
    Year,
    TrackNumber,
    TrackCount,
    DiscNumber,
    DiscCount,
    Compilation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TagChange {
    pub field: Field,
    pub ipod: Option<String>,
    pub library: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoveReason {
    /// The file is gone from the library.
    MissingFromLibrary,
    /// The track is no longer in the sync scope.
    OutOfScope,
    /// Not managed by iprune, removed because `remove_unmanaged` is set.
    Unmanaged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoveOp {
    pub dbid: u64,
    pub track_id: Option<i64>,
    pub size: u64,
    pub reason: RemoveReason,
}

/// A member of a playlist after the sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Member {
    /// A track already on the iPod.
    Track(u64),
    /// The library track copied by this plan (a copy or a replace).
    New(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaylistDeleteReason {
    /// The playlist file was deleted.
    FileDeleted,
    /// The playlist is no longer in the sync scope.
    OutOfScope,
    /// Not managed by iprune, removed because `remove_unmanaged` is set.
    Unmanaged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PlaylistOp {
    Delete {
        playlist_id: Option<i64>,
        ipod_id: u64,
        name: String,
        reason: PlaylistDeleteReason,
    },
    Rename {
        playlist_id: i64,
        ipod_id: u64,
        from: String,
        to: String,
    },
    Create {
        playlist_id: i64,
        name: String,
        members: Vec<Member>,
    },
    /// Replace the members as a whole.
    SetMembers {
        playlist_id: i64,
        ipod_id: u64,
        name: String,
        members: Vec<Member>,
    },
}

impl PlaylistOp {
    /// Deletes first, then renames, so a create or a rename never meets a
    /// name about to be freed.
    fn order(&self) -> (u8, &str) {
        match self {
            PlaylistOp::Delete { name, .. } => (0, name),
            PlaylistOp::Rename { to, .. } => (1, to),
            PlaylistOp::Create { name, .. } => (2, name),
            PlaylistOp::SetMembers { name, .. } => (3, name),
        }
    }
}

/// iPod items iprune does not manage, kept as they are.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Unmanaged {
    pub tracks: Vec<u64>,
    pub playlists: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// An iTunes Store purchase the iPod may not be authorised to play.
    FairPlay,
    /// Sampled above 48 kHz, which the iPod Classic cannot play.
    SampleRate,
    /// 4 GiB or more, which a FAT32 iPod cannot hold.
    TooLarge,
}

/// The largest file FAT32 holds: 4 GiB less one byte.
const FAT32_MAX: u64 = 0xFFFF_FFFF;

fn too_big(t: &TrackRow) -> bool {
    t.size > FAT32_MAX
}

/// The highest sample rate an iPod Classic plays.
const MAX_SAMPLE_RATE: u32 = 48_000;

fn too_fast(t: &TrackRow) -> bool {
    t.info.samplerate.is_some_and(|r| r > MAX_SAMPLE_RATE)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub track_id: i64,
    pub path: PathBuf,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryReason {
    /// The entry matches no indexed file.
    Unresolved,
    /// The file it points to is missing.
    Missing,
    /// The track is skipped (see [`Plan::skipped`]).
    TrackSkipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaylistSkipReason {
    /// Named like a smart playlist on the iPod: the file is an export of it,
    /// and a static copy would sit next to the live one.
    SmartOnIpod,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlaylistSkipped {
    pub name: String,
    pub reason: PlaylistSkipReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntrySkipped {
    pub playlist: String,
    /// 1-based, as `iprune playlist show` prints it.
    pub position: usize,
    pub reason: EntryReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StaleMapping {
    pub track_id: i64,
    pub dbid: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SpaceCheck {
    /// Bytes copied minus bytes freed; negative when the plan frees space.
    pub needed: i64,
    /// The most the plan adds at any point: copies minus removals, since
    /// a replaced file is removed only after its new copy is written.
    pub peak: i64,
    pub device: Option<Space>,
    /// False when `peak` does not fit in the free space with a 1% margin
    /// of the capacity. The plan must then not be applied.
    pub fits: bool,
}

/// The iPod after the plan, for the checks before and after writing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Expected {
    pub track_count: usize,
    /// Every managed playlist, by name.
    pub playlists: Vec<ExpectedPlaylist>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExpectedPlaylist {
    pub playlist_id: i64,
    /// The iPod playlist, `None` when the plan creates it.
    pub ipod_id: Option<u64>,
    pub name: String,
    pub members: Vec<Member>,
}

impl Plan {
    /// True when the plan changes nothing on the iPod.
    pub fn is_empty(&self) -> bool {
        self.copy.is_empty()
            && self.replace.is_empty()
            && self.update_tags.is_empty()
            && self.remove.is_empty()
            && self.playlists.is_empty()
    }
}

/// A tag value without surrounding spaces (ID3v1 pads with them) and in NFC,
/// so the same text stored two ways compares equal. Values separated by NUL,
/// which the iPod's C strings cannot hold, are joined with "/": the tag
/// reader joins them already, so this is left for rows an older reader
/// stored.
/// `None` when empty.
pub(crate) fn clean(s: &Option<String>) -> Option<String> {
    let s = s.as_deref()?;
    let s = if s.contains('\0') {
        let parts: Vec<&str> = s
            .split('\0')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        std::borrow::Cow::Owned(parts.join("/"))
    } else {
        std::borrow::Cow::Borrowed(s.trim())
    };
    (!s.is_empty()).then(|| s.nfc().collect())
}

fn text_change(
    out: &mut Vec<TagChange>,
    field: Field,
    lib: &Option<String>,
    ipod: &Option<String>,
) {
    if let Some(l) = clean(lib)
        && clean(ipod).as_ref() != Some(&l)
    {
        out.push(TagChange {
            field,
            ipod: ipod.clone(),
            library: l,
        });
    }
}

fn number_change(out: &mut Vec<TagChange>, field: Field, lib: Option<u32>, ipod: Option<u32>) {
    if let Some(l) = lib
        && ipod != Some(l)
    {
        out.push(TagChange {
            field,
            ipod: ipod.map(|n| n.to_string()),
            library: l.to_string(),
        });
    }
}

/// The fields where the library has a value the iPod lacks or differs from,
/// ignoring surrounding spaces and Unicode form. The library value is sent
/// cleaned. An empty library field, spaces only included, never clears an
/// iPod value, and a library track not marked as a compilation never clears
/// the iPod's mark.
pub fn tag_changes(lib: &Tags, ipod: &Tags) -> Vec<TagChange> {
    let mut out = Vec::new();
    text_change(&mut out, Field::Title, &lib.title, &ipod.title);
    text_change(&mut out, Field::Artist, &lib.artist, &ipod.artist);
    text_change(&mut out, Field::Album, &lib.album, &ipod.album);
    text_change(
        &mut out,
        Field::AlbumArtist,
        &lib.album_artist,
        &ipod.album_artist,
    );
    text_change(&mut out, Field::Composer, &lib.composer, &ipod.composer);
    text_change(&mut out, Field::Genre, &lib.genre, &ipod.genre);
    number_change(&mut out, Field::Year, lib.year, ipod.year);
    number_change(&mut out, Field::TrackNumber, lib.track_no, ipod.track_no);
    number_change(
        &mut out,
        Field::TrackCount,
        lib.track_total,
        ipod.track_total,
    );
    number_change(&mut out, Field::DiscNumber, lib.disc_no, ipod.disc_no);
    number_change(&mut out, Field::DiscCount, lib.disc_total, ipod.disc_total);
    if lib.compilation && !ipod.compilation {
        out.push(TagChange {
            field: Field::Compilation,
            ipod: Some("false".into()),
            library: "true".into(),
        });
    }
    out
}

/// Whether the file's audio is not what was copied to the iPod.
fn audio_changed(t: &TrackRow, m: &TrackMapping) -> bool {
    // Marked by the scan when the file was paired with this track by its
    // audio properties, not its content (`forget_synced_audio`): copied
    // again whatever its length, a track under a second included.
    if m.synced_size == 0 && m.synced_duration_ms == Some(0) {
        return true;
    }
    match (t.info.duration_ms, m.synced_duration_ms) {
        (Some(now), Some(then)) => {
            t.size != m.synced_size && now.abs_diff(then) > AUDIO_DURATION_MS
        }
        _ => false,
    }
}

/// The name a managed playlist keeps on the iPod: its current one when it
/// differs from the file's only by characters a file name cannot hold
/// ("Dance/Trance" for `Dance_Trance.m3u8`) or by Unicode form, the file's
/// otherwise, so a change of case reaches the iPod.
fn ipod_name(current: &str, file: &str) -> String {
    if adopt::file_chars(current) == adopt::file_chars(file) {
        current.to_owned()
    } else {
        file.to_owned()
    }
}

pub fn plan(input: &PlanInput) -> Plan {
    let mut plan = Plan::default();
    let present: BTreeMap<i64, &TrackRow> = input
        .library
        .iter()
        .filter(|t| t.missing_since.is_none())
        .map(|t| (t.id, t))
        .collect();
    let synced = |p: &LibPlaylist| match input.scope {
        Scope::All => true,
        Scope::Playlists(names) => names.contains(&p.name),
    };
    let in_scope: BTreeSet<i64> = match input.scope {
        Scope::All => present.keys().copied().collect(),
        Scope::Playlists(_) => input
            .playlists
            .iter()
            .filter(|p| synced(p))
            .flat_map(|p| p.members.iter().flatten())
            .filter(|id| present.contains_key(id))
            .copied()
            .collect(),
    };
    let on_ipod: HashMap<u64, &ipod::Track> = input
        .device
        .tracks
        .iter()
        .map(|t| (t.adopt.dbid, t))
        .collect();
    let by_track: HashMap<i64, &TrackMapping> =
        input.tracks_map.iter().map(|m| (m.track_id, m)).collect();
    let managed: HashSet<u64> = input.tracks_map.iter().map(|m| m.dbid).collect();

    // Tracks.
    let mut new_members: HashSet<i64> = HashSet::new();
    let mut sent: HashSet<i64> = HashSet::new();
    for &id in &in_scope {
        let t = present[&id];
        let mapping = by_track.get(&id).copied();
        if let Some(m) = mapping
            && !on_ipod.contains_key(&m.dbid)
        {
            plan.stale_mappings.push(StaleMapping {
                track_id: id,
                dbid: m.dbid,
            });
        }
        match mapping.filter(|m| on_ipod.contains_key(&m.dbid)) {
            None if t.drm || too_fast(t) || too_big(t) => {
                plan.skipped.push(Skipped {
                    track_id: id,
                    path: t.path.clone(),
                    reason: if t.drm {
                        SkipReason::FairPlay
                    } else if too_fast(t) {
                        SkipReason::SampleRate
                    } else {
                        SkipReason::TooLarge
                    },
                });
                continue;
            }
            None => {
                plan.copy.push(CopyOp {
                    track_id: id,
                    path: t.path.clone(),
                    size: t.size,
                });
                new_members.insert(id);
            }
            Some(m) => {
                let it = on_ipod[&m.dbid];
                // A protected file already on the iPod stays as it is.
                let file_missing = input.files_missing.contains(&m.dbid);
                if !t.drm && !too_fast(t) && !too_big(t) && (file_missing || audio_changed(t, m)) {
                    plan.replace.push(ReplaceOp {
                        track_id: id,
                        old_dbid: m.dbid,
                        path: t.path.clone(),
                        size: t.size,
                        old_size: it.adopt.size,
                        file_missing,
                    });
                    new_members.insert(id);
                } else if t.tags_hash != m.synced_tags_hash {
                    let changes = tag_changes(&t.tags, &it.tags);
                    if !changes.is_empty() {
                        plan.update_tags.push(TagOp {
                            track_id: id,
                            dbid: m.dbid,
                            changes,
                        });
                    }
                }
            }
        }
        sent.insert(id);
    }
    let mut mappings: Vec<&TrackMapping> = input.tracks_map.iter().collect();
    mappings.sort_by_key(|m| m.dbid);
    for m in mappings {
        let Some(it) = on_ipod.get(&m.dbid) else {
            continue;
        };
        if sent.contains(&m.track_id) {
            continue;
        }
        let reason = if present.contains_key(&m.track_id) {
            RemoveReason::OutOfScope
        } else {
            RemoveReason::MissingFromLibrary
        };
        plan.remove.push(RemoveOp {
            dbid: m.dbid,
            track_id: Some(m.track_id),
            size: it.adopt.size,
            reason,
        });
    }
    let mut unmanaged: Vec<&ipod::Track> = input
        .device
        .tracks
        .iter()
        .filter(|t| !managed.contains(&t.adopt.dbid))
        .collect();
    unmanaged.sort_by_key(|t| t.adopt.dbid);
    for t in unmanaged {
        if input.remove_unmanaged {
            plan.remove.push(RemoveOp {
                dbid: t.adopt.dbid,
                track_id: None,
                size: t.adopt.size,
                reason: RemoveReason::Unmanaged,
            });
        } else {
            plan.unmanaged.tracks.push(t.adopt.dbid);
        }
    }
    plan.stats_to_pull = input
        .device
        .tracks
        .iter()
        .filter(|t| managed.contains(&t.adopt.dbid) && t.recent_plays + t.recent_skips > 0)
        .count();

    // Playlists. Mapped ones first claim their iPod playlist by the stored
    // name; the others then match free iPod playlists by name, as adoption
    // does; whatever is left on the iPod is unmanaged.
    let member = |id: i64| match new_members.contains(&id) {
        true => Member::New(id),
        false => Member::Track(by_track[&id].dbid),
    };
    let stored: HashMap<i64, &PlaylistMapping> = input
        .playlists_map
        .iter()
        .map(|m| (m.playlist_id, m))
        .collect();
    let mut claimed: HashSet<u64> = HashSet::new();
    // By the recorded id; by name only for mappings saved without one. An
    // id no longer on the iPod claims nothing: the playlist was deleted there.
    let claim = |m: &PlaylistMapping, claimed: &mut HashSet<u64>| -> Option<&IpodPlaylist> {
        let found = input.device.playlists.iter().find(|p| {
            !claimed.contains(&p.id)
                && match m.ipod_id {
                    Some(id) => p.id == id,
                    None => p.name == m.ipod_name,
                }
        })?;
        claimed.insert(found.id);
        Some(found)
    };
    let mut live: Vec<&LibPlaylist> = input.playlists.iter().collect();
    live.sort_by(|a, b| a.name.cmp(&b.name));
    let mut target: HashMap<i64, &IpodPlaylist> = HashMap::new();
    for p in &live {
        let Some(found) = stored.get(&p.id).and_then(|m| claim(m, &mut claimed)) else {
            continue;
        };
        if synced(p) {
            target.insert(p.id, found);
        } else {
            plan.playlists.push(PlaylistOp::Delete {
                playlist_id: Some(p.id),
                ipod_id: found.id,
                name: found.name.clone(),
                reason: PlaylistDeleteReason::OutOfScope,
            });
        }
    }
    for (id, _) in input.removed_playlists {
        if let Some(found) = stored.get(id).and_then(|m| claim(m, &mut claimed)) {
            plan.playlists.push(PlaylistOp::Delete {
                playlist_id: Some(*id),
                ipod_id: found.id,
                name: found.name.clone(),
                reason: PlaylistDeleteReason::FileDeleted,
            });
        }
    }
    let mut pending: Vec<&LibPlaylist> = live
        .iter()
        .copied()
        .filter(|p| synced(p) && !target.contains_key(&p.id))
        .collect();
    // Matched by name: iPod playlists iprune did not manage until now.
    let mut by_name: HashSet<i64> = HashSet::new();
    for key in NAME_PASSES {
        let mut next = Vec::new();
        for p in pending {
            let k = key(&p.name);
            match input
                .device
                .playlists
                .iter()
                .find(|ip| !claimed.contains(&ip.id) && key(&ip.name) == k)
            {
                Some(found) => {
                    claimed.insert(found.id);
                    target.insert(p.id, found);
                    by_name.insert(p.id);
                }
                None => next.push(p),
            }
        }
        pending = next;
    }
    // A file with no iPod playlist of its own but named like a smart one is
    // an export of it: skipped rather than created next to it.
    let mut smart_exports: HashSet<i64> = HashSet::new();
    for p in pending {
        let smart = NAME_PASSES.iter().any(|key| {
            let k = key(&p.name);
            // An unnamed smart playlist matches nothing.
            input
                .device
                .smart_playlists
                .iter()
                .any(|s| !s.trim().is_empty() && key(s) == k)
        });
        if smart {
            smart_exports.insert(p.id);
            plan.playlists_skipped.push(PlaylistSkipped {
                name: p.name.clone(),
                reason: PlaylistSkipReason::SmartOnIpod,
            });
        }
    }
    for p in live
        .iter()
        .filter(|p| synced(p) && !smart_exports.contains(&p.id))
    {
        let mut members = Vec::new();
        for (i, entry) in p.members.iter().enumerate() {
            let reason = match entry {
                None => Some(EntryReason::Unresolved),
                Some(id) if !present.contains_key(id) => Some(EntryReason::Missing),
                Some(id) if !sent.contains(id) => Some(EntryReason::TrackSkipped),
                Some(id) => {
                    members.push(member(*id));
                    None
                }
            };
            if let Some(reason) = reason {
                plan.entries_skipped.push(EntrySkipped {
                    playlist: p.name.clone(),
                    position: i + 1,
                    reason,
                });
            }
        }
        let (ipod_id, name) = match target.get(&p.id) {
            None => {
                plan.playlists.push(PlaylistOp::Create {
                    playlist_id: p.id,
                    name: p.name.clone(),
                    members: members.clone(),
                });
                (None, p.name.clone())
            }
            Some(found) => {
                let name = ipod_name(&found.name, &p.name);
                if name != found.name {
                    plan.playlists.push(PlaylistOp::Rename {
                        playlist_id: p.id,
                        ipod_id: found.id,
                        from: found.name.clone(),
                        to: name.clone(),
                    });
                }
                let current: Vec<Member> =
                    found.members.iter().map(|&d| Member::Track(d)).collect();
                if current != members && by_name.contains(&p.id) && !found.members.is_empty() {
                    let kept: HashSet<u64> = members
                        .iter()
                        .filter_map(|m| match m {
                            Member::Track(d) => Some(*d),
                            _ => None,
                        })
                        .collect();
                    let before: HashSet<u64> = found.members.iter().copied().collect();
                    plan.takeovers.push(Takeover {
                        playlist_id: p.id,
                        ipod_id: found.id,
                        name: found.name.clone(),
                        before: found.members.len(),
                        dropped: before.difference(&kept).count(),
                    });
                }
                if current != members {
                    plan.playlists.push(PlaylistOp::SetMembers {
                        playlist_id: p.id,
                        ipod_id: found.id,
                        name: name.clone(),
                        members: members.clone(),
                    });
                }
                (Some(found.id), name)
            }
        };
        plan.expected.playlists.push(ExpectedPlaylist {
            playlist_id: p.id,
            ipod_id,
            name,
            members,
        });
    }
    for ip in input
        .device
        .playlists
        .iter()
        .filter(|p| !claimed.contains(&p.id))
    {
        if input.remove_unmanaged {
            plan.playlists.push(PlaylistOp::Delete {
                playlist_id: None,
                ipod_id: ip.id,
                name: ip.name.clone(),
                reason: PlaylistDeleteReason::Unmanaged,
            });
        } else {
            plan.unmanaged.playlists.push(ip.name.clone());
        }
    }
    plan.playlists.sort_by(|a, b| a.order().cmp(&b.order()));
    plan.unmanaged.playlists.sort();
    plan.expected.playlists.sort_by(|a, b| a.name.cmp(&b.name));
    plan.expected.track_count = input.device.tracks.len() + plan.copy.len() - plan.remove.len();

    let sum = |sizes: &mut dyn Iterator<Item = u64>| sizes.map(i128::from).sum::<i128>();
    let added =
        sum(&mut plan.copy.iter().map(|c| c.size)) + sum(&mut plan.replace.iter().map(|r| r.size));
    let removed = sum(&mut plan.remove.iter().map(|r| r.size));
    let replaced = sum(&mut plan.replace.iter().map(|r| r.old_size));
    let clamp = |n: i128| n.clamp(i64::MIN.into(), i64::MAX.into()) as i64;
    // Removals run before copies, but a replaced file goes only after its
    // new copy is in place: both are on the iPod at the peak.
    let peak = added - removed;
    plan.space = SpaceCheck {
        needed: clamp(peak - replaced),
        peak: clamp(peak),
        device: input.space,
        fits: match input.space {
            None => true,
            Some(s) => peak <= 0 || peak + i128::from(s.capacity / 100) <= i128::from(s.free),
        },
    };
    plan
}
