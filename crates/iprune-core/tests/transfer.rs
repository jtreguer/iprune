//! `transfer`: the track work worked out from a plan, on hand-built data.
//! Pure functions, so nothing here touches a disk or an iPod.

#![allow(clippy::field_reassign_with_default)]
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use iprune_core::adopt::IpodTrack;
use iprune_core::ipod::{Contents, Track};
use iprune_core::plan::{CopyOp, Field, Plan, RemoveOp, RemoveReason, ReplaceOp, TagChange, TagOp};
use iprune_core::store::TrackRow;
use iprune_core::tags::{AudioInfo, FileKind, Tags};
use iprune_core::transfer::{self, Fields, TrackWork};

const GUID: &str = "0x000A27001234ABCD";

fn row(id: i64, name: &str, tags: Tags) -> TrackRow {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&(id as u64).to_le_bytes());
    TrackRow {
        id,
        path: PathBuf::from(format!("/music/{name}")),
        path_key: format!("/music/{name}"),
        size: 1000 + id as u64,
        mtime_ns: 1_500_000_000_999_999_999,
        hash,
        info: AudioInfo {
            kind: FileKind::Mp3,
            duration_ms: Some(200_000 + id as u32),
            bitrate: Some(192),
            samplerate: Some(44_100),
        },
        tags_hash: tags.hash(),
        tags,
        added_at: 0,
        missing_since: None,
        tag_error: None,
        drm: false,
    }
}

fn tags() -> Tags {
    Tags {
        title: Some("Title".into()),
        artist: Some("Artist".into()),
        year: Some(1999),
        track_no: Some(3),
        ..Tags::default()
    }
}

fn ipod_track(dbid: u64) -> Track {
    Track {
        adopt: IpodTrack {
            dbid,
            size: 1,
            duration_ms: 1,
            title: None,
            artist: None,
            album: None,
            playlists: 0,
            playcount: 0,
            hash: None,
        },
        tags: Tags::default(),
        ipod_path: Some(":iPod_Control:Music:F00:A.mp3".into()),
        recent_plays: 0,
        recent_skips: 0,
        skips: 0,
        rating: 0,
        last_played: 0,
    }
}

fn library(rows: Vec<TrackRow>) -> HashMap<i64, TrackRow> {
    rows.into_iter().map(|r| (r.id, r)).collect()
}

// ---- new_dbid

#[test]
fn a_new_dbid_is_the_same_every_time_and_never_zero() {
    let t = row(1, "a.mp3", tags());
    let a = transfer::new_dbid(GUID, &t, &HashSet::new());
    assert_eq!(a, transfer::new_dbid(GUID, &t, &HashSet::new()));
    assert_ne!(a, 0);
}

#[test]
fn a_new_dbid_depends_on_the_ipod_the_track_and_its_content() {
    let t = row(1, "a.mp3", tags());
    let base = transfer::new_dbid(GUID, &t, &HashSet::new());
    let other_ipod = transfer::new_dbid("0x000A27009999AAAA", &t, &HashSet::new());
    let other_track = transfer::new_dbid(GUID, &row(2, "a.mp3", tags()), &HashSet::new());
    let mut changed = row(1, "a.mp3", tags());
    changed.hash[31] ^= 1;
    let other_content = transfer::new_dbid(GUID, &changed, &HashSet::new());
    let all: HashSet<u64> = [base, other_ipod, other_track, other_content]
        .into_iter()
        .collect();
    assert_eq!(
        all.len(),
        4,
        "{base:x} {other_ipod:x} {other_track:x} {other_content:x}"
    );
}

#[test]
fn a_new_dbid_skips_the_ones_taken_and_each_skip_is_stable() {
    let t = row(1, "a.mp3", tags());
    let first = transfer::new_dbid(GUID, &t, &HashSet::new());
    let mut taken = HashSet::from([first]);
    let second = transfer::new_dbid(GUID, &t, &taken);
    assert_ne!(second, first);
    assert_eq!(second, transfer::new_dbid(GUID, &t, &taken), "stable");
    taken.insert(second);
    let third = transfer::new_dbid(GUID, &t, &taken);
    assert!(![first, second].contains(&third));
    // Taking an unrelated id changes nothing.
    taken.insert(first ^ 0xffff);
    assert_eq!(third, transfer::new_dbid(GUID, &t, &taken));
}

// ---- filetype, fields

