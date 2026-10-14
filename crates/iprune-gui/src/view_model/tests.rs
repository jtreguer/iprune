use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::PathBuf;

use iprune_core::playlist::Entry;
use iprune_core::tags::{AudioInfo, FileKind, Tags};

use super::*;

fn track(id: i64, f: impl FnOnce(&mut TrackRow)) -> TrackRow {
    let mut t = TrackRow {
        id,
        path: PathBuf::from(format!("/music/file{id}.mp3")),
        path_key: format!("/music/file{id}.mp3"),
        size: 1_000_000,
        mtime_ns: 0,
        hash: [0; 32],
        info: AudioInfo {
            kind: FileKind::Mp3,
            duration_ms: Some(60_000),
            bitrate: None,
            samplerate: None,
        },
        tags: Tags::default(),
        tags_hash: [0; 32],
        added_at: 1_700_000_000,
        missing_since: None,
        tag_error: None,
        drm: false,
    };
    f(&mut t);
    t
}

fn tagged(title: &str, artist: &str, album: &str) -> TrackRow {
    track(1, |t| {
        t.tags.title = Some(title.into());
        t.tags.artist = Some(artist.into());
        t.tags.album = Some(album.into());
    })
}

/// A row with every field empty, to set what a test needs.
fn blank() -> Row {
    let mut r = Row::unresolved("/x/blank.mp3", 1);
    r.position = None;
    r.name = String::new();
    r.path = PathBuf::new();
    r.unresolved = false;
    r.fold();
    r
}

fn row(f: impl FnOnce(&mut Row)) -> Row {
    let mut r = blank();
    f(&mut r);
    r.fold();
    r
}

fn from(t: &TrackRow) -> Row {
    Row::from_track(t, None, None)
}

// ---------------------------------------------------------------- matches

#[test]
fn matches_table() {
    let mut full = tagged("Blue Monday", "New Order", "Power, Corruption & Lies");
    full.tags.album_artist = Some("Various Artists".into());
    full.tags.composer = Some("Sumner/Hook".into());
    full.tags.genre = Some("Synthpop".into());
    let full = from(&full);

    let cafe = from(&tagged("Café del Mar", "Énergie", "Ibiza"));
    let nfd = from(&tagged("Cafe\u{301}", "x", "y")); // e + combining acute

    let cases: Vec<(&str, &Row, &str, bool)> = vec![
        ("title", &full, "monday", true),
        ("artist", &full, "order", true),
        ("album", &full, "corruption", true),
        ("album artist", &full, "various", true),
        ("composer", &full, "hook", true),
        ("genre is not searched", &full, "synthpop", false),
        ("case", &full, "BLUE monday", true),
        ("any order", &full, "order blue", true),
        ("across fields", &full, "lies new monday", true),
        ("one word missing", &full, "blue tuesday", false),
        ("no match", &full, "zzz", false),
        ("punctuation in query", &full, "blue, monday!", true),
        ("punctuation in data", &full, "power corruption lies", true),
        ("ampersand only", &full, "&", true),
        ("slash in composer", &full, "sumner hook", true),
        ("empty query", &full, "", true),
        ("spaces only", &full, "   ", true),
        ("punctuation only", &full, " ,;- ", true),
        ("accent in data, plain query", &cafe, "cafe", true),
        ("accent in query, accent in data", &cafe, "CAFÉ del", true),
        ("accent in artist", &cafe, "energie", true),
        ("plain data, accented query", &full, "bluë", true),
        ("decomposed data", &nfd, "café", true),
        ("decomposed query", &cafe, "cafe\u{301}", true),
    ];
    for (what, row, query, want) in cases {
        assert_eq!(row.matches(&Query::new(query)), want, "{what}: {query:?}");
    }
}

#[test]
fn matches_uses_the_file_name_when_the_title_tag_is_empty() {
    let r = from(&track(7, |t| {
        t.path = PathBuf::from("/m/Untitled Demo 3.mp3")
    }));
    assert!(r.matches(&Query::new("demo")));
    assert!(!r.matches(&Query::new("mp3")));
}

