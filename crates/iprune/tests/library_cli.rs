//! End to end tests of `iprune init`, `scan` and `playlist ...`. Every run gets
//! its own HOME, XDG_CONFIG_HOME and XDG_DATA_HOME in a temp dir, and stdin is
//! closed, so nothing touches the real home and no prompt can block.

#[path = "../../iprune-core/tests/common/mod.rs"]
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{Kind, fairplay_m4a, fixture, id3v24_text, mp3_with_id3v2};

struct Sandbox {
    dir: tempfile::TempDir,
    music: PathBuf,
    lists: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let music = base.join("music");
        let lists = base.join("lists");
        fs::create_dir(&music).unwrap();
        fs::create_dir(&lists).unwrap();
        Sandbox { dir, music, lists }
    }

    fn base(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn config_file(&self) -> PathBuf {
        self.base().join("config/iprune/config.toml")
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

    fn init(&self) {
        let o = self.run(&[
            "init",
            "--library",
            self.music.to_str().unwrap(),
            "--playlists",
            self.lists.to_str().unwrap(),
        ]);
        assert_ok(&o);
    }

    fn add_track(&self, rel: &str, kind: Kind, variant: u32) -> PathBuf {
        let dest = self.music.join(rel);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::copy(fixture(kind, variant), &dest).unwrap();
        dest
    }

    fn playlist_file(&self, name: &str) -> PathBuf {
        self.lists.join(format!("{name}.m3u8"))
    }

    fn locations(&self, name: &str) -> Vec<String> {
        fs::read_to_string(self.playlist_file(name))
            .unwrap()
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(String::from)
            .collect()
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn assert_ok(o: &Output) {
    assert_eq!(
        o.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        out(o),
        err(o)
    );
}

fn assert_fails(o: &Output, code: i32) {
    assert_eq!(
        o.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        out(o),
        err(o)
    );
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn init_writes_a_config_with_the_given_folders() {
    let sb = Sandbox::new();
    sb.init();
    let text = fs::read_to_string(sb.config_file()).unwrap();
    assert!(text.contains(s(&sb.music)), "{text}");
    assert!(text.contains(s(&sb.lists)), "{text}");
    assert!(text.contains("absolute"), "{text}");
}

#[test]
fn init_relative_path_style_and_relative_folder_arguments_are_stored_absolute() {
    let sb = Sandbox::new();
    let o = Command::new(env!("CARGO_BIN_EXE_iprune"))
        .args([
            "init",
            "--library",
            "music",
            "--playlists",
            "lists",
            "--path-style",
            "relative",
        ])
        .current_dir(sb.base())
        .env("HOME", sb.base().join("home"))
        .env("XDG_CONFIG_HOME", sb.base().join("config"))
        .env("XDG_DATA_HOME", sb.base().join("data"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_ok(&o);
    let text = fs::read_to_string(sb.config_file()).unwrap();
    assert!(text.contains(&format!("\"{}\"", s(&sb.music))), "{text}");
    assert!(text.contains("relative"), "{text}");
}

#[test]
fn init_refuses_to_overwrite_without_force() {
    let sb = Sandbox::new();
    sb.init();
    let before = fs::read_to_string(sb.config_file()).unwrap();
    let other = sb.base().join("other");
    fs::create_dir(&other).unwrap();

    let o = sb.run(&["init", "--library", s(&other), "--playlists", s(&other)]);
    assert_fails(&o, 1);
    assert!(err(&o).contains("--force"), "{}", err(&o));
    assert_eq!(fs::read_to_string(sb.config_file()).unwrap(), before);

    let o = sb.run(&[
        "init",
        "--force",
        "--library",
        s(&other),
        "--playlists",
        s(&other),
    ]);
    assert_ok(&o);
    let after = fs::read_to_string(sb.config_file()).unwrap();
    assert_ne!(after, before);
    assert!(after.contains(s(&other)));
}

#[test]
fn init_rejects_nonexistent_folders_and_writes_no_config() {
    let sb = Sandbox::new();
    let nope = sb.base().join("does-not-exist");
    let o = sb.run(&["init", "--library", s(&nope), "--playlists", s(&sb.lists)]);
    assert_fails(&o, 1);
    assert!(err(&o).contains("not a folder"), "{}", err(&o));
    let o = sb.run(&["init", "--library", s(&sb.music), "--playlists", s(&nope)]);
    assert_fails(&o, 1);
    // A file is not a folder either.
    let file = sb.base().join("afile");
    fs::write(&file, "x").unwrap();
    let o = sb.run(&["init", "--library", s(&file), "--playlists", s(&sb.lists)]);
    assert_fails(&o, 1);
    assert!(!sb.config_file().exists());
}

#[test]
fn init_without_required_arguments_is_a_usage_error() {
    let sb = Sandbox::new();
    assert_fails(&sb.run(&["init"]), 2);
    assert_fails(&sb.run(&["init", "--library", s(&sb.music)]), 2);
    assert!(!sb.config_file().exists());
}

#[test]
fn commands_before_init_fail_with_a_pointer_to_init() {
    let sb = Sandbox::new();
    for args in [&["scan"][..], &["playlist", "list"]] {
        let o = sb.run(args);
        assert_fails(&o, 1);
        assert!(err(&o).contains("iprune init"), "{args:?}: {}", err(&o));
    }
    assert!(
        !sb.base().join("data").exists(),
        "no database created without a config"
    );
}

#[test]
fn scan_reports_counts_and_second_scan_is_unchanged() {
    let sb = Sandbox::new();
    sb.init();
    sb.add_track("a.mp3", Kind::Mp3, 1);
    sb.add_track("b.m4a", Kind::Alac, 2);
    sb.add_track("c.flac", Kind::Flac, 3);
    fs::write(sb.music.join("bad.mp3"), b"nope").unwrap();

    let o = sb.run(&["scan"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(
        text.contains(
            "library: 0 unchanged, 2 added, 0 updated, 0 back from missing, 0 moved, 0 missing"
        ),
        "{text}"
    );
    assert!(text.contains("unsupported: 1"), "{text}");
    assert!(text.contains("c.flac"), "{text}");
    assert!(text.contains("unreadable: 1"), "{text}");
    assert!(text.contains("bad.mp3"), "{text}");

    let text = out(&sb.run(&["scan"]));
    assert!(text.contains("2 unchanged, 0 added"), "{text}");
}

#[test]
fn scan_reports_moves_missing_and_rewritten_playlists() {
    let sb = Sandbox::new();
    sb.init();
    let a = sb.add_track("a.mp3", Kind::Mp3, 1);
    let b = sb.add_track("b.mp3", Kind::Mp3, 2);
    sb.add_track("keep.mp3", Kind::Mp3, 3);
    assert_ok(&sb.run(&["scan"]));
    assert_ok(&sb.run(&["playlist", "new", "mix"]));
    assert_ok(&sb.run(&["playlist", "add", "mix", s(&a)]));

    let moved = sb.music.join("moved.mp3");
    fs::rename(&a, &moved).unwrap();
    fs::remove_file(&b).unwrap();
    let o = sb.run(&["scan"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(text.contains("1 moved, 1 missing"), "{text}");
    assert!(
        text.contains(&format!("{} -> {}", a.display(), moved.display())),
        "{text}"
    );
    assert!(text.contains("missing: 1"), "{text}");
    assert!(
        text.contains("playlists rewritten for moved files: 1"),
        "{text}"
    );
    assert_eq!(sb.locations("mix"), vec![s(&moved).to_owned()]);
}

#[test]
fn scan_with_unmounted_root_exits_1_and_marks_nothing_missing() {
    let sb = Sandbox::new();
    sb.init();
    sb.add_track("a.mp3", Kind::Mp3, 1);
    assert_ok(&sb.run(&["scan"]));
    let away = sb.base().join("music-away");
    fs::rename(&sb.music, &away).unwrap();

    let o = sb.run(&["scan"]);
    assert_fails(&o, 1);
    assert!(err(&o).contains("mounted"), "{}", err(&o));
    assert!(out(&o).is_empty(), "{}", out(&o));

    fs::rename(&away, &sb.music).unwrap();
    let text = out(&sb.run(&["scan"]));
    assert!(
        text.contains("1 unchanged, 0 added, 0 updated, 0 back from missing, 0 moved, 0 missing"),
        "{text}"
    );
}

#[test]
fn scan_of_emptied_library_exits_1_and_keeps_the_tracks() {
    let sb = Sandbox::new();
    sb.init();
    let a = sb.add_track("a.mp3", Kind::Mp3, 1);
    assert_ok(&sb.run(&["scan"]));
    fs::remove_file(&a).unwrap();
    let o = sb.run(&["scan"]);
    assert_fails(&o, 1);
    assert!(err(&o).contains("refusing"), "{}", err(&o));
    fs::copy(fixture(Kind::Mp3, 1), &a).unwrap();
    let text = out(&sb.run(&["scan"]));
    assert!(
        text.contains("0 added") && text.contains("0 moved, 0 missing"),
        "{text}"
    );
}

#[test]
fn playlist_lifecycle_through_the_cli() {
    let sb = Sandbox::new();
    sb.init();
    let a = sb.add_track("a.mp3", Kind::Mp3, 1);
    let b = sb.add_track("b.mp3", Kind::Mp3, 2);
    let c = sb.add_track("c.mp3", Kind::Mp3, 3);
    assert_ok(&sb.run(&["scan"]));

    assert_ok(&sb.run(&["playlist", "new", "mix"]));
    assert!(sb.playlist_file("mix").is_file());
    assert_ok(&sb.run(&["playlist", "add", "mix", s(&a), s(&b)]));
    assert_ok(&sb.run(&["playlist", "add", "mix", s(&c), "--at", "1"]));
    assert_eq!(sb.locations("mix"), [s(&c), s(&a), s(&b)]);

    let shown = out(&sb.run(&["playlist", "show", "mix"]));
    let lines: Vec<_> = shown
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .collect();
    assert_eq!(lines.len(), 3, "{shown}");
    assert_eq!(lines[0], ["1", "Artist3", "-", "Title3"]);
    assert_eq!(lines[2], ["3", "Artist2", "-", "Title2"]);

    assert_ok(&sb.run(&["playlist", "move", "mix", "3", "1"]));
    assert_eq!(sb.locations("mix"), [s(&b), s(&c), s(&a)]);
    assert_ok(&sb.run(&["playlist", "remove", "mix", "1", "3"]));
    assert_eq!(sb.locations("mix"), [s(&c)]);

    assert_ok(&sb.run(&["playlist", "rename", "mix", "best"]));
    assert!(!sb.playlist_file("mix").exists());
    assert_eq!(sb.locations("best"), [s(&c)]);

    let list = out(&sb.run(&["playlist", "list"]));
    assert!(list.trim_start().starts_with("1  best"), "{list}");

    assert_ok(&sb.run(&["playlist", "rm", "best", "--yes"]));
    assert!(!sb.playlist_file("best").exists());
    assert!(out(&sb.run(&["playlist", "list"])).trim().is_empty());
    assert!(a.exists() && b.exists() && c.exists());
}

#[test]
fn playlist_list_shows_unresolved_counts_and_sees_external_files() {
    let sb = Sandbox::new();
    sb.init();
    let a = sb.add_track("a.mp3", Kind::Mp3, 1);
    assert_ok(&sb.run(&["scan"]));
    fs::write(
        sb.playlist_file("hand"),
        format!("#EXTM3U\n{}\n/missing/x.mp3\n/missing/y.mp3\n", a.display()),
    )
    .unwrap();
    let list = out(&sb.run(&["playlist", "list"]));
    assert!(list.contains("3  hand"), "{list}");
    assert!(list.contains("(2 unresolved)"), "{list}");
    let shown = out(&sb.run(&["playlist", "show", "hand"]));
    assert!(shown.contains("[unresolved] /missing/x.mp3"), "{shown}");
}

#[test]
fn rm_without_yes_and_no_terminal_exits_1_and_keeps_the_file() {
    let sb = Sandbox::new();
    sb.init();
    assert_ok(&sb.run(&["playlist", "new", "keepme"]));
    let o = sb.run(&["playlist", "rm", "keepme"]);
    assert_fails(&o, 1);
    assert!(err(&o).contains("--yes"), "{}", err(&o));
    assert!(sb.playlist_file("keepme").is_file());
    assert!(out(&sb.run(&["playlist", "list"])).contains("keepme"));
}

#[test]
fn rm_of_unknown_playlist_exits_1_even_with_yes() {
    let sb = Sandbox::new();
    sb.init();
    let o = sb.run(&["playlist", "rm", "ghost", "--yes"]);
    assert_fails(&o, 1);
    assert!(err(&o).contains("ghost"), "{}", err(&o));
}

#[test]
fn playlist_errors_exit_1_and_leave_the_file_untouched() {
    let sb = Sandbox::new();
    sb.init();
    let a = sb.add_track("a.mp3", Kind::Mp3, 1);
    let stranger = sb.add_track("later.mp3", Kind::Mp3, 2);
    assert_ok(&sb.run(&["scan"]));
    // `later.mp3` was scanned too; make one that is not indexed.
    let unindexed = sb.base().join("outside.mp3");
    fs::copy(fixture(Kind::Mp3, 4), &unindexed).unwrap();
    assert_ok(&sb.run(&["playlist", "new", "mix"]));
    assert_ok(&sb.run(&["playlist", "add", "mix", s(&a)]));
    let before = fs::read(sb.playlist_file("mix")).unwrap();

    for args in [
        vec!["playlist", "add", "mix", s(&stranger), s(&unindexed)],
        vec!["playlist", "add", "mix", s(&stranger), "--at", "5"],
        vec!["playlist", "remove", "mix", "2"],
        vec!["playlist", "remove", "mix", "1", "0"],
        vec!["playlist", "move", "mix", "1", "2"],
        vec!["playlist", "new", "mix"],
        vec!["playlist", "rename", "mix", "mix"],
        vec!["playlist", "add", "nosuch", s(&a)],
        vec!["playlist", "show", "nosuch"],
    ] {
        let o = sb.run(&args);
        assert_fails(&o, 1);
        assert!(!err(&o).is_empty(), "{args:?}");
        assert_eq!(
            fs::read(sb.playlist_file("mix")).unwrap(),
            before,
            "{args:?}"
        );
    }
    assert!(!sb.playlist_file("nosuch").exists());
}

#[test]
fn bad_invocations_are_usage_errors_with_exit_2() {
    let sb = Sandbox::new();
    sb.init();
    assert_ok(&sb.run(&["playlist", "new", "mix"]));
    let before = fs::read(sb.playlist_file("mix")).unwrap();
    for args in [
        &["playlist"][..],
        &["playlist", "add", "mix"],
        &["playlist", "remove", "mix"],
        &["playlist", "remove", "mix", "x"],
        &["playlist", "remove", "mix", "-1"],
        &["playlist", "move", "mix", "1"],
        &["playlist", "add", "mix", "f.mp3", "--at", "abc"],
        &["nonsense"],
    ] {
        assert_fails(&sb.run(args), 2);
    }
    assert_eq!(fs::read(sb.playlist_file("mix")).unwrap(), before);
}

#[test]
fn invalid_playlist_name_is_rejected() {
    let sb = Sandbox::new();
    sb.init();
    let o = sb.run(&["playlist", "new", "../evil"]);
    assert_fails(&o, 1);
    assert!(!sb.base().join("evil.m3u8").exists());
    assert_eq!(fs::read_dir(&sb.lists).unwrap().count(), 0);
}

#[test]
fn nothing_is_written_outside_the_sandbox_home() {
    // The sandbox sets HOME to a folder that must stay empty: the config and
    // database go to the XDG folders.
    let sb = Sandbox::new();
    sb.init();
    sb.add_track("a.mp3", Kind::Mp3, 1);
    assert_ok(&sb.run(&["scan"]));
    assert!(!sb.base().join("home").exists());
    assert!(sb.base().join("data/iprune/iprune.sqlite").is_file());
}

#[test]
fn scan_lists_untagged_and_fairplay_files_and_still_succeeds() {
    let sb = Sandbox::new();
    sb.init();
    sb.add_track("clean.mp3", Kind::Mp3, 1);
    fs::write(
        sb.music.join("badtag.mp3"),
        mp3_with_id3v2(
            2,
            &[
                id3v24_text("TIT2", 3, b"Lost"),
                id3v24_text("TRCK", 1, &[0x31, 0x00, 0x32, 0x00]),
            ],
        ),
    )
    .unwrap();
    fs::write(sb.music.join("purchase.m4a"), fairplay_m4a(3)).unwrap();
    fs::write(sb.music.join("garbage.mp3"), b"nope").unwrap();

    let o = sb.run(&["scan"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(text.contains("3 added"), "{text}");
    assert!(
        text.contains("indexed without tags (tags could not be parsed): 1\n  ")
            && text.contains("badtag.mp3:"),
        "{text}"
    );
    assert!(
        text.contains("FairPlay protected, indexed as AAC: 1\n  ") && text.contains("purchase.m4a"),
        "{text}"
    );
    assert!(text.contains("unreadable: 1"), "{text}");
    assert!(text.contains("garbage.mp3"), "{text}");
    // Files are listed under their own heading only.
    let untagged_at = text.find("indexed without tags").unwrap();
    let drm_at = text.find("FairPlay").unwrap();
    assert!(
        !text[untagged_at..drm_at].contains("purchase.m4a"),
        "{text}"
    );
    assert!(!text.contains("clean.mp3"), "{text}");
}

#[test]
fn scan_of_ordinary_files_prints_no_untagged_or_fairplay_section() {
    let sb = Sandbox::new();
    sb.init();
    sb.add_track("a.mp3", Kind::Mp3, 1);
    sb.add_track("b.m4a", Kind::Aac, 2);
    let text = out(&sb.run(&["scan"]));
    assert!(text.contains("2 added"), "{text}");
    assert!(
        !text.contains("without tags") && !text.contains("FairPlay"),
        "{text}"
    );
}
