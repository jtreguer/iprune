//! The track part of a sync, worked out from the plan: what to remove, copy,
//! copy again and retag, with the fields to set. Pure: the same plan gives
//! the same work, new dbids included.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::Serialize;

use crate::ipod::Contents;
use crate::plan::{Field, Plan, clean};
use crate::store::TrackRow;
use crate::tags::{FileKind, Tags};

/// The audio facts set on a track when its file is copied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Audio {
    pub size: u64,
    pub tracklen_ms: u32,
    pub bitrate: u32,
    pub samplerate: u32,
    pub filetype: &'static str,
    /// The file's modification time, Unix seconds.
    pub modified: i64,
}

/// Tag values to set: `None` leaves a field as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Fields {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub composer: Option<String>,
    pub genre: Option<String>,
    pub year: Option<u32>,
    pub track_no: Option<u32>,
    pub track_total: Option<u32>,
    pub disc_no: Option<u32>,
    pub disc_total: Option<u32>,
    pub compilation: Option<bool>,
}

impl Fields {
    pub fn to_gpod(&self) -> gpod::TrackFields {
        let n = |v: Option<u32>| v.map(|v| i32::try_from(v).unwrap_or(i32::MAX));
        gpod::TrackFields {
            title: self.title.clone(),
            artist: self.artist.clone(),
            album: self.album.clone(),
            albumartist: self.album_artist.clone(),
            composer: self.composer.clone(),
            genre: self.genre.clone(),
            year: n(self.year),
            track_nr: n(self.track_no),
            tracks: n(self.track_total),
            cd_nr: n(self.disc_no),
            cds: n(self.disc_total),
            compilation: self.compilation,
        }
    }

    /// The tags an iPod track has once these fields are set on `old`, as
    /// `ipod::read` reports them.
    pub fn applied_to(&self, old: &Tags) -> Tags {
        let pos = |v: Option<u32>, o: Option<u32>| match v {
            Some(v) => (v > 0).then_some(v),
            None => o,
        };
        Tags {
            title: self.title.clone().or_else(|| old.title.clone()),
            artist: self.artist.clone().or_else(|| old.artist.clone()),
            album: self.album.clone().or_else(|| old.album.clone()),
            album_artist: self
                .album_artist
                .clone()
                .or_else(|| old.album_artist.clone()),
            composer: self.composer.clone().or_else(|| old.composer.clone()),
            genre: self.genre.clone().or_else(|| old.genre.clone()),
            year: pos(self.year, old.year),
            track_no: pos(self.track_no, old.track_no),
            track_total: pos(self.track_total, old.track_total),
            disc_no: pos(self.disc_no, old.disc_no),
            disc_total: pos(self.disc_total, old.disc_total),
            compilation: self.compilation.unwrap_or(old.compilation),
        }
    }
}

/// A file to copy: a new track, or new audio for a track already there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Copy {
    pub track_id: i64,
    pub path: PathBuf,
    /// The iPod track: a new dbid, or the existing one when `replace`.
    pub dbid: u64,
    pub replace: bool,
    pub fields: Fields,
    pub audio: Audio,
    /// The library's tag hash and duration, recorded once synced.
    pub tags_hash: [u8; 32],
    pub duration_ms: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Retag {
    pub track_id: i64,
    pub dbid: u64,
    pub fields: Fields,
    pub tags_hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Remove {
    pub dbid: u64,
    pub track_id: Option<i64>,
    pub size: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TrackWork {
    pub removes: Vec<Remove>,
    pub copies: Vec<Copy>,
    pub retags: Vec<Retag>,
}

impl TrackWork {
    pub fn is_empty(&self) -> bool {
        self.removes.is_empty() && self.copies.is_empty() && self.retags.is_empty()
    }

    pub fn copy_bytes(&self) -> u64 {
        self.copies.iter().map(|c| c.audio.size).sum()
    }

    /// The dbid each copied library track ends up with.
    pub fn new_dbids(&self) -> HashMap<i64, u64> {
        self.copies.iter().map(|c| (c.track_id, c.dbid)).collect()
    }
}

/// iTunes's name for a file type, which the iPod shows.
pub fn filetype(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Mp3 => "MPEG audio file",
        FileKind::Aac => "AAC audio file",
        FileKind::Alac => "Apple Lossless audio file",
        FileKind::Wav => "WAV audio file",
        FileKind::Aiff => "AIFF audio file",
    }
}

/// Every tag of a library track, cleaned as the planner compares them. The
/// title falls back to the file name, as iTunes did.
pub fn all_fields(t: &TrackRow) -> Fields {
    let num = |v: Option<u32>| v.filter(|&v| v > 0);
    Fields {
        title: clean(&Some(t.display_title().into_owned())),
        artist: clean(&t.tags.artist),
        album: clean(&t.tags.album),
        album_artist: clean(&t.tags.album_artist),
        composer: clean(&t.tags.composer),
        genre: clean(&t.tags.genre),
        year: num(t.tags.year),
        track_no: num(t.tags.track_no),
        track_total: num(t.tags.track_total),
        disc_no: num(t.tags.disc_no),
        disc_total: num(t.tags.disc_total),
        // Never clears the iPod's mark: a library file not marked says
        // nothing (see `plan::tag_changes`).
        compilation: t.tags.compilation.then_some(true),
    }
}

fn audio(t: &TrackRow) -> Audio {
    Audio {
        size: t.size,
        tracklen_ms: t.info.duration_ms.unwrap_or(0),
        bitrate: t.info.bitrate.unwrap_or(0),
        samplerate: t.info.samplerate.unwrap_or(0),
        filetype: filetype(t.info.kind),
        modified: t.mtime_ns.div_euclid(1_000_000_000),
    }
}

/// A dbid for a new track, derived from the iPod's GUID, the library track
/// and its content, so planning twice gives the same work; another is taken
/// if it is zero or already on the iPod.
pub fn new_dbid(guid: &str, t: &TrackRow, taken: &HashSet<u64>) -> u64 {
    (0u32..)
        .map(|n| dbid_candidate(guid, t, n))
        .find(|d| *d != 0 && !taken.contains(d))
        .expect("a free dbid")
}

/// The `n`th dbid [`new_dbid`] would try for this track.
pub fn dbid_candidate(guid: &str, t: &TrackRow, n: u32) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(guid.as_bytes())
        .update(&t.id.to_le_bytes())
        .update(&t.hash)
        .update(&n.to_le_bytes());
    u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().expect("8 bytes"))
}