#[test]
fn matches_an_unresolved_entry_by_its_file_stem_only() {
    let r = Row::unresolved("../old/Gone Song.flac", 2);
    assert!(r.matches(&Query::new("gone song")));
    assert!(!r.matches(&Query::new("flac")));
    assert!(!r.matches(&Query::new("old")));
}

#[test]
fn matches_ignores_a_leading_the_in_the_data_and_in_the_query() {
    let r = from(&tagged("Wall", "The Beatles", "Abbey Road"));
    assert!(r.matches(&Query::new("beatles")));
    assert!(r.matches(&Query::new("the beatles")));
}

// Bug: "the" is dropped from the front of each field when searching, so a
// query that has "the" anywhere but first can never find it.
#[test]
fn matches_a_the_that_is_not_first_in_the_query() {
    let r = from(&tagged("The Wall", "Pink Floyd", "The Wall"));
    assert!(r.matches(&Query::new("floyd the wall")));
}

// ------------------------------------------------------------------ rows

#[test]
fn from_track_copies_tags_info_and_stats() {
    let t = track(42, |t| {
        t.tags.title = Some("Song".into());
        t.tags.artist = Some("Artist".into());
        t.tags.album = Some("Album".into());
        t.tags.album_artist = Some("AA".into());
        t.tags.composer = Some("Comp".into());
        t.tags.genre = Some("Jazz".into());
        t.tags.year = Some(1999);
        t.tags.disc_no = Some(2);
        t.tags.track_no = Some(5);
        t.info.duration_ms = Some(215_000);
        t.size = 4_321_000;
        t.added_at = 1_650_000_000;
    });
    let s = TrackStats {
        plays: 7,
        skips: 2,
        rating: Some(80),
        last_played: Some(1_690_000_000),
    };
    let r = Row::from_track(&t, Some(&s), Some(3));
    assert_eq!(r.position, Some(3));
    assert_eq!(r.track_id, Some(42));
    assert_eq!(
        (r.name.as_str(), r.artist.as_str(), r.album.as_str()),
        ("Song", "Artist", "Album")
    );
    assert_eq!(
        (
            r.album_artist.as_str(),
            r.composer.as_str(),
            r.genre.as_str()
        ),
        ("AA", "Comp", "Jazz")
    );
    assert_eq!((r.year, r.disc, r.track), (Some(1999), Some(2), Some(5)));
    assert_eq!((r.time_ms, r.size), (Some(215_000), Some(4_321_000)));
    assert_eq!((r.plays, r.skips, r.rating), (7, 2, Some(80)));
    assert_eq!(r.last_played, Some(1_690_000_000));
    assert_eq!(r.added, Some(1_650_000_000));
    assert_eq!(r.path, PathBuf::from("/music/file42.mp3"));
    assert!(!r.unresolved);
}

#[test]
fn from_track_without_stats_or_title() {
    let r = Row::from_track(&track(1, |_| {}), None, None);
    assert_eq!(r.name, "file1");
    assert_eq!(
        (r.plays, r.skips, r.rating, r.last_played),
        (0, 0, None, None)
    );
    assert_eq!(r.position, None);
    assert_eq!(r.artist, "");
}

#[test]
fn from_track_names_every_file_kind() {
    for (kind, want) in [
        (FileKind::Mp3, "MP3"),
        (FileKind::Aac, "AAC"),
        (FileKind::Alac, "ALAC"),
        (FileKind::Wav, "WAV"),
        (FileKind::Aiff, "AIFF"),
    ] {
        let r = from(&track(1, |t| t.info.kind = kind));
        assert_eq!(r.kind, want);
    }
}

