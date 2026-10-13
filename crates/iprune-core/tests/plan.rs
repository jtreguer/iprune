//! The sync planner on hand-built input. Nothing here touches a disk, a
//! database or libgpod: `TrackRow`, `Contents`, `TrackMapping` and
//! `LibPlaylist` values go in, a `Plan` comes out.

use std::path::PathBuf;

use iprune_core::adopt::{IpodPlaylist, IpodTrack, LibPlaylist};
use iprune_core::ipod::{Contents, Track};
use iprune_core::plan::{
    self, EntryReason, EntrySkipped, ExpectedPlaylist, Field, Member, Plan, PlanInput,
    PlaylistDeleteReason, PlaylistOp, PlaylistSkipReason, PlaylistSkipped, RemoveReason, Scope,
    SkipReason, Space, tag_changes,
};
use iprune_core::store::TrackRow;
use iprune_core::store::devices::{PlaylistMapping, TrackMapping};
use iprune_core::tags::{AudioInfo, FileKind, Tags};

const SIZE: u64 = 4_000;
const MS: u32 = 200_000;

fn tags(n: i64) -> Tags {
    Tags {
        title: Some(format!("Title{n}")),
        artist: Some(format!("Artist{n}")),
        ..Tags::default()
    }
}

fn lib_track(id: i64, size: u64, ms: u32) -> TrackRow {
    let t = tags(id);
    TrackRow {
        id,
        path: PathBuf::from(format!("/music/{id}.mp3")),
        path_key: format!("/music/{id}.mp3"),
        size,
        mtime_ns: 0,
        hash: [id as u8; 32],
        info: AudioInfo {
            kind: FileKind::Mp3,
            duration_ms: Some(ms),
            bitrate: None,
            samplerate: None,
        },
        tags_hash: t.hash(),
        tags: t,
        added_at: 0,
        missing_since: None,
        tag_error: None,
        drm: false,
    }
}

fn ipod_track(dbid: u64, size: u64, ms: u32, t: Tags) -> Track {
    Track {
        adopt: IpodTrack {
            dbid,
            size,
            duration_ms: ms,
            title: t.title.clone(),
            artist: t.artist.clone(),
            album: t.album.clone(),
            playlists: 0,
            playcount: 0,
            hash: None,
        },
        tags: t,
        ipod_path: None,
        recent_plays: 0,
        recent_skips: 0,
        rating: 0,
        last_played: 0,
    }
}

fn ipod_playlist(id: u64, name: &str, members: &[u64]) -> IpodPlaylist {
    IpodPlaylist {
        id,
        name: name.to_owned(),
        members: members.to_vec(),
    }
}

fn lib_playlist(id: i64, name: &str, members: &[Option<i64>]) -> LibPlaylist {
    LibPlaylist {
        id,
        name: name.to_owned(),
        members: members.to_vec(),
    }
}

fn ids(members: &[i64]) -> Vec<Option<i64>> {
    members.iter().map(|&m| Some(m)).collect()
}

#[derive(Clone, Default)]
struct World {
    library: Vec<TrackRow>,
    playlists: Vec<LibPlaylist>,
    removed: Vec<(i64, String)>,
    device: Contents,
    tracks_map: Vec<TrackMapping>,
    playlists_map: Vec<PlaylistMapping>,
    /// `Some(names)` is `Scope::Playlists(names)`.
    scope: Option<Vec<String>>,
    remove_unmanaged: bool,
    space: Option<Space>,
}

impl World {
    /// A library track that is on the iPod as `dbid` and unchanged since the
    /// last sync.
    fn synced(&mut self, id: i64, dbid: u64) -> &mut Self {
        let t = lib_track(id, SIZE, MS);
        self.tracks_map.push(TrackMapping {
            track_id: id,
            dbid,
            synced_tags_hash: t.tags_hash,
            synced_size: t.size,
            synced_duration_ms: t.info.duration_ms,
        });
        self.device
            .tracks
            .push(ipod_track(dbid, t.size, MS, t.tags.clone()));
        self.library.push(t);
        self
    }

    /// A library track the iPod does not have.
    fn fresh(&mut self, id: i64) -> &mut Self {
        self.library.push(lib_track(id, SIZE, MS));
        self
    }

    fn ipod_playlist(&mut self, id: u64, name: &str, members: &[u64]) -> &mut Self {
        self.device.playlists.push(ipod_playlist(id, name, members));
        self
    }

    /// A playlist file, mapped to the iPod playlist `ipod_name` when given.
    fn playlist(
        &mut self,
        id: i64,
        name: &str,
        members: &[Option<i64>],
        ipod_name: Option<&str>,
    ) -> &mut Self {
        self.playlists.push(lib_playlist(id, name, members));
        if let Some(n) = ipod_name {
            self.map_playlist(id, None, n);
        }
        self
    }

    fn map_playlist(&mut self, id: i64, ipod_id: Option<u64>, name: &str) -> &mut Self {
        self.playlists_map.push(PlaylistMapping {
            playlist_id: id,
            ipod_id,
            ipod_name: name.to_owned(),
        });
        self
    }

    fn plan(&self) -> Plan {
        let names = self.scope.clone();
        plan::plan(&PlanInput {
            library: &self.library,
            playlists: &self.playlists,
            removed_playlists: &self.removed,
            device: &self.device,
            tracks_map: &self.tracks_map,
            playlists_map: &self.playlists_map,
            scope: match &names {
                None => Scope::All,
                Some(n) => Scope::Playlists(n),
            },
            remove_unmanaged: self.remove_unmanaged,
            space: self.space,
        })
    }

    fn mapping_mut(&mut self, track_id: i64) -> &mut TrackMapping {
        self.tracks_map
            .iter_mut()
            .find(|m| m.track_id == track_id)
            .unwrap()
    }

    fn lib_mut(&mut self, id: i64) -> &mut TrackRow {
        self.library.iter_mut().find(|t| t.id == id).unwrap()
    }
}

// ---- tracks

#[test]
fn an_unmapped_track_is_copied_and_nothing_else_happens() {
    let mut w = World::default();
    w.fresh(1);
    let p = w.plan();
    assert_eq!(p.copy.len(), 1);
    assert_eq!(p.copy[0].track_id, 1);
    assert_eq!(p.copy[0].path, PathBuf::from("/music/1.mp3"));
    assert_eq!(p.copy[0].size, SIZE);
    assert!(p.replace.is_empty() && p.update_tags.is_empty() && p.remove.is_empty());
    assert!(p.stale_mappings.is_empty() && p.skipped.is_empty());
    assert_eq!(p.expected.track_count, 1);
    assert!(!p.is_empty());
}

#[test]
fn an_unchanged_synced_track_gives_an_empty_plan() {
    let mut w = World::default();
    w.synced(1, 0xA1).synced(2, u64::MAX - 1);
    w.ipod_playlist(1, "Mix", &[0xA1, u64::MAX - 1]);
    w.playlist(10, "Mix", &ids(&[1, 2]), Some("Mix"));
    let p = w.plan();
    assert!(p.is_empty(), "{p:?}");
    assert_eq!(p.expected.track_count, 2);
    assert_eq!(
        names_and_members(&p.expected.playlists),
        vec![(
            "Mix".to_owned(),
            vec![Member::Track(0xA1), Member::Track(u64::MAX - 1)]
        )]
    );
}

#[test]
fn a_stale_mapping_is_copied_again_and_reported() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    // The track vanished from the device, the mapping stayed.
    w.device.tracks.clear();
    let p = w.plan();
    assert_eq!(p.copy.len(), 1);
    assert_eq!(p.copy[0].track_id, 1);
    assert_eq!(p.stale_mappings.len(), 1);
    assert_eq!(p.stale_mappings[0].track_id, 1);
    assert_eq!(p.stale_mappings[0].dbid, 0xA1);
    // Nothing on the iPod to remove, and the copy is a new track.
    assert!(p.remove.is_empty() && p.replace.is_empty());
    assert_eq!(p.expected.track_count, 1);
}

#[test]
fn a_stale_mapping_out_of_scope_is_neither_copied_nor_removed() {
    let mut w = World::default();
    w.synced(1, 0xA1).synced(2, 0xA2);
    w.device.tracks.retain(|t| t.adopt.dbid != 0xA1);
    w.playlist(10, "Only2", &ids(&[2]), None);
    w.scope = Some(vec!["Only2".into()]);
    let p = w.plan();
    assert!(p.copy.is_empty(), "{:?}", p.copy);
    assert!(p.remove.is_empty(), "{:?}", p.remove);
}

