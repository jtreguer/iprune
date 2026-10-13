//! `ipod::file_path`: turning an iPod database path into a file under the
//! mountpoint, and refusing anything that could leave it.

use std::path::{Component, Path, PathBuf};

use iprune_core::ipod::file_path;

const MP: &str = "/mnt/fake-ipod";

#[test]
fn a_normal_ipod_path_goes_under_the_mountpoint() {
    assert_eq!(
        file_path(Path::new(MP), ":iPod_Control:Music:F12:ABCD.mp3"),
        Some(PathBuf::from(
            "/mnt/fake-ipod/iPod_Control/Music/F12/ABCD.mp3"
        ))
    );
}

#[test]
fn the_spelling_is_kept() {
    assert_eq!(
        file_path(Path::new(MP), ":iPod_Control:Music:f12:AbCd.MP3"),
        Some(PathBuf::from(
            "/mnt/fake-ipod/iPod_Control/Music/f12/AbCd.MP3"
        ))
    );
}

#[test]
fn a_path_without_the_leading_colon_is_refused() {
    for p in [
        "iPod_Control:Music:F12:ABCD.mp3",
        "iPod_Control/Music/F12/ABCD.mp3",
        "/iPod_Control/Music/F12/ABCD.mp3",
        "/etc/passwd",
    ] {
        assert_eq!(file_path(Path::new(MP), p), None, "{p}");
    }
}

#[test]
fn dot_dot_and_dot_segments_are_refused() {
    for p in [
        ":..:etc:passwd",
        ":iPod_Control:..:..:etc:passwd",
        ":iPod_Control:Music:..",
        ":.:iPod_Control:Music:F00:A.mp3",
        ":.",
        ":..",
        // A slash inside a segment is a path separator for the filesystem.
        ":iPod_Control:../../etc/passwd",
        ":iPod_Control:Music/../../x",
    ] {
        assert_eq!(file_path(Path::new(MP), p), None, "{p}");
    }
}

#[test]
fn an_absolute_segment_cannot_replace_the_mountpoint() {
    for p in [":/etc/passwd", ":iPod_Control:/etc/passwd", "://etc"] {
        assert_eq!(file_path(Path::new(MP), p), None, "{p}");
    }
}

#[test]
fn empty_paths_are_refused() {
    for p in ["", ":", "::", ":::"] {
        assert_eq!(file_path(Path::new(MP), p), None, "{p:?}");
    }
}

#[test]
fn empty_and_inner_dot_segments_never_leave_the_mountpoint() {
    for p in [
        ":iPod_Control::Music:A.mp3",
        "::iPod_Control:A.mp3",
        ":a:b:",
        ":iPod_Control:.:Music:A.mp3",
    ] {
        // Refusing or tidying the empty segment are both acceptable.
        if let Some(f) = file_path(Path::new(MP), p) {
            assert!(f.starts_with(MP), "{p}: {}", f.display());
            assert!(
                f.components()
                    .all(|c| matches!(c, Component::Normal(_) | Component::RootDir)),
                "{p}: {}",
                f.display()
            );
        }
    }
}

#[test]
fn double_colon_and_dot_segments_are_accepted_inside_the_mountpoint() {
    assert_eq!(
        file_path(Path::new(MP), ":a::b"),
        Some(PathBuf::from("/mnt/fake-ipod/a/b"))
    );
    assert_eq!(
        file_path(Path::new(MP), ":a:.:b"),
        Some(PathBuf::from("/mnt/fake-ipod/a/b"))
    );
}
