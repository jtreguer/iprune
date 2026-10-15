//! End to end tests of `iprune sync`. Every run gets its own HOME and XDG
//! folders in a temp dir and a closed stdin. The "iPod" is a fake tree in a
//! temp dir whose iTunesDB is written by libgpod (`make_ipod.c`), and the
//! config always points `device.mountpoint` at it, so mount detection never
//! looks at the machine's real mounts. A fake `notify-send` first on the
//! PATH logs what a sync would show on the desktop, to `notify.log`, and a
//! fake `systemd-inhibit` logs the inhibitor a write would take, to
//! `inhibit.log`, without taking one.

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
        // A systemd-inhibit that takes no inhibitor: it logs its options
        // and runs the command it is given.
        let inhibit = base.join("bin/systemd-inhibit");
        fs::write(
            &inhibit,
            format!(
                "#!/bin/sh\nwhile [ \"${{1#--}}\" != \"$1\" ]; do echo \"$1\" >> '{log}'; shift; done\n\
                 # Signals for iprune (our parent), one per line in signals.once, sent once.\n\
                 if [ -f '{sig}' ]; then\n  me=$PPID; list=$(cat '{sig}'); rm -f '{sig}'\n  # Detached: iprune ends this script when it drops the inhibitor.\n  ( for s in $list; do kill -$s $me; sleep 0.05; done ) >/dev/null 2>&1 &\nfi\n\
                 # Not exec: a Ctrl+C or hang-up that reached us would be logged, then end cat.\n\
                 trap 'echo got >> '{caught}'' INT HUP\n\"$@\"\n",
                log = base.join("inhibit.log").display(),
                caught = base.join("inhibit.caught").display(),
                sig = base.join("signals.once").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&inhibit, fs::Permissions::from_mode(0o755)).unwrap();
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

    fn command(&self, args: &[&str]) -> Command {
        let base = self.base();
        let path = format!(
            "{}:{}",
            base.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut c = Command::new(env!("CARGO_BIN_EXE_iprune"));
        c.args(args)
            .env("HOME", base.join("home"))
            .env("XDG_CONFIG_HOME", base.join("config"))
            .env("XDG_DATA_HOME", base.join("data"))
            .env("PATH", path)
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
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
        fs::write(
            cfg,
            text.replace(empty, &format!("mountpoint = {mp:?}\nallow_folder = true")),
        )
        .unwrap();
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
                    // Hidden: the `.ithmb` store, partial backups.
                    .filter(|n| !n.starts_with('.'))
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
    // The fake iPod holds a file for each of its three entries.
    let files_before = sb.music_files();
    assert_eq!(files_before.len(), 3, "set up: {files_before:?}");
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

    // One new audio file on the iPod, with the library file's bytes, and the
    // track in the playlist after the two that were there.
    let files: Vec<PathBuf> = sb
        .music_files()
        .into_iter()
        .filter(|f| !files_before.contains(f))
        .collect();
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
    assert_eq!(sb.music_files().len(), 4);
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
    // The stamp is followed by what the backup holds, from its snapshot:
    // the iPod's name and its counts (three tracks, the master list and Road).
    let line = lines
        .iter()
        .find(|l| l.starts_with(&stamps[0]))
        .unwrap_or_else(|| panic!("no line for the backup: {lines:?}"));
    let what = line[stamps[0].len()..].trim();
    assert!(
        what.contains("iPod de Test") && what.contains("3 tracks") && what.contains("2 playlists"),
        "{line:?}"
    );
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

// ---- restore: --guid, the copy of what it replaces, an unfinished restore

impl Sandbox {
    /// `adopted()`, one sync (Road reversed) and its backup's stamp.
    fn synced(&self) -> String {
        self.reverse_road();
        self.ok(&["sync", "--yes"]);
        self.stamps().remove(0)
    }
}

// Prevents (audit M1) a restore with a backup folder named for another iPod
// than the one plugged in.
#[test]
fn restore_with_a_guid_that_is_not_the_connected_ipods_is_refused_and_changes_nothing() {
    let sb = adopted();
    let stamp = sb.synced();
    let ipod_before = sb.tree(&sb.ipod);
    let backups_before = sb.tree(&sb.backups_dir());

    let o = sb.run(&["restore", &stamp, "--guid", "0x000A27009999AAAA", "--yes"]);
    assert_code(&o, 1);
    let text = err(&o);
    assert!(
        text.contains("9999AAAA") && text.contains("000A27001234ABCD"),
        "both GUIDs are named: {text}"
    );
    assert_eq!(sb.tree(&sb.ipod), ipod_before);
    assert_eq!(sb.tree(&sb.backups_dir()), backups_before, "no new backup");

    // The same GUID written another way is the same iPod.
    let o = sb.ok(&["restore", &stamp, "--guid", "000a27001234abcd", "--yes"]);
    assert!(out(&o).contains("restored iTunesDB"), "{}", out(&o));
}

// Prevents (audit M1) an iPod without a readable GUID being given the only
// backup folder there is.
#[test]
fn restore_onto_an_ipod_without_a_readable_guid_needs_the_folder_named() {
    let sb = adopted();
    let stamp = sb.synced();
    fs::remove_file(sb.ipod.join("iPod_Control/Device/SysInfo")).unwrap();
    let ipod_before = sb.tree(&sb.ipod);
    let backups_before = sb.tree(&sb.backups_dir());

    let o = sb.run(&["restore", &stamp, "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--guid"), "{}", err(&o));
    assert_eq!(sb.tree(&sb.ipod), ipod_before);
    assert_eq!(sb.tree(&sb.backups_dir()), backups_before);

    // `iprune backups` names the folder.
    let listed = out(&sb.ok(&["backups"]));
    assert!(listed.contains(GUID_DIR), "{listed}");
    let o = sb.ok(&["restore", &stamp, "--guid", GUID_DIR, "--yes"]);
    assert!(out(&o).contains("restored iTunesDB"), "{}", out(&o));
    assert_ne!(sb.tree(&sb.ipod), ipod_before);
}

#[test]
fn restore_says_where_it_saved_what_it_replaced() {
    let sb = adopted();
    let stamp = sb.synced();
    let after_sync = sb.itunesdb();
    let o = sb.ok(&["restore", &stamp, "--yes"]);
    let stamps = sb.stamps();
    assert_eq!(stamps.len(), 2, "{stamps:?}");
    let saved = stamps.iter().find(|s| **s != stamp).unwrap();
    assert!(out(&o).contains(saved.as_str()), "{}", out(&o));
    assert_eq!(
        fs::read(sb.backups_dir().join(GUID_DIR).join(saved).join("iTunesDB")).unwrap(),
        after_sync,
        "the replaced database is that backup"
    );
}

// Prevents (audit H2) plays that cannot be pulled being dropped without the
// flag that says so.
#[test]
fn restore_over_a_database_that_cannot_be_read_keeps_play_counts_unless_told_to_discard_them() {
    let sb = adopted();
    let stamp = sb.synced();
    let db = sb.ipod.join("iPod_Control/iTunes/iTunesDB");
    let counts = sb.ipod.join("iPod_Control/iTunes/Play Counts");
    fs::write(&db, b"damaged").unwrap();
    fs::write(&counts, b"some plays").unwrap();
    let ipod_before = sb.tree(&sb.ipod);

    let o = sb.run(&["restore", &stamp, "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--discard-play-counts"), "{}", err(&o));
    assert_eq!(sb.tree(&sb.ipod), ipod_before);
    assert_eq!(sb.stamps().len(), 1, "nothing was saved: nothing changed");

    sb.ok(&["restore", &stamp, "--discard-play-counts", "--yes"]);
    assert!(!counts.exists());
    assert!(gpod::Database::open(&sb.ipod).is_ok());
    let saved = sb.stamps().pop().unwrap();
    assert_eq!(
        fs::read(
            sb.backups_dir()
                .join(GUID_DIR)
                .join(saved)
                .join("Play Counts")
        )
        .unwrap(),
        b"some plays"
    );
}

// Prevents (audit M11) a sync or a repair running on an iPod whose restore
// stopped part way.
#[test]
fn after_a_restore_that_stopped_sync_and_doctor_fix_refuse_until_it_is_run_again() {
    let sb = adopted();
    // An ArtworkDB, so the backup holds one and the restore renames two files.
    let art = sb.ipod.join("iPod_Control/Artwork/ArtworkDB");
    fs::create_dir_all(art.parent().unwrap()).unwrap();
    fs::write(&art, "art").unwrap();
    let stamp = sb.synced();
    // The artwork file cannot be replaced: a folder stands in its place.
    fs::remove_file(&art).unwrap();
    fs::create_dir(&art).unwrap();
    fs::write(art.join("inside"), "x").unwrap();

    let o = sb.run(&["restore", &stamp, "--yes"]);
    assert_code(&o, 1);
    let stopped_ipod = sb.tree(&sb.ipod);
    let stopped_backups = sb.tree(&sb.backups_dir());
    for args in [
        &["sync", "--yes"][..],
        &["sync", "--plan-only"],
        &["doctor", "--fix", "--yes"],
    ] {
        let o = sb.run(args);
        assert_code(&o, 1);
        assert!(
            err(&o).contains(&stamp) && err(&o).contains("restore"),
            "{args:?}: {}",
            err(&o)
        );
        assert_eq!(sb.tree(&sb.ipod), stopped_ipod, "{args:?}");
        assert_eq!(sb.tree(&sb.backups_dir()), stopped_backups, "{args:?}");
    }

    // Run again, once what stopped it is gone.
    fs::remove_dir_all(&art).unwrap();
    sb.ok(&["restore", &stamp, "--yes"]);
    assert_eq!(fs::read_to_string(&art).unwrap(), "art");
    sb.ok(&["sync", "--yes"]);
    sb.ok(&["doctor", "--fix", "--yes"]);
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
    /// Deletes the file of the k-th track of the fake iPod for each k given,
    /// so those entries are missing their file. `fake_ipod` gives every
    /// entry a file (make_ipod.c names the k-th track F(k)/T000k.mp3).
    fn lose_files(&self, ks: impl IntoIterator<Item = usize>) {
        for k in ks {
            let f = self
                .ipod
                .join(format!("iPod_Control/Music/F{k:02}/T{k:04}.mp3"));
            fs::remove_file(&f).unwrap();
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
    let sb = adopted(); // every entry has its file
    let before = sb.tree(&sb.ipod);
    let o = sb.ok(&["doctor"]);
    assert!(out(&o).contains("nothing to repair"), "{}", out(&o));
    assert_eq!(sb.tree(&sb.ipod), before);
}

#[test]
fn doctor_without_fix_lists_the_problems_and_changes_nothing() {
    let sb = adopted();
    sb.lose_files([2]); // the third entry has no file
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
    sb.lose_files([2]);
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
    sb.lose_files([2]); // track 3 (three.wav) has no file
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
    sb.lose_files([2]); // track 3 has no file: a sync runs, then the orphans go
    let orphans = sb.orphans(2);
    let o = sb.ok(&["doctor", "--fix", "--yes"]);
    assert!(out(&o).contains("deleted 2 orphan files"), "{}", out(&o));
    assert!(orphans.iter().all(|f| !f.exists()));
    drop(sb.hold_ipod_lock());
}

#[test]
fn doctor_fix_will_not_delete_orphans_while_the_ipod_lock_is_held() {
    let sb = adopted();
    // nothing missing: only orphans to delete
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
    sb.lose_files([2]); // track 3 has no file: a database repair is due
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
/// its place at F(k)/T000k.mp3, where `fake_ipod` puts its file.
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
    sb.lose_files(4..11); // Extra1..7 have no file; 3 managed + Extra0 do
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
    sb.lose_files(6..11); // Extra3..7 (5 entries) have no file; Extra0..2 do
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
    assert_eq!(sb.music_files().len(), 4);
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
    assert_eq!(sb.music_files().len(), 4, "not copied a second time");
    assert_eq!(sb.mappings().0, 4);
}

// ---- the guard on copying many tracks again for a missing file

/// Eight library files and an iPod holding all of them, with their files.
/// Adopted.
fn adopted_eight() -> Sandbox {
    let sb = Sandbox::new();
    let names: Vec<String> = (1..=8).map(|i| format!("s{i}.mp3")).collect();
    let files: Vec<(&str, Kind, u32)> = names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), Kind::Mp3, i as u32 + 1))
        .collect();
    sb.library(&files);
    let tracks: Vec<IpodSpec> = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let title: &'static str = Box::leak(format!("Title{}", i + 1).into_boxed_str());
            let artist: &'static str = Box::leak(format!("Artist{}", i + 1).into_boxed_str());
            sb.spec(n, 100 + i as u64, title, artist)
        })
        .collect();
    fake_ipod(
        &sb.ipod,
        Some(GUID_SYSINFO),
        &tracks,
        &[ipod_playlist("Road", &[100, 101])],
    );
    fs::write(
        sb.lists.join("Road.m3u8"),
        format!(
            "{}\n{}\n",
            sb.music.join("s1.mp3").display(),
            sb.music.join("s2.mp3").display()
        ),
    )
    .unwrap();
    sb.ok(&["adopt"]);
    assert_eq!(sb.mappings().0, 8, "set up: all adopted");
    sb
}