#[test]
fn every_file_kind_has_the_name_itunes_gives_it() {
    let names: Vec<&str> = [
        FileKind::Mp3,
        FileKind::Aac,
        FileKind::Alac,
        FileKind::Wav,
        FileKind::Aiff,
    ]
    .into_iter()
    .map(transfer::filetype)
    .collect();
    assert_eq!(
        names,
        [
            "MPEG audio file",
            "AAC audio file",
            "Apple Lossless audio file",
            "WAV audio file",
            "AIFF audio file"
        ]
    );
}

#[test]
fn all_fields_cleans_text_and_keeps_only_positive_numbers() {
    let t = row(
        1,
        "a.mp3",
        Tags {
            title: Some("  Cafe\u{301}  ".into()),
            artist: Some("   ".into()),
            album: Some("Album".into()),
            composer: Some("".into()),
            year: Some(0),
            track_no: Some(7),
            track_total: Some(0),
            disc_no: Some(1),
            compilation: true,
            ..Tags::default()
        },
    );
    let f = transfer::all_fields(&t);
    assert_eq!(f.title.as_deref(), Some("Caf\u{e9}"), "trimmed, in NFC");
    assert_eq!(f.artist, None, "blank is empty");
    assert_eq!(f.composer, None);
    assert_eq!(f.album.as_deref(), Some("Album"));
    assert_eq!((f.year, f.track_total), (None, None), "zero is no number");
    assert_eq!((f.track_no, f.disc_no), (Some(7), Some(1)));
    assert_eq!(f.compilation, Some(true));
    let plain = transfer::all_fields(&row(2, "b.mp3", Tags::default()));
    assert_eq!(
        plain.compilation, None,
        "an unmarked file says nothing: it never clears the iPod's mark"
    );
}

#[test]
fn all_fields_uses_the_file_name_when_there_is_no_title() {
    let f = transfer::all_fields(&row(1, "My Song.flac.mp3", Tags::default()));
    assert_eq!(f.title.as_deref(), Some("My Song.flac"));
}

#[test]
fn applied_to_overlays_what_is_set_and_keeps_the_rest() {
    let old = Tags {
        title: Some("Old".into()),
        artist: Some("Old Artist".into()),
        year: Some(1990),
        track_no: Some(2),
        compilation: true,
        ..Tags::default()
    };
    let f = Fields {
        title: Some("New".into()),
        year: Some(2005),
        disc_no: Some(2),
        ..Fields::default()
    };
    let got = f.applied_to(&old);
    assert_eq!(got.title.as_deref(), Some("New"));
    assert_eq!(got.artist.as_deref(), Some("Old Artist"), "None keeps");
    assert_eq!(
        (got.year, got.track_no, got.disc_no),
        (Some(2005), Some(2), Some(2))
    );
    assert!(got.compilation, "None keeps");
    // Some(false) clears; Some(0) reads back as no number, as ipod::read does.
    let g = Fields {
        compilation: Some(false),
        year: Some(0),
        ..Fields::default()
    }
    .applied_to(&old);
    assert!(!g.compilation);
    assert_eq!(g.year, None);
    // Nothing set, nothing changed.
    assert_eq!(Fields::default().applied_to(&old), old);
}

#[test]
fn to_gpod_carries_every_field_and_saturates_big_numbers() {
    let f = Fields {
        title: Some("t".into()),
        artist: Some("a".into()),
        album: Some("al".into()),
        album_artist: Some("aa".into()),
        composer: Some("c".into()),
        genre: Some("g".into()),
        year: Some(1999),
        track_no: Some(2),
        track_total: Some(3),
        disc_no: Some(4),
        disc_total: Some(u32::MAX),
        compilation: Some(true),
    };
    let g = f.to_gpod();
    assert_eq!(g.title.as_deref(), Some("t"));
    assert_eq!(g.artist.as_deref(), Some("a"));
    assert_eq!(g.album.as_deref(), Some("al"));
    assert_eq!(g.albumartist.as_deref(), Some("aa"));
    assert_eq!(g.composer.as_deref(), Some("c"));
    assert_eq!(g.genre.as_deref(), Some("g"));
    assert_eq!(
        (g.year, g.track_nr, g.tracks, g.cd_nr, g.cds),
        (Some(1999), Some(2), Some(3), Some(4), Some(i32::MAX))
    );
    assert_eq!(g.compilation, Some(true));
    assert_eq!(Fields::default().to_gpod(), gpod::TrackFields::default());
}

// ---- track_work

