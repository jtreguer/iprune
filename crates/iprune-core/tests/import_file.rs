//! Lookups and filing (`import::pending`, `look_up`, `file`; technical
//! document, section 7.2, "Filing") against the fake autotag of
//! `tests/common/fake_autotag.py`, with ffmpeg audio in temp folders. The
//! fake really files: it copies the file to `<library>/<destination>` with
//! an ID3v2 tag holding the tags it was sent, and deletes the inbox file.

mod common;

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use common::Kind;
use common::import_env::{Imp, candidate, dest, scenario};
use iprune_core::Error;
use iprune_core::import::{self, Fields, Status};
use iprune_core::store::Store;
use serde_json::json;

/// Puts the files in the inbox, writes the scenario and looks everything up.
fn prepare(imp: &Imp, files: &[(&str, u32)]) {
    for (name, n) in files {
        imp.put(name, *n);
    }
    imp.scenario(scenario(files));
    let mut a = imp.start().unwrap();
    imp.look_up_all(&mut a, false);
}

fn statuses(imp: &Imp) -> Vec<(String, Status)> {
    imp.views()
        .into_iter()
        .map(|(n, v)| (n, v.status))
        .collect()
}

fn applied(imp: &Imp) -> Vec<String> {
    imp.requests_of("apply")
        .iter()
        .map(|r| {
            Path::new(r["file"].as_str().unwrap())
                .strip_prefix(&imp.inbox)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

fn rows(imp: &Imp) -> usize {
    import::items(imp.env.store.conn()).unwrap().len()
}

fn playlist_with_seed(imp: &Imp, name: &str) {
    imp.env
        .write_playlist(name, &format!("#EXTM3U\n{}\n", imp.seed.display()));
}

// -- pending and look_up -----------------------------------------------------

#[test]
fn files_already_in_the_library_are_never_looked_up() {
    let imp = Imp::new();
    imp.put("new.mp3", 2);
    // A byte for byte copy of a library file.
    fs::copy(&imp.seed, imp.inbox.join("copy.mp3")).unwrap();
    fs::write(imp.inbox.join("notes.txt"), b"x").unwrap();
    imp.scenario(scenario(&[("new.mp3", 2)]));
    let mut imp = imp;
    imp.env.scan();

    let mut a = imp.start().unwrap();
    assert_eq!(imp.look_up_all(&mut a, false), 1);
    let looked: Vec<_> = imp
        .requests_of("lookup")
        .iter()
        .map(|r| {
            Path::new(r["file"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(looked, ["new.mp3"]);
    assert!(imp.item("copy.mp3").lookup.is_none());
    let s = statuses(&imp);
    assert!(
        s.iter()
            .any(|(n, s)| n == "copy.mp3" && matches!(s, Status::AlreadyInLibrary(_))),
        "{s:?}"
    );
}

#[test]
fn a_file_equal_to_a_missing_track_is_looked_up_like_any_other() {
    let mut imp = Imp::new();
    imp.put("back.mp3", 2);
    imp.scenario(scenario(&[("back.mp3", 2)]));
    fs::copy(imp.inbox.join("back.mp3"), imp.music().join("Old.mp3")).unwrap();
    imp.env.scan();
    fs::remove_file(imp.music().join("Old.mp3")).unwrap();
    imp.env.scan();
    assert!(
        imp.env.tracks().iter().any(|t| t.missing_since.is_some()),
        "set up"
    );

    let mut a = imp.start().unwrap();
    assert_eq!(imp.look_up_all(&mut a, false), 1);
    assert!(imp.item("back.mp3").lookup.is_some());
}

#[test]
fn answers_are_stored_as_they_came_and_survive_closing_and_reopening_the_index() {
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2), ("b.mp3", 3)]);
    let stored = imp.item("a.mp3").lookup.unwrap();
    let parsed = import::Lookup::parse(&stored).unwrap();
    assert_eq!(parsed.candidates[0].recording, "rec-2");
    assert_eq!(
        parsed.candidates[0].destination.as_deref(),
        Some(dest(2).as_str())
    );

    imp.env.store = Store::open(&imp.base.join("db/iprune.sqlite")).unwrap();
    assert_eq!(imp.item("a.mp3").lookup.as_deref(), Some(stored.as_str()));
    assert_eq!(
        statuses(&imp),
        [
            ("a.mp3".into(), Status::Ready),
            ("b.mp3".into(), Status::Ready)
        ]
    );
    // Nothing is left to look up, so a second pass sends nothing.
    let before = imp.requests().len();
    let mut a = imp.start().unwrap();
    assert_eq!(imp.look_up_all(&mut a, true), 0);
    assert_eq!(imp.requests().len(), before);
}

#[test]
fn a_failed_lookup_is_stored_as_the_files_error_and_retried_only_on_request() {
    let imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.put("b.mp3", 3);
    let mut sc = scenario(&[("a.mp3", 2), ("b.mp3", 3)]);
    sc["lookups"]["b.mp3"] = json!({"error": "network down"});
    imp.scenario(sc);
    let mut a = imp.start().unwrap();
    assert_eq!(imp.look_up_all(&mut a, false), 2, "both were tried");

    let b = imp.item("b.mp3");
    assert_eq!((b.lookup, b.error.as_deref()), (None, Some("network down")));
    assert!(imp.item("a.mp3").error.is_none());
    assert_eq!(
        imp.view_of("b.mp3").status,
        Status::Error("network down".into())
    );

    // Not tried again by a plain pass, tried again with retry.
    let before = imp.requests_of("lookup").len();
    assert_eq!(imp.look_up_all(&mut a, false), 0);
    assert_eq!(imp.requests_of("lookup").len(), before);
    drop(a);
    imp.scenario(scenario(&[("a.mp3", 2), ("b.mp3", 3)]));
    let mut a = imp.start().unwrap();
    assert_eq!(imp.look_up_all(&mut a, true), 1);
    let b = imp.item("b.mp3");
    assert!(b.lookup.is_some() && b.error.is_none(), "{b:?}");
    assert_eq!(imp.view_of("b.mp3").status, Status::Ready);
}

#[test]
fn search_again_sends_the_typed_artist_and_title() {
    let imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.scenario(scenario(&[("a.mp3", 2)]));
    imp.read();
    let q = import::Query {
        artist: "Real Artist".into(),
        title: "Real Title".into(),
    };
    import::look_up_again(imp.env.store.conn(), imp.item_id("a.mp3"), Some(&q)).unwrap();
    let mut a = imp.start().unwrap();
    imp.look_up_all(&mut a, false);
    let req = &imp.requests_of("lookup")[0];
    assert_eq!(req["artist"], "Real Artist");
    assert_eq!(req["title"], "Real Title");
}

#[test]
fn stop_ends_the_lookups_between_files() {
    let imp = Imp::new();
    for (n, name) in ["a.mp3", "b.mp3", "c.mp3"].iter().enumerate() {
        imp.put(name, n as u32 + 2);
    }
    imp.scenario(scenario(&[("a.mp3", 2), ("b.mp3", 3), ("c.mp3", 4)]));
    let inbox = imp.read();
    let index = imp.index();
    let pending = import::pending(&inbox, &index, false);
    let stop = AtomicBool::new(false);
    let mut seen = Vec::new();
    let mut a = imp.start().unwrap();
    let done = import::look_up(
        imp.env.store.conn(),
        &pending,
        &mut a,
        &mut |n, total, p| {
            seen.push((
                n,
                total,
                p.file_name().unwrap().to_str().unwrap().to_owned(),
            ));
            if n == 1 {
                stop.store(true, Ordering::SeqCst);
            }
        },
        &stop,
    )
    .unwrap();
    // Stop set while the second was being announced: the second finishes, the
    // third is not started.
    assert_eq!(done, 2);
    assert_eq!(seen, [(0, 3, "a.mp3".into()), (1, 3, "b.mp3".into())]);
    assert!(imp.item("a.mp3").lookup.is_some() && imp.item("b.mp3").lookup.is_some());
    assert!(imp.item("c.mp3").lookup.is_none());
    assert_eq!(imp.requests_of("lookup").len(), 2);

    // Stopped from the start: nothing is sent.
    let stop = AtomicBool::new(true);
    let before = imp.requests().len();
    let done = import::look_up(
        imp.env.store.conn(),
        &pending,
        &mut a,
        &mut |_, _, _| {},
        &stop,
    )
    .unwrap();
    assert_eq!(done, 0);
    assert_eq!(imp.requests().len(), before);
}

#[test]
fn autotag_dying_part_way_keeps_what_was_stored_and_leaves_the_rest_to_a_later_start() {
    let imp = Imp::new();
    for (n, name) in ["a.mp3", "b.mp3", "c.mp3"].iter().enumerate() {
        imp.put(name, n as u32 + 2);
    }
    let mut sc = scenario(&[("a.mp3", 2), ("b.mp3", 3), ("c.mp3", 4)]);
    sc["quirks"] = json!({"lookup:b.mp3": "exit"});
    sc["stderr"] = json!(["boom in b"]);
    imp.scenario(sc);
    let inbox = imp.read();
    let index = imp.index();
    let pending = import::pending(&inbox, &index, false);
    let mut a = imp.start().unwrap();
    let r = import::look_up(
        imp.env.store.conn(),
        &pending,
        &mut a,
        &mut |_, _, _| {},
        &AtomicBool::new(false),
    );
    assert!(
        matches!(&r, Err(Error::Autotag(m)) if m.contains("boom in b")),
        "{r:?}"
    );
    assert!(imp.item("a.mp3").lookup.is_some(), "kept");
    for n in ["b.mp3", "c.mp3"] {
        let it = imp.item(n);
        assert!(
            it.lookup.is_none() && it.error.is_none(),
            "{n} is tried again later: {it:?}"
        );
    }
    assert_eq!(imp.requests_of("lookup").len(), 2, "c was not asked");
}

// -- file: refusals before anything is filed ---------------------------------

#[test]
fn filing_is_refused_when_a_library_root_is_absent_and_nothing_is_sent() {
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2)]);
    let gone = imp.base.join("unmounted");
    imp.env.cfg.library.roots.push(gone.clone());
    let r = imp.file(None);
    assert!(
        matches!(&r, Err(Error::RootMissing(p)) if *p == gone),
        "{r:?}"
    );
    assert!(imp.requests_of("apply").is_empty());
    assert!(imp.inbox.join("a.mp3").exists());
    assert!(!imp.music().join(dest(2)).exists());
}

#[test]
fn filing_is_refused_when_the_root_to_file_into_is_empty() {
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2)]);
    // The first root has nothing in it: a scan would count it unreadable.
    fs::remove_file(&imp.seed).unwrap();
    let music = imp.music().to_owned();
    fs::remove_dir(music.join("Seed")).unwrap();
    let r = imp.file(None);
    assert!(
        matches!(&r, Err(Error::RootMissing(p)) if *p == music),
        "{r:?}"
    );
    assert!(imp.requests_of("apply").is_empty());

    // The same for an import root that is a folder of the library with
    // nothing in it yet, though the library is not empty.
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2)]);
    let empty = imp.music().join("Filed");
    fs::create_dir(&empty).unwrap();
    imp.env.cfg.import.root = empty.clone();
    let r = imp.file(None);
    assert!(
        matches!(&r, Err(Error::RootMissing(p)) if *p == empty),
        "{r:?}"
    );
    assert!(imp.requests_of("apply").is_empty());
}

