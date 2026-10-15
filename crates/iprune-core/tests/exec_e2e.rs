//! `exec::sync_playlists` and `exec::restore` end to end on a fake iPod: a
//! temp folder whose iTunesDB is written by libgpod (`make_ipod.c`) with a
//! SysInfo holding a FirewireGuid. The iPod, the index and the data folder
//! are all in temp dirs; the real iPod and the real data folder are never
//! touched.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use common::{Env, IpodSpec, Kind, PlaylistSpec, fake_ipod, ipod_playlist};
use iprune_core::config::Paths;
use iprune_core::exec::{self, Op, Options, Outcome, Target, Work};
use iprune_core::store::devices;
use iprune_core::sync::{self, Prepared};
use iprune_core::{Error, adoption, backup, device, ipod, playlists, stats, verify};

const SYSINFO: &str = "FirewireGuid: 0x000A27001234ABCD\n";
/// The device's GUID as libgpod reports it, which names the backup folder.
const GUID: &str = "0x000A27001234ABCD";
/// The GUID as `device::firewire_guid` reads it, without a database.
const BARE_GUID: &str = "000A27001234ABCD";
const D1: u64 = 0xFFFF_FFFF_FFFF_FFF0;
const D2: u64 = 2;
const D3: u64 = 3;

/// `(track_id, plays, skips, rating, last_played)`.
type StatsRow = (i64, i64, i64, Option<i64>, Option<i64>);
/// Track mappings `(track, dbid)` and playlist mappings `(playlist, ipod id, name)`.
type MappingView = (Vec<(i64, u64)>, Vec<(i64, Option<u64>, String)>);