fn plan() -> Plan {
    let mut plan = Plan::default();
    plan.copy = vec![
        CopyOp {
            track_id: 1,
            path: "/music/one.mp3".into(),
            size: 1001,
        },
        CopyOp {
            track_id: 2,
            path: "/music/two.mp3".into(),
            size: 1002,
        },
        CopyOp {
            track_id: 99,
            path: "/music/gone.mp3".into(),
            size: 5,
        },
    ];
    plan.replace = vec![ReplaceOp {
        track_id: 3,
        old_dbid: 0x33,
        path: "/music/three.mp3".into(),
        size: 1003,
        old_size: 500,
        file_missing: false,
    }];
    plan.update_tags = vec![
        TagOp {
            track_id: 4,
            dbid: 0x44,
            changes: vec![
                TagChange {
                    field: Field::Title,
                    ipod: Some("Old".into()),
                    library: "New".into(),
                },
                TagChange {
                    field: Field::Year,
                    ipod: None,
                    library: "2001".into(),
                },
                TagChange {
                    field: Field::Compilation,
                    ipod: Some("false".into()),
                    library: "true".into(),
                },
            ],
        },
        // The iPod track is not on the iPod any more.
        TagOp {
            track_id: 5,
            dbid: 0x55,
            changes: vec![TagChange {
                field: Field::Genre,
                ipod: None,
                library: "Jazz".into(),
            }],
        },
    ];
    plan.remove = vec![
        RemoveOp {
            dbid: 0x66,
            track_id: Some(6),
            size: 66,
            reason: RemoveReason::MissingFromLibrary,
        },
        RemoveOp {
            dbid: 0x77,
            track_id: None,
            size: 77,
            reason: RemoveReason::Unmanaged,
        },
    ];
    plan
}

fn lib() -> HashMap<i64, TrackRow> {
    library(
        (1..=6)
            .map(|i| row(i, &format!("t{i}.mp3"), tags()))
            .collect(),
    )
}

fn ipod() -> Contents {
    Contents {
        tracks: [0x33, 0x44, 0x66, 0x77]
            .into_iter()
            .map(ipod_track)
            .collect(),
        ..Contents::default()
    }
}

#[test]
fn track_work_turns_a_plan_into_copies_replaces_retags_and_removes() {
    let lib = lib();
    let w = transfer::track_work(&plan(), &lib, &ipod(), GUID);

    // Copies: the two whose library track exists, new dbids; then the replace.
    assert_eq!(
        w.copies.len(),
        3,
        "the copy of a track not in the library is dropped"
    );
    let (c1, c2, r3) = (&w.copies[0], &w.copies[1], &w.copies[2]);
    assert_eq!((c1.track_id, c2.track_id, r3.track_id), (1, 2, 3));
    assert_eq!((c1.replace, c2.replace, r3.replace), (false, false, true));
    assert_eq!(c1.path, lib[&1].path);
    assert_eq!(c1.dbid, transfer::new_dbid(GUID, &lib[&1], &ipod_dbids()));
    assert_ne!(c1.dbid, c2.dbid);
    for c in [c1, c2] {
        assert!(
            !ipod_dbids().contains(&c.dbid),
            "{:x} already on the iPod",
            c.dbid
        );
    }
    assert_eq!(r3.dbid, 0x33, "a replace keeps the iPod track");
    // Everything a copy needs: all the tags, the audio facts, what to record.
    assert_eq!(c1.fields, transfer::all_fields(&lib[&1]));
    assert_eq!(c1.fields.title.as_deref(), Some("Title"));
    assert_eq!(c1.audio.size, 1001);
    assert_eq!(c1.audio.tracklen_ms, 200_001);
    assert_eq!((c1.audio.bitrate, c1.audio.samplerate), (192, 44_100));
    assert_eq!(c1.audio.filetype, "MPEG audio file");
    assert_eq!(c1.audio.modified, 1_500_000_000, "seconds, rounded down");
    assert_eq!(c1.tags_hash, lib[&1].tags_hash);
    assert_eq!(c1.duration_ms, Some(200_001));

    // Retags: only the fields the plan lists; the one off the iPod is dropped.
    assert_eq!(w.retags.len(), 1);
    let r = &w.retags[0];
    assert_eq!((r.track_id, r.dbid), (4, 0x44));
    assert_eq!(
        r.fields,
        Fields {
            title: Some("New".into()),
            year: Some(2001),
            compilation: Some(true),
            ..Fields::default()
        }
    );
    assert_eq!(r.tags_hash, lib[&4].tags_hash);

    // Removals carry over as they are, unmanaged ones included.
    let removes: Vec<(u64, Option<i64>, u64)> = w
        .removes
        .iter()
        .map(|r| (r.dbid, r.track_id, r.size))
        .collect();
    assert_eq!(removes, [(0x66, Some(6), 66), (0x77, None, 77)]);
}

fn ipod_dbids() -> HashSet<u64> {
    ipod().tracks.iter().map(|t| t.adopt.dbid).collect()
}

