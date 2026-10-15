//! Audit step 6: what a track is when its file moves and is retagged (H6),
//! links and iPods and empty folders under a library root (M10, M18, L7), and
//! a playlists folder that cannot be fully read (L8). Everything happens in
//! temp dirs; the iPods are fake folders made with libgpod.

mod common;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use common::{Env, IpodSpec, Kind, fake_ipod, fixture, fixture_secs, id3v24_text, mp3_with_id3v2};
use iprune_core::config::Config;
use iprune_core::store::{self, devices};
use iprune_core::{Error, playlists};

/// Mp3 fixture `variant`'s audio under hand-written tags.
fn tagged(variant: u32, title: &str, artist: &str, album: Option<&str>) -> Vec<u8> {
    let mut frames = vec![
        id3v24_text("TIT2", 3, title.as_bytes()),
        id3v24_text("TPE1", 3, artist.as_bytes()),
    ];
    if let Some(a) = album {
        frames.push(id3v24_text("TALB", 3, a.as_bytes()));
    }
    mp3_with_id3v2(variant, &frames)
}

fn has_reason(r: &[(PathBuf, String)], path: &Path, fragment: &str) -> bool {
    r.iter().any(|(p, why)| p == path && why.contains(fragment))
}

// ---- H6: a moved and retagged file

/// The old file (a/old.mp3) is replaced by `new` at b/new.mp3. Returns
/// (old id, scan report, new track).
fn replace_file(
    old: Vec<u8>,
    new: Vec<u8>,
) -> (
    Env,
    i64,
    iprune_core::scan::ScanReport,
    iprune_core::store::TrackRow,
) {
    let mut env = Env::new();
    env.add("keep.mp3", Kind::Mp3, 9);
    let old_path = env.add_bytes("a/old.mp3", &old);
    env.scan();
    let id = env.track(&old_path).id;
    fs::remove_file(&old_path).unwrap();
    let new_path = env.add_bytes("b/new.mp3", &new);
    let r = env.scan();
    let t = env.track(&new_path);
    (env, id, r, t)
}

#[test]
fn pairing_by_audio_needs_the_same_title_or_the_same_artist_and_album() {
    let old = || tagged(1, "Song", "Band", Some("LP"));
    // (new title, artist, album, paired)
    let cases: [(&str, &str, Option<&str>, bool); 8] = [
        ("Renamed", "Band", Some("LP"), true),
        ("Song", "Other", Some("Other LP"), true),
        ("  SONG ", "x", None, true),
        ("Renamed", "BAND", Some(" lp "), true),
        ("Renamed", "Band", None, false),
        ("Renamed", "Band", Some("Other LP"), false),
        ("Renamed", "Other", Some("LP"), false),
        ("Another", "Other", Some("Other LP"), false),
    ];
    for (title, artist, album, paired) in cases {
        let (env, id, r, new) = replace_file(old(), tagged(1, title, artist, album));
        let what = format!("{title:?} {artist:?} {album:?}: {r:?}");
        let old_row = env.tracks().into_iter().find(|t| t.id == id).unwrap();
        if paired {
            assert_eq!(r.moved.len(), 1, "{what}");
            assert_eq!((r.added, r.missing.len()), (0, 0), "{what}");
            assert_eq!(new.id, id, "{what}");
            assert!(old_row.missing_since.is_none(), "{what}");
            assert_eq!(old_row.path, new.path, "{what}");
            assert_eq!(old_row.tags.title.as_deref(), Some(title), "{what}");
        } else {
            assert!(r.moved.is_empty(), "{what}");
            assert_eq!((r.added, r.missing.len()), (1, 1), "{what}");
            assert_ne!(new.id, id, "{what}");
            assert!(old_row.missing_since.is_some(), "{what}");
        }
    }
}

