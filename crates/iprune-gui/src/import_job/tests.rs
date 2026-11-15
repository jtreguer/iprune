//! The Import view's workers against a temp library, a temp inbox and the
//! fake autotag of the core crate's tests (no network, no real autotag, no
//! real home). The jobs open their own connection to the index: `paths` is
//! pointed at the database the test's `Env` already uses.

#[allow(clippy::duplicate_mod)] // the fixture helpers are shared with the other tests
#[path = "../../../iprune-core/tests/common/mod.rs"]
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use common::import_env::{Imp, answer, candidate, dest, process_exists, scenario, scenario_dir};
use iprune_core::import::{FileReport, Filed, Status};
use iprune_core::scan::ScanReport;
use serde_json::{Value, json};

use super::*;

/// The library, inbox and fake autotag of `Imp`, with the jobs' paths on the
/// index its `Env` uses.
fn world() -> Imp {
    let mut imp = Imp::new();
    imp.paths.data_dir = imp.env.music.parent().unwrap().join("db");
    imp.env.scan();
    imp
}

fn no_progress() -> impl Fn(usize, usize, &str) + Sync {
    |_, _, _| {}
}

fn slot() -> Mutex<Option<Killer>> {
    Mutex::new(None)
}

fn fake_started(imp: &Imp) -> bool {
    scenario_dir(imp.music()).join("fake-autotag.pid").exists()
}

fn looked_up_with(imp: &Imp, rel: &str, candidates: Vec<Value>) {
    imp.read();
    import::set_lookup(
        imp.env.store.conn(),
        imp.item_id(rel),
        &answer(candidates).to_string(),
    )
    .unwrap();
}

fn names(rows: &[ImportRow]) -> Vec<&str> {
    rows.iter().map(|r| r.file.as_str()).collect()
}

// -------------------------------------------------------------------- read

#[test]
fn read_gives_the_rows_in_the_views_order_and_the_paths_it_could_not_read() {
    let imp = world();
    imp.put("a-ready.mp3", 2);
    imp.put("b-choose.mp3", 3);
    imp.put("c-waiting.mp3", 4);
    imp.read();
    looked_up_with(
        &imp,
        "a-ready.mp3",
        vec![candidate("r2", "Title2", "Artist2", true, "x.mp3")],
    );
    looked_up_with(
        &imp,
        "b-choose.mp3",
        vec![candidate("r3", "Title3", "Artist3", false, "y.mp3")],
    );
    std::os::unix::fs::symlink("/etc/hostname", imp.inbox.join("link.mp3")).unwrap();

    let listing = read(&imp.env.cfg, &imp.paths).unwrap();
    // Choose first, then the one still waiting, then the ready one.
    assert_eq!(
        names(&listing.rows),
        ["b-choose.mp3", "c-waiting.mp3", "a-ready.mp3"]
    );
    assert_eq!(listing.rows[0].status, Status::Choose);
    assert_eq!(listing.rows[1].status, Status::LookingUp);
    assert!(listing.rows[2].ready());
    assert_eq!(listing.unreadable.len(), 1);
    assert_eq!(listing.unreadable[0].0, imp.inbox.join("link.mp3"));
    assert!(
        listing.unreadable[0].1.contains("symbolic link"),
        "{:?}",
        listing.unreadable
    );
    // It made the rows: a second read through another connection agrees.
    assert_eq!(imp.item_id("a-ready.mp3"), listing.rows[2].item.unwrap());
}

#[test]
fn read_of_an_inbox_with_nothing_in_it_is_empty_and_makes_the_folder() {
    let imp = world();
    assert!(!imp.inbox.exists());
    let listing = read(&imp.env.cfg, &imp.paths).unwrap();
    assert!(listing.rows.is_empty() && listing.unreadable.is_empty());
    assert!(imp.inbox.is_dir());
}

#[test]
fn read_marks_a_file_identical_to_a_library_file_as_already_there() {
    let imp = world();
    fs::create_dir_all(&imp.inbox).unwrap();
    fs::copy(&imp.seed, imp.inbox.join("same.mp3")).unwrap();
    let listing = read(&imp.env.cfg, &imp.paths).unwrap();
    assert!(
        matches!(listing.rows[0].status, Status::AlreadyInLibrary(_)),
        "{:?}",
        listing.rows[0].status
    );
}

