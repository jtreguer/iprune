//! The autotag client (`import::Autotag`) against the fake `autotag serve`
//! of `tests/common/fake_autotag.py` (technical document, section 7.2,
//! "autotag protocol"). No network and no real autotag.

mod common;

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::import_env::{Imp, fake_pids, process_exists};
use iprune_core::Error;
use iprune_core::import::{Applied, Fields, Query};
use serde_json::{Value, json};

/// The message of an `Error::Autotag`, or a failure of the test.
fn autotag_message<T: std::fmt::Debug>(r: iprune_core::Result<T>) -> String {
    match r {
        Err(Error::Autotag(m)) => m,
        other => panic!("expected Error::Autotag, got {other:?}"),
    }
}

fn lookup_answer(artist: &str, title: &str) -> Value {
    json!({
        "by": "name",
        "query": {"artist": artist, "title": title},
        "duration_ms": 2000,
        "candidates": [{
            "recording": "rec-1", "release": "rel-1", "label": "l", "length_ms": 2000,
            "certain": true, "tags": {"title": title, "artist": artist},
            "destination": "x/y.mp3", "same_title": []
        }]
    })
}

fn our_process_group() -> u32 {
    let stat = fs::read_to_string("/proc/self/stat").unwrap();
    // pid (comm) state ppid pgrp ...; comm may hold spaces, so count from its end.
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    after.split(' ').nth(2).unwrap().parse().unwrap()
}

// -- Starting ----------------------------------------------------------------

#[test]
fn start_reads_the_hello_line() {
    let imp = Imp::new();
    imp.scenario(json!({"hello": {
        "protocol": 1, "formats": ["mp3", "m4a"], "acoustid": true, "fpcalc": false
    }}));
    let a = imp.start().unwrap();
    assert_eq!(a.hello.protocol, 1);
    assert_eq!(a.hello.formats, ["mp3", "m4a"]);
    assert!(a.hello.acoustid && !a.hello.fpcalc);
}

#[test]
fn it_runs_autotag_serve_for_the_root_in_its_own_process_group() {
    let imp = Imp::new();
    let a = imp.start().unwrap();
    // The fake reads --library from its arguments: it found the scenario
    // file next to this root, and wrote its pid file there.
    let (pid, pgid, parent) = fake_pids(imp.music());
    assert_eq!(parent, std::process::id());
    assert_eq!(pgid, pid, "its own group, so Ctrl+C does not reach it");
    assert_ne!(pgid, our_process_group());
    drop(a);
}

#[test]
fn the_root_it_serves_is_import_root_when_set_and_the_first_library_root_otherwise() {
    let mut imp = Imp::new();
    // The fake keeps its files in the folder above the root it was given.
    drop(imp.start().unwrap());
    assert!(
        imp.base.join("fake-autotag.pid").exists(),
        "--library music"
    );
    fs::remove_file(imp.base.join("fake-autotag.pid")).unwrap();

    let sub = imp.music().join("Filed");
    fs::create_dir_all(&sub).unwrap();
    imp.env.cfg.import.root = sub;
    drop(imp.start().unwrap());
    assert!(
        imp.music().join("fake-autotag.pid").exists(),
        "--library music/Filed"
    );
    assert!(!imp.base.join("fake-autotag.pid").exists());
}

#[test]
fn another_protocol_version_is_refused_and_the_process_does_not_linger() {
    let imp = Imp::new();
    imp.scenario(json!({"hello": {
        "protocol": 2, "formats": ["mp3"], "acoustid": false, "fpcalc": false
    }}));
    let m = autotag_message(imp.start().map(|_| ()));
    assert!(m.contains("protocol 2"), "{m}");
    assert!(m.contains("packaging/install.sh"), "{m}");
    let (pid, _, _) = fake_pids(imp.music());
    assert!(
        !process_exists(pid),
        "the refused process is ended and reaped"
    );

    // The lock was given back: a good autotag starts now.
    imp.scenario(json!({}));
    imp.start().unwrap();
}

