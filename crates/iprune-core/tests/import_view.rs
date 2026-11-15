//! `import::view`, `import::pending`, `View::ids`, `Fields::over`,
//! `Fields::differences` and `Lookup::parse` (technical document, section
//! 7.2, "Status"). Pure: entries and index tracks are built by hand, so no
//! file, process or database is involved.

use std::path::PathBuf;

use iprune_core::import::{
    self, Entry, Fields, Inbox, Index, Item, Lookup, Query, Status, View, pending,
};
use iprune_core::store::TrackRow;
use iprune_core::tags::{AudioInfo, FileKind, FileTags, ReadError, Tags};
use serde_json::{Value, json};

const HASH: [u8; 32] = [7; 32];

fn info(ms: u32) -> AudioInfo {
    AudioInfo {
        kind: FileKind::Mp3,
        duration_ms: Some(ms),
        bitrate: Some(128_000),
        samplerate: Some(44_100),
    }
}

fn tags(title: &str, artist: &str) -> Tags {
    Tags {
        title: Some(title.into()),
        artist: Some(artist.into()),
        ..Tags::default()
    }
}

/// A library track; `n` makes its id, hash and path.
fn track(n: u8, tags: Tags, ms: u32) -> TrackRow {
    TrackRow {
        id: i64::from(n),
        path: PathBuf::from(format!("/music/{n}.mp3")),
        path_key: format!("/music/{n}.mp3"),
        size: 1,
        mtime_ns: 1,
        hash: [n; 32],
        info: info(ms),
        tags,
        tags_hash: [0; 32],
        added_at: 0,
        missing_since: None,
        tag_error: None,
        drm: false,
    }
}

fn missing(mut t: TrackRow) -> TrackRow {
    t.missing_since = Some(5);
    t
}

/// A candidate of a stored lookup.
fn cand(title: &str, artist: &str, certain: bool, same_title: &[&str]) -> Value {
    json!({
        "recording": format!("rec-{title}"),
        "release": format!("rel-{title}"),
        "label": "label",
        "length_ms": 200_000,
        "certain": certain,
        "tags": {"title": title, "artist": artist},
        "destination": format!("{artist}/{title}.mp3"),
        "same_title": same_title,
    })
}

fn lookup_text(candidates: Vec<Value>) -> String {
    json!({
        "by": "name",
        "query": {"artist": "a", "title": "t"},
        "duration_ms": 200_000,
        "candidates": candidates,
    })
    .to_string()
}

fn item(lookup: Option<String>) -> Item {
    Item {
        id: 1,
        path: PathBuf::from("/inbox/a.mp3"),
        size: 10,
        mtime_ns: 20,
        hash: HASH,
        lookup,
        search: None,
        choice: None,
        tags: None,
        confirmed: false,
        error: None,
        added_at: 0,
    }
}

fn entry(item: Item) -> Entry {
    Entry {
        path: item.path.clone(),
        file: Ok(FileTags {
            info: info(200_000),
            tags: tags("Song", "Art"),
            tag_error: None,
            drm: false,
        }),
        item: Some(item),
    }
}

fn view_of(e: &Entry, library: &[TrackRow]) -> View {
    import::view(e, &Index::new(library))
}

fn typed(title: &str, artist: &str) -> Fields {
    Fields {
        title: Some(title.into()),
        artist: Some(artist.into()),
        ..Fields::default()
    }
}

// -- The statuses, in their order ------------------------------------------

