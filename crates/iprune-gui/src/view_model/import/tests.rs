//! The Import view's rows: what each status shows, the grid's order, the
//! "was" values, search and the labels. The statuses come from real inbox
//! files in a temp dir (the core's own `view`), with lookups stored by hand:
//! no autotag runs here, and nothing touches the network or the real home.

#[allow(clippy::duplicate_mod)] // the fixture helpers are shared with the other tests
#[path = "../../../../iprune-core/tests/common/mod.rs"]
mod common;

use std::fs;
use std::path::PathBuf;

use common::fairplay_m4a;
use common::import_env::{Imp, answer, candidate};
use iprune_core::import::{self, Inbox, Index};
use iprune_core::tag_edit::Changes;
use serde_json::Value;

use super::*;

// ------------------------------------------------------------ literal rows

fn tags(f: impl FnOnce(&mut Tags)) -> Tags {
    let mut t = Tags::default();
    f(&mut t);
    t
}

/// A row with nothing in it, to set what a test needs.
fn blank(file: &str) -> ImportRow {
    ImportRow {
        item: Some(1),
        path: PathBuf::from("/inbox").join(file),
        file: file.to_owned(),
        status: Status::Ready,
        before: None,
        after: None,
        info: None,
        destination: None,
        candidates: Vec::new(),
        candidate: None,
        by: None,
        query: None,
        duplicates: Vec::new(),
        typed: false,
        confirmed: false,
        error: None,
    }
}

fn row(file: &str, f: impl FnOnce(&mut ImportRow)) -> ImportRow {
    let mut r = blank(file);
    f(&mut r);
    r
}

fn info(ms: Option<u32>) -> AudioInfo {
    AudioInfo {
        kind: iprune_core::tags::FileKind::Mp3,
        duration_ms: ms,
        bitrate: None,
        samplerate: None,
    }
}

fn names(rows: &[ImportRow]) -> Vec<&str> {
    rows.iter().map(|r| r.file.as_str()).collect()
}

// ------------------------------------------------------- real inbox files

/// An inbox of every kind of file, each named for what it should be.
struct World {
    imp: Imp,
}

impl World {
    fn new() -> World {
        let mut imp = Imp::new();
        // `Seed/Seed.mp3` is in the library: Title90 by Artist90, 1 s.
        imp.env.scan();
        World { imp }
    }

    fn put(&self, rel: &str, n: u32) -> i64 {
        self.imp.put(rel, n);
        self.imp.read();
        self.imp.item_id(rel)
    }

    fn lookup(&self, id: i64, candidates: Vec<Value>) {
        import::set_lookup(
            self.imp.env.store.conn(),
            id,
            &answer(candidates).to_string(),
        )
        .unwrap();
    }

    fn rows(&self) -> Vec<ImportRow> {
        let inbox = self.imp.read();
        let index = self.imp.index();
        rows(&inbox, &index)
    }

    fn row(&self, rel: &str) -> ImportRow {
        self.rows()
            .into_iter()
            .find(|r| r.file == rel)
            .unwrap_or_else(|| panic!("no row for {rel}"))
    }
}

fn certain(n: u32) -> Value {
    candidate(
        &format!("rec-{n}"),
        &format!("Title{n}"),
        &format!("Artist{n}"),
        true,
        &format!("Artist{n}/Album{n}/{n:02} Title{n}.mp3"),
    )
}

fn uncertain(n: u32) -> Value {
    candidate(
        &format!("rec-{n}"),
        &format!("Title{n}"),
        &format!("Artist{n}"),
        false,
        &format!("Artist{n}/Album{n}/{n:02} Title{n}.mp3"),
    )
}

/// Every status, and the words the grid and the pane use for it.
fn every_status() -> World {
    let w = World::new();
    // Ready: a certain candidate.
    let id = w.put("ready.mp3", 2);
    w.lookup(id, vec![certain(2)]);
    // Ready, with the tags typed.
    let id = w.put("typed.mp3", 3);
    w.lookup(id, vec![uncertain(3)]);
    import::set_tags(
        w.imp.env.store.conn(),
        id,
        Some(&import::Fields {
            title: Some("Typed".into()),
            ..import::Fields::default()
        }),
    )
    .unwrap();
    // Choose: the only candidate is not certain.
    let id = w.put("choose.mp3", 4);
    w.lookup(id, vec![uncertain(4)]);
    // No match.
    let id = w.put("nomatch.mp3", 5);
    w.lookup(id, vec![]);
    // Looking up: no lookup yet.
    w.put("waiting.mp3", 6);
    // Error: the lookup failed.
    let id = w.put("error.mp3", 7);
    import::set_error(w.imp.env.store.conn(), id, Some("MusicBrainz is down")).unwrap();
    // Already in the library: the very bytes of a library file.
    fs::copy(&w.imp.seed, w.imp.inbox.join("same.mp3")).unwrap();
    w.imp.read();
    // Possible duplicate: another file with the library song's title and artist.
    let id = w.put("maybe.mp3", 8);
    w.lookup(
        id,
        vec![candidate("rec-90", "Title90", "Artist90", true, "x/y.mp3")],
    );
    // The same, accepted.
    let id = w.put("accepted.mp3", 9);
    w.lookup(
        id,
        vec![candidate("rec-90", "Title90", "Artist90", true, "x/z.mp3")],
    );
    import::set_confirmed(w.imp.env.store.conn(), id, true).unwrap();
    // Not supported: not audio, and a protected file.
    fs::write(w.imp.inbox.join("notes.txt"), "hello").unwrap();
    fs::write(w.imp.inbox.join("store.m4a"), fairplay_m4a(1)).unwrap();
    w
}