// ------------------------------------------------------------------- count

#[test]
fn count_adds_up_the_files_in_the_inbox_and_its_folders_but_not_hidden_ones() {
    let imp = world();
    assert_eq!(count(&imp.env.cfg), 0, "no inbox yet");
    assert!(!imp.inbox.exists(), "counting does not make the folder");

    imp.put("a.mp3", 2);
    imp.put("sub/b.mp3", 3);
    imp.put("sub/deep/er/c.mp3", 4);
    imp.put(".hidden.mp3", 5);
    imp.put(".hidden-folder/d.mp3", 6);
    imp.put("sub/.also-hidden.mp3", 7);
    imp.put("sub/.hidden-inside/e.mp3", 8);
    fs::write(imp.inbox.join("notes.txt"), "x").unwrap(); // listed too, as not supported
    fs::create_dir_all(imp.inbox.join("empty/folder")).unwrap();
    // A link is not a file it counts (the read lists it as unreadable).
    std::os::unix::fs::symlink(imp.inbox.join("a.mp3"), imp.inbox.join("link.mp3")).unwrap();

    assert_eq!(count(&imp.env.cfg), 4, "a, b, c and notes.txt");
    // The sidebar's number is the number of rows the view shows, hidden
    // files and links apart.
    let listing = read(&imp.env.cfg, &imp.paths).unwrap();
    assert_eq!(listing.rows.len(), 4);
}

#[test]
fn count_of_an_inbox_that_is_a_file_or_unreadable_is_zero() {
    let mut imp = world();
    let not_a_dir = imp.env.music.parent().unwrap().join("inbox-file");
    fs::write(&not_a_dir, "x").unwrap();
    imp.env.cfg.import.inbox = not_a_dir;
    assert_eq!(count(&imp.env.cfg), 0);
}

// --------------------------------------------------------------------- add

#[test]
fn add_copies_files_and_folders_into_the_inbox_and_counts_the_files() {
    let imp = world();
    let src = imp.env.music.parent().unwrap().join("downloads");
    fs::create_dir_all(src.join("Album/CD2")).unwrap();
    fs::copy(
        common::fixture(common::Kind::Mp3, 2),
        src.join("Album/01.mp3"),
    )
    .unwrap();
    fs::copy(
        common::fixture(common::Kind::Mp3, 3),
        src.join("Album/CD2/02.mp3"),
    )
    .unwrap();
    fs::copy(
        common::fixture(common::Kind::Mp3, 4),
        src.join("Album/.hidden.mp3"),
    )
    .unwrap();
    fs::copy(
        common::fixture(common::Kind::Mp3, 5),
        src.join("single.mp3"),
    )
    .unwrap();

    let n = add(&imp.env.cfg, &[src.join("Album"), src.join("single.mp3")]).unwrap();
    assert_eq!(n, 3);
    assert_eq!(
        imp.inbox_files(),
        ["Album/01.mp3", "Album/CD2/02.mp3", "single.mp3"]
    );
    assert_eq!(count(&imp.env.cfg), 3);
    // The originals stay.
    assert!(src.join("single.mp3").is_file() && src.join("Album/01.mp3").is_file());

    // The same again: nothing is replaced, the copies are numbered.
    let before = fs::read(imp.inbox.join("single.mp3")).unwrap();
    assert_eq!(add(&imp.env.cfg, &[src.join("single.mp3")]).unwrap(), 1);
    assert_eq!(fs::read(imp.inbox.join("single.mp3")).unwrap(), before);
    assert!(imp.inbox.join("single (2).mp3").is_file());
}

#[test]
fn add_of_a_file_that_does_not_exist_is_an_error_naming_what_it_was_doing() {
    let imp = world();
    let err = add(
        &imp.env.cfg,
        &[PathBuf::from("/nonexistent/iprune-test/a.mp3")],
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("inbox"), "{err:#}");
    assert!(imp.inbox_files().is_empty());
}

// ----------------------------------------------------------------- look_up