#[test]
fn a_file_that_cannot_be_imported_is_not_supported_or_an_error_whatever_else_is_known() {
    // Not supported and unreadable come before the library check.
    let mut e = entry(item(None));
    e.file = Err(ReadError::Unsupported(
        "txt files cannot be imported".into(),
    ));
    let v = view_of(&e, &[track(1, tags("x", "y"), 1)]);
    assert_eq!(
        v.status,
        Status::NotSupported("txt files cannot be imported".into())
    );
    assert_eq!(v.fields, None);

    let mut same_hash = track(1, tags("x", "y"), 1);
    same_hash.hash = HASH;
    let mut e = entry(item(Some(lookup_text(vec![cand(
        "Song",
        "Art",
        true,
        &[],
    )]))));
    e.file = Err(ReadError::Unreadable("bad header".into()));
    assert_eq!(
        view_of(&e, &[same_hash.clone()]).status,
        Status::Error("bad header".into()),
        "unreadable wins over already in library"
    );

    // No row at all (an unsupported extension has none).
    let mut e = entry(item(None));
    e.item = None;
    assert!(matches!(view_of(&e, &[]).status, Status::NotSupported(_)));

    // FairPlay, even when the same file is in the library and a lookup is stored.
    let mut e = entry(item(Some(lookup_text(vec![cand(
        "Song",
        "Art",
        true,
        &[],
    )]))));
    e.file.as_mut().unwrap().drm = true;
    let v = view_of(&e, &[same_hash]);
    assert!(
        matches!(&v.status, Status::NotSupported(w) if w.contains("FairPlay")),
        "{:?}",
        v.status
    );
    assert!(v.duplicates.is_empty() && v.fields.is_none());
}

#[test]
fn a_file_not_looked_up_waits_or_shows_its_error() {
    let v = view_of(&entry(item(None)), &[]);
    assert_eq!(v.status, Status::LookingUp);

    let mut i = item(None);
    i.error = Some("network down".into());
    assert_eq!(
        view_of(&entry(i), &[]).status,
        Status::Error("network down".into())
    );
}

#[test]
fn an_identical_file_in_the_library_is_already_there_and_never_otherwise() {
    let mine = {
        let mut t = track(3, tags("Other", "Other"), 5_000);
        t.hash = HASH;
        t
    };
    // Even with a certain lookup stored and the file confirmed.
    let mut i = item(Some(lookup_text(vec![cand("Song", "Art", true, &[])])));
    i.confirmed = true;
    let v = view_of(&entry(i), std::slice::from_ref(&mine));
    assert_eq!(v.status, Status::AlreadyInLibrary(3));
    assert_eq!(v.duplicates, vec![3]);
    assert_eq!(v.fields, None, "nothing would be filed");

    // Not looked up yet: already in library all the same, never LookingUp.
    assert_eq!(
        view_of(&entry(item(None)), std::slice::from_ref(&mine)).status,
        Status::AlreadyInLibrary(3)
    );
}

#[test]
fn a_hash_equal_to_a_missing_tracks_is_no_duplicate() {
    // The same hash, the same audio and the same title, but the track is
    // missing: once filed, the scan gives it its file back.
    let gone = missing({
        let mut t = track(3, tags("Song", "Art"), 200_000);
        t.hash = HASH;
        t
    });
    let e = entry(item(Some(lookup_text(vec![cand(
        "Song",
        "Art",
        true,
        &[],
    )]))));
    let v = view_of(&e, &[gone]);
    assert_eq!(v.status, Status::Ready);
    assert!(v.duplicates.is_empty());
}

#[test]
fn a_stored_lookup_that_no_longer_parses_is_an_error_not_a_panic() {
    for bad in [
        "not json",
        "{}",
        r#"{"by": "name"}"#,
        r#"{"by":"x","query":{"artist":"","title":""},"duration_ms":1,"candidates":[]}"#,
    ] {
        let v = view_of(&entry(item(Some(bad.into()))), &[]);
        assert!(
            matches!(&v.status, Status::Error(m) if m.contains("look it up again")),
            "{bad}: {:?}",
            v.status
        );
    }
}

#[test]
fn no_candidate_and_no_typed_tags_is_no_match_and_keeps_the_last_error() {
    let mut i = item(Some(lookup_text(vec![])));
    i.error = Some("a filing error".into());
    let v = view_of(&entry(i), &[]);
    assert_eq!(v.status, Status::NoMatch);
    assert_eq!(v.error.as_deref(), Some("a filing error"));
    assert!(v.lookup.is_some());
    assert_eq!(
        (v.candidate, v.fields.as_ref(), v.destination.as_ref()),
        (None, None, None)
    );
}

#[test]
fn typed_tags_make_a_file_without_candidates_ready() {
    let mut i = item(Some(lookup_text(vec![])));
    i.tags = Some(typed("Typed", "Me"));
    let v = view_of(&entry(i), &[]);
    assert_eq!(v.status, Status::Ready);
    assert_eq!(v.fields, Some(typed("Typed", "Me")));
    assert_eq!(v.candidate, None);
    assert_eq!(v.destination, None, "autotag chooses it for typed tags");
    assert_eq!(v.ids(), None, "typed tags send no MusicBrainz ids");
}