#[test]
fn filing_is_refused_when_the_playlist_does_not_exist_before_anything_is_filed() {
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2)]);
    let r = imp.file(Some("No Such List"));
    assert!(
        matches!(&r, Err(Error::NoSuchPlaylist(n)) if n == "No Such List"),
        "{r:?}"
    );
    assert!(imp.requests_of("apply").is_empty());
    assert!(imp.inbox.join("a.mp3").exists());
    assert!(
        !imp.env.playlist_file("No Such List").exists(),
        "none was created"
    );
}

// -- file: the whole pass ----------------------------------------------------

#[test]
fn ready_songs_are_filed_in_path_order_scanned_indexed_and_added_to_the_playlist_in_order() {
    let mut imp = Imp::new();
    prepare(&imp, &[("b.mp3", 3), ("sub/c.mp3", 4), ("a.mp3", 2)]);
    playlist_with_seed(&imp, "Mix");
    imp.env.scan();
    let seed_track = imp.env.track(&imp.seed).id;
    let mut announced = Vec::new();
    let r = imp
        .file_with(
            Some("Mix"),
            &mut |n, total, p| announced.push((n, total, p.to_owned())),
            &AtomicBool::new(false),
        )
        .unwrap();

    // In path order.
    assert_eq!(applied(&imp), ["a.mp3", "b.mp3", "sub/c.mp3"]);
    assert_eq!(
        announced,
        [
            (0, 3, imp.inbox.join("a.mp3")),
            (1, 3, imp.inbox.join("b.mp3")),
            (2, 3, imp.inbox.join("sub/c.mp3")),
        ]
    );
    assert_eq!(
        r.filed.iter().map(|f| f.from.clone()).collect::<Vec<_>>(),
        [
            imp.inbox.join("a.mp3"),
            imp.inbox.join("b.mp3"),
            imp.inbox.join("sub/c.mp3")
        ]
    );
    assert_eq!(
        r.filed.iter().map(|f| f.to.clone()).collect::<Vec<_>>(),
        [2, 3, 4].map(|n| imp.music().join(dest(n)))
    );
    assert!(r.failed.is_empty() && r.mismatches.is_empty() && r.not_indexed.is_empty());
    assert!(r.error.is_none() && !r.stopped);

    // Gone from the inbox, rows dropped, in the library, indexed.
    assert!(imp.inbox_files().is_empty(), "{:?}", imp.inbox_files());
    assert_eq!(rows(&imp), 0);
    for f in &r.filed {
        assert!(f.to.is_file());
        let t = imp.env.track(&f.to);
        assert_eq!(f.track, Some(t.id));
    }
    let scan = r.scan.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(scan.added, 3);
    assert_eq!(scan.unchanged, 1, "the seed, which was not hashed again");
    // What was filed carries the tags that were sent.
    let t = imp.env.track(&imp.music().join(dest(3)));
    assert_eq!(t.tags.title.as_deref(), Some("Title3"));
    assert_eq!(t.tags.artist.as_deref(), Some("Artist3"));

    // The playlist: what it had, then the new songs in inbox order.
    assert_eq!(r.playlist, Some(Ok("Mix".to_owned())));
    let want: Vec<String> = std::iter::once(imp.seed.display().to_string())
        .chain([2, 3, 4].map(|n| imp.music().join(dest(n)).display().to_string()))
        .collect();
    assert_eq!(imp.env.locations("Mix"), want);
    let items = imp.env.indexed_items("Mix");
    assert_eq!(items.len(), 4);
    assert_eq!(items[0].1, Some(seed_track));
    assert_eq!(
        items[1..].iter().map(|i| i.1).collect::<Vec<_>>(),
        r.filed.iter().map(|f| f.track).collect::<Vec<_>>()
    );
}