// Prevents pairing a deleted track with an unrelated file that happens to
// have the same length and bitrate.
#[test]
fn a_delete_and_an_unrelated_add_with_the_same_audio_key_are_not_paired() {
    let (env, id, r, new) = replace_file(
        tagged(1, "Song", "Band", Some("LP")),
        tagged(1, "Different", "Someone Else", Some("Another LP")),
    );
    assert!(r.moved.is_empty(), "{r:?}");
    assert_eq!((r.added, r.missing.len()), (1, 1));
    assert_ne!(new.id, id);
    let old = env.tracks().into_iter().find(|t| t.id == id).unwrap();
    assert!(old.missing_since.is_some());
    assert_eq!(old.tags.title.as_deref(), Some("Song"));
}

#[test]
fn audio_of_another_length_is_not_paired_even_with_the_same_tags() {
    let mut env = Env::new();
    env.add("keep.mp3", Kind::Mp3, 9);
    let old = env.add("old.mp3", Kind::Mp3, 1);
    env.scan();
    fs::remove_file(&old).unwrap();
    // Same title and artist ("Title1", "Artist1"), twice as long.
    fs::copy(fixture_secs(Kind::Mp3, 1, 2), env.music.join("new.mp3")).unwrap();
    let r = env.scan();
    assert!(r.moved.is_empty(), "{r:?}");
    assert_eq!((r.added, r.missing.len()), (1, 1));
}

#[test]
fn two_vanished_candidates_for_one_new_file_are_not_paired() {
    let mut env = Env::new();
    env.add("keep.mp3", Kind::Mp3, 9);
    // Two files with the same audio, told apart by their album.
    let a = env.add_bytes("a.mp3", &tagged(1, "Song", "Band", Some("LP")));
    let b = env.add_bytes("b.mp3", &tagged(1, "Song", "Band", Some("LP2")));
    env.scan();
    let (ida, idb) = (env.track(&a).id, env.track(&b).id);
    fs::remove_file(&a).unwrap();
    fs::remove_file(&b).unwrap();
    let new = env.add_bytes("c.mp3", &tagged(1, "Song", "Band", Some("Zed")));
    let r = env.scan();
    assert!(r.moved.is_empty(), "{r:?}");
    assert_eq!((r.added, r.missing.len()), (1, 2));
    let t = env.track(&new);
    assert!(t.id != ida && t.id != idb);
    for id in [ida, idb] {
        let old = env.tracks().into_iter().find(|t| t.id == id).unwrap();
        assert!(old.missing_since.is_some());
    }
}

#[test]
fn two_new_candidates_for_one_vanished_file_are_not_paired() {
    let mut env = Env::new();
    env.add("keep.mp3", Kind::Mp3, 9);
    let a = env.add_bytes("a.mp3", &tagged(1, "Song", "Band", Some("LP")));
    env.scan();
    let ida = env.track(&a).id;
    fs::remove_file(&a).unwrap();
    env.add_bytes("c.mp3", &tagged(1, "Song", "Band", Some("X")));
    env.add_bytes("d.mp3", &tagged(1, "Song", "Band", Some("Y")));
    let r = env.scan();
    assert!(r.moved.is_empty(), "{r:?}");
    assert_eq!((r.added, r.missing.len()), (2, 1));
    let old = env.tracks().into_iter().find(|t| t.id == ida).unwrap();
    assert!(old.missing_since.is_some());
}

// A track missing since an earlier scan is a candidate too: the file comes
// back retagged under another name.
#[test]
fn a_track_already_missing_is_paired_with_the_retagged_file_that_returns() {
    let mut env = Env::new();
    env.add("keep.mp3", Kind::Mp3, 9);
    let a = env.add_bytes("a.mp3", &tagged(1, "Song", "Band", Some("LP")));
    env.scan();
    let id = env.track(&a).id;
    fs::remove_file(&a).unwrap();
    assert_eq!(env.scan().missing.len(), 1);
    let new = env.add_bytes("z/b.mp3", &tagged(1, "Song 2", "Band", Some("LP")));
    let r = env.scan();
    assert_eq!(r.moved.len(), 1, "{r:?}");
    assert_eq!(r.added, 0);
    let t = env.track(&new);
    assert_eq!(t.id, id);
    assert!(t.missing_since.is_none());
}

