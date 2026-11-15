//! `genres_wanted` and `fill_genres`: the genre of a candidate chosen with
//! Use This Match, which a lookup leaves out for all but the first. The fake
//! autotag answers `tags`; no network.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::import_env::{Imp, answer, candidate, requests, scenario};
use iprune_core::Error;
use iprune_core::import::{self, Fields, Lookup};
use serde_json::{Value, json};

/// Two candidates; the second's genre was not looked up.
fn two(n: u32) -> Value {
    let mut second = candidate(
        &format!("rec-{n}b"),
        &format!("Live {n}"),
        &format!("Artist{n}"),
        false,
        &format!("Live/{n}.mp3"),
    );
    second["tags"]["genre"] = Value::Null;
    answer(vec![
        candidate(
            &format!("rec-{n}"),
            &format!("Title{n}"),
            &format!("Artist{n}"),
            true,
            &format!("A/{n}.mp3"),
        ),
        second,
    ])
}

fn genre_tags(title: &str) -> Value {
    json!({"title": title, "artist": "Artist2", "genre": "Live Rock", "year": 2001})
}

/// A file in the inbox with the lookup `two(n)` stored.
fn looked_up(imp: &Imp, rel: &str, n: u32) -> i64 {
    imp.put(rel, n);
    imp.read();
    let id = imp.item_id(rel);
    import::set_lookup(imp.env.store.conn(), id, &two(n).to_string()).unwrap();
    id
}

fn wanted_list(imp: &Imp) -> Vec<(String, usize)> {
    let inbox = imp.read();
    import::genres_wanted(&inbox)
        .into_iter()
        .map(|(e, c)| {
            (
                e.path
                    .strip_prefix(&inbox.dir)
                    .unwrap()
                    .display()
                    .to_string(),
                c,
            )
        })
        .collect()
}

#[test]
fn only_a_chosen_candidate_with_an_unknown_genre_and_no_typed_tags_is_wanted() {
    let imp = Imp::new();
    let conn = imp.env.store.conn();
    // Wanted: the second candidate, chosen, genre not looked up.
    let a = looked_up(&imp, "a.mp3", 2);
    import::set_choice(conn, a, Some(1)).unwrap();
    // Not wanted: nothing chosen, although the default (first) candidate's
    // genre is unknown too: Use This Match was not clicked.
    let b = looked_up(&imp, "b.mp3", 3);
    let mut unknown_first = two(3);
    unknown_first["candidates"][0]["tags"]["genre"] = Value::Null;
    import::set_lookup(conn, b, &unknown_first.to_string()).unwrap();
    // Not wanted: tags typed instead.
    let c = looked_up(&imp, "c.mp3", 4);
    import::set_choice(conn, c, Some(1)).unwrap();
    import::set_tags(
        conn,
        c,
        Some(&Fields {
            title: Some("Typed".into()),
            ..Fields::default()
        }),
    )
    .unwrap();
    // Not wanted: the chosen candidate's genre is known (the first one's).
    let d = looked_up(&imp, "d.mp3", 5);
    import::set_choice(conn, d, Some(0)).unwrap();
    // Not wanted: a choice with no candidate behind it.
    let e = looked_up(&imp, "e.mp3", 6);
    import::set_choice(conn, e, Some(7)).unwrap();
    // Not wanted: chosen, but no lookup stored.
    imp.put("f.mp3", 7);
    imp.read();
    import::set_choice(conn, imp.item_id("f.mp3"), Some(1)).unwrap();
    // Not wanted: the lookup is unreadable.
    let g = looked_up(&imp, "g.mp3", 8);
    import::set_choice(conn, g, Some(1)).unwrap();
    import::set_lookup(conn, g, "not json").unwrap();
    // Wanted: the choice is the first candidate, whose genre was not looked up.
    let mut first_unknown = two(9);
    first_unknown["candidates"][0]["tags"]["genre"] = Value::Null;
    imp.put("h.mp3", 9);
    imp.read();
    let h = imp.item_id("h.mp3");
    import::set_lookup(conn, h, &first_unknown.to_string()).unwrap();
    import::set_choice(conn, h, Some(0)).unwrap();

    assert_eq!(
        wanted_list(&imp),
        [("a.mp3".to_owned(), 1), ("h.mp3".to_owned(), 0)]
    );
}

