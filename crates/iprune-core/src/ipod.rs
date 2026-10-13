//! Reading an iPod database into plain data for adoption and planning.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};

use crate::adopt::{IpodPlaylist, IpodTrack};
use crate::tags::Tags;

/// One iPod track as iprune sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub adopt: IpodTrack,
    pub tags: Tags,
    pub ipod_path: Option<String>,
    /// Plays and skips since the last database write, from `Play Counts`.
    pub recent_plays: u32,
    pub recent_skips: u32,
    /// 0 to 100, in steps of 20 per star; 0 when unrated.
    pub rating: u32,
    /// Unix time of the last play, 0 when never played.
    pub last_played: i64,
}

#[derive(Debug, Clone, Default)]
pub struct Contents {
    /// The master playlist's name, which is the iPod's name.
    pub name: Option<String>,
    pub firewire_guid: Option<String>,
    pub model: Option<String>,
    pub tracks: Vec<Track>,
    /// Regular playlists, in iPod order.
    pub playlists: Vec<IpodPlaylist>,
    /// The master playlist's members, in order.
    pub master_members: Vec<u64>,
    /// Names of the smart playlists, in iPod order. Never edited.
    pub smart_playlists: Vec<String>,
}

fn non_empty(s: Option<std::borrow::Cow<'_, str>>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty()).map(|s| s.into_owned())
}

fn positive(n: i32) -> Option<u32> {
    u32::try_from(n).ok().filter(|&n| n > 0)
}

/// The synced fields as stored on the iPod, built the same way as from a
/// file, so identical tags hash the same.
pub fn tags_of(t: &gpod::TrackRef<'_>) -> Tags {
    Tags {
        title: non_empty(t.title()),
        artist: non_empty(t.artist()),
        album: non_empty(t.album()),
        album_artist: non_empty(t.albumartist()),
        composer: non_empty(t.composer()),
        genre: non_empty(t.genre()),
        year: positive(t.year()),
        track_no: positive(t.track_nr()),
        track_total: positive(t.tracks()),
        disc_no: positive(t.cd_nr()),
        disc_total: positive(t.cds()),
        compilation: t.compilation(),
    }
}

/// The file behind an iPod path such as `:iPod_Control:Music:F12:ABCD.mp3`,
/// under `mountpoint`. `None` for a path that is empty or would leave the
/// mountpoint. FAT ignores case, so the iPod's spelling is used as is.
pub fn file_path(mountpoint: &Path, ipod_path: &str) -> Option<PathBuf> {
    let rel: PathBuf = ipod_path.strip_prefix(':')?.split(':').collect();
    let plain = rel.components().all(|c| matches!(c, Component::Normal(_)));
    (plain && rel.components().next().is_some()).then(|| mountpoint.join(rel))
}

pub fn read(db: &gpod::Database) -> Contents {
    let mut in_playlists: HashMap<u64, usize> = HashMap::new();
    let mut playlists = Vec::new();
    let mut smart_playlists = Vec::new();
    let mut master_members = Vec::new();
    for pl in db.playlists() {
        // The master and Podcasts playlists belong to the iPod itself.
        if pl.is_master() {
            master_members = pl.members().map(|t| t.dbid()).collect();
            continue;
        }
        if pl.is_podcasts() {
            continue;
        }
        if pl.is_smart() {
            smart_playlists.push(pl.name().map(|n| n.into_owned()).unwrap_or_default());
            continue;
        }
        let members: Vec<u64> = pl.members().map(|t| t.dbid()).collect();
        for dbid in members.iter().collect::<BTreeSet<_>>() {
            *in_playlists.entry(*dbid).or_default() += 1;
        }
        playlists.push(IpodPlaylist {
            id: pl.id(),
            name: pl.name().map(|n| n.into_owned()).unwrap_or_default(),
            members,
        });
    }
    let tracks = db
        .tracks()
        .map(|t| {
            let tags = tags_of(&t);
            Track {
                adopt: IpodTrack {
                    dbid: t.dbid(),
                    size: u64::from(t.size()),
                    duration_ms: u32::try_from(t.tracklen()).unwrap_or(0),
                    title: tags.title.clone(),
                    artist: tags.artist.clone(),
                    album: tags.album.clone(),
                    playlists: in_playlists.get(&t.dbid()).copied().unwrap_or(0),
                    playcount: t.playcount(),
                    hash: None,
                },
                tags,
                ipod_path: t.ipod_path().map(|p| p.into_owned()),
                recent_plays: t.recent_playcount(),
                recent_skips: t.recent_skipcount(),
                rating: t.rating(),
                last_played: t.time_played(),
            }
        })
        .collect();
    // Without a usable model number libgpod still returns a placeholder entry
    // whose number is "Invalid" or "Unknown".
    let model = db
        .device_info()
        .filter(|i| {
            i.model_number
                .as_deref()
                .is_some_and(|n| !matches!(n, "Invalid" | "Unknown"))
        })
        .map(|i| {
            let gb = if i.capacity_gb > 0.0 {
                format!(" {} GB", i.capacity_gb)
            } else {
                String::new()
            };
            format!(
                "{}{gb}{}",
                i.model.unwrap_or_else(|| "unknown model".into()),
                i.generation.map(|g| format!(" ({g})")).unwrap_or_default()
            )
        });
    Contents {
        name: db
            .master_playlist()
            .and_then(|p| p.name().map(|n| n.into_owned())),
        firewire_guid: db.firewire_guid(),
        model,
        tracks,
        playlists,
        master_members,
        smart_playlists,
    }
}