#[test]
fn an_unmapped_fairplay_file_is_skipped_not_copied() {
    let mut w = World::default();
    w.fresh(1);
    w.lib_mut(1).drm = true;
    w.playlist(10, "Store", &ids(&[1]), None);
    let p = w.plan();
    assert!(p.copy.is_empty());
    assert_eq!(p.skipped.len(), 1);
    assert_eq!(p.skipped[0].track_id, 1);
    assert_eq!(p.skipped[0].reason, SkipReason::FairPlay);
    assert_eq!(p.expected.track_count, 0);
    // The entry is left out of the playlist and listed.
    assert_eq!(
        p.entries_skipped,
        vec![EntrySkipped {
            playlist: "Store".into(),
            position: 1,
            reason: EntryReason::TrackSkipped,
        }]
    );
    assert_eq!(
        names_and_members(&p.expected.playlists),
        vec![("Store".to_owned(), vec![])]
    );
}

#[test]
fn a_stale_fairplay_mapping_is_skipped_too() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.lib_mut(1).drm = true;
    w.device.tracks.clear();
    let p = w.plan();
    assert!(p.copy.is_empty());
    assert_eq!(p.skipped.len(), 1);
    assert_eq!(p.stale_mappings.len(), 1);
}

#[test]
fn a_mapped_fairplay_file_is_kept_untouched_even_if_its_audio_changed() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.lib_mut(1).drm = true;
    // New size and a duration 60 s off: would be a replace for any other file.
    w.lib_mut(1).size += 777;
    w.lib_mut(1).info.duration_ms = Some(MS + 60_000);
    w.ipod_playlist(1, "Store", &[0xA1]);
    w.playlist(10, "Store", &ids(&[1]), Some("Store"));
    let p = w.plan();
    assert!(p.replace.is_empty(), "{:?}", p.replace);
    assert!(p.copy.is_empty() && p.remove.is_empty() && p.skipped.is_empty());
    // Still a playlist member, as the existing iPod track.
    assert!(p.is_empty(), "{p:?}");
    assert_eq!(p.expected.playlists[0].members, vec![Member::Track(0xA1)]);
}

#[test]
fn replace_needs_a_different_size_and_a_duration_more_than_a_second_off() {
    // (library size delta, library duration, expect a replace)
    let cases: [(i64, Option<u32>, bool); 8] = [
        (0, Some(MS + 60_000), false),  // same bytes: the audio is the same
        (500, Some(MS), false),         // a tag edit changes the size only
        (500, Some(MS + 1_000), false), // exactly one second is not "more than"
        (500, Some(MS - 1_000), false),
        (500, Some(MS + 1_001), true),
        (500, Some(MS - 1_001), true),
        (-500, Some(MS + 30_000), true), // smaller file, replaced all the same
        (500, None, false),              // no duration in the library: cannot tell
    ];
    for (delta, ms, expect) in cases {
        let mut w = World::default();
        w.synced(1, 0xA1);
        w.lib_mut(1).size = (SIZE as i64 + delta) as u64;
        w.lib_mut(1).info.duration_ms = ms;
        let p = w.plan();
        let case = format!("delta {delta}, duration {ms:?}");
        if expect {
            assert_eq!(p.replace.len(), 1, "{case}: {p:?}");
            let r = &p.replace[0];
            assert_eq!(r.track_id, 1, "{case}");
            assert_eq!(r.old_dbid, 0xA1, "{case}");
            assert_eq!(r.size, (SIZE as i64 + delta) as u64, "{case}");
            assert_eq!(r.old_size, SIZE, "{case}");
            assert!(p.copy.is_empty() && p.remove.is_empty(), "{case}: {p:?}");
            // One track out, one in.
            assert_eq!(p.expected.track_count, 1, "{case}");
            assert_eq!(p.space.needed, delta, "{case}");
        } else {
            assert!(p.replace.is_empty(), "{case}: {:?}", p.replace);
        }
    }
}

#[test]
fn a_replaced_track_is_not_also_tag_updated_and_is_a_new_playlist_member() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.lib_mut(1).size += 500;
    w.lib_mut(1).info.duration_ms = Some(MS + 30_000);
    w.lib_mut(1).tags.title = Some("Edited".into());
    w.lib_mut(1).tags_hash = w.library[0].tags.hash();
    w.ipod_playlist(1, "Mix", &[0xA1]);
    w.playlist(10, "Mix", &ids(&[1]), Some("Mix"));
    let p = w.plan();
    assert_eq!(p.replace.len(), 1);
    assert!(p.update_tags.is_empty(), "{:?}", p.update_tags);
    assert_eq!(
        p.playlists,
        vec![PlaylistOp::SetMembers {
            playlist_id: 10,
            ipod_id: 1,
            name: "Mix".into(),
            members: vec![Member::New(1)],
        }]
    );
}

#[test]
fn tags_are_updated_only_when_the_hash_differs() {
    // Same library tags on both sides but a different synced hash: updates.
    let mut w = World::default();
    w.synced(1, 0xA1).synced(2, 0xA2);
    // Track 1: the library title changed since the sync (hash differs).
    w.lib_mut(1).tags.title = Some("New title".into());
    w.lib_mut(1).tags_hash = w.library[0].tags.hash();
    // Track 2: the iPod differs from the library but the hash says "synced":
    // the planner trusts the hash and does not touch it.
    w.device
        .tracks
        .iter_mut()
        .find(|t| t.adopt.dbid == 0xA2)
        .unwrap()
        .tags
        .title = Some("Edited on the iPod".into());
    let p = w.plan();
    assert_eq!(p.update_tags.len(), 1, "{:?}", p.update_tags);
    let op = &p.update_tags[0];
    assert_eq!((op.track_id, op.dbid), (1, 0xA1));
    assert_eq!(op.changes.len(), 1);
    assert_eq!(op.changes[0].field, Field::Title);
    assert_eq!(op.changes[0].ipod.as_deref(), Some("Title1"));
    assert_eq!(op.changes[0].library, "New title");
    assert!(p.copy.is_empty() && p.replace.is_empty() && p.remove.is_empty());
}

#[test]
fn a_changed_hash_with_nothing_to_write_makes_no_op() {
    // Only a field the library left empty differs: nothing to send.
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.mapping_mut(1).synced_tags_hash = [9; 32];
    w.lib_mut(1).tags.artist = None;
    let p = w.plan();
    assert!(p.update_tags.is_empty(), "{:?}", p.update_tags);
    assert!(p.is_empty(), "{p:?}");
}

fn t(f: impl FnOnce(&mut Tags)) -> Tags {
    let mut t = Tags::default();
    f(&mut t);
    t
}

fn change(field: Field, ipod: Option<&str>, library: &str) -> (Field, Option<String>, String) {
    (field, ipod.map(String::from), library.to_owned())
}

#[test]
fn tag_changes_table() {
    type Row = (
        &'static str,
        Tags,
        Tags,
        Vec<(Field, Option<String>, String)>,
    );
    let rows: Vec<Row> = vec![
        (
            "identical",
            t(|x| x.title = Some("A".into())),
            t(|x| x.title = Some("A".into())),
            vec![],
        ),
        (
            "text differs",
            t(|x| x.album = Some("New".into())),
            t(|x| x.album = Some("Old".into())),
            vec![change(Field::Album, Some("Old"), "New")],
        ),
        (
            "ipod lacks a value",
            t(|x| x.genre = Some("Jazz".into())),
            Tags::default(),
            vec![change(Field::Genre, None, "Jazz")],
        ),
        (
            "empty library text never clears",
            Tags::default(),
            t(|x| {
                x.title = Some("T".into());
                x.artist = Some("A".into());
                x.album = Some("B".into());
                x.album_artist = Some("C".into());
                x.composer = Some("D".into());
                x.genre = Some("E".into());
            }),
            vec![],
        ),
        (
            "empty library numbers never clear",
            Tags::default(),
            t(|x| {
                x.year = Some(1999);
                x.track_no = Some(1);
                x.track_total = Some(2);
                x.disc_no = Some(1);
                x.disc_total = Some(2);
            }),
            vec![],
        ),
        (
            "numbers",
            t(|x| {
                x.year = Some(2001);
                x.track_no = Some(3);
                x.track_total = Some(12);
                x.disc_no = Some(2);
                x.disc_total = Some(2);
            }),
            t(|x| {
                x.year = Some(2000);
                x.track_no = Some(3);
                x.disc_no = Some(1);
            }),
            vec![
                change(Field::Year, Some("2000"), "2001"),
                change(Field::TrackCount, None, "12"),
                change(Field::DiscNumber, Some("1"), "2"),
                change(Field::DiscCount, None, "2"),
            ],
        ),
        (
            "compilation set in the library",
            t(|x| x.compilation = true),
            Tags::default(),
            vec![change(Field::Compilation, Some("false"), "true")],
        ),
        (
            "compilation not set in the library never clears the ipod's",
            Tags::default(),
            t(|x| x.compilation = true),
            vec![],
        ),
        (
            "compilation on both sides",
            t(|x| x.compilation = true),
            t(|x| x.compilation = true),
            vec![],
        ),
    ];
    for (name, lib, ipod, want) in rows {
        let got: Vec<_> = tag_changes(&lib, &ipod)
            .into_iter()
            .map(|c| (c.field, c.ipod, c.library))
            .collect();
        assert_eq!(got, want, "{name}");
    }
}

