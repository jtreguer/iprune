//! `Data` without a display. Nothing here sees the real config, the real
//! index or the real iPod: `open` runs in a child process whose home and XDG
//! folders are a temp dir (the test threads share one environment, so it
//! cannot be changed in place), and `device` runs on a hand-built `Data`
//! whose `device.mountpoint` is a temp dir, so `/proc/self/mountinfo` is
//! never scanned.

use std::path::{Path, PathBuf};
use std::process::Command;

use iprune_core::config::Config;
use iprune_core::store::devices::{self, TrackMapping};
use iprune_core::store::{self, FileRecord, Store};
use iprune_core::tags::{AudioInfo, FileKind, Tags};

use super::*;

const GUID: &str = "000A27001234ABCD";

fn data_with_mount(mount: &Path) -> Data {
    let mut config = Config::default();
    config.device.mountpoint = mount.to_owned();
    assert!(!config.device.mountpoint.as_os_str().is_empty());
    Data {
        config,
        store: Store::open_in_memory().unwrap(),
        stats: HashMap::new(),
        warning: None,
    }
}

/// A mount point that looks like an iPod with the given GUID line.
fn fake_ipod(dir: &Path, sysinfo: &str) -> PathBuf {
    let mp = dir.join("ipod");
    let dev = mp.join("iPod_Control/Device");
    std::fs::create_dir_all(&dev).unwrap();
    std::fs::write(dev.join("SysInfo"), sysinfo).unwrap();
    mp
}

fn sysinfo(guid: &str) -> String {
    format!("ModelNumStr: xA446\nFirewireGuid: 0x{guid}\n")
}

fn add_track(store: &Store) -> i64 {
    let f = FileRecord {
        path: "/lib/a.mp3".into(),
        path_key: "/lib/a.mp3".into(),
        size: 1,
        mtime_ns: 0,
        hash: [1; 32],
        info: AudioInfo {
            kind: FileKind::Mp3,
            duration_ms: Some(1000),
            bitrate: None,
            samplerate: None,
        },
        tags: Tags::default(),
        tag_error: None,
        drm: false,
    };
    store::insert_track(store.conn(), &f, 0).unwrap()
}

#[test]
fn device_is_none_when_nothing_is_adopted_or_mounted() {
    let tmp = tempfile::tempdir().unwrap();
    let d = data_with_mount(tmp.path());
    assert!(d.device().unwrap().is_none());
}

#[test]
fn an_adopted_ipod_that_is_not_mounted_is_shown_disconnected() {
    let tmp = tempfile::tempdir().unwrap();
    let d = data_with_mount(tmp.path()); // a folder with no iPod_Control
    let id = devices::upsert_device(d.store.conn(), GUID, Some("Jul's iPod"), None).unwrap();
    devices::set_last_sync(d.store.conn(), id, 1_700_000_000).unwrap();
    let info = d.device().unwrap().unwrap();
    assert_eq!(info.name, "Jul's iPod");
    assert!(info.adopted);
    assert!(!info.connected);
    assert_eq!((info.free, info.capacity), (None, None));
    assert_eq!(info.last_sync, Some(1_700_000_000));
    assert_eq!(info.mapped_tracks, 0);
}

#[test]
fn an_unnamed_adopted_ipod_is_called_ipod() {
    let tmp = tempfile::tempdir().unwrap();
    let d = data_with_mount(tmp.path());
    devices::upsert_device(d.store.conn(), GUID, None, None).unwrap();
    assert_eq!(d.device().unwrap().unwrap().name, "iPod");
}

#[test]
fn a_mounted_adopted_ipod_is_connected_with_space_and_its_mapped_tracks() {
    let tmp = tempfile::tempdir().unwrap();
    let mp = fake_ipod(tmp.path(), &sysinfo(GUID));
    let d = data_with_mount(&mp);
    // Stored in another spelling than the one SysInfo gives.
    let id = devices::upsert_device(
        d.store.conn(),
        &format!("0x{}", GUID.to_lowercase()),
        Some("Mine"),
        None,
    )
    .unwrap();
    let t = add_track(&d.store);
    let m = TrackMapping {
        track_id: t,
        dbid: 5,
        synced_tags_hash: [0; 32],
        synced_size: 1,
        synced_duration_ms: None,
    };
    devices::replace_mappings(d.store.conn(), id, &[m], &[]).unwrap();

    let info = d.device().unwrap().unwrap();
    assert_eq!(info.name, "Mine");
    assert!(info.adopted && info.connected);
    assert_eq!(info.mapped_tracks, 1);
    assert!(info.capacity.is_some_and(|c| c > 0));
    assert!(info.free.is_some());
}