// The pair forgets what the iPod copy was made from, on every device, so the
// next sync replaces the audio; a plain content-hash move does not.
#[test]
fn a_pair_by_audio_resets_the_synced_audio_of_every_device_but_a_plain_move_does_not() {
    let mut env = Env::new();
    env.add("keep.mp3", Kind::Mp3, 9);
    let a = env.add_bytes("a.mp3", &tagged(1, "Song", "Band", Some("LP")));
    let b = env.add("b.mp3", Kind::Mp3, 2);
    env.scan();
    let (ida, idb) = (env.track(&a).id, env.track(&b).id);
    let conn = env.store.conn();
    let devs = [
        devices::upsert_device(conn, "G1", None, None).unwrap(),
        devices::upsert_device(conn, "G2", None, None).unwrap(),
    ];
    for d in devs {
        for (id, dbid) in [(ida, 0x10i64), (idb, 0x11)] {
            conn.execute(
                "INSERT INTO device_tracks (device_id, track_id, dbid, synced_tags_hash, \
                 synced_size, synced_duration_ms) VALUES (?1, ?2, ?3, x'00', 4321, 1000)",
                rusqlite::params![d, id, dbid + d * 0x100],
            )
            .unwrap();
        }
    }
    // a: moved and retagged. b: moved, content unchanged.
    fs::remove_file(&a).unwrap();
    fs::remove_file(&b).unwrap();
    env.add_bytes("a2.mp3", &tagged(1, "Song Two", "Band", Some("LP")));
    env.add("b2.mp3", Kind::Mp3, 2);
    let r = env.scan();
    assert_eq!(r.moved.len(), 2, "{r:?}");
    let conn = env.store.conn();
    let synced = |id: i64, d: i64| -> (i64, Option<i64>) {
        conn.query_row(
            "SELECT synced_size, synced_duration_ms FROM device_tracks \
             WHERE track_id = ?1 AND device_id = ?2",
            rusqlite::params![id, d],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    for d in devs {
        assert_eq!(synced(ida, d), (0, Some(0)), "device {d}, paired by audio");
        assert_eq!(synced(idb, d), (4321, Some(1000)), "device {d}, plain move");
    }
}

// ---- M10: links

fn elsewhere(env: &Env) -> PathBuf {
    let d = env.music.parent().unwrap().join("elsewhere");
    fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn an_indexed_folder_replaced_by_a_link_to_a_copy_marks_nothing_missing_and_is_reported() {
    let mut env = Env::new();
    env.add("keep.mp3", Kind::Mp3, 9);
    env.add("Album/a.mp3", Kind::Mp3, 1);
    env.add("Album/b.mp3", Kind::Mp3, 2);
    env.scan();
    let ids: Vec<i64> = env.tracks().iter().map(|t| t.id).collect();
    let copy = elsewhere(&env).join("Album");
    fs::rename(env.music.join("Album"), &copy).unwrap();
    symlink(&copy, env.music.join("Album")).unwrap();

    let r = env.scan();
    assert!(r.missing.is_empty(), "{:?}", r.missing);
    assert_eq!((r.added, r.moved.len()), (0, 0), "{r:?}");
    assert!(
        has_reason(&r.unreadable, &env.music.join("Album"), "symbolic link"),
        "{:?}",
        r.unreadable
    );
    let tracks = env.tracks();
    assert_eq!(tracks.len(), 3);
    assert!(tracks.iter().all(|t| t.missing_since.is_none()));
    assert_eq!(tracks.iter().map(|t| t.id).collect::<Vec<_>>(), ids);

    // Removing the link really deletes them.
    fs::remove_file(env.music.join("Album")).unwrap();
    let r = env.scan();
    assert_eq!(r.missing.len(), 2, "{r:?}");
}

#[test]
fn an_indexed_file_replaced_by_a_link_is_not_missing_and_is_reported() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.add("keep.mp3", Kind::Mp3, 9);
    env.scan();
    let target = elsewhere(&env).join("a.mp3");
    fs::rename(&a, &target).unwrap();
    symlink(&target, &a).unwrap();

    let r = env.scan();
    assert!(r.missing.is_empty(), "{:?}", r.missing);
    assert!(has_reason(&r.unreadable, &a, "symbolic link"), "{r:?}");
    assert!(env.track(&a).missing_since.is_none());
    assert_eq!(env.tracks().len(), 2);
}

#[test]
fn links_on_a_first_scan_are_reported_and_nothing_under_them_is_indexed() {
    let mut env = Env::new();
    env.add("real.mp3", Kind::Mp3, 1);
    let out = elsewhere(&env);
    env.add_at(&out, "Album/x.mp3", Kind::Mp3, 2);
    env.add_at(&out, "y.mp3", Kind::Mp3, 3);
    symlink(out.join("Album"), env.music.join("linked-dir")).unwrap();
    symlink(out.join("y.mp3"), env.music.join("linked-file.mp3")).unwrap();
    symlink(out.join("nowhere.mp3"), env.music.join("dangling.mp3")).unwrap();

    let r = env.scan();
    assert_eq!(r.added, 1, "{r:?}");
    assert_eq!(env.tracks().len(), 1);
    let links: Vec<_> = r
        .unreadable
        .iter()
        .filter(|(_, why)| why.contains("symbolic link"))
        .map(|(p, _)| p.clone())
        .collect();
    assert_eq!(links.len(), 3, "{:?}", r.unreadable);
    for name in ["linked-dir", "linked-file.mp3", "dangling.mp3"] {
        assert!(links.contains(&env.music.join(name)), "{name}: {links:?}");
    }
}

// The root itself may be a link (a mount shortcut): that is the library.
#[test]
fn a_root_that_is_a_link_is_still_walked() {
    let mut env = Env::new();
    env.add("a.mp3", Kind::Mp3, 1);
    let link = env.music.parent().unwrap().join("music-link");
    symlink(&env.music, &link).unwrap();
    env.cfg.library.roots = vec![link.clone()];
    let r = env.scan();
    assert_eq!(r.added, 1, "{r:?}");
    assert!(r.unreadable.is_empty(), "{:?}", r.unreadable);
    assert_eq!(env.tracks()[0].path, link.join("a.mp3"));
}

// ---- M18: an iPod inside the library

const SYSINFO: &str = "FirewireGuid: 0x000A27001234ABCD\n";

fn ipod_at(dir: &Path) {
    let spec = IpodSpec {
        dbid: 0x1000,
        size: 100,
        ms: 1000,
        title: "T",
        artist: "A",
        album: "",
        plays: 0,
    };
    fake_ipod(dir, Some(SYSINFO), &[spec], &[]);
    // Real audio on the iPod, which the scan would index if it went in.
    fs::copy(
        fixture(Kind::Mp3, 3),
        dir.join("iPod_Control/Music/F00/T0000.mp3"),
    )
    .unwrap();
    fs::copy(
        fixture(Kind::Mp3, 4),
        dir.join("iPod_Control/Music/F01/other.mp3"),
    )
    .unwrap();
}

#[test]
fn an_ipod_inside_a_root_indexes_nothing_under_it_and_is_reported() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    let one = env.music.join("IPOD");
    let two = env.music.join("Devices/second");
    ipod_at(&one);
    ipod_at(&two);
    env.add("Devices/b.mp3", Kind::Mp3, 2);

    for round in 0..2 {
        let r = env.scan();
        let mut ipods = r.ipods.clone();
        ipods.sort();
        assert_eq!(ipods, vec![two.clone(), one.clone()], "round {round}");
        assert!(r.unreadable.is_empty(), "{:?}", r.unreadable);
        assert!(r.unsupported.is_empty(), "{:?}", r.unsupported);
        assert!(r.missing.is_empty(), "round {round}: {:?}", r.missing);
        let mut paths: Vec<PathBuf> = env.tracks().into_iter().map(|t| t.path).collect();
        paths.sort();
        let mut want = vec![a.clone(), env.music.join("Devices/b.mp3")];
        want.sort();
        assert_eq!(paths, want, "round {round}");
    }
}

// Rows an older scan indexed from a folder that is (now known to be) an
// iPod are left alone, not declared missing because the walk skips them.
#[test]
fn rows_indexed_from_an_ipod_folder_by_an_older_scan_are_not_marked_missing() {
    let mut env = Env::new();
    env.add("a.mp3", Kind::Mp3, 1);
    // No iTunesDB yet: the folder is library as far as the scan can tell.
    let ipod = env.music.join("IPOD");
    env.add_at(&ipod, "iPod_Control/Music/F00/T0000.mp3", Kind::Mp3, 3);
    env.add_at(&ipod, "iPod_Control/Music/F01/other.mp3", Kind::Mp3, 4);
    let r = env.scan();
    assert_eq!((r.added, r.ipods.len()), (3, 0));
    let before: Vec<(i64, PathBuf)> = env.tracks().into_iter().map(|t| (t.id, t.path)).collect();

    ipod_at(&ipod);
    let r = env.scan();
    assert_eq!(r.ipods, vec![ipod.clone()]);
    assert!(r.missing.is_empty(), "{:?}", r.missing);
    assert_eq!((r.added, r.moved.len()), (0, 0), "{r:?}");
    let tracks = env.tracks();
    assert!(tracks.iter().all(|t| t.missing_since.is_none()));
    let after: Vec<(i64, PathBuf)> = tracks.into_iter().map(|t| (t.id, t.path)).collect();
    assert_eq!(after, before);
}

#[test]
fn a_folder_called_ipod_control_without_an_itunesdb_is_library() {
    let mut env = Env::new();
    env.add("Backup/iPod_Control/Music/F00/x.mp3", Kind::Mp3, 1);
    env.add("Backup/iPod_Control/iTunes/y.mp3", Kind::Mp3, 2);
    let r = env.scan();
    assert!(r.ipods.is_empty(), "{:?}", r.ipods);
    assert_eq!(r.added, 2);
}

// ---- M18: Config::validate

fn config_text(roots: &[&Path], mountpoint: Option<&Path>) -> String {
    let roots: Vec<String> = roots
        .iter()
        .map(|r| format!("{:?}", r.to_str().unwrap()))
        .collect();
    let mut s = format!(
        "[library]\nroots = [{}]\n[playlists]\ndir = \"/p\"\n",
        roots.join(", ")
    );
    if let Some(m) = mountpoint {
        s += &format!("[device]\nmountpoint = {:?}\n", m.to_str().unwrap());
    }
    s
}

fn load(dir: &Path, roots: &[&Path], mountpoint: Option<&Path>) -> iprune_core::Result<Config> {
    let file = dir.join("config.toml");
    fs::write(&file, config_text(roots, mountpoint)).unwrap();
    Config::load(&file)
}

fn refused(r: iprune_core::Result<Config>, field: &str) {
    match r {
        Err(Error::Config { message, .. }) => {
            assert!(message.contains(field), "{message}");
        }
        other => panic!("expected a config error about {field}, got {other:?}"),
    }
}

#[test]
fn a_root_of_slash_is_refused() {
    let t = tempfile::tempdir().unwrap();
    refused(load(t.path(), &[Path::new("/")], None), "library.roots");
    refused(
        load(t.path(), &[t.path(), Path::new("/")], None),
        "library.roots",
    );
    assert!(load(t.path(), &[t.path()], None).is_ok());
}

#[test]
fn a_mountpoint_inside_a_root_or_holding_one_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let base = t.path().canonicalize().unwrap();
    let music = base.join("music");
    fs::create_dir_all(music.join("ipod")).unwrap();
    fs::create_dir_all(base.join("music2")).unwrap();
    fs::create_dir_all(base.join("other")).unwrap();
    // The iPod inside the library, the library as the iPod, the library
    // inside the iPod.
    refused(
        load(&base, &[&music], Some(&music.join("ipod"))),
        "device.mountpoint",
    );
    refused(load(&base, &[&music], Some(&music)), "device.mountpoint");
    refused(
        load(&base, &[&music.join("ipod")], Some(&music)),
        "device.mountpoint",
    );
    refused(load(&base, &[&music], Some(&base)), "device.mountpoint");
    // Folders not there (an unplugged iPod) count by their name.
    refused(
        load(&base, &[&music], Some(&music.join("not/there/yet"))),
        "device.mountpoint",
    );
    // Through ..
    refused(
        load(&base, &[&music], Some(&base.join("other/../music/ipod"))),
        "device.mountpoint",
    );
    // A sibling whose name starts like the root's is fine, so is no mountpoint.
    assert!(load(&base, &[&music], Some(&base.join("music2"))).is_ok());
    assert!(load(&base, &[&music], Some(&base.join("other"))).is_ok());
    assert!(load(&base, &[&music], None).is_ok());
    assert!(load(&base, &[&music], Some(Path::new(""))).is_ok());
}

