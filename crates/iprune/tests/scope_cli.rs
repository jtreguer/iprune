//! `iprune scope` end to end. Every run gets its own HOME and XDG folders in
//! a temp dir and a closed stdin. There is no iPod: the command is pointed
//! at an iPod row of the temp index with `--guid`, so nothing reads a
//! mount and nothing can reach a real device or the real index.

#[path = "../../iprune-core/tests/common/mod.rs"]
mod common;

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use common::{Kind, fixture};
use iprune_core::config::Scope;
use iprune_core::store::{Store, devices};

const GUID: &str = "0x000A27001234ABCD";

struct Sandbox {
    dir: tempfile::TempDir,
    music: PathBuf,
    lists: PathBuf,
}

impl Sandbox {
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

    fn store(&self) -> Store {
        Store::open(&self.base().join("data/iprune/iprune.sqlite")).unwrap()
    }

    fn stored(&self, guid: &str) -> Option<devices::DeviceScope> {
        let store = self.store();
        let id = devices::device_id(store.conn(), guid).unwrap().unwrap();
        devices::scope(store.conn(), id).unwrap()
    }

    fn edit_config(&self, from: &str, to: &str) {
        let cfg = self.base().join("config/iprune/config.toml");
        let text = fs::read_to_string(&cfg).unwrap();
        assert!(text.contains(from), "{from:?} not in {text}");
        fs::write(cfg, text.replace(from, to)).unwrap();
    }