#[test]
fn without_a_playlist_none_is_touched_or_made() {
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2)]);
    playlist_with_seed(&imp, "Mix");
    let before = imp.env.read_playlist("Mix");
    let r = imp.file(None).unwrap();
    assert_eq!(r.filed.len(), 1);
    assert!(r.playlist.is_none());
    assert_eq!(imp.env.read_playlist("Mix"), before);
    let lists = fs::read_dir(&imp.env.lists).unwrap().count();
    assert_eq!(lists, 1);
}

#[test]
fn a_second_pass_after_everything_was_filed_does_nothing() {
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2), ("b.mp3", 3)]);
    playlist_with_seed(&imp, "Mix");
    let first = imp.file(Some("Mix")).unwrap();
    assert_eq!(first.filed.len(), 2);
    let list = imp.env.read_playlist("Mix");
    let tracks = imp.env.tracks().len();
    let before = imp.requests().len();

    let second = imp.file(Some("Mix")).unwrap();
    assert!(second.filed.is_empty() && second.failed.is_empty());
    assert!(second.scan.is_none() && second.playlist.is_none());
    assert_eq!(
        imp.requests().len(),
        before,
        "no request, only a new process"
    );
    assert_eq!(imp.requests_of("apply").len(), 2);
    assert_eq!(imp.env.read_playlist("Mix"), list, "no song twice");
    assert_eq!(imp.env.tracks().len(), tracks);
}