#[test]
fn a_first_line_that_is_not_a_hello_is_refused() {
    let imp = Imp::new();
    imp.scenario(json!({"hello": {"ready": true}}));
    let m = autotag_message(imp.start().map(|_| ()));
    assert!(m.contains("unexpected first line"), "{m}");

    imp.scenario(json!({"hello_raw": "Traceback (most recent call last):"}));
    let m = autotag_message(imp.start().map(|_| ()));
    assert!(m.contains("not JSON") && m.contains("Traceback"), "{m}");
}

#[test]
fn a_process_that_ends_before_its_hello_gives_its_stderr() {
    let imp = Imp::new();
    imp.scenario(json!({
        "quirks": {"start": "exit"},
        "stderr": ["ModuleNotFoundError: No module named 'mutagen'"]
    }));
    let m = autotag_message(imp.start().map(|_| ()));
    assert!(m.contains("stopped"), "{m}");
    assert!(m.contains("exit status: 2"), "{m}");
    assert!(m.contains("No module named 'mutagen'"), "{m}");
}

#[test]
fn a_program_that_cannot_be_run_is_reported_with_its_name() {
    let mut imp = Imp::new();
    let missing = imp.base.join("no-such-autotag");
    imp.env.cfg.import.autotag = missing.clone();
    let m = autotag_message(imp.start().map(|_| ()));
    assert!(
        m.contains(missing.to_str().unwrap()) && m.contains("could not start"),
        "{m}"
    );

    // The lock is free again.
    imp.env.cfg.import.autotag = common::import_env::fake_autotag();
    imp.start().unwrap();
}

#[test]
fn no_library_root_to_serve_is_an_error_not_a_spawn() {
    let mut imp = Imp::new();
    imp.env.cfg.library.roots.clear();
    let m = autotag_message(imp.start().map(|_| ()));
    assert!(m.contains("no library root"), "{m}");
    assert!(imp.requests().is_empty());
}

// -- import.lock -------------------------------------------------------------

#[test]
fn a_second_autotag_is_refused_while_the_first_lives_and_allowed_once_it_is_dropped() {
    let imp = Imp::new();
    let first = imp.start().unwrap();
    let (pid, _, _) = fake_pids(imp.music());

    let started = Instant::now();
    let second = imp.start();
    assert!(
        matches!(second, Err(Error::ImportBusy)),
        "{:?}",
        second.map(|_| ())
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "it waits briefly, not forever"
    );
    assert_eq!(
        fake_pids(imp.music()).0,
        pid,
        "no second process was started"
    );
    assert!(imp.paths.data_dir.join("import.lock").is_file());

    drop(first);
    let third = imp.start().expect("the lock is free again");
    assert_ne!(fake_pids(imp.music()).0, pid);
    drop(third);
}

#[test]
fn the_import_lock_is_not_the_ipod_lock() {
    let imp = Imp::new();
    let _a = imp.start().unwrap();
    let _ipod = iprune_core::lock::ipod(&imp.paths).expect("a sync is not blocked by an import");
}

// -- Dropping ----------------------------------------------------------------

#[test]
fn dropping_ends_the_process_without_leaving_a_zombie() {
    let imp = Imp::new();
    let a = imp.start().unwrap();
    let (pid, _, _) = fake_pids(imp.music());
    assert!(process_exists(pid));
    let started = Instant::now();
    drop(a);
    assert!(
        !process_exists(pid),
        "gone from /proc, so it was waited for and not left a zombie"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a process that follows end of input needs no kill: {:?}",
        started.elapsed()
    );
}

#[test]
fn a_process_that_ignores_end_of_input_is_killed_after_two_seconds() {
    let imp = Imp::new();
    imp.scenario(json!({"hang_on_eof": true}));
    let a = imp.start().unwrap();
    let (pid, _, _) = fake_pids(imp.music());
    let started = Instant::now();
    drop(a);
    let took = started.elapsed();
    assert!(!process_exists(pid), "killed and reaped");
    assert!(
        took >= Duration::from_millis(1900) && took < Duration::from_secs(10),
        "grace of 2 s, then the kill: {took:?}"
    );
}

// -- Requests and ids --------------------------------------------------------