/// Mappings lost by a crash between a verified write and its record: an
/// unmapped library track whose own dbid (one of the first few
/// [`new_dbid`] would have tried) is on the iPod, unmapped, with the file's
/// size, is that track. Returns the mappings to record. Pure.
pub fn lost_mappings(
    guid: &str,
    library: &[TrackRow],
    ipod: &Contents,
    mapped: &[crate::store::devices::TrackMapping],
) -> Vec<crate::store::devices::TrackMapping> {
    let mapped_tracks: HashSet<i64> = mapped.iter().map(|m| m.track_id).collect();
    let mapped_dbids: HashSet<u64> = mapped.iter().map(|m| m.dbid).collect();
    let size_of: HashMap<u64, u64> = ipod
        .tracks
        .iter()
        .map(|t| (t.adopt.dbid, t.adopt.size))
        .collect();
    let mut taken: HashSet<u64> = HashSet::new();
    let mut out = Vec::new();
    for t in library
        .iter()
        .filter(|t| t.missing_since.is_none() && !mapped_tracks.contains(&t.id))
    {
        let found = (0..4).map(|n| dbid_candidate(guid, t, n)).find(|d| {
            !mapped_dbids.contains(d) && !taken.contains(d) && size_of.get(d) == Some(&t.size)
        });
        if let Some(dbid) = found {
            taken.insert(dbid);
            out.push(crate::store::devices::TrackMapping {
                track_id: t.id,
                dbid,
                // The tags written then are not known: the next plan
                // compares the iPod's with the library's.
                synced_tags_hash: [0; 32],
                synced_size: t.size,
                synced_duration_ms: t.info.duration_ms,
            });
        }
    }
    out
}

/// The track part of `plan`. `library` holds every indexed track by id.
pub fn track_work(
    plan: &Plan,
    library: &HashMap<i64, TrackRow>,
    ipod: &Contents,
    guid: &str,
) -> TrackWork {
    let mut taken: HashSet<u64> = ipod.tracks.iter().map(|t| t.adopt.dbid).collect();
    let on_ipod: HashMap<u64, &crate::ipod::Track> =
        ipod.tracks.iter().map(|t| (t.adopt.dbid, t)).collect();
    let mut work = TrackWork {
        removes: plan
            .remove
            .iter()
            .map(|r| Remove {
                dbid: r.dbid,
                track_id: r.track_id,
                size: r.size,
            })
            .collect(),
        ..TrackWork::default()
    };
    for c in &plan.copy {
        let Some(t) = library.get(&c.track_id) else {
            continue;
        };
        let dbid = new_dbid(guid, t, &taken);
        taken.insert(dbid);
        work.copies.push(Copy {
            track_id: t.id,
            path: t.path.clone(),
            dbid,
            replace: false,
            fields: all_fields(t),
            audio: audio(t),
            tags_hash: t.tags_hash,
            duration_ms: t.info.duration_ms,
        });
    }
    for r in &plan.replace {
        let Some(t) = library.get(&r.track_id) else {
            continue;
        };
        work.copies.push(Copy {
            track_id: t.id,
            path: t.path.clone(),
            dbid: r.old_dbid,
            replace: true,
            fields: all_fields(t),
            audio: audio(t),
            tags_hash: t.tags_hash,
            duration_ms: t.info.duration_ms,
        });
    }
    for op in &plan.update_tags {
        let (Some(t), Some(_)) = (library.get(&op.track_id), on_ipod.get(&op.dbid)) else {
            continue;
        };
        let mut fields = Fields::default();
        for c in &op.changes {
            let v = c.library.clone();
            let n = || v.parse::<u32>().ok();
            match c.field {
                Field::Title => fields.title = Some(v.clone()),
                Field::Artist => fields.artist = Some(v.clone()),
                Field::Album => fields.album = Some(v.clone()),
                Field::AlbumArtist => fields.album_artist = Some(v.clone()),
                Field::Composer => fields.composer = Some(v.clone()),
                Field::Genre => fields.genre = Some(v.clone()),
                Field::Year => fields.year = n(),
                Field::TrackNumber => fields.track_no = n(),
                Field::TrackCount => fields.track_total = n(),
                Field::DiscNumber => fields.disc_no = n(),
                Field::DiscCount => fields.disc_total = n(),
                Field::Compilation => fields.compilation = Some(v == "true"),
            }
        }
        work.retags.push(Retag {
            track_id: t.id,
            dbid: op.dbid,
            fields,
            tags_hash: t.tags_hash,
        });
    }
    work
}