#[test]
fn only_ready_files_are_filed_the_others_wait_untouched() {
    let mut imp = Imp::new();
    // Ready.
    imp.put("1-ready.mp3", 2);
    // Choose: the first candidate is not certain.
    imp.put("2-choose.mp3", 3);
    // No match: no candidate.
    imp.put("3-nomatch.mp3", 4);
    // Not looked up yet.
    imp.put("4-unlooked.mp3", 5);
    // Already in the library.
    fs::copy(&imp.seed, imp.inbox.join("5-have.mp3")).unwrap();
    // Not audio.
    fs::write(imp.inbox.join("6-notes.txt"), b"hello").unwrap();
    let mut sc = scenario(&[("1-ready.mp3", 2), ("2-choose.mp3", 3)]);
    sc["lookups"]["2-choose.mp3"]["candidates"][0]["certain"] = json!(false);
    imp.scenario(sc);
    imp.env.scan();
    let mut a = imp.start().unwrap();
    // 4-unlooked has no scenario entry: the fake answers "no candidate", so
    // look up only the three first by hand.
    let inbox = imp.read();
    let index = imp.index();
    let pending = import::pending(&inbox, &index, false);
    let some: Vec<_> = pending
        .into_iter()
        .filter(|e| !e.path.ends_with("4-unlooked.mp3"))
        .collect();
    import::look_up(
        imp.env.store.conn(),
        &some,
        &mut a,
        &mut |_, _, _| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    drop(a);
    let s = statuses(&imp);
    assert_eq!(s.len(), 6);
    assert!(matches!(s[0].1, Status::Ready));
    assert!(matches!(s[1].1, Status::Choose));
    assert!(matches!(s[2].1, Status::NoMatch));
    assert!(matches!(s[3].1, Status::LookingUp));
    assert!(matches!(s[4].1, Status::AlreadyInLibrary(_)));
    assert!(matches!(s[5].1, Status::NotSupported(_)));

    let r = imp.file(None).unwrap();
    assert_eq!(applied(&imp), ["1-ready.mp3"]);
    assert_eq!(r.filed.len(), 1);
    assert_eq!(
        imp.inbox_files(),
        [
            "2-choose.mp3",
            "3-nomatch.mp3",
            "4-unlooked.mp3",
            "5-have.mp3",
            "6-notes.txt"
        ]
    );
}

#[test]
fn a_file_changed_after_the_listing_is_not_filed_and_the_others_are() {
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2), ("b.mp3", 3), ("c.mp3", 4)]);
    let b = imp.inbox.join("b.mp3");
    let b_before = fs::read(&b).unwrap();
    let r = imp
        .file_with(
            None,
            &mut |_, _, p| {
                // Announced just before it is filed: changed in that moment.
                if p.ends_with("b.mp3") {
                    let f = fs::File::options().write(true).open(p).unwrap();
                    f.set_modified(
                        std::time::SystemTime::now() + std::time::Duration::from_secs(300),
                    )
                    .unwrap();
                }
            },
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(
        applied(&imp),
        ["a.mp3", "c.mp3"],
        "b was never sent to autotag"
    );
    assert_eq!(r.filed.len(), 2);
    assert_eq!(r.failed.len(), 1);
    assert_eq!(r.failed[0].0, b);
    assert!(
        r.failed[0].1.contains("changed since it was read"),
        "{}",
        r.failed[0].1
    );
    assert_eq!(fs::read(&b).unwrap(), b_before, "left as it was");
    assert!(r.error.is_none());
    // It starts over at the next read, as any changed file.
    imp.read();
    assert!(imp.item("b.mp3").lookup.is_none());
}