#[test]
fn requests_are_numbered_from_one_and_the_answers_carry_no_id_on() {
    let imp = Imp::new();
    let file = imp.put("a.mp3", 2);
    imp.scenario(json!({
        "lookups": {"a.mp3": lookup_answer("A", "T")},
        "tags": {"rec-1": {"title": "T", "genre": "Pop"}},
    }));
    let mut a = imp.start().unwrap();
    let (lookup, text) = a.lookup(&file, None).unwrap().unwrap();
    let tags = a.tags("rec-1", "rel-1").unwrap().unwrap();
    let dry = a
        .apply(&file, &Fields::default(), None, true, false)
        .unwrap()
        .unwrap();

    let ids: Vec<Value> = imp.requests().iter().map(|r| r["id"].clone()).collect();
    assert_eq!(ids, [json!(1), json!(2), json!(3)]);
    let ops: Vec<Value> = imp.requests().iter().map(|r| r["op"].clone()).collect();
    assert_eq!(ops, ["lookup", "tags", "apply"]);

    // The id is not part of what is stored or returned.
    let stored: Value = serde_json::from_str(&text).unwrap();
    assert!(stored.get("id").is_none(), "{text}");
    assert_eq!(lookup.candidates[0].recording, "rec-1");
    assert_eq!(tags.genre.as_deref(), Some("Pop"));
    assert!(matches!(dry, Applied::WouldFile { .. }));
}

#[test]
fn a_lookup_sends_the_file_and_only_a_search_that_has_text() {
    let imp = Imp::new();
    let file = imp.put("a.mp3", 2);
    imp.scenario(json!({"lookups": {"a.mp3": lookup_answer("A", "T")}}));
    let mut a = imp.start().unwrap();
    a.lookup(&file, None).unwrap().unwrap();
    a.lookup(
        &file,
        Some(&Query {
            artist: "  a-ha ".into(),
            title: "Take On Me".into(),
        }),
    )
    .unwrap()
    .unwrap();
    // A blank half is left out, so autotag guesses it; a blank search is none.
    a.lookup(
        &file,
        Some(&Query {
            artist: "".into(),
            title: "Only Title".into(),
        }),
    )
    .unwrap()
    .unwrap();
    a.lookup(
        &file,
        Some(&Query {
            artist: " ".into(),
            title: "".into(),
        }),
    )
    .unwrap()
    .unwrap();

    let reqs = imp.requests();
    let file = file.to_str().unwrap();
    let strip = |r: &Value| {
        let mut r = r.clone();
        r.as_object_mut().unwrap().remove("id");
        r
    };
    assert_eq!(strip(&reqs[0]), json!({"op": "lookup", "file": file}));
    assert_eq!(
        strip(&reqs[1]),
        json!({"op": "lookup", "file": file, "artist": "a-ha", "title": "Take On Me"})
    );
    assert_eq!(
        strip(&reqs[2]),
        json!({"op": "lookup", "file": file, "title": "Only Title"})
    );
    assert_eq!(strip(&reqs[3]), json!({"op": "lookup", "file": file}));
}

#[test]
fn apply_sends_the_tags_the_flags_and_the_ids_only_when_there_are_some() {
    let imp = Imp::new();
    let file = imp.put("a.mp3", 2);
    let mut a = imp.start().unwrap();
    let fields = Fields {
        title: Some("Take On Me".into()),
        artist: Some("a-ha".into()),
        year: Some(1985),
        track_no: Some(3),
        compilation: Some(false),
        ..Fields::default()
    };
    a.apply(&file, &fields, Some(("rec", "rel")), true, false)
        .unwrap()
        .unwrap();
    a.apply(&file, &fields, None, true, true).unwrap().unwrap();

    let reqs = imp.requests();
    let f = file.to_str().unwrap();
    assert_eq!(
        reqs[0],
        json!({
            "op": "apply", "file": f, "id": 1, "dry_run": true, "allow_duplicate": false,
            "recording": "rec", "release": "rel",
            "tags": {"title": "Take On Me", "artist": "a-ha", "year": 1985, "track_no": 3,
                     "compilation": false}
        })
    );
    assert_eq!(
        reqs[1],
        json!({
            "op": "apply", "file": f, "id": 2, "dry_run": true, "allow_duplicate": true,
            "tags": {"title": "Take On Me", "artist": "a-ha", "year": 1985, "track_no": 3,
                     "compilation": false}
        })
    );
    assert!(file.exists(), "a dry run files nothing");
}

