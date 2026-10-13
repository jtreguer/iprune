//! Getter and database-level tests against a libgpod-written fixture
//! (`tests/c/make_fixture.c`). Expected values come from that C file.

mod common;
use common::{FakeIpod, make_fixture_db};

use std::path::Path;

use gpod::Database;

const BJORK: u64 = 0x86536232fbbc81cf;
const NO_ARTIST: u64 = 1;
const EMPTY_STRINGS: u64 = 0xffff_ffff_ffff_ffff;
const SIGUR: u64 = 0x00ab00cd00ef0012;

fn fixture_db(dir: &Path) -> Database {
    let path = dir.join("iTunesDB");
    make_fixture_db(dir, &path);
    Database::open_file(&path).unwrap()
}

#[test]
fn track_getters_beyond_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture_db(dir.path());
    assert_eq!(db.track_count(), 4);

    let t = db.track_by_dbid(BJORK).expect("track present");
    assert_eq!(t.dbid(), BJORK);
    assert_eq!(t.size(), 4_123_456);
    assert_eq!(t.rating(), 80);
    assert_eq!(t.playcount(), 12);
    assert_eq!(t.tracklen(), 200_000);
    assert_eq!(t.mediatype(), 1); // ITDB_MEDIATYPE_AUDIO
    assert_eq!(t.artist().as_deref(), Some("Björk"));
    assert_eq!(t.album().as_deref(), Some("Homogenic"));
    assert_eq!(t.title().as_deref(), Some("Jóga"));
    assert_eq!(t.filetype().as_deref(), Some("MPEG audio file"));
    assert_eq!(
        t.ipod_path().as_deref(),
        Some(":iPod_Control:Music:F00:AAAA.mp3")
    );
    assert!(!t.has_thumbnails());
    assert!(!t.compilation());
}

#[test]
fn missing_strings_read_as_none() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture_db(dir.path());

    let none = db.track_by_dbid(NO_ARTIST).unwrap();
    assert_eq!(none.artist(), None);
    assert_eq!(none.album(), None);
    assert_eq!(none.genre(), None);
    assert_eq!(none.title().as_deref(), Some("No artist or album"));
    assert_eq!(none.size(), 1);
    assert_eq!(none.rating(), 0);
    assert_eq!(none.playcount(), 0);

    // The fixture writes "" for these. libgpod does not store empty strings in
    // the file, so they read back as NULL, same as the C snapshot's "(null)".
    // A NULL pointer must become None, never a panic or "".
    let empty = db.track_by_dbid(EMPTY_STRINGS).unwrap();
    assert_eq!(empty.dbid(), u64::MAX);
    assert_eq!(empty.artist(), None);
    assert_eq!(empty.album(), None);
    assert_eq!(empty.title(), None);
}

#[test]
fn numeric_extremes_do_not_wrap() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture_db(dir.path());
    let t = db.track_by_dbid(EMPTY_STRINGS).unwrap();
    assert_eq!(t.size(), 3_000_000_000);
    assert_eq!(t.rating(), 100);
    assert_eq!(t.playcount(), u32::MAX);
}

#[test]
fn non_ascii_strings_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture_db(dir.path());
    let t = db.track_by_dbid(SIGUR).unwrap();
    assert_eq!(t.artist().as_deref(), Some("Sigur Rós | pipe"));
    assert_eq!(t.album().as_deref(), Some("Ágætis byrjun"));
    assert_eq!(t.title().as_deref(), Some("日本語 タイトル 🎵"));
    assert_eq!(t.size(), 0);
    assert_eq!(t.rating(), 20);
    assert_eq!(t.playcount(), 3);
}

#[test]
fn track_by_dbid_hit_and_miss() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture_db(dir.path());
    for id in [BJORK, NO_ARTIST, EMPTY_STRINGS, SIGUR] {
        assert_eq!(db.track_by_dbid(id).map(|t| t.dbid()), Some(id));
    }
    assert!(db.track_by_dbid(0).is_none());
    assert!(db.track_by_dbid(2).is_none());
    // Differs from a present id only in the high bits: guards against a
    // comparison truncated to 32 bits.
    assert!(db.track_by_dbid(BJORK ^ (1 << 40)).is_none());
}