#[test]
fn a_mountpoint_that_overlaps_a_root_through_a_link_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let base = t.path().canonicalize().unwrap();
    let real = base.join("real");
    fs::create_dir_all(real.join("ipod")).unwrap();
    let link = base.join("link");
    symlink(&real, &link).unwrap();
    // The root by its link, the iPod by the real path, and the other way round.
    refused(
        load(&base, &[&link], Some(&real.join("ipod"))),
        "device.mountpoint",
    );
    refused(
        load(&base, &[&real], Some(&link.join("ipod"))),
        "device.mountpoint",
    );
    // A link to the iPod folder from elsewhere, root above it.
    let ilink = base.join("ipod-link");
    symlink(real.join("ipod"), &ilink).unwrap();
    refused(load(&base, &[&real], Some(&ilink)), "device.mountpoint");
    // A link to somewhere else entirely is no overlap.
    let other = base.join("other");
    fs::create_dir_all(&other).unwrap();
    let olink = base.join("other-link");
    symlink(&other, &olink).unwrap();
    assert!(load(&base, &[&real], Some(&olink)).is_ok());
}

// ---- L7: an empty folder where a disk should be

fn fill(env: &Env, dir: &str, n: usize) -> Vec<PathBuf> {
    (0..n)
        .map(|i| env.add(&format!("{dir}/t{i:03}.mp3"), Kind::Mp3, 1))
        .collect()
}