#[test]
fn a_path_that_is_not_utf8_is_refused_before_anything_is_sent() {
    let imp = Imp::new();
    let mut a = imp.start().unwrap();
    let bad = PathBuf::from(OsStr::from_bytes(b"/tmp/bad\xff.mp3"));
    for r in [
        a.lookup(&bad, None).map(|_| ()),
        a.apply(&bad, &Fields::default(), None, false, false)
            .map(|_| ()),
    ] {
        assert!(
            matches!(r, Err(Error::NotUtf8(ref p)) if *p == bad),
            "{r:?}"
        );
    }
    assert!(imp.requests().is_empty());
}

// -- Answers -----------------------------------------------------------------

#[test]
fn an_error_answer_fails_only_its_request() {
    let imp = Imp::new();
    let file = imp.put("a.mp3", 2);
    imp.scenario(json!({
        "lookups": {"a.mp3": lookup_answer("A", "T"), "odd.mp3": {"error": 5}},
    }));
    let mut a = imp.start().unwrap();

    // lookup: the file is not there.
    let gone = imp.inbox.join("absent.mp3");
    let e = a.lookup(&gone, None).unwrap().unwrap_err();
    assert!(
        e.contains("no such file") && e.contains("absent.mp3"),
        "{e}"
    );

    // tags: unknown recording.
    let e = a.tags("nope", "nope").unwrap().unwrap_err();
    assert!(e.contains("not found"), "{e}");

    // apply: the file is not there.
    let r = a
        .apply(&gone, &Fields::default(), None, false, false)
        .unwrap();
    assert!(r.unwrap_err().contains("no such file"));

    // An error that is not text is shown as JSON, not lost.
    let odd = imp.inbox.join("odd.mp3");
    fs::write(&odd, b"x").unwrap();
    assert_eq!(a.lookup(&odd, None).unwrap().unwrap_err(), "5");

    // And the same process still answers.
    let (l, _) = a.lookup(&file, None).unwrap().unwrap();
    assert_eq!(l.candidates.len(), 1);
    assert_eq!(imp.requests().len(), 5, "every request reached it");
}

#[test]
fn a_refusal_and_a_filing_are_answers_of_apply_and_the_filing_really_happened() {
    let imp = Imp::new();
    let a_file = imp.put("a.mp3", 2);
    let b_file = imp.put("b.mp3", 3);
    imp.scenario(json!({
        "destinations": {"a.mp3": "A/Album/01 A.mp3", "b.mp3": "A/Album/01 A.mp3"},
    }));
    let mut a = imp.start().unwrap();
    let fields = Fields {
        title: Some("A".into()),
        ..Fields::default()
    };
    let first = a
        .apply(&a_file, &fields, None, false, false)
        .unwrap()
        .unwrap();
    assert_eq!(
        first,
        Applied::Filed {
            destination: "A/Album/01 A.mp3".into()
        }
    );
    assert!(imp.music().join("A/Album/01 A.mp3").is_file());
    assert!(!a_file.exists(), "the inbox file went last");

    let second = a
        .apply(&b_file, &fields, None, false, false)
        .unwrap()
        .unwrap();
    match second {
        Applied::Refused {
            reason,
            destination,
            ..
        } => {
            assert!(reason.contains("already in the library"), "{reason}");
            assert_eq!(destination.as_deref(), Some("A/Album/01 A.mp3"));
        }
        other => panic!("{other:?}"),
    }
    assert!(b_file.exists(), "a refusal leaves the inbox file");
}

#[test]
fn an_answer_with_the_wrong_id_is_autotag_failing_not_an_answer() {
    let imp = Imp::new();
    let file = imp.put("a.mp3", 2);
    imp.scenario(json!({"quirks": {"2": "wrong_id"}}));
    let mut a = imp.start().unwrap();
    a.lookup(&file, None).unwrap().unwrap();
    let m = autotag_message(a.lookup(&file, None));
    assert!(m.contains("answered request 2 with id 102"), "{m}");
}

#[test]
fn an_answer_with_no_id_is_autotag_failing() {
    let imp = Imp::new();
    let file = imp.put("a.mp3", 2);
    imp.scenario(json!({"quirks": {"1": "no_id"}}));
    let mut a = imp.start().unwrap();
    let m = autotag_message(a.lookup(&file, None));
    assert!(m.contains("answered request 1 with id null"), "{m}");
}