#[test]
fn track_work_is_the_same_every_time() {
    let lib = lib();
    let a = transfer::track_work(&plan(), &lib, &ipod(), GUID);
    let b = transfer::track_work(&plan(), &lib, &ipod(), GUID);
    assert_eq!(a, b);
}

#[test]
fn a_plan_listing_a_track_twice_still_gives_each_copy_its_own_dbid() {
    // Same id, same content, same iPod: the same first choice, so only the
    // dbids already handed out keep the two apart.
    let lib = lib();
    let mut p = Plan::default();
    for _ in 0..3 {
        p.copy.push(CopyOp {
            track_id: 1,
            path: "/music/t1.mp3".into(),
            size: 1001,
        });
    }
    let w = transfer::track_work(&p, &lib, &ipod(), GUID);
    let dbids: Vec<u64> = w.copies.iter().map(|c| c.dbid).collect();
    assert_eq!(dbids.len(), 3);
    assert_eq!(dbids.iter().collect::<HashSet<_>>().len(), 3, "{dbids:x?}");
    assert_eq!(dbids[0], transfer::new_dbid(GUID, &lib[&1], &ipod_dbids()));
}

#[test]
fn a_dbid_already_on_the_ipod_is_not_reused_for_a_copy() {
    let lib = lib();
    let wanted = transfer::new_dbid(GUID, &lib[&1], &HashSet::new());
    let mut ipod = ipod();
    ipod.tracks.push(ipod_track(wanted));
    let w = transfer::track_work(&plan(), &lib, &ipod, GUID);
    assert_ne!(w.copies[0].dbid, wanted);
    assert_ne!(w.copies[0].dbid, 0);
}

#[test]
fn new_dbids_maps_every_copied_library_track_replaces_included() {
    let w = transfer::track_work(&plan(), &lib(), &ipod(), GUID);
    let m = w.new_dbids();
    assert_eq!(m.len(), 3);
    assert_eq!(m[&3], 0x33, "a replaced track is found at its own dbid");
    assert_eq!(m[&1], w.copies[0].dbid);
    assert!(!m.contains_key(&4), "a retag copies nothing");
}

#[test]
fn work_totals_and_emptiness() {
    let w = transfer::track_work(&plan(), &lib(), &ipod(), GUID);
    assert_eq!(w.copy_bytes(), 1001 + 1002 + 1003);
    assert!(!w.is_empty());
    assert!(TrackWork::default().is_empty());
    for part in [
        TrackWork {
            removes: w.removes.clone(),
            ..TrackWork::default()
        },
        TrackWork {
            copies: w.copies.clone(),
            ..TrackWork::default()
        },
        TrackWork {
            retags: w.retags.clone(),
            ..TrackWork::default()
        },
    ] {
        assert!(!part.is_empty());
    }
    let none = transfer::track_work(&Plan::default(), &lib(), &ipod(), GUID);
    assert!(none.is_empty());
}

#[test]
fn a_retag_with_every_field_maps_each_field_to_its_own_slot() {
    let all = [
        (Field::Title, "T"),
        (Field::Artist, "A"),
        (Field::Album, "Al"),
        (Field::AlbumArtist, "AA"),
        (Field::Composer, "C"),
        (Field::Genre, "G"),
        (Field::Year, "1990"),
        (Field::TrackNumber, "2"),
        (Field::TrackCount, "12"),
        (Field::DiscNumber, "1"),
        (Field::DiscCount, "3"),
        (Field::Compilation, "false"),
    ];
    let mut p = Plan::default();
    p.update_tags = vec![TagOp {
        track_id: 4,
        dbid: 0x44,
        changes: all
            .iter()
            .map(|(f, v)| TagChange {
                field: *f,
                ipod: None,
                library: (*v).into(),
            })
            .collect(),
    }];
    let w = transfer::track_work(&p, &lib(), &ipod(), GUID);
    assert_eq!(
        w.retags[0].fields,
        Fields {
            title: Some("T".into()),
            artist: Some("A".into()),
            album: Some("Al".into()),
            album_artist: Some("AA".into()),
            composer: Some("C".into()),
            genre: Some("G".into()),
            year: Some(1990),
            track_no: Some(2),
            track_total: Some(12),
            disc_no: Some(1),
            disc_total: Some(3),
            compilation: Some(false),
        }
    );
}

// ---- dbid_candidate, lost_mappings

fn mapping(t: &TrackRow, dbid: u64) -> iprune_core::store::devices::TrackMapping {
    iprune_core::store::devices::TrackMapping {
        track_id: t.id,
        dbid,
        synced_tags_hash: [9; 32],
        synced_size: t.size,
        synced_duration_ms: t.info.duration_ms,
    }
}