#[test]
fn each_status_has_its_symbol_words_and_explanation() {
    let w = every_status();
    // (file, status, symbol, a fragment of the words, fragments of the
    // explanation, ready)
    #[allow(clippy::type_complexity)]
    let cases: Vec<(&str, Status, &str, &str, &[&str], bool)> = vec![
        (
            "ready.mp3",
            Status::Ready,
            "✓",
            "Ready",
            &["Ready to import"],
            true,
        ),
        ("typed.mp3", Status::Ready, "✓", "Ready", &["typed"], true),
        (
            "choose.mp3",
            Status::Choose,
            "?",
            "Choose",
            &["choose one", "type its tags"],
            false,
        ),
        (
            "nomatch.mp3",
            Status::NoMatch,
            "✗",
            "No match",
            &["No match", "search again"],
            false,
        ),
        (
            "waiting.mp3",
            Status::LookingUp,
            "⋯",
            "Looking up",
            &["Waiting"],
            false,
        ),
        (
            "error.mp3",
            Status::Error("MusicBrainz is down".into()),
            "!",
            "Error",
            &["MusicBrainz is down"],
            false,
        ),
        (
            "maybe.mp3",
            Status::PossibleDuplicate,
            "≈",
            "Possible duplicate",
            &["may already be in the library", "Import anyway"],
            false,
        ),
        (
            "accepted.mp3",
            Status::Ready,
            "✓",
            "Ready",
            &["accepted", "already be in the library"],
            true,
        ),
    ];
    for (file, status, symbol, words, explanation, ready) in cases {
        let r = w.row(file);
        assert_eq!(r.status, status, "{file}");
        assert_eq!(ImportColumn::Status.text(&r), symbol, "{file}");
        assert!(
            status_words(&r.status).contains(words),
            "{file}: {}",
            status_words(&r.status)
        );
        assert_eq!(
            ImportColumn::Status.tooltip(&r).as_deref(),
            Some(status_words(&r.status)),
            "{file}: the status tooltip is its words"
        );
        let said = r.explanation();
        for f in explanation {
            assert!(said.contains(f), "{file}: {said:?} lacks {f:?}");
        }
        assert!(!said.contains("Last try"), "{file}: {said:?}");
        assert_eq!(r.ready(), ready, "{file}");
    }

    // The statuses that need no lookup.
    let same = w.row("same.mp3");
    assert!(
        matches!(same.status, Status::AlreadyInLibrary(_)),
        "{:?}",
        same.status
    );
    assert_eq!(ImportColumn::Status.text(&same), "≡");
    assert!(same.explanation().contains("already in the library"));
    assert!(!same.ready());

    let txt = w.row("notes.txt");
    assert!(
        matches!(txt.status, Status::NotSupported(_)),
        "{:?}",
        txt.status
    );
    assert_eq!(ImportColumn::Status.text(&txt), "–");
    assert!(
        txt.explanation().starts_with("Not supported"),
        "{}",
        txt.explanation()
    );
    assert!(txt.explanation().contains("txt"), "{}", txt.explanation());
    assert_eq!(
        txt.item, None,
        "no row in the index for a file that cannot be imported"
    );
    assert!(!txt.ready());

    let store = w.row("store.m4a");
    assert!(
        store.explanation().contains("FairPlay"),
        "{}",
        store.explanation()
    );
}

#[test]
fn only_the_ready_rows_are_ready() {
    let w = every_status();
    let ready: Vec<String> = w
        .rows()
        .iter()
        .filter(|r| r.ready())
        .map(|r| r.file.clone())
        .collect();
    assert_eq!(ready, ["accepted.mp3", "ready.mp3", "typed.mp3"]);
}