// ---- tag comparison: library wins, compared cleaned

/// Every text field set to `v`.
fn all_text(v: &str) -> Tags {
    t(|x| {
        x.title = Some(v.into());
        x.artist = Some(v.into());
        x.album = Some(v.into());
        x.album_artist = Some(v.into());
        x.composer = Some(v.into());
        x.genre = Some(v.into());
    })
}

fn pairs(lib: &Tags, ipod: &Tags) -> Vec<(Field, Option<String>, String)> {
    tag_changes(lib, ipod)
        .into_iter()
        .map(|c| (c.field, c.ipod, c.library))
        .collect()
}

// Prevents ID3v1 space padding from making every such track a tag update.
#[test]
fn spaces_around_a_text_tag_are_not_a_change_on_either_side() {
    let clean = all_text("Stella By Starlight");
    let padded = all_text("Stella By Starlight           ");
    let lead = all_text("  Stella By Starlight");
    assert_eq!(pairs(&clean, &padded), vec![], "padded on the iPod");
    assert_eq!(pairs(&padded, &clean), vec![], "padded in the library");
    assert_eq!(pairs(&lead, &clean), vec![], "leading, library");
    assert_eq!(pairs(&clean, &lead), vec![], "leading, iPod");
}

// Prevents NFD vs NFC spellings of one name from being re-sent every sync.
#[test]
fn the_same_text_in_nfd_and_nfc_is_not_a_change() {
    let nfc = all_text("Soukai\u{ef}na");
    let nfd = all_text("Soukai\u{69}\u{308}na");
    assert_ne!(
        nfc.title, nfd.title,
        "the fixture must differ at byte level"
    );
    assert_eq!(pairs(&nfd, &nfc), vec![], "NFD in the library");
    assert_eq!(pairs(&nfc, &nfd), vec![], "NFD on the iPod");
}

// Prevents a whitespace-only library tag from wiping the iPod value.
#[test]
fn a_library_value_of_only_spaces_is_empty_and_never_clears_the_ipod() {
    let blank = all_text("   ");
    assert_eq!(pairs(&blank, &all_text("Kept")), vec![]);
    // Nothing on the iPod either: still nothing to send.
    assert_eq!(pairs(&blank, &Tags::default()), vec![]);
    assert_eq!(pairs(&blank, &all_text("")), vec![]);
}

// Prevents the raw (padded, NFD) library value from reaching the iPod.
#[test]
fn a_real_change_sends_the_trimmed_nfc_library_value() {
    let lib = t(|x| x.album = Some("  Cafe\u{301} Noir   ".into()));
    let ipod = t(|x| x.album = Some("Old".into()));
    let got = tag_changes(&lib, &ipod);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].field, Field::Album);
    assert_eq!(got[0].library, "Caf\u{e9} Noir");
    assert_eq!(got[0].ipod.as_deref(), Some("Old"));
}

// An iPod value with padding still differs from another text.
#[test]
fn a_padded_ipod_value_against_a_different_library_value_is_a_change() {
    let lib = t(|x| x.title = Some("New".into()));
    let ipod = t(|x| x.title = Some("Old   ".into()));
    assert_eq!(
        pairs(&lib, &ipod),
        vec![change(Field::Title, Some("Old   "), "New")]
    );
}

// Prevents a padded tag from producing an update op in a whole plan.
#[test]
fn a_padded_tag_on_a_track_with_a_changed_hash_makes_no_update_op() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.lib_mut(1).tags.title = Some("Title1   ".into());
    w.mapping_mut(1).synced_tags_hash = [9; 32];
    let p = w.plan();
    assert!(p.update_tags.is_empty(), "{:?}", p.update_tags);
}

#[test]
fn an_empty_library_compilation_mark_does_not_clear_the_ipod_in_a_plan() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.device.tracks[0].tags.compilation = true;
    w.mapping_mut(1).synced_tags_hash = [9; 32];
    let p = w.plan();
    assert!(p.update_tags.is_empty(), "{:?}", p.update_tags);
}

#[test]
fn removal_reasons() {
    let mut w = World::default();
    w.synced(1, 0xA1).synced(2, 0xA2).synced(3, 0xA3);
    // 1: the file is gone (marked missing). 2: no longer in any listed
    // playlist. 3: stays. 4: mapped but absent from the index altogether.
    w.lib_mut(1).missing_since = Some(5);
    w.device.tracks.push(ipod_track(0xA4, 123, MS, tags(4)));
    w.tracks_map.push(TrackMapping {
        track_id: 4,
        dbid: 0xA4,
        synced_tags_hash: [0; 32],
        synced_size: 123,
        synced_duration_ms: Some(MS),
    });
    w.playlist(10, "Keep", &ids(&[3]), None);
    w.scope = Some(vec!["Keep".into()]);
    let p = w.plan();
    let got: Vec<_> = p
        .remove
        .iter()
        .map(|r| (r.dbid, r.track_id, r.reason, r.size))
        .collect();
    assert_eq!(
        got,
        vec![
            (0xA1, Some(1), RemoveReason::MissingFromLibrary, SIZE),
            (0xA2, Some(2), RemoveReason::OutOfScope, SIZE),
            (0xA4, Some(4), RemoveReason::MissingFromLibrary, 123),
        ]
    );
    assert!(p.copy.is_empty());
    assert_eq!(p.expected.track_count, 1);
    assert_eq!(p.space.needed, -(2 * SIZE as i64 + 123));
}

#[test]
fn a_missing_track_in_scope_all_is_removed_not_kept() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.lib_mut(1).missing_since = Some(5);
    let p = w.plan();
    assert_eq!(p.remove.len(), 1);
    assert_eq!(p.remove[0].reason, RemoveReason::MissingFromLibrary);
    assert!(p.unmanaged.tracks.is_empty());
}

#[test]
fn unmanaged_tracks_are_kept_and_reported_by_default() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.device.tracks.push(ipod_track(0xF2, 50, MS, tags(8)));
    w.device.tracks.push(ipod_track(0xF1, 60, MS, tags(9)));
    let p = w.plan();
    assert!(p.remove.is_empty(), "{:?}", p.remove);
    assert_eq!(p.unmanaged.tracks, vec![0xF1, 0xF2]);
    assert!(p.is_empty());
    assert_eq!(p.expected.track_count, 3);
}

#[test]
fn unmanaged_tracks_are_removed_when_asked() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.device.tracks.push(ipod_track(0xF2, 50, MS, tags(8)));
    w.device.tracks.push(ipod_track(0xF1, 60, MS, tags(9)));
    w.remove_unmanaged = true;
    let p = w.plan();
    let got: Vec<_> = p
        .remove
        .iter()
        .map(|r| (r.dbid, r.track_id, r.reason, r.size))
        .collect();
    assert_eq!(
        got,
        vec![
            (0xF1, None, RemoveReason::Unmanaged, 60),
            (0xF2, None, RemoveReason::Unmanaged, 50),
        ]
    );
    assert!(p.unmanaged.tracks.is_empty());
    assert_eq!(p.expected.track_count, 1);
    // The managed track is untouched.
    assert!(p.remove.iter().all(|r| r.dbid != 0xA1));
}

#[test]
fn stats_to_pull_counts_managed_tracks_with_plays_or_skips() {
    let mut w = World::default();
    w.synced(1, 0xA1)
        .synced(2, 0xA2)
        .synced(3, 0xA3)
        .synced(4, 0xA4);
    w.device.tracks.push(ipod_track(0xF1, 60, MS, tags(9)));
    let set = |w: &mut World, dbid: u64, plays: u32, skips: u32| {
        let t = w
            .device
            .tracks
            .iter_mut()
            .find(|t| t.adopt.dbid == dbid)
            .unwrap();
        t.recent_plays = plays;
        t.recent_skips = skips;
    };
    set(&mut w, 0xA1, 2, 0); // plays
    set(&mut w, 0xA2, 0, 1); // skips only
    // 0xA3: nothing. 0xA4 stays at zero too.
    set(&mut w, 0xF1, 5, 5); // unmanaged: not ours
    assert_eq!(w.plan().stats_to_pull, 2);

    // A mapped track about to be removed still has stats to pull first.
    w.lib_mut(1).missing_since = Some(1);
    assert_eq!(w.plan().stats_to_pull, 2);
}

