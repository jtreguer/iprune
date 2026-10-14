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

/// Paths the tests never use: the sync worker is not run here.
fn unused_paths() -> iprune_core::config::Paths {
    iprune_core::config::Paths {
        config_file: PathBuf::from("/nonexistent/iprune/config.toml"),
        data_dir: PathBuf::from("/nonexistent/iprune"),
    }
}

fn data_with_mount(mount: &Path) -> Data {
    let mut config = Config::default();
    config.device.mountpoint = mount.to_owned();
    assert!(!config.device.mountpoint.as_os_str().is_empty());
    Data {
        config,
        paths: unused_paths(),
        store: Store::open_in_memory().unwrap(),
        stats: std::cell::RefCell::new(HashMap::new()),
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

// ------------------------------------------------------ playlist editing

/// A `Data` whose playlists folder and tracks live in a temp dir.
struct Sandbox {
    _tmp: tempfile::TempDir,
    data: Data,
    lists: PathBuf,
    tracks: Vec<PathBuf>,
}

fn sandbox(n: usize) -> Sandbox {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let lists = root.join("lists");
    std::fs::create_dir_all(&lists).unwrap();
    let mut config = Config::default();
    config.library.roots = vec![root.join("music")];
    config.playlists.dir = lists.clone();
    config.device.mountpoint = root.join("nomount");
    let store = Store::open_in_memory().unwrap();
    let mut tracks = Vec::new();
    for i in 0..n {
        let path = root.join(format!("music/t{i}.mp3"));
        let key = path.to_str().unwrap().to_owned();
        let f = FileRecord {
            path: path.clone(),
            path_key: key,
            size: 1,
            mtime_ns: 0,
            hash: [i as u8 + 1; 32],
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
        store::insert_track(store.conn(), &f, 0).unwrap();
        tracks.push(path);
    }
    Sandbox {
        _tmp: tmp,
        data: Data {
            config,
            paths: unused_paths(),
            store,
            stats: std::cell::RefCell::new(HashMap::new()),
            warning: None,
        },
        lists,
        tracks,
    }
}

impl Sandbox {
    fn file(&self, name: &str) -> PathBuf {
        self.lists.join(format!("{name}.m3u8"))
    }
    fn text(&self, name: &str) -> String {
        std::fs::read_to_string(self.file(name)).unwrap()
    }
    /// Track paths of a playlist as the window would show them.
    fn shown(&self, name: &str) -> Vec<PathBuf> {
        self.data
            .playlist(name)
            .unwrap()
            .into_iter()
            .map(|r| r.path)
            .collect()
    }
    fn pick(&self, idx: &[usize]) -> Vec<PathBuf> {
        idx.iter().map(|&i| self.tracks[i].clone()).collect()
    }
}

#[test]
fn playlists_dir_is_the_configured_folder() {
    let sb = sandbox(0);
    assert_eq!(sb.data.playlists_dir(), sb.lists.as_path());
}

#[test]
fn create_makes_an_empty_playlist_that_is_listed_and_a_second_create_fails() {
    let sb = sandbox(1);
    sb.data.create("mix").unwrap();
    assert_eq!(sb.text("mix"), "#EXTM3U\n");
    assert_eq!(sb.data.playlist_names().unwrap(), ["mix"]);
    assert!(sb.data.playlist("mix").unwrap().is_empty());

    sb.data.add("mix", &sb.pick(&[0]), None).unwrap();
    let before = sb.text("mix");
    assert!(sb.data.create("mix").is_err());
    assert_eq!(sb.text("mix"), before, "the existing playlist is untouched");
    assert!(sb.data.create("bad/name").is_err());
    assert_eq!(std::fs::read_dir(&sb.lists).unwrap().count(), 1);
}

#[test]
fn add_appends_or_inserts_and_the_window_rows_follow() {
    let sb = sandbox(4);
    sb.data.create("mix").unwrap();
    sb.data.add("mix", &sb.pick(&[0, 1]), None).unwrap();
    assert_eq!(sb.shown("mix"), sb.pick(&[0, 1]));
    sb.data.add("mix", &sb.pick(&[2, 3]), Some(2)).unwrap();
    assert_eq!(sb.shown("mix"), sb.pick(&[0, 2, 3, 1]));
    let rows = sb.data.playlist("mix").unwrap();
    assert_eq!(
        rows.iter().map(|r| r.position).collect::<Vec<_>>(),
        [Some(1), Some(2), Some(3), Some(4)]
    );
    // the file says the same as the rows
    let text = sb.text("mix");
    let on_disk: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
    assert_eq!(
        on_disk,
        sb.pick(&[0, 2, 3, 1])
            .iter()
            .map(|p| p.to_str().unwrap())
            .collect::<Vec<_>>()
    );
}

#[test]
fn add_of_an_unindexed_file_or_to_an_unknown_playlist_fails_and_writes_nothing() {
    let sb = sandbox(1);
    sb.data.create("mix").unwrap();
    let before = sb.text("mix");
    let stranger = sb.lists.join("../music/stranger.mp3");
    assert!(sb.data.add("mix", &[stranger], None).is_err());
    assert!(sb.data.add("nope", &sb.pick(&[0]), None).is_err());
    assert!(sb.data.add("mix", &sb.pick(&[0]), Some(5)).is_err());
    assert_eq!(sb.text("mix"), before);
    assert!(!sb.file("nope").exists());
}

#[test]
fn move_entries_reorders_the_file_and_the_rows() {
    let sb = sandbox(5);
    sb.data.create("mix").unwrap();
    sb.data
        .add("mix", &sb.pick(&[0, 1, 2, 3, 4]), None)
        .unwrap();
    sb.data.move_entries("mix", &[2, 4], 1).unwrap();
    assert_eq!(sb.shown("mix"), sb.pick(&[1, 3, 0, 2, 4]));
    sb.data.move_entries("mix", &[1], 6).unwrap();
    assert_eq!(sb.shown("mix"), sb.pick(&[3, 0, 2, 4, 1]));

    // a bad position changes nothing
    let before = sb.text("mix");
    assert!(sb.data.move_entries("mix", &[9], 1).is_err());
    assert!(sb.data.move_entries("mix", &[1], 7).is_err());
    assert_eq!(sb.text("mix"), before);
}

#[test]
fn remove_drops_exactly_those_entries() {
    let sb = sandbox(4);
    sb.data.create("mix").unwrap();
    sb.data.add("mix", &sb.pick(&[0, 1, 2, 3]), None).unwrap();
    sb.data.remove("mix", &[2, 4, 2]).unwrap();
    assert_eq!(sb.shown("mix"), sb.pick(&[0, 2]));
    let before = sb.text("mix");
    assert!(sb.data.remove("mix", &[1, 3]).is_err());
    assert_eq!(sb.text("mix"), before, "a bad position removes nothing");
    // the library still has all four tracks
    assert_eq!(sb.data.library().unwrap().len(), 4);
}

#[test]
fn rename_moves_the_file_and_keeps_the_entries() {
    let sb = sandbox(2);
    sb.data.create("old").unwrap();
    sb.data.add("old", &sb.pick(&[1, 0]), None).unwrap();
    let text = sb.text("old");
    sb.data.rename("old", "new").unwrap();
    assert!(!sb.file("old").exists());
    assert_eq!(sb.text("new"), text);
    assert_eq!(sb.data.playlist_names().unwrap(), ["new"]);
    assert_eq!(sb.shown("new"), sb.pick(&[1, 0]));

    // onto an existing one, from an unknown one: nothing changes
    sb.data.create("other").unwrap();
    assert!(sb.data.rename("new", "other").is_err());
    assert!(sb.data.rename("ghost", "x").is_err());
    assert_eq!(sb.text("new"), text);
    assert_eq!(sb.text("other"), "#EXTM3U\n");
    assert!(!sb.file("x").exists());
}

#[test]
fn delete_removes_the_file_and_the_listing_but_not_the_tracks() {
    let sb = sandbox(2);
    sb.data.create("keep").unwrap();
    sb.data.create("gone").unwrap();
    sb.data.add("gone", &sb.pick(&[0, 1]), None).unwrap();
    let keep = sb.text("keep");
    sb.data.delete("gone").unwrap();
    assert!(!sb.file("gone").exists());
    assert_eq!(sb.data.playlist_names().unwrap(), ["keep"]);
    assert_eq!(sb.text("keep"), keep);
    assert_eq!(sb.data.library().unwrap().len(), 2);
    assert!(sb.data.delete("gone").is_err());
    assert!(sb.data.playlist("gone").is_err());
}

#[test]
fn playlist_mtime_follows_every_kind_of_edit_and_is_none_for_unknown_playlists() {
    let sb = sandbox(3);
    assert_eq!(sb.data.playlist_mtime("mix"), None);
    sb.data.create("mix").unwrap();
    let m0 = sb.data.playlist_mtime("mix").unwrap();
    assert!(m0 > 0);
    // Make the previous mtime unreachable by "now", whatever the clock grain.
    let set_past = |secs: u64| {
        std::fs::File::options()
            .write(true)
            .open(sb.file("mix"))
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .unwrap();
    };
    set_past(1_000_000);
    let past = sb.data.playlist_mtime("mix").unwrap();
    assert_ne!(past, m0);

    sb.data.add("mix", &sb.pick(&[0, 1, 2]), None).unwrap();
    let after_add = sb.data.playlist_mtime("mix").unwrap();
    assert_ne!(after_add, past);
    set_past(1_000_000);
    sb.data.move_entries("mix", &[3], 1).unwrap();
    let after_move = sb.data.playlist_mtime("mix").unwrap();
    assert_ne!(after_move, past);
    set_past(1_000_000);
    sb.data.remove("mix", &[1]).unwrap();
    assert_ne!(sb.data.playlist_mtime("mix").unwrap(), past);

    // equals what the file system says, so a monitor can compare it
    let fs_mtime = std::fs::metadata(sb.file("mix"))
        .unwrap()
        .modified()
        .unwrap();
    let ns = fs_mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    assert_eq!(sb.data.playlist_mtime("mix"), Some(ns));
    sb.data.delete("mix").unwrap();
    assert_eq!(sb.data.playlist_mtime("mix"), None);
}

#[test]
fn reload_playlists_picks_up_outside_edits_and_deletions_and_writes_no_file() {
    let sb = sandbox(3);
    sb.data.create("mix").unwrap();
    sb.data.add("mix", &sb.pick(&[0]), None).unwrap();
    sb.data.create("doomed").unwrap();

    // Another program rewrites one list, adds one, deletes one.
    let body = |idx: &[usize]| {
        let mut s = String::from("#EXTM3U\n");
        for p in sb.pick(idx) {
            s.push_str(&format!("{}\n", p.display()));
        }
        s
    };
    std::fs::write(sb.file("mix"), body(&[2, 1])).unwrap();
    std::fs::File::options()
        .write(true)
        .open(sb.file("mix"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
        .unwrap();
    std::fs::write(sb.file("fresh"), body(&[0])).unwrap();
    std::fs::remove_file(sb.file("doomed")).unwrap();
    let mix_text = sb.text("mix");

    let report = sb.data.reload_playlists().unwrap();
    assert_eq!(report.removed, ["doomed"]);
    assert!(report.loaded >= 2, "{report:?}");
    assert_eq!(sb.data.playlist_names().unwrap(), ["fresh", "mix"]);
    assert_eq!(sb.shown("mix"), sb.pick(&[2, 1]));
    assert_eq!(sb.text("mix"), mix_text, "reload never rewrites a file");
    assert!(!sb.file("doomed").exists());

    // a second reload with nothing changed loads nothing
    assert_eq!(sb.data.reload_playlists().unwrap().loaded, 0);
}

#[test]
fn reload_stats_picks_up_rows_written_after_open() {
    let sb = sandbox(2);
    let plays =
        |sb: &Sandbox| -> Vec<u32> { sb.data.library().unwrap().iter().map(|r| r.plays).collect() };
    assert_eq!(plays(&sb), vec![0, 0]);
    let id = store::all_tracks(sb.data.store.conn()).unwrap()[1].id;
    sb.data
        .store
        .conn()
        .execute(
            "INSERT INTO stats (track_id, plays, skips) VALUES (?1, 7, 1)",
            [id],
        )
        .unwrap();
    // Not read until asked.
    assert_eq!(plays(&sb), vec![0, 0]);
    sb.data.reload_stats().unwrap();
    let rows = sb.data.library().unwrap();
    let got: Vec<(i64, u32)> = rows
        .iter()
        .map(|r| (r.track_id.unwrap_or(-1), r.plays))
        .collect();
    assert_eq!(rows.iter().map(|r| r.plays).sum::<u32>(), 7, "{got:?}");
    assert_eq!(rows.iter().filter(|r| r.plays == 7).count(), 1);
}