#[test]
fn a_refusal_is_kept_as_the_files_error_and_the_file_stays_in_the_inbox() {
    let mut imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.put("b.mp3", 3);
    imp.put("c.mp3", 4);
    let mut sc = scenario(&[("a.mp3", 2), ("b.mp3", 3), ("c.mp3", 4)]);
    sc["same_title"] = json!({"b.mp3": ["Artist3/Old/01 Title3.mp3"]});
    imp.scenario(sc.clone());
    let mut a = imp.start().unwrap();
    // The lookup knows nothing of the same title: it is Ready, and autotag
    // refuses at filing time.
    imp.look_up_all(&mut a, false);
    drop(a);
    let r = imp.file(None).unwrap();

    assert_eq!(r.filed.len(), 2);
    assert_eq!(r.failed.len(), 1);
    let (path, why) = &r.failed[0];
    assert_eq!(*path, imp.inbox.join("b.mp3"));
    assert!(why.contains("same title"), "{why}");
    assert_eq!(imp.item("b.mp3").error.as_ref(), Some(why));
    assert_eq!(imp.inbox_files(), ["b.mp3"]);
    assert!(r.error.is_none(), "a refusal does not stop the batch");
    assert!(
        !imp.music().join(dest(3)).exists(),
        "nothing was filed for it"
    );
    // The row stays with its lookup, so it can be dealt with.
    assert!(imp.item("b.mp3").lookup.is_some());
    assert_eq!(rows(&imp), 1);
}

#[test]
fn an_error_answer_is_kept_in_the_files_error_and_the_batch_goes_on() {
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2), ("b.mp3", 3)]);
    let mut sc = scenario(&[("a.mp3", 2), ("b.mp3", 3)]);
    sc["apply_errors"] = json!({"a.mp3": "disk full"});
    imp.scenario(sc);
    let r = imp.file(None).unwrap();
    assert_eq!(
        r.failed,
        vec![(imp.inbox.join("a.mp3"), "disk full".to_owned())]
    );
    assert_eq!(r.filed.len(), 1, "b was filed after a failed");
    assert_eq!(imp.item("a.mp3").error.as_deref(), Some("disk full"));
    assert!(imp.inbox.join("a.mp3").exists());
    assert!(r.error.is_none());
}

#[test]
fn a_file_refused_once_is_filed_by_the_next_pass_when_autotag_agrees() {
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2)]);
    let mut sc = scenario(&[("a.mp3", 2)]);
    sc["apply_errors"] = json!({"a.mp3": "disk full"});
    imp.scenario(sc);
    imp.file(None).unwrap();
    assert!(imp.item("a.mp3").error.is_some());

    imp.scenario(scenario(&[("a.mp3", 2)]));
    let r = imp.file(None).unwrap();
    assert_eq!(r.filed.len(), 1);
    assert_eq!(rows(&imp), 0, "the row went with the file");
    assert!(imp.music().join(dest(2)).is_file());
}

#[test]
fn autotag_dying_mid_batch_is_reported_and_what_was_filed_before_it_is_kept() {
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2), ("b.mp3", 3), ("c.mp3", 4)]);
    playlist_with_seed(&imp, "Mix");
    let mut sc = scenario(&[("a.mp3", 2), ("b.mp3", 3), ("c.mp3", 4)]);
    sc["quirks"] = json!({"apply:b.mp3": "exit"});
    sc["stderr"] = json!(["Traceback: the disk went away"]);
    imp.scenario(sc);
    let mut announced = Vec::new();
    let r = imp
        .file_with(
            Some("Mix"),
            &mut |n, _, _| announced.push(n),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(announced, [0, 1], "c was never started");

    let err = r.error.as_deref().expect("the batch stopped");
    assert!(err.contains("the disk went away"), "{err}");
    assert_eq!(r.filed.len(), 1);
    assert_eq!(r.filed[0].from, imp.inbox.join("a.mp3"));
    assert!(r.failed.is_empty(), "b is not 'failed': autotag is gone");
    // a: filed, scanned, indexed, in the playlist. b and c: untouched.
    assert!(r.filed[0].track.is_some());
    assert!(r.scan.as_ref().unwrap().is_ok());
    assert_eq!(r.playlist, Some(Ok("Mix".into())));
    assert_eq!(imp.env.locations("Mix").len(), 2);
    assert_eq!(imp.inbox_files(), ["b.mp3", "c.mp3"]);
    assert_eq!(applied(&imp), ["a.mp3", "b.mp3"], "c was never asked");
    assert_eq!(rows(&imp), 2);
    assert!(
        imp.item("b.mp3").error.is_none(),
        "tried again at the next start"
    );
}