    fn size_of(&self, files: &[&str]) -> u64 {
        files
            .iter()
            .map(|f| fs::metadata(self.music.join(f)).unwrap().len())
            .sum()
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

/// Three files; playlists Road (one, two), Mix (three) and Empty (nothing);
/// an adopted iPod named "Test iPod" that follows the config.
fn world() -> Sandbox {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (music, lists) = (base.join("music"), base.join("lists"));
    for d in [&music, &lists] {
        fs::create_dir(d).unwrap();
    }
    let sb = Sandbox { dir, music, lists };
    sb.ok(&[
        "init",
        "--library",
        sb.music.to_str().unwrap(),
        "--playlists",
        sb.lists.to_str().unwrap(),
    ]);
    for (name, kind, variant) in [
        ("one.mp3", Kind::Mp3, 1),
        ("two.m4a", Kind::Aac, 1),
        ("three.wav", Kind::Wav, 3),
    ] {
        fs::copy(fixture(kind, variant), sb.music.join(name)).unwrap();
    }
    sb.ok(&["scan"]);
    let m = |f: &str| sb.music.join(f).display().to_string();
    fs::write(
        sb.lists.join("Road.m3u8"),
        format!("{}\n{}\n", m("one.mp3"), m("two.m4a")),
    )
    .unwrap();
    fs::write(sb.lists.join("Mix.m3u8"), format!("{}\n", m("three.wav"))).unwrap();
    fs::write(sb.lists.join("Empty.m3u8"), "").unwrap();
    {
        let store = sb.store();
        devices::upsert_device(store.conn(), GUID, Some("Test iPod"), None).unwrap();
    }
    sb
}

fn lines(o: &Output) -> Vec<String> {
    out(o).lines().map(str::to_owned).collect()
}

fn sc(kind: Scope, names: &[&str]) -> Option<devices::DeviceScope> {
    Some(devices::DeviceScope {
        kind,
        playlists: names.iter().map(|n| (*n).to_owned()).collect(),
    })
}

#[test]
fn showing_an_ipod_that_follows_the_config_changes_nothing() {
    let sb = world();
    let o = sb.ok(&["scope", "--guid", GUID]);
    let size = lines(&o).pop().unwrap();
    assert_eq!(
        lines(&o)[..2],
        [
            format!("Test iPod ({GUID})"),
            "syncs the whole library, from [sync] in the config".to_owned()
        ]
    );
    assert!(size.starts_with("3 tracks, "), "{size}");
    assert!(!size.contains("holds"), "no iPod is connected: {size}");
    assert_eq!(sb.stored(GUID), None);

    // The config's own selection is shown as the config's.
    sb.edit_config("scope = \"all\"", "scope = \"except\"");
    sb.edit_config("playlists = []", "playlists = [\"Mix\"]");
    let o = sb.ok(&["scope", "--guid", GUID]);
    assert_eq!(
        lines(&o)[1],
        "syncs the whole library except 1 playlist: Mix, from [sync] in the config"
    );
    assert!(lines(&o)[2].starts_with("2 tracks, "), "{}", out(&o));
    assert_eq!(sb.stored(GUID), None);
}

#[test]
fn only_sets_and_saves_the_playlists_sorted_without_duplicates() {
    let sb = world();
    let o = sb.ok(&["scope", "only", "Road", "Mix", "Road", "--guid", GUID]);
    assert_eq!(
        lines(&o)[1],
        "syncs only 2 playlists: Mix, Road, set for this iPod"
    );
    assert!(lines(&o)[2].starts_with("3 tracks, "), "{}", out(&o));
    assert_eq!(sb.stored(GUID), sc(Scope::Playlists, &["Mix", "Road"]));

    // The choice survives to the next run, which only shows it.
    let again = sb.ok(&["scope", "--guid", GUID]);
    assert_eq!(out(&again), out(&o));
}

#[test]
fn only_counts_and_sizes_just_the_tracks_of_the_playlist() {
    let sb = world();
    let o = sb.ok(&["scope", "only", "Mix", "--guid", GUID]);
    assert_eq!(
        lines(&o)[1],
        "syncs only 1 playlist: Mix, set for this iPod"
    );
    let want = format!(
        "1 tracks, {}",
        iprune_core::sync::bytes(sb.size_of(&["three.wav"]))
    );
    assert_eq!(lines(&o)[2], want);
}

#[test]
fn except_sets_the_scope_and_sizes_what_is_left() {
    let sb = world();
    let o = sb.ok(&["scope", "except", "Mix", "--guid", GUID]);
    assert_eq!(
        lines(&o)[1],
        "syncs the whole library except 1 playlist: Mix, set for this iPod"
    );
    let want = format!(
        "2 tracks, {}",
        iprune_core::sync::bytes(sb.size_of(&["one.mp3", "two.m4a"]))
    );
    assert_eq!(lines(&o)[2], want);
    assert_eq!(sb.stored(GUID), sc(Scope::Except, &["Mix"]));
}

#[test]
fn all_and_default_replace_an_earlier_choice() {
    let sb = world();
    sb.ok(&["scope", "only", "Road", "--guid", GUID]);
    let o = sb.ok(&["scope", "all", "--guid", GUID]);
    assert_eq!(lines(&o)[1], "syncs the whole library, set for this iPod");
    assert_eq!(sb.stored(GUID), sc(Scope::All, &[]));

    sb.ok(&["scope", "except", "Road", "--guid", GUID]);
    let o = sb.ok(&["scope", "default", "--guid", GUID]);
    assert_eq!(
        lines(&o)[1],
        "syncs the whole library, from [sync] in the config"
    );
    assert_eq!(sb.stored(GUID), None);
}

#[test]
fn an_unknown_playlist_is_refused_and_the_stored_scope_stays() {
    let sb = world();
    sb.ok(&["scope", "only", "Road", "--guid", GUID]);
    for args in [
        vec!["scope", "only", "Road", "Typo"],
        vec!["scope", "except", "Typo"],
    ] {
        let mut args = args;
        args.extend(["--guid", GUID]);
        let o = sb.run(&args);
        assert_code(&o, 1);
        let text = err(&o);
        assert!(
            text.contains("Typo") && text.contains("nothing changed"),
            "{text}"
        );
        assert!(out(&o).is_empty(), "{}", out(&o));
        assert_eq!(sb.stored(GUID), sc(Scope::Playlists, &["Road"]), "{args:?}");
    }
}

#[test]
fn keeping_only_playlists_with_no_track_is_refused() {
    let sb = world();
    let o = sb.run(&["scope", "only", "Empty", "--guid", GUID]);
    assert_code(&o, 1);
    assert!(err(&o).contains("nothing changed"), "{}", err(&o));
    assert_eq!(sb.stored(GUID), None);
}

#[test]
fn an_ipod_that_was_never_adopted_is_refused_and_nothing_is_stored() {
    let sb = world();
    for args in [
        vec!["scope", "--guid", "0x000A27009999AAAA"],
        vec!["scope", "all", "--guid", "0x000A27009999AAAA"],
    ] {
        let o = sb.run(&args);
        assert_code(&o, 1);
        let text = err(&o);
        assert!(
            text.contains("9999AAAA") && text.contains("adopt"),
            "{text}"
        );
    }
    let store = sb.store();
    let known = devices::devices(store.conn()).unwrap();
    assert_eq!(known.len(), 1, "no row was made for the unknown iPod");
    assert_eq!(devices::scope(store.conn(), known[0].id).unwrap(), None);
}

#[test]
fn the_guid_may_be_written_without_0x_and_in_lower_case() {
    let sb = world();
    let o = sb.ok(&["scope", "only", "Mix", "--guid", "000a27001234abcd"]);
    assert_eq!(lines(&o)[0], format!("Test iPod ({GUID})"));
    assert_eq!(sb.stored(GUID), sc(Scope::Playlists, &["Mix"]));
}

#[test]
fn only_and_except_need_a_playlist_name() {
    let sb = world();
    for sub in ["only", "except"] {
        let o = sb.run(&["scope", sub, "--guid", GUID]);
        assert_code(&o, 2);
    }
    assert_eq!(sb.stored(GUID), None);
}
