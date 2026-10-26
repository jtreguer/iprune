//! `write` against a library in a temp dir: the index, the music folder
//! and the data folder are all temp paths.

#[allow(clippy::duplicate_mod)] // the fixture helpers are shared with the other jobs
#[path = "../../../iprune-core/tests/common/mod.rs"]
mod common;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use common::{Env, Kind};
use iprune_core::config::Paths;
use iprune_core::tag_edit::{FileResult, Outcome};

use super::*;

fn world() -> (Env, Paths) {
    let env = Env::new();
    let base = env.music.parent().unwrap().to_path_buf();
    let paths = Paths {
        config_file: base.join("config.toml"),
        data_dir: base.join("db"),
    };
    (env, paths)
}

#[test]
fn write_edits_the_file_and_the_index_through_its_own_connection() {
    let (mut env, paths) = world();
    let file = env.add("a/01.mp3", Kind::Mp3, 1);
    env.scan();
    let id = env.track(&file).id;
    let changes = Changes {
        artist: Some(Some("a-ha".into())),
        ..Changes::default()
    };
    let seen = std::cell::RefCell::new(Vec::new());
    let report = write(
        &paths,
        &[id],
        &changes,
        &|done, total| seen.borrow_mut().push((done, total)),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(report.written(), 1);
    assert_eq!(*seen.borrow(), [(0, 1)]);
    assert_eq!(env.track(&file).tags.artist.as_deref(), Some("a-ha"));
}

#[test]
fn describe_lists_refusals_and_what_a_stop_left() {
    let report = Report {
        results: vec![
            FileResult {
                track_id: 1,
                path: PathBuf::from("/m/a.mp3"),
                outcome: Outcome::Written,
            },
            FileResult {
                track_id: 2,
                path: PathBuf::from("/m/b.m4a"),
                outcome: Outcome::Refused("FairPlay protected".into()),
            },
        ],
        stopped: true,
        index_error: None,
    };
    assert_eq!(
        describe(&report, 5),
        "1 file written.\nNot written, FairPlay protected: /m/b.m4a\nStopped: 3 files were not edited."
    );
}

#[test]
fn describe_says_when_the_index_was_not_updated() {
    let report = Report {
        results: vec![FileResult {
            track_id: 1,
            path: PathBuf::from("/m/a.mp3"),
            outcome: Outcome::Written,
        }],
        stopped: true,
        index_error: Some("disk I/O error".into()),
    };
    assert_eq!(
        describe(&report, 3),
        "1 file written.\nThe index could not be updated: disk I/O error\nStopped: 2 files were not edited."
    );
}
