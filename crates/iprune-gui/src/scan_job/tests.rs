//! `scan` against a library in a temp dir. The index, the music folder and
//! the data folder are all temp paths, so nothing real is read or written.
//! The fixture helpers are the ones the core crate's integration tests use.

#[allow(clippy::duplicate_mod)] // the fixture helpers are shared with sync_job and adopt_job
#[path = "../../../iprune-core/tests/common/mod.rs"]
mod common;

use std::fs;

use common::{Env, Kind};
use iprune_core::config::Paths;
use iprune_core::store;

use super::*;

/// An `Env` whose index is the one `scan` opens through `paths`.
fn world() -> (Env, Paths) {
    let env = Env::new();
    let base = env.music.parent().unwrap().to_path_buf();
    let paths = Paths {
        config_file: base.join("config.toml"),
        data_dir: base.join("db"),
    };
    assert_eq!(paths.database(), base.join("db/iprune.sqlite"));
    (env, paths)
}

#[test]
fn scan_indexes_the_library_and_returns_the_cli_report() {
    let (env, paths) = world();
    env.add("a.mp3", Kind::Mp3, 1);
    env.add("b.m4a", Kind::Aac, 2);
    fs::write(env.music.join("bad.mp3"), b"nope").unwrap();

    let text = scan(&env.cfg, &paths).unwrap();

    assert!(
        text.starts_with(
            "library: 0 unchanged, 2 added, 0 updated, 0 back from missing, 0 moved, 0 missing\n"
        ),
        "{text}"
    );
    assert!(text.contains("unreadable: 1\n  "), "{text}");
    assert!(text.contains("bad.mp3"), "{text}");
    // The index really holds the two tracks, read back through another
    // connection (the worker's own is closed).
    let mut found: Vec<String> = env
        .tracks()
        .iter()
        .map(|t| t.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    found.sort();
    assert_eq!(found, ["a.mp3", "b.m4a"]);
}

#[test]
fn a_second_scan_reports_everything_unchanged() {
    let (env, paths) = world();
    env.add("a.mp3", Kind::Mp3, 1);
    scan(&env.cfg, &paths).unwrap();
    let text = scan(&env.cfg, &paths).unwrap();
    assert!(
        text.starts_with(
            "library: 1 unchanged, 0 added, 0 updated, 0 back from missing, 0 moved, 0 missing\n"
        ),
        "{text}"
    );
}

#[test]
fn a_file_removed_between_scans_is_reported_missing_and_marked() {
    let (env, paths) = world();
    env.add("keep.mp3", Kind::Mp3, 1);
    let gone = env.add("gone.mp3", Kind::Mp3, 2);
    scan(&env.cfg, &paths).unwrap();
    fs::remove_file(&gone).unwrap();

    let text = scan(&env.cfg, &paths).unwrap();

    assert!(text.contains("1 unchanged"), "{text}");
    assert!(text.contains("1 missing\nmissing: 1\n  "), "{text}");
    assert!(text.contains("gone.mp3"), "{text}");
    assert!(env.track(&gone).missing_since.is_some());
}

// Prevents: the window's Scan marking the whole library missing when its disk
// is not mounted. The error must say so, and the index must stay as it was.
#[test]
fn a_missing_library_root_is_an_error_and_marks_nothing_missing() {
    let (env, paths) = world();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    scan(&env.cfg, &paths).unwrap();
    let away = env.music.with_file_name("music-away");
    fs::rename(&env.music, &away).unwrap();

    let e = scan(&env.cfg, &paths).unwrap_err();

    let chain = format!("{e:#}");
    assert!(chain.contains("scanning the library"), "{chain}");
    assert!(chain.contains("mounted"), "{chain}");
    let rows = store::all_tracks(Store::open(&paths.database()).unwrap().conn()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, a);
    assert!(rows[0].missing_since.is_none(), "marked missing");

    // Control: with the folder back, the same call succeeds.
    fs::rename(&away, &env.music).unwrap();
    let text = scan(&env.cfg, &paths).unwrap();
    assert!(text.contains("1 unchanged"), "{text}");
}

#[test]
fn an_emptied_library_is_an_error_and_keeps_the_tracks() {
    let (env, paths) = world();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    scan(&env.cfg, &paths).unwrap();
    fs::remove_file(&a).unwrap();

    let e = scan(&env.cfg, &paths).unwrap_err();

    assert!(format!("{e:#}").contains("refusing"), "{e:#}");
    assert!(env.track(&a).missing_since.is_none(), "marked missing");
}