// ---- playlists

fn names(p: &Plan) -> Vec<(&'static str, String)> {
    p.playlists
        .iter()
        .map(|o| match o {
            PlaylistOp::Delete { name, .. } => ("delete", name.clone()),
            PlaylistOp::Rename { to, .. } => ("rename", to.clone()),
            PlaylistOp::Create { name, .. } => ("create", name.clone()),
            PlaylistOp::SetMembers { name, .. } => ("set", name.clone()),
        })
        .collect()
}

#[test]
fn a_mapped_playlist_claims_its_ipod_playlist_by_the_stored_name() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    // The file was renamed since the last sync: the stored name finds the
    // iPod playlist, which is renamed rather than replaced.
    w.ipod_playlist(7, "Old name", &[0xA1]);
    w.playlist(10, "New name", &ids(&[1]), Some("Old name"));
    let p = w.plan();
    assert_eq!(
        p.playlists,
        vec![PlaylistOp::Rename {
            playlist_id: 10,
            ipod_id: 7,
            from: "Old name".into(),
            to: "New name".into(),
        }]
    );
    assert!(p.unmanaged.playlists.is_empty());
    assert_eq!(p.expected.playlists[0].name, "New name");
}

#[test]
fn the_stored_name_wins_over_a_name_match_of_another_playlist() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    // File A is mapped to iPod "X". File X has no mapping. A must get "X"
    // (and be renamed to A); X must not steal it and gets created.
    w.ipod_playlist(7, "X", &[0xA1]);
    w.playlist(10, "A", &ids(&[1]), Some("X"));
    w.playlist(11, "X", &ids(&[1]), None);
    let p = w.plan();
    assert_eq!(
        p.playlists,
        vec![
            PlaylistOp::Rename {
                playlist_id: 10,
                ipod_id: 7,
                from: "X".into(),
                to: "A".into(),
            },
            PlaylistOp::Create {
                playlist_id: 11,
                name: "X".into(),
                members: vec![Member::Track(0xA1)],
            },
        ]
    );
}

#[test]
fn a_tombstone_deletes_its_ipod_playlist() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.ipod_playlist(7, "Gone", &[0xA1]);
    w.ipod_playlist(8, "Other", &[]);
    w.removed.push((10, "Gone".into()));
    w.map_playlist(10, None, "Gone");
    let p = w.plan();
    assert_eq!(
        p.playlists,
        vec![PlaylistOp::Delete {
            playlist_id: Some(10),
            ipod_id: 7,
            name: "Gone".into(),
            reason: PlaylistDeleteReason::FileDeleted,
        }]
    );
    // Only the mapped one goes; the other is unmanaged and kept.
    assert_eq!(p.unmanaged.playlists, vec!["Other"]);
    // The tracks stay.
    assert!(p.remove.is_empty());
}

#[test]
fn a_tombstone_whose_ipod_playlist_is_already_gone_does_nothing() {
    let mut w = World::default();
    w.removed.push((10, "Gone".into()));
    w.map_playlist(10, None, "Gone");
    let p = w.plan();
    assert!(p.playlists.is_empty(), "{:?}", p.playlists);
}

#[test]
fn a_tombstone_never_deletes_an_unmapped_playlist_of_the_same_name() {
    let mut w = World::default();
    // A tombstone with no mapping on this device must not touch the iPod's
    // playlist of that name.
    w.ipod_playlist(7, "Gone", &[]);
    w.removed.push((10, "Gone".into()));
    let p = w.plan();
    assert!(p.playlists.is_empty(), "{:?}", p.playlists);
    assert_eq!(p.unmanaged.playlists, vec!["Gone"]);
}

#[test]
fn a_mapped_playlist_out_of_scope_is_deleted_from_the_ipod() {
    let mut w = World::default();
    w.synced(1, 0xA1).synced(2, 0xA2);
    w.ipod_playlist(7, "Out", &[0xA2]);
    w.ipod_playlist(8, "In", &[0xA1]);
    w.playlist(10, "Out", &ids(&[2]), Some("Out"));
    w.playlist(11, "In", &ids(&[1]), Some("In"));
    w.scope = Some(vec!["In".into()]);
    let p = w.plan();
    assert_eq!(
        p.playlists,
        vec![PlaylistOp::Delete {
            playlist_id: Some(10),
            ipod_id: 7,
            name: "Out".into(),
            reason: PlaylistDeleteReason::OutOfScope,
        }]
    );
    assert_eq!(
        p.expected
            .playlists
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        vec!["In"]
    );
    // Its track left the scope too.
    assert_eq!(p.remove.len(), 1);
    assert_eq!(p.remove[0].reason, RemoveReason::OutOfScope);
}

#[test]
fn an_unmapped_playlist_out_of_scope_is_ignored() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.playlist(10, "Out", &ids(&[1]), None);
    w.playlist(11, "In", &ids(&[1]), None);
    w.scope = Some(vec!["In".into()]);
    let p = w.plan();
    assert_eq!(names(&p), vec![("create", "In".to_owned())]);
}

#[test]
fn unmapped_playlists_match_free_ipod_playlists_by_name_or_are_created() {
    // (file name, iPod name, matches)
    let cases = [
        ("Road", "Road", true),
        ("Dance_Trance", "Dance/Trance", true),
        (
            "BO - 2001_ a Space Odyssey",
            "BO - 2001: a Space Odyssey",
            true,
        ),
        ("Road", "Roads", false),
        ("Road", "Other", false),
    ];
    for (file, on_ipod, matches) in cases {
        let mut w = World::default();
        w.synced(1, 0xA1).synced(2, 0xA2);
        // Different members so a match shows as SetMembers on that iPod id.
        w.ipod_playlist(7, on_ipod, &[0xA2]);
        w.playlist(10, file, &ids(&[1]), None);
        let p = w.plan();
        let case = format!("{file:?} vs {on_ipod:?}");
        if matches {
            assert_eq!(
                p.playlists,
                vec![PlaylistOp::SetMembers {
                    playlist_id: 10,
                    ipod_id: 7,
                    // Differs from the file only by file-name characters: the
                    // iPod's own name stays.
                    name: on_ipod.into(),
                    members: vec![Member::Track(0xA1)],
                }],
                "{case}"
            );
            assert!(p.unmanaged.playlists.is_empty(), "{case}");
        } else {
            assert_eq!(
                p.playlists,
                vec![PlaylistOp::Create {
                    playlist_id: 10,
                    name: file.into(),
                    members: vec![Member::Track(0xA1)],
                }],
                "{case}"
            );
            assert_eq!(p.unmanaged.playlists, vec![on_ipod.to_owned()], "{case}");
        }
    }
}

// ---- smart playlist exports

impl World {
    fn smart(&mut self, names: &[&str]) -> &mut Self {
        self.device.smart_playlists = names.iter().map(|n| n.to_string()).collect();
        self
    }
}

fn skipped(name: &str) -> PlaylistSkipped {
    PlaylistSkipped {
        name: name.into(),
        reason: PlaylistSkipReason::SmartOnIpod,
    }
}

#[test]
fn a_file_named_like_a_smart_playlist_is_skipped_by_each_name_pass() {
    // (file name, smart playlist name on the iPod)
    let cases = [
        ("Top rated", "Top rated"),
        ("top RATED", "Top rated"),
        ("Dance_Trance", "Dance/Trance"),
        // NFD in the file, NFC on the iPod.
        ("Ann\u{65}\u{301}es 90", "Ann\u{e9}es 90"),
    ];
    for (file, smart) in cases {
        let mut w = World::default();
        w.synced(1, 0xA1).smart(&[smart]);
        // An unresolved entry would show up in entries_skipped if the file
        // were planned at all.
        w.playlist(10, file, &[Some(1), None], None);
        let p = w.plan();
        let case = format!("{file:?} vs {smart:?}");
        assert_eq!(p.playlists_skipped, vec![skipped(file)], "{case}");
        assert!(p.playlists.is_empty(), "{case}: {:?}", p.playlists);
        assert!(p.entries_skipped.is_empty(), "{case}");
        assert!(p.expected.playlists.is_empty(), "{case}");
        assert!(p.is_empty(), "{case}: {p:?}");
    }
}

#[test]
fn a_file_not_named_like_a_smart_playlist_is_created_and_not_listed() {
    let mut w = World::default();
    w.synced(1, 0xA1).smart(&["Top rated"]);
    w.playlist(10, "Top rated 2", &ids(&[1]), None);
    let p = w.plan();
    assert!(p.playlists_skipped.is_empty(), "{:?}", p.playlists_skipped);
    assert_eq!(names(&p), vec![("create", "Top rated 2".to_owned())]);
    assert_eq!(p.expected.playlists.len(), 1);
}

