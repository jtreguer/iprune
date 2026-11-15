//! End to end tests of `iprune import` (technical document, sections 7.2 and
//! 13) with the fake autotag of `tests/common/fake_autotag.py`: no network,
//! no real autotag. Every run gets its own HOME, XDG folders, library,
//! playlists folder and inbox in a temp dir, and stdin is closed.

#[path = "../../iprune-core/tests/common/mod.rs"]
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::import_env::{dest, fake_autotag, fake_pids, requests, scenario, write_scenario};
use common::{Kind, fixture, fixture_secs};
use iprune_core::config::{Config, Paths};
use iprune_core::store::Store;
use serde_json::{Value, json};

struct Sandbox {
    dir: tempfile::TempDir,
    music: PathBuf,
    lists: PathBuf,
    inbox: PathBuf,
}

impl Sandbox {
    /// A library holding one file, indexed, an inbox that does not exist yet,
    /// and a config naming the fake autotag.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let (music, lists, inbox) = (base.join("music"), base.join("lists"), base.join("inbox"));
        fs::create_dir_all(music.join("Seed")).unwrap();
        fs::create_dir(&lists).unwrap();
        fs::copy(fixture(Kind::Mp3, 90), music.join("Seed/Seed.mp3")).unwrap();
        let sb = Sandbox {
            dir,
            music,
            lists,
            inbox,
        };
        assert_ok(&sb.run(&[
            "init",
            "--library",
            s(&sb.music),
            "--playlists",
            s(&sb.lists),
        ]));
        let mut cfg = Config::load(&sb.config_file()).unwrap();
        cfg.import.inbox = sb.inbox.clone();
        cfg.import.autotag = fake_autotag();
        cfg.save(&sb.config_file()).unwrap();
        assert_ok(&sb.run(&["scan"]));
        sb
    }

    fn base(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn config_file(&self) -> PathBuf {
        self.base().join("config/iprune/config.toml")
    }

    fn data_dir(&self) -> PathBuf {
        self.base().join("data/iprune")
    }

    fn command(&self, args: &[&str]) -> Command {
        let base = self.base();
        let mut c = Command::new(env!("CARGO_BIN_EXE_iprune"));
        c.args(args)
            .env("HOME", base.join("home"))
            .env("XDG_CONFIG_HOME", base.join("config"))
            .env("XDG_DATA_HOME", base.join("data"))
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// An MP3 in the inbox: `Title<n>` by `Artist<n>`, `n + 1` seconds long.
    fn put(&self, rel: &str, n: u32) -> PathBuf {
        let p = self.inbox.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::copy(fixture_secs(Kind::Mp3, n, n + 1), &p).unwrap();
        p
    }

    fn scenario(&self, v: Value) {
        write_scenario(&self.music, &v);
    }

    fn requests(&self, op: &str) -> Vec<Value> {
        requests(&self.music)
            .into_iter()
            .filter(|r| r["op"] == op)
            .collect()
    }

    fn inbox_files(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.inbox.exists() {
            common::import_env::files_under(&self.inbox, &self.inbox, &mut v);
        }
        v.sort();
        v
    }

    /// Present tracks of the index, by path relative to the library.
    fn indexed(&self) -> Vec<String> {
        let store = Store::open(
            &Paths {
                config_file: self.config_file(),
                data_dir: self.data_dir(),
            }
            .database(),
        )
        .unwrap();
        let mut v: Vec<String> = iprune_core::store::all_tracks(store.conn())
            .unwrap()
            .into_iter()
            .filter(|t| t.missing_since.is_none())
            .map(|t| {
                t.path
                    .strip_prefix(&self.music)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        v.sort();
        v
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

fn assert_ok(o: &Output) {
    assert_code(o, 0);
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// The lines of `text` whose first word is `word`.
fn lines<'a>(text: &'a str, word: &str) -> Vec<&'a str> {
    text.lines()
        .filter(|l| l.split_whitespace().next() == Some(word))
        .collect()
}

/// `filed`, `left`, `would`, `not` and `check` lines mention `name`.
fn line_for<'a>(text: &'a str, word: &str, name: &str) -> &'a str {
    lines(text, word)
        .into_iter()
        .find(|l| l.contains(name))
        .unwrap_or_else(|| panic!("no `{word}` line for {name} in:\n{text}"))
}

/// The last line of stderr, the error iprune ends with.
fn final_message(o: &Output) -> String {
    let e = err(o);
    let last = e.lines().last().unwrap_or("").to_owned();
    last.strip_prefix("iprune: ")
        .unwrap_or_else(|| panic!("no `iprune:` error in {e:?}"))
        .to_owned()
}

/// Everything ready: `a.mp3` and `b.mp3` with certain candidates.
fn two_ready(sb: &Sandbox) {
    sb.put("a.mp3", 2);
    sb.put("b.mp3", 3);
    sb.scenario(scenario(&[("a.mp3", 2), ("b.mp3", 3)]));
}

// -- Dry run -----------------------------------------------------------------

#[test]
fn a_dry_run_looks_up_prints_what_would_be_filed_and_files_nothing() {
    let sb = Sandbox::new();
    sb.put("a.mp3", 2);
    sb.put("b.mp3", 3);
    fs::write(sb.inbox.join("notes.txt"), b"x").unwrap();
    let mut sc = scenario(&[("a.mp3", 2), ("b.mp3", 3)]);
    sc["lookups"]["b.mp3"]["candidates"][0]["certain"] = json!(false);
    sb.scenario(sc);
    let indexed = sb.indexed();

    let o = sb.run(&["import", "--dry-run"]);
    // One song is not certain, one file is not audio: items are left.
    assert_code(&o, 1);
    let text = out(&o);
    let would = line_for(&text, "would", "a.mp3");
    assert!(would.contains(&dest(2)), "{would}");
    assert!(lines(&text, "would").len() == 1, "{text}");
    let left_b = line_for(&text, "left", "b.mp3");
    assert!(left_b.contains("no certain match"), "{left_b}");
    let left_txt = line_for(&text, "left", "notes.txt");
    assert!(left_txt.contains("not supported"), "{left_txt}");
    assert!(lines(&text, "filed").is_empty());
    assert_eq!(final_message(&o), "2 of 3 files left in the inbox");

    // Looked up, never applied; nothing moved or indexed.
    assert_eq!(sb.requests("lookup").len(), 2);
    assert!(sb.requests("apply").is_empty());
    assert_eq!(sb.inbox_files(), ["a.mp3", "b.mp3", "notes.txt"]);
    assert!(!sb.music.join(dest(2)).exists());
    assert_eq!(sb.indexed(), indexed);
}

#[test]
fn a_dry_run_where_everything_would_be_filed_exits_0() {
    let sb = Sandbox::new();
    two_ready(&sb);
    let o = sb.run(&["import", "--dry-run"]);
    assert_ok(&o);
    let text = out(&o);
    assert_eq!(lines(&text, "would").len(), 2, "{text}");
    assert!(line_for(&text, "would", "b.mp3").contains(&dest(3)));
    assert!(lines(&text, "left").is_empty());
    assert!(sb.requests("apply").is_empty());
    assert_eq!(sb.inbox_files(), ["a.mp3", "b.mp3"]);
}

// -- Filing ------------------------------------------------------------------

#[test]
fn everything_ready_is_filed_scanned_and_the_exit_code_is_0() {
    let sb = Sandbox::new();
    two_ready(&sb);
    let o = sb.run(&["import"]);
    assert_ok(&o);
    let text = out(&o);
    assert!(
        line_for(&text, "filed", "a.mp3").contains(&dest(2)),
        "{text}"
    );
    assert!(
        line_for(&text, "filed", "b.mp3").contains(&dest(3)),
        "{text}"
    );
    assert!(lines(&text, "left").is_empty() && lines(&text, "not").is_empty());
    assert!(text.contains("2 added"), "the scan's report: {text}");

    assert!(sb.inbox_files().is_empty());
    assert!(sb.music.join(dest(2)).is_file() && sb.music.join(dest(3)).is_file());
    let mut want = vec![dest(2), dest(3), "Seed/Seed.mp3".to_owned()];
    want.sort();
    assert_eq!(sb.indexed(), want);
    // In path order.
    let sent: Vec<String> = sb
        .requests("apply")
        .iter()
        .map(|r| {
            Path::new(r["file"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(sent, ["a.mp3", "b.mp3"]);
}

#[test]
fn a_second_run_after_everything_was_filed_has_nothing_to_do() {
    let sb = Sandbox::new();
    two_ready(&sb);
    assert_ok(&sb.run(&["import"]));
    let before = requests(&sb.music).len();
    let o = sb.run(&["import"]);
    assert_ok(&o);
    assert!(out(&o).contains("empty"), "{}", out(&o));
    assert_eq!(
        requests(&sb.music).len(),
        before,
        "no autotag was started for it"
    );
}

#[test]
fn songs_left_in_the_inbox_give_exit_1_while_the_ready_ones_are_filed_all_the_same() {
    let sb = Sandbox::new();
    sb.put("a.mp3", 2);
    sb.put("b.mp3", 3);
    let mut sc = scenario(&[("a.mp3", 2), ("b.mp3", 3)]);
    sc["lookups"]["b.mp3"]["candidates"][0]["certain"] = json!(false);
    sb.scenario(sc);
    let o = sb.run(&["import"]);
    assert_code(&o, 1);
    let text = out(&o);
    assert!(line_for(&text, "filed", "a.mp3").contains(&dest(2)));
    assert!(line_for(&text, "left", "b.mp3").contains("no certain match"));
    assert_eq!(final_message(&o), "1 of 2 files left in the inbox");
    assert_eq!(sb.inbox_files(), ["b.mp3"]);
    assert!(sb.music.join(dest(2)).is_file());
    assert!(!sb.music.join(dest(3)).exists());
    assert!(sb.indexed().contains(&dest(2)));

    // The next run does not look b up again (it has its answer) and leaves
    // it where it is, still with exit 1.
    let lookups = sb.requests("lookup").len();
    let o = sb.run(&["import"]);
    assert_code(&o, 1);
    assert_eq!(sb.requests("lookup").len(), lookups);
    assert!(lines(&out(&o), "filed").is_empty());
    assert_eq!(sb.inbox_files(), ["b.mp3"]);
}

#[test]
fn nothing_ready_means_nothing_is_filed_and_exit_1_without_starting_a_second_process() {
    let sb = Sandbox::new();
    sb.put("a.mp3", 2);
    let mut sc = scenario(&[("a.mp3", 2)]);
    sc["lookups"]["a.mp3"]["candidates"] = json!([]);
    sb.scenario(sc);
    let o = sb.run(&["import"]);
    assert_code(&o, 1);
    let text = out(&o);
    assert!(
        line_for(&text, "left", "a.mp3").contains("no match"),
        "{text}"
    );
    assert_eq!(final_message(&o), "1 file left in the inbox");
    assert!(sb.requests("apply").is_empty());
    assert_eq!(sb.inbox_files(), ["a.mp3"]);
}

#[test]
fn a_file_already_in_the_library_is_left_and_never_looked_up() {
    let sb = Sandbox::new();
    fs::create_dir_all(&sb.inbox).unwrap();
    fs::copy(sb.music.join("Seed/Seed.mp3"), sb.inbox.join("copy.mp3")).unwrap();
    let o = sb.run(&["import"]);
    assert_code(&o, 1);
    let text = out(&o);
    let l = line_for(&text, "left", "copy.mp3");
    assert!(
        l.contains("already in the library") && l.contains("Seed.mp3"),
        "{l}"
    );
    assert!(
        requests(&sb.music).is_empty(),
        "autotag was not even started"
    );
    assert_eq!(sb.inbox_files(), ["copy.mp3"]);
}

#[test]
fn a_file_that_is_not_audio_is_left_as_not_supported() {
    let sb = Sandbox::new();
    fs::create_dir_all(&sb.inbox).unwrap();
    fs::write(sb.inbox.join("notes.txt"), b"hello").unwrap();
    let o = sb.run(&["import"]);
    assert_code(&o, 1);
    assert!(line_for(&out(&o), "left", "notes.txt").contains("not supported"));
    assert!(requests(&sb.music).is_empty());
}

#[test]
fn an_empty_inbox_is_said_to_be_empty_and_is_not_an_error() {
    let sb = Sandbox::new();
    let o = sb.run(&["import"]);
    assert_ok(&o);
    assert!(out(&o).contains("empty"), "{}", out(&o));
    assert!(sb.inbox.is_dir(), "created when missing");
    assert!(requests(&sb.music).is_empty());
}

#[test]
fn a_failed_lookup_is_left_with_its_error_and_tried_again_by_the_next_run() {
    let sb = Sandbox::new();
    sb.put("a.mp3", 2);
    let mut sc = scenario(&[("a.mp3", 2)]);
    sc["lookups"]["a.mp3"] = json!({"error": "network down"});
    sb.scenario(sc);
    let o = sb.run(&["import"]);
    assert_code(&o, 1);
    assert!(line_for(&out(&o), "left", "a.mp3").contains("network down"));
    assert_eq!(sb.requests("lookup").len(), 1);

    sb.scenario(scenario(&[("a.mp3", 2)]));
    let o = sb.run(&["import"]);
    assert_ok(&o);
    assert_eq!(sb.requests("lookup").len(), 2, "retried");
    assert!(line_for(&out(&o), "filed", "a.mp3").contains(&dest(2)));
}

#[test]
fn a_refusal_is_a_not_filed_line_and_exit_1_and_the_file_stays() {
    let sb = Sandbox::new();
    two_ready(&sb);
    let mut sc = scenario(&[("a.mp3", 2), ("b.mp3", 3)]);
    sc["apply_errors"] = json!({"a.mp3": "disk full"});
    sb.scenario(sc);
    let o = sb.run(&["import"]);
    assert_code(&o, 1);
    let text = out(&o);
    let l = line_for(&text, "not", "a.mp3");
    assert!(l.starts_with("not filed") && l.contains("disk full"), "{l}");
    assert!(line_for(&text, "filed", "b.mp3").contains(&dest(3)));
    assert_eq!(final_message(&o), "1 of 2 files left in the inbox");
    assert_eq!(sb.inbox_files(), ["a.mp3"]);
}

#[test]
fn a_song_that_reads_back_otherwise_gets_a_check_line_and_is_still_filed() {
    let sb = Sandbox::new();
    sb.put("a.mp3", 2);
    let mut sc = scenario(&[("a.mp3", 2)]);
    sc["tag_override"] = json!({"title": "Quite Another"});
    sb.scenario(sc);
    let o = sb.run(&["import"]);
    assert_ok(&o);
    let text = out(&o);
    let check = line_for(&text, "check", &dest(2));
    assert!(check.contains("title"), "{check}");
    assert!(line_for(&text, "filed", "a.mp3").contains(&dest(2)));
    assert!(sb.music.join(dest(2)).is_file());
    assert!(sb.indexed().contains(&dest(2)));
}

#[test]
fn autotag_stopping_part_way_is_said_and_the_exit_code_is_1() {
    let sb = Sandbox::new();
    two_ready(&sb);
    let mut sc = scenario(&[("a.mp3", 2), ("b.mp3", 3)]);
    sc["quirks"] = json!({"apply:b.mp3": "exit"});
    sc["stderr"] = json!(["Traceback: out of luck"]);
    sb.scenario(sc);
    let o = sb.run(&["import"]);
    assert_code(&o, 1);
    let text = out(&o);
    assert!(line_for(&text, "filed", "a.mp3").contains(&dest(2)));
    assert!(
        text.contains("autotag stopped part way") && text.contains("out of luck"),
        "{text}"
    );
    assert_eq!(sb.inbox_files(), ["b.mp3"]);
    assert!(sb.indexed().contains(&dest(2)), "what was filed is scanned");
}

#[test]
fn a_missing_autotag_program_is_an_error_naming_it() {
    let sb = Sandbox::new();
    sb.put("a.mp3", 2);
    let mut cfg = Config::load(&sb.config_file()).unwrap();
    let missing = sb.base().join("no-such-autotag");
    cfg.import.autotag = missing.clone();
    cfg.save(&sb.config_file()).unwrap();
    let o = sb.run(&["import"]);
    assert_code(&o, 1);
    assert!(err(&o).contains(s(&missing)), "{}", err(&o));
    assert_eq!(sb.inbox_files(), ["a.mp3"]);
}

#[test]
fn it_warns_when_autotag_has_no_acoustid_key_and_not_when_it_has() {
    let sb = Sandbox::new();
    two_ready(&sb);
    let o = sb.run(&["import", "--dry-run"]);
    assert!(err(&o).contains("AcoustID"), "{}", err(&o));

    let mut sc = scenario(&[("a.mp3", 2), ("b.mp3", 3)]);
    sc["hello"] = json!({"protocol": 1, "formats": ["mp3"], "acoustid": true, "fpcalc": true});
    sb.scenario(sc);
    let o = sb.run(&["import", "--dry-run"]);
    assert!(!err(&o).contains("AcoustID"), "{}", err(&o));
}

#[test]
fn another_protocol_version_is_refused_and_nothing_is_filed() {
    let sb = Sandbox::new();
    two_ready(&sb);
    let mut sc = scenario(&[("a.mp3", 2), ("b.mp3", 3)]);
    sc["hello"] = json!({"protocol": 2, "formats": [], "acoustid": false, "fpcalc": false});
    sb.scenario(sc);
    let o = sb.run(&["import"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("protocol 2"), "{}", err(&o));
    assert_eq!(sb.inbox_files(), ["a.mp3", "b.mp3"]);
}

#[test]
fn a_second_import_is_refused_while_another_process_holds_import_lock() {
    let sb = Sandbox::new();
    two_ready(&sb);
    let paths = Paths {
        config_file: sb.config_file(),
        data_dir: sb.data_dir(),
    };
    let held = iprune_core::lock::import(&paths).unwrap();
    let o = sb.run(&["import"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("another iprune"), "{}", err(&o));
    assert!(requests(&sb.music).is_empty());
    assert_eq!(sb.inbox_files(), ["a.mp3", "b.mp3"]);
    drop(held);
    assert_ok(&sb.run(&["import"]));
}

// -- Playlist ----------------------------------------------------------------

#[test]
fn filed_songs_are_added_to_the_playlist_in_order_with_the_count_in_words() {
    let sb = Sandbox::new();
    assert_ok(&sb.run(&["playlist", "new", "Mix"]));
    two_ready(&sb);
    let o = sb.run(&["import", "--playlist", "Mix"]);
    assert_ok(&o);
    assert!(out(&o).contains("added 2 songs to Mix"), "{}", out(&o));
    let list = fs::read_to_string(sb.lists.join("Mix.m3u8")).unwrap();
    let locations: Vec<&str> = list.lines().filter(|l| !l.starts_with('#')).collect();
    assert_eq!(
        locations,
        [sb.music.join(dest(2)), sb.music.join(dest(3))].map(|p| p.display().to_string())
    );

    // One song: "1 song".
    sb.put("c.mp3", 4);
    sb.scenario(scenario(&[("c.mp3", 4)]));
    let o = sb.run(&["import", "--playlist", "Mix"]);
    assert_ok(&o);
    assert!(out(&o).contains("added 1 song to Mix"), "{}", out(&o));
    assert!(!out(&o).contains("1 songs"));
    let list = fs::read_to_string(sb.lists.join("Mix.m3u8")).unwrap();
    assert_eq!(list.lines().filter(|l| !l.starts_with('#')).count(), 3);
}

#[test]
fn a_playlist_that_does_not_exist_is_refused_before_anything_is_looked_up_or_copied() {
    let sb = Sandbox::new();
    two_ready(&sb);
    let outside = sb.base().join("given.mp3");
    fs::copy(fixture(Kind::Mp3, 5), &outside).unwrap();
    let o = sb.run(&["import", "--playlist", "Nowhere", s(&outside)]);
    assert_code(&o, 1);
    assert!(err(&o).contains("Nowhere"), "{}", err(&o));
    assert!(requests(&sb.music).is_empty(), "autotag was not started");
    assert_eq!(
        sb.inbox_files(),
        ["a.mp3", "b.mp3"],
        "the file given was not copied in"
    );
    assert!(!sb.lists.join("Nowhere.m3u8").exists());
}

#[test]
fn without_a_playlist_none_is_changed() {
    let sb = Sandbox::new();
    assert_ok(&sb.run(&["playlist", "new", "Mix"]));
    let before = fs::read(sb.lists.join("Mix.m3u8")).unwrap();
    two_ready(&sb);
    assert_ok(&sb.run(&["import"]));
    assert_eq!(fs::read(sb.lists.join("Mix.m3u8")).unwrap(), before);
}

// -- Files given -------------------------------------------------------------

#[test]
fn files_given_are_copied_into_the_inbox_looked_up_and_filed_and_the_originals_stay() {
    let sb = Sandbox::new();
    let src = sb.base().join("downloads");
    fs::create_dir(&src).unwrap();
    let given = src.join("a.mp3");
    fs::copy(fixture_secs(Kind::Mp3, 2, 3), &given).unwrap();
    sb.scenario(scenario(&[("a.mp3", 2)]));

    let o = sb.run(&["import", s(&given)]);
    assert_ok(&o);
    let text = out(&o);
    assert!(text.contains("1 file in the inbox"), "{text}");
    assert!(!text.contains("1 files"));
    assert!(line_for(&text, "filed", "a.mp3").contains(&dest(2)));
    assert!(given.is_file(), "the original is not touched");
    assert!(sb.inbox_files().is_empty(), "the copy was filed");
    assert!(sb.music.join(dest(2)).is_file());
}

#[test]
fn several_files_and_a_folder_given_are_counted_as_files_and_a_relative_path_is_read_from_the_current_folder()
 {
    let sb = Sandbox::new();
    let src = sb.base().join("downloads");
    fs::create_dir_all(src.join("Album")).unwrap();
    fs::copy(fixture_secs(Kind::Mp3, 2, 3), src.join("one.mp3")).unwrap();
    fs::copy(fixture_secs(Kind::Mp3, 3, 4), src.join("Album/two.mp3")).unwrap();
    fs::copy(fixture_secs(Kind::Mp3, 4, 5), src.join("Album/three.mp3")).unwrap();
    fs::write(src.join("Album/cover.jpg"), b"jpg").unwrap();
    sb.scenario(scenario(&[
        ("one.mp3", 2),
        ("two.mp3", 3),
        ("three.mp3", 4),
    ]));

    let o = sb
        .command(&["import", "--dry-run", "one.mp3", "Album"])
        .current_dir(&src)
        .output()
        .unwrap();
    assert_ok(&o);
    let text = out(&o);
    assert!(text.contains("3 files in the inbox"), "{text}");
    assert_eq!(
        sb.inbox_files(),
        ["Album/three.mp3", "Album/two.mp3", "one.mp3"],
        "the folder keeps its name and brings its audio only"
    );
    assert_eq!(lines(&text, "would").len(), 3);
    assert!(src.join("Album/cover.jpg").is_file());
}

#[test]
fn a_file_given_that_does_not_exist_is_an_error_and_nothing_is_looked_up() {
    let sb = Sandbox::new();
    two_ready(&sb);
    let o = sb.run(&["import", s(&sb.base().join("absent.mp3"))]);
    assert_code(&o, 1);
    assert!(err(&o).contains("absent.mp3"), "{}", err(&o));
    assert!(requests(&sb.music).is_empty());
    assert_eq!(sb.inbox_files(), ["a.mp3", "b.mp3"]);
}

// -- init --------------------------------------------------------------------

#[test]
fn init_writes_the_default_inbox_in_the_music_folder_of_the_home() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (music, lists) = (base.join("music"), base.join("lists"));
    fs::create_dir(&music).unwrap();
    fs::create_dir(&lists).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_iprune"))
            .args(args)
            .env("HOME", base.join("home"))
            .env("XDG_CONFIG_HOME", base.join("config"))
            .env("XDG_DATA_HOME", base.join("data"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    assert_ok(&run(&[
        "init",
        "--library",
        s(&music),
        "--playlists",
        s(&lists),
    ]));
    let file = base.join("config/iprune/config.toml");
    let cfg = Config::load(&file).unwrap();
    assert_eq!(cfg.import.inbox, base.join("home/Music/Inbox"));
    assert!(cfg.import.root.as_os_str().is_empty() && cfg.import.autotag.as_os_str().is_empty());
    let text = fs::read_to_string(&file).unwrap();
    assert!(
        text.contains("[import]") && text.contains("Inbox"),
        "{text}"
    );

    // --force keeps an inbox the user chose.
    let mut cfg = cfg;
    cfg.import.inbox = base.join("my-inbox");
    cfg.save(&file).unwrap();
    assert_ok(&run(&[
        "init",
        "--force",
        "--library",
        s(&music),
        "--playlists",
        s(&lists),
    ]));
    assert_eq!(
        Config::load(&file).unwrap().import.inbox,
        base.join("my-inbox")
    );
}

#[test]
fn no_autotag_process_is_left_running_when_the_command_ends() {
    let sb = Sandbox::new();
    sb.put("a.mp3", 2);
    sb.scenario(scenario(&[("a.mp3", 2)]));
    assert_ok(&sb.run(&["import", "--dry-run"]));
    let (pid, _, _) = fake_pids(&sb.music);
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
}

#[test]
fn when_nothing_is_filed_the_message_counts_the_files_left_in_words() {
    let sb = Sandbox::new();
    fs::create_dir_all(&sb.inbox).unwrap();
    fs::write(sb.inbox.join("notes.txt"), b"x").unwrap();
    fs::write(sb.inbox.join("README"), b"x").unwrap();
    fs::write(sb.inbox.join("cover.flac"), b"x").unwrap();
    let o = sb.run(&["import"]);
    assert_code(&o, 1);
    assert_eq!(final_message(&o), "3 files left in the inbox");
}

// -- init and the default inbox ------------------------------------------------

#[test]
fn init_force_on_a_config_without_an_import_section_leaves_it_out() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (music, lists) = (base.join("music"), base.join("lists"));
    fs::create_dir(&music).unwrap();
    fs::create_dir(&lists).unwrap();
    let file = base.join("config/iprune/config.toml");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        &file,
        format!("[library]\nroots = [{music:?}]\n\n[playlists]\ndir = {lists:?}\n"),
    )
    .unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_iprune"))
        .args([
            "init",
            "--force",
            "--library",
            s(&music),
            "--playlists",
            s(&lists),
        ])
        .env("HOME", base.join("home"))
        .env("XDG_CONFIG_HOME", base.join("config"))
        .env("XDG_DATA_HOME", base.join("data"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_ok(&o);
    assert!(
        Config::load(&file)
            .unwrap()
            .import
            .inbox
            .as_os_str()
            .is_empty()
    );
    let text = fs::read_to_string(&file).unwrap();
    assert!(!text.contains("inbox"), "{text}");
}

/// A sandbox whose library is `home/Music`, which holds the default inbox
/// `home/Music/Inbox`, with a config that has no `[import]` but the fake.
fn library_in_music() -> (tempfile::TempDir, PathBuf, Command) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let music = base.join("home/Music");
    let lists = base.join("lists");
    fs::create_dir_all(music.join("Seed")).unwrap();
    fs::create_dir(&lists).unwrap();
    fs::copy(fixture(Kind::Mp3, 90), music.join("Seed/Seed.mp3")).unwrap();
    let file = base.join("config/iprune/config.toml");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        &file,
        format!(
            "[library]\nroots = [{music:?}]\n[playlists]\ndir = {lists:?}\n\
             [import]\nautotag = {:?}\n",
            fake_autotag()
        ),
    )
    .unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_iprune"));
    c.env("HOME", base.join("home"))
        .env("XDG_CONFIG_HOME", base.join("config"))
        .env("XDG_DATA_HOME", base.join("data"))
        .env_remove("XDG_MUSIC_DIR")
        .stdin(Stdio::null());
    (dir, music, c)
}

fn run_in(base: &Command, args: &[&str]) -> Output {
    let mut c = Command::new(base.get_program());
    for (k, v) in base.get_envs() {
        match v {
            Some(v) => c.env(k, v),
            None => c.env_remove(k),
        };
    }
    c.args(args).stdin(Stdio::null()).output().unwrap()
}

#[test]
fn a_library_that_holds_the_default_inbox_works_for_every_command_but_the_import_refuses() {
    let (_dir, music, c) = library_in_music();
    assert_ok(&run_in(&c, &["scan"]));
    assert_ok(&run_in(&c, &["playlist", "list"]));

    let inbox = music.join("Inbox");
    let given = music.parent().unwrap().join("given.mp3");
    fs::copy(fixture(Kind::Mp3, 5), &given).unwrap();
    for args in [
        vec!["import"],
        vec!["import", s(&given)],
        vec!["import", "--dry-run"],
    ] {
        let o = run_in(&c, &args);
        assert_code(&o, 1);
        let e = err(&o);
        assert!(
            e.contains(s(&inbox)) && e.contains("library"),
            "{args:?}: {e}"
        );
    }
    assert!(!inbox.exists(), "nothing was made in the library");
    assert!(requests(&music).is_empty(), "autotag was not started");
    assert!(given.is_file());
}

#[test]
fn an_inbox_set_in_the_config_that_overlaps_the_library_is_still_a_config_error() {
    let (dir, music, c) = library_in_music();
    let file = dir
        .path()
        .canonicalize()
        .unwrap()
        .join("config/iprune/config.toml");
    let mut text = fs::read_to_string(&file).unwrap();
    text.push_str(&format!("inbox = {:?}\n", music.join("Mine")));
    fs::write(&file, text).unwrap();
    let o = run_in(&c, &["scan"]);
    assert_code(&o, 1);
    assert!(err(&o).contains("import.inbox"), "{}", err(&o));
}

#[test]
fn init_with_a_library_that_holds_the_default_inbox_succeeds_without_an_inbox_and_the_import_refuses()
 {
    for library_rel in ["home/Music", "home"] {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let music = base.join(library_rel);
        let lists = base.join("lists");
        fs::create_dir_all(music.join("Rock")).unwrap();
        fs::create_dir_all(base.join("home/Music")).unwrap();
        fs::create_dir(&lists).unwrap();
        let run = |args: &[&str]| {
            Command::new(env!("CARGO_BIN_EXE_iprune"))
                .args(args)
                .env("HOME", base.join("home"))
                .env("XDG_CONFIG_HOME", base.join("config"))
                .env("XDG_DATA_HOME", base.join("data"))
                .env_remove("XDG_MUSIC_DIR")
                .stdin(Stdio::null())
                .output()
                .unwrap()
        };
        assert_ok(&run(&[
            "init",
            "--library",
            s(&music),
            "--playlists",
            s(&lists),
        ]));
        let file = base.join("config/iprune/config.toml");
        let text = fs::read_to_string(&file).unwrap();
        assert!(!text.contains("inbox"), "{library_rel}: {text}");
        assert!(
            Config::load(&file)
                .unwrap()
                .import
                .inbox
                .as_os_str()
                .is_empty()
        );
        assert_ok(&run(&["scan"]));
        assert_ok(&run(&["playlist", "list"]));
        let o = run(&["import"]);
        assert_code(&o, 1);
        let e = err(&o);
        assert!(
            e.contains(s(&base.join("home/Music/Inbox"))) && e.contains("library"),
            "{library_rel}: {e}"
        );
        assert!(!base.join("home/Music/Inbox").exists());
    }
}

#[test]
fn a_missing_library_root_stops_a_real_import_before_any_lookup_but_not_a_dry_run() {
    let sb = Sandbox::new();
    two_ready(&sb);
    let gone = sb.base().join("unmounted");
    let mut cfg = Config::load(&sb.config_file()).unwrap();
    cfg.library.roots.push(gone.clone());
    cfg.save(&sb.config_file()).unwrap();

    let o = sb.run(&["import"]);
    assert_code(&o, 1);
    assert!(err(&o).contains(s(&gone)), "{}", err(&o));
    assert!(
        requests(&sb.music).is_empty(),
        "autotag was not even started"
    );
    assert_eq!(sb.inbox_files(), ["a.mp3", "b.mp3"]);

    // A dry run files nothing, so it does not need the root.
    let o = sb.run(&["import", "--dry-run"]);
    assert_ok(&o);
    assert_eq!(sb.requests("lookup").len(), 2);
    assert_eq!(lines(&out(&o), "would").len(), 2);
}