#[test]
fn a_song_that_reads_back_otherwise_is_reported_and_stays_filed_scanned_and_in_the_playlist() {
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2)]);
    playlist_with_seed(&imp, "Mix");
    let mut sc = scenario(&[("a.mp3", 2)]);
    sc["tag_override"] = json!({"title": "Quite Another Title"});
    imp.scenario(sc);
    let r = imp.file(Some("Mix")).unwrap();

    let to = imp.music().join(dest(2));
    assert_eq!(r.mismatches, vec![(to.clone(), vec!["title"])]);
    assert_eq!(r.filed.len(), 1);
    assert!(r.failed.is_empty() && r.error.is_none());
    assert!(to.is_file());
    assert_eq!(rows(&imp), 0, "it is in the library: its row is dropped");
    assert!(r.filed[0].track.is_some());
    assert_eq!(r.playlist, Some(Ok("Mix".into())));
    assert_eq!(imp.env.locations("Mix").len(), 2);
    assert_eq!(
        imp.env.track(&to).tags.title.as_deref(),
        Some("Quite Another Title"),
        "what the file says is what is indexed"
    );
}

#[test]
fn every_field_sent_that_does_not_read_back_is_named() {
    let mut imp = Imp::new();
    imp.put("a.mp3", 2);
    let mut sc = scenario(&[("a.mp3", 2)]);
    // The fake files the bytes as they are: the fixture has a title and an
    // artist, and nothing else.
    sc["lookups"]["a.mp3"]["candidates"][0]["tags"] = json!({
        "title": "Title2", "artist": "Artist2", "album": "An Album", "year": 1985,
        "track_no": 3, "genre": "Pop"
    });
    sc["write_tags"] = json!(false);
    imp.scenario(sc);
    let mut a = imp.start().unwrap();
    imp.look_up_all(&mut a, false);
    drop(a);
    let r = imp.file(None).unwrap();
    assert_eq!(
        r.mismatches,
        vec![(
            imp.music().join(dest(2)),
            vec!["album", "genre", "year", "track"]
        )],
        "title and artist read back; the rest was not written"
    );
    assert_eq!(r.filed.len(), 1);
}

#[test]
fn a_filed_file_that_cannot_be_read_back_is_reported_and_not_indexed() {
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2)]);
    playlist_with_seed(&imp, "Mix");
    let mut sc = scenario(&[("a.mp3", 2)]);
    sc["corrupt"] = json!(["a.mp3"]);
    imp.scenario(sc);
    let r = imp.file(Some("Mix")).unwrap();
    let to = imp.music().join(dest(2));
    assert_eq!(r.mismatches, vec![(to.clone(), vec!["unreadable"])]);
    assert_eq!(r.filed.len(), 1, "autotag filed it, so it is filed");
    assert_eq!(r.filed[0].track, None);
    assert_eq!(r.not_indexed, vec![to.clone()]);
    // Not added to the playlist: it has no track.
    assert!(r.playlist.is_none(), "{:?}", r.playlist);
    assert_eq!(imp.env.locations("Mix").len(), 1);
}

#[test]
fn stop_takes_effect_between_files() {
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2), ("b.mp3", 3), ("c.mp3", 4)]);
    playlist_with_seed(&imp, "Mix");
    let stop = AtomicBool::new(false);
    let r = imp
        .file_with(
            Some("Mix"),
            &mut |n, _, _| {
                if n == 0 {
                    stop.store(true, Ordering::SeqCst);
                }
            },
            &stop,
        )
        .unwrap();
    // The file being announced when stop was set is finished; no other starts.
    assert!(r.stopped);
    assert_eq!(r.filed.len(), 1);
    assert_eq!(applied(&imp), ["a.mp3"]);
    assert_eq!(imp.inbox_files(), ["b.mp3", "c.mp3"]);
    // What was filed is still scanned and added.
    assert!(r.scan.as_ref().unwrap().is_ok());
    assert_eq!(r.playlist, Some(Ok("Mix".into())));
    assert_eq!(imp.env.locations("Mix").len(), 2);

    // Stopped before the first: nothing at all.
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2)]);
    let r = imp
        .file_with(None, &mut |_, _, _| {}, &AtomicBool::new(true))
        .unwrap();
    assert!(r.stopped && r.filed.is_empty() && r.scan.is_none());
    assert!(imp.requests_of("apply").is_empty());
    assert_eq!(imp.inbox_files(), ["a.mp3"]);
}