#[test]
fn only_the_smart_named_files_are_skipped_and_the_others_still_synced() {
    let mut w = World::default();
    w.synced(1, 0xA1).smart(&["Zeta", "Alpha"]);
    w.playlist(10, "Zeta", &ids(&[1]), None);
    w.playlist(11, "Alpha", &ids(&[1]), None);
    w.playlist(12, "Normal", &ids(&[1]), None);
    let p = w.plan();
    // In name order, like the rest of the plan.
    assert_eq!(p.playlists_skipped, vec![skipped("Alpha"), skipped("Zeta")]);
    assert_eq!(names(&p), vec![("create", "Normal".to_owned())]);
    let exp: Vec<&str> = p
        .expected
        .playlists
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(exp, vec!["Normal"]);
}

#[test]
fn a_smart_named_file_mapped_to_a_regular_ipod_playlist_is_synced() {
    let mut w = World::default();
    w.synced(1, 0xA1).synced(2, 0xA2).smart(&["Mix"]);
    w.ipod_playlist(7, "Mix", &[0xA2]);
    w.playlist(10, "Mix", &ids(&[1]), Some("Mix"));
    let p = w.plan();
    assert!(p.playlists_skipped.is_empty(), "{:?}", p.playlists_skipped);
    assert_eq!(
        p.playlists,
        vec![PlaylistOp::SetMembers {
            playlist_id: 10,
            ipod_id: 7,
            name: "Mix".into(),
            members: vec![Member::Track(0xA1)],
        }]
    );
    assert_eq!(p.expected.playlists.len(), 1);
}

#[test]
fn a_smart_named_file_mapped_by_id_to_a_differently_named_playlist_is_synced() {
    let mut w = World::default();
    w.synced(1, 0xA1).synced(2, 0xA2).smart(&["Mix"]);
    w.ipod_playlist(7, "Old name", &[0xA2]);
    w.playlist(10, "Mix", &ids(&[1]), None);
    w.map_playlist(10, Some(7), "Old name");
    let p = w.plan();
    assert!(p.playlists_skipped.is_empty(), "{:?}", p.playlists_skipped);
    assert_eq!(
        names(&p),
        vec![("rename", "Mix".to_owned()), ("set", "Mix".to_owned())]
    );
}

#[test]
fn a_smart_named_file_matching_a_regular_ipod_playlist_by_name_is_synced() {
    // Each pass: exact, case, file-name characters.
    for (file, regular) in [("Mix", "Mix"), ("mix", "MIX"), ("A_B", "A/B")] {
        let mut w = World::default();
        w.synced(1, 0xA1).synced(2, 0xA2).smart(&[regular]);
        w.ipod_playlist(7, regular, &[0xA2]);
        w.playlist(10, file, &ids(&[1]), None);
        let p = w.plan();
        let case = format!("{file:?} vs {regular:?}");
        assert!(p.playlists_skipped.is_empty(), "{case}");
        assert!(
            p.playlists
                .iter()
                .any(|o| matches!(o, PlaylistOp::SetMembers { ipod_id: 7, .. })),
            "{case}: {:?}",
            p.playlists
        );
        assert!(
            !p.playlists
                .iter()
                .any(|o| matches!(o, PlaylistOp::Create { .. })),
            "{case}"
        );
        assert_eq!(p.expected.playlists.len(), 1, "{case}");
    }
}

#[test]
fn a_regular_playlist_claimed_once_leaves_a_second_smart_named_file_skipped() {
    // Two files "Mix" and "mix"; one regular "Mix" and one smart "Mix". The
    // regular one goes to the first file by name, the other is an export.
    let mut w = World::default();
    w.synced(1, 0xA1).smart(&["Mix"]);
    w.ipod_playlist(7, "Mix", &[]);
    w.playlist(10, "Mix", &ids(&[1]), None);
    w.playlist(11, "mix", &ids(&[1]), None);
    let p = w.plan();
    // Sorted by name: "Mix" < "mix", so "Mix" takes the regular playlist.
    assert_eq!(p.playlists_skipped, vec![skipped("mix")]);
    assert_eq!(
        p.playlists,
        vec![PlaylistOp::SetMembers {
            playlist_id: 10,
            ipod_id: 7,
            name: "Mix".into(),
            members: vec![Member::Track(0xA1)],
        }]
    );
}

#[test]
fn playlists_out_of_scope_are_not_listed_as_skipped() {
    let mut w = World::default();
    w.synced(1, 0xA1).smart(&["Top rated"]);
    w.playlist(10, "In", &ids(&[1]), None);
    w.playlist(11, "Top rated", &ids(&[1]), None);
    w.scope = Some(vec!["In".into()]);
    let p = w.plan();
    assert!(p.playlists_skipped.is_empty(), "{:?}", p.playlists_skipped);
    assert_eq!(names(&p), vec![("create", "In".to_owned())]);
}

// An unnamed smart playlist matches nothing, whatever the file is called.
#[test]
fn an_unnamed_smart_playlist_skips_no_file() {
    for file in ["", "  "] {
        let mut w = World::default();
        w.synced(1, 0xA1).smart(&["", "  "]);
        w.playlist(10, file, &ids(&[1]), None);
        let p = w.plan();
        let case = format!("file {file:?}");
        assert!(p.playlists_skipped.is_empty(), "{case}");
        assert_eq!(names(&p), vec![("create", file.to_owned())], "{case}");
    }
    // And it does not hide a real name next to it.
    let mut w = World::default();
    w.synced(1, 0xA1).smart(&["", "Top rated"]);
    w.playlist(10, "Top rated", &ids(&[1]), None);
    w.playlist(11, "Other", &ids(&[1]), None);
    let p = w.plan();
    assert_eq!(p.playlists_skipped, vec![skipped("Top rated")]);
    assert_eq!(names(&p), vec![("create", "Other".to_owned())]);
}

#[test]
fn no_smart_playlists_on_the_ipod_skips_nothing() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.playlist(10, "Top rated", &ids(&[1]), None);
    let p = w.plan();
    assert!(p.playlists_skipped.is_empty());
    assert_eq!(names(&p), vec![("create", "Top rated".to_owned())]);
}

#[test]
fn a_name_differing_only_in_case_is_matched_and_renamed() {
    let mut w = World::default();
    w.synced(1, 0xA1).synced(2, 0xA2);
    w.ipod_playlist(7, "MIX", &[0xA2]);
    w.playlist(10, "Mix", &ids(&[1]), None);
    let p = w.plan();
    assert!(
        !p.playlists
            .iter()
            .any(|o| matches!(o, PlaylistOp::Create { .. })),
        "{:?}",
        p.playlists
    );
    assert!(p.unmanaged.playlists.is_empty());
    assert!(
        p.playlists
            .iter()
            .any(|o| matches!(o, PlaylistOp::SetMembers { ipod_id: 7, .. }))
    );
    // A change of case is a real rename and reaches the iPod.
    assert!(
        p.playlists.contains(&PlaylistOp::Rename {
            playlist_id: 10,
            ipod_id: 7,
            from: "MIX".into(),
            to: "Mix".into(),
        }),
        "{:?}",
        p.playlists
    );
}

#[test]
fn an_exact_name_wins_over_a_looser_one() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    // Both could match "Mix"; the exact one must be taken, the other kept.
    w.ipod_playlist(6, "MIX", &[]);
    w.ipod_playlist(7, "Mix", &[]);
    w.playlist(10, "Mix", &ids(&[1]), None);
    let p = w.plan();
    assert_eq!(
        p.playlists,
        vec![PlaylistOp::SetMembers {
            playlist_id: 10,
            ipod_id: 7,
            name: "Mix".into(),
            members: vec![Member::Track(0xA1)],
        }]
    );
    assert_eq!(p.unmanaged.playlists, vec!["MIX"]);
}

#[test]
fn the_ipod_name_is_renamed_when_it_differs_by_more_than_file_name_characters() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.ipod_playlist(7, "Old", &[0xA1]);
    w.playlist(10, "New", &ids(&[1]), Some("Old"));
    // And one that only differs by a character a file name cannot hold.
    w.ipod_playlist(8, "Dance/Trance", &[0xA1]);
    w.playlist(11, "Dance_Trance", &ids(&[1]), Some("Dance/Trance"));
    let p = w.plan();
    assert_eq!(
        p.playlists,
        vec![PlaylistOp::Rename {
            playlist_id: 10,
            ipod_id: 7,
            from: "Old".into(),
            to: "New".into(),
        }]
    );
    assert_eq!(
        p.expected
            .playlists
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Dance/Trance", "New"]
    );
}

