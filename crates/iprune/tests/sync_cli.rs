//! End to end tests of `iprune sync`. Every run gets its own HOME and XDG
//! folders in a temp dir and a closed stdin. The "iPod" is a fake tree in a
//! temp dir whose iTunesDB is written by libgpod (`make_ipod.c`), and the
//! config always points `device.mountpoint` at it, so mount detection never
//! looks at the machine's real mounts.

#[path = "../../iprune-core/tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::fs;
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

    fn plan_json(&self) -> PathBuf {
        self.base().join("data/iprune/plan.json")
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
fn sync_without_plan_only_is_refused_and_touches_nothing() {
    let sb = adopted();
    let (base_before, ipod_before) = (sb.tree(&sb.base()), sb.tree(&sb.ipod));
    let o = sb.run(&["sync"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--plan-only"), "{}", err(&o));
    assert!(out(&o).is_empty(), "{}", out(&o));
    assert!(!sb.plan_json().exists());
    assert_eq!(sb.tree(&sb.ipod), ipod_before, "the iPod is untouched");
    assert_eq!(sb.tree(&sb.base()), base_before, "nothing at all changed");
}

#[test]
fn sync_without_plan_only_is_refused_before_it_reads_any_config() {
    // No config exists: a refusal must not depend on one, nor create one.
    let sb = Sandbox::new();
    let o = sb.run(&["sync"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("--plan-only"), "{}", err(&o));
    assert!(sb.tree(&sb.base()).is_empty());
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