#[test]
fn a_possible_duplicate_is_filed_only_once_the_user_accepted_it() {
    let mut imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.put("b.mp3", 3);
    // The library already has Title2 by Artist2.
    imp.env.add("Existing/Title2.mp3", Kind::Mp3, 2);
    imp.env.scan();
    let mut sc = scenario(&[("a.mp3", 2), ("b.mp3", 3)]);
    sc["same_title"] = json!({"a.mp3": ["Artist2/Existing/Title2.mp3"]});
    imp.scenario(sc.clone());
    let mut a = imp.start().unwrap();
    imp.look_up_all(&mut a, false);
    drop(a);
    assert_eq!(imp.view_of("a.mp3").status, Status::PossibleDuplicate);
    assert!(!imp.view_of("a.mp3").duplicates.is_empty());

    let r = imp.file(None).unwrap();
    assert_eq!(
        applied(&imp),
        ["b.mp3"],
        "a was not even offered to autotag"
    );
    assert_eq!(r.filed.len(), 1);
    assert_eq!(imp.requests_of("apply")[0]["allow_duplicate"], false);
    assert!(imp.inbox.join("a.mp3").exists());

    // Accepted: sent with allow_duplicate, and filed (the fake would refuse
    // it otherwise, as autotag does).
    import::set_confirmed(imp.env.store.conn(), imp.item_id("a.mp3"), true).unwrap();
    assert_eq!(imp.view_of("a.mp3").status, Status::Ready);
    let r = imp.file(None).unwrap();
    assert_eq!(r.filed.len(), 1, "{r:?}");
    let sent = imp.requests_of("apply");
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["file"], imp.inbox.join("a.mp3").to_str().unwrap());
    assert_eq!(sent[1]["allow_duplicate"], true);
    assert!(imp.music().join(dest(2)).is_file());
}

#[test]
fn what_is_sent_is_the_candidates_tags_and_ids_or_the_typed_tags_alone() {
    let mut imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.put("b.mp3", 3);
    imp.put("c.mp3", 4);
    let mut sc = scenario(&[("a.mp3", 2), ("b.mp3", 3), ("c.mp3", 4)]);
    // c: two candidates, the user chose the second.
    sc["lookups"]["c.mp3"]["candidates"] = json!([
        candidate("rec-c1", "First", "One", false, "One/First.mp3"),
        candidate("rec-c2", "Second", "Two", false, "Two/Second.mp3"),
    ]);
    sc["destinations"]["c.mp3"] = json!("Two/Second.mp3");
    imp.scenario(sc);
    let mut a = imp.start().unwrap();
    imp.look_up_all(&mut a, false);
    drop(a);
    // b: typed tags over a candidate and a choice.
    let typed = Fields {
        title: Some("Typed Title".into()),
        artist: Some("Typed Artist".into()),
        ..Fields::default()
    };
    let conn = imp.env.store.conn();
    import::set_tags(conn, imp.item_id("b.mp3"), Some(&typed)).unwrap();
    import::set_choice(conn, imp.item_id("b.mp3"), Some(0)).unwrap();
    import::set_choice(conn, imp.item_id("c.mp3"), Some(1)).unwrap();
    for n in ["a.mp3", "b.mp3", "c.mp3"] {
        assert_eq!(imp.view_of(n).status, Status::Ready, "{n}");
    }
    imp.file(None).unwrap();

    let sent = imp.requests_of("apply");
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[0]["recording"], "rec-2");
    assert_eq!(sent[0]["release"], "rel-rec-2");
    assert_eq!(
        sent[0]["tags"],
        json!({"title": "Title2", "artist": "Artist2"})
    );
    assert!(
        sent[1].get("recording").is_none() && sent[1].get("release").is_none(),
        "{}",
        sent[1]
    );
    assert_eq!(
        sent[1]["tags"],
        json!({"title": "Typed Title", "artist": "Typed Artist"})
    );
    assert_eq!(sent[2]["recording"], "rec-c2");
    assert_eq!(sent[2]["release"], "rel-rec-c2");
    assert_eq!(sent[2]["tags"], json!({"title": "Second", "artist": "Two"}));
    for s in &sent {
        assert_eq!(s["dry_run"], false);
    }
}