#[test]
fn a_filled_genre_is_stored_with_the_full_tags_and_clears_the_error() {
    let imp = Imp::new();
    let id = looked_up(&imp, "a.mp3", 2);
    let conn = imp.env.store.conn();
    import::set_choice(conn, id, Some(1)).unwrap();
    import::set_error(conn, id, Some("an old error")).unwrap();
    let mut sc = scenario(&[]);
    sc["tags"] = json!({"rec-2b": genre_tags("Live 2")});
    imp.scenario(sc);

    let inbox = imp.read();
    let wanted = import::genres_wanted(&inbox);
    assert_eq!(wanted.len(), 1);
    let mut autotag = imp.start().unwrap();
    let done = import::fill_genres(
        imp.env.store.conn(),
        &wanted,
        &mut autotag,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(done, 1);

    let asked = imp.requests_of("tags");
    assert_eq!(asked.len(), 1, "one request: {asked:?}");
    assert_eq!(asked[0]["recording"], "rec-2b");
    assert_eq!(asked[0]["release"], "rel-rec-2b");

    let item = imp.item("a.mp3");
    assert_eq!(item.error, None, "a lookup that worked clears the error");
    let l = Lookup::parse(item.lookup.as_deref().unwrap()).unwrap();
    assert_eq!(l.candidates[1].tags.genre.as_deref(), Some("Live Rock"));
    assert_eq!(l.candidates[1].tags.year, Some(2001));
    assert!(!l.candidates[1].genre_unknown);
    assert_eq!(
        l.candidates[0].tags.title.as_deref(),
        Some("Title2"),
        "the other candidate is as it was"
    );
    assert_eq!(item.choice, Some(1), "the choice stays");
    // Nothing is wanted any more.
    assert!(wanted_list(&imp).is_empty());
}

#[test]
fn an_error_answer_is_stored_and_the_genre_stays_unknown() {
    let imp = Imp::new();
    let id = looked_up(&imp, "a.mp3", 2);
    import::set_choice(imp.env.store.conn(), id, Some(1)).unwrap();
    let before = imp.item("a.mp3").lookup;
    // The fake has no tags for rec-2b: "not found on MusicBrainz".
    imp.scenario(scenario(&[]));

    let inbox = imp.read();
    let wanted = import::genres_wanted(&inbox);
    let mut autotag = imp.start().unwrap();
    let done = import::fill_genres(
        imp.env.store.conn(),
        &wanted,
        &mut autotag,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(done, 1, "autotag answered, with an error");

    let item = imp.item("a.mp3");
    assert!(
        item.error.as_deref().unwrap_or("").contains("not found"),
        "stored: {:?}",
        item.error
    );
    assert_eq!(item.lookup, before, "the lookup is untouched");
    let l = Lookup::parse(item.lookup.as_deref().unwrap()).unwrap();
    assert!(l.candidates[1].genre_unknown);
    assert_eq!(item.choice, Some(1));
}

#[test]
fn a_stop_already_set_asks_for_nothing_and_stores_nothing() {
    let imp = Imp::new();
    let id = looked_up(&imp, "a.mp3", 2);
    import::set_choice(imp.env.store.conn(), id, Some(1)).unwrap();
    let mut sc = scenario(&[]);
    sc["tags"] = json!({"rec-2b": genre_tags("Live 2")});
    imp.scenario(sc);
    let before = imp.item("a.mp3");

    let inbox = imp.read();
    let wanted = import::genres_wanted(&inbox);
    let mut autotag = imp.start().unwrap();
    let done = import::fill_genres(
        imp.env.store.conn(),
        &wanted,
        &mut autotag,
        &AtomicBool::new(true),
    )
    .unwrap();
    assert_eq!(done, 0);
    assert!(imp.requests_of("tags").is_empty());
    assert_eq!(imp.item("a.mp3"), before);
}

// Stop ends the pass between files: the one in flight is finished and
// stored, the next is never asked.
#[test]
fn stop_set_during_the_first_file_ends_it_before_the_second() {
    let imp = Imp::new();
    for (rel, n) in [("a.mp3", 2), ("b.mp3", 3)] {
        let id = looked_up(&imp, rel, n);
        import::set_choice(imp.env.store.conn(), id, Some(1)).unwrap();
    }
    let mut sc = scenario(&[]);
    sc["tags"] = json!({
        "rec-2b": genre_tags("Live 2"),
        "rec-3b": genre_tags("Live 3"),
    });
    // The first answer comes half a second late: time to set the flag.
    sc["quirks"] = json!({"1": "slow"});
    imp.scenario(sc);

    let inbox = imp.read();
    let wanted = import::genres_wanted(&inbox);
    assert_eq!(wanted.len(), 2);
    let stop = AtomicBool::new(false);
    let root = imp.music().to_owned();
    let mut autotag = imp.start().unwrap();
    let done = std::thread::scope(|s| {
        s.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while requests(&root).iter().all(|r| r["op"] != "tags") && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            stop.store(true, Ordering::SeqCst);
        });
        import::fill_genres(imp.env.store.conn(), &wanted, &mut autotag, &stop).unwrap()
    });
    assert_eq!(done, 1);
    assert_eq!(imp.requests_of("tags").len(), 1, "the second was not asked");
    let genre = |rel: &str| {
        let l = Lookup::parse(imp.item(rel).lookup.as_deref().unwrap()).unwrap();
        l.candidates[1].tags.genre.clone()
    };
    assert_eq!(genre("a.mp3").as_deref(), Some("Live Rock"));
    assert_eq!(genre("b.mp3"), None, "b.mp3 waits for the next pass");
}

#[test]
fn autotag_dying_is_an_error_and_what_is_left_is_not_touched() {
    let imp = Imp::new();
    for (rel, n) in [("a.mp3", 2), ("b.mp3", 3)] {
        let id = looked_up(&imp, rel, n);
        import::set_choice(imp.env.store.conn(), id, Some(1)).unwrap();
    }
    let mut sc = scenario(&[]);
    sc["tags"] = json!({"rec-2b": genre_tags("Live 2"), "rec-3b": genre_tags("Live 3")});
    sc["quirks"] = json!({"1": "exit"});
    imp.scenario(sc);
    let (a_before, b_before) = (imp.item("a.mp3"), imp.item("b.mp3"));

    let inbox = imp.read();
    let wanted = import::genres_wanted(&inbox);
    let mut autotag = imp.start().unwrap();
    let r = import::fill_genres(
        imp.env.store.conn(),
        &wanted,
        &mut autotag,
        &AtomicBool::new(false),
    );
    assert!(matches!(r, Err(Error::Autotag(_))), "{r:?}");
    assert_eq!(
        imp.item("a.mp3"),
        a_before,
        "no error stored for a dead autotag"
    );
    assert_eq!(imp.item("b.mp3"), b_before);
    assert_eq!(imp.requests_of("tags").len(), 1);
}
