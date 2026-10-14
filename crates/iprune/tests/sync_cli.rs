//! End to end tests of `iprune sync`. Every run gets its own HOME and XDG
//! folders in a temp dir and a closed stdin. The "iPod" is a fake tree in a
//! temp dir whose iTunesDB is written by libgpod (`make_ipod.c`), and the
//! config always points `device.mountpoint` at it, so mount detection never
//! looks at the machine's real mounts. A fake `notify-send` first on the
//! PATH logs what a sync would show on the desktop, to `notify.log`.

#[path = "../../iprune-core/tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{IpodSpec, Kind, fake_ipod, fixture, ipod_playlist};
use iprune_core::store::{self, Store};

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
        for d in [&music, &lists, &ipod, &base.join("bin")] {
            fs::create_dir(d).unwrap();
        }
        let tool = base.join("bin/notify-send");
        fs::write(
            &tool,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done >> '{}'\n",
                base.join("notify.log").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
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

    fn plan_json(&self) -> PathBuf {
        self.base().join("data/iprune/plan.json")
    }

    fn run(&self, args: &[&str]) -> Output {
        let base = self.base();
        let path = format!(
            "{}:{}",
            base.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(env!("CARGO_BIN_EXE_iprune"))
            .args(args)
            .env("HOME", base.join("home"))
            .env("XDG_CONFIG_HOME", base.join("config"))
            .env("XDG_DATA_HOME", base.join("data"))
            .env("PATH", path)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let o = self.run(args);
        assert_code(&o, 0);
        o
    }

    fn set_mountpoint(&self, mp: &Path) {
        let cfg = self.base().join("config/iprune/config.toml");
        let text = fs::read_to_string(&cfg).unwrap();
        let empty = "mountpoint = \"\"";
        assert!(text.contains(empty), "{text}");
        fs::write(cfg, text.replace(empty, &format!("mountpoint = {mp:?}"))).unwrap();
    }

    /// Replaces a line of the generated config.
    fn edit_config(&self, from: &str, to: &str) {
        let cfg = self.base().join("config/iprune/config.toml");
        let text = fs::read_to_string(&cfg).unwrap();
        assert!(text.contains(from), "{from:?} not in {text}");
        fs::write(cfg, text.replace(from, to)).unwrap();
    }

    /// Init (with the fake iPod as the mountpoint), copy the files in, scan.
    fn library(&self, files: &[(&str, Kind, u32)]) {
        self.ok(&[
            "init",
            "--library",
            self.music.to_str().unwrap(),
            "--playlists",
            self.lists.to_str().unwrap(),
        ]);
        self.set_mountpoint(&self.ipod);
        for (name, kind, variant) in files {
            fs::copy(fixture(*kind, *variant), self.music.join(name)).unwrap();
        }
        self.ok(&["scan"]);
    }

    fn spec(&self, file: &str, dbid: u64, title: &'static str, artist: &'static str) -> IpodSpec {
        let p = self.music.join(file);
        let store = Store::open(&self.base().join("data/iprune/iprune.sqlite")).unwrap();
        let t = store::all_tracks(store.conn())
            .unwrap()
            .into_iter()
            .find(|t| t.path == p)
            .unwrap();
        IpodSpec {
            dbid,
            size: fs::metadata(&p).unwrap().len(),
            ms: t.info.duration_ms.unwrap(),
            title,
            artist,
            album: "",
            plays: 0,
        }
    }

    /// Every file under `dir`, by relative path, with its bytes.
    fn tree(&self, dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            for e in rd {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(root, &p, out);
                } else {
                    out.insert(
                        p.strip_prefix(root).unwrap().to_owned(),
                        fs::read(&p).unwrap(),
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(dir, dir, &mut out);
        out
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

/// Three files and an iPod holding two of them, with the playlist Road. The
/// iPod is not adopted yet.
fn world() -> Sandbox {
    let sb = Sandbox::new();
    sb.library(&[
        ("one.mp3", Kind::Mp3, 1),
        ("two.m4a", Kind::Aac, 1),
        ("three.wav", Kind::Wav, 3),
    ]);
    let tracks = [
        sb.spec("one.mp3", u64::MAX - 1, "Title1", "Artist1"),
        sb.spec("two.m4a", 2, "Title1", "Artist1"),
        sb.spec("three.wav", 3, "Title3", "Artist3"),
    ];
    fake_ipod(
        &sb.ipod,
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
    sb
}

fn adopted() -> Sandbox {
    let sb = world();
    sb.ok(&["adopt"]);
    sb
}

#[test]
fn plan_only_prints_the_summary_saves_the_plan_and_writes_nothing_else() {
    let sb = adopted();
    let ipod_before = sb.tree(&sb.ipod);
    let music_before = sb.tree(&sb.music);
    let lists_before = sb.tree(&sb.lists);
    let o = sb.ok(&["sync", "--plan-only"]);
    let text = out(&o);
    assert!(text.contains("0 tracks to copy"), "{text}");
    assert!(text.contains("nothing was written to the iPod"), "{text}");
    let plan: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(sb.plan_json()).unwrap()).unwrap();
    assert_eq!(plan["copy"].as_array().unwrap().len(), 0, "{plan}");
    assert_eq!(plan["playlists"].as_array().unwrap().len(), 0, "{plan}");
    assert_eq!(plan["expected"]["track_count"], 3);
    assert_eq!(sb.tree(&sb.ipod), ipod_before, "the iPod is only read");
    assert_eq!(sb.tree(&sb.music), music_before);
    assert_eq!(sb.tree(&sb.lists), lists_before);
}

#[test]
fn plan_only_scans_first_so_a_new_file_is_planned_for_copy() {
    let sb = adopted();
    fs::copy(fixture(Kind::Mp3, 4), sb.music.join("four.mp3")).unwrap();
    let o = sb.ok(&["sync", "--plan-only"]);
    assert!(out(&o).contains("1 track to copy"), "{}", out(&o));
    let plan: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(sb.plan_json()).unwrap()).unwrap();
    let copy = plan["copy"].as_array().unwrap();
    assert_eq!(copy.len(), 1, "{plan}");
    assert_eq!(
        copy[0]["path"].as_str().unwrap(),
        sb.music.join("four.mp3").to_str().unwrap()
    );
    let verbose = sb.ok(&["sync", "--plan-only", "--verbose"]);
    assert!(
        out(&verbose).contains(&sb.music.join("four.mp3").display().to_string()),
        "{}",
        out(&verbose)
    );
}

#[test]
fn json_prints_the_plan_that_is_saved() {
    let sb = adopted();
    fs::copy(fixture(Kind::Mp3, 4), sb.music.join("four.mp3")).unwrap();
    let o = sb.ok(&["sync", "--plan-only", "--json"]);
    let printed: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(sb.plan_json()).unwrap()).unwrap();
    assert_eq!(printed, saved);
    assert_eq!(printed["copy"].as_array().unwrap().len(), 1);
    // Only the JSON on stdout.
    assert!(!out(&o).contains("nothing was written"), "{}", out(&o));
}

/// The adopted world, plus a file `Top rated.m3u8` and a smart playlist of
/// that name on the iPod (written before adoption).
fn with_smart_export() -> Sandbox {
    let sb = world();
    let tracks = [
        sb.spec("one.mp3", u64::MAX - 1, "Title1", "Artist1"),
        sb.spec("two.m4a", 2, "Title1", "Artist1"),
        sb.spec("three.wav", 3, "Title3", "Artist3"),
    ];
    let mut lists = vec![ipod_playlist("Road", &[u64::MAX - 1, 2])];
    lists.push(common::PlaylistSpec {
        name: "Top rated",
        smart: true,
        podcasts: false,
        members: vec![2],
    });
    fake_ipod(&sb.ipod, Some(GUID_SYSINFO), &tracks, &lists);
    fs::write(
        sb.lists.join("Top rated.m3u8"),
        format!("{}\n", sb.music.join("one.mp3").display()),
    )
    .unwrap();
    sb.ok(&["adopt"]);
    sb
}

#[test]
fn a_smart_playlist_export_is_in_the_json_plan_and_not_in_its_operations() {
    let sb = with_smart_export();
    let ipod_before = sb.tree(&sb.ipod);
    let o = sb.ok(&["sync", "--plan-only", "--json"]);
    let plan: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(
        plan["playlists_skipped"],
        serde_json::json!([{"name": "Top rated", "reason": "smart_on_ipod"}]),
        "{plan}"
    );
    assert_eq!(plan["playlists"].as_array().unwrap().len(), 0, "{plan}");
    let expected: Vec<&str> = plan["expected"]["playlists"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(expected, vec!["Road"], "{plan}");
    assert_eq!(sb.tree(&sb.ipod), ipod_before, "the iPod is only read");

    let text = out(&sb.ok(&["sync", "--plan-only", "--verbose"]));
    assert!(text.contains("Playlist files not synced"), "{text}");
    assert!(text.contains("Top rated"), "{text}");
}

#[test]
fn json_and_verbose_together_are_a_usage_error() {
    let sb = adopted();
    let o = sb.run(&["sync", "--plan-only", "--json", "--verbose"]);
    assert_code(&o, 2);
    assert!(!sb.plan_json().exists());
}

#[test]
fn an_ipod_that_was_not_adopted_is_refused_with_no_plan_saved() {
    let sb = world();
    let ipod_before = sb.tree(&sb.ipod);
    let o = sb.run(&["sync", "--plan-only"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("adopt"), "{}", err(&o));
    assert!(!err(&o).contains("panicked"), "{}", err(&o));
    assert!(!sb.plan_json().exists());
    assert_eq!(sb.tree(&sb.ipod), ipod_before);
}

#[test]
fn a_mountpoint_that_is_not_an_ipod_fails_cleanly() {
    let sb = adopted();
    let empty = sb.base().join("not-an-ipod");
    fs::create_dir(&empty).unwrap();
    // Point the config at it instead of the fake iPod.
    let cfg = sb.base().join("config/iprune/config.toml");
    let text = fs::read_to_string(&cfg).unwrap();
    fs::write(
        &cfg,
        text.replace(&format!("{:?}", sb.ipod), &format!("{empty:?}")),
    )
    .unwrap();
    assert!(fs::read_to_string(&cfg).unwrap().contains("not-an-ipod"));

    let o = sb.run(&["sync", "--plan-only"]);
    assert_code(&o, 1);
    assert!(!err(&o).is_empty());
    assert!(!err(&o).contains("panicked"), "{}", err(&o));
    assert!(!sb.plan_json().exists());
    assert!(
        sb.tree(&empty).is_empty(),
        "nothing written to the mountpoint"
    );
}

#[test]
fn a_bad_scope_is_refused_with_no_plan_saved() {
    let sb = adopted();
    sb.edit_config("scope = \"all\"", "scope = \"playlists\"");
    sb.edit_config("playlists = []", "playlists = [\"No such list\"]");
    let o = sb.run(&["sync", "--plan-only"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("No such list"), "{}", err(&o));
    assert!(!sb.plan_json().exists());
}

// ---- applying (M4)

const GUID_DIR: &str = "0x000A27001234ABCD";

impl Sandbox {
    fn backups_dir(&self) -> PathBuf {
        self.base().join("data/iprune/backups")
    }

    fn itunesdb(&self) -> Vec<u8> {
        fs::read(self.ipod.join("iPod_Control/iTunes/iTunesDB")).unwrap()
    }

    /// The members of an iPod playlist, by dbid, read with libgpod.
    fn ipod_playlist(&self, name: &str) -> Option<Vec<u64>> {
        let db = gpod::Database::open(&self.ipod).unwrap();
        db.playlists()
            .find(|p| p.name().as_deref() == Some(name))
            .map(|p| p.members().map(|t| t.dbid()).collect())
    }

    /// The audio files under the fake iPod's Music folder.
    fn music_files(&self) -> Vec<PathBuf> {
        self.tree(&self.ipod.join("iPod_Control/Music"))
            .into_keys()
            .map(|k| self.ipod.join("iPod_Control/Music").join(k))
            .collect()
    }

    fn stamps(&self) -> Vec<String> {
        let dir = self.backups_dir().join(GUID_DIR);
        let mut v: Vec<String> = fs::read_dir(dir)
            .map(|d| {
                d.map(|e| e.unwrap().file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Rewrites Road.m3u8 to list `two` then `one`.
    fn reverse_road(&self) {
        fs::write(
            self.lists.join("Road.m3u8"),
            format!(
                "{}\n{}\n",
                self.music.join("two.m4a").display(),
                self.music.join("one.mp3").display()
            ),
        )
        .unwrap();
    }
}

#[test]
fn sync_with_yes_applies_the_playlist_changes_to_the_fake_ipod() {
    let sb = adopted();
    sb.reverse_road();
    let db_before = sb.itunesdb();
    let o = sb.ok(&["sync", "--yes"]);
    let text = out(&o);
    assert!(text.contains("applied 1 "), "{text}");
    assert!(text.contains("verified"), "{text}");
    assert_eq!(sb.ipod_playlist("Road"), Some(vec![2, u64::MAX - 1]));
    assert_ne!(sb.itunesdb(), db_before);
    // The backup holds the database as it was.
    let stamps = sb.stamps();
    assert_eq!(stamps.len(), 1, "{stamps:?}");
    assert!(text.contains(&stamps[0]), "{text}");
    assert_eq!(
        fs::read(
            sb.backups_dir()
                .join(GUID_DIR)
                .join(&stamps[0])
                .join("iTunesDB")
        )
        .unwrap(),
        db_before
    );
    assert!(sb.base().join("data/iprune/stats.csv").is_file());
    assert!(sb.ipod.join("iPod_Control/iprune/manifest.json").is_file());
    // The next sync has nothing to do.
    let again = sb.ok(&["sync", "--yes"]);
    assert!(out(&again).contains("Nothing to apply"), "{}", out(&again));
    assert_eq!(sb.stamps().len(), 1, "no second backup for an empty plan");
}

#[test]
fn sync_without_yes_and_without_a_terminal_fails_and_writes_nothing() {
    let sb = adopted();
    sb.reverse_road();
    let ipod_before = sb.tree(&sb.ipod);
    let o = sb.run(&["sync"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--yes"), "{}", err(&o));
    assert!(
        out(&o).contains("1 changed"),
        "the plan is shown first: {}",
        out(&o)
    );
    assert_eq!(sb.tree(&sb.ipod), ipod_before, "the iPod is untouched");
    assert!(
        !sb.backups_dir().exists(),
        "no backup, so nothing was started"
    );
    assert_eq!(sb.ipod_playlist("Road"), Some(vec![u64::MAX - 1, 2]));
}

#[test]
fn json_without_plan_only_is_a_usage_error_and_does_nothing() {
    let sb = adopted();
    sb.reverse_road();
    let ipod_before = sb.tree(&sb.ipod);
    for args in [&["sync", "--json"][..], &["sync", "--json", "--yes"]] {
        let o = sb.run(args);
        assert_code(&o, 2);
        assert!(out(&o).is_empty(), "{}", out(&o));
    }
    assert!(!sb.plan_json().exists());
    assert_eq!(sb.tree(&sb.ipod), ipod_before);
}

#[test]
fn a_sync_with_no_playlist_change_says_so_and_takes_no_backup() {
    let sb = adopted();
    let ipod_before = sb.tree(&sb.ipod);
    let o = sb.ok(&["sync", "--yes"]);
    assert!(out(&o).contains("Nothing to apply"), "{}", out(&o));
    assert_eq!(sb.tree(&sb.ipod), ipod_before);
    assert!(!sb.backups_dir().exists());
}

#[test]
fn sync_copies_a_new_file_to_the_fake_ipod_and_reports_the_counts() {
    let sb = adopted();
    let four = sb.music.join("four.mp3");
    fs::copy(fixture(Kind::Mp3, 4), &four).unwrap();
    fs::write(
        sb.lists.join("Road.m3u8"),
        format!(
            "{}\n{}\n{}\n",
            sb.music.join("one.mp3").display(),
            sb.music.join("two.m4a").display(),
            four.display()
        ),
    )
    .unwrap();
    let files_before = sb.music_files();
    assert!(
        files_before.is_empty(),
        "set up: the fake iPod holds no audio"
    );
    let size = fs::metadata(&four).unwrap().len();

    let o = sb.ok(&["sync", "--yes"]);
    let text = out(&o);
    assert!(text.contains("copied 1 tracks"), "{text}");
    assert!(
        text.contains("copied 0 again, removed 0, retagged 0"),
        "{text}"
    );
    assert!(text.contains("applied 1 playlist changes"), "{text}");
    assert!(text.contains("verified"), "{text}");
    assert!(!text.contains("left out"), "{text}");

    // One audio file on the iPod, with the library file's bytes, and the
    // track in the playlist after the two that were there.
    let files = sb.music_files();
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(fs::read(&files[0]).unwrap(), fs::read(&four).unwrap());
    assert_eq!(files[0].metadata().unwrap().len(), size);
    let road = sb.ipod_playlist("Road").unwrap();
    assert_eq!(&road[..2], [u64::MAX - 1, 2]);
    assert_eq!(road.len(), 3);
    let db = gpod::Database::open(&sb.ipod).unwrap();
    let t = db.track_by_dbid(road[2]).expect("the new track");
    assert_eq!(t.title().as_deref(), Some("Title4"));
    assert_eq!(db.track_count(), 4);
    drop(db);
    assert_eq!(sb.stamps().len(), 1, "one backup, taken first");

    // Nothing left to do, and no second copy of the file.
    let again = sb.ok(&["sync", "--yes"]);
    assert!(out(&again).contains("Nothing to apply"), "{}", out(&again));
    assert_eq!(sb.music_files().len(), 1);
    assert_eq!(sb.stamps().len(), 1);
}

#[test]
fn a_file_removed_from_the_library_is_removed_from_the_fake_ipod() {
    let sb = adopted();
    fs::remove_file(sb.music.join("three.wav")).unwrap();
    let o = sb.ok(&["sync", "--yes"]);
    let text = out(&o);
    assert!(text.contains("removed 1"), "{text}");
    let db = gpod::Database::open(&sb.ipod).unwrap();
    assert!(db.track_by_dbid(3).is_none());
    assert_eq!(db.track_count(), 2);
}

/// Eight managed playlists besides Road, each with a file.
fn many() -> Sandbox {
    let sb = Sandbox::new();
    sb.library(&[
        ("one.mp3", Kind::Mp3, 1),
        ("two.m4a", Kind::Aac, 1),
        ("three.wav", Kind::Wav, 3),
    ]);
    let tracks = [
        sb.spec("one.mp3", u64::MAX - 1, "Title1", "Artist1"),
        sb.spec("two.m4a", 2, "Title1", "Artist1"),
        sb.spec("three.wav", 3, "Title3", "Artist3"),
    ];
    let names = ["P1", "P2", "P3", "P4", "P5", "P6", "P7"];
    let mut lists = vec![ipod_playlist("Road", &[u64::MAX - 1, 2])];
    for n in names {
        lists.push(ipod_playlist(n, &[3]));
    }
    fake_ipod(&sb.ipod, Some(GUID_SYSINFO), &tracks, &lists);
    let entry = |f: &str| format!("{}\n", sb.music.join(f).display());
    fs::write(
        sb.lists.join("Road.m3u8"),
        entry("one.mp3") + &entry("two.m4a"),
    )
    .unwrap();
    for n in names {
        fs::write(sb.lists.join(format!("{n}.m3u8")), entry("three.wav")).unwrap();
    }
    sb.ok(&["adopt"]);
    sb
}

#[test]
fn deleting_most_playlists_is_refused_without_allow_mass_removal() {
    let sb = many();
    for n in ["P1", "P2", "P3", "P4", "P5", "P6", "P7"] {
        fs::remove_file(sb.lists.join(format!("{n}.m3u8"))).unwrap();
    }
    let ipod_before = sb.tree(&sb.ipod);
    let o = sb.run(&["sync", "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--allow-mass-removal"), "{}", err(&o));
    assert_eq!(sb.tree(&sb.ipod), ipod_before);
    assert!(!sb.backups_dir().exists());

    sb.ok(&["sync", "--yes", "--allow-mass-removal"]);
    for n in ["P1", "P4", "P7"] {
        assert_eq!(sb.ipod_playlist(n), None, "{n}");
    }
    assert_eq!(sb.ipod_playlist("Road"), Some(vec![u64::MAX - 1, 2]));
}

#[test]
fn backups_lists_the_backups_of_each_ipod() {
    let sb = adopted();
    let none = sb.ok(&["backups"]);
    assert!(out(&none).contains("no backups"), "{}", out(&none));
    sb.reverse_road();
    sb.ok(&["sync", "--yes"]);
    let o = sb.ok(&["backups"]);
    let stamps = sb.stamps();
    assert_eq!(stamps.len(), 1);
    let text = out(&o);
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    assert!(lines.iter().any(|l| l.contains(GUID_DIR)), "{lines:?}");
    assert!(lines.iter().any(|l| *l == stamps[0]), "{lines:?}");
}

#[test]
fn restore_with_yes_puts_the_database_and_the_mappings_back() {
    let sb = adopted();
    let db_before = sb.itunesdb();
    sb.reverse_road();
    sb.ok(&["sync", "--yes"]);
    let stamp = sb.stamps().remove(0);
    assert_ne!(sb.itunesdb(), db_before);

    let o = sb.ok(&["restore", &stamp, "--yes"]);
    assert!(out(&o).contains("restored iTunesDB"), "{}", out(&o));
    assert_eq!(sb.itunesdb(), db_before);
    assert_eq!(sb.ipod_playlist("Road"), Some(vec![u64::MAX - 1, 2]));
    // The mappings are back too: the file still says two, one, so the plan
    // sets the members again, against the restored iPod list.
    let plan = sb.ok(&["sync", "--plan-only", "--json"]);
    let plan: serde_json::Value = serde_json::from_str(&out(&plan)).unwrap();
    let ops = plan["playlists"].as_array().unwrap();
    assert_eq!(ops.len(), 1, "{plan}");
    assert_eq!(ops[0]["op"], "set_members");
    assert_eq!(ops[0]["name"], "Road");
}

#[test]
fn restore_without_yes_and_without_a_terminal_changes_nothing() {
    let sb = adopted();
    sb.reverse_road();
    sb.ok(&["sync", "--yes"]);
    let stamp = sb.stamps().remove(0);
    let ipod_after_sync = sb.tree(&sb.ipod);
    let o = sb.run(&["restore", &stamp]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--yes"), "{}", err(&o));
    assert_eq!(sb.tree(&sb.ipod), ipod_after_sync);
}

#[test]
fn restoring_an_unknown_stamp_fails_and_changes_nothing() {
    let sb = adopted();
    sb.reverse_road();
    sb.ok(&["sync", "--yes"]);
    let ipod_after_sync = sb.tree(&sb.ipod);
    let o = sb.run(&["restore", "19990101-000000", "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("19990101-000000"), "{}", err(&o));
    assert_eq!(sb.tree(&sb.ipod), ipod_after_sync);
}

#[test]
fn restore_with_no_backups_at_all_fails_cleanly() {
    let sb = adopted();
    let ipod_before = sb.tree(&sb.ipod);
    let o = sb.run(&["restore", "20260101-000000", "--yes"]);
    assert_code(&o, 1);
    assert!(!err(&o).contains("panicked"), "{}", err(&o));
    assert_eq!(sb.tree(&sb.ipod), ipod_before);
}

// ---- the iPod lock

impl Sandbox {
    /// Holds the iPod lock the way a running sync would.
    fn hold_ipod_lock(&self) -> iprune_core::lock::IpodLock {
        iprune_core::lock::ipod(&iprune_core::config::Paths {
            config_file: self.base().join("config/iprune/config.toml"),
            data_dir: self.base().join("data/iprune"),
        })
        .unwrap()
    }
}

#[test]
fn sync_while_the_ipod_lock_is_held_fails_busy_and_leaves_the_ipod_alone() {
    let sb = adopted();
    sb.reverse_road();
    let ipod_before = sb.tree(&sb.ipod);
    let lock = sb.hold_ipod_lock();
    let o = sb.run(&["sync", "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("another iprune is"), "{}", err(&o));
    assert_eq!(sb.tree(&sb.ipod), ipod_before);
    assert!(sb.stamps().is_empty(), "no backup");

    // Once the lock is free the same command applies the change.
    drop(lock);
    sb.ok(&["sync", "--yes"]);
    assert_eq!(sb.ipod_playlist("Road"), Some(vec![2, u64::MAX - 1]));
}

#[test]
fn plan_only_and_doctor_read_the_ipod_only_under_the_lock() {
    let sb = adopted();
    let lock = sb.hold_ipod_lock();
    for args in [&["sync", "--plan-only"][..], &["doctor"][..]] {
        let o = sb.run(args);
        assert_code(&o, 1);
        assert!(
            err(&o).contains("another iprune is"),
            "{args:?}: {}",
            err(&o)
        );
        assert!(
            !err(&o).contains("reading the iPod"),
            "{args:?}: {}",
            err(&o)
        );
    }
    assert!(!sb.plan_json().exists(), "no plan saved");
    drop(lock);
    sb.ok(&["sync", "--plan-only"]);
    sb.ok(&["doctor"]);
}

// ---- doctor

impl Sandbox {
    /// Puts a file at each track's place on the fake iPod, so no entry is
    /// missing its file (make_ipod.c names the k-th track F(k)/T000k.mp3).
    fn give_files(&self, n: usize) {
        for k in 0..n {
            let f = self
                .ipod
                .join(format!("iPod_Control/Music/F{k:02}/T{k:04}.mp3"));
            fs::write(f, format!("audio {k}")).unwrap();
        }
    }

    fn orphans(&self, n: usize) -> Vec<PathBuf> {
        (0..n)
            .map(|i| {
                let f = self
                    .ipod
                    .join(format!("iPod_Control/Music/F20/LOST{i}.mp3"));
                fs::write(&f, b"x").unwrap();
                f
            })
            .collect()
    }
}

#[test]
fn doctor_on_a_clean_ipod_says_so_and_changes_nothing() {
    let sb = adopted();
    sb.give_files(3);
    let before = sb.tree(&sb.ipod);
    let o = sb.ok(&["doctor"]);
    assert!(out(&o).contains("nothing to repair"), "{}", out(&o));
    assert_eq!(sb.tree(&sb.ipod), before);
}

#[test]
fn doctor_without_fix_lists_the_problems_and_changes_nothing() {
    let sb = adopted();
    sb.give_files(2); // the third entry has no file
    let orphan = sb.orphans(1);
    let before = sb.tree(&sb.ipod);
    let o = sb.ok(&["doctor"]);
    let text = out(&o);
    assert!(text.contains(&orphan[0].display().to_string()), "{text}");
    assert!(
        text.contains(&format!("{:016x}", 3)),
        "the missing entry: {text}"
    );
    assert!(text.contains("doctor --fix"), "{text}");
    assert_eq!(sb.tree(&sb.ipod), before);
    assert!(!sb.backups_dir().exists());
}

#[test]
fn doctor_fix_without_yes_and_without_a_terminal_changes_nothing() {
    let sb = adopted();
    sb.give_files(2);
    sb.orphans(1);
    let before = sb.tree(&sb.ipod);
    let o = sb.run(&["doctor", "--fix"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--yes"), "{}", err(&o));
    assert_eq!(sb.tree(&sb.ipod), before);
    assert!(!sb.backups_dir().exists());
}

#[test]
fn doctor_fix_repairs_the_entries_the_orphans_and_keeps_the_playlist_mappings() {
    let sb = adopted();
    sb.give_files(2); // track 3 (three.wav) has no file
    let orphans = sb.orphans(2);
    let mappings_before = sb.mappings();
    assert_eq!(mappings_before.0, 3);
    let o = sb.ok(&["doctor", "--fix", "--yes"]);
    let text = out(&o);
    assert!(text.contains("removed 1 database entries"), "{text}");
    assert!(text.contains("deleted 2 orphan files"), "{text}");

    let db = gpod::Database::open(&sb.ipod).unwrap();
    assert!(
        db.track_by_dbid(3).is_none(),
        "the entry without a file is gone"
    );
    assert!(db.track_by_dbid(2).is_some() && db.track_by_dbid(u64::MAX - 1).is_some());
    drop(db);
    for f in orphans {
        assert!(!f.exists(), "{}", f.display());
    }
    // The two files that belong to entries are still there.
    assert!(sb.ipod.join("iPod_Control/Music/F00/T0000.mp3").is_file());
    assert!(sb.ipod.join("iPod_Control/Music/F01/T0001.mp3").is_file());
    assert_eq!(sb.ipod_playlist("Road"), Some(vec![u64::MAX - 1, 2]));
    assert_eq!(sb.stamps().len(), 1, "a backup was taken");
    // Playlist mappings are exactly as before; the track mapping went.
    let after = sb.mappings();
    assert_eq!(after.1, mappings_before.1);
    assert_eq!(after.0, 2);
    // And doctor now finds nothing.
    let o = sb.ok(&["doctor"]);
    assert!(out(&o).contains("nothing to repair"), "{}", out(&o));
}

// Doctor holds one lock from before it reads the iPod to the last repair,
// and `exec::sync` borrows it, so the repair does not block itself; a lock
// held by someone else stops it before anything is read.
#[test]
fn doctor_fix_deletes_orphans_after_its_own_sync_and_leaves_the_lock_free() {
    let sb = adopted();
    sb.give_files(2); // track 3 has no file: a sync runs, then the orphans go
    let orphans = sb.orphans(2);
    let o = sb.ok(&["doctor", "--fix", "--yes"]);
    assert!(out(&o).contains("deleted 2 orphan files"), "{}", out(&o));
    assert!(orphans.iter().all(|f| !f.exists()));
    drop(sb.hold_ipod_lock());
}

#[test]
fn doctor_fix_will_not_delete_orphans_while_the_ipod_lock_is_held() {
    let sb = adopted();
    sb.give_files(3); // nothing missing: only orphans to delete
    let orphans = sb.orphans(2);
    let before = sb.tree(&sb.ipod);
    let lock = sb.hold_ipod_lock();
    let o = sb.run(&["doctor", "--fix", "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("another iprune is"), "{}", err(&o));
    assert!(orphans.iter().all(|f| f.exists()));
    assert_eq!(sb.tree(&sb.ipod), before);

    drop(lock);
    let o = sb.ok(&["doctor", "--fix", "--yes"]);
    assert!(out(&o).contains("deleted 2 orphan files"), "{}", out(&o));
}

#[test]
fn many_orphans_are_refused_until_allowed() {
    let sb = adopted();
    sb.give_files(3);
    let orphans = sb.orphans(6);
    let before = sb.tree(&sb.ipod);
    let o = sb.run(&["doctor", "--fix", "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--allow-mass-removal"), "{}", err(&o));
    assert_eq!(sb.tree(&sb.ipod), before, "nothing deleted");

    let o = sb.ok(&["doctor", "--fix", "--yes", "--allow-mass-removal"]);
    assert!(out(&o).contains("deleted 6 orphan files"), "{}", out(&o));
    assert!(orphans.iter().all(|f| !f.exists()));
}

// Prevents a mass-orphan refusal coming after the database repair: with
// entries to repair and too many orphans, `--fix` must change nothing.
#[test]
fn many_orphans_with_missing_entries_refuse_before_any_repair() {
    let sb = adopted();
    sb.give_files(2); // track 3 has no file: a database repair is due
    let orphans = sb.orphans(6);
    let before = sb.tree(&sb.ipod);
    let mappings_before = sb.mappings();
    let o = sb.run(&["doctor", "--fix", "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--allow-mass-removal"), "{}", err(&o));
    assert!(!out(&o).contains("removed"), "{}", out(&o));
    assert_eq!(sb.tree(&sb.ipod), before, "iTunesDB and files untouched");
    assert!(!sb.backups_dir().exists(), "no backup, so no write began");
    assert!(orphans.iter().all(|f| f.exists()));
    assert_eq!(sb.mappings(), mappings_before);
    let db = gpod::Database::open(&sb.ipod).unwrap();
    assert!(db.track_by_dbid(3).is_some(), "the entry was not removed");
}

/// Like `adopted()`, with `extra` more iPod tracks no library file
/// matches (dbids 100 and up). The k-th track of the iPod database has
/// its place at F(k)/T000k.mp3, which `give_files` fills.
fn adopted_with_extras(extra: usize) -> Sandbox {
    let sb = Sandbox::new();
    sb.library(&[
        ("one.mp3", Kind::Mp3, 1),
        ("two.m4a", Kind::Aac, 1),
        ("three.wav", Kind::Wav, 3),
    ]);
    let mut tracks = vec![
        sb.spec("one.mp3", u64::MAX - 1, "Title1", "Artist1"),
        sb.spec("two.m4a", 2, "Title1", "Artist1"),
        sb.spec("three.wav", 3, "Title3", "Artist3"),
    ];
    for k in 0..extra {
        tracks.push(IpodSpec {
            dbid: 100 + k as u64,
            size: 100,
            ms: 1000,
            title: Box::leak(format!("Extra{k}").into_boxed_str()),
            artist: "Nobody",
            album: "",
            plays: 0,
        });
    }
    fake_ipod(
        &sb.ipod,
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
    sb.ok(&["adopt"]);
    sb
}

// Prevents `doctor --fix` removing most of the database because the music
// folder looked empty: refused before any prompt, backup or write.
#[test]
fn many_missing_entries_are_refused_until_allowed() {
    let sb = adopted_with_extras(8);
    sb.give_files(4); // 3 managed + Extra0 have files; Extra1..7 do not
    let before = sb.tree(&sb.ipod);
    let mappings_before = sb.mappings();
    let o = sb.run(&["doctor", "--fix", "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--allow-mass-removal"), "{}", err(&o));
    assert!(!out(&o).contains("removed"), "{}", out(&o));
    assert_eq!(sb.tree(&sb.ipod), before, "iTunesDB and files untouched");
    assert!(!sb.backups_dir().exists(), "no backup, so no write began");
    assert_eq!(sb.mappings(), mappings_before);
    let db = gpod::Database::open(&sb.ipod).unwrap();
    assert!(
        (1..8).all(|k| db.track_by_dbid(100 + k).is_some()),
        "no entry was removed"
    );
    drop(db);

    let o = sb.ok(&["doctor", "--fix", "--yes", "--allow-mass-removal"]);
    assert!(
        out(&o).contains("removed 7 database entries"),
        "{}",
        out(&o)
    );
    let db = gpod::Database::open(&sb.ipod).unwrap();
    assert!((1..8).all(|k| db.track_by_dbid(100 + k).is_none()));
    assert!(db.track_by_dbid(100).is_some(), "the one with a file stays");
    assert!(db.track_by_dbid(2).is_some() && db.track_by_dbid(3).is_some());
    drop(db);
    assert_eq!(sb.stamps().len(), 1, "a backup was taken");
    assert_eq!(sb.mappings().0, 3, "managed tracks keep their mappings");
}

// Five missing entries is the largest number that passes without the flag.
#[test]
fn five_missing_entries_pass_without_the_flag() {
    let sb = adopted_with_extras(8);
    sb.give_files(6); // Extra0..2 have files; Extra3..7 (5 entries) do not
    let o = sb.ok(&["doctor", "--fix", "--yes"]);
    assert!(
        out(&o).contains("removed 5 database entries"),
        "{}",
        out(&o)
    );
}

#[test]
fn allow_mass_removal_without_fix_is_a_usage_error() {
    let sb = adopted();
    let o = sb.run(&["doctor", "--allow-mass-removal"]);
    assert_code(&o, 2);
}

impl Sandbox {
    /// `(track mappings, playlist mappings as (name, ipod id))` of the iPod.
    fn mappings(&self) -> (usize, Vec<(String, Option<u64>)>) {
        let store = Store::open(&self.base().join("data/iprune/iprune.sqlite")).unwrap();
        let conn = store.conn();
        let dev = iprune_core::store::devices::device_id(conn, "0x000A27001234ABCD")
            .unwrap()
            .unwrap();
        let t = iprune_core::store::devices::track_mappings(conn, dev)
            .unwrap()
            .len();
        let mut p: Vec<(String, Option<u64>)> =
            iprune_core::store::devices::playlist_mappings(conn, dev)
                .unwrap()
                .into_iter()
                .map(|m| (m.ipod_name, m.ipod_id))
                .collect();
        p.sort();
        (t, p)
    }
}

#[test]
fn sync_takes_back_tracks_a_crash_left_unrecorded_and_says_so() {
    let sb = adopted();
    let four = sb.music.join("four.mp3");
    fs::copy(fixture(Kind::Mp3, 4), &four).unwrap();
    sb.ok(&["sync", "--yes"]);
    assert_eq!(sb.music_files().len(), 1);
    // The crash: the index forgot the track the iPod now holds.
    {
        let store = Store::open(&sb.base().join("data/iprune/iprune.sqlite")).unwrap();
        let n = store
            .conn()
            .execute(
                "DELETE FROM device_tracks WHERE track_id = (SELECT id FROM tracks WHERE path = ?1)",
                [four.to_str().unwrap()],
            )
            .unwrap();
        assert_eq!(n, 1);
    }
    let o = sb.ok(&["sync", "--yes"]);
    assert!(
        err(&o).contains("took back the mappings of 1 tracks"),
        "{}",
        err(&o)
    );
    assert!(out(&o).contains("Nothing to apply"), "{}", out(&o));
    assert_eq!(sb.music_files().len(), 1, "not copied a second time");
    assert_eq!(sb.mappings().0, 4);
}

// ---- desktop notifications (M7)

impl Sandbox {
    /// What the fake `notify-send` was given, one argument per line. `send`
    /// does not wait, so this polls for `want` lines for a moment; with 0 it
    /// waits a while to catch a notification that comes late.
    fn notifications(&self, want: usize) -> Vec<String> {
        let log = self.base().join("notify.log");
        let mut lines = Vec::new();
        for _ in 0..if want == 0 { 40 } else { 200 } {
            lines = fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect();
            if want > 0 && lines.len() >= want {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        lines
    }
}

/// The arguments of the notification: `--` then summary and body.
fn text_of(lines: &[String]) -> &[String] {
    let dash = lines
        .iter()
        .position(|l| l == "--")
        .expect("-- before text");
    &lines[dash + 1..]
}

#[test]
fn a_sync_that_applies_sends_one_normal_notification_and_an_empty_one_sends_none() {
    let sb = adopted();
    sb.reverse_road();
    sb.ok(&["sync", "--yes"]);
    let lines = sb.notifications(7);
    assert!(lines.contains(&"--urgency=normal".to_owned()), "{lines:?}");
    let text = text_of(&lines);
    assert_eq!(text.len(), 2, "one summary and one body: {lines:?}");
    assert_eq!(text[0], "iPod synced");
    assert!(text[1].contains("1 playlist change"), "{}", text[1]);

    // Nothing left to apply: no second notification.
    let again = sb.ok(&["sync", "--yes"]);
    assert!(out(&again).contains("Nothing to apply"), "{}", out(&again));
    assert_eq!(sb.notifications(0), lines, "no new notification");
}

#[test]
fn plan_only_a_refused_plan_and_a_busy_ipod_send_no_notification() {
    let sb = adopted();
    sb.reverse_road();
    sb.ok(&["sync", "--plan-only"]);
    // No terminal and no --yes: the plan is refused.
    assert_code(&sb.run(&["sync"]), 1);
    let lock = sb.hold_ipod_lock();
    assert_code(&sb.run(&["sync", "--yes"]), 1);
    drop(lock);
    assert_eq!(sb.notifications(0), Vec::<String>::new());
    assert!(!sb.base().join("notify.log").exists());
}

#[test]
fn a_sync_that_fails_while_applying_sends_a_critical_notification() {
    let sb = adopted();
    sb.reverse_road();
    // The database cannot be written: libgpod fails after applying began.
    let dir = sb.ipod.join("iPod_Control/iTunes");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let o = sb.run(&["sync", "--yes"]);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_code(&o, 1);
    let lines = sb.notifications(7);
    assert!(
        lines.contains(&"--urgency=critical".to_owned()),
        "{lines:?}"
    );
    let text = text_of(&lines);
    assert_eq!(text.len(), 2, "{lines:?}");
    assert_eq!(text[0], "iPod sync failed");
    assert!(!text[1].is_empty());
}