pub struct World {
    pub env: Env,
    pub files: Vec<(&'static str, PathBuf)>,
    _ipod_dir: tempfile::TempDir,
    _data_dir: tempfile::TempDir,
    pub root: PathBuf,
    pub paths: Paths,
}

impl World {
    /// Library one, two, three (dbids D1, D2, D3 on the iPod). The iPod also
    /// has, besides `extra` managed playlists (name, dbids; a file with the
    /// same content is written for each), the playlists Road [D1, D2] and Mix
    /// [D3] with files, a smart playlist "Top", the Podcasts playlist and
    /// "Stray" [D2], which has no file. Adopted.
    pub fn new(extra: &[(&'static str, Vec<u64>)]) -> World {
        let mut env = Env::new();
        let files = vec![
            ("one", env.add("one.mp3", Kind::Mp3, 1)),
            ("two", env.add("two.m4a", Kind::Aac, 1)),
            ("three", env.add("three.wav", Kind::Wav, 3)),
        ];
        env.scan();
        let ipod_dir = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
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
                name: "Top",
                smart: true,
                podcasts: false,
                members: vec![D1],
            },
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
        let paths = Paths {
            config_file: data_dir.path().join("config.toml"),
            data_dir: data_dir.path().join("data"),
        };
        let w = World {
            env,
            files,
            _ipod_dir: ipod_dir,
            _data_dir: data_dir,
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

    pub fn file(&self, name: &str) -> &Path {
        &self.files.iter().find(|(n, _)| *n == name).unwrap().1
    }

    fn file_of(&self, dbid: u64) -> &Path {
        self.file(match dbid {
            D1 => "one",
            D2 => "two",
            D3 => "three",
            other => panic!("{other}"),
        })
    }

    /// Writes playlist file `name` listing the library files of `dbids`.
    pub fn write_list(&self, name: &str, dbids: &[u64]) {
        let text: String = dbids
            .iter()
            .map(|d| format!("{}\n", self.file_of(*d).display()))
            .collect();
        self.env.write_playlist(name, &text);
    }

    pub fn reload(&self) {
        playlists::sync_from_disk(self.env.store.conn(), &self.env.cfg, true).unwrap();
    }

    pub fn open(&self) -> gpod::Database {
        gpod::Database::open(&self.root).unwrap()
    }

    pub fn contents(&self) -> ipod::Contents {
        ipod::read(&self.open())
    }

    pub fn prepare(&self) -> Prepared {
        sync::prepare(self.env.store.conn(), &self.env.cfg, &self.open()).unwrap()
    }

    pub fn sync(&self) -> iprune_core::Result<Outcome> {
        self.sync_with(Options::default())
    }

    pub fn sync_with(&self, opts: Options) -> iprune_core::Result<Outcome> {
        let prepared = self.prepare();
        let work = exec::playlist_work(&prepared.plan, &prepared.tracks_map, &prepared.ipod);
        self.run_work(&prepared, &work, opts)
    }

    pub fn run_work(
        &self,
        p: &Prepared,
        work: &Work,
        opts: Options,
    ) -> iprune_core::Result<Outcome> {
        exec::sync_playlists(
            self.env.store.conn(),
            &self.env.cfg,
            &self.paths,
            &iprune_core::lock::ipod(&self.paths).unwrap(),
            &self.root,
            self.open(),
            p,
            work,
            opts,
        )
    }

    pub fn snapshot(&self) -> String {
        let mut out = Vec::new();
        gpod::snapshot::write(&self.open(), &mut out).unwrap();
        verify::normalize(&String::from_utf8(out).unwrap())
    }

    pub fn ipod_playlist(&self, name: &str) -> Option<(u64, Vec<u64>)> {
        self.contents()
            .playlists
            .into_iter()
            .find(|p| p.name == name)
            .map(|p| (p.id, p.members))
    }

    pub fn device(&self) -> i64 {
        devices::device_id(self.env.store.conn(), GUID)
            .unwrap()
            .expect("adopted")
    }

    pub fn backups(&self) -> Vec<backup::Backup> {
        backup::list(&self.paths.data_dir.join("backups"), GUID).unwrap()
    }

    /// Every file under the iPod, by relative path, with its bytes.
    pub fn ipod_tree(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        tree(&self.root)
    }

    pub fn stats_rows(&self) -> Vec<StatsRow> {
        let mut stmt = self
            .env
            .store
            .conn()
            .prepare(
                "SELECT track_id, plays, skips, rating, last_played FROM stats ORDER BY track_id",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    pub fn pending_pulls(&self) -> i64 {
        self.env
            .store
            .conn()
            .query_row("SELECT COUNT(*) FROM pending_pulls", [], |r| r.get(0))
            .unwrap()
    }

    /// Plays on the iPod's `Play Counts`: one `(plays, unix last played,
    /// rating, skips)` per track, in database order.
    pub fn write_play_counts(&self, entries: &[(u32, i64, u32, u32)]) {
        const ENTRY: u32 = 28;
        const HEADER: u32 = 96;
        let mut b = b"mhdp".to_vec();
        b.extend(HEADER.to_le_bytes());
        b.extend(ENTRY.to_le_bytes());
        b.extend((entries.len() as u32).to_le_bytes());
        b.resize(HEADER as usize, 0);
        for (plays, last, rating, skips) in entries {
            b.extend(plays.to_le_bytes());
            b.extend(((*last + 2_082_844_800) as u32).to_le_bytes());
            b.extend(0u32.to_le_bytes()); // bookmark
            b.extend(rating.to_le_bytes());
            b.extend(0u32.to_le_bytes());
            b.extend(skips.to_le_bytes());
            b.extend(0u32.to_le_bytes());
        }
        fs::write(self.root.join("iPod_Control/iTunes/Play Counts"), b).unwrap();
    }

    pub fn play_counts(&self) -> PathBuf {
        self.root.join("iPod_Control/iTunes/Play Counts")
    }

    /// Makes libgpod think the iPod keeps artwork (an iPod Classic model in
    /// SysInfo and an Artwork folder), so a write also writes an ArtworkDB.
    pub fn enable_artwork(&self) {
        fs::write(
            self.root.join("iPod_Control/Device/SysInfo"),
            format!("ModelNumStr: MC297\n{SYSINFO}"),
        )
        .unwrap();
        fs::create_dir_all(self.root.join("iPod_Control/Artwork")).unwrap();
    }

    pub fn track_id(&self, name: &str) -> i64 {
        self.env.track(self.file(name)).id
    }
}

pub fn tree(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd {
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

/// Edits that exercise every playlist operation: Road reordered with a
/// repeat, Mix renamed, Old deleted, Fresh created.
fn change_everything(w: &World) {
    let conn = w.env.store.conn();
    w.write_list("Road", &[D2, D1, D1]);
    playlists::rename(conn, &w.env.cfg, "Mix", "Mixed").unwrap();
    playlists::delete(conn, &w.env.cfg, "Old").unwrap();
    w.write_list("Fresh", &[D3, D1]);
    w.reload();
}

fn world_with_old() -> World {
    World::new(&[("Old", vec![D1])])
}

// ---- the happy path

#[test]
fn every_playlist_operation_is_applied_and_read_back() {
    let w = world_with_old();
    let (road, mix, stray, old) = (
        w.ipod_playlist("Road").unwrap().0,
        w.ipod_playlist("Mix").unwrap().0,
        w.ipod_playlist("Stray").unwrap(),
        w.ipod_playlist("Old").unwrap().0,
    );
    let tracks_before = w.contents().tracks;
    change_everything(&w);

    let out = w.sync().unwrap();
    assert_eq!(out.applied, 4);
    assert_eq!(out.left_out, 0);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    let c = w.contents();
    assert_eq!(
        w.ipod_playlist("Road"),
        Some((road, vec![D2, D1, D1])),
        "reordered, repeats kept"
    );
    assert_eq!(
        w.ipod_playlist("Mixed"),
        Some((mix, vec![D3])),
        "renamed in place, same id"
    );
    assert!(w.ipod_playlist("Mix").is_none());
    assert!(w.ipod_playlist("Old").is_none(), "{old} deleted");
    let fresh = w.ipod_playlist("Fresh").expect("created");
    assert_eq!(fresh.1, vec![D3, D1]);
    assert!(![road, mix, stray.0, old].contains(&fresh.0));
    // Unmanaged things are exactly as they were.
    assert_eq!(w.ipod_playlist("Stray"), Some(stray));
    assert_eq!(c.smart_playlists, vec!["Top"]);
    assert_eq!(c.name.as_deref(), Some("iPod de Test"));
    assert_eq!(c.tracks.len(), tracks_before.len());
    for (a, b) in c.tracks.iter().zip(&tracks_before) {
        let (mut a, mut b) = (a.clone(), b.clone());
        a.adopt.playlists = 0;
        b.adopt.playlists = 0;
        assert_eq!(a, b);
    }
}

#[test]
fn mappings_are_updated_with_the_created_id_and_tombstones_are_dropped() {
    let w = world_with_old();
    change_everything(&w);
    let conn = w.env.store.conn();
    assert_eq!(
        iprune_core::store::removed_playlists(conn).unwrap().len(),
        1,
        "a tombstone for Old"
    );
    w.sync().unwrap();

    let dev = w.device();
    let mut got: Vec<(String, Option<u64>)> = devices::playlist_mappings(conn, dev)
        .unwrap()
        .into_iter()
        .map(|m| (m.ipod_name, m.ipod_id))
        .collect();
    got.sort();
    let id = |n: &str| Some(w.ipod_playlist(n).unwrap().0);
    assert_eq!(
        got,
        vec![
            ("Fresh".to_string(), id("Fresh")),
            ("Mixed".to_string(), id("Mixed")),
            ("Road".to_string(), id("Road")),
        ]
    );
    assert!(
        iprune_core::store::removed_playlists(conn)
            .unwrap()
            .is_empty(),
        "tombstone dropped"
    );
    let last: Option<i64> = conn
        .query_row(
            "SELECT last_sync_at FROM devices WHERE id = ?1",
            [dev],
            |r| r.get(0),
        )
        .unwrap();
    assert!(last.is_some_and(|t| t > 1_700_000_000), "{last:?}");
}

#[test]
fn stats_csv_manifest_and_backup_are_written() {
    let w = world_with_old();
    change_everything(&w);
    let before_db = fs::read(w.root.join("iPod_Control/iTunes/iTunesDB")).unwrap();
    let snap_before = {
        let mut o = Vec::new();
        gpod::snapshot::write(&w.open(), &mut o).unwrap();
        String::from_utf8(o).unwrap()
    };
    let out = w.sync().unwrap();

    let csv = fs::read_to_string(w.paths.data_dir.join("stats.csv")).unwrap();
    assert!(
        csv.starts_with("path,plays,skips,rating,last_played\n"),
        "{csv}"
    );

    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(w.root.join("iPod_Control/iprune/manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["firewire_guid"], GUID);
    assert_eq!(manifest["tracks"].as_array().unwrap().len(), 3);
    let names: Vec<String> = manifest["playlists"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            p["file"]
                .as_str()
                .unwrap()
                .rsplit('/')
                .next()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(names.len(), 3, "{names:?}");
    for n in ["Fresh.m3u8", "Mixed.m3u8", "Road.m3u8"] {
        assert!(names.iter().any(|x| x == n), "{n} in {names:?}");
    }
    let dbids: Vec<u64> = manifest["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["dbid"].as_u64().unwrap())
        .collect();
    for d in [D1, D2, D3] {
        assert!(dbids.contains(&d), "{d} in {dbids:?}");
    }

    // The backup holds the files as they were before, and what iprune knew.
    let b = &out.backup;
    assert_eq!(fs::read(b.dir.join("iTunesDB")).unwrap(), before_db);
    assert_eq!(
        fs::read_to_string(b.dir.join("snapshot.txt")).unwrap(),
        snap_before
    );
    for f in ["plan.json", "work.json", "mappings.json"] {
        let v: serde_json::Value =
            serde_json::from_slice(&fs::read(b.dir.join(f)).unwrap()).unwrap();
        assert!(!v.is_null(), "{f}");
    }
    let work: serde_json::Value =
        serde_json::from_slice(&fs::read(b.dir.join("work.json")).unwrap()).unwrap();
    assert_eq!(work["playlist_ops"].as_array().unwrap().len(), 4);
    assert_eq!(w.backups(), vec![b.clone()]);
    assert_ne!(
        fs::read(w.root.join("iPod_Control/iTunes/iTunesDB")).unwrap(),
        before_db
    );
}

#[test]
fn old_backups_are_pruned_to_keep_backups_and_the_new_one_is_never_deleted() {
    let mut w = world_with_old();
    // Older backups, taken long ago (distinct seconds, so naming is not in play).
    let root = w.paths.data_dir.join("backups");
    for t in [1_000, 2_000, 3_000, 4_000] {
        backup::create(&root, GUID, &w.root, t, &[]).unwrap();
    }
    w.env.cfg.sync.keep_backups = 3;
    w.write_list("Road", &[D2]);
    w.reload();
    let out = w.sync().unwrap();
    assert_eq!(out.pruned, 2);
    let stamps: Vec<String> = w.backups().into_iter().map(|b| b.stamp).collect();
    assert_eq!(
        stamps,
        vec![
            backup::stamp(3_000),
            backup::stamp(4_000),
            out.backup.stamp.clone()
        ]
    );

    // keep_backups = 0 still keeps the backup just taken.
    w.env.cfg.sync.keep_backups = 0;
    w.write_list("Road", &[D1]);
    w.reload();
    let out = w.sync().unwrap();
    let left = w.backups();
    assert_eq!(left.len(), 1);
    assert!(left[0].dir.join("iTunesDB").is_file());
    assert_eq!(left[0], out.backup);
}

#[test]
fn stats_csv_failing_is_a_warning_and_the_sync_still_stands() {
    let w = world_with_old();
    // A directory where the file should go.
    fs::create_dir_all(w.paths.data_dir.join("stats.csv")).unwrap();
    w.write_list("Road", &[D3]);
    w.reload();
    let out = w.sync().unwrap();
    assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
    assert!(out.warnings[0].contains("stats.csv"), "{:?}", out.warnings);
    assert_eq!(w.ipod_playlist("Road").unwrap().1, vec![D3]);
    let last: Option<i64> = w
        .env
        .store
        .conn()
        .query_row(
            "SELECT last_sync_at FROM devices WHERE id = ?1",
            [w.device()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(last.is_some());
    assert!(
        w.prepare().plan.playlists.is_empty(),
        "mappings were committed"
    );
}

// ---- regression: syncing twice

// Prevents: a second sync repeating work (doubled playlists, new ids) or
// drifting the database.
#[test]
fn syncing_twice_gives_an_empty_second_plan_and_an_unchanged_snapshot() {
    let w = world_with_old();
    change_everything(&w);
    w.sync().unwrap();
    let snap = w.snapshot();

    let prepared = w.prepare();
    assert!(
        prepared.plan.playlists.is_empty(),
        "{:?}",
        prepared.plan.playlists
    );
    let work = exec::playlist_work(&prepared.plan, &prepared.tracks_map, &prepared.ipod);
    assert!(work.ops.is_empty(), "{:?}", work.ops);
    assert_eq!(work.left_out, 0);
    // Even running the empty work through the executor changes nothing.
    let out = w.run_work(&prepared, &work, Options::default()).unwrap();
    assert_eq!(out.applied, 0);
    assert_eq!(w.snapshot(), snap);
    assert!(w.prepare().plan.playlists.is_empty());
}

mod prop {
    use super::*;
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum Edit {
        Keep,
        Rewrite(Vec<u8>),
        Delete,
    }

    fn edit() -> impl Strategy<Value = Edit> {
        prop_oneof![
            Just(Edit::Keep),
            proptest::collection::vec(0u8..3, 0..7).prop_map(Edit::Rewrite),
            Just(Edit::Delete),
        ]
    }

    fn dbid(i: u8) -> u64 {
        [D1, D2, D3][i as usize]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]

        // Random edits to the playlist files, a sync, then another: the
        // iPod matches the files, the second plan has no playlist work and
        // the snapshot does not move.
        #[test]
        fn random_playlist_edits_sync_once_and_then_settle(
            road in edit(),
            mix in edit(),
            old in edit(),
            news in proptest::collection::vec(proptest::collection::vec(0u8..3, 0..5), 0..3),
        ) {
            let w = world_with_old();
            let mut want: BTreeMap<String, Vec<u64>> = BTreeMap::new();
            let conn = w.env.store.conn();
            for (name, e, base) in [("Road", &road, vec![D1, D2]), ("Mix", &mix, vec![D3]), ("Old", &old, vec![D1])] {
                match e {
                    Edit::Keep => { want.insert(name.into(), base); }
                    Edit::Rewrite(v) => {
                        let ids: Vec<u64> = v.iter().map(|i| dbid(*i)).collect();
                        w.write_list(name, &ids);
                        want.insert(name.into(), ids);
                    }
                    Edit::Delete => playlists::delete(conn, &w.env.cfg, name).unwrap(),
                }
            }
            for (i, v) in news.iter().enumerate() {
                let ids: Vec<u64> = v.iter().map(|i| dbid(*i)).collect();
                let name = format!("New{i}");
                w.write_list(&name, &ids);
                want.insert(name, ids);
            }
            w.reload();
            w.sync().unwrap();

            let got: BTreeMap<String, Vec<u64>> = w
                .contents()
                .playlists
                .into_iter()
                .filter(|p| p.name != "Stray")
                .map(|p| (p.name, p.members))
                .collect();
            prop_assert_eq!(&got, &want);
            prop_assert_eq!(w.ipod_playlist("Stray").unwrap().1, vec![D2]);

            let snap = w.snapshot();
            let prepared = w.prepare();
            prop_assert!(prepared.plan.playlists.is_empty(), "{:?}", prepared.plan.playlists);
            let work = exec::playlist_work(&prepared.plan, &prepared.tracks_map, &prepared.ipod);
            prop_assert!(work.ops.is_empty());
            w.run_work(&prepared, &work, Options::default()).unwrap();
            prop_assert_eq!(w.snapshot(), snap);
        }
    }
}

// ---- statistics

#[test]
fn plays_are_pulled_once_and_not_again_after_the_write() {
    let w = world_with_old();
    w.write_play_counts(&[
        (3, 1_700_000_000, 80, 1),
        (0, 0, 0, 0),
        (2, 1_600_000_000, 0, 0),
    ]);
    let before = w.contents();
    assert_eq!(
        (
            before.tracks[0].recent_plays,
            before.tracks[0].recent_skips,
            before.tracks[0].rating
        ),
        (3, 1, 80),
        "the Play Counts file is read by libgpod"
    );
    w.write_list("Road", &[D2]);
    w.reload();
    let out = w.sync().unwrap();
    assert_eq!((out.pulled.tracks(), out.pulled.plays()), (2, 5));
    let one = w.track_id("one");
    let three = w.track_id("three");
    let mut rows = w.stats_rows();
    rows.sort();
    // libgpod reads iPod times as local time, so allow for the offset.
    let near = |got: Option<i64>, want: i64| got.is_some_and(|g| (g - want).abs() <= 86_400);
    assert_eq!(rows.len(), 2);
    let want: Vec<_> = rows.clone();
    let r1 = rows.iter().find(|r| r.0 == one).unwrap();
    let r3 = rows.iter().find(|r| r.0 == three).unwrap();
    assert_eq!((r1.1, r1.2, r1.3), (3, 1, Some(80)));
    assert!(near(r1.4, 1_700_000_000), "{r1:?}");
    assert_eq!((r3.1, r3.2, r3.3), (2, 0, None));
    assert!(near(r3.4, 1_600_000_000), "{r3:?}");
    assert_eq!(w.pending_pulls(), 0, "settled after the write");
    // libgpod renamed Play Counts: it will not be pulled again.
    assert!(!w.play_counts().exists());

    w.write_list("Road", &[D1]);
    w.reload();
    let out = w.sync().unwrap();
    assert_eq!(out.pulled.tracks(), 0);
    let mut rows = w.stats_rows();
    rows.sort();
    assert_eq!(rows, want, "no double counting");
    let csv = fs::read_to_string(w.paths.data_dir.join("stats.csv")).unwrap();
    assert!(csv.contains(",3,1,80,1"), "{csv}");
}

/// Work that applies but fails the checks before the write: it sets Road to
/// [D1] while the playlist is expected to hold [D1, D2].
fn failing_work(w: &World) -> (Prepared, Work) {
    let prepared = w.prepare();
    let (road, _) = w.ipod_playlist("Road").unwrap();
    let work = Work {
        ops: vec![Op::SetMembers {
            ipod_id: road,
            name: "Road".into(),
            members: vec![D1],
        }],
        managed: vec![Target {
            playlist_id: w.env.playlist_id("Road").unwrap(),
            ipod_id: Some(road),
            name: "Road".into(),
            members: vec![D1, D2],
        }],
        deleted: vec![],
        left_out: 0,
    };
    (prepared, work)
}

#[test]
fn a_problem_before_the_write_writes_nothing_and_undoes_the_stats() {
    let w = world_with_old();
    w.write_play_counts(&[
        (3, 1_700_000_000, 80, 1),
        (0, 0, 0, 0),
        (2, 1_600_000_000, 0, 0),
    ]);
    let (prepared, work) = failing_work(&w);
    let tree_before = w.ipod_tree();
    let rows_before = w.stats_rows();

    let e = w
        .run_work(&prepared, &work, Options::default())
        .unwrap_err();
    match &e {
        Error::PreWriteCheck(p) => assert!(p.iter().any(|m| m.contains("\"Road\"")), "{p:?}"),
        other => panic!("{other}"),
    }
    assert!(e.is_check_failure());
    // The iPod: every file byte for byte, Play Counts included.
    let after = w.ipod_tree();
    let itunes_db = Path::new("iPod_Control/iTunes/iTunesDB");
    assert_eq!(after.get(itunes_db), tree_before.get(itunes_db));
    assert_eq!(
        after
            .keys()
            .filter(|k| !k.starts_with("iPod_Control/iprune"))
            .collect::<Vec<_>>(),
        tree_before.keys().collect::<Vec<_>>()
    );
    for (k, v) in &tree_before {
        assert_eq!(after.get(k), Some(v), "{}", k.display());
    }
    assert!(
        !after.keys().any(|k| k.starts_with("iPod_Control/iprune")),
        "no manifest"
    );
    // The stats pull was undone and nothing is pending.
    assert_eq!(w.stats_rows(), rows_before);
    assert!(w.stats_rows().is_empty());
    assert_eq!(w.pending_pulls(), 0);
    // Mappings and last_sync untouched.
    let last: Option<i64> = w
        .env
        .store
        .conn()
        .query_row(
            "SELECT last_sync_at FROM devices WHERE id = ?1",
            [w.device()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(last, None);

    // The plays are still on the iPod, so the next good sync counts them once.
    w.write_list("Road", &[D2]);
    w.reload();
    w.sync().unwrap();
    let plays: i64 = w.stats_rows().iter().map(|r| r.1).sum();
    assert_eq!(plays, 5);
}

#[test]
fn a_pull_left_pending_by_a_sync_that_never_wrote_is_undone_by_the_next_sync() {
    let w = world_with_old();
    w.write_play_counts(&[(3, 1_700_000_000, 80, 1), (0, 0, 0, 0), (0, 0, 0, 0)]);
    // A sync that pulled and then died before writing.
    let prepared = w.prepare();
    let hash = stats::play_counts(&w.root).unwrap().live;
    assert!(hash.is_some());
    stats::pull(
        w.env.store.conn(),
        w.device(),
        &prepared.tracks_map,
        &prepared.ipod,
        hash,
    )
    .unwrap();
    assert_eq!(w.stats_rows().len(), 1);
    assert_eq!(w.pending_pulls(), 1);

    w.write_list("Road", &[D2]);
    w.reload();
    let out = w.sync().unwrap();
    assert_eq!(out.recovered, 1);
    assert_eq!(out.pulled.tracks(), 1);
    assert_eq!(
        w.stats_rows().iter().map(|r| r.1).sum::<i64>(),
        3,
        "counted once, not twice"
    );
    assert_eq!(w.pending_pulls(), 0);
}

#[test]
fn a_pull_left_pending_by_a_sync_that_did_write_is_kept() {
    let w = world_with_old();
    w.write_play_counts(&[(3, 1_700_000_000, 80, 1), (0, 0, 0, 0), (0, 0, 0, 0)]);
    let prepared = w.prepare();
    let hash = stats::play_counts(&w.root).unwrap().live;
    stats::pull(
        w.env.store.conn(),
        w.device(),
        &prepared.tracks_map,
        &prepared.ipod,
        hash,
    )
    .unwrap();
    // The write happened (libgpod renamed the file) but the process died
    // before settling.
    fs::rename(w.play_counts(), w.play_counts().with_extension("bak")).unwrap();

    w.write_list("Road", &[D2]);
    w.reload();
    let out = w.sync().unwrap();
    assert_eq!(out.recovered, 0);
    assert_eq!(w.stats_rows().iter().map(|r| r.1).sum::<i64>(), 3);
    assert_eq!(w.pending_pulls(), 0);
}

// ---- refusals

#[test]
fn a_mass_removal_is_refused_before_anything_changes() {
    let extra: Vec<(&'static str, Vec<u64>)> = ["P1", "P2", "P3", "P4", "P5", "P6", "P7"]
        .into_iter()
        .map(|n| (n, vec![D1]))
        .collect();
    let w = World::new(&extra);
    w.write_play_counts(&[(3, 1_700_000_000, 80, 1), (0, 0, 0, 0), (0, 0, 0, 0)]);
    for (n, _) in &extra {
        playlists::delete(w.env.store.conn(), &w.env.cfg, n).unwrap();
    }
    let tree_before = w.ipod_tree();
    let e = w.sync().unwrap_err();
    assert!(
        matches!(
            e,
            Error::MassRemoval {
                deletes: 7,
                managed: 9
            }
        ),
        "{e}"
    );
    assert_eq!(w.ipod_tree(), tree_before, "the iPod is untouched");
    assert!(w.stats_rows().is_empty(), "no stats pulled");
    assert_eq!(w.pending_pulls(), 0);
    assert!(w.backups().is_empty(), "no backup taken");
    assert!(!w.paths.data_dir.join("backups").exists());

    // With the flag it goes through.
    let out = w
        .sync_with(Options {
            allow_mass_removal: true,
        })
        .unwrap();
    assert_eq!(out.applied, 7);
    for (n, _) in &extra {
        assert!(w.ipod_playlist(n).is_none(), "{n}");
    }
    assert!(w.ipod_playlist("Road").is_some() && w.ipod_playlist("Stray").is_some());
}

#[test]
fn a_device_that_is_not_adopted_is_refused_and_nothing_changes() {
    let w = world_with_old();
    let prepared = w.prepare();
    let work = exec::playlist_work(&prepared.plan, &prepared.tracks_map, &prepared.ipod);
    w.env
        .store
        .conn()
        .execute("PRAGMA foreign_keys = OFF", [])
        .unwrap();
    let conn = w.env.store.conn();
    conn.execute("DELETE FROM device_tracks", []).unwrap();
    conn.execute("DELETE FROM device_playlists", []).unwrap();
    conn.execute("DELETE FROM devices", []).unwrap();
    let tree_before = w.ipod_tree();
    let e = w
        .run_work(&prepared, &work, Options::default())
        .unwrap_err();
    assert!(matches!(e, Error::NotAdopted(_)), "{e}");
    assert_eq!(w.ipod_tree(), tree_before);
    assert!(w.backups().is_empty());
}

/// Makes a folder read-only and puts the permissions back when dropped, even
/// if the test panics, so no read-only folder is left in the temp dir.
#[cfg(unix)]
struct ReadOnly(PathBuf);

#[cfg(unix)]
impl ReadOnly {
    /// `None` when permissions do not block writes (running as root).
    fn new(dir: &Path) -> Option<ReadOnly> {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
        let guard = ReadOnly(dir.to_owned());
        let blocked = fs::File::create(dir.join(".probe")).is_err();
        blocked.then_some(guard)
    }
}

#[cfg(unix)]
impl Drop for ReadOnly {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

#[test]
#[cfg(unix)]
fn a_write_the_ipod_refuses_leaves_it_unchanged_and_undoes_the_stats() {
    let w = world_with_old();
    w.write_play_counts(&[(3, 1_700_000_000, 80, 1), (0, 0, 0, 0), (0, 0, 0, 0)]);
    w.write_list("Road", &[D2]);
    w.reload();
    let tree_before = w.ipod_tree();
    let Some(guard) = ReadOnly::new(&w.root.join("iPod_Control/iTunes")) else {
        eprintln!("running as root: permissions do not block writes, test skipped");
        return;
    };
    let res = w.sync();
    drop(guard);
    let e = res.unwrap_err();
    assert!(
        matches!(e, Error::Gpod(gpod::Error::Write(_))),
        "an untouched iPod gives the plain write error: {e}"
    );
    assert_eq!(w.ipod_tree(), tree_before);
    assert!(w.stats_rows().is_empty(), "stats undone");
    assert_eq!(w.pending_pulls(), 0);
    assert_eq!(w.ipod_playlist("Road").unwrap().1, vec![D1, D2]);
}

/// A sync whose write fails part way, for real: libgpod writes ArtworkDB
/// before iTunesDB, so with artwork enabled and a read-only iTunes folder
/// the ArtworkDB is rewritten and the iTunesDB is not. Returns `None` when
/// permissions do not block writes (running as root).
#[cfg(unix)]
fn half_written() -> Option<(World, Error, BTreeMap<PathBuf, Vec<u8>>)> {
    let w = world_with_old();
    w.enable_artwork();
    w.write_play_counts(&[(3, 1_700_000_000, 80, 1), (0, 0, 0, 0), (0, 0, 0, 0)]);
    w.write_list("Road", &[D2]);
    w.reload();
    let before = w.ipod_tree();
    let guard = ReadOnly::new(&w.root.join("iPod_Control/iTunes"))?;
    let res = w.sync();
    drop(guard);
    Some((w, res.unwrap_err(), before))
}

#[test]
#[cfg(unix)]
fn a_write_that_fails_part_way_is_reported_as_incomplete_and_leaves_the_pull_pending() {
    let Some((w, e, before)) = half_written() else {
        eprintln!("running as root: permissions do not block writes, test skipped");
        return;
    };
    let stamp = w.backups().last().unwrap().stamp.clone();
    match &e {
        Error::WriteIncomplete { stamp: s, cause } => {
            assert_eq!(*s, stamp);
            // libgpod's wording is not part of the contract.
            assert!(!cause.is_empty());
        }
        other => panic!("{other}"),
    }
    assert!(e.is_check_failure(), "exit code 3");
    // The iPod really is half written: ArtworkDB changed, iTunesDB and
    // Play Counts did not.
    let after = w.ipod_tree();
    let key = |p: &str| PathBuf::from(p);
    assert_ne!(
        after.get(&key("iPod_Control/Artwork/ArtworkDB")),
        before.get(&key("iPod_Control/Artwork/ArtworkDB"))
    );
    for same in [
        "iPod_Control/iTunes/iTunesDB",
        "iPod_Control/iTunes/Play Counts",
    ] {
        assert_eq!(after.get(&key(same)), before.get(&key(same)), "{same}");
    }
    // The pull is neither undone nor settled: the next sync decides.
    assert_eq!(w.pending_pulls(), 1);
    assert_eq!(w.stats_rows().iter().map(|r| r.1).sum::<i64>(), 3);
    // The backup taken first can put things right.
    let b = w.backups().pop().unwrap();
    assert_eq!(
        fs::read(b.dir.join("iTunesDB")).unwrap(),
        before[&key("iPod_Control/iTunes/iTunesDB")]
    );
}

#[test]
#[cfg(unix)]
fn after_an_incomplete_write_the_next_sync_counts_every_play_once() {
    let Some((w, _, _)) = half_written() else {
        return;
    };
    let out = w.sync().unwrap();
    // Play Counts is still there, so the write never happened: undone, and
    // pulled again from the same file.
    assert_eq!(out.recovered, 1);
    assert_eq!(out.pulled.plays(), 3);
    assert_eq!(w.stats_rows().iter().map(|r| r.1).sum::<i64>(), 3);
    assert_eq!(w.pending_pulls(), 0);
    assert!(!w.play_counts().exists(), "this time libgpod renamed it");
    assert_eq!(w.ipod_playlist("Road").unwrap().1, vec![D2]);
}

// The case that matters most: the pull is pending, the user plays more, so
// Play Counts grows and hashes differently, then the next sync runs.
#[test]
#[cfg(unix)]
fn plays_added_after_an_incomplete_write_are_counted_once_not_twice() {
    let Some((w, _, _)) = half_written() else {
        return;
    };
    // 3 plays were pulled and are pending; the iPod now says 5 in total.
    w.write_play_counts(&[(5, 1_700_000_500, 80, 1), (0, 0, 0, 0), (0, 0, 0, 0)]);
    let out = w.sync().unwrap();
    assert_eq!(out.recovered, 1, "the earlier pull is undone");
    assert_eq!(out.pulled.plays(), 5);
    assert_eq!(
        w.stats_rows().iter().map(|r| r.1).sum::<i64>(),
        5,
        "not 3 + 5"
    );
    assert_eq!(w.pending_pulls(), 0);
}

#[test]
fn an_unreadable_pending_pull_is_dropped_with_a_warning_and_the_sync_goes_on() {
    let w = world_with_old();
    w.write_play_counts(&[(3, 1_700_000_000, 80, 1), (0, 0, 0, 0), (0, 0, 0, 0)]);
    let prepared = w.prepare();
    let live = stats::play_counts(&w.root).unwrap().live;
    stats::pull(
        w.env.store.conn(),
        w.device(),
        &prepared.tracks_map,
        &prepared.ipod,
        live,
    )
    .unwrap();
    w.env
        .store
        .conn()
        .execute("UPDATE pending_pulls SET changes = '{broken'", [])
        .unwrap();
    w.write_list("Road", &[D2]);
    w.reload();
    let out = w.sync().unwrap();
    assert_eq!(out.recovered, 0);
    assert!(
        out.warnings.iter().any(|m| m.contains("pending")),
        "{:?}",
        out.warnings
    );
    assert_eq!(w.pending_pulls(), 0);
    assert_eq!(w.ipod_playlist("Road").unwrap().1, vec![D2]);
}

// ---- restore

fn restore(w: &World, stamp: &str) -> iprune_core::Result<exec::Restored> {
    exec::restore(w.env.store.conn(), &w.paths, &w.root, GUID, stamp, false)
}

/// Like `exec::restore`, trying again while the iPod lock reports busy. The
/// tests start helper programs from other threads; one forked at the wrong
/// moment holds a copy of a just released lock file for a few milliseconds.
fn restore_with(w: &World, stamp: &str, discard: bool) -> iprune_core::Result<exec::Restored> {
    for _ in 0..200 {
        match exec::restore(w.env.store.conn(), &w.paths, &w.root, GUID, stamp, discard) {
            Err(Error::IpodBusy) => std::thread::sleep(std::time::Duration::from_millis(5)),
            other => return other,
        }
    }
    unreachable!("the iPod lock stayed busy")
}

fn mapping_view(w: &World) -> MappingView {
    let conn = w.env.store.conn();
    let dev = w.device();
    let mut t: Vec<_> = devices::track_mappings(conn, dev)
        .unwrap()
        .into_iter()
        .map(|m| (m.track_id, m.dbid))
        .collect();
    let mut p: Vec<_> = devices::playlist_mappings(conn, dev)
        .unwrap()
        .into_iter()
        .map(|m| (m.playlist_id, m.ipod_id, m.ipod_name))
        .collect();
    t.sort();
    p.sort();
    (t, p)
}

#[test]
fn restore_puts_back_the_database_and_the_mappings_and_drops_what_is_gone() {
    let w = world_with_old();
    let db_path = w.root.join("iPod_Control/iTunes/iTunesDB");
    let db_before = fs::read(&db_path).unwrap();
    let snap_before = w.snapshot();
    let maps_before = mapping_view(&w);
    change_everything(&w);
    let out = w.sync().unwrap();
    assert_ne!(fs::read(&db_path).unwrap(), db_before);
    assert_ne!(mapping_view(&w), maps_before);

    let r = restore(&w, &out.backup.stamp).unwrap();
    assert_eq!(r.files, vec!["iTunesDB"]);
    assert_eq!(fs::read(&db_path).unwrap(), db_before);
    assert_eq!(w.snapshot(), snap_before);
    // Old's playlist row is gone from the index, so its iPod playlist,
    // back on the iPod, is mapped to a tombstone for the next sync to
    // delete; Mix is now called Mixed in the index, but the row is the same.
    assert_eq!(
        (r.tracks, r.dropped_tracks, r.playlists_to_delete),
        (3, 0, 1)
    );
    let (t, p) = mapping_view(&w);
    assert_eq!(t, maps_before.0);
    assert_eq!(p.len(), 3);
    assert_eq!(r.playlists, 2);
    let names: Vec<&str> = p.iter().map(|m| m.2.as_str()).collect();
    assert!(
        names.contains(&"Road") && names.contains(&"Mix") && names.contains(&"Old"),
        "{names:?}"
    );
    // Backups stay.
    assert!(w.backups().iter().any(|b| b.stamp == out.backup.stamp));
}

#[test]
fn restore_never_puts_back_play_counts() {
    let w = world_with_old();
    w.write_play_counts(&[(3, 1_700_000_000, 0, 0), (0, 0, 0, 0), (0, 0, 0, 0)]);
    w.write_list("Road", &[D2]);
    w.reload();
    let out = w.sync().unwrap();
    assert!(
        out.backup.dir.join("Play Counts").is_file(),
        "kept in the backup"
    );
    assert!(!w.play_counts().exists());
    restore(&w, &out.backup.stamp).unwrap();
    assert!(
        !w.play_counts().exists(),
        "not restored: its plays were pulled"
    );
}

#[test]
fn restoring_an_unknown_stamp_changes_nothing() {
    let w = world_with_old();
    w.write_list("Road", &[D2]);
    w.reload();
    w.sync().unwrap();
    let tree_before = w.ipod_tree();
    let maps = mapping_view(&w);
    let e = restore(&w, "19990101-000000").unwrap_err();
    assert!(
        matches!(e, Error::NoSuchBackup(ref s) if s == "19990101-000000"),
        "{e}"
    );
    assert_eq!(w.ipod_tree(), tree_before);
    assert_eq!(mapping_view(&w), maps);
}

#[test]
fn backup_guid_matches_the_folder_without_regard_to_case_or_prefix() {
    let w = world_with_old();
    w.write_list("Road", &[D2]);
    w.reload();
    w.sync().unwrap();
    let find = |g: Option<&str>| exec::backup_guid(&w.paths, g);
    for g in [BARE_GUID, GUID, "000a27001234abcd", " 0X000a27001234abcd "] {
        assert_eq!(find(Some(g)).unwrap(), GUID, "{g}");
    }
    // Without a GUID the iPod cannot be told from another one, even when
    // only one iPod has backups.
    assert!(matches!(find(None), Err(Error::RestoreNeedsGuid)));
    assert!(matches!(
        find(Some("000A270000000000")),
        Err(Error::NoSuchBackup(_))
    ));
    // A hidden folder does not count, and a second iPod's folder does not
    // confuse the lookup by GUID.
    let root = w.paths.data_dir.join("backups");
    fs::create_dir(root.join(".partial-x")).unwrap();
    assert_eq!(find(Some(GUID)).unwrap(), GUID);
    fs::create_dir(root.join("OTHER")).unwrap();
    assert!(matches!(find(None), Err(Error::RestoreNeedsGuid)));
    assert_eq!(find(Some("0xother")).unwrap(), "OTHER");
    assert_eq!(find(Some(GUID)).unwrap(), GUID);
}

#[test]
fn backup_guid_without_any_backup_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_file: dir.path().join("c.toml"),
        data_dir: dir.path().join("data"),
    };
    assert!(matches!(
        exec::backup_guid(&paths, None),
        Err(Error::RestoreNeedsGuid)
    ));
    assert!(matches!(
        exec::backup_guid(&paths, Some(GUID)),
        Err(Error::NoSuchBackup(_))
    ));
}

#[test]
fn the_guid_read_without_the_database_names_the_backup_folder() {
    let w = world_with_old();
    let bare = device::firewire_guid(&w.root);
    assert_eq!(bare.as_deref(), Some(BARE_GUID));
    assert!(
        matches!(
            exec::backup_guid(&w.paths, bare.as_deref()),
            Err(Error::NoSuchBackup(_))
        ),
        "no backups yet"
    );
}

// ---- the iPod lock

fn sync_with_lock(w: &World, lock: &iprune_core::lock::IpodLock) -> iprune_core::Result<Outcome> {
    let prepared = w.prepare();
    let work = exec::playlist_work(&prepared.plan, &prepared.tracks_map, &prepared.ipod);
    exec::sync_playlists(
        w.env.store.conn(),
        &w.env.cfg,
        &w.paths,
        lock,
        &w.root,
        w.open(),
        &prepared,
        &work,
        Options::default(),
    )
}

#[test]
fn a_sync_with_the_lock_of_another_data_folder_is_refused_and_changes_nothing() {
    let w = world_with_old();
    // A play on the iPod, which a sync would pull before writing.
    w.write_play_counts(&[(3, 1_700_000_000, 0, 0), (0, 0, 0, 0), (0, 0, 0, 0)]);
    change_everything(&w);
    let tree_before = w.ipod_tree();
    let snap_before = w.snapshot();
    let stats_before = w.stats_rows();

    let other = tempfile::tempdir().unwrap();
    let elsewhere = Paths {
        config_file: other.path().join("config.toml"),
        data_dir: other.path().join("data"),
    };
    let wrong = iprune_core::lock::ipod(&elsewhere).unwrap();
    let e = sync_with_lock(&w, &wrong).unwrap_err();
    assert!(matches!(e, Error::NotWritable(_)), "{e}");
    assert_eq!(w.ipod_tree(), tree_before);
    assert_eq!(w.snapshot(), snap_before);
    assert!(w.backups().is_empty());
    assert!(!w.paths.data_dir.join("backups").exists());
    assert_eq!(w.pending_pulls(), 0);
    assert_eq!(w.stats_rows(), stats_before);
    assert!(w.play_counts().exists());
}

// The caller holds the lock for the whole sync, so the sync must not take it
// again: a second attempt in the same process would be refused.
#[test]
fn a_sync_given_the_held_lock_of_its_data_folder_goes_through() {
    let w = world_with_old();
    change_everything(&w);
    let snap_before = w.snapshot();
    let lock = iprune_core::lock::ipod(&w.paths).unwrap();
    sync_with_lock(&w, &lock).unwrap();
    assert_ne!(w.snapshot(), snap_before);
    assert_eq!(w.backups().len(), 1);
    // Still held afterwards, by the caller.
    assert!(matches!(
        iprune_core::lock::ipod(&w.paths),
        Err(Error::IpodBusy)
    ));
}

#[test]
fn a_restore_while_the_ipod_lock_is_held_is_refused_and_changes_nothing() {
    let w = world_with_old();
    let db_before = fs::read(w.root.join("iPod_Control/iTunes/iTunesDB")).unwrap();
    change_everything(&w);
    let out = w.sync().unwrap();
    let tree_after_sync = w.ipod_tree();
    let snap_after_sync = w.snapshot();
    let maps = mapping_view(&w);

    let lock = iprune_core::lock::ipod(&w.paths).unwrap();
    let e = restore(&w, &out.backup.stamp).unwrap_err();
    assert!(matches!(e, Error::IpodBusy), "{e}");
    assert_eq!(w.ipod_tree(), tree_after_sync);
    assert_eq!(w.snapshot(), snap_after_sync);
    assert_eq!(mapping_view(&w), maps);

    drop(lock);
    restore(&w, &out.backup.stamp).unwrap();
    assert_eq!(
        fs::read(w.root.join("iPod_Control/iTunes/iTunesDB")).unwrap(),
        db_before
    );
}

// ---- restore: what it keeps, refuses and marks

fn db_path(w: &World) -> PathBuf {
    w.root.join("iPod_Control/iTunes/iTunesDB")
}

fn restoring(w: &World) -> Option<String> {
    devices::restoring(w.env.store.conn(), w.device()).unwrap()
}

/// Road reduced to one track and synced: the backup of that sync holds the
/// iPod as `world_with_old` made it.
fn synced_once() -> (World, String) {
    let w = world_with_old();
    w.write_list("Road", &[D2]);
    w.reload();
    let out = w.sync().unwrap();
    (w, out.backup.stamp)
}

fn prepare_result(w: &World) -> iprune_core::Result<Prepared> {
    sync::prepare(w.env.store.conn(), &w.env.cfg, &w.open())
}

// Prevents (audit M2) a restore throwing away the database it replaces.
#[test]
fn restore_keeps_a_copy_of_the_database_it_replaces_with_the_mappings_and_a_snapshot() {
    let (w, stamp) = synced_once();
    let after_sync = fs::read(db_path(&w)).unwrap();
    let snap_after_sync = w.snapshot();
    let maps_after_sync = mapping_view(&w);

    let r = restore_with(&w, &stamp, false).unwrap();

    assert_ne!(r.saved.stamp, stamp);
    assert_eq!(
        fs::read(r.saved.dir.join("iTunesDB")).unwrap(),
        after_sync,
        "the replaced database"
    );
    assert_eq!(
        verify::normalize(&fs::read_to_string(r.saved.dir.join("snapshot.txt")).unwrap()),
        snap_after_sync
    );
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(r.saved.dir.join("mappings.json")).unwrap()).unwrap();
    assert_eq!(
        saved["tracks"].as_array().unwrap().len(),
        maps_after_sync.0.len()
    );
    assert_eq!(
        saved["playlists"].as_array().unwrap().len(),
        maps_after_sync.1.len()
    );
    // It is a backup like the others: listed, and it can be restored.
    assert!(w.backups().iter().any(|b| b.stamp == r.saved.stamp));
    assert_ne!(w.snapshot(), snap_after_sync, "the restore did replace it");
    restore_with(&w, &r.saved.stamp, false).unwrap();
    assert_eq!(w.snapshot(), snap_after_sync);
    assert_eq!(mapping_view(&w), maps_after_sync);
}

// Prevents (audit M2) a restore of a backup that cannot be read replacing a
// working database with a broken one.
#[test]
fn restoring_a_backup_that_cannot_be_read_is_refused_and_changes_nothing() {
    type Damage = fn(&backup::Backup);
    let damages: [(&str, Damage); 4] = [
        ("garbage iTunesDB", |b| {
            fs::write(b.dir.join("iTunesDB"), b"not an iTunesDB").unwrap()
        }),
        ("empty iTunesDB", |b| {
            fs::write(b.dir.join("iTunesDB"), b"").unwrap()
        }),
        ("garbage mappings.json", |b| {
            fs::write(b.dir.join("mappings.json"), b"{").unwrap()
        }),
        ("no mappings.json", |b| {
            fs::remove_file(b.dir.join("mappings.json")).unwrap()
        }),
    ];
    for (what, damage) in damages {
        let (w, stamp) = synced_once();
        let bad = w.backups().into_iter().find(|b| b.stamp == stamp).unwrap();
        damage(&bad);
        let tree_before = w.ipod_tree();
        let maps = mapping_view(&w);
        let backups = tree(&w.paths.data_dir.join("backups"));

        let e = restore_with(&w, &stamp, false).expect_err(what);
        assert!(
            matches!(&e, Error::BadBackup { stamp: s, .. } if *s == stamp),
            "{what}: {e:?}"
        );
        assert_eq!(w.ipod_tree(), tree_before, "{what}: the iPod");
        assert_eq!(mapping_view(&w), maps, "{what}: the mappings");
        assert_eq!(
            tree(&w.paths.data_dir.join("backups")),
            backups,
            "{what}: no backup of the iPod was taken"
        );
        assert_eq!(restoring(&w), None, "{what}: nothing was started");
        assert!(prepare_result(&w).is_ok(), "{what}: a sync is not refused");
    }
}

// Prevents (audit H7) a deleted playlist's row id being given to a new
// playlist, which a restored mapping would then attach to the old iPod list.
// With the id never reused, the old list is recognised as the removed one and
// the next sync deletes it, leaving the new playlist its own list.
#[test]
fn a_new_playlist_does_not_inherit_the_ipod_list_of_a_deleted_one_with_its_row_id() {
    // Zold is the newest row, so its id is the highest.
    let w = World::new(&[("Zold", vec![D1])]);
    let conn = w.env.store.conn();
    let zold_id = playlist_id(&w, "Zold").unwrap();
    let max: i64 = conn
        .query_row("SELECT MAX(id) FROM playlists", [], |r| r.get(0))
        .unwrap();
    assert_eq!(zold_id, max, "set up: Zold holds the highest row id");
    let (zold_ipod, zold_members) = w.ipod_playlist("Zold").unwrap();
    assert_eq!(zold_members, vec![D1]);

    // The backup of this sync still maps zold_id to Zold's iPod list.
    playlists::delete(conn, &w.env.cfg, "Zold").unwrap();
    w.reload();
    let out = w.sync().unwrap();
    assert!(w.ipod_playlist("Zold").is_none());
    assert_eq!(playlist_id(&w, "Zold"), None, "tombstone dropped");

    w.write_list("Fresh", &[D3]);
    w.reload();
    let fresh_id = playlist_id(&w, "Fresh").unwrap();
    assert_ne!(fresh_id, zold_id, "a freed id is not handed out again");

    let r = restore_with(&w, &out.backup.stamp, false).unwrap();
    assert_eq!(r.playlists_to_delete, 1, "Zold's row is gone");
    assert_eq!(w.ipod_playlist("Zold"), Some((zold_ipod, vec![D1])));
    let maps = mapping_view(&w).1;
    assert!(
        maps.iter()
            .all(|m| m.0 != fresh_id || m.1 != Some(zold_ipod)),
        "Fresh must not be mapped to Zold's list: {maps:?}"
    );

    w.sync().unwrap();
    assert_eq!(w.ipod_playlist("Zold"), None, "the removed list is deleted");
    let fresh = w.ipod_playlist("Fresh").expect("Fresh was created");
    assert_ne!(fresh.0, zold_ipod);
    assert_eq!(fresh.1, vec![D3]);
}

fn playlist_id(w: &World, name: &str) -> Option<i64> {
    w.env
        .store
        .conn()
        .query_row("SELECT id FROM playlists WHERE name = ?1", [name], |r| {
            r.get(0)
        })
        .ok()
}

// Prevents (audit L78) a playlist the library no longer has coming back from
// a restore and staying on the iPod for good, unmapped.
#[test]
fn a_playlist_the_library_dropped_since_the_backup_is_deleted_by_the_next_sync() {
    let w = world_with_old();
    let conn = w.env.store.conn();
    playlists::delete(conn, &w.env.cfg, "Old").unwrap();
    w.reload();
    let out = w.sync().unwrap();
    assert!(w.ipod_playlist("Old").is_none());
    let road = w.ipod_playlist("Road").unwrap();
    let mix = w.ipod_playlist("Mix").unwrap();
    let stray = w.ipod_playlist("Stray").unwrap();

    let r = restore_with(&w, &out.backup.stamp, false).unwrap();
    assert_eq!(r.playlists_to_delete, 1);
    let (old_ipod, old_members) = w.ipod_playlist("Old").expect("back on the iPod");
    assert_eq!(old_members, vec![D1]);
    // Mapped to a row the library has marked as removed.
    let removed = iprune_core::store::removed_playlists(w.env.store.conn()).unwrap();
    assert_eq!(removed.len(), 1, "{removed:?}");
    let maps = mapping_view(&w).1;
    let m = maps.iter().find(|m| m.1 == Some(old_ipod)).expect("mapped");
    assert_eq!(m.0, removed[0].0);

    let p = w.prepare();
    assert_eq!(p.plan.playlists.len(), 1, "{:?}", p.plan.playlists);
    w.sync().unwrap();
    assert_eq!(w.ipod_playlist("Old"), None);
    assert!(
        iprune_core::store::removed_playlists(w.env.store.conn())
            .unwrap()
            .is_empty(),
        "the tombstone is dropped once deleted"
    );
    // The other lists are as they were.
    assert_eq!(w.ipod_playlist("Road"), Some(road));
    assert_eq!(w.ipod_playlist("Mix"), Some(mix));
    assert_eq!(w.ipod_playlist("Stray"), Some(stray));
    assert!(w.prepare().plan.playlists.is_empty());
}

// The restored list and a library playlist of the same name, written after
// the backup: the list is deleted, the library playlist gets its own.
#[test]
fn a_restored_list_whose_name_a_new_library_playlist_took_is_still_deleted() {
    let w = world_with_old();
    let conn = w.env.store.conn();
    playlists::delete(conn, &w.env.cfg, "Old").unwrap();
    w.reload();
    let out = w.sync().unwrap();
    w.write_list("Old", &[D3, D2]);
    w.reload();

    let r = restore_with(&w, &out.backup.stamp, false).unwrap();
    assert_eq!(r.playlists_to_delete, 1);
    let (restored_id, _) = w.ipod_playlist("Old").unwrap();

    w.sync().unwrap();
    let lists: Vec<(u64, Vec<u64>)> = w
        .contents()
        .playlists
        .into_iter()
        .filter(|p| p.name == "Old")
        .map(|p| (p.id, p.members))
        .collect();
    assert_eq!(lists.len(), 1, "exactly one list of that name: {lists:?}");
    assert_ne!(lists[0].0, restored_id, "the restored one is gone");
    assert_eq!(lists[0].1, vec![D3, D2]);
}

// Prevents (audit M1) a restore onto an iPod whose GUID cannot be read taking
// the only backup folder, which may belong to another iPod.
#[test]
fn a_restore_onto_an_ipod_without_a_readable_guid_is_refused_and_changes_nothing() {
    let (w, stamp) = synced_once();
    // Another iPod: no GUID in SysInfo, no SysInfoExtended.
    let b_dir = tempfile::tempdir().unwrap();
    let b_tracks = [IpodSpec {
        dbid: 0x0B0B,
        size: 1234,
        ms: 1000,
        title: "OnlyOnB",
        artist: "ArtistB",
        album: "",
        plays: 7,
    }];
    let b_root = fake_ipod(
        b_dir.path(),
        None,
        &b_tracks,
        &[ipod_playlist("B list", &[0x0B0B])],
    );
    let b_before = tree(&b_root);
    let maps = mapping_view(&w);
    let backups = tree(&w.paths.data_dir.join("backups"));
    assert_eq!(device::firewire_guid(&b_root), None, "set up");

    // What `iprune restore` does with no --guid.
    let e = exec::backup_guid(&w.paths, device::firewire_guid(&b_root).as_deref()).unwrap_err();
    assert!(matches!(e, Error::RestoreNeedsGuid), "{e:?}");

    assert_eq!(tree(&b_root), b_before, "iPod B is untouched");
    assert_eq!(mapping_view(&w), maps);
    assert_eq!(tree(&w.paths.data_dir.join("backups")), backups);
    // A named folder is accepted (the user's choice).
    assert_eq!(exec::backup_guid(&w.paths, Some(GUID)).unwrap(), GUID);
    let _ = stamp;
}

// ---- restore: the unfinished marker

/// Ways to make a restore fail after it has begun changing the iPod. Each
/// returns what to remove to let the next try go through.
fn obstruct_rename(w: &World) -> PathBuf {
    // The artwork file cannot be replaced: a folder is in its place.
    let art = w.root.join("iPod_Control/Artwork/ArtworkDB");
    fs::remove_file(&art).unwrap();
    fs::create_dir(&art).unwrap();
    fs::write(art.join("inside"), "x").unwrap();
    art
}

fn obstruct_copy(w: &World) -> PathBuf {
    // The iTunesDB is staged under this name; a folder is there already.
    let tmp = w.root.join("iPod_Control/iTunes/.iprune-restore-iTunesDB");
    fs::create_dir(&tmp).unwrap();
    tmp
}

// Prevents (audit M11) a restore stopped part way leaving a database that no
// longer matches the mappings, with a sync free to run on it.
#[test]
fn a_restore_that_stops_part_way_blocks_sync_until_it_is_run_again() {
    type Obstruct = fn(&World) -> PathBuf;
    let cases: [(&str, Obstruct); 2] = [("rename", obstruct_rename), ("copy", obstruct_copy)];
    for (what, obstruct) in cases {
        let w = world_with_old();
        // An ArtworkDB, so the backup holds one and the restore renames two files.
        let art = w.root.join("iPod_Control/Artwork/ArtworkDB");
        fs::create_dir_all(art.parent().unwrap()).unwrap();
        fs::write(&art, "art").unwrap();
        // Old is dropped, so the mappings differ before and after the restore.
        playlists::delete(w.env.store.conn(), &w.env.cfg, "Old").unwrap();
        w.reload();
        let out = w.sync().unwrap();
        let stamp = out.backup.stamp;
        let maps_before_restore = mapping_view(&w);
        let db_after_sync = fs::read(db_path(&w)).unwrap();
        // Plays waiting: set aside before the restore stops.
        w.write_play_counts(&[(1, 1_700_000_000, 0, 0), (0, 0, 0, 0), (0, 0, 0, 0)]);

        let blocker = obstruct(&w);
        let e = restore_with(&w, &stamp, false).unwrap_err();
        assert!(!matches!(e, Error::Verification { .. }), "{what}: {e:?}");

        assert_eq!(
            fs::read(db_path(&w)).unwrap(),
            db_after_sync,
            "{what}: the old iTunesDB is whole"
        );
        assert!(!w.play_counts().exists(), "{what}: the restore had begun");
        assert_eq!(restoring(&w).as_deref(), Some(&stamp[..]), "{what}");
        assert_eq!(
            mapping_view(&w),
            maps_before_restore,
            "{what}: the mappings are replaced last"
        );
        let refused = prepare_result(&w).err();
        assert!(
            matches!(&refused, Some(Error::RestoreUnfinished(s)) if *s == stamp),
            "{what}: {refused:?}"
        );

        // Run again, with the obstacle gone.
        if blocker.is_dir() {
            fs::remove_dir_all(&blocker).unwrap();
        } else {
            fs::remove_file(&blocker).unwrap();
        }
        let r = restore_with(&w, &stamp, false).unwrap();
        assert_eq!(restoring(&w), None, "{what}: the mark is cleared");
        assert!(prepare_result(&w).is_ok(), "{what}: sync is free again");
        assert_eq!(
            fs::read(db_path(&w)).unwrap(),
            fs::read(r.saved.dir.parent().unwrap().join(&stamp).join("iTunesDB")).unwrap()
        );
        assert_ne!(fs::read(db_path(&w)).unwrap(), db_after_sync);
        assert_eq!(
            mapping_view(&w).1.len(),
            maps_before_restore.1.len() + 1,
            "{what}: Old's mapping is back (as a removed list)"
        );
        assert_eq!(fs::read_to_string(&art).unwrap(), "art", "{what}");
    }
}

// ---- restore: Play Counts when the database cannot be read

/// `synced_once` and then a database that no longer parses, with a
/// `Play Counts` of `counts` bytes when given.
fn damaged(counts: Option<&[u8]>) -> (World, String) {
    let (w, stamp) = synced_once();
    fs::write(db_path(&w), b"damaged").unwrap();
    if let Some(c) = counts {
        fs::write(w.play_counts(), c).unwrap();
    }
    assert!(gpod::Database::open(&w.root).is_err(), "set up");
    (w, stamp)
}

// Prevents (audit H2) discarding plays that cannot be pulled without being
// asked to.
#[test]
fn an_unreadable_database_with_plays_waiting_is_refused_unless_they_may_be_discarded() {
    let counts = b"mhdp-not-really-but-not-empty";
    let (w, stamp) = damaged(Some(counts));
    let tree_before = w.ipod_tree();
    let maps = mapping_view(&w);
    let backups = tree(&w.paths.data_dir.join("backups"));

    let e = restore_with(&w, &stamp, false).unwrap_err();
    assert!(matches!(e, Error::PlayCountsUnread), "{e:?}");
    assert_eq!(w.ipod_tree(), tree_before, "nothing changed on the iPod");
    assert_eq!(mapping_view(&w), maps);
    assert_eq!(tree(&w.paths.data_dir.join("backups")), backups);
    assert_eq!(restoring(&w), None);

    let r = restore_with(&w, &stamp, true).unwrap();
    assert_eq!(r.set_aside, vec!["Play Counts"]);
    assert_eq!(r.pulled.tracks(), 0);
    assert!(!w.play_counts().exists());
    // Kept in the backup of what the restore replaced, with the database.
    assert_eq!(fs::read(r.saved.dir.join("Play Counts")).unwrap(), counts);
    assert_eq!(fs::read(r.saved.dir.join("iTunesDB")).unwrap(), b"damaged");
    assert!(!r.saved.dir.join("snapshot.txt").exists(), "none to take");
    assert!(w.open().track_count() > 0, "the iPod reads again");
    assert_eq!(restoring(&w), None);
}

// An empty Play Counts holds no plays, and with none the damage alone does
// not stop a restore, which exists to repair it.
#[test]
fn an_unreadable_database_with_no_plays_waiting_restores_without_the_flag() {
    for counts in [None, Some(&b""[..])] {
        let (w, stamp) = damaged(counts);
        let r = restore_with(&w, &stamp, false).unwrap();
        assert!(w.open().track_count() > 0, "{counts:?}");
        assert!(!w.play_counts().exists(), "{counts:?}");
        assert_eq!(
            r.set_aside.len(),
            usize::from(counts.is_some()),
            "{counts:?}"
        );
        assert_eq!(restoring(&w), None);
    }
}

// ---- restore: the read-back

// Prevents (audit M2) a restore that does not read back as the backup
// passing for done. A leftover `iTunesCDB` makes libgpod read that file
// instead of the restored `iTunesDB`.
#[test]
fn a_restore_whose_result_cannot_be_read_back_says_so_and_stays_marked() {
    let (w, stamp) = synced_once();
    fs::write(
        w.root.join("iPod_Control/iTunes/iTunesCDB"),
        b"not a database",
    )
    .unwrap();
    assert!(gpod::Database::open(&w.root).is_err(), "set up");

    let e = restore_with(&w, &stamp, false).unwrap_err();
    let Error::RestoreVerification {
        problems,
        stamp: s,
        saved,
    } = &e
    else {
        panic!("{e:?}");
    };
    assert_eq!(s, &stamp);
    assert!(!problems.is_empty());
    assert!(e.is_check_failure(), "exit 3");
    // The message names both ways out.
    let text = e.to_string();
    assert!(
        text.contains(&stamp) && text.contains(saved.as_str()),
        "{text}"
    );
    // The pre-restore copy exists, and the mark stays.
    assert!(w.backups().iter().any(|b| b.stamp == *saved));
    assert_eq!(restoring(&w).as_deref(), Some(&stamp[..]));
    // (A sync is refused for the mark, see the test of a stopped restore.)
}
