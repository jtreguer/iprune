//! The adoption worker against a fake iPod: a temp folder whose iTunesDB is
//! written by libgpod (`make_ipod.c`). The iPod, the library, the playlists
//! folder, the index and the data folder are all in temp dirs, and
//! `device.mountpoint` is set, so `/proc/self/mountinfo` is never scanned
//! and no real device is touched. The fixture helpers are the ones the core
//! crate's integration tests use.

// The sync worker tests include the same file; sharing it would mean
// exposing their module outside `cfg(test)` code.
#[allow(clippy::duplicate_mod)]
#[path = "../../../iprune-core/tests/common/mod.rs"]
mod common;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use common::{Env, IpodSpec, Kind, fake_ipod, ipod_playlist};
use iprune_core::adoption;
use iprune_core::config::Paths;
use iprune_core::store::devices;

use super::*;

const SYSINFO: &str = "FirewireGuid: 0x000A27001234ABCD\n";
/// Exact copy of a: accepted without a question.
const EXACT: u64 = 1;
/// File b under other tags: weak, candidates b then a.
const WEAK: u64 = 2;
/// Two copies of c, only 5 in a playlist: 5 has a default, 6 has none.
const COPY_IN_LIST: u64 = 5;
const COPY_LOOSE: u64 = 6;
/// On the iPod only.
const ONLY_IPOD: u64 = 9;

struct World {
    env: Env,
    files: Vec<(&'static str, PathBuf)>,
    root: PathBuf,
    _ipod_dir: tempfile::TempDir,
    paths: Paths,
}

impl World {
    fn new() -> World {
        World::with_sysinfo(Some(SYSINFO))
    }

