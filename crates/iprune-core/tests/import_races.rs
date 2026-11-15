//! A Search again (or another reader's answer) that lands while a lookup is
//! in flight must not be overwritten by the stale answer. The in-flight
//! request is held open with the fake autotag's `slow` quirk (answers half a
//! second late); a second connection to the index plays the window. No
//! network, no real autotag.

mod common;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use common::import_env::{Imp, answer, candidate, requests, scenario};
use iprune_core::import::{self, Lookup, Query};
use iprune_core::store::Store;
use serde_json::{Value, json};

fn db(imp: &Imp) -> PathBuf {
    imp.env.music.parent().unwrap().join("db/iprune.sqlite")
}

/// Runs `during` on another connection once a request of `op` has reached
/// the fake, while `main` runs on this thread.
fn while_in_flight<T>(
    imp: &Imp,
    op: &str,
    during: impl FnOnce(&Store) + Send,
    main: impl FnOnce() -> T,
) -> T {
    let (root, path, op) = (imp.music().to_owned(), db(imp), op.to_owned());
    std::thread::scope(|s| {
        s.spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while requests(&root).iter().all(|r| r["op"] != op.as_str()) {
                assert!(Instant::now() < deadline, "no {op} request");
                std::thread::sleep(Duration::from_millis(5));
            }
            during(&Store::open(&path).unwrap());
        });
        main()
    })
}

fn slow_first(imp: &Imp, mut sc: Value) {
    sc["quirks"] = json!({"1": "slow"});
    imp.scenario(sc);
}

fn query(artist: &str, title: &str) -> Query {
    Query {
        artist: artist.into(),
        title: title.into(),
    }
}

fn run_look_up(imp: &Imp) -> usize {
    let inbox = imp.read();
    let index = imp.index();
    let pending = import::pending(&inbox, &index, false);
    let mut autotag = imp.start().unwrap();
    import::look_up(
        imp.env.store.conn(),
        &pending,
        &mut autotag,
        &mut |_, _, _| {},
        &AtomicBool::new(false),
    )
    .unwrap()
}

// A Search again made during a lookup leaves the file waiting with the new
// search; the old answer, for the old search, is dropped.
#[test]
fn a_search_again_during_a_lookup_beats_the_answer_and_is_used_next_time() {
    let imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.read();
    let id = imp.item_id("a.mp3");
    slow_first(&imp, scenario(&[("a.mp3", 2)]));

    let q = query("a-ha", "Take On Me");
    while_in_flight(
        &imp,
        "lookup",
        |store| import::look_up_again(store.conn(), id, Some(&q)).unwrap(),
        || run_look_up(&imp),
    );

    let item = imp.item("a.mp3");
    assert_eq!(item.lookup, None, "the stale answer was not stored");
    assert_eq!(item.search, Some(q.clone()));
    assert_eq!(imp.requests_of("lookup").len(), 1);

    // Next time the new search is what autotag is asked.
    imp.scenario(scenario(&[("a.mp3", 2)]));
    run_look_up(&imp);
    let asked = imp.requests_of("lookup");
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[1]["artist"], "a-ha");
    assert_eq!(asked[1]["title"], "Take On Me");
    assert!(imp.item("a.mp3").lookup.is_some());
    assert_eq!(imp.item("a.mp3").search, Some(q));
}

#[test]
fn a_stale_error_is_not_stored_over_a_search_again_either() {
    let imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.read();
    let id = imp.item_id("a.mp3");
    slow_first(
        &imp,
        json!({"lookups": {"a.mp3": {"error": "MusicBrainz is down"}}}),
    );

    let q = query("x", "y");
    while_in_flight(
        &imp,
        "lookup",
        |store| import::look_up_again(store.conn(), id, Some(&q)).unwrap(),
        || run_look_up(&imp),
    );

    let item = imp.item("a.mp3");
    assert_eq!((item.lookup, item.error), (None, None));
    assert_eq!(item.search, Some(q));
}

// Without interference the answer and the error are stored as before.
#[test]
fn an_answer_and_an_error_are_stored_when_nothing_changed_meanwhile() {
    let imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.put("b.mp3", 3);
    let mut sc = scenario(&[("a.mp3", 2)]);
    sc["lookups"]["b.mp3"] = json!({"error": "nope"});
    imp.scenario(sc);
    assert_eq!(run_look_up(&imp), 2);
    assert!(imp.item("a.mp3").lookup.is_some());
    assert_eq!(imp.item("a.mp3").error, None);
    let b = imp.item("b.mp3");
    assert_eq!((b.lookup, b.error.as_deref()), (None, Some("nope")));
}