fn ipod_with(tracks: &[(u64, u64)]) -> Contents {
    Contents {
        tracks: tracks
            .iter()
            .map(|(dbid, size)| {
                let mut t = ipod_track(*dbid);
                t.adopt.size = *size;
                t
            })
            .collect(),
        ..Contents::default()
    }
}

#[test]
fn dbid_candidate_is_the_sequence_new_dbid_walks() {
    let t = row(1, "a.mp3", tags());
    let c: Vec<u64> = (0..3)
        .map(|n| transfer::dbid_candidate(GUID, &t, n))
        .collect();
    assert_eq!(c.iter().collect::<HashSet<_>>().len(), 3);
    assert_eq!(transfer::new_dbid(GUID, &t, &HashSet::new()), c[0]);
    assert_eq!(transfer::new_dbid(GUID, &t, &HashSet::from([c[0]])), c[1]);
    assert_eq!(
        transfer::new_dbid(GUID, &t, &HashSet::from([c[0], c[1]])),
        c[2]
    );
}

#[test]
fn a_track_a_crash_left_on_the_ipod_is_taken_back_by_its_own_dbid_and_size() {
    let (a, b) = (row(1, "a.mp3", tags()), row(2, "b.mp3", tags()));
    // a was copied under its second candidate (the first was taken then);
    // b under its first.
    let da = transfer::dbid_candidate(GUID, &a, 1);
    let db = transfer::dbid_candidate(GUID, &b, 0);
    let ipod = ipod_with(&[(da, a.size), (db, b.size), (0x44, 5)]);
    let got = transfer::lost_mappings(GUID, &[a.clone(), b.clone()], &ipod, &[]);
    assert_eq!(got.len(), 2);
    let ga = got.iter().find(|m| m.track_id == 1).unwrap();
    assert_eq!(ga.dbid, da);
    assert_eq!(ga.synced_size, a.size);
    assert_eq!(ga.synced_tags_hash, [0; 32]);
    assert_eq!(ga.synced_duration_ms, a.info.duration_ms);
    assert_eq!(got.iter().find(|m| m.track_id == 2).unwrap().dbid, db);
    // Only the first four candidates are looked at.
    let far = transfer::dbid_candidate(GUID, &a, 4);
    let ipod = ipod_with(&[(far, a.size)]);
    assert!(transfer::lost_mappings(GUID, std::slice::from_ref(&a), &ipod, &[]).is_empty());
    let last = transfer::dbid_candidate(GUID, &a, 3);
    let ipod = ipod_with(&[(last, a.size)]);
    assert_eq!(transfer::lost_mappings(GUID, &[a], &ipod, &[]).len(), 1);
}

#[test]
fn a_track_on_the_ipod_with_another_size_is_not_taken_back() {
    let a = row(1, "a.mp3", tags());
    let d = transfer::dbid_candidate(GUID, &a, 0);
    let ipod = ipod_with(&[(d, a.size + 1)]);
    assert!(transfer::lost_mappings(GUID, &[a], &ipod, &[]).is_empty());
}

#[test]
fn only_unmapped_present_tracks_on_unmapped_dbids_are_taken_back() {
    let (a, b, c) = (
        row(1, "a.mp3", tags()),
        row(2, "b.mp3", tags()),
        row(3, "c.mp3", tags()),
    );
    let (da, db, dc) = (
        transfer::dbid_candidate(GUID, &a, 0),
        transfer::dbid_candidate(GUID, &b, 0),
        transfer::dbid_candidate(GUID, &c, 0),
    );
    let ipod = ipod_with(&[(da, a.size), (db, b.size), (dc, c.size)]);
    // a is mapped already (to something else); b is missing from the
    // library; c's dbid is mapped to another track.
    let mut missing = b.clone();
    missing.missing_since = Some(5);
    let mapped = [mapping(&a, 0x77), mapping(&row(9, "z.mp3", tags()), dc)];
    let got = transfer::lost_mappings(GUID, &[a, missing, c], &ipod, &mapped);
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn two_tracks_never_take_back_the_same_dbid() {
    // The same content and size under two library ids cannot share a dbid,
    // since the id is part of it; force it with equal candidates anyway by
    // listing one track twice.
    let a = row(1, "a.mp3", tags());
    let d = transfer::dbid_candidate(GUID, &a, 0);
    let ipod = ipod_with(&[(d, a.size)]);
    let got = transfer::lost_mappings(GUID, &[a.clone(), a], &ipod, &[]);
    assert_eq!(got.len(), 1, "{got:?}");
}