#[test]
fn an_error_kept_with_a_lookup_shows_as_the_last_try_under_the_status() {
    let w = World::new();
    let id = w.put("a.mp3", 4);
    w.lookup(id, vec![uncertain(4)]);
    import::set_error(w.imp.env.store.conn(), id, Some("tags: not found")).unwrap();
    let r = w.row("a.mp3");
    assert_eq!(r.status, Status::Choose, "the lookup stands");
    let said = r.explanation();
    assert!(said.contains("choose one"), "{said:?}");
    assert!(said.ends_with("\nLast try: tags: not found"), "{said:?}");
    assert_eq!(r.error.as_deref(), Some("tags: not found"));

    // A file whose status is the error says it once, not twice.
    let id = w.put("b.mp3", 5);
    import::set_error(w.imp.env.store.conn(), id, Some("it failed")).unwrap();
    let said = w.row("b.mp3").explanation();
    assert_eq!(said.matches("it failed").count(), 1, "{said:?}");
    assert!(!said.contains("Last try"));
}

#[test]
fn the_default_order_puts_what_waits_for_the_user_first_and_ready_last_then_by_path() {
    let w = every_status();
    // A second file in a few statuses, to see the path break the tie.
    for (rel, n) in [("b-ready.mp3", 10), ("a-ready.mp3", 11)] {
        let id = w.put(rel, n);
        w.lookup(id, vec![certain(n)]);
    }
    let id = w.put("a-choose.mp3", 12);
    w.lookup(id, vec![uncertain(12)]);

    let inbox = w.imp.read();
    let index = w.imp.index();
    let expected = [
        "a-choose.mp3",
        "choose.mp3",
        "nomatch.mp3",
        "maybe.mp3",
        "error.mp3",
        "same.mp3",
        "notes.txt",
        "store.m4a",
        "waiting.mp3",
        "a-ready.mp3",
        "accepted.mp3",
        "b-ready.mp3",
        "ready.mp3",
        "typed.mp3",
    ];
    // `notes.txt` and `store.m4a` are both not supported: by path.
    assert_eq!(names(&rows(&inbox, &index)), expected);

    // The order is the function's, not the inbox's: reversed in, same out.
    let mut reversed: Inbox = inbox.clone();
    reversed.entries.reverse();
    assert_eq!(names(&rows(&reversed, &index)), expected);
}

#[test]
fn a_row_carries_what_the_pane_needs() {
    let w = World::new();
    // A song the library holds, as a possible duplicate, and one same-title
    // file autotag found.
    let id = w.put("sub/maybe.mp3", 8);
    let mut c = candidate("rec-90", "Title90", "Artist90", true, "Artist90/X/01 T.mp3");
    c["same_title"] = serde_json::json!(["Artist90/Other/02 T.mp3"]);
    w.lookup(id, vec![c, uncertain(8)]);
    import::set_choice(w.imp.env.store.conn(), id, Some(1)).unwrap();
    let seed = w.imp.env.track(&w.imp.seed);

    let r = w.row("sub/maybe.mp3");
    assert_eq!(r.file, "sub/maybe.mp3", "relative to the inbox");
    assert_eq!(r.item, Some(id));
    assert_eq!(r.candidate, Some(1));
    assert_eq!(r.candidates.len(), 2);
    assert!(r.candidates[0].certain && !r.candidates[1].certain);
    assert_eq!(r.by, Some(By::Name));
    assert_eq!(
        r.destination.as_deref(),
        Some("Artist8/Album8/08 Title8.mp3")
    );
    assert!(!r.typed && !r.confirmed);
    assert!(r.info.as_ref().is_some_and(|i| i.duration_ms.is_some()));
    // The file's own tags, and those it would get.
    assert_eq!(r.before.as_ref().unwrap().title.as_deref(), Some("Title8"));
    assert_eq!(r.after.as_ref().unwrap().title.as_deref(), Some("Title8"));

    // The chosen candidate is Title8, which is not the library's song, so no
    // duplicate: choose the first and the library track is listed, then the
    // same-title file, which has no track.
    import::set_choice(w.imp.env.store.conn(), id, Some(0)).unwrap();
    let r = w.row("sub/maybe.mp3");
    assert_eq!(r.status, Status::PossibleDuplicate);
    assert_eq!(
        r.duplicates,
        vec![
            (Some(seed.id), seed.path.display().to_string()),
            (None, "Artist90/Other/02 T.mp3".to_owned()),
        ]
    );
}