#[test]
fn many_files_missing_from_the_ipod_are_refused_with_exit_1_until_allowed() {
    let sb = adopted_eight();
    sb.lose_files(1..7); // six entries
    let ipod_before = sb.tree(&sb.ipod);
    let o = sb.run(&["sync", "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--allow-mass-removal"), "{}", err(&o));
    assert!(err(&o).contains("6 of 8"), "{}", err(&o));
    // Refused before the prompt: without --yes the reason is still this one.
    let o = sb.run(&["sync"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--allow-mass-removal"), "{}", err(&o));
    assert_eq!(sb.tree(&sb.ipod), ipod_before, "the iPod is untouched");
    assert!(!sb.backups_dir().exists(), "no backup, so no write began");

    let o = sb.ok(&["sync", "--yes", "--allow-mass-removal"]);
    assert!(out(&o).contains("copied 6 again"), "{}", out(&o));
    let db = gpod::Database::open(&sb.ipod).unwrap();
    let contents = iprune_core::ipod::read(&db);
    assert_eq!(contents.tracks.len(), 8);
    for t in &contents.tracks {
        let f = iprune_core::ipod::file_path(&sb.ipod, t.ipod_path.as_deref().unwrap()).unwrap();
        assert!(f.is_file(), "{:x} has no file", t.adopt.dbid);
    }
    drop(db);
    let again = sb.ok(&["sync", "--yes"]);
    assert!(out(&again).contains("Nothing to apply"), "{}", out(&again));
}

#[test]
fn five_files_missing_from_the_ipod_are_copied_again_without_the_flag() {
    let sb = adopted_eight();
    sb.lose_files(1..6); // five entries
    let o = sb.ok(&["sync", "--yes"]);
    assert!(out(&o).contains("copied 5 again"), "{}", out(&o));
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

// ---- audit step 3: a prompt that waited (H1), damaged paths (L2), folders (M6)

impl Sandbox {
    /// Runs `iprune args` on a terminal (a pty made by `script`, so the
    /// confirmation prompt is asked), waits for the `[y/N]` prompt, calls
    /// `during` while it waits and answers `y`. Returns the exit code and
    /// what appeared on the terminal.
    fn run_on_terminal(&self, args: &[&str], during: impl FnOnce()) -> (Option<i32>, String) {
        self.run_on_terminal_answering(args, "y", during)
    }

    /// [`run_on_terminal`] answering `answer`. The answer is written without
    /// checking: `during` may have ended the process.
    fn run_on_terminal_answering(
        &self,
        args: &[&str],
        answer: &str,
        during: impl FnOnce(),
    ) -> (Option<i32>, String) {
        use std::io::{Read, Write};
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let base = self.base();
        let path = format!(
            "{}:{}",
            base.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let cmd = format!("'{}' {}", env!("CARGO_BIN_EXE_iprune"), args.join(" "));
        let mut child = Command::new("script")
            .args(["-qec", &cmd, "/dev/null"])
            .env("HOME", base.join("home"))
            .env("XDG_CONFIG_HOME", base.join("config"))
            .env("XDG_DATA_HOME", base.join("data"))
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("these tests need `script` (util-linux) to give iprune a terminal");
        let seen = Arc::new(Mutex::new(String::new()));
        let reader = {
            let (seen, mut stdout) = (seen.clone(), child.stdout.take().unwrap());
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = stdout.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    seen.lock()
                        .unwrap()
                        .push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            })
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        while !seen.lock().unwrap().contains("[y/N]") {
            assert!(
                Instant::now() < deadline && child.try_wait().unwrap().is_none(),
                "no prompt: {}",
                seen.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        during();
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(format!("{answer}\n").as_bytes());
        let _ = stdin.flush();
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            assert!(Instant::now() < deadline, "no exit");
            std::thread::sleep(Duration::from_millis(20));
        };
        drop(stdin);
        reader.join().unwrap();
        let text = seen.lock().unwrap().clone();
        (status.code(), text)
    }

    /// A Play Counts with no entry yet: what the iPod holds after a play.
    fn add_play_counts(&self) {
        let mut b = b"mhdp".to_vec();
        b.extend(96u32.to_le_bytes());
        b.extend(28u32.to_le_bytes());
        b.resize(96, 0);
        fs::write(self.ipod.join("iPod_Control/iTunes/Play Counts"), b).unwrap();
    }
}

// Prevents (audit H1) `sync` writing a plan made before the prompt over an
// iPod that was played, swapped or rewritten while the prompt waited.
#[test]
fn sync_refuses_an_ipod_that_changed_while_its_prompt_waited() {
    let sb = adopted();
    sb.reverse_road();
    let mut during = BTreeMap::new();
    let (code, text) = sb.run_on_terminal(&["sync"], || {
        sb.add_play_counts();
        during = sb.tree(&sb.ipod);
    });
    assert_eq!(code, Some(1), "{text}");
    assert!(text.contains("changed"), "{text}");
    assert!(
        text.contains(&sb.ipod.display().to_string()),
        "the prompt names the iPod: {text}"
    );
    assert_eq!(sb.tree(&sb.ipod), during, "the iPod was written");
    assert!(sb.stamps().is_empty(), "no backup, so no write began");
    assert_eq!(sb.ipod_playlist("Road"), Some(vec![u64::MAX - 1, 2]));

    // Without a change during the wait the same command applies the edit.
    let (code, text) = sb.run_on_terminal(&["sync"], || {});
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(sb.ipod_playlist("Road"), Some(vec![2, u64::MAX - 1]));
}

// Prevents (audit H1) `doctor --fix` repairing from a report of an iPod that
// is no longer the one it read, on either path: the database repair and the
// orphans-only one.
#[test]
fn doctor_fix_refuses_an_ipod_that_changed_while_its_prompt_waited() {
    for lose_an_entry in [true, false] {
        let sb = adopted();
        if lose_an_entry {
            sb.lose_files([2]);
        }
        let orphans = sb.orphans(1);
        let mappings = sb.mappings();
        let mut during = BTreeMap::new();
        let (code, text) = sb.run_on_terminal(&["doctor", "--fix"], || {
            sb.add_play_counts();
            during = sb.tree(&sb.ipod);
        });
        let ctx = format!("entry lost: {lose_an_entry}: {text}");
        assert_eq!(code, Some(1), "{ctx}");
        assert!(text.contains("changed"), "{ctx}");
        assert!(
            text.contains(&sb.ipod.display().to_string()),
            "the prompt names the iPod: {ctx}"
        );
        assert_eq!(sb.tree(&sb.ipod), during, "{ctx}: the iPod was written");
        assert!(orphans[0].exists(), "{ctx}: an orphan was deleted");
        assert!(!sb.backups_dir().exists(), "{ctx}: a backup was made");
        assert_eq!(sb.mappings(), mappings, "{ctx}");
    }
}

#[test]
fn doctor_fix_on_a_terminal_repairs_when_nothing_changed_during_the_prompt() {
    let sb = adopted();
    sb.lose_files([2]);
    let orphans = sb.orphans(1);
    let (code, text) = sb.run_on_terminal(&["doctor", "--fix"], || {});
    assert_eq!(code, Some(0), "{text}");
    assert!(
        text.contains(&sb.ipod.display().to_string()),
        "the prompt names the iPod: {text}"
    );
    assert!(!orphans[0].exists());
    assert!(
        gpod::Database::open(&sb.ipod)
            .unwrap()
            .track_by_dbid(3)
            .is_none()
    );
}

/// The adopted world with track 3's path made to leave the iPod, and the
/// file it names outside it.
fn with_path_out_of_the_ipod() -> (Sandbox, PathBuf) {
    let sb = adopted();
    let outside = sb.base().join("outside.mp3");
    fs::write(&outside, b"not the iPod's").unwrap();
    // Four steps up from F00 leave the iPod's folder (its parent is `base`).
    common::set_ipod_path(
        &sb.ipod.join("iPod_Control/iTunes/iTunesDB"),
        2,
        ":iPod_Control:Music:F00:..:..:..:..:outside.mp3",
    );
    (sb, outside)
}

// Prevents (audit L2) `doctor --fix` treating an entry whose path leaves the
// iPod as an entry to remove, deleting the file it names.
#[test]
fn doctor_lists_an_entry_whose_path_leaves_the_ipod_and_fix_neither_removes_it_nor_its_target() {
    let (sb, outside) = with_path_out_of_the_ipod();
    let before = sb.tree(&sb.ipod);
    let o = sb.ok(&["doctor"]);
    let text = out(&o);
    assert!(text.contains("outside.mp3"), "{text}");
    assert!(text.contains(&format!("{:016x}", 3)), "{text}");
    assert!(!text.contains("nothing to repair"), "{text}");
    assert_eq!(sb.tree(&sb.ipod), before);

    let orphans = sb.orphans(1);
    let o = sb.ok(&["doctor", "--fix", "--yes"]);
    assert!(!orphans[0].exists(), "the real orphan is still repaired");
    assert!(
        gpod::Database::open(&sb.ipod)
            .unwrap()
            .track_by_dbid(3)
            .is_some(),
        "the entry stays"
    );
    assert_eq!(fs::read(&outside).unwrap(), b"not the iPod's");
    assert!(!out(&o).contains("removed"), "{}", out(&o));
    // And it is still reported.
    assert!(out(&sb.ok(&["doctor"])).contains("outside.mp3"));
}

// ---- M6

impl Sandbox {
    fn forbid_folders(&self) {
        self.edit_config("allow_folder = true", "allow_folder = false");
    }
}

// Prevents (audit M6) a configured folder, such as a copy of the iPod or the
// backup, being written as if it were the iPod.
#[test]
fn sync_to_a_folder_is_refused_unless_the_config_allows_folders() {
    let sb = adopted();
    sb.reverse_road();
    sb.forbid_folders();
    let before = sb.tree(&sb.ipod);
    let o = sb.run(&["sync", "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("folder"), "{}", err(&o));
    assert_eq!(sb.tree(&sb.ipod), before);
    assert!(sb.stamps().is_empty());

    sb.edit_config("allow_folder = false", "allow_folder = true");
    sb.ok(&["sync", "--yes"]);
    assert_eq!(sb.ipod_playlist("Road"), Some(vec![2, u64::MAX - 1]));
}

// With only orphans to delete no sync runs, so the folder check in `doctor`
// itself is what stops the deletion.
#[test]
fn doctor_fix_to_a_folder_is_refused_unless_the_config_allows_folders() {
    for lose_an_entry in [true, false] {
        let sb = adopted();
        if lose_an_entry {
            sb.lose_files([2]);
        }
        let orphans = sb.orphans(1);
        sb.forbid_folders();
        let before = sb.tree(&sb.ipod);
        let o = sb.run(&["doctor", "--fix", "--yes"]);
        assert_code(&o, 1);
        assert!(err(&o).contains("folder"), "{}", err(&o));
        assert_eq!(sb.tree(&sb.ipod), before, "entry lost: {lose_an_entry}");
        assert!(orphans[0].exists());
        assert!(!sb.backups_dir().exists());
        // Reading is not writing: the report still works.
        let o = sb.ok(&["doctor"]);
        assert!(out(&o).contains("doctor --fix"), "{}", out(&o));

        sb.edit_config("allow_folder = false", "allow_folder = true");
        sb.ok(&["doctor", "--fix", "--yes"]);
        assert!(!orphans[0].exists());
    }
}

#[test]
fn restore_to_a_folder_is_refused_unless_the_config_allows_folders() {
    let sb = adopted();
    let stamp = sb.synced();
    sb.forbid_folders();
    let ipod_before = sb.tree(&sb.ipod);
    let backups_before = sb.tree(&sb.backups_dir());
    let o = sb.run(&["restore", &stamp, "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("folder"), "{}", err(&o));
    assert_eq!(sb.tree(&sb.ipod), ipod_before);
    assert_eq!(sb.tree(&sb.backups_dir()), backups_before);

    sb.edit_config("allow_folder = false", "allow_folder = true");
    sb.ok(&["restore", &stamp, "--yes"]);
    assert_eq!(sb.ipod_playlist("Road"), Some(vec![u64::MAX - 1, 2]));
}

#[test]
fn a_relative_mountpoint_in_the_config_stops_every_command_that_loads_it() {
    let sb = adopted();
    let mp = format!("mountpoint = {:?}", sb.ipod);
    sb.edit_config(&mp, "mountpoint = \"relative/ipod\"");
    let before = sb.tree(&sb.ipod);
    for args in [&["doctor"][..], &["sync", "--plan-only"], &["scan"]] {
        let o = sb.run(args);
        assert_code(&o, 1);
        assert!(
            err(&o).contains("device.mountpoint"),
            "{args:?}: {}",
            err(&o)
        );
    }
    assert_eq!(sb.tree(&sb.ipod), before);
}

// Prevents (review of step 3) a restore of one iPod's backup onto another iPod
// swapped in at the same mountpoint before the command.
#[test]
fn restore_after_the_ipod_was_swapped_for_another_is_refused_and_changes_nothing() {
    let sb = adopted();
    let stamp = sb.synced();
    fs::write(
        sb.ipod.join("iPod_Control/Device/SysInfo"),
        "FirewireGuid: 0x000A2700BBBBBBBB\n",
    )
    .unwrap();
    let ipod_before = sb.tree(&sb.ipod);
    let backups_before = sb.tree(&sb.backups_dir());
    for args in [
        vec!["restore", &stamp, "--yes"],
        vec!["restore", &stamp, "--yes", "--guid", "0x000A27001234ABCD"],
    ] {
        let o = sb.run(&args);
        assert_code(&o, 1);
        assert_eq!(sb.tree(&sb.ipod), ipod_before, "{args:?}");
        assert_eq!(sb.tree(&sb.backups_dir()), backups_before, "{args:?}");
    }
}

// Prevents (review of step 3) `doctor --fix` asking to repair what it never
// repairs: with only a damaged path there is nothing to ask about.
#[test]
fn doctor_fix_with_only_a_damaged_path_does_not_ask_and_changes_nothing() {
    let (sb, outside) = with_path_out_of_the_ipod();
    // Track 3's own old file is now an orphan the fix would delete: remove
    // it, so that only the damaged path is left to report.
    fs::remove_file(sb.ipod.join("iPod_Control/Music/F02/T0002.mp3")).unwrap();
    let listed = out(&sb.ok(&["doctor"]));
    assert!(listed.contains("outside.mp3"), "{listed}");
    assert!(!listed.contains("not in its database"), "{listed}");
    let before = sb.tree(&sb.ipod);
    // No --yes and no terminal: a question would be an error (code 1).
    let o = sb.run(&["doctor", "--fix"]);
    assert_code(&o, 0);
    assert!(
        out(&o).contains("nothing to repair automatically"),
        "{}",
        out(&o)
    );
    assert_eq!(sb.tree(&sb.ipod), before);
    assert!(!sb.backups_dir().exists());
    assert_eq!(fs::read(&outside).unwrap(), b"not the iPod's");
}

// ---- audit step 8: signals and the inhibitor (M7, L4, L43)

impl Sandbox {
    /// The options the fake systemd-inhibit was run with, one per line, for
    /// every inhibitor taken; empty when none was.
    fn inhibit_log(&self) -> Vec<String> {
        fs::read_to_string(self.base().join("inhibit.log"))
            .map(|t| t.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// Makes the fake systemd-inhibit send these signals to iprune (its
    /// parent) the next time it is run, at the moment the inhibitor is taken:
    /// after the confirmation, before the first copy. Deterministic.
    fn signal_when_inhibited(&self, signals: &[&str]) {
        fs::write(self.base().join("signals.once"), signals.join("\n")).unwrap();
    }

    /// The pid of the iprune running with this sandbox's data folder (under
    /// `script`, iprune is not the process spawned).
    fn iprune_pid(&self) -> Option<u32> {
        let exe = Path::new(env!("CARGO_BIN_EXE_iprune"));
        let want = format!("XDG_DATA_HOME={}", self.base().join("data").display());
        for e in fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            let env = fs::read(e.path().join("environ")).unwrap_or_default();
            if fs::read_link(e.path().join("exe")).is_ok_and(|p| p == exe)
                && env.split(|b| *b == 0).any(|v| v == want.as_bytes())
            {
                return Some(pid);
            }
        }
        None
    }
}

fn kill(signal: &str, pid: u32) {
    let st = Command::new("kill")
        .args([&format!("-{signal}"), &pid.to_string()])
        .status()
        .unwrap();
    assert!(st.success(), "kill -{signal} {pid}");
}

/// Waits until the process has ended (gone, or a zombie its parent has yet
/// to reap).
fn wait_gone(pid: u32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let alive = fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
            !s.rsplit(')')
                .next()
                .unwrap_or("")
                .trim_start()
                .starts_with('Z')
        });
        if !alive {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "{pid} did not end");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

const WHAT: &str = "--what=sleep:shutdown:idle:handle-lid-switch";

/// The log of one inhibitor: its four options, the reason non-empty.
fn assert_one_inhibitor(log: &[String], who: &str) {
    assert_eq!(log.len(), 4, "{who}: {log:?}");
    assert_eq!(log[0], WHAT, "{who}");
    assert_eq!(log[1], "--mode=block", "{who}");
    assert_eq!(log[2], "--who=iprune", "{who}");
    let why = log[3].strip_prefix("--why=").unwrap_or_default();
    assert!(!why.is_empty(), "{who}: a reason is given: {log:?}");
}

// Prevents (audit M7) a sync, a restore or a repair running without
// anything to keep the machine from suspending or shutting down.
#[test]
fn sync_doctor_fix_and_restore_each_take_one_blocking_inhibitor() {
    let sb = adopted();
    sb.reverse_road();
    assert!(sb.inhibit_log().is_empty(), "set up");
    let o = sb.ok(&["sync", "--yes"]);
    assert!(!err(&o).contains("could not keep"), "{}", err(&o));
    assert_one_inhibitor(&sb.inhibit_log(), "sync");
    let stamp = sb.stamps().remove(0);

    fs::remove_file(sb.base().join("inhibit.log")).unwrap();
    sb.ok(&["restore", &stamp, "--yes"]);
    assert_one_inhibitor(&sb.inhibit_log(), "restore");

    fs::remove_file(sb.base().join("inhibit.log")).unwrap();
    sb.lose_files([2]);
    sb.ok(&["doctor", "--fix", "--yes"]);
    assert_one_inhibitor(&sb.inhibit_log(), "doctor --fix");
}

// A command that only reads, or that was declined, holds nothing.
#[test]
fn plan_only_doctor_without_fix_and_a_declined_plan_take_no_inhibitor() {
    let sb = adopted();
    sb.reverse_road();
    sb.ok(&["sync", "--plan-only"]);
    sb.ok(&["doctor"]);
    assert!(sb.inhibit_log().is_empty(), "{:?}", sb.inhibit_log());

    let before = sb.tree(&sb.ipod);
    let (code, text) = sb.run_on_terminal_answering(&["sync"], "n", || {});
    assert_eq!(code, Some(4), "{text}");
    assert!(sb.inhibit_log().is_empty(), "{:?}", sb.inhibit_log());
    assert_eq!(sb.tree(&sb.ipod), before, "a declined plan wrote");
    assert!(sb.stamps().is_empty());
}

// Prevents (audit L4) a signal while the prompt waits being ignored (the
// process hanging on, or exiting 0) or, with the handler's state mixed up,
// stopping something that never began. Nothing has been applied: it ends at
// once with 130 and the iPod, the backups and the inhibitors are untouched.
#[test]
fn a_signal_while_the_prompt_waits_exits_130_and_writes_nothing() {
    for signal in ["TERM", "HUP", "INT"] {
        let sb = adopted();
        sb.reverse_road();
        let before = sb.tree(&sb.ipod);
        let mut pid = None;
        let (code, text) = sb.run_on_terminal(&["sync"], || {
            pid = sb.iprune_pid();
            let pid = pid.expect("iprune is running at the prompt");
            kill(signal, pid);
            // The answer must not race the handler thread: wait for the end.
            wait_gone(pid);
        });
        assert_eq!(code, Some(130), "{signal}: {text}");
        assert_eq!(sb.tree(&sb.ipod), before, "{signal}: the iPod was written");
        assert!(sb.stamps().is_empty(), "{signal}: a backup was made");
        assert!(sb.inhibit_log().is_empty(), "{signal}: an inhibitor");
        assert_eq!(sb.ipod_playlist("Road"), Some(vec![u64::MAX - 1, 2]));
        // And the next run is not marked by it.
        sb.ok(&["sync", "--yes"]);
        assert_eq!(sb.ipod_playlist("Road"), Some(vec![2, u64::MAX - 1]));
    }
}

/// Audio files under an iPod's Music folder (one level of F00..F49).
fn count_files(music: &Path) -> usize {
    fs::read_dir(music)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| fs::read_dir(e.path()).map_or(0, Iterator::count))
                .sum()
        })
        .unwrap_or(0)
}

/// New library files in `with_new_files`: enough that the copies take far
/// longer than a signal needs to arrive.
const NEW: usize = 24;

/// The adopted world (3 tracks) plus `NEW` new wav files, all to be copied.
fn with_new_files() -> Sandbox {
    let sb = adopted();
    for i in 0..NEW {
        fs::copy(
            common::fixture_secs(Kind::Wav, 10 + i as u32, 10),
            sb.music.join(format!("new{i:02}.wav")),
        )
        .unwrap();
    }
    sb
}

/// Checks that `o` is the end of a sync that was stopped (as Stop does, not
/// killed) after some of the copies, that what it did is on the iPod,
/// recorded and verified, and that the next sync copies the rest. Returns the
/// number of copies made before the stop.
fn assert_stopped_then_continued(sb: &Sandbox, o: &Output, ctx: &str) -> usize {
    assert_code(o, 1);
    assert!(err(o).contains("stopped early"), "{ctx}: {}", err(o));
    assert!(out(o).contains("verified"), "{ctx}: {}", out(o));
    let done = sb.music_files().len() - 3;
    assert!(done < NEW, "{ctx}: nothing was stopped, all {done} copied");
    assert_eq!(sb.stamps().len(), 1, "{ctx}: a backup, so a checkpoint");
    // Every copy is in the database with its file, and mapped: the next
    // plan holds exactly the rest.
    let db = gpod::Database::open(&sb.ipod).unwrap();
    assert_eq!(db.track_count(), 3 + done, "{ctx}");
    drop(db);
    let plan = sb.ok(&["sync", "--plan-only", "--json"]);
    let plan: serde_json::Value = serde_json::from_str(&out(&plan)).unwrap();
    assert_eq!(
        plan["copy"].as_array().map(Vec::len).unwrap_or(0),
        NEW - done,
        "{ctx}: {plan}"
    );

    let o = sb.ok(&["sync", "--yes"]);
    assert!(
        out(&o).contains(&format!("copied {} tracks", NEW - done)),
        "{ctx}: {}",
        out(&o)
    );
    assert_eq!(
        gpod::Database::open(&sb.ipod).unwrap().track_count(),
        3 + NEW,
        "{ctx}"
    );
    assert_eq!(sb.music_files().len(), 3 + NEW, "{ctx}");
    assert!(out(&sb.ok(&["sync", "--yes"])).contains("Nothing to apply"));
    done
}

// Prevents (audit L4) SIGTERM and SIGHUP killing a sync in the middle of its
// copies, with no checkpoint: they must act as Stop. The fake systemd-inhibit
// sends the signal as the inhibitor is taken, so it lands at the start of the
// copies. Spawning does not wait for the script, so exactly when it lands is
// timing; with 24 copies behind it, it lands before the end.
#[test]
fn sigterm_and_sighup_once_a_sync_applies_stop_it_cleanly_and_the_next_sync_continues() {
    for signal in ["TERM", "HUP"] {
        let sb = with_new_files();
        sb.signal_when_inhibited(&[signal]);
        let o = sb.run(&["sync", "--yes"]);
        assert!(
            !sb.base().join("signals.once").exists(),
            "set up: the fake sent it"
        );
        assert_stopped_then_continued(&sb, &o, signal);
    }
}

// The same stop, with the signal sent from outside once the first copy is on
// the iPod: a stop after at least one copy, so the checkpoint holds copies.
#[test]
fn a_signal_after_the_first_copy_keeps_the_copies_made_and_the_next_sync_finishes() {
    for signal in ["TERM", "HUP"] {
        let sb = with_new_files();
        let before = sb.music_files().len();
        let child = sb
            .command(&["sync", "--yes"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let (music, stop) = (
            sb.ipod.join("iPod_Control/Music"),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        let poller = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if count_files(&music) > before {
                        kill(signal, pid);
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_micros(300));
                }
            })
        };
        let o = child.wait_with_output().unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        poller.join().unwrap();
        let done = assert_stopped_then_continued(&sb, &o, signal);
        assert!(done >= 1, "{signal}: the signal came after a copy");
    }
}

// Prevents (audit L4) a second signal ending the process. It used to exit at
// once; it must keep finishing, and the result is the same as for one.
#[test]
fn a_second_and_third_signal_do_not_end_a_sync_that_is_stopping() {
    let sb = with_new_files();
    sb.signal_when_inhibited(&["TERM", "TERM", "HUP"]);
    let o = sb.run(&["sync", "--yes"]);
    assert!(
        !sb.base().join("signals.once").exists(),
        "set up: the fake sent them all"
    );
    assert!(
        err(&o).contains("still finishing"),
        "the later signals were received: {}",
        err(&o)
    );
    assert_stopped_then_continued(&sb, &o, "three signals");
}

// Prevents (audit L43) a terminal that went away (SIGHUP) making the
// handler, the progress or the final error message panic: once stderr's
// reader is gone every write to it fails (EPIPE here, EIO on a dead tty), and
// the sync must still stop and verify with exit 1, not 101.
#[test]
fn sighup_with_stderr_gone_still_stops_the_sync_cleanly() {
    let sb = with_new_files();
    let before = sb.music_files().len();
    let mut child = sb
        .command(&["sync", "--yes"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (pid, stderr) = (child.id(), child.stderr.take().unwrap());
    let music = sb.ipod.join("iPod_Control/Music");
    let poller = std::thread::spawn(move || {
        // Once the first copy is there: the reader goes, then the hangup.
        for _ in 0..200_000 {
            if count_files(&music) > before {
                drop(stderr);
                kill("HUP", pid);
                return;
            }
            std::thread::sleep(std::time::Duration::from_micros(300));
        }
    });
    let o = child.wait_with_output().unwrap();
    poller.join().unwrap();
    assert_code(&o, 1);
    assert!(out(&o).contains("verified"), "{}", out(&o));
    let done = sb.music_files().len() - 3;
    assert!((1..NEW).contains(&done), "stopped after {done} copies");
    assert_eq!(sb.stamps().len(), 1);
    assert_eq!(
        gpod::Database::open(&sb.ipod).unwrap().track_count(),
        3 + done
    );
}

// Prevents (audit L43) the terminal going away in the middle of the copies
// (the window closed, ssh dropped): the kernel sends SIGHUP and every write to
// the terminal then fails with EIO. The progress line must not panic on it:
// the sync stops after the current file, writes and verifies. The exit code
// is lost with the parent (script), so the outcome is read from the iPod.
#[test]
fn a_terminal_that_vanishes_during_the_copies_does_not_cost_the_checkpoint() {
    use std::io::Write;

    let sb = with_new_files();
    let (mut script, wait_for) = sb.terminal_session("sync");
    wait_for("[y/N]");
    let pid = sb.iprune_pid().expect("iprune is running at the prompt");
    script.stdin.as_mut().unwrap().write_all(b"y\n").unwrap();
    wait_for("copying 2/");
    // The terminal vanishes: the master side closes.
    script.kill().unwrap();
    script.wait().unwrap();
    drop(script.stdin.take());
    wait_gone(pid);

    let done = sb.music_files().len() - 3;
    assert!((1..NEW).contains(&done), "stopped after {done} copies");
    assert_eq!(sb.stamps().len(), 1, "a checkpoint was written");
    let db = gpod::Database::open(&sb.ipod).unwrap();
    assert_eq!(db.track_count(), 3 + done, "the copies are in the database");
    drop(db);
    let o = sb.ok(&["sync", "--yes"]);
    assert!(
        out(&o).contains(&format!("copied {} tracks", NEW - done)),
        "{}",
        out(&o)
    );
    assert_eq!(sb.music_files().len(), 3 + NEW);
}

impl Sandbox {
    /// Starts `iprune <args>` (one string, split by the shell) on a pty made
    /// by `script`. Returns it and a function that waits until the terminal
    /// shows a text.
    fn terminal_session(&self, args: &str) -> (std::process::Child, impl Fn(&str) + use<>) {
        use std::io::Read;
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let cmd = format!("'{}' {args}", env!("CARGO_BIN_EXE_iprune"));
        let base = self.base();
        let mut script = Command::new("script")
            .args(["-qec", &cmd, "/dev/null"])
            .env("HOME", base.join("home"))
            .env("XDG_CONFIG_HOME", base.join("config"))
            .env("XDG_DATA_HOME", base.join("data"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    base.join("bin").display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("this test needs `script` (util-linux) to give iprune a terminal");
        let seen = Arc::new(Mutex::new(String::new()));
        let mut stdout = script.stdout.take().unwrap();
        {
            let seen = seen.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = stdout.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    seen.lock()
                        .unwrap()
                        .push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            });
        }
        let wait_for = move |what: &str| {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !seen.lock().unwrap().contains(what) {
                assert!(
                    Instant::now() < deadline,
                    "no {what:?}: {}",
                    seen.lock().unwrap()
                );
                std::thread::sleep(Duration::from_micros(200));
            }
        };
        (script, wait_for)
    }
}

// Prevents (audit M7) the inhibitor dying with the Ctrl+C or the hang-up that a
// terminal sends to iprune's whole process group, in the middle of the write
// it covers. The fake logs any INT or HUP that reaches it.
#[test]
fn a_ctrl_c_or_hangup_sent_to_the_process_group_does_not_reach_the_inhibitor() {
    for signal in ["INT", "HUP"] {
        let sb = with_new_files();
        let (mut script, wait_for) = sb.terminal_session("sync");
        wait_for("[y/N]");
        let pid = sb.iprune_pid().expect("iprune is running at the prompt");
        std::io::Write::write_all(script.stdin.as_mut().unwrap(), b"y\n").unwrap();
        wait_for("copying 2/");
        // The group, as the terminal's Ctrl+C does.
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let pgid: u32 = stat
            .rsplit(')')
            .next()
            .unwrap()
            .split_whitespace()
            .nth(2)
            .unwrap()
            .parse()
            .unwrap();
        assert!(pgid > 1);
        let st = Command::new("kill")
            .args([&format!("-{signal}"), "--", &format!("-{pgid}")])
            .status()
            .unwrap();
        assert!(st.success());
        wait_gone(pid);
        let _ = script.wait();
        assert!(
            !sb.base().join("inhibit.caught").exists(),
            "{signal}: the inhibitor was in iprune's process group"
        );
        // The signal still stopped the sync, cleanly.
        let done = sb.music_files().len() - 3;
        assert!(done < NEW, "{signal}: nothing was stopped");
        assert_eq!(sb.stamps().len(), 1, "{signal}");
    }
}

// Prevents (audit L43) `doctor --fix` stopping part way (the orphans kept, the
// index not fixed) because a print after a hang-up panicked on a closed stdout.
#[test]
fn doctor_fix_with_stdout_gone_after_a_hangup_still_deletes_orphans_and_fixes_the_index() {
    let sb = adopted();
    sb.lose_files([2]);
    let orphans = sb.orphans(1);
    let (tracks_before, _) = sb.mappings();
    let mut child = sb
        .command(&["doctor", "--fix", "--yes"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let log = sb.base().join("inhibit.log");
    // The inhibitor is taken: the command applies, and holds for a moment
    // before it prints anything more. The terminal goes now.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !log.exists() {
        assert!(std::time::Instant::now() < deadline, "doctor never applied");
        std::thread::sleep(std::time::Duration::from_micros(200));
    }
    drop(stdout);
    kill("HUP", child.id());
    let o = child.wait_with_output().unwrap();
    assert_code(&o, 0);
    assert!(!orphans[0].exists(), "the orphan was kept: {}", err(&o));
    assert_eq!(
        sb.mappings().0,
        tracks_before - 1,
        "the index was not fixed: {}",
        err(&o)
    );
    assert!(
        gpod::Database::open(&sb.ipod)
            .unwrap()
            .track_by_dbid(3)
            .is_none()
    );
}

// The warning, not a refusal: a systemd-inhibit that ends at once (no logind)
// leaves the sync to go ahead, and says so.
#[test]
fn a_systemd_inhibit_that_fails_is_warned_about_and_the_sync_still_applies() {
    let sb = adopted();
    sb.reverse_road();
    let tool = sb.base().join("bin/systemd-inhibit");
    fs::write(&tool, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let o = sb.ok(&["sync", "--yes"]);
    assert!(err(&o).contains("could not keep"), "{}", err(&o));
    assert!(out(&o).contains("verified"), "{}", out(&o));
    assert_eq!(sb.ipod_playlist("Road"), Some(vec![2, u64::MAX - 1]));
}

// restore and doctor --fix also hold, and a signal during them does not end
// them half done: they finish.
#[test]
fn a_signal_during_restore_or_doctor_fix_does_not_end_it_half_done() {
    let sb = adopted();
    let db_before = sb.itunesdb();
    sb.reverse_road();
    sb.ok(&["sync", "--yes"]);
    let stamp = sb.stamps().remove(0);
    sb.signal_when_inhibited(&["TERM", "HUP"]);
    let o = sb.run(&["restore", &stamp, "--yes"]);
    assert_code(&o, 0);
    assert!(out(&o).contains("restored iTunesDB"), "{}", out(&o));
    assert_eq!(sb.itunesdb(), db_before);
    assert!(!sb.base().join("signals.once").exists(), "set up");

    sb.lose_files([2]);
    let orphans = sb.orphans(1);
    sb.signal_when_inhibited(&["TERM", "TERM"]);
    let o = sb.run(&["doctor", "--fix", "--yes"]);
    assert_code(&o, 0);
    assert!(
        !sb.base().join("signals.once").exists(),
        "set up: doctor took the inhibitor, so the fake sent the signals"
    );
    assert!(!orphans[0].exists(), "{}", err(&o));
    assert!(
        gpod::Database::open(&sb.ipod)
            .unwrap()
            .track_by_dbid(3)
            .is_none(),
        "the entry is removed"
    );
}

// ---- a new playlist taking over an iPod playlist iprune never managed
// (audit M9)

/// An adopted iPod with Road managed and "Stray" [three, two] only on the
/// iPod; then a new file Stray.m3u8 holding only one.mp3 appears, which
/// would take Stray over and drop its two tracks.
fn with_takeover() -> Sandbox {
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
        &[
            ipod_playlist("Road", &[u64::MAX - 1, 2]),
            ipod_playlist("Stray", &[3, 2]),
        ],
    );
    let entry = |f: &str| format!("{}\n", sb.music.join(f).display());
    fs::write(
        sb.lists.join("Road.m3u8"),
        entry("one.mp3") + &entry("two.m4a"),
    )
    .unwrap();
    sb.ok(&["adopt"]);
    assert_eq!(sb.ipod_playlist("Stray"), Some(vec![3, 2]), "set up");
    fs::write(sb.lists.join("Stray.m3u8"), entry("one.mp3")).unwrap();
    sb
}

#[test]
fn the_plan_text_names_the_playlist_a_new_file_takes_over() {
    let sb = with_takeover();
    let before = sb.tree(&sb.ipod);
    let o = sb.ok(&["sync", "--plan-only"]);
    let text = out(&o);
    let line = text
        .lines()
        .find(|l| l.contains("takes over"))
        .unwrap_or_else(|| panic!("no takeover line: {text}"));
    assert!(line.contains("Stray") && line.contains('2'), "{line}");
    assert_eq!(sb.tree(&sb.ipod), before);
}

#[test]
fn a_takeover_that_drops_tracks_is_refused_until_allowed() {
    let sb = with_takeover();
    let before = sb.tree(&sb.ipod);
    let o = sb.run(&["sync", "--yes"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--allow-mass-removal"), "{}", err(&o));
    assert!(err(&o).contains("Stray"), "{}", err(&o));
    assert_eq!(sb.tree(&sb.ipod), before, "nothing written");
    assert!(!sb.backups_dir().exists(), "no backup, so no write began");

    sb.ok(&["sync", "--yes", "--allow-mass-removal"]);
    assert_eq!(sb.ipod_playlist("Stray"), Some(vec![u64::MAX - 1]));
    assert_eq!(sb.ipod_playlist("Road"), Some(vec![u64::MAX - 1, 2]));
}

#[test]
fn a_takeover_that_only_adds_tracks_needs_no_flag() {
    let sb = with_takeover();
    let entry = |f: &str| format!("{}\n", sb.music.join(f).display());
    fs::write(
        sb.lists.join("Stray.m3u8"),
        entry("three.wav") + &entry("two.m4a") + &entry("one.mp3"),
    )
    .unwrap();
    sb.ok(&["sync", "--yes"]);
    assert_eq!(sb.ipod_playlist("Stray"), Some(vec![3, 2, u64::MAX - 1]));
}

// Prevents the takeover check blocking doctor's repair, which changes no
// playlist, because the library has a playlist that would take one over.
#[test]
fn doctor_fix_is_not_blocked_by_a_takeover_elsewhere_in_the_plan() {
    let sb = with_takeover();
    sb.lose_files([2]); // three.wav's entry has no file
    let o = sb.ok(&["doctor", "--fix", "--yes"]);
    assert!(
        out(&o).contains("removed 1 database entries"),
        "{}",
        out(&o)
    );
    // Stray was not taken over by the repair.
    assert_eq!(sb.ipod_playlist("Stray"), Some(vec![2]));
    let db = gpod::Database::open(&sb.ipod).unwrap();
    assert!(db.track_by_dbid(3).is_none());
}

// The refusal comes before the question, so nobody confirms a plan that
// then fails.
#[test]
fn a_takeover_that_drops_tracks_is_refused_before_the_prompt() {
    let sb = with_takeover();
    let o = sb.run(&["sync"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--allow-mass-removal"), "{}", err(&o));
    assert!(err(&o).contains("Stray"), "{}", err(&o));
    assert!(!sb.backups_dir().exists());
}
