//! Device persistence (`store::devices`), `adoption::save` on hand-built runs,
//! and the mountinfo parser.

mod common;

use std::collections::HashMap;
use std::path::Path;

use common::{Env, Kind};
use iprune_core::Error;
use iprune_core::adopt::{Adoption, IpodTrack, PlaylistAdoption, PlaylistMatch};
use iprune_core::adoption::{self, Run};
use iprune_core::device::{Mount, parse_mountinfo};
use iprune_core::ipod::{self, Contents};
use iprune_core::store::devices::{self, TrackMapping};
use iprune_core::store::{self, TrackRow};
use iprune_core::tags::Tags;

fn env_with_tracks(n: u32) -> (Env, Vec<TrackRow>) {
    let mut env = Env::new();
    for i in 1..=n {
        env.add(&format!("t{i}.mp3"), Kind::Mp3, i);
    }
    env.scan();
    let tracks = env.tracks();
    assert_eq!(tracks.len(), n as usize);
    (env, tracks)
}

fn mapping(track_id: i64, dbid: u64, tag: u8, size: u64) -> TrackMapping {
    TrackMapping {
        track_id,
        dbid,
        synced_tags_hash: [tag; 32],
        synced_size: size,
    }
}

fn playlist_rows(conn: &rusqlite::Connection, dev: i64) -> Vec<(i64, String)> {
    let mut stmt = conn
        .prepare("SELECT playlist_id, ipod_name FROM device_playlists WHERE device_id = ?1 ORDER BY playlist_id")
        .unwrap();
    stmt.query_map([dev], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

// ---- store::devices

#[test]
fn device_ids_are_stable_and_upsert_updates_name_and_model() {
    let env = Env::new();
    let conn = env.store.conn();
    assert_eq!(devices::device_id(conn, "G1").unwrap(), None);
    let a = devices::upsert_device(conn, "G1", Some("Old"), None).unwrap();
    let b = devices::upsert_device(conn, "G1", Some("New"), Some("Classic")).unwrap();
    assert_eq!(a, b);
    assert_eq!(devices::device_id(conn, "G1").unwrap(), Some(a));
    let c = devices::upsert_device(conn, "G2", None, None).unwrap();
    assert_ne!(a, c);
    let (name, model): (Option<String>, Option<String>) = conn
        .query_row("SELECT name, model FROM devices WHERE id = ?1", [a], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(
        (name.as_deref(), model.as_deref()),
        (Some("New"), Some("Classic"))
    );
}

#[test]
fn mappings_round_trip_including_dbids_with_the_high_bit() {
    let (env, t) = env_with_tracks(4);
    let conn = env.store.conn();
    let dev = devices::upsert_device(conn, "G1", None, None).unwrap();
    let want = vec![
        mapping(t[0].id, u64::MAX, 1, 3_000_000_000),
        mapping(t[1].id, 1 << 63, 2, 0),
        mapping(t[2].id, 0x8653_6232_fbbc_81cf, 3, u64::from(u32::MAX)),
        mapping(t[3].id, 1, 4, 5),
    ];
    devices::replace_mappings(conn, dev, &want, &[]).unwrap();
    assert_eq!(devices::track_mappings(conn, dev).unwrap(), want);
    assert_eq!(devices::mapped_track_count(conn, dev).unwrap(), 4);
}

#[test]
fn replace_mappings_replaces_and_leaves_other_devices_alone() {
    let (env, t) = env_with_tracks(3);
    let conn = env.store.conn();
    let p1 = store::put_playlist(conn, "One", Path::new("/p/One.m3u8"), 1, &[]).unwrap();
    let p2 = store::put_playlist(conn, "Two", Path::new("/p/Two.m3u8"), 1, &[]).unwrap();
    let a = devices::upsert_device(conn, "A", None, None).unwrap();
    let b = devices::upsert_device(conn, "B", None, None).unwrap();

    devices::replace_mappings(
        conn,
        a,
        &[mapping(t[0].id, 10, 1, 1), mapping(t[1].id, 11, 1, 1)],
        &[(p1, "One".into()), (p2, "Two".into())],
    )
    .unwrap();
    let b_rows = vec![mapping(t[0].id, 10, 9, 9)];
    devices::replace_mappings(conn, b, &b_rows, &[(p1, "uno".into())]).unwrap();

    devices::replace_mappings(
        conn,
        a,
        &[mapping(t[2].id, 12, 2, 2)],
        &[(p2, "TWO".into())],
    )
    .unwrap();

    assert_eq!(
        devices::track_mappings(conn, a).unwrap(),
        vec![mapping(t[2].id, 12, 2, 2)]
    );
    assert_eq!(playlist_rows(conn, a), vec![(p2, "TWO".into())]);
    // The same dbid and library track on another device are untouched.
    assert_eq!(devices::track_mappings(conn, b).unwrap(), b_rows);
    assert_eq!(playlist_rows(conn, b), vec![(p1, "uno".into())]);

    // Replacing with nothing clears the device.
    devices::replace_mappings(conn, a, &[], &[]).unwrap();
    assert_eq!(devices::mapped_track_count(conn, a).unwrap(), 0);
    assert!(playlist_rows(conn, a).is_empty());
}

#[test]
fn library_playlists_tell_empty_from_unresolved_and_keep_order() {
    let (env, t) = env_with_tracks(2);
    let conn = env.store.conn();
    let (x, y) = (t[0].id, t[1].id);
    let raw = |s: &str| s.to_owned();
    let put = |name: &str, items: &[(Option<i64>, String)]| {
        store::put_playlist(conn, name, Path::new(&format!("/p/{name}.m3u8")), 1, items).unwrap()
    };
    let empty = put("Empty", &[]);
    let only_unresolved = put("Lost", &[(None, raw("gone.mp3")), (None, raw("gone2.mp3"))]);
    let mixed = put(
        "Mixed",
        &[
            (Some(y), raw("y")),
            (None, raw("?")),
            (Some(x), raw("x")),
            (Some(y), raw("y")),
        ],
    );
    let got = devices::library_playlists(conn).unwrap();
    let by_name: HashMap<&str, _> = got.iter().map(|p| (p.name.as_str(), p)).collect();
    assert_eq!(got.len(), 3);
    assert_eq!(by_name["Empty"].id, empty);
    assert!(by_name["Empty"].members.is_empty());
    assert_eq!(by_name["Lost"].id, only_unresolved);
    assert_eq!(by_name["Lost"].members, vec![None, None]);
    assert_eq!(by_name["Mixed"].id, mixed);
    assert_eq!(
        by_name["Mixed"].members,
        vec![Some(y), None, Some(x), Some(y)]
    );
    // Sorted by name.
    let names: Vec<&str> = got.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["Empty", "Lost", "Mixed"]);
}

#[test]
fn library_playlists_of_an_empty_index_is_empty() {
    let env = Env::new();
    assert!(
        devices::library_playlists(env.store.conn())
            .unwrap()
            .is_empty()
    );
}

// ---- adoption::save on hand-built runs

fn ipod_track(dbid: u64, size: u64, tags: Tags) -> ipod::Track {
    ipod::Track {
        adopt: IpodTrack {
            dbid,
            size,
            duration_ms: 1000,
            title: tags.title.clone(),
            artist: tags.artist.clone(),
            album: tags.album.clone(),
            playlists: 0,
            playcount: 0,
            hash: None,
        },
        tags,
        ipod_path: None,
    }
}

fn tags(title: &str, artist: &str) -> Tags {
    Tags {
        title: Some(title.into()),
        artist: Some(artist.into()),
        ..Tags::default()
    }
}

/// A run whose mapping is exactly `pairs` (`dbid`, track).
fn run_of(tracks: &[TrackRow], ipod_tracks: Vec<ipod::Track>, pairs: &[(u64, usize)]) -> Run {
    Run {
        ipod: Contents {
            name: Some("My iPod".into()),
            model: Some("Classic".into()),
            tracks: ipod_tracks,
            ..Contents::default()
        },
        adoption: Adoption::default(),
        mapping: pairs.iter().map(|&(d, i)| (d, tracks[i].id)).collect(),
        left: vec![],
        playlists: PlaylistAdoption::default(),
        library: tracks.iter().map(|t| (t.id, t.clone())).collect(),
        compared: 0,
        unreadable: vec![],
    }
}

#[test]
fn save_stores_the_ipods_tag_hash_and_size_not_the_files() {
    let (env, t) = env_with_tracks(2);
    let conn = env.store.conn();
    // Track 0: the iPod has exactly the file's tags. Track 1: it differs.
    let same = t[0].tags.clone();
    let other = tags("Edited on the iPod", "Artist2");
    let run = run_of(
        &t,
        vec![
            ipod_track(7, 4000, same),
            ipod_track(u64::MAX, 4001, other.clone()),
        ],
        &[(7, 0), (u64::MAX, 1)],
    );
    adoption::save(conn, &run, "G1", false).unwrap();
    let dev = devices::device_id(conn, "G1").unwrap().unwrap();
    let got = devices::track_mappings(conn, dev).unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].track_id, t[0].id);
    assert_eq!(got[0].dbid, 7);
    assert_eq!(got[0].synced_size, 4000);
    assert_eq!(
        got[0].synced_tags_hash, t[0].tags_hash,
        "identical tags hash alike"
    );
    assert_eq!(got[1].dbid, u64::MAX);
    assert_eq!(got[1].synced_size, 4001);
    assert_eq!(got[1].synced_tags_hash, other.hash());
    assert_ne!(got[1].synced_tags_hash, t[1].tags_hash);
}