// -- Which tags: typed, chosen, first ---------------------------------------

#[test]
fn the_first_candidate_is_taken_when_nothing_is_chosen_and_ready_only_when_certain() {
    let lookup = lookup_text(vec![
        cand("First", "A", true, &[]),
        cand("Second", "B", false, &[]),
    ]);
    let v = view_of(&entry(item(Some(lookup))), &[]);
    assert_eq!(v.status, Status::Ready);
    assert_eq!(v.candidate, Some(0));
    assert_eq!(v.fields, Some(typed("First", "A")));
    assert_eq!(v.destination.as_deref(), Some("A/First.mp3"));
    assert_eq!(v.ids(), Some(("rec-First", "rel-First")));

    // The tags it would have: the candidate's over the file's.
    let t = v.tags.unwrap();
    assert_eq!(
        (t.title.as_deref(), t.artist.as_deref()),
        (Some("First"), Some("A"))
    );
}

#[test]
fn the_ten_second_rule_is_the_first_candidates_certain_flag() {
    // Not certain: the user has to choose.
    let lookup = lookup_text(vec![
        cand("First", "A", false, &[]),
        cand("Second", "B", true, &[]),
    ]);
    let v = view_of(&entry(item(Some(lookup.clone()))), &[]);
    assert_eq!(v.status, Status::Choose);
    // The tags and destination are shown all the same, for the choice.
    assert_eq!(v.candidate, Some(0));
    assert_eq!(v.fields, Some(typed("First", "A")));

    // A certain second candidate does not make the first ready.
    assert!(v.lookup.as_ref().unwrap().candidates[1].certain);

    // Choosing the second, whatever its flag, is a decision.
    let mut i = item(Some(lookup));
    i.choice = Some(1);
    let v = view_of(&entry(i), &[]);
    assert_eq!(v.status, Status::Ready);
    assert_eq!(v.candidate, Some(1));
    assert_eq!(v.ids(), Some(("rec-Second", "rel-Second")));
}

#[test]
fn choosing_an_uncertain_candidate_is_ready() {
    let lookup = lookup_text(vec![
        cand("First", "A", true, &[]),
        cand("Second", "B", false, &[]),
    ]);
    let mut i = item(Some(lookup));
    i.choice = Some(1);
    let v = view_of(&entry(i), &[]);
    assert_eq!(v.status, Status::Ready);
    assert_eq!(v.fields, Some(typed("Second", "B")));
}

#[test]
fn a_choice_out_of_range_falls_back_to_the_first_candidate() {
    let uncertain = lookup_text(vec![
        cand("First", "A", false, &[]),
        cand("Second", "B", true, &[]),
    ]);
    let mut i = item(Some(uncertain));
    i.choice = Some(2);
    let v = view_of(&entry(i), &[]);
    assert_eq!(v.candidate, Some(0));
    assert_eq!(v.fields, Some(typed("First", "A")));
    assert_eq!(v.status, Status::Choose, "a stale choice is no decision");

    let certain = lookup_text(vec![cand("First", "A", true, &[])]);
    let mut i = item(Some(certain));
    i.choice = Some(99);
    let v = view_of(&entry(i), &[]);
    assert_eq!((v.status, v.candidate), (Status::Ready, Some(0)));
}

#[test]
fn typed_tags_win_over_a_choice_which_wins_over_the_first_candidate() {
    let lookup = lookup_text(vec![
        cand("First", "A", false, &[]),
        cand("Second", "B", false, &[]),
    ]);
    let mut i = item(Some(lookup));
    i.choice = Some(1);
    i.tags = Some(typed("Typed", "T"));
    let v = view_of(&entry(i), &[]);
    assert_eq!(v.fields, Some(typed("Typed", "T")));
    assert_eq!(v.candidate, None, "typed tags take no candidate");
    assert_eq!(v.destination, None);
    assert!(v.same_title.is_empty());
    assert_eq!(v.ids(), None);
    assert_eq!(v.status, Status::Ready, "typing is a decision");
    // The file's own tags fill what the typed ones leave out.
    let t = v.tags.unwrap();
    assert_eq!(
        (t.title.as_deref(), t.artist.as_deref()),
        (Some("Typed"), Some("T"))
    );
}