    fn with_sysinfo(sysinfo: Option<&str>) -> World {
        let mut env = Env::new();
        let files = vec![
            ("a", env.add("a.mp3", Kind::Mp3, 1)),
            ("b", env.add("b.mp3", Kind::Mp3, 2)),
            ("c", env.add("c.wav", Kind::Wav, 3)),
            ("d", env.add("d.m4a", Kind::Aac, 2)),
        ];
        env.scan();
        let ipod_dir = tempfile::tempdir().unwrap();
        let spec = |f: usize, dbid, title, artist| {
            let t = env.track(&files[f].1);
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
        let mut gone = spec(0, ONLY_IPOD, "Gone", "Nobody");
        gone.size = 12345;
        let tracks = [
            spec(0, EXACT, "Title1", "Artist1"),
            spec(1, WEAK, "Renamed", "Artist2"),
            spec(2, COPY_IN_LIST, "Title3", "Artist3"),
            spec(2, COPY_LOOSE, "Title3", "Artist3"),
            gone,
        ];
        let lists = [
            ipod_playlist("Mix", &[EXACT, WEAK]),
            ipod_playlist("Other", &[COPY_IN_LIST]),
        ];
        let root = fake_ipod(ipod_dir.path(), sysinfo, &tracks, &lists);
        env.cfg.device.mountpoint = root.clone();
        let abs = |i: usize| files[i].1.display().to_string();
        env.write_playlist("Mix", &format!("{}\n{}\n", abs(0), abs(1)));
        env.write_playlist("Other", &format!("{}\n", abs(2)));
        iprune_core::playlists::sync_from_disk(env.store.conn(), &env.cfg, true).unwrap();
        // `Env` keeps its index at <base>/db/iprune.sqlite.
        let base = env.music.parent().unwrap().to_owned();
        let paths = Paths {
            config_file: base.join("config.toml"),
            data_dir: base.join("db"),
        };
        assert_eq!(paths.database(), base.join("db/iprune.sqlite"));
        World {
            env,
            files,
            root,
            _ipod_dir: ipod_dir,
            paths,
        }
    }

    fn id(&self, name: &str) -> i64 {
        let p = &self.files.iter().find(|(n, _)| *n == name).unwrap().1;
        self.env.track(p).id
    }

    fn prepare(&self) -> Result<Review> {
        prepare(&self.env.cfg, &self.paths, &|_, _| {})
    }

    fn save(&self, review: Review, picked: &[usize]) -> Result<Saved> {
        save(&self.env.cfg, &self.paths, review, picked)
    }

    fn report(&self) -> PathBuf {
        self.paths.data_dir.join("adopt-report.txt")
    }

    /// The index, opened afresh.
    fn store(&self) -> Store {
        Store::open(&self.paths.database()).unwrap()
    }

    /// `(dbid, library id)` of every saved track mapping, over all devices.
    fn saved_mapping(&self) -> Vec<(u64, i64)> {
        let store = self.store();
        let mut out = Vec::new();
        for d in devices::devices(store.conn()).unwrap() {
            for m in devices::track_mappings(store.conn(), d.id).unwrap() {
                out.push((m.dbid, m.track_id));
            }
        }
        out.sort_unstable();
        out
    }

    fn saved_playlists(&self) -> usize {
        let store = self.store();
        devices::devices(store.conn())
            .unwrap()
            .iter()
            .map(|d| {
                devices::playlist_mappings(store.conn(), d.id)
                    .unwrap()
                    .len()
            })
            .sum()
    }

    fn set_guid(&self, guid: &str) {
        fs::write(
            self.root.join("iPod_Control/Device/SysInfo"),
            format!("FirewireGuid: {guid}\n"),
        )
        .unwrap();
    }
}

fn row(r: &Review, dbid: u64) -> &ReviewRow {
    r.rows
        .iter()
        .find(|x| x.dbid == dbid)
        .unwrap_or_else(|| panic!("no row for {dbid}"))
}

/// The picked indexes for `r.rows`, `0` (leave it) unless listed.
fn picks(r: &Review, p: &[(u64, usize)]) -> Vec<usize> {
    r.rows
        .iter()
        .map(|x| p.iter().find(|(d, _)| *d == x.dbid).map_or(0, |(_, i)| *i))
        .collect()
}

fn ids(row: &ReviewRow) -> Vec<Option<i64>> {
    row.choices.iter().map(|c| c.track_id).collect()
}

// ---- review_rows

#[test]
fn rows_list_leaving_it_first_then_the_candidates_best_first() {
    let w = World::new();
    let r = w.prepare().unwrap();
    let dbids: Vec<u64> = r.rows.iter().map(|x| x.dbid).collect();
    assert_eq!(dbids, vec![WEAK, COPY_IN_LIST, COPY_LOOSE]);

    let weak = row(&r, WEAK);
    // b agrees on the artist with the iPod's tags, a does not: b is first.
    assert_eq!(
        ids(weak),
        vec![None, Some(w.id("b")), Some(w.id("a"))],
        "{:?}",
        weak.choices
    );
    assert!(
        weak.choices[1].label.contains("b.mp3"),
        "{:?}",
        weak.choices
    );
    assert!(
        weak.choices[2].label.contains("a.mp3"),
        "{:?}",
        weak.choices
    );
    assert_eq!(
        weak.why,
        adoption::reason(r.run.adoption.proposals[0].reason)
    );
    assert_eq!(weak.ipod, "Artist2 – Renamed");

    let copy = row(&r, COPY_LOOSE);
    assert_eq!(ids(copy), vec![None, Some(w.id("c"))]);
}

#[test]
fn rows_suggest_what_accept_proposals_maps_or_leaving_it() {
    let w = World::new();
    let r = w.prepare().unwrap();
    assert_eq!(row(&r, WEAK).suggested, 1);
    assert_eq!(row(&r, COPY_IN_LIST).suggested, 1);
    // Two iPod copies of one file: only the one in a playlist gets it.
    assert_eq!(row(&r, COPY_LOOSE).suggested, 0);

    // The same, from the library function the CLI uses.
    let mapped: HashMap<u64, i64> = adopt::resolve(&r.run.adoption, true)
        .0
        .into_iter()
        .collect();
    for x in &r.rows {
        let want = mapped.get(&x.dbid).copied();
        assert_eq!(x.choices[x.suggested].track_id, want, "dbid {}", x.dbid);
    }
}

// ---- choices

#[test]
fn choices_maps_indexes_to_library_ids() {
    let w = World::new();
    let r = w.prepare().unwrap();
    let (b, c) = (w.id("b"), w.id("c"));
    let want = |weak: Option<i64>, in_list: Option<i64>, loose: Option<i64>| {
        HashMap::from([(WEAK, weak), (COPY_IN_LIST, in_list), (COPY_LOOSE, loose)])
    };
    let a = w.id("a");
    #[allow(clippy::type_complexity)]
    let cases: Vec<(&str, Vec<usize>, HashMap<u64, Option<i64>>)> = vec![
        ("all leave", vec![0, 0, 0], want(None, None, None)),
        ("picks", vec![1, 1, 1], want(Some(b), Some(c), Some(c))),
        ("second candidate", vec![2, 0, 0], want(Some(a), None, None)),
        // The last two rows have a single candidate: index 2 does not exist.
        ("out of range", vec![9, 2, 2], want(None, None, None)),
    ];
    for (name, picked, expected) in cases {
        assert_eq!(choices(&r.rows, &picked), expected, "{name}");
    }
    // Rows without an index are not in the map at all.
    assert_eq!(choices(&r.rows, &[1]), HashMap::from([(WEAK, Some(b))]));
}

// ---- conflicts

#[test]
fn no_conflict_for_the_suggestions_or_for_leaving_everything() {
    let w = World::new();
    let r = w.prepare().unwrap();
    let suggested: Vec<usize> = r.rows.iter().map(|x| x.suggested).collect();
    assert_eq!(conflicts(&r, &suggested), Vec::<String>::new());
    assert_eq!(conflicts(&r, &[0, 0, 0]), Vec::<String>::new());
}

#[test]
fn a_file_picked_for_two_rows_is_a_conflict() {
    let w = World::new();
    let r = w.prepare().unwrap();
    let c = conflicts(&r, &picks(&r, &[(COPY_IN_LIST, 1), (COPY_LOOSE, 1)]));
    assert_eq!(c.len(), 1, "{c:?}");
    assert!(c[0].contains("c.wav"), "{c:?}");
    assert!(c[0].contains("2 iPod tracks"), "{c:?}");
    assert!(!c[0].contains("already matched"), "{c:?}");
}

#[test]
fn a_file_an_accepted_match_holds_is_a_conflict_of_the_other_wording() {
    let w = World::new();
    let r = w.prepare().unwrap();
    // Index 2 of WEAK is a.mp3, which dbid 1 matched without doubt.
    let c = conflicts(&r, &picks(&r, &[(WEAK, 2)]));
    assert_eq!(c.len(), 1, "{c:?}");
    assert!(c[0].contains("a.mp3"), "{c:?}");
    assert!(c[0].contains("already matched"), "{c:?}");
    assert!(!c[0].contains("2 iPod tracks"), "{c:?}");
}

#[test]
fn every_conflict_gets_a_line() {
    let w = World::new();
    let r = w.prepare().unwrap();
    let c = conflicts(
        &r,
        &picks(&r, &[(WEAK, 2), (COPY_IN_LIST, 1), (COPY_LOOSE, 1)]),
    );
    assert_eq!(c.len(), 2, "{c:?}");
    assert_eq!(
        c.iter().filter(|l| l.contains("already matched")).count(),
        1
    );
    assert_eq!(c.iter().filter(|l| l.contains("2 iPod tracks")).count(), 1);
}

// ---- prepare

#[test]
fn prepare_writes_the_report_and_summarises_the_match() {
    let w = World::new();
    assert!(!w.report().exists());
    let r = w.prepare().unwrap();
    assert_eq!(r.report, w.report());
    assert_eq!(
        fs::read_to_string(&r.report).unwrap(),
        adoption::report(&r.run)
    );
    assert_eq!(r.ipod_name, "iPod de Test");
    assert!(r.guid.ends_with("000A27001234ABCD"), "{}", r.guid);
    assert!(r.summary.contains("proposals: 3"), "{}", r.summary);
    // The accepted match is in the run's mapping; the proposals are not yet.
    assert_eq!(r.run.mapping, vec![(EXACT, w.id("a"))]);
    // Preparing saves nothing.
    assert_eq!(w.saved_mapping(), vec![]);
}

#[test]
fn prepare_is_refused_on_an_adopted_ipod_and_writes_no_report() {
    let w = World::new();
    let r = w.prepare().unwrap();
    w.save(r, &[1, 1, 0]).unwrap();
    fs::remove_file(w.report()).unwrap();
    let before = w.saved_mapping();

    let err = w.prepare().err().expect("refused").to_string();
    assert!(err.contains("already adopted"), "{err}");
    assert!(!w.report().exists());
    assert_eq!(w.saved_mapping(), before);
}

#[test]
fn prepare_needs_a_guid_to_identify_the_ipod() {
    let w = World::with_sysinfo(None);
    let err = w.prepare().err().expect("refused").to_string();
    assert!(
        err.contains("cannot be identified") && err.contains("USB serial"),
        "{err}"
    );
    assert!(!w.report().exists());
    assert_eq!(w.saved_mapping(), vec![]);
}

// ---- save

#[test]
fn save_records_the_picks_and_counts_what_it_saved() {
    let w = World::new();
    let r = w.prepare().unwrap();
    // WEAK -> b, COPY_IN_LIST -> c, COPY_LOOSE left on the iPod.
    let picked = picks(&r, &[(WEAK, 1), (COPY_IN_LIST, 1)]);
    let saved = w.save(r, &picked).unwrap();
    assert_eq!(
        w.saved_mapping(),
        {
            let mut m = vec![
                (EXACT, w.id("a")),
                (WEAK, w.id("b")),
                (COPY_IN_LIST, w.id("c")),
            ];
            m.sort_unstable();
            m
        },
        "{:?}",
        w.saved_mapping()
    );
    assert_eq!(
        saved,
        Saved {
            tracks: 3,
            playlists: 2,
            // The loose copy of c, and the track with no library file.
            unmanaged: 2,
            // Only d is in no iPod track's mapping.
            to_copy: 1,
        }
    );
    assert_eq!(w.saved_playlists(), 2);
}

#[test]
fn save_with_everything_left_keeps_only_the_accepted_match() {
    let w = World::new();
    let r = w.prepare().unwrap();
    let saved = w.save(r, &[0, 0, 0]).unwrap();
    assert_eq!(w.saved_mapping(), vec![(EXACT, w.id("a"))]);
    assert_eq!(
        saved,
        Saved {
            tracks: 1,
            playlists: 2,
            unmanaged: 4,
            to_copy: 3,
        }
    );
}

#[test]
fn save_refuses_another_ipod_and_saves_nothing() {
    let w = World::new();
    let r = w.prepare().unwrap();
    w.set_guid("0x000A27009999FFFF");
    let err = w.save(r, &[1, 1, 0]).unwrap_err().to_string();
    assert!(err.contains("not the one reviewed"), "{err}");
    assert_eq!(w.saved_mapping(), vec![]);
    assert_eq!(w.saved_playlists(), 0);
    let store = w.store();
    assert!(devices::devices(store.conn()).unwrap().is_empty());
}

#[test]
fn save_refuses_conflicting_picks_and_saves_nothing() {
    let w = World::new();
    let r = w.prepare().unwrap();
    // c twice.
    let err = w.save(r, &[0, 1, 1]).unwrap_err();
    match err.downcast_ref::<iprune_core::Error>() {
        Some(iprune_core::Error::ChoiceConflicts(lines)) => {
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert!(lines[0].contains("c.wav"), "{lines:?}");
        }
        other => panic!("{err:#} ({other:?})"),
    }
    assert_eq!(w.saved_mapping(), vec![]);
    assert_eq!(w.saved_playlists(), 0);
    // The iPod still counts as not adopted: preparing is allowed again.
    w.prepare().unwrap();

    // And a file an accepted match holds.
    let r = w.prepare().unwrap();
    assert!(w.save(r, &[2, 0, 0]).is_err());
    assert_eq!(w.saved_mapping(), vec![]);
}

#[test]
fn a_second_review_cannot_replace_a_saved_adoption() {
    let w = World::new();
    let first = w.prepare().unwrap();
    let second = w.prepare().unwrap();
    w.save(first, &[1, 1, 0]).unwrap();
    let saved = w.saved_mapping();
    assert_eq!(saved.len(), 3);

    let err = w.save(second, &[0, 0, 0]).unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<iprune_core::Error>(),
            Some(iprune_core::Error::AlreadyAdopted(_))
        ),
        "{err:#}"
    );
    assert_eq!(w.saved_mapping(), saved);
}

