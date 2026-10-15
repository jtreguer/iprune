//! End to end tests of `iprune adopt`. Every run gets its own HOME and XDG
//! folders in a temp dir and a closed stdin. The "iPod" is a fake tree in a
//! temp dir whose iTunesDB is written by libgpod (`make_ipod.c`); the config
//! points `device.mountpoint` at it, so no real device or mount is involved.

#[path = "../../iprune-core/tests/common/mod.rs"]
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{IpodSpec, Kind, PlaylistSpec, fake_ipod, fixture, ipod_playlist, write_itunesdb};
use iprune_core::store::devices;
use iprune_core::store::{self, Store, TrackRow};

const GUID_SYSINFO: &str = "FirewireGuid: 0x000A27001234ABCD\n";

struct Sandbox {
    dir: tempfile::TempDir,
    music: PathBuf,
    lists: PathBuf,
    ipod: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let (music, lists, ipod) = (base.join("music"), base.join("lists"), base.join("ipod"));
        for d in [&music, &lists, &ipod] {
            fs::create_dir(d).unwrap();
        }
        Sandbox {
            dir,
            music,
            lists,
            ipod,
        }
    }

    fn base(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        let base = self.base();
        Command::new(env!("CARGO_BIN_EXE_iprune"))
            .args(args)
            .env("HOME", base.join("home"))
            .env("XDG_CONFIG_HOME", base.join("config"))
            .env("XDG_DATA_HOME", base.join("data"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let o = self.run(args);
        assert_code(&o, 0);
        o
    }

    /// Init, copy the files in and scan.
    fn library(&self, files: &[(&str, Kind, u32)]) {
        self.ok(&[
            "init",
            "--library",
            self.music.to_str().unwrap(),
            "--playlists",
            self.lists.to_str().unwrap(),
        ]);
        for (name, kind, variant) in files {
            fs::copy(fixture(*kind, *variant), self.music.join(name)).unwrap();
        }
        self.ok(&["scan"]);
    }

    fn set_mountpoint(&self) {
        let cfg = self.base().join("config/iprune/config.toml");
        let text = fs::read_to_string(&cfg).unwrap();
        let empty = "mountpoint = \"\"";
        assert!(text.contains(empty), "{text}");
        let text = text.replace(
            empty,
            &format!("mountpoint = {:?}\nallow_folder = true", self.ipod),
        );
        fs::write(cfg, text).unwrap();
    }

    fn store(&self) -> Store {
        Store::open(&self.base().join("data/iprune/iprune.sqlite")).unwrap()
    }

    fn rows(&self) -> Vec<TrackRow> {
        store::all_tracks(self.store().conn()).unwrap()
    }

    fn row(&self, file: &str) -> TrackRow {
        let p = self.music.join(file);
        self.rows().into_iter().find(|t| t.path == p).unwrap()
    }

    /// An iPod spec for a library file: its byte size and scanned duration.
    fn spec(&self, file: &str, dbid: u64, title: &'static str, artist: &'static str) -> IpodSpec {
        let t = self.row(file);
        IpodSpec {
            dbid,
            size: fs::metadata(&t.path).unwrap().len(),
            ms: t.info.duration_ms.unwrap(),
            title,
            artist,
            album: "",
            plays: 0,
        }
    }

    fn make_ipod(&self, sysinfo: Option<&str>, tracks: &[IpodSpec], pl: &[PlaylistSpec]) {
        fake_ipod(&self.ipod, sysinfo, tracks, pl);
    }

    fn itunesdb(&self) -> PathBuf {
        self.ipod.join("iPod_Control/iTunes/iTunesDB")
    }

    fn report(&self) -> String {
        fs::read_to_string(self.base().join("data/iprune/adopt-report.txt")).unwrap()
    }

    /// The id of the single saved device, saved under the SysInfo GUID.
    fn only_device(&self) -> i64 {
        let s = self.store();
        let (id, guid): (i64, String) = s
            .conn()
            .query_row("SELECT id, firewire_guid FROM devices", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert!(guid.ends_with("000A27001234ABCD"), "{guid}");
        id
    }

    /// (devices, mapped tracks, mapped playlists) in the index.
    fn saved(&self) -> (i64, i64, i64) {
        let s = self.store();
        let n = |t: &str| {
            s.conn()
                .query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
                .unwrap()
        };
        (n("devices"), n("device_tracks"), n("device_playlists"))
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn assert_code(o: &Output, code: i32) {
    assert_eq!(
        o.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        out(o),
        err(o)
    );
}

/// Three files; the iPod holds two of them exactly (no proposals).
fn clean_world() -> Sandbox {
    let sb = Sandbox::new();
    sb.library(&[
        ("one.mp3", Kind::Mp3, 1),
        ("two.m4a", Kind::Aac, 1),
        ("three.wav", Kind::Wav, 3),
    ]);
    let tracks = [
        sb.spec("one.mp3", u64::MAX - 1, "Title1", "Artist1"),
        sb.spec("two.m4a", 2, "Title1", "Artist1"),
    ];
    sb.make_ipod(
        Some(GUID_SYSINFO),
        &tracks,
        &[ipod_playlist("Road", &[u64::MAX - 1, 2])],
    );
    fs::write(
        sb.lists.join("Road.m3u8"),
        format!(
            "{}\n{}\n",
            sb.music.join("one.mp3").display(),
            sb.music.join("two.m4a").display()
        ),
    )
    .unwrap();
    sb.set_mountpoint();
    sb
}

/// The same library plus an iPod track whose title was edited: one weak
/// proposal (dbid 3 for three.wav), one iPod track with no file.
fn world_with_a_proposal() -> Sandbox {
    let sb = clean_world();
    let tracks = [
        sb.spec("one.mp3", u64::MAX - 1, "Title1", "Artist1"),
        sb.spec("two.m4a", 2, "Title1", "Artist1"),
        sb.spec("three.wav", 3, "Edited title", "Artist3"),
        IpodSpec {
            dbid: 4,
            size: 1,
            ms: 1000,
            title: "Orphan",
            artist: "Nobody",
            ..IpodSpec::default()
        },
    ];
    sb.make_ipod(
        Some(GUID_SYSINFO),
        &tracks,
        &[ipod_playlist("Road", &[u64::MAX - 1, 2])],
    );
    sb
}

fn digest(p: &Path) -> Vec<u8> {
    fs::read(p).unwrap()
}

#[test]
fn file_mode_writes_the_report_and_saves_nothing() {
    let sb = world_with_a_proposal();
    let db_file = sb.itunesdb();
    let before = digest(&db_file);
    // Flags that would save in device mode do nothing with --file.
    let o = sb.ok(&[
        "adopt",
        "--file",
        db_file.to_str().unwrap(),
        "--accept-proposals",
        "--force",
    ]);
    assert_eq!(sb.saved(), (0, 0, 0), "{}", out(&o));
    assert_eq!(digest(&db_file), before);
    let report = sb.report();
    assert!(
        report.contains(&sb.music.join("three.wav").display().to_string()),
        "{report}"
    );
    assert!(
        report.contains(&format!("{:016x}", 4)),
        "orphan iPod track listed: {report}"
    );
    assert!(out(&o).contains("adopt-report.txt"), "{}", out(&o));
    // No device was needed: the mountpoint is not read in file mode.
    fs::remove_dir_all(sb.ipod.join("iPod_Control/Device")).unwrap();
    sb.ok(&["adopt", "--file", db_file.to_str().unwrap()]);
}

#[test]
fn file_mode_works_without_a_firewire_guid() {
    let sb = Sandbox::new();
    sb.library(&[("one.mp3", Kind::Mp3, 1)]);
    let db = sb.base().join("copy-of-itunesdb");
    write_itunesdb(
        &db,
        "Backup",
        &[sb.spec("one.mp3", 1, "Title1", "Artist1")],
        &[],
    );
    sb.ok(&["adopt", "--file", db.to_str().unwrap()]);
    assert!(sb.report().contains("Backup"));
    assert_eq!(sb.saved(), (0, 0, 0));
}

#[test]
fn pending_proposals_block_saving_in_device_mode() {
    let sb = world_with_a_proposal();
    let before = digest(&sb.itunesdb());
    let o = sb.ok(&["adopt"]);
    assert_eq!(sb.saved(), (0, 0, 0), "{}", out(&o));
    assert_eq!(digest(&sb.itunesdb()), before);
    // The report is still written so the proposals can be reviewed, and
    // without --accept-proposals it does not claim a proposal was mapped.
    let report = sb.report();
    assert!(report.contains("Edited title"), "{report}");
    assert!(!report.contains("mapped to"), "{report}");
}

#[test]
fn accept_proposals_saves_the_defaults() {
    let sb = world_with_a_proposal();
    let before = digest(&sb.itunesdb());
    sb.ok(&["adopt", "--accept-proposals"]);
    // one, two and (by proposal) three; the orphan stays unmanaged.
    assert_eq!(sb.saved(), (1, 3, 1));
    assert!(sb.report().contains("mapped to"), "{}", sb.report());
    assert_eq!(digest(&sb.itunesdb()), before, "the iPod is only read");

    let store = sb.store();
    let conn = store.conn();
    let dev = sb.only_device();
    let maps = devices::track_mappings(conn, dev).unwrap();
    let one = sb.row("one.mp3");
    let m = maps.iter().find(|m| m.track_id == one.id).unwrap();
    assert_eq!(m.dbid, u64::MAX - 1);
    assert_eq!(
        m.synced_tags_hash, one.tags_hash,
        "identical tags: no change on first sync"
    );
    assert_eq!(m.synced_size, one.size);
    let three = sb.row("three.wav");
    let m = maps.iter().find(|m| m.track_id == three.id).unwrap();
    assert_eq!(m.dbid, 3);
    assert_ne!(
        m.synced_tags_hash, three.tags_hash,
        "the iPod had another title"
    );
    assert!(maps.iter().all(|m| m.dbid != 4), "the orphan is not mapped");
}

#[test]
fn a_second_adoption_needs_force() {
    let sb = world_with_a_proposal();
    sb.ok(&["adopt", "--accept-proposals"]);
    let before = {
        let s = sb.store();
        devices::track_mappings(s.conn(), sb.only_device()).unwrap()
    };
    let o = sb.run(&["adopt", "--accept-proposals"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--force"), "{}", err(&o));
    assert_eq!(sb.saved(), (1, 3, 1));

    // Change the library so a replaced mapping is visible: lose three.wav.
    fs::remove_file(sb.music.join("three.wav")).unwrap();
    sb.ok(&["scan"]);
    sb.ok(&["adopt", "--accept-proposals", "--force"]);
    assert_eq!(sb.saved(), (1, 2, 1), "replaced, not merged");
    assert_eq!(before.len(), 3);
}

#[test]
fn without_proposals_the_adoption_is_saved_directly() {
    let sb = clean_world();
    let o = sb.ok(&["adopt"]);
    assert_eq!(sb.saved(), (1, 2, 1), "{}", out(&o));
    // Running it again is refused whether or not proposals are accepted.
    assert_code(&sb.run(&["adopt"]), 1);
    assert_eq!(sb.saved(), (1, 2, 1));
}

#[test]
fn an_ipod_without_a_firewire_guid_is_never_saved() {
    for sysinfo in [
        None,
        Some("ModelNumStr: MC297\n"),
        Some("FirewireGuid:\n"),
        Some("FirewireGuid:   \n"),
    ] {
        let sb = clean_world();
        sb.make_ipod(sysinfo, &[sb.spec("one.mp3", 1, "Title1", "Artist1")], &[]);
        if sysinfo.is_none() {
            let _ = fs::remove_file(sb.ipod.join("iPod_Control/Device/SysInfo"));
        }
        let o = sb.run(&["adopt", "--accept-proposals", "--force"]);
        assert_code(&o, 1);
        assert!(err(&o).contains("FirewireGuid"), "{}", err(&o));
        assert_eq!(sb.saved(), (0, 0, 0));
        assert!(
            sb.report().contains("iPod"),
            "the report is written before refusing"
        );
    }
}

#[test]
fn an_empty_index_fails_and_saves_nothing() {
    let sb = Sandbox::new();
    sb.ok(&[
        "init",
        "--library",
        sb.music.to_str().unwrap(),
        "--playlists",
        sb.lists.to_str().unwrap(),
    ]);
    let db = sb.base().join("db");
    write_itunesdb(&db, "x", &[], &[]);
    let o = sb.run(&["adopt", "--file", db.to_str().unwrap()]);
    assert_code(&o, 1);
    assert!(err(&o).contains("scan"), "{}", err(&o));
    assert_eq!(sb.saved(), (0, 0, 0));
}

#[test]
fn a_mountpoint_without_a_database_is_an_error() {
    let sb = clean_world();
    fs::remove_file(sb.itunesdb()).unwrap();
    let o = sb.run(&["adopt", "--accept-proposals"]);
    assert_code(&o, 1);
    assert_eq!(sb.saved(), (0, 0, 0));
}

#[test]
fn a_missing_file_is_an_error() {
    let sb = clean_world();
    let o = sb.run(&["adopt", "--file", "/nonexistent/iTunesDB"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("iTunesDB"), "{}", err(&o));
}

#[test]
fn adopt_needs_a_config() {
    let sb = Sandbox::new();
    let o = sb.run(&["adopt", "--file", "/nonexistent"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("init"), "{}", err(&o));
}

#[test]
fn bad_options_are_usage_errors() {
    let sb = Sandbox::new();
    assert_code(&sb.run(&["adopt", "--file"]), 2);
    assert_code(&sb.run(&["adopt", "--nonsense"]), 2);
}