#[test]
fn the_tags_a_file_would_get_keep_what_the_candidate_leaves_out() {
    let lookup = json!({
        "by": "name", "query": {"artist": "", "title": ""}, "duration_ms": 1,
        "candidates": [{
            "recording": "r", "release": "l", "label": "", "length_ms": null,
            "certain": true, "tags": {"title": "New", "year": 1999},
            "destination": null, "same_title": []
        }]
    })
    .to_string();
    let mut e = entry(item(Some(lookup)));
    e.file.as_mut().unwrap().tags.album = Some("Old album".into());
    let v = view_of(&e, &[]);
    let t = v.tags.unwrap();
    assert_eq!(t.title.as_deref(), Some("New"));
    assert_eq!(t.artist.as_deref(), Some("Art"), "the file's artist stays");
    assert_eq!(t.album.as_deref(), Some("Old album"));
    assert_eq!(t.year, Some(1999));
    assert_eq!(v.destination, None, "the root is not mounted");
}

// -- Possible duplicates -----------------------------------------------------

fn certain_lookup() -> String {
    lookup_text(vec![cand("Take On Me", "a-ha", true, &[])])
}

#[test]
fn the_same_audio_in_the_library_is_a_possible_duplicate_until_confirmed() {
    // Same kind, sample rate, bitrate and duration; different tags and bytes.
    let lib = track(4, tags("Unrelated", "Someone"), 200_000);
    let v = view_of(
        &entry(item(Some(certain_lookup()))),
        std::slice::from_ref(&lib),
    );
    assert_eq!(v.status, Status::PossibleDuplicate);
    assert_eq!(v.duplicates, vec![4]);

    let mut i = item(Some(certain_lookup()));
    i.confirmed = true;
    let v = view_of(&entry(i), std::slice::from_ref(&lib));
    assert_eq!(v.status, Status::Ready);
    assert_eq!(v.duplicates, vec![4], "still shown once accepted");

    // One millisecond apart is not the same audio.
    let other = track(4, tags("Unrelated", "Someone"), 200_001);
    let v = view_of(&entry(item(Some(certain_lookup()))), &[other]);
    assert_eq!((v.status, v.duplicates.len()), (Status::Ready, 0));
}

#[test]
fn the_same_title_under_the_same_album_artist_is_a_possible_duplicate() {
    // Normalized: case, accents, punctuation and a leading "the" do not count.
    let mut lib_tags = tags("TAKE on me!", "ignored artist");
    lib_tags.album_artist = Some("A-Ha".into());
    let lib = track(5, lib_tags, 1_000);
    let mut i = item(Some(certain_lookup()));
    // The candidate gives an artist; with no album artist the artist is used.
    let v = view_of(&entry(i.clone()), std::slice::from_ref(&lib));
    assert_eq!(v.status, Status::PossibleDuplicate, "{v:?}");
    assert_eq!(v.duplicates, vec![5]);
    i.confirmed = true;
    assert_eq!(view_of(&entry(i), &[lib]).status, Status::Ready);
}

#[test]
fn the_title_is_compared_under_the_album_artist_else_the_artist() {
    // Library: no album artist, artist a-ha. Incoming: album artist a-ha.
    let lib = track(5, tags("Take On Me", "a-ha"), 1_000);
    let mut typed_tags = typed("Take On Me", "someone else");
    typed_tags.album_artist = Some("a-ha".into());
    let mut i = item(Some(lookup_text(vec![])));
    i.tags = Some(typed_tags);
    let v = view_of(&entry(i), std::slice::from_ref(&lib));
    assert_eq!(v.status, Status::PossibleDuplicate);

    // Another album artist: a different song, a cover for instance.
    let mut other = typed("Take On Me", "a-ha");
    other.album_artist = Some("Various Artists".into());
    let mut i = item(Some(lookup_text(vec![])));
    i.tags = Some(other);
    let v = view_of(&entry(i), &[lib]);
    assert_eq!(v.status, Status::Ready);
    assert!(v.duplicates.is_empty());
}

