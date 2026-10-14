//! `adoption::choose`: the user's decision per proposal replaces the run's
//! mapping, the proposals left and the playlist matching. The iPod is a fake
//! tree in a temp dir written by libgpod; nothing real is touched.

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use common::{Env, IpodSpec, Kind, fake_ipod, ipod_playlist};
use iprune_core::adoption::{self, Run};
use iprune_core::{Error, playlists};

const D_EXACT: u64 = 1;
const D_WEAK: u64 = 2;

struct World {
    env: Env,
    a: PathBuf,
    b: PathBuf,
    _ipod_dir: tempfile::TempDir,
}

impl World {
    fn id(&self, p: &Path) -> i64 {
        self.env.track(p).id
    }

    /// Library a and b (same size and duration). The iPod holds a exactly as
    /// dbid 1, and b under other tags as dbid 2, which is a proposal for b
    /// (a is the other candidate). iPod playlist "Mix" is [1, 2]; the file
    /// "Mix" is [a, b].
    fn new() -> World {
        let mut env = Env::new();
        let a = env.add("a.mp3", Kind::Mp3, 1);
        let b = env.add("b.mp3", Kind::Mp3, 2);
        env.scan();
        let ipod_dir = tempfile::tempdir().unwrap();
        let spec = |p: &PathBuf, dbid, title, artist| {
            let t = env.track(p);
            IpodSpec {
                dbid,
                size: fs::metadata(&t.path).unwrap().len(),
                ms: t.info.duration_ms.unwrap(),
                title,
                artist,
                album: "",
                plays: 0,
            }
        };
        let tracks = [
            spec(&a, D_EXACT, "Title1", "Artist1"),
            spec(&b, D_WEAK, "Renamed", "Artist2"),
        ];
        let root = fake_ipod(
            ipod_dir.path(),
            Some("FirewireGuid: 0x000A27001234ABCD\n"),
            &tracks,
            &[ipod_playlist("Mix", &[D_EXACT, D_WEAK])],
        );
        env.cfg.device.mountpoint = root;
        env.write_playlist("Mix", &format!("{}\n{}\n", a.display(), b.display()));
        playlists::sync_from_disk(env.store.conn(), &env.cfg, true).unwrap();
        World {
            env,
            a,
            b,
            _ipod_dir: ipod_dir,
        }
    }

    /// A run with no proposal accepted.
    fn run(&self) -> Run {
        let db = gpod::Database::open(&self.env.cfg.device.mountpoint).unwrap();
        let run = adoption::run(self.env.store.conn(), &db, false, &|_, _| {}).unwrap();
        assert_eq!(run.mapping, vec![(D_EXACT, self.id(&self.a))]);
        assert_eq!(run.left.len(), 1, "{:?}", run.left);
        assert_eq!(run.left[0].dbid, D_WEAK);
        assert!(run.left[0].candidates.contains(&self.id(&self.b)));
        run
    }
}

fn picks(p: &[(u64, Option<i64>)]) -> HashMap<u64, Option<i64>> {
    p.iter().copied().collect()
}

#[test]
fn a_pick_joins_the_mapping_and_changes_the_playlist_match() {
    let w = World::new();
    let mut run = w.run();
    let (a, b) = (w.id(&w.a), w.id(&w.b));
    assert_eq!(run.playlists.matched.len(), 1);
    assert!(
        !run.playlists.matched[0].same_contents,
        "dbid 2 is unmapped, so the iPod playlist cannot equal the file"
    );

    adoption::choose(w.env.store.conn(), &mut run, &picks(&[(D_WEAK, Some(b))])).unwrap();
    assert_eq!(run.mapping, vec![(D_EXACT, a), (D_WEAK, b)]);
    assert!(run.left.is_empty(), "{:?}", run.left);
    assert_eq!(run.playlists.matched.len(), 1);
    assert_eq!(run.playlists.matched[0].ipod_name, "Mix");
    assert!(run.playlists.matched[0].same_contents);
}

#[test]
fn choosing_again_replaces_the_earlier_choice() {
    let w = World::new();
    let mut run = w.run();
    let b = w.id(&w.b);
    let conn = w.env.store.conn();
    adoption::choose(conn, &mut run, &picks(&[(D_WEAK, Some(b))])).unwrap();
    assert!(run.playlists.matched[0].same_contents);

    // Leaving it unmanaged afterwards undoes the pick: no leftover mapping,
    // the proposal is back in `left` and the playlist differs again.
    adoption::choose(conn, &mut run, &picks(&[(D_WEAK, None)])).unwrap();
    assert_eq!(run.mapping, vec![(D_EXACT, w.id(&w.a))]);
    assert_eq!(run.left.len(), 1);
    assert_eq!(run.left[0].dbid, D_WEAK);
    assert!(!run.playlists.matched[0].same_contents);

    // No entry at all is the same as None.
    adoption::choose(conn, &mut run, &picks(&[(D_WEAK, Some(b))])).unwrap();
    adoption::choose(conn, &mut run, &picks(&[])).unwrap();
    assert_eq!(run.mapping, vec![(D_EXACT, w.id(&w.a))]);
    assert_eq!(run.left.len(), 1);
}

#[test]
fn a_conflict_is_refused_naming_the_file_and_leaves_the_run_unchanged() {
    let w = World::new();
    let mut run = w.run();
    let (mapping, left, playlists) = (run.mapping.clone(), run.left.clone(), run.playlists.clone());
    // File a is already held by dbid 1 (an accepted match).
    let err = adoption::choose(
        w.env.store.conn(),
        &mut run,
        &picks(&[(D_WEAK, Some(w.id(&w.a)))]),
    )
    .unwrap_err();
    match err {
        Error::ChoiceConflicts(lines) => {
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert!(lines[0].contains("a.mp3"), "{lines:?}");
            assert!(lines[0].contains("0000000000000001"), "{lines:?}");
            assert!(lines[0].contains("0000000000000002"), "{lines:?}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(run.mapping, mapping);
    assert_eq!(run.left, left);
    assert_eq!(run.playlists, playlists);
}