#[test]
fn after_a_save_the_ipod_counts_as_adopted_and_sync_plans() {
    let w = World::new();
    let before = crate::sync_job::plan(&w.env.cfg, &w.paths);
    assert!(before.is_err(), "an unadopted iPod must not plan");

    let r = w.prepare().unwrap();
    w.save(r, &[1, 1, 0]).unwrap();
    let planned = crate::sync_job::plan(&w.env.cfg, &w.paths).unwrap();
    assert_eq!(planned.ipod_name, "iPod de Test");
    // Library file d is on no iPod track: the plan copies it, and only it.
    assert_eq!(planned.work.tracks.copies.len(), 1, "{}", planned.details);
}

#[test]
fn two_rows_and_an_accepted_match_sharing_a_file_get_the_third_wording() {
    let w = World::new();
    let mut r = w.prepare().unwrap();
    // A second proposal that also lists a.mp3, which dbid 1 matched.
    let a = w.id("a");
    let mut extra = r.run.adoption.proposals[0].clone();
    extra.dbid = 77;
    extra.candidates = vec![a];
    r.run.adoption.proposals.push(extra);
    let mut extra_row = row(&r, WEAK).clone();
    extra_row.dbid = 77;
    extra_row.choices = vec![
        row(&r, WEAK).choices[0].clone(),
        row(&r, WEAK).choices[2].clone(),
    ];
    r.rows.push(extra_row);
    assert_eq!(r.rows.len(), 4);

    let c = conflicts(&r, &picks(&r, &[(WEAK, 2), (77, 1)]));
    assert_eq!(c.len(), 1, "{c:?}");
    assert!(c[0].contains("a.mp3"), "{c:?}");
    assert!(c[0].contains("2 iPod tracks"), "{c:?}");
    assert!(c[0].contains("already matched"), "{c:?}");
}