fn empty_dir(files: &[PathBuf]) {
    for f in files {
        fs::remove_file(f).unwrap();
    }
}

#[test]
fn an_emptied_folder_that_held_more_than_50_tracks_marks_none_missing_and_is_reported() {
    let mut env = Env::new();
    env.add("keep.mp3", Kind::Mp3, 9);
    let files = fill(&env, "Disk", 51);
    env.scan();
    empty_dir(&files);

    let r = env.scan();
    assert!(r.missing.is_empty(), "{} missing", r.missing.len());
    assert!(
        has_reason(&r.unreadable, &env.music.join("Disk"), "empty"),
        "{:?}",
        r.unreadable
    );
    assert!(env.tracks().iter().all(|t| t.missing_since.is_none()));

    // The disk comes back: the same tracks, nothing added or moved.
    let ids: Vec<i64> = env.tracks().iter().map(|t| t.id).collect();
    fill(&env, "Disk", 51);
    let r = env.scan();
    assert_eq!(
        (r.added, r.moved.len(), r.missing.len()),
        (0, 0, 0),
        "{r:?}"
    );
    assert!(r.unreadable.is_empty());
    assert_eq!(env.tracks().iter().map(|t| t.id).collect::<Vec<_>>(), ids);
}

#[test]
fn a_folder_of_exactly_50_tracks_emptied_by_hand_is_missing() {
    let mut env = Env::new();
    env.add("keep.mp3", Kind::Mp3, 9);
    let files = fill(&env, "Disk", 50);
    env.scan();
    empty_dir(&files);
    let r = env.scan();
    assert_eq!(r.missing.len(), 50);
    assert!(r.unreadable.is_empty(), "{:?}", r.unreadable);
}