#[test]
fn look_up_starts_no_autotag_when_nothing_is_waiting() {
    let imp = world();
    imp.scenario(scenario(&[]));
    // An empty inbox.
    let killer = slot();
    let got = look_up(
        &imp.env.cfg,
        &imp.paths,
        &no_progress(),
        &AtomicBool::new(false),
        &killer,
    )
    .unwrap();
    assert_eq!(got, Looked::default());
    // A file already looked up, one whose lookup failed, and one in the
    // library: none is pending.
    imp.put("a.mp3", 2);
    imp.put("b.mp3", 3);
    fs::copy(&imp.seed, imp.inbox.join("same.mp3")).unwrap();
    looked_up_with(
        &imp,
        "a.mp3",
        vec![candidate("r", "Title2", "Artist2", true, "x.mp3")],
    );
    import::set_error(imp.env.store.conn(), imp.item_id("b.mp3"), Some("failed")).unwrap();
    let got = look_up(
        &imp.env.cfg,
        &imp.paths,
        &no_progress(),
        &AtomicBool::new(false),
        &killer,
    )
    .unwrap();
    assert_eq!(got, Looked::default());
    assert!(!fake_started(&imp), "autotag was started for nothing");
    assert!(imp.requests().is_empty());
    assert!(killer.lock().unwrap().is_none());
}

#[test]
fn look_up_looks_up_the_waiting_files_then_fetches_the_genres_of_the_matches_chosen() {
    let imp = world();
    imp.put("a.mp3", 2);
    imp.put("b.mp3", 3);
    // b.mp3 was looked up before; its second candidate was chosen, and its
    // genre is not known.
    let mut second = candidate("r3b", "Live", "Artist3", false, "L/3.mp3");
    second["tags"]["genre"] = Value::Null;
    looked_up_with(
        &imp,
        "b.mp3",
        vec![candidate("r3", "Title3", "Artist3", true, &dest(3)), second],
    );
    import::set_choice(imp.env.store.conn(), imp.item_id("b.mp3"), Some(1)).unwrap();
    let mut sc = scenario(&[("a.mp3", 2)]);
    sc["tags"] = json!({"r3b": {"title": "Live", "artist": "Artist3", "genre": "Live Rock"}});
    imp.scenario(sc);

    let seen = Mutex::new(Vec::new());
    let killer = slot();
    let progress = |n: usize, total: usize, file: &str| {
        // The handle is in the slot from before the first request.
        assert!(
            killer.lock().unwrap().is_some(),
            "no killer while autotag runs"
        );
        seen.lock().unwrap().push((n, total, file.to_owned()));
    };
    let got = look_up(
        &imp.env.cfg,
        &imp.paths,
        &progress,
        &AtomicBool::new(false),
        &killer,
    )
    .unwrap();

    assert_eq!(
        got,
        Looked {
            looked_up: 1,
            genres: 1,
            stopped: false
        }
    );
    assert_eq!(*seen.lock().unwrap(), [(0, 1, "a.mp3".to_owned())]);
    let ops: Vec<String> = imp
        .requests()
        .iter()
        .map(|r| r["op"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ops, ["lookup", "tags"], "lookups first, then the genres");
    assert!(
        killer.lock().unwrap().is_none(),
        "the slot is emptied when it is done"
    );

    // What was asked for is stored.
    let a = imp.item("a.mp3");
    assert!(a.lookup.is_some() && a.error.is_none());
    let b = import::Lookup::parse(imp.item("b.mp3").lookup.as_deref().unwrap()).unwrap();
    assert_eq!(b.candidates[1].tags.genre.as_deref(), Some("Live Rock"));
    assert!(!b.candidates[1].genre_unknown);
    // The fake autotag is gone with the call.
    let (pid, _, _) = common::import_env::fake_pids(imp.music());
    assert!(!process_exists(pid));
}

#[test]
fn look_up_with_only_genres_to_fetch_starts_autotag_for_them() {
    let imp = world();
    imp.put("a.mp3", 2);
    let mut second = candidate("r2b", "Live", "Artist2", false, "L/2.mp3");
    second["tags"]["genre"] = Value::Null;
    looked_up_with(
        &imp,
        "a.mp3",
        vec![candidate("r2", "Title2", "Artist2", true, "x"), second],
    );
    import::set_choice(imp.env.store.conn(), imp.item_id("a.mp3"), Some(1)).unwrap();
    let mut sc = scenario(&[]);
    sc["tags"] = json!({"r2b": {"title": "Live", "artist": "Artist2", "genre": "Rock"}});
    imp.scenario(sc);
    let got = look_up(
        &imp.env.cfg,
        &imp.paths,
        &no_progress(),
        &AtomicBool::new(false),
        &slot(),
    )
    .unwrap();
    assert_eq!(
        got,
        Looked {
            looked_up: 0,
            genres: 1,
            stopped: false
        }
    );
}

// Stop on a lookup that does not end by itself: the call returns as stopped,
// not as an error, and what was looked up before it stays.
#[test]
fn look_up_killed_by_stop_returns_stopped_keeps_earlier_answers_and_empties_the_slot() {
    let imp = world();
    imp.put("a.mp3", 2);
    imp.put("b.mp3", 3);
    let mut sc = scenario(&[("a.mp3", 2), ("b.mp3", 3)]);
    sc["quirks"] = json!({"lookup:b.mp3": "hang"});
    imp.scenario(sc);

    let stop = AtomicBool::new(false);
    let killer = slot();
    let root = imp.music().to_owned();
    let began = Instant::now();
    let (got, killed_at) = std::thread::scope(|s| {
        let stopper = s.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(10);
            // b.mp3's request is the second one: autotag is hanging on it.
            while common::import_env::requests(&root).len() < 2 {
                assert!(Instant::now() < deadline, "the second lookup never started");
                std::thread::sleep(Duration::from_millis(5));
            }
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            let k = killer
                .lock()
                .unwrap()
                .clone()
                .expect("a killer while it runs");
            k.kill();
            Instant::now()
        });
        let got = look_up(&imp.env.cfg, &imp.paths, &no_progress(), &stop, &killer);
        (got, stopper.join().unwrap())
    });
    let got = got.expect("a stop is not an error");
    assert!(got.stopped);
    assert_eq!(got.genres, 0);
    assert!(
        killed_at.elapsed() < Duration::from_secs(3) && began.elapsed() < Duration::from_secs(4),
        "it waited for the hanging lookup to give up on its own"
    );
    assert!(killer.lock().unwrap().is_none());

    // a.mp3's answer is kept; b.mp3 has neither a lookup nor an error, so it
    // is looked up again next time.
    let a = imp.item("a.mp3");
    assert!(a.lookup.is_some(), "kept what was looked up before");
    let b = imp.item("b.mp3");
    assert_eq!((b.lookup, b.error), (None, None));
    let (pid, _, _) = common::import_env::fake_pids(imp.music());
    assert!(!process_exists(pid), "reaped");
}