#[test]
fn save_refuses_a_second_adoption_without_force_and_changes_nothing() {
    let (env, t) = env_with_tracks(2);
    let conn = env.store.conn();
    let first = run_of(&t, vec![ipod_track(1, 10, tags("a", "b"))], &[(1, 0)]);
    adoption::save(conn, &first, "G1", false).unwrap();
    let dev = devices::device_id(conn, "G1").unwrap().unwrap();
    let before = devices::track_mappings(conn, dev).unwrap();

    let second = run_of(&t, vec![ipod_track(2, 20, tags("c", "d"))], &[(2, 1)]);
    let err = adoption::save(conn, &second, "G1", false).unwrap_err();
    assert!(
        matches!(&err, Error::AlreadyAdopted(g) if g == "G1"),
        "{err}"
    );
    assert_eq!(devices::track_mappings(conn, dev).unwrap(), before);

    adoption::save(conn, &second, "G1", true).unwrap();
    let after = devices::track_mappings(conn, dev).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!((after[0].dbid, after[0].track_id), (2, t[1].id));
    assert_eq!(
        devices::device_id(conn, "G1").unwrap(),
        Some(dev),
        "same device row"
    );
}

#[test]
fn refused_save_does_not_touch_the_device_name() {
    let (env, t) = env_with_tracks(1);
    let conn = env.store.conn();
    let run = run_of(&t, vec![ipod_track(1, 10, tags("a", "b"))], &[(1, 0)]);
    adoption::save(conn, &run, "G1", false).unwrap();
    let mut renamed = run_of(&t, vec![ipod_track(1, 10, tags("a", "b"))], &[(1, 0)]);
    renamed.ipod.name = Some("Renamed".into());
    assert!(adoption::save(conn, &renamed, "G1", false).is_err());
    let name: Option<String> = conn
        .query_row(
            "SELECT name FROM devices WHERE firewire_guid = 'G1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(name.as_deref(), Some("My iPod"));
}

#[test]
fn a_failed_forced_save_keeps_the_earlier_adoption() {
    let (env, t) = env_with_tracks(2);
    let conn = env.store.conn();
    let first = run_of(&t, vec![ipod_track(1, 10, tags("a", "b"))], &[(1, 0)]);
    adoption::save(conn, &first, "G1", false).unwrap();
    let dev = devices::device_id(conn, "G1").unwrap().unwrap();
    let before = devices::track_mappings(conn, dev).unwrap();

    // Two library tracks claiming one dbid violates UNIQUE(device_id, dbid)
    // halfway through the insert, after the old rows were deleted.
    let bad = run_of(
        &t,
        vec![ipod_track(5, 10, tags("a", "b"))],
        &[(5, 0), (5, 1)],
    );
    let err = adoption::save(conn, &bad, "G1", true).unwrap_err();
    assert!(matches!(err, Error::Db(_)), "{err}");
    assert_eq!(devices::track_mappings(conn, dev).unwrap(), before);
}

#[test]
fn two_devices_are_independent() {
    let (env, t) = env_with_tracks(1);
    let conn = env.store.conn();
    let run = run_of(&t, vec![ipod_track(1, 10, tags("a", "b"))], &[(1, 0)]);
    adoption::save(conn, &run, "G1", false).unwrap();
    // Another iPod needs no --force, and may map the same file and dbid.
    adoption::save(conn, &run, "G2", false).unwrap();
    for g in ["G1", "G2"] {
        let dev = devices::device_id(conn, g).unwrap().unwrap();
        assert_eq!(devices::mapped_track_count(conn, dev).unwrap(), 1);
    }
}

#[test]
fn save_stores_matched_playlists_with_their_ipod_names() {
    let (env, t) = env_with_tracks(1);
    let conn = env.store.conn();
    let p = store::put_playlist(conn, "Road", Path::new("/p/Road.m3u8"), 1, &[]).unwrap();
    let mut run = run_of(&t, vec![ipod_track(1, 10, tags("a", "b"))], &[(1, 0)]);
    run.playlists.matched = vec![PlaylistMatch {
        ipod_name: "ROAD".into(),
        playlist_id: p,
        library_name: "Road".into(),
        same_contents: false,
    }];
    run.playlists.unmatched_ipod = vec!["Not saved".into()];
    adoption::save(conn, &run, "G1", false).unwrap();
    let dev = devices::device_id(conn, "G1").unwrap().unwrap();
    assert_eq!(playlist_rows(conn, dev), vec![(p, "ROAD".into())]);
}

// ---- parse_mountinfo

fn mount(mp: &str, fstype: &str, ro: bool) -> Mount {
    Mount {
        mountpoint: mp.into(),
        fstype: fstype.into(),
        read_only: ro,
    }
}

#[test]
fn mountinfo_decodes_octal_escapes_in_the_mountpoint() {
    let text = "95 22 8:33 / /run/media/j/a\\040b\\011c\\134d\\012e rw,relatime shared:50 - vfat /dev/sdc1 rw\n";
    assert_eq!(
        parse_mountinfo(text),
        vec![mount("/run/media/j/a b\tc\\d\ne", "vfat", false)]
    );
}

#[test]
fn mountinfo_with_and_without_optional_fields() {
    let text = "\
1 0 8:1 / /a rw - ext4 /dev/a rw
2 1 8:2 / /b rw,nosuid shared:1 master:2 - vfat /dev/b rw
3 1 8:3 / /c ro,noatime - hfsplus /dev/c ro
";
    assert_eq!(
        parse_mountinfo(text),
        vec![
            mount("/a", "ext4", false),
            mount("/b", "vfat", false),
            mount("/c", "hfsplus", true),
        ]
    );
}

#[test]
fn mountinfo_read_only_means_the_ro_option_not_a_substring() {
    let text = "1 0 8:1 / /a rw,errors=remount-ro,uro - ext4 /dev/a rw\n";
    assert!(!parse_mountinfo(text)[0].read_only);
}

#[test]
fn mountinfo_skips_malformed_lines_and_empty_input() {
    assert!(parse_mountinfo("").is_empty());
    let text = "\
not a mount line
1 0 8:1 / /a
1 0 8:1 / /ok rw - ext4 /dev/a rw
 - 
";
    assert_eq!(parse_mountinfo(text), vec![mount("/ok", "ext4", false)]);
}

#[test]
fn mountinfo_keeps_a_mountpoint_that_contains_a_dash_separator_lookalike() {
    // The mountpoint is escaped by the kernel, so a space-dash-space inside it
    // arrives as \040-\040 and must not split the line early.
    let text = "1 0 8:1 / /mnt/my\\040-\\040ipod rw - vfat /dev/a rw\n";
    assert_eq!(
        parse_mountinfo(text),
        vec![mount("/mnt/my - ipod", "vfat", false)]
    );
}

#[test]
fn mountinfo_leaves_escapes_above_255_as_text_and_does_not_panic() {
    for (raw, want) in [
        ("/m/a\\777b", "/m/a\\777b"),
        ("/m/a\\400b", "/m/a\\400b"),
        ("/m/a\\377", "/m/a\u{fffd}"),
        ("/m/a\\101b", "/m/aAb"),
        ("/m/a\\777", "/m/a\\777"),
    ] {
        let text = format!("1 0 8:1 / {raw} rw - ext4 /dev/a rw\n");
        assert_eq!(
            parse_mountinfo(&text),
            vec![mount(want, "ext4", false)],
            "{raw}"
        );
    }
}