#[test]
fn a_small_album_folder_emptied_file_by_file_is_missing() {
    let mut env = Env::new();
    env.add("keep.mp3", Kind::Mp3, 9);
    let files = fill(&env, "Artist/Album", 12);
    env.scan();
    empty_dir(&files);
    let r = env.scan();
    assert_eq!(r.missing.len(), 12);
    assert!(r.unreadable.is_empty(), "{:?}", r.unreadable);
    assert_eq!(
        env.tracks()
            .iter()
            .filter(|t| t.missing_since.is_some())
            .count(),
        12
    );
}

// A big folder that is gone altogether, or that still holds something, is a
// deletion the user made.
#[test]
fn a_big_folder_removed_or_not_empty_is_deleted_music() {
    let mut env = Env::new();
    env.add("keep.mp3", Kind::Mp3, 9);
    let gone = fill(&env, "Gone", 51);
    let part = fill(&env, "Part", 51);
    env.scan();
    drop(gone);
    fs::remove_dir_all(env.music.join("Gone")).unwrap();
    empty_dir(&part[..2]);
    let r = env.scan();
    assert_eq!(r.missing.len(), 53, "{} missing", r.missing.len());
    assert!(r.unreadable.is_empty(), "{:?}", r.unreadable);
}

// A folder that holds only a non-audio file (a cover) is not empty.
#[test]
fn a_big_folder_left_with_a_cover_file_is_deleted_music() {
    let mut env = Env::new();
    env.add("keep.mp3", Kind::Mp3, 9);
    let files = fill(&env, "Disk", 51);
    env.scan();
    empty_dir(&files);
    fs::write(env.music.join("Disk/cover.jpg"), b"x").unwrap();
    let r = env.scan();
    assert_eq!(r.missing.len(), 51);
}