#[test]
fn set_members_when_the_dbids_differ_and_not_when_they_are_identical() {
    let cases: [(&str, Vec<u64>, Vec<i64>, bool); 6] = [
        ("identical", vec![0xA1, 0xA2], vec![1, 2], false),
        ("empty both", vec![], vec![], false),
        ("order differs", vec![0xA2, 0xA1], vec![1, 2], true),
        ("member added", vec![0xA1], vec![1, 2], true),
        ("member dropped", vec![0xA1, 0xA2], vec![1], true),
        ("emptied", vec![0xA1], vec![], true),
    ];
    for (case, on_ipod, in_file, expect) in cases {
        let mut w = World::default();
        w.synced(1, 0xA1).synced(2, 0xA2);
        w.ipod_playlist(7, "Mix", &on_ipod);
        w.playlist(10, "Mix", &ids(&in_file), Some("Mix"));
        let p = w.plan();
        if expect {
            let want: Vec<Member> = in_file
                .iter()
                .map(|&i| Member::Track(0xA0 + i as u64))
                .collect();
            assert_eq!(
                p.playlists,
                vec![PlaylistOp::SetMembers {
                    playlist_id: 10,
                    ipod_id: 7,
                    name: "Mix".into(),
                    members: want,
                }],
                "{case}"
            );
        } else {
            assert!(p.playlists.is_empty(), "{case}: {:?}", p.playlists);
            assert!(p.is_empty(), "{case}");
        }
    }
}

#[test]
fn a_copied_member_makes_a_set_members_with_the_new_track() {
    let mut w = World::default();
    w.synced(1, 0xA1).fresh(2);
    w.ipod_playlist(7, "Mix", &[0xA1]);
    w.playlist(10, "Mix", &ids(&[1, 2]), Some("Mix"));
    let p = w.plan();
    assert_eq!(p.copy.len(), 1);
    assert_eq!(
        p.playlists,
        vec![PlaylistOp::SetMembers {
            playlist_id: 10,
            ipod_id: 7,
            name: "Mix".into(),
            members: vec![Member::Track(0xA1), Member::New(2)],
        }]
    );
}

#[test]
fn a_new_playlist_is_created_with_its_members() {
    let mut w = World::default();
    w.synced(1, 0xA1).fresh(2);
    w.playlist(10, "Fresh", &ids(&[2, 1]), None);
    let p = w.plan();
    assert_eq!(
        p.playlists,
        vec![PlaylistOp::Create {
            playlist_id: 10,
            name: "Fresh".into(),
            members: vec![Member::New(2), Member::Track(0xA1)],
        }]
    );
}

#[test]
fn repeated_tracks_are_kept_in_order() {
    let mut w = World::default();
    w.synced(1, 0xA1).synced(2, 0xA2);
    w.ipod_playlist(7, "Loop", &[0xA1, 0xA2, 0xA1]);
    w.playlist(10, "Loop", &ids(&[1, 2, 1]), Some("Loop"));
    let p = w.plan();
    assert!(p.playlists.is_empty(), "{:?}", p.playlists);
    assert_eq!(
        p.expected.playlists[0].members,
        vec![
            Member::Track(0xA1),
            Member::Track(0xA2),
            Member::Track(0xA1)
        ]
    );

    // The iPod lost the repeat: the whole list is set again, repeat included.
    let mut w = w.clone();
    w.device.playlists[0].members = vec![0xA1, 0xA2];
    let p = w.plan();
    assert_eq!(
        p.playlists,
        vec![PlaylistOp::SetMembers {
            playlist_id: 10,
            ipod_id: 7,
            name: "Loop".into(),
            members: vec![
                Member::Track(0xA1),
                Member::Track(0xA2),
                Member::Track(0xA1)
            ],
        }]
    );
}

#[test]
fn entries_that_cannot_be_synced_are_left_out_and_listed() {
    let mut w = World::default();
    w.synced(1, 0xA1).synced(2, 0xA2).fresh(3);
    w.lib_mut(2).missing_since = Some(4);
    w.fresh(4);
    w.lib_mut(4).drm = true;
    // 1 ok; unresolved; 2 missing; 99 not indexed at all; 4 FairPlay; 3 copy.
    let entries = [Some(1), None, Some(2), Some(99), Some(4), Some(3)];
    w.playlist(10, "Mix", &entries, None);
    // A second playlist so positions are per playlist.
    w.playlist(11, "Aaa", &[None], None);
    let p = w.plan();
    assert_eq!(
        p.entries_skipped,
        vec![
            EntrySkipped {
                playlist: "Aaa".into(),
                position: 1,
                reason: EntryReason::Unresolved
            },
            EntrySkipped {
                playlist: "Mix".into(),
                position: 2,
                reason: EntryReason::Unresolved
            },
            EntrySkipped {
                playlist: "Mix".into(),
                position: 3,
                reason: EntryReason::Missing
            },
            EntrySkipped {
                playlist: "Mix".into(),
                position: 4,
                reason: EntryReason::Missing
            },
            EntrySkipped {
                playlist: "Mix".into(),
                position: 5,
                reason: EntryReason::TrackSkipped
            },
        ]
    );
    let mix = p
        .expected
        .playlists
        .iter()
        .find(|e| e.name == "Mix")
        .unwrap();
    assert_eq!(mix.members, vec![Member::Track(0xA1), Member::New(3)]);
    // Track 2 is missing so its iPod copy goes.
    assert_eq!(p.remove.len(), 1);
    assert_eq!(p.remove[0].dbid, 0xA2);
}

#[test]
fn entries_of_playlists_out_of_scope_are_not_reported() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.playlist(10, "In", &ids(&[1]), None);
    w.playlist(11, "Out", &[None], None);
    w.scope = Some(vec!["In".into()]);
    assert!(w.plan().entries_skipped.is_empty());
}

#[test]
fn unmanaged_playlists_are_kept_by_default_and_deleted_when_asked() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.ipod_playlist(7, "Zed", &[0xA1]);
    w.ipod_playlist(8, "Alpha", &[]);
    let p = w.plan();
    assert!(p.playlists.is_empty());
    assert_eq!(p.unmanaged.playlists, vec!["Alpha", "Zed"]);
    assert!(p.is_empty());

    w.remove_unmanaged = true;
    let p = w.plan();
    assert_eq!(
        p.playlists,
        vec![
            PlaylistOp::Delete {
                playlist_id: None,
                ipod_id: 8,
                name: "Alpha".into(),
                reason: PlaylistDeleteReason::Unmanaged,
            },
            PlaylistOp::Delete {
                playlist_id: None,
                ipod_id: 7,
                name: "Zed".into(),
                reason: PlaylistDeleteReason::Unmanaged,
            },
        ]
    );
    assert!(p.unmanaged.playlists.is_empty());
    // A deleted playlist never leaves the tracks behind removed.
    assert!(p.remove.is_empty());
}

#[test]
fn of_two_ipod_playlists_with_one_name_the_first_is_taken_and_the_second_is_unmanaged() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.ipod_playlist(7, "Dup", &[0xA1]);
    w.ipod_playlist(8, "Dup", &[]);
    w.playlist(10, "Dup", &ids(&[1]), None);
    let p = w.plan();
    // The first one already has the right members.
    assert!(p.playlists.is_empty(), "{:?}", p.playlists);
    assert_eq!(p.unmanaged.playlists, vec!["Dup"]);

    // With remove_unmanaged only the second one goes.
    w.remove_unmanaged = true;
    let p = w.plan();
    assert_eq!(
        p.playlists,
        vec![PlaylistOp::Delete {
            playlist_id: None,
            ipod_id: 8,
            name: "Dup".into(),
            reason: PlaylistDeleteReason::Unmanaged,
        }]
    );
}

#[test]
fn a_mapped_playlist_claims_one_ipod_playlist_when_two_share_the_name() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.ipod_playlist(7, "Dup", &[]);
    w.ipod_playlist(8, "Dup", &[]);
    w.playlist(10, "Dup", &ids(&[1]), Some("Dup"));
    w.playlist(11, "Dup2", &ids(&[1]), Some("Dup"));
    let p = w.plan();
    // Two files mapped to the same stored name get one iPod playlist each,
    // never the same one twice.
    let mut targets: Vec<u64> = p
        .playlists
        .iter()
        .filter_map(|o| match o {
            PlaylistOp::SetMembers { ipod_id, .. } | PlaylistOp::Rename { ipod_id, .. } => {
                Some(*ipod_id)
            }
            _ => None,
        })
        .collect();
    targets.sort_unstable();
    targets.dedup();
    assert_eq!(targets, vec![7, 8], "{:?}", p.playlists);
}

// ---- ordering, expected state, space, determinism