// The early check reads only SysInfo and the index: it must hold even when
// the iPod database cannot be read, and before any report is written.
#[test]
fn prepare_refuses_an_adopted_ipod_without_reading_its_database() {
    let w = World::new();
    let r = w.prepare().unwrap();
    w.save(r, &[1, 1, 0]).unwrap();
    fs::remove_file(w.report()).unwrap();
    fs::write(
        w.root.join("iPod_Control/iTunes/iTunesDB"),
        b"not a database",
    )
    .unwrap();
    let err = format!("{:#}", w.prepare().err().expect("refused"));
    assert!(err.contains("already adopted"), "{err}");
    assert!(!w.report().exists());
}

#[test]
fn an_unreadable_database_on_an_unadopted_ipod_is_still_an_error() {
    let w = World::new();
    fs::write(
        w.root.join("iPod_Control/iTunes/iTunesDB"),
        b"not a database",
    )
    .unwrap();
    let err = format!("{:#}", w.prepare().err().expect("fails"));
    assert!(!err.contains("already adopted"), "{err}");
    assert!(!w.report().exists());
}

#[test]
fn save_says_so_when_the_reviewed_ipod_is_no_longer_connected() {
    let w = World::new();
    let r = w.prepare().unwrap();
    fs::remove_file(w.root.join("iPod_Control/Device/SysInfo")).unwrap();
    let err = w.save(r, &[1, 1, 0]).unwrap_err().to_string();
    assert!(err.contains("no longer connected"), "{err}");
    assert!(!err.contains("not the one reviewed"), "{err}");
    assert_eq!(w.saved_mapping(), vec![]);
    let store = w.store();
    assert!(devices::devices(store.conn()).unwrap().is_empty());
}