// ---- L8: a playlists folder that cannot be fully read

/// An env with playlists `keep` and `gone` loaded; "keep" is mapped to an
/// iPod playlist so that deleting it would leave a tombstone.
fn two_playlists() -> Env {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    for name in ["keep", "gone"] {
        env.write_playlist(name, &format!("{}\n", a.display()));
    }
    let conn = env.store.conn();
    playlists::sync_from_disk(conn, &env.cfg, true).unwrap();
    let id = env.playlist_id("keep").unwrap();
    let dev = devices::upsert_device(conn, "G1", None, None).unwrap();
    devices::replace_mappings(
        conn,
        dev,
        &[],
        &[devices::PlaylistMapping {
            playlist_id: id,
            ipod_id: Some(0x77),
            ipod_name: "keep".into(),
        }],
    )
    .unwrap();
    env
}

fn assert_untouched(env: &Env) {
    let conn = env.store.conn();
    assert!(store::removed_playlists(conn).unwrap().is_empty());
    assert!(env.playlist_id("keep").is_some());
    assert!(env.playlist_id("gone").is_some());
    assert_eq!(env.indexed_items("keep").len(), 1);
}

// Prevents (audit L8) a playlist that could not be examined being taken as
// deleted, which tombstones it and makes the next sync delete it on the iPod.
#[test]
fn a_playlist_whose_stat_fails_is_reported_and_no_playlist_is_removed() {
    let env = two_playlists();
    let conn = env.store.conn();
    // keep.m3u8 becomes a link to itself: stat fails with a loop error.
    let keep = env.playlist_file("keep");
    fs::remove_file(&keep).unwrap();
    symlink(&keep, &keep).unwrap();
    // gone.m3u8 is really deleted, but nothing may go while the folder is
    // not fully read.
    fs::remove_file(env.playlist_file("gone")).unwrap();

    let r = playlists::sync_from_disk(conn, &env.cfg, true).unwrap();
    assert!(r.removed.is_empty(), "{r:?}");
    assert!(r.skipped.iter().any(|(p, _)| *p == keep), "{:?}", r.skipped);
    assert_untouched(&env);

    // Once the link is gone too, both really are deleted: the guard does
    // not stick.
    fs::remove_file(&keep).unwrap();
    let r = playlists::sync_from_disk(conn, &env.cfg, true).unwrap();
    let mut removed = r.removed.clone();
    removed.sort();
    assert_eq!(removed, vec!["gone", "keep"]);
    assert!(r.skipped.is_empty(), "{:?}", r.skipped);
    assert_eq!(store::removed_playlists(conn).unwrap().len(), 1);
}