// An item answered by another reader between the listing and its turn is
// not asked again.
#[test]
fn an_item_answered_since_the_listing_is_skipped() {
    let imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.put("b.mp3", 3);
    imp.scenario(scenario(&[("a.mp3", 2), ("b.mp3", 3)]));
    let inbox = imp.read();
    let index = imp.index();
    let pending = import::pending(&inbox, &index, false);
    assert_eq!(pending.len(), 2);
    // Another reader answers a.mp3 after the listing.
    let theirs = answer(vec![candidate(
        "theirs", "Theirs", "Other", true, "x/y.mp3",
    )])
    .to_string();
    import::set_lookup(imp.env.store.conn(), imp.item_id("a.mp3"), &theirs).unwrap();

    let mut autotag = imp.start().unwrap();
    let done = import::look_up(
        imp.env.store.conn(),
        &pending,
        &mut autotag,
        &mut |_, _, _| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(done, 1);
    let asked = imp.requests_of("lookup");
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0]["file"].as_str().unwrap().ends_with("b.mp3"));
    assert_eq!(
        imp.item("a.mp3").lookup.as_deref(),
        Some(theirs.as_str()),
        "theirs stays"
    );
}

// ---------------------------------------------------------------- genres

fn unknown_genre_lookup(recording: &str) -> String {
    let mut second = candidate(recording, "Live", "Artist2", false, "L/2.mp3");
    second["tags"]["genre"] = Value::Null;
    answer(vec![
        candidate("r1", "Title2", "Artist2", true, "x"),
        second,
    ])
    .to_string()
}

fn genre_world(tag: &str) -> (Imp, i64) {
    let imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.read();
    let id = imp.item_id("a.mp3");
    import::set_lookup(imp.env.store.conn(), id, &unknown_genre_lookup("r2b")).unwrap();
    import::set_choice(imp.env.store.conn(), id, Some(1)).unwrap();
    let mut sc = scenario(&[]);
    if !tag.is_empty() {
        sc["tags"] = json!({"r2b": {"title": "Live", "artist": "Artist2", "genre": tag}});
    }
    slow_first(&imp, sc);
    (imp, id)
}

fn fill(imp: &Imp) -> usize {
    let inbox = imp.read();
    let wanted = import::genres_wanted(&inbox);
    assert_eq!(wanted.len(), 1);
    let mut autotag = imp.start().unwrap();
    import::fill_genres(
        imp.env.store.conn(),
        &wanted,
        &mut autotag,
        &AtomicBool::new(false),
    )
    .unwrap()
}

// Search again during the tags request: the row waits for a lookup, and
// neither the genre nor an error answer lands on it.
#[test]
fn a_search_again_during_the_genre_request_leaves_the_new_row_alone() {
    for tag in ["Live Rock", ""] {
        let (imp, id) = genre_world(tag);
        let done = while_in_flight(
            &imp,
            "tags",
            |s| import::look_up_again(s.conn(), id, Some(&query("a", "b"))).unwrap(),
            || fill(&imp),
        );
        let item = imp.item("a.mp3");
        assert_eq!(done, 0, "{tag:?}: nothing was stored");
        assert_eq!((item.lookup, item.error), (None, None), "{tag:?}");
        assert_eq!(item.search, Some(query("a", "b")));
    }
}

// Another lookup answered meanwhile: its candidate with the same index must
// not receive the genre asked for the old one.
#[test]
fn a_genre_for_the_old_lookup_is_not_written_into_a_newer_one() {
    let (imp, id) = genre_world("Live Rock");
    let newer = unknown_genre_lookup("other");
    while_in_flight(
        &imp,
        "tags",
        |s| import::set_lookup(s.conn(), id, &newer).unwrap(),
        || fill(&imp),
    );
    let item = imp.item("a.mp3");
    assert_eq!(item.lookup.as_deref(), Some(newer.as_str()));
    let l = Lookup::parse(item.lookup.as_deref().unwrap()).unwrap();
    assert!(l.candidates[1].genre_unknown && l.candidates[1].tags.genre.is_none());
}

#[test]
fn the_genre_is_stored_when_the_lookup_did_not_change() {
    let (imp, _) = genre_world("Live Rock");
    assert_eq!(fill(&imp), 1);
    let l = Lookup::parse(imp.item("a.mp3").lookup.as_deref().unwrap()).unwrap();
    assert_eq!(l.candidates[1].tags.genre.as_deref(), Some("Live Rock"));
}

// A Search again made after the listing and before the file's turn is used,
// not the search the listing had.
#[test]
fn the_search_typed_since_the_listing_is_the_one_asked() {
    let imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.scenario(scenario(&[("a.mp3", 2)]));
    let inbox = imp.read();
    let index = imp.index();
    let pending = import::pending(&inbox, &index, false);
    import::look_up_again(
        imp.env.store.conn(),
        imp.item_id("a.mp3"),
        Some(&query("a-ha", "Take On Me")),
    )
    .unwrap();
    let mut autotag = imp.start().unwrap();
    import::look_up(
        imp.env.store.conn(),
        &pending,
        &mut autotag,
        &mut |_, _, _| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    let asked = imp.requests_of("lookup");
    assert_eq!(
        (asked[0]["artist"].as_str(), asked[0]["title"].as_str()),
        (Some("a-ha"), Some("Take On Me"))
    );
    assert!(
        imp.item("a.mp3").lookup.is_some(),
        "stored: the search is the row's"
    );
}