#[test]
fn look_up_stopped_between_files_by_the_flag_alone_ends_with_stopped_true() {
    let imp = world();
    imp.put("a.mp3", 2);
    imp.put("b.mp3", 3);
    imp.scenario(scenario(&[("a.mp3", 2), ("b.mp3", 3)]));
    let got = look_up(
        &imp.env.cfg,
        &imp.paths,
        &no_progress(),
        &AtomicBool::new(true),
        &slot(),
    )
    .unwrap();
    assert!(got.stopped);
    assert_eq!(got.looked_up, 0);
    assert!(imp.requests_of("lookup").is_empty(), "asked for nothing");
    assert_eq!(imp.item("a.mp3").lookup, None);
}

#[test]
fn look_up_when_autotag_dies_is_an_error_empties_the_slot_and_keeps_earlier_answers() {
    let imp = world();
    imp.put("a.mp3", 2);
    imp.put("b.mp3", 3);
    let mut sc = scenario(&[("a.mp3", 2), ("b.mp3", 3)]);
    sc["quirks"] = json!({"lookup:b.mp3": "exit"});
    imp.scenario(sc);
    let killer = slot();
    let err = look_up(
        &imp.env.cfg,
        &imp.paths,
        &no_progress(),
        &AtomicBool::new(false),
        &killer,
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("autotag"), "{err:#}");
    assert!(killer.lock().unwrap().is_none());
    assert!(imp.item("a.mp3").lookup.is_some());
    let b = imp.item("b.mp3");
    assert_eq!(
        (b.lookup, b.error),
        (None, None),
        "autotag dying is not b's fault"
    );
}