#[test]
fn the_query_is_the_lookups_else_the_search_typed_before_any_lookup() {
    let w = World::new();
    let id = w.put("a.mp3", 2);
    assert_eq!(w.row("a.mp3").query, None);
    import::look_up_again(
        w.imp.env.store.conn(),
        id,
        Some(&import::Query {
            artist: "a-ha".into(),
            title: "Take On Me".into(),
        }),
    )
    .unwrap();
    assert_eq!(
        w.row("a.mp3").query,
        Some(("a-ha".into(), "Take On Me".into())),
        "no lookup yet: what was typed"
    );
    let mut a = answer(vec![certain(2)]);
    a["query"] = serde_json::json!({"artist": "A-Ha", "title": "Take on me"});
    import::set_lookup(w.imp.env.store.conn(), id, &a.to_string()).unwrap();
    assert_eq!(
        w.row("a.mp3").query,
        Some(("A-Ha".into(), "Take on me".into())),
        "what autotag searched wins"
    );
}

#[test]
fn rows_of_an_empty_inbox_are_empty() {
    let w = World::new();
    assert!(w.rows().is_empty());
    let inbox = Inbox {
        dir: w.imp.inbox.clone(),
        entries: Vec::new(),
        unreadable: Vec::new(),
    };
    assert!(rows(&inbox, &Index::default()).is_empty());
}

// -------------------------------------------------------------- was / text

#[test]
fn was_is_the_value_the_file_has_now_when_it_would_get_another() {
    let r = row("a.mp3", |r| {
        r.before = Some(tags(|t| {
            t.title = Some("Old Title".into());
            t.artist = Some("Same".into());
            t.year = Some(1999);
            t.track_no = Some(3);
            t.track_total = Some(10);
        }));
        r.after = Some(tags(|t| {
            t.title = Some("New Title".into());
            t.artist = Some("Same".into());
            t.album = Some("Brand New".into());
            t.year = Some(2001);
            t.track_no = Some(3);
            t.track_total = Some(12);
            t.genre = None;
        }));
    });
    // (column, text shown, was, tooltip)
    let cases: Vec<(ImportColumn, &str, Option<&str>, Option<&str>)> = vec![
        (
            ImportColumn::Name,
            "New Title",
            Some("Old Title"),
            Some("Now: Old Title"),
        ),
        (ImportColumn::Artist, "Same", None, None),
        // The file had no album: "(empty)".
        (
            ImportColumn::Album,
            "Brand New",
            Some("(empty)"),
            Some("Now: (empty)"),
        ),
        (ImportColumn::Year, "2001", Some("1999"), Some("Now: 1999")),
        (
            ImportColumn::Track,
            "3 of 12",
            Some("3 of 10"),
            Some("Now: 3 of 10"),
        ),
        // Nothing before, nothing after: unchanged.
        (ImportColumn::Genre, "", None, None),
        (ImportColumn::Disc, "", None, None),
        (ImportColumn::AlbumArtist, "", None, None),
    ];
    for (c, shown, was, tooltip) in cases {
        assert_eq!(c.text(&r), shown, "{c:?}");
        assert_eq!(c.was(&r).as_deref(), was, "{c:?}");
        assert_eq!(c.tooltip(&r).as_deref(), tooltip, "{c:?}");
    }
}

#[test]
fn a_value_the_file_would_lose_shows_empty_and_was_the_old_one() {
    let r = row("a.mp3", |r| {
        r.before = Some(tags(|t| t.genre = Some("Rock".into())));
        r.after = Some(tags(|_| {}));
    });
    assert_eq!(ImportColumn::Genre.text(&r), "");
    assert_eq!(ImportColumn::Genre.was(&r).as_deref(), Some("Rock"));
}

#[test]
fn a_row_without_both_tag_sets_has_no_was_and_shows_what_it_has() {
    let t = tags(|t| t.title = Some("Only".into()));
    // Not yet looked up: the file's own tags, nothing to compare to.
    let before_only = row("a.mp3", |r| r.before = Some(t.clone()));
    assert_eq!(ImportColumn::Name.text(&before_only), "Only");
    assert_eq!(ImportColumn::Name.was(&before_only), None);
    assert_eq!(ImportColumn::Name.tooltip(&before_only), None);
    // A file that cannot be read has neither.
    let none = blank("b.txt");
    for c in ImportColumn::ALL {
        assert_eq!(c.was(&none), None, "{c:?}");
    }
    assert_eq!(ImportColumn::Name.text(&none), "");
    assert_eq!(ImportColumn::Time.text(&none), "");
    assert_eq!(ImportColumn::Destination.text(&none), "");
}

