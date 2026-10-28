use std::path::PathBuf;

use iprune_core::tags::{AudioInfo, FileKind, Tags};

use super::*;

#[test]
fn size_text_compares_with_the_ipod() {
    assert_eq!(size_text(3, 2_000_000_000, None), "3 tracks, 2.0 GB");
    assert_eq!(
        size_text(3, 2_000_000_000, Some(80_000_000_000)),
        "3 tracks, 2.0 GB, of the 80.0 GB the iPod holds"
    );
    assert_eq!(
        size_text(9, 82_200_000_000, Some(80_000_000_000)),
        "9 tracks, 82.2 GB: 2.2 GB more than the iPod holds"
    );
}

fn track(id: i64) -> TrackRow {
    TrackRow {
        id,
        path: PathBuf::from(format!("/music/file{id}.mp3")),
        path_key: format!("/music/file{id}.mp3"),
        size: 1_000_000_000,
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
    }
}

fn playlist(id: i64, name: &str, members: &[i64]) -> LibPlaylist {
    LibPlaylist {
        id,
        name: name.into(),
        members: members.iter().map(|m| Some(*m)).collect(),
    }
}

fn scope(kind: Scope, names: &[&str]) -> DeviceScope {
    DeviceScope {
        kind,
        playlists: names.iter().map(|n| (*n).to_owned()).collect(),
    }
}

// Save is offered only for what a sync would accept; the line says why not.
#[test]
fn status_refuses_what_a_sync_would_refuse() {
    let library = [track(1), track(2), track(3)];
    let lists = [
        playlist(1, "A", &[1]),
        playlist(2, "B", &[2, 3]),
        playlist(3, "Empty", &[]),
    ];
    let cap = Some(10_000_000_000);
    let cases: [(DeviceScope, &str, bool); 6] = [
        (
            scope(Scope::All, &[]),
            "3 tracks, 3.0 GB, of the 10.0 GB the iPod holds",
            true,
        ),
        (
            scope(Scope::Playlists, &["B"]),
            "2 tracks, 2.0 GB, of the 10.0 GB the iPod holds",
            true,
        ),
        (
            scope(Scope::Except, &["B"]),
            "1 track, 1.0 GB, of the 10.0 GB the iPod holds",
            true,
        ),
        (
            scope(Scope::Playlists, &[]),
            "Cannot be saved: this iPod's selection keeps only some playlists but names none",
            false,
        ),
        (
            scope(Scope::Playlists, &["Empty"]),
            "Cannot be saved: the playlists this iPod keeps hold no library track",
            false,
        ),
        (
            scope(Scope::Except, &["A", "B"]),
            "Cannot be saved: this iPod's selection leaves out every library track",
            false,
        ),
    ];
    for (s, text, can_save) in cases {
        assert_eq!(
            status(&s, &library, &lists, cap),
            (text.to_owned(), can_save),
            "{s:?}"
        );
    }
}

#[test]
fn gone_lists_the_playlists_no_longer_in_the_library() {
    let lists = [playlist(1, "A", &[1])];
    assert_eq!(
        gone(&scope(Scope::Except, &["A", "Old", "Older"]), &lists),
        ["Old", "Older"]
    );
    assert!(gone(&scope(Scope::Playlists, &["A"]), &lists).is_empty());
}

fn sized(id: i64, size: u64) -> TrackRow {
    TrackRow { size, ..track(id) }
}

fn bytes_of(library: &[TrackRow], lists: &[LibPlaylist]) -> Vec<(String, u64)> {
    rows(library, lists)
        .into_iter()
        .map(|r| (r.name, r.bytes))
        .collect()
}

// A playlist weighs its distinct present tracks: a repeated track once, a
// missing track, an unresolved entry and an unknown id not at all.
#[test]
fn rows_weigh_the_distinct_present_tracks() {
    let mut gone = sized(3, 400);
    gone.missing_since = Some(1_700_000_100);
    let library = [sized(1, 10), sized(2, 200), gone, sized(4, 5_000)];
    let mut unresolved = playlist(5, "Unresolved", &[1]);
    unresolved.members.push(None);
    unresolved.members.push(Some(2));
    let lists = [
        playlist(1, "Plain", &[1, 2]),
        playlist(2, "Repeats", &[2, 2, 1, 2]),
        playlist(3, "Missing", &[1, 3]),
        playlist(4, "Unknown", &[1, 99]),
        unresolved,
        playlist(6, "Empty", &[]),
        playlist(7, "Only missing", &[3]),
    ];
    assert_eq!(
        bytes_of(&library, &lists),
        [
            ("Plain".to_owned(), 210),
            ("Repeats".to_owned(), 210),
            ("Missing".to_owned(), 10),
            ("Unknown".to_owned(), 10),
            ("Unresolved".to_owned(), 210),
            ("Empty".to_owned(), 0),
            ("Only missing".to_owned(), 0),
        ]
    );
}