#[test]
fn look_up_with_an_autotag_that_cannot_start_is_an_error_and_leaves_the_slot_empty() {
    let mut imp = world();
    imp.put("a.mp3", 2);
    imp.env.cfg.import.autotag = PathBuf::from("/nonexistent/iprune-test/autotag");
    let killer = slot();
    let err = look_up(
        &imp.env.cfg,
        &imp.paths,
        &no_progress(),
        &AtomicBool::new(false),
        &killer,
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("autotag"), "{err:#}");
    assert!(killer.lock().unwrap().is_none());
}

// -------------------------------------------------------------------- file

fn ready(imp: &Imp, rel: &str, n: u32) {
    imp.put(rel, n);
}

#[test]
fn file_refuses_a_missing_library_before_autotag_starts() {
    let mut imp = world();
    ready(&imp, "a.mp3", 2);
    imp.scenario(scenario(&[("a.mp3", 2)]));

    // A root that is not there.
    imp.env.cfg.library.roots = vec![imp.env.music.parent().unwrap().join("unmounted")];
    let err = file(
        &imp.env.cfg,
        &imp.paths,
        None,
        &no_progress(),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("unmounted"), "{err:#}");
    // A root that is there but empty: an unmounted disk's mount point.
    let empty = imp.env.music.parent().unwrap().join("empty-root");
    fs::create_dir(&empty).unwrap();
    imp.env.cfg.library.roots = vec![empty.clone()];
    let err = file(
        &imp.env.cfg,
        &imp.paths,
        None,
        &no_progress(),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("empty-root"), "{err:#}");

    assert!(
        !fake_started(&imp),
        "autotag started although the library is gone"
    );
    assert!(imp.inbox.join("a.mp3").is_file(), "the inbox file stays");
    assert_eq!(
        fs::read_dir(&empty).unwrap().count(),
        0,
        "nothing filed there"
    );
    assert_eq!(imp.library(), ["Seed/Seed.mp3"]);
}

#[test]
fn file_files_the_ready_songs_reports_progress_and_adds_them_to_the_playlist() {
    let imp = world();
    imp.env
        .write_playlist("Mix", &format!("#EXTM3U\n{}\n", imp.seed.display()));
    ready(&imp, "sub/a.mp3", 2);
    ready(&imp, "b.mp3", 3);
    imp.scenario(scenario(&[("a.mp3", 2), ("b.mp3", 3)]));
    // Both need their lookup stored to be Ready.
    imp.read();
    for (rel, n) in [("sub/a.mp3", 2), ("b.mp3", 3)] {
        looked_up_with(
            &imp,
            rel,
            vec![candidate(
                &format!("r{n}"),
                &format!("Title{n}"),
                &format!("Artist{n}"),
                true,
                &dest(n),
            )],
        );
    }

    let seen = Mutex::new(Vec::new());
    let progress =
        |n: usize, total: usize, f: &str| seen.lock().unwrap().push((n, total, f.to_owned()));
    let report = file(
        &imp.env.cfg,
        &imp.paths,
        Some("Mix"),
        &progress,
        &AtomicBool::new(false),
    )
    .unwrap();

    assert_eq!(report.filed.len(), 2, "{report:?}");
    // Path order: b.mp3 before sub/a.mp3.
    assert_eq!(
        *seen.lock().unwrap(),
        [(0, 2, "b.mp3".to_owned()), (1, 2, "sub/a.mp3".to_owned())]
    );
    assert_eq!(
        imp.library(),
        [
            "Artist2/Album2/02 Title2.mp3",
            "Artist3/Album3/03 Title3.mp3",
            "Seed/Seed.mp3"
        ]
    );
    assert!(imp.inbox_files().is_empty(), "the inbox files went");
    assert_eq!(report.playlist, Some(Ok("Mix".to_owned())));
    assert!(report.filed.iter().all(|f| f.track.is_some()));
    // What the playlist had, then the songs in the order they were filed.
    let want: Vec<String> = std::iter::once(imp.seed.display().to_string())
        .chain([3, 2].map(|n| imp.music().join(dest(n)).display().to_string()))
        .collect();
    assert_eq!(imp.env.locations("Mix"), want);
}