#[test]
fn a_song_without_a_title_matches_nothing_by_title() {
    // The library track has no title either, and the same (empty) artist.
    let lib = track(
        6,
        Tags {
            artist: Some("Art".into()),
            ..Tags::default()
        },
        1_000,
    );
    let mut i = item(Some(lookup_text(vec![])));
    i.tags = Some(Fields {
        artist: Some("Art".into()),
        ..Fields::default()
    });
    let mut e = entry(i);
    e.file.as_mut().unwrap().tags.title = None;
    let v = view_of(&e, &[lib]);
    assert_eq!(v.status, Status::Ready);
    assert!(v.duplicates.is_empty());
}

#[test]
fn the_duplicate_check_uses_the_tags_the_file_would_get_not_the_ones_it_has() {
    // The file says "Song" by "Art"; the candidate retitles it.
    let lib = track(11, tags("Take On Me", "a-ha"), 1_000);
    let v = view_of(&entry(item(Some(certain_lookup()))), &[lib]);
    assert_eq!(v.status, Status::PossibleDuplicate);
}

#[test]
fn files_autotag_found_with_the_same_title_are_a_possible_duplicate_of_the_candidate_taken() {
    let lookup = lookup_text(vec![
        cand("First", "A", true, &["A/Old/01 First.mp3"]),
        cand("Second", "B", false, &[]),
    ]);
    let v = view_of(&entry(item(Some(lookup.clone()))), &[]);
    assert_eq!(v.status, Status::PossibleDuplicate);
    assert_eq!(v.same_title, vec!["A/Old/01 First.mp3".to_owned()]);
    assert!(
        v.duplicates.is_empty(),
        "no library track, only autotag's list"
    );

    let mut i = item(Some(lookup.clone()));
    i.confirmed = true;
    assert_eq!(view_of(&entry(i), &[]).status, Status::Ready);

    // The list is the chosen candidate's: the second has none.
    let mut i = item(Some(lookup));
    i.choice = Some(1);
    let v = view_of(&entry(i), &[]);
    assert_eq!(v.status, Status::Ready);
    assert!(v.same_title.is_empty());

    // Typed tags take no candidate, so no list.
    let lookup = lookup_text(vec![cand("First", "A", true, &["A/Old/01 First.mp3"])]);
    let mut i = item(Some(lookup));
    i.tags = Some(typed("Typed", "T"));
    assert_eq!(view_of(&entry(i), &[]).status, Status::Ready);
}

#[test]
fn confirming_a_duplicate_does_not_make_an_undecided_file_ready() {
    let lookup = lookup_text(vec![cand("First", "A", false, &["A/Old/01 First.mp3"])]);
    let mut i = item(Some(lookup));
    i.confirmed = true;
    assert_eq!(view_of(&entry(i), &[]).status, Status::Choose);
}

#[test]
fn duplicates_are_listed_once_each_in_id_order() {
    // Track 9 matches by audio and by title; 8 only by title; 2 only by audio.
    let both = track(9, tags("Take On Me", "a-ha"), 200_000);
    let title = track(8, tags("take on me", "A-HA"), 1);
    let audio = track(2, tags("Other", "Other"), 200_000);
    let v = view_of(&entry(item(Some(certain_lookup()))), &[both, title, audio]);
    assert_eq!(v.duplicates, vec![2, 8, 9]);
}

#[test]
fn view_is_pure() {
    let lib = [track(4, tags("Unrelated", "Someone"), 200_000)];
    let e = entry(item(Some(certain_lookup())));
    assert_eq!(view_of(&e, &lib), view_of(&e, &lib));
}

// -- pending -----------------------------------------------------------------

fn inbox(entries: Vec<Entry>) -> Inbox {
    Inbox {
        dir: PathBuf::from("/inbox"),
        entries,
        unreadable: Vec::new(),
    }
}