#[test]
fn a_line_that_is_not_json_is_autotag_failing_with_what_it_said() {
    let imp = Imp::new();
    let file = imp.put("a.mp3", 2);
    imp.scenario(json!({"quirks": {"1": "garbage"}, "stderr": []}));
    let mut a = imp.start().unwrap();
    let m = autotag_message(a.lookup(&file, None));
    assert!(
        m.contains("not JSON") && m.contains("this is not json"),
        "{m}"
    );
}

#[test]
fn a_lookup_answer_of_the_wrong_shape_is_autotag_failing() {
    let imp = Imp::new();
    let file = imp.put("a.mp3", 2);
    imp.scenario(json!({"lookups": {"a.mp3": {"by": "magic"}}}));
    let mut a = imp.start().unwrap();
    let m = autotag_message(a.lookup(&file, None));
    assert!(m.contains("unexpected answer to lookup"), "{m}");
}

#[test]
fn a_process_that_ends_mid_request_gives_its_status_and_stderr_and_stays_failed() {
    let imp = Imp::new();
    let file = imp.put("a.mp3", 2);
    imp.scenario(json!({
        "lookups": {"a.mp3": lookup_answer("A", "T")},
        "quirks": {"lookup:a.mp3": "exit"},
        "stderr": ["Traceback (most recent call last):", "RuntimeError: out of memory"]
    }));
    let mut a = imp.start().unwrap();
    let m = autotag_message(a.lookup(&file, None));
    assert!(m.contains("stopped"), "{m}");
    assert!(m.contains("exit status: 3"), "{m}");
    assert!(m.contains("RuntimeError: out of memory"), "{m}");
    assert!(m.contains("Traceback"), "{m}");
    // Asking again fails again, with Error::Autotag and not a panic.
    let m2 = autotag_message(a.lookup(&file, None));
    assert!(!m2.is_empty());
    assert!(
        a.apply(&file, &Fields::default(), None, false, false)
            .is_err()
    );
    assert!(file.exists());
}

#[test]
fn only_the_last_twenty_lines_of_stderr_are_kept() {
    let imp = Imp::new();
    let file = imp.put("a.mp3", 2);
    let lines: Vec<String> = (1..=30).map(|n| format!("stderr line {n:02}")).collect();
    imp.scenario(json!({"quirks": {"1": "exit"}, "stderr": lines}));
    let mut a = imp.start().unwrap();
    let m = autotag_message(a.lookup(&file, None));
    assert!(
        m.contains("stderr line 30") && m.contains("stderr line 11"),
        "{m}"
    );
    assert!(!m.contains("stderr line 10"), "{m}");
    assert!(
        m.find("line 11").unwrap() < m.find("line 30").unwrap(),
        "in order: {m}"
    );
}

#[test]
fn dropping_kills_the_whole_process_group_when_autotag_runs_python_as_a_child() {
    // As `uv run` does: the process we started only waits for python, which
    // is its child, and python ignores the end of input.
    let imp = Imp::new();
    imp.scenario(json!({"wrapper": true, "hang_on_eof": true}));
    let a = imp.start().unwrap();
    let dir = common::import_env::scenario_dir(imp.music());
    let wrapper: Vec<u32> = fs::read_to_string(dir.join("fake-autotag.wrapper.pid"))
        .unwrap()
        .split_whitespace()
        .map(|n| n.parse().unwrap())
        .collect();
    let (wrapper_pid, wrapper_group) = (wrapper[0], wrapper[1]);
    let (inner, inner_group, inner_parent) = fake_pids(imp.music());
    assert_eq!(inner_parent, wrapper_pid, "python is the wrapper's child");
    assert_ne!(inner, wrapper_pid);
    assert_eq!(inner_group, wrapper_group, "one group, led by the wrapper");
    assert_eq!(wrapper_group, wrapper_pid);
    assert!(process_exists(wrapper_pid) && process_exists(inner));

    drop(a);
    // The orphan is reaped by init: give it a moment.
    let deadline = Instant::now() + Duration::from_secs(3);
    while process_exists(inner) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!process_exists(wrapper_pid), "the wrapper is gone");
    assert!(
        !process_exists(inner),
        "python is gone too, not left running"
    );
}