// Prevents a playlist stored under a non-UTF-8 name reading as deleted.
#[test]
fn a_non_utf8_playlist_name_is_reported_and_no_playlist_is_removed() {
    use std::os::unix::ffi::OsStrExt;
    let env = two_playlists();
    let conn = env.store.conn();
    let odd = env.lists.join(std::ffi::OsStr::from_bytes(b"caf\xe9.m3u8"));
    fs::write(&odd, "x\n").unwrap();
    // The folder is read in full otherwise, with "keep" now absent.
    fs::remove_file(env.playlist_file("keep")).unwrap();

    let r = playlists::sync_from_disk(conn, &env.cfg, true).unwrap();
    assert!(r.removed.is_empty(), "{r:?}");
    assert!(r.skipped.iter().any(|(p, _)| *p == odd), "{:?}", r.skipped);
    assert!(store::removed_playlists(conn).unwrap().is_empty());
    assert!(env.playlist_id("keep").is_some());
    assert_eq!(env.indexed_items("keep").len(), 1);
    // Nothing was invented from the odd name either.
    assert_eq!(store::playlists(conn).unwrap().len(), 2);
}

// A non-UTF-8 name that is not a playlist is no problem.
#[test]
fn a_non_utf8_name_that_is_not_a_playlist_does_not_hold_back_deletions() {
    use std::os::unix::ffi::OsStrExt;
    let env = two_playlists();
    let conn = env.store.conn();
    fs::write(
        env.lists.join(std::ffi::OsStr::from_bytes(b"caf\xe9.txt")),
        "x",
    )
    .unwrap();
    fs::remove_file(env.playlist_file("gone")).unwrap();
    let r = playlists::sync_from_disk(conn, &env.cfg, true).unwrap();
    assert_eq!(r.removed, vec!["gone"]);
    assert!(r.skipped.is_empty(), "{:?}", r.skipped);
}

// A link to nothing is a deleted playlist.
#[test]
fn a_dangling_playlist_link_counts_as_deleted() {
    let env = two_playlists();
    let conn = env.store.conn();
    let gone = env.playlist_file("gone");
    fs::remove_file(&gone).unwrap();
    symlink(env.lists.join("nowhere.m3u8"), &gone).unwrap();
    let r = playlists::sync_from_disk(conn, &env.cfg, true).unwrap();
    assert_eq!(r.removed, vec!["gone"], "{r:?}");
    assert!(r.skipped.is_empty(), "{:?}", r.skipped);
    assert!(env.playlist_id("keep").is_some());
}

// Needs a user the permission bits bind (not root).
#[test]
fn a_playlists_folder_whose_files_cannot_be_examined_removes_nothing() {
    let env = two_playlists();
    let conn = env.store.conn();
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }
    let _restore = Restore(env.lists.clone());
    // Listable, not searchable: names are read, no file can be stat'ed.
    fs::set_permissions(&env.lists, fs::Permissions::from_mode(0o444)).unwrap();
    if fs::metadata(env.playlist_file("keep")).is_ok() {
        eprintln!("skipped: permission bits do not bind this user (root)");
        return;
    }
    let r = playlists::sync_from_disk(conn, &env.cfg, true).unwrap();
    assert!(r.removed.is_empty(), "{r:?}");
    assert!(
        r.skipped.iter().any(|(p, _)| p.ends_with("keep.m3u8")),
        "{:?}",
        r.skipped
    );
    assert_untouched(&env);
}