#[test]
fn a_song_whose_hash_is_a_missing_tracks_is_filed_and_the_scan_gives_that_track_its_file_back() {
    let mut imp = Imp::new();
    imp.put("a.mp3", 2);
    // The library had this very file, and lost it.
    let old = imp.music().join("Old/Title2.mp3");
    fs::create_dir_all(old.parent().unwrap()).unwrap();
    fs::copy(imp.inbox.join("a.mp3"), &old).unwrap();
    imp.env.scan();
    let id = imp.env.track(&old).id;
    fs::remove_file(&old).unwrap();
    imp.env.scan();
    assert!(imp.env.track(&old).missing_since.is_some(), "set up");

    let mut sc = scenario(&[("a.mp3", 2)]);
    // The fake files the bytes untouched, as the inbox copy had them.
    sc["write_tags"] = json!(false);
    imp.scenario(sc);
    let mut a = imp.start().unwrap();
    imp.look_up_all(&mut a, false);
    drop(a);
    assert_eq!(
        imp.view_of("a.mp3").status,
        Status::Ready,
        "not a duplicate"
    );
    let r = imp.file(None).unwrap();

    assert_eq!(r.filed.len(), 1);
    assert_eq!(r.filed[0].track, Some(id), "the old track, not a new one");
    let t = imp.env.track(&imp.music().join(dest(2)));
    assert_eq!(t.id, id);
    assert!(t.missing_since.is_none());
    assert_eq!(imp.env.tracks().iter().filter(|t| t.id == id).count(), 1);
}

#[test]
fn a_playlist_that_cannot_be_written_after_filing_is_reported_and_the_songs_stay_filed() {
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2), ("b.mp3", 3)]);
    playlist_with_seed(&imp, "Mix");
    let list = imp.env.playlist_file("Mix");
    let r = imp
        .file_with(
            Some("Mix"),
            &mut |n, _, _| {
                // The playlist is deleted by someone else while filing.
                if n == 0 {
                    fs::remove_file(&list).unwrap();
                }
            },
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(r.filed.len(), 2);
    let Some(Err(why)) = &r.playlist else {
        panic!("{:?}", r.playlist)
    };
    assert!(!why.is_empty());
    assert!(imp.music().join(dest(2)).is_file() && imp.music().join(dest(3)).is_file());
    assert!(r.scan.as_ref().unwrap().is_ok());
    assert!(!list.exists(), "it was not made again");
}

#[test]
fn the_progress_reports_only_the_files_to_file() {
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2), ("b.mp3", 3)]);
    fs::write(imp.inbox.join("zz.txt"), b"x").unwrap();
    let mut got = Vec::new();
    imp.file_with(
        None,
        &mut |n, t, _| got.push((n, t)),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(got, [(0, 2), (1, 2)]);
}

#[test]
fn a_song_filed_by_an_autotag_that_died_before_answering_is_scanned_and_indexed() {
    let mut imp = Imp::new();
    prepare(&imp, &[("a.mp3", 2), ("b.mp3", 3)]);
    let mut sc = scenario(&[("a.mp3", 2), ("b.mp3", 3)]);
    sc["quirks"] = json!({"apply:a.mp3": "file_then_exit"});
    sc["stderr"] = json!(["killed by the OOM killer"]);
    imp.scenario(sc);
    let r = imp.file(None).unwrap();

    assert!(
        r.error.as_deref().unwrap().contains("OOM killer"),
        "{:?}",
        r.error
    );
    assert!(r.filed.is_empty(), "no answer, so not reported as filed");
    assert!(
        r.scan.as_ref().is_some_and(|s| s.is_ok()),
        "scanned all the same: {:?}",
        r.scan
    );
    let to = imp.music().join(dest(2));
    assert!(to.is_file(), "autotag did file it");
    assert!(
        imp.env.tracks().iter().any(|t| t.path == to),
        "and it is indexed"
    );
    assert!(!imp.music().join(dest(3)).exists());
    // The inbox file is gone, and its row goes at the next read.
    assert_eq!(imp.inbox_files(), ["b.mp3"]);
    assert_eq!(rows(&imp), 2);
    imp.read();
    assert_eq!(rows(&imp), 1);
}

#[test]
fn an_item_whose_stored_lookup_no_longer_parses_is_looked_up_again() {
    let imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.scenario(scenario(&[("a.mp3", 2)]));
    imp.read();
    import::set_lookup(
        imp.env.store.conn(),
        imp.item_id("a.mp3"),
        "{\"no longer\": 1}",
    )
    .unwrap();
    assert!(matches!(imp.view_of("a.mp3").status, Status::Error(_)));

    let inbox = imp.read();
    let index = imp.index();
    // Without retry: it has no error of its own, only text nobody can read.
    assert_eq!(import::pending(&inbox, &index, false).len(), 1);
    let mut a = imp.start().unwrap();
    assert_eq!(imp.look_up_all(&mut a, false), 1);
    let stored = imp.item("a.mp3").lookup.unwrap();
    assert!(import::Lookup::parse(&stored).is_ok(), "{stored}");
    assert_eq!(imp.view_of("a.mp3").status, Status::Ready);
    // And now it is not pending any more.
    let inbox = imp.read();
    assert!(import::pending(&inbox, &imp.index(), true).is_empty());
}