#[test]
fn the_columns_that_are_not_tags_never_have_a_was() {
    let r = row("a.mp3", |r| {
        r.before = Some(tags(|t| t.title = Some("A".into())));
        r.after = Some(tags(|t| t.title = Some("B".into())));
        r.info = Some(info(Some(226_000)));
        r.destination = Some("X/Y/01 Z.mp3".into());
    });
    for c in [
        ImportColumn::Status,
        ImportColumn::File,
        ImportColumn::Time,
        ImportColumn::Destination,
    ] {
        assert_eq!(c.was(&r), None, "{c:?}");
    }
    assert_eq!(ImportColumn::File.text(&r), "a.mp3");
    assert_eq!(ImportColumn::Time.text(&r), "3:46");
    assert_eq!(ImportColumn::Destination.text(&r), "X/Y/01 Z.mp3");
    assert_eq!(ImportColumn::Status.title(), "");
}

#[test]
fn track_and_disc_cells_show_n_of_total_in_all_four_forms() {
    let cell = |n, total| {
        let r = row("a.mp3", |r| {
            r.before = Some(tags(|t| {
                t.track_no = n;
                t.track_total = total;
                t.disc_no = n;
                t.disc_total = total;
            }));
        });
        (ImportColumn::Track.text(&r), ImportColumn::Disc.text(&r))
    };
    for (n, total, shown) in [
        (Some(3), Some(10), "3 of 10"),
        (Some(3), None, "3"),
        (None, Some(10), "of 10"),
        (None, None, ""),
    ] {
        assert_eq!(cell(n, total), (shown.to_owned(), shown.to_owned()));
    }
}

// ----------------------------------------------------------------- compare

fn sorted(c: ImportColumn, mut rows: Vec<ImportRow>) -> Vec<String> {
    rows.sort_by(|a, b| c.compare(a, b));
    rows.into_iter().map(|r| r.file).collect()
}

fn with_tags(file: &str, f: impl FnOnce(&mut Tags)) -> ImportRow {
    row(file, |r| r.before = Some(tags(f)))
}

#[test]
fn numeric_columns_sort_by_their_leading_number_not_their_text() {
    let rows = vec![
        with_tags("t12", |t| t.track_no = Some(12)),
        with_tags("t3of10", |t| {
            t.track_no = Some(3);
            t.track_total = Some(10);
        }),
        with_tags("t100", |t| t.track_no = Some(100)),
        with_tags("tnone", |_| {}),
        // No number, only a total: it starts with "of", so it has no number.
        with_tags("tofonly", |t| t.track_total = Some(5)),
    ];
    // Text order would put "100" and "12" before "3 of 10".
    assert_eq!(
        sorted(ImportColumn::Track, rows),
        ["tnone", "tofonly", "t3of10", "t12", "t100"]
    );
    let years = vec![
        with_tags("y2001", |t| t.year = Some(2001)),
        with_tags("y999", |t| t.year = Some(999)),
        with_tags("ynone", |_| {}),
    ];
    assert_eq!(
        sorted(ImportColumn::Year, years),
        ["ynone", "y999", "y2001"]
    );
}

#[test]
fn ties_are_broken_by_path_in_every_column_but_time() {
    let same = |file: &str| {
        row(file, |r| {
            r.before = Some(tags(|t| {
                t.title = Some("Same".into());
                t.artist = Some("Same".into());
                t.album = Some("Same".into());
                t.album_artist = Some("Same".into());
                t.genre = Some("Same".into());
                t.year = Some(2000);
                t.track_no = Some(1);
                t.disc_no = Some(1);
            }));
            r.destination = Some("same".into());
        })
    };
    for c in ImportColumn::ALL {
        if c == ImportColumn::Time || c == ImportColumn::File {
            continue;
        }
        let rows = vec![same("c"), same("a"), same("b")];
        assert_eq!(sorted(c, rows), ["a", "b", "c"], "{c:?}");
    }
}

#[test]
fn the_status_column_sorts_by_rank_then_path() {
    let at = |file: &str, s: Status| row(file, |r| r.status = s);
    let rows = vec![
        at("z-ready", Status::Ready),
        at("a-ready", Status::Ready),
        at("m-choose", Status::Choose),
        at("k-dup", Status::PossibleDuplicate),
        at("x-wait", Status::LookingUp),
        at("e-err", Status::Error("e".into())),
        at("n-no", Status::NoMatch),
        at("s-same", Status::AlreadyInLibrary(1)),
        at("u-un", Status::NotSupported("u".into())),
    ];
    assert_eq!(
        sorted(ImportColumn::Status, rows),
        [
            "m-choose", "n-no", "k-dup", "e-err", "s-same", "u-un", "x-wait", "a-ready", "z-ready"
        ]
    );
}

#[test]
fn text_columns_sort_without_case_or_accents_then_by_path() {
    let rows = vec![
        with_tags("p-zebra", |t| t.title = Some("Zebra".into())),
        with_tags("p-b", |t| t.title = Some("émile".into())),
        with_tags("p-a", |t| t.title = Some("Emile".into())),
        with_tags("p-c", |t| t.title = Some("apple".into())),
        with_tags("p-none", |_| {}),
    ];
    assert_eq!(
        sorted(ImportColumn::Name, rows),
        ["p-none", "p-c", "p-a", "p-b", "p-zebra"]
    );
}