#[test]
fn rows_keep_the_given_order_and_handle_no_playlists() {
    let library = [sized(1, 10)];
    let lists = [playlist(2, "Zed", &[1]), playlist(1, "Abe", &[1])];
    let names: Vec<_> = rows(&library, &lists).into_iter().map(|r| r.name).collect();
    assert_eq!(names, ["Zed", "Abe"]);
    assert!(rows(&library, &[]).is_empty());
}

// The row size must be what a selection of that playlist alone sends.
#[test]
fn rows_agree_with_the_size_of_a_selection_of_one_playlist() {
    let mut gone = sized(3, 400);
    gone.missing_since = Some(1_700_000_100);
    let library = [sized(1, 10), sized(2, 200), gone, sized(4, 5_000)];
    let lists = [
        playlist(1, "A", &[1, 2, 2]),
        playlist(2, "B", &[2, 3, 4, 99]),
        playlist(3, "C", &[3]),
        playlist(4, "D", &[1, 2, 4]),
    ];
    for (row, list) in rows(&library, &lists).iter().zip(&lists) {
        let (_, bytes) = scope(Scope::Playlists, &[&list.name]).size(&library, &lists);
        assert_eq!(row.bytes, bytes, "{}", list.name);
    }
}

fn names_sorted(names: &[&str], cmp: fn(&PlaylistRow, &PlaylistRow) -> Ordering) -> Vec<String> {
    let lists: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, n)| playlist(i as i64, n, &[]))
        .collect();
    let mut r = rows(&[], &lists);
    r.sort_by(cmp);
    r.into_iter().map(|r| r.name).collect()
}

// Case, Latin accents and a leading "The" do not decide the order; the raw
// name only breaks a tie.
#[test]
fn by_name_folds_case_accents_and_the() {
    assert_eq!(
        names_sorted(
            &["Zebra", "étude", "The Beatles", "apple", "Banana", "Eclair"],
            PlaylistRow::by_name
        ),
        ["apple", "Banana", "The Beatles", "Eclair", "étude", "Zebra"]
    );
    // Equal once folded: the raw names decide, whatever the input order.
    for input in [["abc", "ABC", "The abc"], ["The abc", "abc", "ABC"]] {
        assert_eq!(
            names_sorted(&input, PlaylistRow::by_name),
            ["ABC", "The abc", "abc"],
            "{input:?}"
        );
    }
    // A name that is only "The" keeps it.
    assert_eq!(
        names_sorted(&["The", "Tha", "Thf"], PlaylistRow::by_name),
        ["Tha", "The", "Thf"]
    );
}

#[test]
fn by_size_orders_by_bytes_then_name() {
    let library = [sized(1, 100), sized(2, 100), sized(3, 50), sized(4, 900)];
    let lists = [
        playlist(1, "b", &[1]),
        playlist(2, "Big", &[4]),
        playlist(3, "A", &[2]),
        playlist(4, "Small", &[3]),
        playlist(5, "Empty", &[]),
        playlist(6, "Both", &[1, 2]),
    ];
    let mut r = rows(&library, &lists);
    r.sort_by(PlaylistRow::by_size);
    let order: Vec<_> = r.iter().map(|r| (r.name.as_str(), r.bytes)).collect();
    assert_eq!(
        order,
        [
            ("Empty", 0),
            ("Small", 50),
            ("A", 100),
            ("b", 100),
            ("Both", 200),
            ("Big", 900),
        ]
    );
    // Descending is the exact reverse, as the column view applies it.
    assert_eq!(r[0].by_size(&r[1]), Ordering::Less);
    assert_eq!(r[1].by_size(&r[0]), Ordering::Greater);
    assert_eq!(r[2].by_size(&r[2].clone()), Ordering::Equal);
}

#[test]
fn size_label_switches_to_gb_at_a_billion_bytes() {
    // Never "1000.0 MB": what would round to it is shown as 1.0 GB.
    let cases: [(u64, &str); 9] = [
        (0, "0.0 MB"),
        (400_000, "0.4 MB"),
        (1_000_000, "1.0 MB"),
        (123_456_789, "123.5 MB"),
        (999_949_999, "999.9 MB"),
        (999_950_000, "1.0 GB"),
        (999_999_999, "1.0 GB"),
        (1_000_000_000, "1.0 GB"),
        (82_200_000_000, "82.2 GB"),
    ];
    for (bytes, text) in cases {
        assert_eq!(size_label(bytes), text, "{bytes}");
    }
}