#[test]
fn file_with_a_stop_already_set_files_nothing() {
    let imp = world();
    ready(&imp, "a.mp3", 2);
    imp.scenario(scenario(&[("a.mp3", 2)]));
    looked_up_with(
        &imp,
        "a.mp3",
        vec![candidate("r2", "Title2", "Artist2", true, &dest(2))],
    );
    let report = file(
        &imp.env.cfg,
        &imp.paths,
        None,
        &no_progress(),
        &AtomicBool::new(true),
    )
    .unwrap();
    assert!(report.stopped);
    assert!(report.filed.is_empty());
    assert!(imp.inbox.join("a.mp3").is_file());
    assert_eq!(imp.library(), ["Seed/Seed.mp3"]);
    assert!(imp.requests_of("apply").is_empty());
}

#[test]
fn file_with_a_playlist_that_does_not_exist_is_an_error_and_files_nothing() {
    let imp = world();
    ready(&imp, "a.mp3", 2);
    imp.scenario(scenario(&[("a.mp3", 2)]));
    looked_up_with(
        &imp,
        "a.mp3",
        vec![candidate("r2", "Title2", "Artist2", true, &dest(2))],
    );
    let r = file(
        &imp.env.cfg,
        &imp.paths,
        Some("No Such List"),
        &no_progress(),
        &AtomicBool::new(false),
    );
    assert!(r.is_err());
    assert!(imp.inbox.join("a.mp3").is_file());
    assert_eq!(imp.library(), ["Seed/Seed.mp3"]);
}

// ---------------------------------------------------------------- describe

fn shown_report(f: impl FnOnce(&mut FileReport)) -> String {
    let mut r = FileReport::default();
    f(&mut r);
    describe(&r, Path::new("/inbox"), Path::new("/music"))
}

fn filed(to: &str, track: Option<i64>) -> Filed {
    Filed {
        from: PathBuf::from("/inbox/x.mp3"),
        to: PathBuf::from("/music").join(to),
        track,
    }
}

#[test]
fn describe_says_nothing_was_imported_for_an_empty_report() {
    let text = shown_report(|_| {});
    assert!(text.starts_with("Nothing was imported"), "{text:?}");
    assert_eq!(text.lines().count(), 1, "{text:?}");
}

#[test]
fn describe_lists_the_filed_songs_relative_to_the_library_in_singular_and_plural() {
    let one = shown_report(|r| r.filed = vec![filed("A/B/01 C.mp3", Some(1))]);
    assert!(one.starts_with("1 song imported"), "{one:?}");
    assert!(one.contains("\n  A/B/01 C.mp3"), "{one:?}");
    assert!(!one.contains("/music"), "{one:?}");
    let two =
        shown_report(|r| r.filed = vec![filed("A/1.mp3", Some(1)), filed("A/2.mp3", Some(2))]);
    assert!(two.starts_with("2 songs imported"), "{two:?}");
    assert!(
        two.contains("\n  A/1.mp3") && two.contains("\n  A/2.mp3"),
        "{two:?}"
    );
}

#[test]
fn describe_gives_each_failure_its_reason_and_its_inbox_path() {
    let text = shown_report(|r| {
        r.failed = vec![
            (
                PathBuf::from("/inbox/sub/a.mp3"),
                "already in the library: X".into(),
            ),
            (PathBuf::from("/inbox/b.mp3"), "it changed".into()),
        ]
    });
    assert!(text.starts_with("Nothing was imported"), "{text:?}");
    assert!(
        text.contains("already in the library: X: sub/a.mp3"),
        "{text:?}"
    );
    assert!(text.contains("it changed: b.mp3"), "{text:?}");
    assert!(!text.contains("/inbox"), "{text:?}");
}