#[test]
fn a_mounted_ipod_that_was_never_adopted_is_reported_as_such() {
    let tmp = tempfile::tempdir().unwrap();
    let mp = fake_ipod(tmp.path(), &sysinfo(GUID));
    let d = data_with_mount(&mp);
    let info = d.device().unwrap().unwrap();
    assert!(!info.adopted);
    assert!(info.connected);
    assert_eq!(info.name, "iPod not adopted");
    assert_eq!(info.mapped_tracks, 0);
    assert_eq!(info.last_sync, None);
    assert!(info.capacity.is_some());
}

#[test]
fn a_mounted_unknown_ipod_wins_over_another_adopted_one_and_borrows_nothing_from_it() {
    let tmp = tempfile::tempdir().unwrap();
    let mp = fake_ipod(tmp.path(), &sysinfo(GUID));
    let d = data_with_mount(&mp);
    let other =
        devices::upsert_device(d.store.conn(), "FFFF0000FFFF0000", Some("Old"), None).unwrap();
    devices::set_last_sync(d.store.conn(), other, 99).unwrap();
    let info = d.device().unwrap().unwrap();
    assert!(!info.adopted && info.connected);
    assert_eq!(info.name, "iPod not adopted");
    assert_eq!(info.last_sync, None);
}

#[test]
fn the_mounted_ipod_is_picked_by_guid_not_by_order() {
    let tmp = tempfile::tempdir().unwrap();
    let mp = fake_ipod(tmp.path(), &sysinfo(GUID));
    let d = data_with_mount(&mp);
    devices::upsert_device(d.store.conn(), "FFFF0000FFFF0000", Some("First"), None).unwrap();
    devices::upsert_device(d.store.conn(), GUID, Some("Second"), None).unwrap();
    let info = d.device().unwrap().unwrap();
    assert_eq!(info.name, "Second");
    assert!(info.adopted && info.connected);
}

// ------------------------------------------------------------------ open

const CHILD: &str = "IPRUNE_GUI_TEST_CHILD";

fn child(test: &str, home: &Path) -> std::process::Output {
    Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--ignored", "--nocapture"])
        .env(CHILD, "1")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .output()
        .unwrap()
}

fn run_child(test: &str, home: &Path) {
    let out = child(test, home);
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("1 passed"),
        "child failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn in_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

#[test]
fn open_without_a_config_says_so_and_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    run_child("data::tests::open_without_config_child", tmp.path());
    assert!(!tmp.path().join("data").exists(), "no index may be created");
    assert!(!tmp.path().join("config").exists());
}

#[test]
#[ignore = "run in a child process by open_without_a_config_says_so_and_creates_nothing"]
fn open_without_config_child() {
    assert!(in_child(), "only meant to run as a child");
    let err = Data::open().err().expect("open must fail").to_string();
    assert!(err.contains("no config"), "{err}");
    assert!(err.contains("iprune init"), "{err}");
}

fn write_config(home: &Path, playlists: &Path, mountpoint: &Path) {
    let mut c = Config::default();
    c.library.roots = vec![home.join("music")];
    c.playlists.dir = playlists.to_owned();
    c.device.mountpoint = mountpoint.to_owned();
    c.save(&home.join("config/iprune/config.toml")).unwrap();
}

#[test]
fn open_with_a_config_gives_an_empty_library_and_no_device() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(home.join("lists")).unwrap();
    std::fs::create_dir_all(home.join("nomount")).unwrap();
    write_config(&home, &home.join("lists"), &home.join("nomount"));
    run_child("data::tests::open_ok_child", &home);
    assert!(home.join("data/iprune/iprune.sqlite").is_file());
}

#[test]
#[ignore = "run in a child process by open_with_a_config_gives_an_empty_library_and_no_device"]
fn open_ok_child() {
    assert!(in_child());
    let d = Data::open().unwrap();
    assert_eq!(d.warning, None);
    assert!(d.library().unwrap().is_empty());
    assert!(d.playlist_names().unwrap().is_empty());
    assert!(d.device().unwrap().is_none());
}

#[test]
fn an_unreadable_playlists_folder_is_a_warning_not_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(home.join("nomount")).unwrap();
    // The playlists folder does not exist, as when its disk is unmounted.
    write_config(&home, &home.join("absent-lists"), &home.join("nomount"));
    run_child("data::tests::open_warning_child", &home);
}

#[test]
#[ignore = "run in a child process by an_unreadable_playlists_folder_is_a_warning_not_an_error"]
fn open_warning_child() {
    assert!(in_child());
    let d = Data::open().expect("an unreadable folder must not be fatal");
    let w = d.warning.clone().expect("the failure is reported");
    assert!(w.contains("playlists folder"), "{w}");
    assert!(d.library().unwrap().is_empty());
}