/// A scene that exercises every kind of playlist op and several kinds of
/// track op.
fn busy() -> World {
    let mut w = World::default();
    for i in 1..=4 {
        w.synced(i, 0xA0 + i as u64);
    }
    w.fresh(5).fresh(6);
    // iPod playlists.
    w.ipod_playlist(1, "z gone", &[0xA1]);
    w.ipod_playlist(2, "b gone", &[0xA2]);
    w.ipod_playlist(3, "old y", &[0xA1]);
    w.ipod_playlist(4, "old a", &[0xA2]);
    w.ipod_playlist(5, "same", &[0xA3]);
    w.ipod_playlist(6, "change q", &[0xA3]);
    w.ipod_playlist(7, "change c", &[0xA3]);
    w.ipod_playlist(8, "stray", &[]);
    // Tombstones.
    w.removed.push((90, "z gone".into()));
    w.map_playlist(90, None, "z gone");
    w.removed.push((91, "b gone".into()));
    w.map_playlist(91, None, "b gone");
    // Renames.
    w.playlist(20, "y", &ids(&[1]), Some("old y"));
    w.playlist(21, "a", &ids(&[2]), Some("old a"));
    // Unchanged.
    w.playlist(22, "same", &ids(&[3]), Some("same"));
    // Member changes (one of them includes a copy).
    w.playlist(23, "change q", &ids(&[3, 4]), Some("change q"));
    w.playlist(24, "change c", &ids(&[5]), Some("change c"));
    // Creates.
    w.playlist(25, "new m", &ids(&[6]), None);
    w.playlist(26, "new d", &ids(&[1]), None);
    w.remove_unmanaged = true;
    w
}

#[test]
fn playlist_ops_run_deletes_renames_creates_then_member_updates_each_by_name() {
    let p = busy().plan();
    let got: Vec<_> = names(&p)
        .into_iter()
        .map(|(k, n)| format!("{k}:{n}"))
        .collect();
    assert_eq!(
        got,
        vec![
            "delete:b gone",
            "delete:stray",
            "delete:z gone",
            "rename:a",
            "rename:y",
            "create:new d",
            "create:new m",
            "set:change c",
            "set:change q",
        ]
    );
}

#[test]
fn a_rename_onto_a_name_freed_by_a_delete_comes_after_the_delete() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.ipod_playlist(1, "Taken", &[]);
    w.ipod_playlist(2, "Old", &[0xA1]);
    w.playlist(10, "Taken", &ids(&[1]), Some("Old"));
    // An unmanaged iPod playlist of that name is deleted by remove_unmanaged;
    // here it is a tombstone's.
    w.removed.push((11, "Taken".into()));
    w.map_playlist(11, None, "Taken");
    let p = w.plan();
    let kinds: Vec<&str> = names(&p).into_iter().map(|(k, _)| k).collect();
    assert_eq!(kinds, vec!["delete", "rename"], "{:?}", p.playlists);
}

#[test]
fn expected_state_counts_tracks_and_lists_managed_playlists_by_name() {
    let p = busy().plan();
    // 4 synced + 2 fresh; nothing removed (every synced track is in scope).
    assert_eq!(p.copy.len(), 2);
    assert_eq!(p.remove.len(), 0);
    assert_eq!(p.expected.track_count, 6);
    let got: Vec<(&str, Vec<Member>)> = p
        .expected
        .playlists
        .iter()
        .map(|e| (e.name.as_str(), e.members.clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("a", vec![Member::Track(0xA2)]),
            ("change c", vec![Member::New(5)]),
            ("change q", vec![Member::Track(0xA3), Member::Track(0xA4)]),
            ("new d", vec![Member::Track(0xA1)]),
            ("new m", vec![Member::New(6)]),
            ("same", vec![Member::Track(0xA3)]),
            // "old y" keeps nothing of "y": renamed to the file's name.
            ("y", vec![Member::Track(0xA1)]),
        ]
    );
}

#[test]
fn expected_track_count_follows_copies_and_removals_not_replacements() {
    let mut w = World::default();
    w.synced(1, 0xA1)
        .synced(2, 0xA2)
        .synced(3, 0xA3)
        .fresh(4)
        .fresh(5);
    // 2 replaced, 3 gone from the library, one unmanaged removed.
    w.lib_mut(2).size += 10;
    w.lib_mut(2).info.duration_ms = Some(MS + 5_000);
    w.lib_mut(3).missing_since = Some(1);
    w.device.tracks.push(ipod_track(0xF1, 10, MS, tags(9)));
    w.remove_unmanaged = true;
    let p = w.plan();
    assert_eq!((p.copy.len(), p.replace.len(), p.remove.len()), (2, 1, 2));
    // 4 on the iPod + 2 copies - 2 removals.
    assert_eq!(p.expected.track_count, 4);
}

fn sp(capacity: u64, free: u64) -> Option<Space> {
    Some(Space { capacity, free })
}

#[test]
fn the_space_check_keeps_a_one_percent_margin() {
    // needed = one copy of `size` bytes; capacity 10_000 so the margin is 100.
    let cases: [(&str, u64, Option<Space>, bool); 9] = [
        ("exact fit with the margin", 1_000, sp(10_000, 1_100), true),
        ("one byte over the margin", 1_001, sp(10_000, 1_100), false),
        ("fits without margin only", 1_100, sp(10_000, 1_100), false),
        ("plenty", 10, sp(10_000, 5_000), true),
        ("no room", 500, sp(10_000, 0), false),
        (
            "margin is capacity / 100, rounded down",
            100,
            sp(10_099, 200),
            true,
        ),
        ("one byte short of that", 101, sp(10_099, 200), false),
        ("unknown space cannot refuse", 1_000_000_000, None, true),
        ("nothing to copy, no room", 0, sp(10_000, 0), true),
    ];
    for (case, size, space, fits) in cases {
        let mut w = World::default();
        if size > 0 {
            w.fresh(1);
            w.lib_mut(1).size = size;
        }
        w.space = space;
        let p = w.plan();
        assert_eq!(p.space.needed, size as i64, "{case}");
        assert_eq!(p.space.device, space, "{case}");
        assert_eq!(p.space.fits, fits, "{case}");
    }
}

#[test]
fn a_plan_that_frees_space_has_negative_needed_and_always_fits() {
    let mut w = World::default();
    w.synced(1, 0xA1).synced(2, 0xA2);
    w.lib_mut(1).missing_since = Some(1);
    w.lib_mut(2).missing_since = Some(1);
    w.fresh(3);
    w.lib_mut(3).size = 1_000;
    // Frees 8000, copies 1000; the device is completely full.
    w.space = sp(10_000, 0);
    let p = w.plan();
    assert_eq!(p.space.needed, 1_000 - 2 * SIZE as i64);
    assert!(p.space.needed < 0);
    assert!(p.space.fits);
}

#[test]
fn a_replace_needs_room_for_both_files_at_the_peak() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.lib_mut(1).size = SIZE + 3_000;
    w.lib_mut(1).info.duration_ms = Some(MS + 5_000);
    // The old file goes only after the new one is written: the peak is the
    // whole new file, 7000 bytes, plus a margin of 100.
    w.space = sp(10_000, 7_099);
    let p = w.plan();
    assert_eq!(p.space.needed, 3_000, "net");
    assert_eq!(p.space.peak, (SIZE + 3_000) as i64);
    assert!(!p.space.fits);
    w.space = sp(10_000, 7_100);
    assert!(w.plan().space.fits);
}

#[test]
fn a_same_size_replace_has_no_net_change_but_still_needs_room() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    // A bigger file is not needed: a different recording of the same size
    // cannot be told by size, so make it one byte bigger.
    w.lib_mut(1).size = SIZE + 1;
    w.lib_mut(1).info.duration_ms = Some(MS + 5_000);
    w.space = sp(10_000, 0);
    let p = w.plan();
    assert_eq!(p.space.needed, 1);
    assert_eq!(p.space.peak, (SIZE + 1) as i64);
    assert!(!p.space.fits, "a full iPod cannot hold both files");
}

#[test]
fn removals_free_room_before_copies_and_replaced_files_do_not() {
    let mut w = World::default();
    w.synced(1, 0xA1).synced(2, 0xA2).fresh(3);
    w.lib_mut(1).missing_since = Some(1); // removal: frees SIZE first
    w.lib_mut(2).size = SIZE + 10; // replace: old file freed last
    w.lib_mut(2).info.duration_ms = Some(MS + 5_000);
    let p = w.plan();
    // peak = copy 4000 + new file 4010 - removed 4000
    assert_eq!(p.space.peak, (SIZE + 10) as i64);
    // net = peak - the replaced old file
    assert_eq!(p.space.needed, 10);
}

