//! End to end tests of `iprune tag`. Every run gets its own HOME and XDG
//! folders in a temp dir and stdin is closed; the library is a temp folder
//! of ffmpeg-generated files, so nothing touches a real library or home.

#[path = "../../iprune-core/tests/common/mod.rs"]
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{Kind, fixture};
use iprune_core::tags;

struct Sandbox {
    dir: tempfile::TempDir,
    music: PathBuf,
}

impl Sandbox {
    /// A library with `init` and `scan` done, holding `n` MP3s `t1.mp3` to
    /// `tN.mp3` (fixtures 1 to N: titles "Title1"..., artists "Artist1"...).
    fn new(n: u32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let music = base.join("music");
        let lists = base.join("lists");
        fs::create_dir(&music).unwrap();
        fs::create_dir(&lists).unwrap();
        for i in 1..=n {
            fs::copy(fixture(Kind::Mp3, i), music.join(format!("t{i}.mp3"))).unwrap();
        }
        let sb = Sandbox { dir, music };
        assert_ok(&sb.run(&["init", "--library", s(&sb.music), "--playlists", s(&lists)]));
        assert_ok(&sb.run(&["scan"]));
        sb
    }

    fn base(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn file(&self, name: &str) -> PathBuf {
        self.music.join(name)
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

fn read(p: &Path) -> tags::Tags {
    tags::read(p).unwrap().tags
}

/// The status word and path of each result line of a write, in order.
fn results(o: &Output) -> Vec<(String, String)> {
    out(o)
        .lines()
        .map(|l| {
            let (word, rest) = l.split_once(char::is_whitespace).expect("a status word");
            let path = rest.trim_start().split(": ").next().unwrap();
            (word.to_owned(), path.to_owned())
        })
        .collect()
}

/// The value printed for `name` in the listing of one file: lines are the
/// name padded to a column, then the value.
fn printed(o: &Output, name: &str) -> String {
    out(o)
        .lines()
        .find_map(|l| {
            let l = l.strip_prefix("  ")?;
            let (n, v) = l.split_at(l.len().min(14));
            (n.trim() == name).then(|| v.trim().to_owned())
        })
        .unwrap_or_else(|| panic!("no {name} line in {}", out(o)))
}

#[test]
fn with_no_option_it_prints_the_twelve_fields_and_writes_nothing() {
    let sb = Sandbox::new(2);
    let f = sb.file("t1.mp3");
    let before = fs::read(&f).unwrap();
    let o = sb.run(&["tag", s(&f)]);
    assert_ok(&o);
    assert_eq!(out(&o).lines().next(), Some(s(&f)));
    assert_eq!(printed(&o, "title"), "Title1");
    assert_eq!(printed(&o, "artist"), "Artist1");
    assert_eq!(printed(&o, "album"), "");
    assert_eq!(printed(&o, "compilation"), "no");
    // Twelve field lines under the path.
    assert_eq!(out(&o).lines().count(), 13, "{}", out(&o));
    assert_eq!(fs::read(&f).unwrap(), before, "printing writes nothing");
}

#[test]
fn it_writes_the_file_and_the_index() {
    let sb = Sandbox::new(1);
    let f = sb.file("t1.mp3");
    let o = sb.run(&[
        "tag",
        s(&f),
        "--artist",
        "a-ha",
        "--album",
        "Hunting High and Low",
        "--year",
        "1985",
        "--track",
        "3",
        "--track-total",
        "10",
        "--disc",
        "1",
        "--disc-total",
        "2",
        "--genre",
        "Pop",
        "--composer",
        "Waaktaar",
        "--album-artist",
        "a-ha",
        "--compilation",
        "yes",
        "--title",
        "Take On Me",
    ]);
    assert_ok(&o);
    assert_eq!(results(&o), [("written".to_string(), s(&f).to_string())]);

    let t = read(&f);
    assert_eq!(t.title.as_deref(), Some("Take On Me"));
    assert_eq!(t.artist.as_deref(), Some("a-ha"));
    assert_eq!(t.album.as_deref(), Some("Hunting High and Low"));
    assert_eq!(t.album_artist.as_deref(), Some("a-ha"));
    assert_eq!(t.composer.as_deref(), Some("Waaktaar"));
    assert_eq!(t.genre.as_deref(), Some("Pop"));
    assert_eq!(t.year, Some(1985));
    assert_eq!((t.track_no, t.track_total), (Some(3), Some(10)));
    assert_eq!((t.disc_no, t.disc_total), (Some(1), Some(2)));
    assert!(t.compilation);

    // The index follows the file: printing reads it from the index, and a
    // scan finds nothing to update.
    let p = sb.run(&["tag", s(&f)]);
    assert_ok(&p);
    assert_eq!(printed(&p, "title"), "Take On Me");
    assert_eq!(printed(&p, "year"), "1985");
    assert_eq!(printed(&p, "track total"), "10");
    assert_eq!(printed(&p, "compilation"), "yes");
    let scan = sb.run(&["scan"]);
    assert_ok(&scan);
    assert!(
        out(&scan).contains("1 unchanged, 0 added, 0 updated"),
        "{}{}",
        out(&scan),
        err(&scan)
    );
    let again = sb.run(&["tag", s(&f), "--artist", "a-ha"]);
    assert_ok(&again);
    assert_eq!(results(&again)[0].0, "unchanged");
}

#[test]
fn an_empty_value_clears_the_field() {
    let sb = Sandbox::new(1);
    let f = sb.file("t1.mp3");
    assert_ok(&sb.run(&[
        "tag",
        s(&f),
        "--year",
        "1999",
        "--track",
        "4",
        "--track-total",
        "9",
        "--genre",
        "Rock",
    ]));
    assert_eq!(read(&f).year, Some(1999));
    let o = sb.run(&[
        "tag",
        s(&f),
        "--artist",
        "",
        "--year",
        "",
        "--track",
        "",
        "--genre",
        "  ",
    ]);
    assert_ok(&o);
    assert_eq!(results(&o)[0].0, "written");
    let t = read(&f);
    assert_eq!(t.artist, None);
    assert_eq!(t.year, None);
    assert_eq!(t.track_no, None);
    assert_eq!(t.genre, None);
    assert_eq!(t.track_total, Some(9), "not listed, left alone");
    assert_eq!(t.title.as_deref(), Some("Title1"), "not listed, left alone");
    let p = sb.run(&["tag", s(&f)]);
    assert_eq!(printed(&p, "artist"), "");
    assert_eq!(printed(&p, "year"), "");
}

#[test]
fn compilation_no_turns_it_off() {
    let sb = Sandbox::new(1);
    let f = sb.file("t1.mp3");
    assert_ok(&sb.run(&["tag", s(&f), "--compilation", "yes"]));
    assert!(read(&f).compilation);
    assert_ok(&sb.run(&["tag", s(&f), "--compilation", "no"]));
    assert!(!read(&f).compilation);
}

#[test]
fn several_files_get_the_same_fields_and_one_line_each_in_argument_order() {
    let sb = Sandbox::new(3);
    let (a, b, c) = (sb.file("t3.mp3"), sb.file("t1.mp3"), sb.file("t2.mp3"));
    let o = sb.run(&[
        "tag",
        s(&a),
        s(&b),
        s(&c),
        "--album",
        "Shared",
        "--disc",
        "2",
    ]);
    assert_ok(&o);
    assert_eq!(
        results(&o),
        [
            ("written".to_string(), s(&a).to_string()),
            ("written".to_string(), s(&b).to_string()),
            ("written".to_string(), s(&c).to_string()),
        ]
    );
    for (f, title) in [(&a, "Title3"), (&b, "Title1"), (&c, "Title2")] {
        let t = read(f);
        assert_eq!(t.album.as_deref(), Some("Shared"));
        assert_eq!(t.disc_no, Some(2));
        assert_eq!(t.title.as_deref(), Some(title), "own title kept");
    }
}

#[test]
fn a_file_given_twice_is_edited_and_reported_once() {
    let sb = Sandbox::new(2);
    let (a, b) = (sb.file("t1.mp3"), sb.file("t2.mp3"));
    let o = sb.run(&["tag", s(&a), s(&b), s(&a), "--album", "Twice"]);
    assert_ok(&o);
    assert_eq!(
        results(&o),
        [
            ("written".to_string(), s(&a).to_string()),
            ("written".to_string(), s(&b).to_string()),
        ]
    );
}

#[test]
fn title_and_track_take_a_single_file() {
    let sb = Sandbox::new(2);
    let (a, b) = (sb.file("t1.mp3"), sb.file("t2.mp3"));
    let before = (fs::read(&a).unwrap(), fs::read(&b).unwrap());
    for args in [
        vec!["tag", s(&a), s(&b), "--title", "Same"],
        vec!["tag", s(&a), s(&b), "--track", "1"],
        // Another option beside it does not make it acceptable, or write.
        vec!["tag", s(&a), s(&b), "--album", "X", "--title", "Same"],
    ] {
        let o = sb.run(&args);
        assert_code(&o, 2);
        assert!(
            out(&o).is_empty(),
            "nothing is reported as done: {}",
            out(&o)
        );
        assert!(
            err(&o).contains("--title") || err(&o).contains("--track"),
            "{}",
            err(&o)
        );
    }
    assert_eq!(fs::read(&a).unwrap(), before.0);
    assert_eq!(fs::read(&b).unwrap(), before.1);
    // Both are fine for one file.
    assert_ok(&sb.run(&["tag", s(&a), "--title", "Alone", "--track", "7"]));
    assert_eq!(read(&a).title.as_deref(), Some("Alone"));
    assert_eq!(read(&a).track_no, Some(7));
}

#[test]
fn out_of_range_and_non_numeric_values_are_usage_errors_that_write_nothing() {
    let sb = Sandbox::new(1);
    let f = sb.file("t1.mp3");
    let before = fs::read(&f).unwrap();
    for bad in [
        ["--year", "0"],
        ["--year", "10000"],
        ["--track", "1000"],
        ["--track", "0"],
        ["--track-total", "1000"],
        ["--disc", "0"],
        ["--disc-total", "1000"],
        ["--year", "abc"],
        ["--track", "-1"],
        ["--year", "19.5"],
    ] {
        // A good field beside the bad one must not be written either.
        let o = sb.run(&["tag", s(&f), "--album", "Not Written", bad[0], bad[1]]);
        assert_code(&o, 2);
        assert!(out(&o).is_empty(), "{bad:?}: {}", out(&o));
        assert_eq!(fs::read(&f).unwrap(), before, "{bad:?}");
    }
    // The limits themselves are accepted.
    let o = sb.run(&[
        "tag",
        s(&f),
        "--year",
        "9999",
        "--track",
        "999",
        "--disc-total",
        "999",
    ]);
    assert_ok(&o);
    let t = read(&f);
    assert_eq!(
        (t.year, t.track_no, t.disc_total),
        (Some(9999), Some(999), Some(999))
    );
}

#[test]
fn an_unindexed_file_is_refused_and_the_others_are_still_written() {
    let sb = Sandbox::new(2);
    let (a, b) = (sb.file("t1.mp3"), sb.file("t2.mp3"));
    // On disk in the library but never scanned, a file outside it, and a
    // name that does not exist.
    let fresh = sb.file("fresh.mp3");
    fs::copy(fixture(Kind::Mp3, 3), &fresh).unwrap();
    let elsewhere = sb.base().join("elsewhere.mp3");
    fs::copy(fixture(Kind::Mp3, 4), &elsewhere).unwrap();
    let nowhere = sb.file("nowhere.mp3");
    let untouched = (fs::read(&fresh).unwrap(), fs::read(&elsewhere).unwrap());

    let o = sb.run(&[
        "tag",
        s(&fresh),
        s(&a),
        s(&elsewhere),
        s(&nowhere),
        s(&b),
        "--album",
        "Partial",
    ]);
    assert_code(&o, 1);
    assert_eq!(
        results(&o),
        [
            ("refused".to_string(), s(&fresh).to_string()),
            ("written".to_string(), s(&a).to_string()),
            ("refused".to_string(), s(&elsewhere).to_string()),
            ("refused".to_string(), s(&nowhere).to_string()),
            ("written".to_string(), s(&b).to_string()),
        ],
        "{}",
        out(&o)
    );
    assert!(out(&o).contains("scan"), "tells what to do: {}", out(&o));
    assert_eq!(read(&a).album.as_deref(), Some("Partial"));
    assert_eq!(read(&b).album.as_deref(), Some("Partial"));
    assert_eq!(fs::read(&fresh).unwrap(), untouched.0);
    assert_eq!(fs::read(&elsewhere).unwrap(), untouched.1);
    assert!(!nowhere.exists());
}

#[test]
fn printing_an_unindexed_file_is_refused_with_exit_1() {
    let sb = Sandbox::new(1);
    let fresh = sb.file("fresh.mp3");
    fs::copy(fixture(Kind::Mp3, 3), &fresh).unwrap();
    let o = sb.run(&["tag", s(&fresh), s(&sb.file("t1.mp3"))]);
    assert_code(&o, 1);
    let lines: Vec<String> = out(&o).lines().map(String::from).collect();
    assert!(lines[0].starts_with("refused"), "{lines:?}");
    assert_eq!(
        lines[1],
        s(&sb.file("t1.mp3")),
        "the indexed one is still shown"
    );
    assert_eq!(printed(&o, "title"), "Title1");
}

#[test]
fn a_refused_file_is_not_changed_and_leaves_no_temporary_file() {
    let sb = Sandbox::new(2);
    let (a, b) = (sb.file("t1.mp3"), sb.file("t2.mp3"));
    // `a` changed after the scan; `b` did not.
    let mut bytes = fs::read(&a).unwrap();
    bytes.push(0);
    fs::write(&a, &bytes).unwrap();
    let o = sb.run(&["tag", s(&a), s(&b), "--genre", "Soul"]);
    assert_code(&o, 1);
    assert_eq!(
        results(&o).iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        ["refused", "written"]
    );
    assert!(out(&o).to_lowercase().contains("scan"), "{}", out(&o));
    assert_eq!(fs::read(&a).unwrap(), bytes);
    assert_eq!(read(&b).genre.as_deref(), Some("Soul"));
    let hidden: Vec<_> = fs::read_dir(&sb.music)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect();
    assert!(hidden.is_empty(), "{hidden:?}");
}

#[test]
fn a_relative_path_is_taken_from_the_current_folder() {
    let sb = Sandbox::new(1);
    let o = sb
        .command(&["tag", "t1.mp3", "--album", "Relative"])
        .current_dir(&sb.music)
        .output()
        .unwrap();
    assert_ok(&o);
    assert_eq!(results(&o)[0].0, "written");
    assert_eq!(read(&sb.file("t1.mp3")).album.as_deref(), Some("Relative"));
    let o = sb
        .command(&["tag", "./nothing.mp3", "--album", "X"])
        .current_dir(&sb.music)
        .output()
        .unwrap();
    assert_code(&o, 1);
}

#[test]
fn a_fairplay_file_is_refused_with_its_reason() {
    let sb = Sandbox::new(1);
    let drm = sb.file("bought.m4a");
    fs::write(&drm, common::fairplay_m4a(1)).unwrap();
    assert_ok(&sb.run(&["scan"]));
    let before = fs::read(&drm).unwrap();
    let o = sb.run(&["tag", s(&drm), "--album", "X"]);
    assert_code(&o, 1);
    assert_eq!(results(&o)[0].0, "refused");
    assert!(out(&o).to_lowercase().contains("fairplay"), "{}", out(&o));
    assert_eq!(fs::read(&drm).unwrap(), before);
}

#[test]
fn no_file_is_a_usage_error() {
    let sb = Sandbox::new(1);
    assert_code(&sb.run(&["tag"]), 2);
    assert_code(&sb.run(&["tag", "--album", "X"]), 2);
}

#[test]
fn a_failing_index_update_is_said_after_the_file_lines_and_fails() {
    let sb = Sandbox::new(3);
    let files = [sb.file("t1.mp3"), sb.file("t2.mp3"), sb.file("t3.mp3")];
    let db = fs::read_dir(sb.base().join("data"))
        .unwrap()
        .map(|e| e.unwrap().path().join("iprune.sqlite"))
        .find(|p| p.exists())
        .expect("the index file");
    {
        // A trigger in the throwaway index makes the second update fail.
        let store = iprune_core::store::Store::open(&db).unwrap();
        let id: i64 = store
            .conn()
            .query_row("SELECT id FROM tracks WHERE path LIKE '%t2.mp3'", [], |r| {
                r.get(0)
            })
            .unwrap();
        store
            .conn()
            .execute_batch(&format!(
                "CREATE TRIGGER boom BEFORE UPDATE ON tracks WHEN OLD.id = {id} \
                 BEGIN SELECT RAISE(ABORT, 'boom'); END;"
            ))
            .unwrap();
    }
    let third = fs::read(&files[2]).unwrap();
    let o = sb.run(&[
        "tag",
        s(&files[0]),
        s(&files[1]),
        s(&files[2]),
        "--album",
        "Indexed",
    ]);
    assert_code(&o, 1);
    // The files written are listed, then the index failure is the error.
    assert_eq!(
        results(&o)
            .iter()
            .map(|r| r.0.as_str())
            .take(2)
            .collect::<Vec<_>>(),
        ["written", "written"],
        "{}",
        out(&o)
    );
    assert!(
        err(&o).contains("index could not be updated") && err(&o).contains("boom"),
        "{}",
        err(&o)
    );
    assert_eq!(read(&files[0]).album.as_deref(), Some("Indexed"));
    assert_eq!(read(&files[1]).album.as_deref(), Some("Indexed"));
    assert_eq!(
        fs::read(&files[2]).unwrap(),
        third,
        "the third is not started"
    );
}