#[test]
fn tracks_iterate_in_file_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture_db(dir.path());
    let ids: Vec<u64> = db.tracks().map(|t| t.dbid()).collect();
    assert_eq!(ids, [BJORK, NO_ARTIST, EMPTY_STRINGS, SIGUR]);
}

#[test]
fn playlist_flags_len_and_members() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture_db(dir.path());
    assert_eq!(db.playlist_count(), 5);

    let pls: Vec<_> = db.playlists().collect();
    let names: Vec<_> = pls.iter().map(|p| p.name().unwrap().into_owned()).collect();
    assert_eq!(
        names,
        ["iPod de Test", "80s", "+Empty é", "Top rated", "-Last"]
    );

    let flags: Vec<_> = pls.iter().map(|p| (p.is_master(), p.is_smart())).collect();
    assert_eq!(
        flags,
        [
            (true, false),
            (false, false),
            (false, false),
            (false, true),
            (false, false)
        ]
    );

    let master = &pls[0];
    assert_eq!(master.len(), 4);
    assert!(!master.is_empty());

    // Repeats count and keep their position.
    let eighties = &pls[1];
    assert_eq!(eighties.len(), 4);
    let ids: Vec<u64> = eighties.members().map(|t| t.dbid()).collect();
    assert_eq!(ids, [SIGUR, BJORK, SIGUR, EMPTY_STRINGS]);
    assert_eq!(eighties.members().count(), eighties.len());

    let empty = &pls[2];
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
    assert_eq!(empty.members().count(), 0);

    // Members resolve to the same track data as the database list.
    let last = &pls[4];
    assert_eq!(last.len(), 1);
    let m = last.members().next().unwrap();
    assert_eq!(m.title().as_deref(), Some("No artist or album"));
}

#[test]
fn open_file_has_no_mountpoint_or_guid() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture_db(dir.path());
    assert_eq!(db.mountpoint(), None);
    assert_eq!(db.firewire_guid(), None);
}

#[test]
fn open_mountpoint_reads_the_database() {
    let ipod = FakeIpod::new(None);
    let db = Database::open(&ipod.root()).unwrap();
    assert_eq!(db.track_count(), 4);
    assert_eq!(db.playlist_count(), 5);
    assert_eq!(db.mountpoint().as_deref(), Some(ipod.root().as_path()));
    assert!(db.track_by_dbid(SIGUR).is_some());
}

#[test]
fn mountpoint_and_file_open_agree() {
    let ipod = FakeIpod::new(None);
    let render = |db: &Database| {
        let mut v = Vec::new();
        gpod::snapshot::write(db, &mut v).unwrap();
        v
    };
    let a = render(&Database::open(&ipod.root()).unwrap());
    let b = render(&Database::open_file(&ipod.itunesdb()).unwrap());
    assert_eq!(a, b);
}

#[test]
fn firewire_guid_read_from_sysinfo() {
    let ipod = FakeIpod::new(Some(
        "ModelNumStr: xA450\nFirewireGuid: 0x000A27001234ABCD\n",
    ));
    let db = Database::open(&ipod.root()).unwrap();
    let guid = db.firewire_guid().expect("guid present");
    assert!(guid.ends_with("000A27001234ABCD"), "{guid}");
}

#[test]
fn firewire_guid_none_without_sysinfo() {
    let ipod = FakeIpod::new(None);
    let db = Database::open(&ipod.root()).unwrap();
    assert_eq!(db.firewire_guid(), None);
}

#[test]
fn firewire_guid_none_when_sysinfo_lacks_the_key() {
    let ipod = FakeIpod::new(Some("ModelNumStr: xA450\n"));
    let db = Database::open(&ipod.root()).unwrap();
    assert_eq!(db.firewire_guid(), None);
}

#[test]
fn open_empty_directory_fails_with_parse_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = Database::open(dir.path()).unwrap_err();
    match err {
        gpod::Error::Parse { path, .. } => assert_eq!(path, dir.path()),
        other => panic!("expected Parse, got {other}"),
    }
}