#[test]
fn the_time_column_sorts_by_length_and_a_file_of_unknown_length_goes_first() {
    let at = |file: &str, ms: Option<u32>| row(file, |r| r.info = Some(info(ms)));
    let rows = vec![
        at("long", Some(600_000)),
        at("short", Some(9_000)),
        at("unknown", None),
        row("none", |_| {}),
        at("mid", Some(61_000)),
    ];
    let order = sorted(ImportColumn::Time, rows);
    assert_eq!(order[2..], ["short", "mid", "long"]);
    assert!(order[..2].contains(&"unknown".to_owned()) && order[..2].contains(&"none".to_owned()));
}

// ------------------------------------------------------------------ search

#[test]
fn search_matches_the_file_name_and_the_name_artist_and_album_it_would_get() {
    let r = row("Rips/a-ha/take.mp3", |r| {
        r.before = Some(tags(|t| {
            t.title = Some("Old Name".into());
            t.genre = Some("Pop".into());
        }));
        r.after = Some(tags(|t| {
            t.title = Some("Take On Me".into());
            t.artist = Some("a-ha".into());
            t.album = Some("Hunting High and Low".into());
            t.genre = Some("Synth-Pop".into());
        }));
        r.destination = Some("Elsewhere/dest.mp3".into());
    });
    let cases = [
        ("", true),
        ("take on", true),
        ("TAKE", true),
        ("hunting low", true),
        // Every word must be there, in any field.
        ("a-ha rips hunting", true),
        ("rips/a-ha", true),
        ("take nothing", false),
        // The name it has now, but would not get.
        ("old name", false),
        // Not searched: genre, destination.
        ("synth", false),
        ("elsewhere", false),
    ];
    for (q, hit) in cases {
        assert_eq!(r.matches(&Query::new(q)), hit, "{q:?}");
    }
    // Accents and case are folded.
    let cafe = row("c.mp3", |r| {
        r.after = Some(tags(|t| t.artist = Some("Café del Mar".into())))
    });
    assert!(cafe.matches(&Query::new("CAFE")));
    assert!(cafe.matches(&Query::new("café mar")));
}

#[test]
fn search_uses_the_tags_the_file_has_when_it_has_none_to_get() {
    let r = row("a.mp3", |r| {
        r.before = Some(tags(|t| t.title = Some("Own Title".into())))
    });
    assert!(r.matches(&Query::new("own title")));
    let unreadable = blank("notes.txt");
    assert!(unreadable.matches(&Query::new("notes")));
    assert!(!unreadable.matches(&Query::new("title")));
}

// ----------------------------------------------------------------- heading

#[test]
fn the_heading_says_how_the_song_was_found_and_how_long_it_is() {
    let at = |by: Option<By>, ms: Option<u32>| {
        row("sub/a.mp3", |r| {
            r.by = by;
            r.info = Some(info(ms));
        })
        .heading()
    };
    let h = at(Some(By::Fingerprint), Some(226_000));
    assert!(h.starts_with("sub/a.mp3"), "{h}");
    assert!(
        h.contains("found by fingerprint") && h.contains("3:46"),
        "{h}"
    );
    assert!(!h.contains("by name"));
    let h = at(Some(By::Name), Some(61_000));
    assert!(h.contains("found by name") && h.contains("1:01"), "{h}");
    let h = at(Some(By::Name), None);
    assert!(h.contains("found by name") && !h.contains(':'), "{h}");
    let h = at(None, Some(5_000));
    assert!(h.starts_with("sub/a.mp3") && h.contains("0:05"), "{h}");
    assert!(!h.contains("found"), "{h}");
    // Nothing to add: the file alone.
    assert_eq!(at(None, None), "sub/a.mp3");
    assert_eq!(blank("x.txt").heading(), "x.txt");
}

// --------------------------------------------------- typed tags and changes

fn full_tags() -> Tags {
    Tags {
        title: Some("Take On Me".into()),
        artist: Some("a-ha".into()),
        album: Some("Hunting High and Low".into()),
        album_artist: Some("a-ha".into()),
        composer: Some("Harket".into()),
        genre: Some("Synth-Pop".into()),
        year: Some(1985),
        track_no: Some(3),
        track_total: Some(9),
        disc_no: Some(1),
        disc_total: Some(2),
        compilation: true,
    }
}