#[test]
fn huge_sizes_do_not_overflow_the_space_arithmetic() {
    let mut w = World::default();
    w.fresh(1).fresh(2);
    w.lib_mut(1).size = u64::MAX;
    w.lib_mut(2).size = u64::MAX;
    w.space = sp(10_000, 10_000);
    let p = w.plan();
    assert_eq!(p.space.needed, i64::MAX);
    assert_eq!(p.space.peak, i64::MAX);
    assert!(!p.space.fits);
}

/// xorshift: a fixed, dependency free source of shuffles.
fn shuffle<T>(v: &mut [T], seed: u64) {
    let mut x = seed | 1;
    for i in (1..v.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
}

#[test]
fn the_same_input_in_any_order_gives_the_same_plan() {
    let mut base = busy();
    // Some more: stale, unmanaged, FairPlay, plays, out-of-scope-free entries.
    base.synced(7, 0xB7);
    base.device.tracks.retain(|t| t.adopt.dbid != 0xB7);
    base.device.tracks.push(ipod_track(0xF1, 10, MS, tags(9)));
    base.device.tracks.push(ipod_track(0xF2, 20, MS, tags(10)));
    base.device.tracks[0].recent_plays = 3;
    // Two mapped tracks whose files are gone: removals whose order must not depend on input order.
    base.lib_mut(3).missing_since = Some(1);
    base.lib_mut(4).missing_since = Some(1);
    base.fresh(8);
    base.lib_mut(8).drm = true;
    base.playlist(27, "drm list", &[Some(8), None, Some(1)], None);
    base.space = sp(1_000_000, 500_000);
    let want = base.plan();
    assert!(!want.is_empty());
    assert!(!want.stale_mappings.is_empty() && !want.skipped.is_empty());
    assert!(!want.unmanaged.tracks.is_empty() || !want.remove.is_empty());

    for seed in 1..=40u64 {
        let mut w = base.clone();
        shuffle(&mut w.library, seed);
        shuffle(&mut w.playlists, seed + 1);
        shuffle(&mut w.removed, seed + 2);
        shuffle(&mut w.tracks_map, seed + 3);
        shuffle(&mut w.playlists_map, seed + 4);
        shuffle(&mut w.device.tracks, seed + 5);
        shuffle(&mut w.device.playlists, seed + 6);
        assert_eq!(w.plan(), want, "seed {seed}");
    }
    // And the same call twice.
    assert_eq!(base.plan(), want);
}

// ---- the synced duration decides between replace and retag

#[test]
fn a_retag_on_a_track_whose_ipod_duration_is_wrong_is_a_tag_update() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    // The iPod reports a duration ten times too long (some do); the file's
    // size and tags changed by a retag, its audio did not.
    w.device.tracks[0].adopt.duration_ms = MS * 10;
    w.lib_mut(1).size += 400;
    w.lib_mut(1).tags.title = Some("Retitled".into());
    w.lib_mut(1).tags_hash = w.library[0].tags.hash();
    let p = w.plan();
    assert!(p.replace.is_empty(), "{:?}", p.replace);
    assert!(p.copy.is_empty() && p.remove.is_empty());
    assert_eq!(p.update_tags.len(), 1, "{p:?}");
    assert_eq!(p.update_tags[0].changes[0].field, Field::Title);
}

#[test]
fn replace_compares_with_the_synced_duration_not_the_ipods() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    // The iPod's duration happens to equal the new file's, but the file
    // differs from the one synced: the audio changed.
    w.lib_mut(1).size += 400;
    w.lib_mut(1).info.duration_ms = Some(MS + 30_000);
    w.device.tracks[0].adopt.duration_ms = MS + 30_000;
    let p = w.plan();
    assert_eq!(p.replace.len(), 1, "{p:?}");
}

#[test]
fn replace_boundaries_on_the_synced_duration() {
    // (synced duration, library duration, size changed, expect a replace)
    let cases: [(Option<u32>, Option<u32>, bool, bool); 7] = [
        (Some(MS), Some(MS + 1_001), true, true),
        (Some(MS), Some(MS - 1_001), true, true),
        (Some(MS), Some(MS + 1_000), true, false),
        (Some(MS), Some(MS + 90_000), false, false), // same size
        (None, Some(MS + 90_000), true, false),      // nothing to compare with
        (Some(MS), None, true, false),
        (None, None, true, false),
    ];
    for (synced, now, size_changed, expect) in cases {
        let mut w = World::default();
        w.synced(1, 0xA1);
        w.mapping_mut(1).synced_duration_ms = synced;
        w.lib_mut(1).info.duration_ms = now;
        if size_changed {
            w.lib_mut(1).size += 400;
        }
        let p = w.plan();
        assert_eq!(
            p.replace.len(),
            usize::from(expect),
            "{synced:?} -> {now:?}, size changed {size_changed}: {p:?}"
        );
    }
}

// ---- playlists claimed by the iPod's playlist id

#[test]
fn a_recorded_id_picks_the_right_playlist_among_two_with_the_same_name() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.ipod_playlist(7, "Mix", &[]);
    w.ipod_playlist(8, "Mix", &[0xA1]);
    w.map_playlist(10, Some(8), "Mix");
    w.playlists.push(lib_playlist(10, "Mix", &ids(&[1])));
    let p = w.plan();
    // The second one is ours and already right; the first stays unmanaged.
    assert!(p.playlists.is_empty(), "{:?}", p.playlists);
    assert_eq!(p.unmanaged.playlists, vec!["Mix"]);
    w.remove_unmanaged = true;
    assert_eq!(
        w.plan().playlists,
        vec![PlaylistOp::Delete {
            playlist_id: None,
            ipod_id: 7,
            name: "Mix".into(),
            reason: PlaylistDeleteReason::Unmanaged,
        }]
    );
}

#[test]
fn a_recorded_id_that_left_the_ipod_falls_back_to_the_name_passes() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    // The mapped playlist (id 8) was deleted on the iPod. The id claims
    // nothing; the playlist is then matched by name like an unmapped one, so
    // the iPod does not end up with two playlists called "Mix".
    w.ipod_playlist(7, "Mix", &[]);
    w.map_playlist(10, Some(8), "Mix");
    w.playlists.push(lib_playlist(10, "Mix", &ids(&[1])));
    let p = w.plan();
    assert_eq!(
        p.playlists,
        vec![PlaylistOp::SetMembers {
            playlist_id: 10,
            ipod_id: 7,
            name: "Mix".into(),
            members: vec![Member::Track(0xA1)],
        }],
        "{:?}",
        p.playlists
    );
    assert!(p.unmanaged.playlists.is_empty());
}

#[test]
fn a_recorded_id_that_left_the_ipod_is_created_when_no_name_matches() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.ipod_playlist(7, "Other", &[]);
    w.map_playlist(10, Some(8), "Mix");
    w.playlists.push(lib_playlist(10, "Mix", &ids(&[1])));
    let p = w.plan();
    assert_eq!(
        p.playlists,
        vec![PlaylistOp::Create {
            playlist_id: 10,
            name: "Mix".into(),
            members: vec![Member::Track(0xA1)],
        }]
    );
    assert_eq!(p.unmanaged.playlists, vec!["Other"]);
}

#[test]
fn a_mapping_without_an_id_falls_back_to_the_name() {
    let mut w = World::default();
    w.synced(1, 0xA1);
    w.ipod_playlist(7, "Mix", &[]);
    w.map_playlist(10, None, "Mix");
    w.playlists.push(lib_playlist(10, "Mix", &ids(&[1])));
    let p = w.plan();
    assert_eq!(
        p.playlists,
        vec![PlaylistOp::SetMembers {
            playlist_id: 10,
            ipod_id: 7,
            name: "Mix".into(),
            members: vec![Member::Track(0xA1)],
        }]
    );
}

#[test]
fn a_tombstone_with_a_recorded_id_deletes_that_playlist_only() {
    let mut w = World::default();
    w.ipod_playlist(7, "Gone", &[]);
    w.ipod_playlist(8, "Gone", &[]);
    w.removed.push((10, "Gone".into()));
    w.map_playlist(10, Some(8), "Gone");
    let p = w.plan();
    assert_eq!(
        p.playlists,
        vec![PlaylistOp::Delete {
            playlist_id: Some(10),
            ipod_id: 8,
            name: "Gone".into(),
            reason: PlaylistDeleteReason::FileDeleted,
        }]
    );
    assert_eq!(p.unmanaged.playlists, vec!["Gone"]);
}

fn names_and_members(e: &[ExpectedPlaylist]) -> Vec<(String, Vec<Member>)> {
    e.iter()
        .map(|e| (e.name.clone(), e.members.clone()))
        .collect()
}