#[test]
fn pending_lists_readable_unlooked_files_not_in_the_library() {
    let named = |name: &str, mut i: Item| {
        i.path = PathBuf::from(format!("/inbox/{name}"));
        entry(i)
    };
    let looked = named("looked.mp3", item(Some(lookup_text(vec![]))));
    let fresh = named("fresh.mp3", item(None));
    let mut failed_item = item(None);
    failed_item.error = Some("timeout".into());
    let failed = named("failed.mp3", failed_item);
    let mut unsupported = named("u.txt", item(None));
    unsupported.item = None;
    unsupported.file = Err(ReadError::Unsupported("txt".into()));
    let mut unreadable = named("bad.mp3", item(None));
    unreadable.file = Err(ReadError::Unreadable("bad".into()));
    let mut drm = named("drm.m4a", item(None));
    drm.file.as_mut().unwrap().drm = true;
    let mut held_item = item(None);
    held_item.hash = [9; 32];
    let held = named("held.mp3", held_item);

    let lib = track(9, tags("x", "y"), 1);
    let ib = inbox(vec![
        looked,
        fresh,
        failed,
        unsupported,
        unreadable,
        drm,
        held,
    ]);
    let index = Index::new(&[lib]);
    let names = |retry| -> Vec<String> {
        pending(&ib, &index, retry)
            .iter()
            .map(|e| e.path.file_name().unwrap().to_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(names(false), ["fresh.mp3"]);
    assert_eq!(
        names(true),
        ["fresh.mp3", "failed.mp3"],
        "retry adds the failed ones, in inbox order"
    );
}

// -- Fields ------------------------------------------------------------------

fn full_tags() -> Tags {
    Tags {
        title: Some("t".into()),
        artist: Some("a".into()),
        album: Some("al".into()),
        album_artist: Some("aa".into()),
        composer: Some("c".into()),
        genre: Some("g".into()),
        year: Some(1990),
        track_no: Some(2),
        track_total: Some(9),
        disc_no: Some(1),
        disc_total: Some(2),
        compilation: true,
    }
}

#[test]
fn over_writes_the_fields_given_and_keeps_the_others() {
    let sent = Fields {
        title: Some("  New  ".into()),
        album: Some("New album".into()),
        year: Some(2001),
        track_no: Some(5),
        disc_total: Some(3),
        compilation: Some(false),
        ..Fields::default()
    };
    let got = sent.over(&full_tags());
    let want = Tags {
        title: Some("New".into()),
        album: Some("New album".into()),
        year: Some(2001),
        track_no: Some(5),
        disc_total: Some(3),
        compilation: false,
        ..full_tags()
    };
    assert_eq!(got, want);
}

#[test]
fn over_cannot_clear_a_field() {
    // Empty and blank strings, and absent numbers, leave the file's value.
    let sent = Fields {
        title: Some(String::new()),
        artist: Some("   ".into()),
        genre: None,
        ..Fields::default()
    };
    assert_eq!(sent.over(&full_tags()), full_tags());
    assert_eq!(Fields::default().over(&full_tags()), full_tags());
    // A compilation flag not sent stays; one sent as true over false sets it.
    let base = Tags {
        compilation: false,
        ..Tags::default()
    };
    let set = Fields {
        compilation: Some(true),
        ..Fields::default()
    };
    assert!(set.over(&base).compilation);
    assert!(Fields::default().over(&full_tags()).compilation);
}

#[test]
fn differences_names_each_field_that_reads_back_otherwise() {
    let read = full_tags();
    let cases: Vec<(&str, Fields)> = vec![
        (
            "title",
            Fields {
                title: Some("x".into()),
                ..Fields::default()
            },
        ),
        (
            "artist",
            Fields {
                artist: Some("x".into()),
                ..Fields::default()
            },
        ),
        (
            "album",
            Fields {
                album: Some("x".into()),
                ..Fields::default()
            },
        ),
        (
            "album artist",
            Fields {
                album_artist: Some("x".into()),
                ..Fields::default()
            },
        ),
        (
            "composer",
            Fields {
                composer: Some("x".into()),
                ..Fields::default()
            },
        ),
        (
            "genre",
            Fields {
                genre: Some("x".into()),
                ..Fields::default()
            },
        ),
        (
            "year",
            Fields {
                year: Some(1),
                ..Fields::default()
            },
        ),
        (
            "track",
            Fields {
                track_no: Some(1),
                ..Fields::default()
            },
        ),
        (
            "track total",
            Fields {
                track_total: Some(1),
                ..Fields::default()
            },
        ),
        (
            "disc",
            Fields {
                disc_no: Some(7),
                ..Fields::default()
            },
        ),
        (
            "disc total",
            Fields {
                disc_total: Some(7),
                ..Fields::default()
            },
        ),
        (
            "compilation",
            Fields {
                compilation: Some(false),
                ..Fields::default()
            },
        ),
    ];
    for (name, sent) in cases {
        assert_eq!(sent.differences(&read), vec![name], "{name}");
    }
}

#[test]
fn differences_is_empty_when_everything_sent_reads_back() {
    let read = full_tags();
    let sent = Fields {
        title: Some(" t ".into()), // trimmed on both sides
        artist: Some("a".into()),
        album: Some("al".into()),
        album_artist: Some("aa".into()),
        composer: Some("c".into()),
        genre: Some("g".into()),
        year: Some(1990),
        track_no: Some(2),
        track_total: Some(9),
        disc_no: Some(1),
        disc_total: Some(2),
        compilation: Some(true),
    };
    assert!(sent.differences(&read).is_empty());
    assert!(
        Fields::default().differences(&Tags::default()).is_empty(),
        "nothing sent, nothing to check"
    );
    // A reader that pads a value with blanks is not a difference.
    let padded = Tags {
        title: Some("  t ".into()),
        ..read.clone()
    };
    assert!(sent.differences(&padded).is_empty());
    // An empty field sent is left alone by autotag, so it is not checked.
    let blank = Fields {
        title: Some("  ".into()),
        ..Fields::default()
    };
    assert!(blank.differences(&Tags::default()).is_empty());
}

#[test]
fn differences_reports_a_field_sent_and_not_read_at_all() {
    let sent = Fields {
        title: Some("t".into()),
        year: Some(1999),
        ..Fields::default()
    };
    assert_eq!(sent.differences(&Tags::default()), vec!["title", "year"]);
    // A text field that came back with other text in the same position.
    let read = Tags {
        title: Some("t2".into()),
        year: Some(1999),
        ..Tags::default()
    };
    assert_eq!(sent.differences(&read), vec!["title"]);
}

// -- Lookup::parse -----------------------------------------------------------

#[test]
fn a_null_genre_means_not_looked_up_and_an_absent_one_means_none_known() {
    let text = json!({
        "by": "fingerprint",
        "query": {"artist": "a", "title": "t"},
        "duration_ms": 1000,
        "candidates": [
            {"recording": "r1", "release": "l1", "label": "", "length_ms": 1, "certain": true,
             "tags": {"title": "t", "genre": "Pop"}, "destination": "x/y.mp3", "same_title": []},
            {"recording": "r2", "release": "l2", "label": "", "length_ms": null, "certain": false,
             "tags": {"title": "t"}, "destination": null, "same_title": ["a.mp3"]},
            {"recording": "r3", "release": "l3", "label": "", "length_ms": 3, "certain": false,
             "tags": {"title": "t", "genre": null}, "destination": null, "same_title": []}
        ]
    })
    .to_string();
    let l = Lookup::parse(&text).unwrap();
    assert_eq!(l.by, import::By::Fingerprint);
    assert_eq!(
        l.query,
        Query {
            artist: "a".into(),
            title: "t".into()
        }
    );
    assert_eq!(l.duration_ms, 1000);
    let c = &l.candidates;
    assert_eq!(
        c.iter().map(|c| c.genre_unknown).collect::<Vec<_>>(),
        [false, false, true]
    );
    assert_eq!(c[0].tags.genre.as_deref(), Some("Pop"));
    assert_eq!(c[1].tags.genre, None);
    assert_eq!(c[2].tags.genre, None);
    assert_eq!(c[1].length_ms, None);
    assert_eq!(c[1].destination, None);
    assert_eq!(c[1].same_title, vec!["a.mp3".to_owned()]);
    assert!(c[0].certain && !c[1].certain);
}

#[test]
fn a_lookup_with_no_candidates_parses_and_a_malformed_one_does_not() {
    let l = Lookup::parse(
        r#"{"by":"name","query":{"artist":"","title":""},"duration_ms":0,"candidates":[]}"#,
    )
    .unwrap();
    assert!(l.candidates.is_empty());
    for bad in [
        "",
        "[]",
        r#"{"by":"name"}"#,
        r#"{"by":"magic","query":{"artist":"","title":""},"duration_ms":0,"candidates":[]}"#,
    ] {
        assert!(Lookup::parse(bad).is_err(), "{bad:?}");
    }
}