#[test]
fn typed_fields_stand_for_the_whole_form_and_read_back_as_the_tags() {
    let t = full_tags();
    let f = typed_fields(&t);
    assert_eq!(f.title.as_deref(), Some("Take On Me"));
    assert_eq!(f.composer.as_deref(), Some("Harket"));
    assert_eq!(
        (f.year, f.track_no, f.track_total, f.disc_no, f.disc_total),
        (Some(1985), Some(3), Some(9), Some(1), Some(2))
    );
    assert_eq!(f.compilation, Some(true));
    // Written over nothing, they give the tags back.
    assert_eq!(f.over(&Tags::default()), t);

    // Compilation off is a value too, not an absent one.
    let off = Tags {
        compilation: false,
        ..full_tags()
    };
    assert_eq!(typed_fields(&off).compilation, Some(false));
    // A field with no value stays out of it.
    let sparse = tags(|t| t.title = Some("Only".into()));
    let f = typed_fields(&sparse);
    assert_eq!(f.title.as_deref(), Some("Only"));
    assert_eq!((&f.artist, f.year, f.track_no), (&None, None, None));
    assert_eq!(f.over(&Tags::default()), sparse);
}

#[test]
fn with_changes_applies_each_change_and_leaves_the_rest() {
    let t = full_tags();
    let changes = Changes {
        title: Some(Some("Renamed".into())),
        album: Some(None),
        year: Some(Some(1986)),
        track_total: Some(None),
        compilation: Some(false),
        ..Changes::default()
    };
    let got = with_changes(&t, &changes);
    assert_eq!(
        got,
        Tags {
            title: Some("Renamed".into()),
            album: None,
            year: Some(1986),
            track_total: None,
            compilation: false,
            ..full_tags()
        }
    );
}

#[test]
fn with_changes_of_nothing_is_the_tags_themselves() {
    for t in [full_tags(), Tags::default()] {
        assert_eq!(with_changes(&t, &Changes::default()), t);
    }
    // Clearing a field that is already empty, and the flag set from off.
    let got = with_changes(
        &Tags::default(),
        &Changes {
            genre: Some(None),
            disc_no: Some(None),
            compilation: Some(true),
            ..Changes::default()
        },
    );
    assert_eq!(
        got,
        Tags {
            compilation: true,
            ..Tags::default()
        }
    );
    // Every field can be set from empty.
    let all = Changes {
        title: Some(Some("a".into())),
        artist: Some(Some("b".into())),
        album: Some(Some("c".into())),
        album_artist: Some(Some("d".into())),
        composer: Some(Some("e".into())),
        genre: Some(Some("f".into())),
        year: Some(Some(1)),
        track_no: Some(Some(2)),
        track_total: Some(Some(3)),
        disc_no: Some(Some(4)),
        disc_total: Some(Some(5)),
        compilation: Some(true),
    };
    let got = with_changes(&Tags::default(), &all);
    assert_eq!(
        got,
        Tags {
            title: Some("a".into()),
            artist: Some("b".into()),
            album: Some("c".into()),
            album_artist: Some("d".into()),
            composer: Some("e".into()),
            genre: Some("f".into()),
            year: Some(1),
            track_no: Some(2),
            track_total: Some(3),
            disc_no: Some(4),
            disc_total: Some(5),
            compilation: true,
        }
    );
}

#[test]
fn the_proposal_is_what_the_file_would_get_else_what_it_has() {
    let a = tags(|t| t.title = Some("After".into()));
    let b = tags(|t| t.title = Some("Before".into()));
    assert_eq!(
        proposal(&row("x", |r| {
            r.before = Some(b.clone());
            r.after = Some(a.clone());
        })),
        Some(a)
    );
    assert_eq!(proposal(&row("x", |r| r.before = Some(b.clone()))), Some(b));
    assert_eq!(proposal(&blank("x")), None);
}

// ------------------------------------------------------------------ labels

#[test]
fn the_import_button_counts_the_ready_songs_in_singular_and_plural() {
    assert_eq!(import_label(0), "Nothing Ready to Import");
    assert_eq!(import_label(1), "Import 1 Ready Song");
    assert_eq!(import_label(2), "Import 2 Ready Songs");
    assert_eq!(import_label(60), "Import 60 Ready Songs");
}

#[test]
fn the_summary_counts_files_and_ready_ones() {
    assert_eq!(summary(&[]), "The inbox is empty");
    let ready = || blank("r.mp3");
    let waiting = || row("w.mp3", |r| r.status = Status::Choose);
    assert_eq!(summary(&[waiting()]), "1 file in the inbox, 0 ready");
    assert_eq!(summary(&[ready()]), "1 file in the inbox, 1 ready");
    assert_eq!(
        summary(&[ready(), waiting(), ready()]),
        "3 files in the inbox, 2 ready"
    );
}