#[test]
fn describe_names_the_fields_that_read_back_otherwise_and_where_to_look() {
    let text = shown_report(|r| {
        r.filed = vec![filed("A/1.mp3", Some(1))];
        r.mismatches = vec![(PathBuf::from("/music/A/1.mp3"), vec!["title", "year"])];
    });
    assert!(text.contains("title, year"), "{text:?}");
    assert!(text.contains("Get Info"), "{text:?}");
    assert!(text.contains("A/1.mp3"), "{text:?}");
}

#[test]
fn describe_says_when_autotag_stopped_or_the_user_did() {
    let died = shown_report(|r| r.error = Some("autotag stopped (exit status: 3)".into()));
    assert!(
        died.contains("part way") && died.contains("exit status: 3"),
        "{died:?}"
    );
    assert!(!died.contains("Stopped:"), "{died:?}");
    let stopped = shown_report(|r| r.stopped = true);
    assert!(
        stopped.contains("Stopped") && stopped.contains("wait in the inbox"),
        "{stopped:?}"
    );
    assert!(!stopped.contains("part way"), "{stopped:?}");
}

#[test]
fn describe_lists_songs_the_scan_did_not_index_in_full() {
    let text = shown_report(|r| {
        r.filed = vec![filed("A/1.mp3", None)];
        r.not_indexed = vec![PathBuf::from("/music/A/1.mp3")];
    });
    assert!(text.contains("Not in the library index"), "{text:?}");
    assert!(text.contains("/music/A/1.mp3"), "{text:?}");
}

#[test]
fn describe_counts_only_the_songs_in_the_index_as_added_to_the_playlist() {
    let added = shown_report(|r| {
        r.filed = vec![
            filed("A/1.mp3", Some(1)),
            filed("A/2.mp3", None),
            filed("A/3.mp3", Some(3)),
        ];
        r.playlist = Some(Ok("Road Trip".into()));
    });
    assert!(added.contains("Added 2 songs to Road Trip"), "{added:?}");
    let one = shown_report(|r| {
        r.filed = vec![filed("A/1.mp3", Some(1))];
        r.playlist = Some(Ok("Mix".into()));
    });
    assert!(one.contains("Added 1 song to Mix"), "{one:?}");
    let refused = shown_report(|r| {
        r.filed = vec![filed("A/1.mp3", Some(1))];
        r.playlist = Some(Err("the playlist file is read only".into()));
    });
    assert!(refused.contains("Not added to the playlist"), "{refused:?}");
    assert!(refused.contains("read only"), "{refused:?}");
    assert!(!refused.contains("Added "), "{refused:?}");
    // No playlist asked for: nothing about one.
    let none = shown_report(|r| r.filed = vec![filed("A/1.mp3", Some(1))]);
    assert!(!none.contains("playlist"), "{none:?}");
}

#[test]
fn describe_ends_with_the_scan_or_says_it_failed() {
    let ok = shown_report(|r| {
        r.filed = vec![filed("A/1.mp3", Some(1))];
        r.scan = Some(Ok(ScanReport {
            added: 7,
            ..ScanReport::default()
        }));
    });
    assert!(ok.contains("7 added"), "{ok:?}");
    assert!(ok.contains("\n\n"), "a blank line before the scan: {ok:?}");
    let failed = shown_report(|r| {
        r.filed = vec![filed("A/1.mp3", None)];
        r.scan = Some(Err("disk on fire".into()));
    });
    assert!(
        failed.contains("scan after it failed") && failed.contains("disk on fire"),
        "{failed:?}"
    );
    assert!(failed.contains("scan again"), "{failed:?}");
}

#[test]
fn describe_orders_filed_then_failures_then_the_playlist_then_the_scan() {
    let text = shown_report(|r| {
        r.filed = vec![filed("A/1.mp3", Some(1))];
        r.failed = vec![(PathBuf::from("/inbox/b.mp3"), "why".into())];
        r.playlist = Some(Ok("Mix".into()));
        r.scan = Some(Ok(ScanReport::default()));
    });
    let at = |s: &str| {
        text.find(s)
            .unwrap_or_else(|| panic!("{s} not in {text:?}"))
    };
    assert!(
        at("imported") < at("b.mp3") && at("b.mp3") < at("Added") && at("Added") < at("library:")
    );
}