#[test]
fn open_missing_directory_fails_with_parse_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope");
    assert!(matches!(
        Database::open(&missing),
        Err(gpod::Error::Parse { .. })
    ));
}

#[test]
fn open_file_on_garbage_fails_and_error_names_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("iTunesDB");
    std::fs::write(&p, b"this is not an iTunesDB").unwrap();
    let err = Database::open_file(&p).unwrap_err();
    match &err {
        gpod::Error::Parse { path, message } => {
            assert_eq!(path, &p);
            assert!(!message.is_empty());
        }
        other => panic!("expected Parse, got {other}"),
    }
    assert!(err.to_string().contains("iTunesDB"));
}

#[test]
fn open_file_on_truncated_database_fails() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good");
    make_fixture_db(dir.path(), &good);
    let bytes = std::fs::read(&good).unwrap();
    let cut = dir.path().join("cut");
    std::fs::write(&cut, &bytes[..bytes.len() / 3]).unwrap();
    // Either libgpod rejects it, or it yields strictly fewer tracks. It must
    // never report the full database.
    match Database::open_file(&cut) {
        Err(gpod::Error::Parse { .. }) => {}
        Ok(db) => assert!(db.track_count() < 4, "truncated file parsed in full"),
        Err(e) => panic!("{e}"),
    }
}
#[test]
fn master_playlist_is_the_one_named_after_the_ipod() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture_db(dir.path());
    let master = db.master_playlist().expect("fixture has a master playlist");
    assert!(master.is_master());
    assert_eq!(master.name().as_deref(), Some("iPod de Test"));
    // The fixture has a regular playlist called "80s" too; it is not the master.
    assert_eq!(master.len(), db.track_count());
    assert_eq!(db.playlists().filter(|p| p.is_master()).count(), 1);
}

#[test]
fn device_info_reads_the_model_from_sysinfo() {
    // MC297 is the 160 GB black iPod Classic (7th generation).
    let ipod = FakeIpod::new(Some(
        "ModelNumStr: MC297\nFirewireGuid: 0x000A27001234ABCD\n",
    ));
    let info = Database::open(&ipod.root())
        .unwrap()
        .device_info()
        .expect("info");
    assert_eq!(info.capacity_gb, 160.0);
    assert_eq!(info.generation.as_deref(), Some("Classic"));
    assert!(
        info.model.as_deref().unwrap().starts_with("Classic"),
        "{info:?}"
    );
    assert!(
        info.model_number.as_deref().unwrap().ends_with("C297"),
        "{info:?}"
    );
}

#[test]
fn device_info_distinguishes_models() {
    let ipod = FakeIpod::new(Some("ModelNumStr: xA450\n"));
    let info = Database::open(&ipod.root())
        .unwrap()
        .device_info()
        .expect("info");
    assert_eq!(info.capacity_gb, 80.0);
    assert!(
        info.generation.as_deref().unwrap().starts_with("Video"),
        "{info:?}"
    );
}

#[test]
fn device_info_has_no_capacity_without_a_known_model() {
    for sysinfo in [
        None,
        Some("ModelNumStr: ZZZ\n"),
        Some("FirewireGuid: 0x1\n"),
    ] {
        let ipod = FakeIpod::new(sysinfo);
        let info = Database::open(&ipod.root()).unwrap().device_info();
        // libgpod still answers with a placeholder entry; it must not claim a size.
        let info = info.expect("placeholder entry");
        assert_eq!(info.capacity_gb, 0.0, "{sysinfo:?}");
        assert_eq!(info.generation.as_deref(), Some("Unknown"), "{sysinfo:?}");
    }
}

#[test]
fn firewire_guid_none_when_the_value_is_empty_or_blank() {
    for sysinfo in [
        "FirewireGuid:\n",
        "FirewireGuid: \n",
        "FirewireGuid:   \t\n",
    ] {
        let ipod = FakeIpod::new(Some(sysinfo));
        let db = Database::open(&ipod.root()).unwrap();
        assert_eq!(db.firewire_guid(), None, "{sysinfo:?}");
    }
}
