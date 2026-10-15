//! `plan` and `apply` against a fake iPod: a temp folder whose iTunesDB is
//! written by libgpod (`make_ipod.c`). The iPod, the library, the playlists
//! folder, the index and the data folder are all in temp dirs, and
//! `device.mountpoint` is set, so `/proc/self/mountinfo` is never scanned.
//! The fixture helpers are the ones the core crate's integration tests use.

#[path = "../../../iprune-core/tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use common::{Env, IpodSpec, Kind, PlaylistSpec, fake_ipod, ipod_playlist};
use iprune_core::config::Paths;
use iprune_core::{adoption, backup, ipod, playlists};

use super::*;

const SYSINFO: &str = "FirewireGuid: 0x000A27001234ABCD\n";
const GUID: &str = "0x000A27001234ABCD";
const D1: u64 = 0xFFFF_FFFF_FFFF_FFF0;
const D2: u64 = 2;
const D3: u64 = 3;

struct World {
    env: Env,
    files: Vec<(&'static str, PathBuf)>,
    _ipod_dir: tempfile::TempDir,
    root: PathBuf,
    paths: Paths,
}

impl World {
    /// Library one, two, three, on the iPod as D1, D2, D3. Playlists Road
    /// [D1, D2] and Mix [D3] (with files), plus `extra`, and "Stray" [D2]
    /// with no file. Adopted. The index is the one `plan` opens.
    fn new(extra: &[(&'static str, Vec<u64>)]) -> World {
        let mut env = Env::new();
        let files = vec![
            ("one", env.add("one.mp3", Kind::Mp3, 1)),
            ("two", env.add("two.m4a", Kind::Aac, 1)),
            ("three", env.add("three.wav", Kind::Wav, 3)),
        ];
        env.scan();
        let ipod_dir = tempfile::tempdir().unwrap();
        let spec = |i: usize, dbid, title, artist| {
            let t = env.track(&files[i].1);
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
            spec(0, D1, "Title1", "Artist1"),
            spec(1, D2, "Title1", "Artist1"),
            spec(2, D3, "Title3", "Artist3"),
        ];
        let mut lists = vec![
            ipod_playlist("Road", &[D1, D2]),
            ipod_playlist("Mix", &[D3]),
            PlaylistSpec {
                name: "Podcasts",
                smart: false,
                podcasts: true,
                members: vec![],
            },
            ipod_playlist("Stray", &[D2]),
        ];
        for (name, members) in extra {
            lists.push(ipod_playlist(name, members));
        }
        let root = fake_ipod(ipod_dir.path(), Some(SYSINFO), &tracks, &lists);
        env.cfg.device.mountpoint = root.clone();
        // `Env` keeps its index at <base>/db/iprune.sqlite.
        let paths = Paths {
            config_file: env.music.parent().unwrap().join("config.toml"),
            data_dir: env.music.parent().unwrap().join("db"),
        };
        assert_eq!(
            paths.database(),
            env.music.parent().unwrap().join("db/iprune.sqlite")
        );
        let w = World {
            env,
            files,
            _ipod_dir: ipod_dir,
            root,
            paths,
        };
        w.write_list("Road", &[D1, D2]);
        w.write_list("Mix", &[D3]);
        for (name, members) in extra {
            w.write_list(name, members);
        }
        w.reload();
        let conn = w.env.store.conn();
        let db = w.open();
        let run = adoption::run(conn, &db, false, &|_, _| {}).unwrap();
        assert_eq!(run.mapping.len(), 3, "clean adoption: {:?}", run.left);
        adoption::save(conn, &run, &ipod::read(&db).firewire_guid.unwrap(), false).unwrap();
        w
    }

    fn file_of(&self, dbid: u64) -> &Path {
        let name = match dbid {
            D1 => "one",
            D2 => "two",
            D3 => "three",
            other => panic!("{other}"),
        };
        &self.files.iter().find(|(n, _)| *n == name).unwrap().1
    }

    fn write_list(&self, name: &str, dbids: &[u64]) {
        let text: String = dbids
            .iter()
            .map(|d| format!("{}\n", self.file_of(*d).display()))
            .collect();
        self.env.write_playlist(name, &text);
    }

    fn reload(&self) {
        playlists::sync_from_disk(self.env.store.conn(), &self.env.cfg, true).unwrap();
    }

    fn open(&self) -> gpod::Database {
        gpod::Database::open(&self.root).unwrap()
    }

    fn members(&self, name: &str) -> Option<Vec<u64>> {
        ipod::read(&self.open())
            .playlists
            .into_iter()
            .find(|p| p.name == name)
            .map(|p| p.members)
    }

    fn snapshot(&self) -> String {
        let mut out = Vec::new();
        gpod::snapshot::write(&self.open(), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn tree(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        common_tree(&self.root)
    }

    fn backups(&self) -> Vec<backup::Backup> {
        backup::list(&self.paths.data_dir.join("backups"), GUID).unwrap()
    }

    fn plan(&self) -> Planned {
        plan(&self.env.cfg, &self.paths).unwrap()
    }
}

fn common_tree(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.insert(
                    p.strip_prefix(root).unwrap().to_owned(),
                    fs::read(&p).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn many(names: &[&'static str]) -> Vec<(&'static str, Vec<u64>)> {
    names.iter().map(|n| (*n, vec![D1])).collect()
}

const SEVEN: [&str; 7] = ["P1", "P2", "P3", "P4", "P5", "P6", "P7"];

// ---- plan

#[test]
fn plan_of_an_unchanged_library_has_nothing_to_apply() {
    let w = World::new(&[]);
    let p = w.plan();
    assert_eq!(
        (p.work.playlists.ops.len(), p.work.playlists.left_out),
        (0, 0)
    );
    assert!(p.mass_removal.is_none());
    assert_eq!(p.ipod_name, "iPod de Test");
    assert!(p.scan.contains("3 unchanged, 0 added"), "{}", p.scan);
    assert!(
        p.summary
            .contains("playlists: 0 new, 0 changed, 0 renamed, 0 removed"),
        "{}",
        p.summary
    );
    assert!(p.plan.playlists.is_empty());
}

#[test]
fn plan_counts_each_kind_of_playlist_change_and_lists_them() {
    let w = World::new(&[("Old", vec![D1])]);
    w.write_list("Road", &[D2, D1, D1]);
    playlists::rename(w.env.store.conn(), &w.env.cfg, "Mix", "Mixed").unwrap();
    playlists::delete(w.env.store.conn(), &w.env.cfg, "Old").unwrap();
    w.write_list("Fresh", &[D3, D1]);
    let tree = w.tree();
    let p = w.plan();
    assert_eq!(p.work.playlists.ops.len(), 4);
    assert_eq!(p.work.playlists.left_out, 0);
    assert!(p.mass_removal.is_none());
    assert!(
        p.summary
            .contains("playlists: 1 new, 1 changed, 1 renamed, 1 removed"),
        "{}",
        p.summary
    );
    // The summary is the counts; the details are the summary plus every change.
    assert!(p.details.starts_with(&p.summary), "{}", p.details);
    for name in ["Road", "Mixed", "Old", "Fresh"] {
        assert!(
            p.details.contains(name),
            "{name} missing from:\n{}",
            p.details
        );
        assert!(!p.summary.contains(name), "{name} in the summary");
    }
    assert_eq!(w.tree(), tree, "planning writes nothing to the iPod");
    assert!(w.backups().is_empty());
}

#[test]
fn plan_copies_a_new_file_and_puts_it_in_its_playlist() {
    let w = World::new(&[]);
    let four = w.env.add("four.mp3", Kind::Mp3, 4);
    w.env.write_playlist(
        "Road",
        &format!("{}\n{}\n", w.file_of(D1).display(), four.display()),
    );
    let p = w.plan();
    // Road gets the new track, which this plan copies first.
    assert_eq!(
        (p.work.playlists.ops.len(), p.work.playlists.left_out),
        (1, 0)
    );
    assert_eq!(p.plan.copy.len(), 1);
    assert_eq!(p.work.tracks.copies.len(), 1);
    assert!(p.fits);
    assert!(p.to_apply.contains("copies 1 track"), "{}", p.to_apply);
    assert!(p.scan.contains("1 added"), "{}", p.scan);
}

#[test]
fn plan_sets_mass_removal_only_for_many_deleted_playlists() {
    let w = World::new(&many(&SEVEN));
    for n in &SEVEN[..5] {
        playlists::delete(w.env.store.conn(), &w.env.cfg, n).unwrap();
    }
    let p = w.plan();
    assert_eq!(p.work.playlists.ops.len(), 5);
    assert!(p.mass_removal.is_none(), "5 deletions are not many");

    for n in &SEVEN[5..] {
        playlists::delete(w.env.store.conn(), &w.env.cfg, n).unwrap();
    }
    let p = w.plan();
    assert_eq!(p.work.playlists.ops.len(), 7);
    let m = p.mass_removal.expect("7 of 9 mapped playlists");
    assert!(m.contains('7') && m.contains('9'), "{m}");
}

// Prevents (audit M9) the window syncing without a word a new playlist file
// that empties an iPod playlist iprune never managed.
#[test]
fn plan_sets_mass_removal_for_a_takeover_that_drops_tracks_only() {
    let w = World::new(&[]);
    // "Stray" is on the iPod only, holding [D2]. A file of that name with
    // D2 and one more track adds and drops nothing.
    w.write_list("Stray", &[D2, D1]);
    let p = w.plan();
    assert_eq!(p.plan.takeovers.len(), 1, "{:?}", p.plan.takeovers);
    assert_eq!(p.plan.takeovers[0].dropped, 0);
    assert!(p.mass_removal.is_none(), "{:?}", p.mass_removal);

    // Without D2 it drops the iPod playlist's only track.
    w.write_list("Stray", &[D1, D3]);
    let p = w.plan();
    assert_eq!(p.plan.takeovers[0].dropped, 1);
    let m = p.mass_removal.expect("a takeover that drops a track");
    assert!(
        m.contains("Stray") && m.contains("--allow-mass-removal"),
        "{m}"
    );
    assert!(p.details.contains("takes over"), "{}", p.details);
}

#[test]
fn plan_sees_playlist_files_deleted_from_disk() {
    // The scan inside `plan` reads the folder: files removed behind the
    // index's back are deletions in the plan.
    let w = World::new(&many(&SEVEN));
    for n in SEVEN {
        fs::remove_file(w.env.playlist_file(n)).unwrap();
    }
    let p = w.plan();
    assert_eq!(p.work.playlists.ops.len(), 7);
    assert!(p.mass_removal.is_some());
}

#[test]
fn plan_without_an_ipod_fails_and_names_the_mountpoint() {
    let mut w = World::new(&[]);
    w.env.cfg.device.mountpoint = w.env.music.parent().unwrap().join("not-mounted");
    let e = plan(&w.env.cfg, &w.paths).unwrap_err();
    assert!(
        format!("{e:#}").contains("not-mounted")
            || format!("{e:#}").to_lowercase().contains("ipod"),
        "{e:#}"
    );
}

// ---- apply

#[test]
fn apply_changes_the_ipod_and_reads_back() {
    let w = World::new(&[("Old", vec![D1])]);
    w.write_list("Road", &[D2, D1, D1]);
    playlists::rename(w.env.store.conn(), &w.env.cfg, "Mix", "Mixed").unwrap();
    playlists::delete(w.env.store.conn(), &w.env.cfg, "Old").unwrap();
    w.write_list("Fresh", &[D3, D1]);
    let stray = w.members("Stray");
    assert!(w.snapshot().contains("Podcasts"));

    let p = w.plan();
    let done = apply(
        &w.env.cfg,
        &w.paths,
        &p.work,
        false,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap();

    assert_eq!(done.applied, 4);
    assert!(done.warnings.is_empty(), "{:?}", done.warnings);
    // The notification says what was done, and is not the failure one.
    let note = done.note.expect("a write gives a notification");
    assert_eq!(note.summary, "iPod synced");
    assert!(!note.urgent);
    assert!(note.body.contains("4 playlist changes"), "{}", note.body);
    assert_eq!(w.members("Road"), Some(vec![D2, D1, D1]));
    assert_eq!(w.members("Mixed"), Some(vec![D3]));
    assert_eq!(w.members("Mix"), None);
    assert_eq!(w.members("Old"), None);
    assert_eq!(w.members("Fresh"), Some(vec![D3, D1]));
    assert_eq!(w.members("Stray"), stray, "unmanaged playlist untouched");
    assert!(w.snapshot().contains("Podcasts"), "Podcasts playlist kept");
    let backups = w.backups();
    assert_eq!(backups.len(), 1);
    assert_eq!(done.stamp.as_deref(), Some(backups[0].stamp.as_str()));
}

#[test]
fn after_apply_the_next_plan_is_empty_and_applies_nothing() {
    let w = World::new(&[]);
    w.write_list("Road", &[D2, D1]);
    let p = w.plan();
    assert_eq!(
        apply(
            &w.env.cfg,
            &w.paths,
            &p.work,
            false,
            &|_| {},
            &AtomicBool::new(false)
        )
        .unwrap()
        .applied,
        1
    );

    let again = w.plan();
    assert_eq!(again.work.playlists.ops.len(), 0);
    assert!(again.plan.playlists.is_empty());
    let tree = w.tree();
    let done = apply(
        &w.env.cfg,
        &w.paths,
        &again.work,
        false,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(done.applied, 0);
    assert_eq!(w.tree(), tree);
    assert_eq!(w.backups().len(), 1, "only the first apply backed up");
}

#[test]
fn apply_refuses_a_plan_that_no_longer_matches_and_writes_nothing() {
    let w = World::new(&[]);
    w.write_list("Road", &[D2, D1]);
    let p = w.plan();

    // The playlist changes again after the plan was shown.
    w.write_list("Road", &[D3]);
    w.reload();
    let tree = w.tree();
    let e = apply(
        &w.env.cfg,
        &w.paths,
        &p.work,
        false,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(format!("{e:#}").contains("changed since the plan"), "{e:#}");
    assert_eq!(w.tree(), tree, "the iPod is byte-identical");
    assert!(w.backups().is_empty(), "no backup either");
    assert!(!w.paths.data_dir.join("backups").exists());
    assert_eq!(w.members("Road"), Some(vec![D1, D2]));

    // A fresh plan goes through.
    let p = w.plan();
    assert_eq!(
        apply(
            &w.env.cfg,
            &w.paths,
            &p.work,
            false,
            &|_| {},
            &AtomicBool::new(false)
        )
        .unwrap()
        .applied,
        1
    );
    assert_eq!(w.members("Road"), Some(vec![D3]));
}

#[test]
fn apply_refuses_when_the_ipod_changed_since_the_plan() {
    let w = World::new(&[]);
    w.write_list("Road", &[D2, D1]);
    let p = w.plan();

    // Someone changes the iPod's Mix behind the window's back.
    let mut db = w.open();
    let mix = ipod::read(&db)
        .playlists
        .into_iter()
        .find(|p| p.name == "Mix")
        .unwrap();
    db.rename_playlist(mix.id, "Elsewhere").unwrap();
    db.write().unwrap();
    drop(db);
    let tree = w.tree();
    let e = apply(
        &w.env.cfg,
        &w.paths,
        &p.work,
        false,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(format!("{e:#}").contains("changed since the plan"), "{e:#}");
    assert_eq!(w.tree(), tree);
    assert!(w.backups().is_empty());
}

#[test]
fn apply_with_no_changes_writes_nothing_and_takes_no_backup() {
    let w = World::new(&[]);
    let p = w.plan();
    let tree = w.tree();
    let done = apply(
        &w.env.cfg,
        &w.paths,
        &p.work,
        false,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        (done.applied, done.stats_tracks, done.plays, done.recovered),
        (0, 0, 0, 0)
    );
    assert!(done.stamp.is_none() && done.warnings.is_empty());
    assert_eq!(done.note, None, "nothing applied, nothing to notify");
    assert_eq!(w.tree(), tree);
    assert!(w.backups().is_empty());
    assert!(!w.paths.data_dir.join("backups").exists());
}

#[test]
fn apply_refuses_a_mass_removal_without_the_flag_and_allows_it_with() {
    let w = World::new(&many(&SEVEN));
    for n in SEVEN {
        playlists::delete(w.env.store.conn(), &w.env.cfg, n).unwrap();
    }
    let p = w.plan();
    assert!(p.mass_removal.is_some());
    let tree = w.tree();

    let e = apply(
        &w.env.cfg,
        &w.paths,
        &p.work,
        false,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(format!("{e:#}").contains('7'), "{e:#}");
    assert_eq!(w.tree(), tree, "the iPod is byte-identical");
    assert!(w.backups().is_empty());
    for n in SEVEN {
        assert!(w.members(n).is_some(), "{n} still on the iPod");
    }

    let done = apply(
        &w.env.cfg,
        &w.paths,
        &p.work,
        true,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(done.applied, 7);
    for n in SEVEN {
        assert_eq!(w.members(n), None, "{n}");
    }
    assert!(w.members("Road").is_some() && w.members("Stray").is_some());
}

#[test]
fn apply_pulls_play_counts_before_writing() {
    let w = World::new(&[]);
    // Three tracks on the iPod's Play Counts: 3 plays on the first.
    let mut b = b"mhdp".to_vec();
    b.extend(96u32.to_le_bytes());
    b.extend(28u32.to_le_bytes());
    b.extend(3u32.to_le_bytes());
    b.resize(96, 0);
    for plays in [3u32, 0, 0] {
        b.extend(plays.to_le_bytes());
        b.extend(
            ((if plays > 0 {
                1_700_000_000i64 + 2_082_844_800
            } else {
                0
            }) as u32)
                .to_le_bytes(),
        );
        b.extend([0u8; 4]);
        b.extend((if plays > 0 { 80u32 } else { 0 }).to_le_bytes());
        b.extend([0u8; 4]);
        b.extend((if plays > 0 { 1u32 } else { 0 }).to_le_bytes());
        b.extend([0u8; 4]);
    }
    fs::write(w.root.join("iPod_Control/iTunes/Play Counts"), b).unwrap();
    w.write_list("Road", &[D2]);
    let p = w.plan();
    let done = apply(
        &w.env.cfg,
        &w.paths,
        &p.work,
        false,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!((done.stats_tracks, done.plays), (1, 3));
}

// The window plans, waits for the user, then applies: apply reads the iPod
// again under the lock, so the identity check (audit H1) compares the iPod
// with itself and a Play Counts that appeared during the wait is pulled, not
// refused.
#[test]
fn apply_after_plays_arrived_since_the_plan_reads_the_ipod_again_and_pulls_them() {
    let w = World::new(&[]);
    w.write_list("Road", &[D2, D1]);
    let p = w.plan();
    assert!(!p.work.is_empty(), "set up");
    let mut b = b"mhdp".to_vec();
    b.extend(96u32.to_le_bytes());
    b.extend(28u32.to_le_bytes());
    b.extend(3u32.to_le_bytes());
    b.resize(96, 0);
    for plays in [2u32, 0, 0] {
        b.extend(plays.to_le_bytes());
        b.extend(
            ((if plays > 0 {
                1_700_000_000i64 + 2_082_844_800
            } else {
                0
            }) as u32)
                .to_le_bytes(),
        );
        b.extend([0u8; 4]);
        b.extend((if plays > 0 { 80u32 } else { 0 }).to_le_bytes());
        b.extend([0u8; 4]);
        b.extend([0u8; 4]);
        b.extend([0u8; 4]);
    }
    fs::write(w.root.join("iPod_Control/iTunes/Play Counts"), b).unwrap();
    let done = apply(
        &w.env.cfg,
        &w.paths,
        &p.work,
        false,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!((done.stats_tracks, done.plays), (1, 2));
    assert_eq!(w.members("Road"), Some(vec![D2, D1]));
    assert_eq!(w.backups().len(), 1);
}

// Guards against comparing the whole plan: free space moves whenever anything
// writes to the iPod's filesystem, and must not refuse an unchanged change set.
#[test]
fn apply_ignores_a_change_in_free_space_since_the_plan() {
    let w = World::new(&[]);
    w.write_list("Road", &[D2, D1]);
    let p = w.plan();
    fs::write(
        w.root.join("iPod_Control/Music/filler.bin"),
        vec![7u8; 8 << 20],
    )
    .unwrap();
    let done = apply(
        &w.env.cfg,
        &w.paths,
        &p.work,
        false,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(done.applied, 1);
    assert_eq!(w.members("Road"), Some(vec![D2, D1]));
}

#[test]
fn plan_reports_that_the_plan_fits() {
    let w = World::new(&[]);
    assert!(w.plan().fits);
}

#[test]
fn apply_refuses_after_a_playlist_edit_reaches_the_index() {
    let w = World::new(&[]);
    w.write_list("Mix", &[D3, D1]);
    let p = w.plan();
    assert_eq!(p.work.playlists.ops.len(), 1);
    // The file is edited again and the window re-reads the folder.
    w.write_list("Mix", &[D1]);
    w.reload();
    let tree = w.tree();
    let e = apply(
        &w.env.cfg,
        &w.paths,
        &p.work,
        false,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(format!("{e:#}").contains("changed since the plan"), "{e:#}");
    assert_eq!(w.tree(), tree);
    assert!(w.backups().is_empty());
}

// Documents what apply does not see: a file edited on disk but not yet read
// into the index is not part of the plan either way, so the planned change
// is applied as shown and the edit waits for the next sync.
#[test]
fn apply_applies_the_shown_plan_when_a_file_is_edited_but_not_yet_read() {
    let w = World::new(&[]);
    w.write_list("Mix", &[D3, D1]);
    let p = w.plan();
    w.write_list("Mix", &[D1]);
    let done = apply(
        &w.env.cfg,
        &w.paths,
        &p.work,
        false,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(done.applied, 1);
    assert_eq!(w.members("Mix"), Some(vec![D3, D1]), "what the user saw");
    // The next plan picks the edit up.
    assert_eq!(w.plan().work.playlists.ops.len(), 1);
}

// ---- tracks

fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

#[test]
fn apply_copies_new_files_reports_progress_and_the_next_plan_is_empty() {
    let w = World::new(&[]);
    let four = w.env.add("four.mp3", Kind::Mp3, 4);
    w.env.write_playlist(
        "Road",
        &format!("{}\n{}\n", w.file_of(D1).display(), four.display()),
    );
    let p = w.plan();
    assert_eq!(p.work.tracks.copies.len(), 1);
    let dbid = p.work.tracks.copies[0].dbid;
    let seen = std::cell::RefCell::new(Vec::new());
    let done = apply(
        &w.env.cfg,
        &w.paths,
        &p.work,
        false,
        &|e| seen.borrow_mut().push(e),
        &no_cancel(),
    )
    .unwrap();
    assert_eq!(
        (done.copied, done.recopied, done.removed, done.retagged),
        (1, 0, 0, 0)
    );
    assert_eq!(done.bytes, fs::metadata(&four).unwrap().len());
    assert_eq!(done.stopped, None);
    assert!(done.skipped.is_empty() && done.warnings.is_empty());
    assert_eq!(w.members("Road"), Some(vec![D1, dbid]));
    assert!(
        seen.borrow()
            .iter()
            .any(|e| matches!(e, Progress::Copying { total: 1, .. }))
    );
    assert!(
        seen.borrow()
            .iter()
            .any(|e| matches!(e, Progress::Verifying))
    );

    let again = w.plan();
    assert!(again.work.is_empty(), "{:?}", again.work);
    assert_eq!(again.to_apply, "Nothing to apply.");
}

#[test]
fn apply_reports_a_file_that_vanished_after_the_plan_as_skipped() {
    let w = World::new(&[]);
    let four = w.env.add("four.mp3", Kind::Mp3, 4);
    w.env.write_playlist(
        "Road",
        &format!("{}\n{}\n", w.file_of(D1).display(), four.display()),
    );
    let p = w.plan();
    fs::remove_file(&four).unwrap();
    // The window re-reads the index before applying, not the music folder,
    // so the same work is rebuilt and the file is found missing at its turn.
    let done = apply(&w.env.cfg, &w.paths, &p.work, false, &|_| {}, &no_cancel()).unwrap();
    assert_eq!(done.copied, 0);
    assert_eq!(done.skipped.len(), 1);
    assert_eq!(done.skipped[0].0, four);
    assert_eq!(done.stopped, None);
    assert_eq!(
        w.members("Road"),
        Some(vec![D1]),
        "the skipped file is left out of the playlist"
    );
}

#[test]
fn apply_stops_when_asked_and_says_so() {
    let w = World::new(&[]);
    let news: Vec<PathBuf> = (0..3)
        .map(|k| w.env.add(&format!("n{k}.mp3"), Kind::Mp3, 40 + k))
        .collect();
    let text: String = news.iter().map(|f| format!("{}\n", f.display())).collect();
    w.env.write_playlist("Fresh", &text);
    let p = w.plan();
    assert_eq!(p.work.tracks.copies.len(), 3);
    let cancel = AtomicBool::new(false);
    let progress = |e: Progress| {
        if matches!(e, Progress::Copying { done: 1, .. }) {
            cancel.store(true, Ordering::Relaxed);
        }
    };
    let done = apply(&w.env.cfg, &w.paths, &p.work, false, &progress, &cancel).unwrap();
    assert!(done.stopped.is_some());
    assert_eq!(done.copied, 2);
    assert_eq!(done.applied, 0);
    assert_eq!(w.members("Fresh"), None, "playlists wait for the next sync");
    // The next plan has the third copy and the playlist.
    let rest = w.plan();
    assert_eq!(rest.work.tracks.copies.len(), 1);
    assert_eq!(rest.work.playlists.ops.len(), 1);
}

#[test]
fn to_apply_names_each_kind_of_work() {
    use iprune_core::transfer::TrackWork;
    assert_eq!(to_apply(&FullWork::default()), "Nothing to apply.");
    let w = World::new(&[]);
    let four = w.env.add("four.mp3", Kind::Mp3, 4);
    w.env
        .write_playlist("Road", &format!("{}\n", four.display()));
    let p = w.plan();
    let line = to_apply(&p.work);
    assert!(line.contains("copies 1 track "), "{line}");
    assert!(line.contains("removes 0 tracks"), "{line}");
    let only_playlists = FullWork {
        tracks: TrackWork::default(),
        ..p.work.clone()
    };
    assert!(to_apply(&only_playlists).contains("copies 0 tracks"));
}

fn copying() -> Progress {
    Progress::Copying {
        done: 1,
        total: 3,
        bytes: 10,
        bytes_total: 30,
        name: "a.mp3".into(),
    }
}

// Prevents Stop staying off for good after the first checkpoint write.
#[test]
fn stop_comes_back_at_the_next_copy_after_a_write() {
    // What the button's sensitivity does over a run with a checkpoint.
    let mut enabled = true;
    for p in [copying(), Progress::Writing, copying()] {
        if let Some(e) = stop_enabled(&p, false) {
            enabled = e;
        }
        if matches!(p, Progress::Writing) {
            assert!(!enabled, "off while the database is written");
        }
    }
    assert!(enabled, "on again at the next copy");
}

#[test]
fn stop_stays_off_at_a_copy_once_pressed_and_during_a_write() {
    assert_eq!(stop_enabled(&copying(), true), Some(false));
    assert_eq!(stop_enabled(&Progress::Writing, true), Some(false));
    assert_eq!(stop_enabled(&Progress::Writing, false), Some(false));
    assert_eq!(stop_enabled(&copying(), false), Some(true));
}

#[test]
fn stop_is_untouched_by_removing_and_verifying() {
    assert_eq!(stop_enabled(&Progress::Verifying, false), None);
    assert_eq!(
        stop_enabled(&Progress::Removing { done: 0, total: 1 }, true),
        None
    );
}

// ---- the iPod lock

#[test]
fn apply_is_refused_while_the_ipod_lock_is_held_and_changes_nothing() {
    let w = World::new(&[]);
    w.write_list("Road", &[D2, D1]);
    let p = w.plan();
    assert_eq!(p.work.playlists.ops.len(), 1, "there is something to apply");
    let tree = w.tree();
    let snap = w.snapshot();

    let lock = iprune_core::lock::ipod(&w.paths).unwrap();
    let e = apply(
        &w.env.cfg,
        &w.paths,
        &p.work,
        false,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(
        matches!(
            e.downcast_ref::<iprune_core::Error>(),
            Some(iprune_core::Error::IpodBusy)
        ),
        "{e:#}"
    );
    assert_eq!(w.tree(), tree);
    assert_eq!(w.snapshot(), snap);
    assert!(w.backups().is_empty());

    drop(lock);
    let done = apply(
        &w.env.cfg,
        &w.paths,
        &p.work,
        false,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(done.applied, 1);
}

// The lock is taken before the iPod is read: with the database unreadable
// the answer is still "busy", not a libgpod error.
#[test]
fn apply_takes_the_lock_before_it_reads_the_ipod() {
    let w = World::new(&[]);
    let p = w.plan();
    fs::write(
        w.root.join("iPod_Control/iTunes/iTunesDB"),
        b"not a database",
    )
    .unwrap();
    let args = || {
        apply(
            &w.env.cfg,
            &w.paths,
            &p.work,
            false,
            &|_| {},
            &AtomicBool::new(false),
        )
    };
    let lock = iprune_core::lock::ipod(&w.paths).unwrap();
    let e = args().unwrap_err();
    assert!(
        matches!(
            e.downcast_ref::<iprune_core::Error>(),
            Some(iprune_core::Error::IpodBusy)
        ),
        "{e:#}"
    );
    // Without the lock the unreadable database is what fails.
    drop(lock);
    let e = args().unwrap_err();
    assert!(
        e.downcast_ref::<iprune_core::Error>()
            .is_none_or(|e| !matches!(e, iprune_core::Error::IpodBusy)),
        "{e:#}"
    );
}

#[test]
fn a_default_applied_has_no_notification() {
    assert_eq!(Applied::default().note, None);
}