#[test]
fn library_rows_skips_missing_tracks_keeps_order_and_applies_stats_by_id() {
    let tracks = vec![
        track(1, |t| t.tags.title = Some("one".into())),
        track(2, |t| {
            t.tags.title = Some("gone".into());
            t.missing_since = Some(1_700_000_001);
        }),
        track(3, |t| t.tags.title = Some("three".into())),
    ];
    let stats = HashMap::from([
        (
            3,
            TrackStats {
                plays: 9,
                ..Default::default()
            },
        ),
        (
            2,
            TrackStats {
                plays: 100,
                ..Default::default()
            },
        ),
    ]);
    let rows = library_rows(&tracks, &stats);
    let got: Vec<_> = rows
        .iter()
        .map(|r| (r.track_id, r.name.as_str(), r.plays, r.position))
        .collect();
    assert_eq!(
        got,
        [(Some(1), "one", 0, None), (Some(3), "three", 9, None)]
    );
}

#[test]
fn library_rows_of_nothing_is_empty() {
    assert!(library_rows(&[], &HashMap::new()).is_empty());
}

fn item(location: &str, t: Option<TrackRow>) -> Item {
    Item {
        entry: Entry {
            location: location.into(),
            extinf: None,
        },
        track: t,
    }
}

#[test]
fn playlist_rows_number_from_one_in_file_order_and_name_unresolved_by_stem() {
    let items = vec![
        item(
            "a.mp3",
            Some(track(5, |t| t.tags.title = Some("Five".into()))),
        ),
        item("../gone/Lost Track.flac", None),
        item(
            "a.mp3",
            Some(track(5, |t| t.tags.title = Some("Five".into()))),
        ),
        item("dotted.name.v2.ogg", None),
    ];
    let stats = HashMap::from([(
        5,
        TrackStats {
            plays: 4,
            ..Default::default()
        },
    )]);
    let rows = playlist_rows(&items, &stats);
    let got: Vec<_> = rows
        .iter()
        .map(|r| {
            (
                r.position,
                r.name.as_str(),
                r.unresolved,
                r.track_id,
                r.plays,
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (Some(1), "Five", false, Some(5), 4),
            (Some(2), "Lost Track", true, None, 0),
            (Some(3), "Five", false, Some(5), 4),
            (Some(4), "dotted.name.v2", true, None, 0),
        ]
    );
    assert_eq!(rows[1].path, PathBuf::from("../gone/Lost Track.flac"));
    assert_eq!(rows[1].kind, "");
    assert_eq!(rows[1].time_ms, None);
}

#[test]
fn playlist_rows_of_nothing_is_empty() {
    assert!(playlist_rows(&[], &HashMap::new()).is_empty());
}

#[test]
fn an_unresolved_entry_without_a_stem_keeps_its_location_as_name() {
    assert_eq!(Row::unresolved("/", 1).name, "/");
    assert_eq!(Row::unresolved("", 1).name, "");
}

// ------------------------------------------------------------------ text

#[test]
fn text_table() {
    let full = row(|r| {
        r.position = Some(3);
        r.name = "Song".into();
        r.time_ms = Some(215_000);
        r.artist = "Artist".into();
        r.album = "Album".into();
        r.album_artist = "AA".into();
        r.composer = "Comp".into();
        r.genre = "Jazz".into();
        r.year = Some(1999);
        r.disc = Some(2);
        r.track = Some(5);
        r.plays = 7;
        r.skips = 2;
        r.rating = Some(80);
        r.last_played = Some(1);
        r.added = Some(2);
        r.kind = "AAC";
        r.size = Some(4_321_000);
    });
    let want = [
        (Column::Position, "3"),
        (Column::Name, "Song"),
        (Column::Time, "3:35"),
        (Column::Artist, "Artist"),
        (Column::Album, "Album"),
        (Column::AlbumArtist, "AA"),
        (Column::Composer, "Comp"),
        (Column::Genre, "Jazz"),
        (Column::Year, "1999"),
        (Column::Disc, "2"),
        (Column::Track, "5"),
        (Column::Plays, "7"),
        (Column::Skips, "2"),
        (Column::Rating, "★★★★"),
        (Column::LastPlayed, ""),
        (Column::Added, ""),
        (Column::Kind, "AAC"),
        (Column::Size, "4.3 MB"),
    ];
    assert_eq!(want.len(), Column::ALL.len());
    for (col, text) in want {
        assert_eq!(col.text(&full), text, "{col:?}");
    }
}

#[test]
fn text_of_an_empty_row_is_empty_everywhere() {
    let r = blank();
    for col in Column::ALL {
        assert_eq!(col.text(&r), "", "{col:?}");
    }
}

#[test]
fn plays_and_skips_are_blank_at_zero_and_shown_from_one() {
    for (n, want) in [(0, ""), (1, "1"), (10, "10")] {
        let r = row(|r| {
            r.plays = n;
            r.skips = n;
        });
        assert_eq!(Column::Plays.text(&r), want);
        assert_eq!(Column::Skips.text(&r), want);
    }
}

#[test]
fn rating_text_in_stars() {
    let mk = |v: Option<u32>| row(|r| r.rating = v);
    assert_eq!(Column::Rating.text(&mk(None)), "");
    for (rating, n) in [
        (0, 0),
        (9, 0),
        (10, 1), // rounds half up
        (20, 1),
        (30, 2),
        (40, 2),
        (50, 3),
        (60, 3),
        (80, 4),
        (99, 5),
        (100, 5),
        (101, 5),
        (255, 5),
        (u32::MAX, 5),
    ] {
        assert_eq!(
            Column::Rating.text(&mk(Some(rating))),
            "★".repeat(n),
            "rating {rating}"
        );
        assert_eq!(stars(rating), "★".repeat(n), "rating {rating}");
    }
}

#[test]
fn duration_under_and_over_an_hour() {
    for (ms, want) in [
        (0, "0:00"),
        (999, "0:00"),
        (1_000, "0:01"),
        (59_999, "0:59"),
        (60_000, "1:00"),
        (215_000, "3:35"),
        (3_599_999, "59:59"),
        (3_600_000, "1:00:00"),
        (3_661_000, "1:01:01"),
        (36_000_000, "10:00:00"),
    ] {
        assert_eq!(duration(ms), want, "{ms} ms");
    }
    assert_eq!(
        Column::Time.text(&row(|r| r.time_ms = Some(3_661_000))),
        "1:01:01"
    );
}

#[test]
fn total_time_in_hours_and_minutes() {
    for (ms, want) in [
        (0, "0 min"),
        (59_999, "0 min"),
        (60_000, "1 min"),
        (3_540_000, "59 min"),
        (3_600_000, "1 h 0 min"),
        (3_660_000, "1 h 1 min"),
        (50_580_000, "14 h 3 min"),
    ] {
        assert_eq!(total_time(ms), want, "{ms} ms");
    }
}

#[test]
fn size_in_decimal_megabytes() {
    for (bytes, want) in [
        (0, "0.0 MB"),
        (1_500_000, "1.5 MB"),
        (1_234_567, "1.2 MB"),
        (123_456_789, "123.5 MB"),
    ] {
        assert_eq!(megabytes(bytes), want);
    }
    assert_eq!(
        Column::Size.text(&row(|r| r.size = Some(4_321_000))),
        "4.3 MB"
    );
    assert_eq!(Column::Size.text(&row(|r| r.size = Some(0))), "0.0 MB");
}

#[test]
fn only_the_date_columns_give_a_time() {
    let r = row(|r| {
        r.last_played = Some(11);
        r.added = Some(22);
    });
    for col in Column::ALL {
        let want = match col {
            Column::LastPlayed => Some(11),
            Column::Added => Some(22),
            _ => None,
        };
        assert_eq!(col.time(&r), want, "{col:?}");
    }
}

#[test]
fn columns_have_unique_titles_and_sane_metadata() {
    let mut titles: Vec<_> = Column::ALL.iter().map(|c| c.title()).collect();
    assert!(titles.iter().all(|t| !t.is_empty()));
    titles.sort_unstable();
    titles.dedup();
    assert_eq!(titles.len(), Column::ALL.len());
    assert!(Column::ALL.iter().all(|c| c.width() > 0));
    assert!(Column::Name.visible_by_default());
    assert!(!Column::Composer.visible_by_default());
    assert!(Column::Year.numeric() && Column::Size.numeric());
    assert!(!Column::Name.numeric() && !Column::Rating.numeric());
}

// --------------------------------------------------------------- compare

/// How to put a "low" value, a "high" value or an empty one in a column.
#[derive(Clone, Copy)]
enum Val {
    Low,
    High,
    Empty,
}

fn put(col: Column, r: &mut Row, v: Val) {
    let (lo, hi) = (9u32, 10u32);
    let num = |v: Val| match v {
        Val::Low => Some(lo),
        Val::High => Some(hi),
        Val::Empty => None,
    };
    let s = |v: Val| match v {
        Val::Low => "alpha",
        Val::High => "Beta",
        Val::Empty => "",
    };
    match col {
        Column::Position => r.position = num(v).map(|n| n as usize),
        Column::Name => r.name = s(v).into(),
        Column::Time => r.time_ms = num(v).map(|n| n * 1000),
        Column::Artist => r.artist = s(v).into(),
        Column::Album => r.album = s(v).into(),
        Column::AlbumArtist => r.album_artist = s(v).into(),
        Column::Composer => r.composer = s(v).into(),
        Column::Genre => r.genre = s(v).into(),
        Column::Year => r.year = num(v),
        Column::Disc => r.disc = num(v),
        Column::Track => r.track = num(v),
        Column::Plays => r.plays = num(v).unwrap_or(0),
        Column::Skips => r.skips = num(v).unwrap_or(0),
        Column::Rating => r.rating = num(v),
        Column::LastPlayed => r.last_played = num(v).map(i64::from),
        Column::Added => r.added = num(v).map(i64::from),
        Column::Kind => {
            r.kind = match v {
                Val::Low => "AAC",
                Val::High => "mp3",
                Val::Empty => "",
            }
        }
        Column::Size => r.size = num(v).map(u64::from),
    }
}

/// Plays and skips are plain counts: zero is a value, not an absence.
fn has_empty(col: Column) -> bool {
    !matches!(col, Column::Plays | Column::Skips)
}

fn pair(col: Column, a: Val, b: Val) -> (Row, Row) {
    (row(|r| put(col, r, a)), row(|r| put(col, r, b)))
}

#[test]
fn compare_orders_low_before_high_in_every_column() {
    for col in Column::ALL {
        let (lo, hi) = pair(col, Val::Low, Val::High);
        assert_eq!(col.compare(&lo, &hi), Ordering::Less, "{col:?}");
        assert_eq!(col.compare(&hi, &lo), Ordering::Greater, "{col:?}");
        assert_eq!(col.compare(&lo, &lo.clone()), Ordering::Equal, "{col:?}");
    }
}

#[test]
fn compare_puts_empty_values_last_in_every_column() {
    for col in Column::ALL.into_iter().filter(|c| has_empty(*c)) {
        let (val, empty) = pair(col, Val::High, Val::Empty);
        assert_eq!(col.compare(&val, &empty), Ordering::Less, "{col:?}");
        assert_eq!(col.compare(&empty, &val), Ordering::Greater, "{col:?}");
        let (e1, e2) = pair(col, Val::Empty, Val::Empty);
        assert_eq!(col.compare(&e1, &e2), Ordering::Equal, "{col:?}");
    }
}

#[test]
fn compare_text_ignores_case_accents_and_the_leading_the() {
    let artists = ["The Beatles", "Abba", "beck", "Émile", "Cher", "ZZ Top"];
    let mut rows: Vec<Row> = artists
        .iter()
        .map(|a| row(|r| r.artist = (*a).into()))
        .collect();
    rows.sort_by(|a, b| Column::Artist.compare(a, b));
    let got: Vec<_> = rows.iter().map(|r| r.artist.as_str()).collect();
    assert_eq!(
        got,
        ["Abba", "The Beatles", "beck", "Cher", "Émile", "ZZ Top"]
    );
}

#[test]
fn compare_text_applies_the_to_every_text_column() {
    for col in [Column::Name, Column::Album, Column::Genre, Column::Composer] {
        let (the_b, a) = (
            row(|r| put_text(col, r, "The Bee")),
            row(|r| put_text(col, r, "Ant")),
        );
        let c = row(|r| put_text(col, r, "Cat"));
        assert_eq!(col.compare(&a, &the_b), Ordering::Less, "{col:?}");
        assert_eq!(col.compare(&the_b, &c), Ordering::Less, "{col:?}");
    }
}

fn put_text(col: Column, r: &mut Row, v: &str) {
    match col {
        Column::Name => r.name = v.into(),
        Column::Album => r.album = v.into(),
        Column::Genre => r.genre = v.into(),
        Column::Composer => r.composer = v.into(),
        _ => unreachable!(),
    }
}

#[test]
fn compare_numbers_as_numbers_not_as_text() {
    for (col, set) in [
        (
            Column::Year,
            (|r: &mut Row, n: u32| r.year = Some(n)) as fn(&mut Row, u32),
        ),
        (Column::Disc, |r, n| r.disc = Some(n)),
        (Column::Track, |r, n| r.track = Some(n)),
        (Column::Plays, |r, n| r.plays = n),
        (Column::Skips, |r, n| r.skips = n),
        (Column::Rating, |r, n| r.rating = Some(n)),
        (Column::Time, |r, n| r.time_ms = Some(n)),
        (Column::Size, |r, n| r.size = Some(u64::from(n))),
        (Column::Position, |r, n| r.position = Some(n as usize)),
        (Column::LastPlayed, |r, n| {
            r.last_played = Some(i64::from(n))
        }),
        (Column::Added, |r, n| r.added = Some(i64::from(n))),
    ] {
        for (small, big) in [(9, 10), (2, 10), (99, 100), (999, 2000)] {
            let (a, b) = (row(|r| set(r, small)), row(|r| set(r, big)));
            assert_eq!(
                col.compare(&a, &b),
                Ordering::Less,
                "{col:?} {small} < {big}"
            );
            assert_eq!(
                col.compare(&b, &a),
                Ordering::Greater,
                "{col:?} {big} > {small}"
            );
        }
    }
}

#[test]
fn compare_zero_plays_sort_before_one_play() {
    let (zero, one) = (row(|r| r.plays = 0), row(|r| r.plays = 1));
    assert_eq!(Column::Plays.compare(&zero, &one), Ordering::Less);
}

/// Two rows tied on the sort column, each level of the tie break in turn.
/// `a` wins at the tested level and loses at every level below it, so the
/// order only comes out right when the levels are tried in sequence.
#[test]
fn compare_ties_fall_back_to_artist_album_disc_track_name_position_then_path() {
    type Set = fn(&mut Row, bool);
    // `low` true gives the value that sorts first.
    let levels: [(&str, Set); 7] = [
        ("artist", |r, low| {
            r.artist = if low { "A" } else { "B" }.into()
        }),
        ("album", |r, low| {
            r.album = if low { "A" } else { "B" }.into()
        }),
        ("disc", |r, low| r.disc = Some(if low { 1 } else { 2 })),
        ("track", |r, low| r.track = Some(if low { 1 } else { 2 })),
        ("name", |r, low| r.name = if low { "A" } else { "B" }.into()),
        ("position", |r, low| {
            r.position = Some(if low { 1 } else { 2 })
        }),
        ("path", |r, low| {
            r.path = PathBuf::from(if low { "/a" } else { "/b" })
        }),
    ];
    // Sorting by Genre (equal in both rows) and by Kind (empty in both).
    for col in [Column::Genre, Column::Kind] {
        for (i, (what, set)) in levels.iter().enumerate() {
            let mut a = blank();
            let mut b = blank();
            for (j, (_, set_j)) in levels.iter().enumerate() {
                match j.cmp(&i) {
                    Ordering::Less => {
                        set_j(&mut a, true);
                        set_j(&mut b, true);
                    }
                    Ordering::Equal => {
                        set(&mut a, true);
                        set(&mut b, false);
                    }
                    Ordering::Greater => {
                        set_j(&mut a, false);
                        set_j(&mut b, true);
                    }
                }
            }
            a.fold();
            b.fold();
            assert_eq!(col.compare(&a, &b), Ordering::Less, "{col:?} by {what}");
            assert_eq!(col.compare(&b, &a), Ordering::Greater, "{col:?} by {what}");
        }
    }
}

#[test]
fn compare_primary_column_beats_the_tie_break() {
    let a = row(|r| {
        r.genre = "Blues".into();
        r.artist = "Zed".into();
    });
    let b = row(|r| {
        r.genre = "Rock".into();
        r.artist = "Abe".into();
    });
    assert_eq!(Column::Genre.compare(&a, &b), Ordering::Less);
}

#[test]
fn compare_tie_break_puts_a_missing_disc_or_position_after_a_present_one() {
    let with = row(|r| r.disc = Some(3));
    let without = row(|r| r.disc = None);
    assert_eq!(Column::Genre.compare(&with, &without), Ordering::Less);
    let pos = row(|r| r.position = Some(1));
    let nopos = blank();
    assert_eq!(Column::Genre.compare(&pos, &nopos), Ordering::Less);
}

#[test]
fn sorting_by_album_gives_an_album_in_disc_and_track_order() {
    let mk = |album: &str, disc, track, name: &str| {
        row(|r| {
            r.artist = "Band".into();
            r.album = album.into();
            r.disc = disc;
            r.track = track;
            r.name = name.into();
        })
    };
    let mut rows = [
        mk("Live", Some(1), Some(10), "ten"),
        mk("Live", Some(2), Some(1), "disc two"),
        mk("Live", Some(1), Some(9), "nine"),
        mk("Live", None, None, "loose"),
        mk("Studio", Some(1), Some(1), "first"),
    ];
    rows.sort_by(|a, b| Column::Album.compare(a, b));
    let got: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(got, ["nine", "ten", "disc two", "loose", "first"]);
}

// --------------------------------------------------------------- summary

#[test]
fn summary_totals_and_plural() {
    let mk = |ms: Option<u32>, size: Option<u64>| {
        row(|r| {
            r.time_ms = ms;
            r.size = size;
        })
    };
    let none: Vec<Row> = vec![];
    assert_eq!(summary(none.iter()), "0 songs, 0 min, 0 B");

    let one = [mk(Some(180_000), Some(4_500_000))];
    assert_eq!(summary(one.iter()), "1 song, 3 min, 4.5 MB");

    let two = [
        mk(Some(90_000), Some(1_500_000)),
        mk(Some(30_000), Some(700_000)),
    ];
    assert_eq!(summary(two.iter()), "2 songs, 2 min, 2.2 MB");

    // Rows without a duration or size count as songs and add nothing.
    let gaps = [
        mk(None, None),
        mk(Some(3_600_000), Some(2_000_000_000)),
        mk(None, None),
    ];
    assert_eq!(summary(gaps.iter()), "3 songs, 1 h 0 min, 2.0 GB");
}

#[test]
fn summary_adds_durations_before_rounding() {
    // Two 40 s tracks make 1 min 20 s; rounding each first would give 0 min.
    let rows = [
        row(|r| r.time_ms = Some(40_000)),
        row(|r| r.time_ms = Some(40_000)),
    ];
    assert!(summary(rows.iter()).starts_with("2 songs, 1 min,"));
}

#[test]
fn summary_does_not_overflow_a_u32_of_milliseconds() {
    // 3 x 2^31 ms is more than u32::MAX; the sum must be done in u64.
    let big = 2_147_483_648u32;
    let rows = [
        row(|r| r.time_ms = Some(big)),
        row(|r| r.time_ms = Some(big)),
        row(|r| r.time_ms = Some(big)),
    ];
    assert_eq!(summary(rows.iter()), "3 songs, 1789 h 34 min, 0 B");
}

// ------------------------------------------- folded keys vs folding live

/// The semantics before the keys were precomputed: fold on every call.
fn old_text_cmp(a: &str, b: &str) -> Ordering {
    let (x, y) = (normalize(Some(a)), normalize(Some(b)));
    match (x.is_empty(), y.is_empty()) {
        (false, true) => Ordering::Less,
        (true, false) => Ordering::Greater,
        _ => x.cmp(&y),
    }
}

/// The search rule spelled out independently: each field folded as for
/// sorting, but with a leading "The" kept, so "floyd the wall" finds it.
fn old_matches(r: &Row, query: &str) -> bool {
    let keep_the = |s: &str| {
        let folded = normalize(Some(s));
        let first = s
            .split(|c: char| !c.is_alphanumeric())
            .find(|w| !w.is_empty())
            .map(str::to_lowercase);
        match first.as_deref() {
            Some("the") if folded != "the" => format!("the {folded}"),
            _ => folded,
        }
    };
    let hay = [&r.name, &r.artist, &r.album, &r.album_artist, &r.composer]
        .iter()
        .map(|s| keep_the(s))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    normalize(Some(query))
        .split(' ')
        .filter(|w| !w.is_empty())
        .all(|w| hay.contains(w))
}

const SAMPLES: [&str; 14] = [
    "",
    "The Beatles",
    "the beatles",
    "THE BEATLES",
    "Beatles",
    "The",
    "Théo",
    "Theo",
    "Éric Clapton",
    "eric clapton",
    "Zoé",
    "(Intro)",
    "Sigur Rós",
    "Cafe\u{301}",
];

#[test]
fn folded_sort_keys_order_text_columns_as_folding_live_would() {
    let text_cols = [
        (
            Column::Name,
            (|r: &mut Row, s: &str| r.name = s.into()) as fn(&mut Row, &str),
        ),
        (Column::Artist, |r, s| r.artist = s.into()),
        (Column::Album, |r, s| r.album = s.into()),
        (Column::AlbumArtist, |r, s| r.album_artist = s.into()),
        (Column::Composer, |r, s| r.composer = s.into()),
        (Column::Genre, |r, s| r.genre = s.into()),
    ];
    for (col, set) in text_cols {
        for a in SAMPLES {
            for b in SAMPLES {
                let (ra, rb) = (row(|r| set(r, a)), row(|r| set(r, b)));
                assert_eq!(
                    col.compare(&ra, &rb),
                    old_text_cmp(a, b),
                    "{col:?}: {a:?} vs {b:?}"
                );
            }
        }
    }
}

#[test]
fn folded_tie_break_keys_agree_with_folding_live() {
    // Sorting by Year with equal years: the order is the artist, then album.
    for a in SAMPLES {
        for b in SAMPLES {
            let ra = row(|r| r.artist = a.into());
            let rb = row(|r| r.artist = b.into());
            assert_eq!(
                Column::Year.compare(&ra, &rb),
                old_text_cmp(a, b),
                "{a:?} {b:?}"
            );
        }
    }
}

#[test]
fn query_and_folded_search_text_agree_with_folding_live() {
    let rows = [
        from(&tagged("The Wall", "Pink Floyd", "The Wall")),
        from(&tagged("Café del Mar", "Énergie", "Ibiza")),
        from(&tagged("Cafe\u{301}", "THE BEATLES", "Abbey Road")),
        from(&tagged("(Intro)", "Sigur Rós", "Ágætis byrjun")),
        from(&track(3, |_| {})),
    ];
    let queries = [
        "",
        " ",
        "the",
        "The Wall",
        "wall the",
        "cafe",
        "CAFÉ",
        "energie",
        "énergie",
        "beatles",
        "the beatles",
        "sigur ros",
        "ROS sigur",
        "intro",
        "(intro)",
        "agaetis",
        "zzz",
        "file3",
        "floyd wall",
    ];
    for r in &rows {
        for q in queries {
            assert_eq!(
                r.matches(&Query::new(q)),
                old_matches(r, q),
                "{:?} / {q:?}",
                r.name
            );
        }
    }
}

#[test]
fn a_query_is_folded_once_and_empty_ones_match_everything() {
    assert_eq!(Query::new("  "), Query::default());
    assert_eq!(Query::new("CAFÉ  Del"), Query::new("cafe del"));
    assert!(blank().matches(&Query::default()));
    assert!(!blank().matches(&Query::new("a")));
}
