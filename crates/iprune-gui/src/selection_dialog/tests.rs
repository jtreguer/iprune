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