#[test]
fn the_sidebar_label_shows_the_file_count() {
    // An empty inbox shows no count; the row is always there.
    assert_eq!(sidebar_label(0), "Import");
    assert_eq!(sidebar_label(1), "Import (1)");
    assert_eq!(sidebar_label(6), "Import (6)");
}

#[test]
fn the_progress_line_names_the_work_the_position_and_the_file() {
    // Looking up the second of five: the callback gives the zero-based index.
    let l = progress_line(false, 1, 5, "a/b.mp3");
    assert!(l.starts_with("Looking up 2 of 5"), "{l}");
    assert!(l.contains("a/b.mp3"), "{l}");
    let f = progress_line(true, 0, 3, "c.mp3");
    assert!(f.starts_with("Importing 1 of 3"), "{f}");
    // Without a file there is only the work.
    assert!(progress_line(false, 0, 0, "").starts_with("Looking up"));
    assert!(!progress_line(false, 0, 0, "").contains(" of "));
    assert!(progress_line(true, 0, 0, "").starts_with("Importing"));
    // The position never passes the total.
    assert!(progress_line(true, 3, 3, "z.mp3").starts_with("Importing 3 of 3"));
}

// Two songs of the same length keep a fixed order, whatever the order the
// sorter meets them in.
#[test]
fn rows_of_the_same_length_sort_by_path_under_time() {
    let at = |file: &str, ms: Option<u32>| row(file, |r| r.info = Some(info(ms)));
    for ms in [Some(61_000), None] {
        let rows = vec![at("c", ms), at("a", ms), at("b", ms)];
        assert_eq!(sorted(ImportColumn::Time, rows), ["a", "b", "c"], "{ms:?}");
        let rows = vec![at("b", ms), at("c", ms), at("a", ms)];
        assert_eq!(sorted(ImportColumn::Time, rows), ["a", "b", "c"], "{ms:?}");
    }
    let (x, y) = (at("a", Some(1)), at("b", Some(1)));
    assert_eq!(ImportColumn::Time.compare(&x, &y), Ordering::Less);
    assert_eq!(ImportColumn::Time.compare(&y, &x), Ordering::Greater);
}

// ------------------------------------------------------ window decisions

// Lookups start by themselves only for files waiting, in the view, with
// nothing running and no hold; every other combination stays quiet.
#[test]
fn lookups_start_by_themselves_only_when_all_four_conditions_hold() {
    for bits in 0u8..16 {
        let (waiting, shown, idle, held) =
            (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0);
        assert_eq!(
            auto_look_up(waiting, shown, idle, held),
            waiting && shown && idle && !held,
            "waiting {waiting}, shown {shown}, idle {idle}, held {held}"
        );
    }
    assert!(auto_look_up(true, true, true, false));
    // The single condition that stops it, each in turn.
    assert!(!auto_look_up(false, true, true, false));
    assert!(!auto_look_up(true, false, true, false));
    assert!(!auto_look_up(true, true, false, false));
    assert!(!auto_look_up(true, true, true, true));
}

// A reload that kept the pane's radios must not re-tick them from the
// stored choice: the user's tick, not yet confirmed, would be lost.
#[test]
fn the_pane_keeps_its_radios_only_for_the_same_file_with_the_same_labels_in_order() {
    let line = |l: &str| CandidateLine {
        label: l.to_owned(),
        certain: false,
    };
    let shown = |ls: &[&str]| ls.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let cands = |ls: &[&str]| ls.iter().map(|s| line(s)).collect::<Vec<_>>();
    // (same file, labels shown, candidates now, kept)
    #[allow(clippy::type_complexity)]
    let cases: Vec<(&str, bool, Vec<&str>, Vec<&str>, bool)> = vec![
        ("all equal", true, vec!["A", "B"], vec!["A", "B"], true),
        ("one candidate", true, vec!["A"], vec!["A"], true),
        (
            "not the same file",
            false,
            vec!["A", "B"],
            vec!["A", "B"],
            false,
        ),
        ("no radios shown yet", true, vec![], vec!["A", "B"], false),
        ("no radios at all", true, vec![], vec![], false),
        ("fewer candidates", true, vec!["A", "B"], vec!["A"], false),
        ("more candidates", true, vec!["A"], vec!["A", "B"], false),
        (
            "a label changed",
            true,
            vec!["A", "B"],
            vec!["A", "C"],
            false,
        ),
        ("another order", true, vec!["A", "B"], vec!["B", "A"], false),
        ("none left", true, vec!["A"], vec![], false),
    ];
    for (name, same, labels, now, kept) in cases {
        assert_eq!(
            keep_radios(same, &shown(&labels), &cands(&now)),
            kept,
            "{name}"
        );
    }
}
