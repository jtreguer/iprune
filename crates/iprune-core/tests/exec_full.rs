//! `exec::sync` end to end with tracks: copies, removals, new audio, tag
//! updates, checkpoints and interruption, on a fake iPod (a temp folder whose
//! iTunesDB is written by libgpod through `make_ipod.c`, with the F00 to F49
//! folders and a SysInfo holding a FirewireGuid). The iPod, the index, the
//! music folder and the data folder are all in temp dirs; no real iPod or
//! library is touched.

mod common;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use common::{
    Env, IpodSpec, Kind, PlaylistSpec, fairplay_m4a, fake_ipod, fixture, fixture_secs, id3v24_text,
    ipod_playlist, mp3_with_id3v2,
};
use iprune_core::config::Paths;
use iprune_core::exec::{self, FullWork, Options, Outcome, Progress};
use iprune_core::store::devices;
use iprune_core::sync::{self, Prepared};
use iprune_core::{Error, adoption, backup, ipod, playlists, verify};

const SYSINFO: &str = "FirewireGuid: 0x000A27001234ABCD\n";
const GUID: &str = "0x000A27001234ABCD";
/// An iPod track no library file matches, in no playlist but the master.
const ORPHAN: u64 = 0x9999;

fn dbid(i: usize) -> u64 {
    0x1000 + i as u64
}

pub struct World {
    pub env: Env,
    /// The library files of the tracks that start on the iPod, in order; the
    /// iPod's track `i` (1-based) has dbid `dbid(i)`.
    pub files: Vec<PathBuf>,
    _ipod_dir: tempfile::TempDir,
    _data_dir: tempfile::TempDir,
    pub root: PathBuf,
    pub paths: Paths,
}

impl World {
    /// `n` library files `s1.mp3` to `sN.mp3` (Mp3 fixtures 1 to N) that are
    /// also on the iPod with their files, an unmanaged iPod track, the
    /// playlists Road (all N) and Mix (the last), the unmanaged "Stray" [2], a
    /// smart "Top" [1] and Podcasts. Track 1 has the album "Kept Album" on the
    /// iPod and none in the library. Adopted.
    pub fn new(n: usize) -> World {
        assert!((2..=40).contains(&n));
        let mut env = Env::new();
        let files: Vec<PathBuf> = (1..=n)
            .map(|i| env.add(&format!("s{i}.mp3"), Kind::Mp3, i as u32))
            .collect();
        env.scan();
        let ipod_dir = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let mut tracks: Vec<IpodSpec> = files
            .iter()
            .enumerate()
            .map(|(k, f)| {
                let t = env.track(f);
                IpodSpec {
                    dbid: dbid(k + 1),
                    size: t.size,
                    ms: t.info.duration_ms.unwrap(),
                    title: Box::leak(format!("Title{}", k + 1).into_boxed_str()),
                    artist: Box::leak(format!("Artist{}", k + 1).into_boxed_str()),
                    album: if k == 0 { "Kept Album" } else { "" },
                    plays: 0,
                }
            })
            .collect();
        tracks.push(IpodSpec {
            dbid: ORPHAN,
            size: 1234,
            ms: 5000,
            title: "Orphan",
            artist: "Nobody",
            album: "",
            plays: 0,
        });
        let all: Vec<u64> = (1..=n).map(dbid).collect();
        let lists = vec![
            ipod_playlist("Road", &all),
            ipod_playlist("Mix", &[dbid(n)]),
            PlaylistSpec {
                name: "Top",
                smart: true,
                podcasts: false,
                members: vec![dbid(1)],
            },
            PlaylistSpec {
                name: "Podcasts",
                smart: false,
                podcasts: true,
                members: vec![],
            },
            ipod_playlist("Stray", &[dbid(2)]),
        ];
        let root = fake_ipod(ipod_dir.path(), Some(SYSINFO), &tracks, &lists);
        // make_ipod.c names the k-th track F(k mod 50)/T(k).mp3 without
        // creating the file: put the audio there.
        for (k, f) in files.iter().enumerate() {
            let dest = root.join(format!("iPod_Control/Music/F{:02}/T{k:04}.mp3", k % 50));
            fs::copy(f, dest).unwrap();
        }
        let orphan = root.join(format!("iPod_Control/Music/F{:02}/T{n:04}.mp3", n % 50));
        fs::write(orphan, b"orphan audio").unwrap();
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
        let all_files = w.files.clone();
        w.write_list("Road", &all_files);
        w.write_list("Mix", &[w.files[n - 1].clone()]);
        w.reload();
        let conn = w.env.store.conn();
        let db = w.open();
        let run = adoption::run(conn, &db, false, &|_, _| {}).unwrap();
        assert_eq!(run.mapping.len(), n, "clean adoption: {:?}", run.left);
        adoption::save(conn, &run, &ipod::read(&db).firewire_guid.unwrap(), false).unwrap();
        drop(db);
        w
    }

    pub fn write_list(&self, name: &str, files: &[PathBuf]) {
        let text: String = files.iter().map(|f| format!("{}\n", f.display())).collect();
        self.env.write_playlist(name, &text);
    }

    pub fn reload(&self) {
        playlists::sync_from_disk(self.env.store.conn(), &self.env.cfg, true).unwrap();
    }

    /// Scans the music folder and reloads the playlist files.
    pub fn rescan(&mut self) {
        self.env.scan();
        self.reload();
    }

    /// Adds a library file (an Mp3 fixture unless it is something else) and
    /// rescans. Returns its path.
    pub fn add_new(&mut self, rel: &str, kind: Kind, variant: u32) -> PathBuf {
        let p = self.env.add(rel, kind, variant);
        self.rescan();
        p
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

    pub fn run(
        &self,
        p: &Prepared,
        work: &FullWork,
        opts: Options,
        progress: &dyn Fn(Progress),
        cancel: &AtomicBool,
    ) -> iprune_core::Result<Outcome> {
        exec::sync(
            self.env.store.conn(),
            &self.env.cfg,
            &self.paths,
            &iprune_core::lock::ipod(&self.paths).unwrap(),
            &self.root,
            self.open(),
            p,
            work,
            opts,
            progress,
            cancel,
        )
    }

    /// Plans and applies everything.
    pub fn sync(&self) -> iprune_core::Result<Outcome> {
        let p = self.prepare();
        let work = exec::full_work(&p);
        self.run(
            &p,
            &work,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
    }

    pub fn snapshot(&self) -> String {
        let mut out = Vec::new();
        gpod::snapshot::write(&self.open(), &mut out).unwrap();
        verify::normalize(&String::from_utf8(out).unwrap())
    }

    pub fn playlist(&self, name: &str) -> Option<Vec<u64>> {
        self.contents()
            .playlists
            .into_iter()
            .find(|p| p.name == name)
            .map(|p| p.members)
    }

    pub fn track(&self, dbid: u64) -> Option<ipod::Track> {
        self.contents()
            .tracks
            .into_iter()
            .find(|t| t.adopt.dbid == dbid)
    }

    /// The file of an iPod track, whether or not it exists.
    pub fn file_of(&self, dbid: u64) -> PathBuf {
        let t = self
            .track(dbid)
            .unwrap_or_else(|| panic!("no track {dbid:x}"));
        ipod::file_path(&self.root, t.ipod_path.as_deref().expect("a path")).unwrap()
    }

    pub fn device(&self) -> i64 {
        devices::device_id(self.env.store.conn(), GUID)
            .unwrap()
            .expect("adopted")
    }

    /// Mapped library track id to iPod dbid.
    pub fn mapping(&self) -> BTreeMap<i64, u64> {
        devices::track_mappings(self.env.store.conn(), self.device())
            .unwrap()
            .into_iter()
            .map(|m| (m.track_id, m.dbid))
            .collect()
    }

    pub fn tree(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        tree(&self.root)
    }

    pub fn backups(&self) -> Vec<backup::Backup> {
        backup::list(&self.paths.data_dir.join("backups"), GUID).unwrap()
    }

    pub fn id_of(&self, file: &Path) -> i64 {
        self.env.track(file).id
    }

    /// Plays on the iPod's `Play Counts`: one `(plays, last played, rating,
    /// skips)` per track in database order.
    pub fn write_play_counts(&self, entries: &[(u32, i64, u32, u32)]) {
        let mut b = b"mhdp".to_vec();
        b.extend(96u32.to_le_bytes());
        b.extend(28u32.to_le_bytes());
        b.extend((entries.len() as u32).to_le_bytes());
        b.resize(96, 0);
        for (plays, last, rating, skips) in entries {
            b.extend(plays.to_le_bytes());
            b.extend(((*last + 2_082_844_800) as u32).to_le_bytes());
            b.extend(0u32.to_le_bytes());
            b.extend(rating.to_le_bytes());
            b.extend(0u32.to_le_bytes());
            b.extend(skips.to_le_bytes());
            b.extend(0u32.to_le_bytes());
        }
        fs::write(self.root.join("iPod_Control/iTunes/Play Counts"), b).unwrap();
    }

    pub fn stats_total_plays(&self) -> i64 {
        self.env
            .store
            .conn()
            .query_row("SELECT COALESCE(SUM(plays), 0) FROM stats", [], |r| {
                r.get(0)
            })
            .unwrap()
    }

    pub fn pending_pulls(&self) -> i64 {
        self.env
            .store
            .conn()
            .query_row("SELECT COUNT(*) FROM pending_pulls", [], |r| r.get(0))
            .unwrap()
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

/// A rewrite of an Mp3 fixture with its tags replaced, same audio.
fn retagged_mp3(
    variant: u32,
    title: &str,
    artist: &str,
    album: Option<&str>,
    year: Option<&str>,
) -> Vec<u8> {
    let mut frames = vec![
        id3v24_text("TIT2", 3, title.as_bytes()),
        id3v24_text("TPE1", 3, artist.as_bytes()),
    ];
    if let Some(a) = album {
        frames.push(id3v24_text("TALB", 3, a.as_bytes()));
    }
    if let Some(y) = year {
        frames.push(id3v24_text("TDRC", 3, y.as_bytes()));
    }
    mp3_with_id3v2(variant, &frames)
}

fn count(events: &[Progress], f: fn(&Progress) -> bool) -> usize {
    events.iter().filter(|e| f(e)).count()
}

// ---- copies

#[test]
fn new_library_files_are_copied_with_their_tags_and_playlists_include_them() {
    let mut w = World::new(3);
    let mp3 = w.add_new("new1.mp3", Kind::Mp3, 21);
    let m4a = w.add_new("new2.m4a", Kind::Aac, 22);
    let before = w.contents();
    let mut road = w.files.clone();
    road.insert(1, mp3.clone());
    road.push(m4a.clone());
    w.write_list("Road", &road);
    w.write_list("Fresh", &[m4a.clone(), mp3.clone()]);
    w.reload();

    let p = w.prepare();
    let work = exec::full_work(&p);
    assert_eq!(work.tracks.copies.len(), 2);
    let dbids: Vec<u64> = work.tracks.copies.iter().map(|c| c.dbid).collect();
    let events = RefCell::new(Vec::new());
    let out = w
        .run(
            &p,
            &work,
            Options::default(),
            &|e| events.borrow_mut().push(e),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(
        (out.copied, out.recopied, out.removed, out.retagged),
        (2, 0, 0, 0)
    );
    assert_eq!(out.stopped, None);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let sizes: u64 = [&mp3, &m4a]
        .iter()
        .map(|f| fs::metadata(f).unwrap().len())
        .sum();
    assert_eq!(out.bytes, sizes);

    // The tracks are on the iPod, last in the master playlist and the track
    // list, in plan order.
    let after = w.contents();
    assert_eq!(after.tracks.len(), before.tracks.len() + 2);
    assert_eq!(
        after.master_members[..before.master_members.len()],
        before.master_members[..]
    );
    assert_eq!(
        after.master_members[before.master_members.len()..],
        dbids[..]
    );
    // Their files exist with the library's bytes, and the tags are the file's.
    for (c, src, title, artist, filetype) in [
        (
            &work.tracks.copies[0],
            &mp3,
            "Title21",
            "Artist21",
            "MPEG audio file",
        ),
        (
            &work.tracks.copies[1],
            &m4a,
            "Title22",
            "Artist22",
            "AAC audio file",
        ),
    ] {
        let t = w.track(c.dbid).expect("on the iPod");
        let file = w.file_of(c.dbid);
        assert_eq!(
            fs::read(&file).unwrap(),
            fs::read(src).unwrap(),
            "{}",
            file.display()
        );
        assert_eq!(t.tags.title.as_deref(), Some(title));
        assert_eq!(t.tags.artist.as_deref(), Some(artist));
        assert_eq!(t.adopt.size, fs::metadata(src).unwrap().len());
        assert_eq!(
            u64::from(t.adopt.duration_ms),
            u64::from(w.env.track(src).info.duration_ms.unwrap())
        );
        let raw = w.open();
        let r = raw.track_by_dbid(c.dbid).unwrap();
        assert_eq!(r.filetype().as_deref(), Some(filetype));
        assert!(r.time_added() > 1_700_000_000, "{}", r.time_added());
        assert_eq!(r.samplerate(), 44_100);
    }
    // Playlists: the new tracks where the files put them.
    let (n1, n2) = (dbids[0], dbids[1]);
    assert_eq!(
        w.playlist("Road"),
        Some(vec![dbid(1), n1, dbid(2), dbid(3), n2])
    );
    assert_eq!(w.playlist("Fresh"), Some(vec![n2, n1]));
    assert_eq!(w.playlist("Mix"), Some(vec![dbid(3)]));
    assert_eq!(
        w.playlist("Stray"),
        Some(vec![dbid(2)]),
        "unmanaged, untouched"
    );
    // Mappings record the new tracks with what was synced.
    let m = devices::track_mappings(w.env.store.conn(), w.device()).unwrap();
    for (c, src) in [
        (&work.tracks.copies[0], &mp3),
        (&work.tracks.copies[1], &m4a),
    ] {
        let row = w.env.track(src);
        let got = m.iter().find(|m| m.track_id == row.id).expect("mapped");
        assert_eq!(got.dbid, c.dbid);
        assert_eq!(got.synced_size, row.size);
        assert_eq!(got.synced_tags_hash, row.tags_hash);
        assert_eq!(got.synced_duration_ms, row.info.duration_ms);
    }
    // Progress: the final copy count, one write and one verification.
    let events = events.into_inner();
    assert!(matches!(
        events
            .iter()
            .rev()
            .find(|e| matches!(e, Progress::Copying { .. })),
        Some(Progress::Copying {
            done: 2,
            total: 2,
            ..
        })
    ));
    assert_eq!(count(&events, |e| matches!(e, Progress::Writing)), 1);
    assert_eq!(count(&events, |e| matches!(e, Progress::Verifying)), 1);
}

// Prevents: a second sync repeating work (new copies, new dbids, doubled
// playlists) after tracks were copied.
#[test]
fn syncing_again_after_copies_gives_an_empty_plan_and_an_unchanged_snapshot() {
    let mut w = World::new(3);
    let new = w.add_new("new1.mp3", Kind::Mp3, 21);
    let mut road = w.files.clone();
    road.push(new);
    w.write_list("Road", &road);
    w.reload();
    w.sync().unwrap();
    let (snap, tree) = (w.snapshot(), w.tree());

    let p = w.prepare();
    assert!(
        p.plan.copy.is_empty() && p.plan.replace.is_empty(),
        "{:?}",
        p.plan
    );
    assert!(p.plan.update_tags.is_empty() && p.plan.remove.is_empty());
    assert!(p.plan.playlists.is_empty(), "{:?}", p.plan.playlists);
    let work = exec::full_work(&p);
    assert!(work.is_empty(), "{work:?}");
    let out = w
        .run(
            &p,
            &work,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(
        (out.copied, out.removed, out.retagged, out.applied),
        (0, 0, 0, 0)
    );
    assert_eq!(w.snapshot(), snap);
    // No file appeared or went (only the databases and the manifest may be rewritten).
    let files = |t: &BTreeMap<PathBuf, Vec<u8>>| -> Vec<PathBuf> {
        t.keys()
            .filter(|k| k.starts_with("iPod_Control/Music"))
            .cloned()
            .collect()
    };
    assert_eq!(files(&w.tree()), files(&tree));
}

#[test]
fn a_copy_that_would_not_fit_is_refused_before_anything_is_touched() {
    let mut w = World::new(3);
    w.add_new("new1.mp3", Kind::Mp3, 21);
    w.write_play_counts(&[
        (3, 1_700_000_000, 80, 1),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
    ]);
    let mut p = w.prepare();
    assert!(!p.plan.copy.is_empty());
    p.plan.space.fits = false;
    let work = exec::full_work(&p);
    let tree_before = w.tree();
    let e = w
        .run(
            &p,
            &work,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert!(matches!(e, Error::NoSpace), "{e}");
    assert_eq!(w.tree(), tree_before);
    assert!(w.backups().is_empty(), "no backup taken");
    assert_eq!(w.stats_total_plays(), 0, "no stats pulled");
    assert_eq!(w.pending_pulls(), 0);
}

#[test]
fn a_plan_that_does_not_fit_but_copies_nothing_is_still_applied() {
    let mut w = World::new(3);
    // Library file 3 gets a new title: a retag, no copy.
    w.env
        .add_bytes("s3.mp3", &retagged_mp3(3, "Renamed", "Artist3", None, None));
    w.rescan();
    let mut p = w.prepare();
    p.plan.space.fits = false;
    let work = exec::full_work(&p);
    assert!(work.tracks.copies.is_empty() && !work.tracks.retags.is_empty());
    let out = w
        .run(
            &p,
            &work,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(out.retagged, 1);
    assert_eq!(
        w.track(dbid(3)).unwrap().tags.title.as_deref(),
        Some("Renamed")
    );
}

// ---- removals

#[test]
fn a_deleted_library_file_removes_its_track_its_file_and_its_mapping() {
    let mut w = World::new(4);
    let gone = w.files[1].clone(); // track 2, also in the unmanaged Stray
    let gone_id = w.id_of(&gone);
    let gone_file = w.file_of(dbid(2));
    assert!(
        gone_file.is_file(),
        "set up: the track's file is on the iPod"
    );
    let kept_files: Vec<PathBuf> = [1, 3, 4].iter().map(|&i| w.file_of(dbid(i))).collect();
    fs::remove_file(&gone).unwrap();
    w.rescan();

    let p = w.prepare();
    assert_eq!(p.plan.remove.len(), 1);
    let out = w.sync().unwrap();
    assert_eq!((out.removed, out.copied, out.retagged), (1, 0, 0));
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    let c = w.contents();
    assert!(
        c.tracks.iter().all(|t| t.adopt.dbid != dbid(2)),
        "gone from the database"
    );
    assert!(!c.master_members.contains(&dbid(2)));
    assert_eq!(w.playlist("Road"), Some(vec![dbid(1), dbid(3), dbid(4)]));
    assert_eq!(
        w.playlist("Stray"),
        Some(vec![]),
        "unmanaged playlists lose it too"
    );
    assert_eq!(w.playlist("Mix"), Some(vec![dbid(4)]));
    assert!(
        !gone_file.exists(),
        "the file is deleted once the write is verified"
    );
    for f in kept_files {
        assert!(f.is_file(), "{} must survive", f.display());
    }
    assert!(w.track(ORPHAN).is_some(), "the unmanaged track stays");
    assert!(!w.mapping().contains_key(&gone_id), "mapping dropped");
    assert_eq!(w.mapping().len(), 3);
    // The old database still pointed at the file when it was backed up.
    assert!(out.backup.dir.join("iTunesDB").is_file());
    // And nothing is left to do.
    let p = w.prepare();
    assert!(exec::full_work(&p).is_empty(), "{:?}", p.plan);
}

#[test]
fn removing_a_track_whose_file_is_already_gone_is_not_a_warning() {
    let mut w = World::new(3);
    fs::remove_file(w.file_of(dbid(3))).unwrap();
    fs::remove_file(&w.files[2]).unwrap();
    w.rescan();
    let out = w.sync().unwrap();
    assert_eq!(out.removed, 1);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(w.track(dbid(3)).is_none());
}

#[test]
fn a_file_that_cannot_be_deleted_after_the_write_is_a_warning_and_the_sync_stands() {
    let mut w = World::new(3);
    let file = w.file_of(dbid(3));
    // A folder where the file was: remove_file fails.
    fs::remove_file(&file).unwrap();
    fs::create_dir(&file).unwrap();
    fs::remove_file(&w.files[2]).unwrap();
    w.rescan();
    let out = w.sync().unwrap();
    assert_eq!(out.removed, 1);
    assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
    assert!(out.warnings[0].contains("deleting"), "{:?}", out.warnings);
    assert!(w.track(dbid(3)).is_none(), "the database change stands");
    assert!(w.mapping().len() == 2);
}

#[test]
fn removing_many_managed_tracks_is_refused_unless_allowed() {
    let mut w = World::new(8);
    for i in 0..7 {
        fs::remove_file(&w.files[i]).unwrap();
    }
    w.rescan();
    w.write_play_counts(&[(3, 1_700_000_000, 80, 1)]);
    let tree_before = w.tree();
    let e = w.sync().unwrap_err();
    assert!(
        matches!(
            e,
            Error::MassTrackRemoval {
                removes: 7,
                managed: 8
            }
        ),
        "{e}"
    );
    assert_eq!(w.tree(), tree_before, "the iPod is untouched");
    assert!(w.backups().is_empty());
    assert_eq!(w.stats_total_plays(), 0);
    assert_eq!(w.mapping().len(), 8);

    let p = w.prepare();
    let work = exec::full_work(&p);
    let out = w
        .run(
            &p,
            &work,
            Options {
                allow_mass_removal: true,
            },
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(out.removed, 7);
    assert_eq!(w.mapping().len(), 1);
    assert_eq!(
        w.contents().tracks.len(),
        2,
        "the last track and the unmanaged one"
    );
}

#[test]
fn a_few_removals_pass_the_guard_even_when_they_are_most_of_the_tracks() {
    // Five removals of six: not over five, so no refusal.
    let mut w = World::new(6);
    for i in 0..5 {
        fs::remove_file(&w.files[i]).unwrap();
    }
    w.rescan();
    let out = w.sync().unwrap();
    assert_eq!(out.removed, 5);
}

#[test]
fn removing_unmanaged_tracks_on_request_is_not_held_by_the_managed_guard() {
    let mut w = World::new(3);
    w.env.cfg.sync.remove_unmanaged = true;
    let orphan_file = w.file_of(ORPHAN);
    let out = w.sync().unwrap();
    assert_eq!(out.removed, 1);
    assert!(w.track(ORPHAN).is_none());
    assert!(!orphan_file.exists());
    assert_eq!(w.mapping().len(), 3, "managed mappings are untouched");
}

// ---- new audio

#[test]
fn changed_audio_is_copied_into_the_same_track_and_keeps_its_place_in_every_playlist() {
    let mut w = World::new(3);
    let src = w.files[2].clone(); // track 3, in Road and Mix
    let id = w.id_of(&src);
    let old_file = w.file_of(dbid(3));
    let old_bytes = fs::read(&old_file).unwrap();
    // Same tags, three seconds instead of one.
    fs::copy(fixture_secs(Kind::Mp3, 3, 3), &src).unwrap();
    w.rescan();
    let new_bytes = fs::read(&src).unwrap();
    assert_ne!(old_bytes, new_bytes);
    let p = w.prepare();
    assert_eq!(p.plan.replace.len(), 1, "{:?}", p.plan);
    let road_before = w.playlist("Road").unwrap();
    let stray_before = w.playlist("Stray").unwrap();
    let count_before = w.contents().tracks.len();

    let out = w.sync().unwrap();
    assert_eq!((out.copied, out.recopied, out.removed), (0, 1, 0));
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    // Same dbid, same playlists, same number of tracks.
    let c = w.contents();
    assert_eq!(c.tracks.len(), count_before);
    assert_eq!(w.playlist("Road").unwrap(), road_before);
    assert_eq!(w.playlist("Stray").unwrap(), stray_before);
    assert_eq!(w.playlist("Mix"), Some(vec![dbid(3)]));
    assert_eq!(w.mapping()[&id], dbid(3));
    // The track now describes the new audio...
    let t = w.track(dbid(3)).unwrap();
    assert_eq!(t.adopt.size, new_bytes.len() as u64);
    assert!(t.adopt.duration_ms > 2500, "{}", t.adopt.duration_ms);
    // ... and its file exists with the new bytes. Whatever the old file was
    // called, the one the track points at must not be deleted.
    let file = w.file_of(dbid(3));
    assert!(
        file.is_file(),
        "the track's file {} is gone",
        file.display()
    );
    assert_eq!(fs::read(&file).unwrap(), new_bytes);
    // The mapping records the new audio, so the next plan is empty.
    let m = devices::track_mappings(w.env.store.conn(), w.device()).unwrap();
    let m = m.iter().find(|m| m.track_id == id).unwrap();
    assert_eq!(m.synced_size, new_bytes.len() as u64);
    assert!(exec::full_work(&w.prepare()).is_empty());
    // If the old file has a different name it is gone.
    if file != old_file {
        assert!(!old_file.exists(), "the old file is deleted");
    }
}

// Prevents a recopy writing onto the track's old file: the track must point at
// a new file, which the sync's verification also demands.
#[test]
fn changed_audio_goes_to_a_new_file_and_the_old_one_is_deleted() {
    let mut w = World::new(3);
    let src = w.files[2].clone();
    let old_file = w.file_of(dbid(3));
    fs::copy(fixture_secs(Kind::Mp3, 3, 3), &src).unwrap();
    w.rescan();
    let out = w.sync().unwrap();
    assert_eq!(out.recopied, 1);
    let file = w.file_of(dbid(3));
    assert_ne!(file, old_file, "the track still uses its old file");
    assert!(file.is_file());
    assert!(!old_file.exists(), "the old file is deleted");
}

// ---- tags

#[test]
fn a_tag_edit_sets_only_the_changed_fields_and_never_clears_from_an_empty_tag() {
    let mut w = World::new(3);
    // Track 1: new title and year; its library file has no album while the
    // iPod has "Kept Album". Track 2 is edited only in its artist.
    w.env.add_bytes(
        "s1.mp3",
        &retagged_mp3(1, "Renamed", "Artist1", None, Some("2001")),
    );
    w.env.add_bytes(
        "s2.mp3",
        &retagged_mp3(2, "Title2", "Other Artist", None, None),
    );
    w.rescan();
    let size1 = w.track(dbid(1)).unwrap().adopt.size;
    let file1 = w.file_of(dbid(1));
    let p = w.prepare();
    assert_eq!(p.plan.update_tags.len(), 2, "{:?}", p.plan.update_tags);

    let out = w.sync().unwrap();
    assert_eq!(
        (out.retagged, out.copied, out.recopied, out.removed),
        (2, 0, 0, 0)
    );
    let t1 = w.track(dbid(1)).unwrap();
    assert_eq!(t1.tags.title.as_deref(), Some("Renamed"));
    assert_eq!(t1.tags.year, Some(2001));
    assert_eq!(t1.tags.artist.as_deref(), Some("Artist1"));
    assert_eq!(
        t1.tags.album.as_deref(),
        Some("Kept Album"),
        "an empty tag does not clear"
    );
    assert_eq!(
        (t1.adopt.size, w.file_of(dbid(1))),
        (size1, file1),
        "audio untouched"
    );
    let t2 = w.track(dbid(2)).unwrap();
    assert_eq!(t2.tags.artist.as_deref(), Some("Other Artist"));
    assert_eq!(t2.tags.title.as_deref(), Some("Title2"));
    assert_eq!(
        w.track(dbid(3)).unwrap().tags.title.as_deref(),
        Some("Title3")
    );
    // The mappings carry the new tag hash; the next plan has nothing.
    let id1 = w.id_of(&w.files[0]);
    let m = devices::track_mappings(w.env.store.conn(), w.device()).unwrap();
    let m1 = m.iter().find(|m| m.track_id == id1).unwrap();
    assert_eq!(m1.synced_tags_hash, w.env.track(&w.files[0]).tags_hash);
    assert!(exec::full_work(&w.prepare()).is_empty());
    // Playlists did not move.
    assert_eq!(w.playlist("Road"), Some(vec![dbid(1), dbid(2), dbid(3)]));
}

#[test]
fn a_tag_edit_keeps_what_the_ipod_learned_about_the_track() {
    let mut w = World::new(3);
    w.write_play_counts(&[
        (4, 1_700_000_000, 80, 2),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
    ]);
    w.env
        .add_bytes("s1.mp3", &retagged_mp3(1, "Renamed", "Artist1", None, None));
    w.rescan();
    let out = w.sync().unwrap();
    assert_eq!(out.pulled.plays(), 4);
    let raw = w.open();
    let t = raw.track_by_dbid(dbid(1)).unwrap();
    assert_eq!(t.title().as_deref(), Some("Renamed"));
    assert_eq!(t.rating(), 80, "rating kept");
    assert_eq!(t.playcount(), 4, "plays kept");
    assert_eq!(w.stats_total_plays(), 4);
}

// ---- FairPlay

#[test]
fn fairplay_files_are_skipped_not_copied_and_do_not_break_playlists() {
    let mut w = World::new(3);
    let drm = w.add_new("bought.m4a", Kind::Aac, 31);
    fs::write(&drm, fairplay_m4a(31)).unwrap();
    w.rescan();
    assert!(w.env.track(&drm).drm, "set up: scanned as protected");
    let mut road = w.files.clone();
    road.push(drm.clone());
    w.write_list("Road", &road);
    w.reload();
    let p = w.prepare();
    assert_eq!(p.plan.skipped.len(), 1);
    let before = w.contents();
    let out = w.sync().unwrap();
    assert_eq!(out.copied, 0);
    assert_eq!(
        w.contents().tracks.len(),
        before.tracks.len(),
        "nothing added"
    );
    assert!(!w.mapping().contains_key(&w.id_of(&drm)));
    assert_eq!(w.playlist("Road"), Some(vec![dbid(1), dbid(2), dbid(3)]));
    assert!(
        !w.tree().keys().any(|k| k.starts_with("iPod_Control/Music")
            && fs::read(w.root.join(k)).is_ok_and(|b| b == fairplay_m4a(31))),
        "no copy of the protected file anywhere on the iPod"
    );
}

// Prevents a multi-value tag (NUL separated, as ID3v2.4 stores them) from
// failing the whole sync, or from reaching the iPod cut at the first value.
// The tag reader (lofty) keeps only the first value of an ID3v2.4 file, so
// no fixture file yields a NUL; the library row is given one directly, as
// an index built by an older reader could hold.
#[test]
fn a_library_track_whose_tags_hold_a_nul_is_copied_with_the_values_joined_by_a_slash() {
    let mut w = World::new(3);
    let f = w.env.add("multi.mp3", Kind::Mp3, 41);
    w.rescan();
    let id = w.id_of(&f);
    w.env
        .store
        .conn()
        .execute(
            "UPDATE tracks SET title = ?1, artist = ?2, album = ?3 WHERE id = ?4",
            rusqlite::params!["Song\0", "A\0B", "\0", id],
        )
        .unwrap();
    // Set up: the row reads back with the NULs (else this tests nothing).
    let row = w.env.track(&f);
    assert_eq!(row.tags.artist.as_deref(), Some("A\0B"), "{:?}", row.tags);
    assert_eq!(row.tags.title.as_deref(), Some("Song\0"));

    let p = w.prepare();
    assert_eq!(p.plan.copy.len(), 1);
    let before = w.contents().tracks.len();
    let out = w.sync().unwrap();
    assert_eq!((out.copied, out.stopped.clone()), (1, None));
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let after = w.contents();
    assert_eq!(after.tracks.len(), before + 1);
    let t = after
        .tracks
        .iter()
        .find(|t| t.tags.artist.as_deref() == Some("A/B"))
        .expect("the copy is on the iPod with the joined artist");
    assert_eq!(t.tags.title.as_deref(), Some("Song"));
    assert_eq!(t.tags.album, None, "a value of only NULs is no album");
    assert!(w.file_of(t.adopt.dbid).is_file());
    // Nothing left to do: the joined values are what the library says.
    assert!(exec::full_work(&w.prepare()).is_empty());
}

// A Copy built by hand with a NUL in its fields (the planner cannot make
// one) stops the sync cleanly: no file copied, the database still verifies.
#[test]
fn a_copy_whose_fields_hold_a_nul_stops_cleanly_before_any_file_is_copied() {
    let mut w = World::new(3);
    w.env.add("n.mp3", Kind::Mp3, 40);
    w.rescan();
    let p = w.prepare();
    let mut work = exec::full_work(&p);
    assert_eq!(work.tracks.copies.len(), 1);
    work.tracks.copies[0].fields.artist = Some("A\0B".into());
    let bad_dbid = work.tracks.copies[0].dbid;
    let files_before = ipod_music_files(&w.root);
    let before = w.snapshot();
    let mapping_before = w.mapping();

    let out = w
        .run(
            &p,
            &work,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .expect("a refused copy is a stop, not a failure");
    assert_eq!(out.copied, 0);
    let why = out.stopped.expect("stopped by the refusal");
    assert!(!why.is_empty());
    assert_eq!(ipod_music_files(&w.root), files_before, "no new file");
    assert!(w.track(bad_dbid).is_none(), "no track added");
    assert_eq!(w.mapping(), mapping_before);
    // The database on the iPod parses and is what it was.
    assert_eq!(w.snapshot(), before);
}

// The replace path is where the check matters: copying the new audio comes
// before setting the fields, so without the check a refusal would leave a
// new file behind and the track on its old one.
#[test]
fn a_replace_whose_fields_hold_a_nul_changes_neither_the_track_nor_its_files() {
    let mut w = World::new(3);
    let src = w.files[2].clone();
    fs::copy(fixture_secs(Kind::Mp3, 3, 3), &src).unwrap();
    w.rescan();
    let p = w.prepare();
    let mut work = exec::full_work(&p);
    assert_eq!(work.tracks.copies.len(), 1);
    assert!(work.tracks.copies[0].replace);
    work.tracks.copies[0].fields.title = Some("T\0x".into());
    let old_file = w.file_of(dbid(3));
    let old_bytes = fs::read(&old_file).unwrap();
    let files_before = ipod_music_files(&w.root);
    let before = w.snapshot();
    let mapping_before = w.mapping();

    let out = w
        .run(
            &p,
            &work,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .expect("a refused copy is a stop, not a failure");
    assert_eq!((out.copied, out.recopied), (0, 0));
    assert!(out.stopped.is_some());
    assert_eq!(ipod_music_files(&w.root), files_before, "no new file");
    assert_eq!(w.file_of(dbid(3)), old_file);
    assert_eq!(fs::read(&old_file).unwrap(), old_bytes);
    assert_eq!(w.mapping(), mapping_before);
    assert_eq!(w.snapshot(), before);
}

/// Every file under the iPod's Music folder.
fn ipod_music_files(root: &Path) -> Vec<PathBuf> {
    tree(&root.join("iPod_Control/Music")).into_keys().collect()
}

// ---- interruption

/// Library with 3 new files added, with a new playlist holding them and
/// Road extended; returns the new files in order.
fn with_three_new(w: &mut World) -> Vec<PathBuf> {
    let news: Vec<PathBuf> = (0..3)
        .map(|k| w.env.add(&format!("n{k}.mp3"), Kind::Mp3, 40 + k))
        .collect();
    let mut road = w.files.clone();
    road.extend(news.clone());
    w.write_list("Road", &road);
    w.write_list("Fresh", &news);
    w.rescan();
    news
}

#[test]
fn a_stop_before_the_copies_writes_the_removals_and_leaves_the_rest_for_next_time() {
    let mut w = World::new(4);
    let news = with_three_new(&mut w);
    // One removal besides the copies, a tag edit and the playlist changes.
    fs::remove_file(&w.files[3]).unwrap();
    w.env
        .add_bytes("s1.mp3", &retagged_mp3(1, "Renamed", "Artist1", None, None));
    w.rescan();
    let road_before = w.playlist("Road").unwrap();
    let p = w.prepare();
    let work = exec::full_work(&p);
    assert_eq!(
        (
            work.tracks.copies.len(),
            work.tracks.removes.len(),
            work.tracks.retags.len()
        ),
        (3, 1, 1)
    );

    let cancel = AtomicBool::new(true);
    let out = w
        .run(&p, &work, Options::default(), &|_| {}, &cancel)
        .unwrap();
    assert!(out.stopped.is_some());
    assert_eq!(
        (out.copied, out.removed, out.retagged, out.applied),
        (0, 1, 0, 0)
    );
    // The removal is written and verified; nothing else happened.
    assert!(w.track(dbid(4)).is_none());
    assert_eq!(
        w.track(dbid(1)).unwrap().tags.title.as_deref(),
        Some("Title1"),
        "no retag yet"
    );
    assert_eq!(w.playlist("Fresh"), None);
    // Road only lost the removed track.
    let expected: Vec<u64> = road_before
        .iter()
        .copied()
        .filter(|d| *d != dbid(4))
        .filter(|d| *d < 0x2000)
        .collect();
    assert_eq!(w.playlist("Road"), Some(expected));
    assert_eq!(w.mapping().len(), 3, "the removed one is no longer mapped");
    assert_eq!(w.backups().len(), 1);

    // The next sync does the rest, in one go.
    let out = w.sync().unwrap();
    assert_eq!((out.copied, out.removed, out.retagged), (3, 0, 1));
    assert_eq!(out.stopped, None);
    assert_eq!(w.playlist("Fresh").unwrap().len(), 3);
    assert_eq!(w.playlist("Road").unwrap().len(), 6);
    assert_eq!(
        w.track(dbid(1)).unwrap().tags.title.as_deref(),
        Some("Renamed")
    );
    assert_eq!(w.mapping().len(), 6);
    for n in &news {
        assert!(w.mapping().contains_key(&w.id_of(n)));
    }
    assert!(exec::full_work(&w.prepare()).is_empty());
}

#[test]
fn a_stop_during_the_copies_keeps_the_done_copies_and_leaves_playlists_alone() {
    let mut w = World::new(3);
    let news = with_three_new(&mut w);
    w.write_play_counts(&[
        (3, 1_700_000_000, 80, 1),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
    ]);
    let p = w.prepare();
    let work = exec::full_work(&p);
    let planned: Vec<u64> = work.tracks.copies.iter().map(|c| c.dbid).collect();
    // Copies run in the plan's order, not the playlist file's.
    let order: Vec<PathBuf> = work.tracks.copies.iter().map(|c| c.path.clone()).collect();
    let road_before = w.playlist("Road").unwrap();
    let cancel = AtomicBool::new(false);
    let events = RefCell::new(Vec::new());
    // Stop while the second file is starting: it and the first are copied.
    let progress = |e: Progress| {
        if matches!(e, Progress::Copying { done: 1, .. }) {
            cancel.store(true, Ordering::Relaxed);
        }
        events.borrow_mut().push(e);
    };
    let out = w
        .run(&p, &work, Options::default(), &progress, &cancel)
        .unwrap();
    assert_eq!(out.stopped.as_deref(), Some("interrupted"));
    assert_eq!(out.copied, 2);
    assert_eq!(out.applied, 0, "no playlist operation applied");

    // The two done copies are on the iPod with their files and mapped.
    let c = w.contents();
    for (d, src) in planned[..2].iter().zip(&order) {
        assert!(w.track(*d).is_some());
        assert_eq!(fs::read(w.file_of(*d)).unwrap(), fs::read(src).unwrap());
        assert_eq!(w.mapping()[&w.id_of(src)], *d);
    }
    assert!(w.track(planned[2]).is_none(), "the third was not copied");
    assert!(!w.mapping().contains_key(&w.id_of(&order[2])));
    assert_eq!(c.master_members[c.master_members.len() - 2..], planned[..2]);
    // Playlists as they were.
    assert_eq!(w.playlist("Road"), Some(road_before));
    assert_eq!(w.playlist("Fresh"), None);
    // The pull is settled by that write and counted once.
    assert_eq!(w.stats_total_plays(), 3);
    assert_eq!(w.pending_pulls(), 0);
    let events = events.into_inner();
    assert_eq!(count(&events, |e| matches!(e, Progress::Writing)), 1);
    assert!(matches!(
        events
            .iter()
            .rev()
            .find(|e| matches!(e, Progress::Copying { .. })),
        Some(Progress::Copying {
            done: 2,
            total: 3,
            ..
        })
    ));

    // The next sync copies the one left, with the dbid planned for it, and
    // completes the playlists. The plays are not counted again.
    let p2 = w.prepare();
    let work2 = exec::full_work(&p2);
    assert_eq!(work2.tracks.copies.len(), 1);
    assert_eq!(work2.tracks.copies[0].dbid, planned[2]);
    let out = w
        .run(
            &p2,
            &work2,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!((out.copied, out.stopped), (1, None));
    assert_eq!(out.pulled.plays(), 0);
    assert_eq!(w.stats_total_plays(), 3);
    let by_file = |f: &PathBuf| planned[order.iter().position(|o| o == f).unwrap()];
    assert_eq!(
        w.playlist("Fresh"),
        Some(news.iter().map(by_file).collect::<Vec<u64>>())
    );
    assert_eq!(w.playlist("Road").unwrap().len(), 6);
    assert!(exec::full_work(&w.prepare()).is_empty());
}

/// Makes the iPod's music folders unwritable and puts the permissions back
/// when dropped, so the temp dir can be cleaned up even if a test panics.
struct NoRoom(Vec<PathBuf>);

impl NoRoom {
    /// `None` when permissions do not stop us (running as root).
    fn new(root: &Path) -> Option<NoRoom> {
        use std::os::unix::fs::PermissionsExt;
        let dirs: Vec<PathBuf> = fs::read_dir(root.join("iPod_Control/Music"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        for d in &dirs {
            fs::set_permissions(d, fs::Permissions::from_mode(0o555)).unwrap();
        }
        let guard = NoRoom(dirs);
        let blocked = fs::File::create(guard.0[0].join(".probe")).is_err();
        blocked.then_some(guard)
    }
}

impl Drop for NoRoom {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        for d in &self.0 {
            let _ = fs::set_permissions(d, fs::Permissions::from_mode(0o755));
        }
    }
}

#[test]
fn a_copy_the_ipod_refuses_stops_the_sync_with_a_checkpoint_of_the_earlier_copies() {
    let mut w = World::new(3);
    let news = with_three_new(&mut w);
    let p = w.prepare();
    let work = exec::full_work(&p);
    let planned: Vec<u64> = work.tracks.copies.iter().map(|c| c.dbid).collect();
    let road_before = w.playlist("Road").unwrap();
    let guard = RefCell::new(None);
    // After the first copy the iPod stops taking files (a full or failing
    // card): the second source is readable, so this is not a skip.
    let progress = |e: Progress| {
        if matches!(e, Progress::Copying { done: 1, .. }) && guard.borrow().is_none() {
            *guard.borrow_mut() = Some(NoRoom::new(&w.root));
        }
    };
    let res = w.run(
        &p,
        &work,
        Options::default(),
        &progress,
        &AtomicBool::new(false),
    );
    let blocked = guard.into_inner().expect("reached the second copy");
    if blocked.is_none() {
        eprintln!("running as root: permissions do not block writes, test skipped");
        return;
    }
    // Let the folders be written again for the checks below.
    drop(blocked);
    let out = res.unwrap();
    let why = out.stopped.expect("stopped by the copy error");
    assert!(!why.is_empty());
    assert!(
        out.skipped.is_empty(),
        "a refusal by the iPod is not a skip"
    );
    assert_eq!(out.copied, 1);
    assert!(w.track(planned[0]).is_some());
    assert_eq!(
        fs::read(w.file_of(planned[0])).unwrap(),
        fs::read(&work.tracks.copies[0].path).unwrap()
    );
    assert!(
        w.track(planned[1]).is_none(),
        "the half-added track was taken out again"
    );
    assert!(
        w.track(planned[2]).is_none(),
        "and the next one was not tried"
    );
    assert_eq!(w.mapping().len(), 4);
    assert_eq!(w.playlist("Road"), Some(road_before), "playlists untouched");
    assert_eq!(w.contents().master_members.len(), 3 + 1 + 1);
    let _ = news;
}

// ---- files skipped one by one

#[test]
fn a_file_gone_or_changed_since_the_scan_is_skipped_and_the_other_copies_go_on() {
    let mut w = World::new(3);
    let news = with_three_new(&mut w);
    let p = w.prepare();
    let work = exec::full_work(&p);
    let order: Vec<PathBuf> = work.tracks.copies.iter().map(|c| c.path.clone()).collect();
    let dbid_of = |f: &Path| {
        work.tracks
            .copies
            .iter()
            .find(|c| c.path == f)
            .unwrap()
            .dbid
    };
    // First planned: gone. Second: copied. Third: changed size.
    let (gone, good, changed) = (order[0].clone(), order[1].clone(), order[2].clone());
    fs::remove_file(&gone).unwrap();
    let mut grown = fs::read(&changed).unwrap();
    grown.push(0);
    fs::write(&changed, grown).unwrap();
    let road_before = w.playlist("Road").unwrap();

    let out = w
        .run(
            &p,
            &work,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(out.stopped, None, "a skip is not a stop");
    assert_eq!(out.copied, 1);
    let skipped: Vec<&Path> = out.skipped.iter().map(|(p, _)| p.as_path()).collect();
    assert_eq!(skipped, [gone.as_path(), changed.as_path()]);
    assert!(out.skipped.iter().all(|(_, why)| !why.is_empty()));

    // Only the good one is on the iPod and mapped.
    assert!(w.track(dbid_of(&good)).is_some());
    assert!(w.track(dbid_of(&gone)).is_none() && w.track(dbid_of(&changed)).is_none());
    assert_eq!(w.mapping().len(), 4);
    assert!(w.mapping().contains_key(&w.id_of(&good)));
    assert!(!w.mapping().contains_key(&w.id_of(&changed)));
    // Playlists list the good one and leave out the skipped ones.
    let mut road = road_before;
    road.push(dbid_of(&good));
    assert_eq!(w.playlist("Road"), Some(road));
    assert_eq!(w.playlist("Fresh"), Some(vec![dbid_of(&good)]));
    let _ = news;

    // The next sync, once the library is rescanned, copies the changed file.
    w.rescan();
    let p2 = w.prepare();
    let work2 = exec::full_work(&p2);
    assert_eq!(work2.tracks.copies.len(), 1, "{:?}", p2.plan);
    assert_eq!(work2.tracks.copies[0].path, changed);
    let out = w
        .run(
            &p2,
            &work2,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!((out.copied, out.skipped.len()), (1, 0));
    assert!(w.mapping().contains_key(&w.id_of(&changed)));
    assert_eq!(w.playlist("Road").unwrap().len(), 3 + 2);
}

#[test]
fn an_unreadable_file_is_skipped_too() {
    use std::os::unix::fs::PermissionsExt;
    let mut w = World::new(3);
    with_three_new(&mut w);
    let p = w.prepare();
    let work = exec::full_work(&p);
    let bad = work.tracks.copies[1].path.clone();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::File::open(&bad).is_ok() {
        eprintln!("running as root: permissions do not block reads, test skipped");
        return;
    }
    let res = w.run(
        &p,
        &work,
        Options::default(),
        &|_| {},
        &AtomicBool::new(false),
    );
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o644)).unwrap();
    let out = res.unwrap();
    assert_eq!(out.stopped, None);
    assert_eq!(out.copied, 2);
    assert_eq!(out.skipped.len(), 1);
    assert_eq!(out.skipped[0].0, bad);
    assert!(w.track(work.tracks.copies[1].dbid).is_none());
}

// A skipped copy of new audio must leave the track already on the iPod as it
// is, in every playlist.
#[test]
fn a_skipped_replace_keeps_the_existing_track_in_its_playlists() {
    let mut w = World::new(3);
    let src = w.files[2].clone();
    let old_file = w.file_of(dbid(3));
    let old_bytes = fs::read(&old_file).unwrap();
    fs::copy(fixture_secs(Kind::Mp3, 3, 3), &src).unwrap();
    w.rescan();
    let p = w.prepare();
    let work = exec::full_work(&p);
    assert_eq!(work.tracks.copies.len(), 1);
    assert!(work.tracks.copies[0].replace);
    fs::remove_file(&src).unwrap();
    let road = w.playlist("Road").unwrap();
    let mix = w.playlist("Mix").unwrap();

    let out = w
        .run(
            &p,
            &work,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(out.skipped.len(), 1);
    assert_eq!(out.recopied, 0);
    assert_eq!(w.playlist("Road"), Some(road), "still holds the track");
    assert_eq!(w.playlist("Mix"), Some(mix));
    assert_eq!(w.track(dbid(3)).unwrap().adopt.size, old_bytes.len() as u64);
    assert_eq!(fs::read(&old_file).unwrap(), old_bytes);
}

// ---- order of the work

#[test]
fn removed_files_are_gone_and_written_before_the_first_copy() {
    let mut w = World::new(4);
    with_three_new(&mut w);
    let gone = w.file_of(dbid(4));
    fs::remove_file(&w.files[3]).unwrap();
    w.rescan();
    let p = w.prepare();
    let work = exec::full_work(&p);
    assert_eq!(
        (work.tracks.removes.len(), work.tracks.copies.len()),
        (1, 3)
    );
    let at_first_copy = RefCell::new(None);
    let events = RefCell::new(Vec::new());
    let progress = |e: Progress| {
        if matches!(e, Progress::Copying { done: 0, .. }) && at_first_copy.borrow().is_none() {
            // On the iPod, not in memory: the file, the database on disk and
            // the index.
            *at_first_copy.borrow_mut() = Some((
                gone.exists(),
                w.contents().tracks.iter().any(|t| t.adopt.dbid == dbid(4)),
                w.mapping().len(),
            ));
        }
        events.borrow_mut().push(e);
    };
    let out = w
        .run(
            &p,
            &work,
            Options::default(),
            &progress,
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!((out.removed, out.copied), (1, 3));
    assert_eq!(
        at_first_copy.into_inner(),
        Some((false, false, 3)),
        "(file exists, still in the database, mappings) when the first copy starts"
    );
    // A write for the removals, one at the end.
    let events = events.into_inner();
    assert_eq!(count(&events, |e| matches!(e, Progress::Writing)), 2);
}

#[test]
fn a_failing_final_check_after_a_checkpoint_says_so_and_keeps_the_checkpoint() {
    let mut w = World::new(4);
    with_three_new(&mut w);
    let gone = w.file_of(dbid(4));
    fs::remove_file(&w.files[3]).unwrap();
    w.rescan();
    let p = w.prepare();
    let work = exec::full_work(&p);
    let known: Vec<PathBuf> = w
        .tree()
        .into_keys()
        .filter(|k| k.starts_with("iPod_Control/Music"))
        .map(|k| w.root.join(k))
        .collect();
    // Just before the final write, a copied file vanishes from the iPod.
    let progress = |e: Progress| {
        if matches!(e, Progress::Copying { done: 3, total: 3, ref name, .. } if name.is_empty()) {
            for k in w.tree().into_keys().map(|k| w.root.join(k)) {
                if k.starts_with(w.root.join("iPod_Control/Music")) && !known.contains(&k) {
                    fs::remove_file(k).unwrap();
                }
            }
        }
    };
    let e = w
        .run(
            &p,
            &work,
            Options::default(),
            &progress,
            &AtomicBool::new(false),
        )
        .unwrap_err();
    match &e {
        Error::CheckAfterCheckpoint(problems) => {
            assert!(
                problems.iter().any(|m| m.contains("missing")),
                "{problems:?}"
            );
        }
        other => panic!("{other}"),
    }
    assert!(e.is_check_failure(), "exit code 3");
    // The removal checkpoint stands; the copies are not in the database.
    assert!(w.track(dbid(4)).is_none());
    assert!(!gone.exists());
    assert_eq!(w.contents().tracks.len(), 3 + 1);
    assert_eq!(w.mapping().len(), 3);
}

// ---- what a replace keeps

// Prevents: a replace giving the track a new "date added".
#[test]
fn new_audio_keeps_the_date_the_track_was_added() {
    let mut w = World::new(3);
    let src = w.files[2].clone();
    let before = w.open().track_by_dbid(dbid(3)).unwrap().time_added();
    fs::copy(fixture_secs(Kind::Mp3, 3, 3), &src).unwrap();
    w.rescan();
    w.sync().unwrap();
    assert_eq!(
        w.open().track_by_dbid(dbid(3)).unwrap().time_added(),
        before
    );
    // A new track does get today's date.
    let new = w.add_new("later.mp3", Kind::Mp3, 21);
    let p = w.prepare();
    let work = exec::full_work(&p);
    w.run(
        &p,
        &work,
        Options::default(),
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    let d = work
        .tracks
        .copies
        .iter()
        .find(|c| c.path == new)
        .unwrap()
        .dbid;
    assert!(w.open().track_by_dbid(d).unwrap().time_added() > 1_700_000_000);
}

#[test]
fn new_audio_never_clears_the_compilation_mark_the_ipod_has() {
    let mut w = World::new(3);
    // Library file 2 is marked a compilation: the iPod gets the mark.
    let marked = |variant: u32| {
        mp3_with_id3v2(
            variant,
            &[
                id3v24_text("TIT2", 3, format!("Title{variant}").as_bytes()),
                id3v24_text("TPE1", 3, format!("Artist{variant}").as_bytes()),
                id3v24_text("TCMP", 3, b"1"),
            ],
        )
    };
    w.env.add_bytes("s2.mp3", &marked(2));
    w.rescan();
    assert!(
        w.env.track(&w.files[1]).tags.compilation,
        "set up: scanned as marked"
    );
    w.sync().unwrap();
    assert!(w.track(dbid(2)).is_some());
    assert!(w.open().track_by_dbid(dbid(2)).unwrap().compilation());
    // New audio, and the new file is not marked.
    fs::copy(fixture_secs(Kind::Mp3, 2, 3), &w.files[1]).unwrap();
    w.rescan();
    let p = w.prepare();
    assert_eq!(p.plan.replace.len(), 1, "{:?}", p.plan);
    w.sync().unwrap();
    assert!(
        w.open().track_by_dbid(dbid(2)).unwrap().compilation(),
        "the replace cleared the mark"
    );
}

// ---- checkpoints

/// A world with `n` new library files and a playlist "Big" holding them.
fn with_many_new(n: usize) -> (World, Vec<PathBuf>) {
    let mut w = World::new(3);
    let src = fixture(Kind::Mp3, 50);
    let news: Vec<PathBuf> = (0..n)
        .map(|k| {
            let d = w.env.music.join(format!("many/m{k:04}.mp3"));
            fs::create_dir_all(d.parent().unwrap()).unwrap();
            fs::copy(&src, &d).unwrap();
            d
        })
        .collect();
    w.write_list("Big", &news);
    w.rescan();
    (w, news)
}

// The database is written every 500 copies, not only at the end.
#[test]
fn a_long_copy_writes_a_checkpoint_after_500_files_with_the_mappings_recorded() {
    let (w, news) = with_many_new(501);
    w.write_play_counts(&[
        (3, 1_700_000_000, 80, 1),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
    ]);
    let p = w.prepare();
    let work = exec::full_work(&p);
    assert_eq!(work.tracks.copies.len(), 501);
    let events = RefCell::new(Vec::new());
    // What the iPod and the index hold when the 501st file starts.
    let at_checkpoint = RefCell::new(None);
    let progress = |e: Progress| {
        if matches!(e, Progress::Copying { done: 500, .. }) && at_checkpoint.borrow().is_none() {
            let c = w.contents();
            let road = c
                .playlists
                .iter()
                .find(|p| p.name == "Road")
                .map(|p| p.members.len());
            let big = c.playlists.iter().any(|p| p.name == "Big");
            *at_checkpoint.borrow_mut() = Some((
                c.tracks.len(),
                road,
                big,
                w.mapping().len(),
                w.stats_total_plays(),
            ));
        }
        events.borrow_mut().push(e);
    };
    let out = w
        .run(
            &p,
            &work,
            Options::default(),
            &progress,
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!((out.copied, out.stopped), (501, None));
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    // At the checkpoint: 500 new tracks on the iPod and mapped, playlists
    // not yet changed, the plays already settled.
    let (tracks, road, big, mapped, plays) = at_checkpoint.into_inner().expect("a checkpoint");
    assert_eq!(tracks, 3 + 1 + 500);
    assert_eq!(road, Some(3));
    assert!(!big, "playlists come with the final write");
    assert_eq!(mapped, 3 + 500);
    assert_eq!(plays, 3);
    // Two writes, each verified.
    let events = events.into_inner();
    assert_eq!(count(&events, |e| matches!(e, Progress::Writing)), 2);
    assert_eq!(count(&events, |e| matches!(e, Progress::Verifying)), 2);
    // At the end everything is there, plays counted once.
    let want: Vec<u64> = news
        .iter()
        .map(|f| {
            work.tracks
                .copies
                .iter()
                .find(|c| &c.path == f)
                .unwrap()
                .dbid
        })
        .collect();
    assert_eq!(w.playlist("Big"), Some(want));
    assert_eq!(w.mapping().len(), 3 + 501);
    assert_eq!(w.stats_total_plays(), 3);
    assert_eq!(w.pending_pulls(), 0);
    assert!(news.iter().take(3).all(|f| f.is_file()));
    assert!(exec::full_work(&w.prepare()).is_empty());
}

#[test]
fn exactly_500_copies_are_one_write_not_two() {
    let (w, _) = with_many_new(500);
    let p = w.prepare();
    let work = exec::full_work(&p);
    assert_eq!(work.tracks.copies.len(), 500);
    let events = RefCell::new(Vec::new());
    let out = w
        .run(
            &p,
            &work,
            Options::default(),
            &|e| events.borrow_mut().push(e),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(out.copied, 500);
    let events = events.into_inner();
    assert_eq!(count(&events, |e| matches!(e, Progress::Writing)), 1);
    assert_eq!(w.playlist("Big").unwrap().len(), 500);
}

// ---- the backup

#[test]
fn the_backup_records_the_track_work_and_the_mappings_before_the_sync() {
    let mut w = World::new(4);
    let news = with_three_new(&mut w);
    fs::remove_file(&w.files[3]).unwrap();
    w.env
        .add_bytes("s1.mp3", &retagged_mp3(1, "Renamed", "Artist1", None, None));
    w.rescan();
    let before = w.mapping();
    let db_bytes = fs::read(w.root.join("iPod_Control/iTunes/iTunesDB")).unwrap();
    let p = w.prepare();
    let work = exec::full_work(&p);
    let out = w
        .run(
            &p,
            &work,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();

    let json = |name: &str| -> serde_json::Value {
        serde_json::from_slice(&fs::read(out.backup.dir.join(name)).unwrap()).unwrap()
    };
    let wf = json("work.json");
    let copies = wf["tracks"]["copies"].as_array().unwrap();
    assert_eq!(copies.len(), 3);
    for (c, planned) in copies.iter().zip(&work.tracks.copies) {
        assert_eq!(c["dbid"].as_u64(), Some(planned.dbid));
        assert_eq!(c["replace"], false);
        assert_eq!(c["path"].as_str(), planned.path.to_str());
        assert!(news.contains(&planned.path));
        assert_eq!(
            c["fields"]["title"]
                .as_str()
                .map(|t| t.starts_with("Title")),
            Some(true)
        );
    }
    let removes = wf["tracks"]["removes"].as_array().unwrap();
    assert_eq!(removes.len(), 1);
    assert_eq!(removes[0]["dbid"].as_u64(), Some(dbid(4)));
    let retags = wf["tracks"]["retags"].as_array().unwrap();
    assert_eq!(retags.len(), 1);
    assert_eq!(retags[0]["dbid"].as_u64(), Some(dbid(1)));
    assert_eq!(retags[0]["fields"]["title"], "Renamed");
    assert_eq!(
        wf["playlist_ops"].as_array().unwrap().len(),
        work.playlists.ops.len()
    );
    assert!(!work.playlists.ops.is_empty());
    // The restore point holds the database and the mappings as they were.
    assert_eq!(fs::read(out.backup.dir.join("iTunesDB")).unwrap(), db_bytes);
    let m = json("mappings.json");
    let saved: BTreeMap<i64, u64> = m["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["track_id"].as_i64().unwrap(), t["dbid"].as_u64().unwrap()))
        .collect();
    assert_eq!(saved, before);
    assert_ne!(w.mapping(), before);
}

#[test]
fn restoring_a_backup_after_copies_returns_the_database_and_mappings_and_leaves_the_files() {
    let mut w = World::new(3);
    let news = with_three_new(&mut w);
    let db_path = w.root.join("iPod_Control/iTunes/iTunesDB");
    let db_before = fs::read(&db_path).unwrap();
    let before = w.mapping();
    let p = w.prepare();
    let work = exec::full_work(&p);
    let out = w
        .run(
            &p,
            &work,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
    let copied: Vec<PathBuf> = work
        .tracks
        .copies
        .iter()
        .map(|c| w.file_of(c.dbid))
        .collect();
    assert_eq!(copied.len(), news.len());

    let r = exec::restore(
        w.env.store.conn(),
        &w.paths,
        &w.root,
        GUID,
        &out.backup.stamp,
        false,
    )
    .unwrap();
    assert_eq!(r.files, vec!["iTunesDB"]);
    assert_eq!(fs::read(&db_path).unwrap(), db_before);
    assert_eq!(w.mapping(), before);
    assert_eq!(w.contents().tracks.len(), 4);
    for f in copied {
        assert!(f.is_file(), "{} stays as an orphan for doctor", f.display());
    }
}

// ---- random library edits

mod prop {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 8, ..ProptestConfig::default() })]

        // Random files added and removed and playlist entries changed, one
        // sync: the iPod holds exactly the library's tracks and the playlists
        // list the right files; a second plan is empty and the snapshot does
        // not move.
        #[test]
        fn random_additions_and_removals_sync_once_and_then_settle(
            add in proptest::collection::vec(0u32..6, 0..5),
            // The fourth file stays: a scan of an empty music folder is refused.
            remove in proptest::collection::vec(any::<bool>(), 3),
            fresh_picks in proptest::collection::vec(0usize..9, 0..6),
        ) {
            let mut w = World::new(4);
            let mut library: Vec<PathBuf> = w.files.clone();
            for (k, i) in add.iter().enumerate() {
                // Distinct names; the same fixture may be added twice.
                library.push(w.env.add(&format!("extra{k}.mp3"), Kind::Mp3, 60 + i));
            }
            for (i, r) in remove.iter().enumerate() {
                if *r {
                    fs::remove_file(&w.files[i]).unwrap();
                    library.retain(|f| f != &w.files[i]);
                }
            }
            w.rescan();
            // Road lists every library file; Fresh a random selection, with repeats.
            w.write_list("Road", &library);
            let fresh: Vec<PathBuf> = fresh_picks.iter().filter_map(|i| library.get(*i).cloned()).collect();
            w.write_list("Fresh", &fresh);
            w.reload();

            let removes = remove.iter().filter(|r| **r).count();
            let opts = Options { allow_mass_removal: removes > 0 };
            let p = w.prepare();
            let work = exec::full_work(&p);
            let out = w.run(&p, &work, opts, &|_| {}, &AtomicBool::new(false)).unwrap();
            prop_assert_eq!(out.stopped, None);
            prop_assert_eq!(out.copied, add.len());
            prop_assert_eq!(out.removed, removes);

            // Each playlist entry resolves, through the mapping, to the dbid
            // of its file.
            let mapping = w.mapping();
            let dbid_of = |f: &PathBuf| mapping[&w.id_of(f)];
            let c = w.contents();
            prop_assert_eq!(c.tracks.len(), library.len() + 1, "library + the unmanaged one");
            for (name, files) in [("Road", &library), ("Fresh", &fresh)] {
                let want: Vec<u64> = files.iter().map(dbid_of).collect();
                match (w.playlist(name), want.is_empty()) {
                    (Some(got), _) => prop_assert_eq!(got, want, "{}", name),
                    (None, true) => {}
                    (None, false) => prop_assert!(false, "{} missing", name),
                }
            }
            for t in c.tracks.iter().filter(|t| t.adopt.dbid != ORPHAN) {
                let f = w.file_of(t.adopt.dbid);
                prop_assert!(f.is_file(), "{} missing", f.display());
            }

            let snap = w.snapshot();
            let tree: Vec<PathBuf> = w.tree().into_keys().filter(|k| k.starts_with("iPod_Control/Music")).collect();
            let p = w.prepare();
            prop_assert!(exec::full_work(&p).is_empty(), "{:?}", p.plan);
            let again = exec::full_work(&p);
            w.run(&p, &again, Options::default(), &|_| {}, &AtomicBool::new(false)).unwrap();
            prop_assert_eq!(w.snapshot(), snap);
            let tree2: Vec<PathBuf> = w.tree().into_keys().filter(|k| k.starts_with("iPod_Control/Music")).collect();
            prop_assert_eq!(tree2, tree);
        }
    }
}

// ---- mappings lost to a crash

// A crash between the verified write and the record leaves tracks on the
// iPod that the index has not heard of.
#[test]
fn tracks_a_crash_left_unrecorded_are_taken_back_without_copying_again() {
    let mut w = World::new(3);
    let news = with_three_new(&mut w);
    w.sync().unwrap();
    let mapped = w.mapping();
    assert_eq!(mapped.len(), 6);
    let tracks_on_ipod = w.contents().tracks.len();
    let ids: Vec<i64> = news.iter().map(|f| w.id_of(f)).collect();
    let conn = w.env.store.conn();
    for id in &ids {
        conn.execute("DELETE FROM device_tracks WHERE track_id = ?1", [id])
            .unwrap();
    }
    assert_eq!(w.mapping().len(), 3, "set up: mappings lost");

    let p = w.prepare();
    assert_eq!(p.recovered, 3);
    assert!(p.plan.copy.is_empty(), "{:?}", p.plan.copy);
    assert_eq!(w.mapping(), mapped, "recorded again, with the same dbids");
    assert!(exec::full_work(&p).is_empty(), "{:?}", p.plan);
    // Preparing again has nothing more to recover.
    assert_eq!(w.prepare().recovered, 0);
    assert_eq!(w.contents().tracks.len(), tracks_on_ipod);
}

#[test]
fn a_changed_file_is_not_mistaken_for_the_track_a_crash_left_behind() {
    let mut w = World::new(3);
    let news = with_three_new(&mut w);
    w.sync().unwrap();
    let ids: Vec<i64> = news.iter().map(|f| w.id_of(f)).collect();
    let conn = w.env.store.conn();
    for id in &ids {
        conn.execute("DELETE FROM device_tracks WHERE track_id = ?1", [id])
            .unwrap();
    }
    // One of them changed on disk since: other content, other size.
    let mut bytes = fs::read(&news[0]).unwrap();
    bytes.extend([0u8; 5]);
    fs::write(&news[0], bytes).unwrap();
    w.rescan();

    let p = w.prepare();
    assert_eq!(p.recovered, 2);
    assert_eq!(p.plan.copy.len(), 1);
    assert_eq!(p.plan.copy[0].path, news[0]);
    let work = exec::full_work(&p);
    let on_ipod: Vec<u64> = w.contents().tracks.iter().map(|t| t.adopt.dbid).collect();
    assert!(
        !on_ipod.contains(&work.tracks.copies[0].dbid),
        "a fresh dbid"
    );
}

/// A copy whose mapping a crash lost. With `edit`, the library row then
/// gets a new title. A tag edit in the file itself changes the content hash,
/// and with it the dbid recovery looks for, so the row is changed directly,
/// as a re-read of the same file with a different tag reader would.
fn copied_then_mapping_lost(edit: bool) -> (World, PathBuf) {
    let mut w = World::new(3);
    let f = w.env.add_bytes(
        "lost.mp3",
        &retagged_mp3(40, "Alpha", "Lost Artist", None, None),
    );
    w.rescan();
    w.sync().unwrap();
    let id = w.id_of(&f);
    assert!(w.mapping().contains_key(&id), "set up: copied and mapped");
    let conn = w.env.store.conn();
    conn.execute("DELETE FROM device_tracks WHERE track_id = ?1", [id])
        .unwrap();
    if edit {
        let mut tags = w.env.track(&f).tags;
        tags.title = Some("Bravo".into());
        conn.execute(
            "UPDATE tracks SET title = 'Bravo', tags_hash = ?1 WHERE id = ?2",
            rusqlite::params![&tags.hash()[..], id],
        )
        .unwrap();
        assert_eq!(w.env.track(&f).tags.title.as_deref(), Some("Bravo"));
    }
    (w, f)
}

// Prevents a recovered mapping from recording the library's current tag
// hash, which would hide tags that differ from the iPod's. A real tag edit
// changes the content hash and so is not recovered at all; the index edit
// above stands in for tags read differently from the same file.
#[test]
fn a_title_edited_after_a_crash_is_still_sent_once_the_mapping_is_recovered() {
    let (w, f) = copied_then_mapping_lost(true);
    let dbid_on_ipod = w
        .contents()
        .tracks
        .iter()
        .find(|t| t.tags.title.as_deref() == Some("Alpha"))
        .expect("the copy is on the iPod")
        .adopt
        .dbid;
    let tracks_before = w.contents().tracks.len();

    let p = w.prepare();
    assert_eq!(p.recovered, 1);
    assert!(p.plan.copy.is_empty(), "{:?}", p.plan.copy);
    assert_eq!(p.plan.update_tags.len(), 1, "{:?}", p.plan.update_tags);
    let op = &p.plan.update_tags[0];
    assert_eq!((op.track_id, op.dbid), (w.id_of(&f), dbid_on_ipod));
    assert_eq!(op.changes.len(), 1, "{:?}", op.changes);
    assert_eq!(op.changes[0].field, iprune_core::plan::Field::Title);

    let out = w.sync().unwrap();
    assert_eq!((out.retagged, out.copied), (1, 0));
    let t = w.track(dbid_on_ipod).unwrap();
    assert_eq!(t.tags.title.as_deref(), Some("Bravo"));
    assert_eq!(t.tags.artist.as_deref(), Some("Lost Artist"));
    assert_eq!(w.contents().tracks.len(), tracks_before, "no second copy");
    assert!(exec::full_work(&w.prepare()).is_empty());
}

#[test]
fn an_unedited_track_with_a_recovered_mapping_plans_nothing_and_changes_nothing() {
    let (w, f) = copied_then_mapping_lost(false);
    let before = w.snapshot();
    let p = w.prepare();
    assert_eq!(p.recovered, 1);
    assert!(p.plan.update_tags.is_empty(), "{:?}", p.plan.update_tags);
    assert!(exec::full_work(&p).is_empty(), "{:?}", p.plan);
    assert_eq!(w.snapshot(), before);
    assert!(w.mapping().contains_key(&w.id_of(&f)));
}

// ---- doctor

mod doctor_tests {
    use super::*;
    use iprune_core::doctor::{self, FORGET_AFTER_SECS, Report};
    use iprune_core::{now_unix, store};

    fn examine(w: &World) -> Report {
        let contents = w.contents();
        doctor::examine(
            w.env.store.conn(),
            &w.root,
            &contents,
            Some(w.device()),
            now_unix(),
        )
        .unwrap()
    }

    fn music(w: &World, rel: &str) -> PathBuf {
        w.root.join("iPod_Control/Music").join(rel)
    }

    /// Rewrites an iPod playlist's members with libgpod and writes the file.
    fn set_ipod_members(w: &World, name: &str, members: &[u64]) {
        let mut db = w.open();
        let id = db
            .playlists()
            .find(|p| p.name().as_deref() == Some(name))
            .unwrap()
            .id();
        db.set_playlist_members(id, members).unwrap();
        db.write().unwrap();
    }

    #[test]
    fn a_freshly_synced_ipod_is_clean() {
        let w = World::new(3);
        let r = examine(&w);
        assert_eq!(r, Report::default(), "{r:?}");
        assert!(r.is_clean());
    }

    #[test]
    fn files_no_entry_uses_are_orphans_whatever_the_case_of_their_name() {
        let w = World::new(3);
        fs::write(music(&w, "F05/LOST.mp3"), b"x").unwrap();
        fs::write(music(&w, "F49/also lost.m4a"), b"x").unwrap();
        // The entry for track 2 says F01/T0001.mp3; FAT would find it under
        // another spelling, so a differently cased name is not an orphan.
        let used = w.file_of(dbid(2));
        let recased = used.with_file_name("t0001.MP3");
        fs::rename(&used, &recased).unwrap();
        // Not files of a track folder: a folder inside one, and a file
        // straight in Music.
        fs::create_dir(music(&w, "F05/subdir")).unwrap();
        fs::write(music(&w, "stray.txt"), b"x").unwrap();

        let r = examine(&w);
        assert_eq!(
            r.orphan_files,
            vec![music(&w, "F05/LOST.mp3"), music(&w, "F49/also lost.m4a")]
        );
        assert!(!r.orphan_files.contains(&recased));
    }

    #[test]
    fn an_entry_whose_file_is_gone_is_reported_with_what_it_is() {
        let w = World::new(3);
        fs::remove_file(w.file_of(dbid(2))).unwrap();
        fs::remove_file(w.file_of(ORPHAN)).unwrap();
        let r = examine(&w);
        assert_eq!(
            r.missing_files,
            vec![
                (dbid(2), "Artist2 - Title2".to_owned()),
                (ORPHAN, "Nobody - Orphan".to_owned()),
            ]
        );
        assert!(r.orphan_files.is_empty());
        assert!(!r.is_clean());
    }

    #[test]
    fn repeated_members_are_reported_unless_the_playlist_file_repeats_them_too() {
        let w = World::new(3);
        // Managed Road repeats track 2 on the iPod, its file does not.
        set_ipod_members(&w, "Road", &[dbid(1), dbid(2), dbid(2), dbid(3)]);
        // Unmanaged Stray repeats too.
        set_ipod_members(&w, "Stray", &[dbid(2), dbid(2)]);
        let r = examine(&w);
        assert_eq!(
            r.repeated,
            vec![("Road".to_owned(), true), ("Stray".to_owned(), false)]
        );

        // The file repeats it as well: that is intended, so Road is fine.
        let f = w.files.clone();
        w.write_list(
            "Road",
            &[f[0].clone(), f[1].clone(), f[1].clone(), f[2].clone()],
        );
        w.reload();
        assert_eq!(examine(&w).repeated, vec![("Stray".to_owned(), false)]);

        // One repeat more on the iPod than the file has.
        set_ipod_members(&w, "Road", &[dbid(1), dbid(2), dbid(2), dbid(2), dbid(3)]);
        assert_eq!(
            examine(&w).repeated,
            vec![("Road".to_owned(), true), ("Stray".to_owned(), false)]
        );
    }

    #[test]
    fn a_mapping_to_a_track_the_ipod_no_longer_has_is_stale_and_fix_index_drops_only_it() {
        let w = World::new(3);
        let id2 = w.id_of(&w.files[1]);
        let mut db = w.open();
        db.remove_track(dbid(2)).unwrap();
        db.write().unwrap();
        drop(db);
        let r = examine(&w);
        assert_eq!(r.stale_mappings, vec![(id2, dbid(2))]);

        // Without a device nothing is dropped.
        doctor::fix_index(w.env.store.conn(), None, &r).unwrap();
        assert_eq!(w.mapping().len(), 3);
        doctor::fix_index(w.env.store.conn(), Some(w.device()), &r).unwrap();
        let m = w.mapping();
        assert_eq!(m.len(), 2);
        assert!(!m.contains_key(&id2));
        assert!(examine(&w).stale_mappings.is_empty());
    }

    /// A library file that is gone since `missing_since`, in a playlist and
    /// with statistics. Returns its track id.
    fn ghost(w: &mut World, missing_since: i64) -> i64 {
        let f = w.env.add("ghost.mp3", Kind::Mp3, 70);
        w.write_list("Ghosts", &[f.clone(), w.files[0].clone()]);
        w.rescan();
        let id = w.id_of(&f);
        fs::remove_file(&f).unwrap();
        // Scan only: reloading the playlist files would unresolve the entry.
        w.env.scan();
        let conn = w.env.store.conn();
        conn.execute(
            "UPDATE tracks SET missing_since = ?2 WHERE id = ?1",
            [id, missing_since],
        )
        .unwrap();
        conn.execute("INSERT INTO stats (track_id, plays) VALUES (?1, 4)", [id])
            .unwrap();
        // The entry points at the track, as it did before the file went.
        conn.execute(
            "UPDATE playlist_items SET track_id = ?1 WHERE raw_entry LIKE '%ghost.mp3'",
            [id],
        )
        .unwrap();
        id
    }

    fn items_of(w: &World, id: i64) -> (i64, i64) {
        let conn = w.env.store.conn();
        let with: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM playlist_items WHERE track_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        let all: i64 = conn
            .query_row("SELECT COUNT(*) FROM playlist_items", [], |r| r.get(0))
            .unwrap();
        (with, all)
    }

    #[test]
    fn tracks_missing_for_over_thirty_days_and_on_no_device_are_forgotten() {
        let mut w = World::new(3);
        let now = now_unix();
        let id = ghost(&mut w, now - FORGET_AFTER_SECS - 1);
        // One more, missing as long but still mapped to the iPod: kept.
        let mapped_id = w.id_of(&w.files[2]);
        w.env
            .store
            .conn()
            .execute(
                "UPDATE tracks SET missing_since = ?2 WHERE id = ?1",
                [mapped_id, now - 2 * FORGET_AFTER_SECS],
            )
            .unwrap();
        let (with, all_items) = items_of(&w, id);
        assert_eq!(with, 1, "set up: the playlist entry resolves to the ghost");

        let contents = w.contents();
        let r = doctor::examine(
            w.env.store.conn(),
            &w.root,
            &contents,
            Some(w.device()),
            now,
        )
        .unwrap();
        assert_eq!(r.forgotten.len(), 1, "{:?}", r.forgotten);
        assert_eq!(r.forgotten[0].0, id);
        assert!(r.forgotten[0].1.ends_with("ghost.mp3"));

        doctor::fix_index(w.env.store.conn(), Some(w.device()), &r).unwrap();
        let conn = w.env.store.conn();
        let rows = |sql: &str, id: i64| -> i64 { conn.query_row(sql, [id], |r| r.get(0)).unwrap() };
        assert_eq!(rows("SELECT COUNT(*) FROM tracks WHERE id = ?1", id), 0);
        assert_eq!(
            rows("SELECT COUNT(*) FROM stats WHERE track_id = ?1", id),
            0
        );
        // The entry stays in the playlist, unresolved, with its line.
        let (with, all_after) = items_of(&w, id);
        assert_eq!((with, all_after), (0, all_items));
        let raw: String = conn
            .query_row(
                "SELECT raw_entry FROM playlist_items WHERE track_id IS NULL AND raw_entry LIKE '%ghost.mp3'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(raw.ends_with("ghost.mp3"));
        // The mapped one is untouched.
        assert_eq!(
            rows("SELECT COUNT(*) FROM tracks WHERE id = ?1", mapped_id),
            1
        );
        assert_eq!(w.mapping().len(), 3);
    }

    #[test]
    fn thirty_days_exactly_is_not_yet_forgotten() {
        let mut w = World::new(3);
        let now = now_unix();
        ghost(&mut w, now - FORGET_AFTER_SECS);
        let contents = w.contents();
        let r = doctor::examine(
            w.env.store.conn(),
            &w.root,
            &contents,
            Some(w.device()),
            now,
        )
        .unwrap();
        assert!(r.forgotten.is_empty(), "{:?}", r.forgotten);
        let r = doctor::examine(
            w.env.store.conn(),
            &w.root,
            &contents,
            Some(w.device()),
            now + 1,
        )
        .unwrap();
        assert_eq!(r.forgotten.len(), 1);
    }

    #[test]
    fn check_orphans_thresholds_decide_without_touching_files() {
        let report = |n: usize| Report {
            orphan_files: (0..n)
                .map(|i| PathBuf::from(format!("/nope/{i}")))
                .collect(),
            ..Report::default()
        };
        let refused = |n, tracks, allow| {
            matches!(
                doctor::check_orphans(&report(n), tracks, allow),
                Err(Error::MassOrphans { files, tracks: t }) if files == n && t == tracks
            )
        };
        assert!(refused(6, 10, false), "over five and over a tenth");
        assert!(
            doctor::check_orphans(&report(6), 10, true).is_ok(),
            "allowed"
        );
        assert!(
            doctor::check_orphans(&report(5), 2, false).is_ok(),
            "five is not over five"
        );
        assert!(
            doctor::check_orphans(&report(6), 60, false).is_ok(),
            "a tenth exactly"
        );
        assert!(refused(7, 60, false), "just over a tenth");
        assert!(doctor::check_orphans(&report(0), 0, false).is_ok());
    }

    #[test]
    fn orphans_are_deleted_unless_there_are_many_compared_with_the_tracks() {
        let dir = tempfile::tempdir().unwrap();
        let make = |n: usize| -> Report {
            Report {
                orphan_files: (0..n)
                    .map(|i| {
                        let f = dir.path().join(format!("o{n}-{i}.mp3"));
                        fs::write(&f, b"x").unwrap();
                        f
                    })
                    .collect(),
                ..Report::default()
            }
        };
        let gone = |r: &Report| r.orphan_files.iter().filter(|f| !f.exists()).count();
        // Six orphans among ten tracks: over five and over a tenth.
        let r = make(6);
        let e = doctor::delete_orphans(&r, 10, false).unwrap_err();
        assert!(
            matches!(
                e,
                Error::MassOrphans {
                    files: 6,
                    tracks: 10
                }
            ),
            "{e}"
        );
        assert_eq!(gone(&r), 0, "nothing deleted when refused");
        // Allowed, they all go.
        assert_eq!(doctor::delete_orphans(&r, 10, true).unwrap(), 6);
        assert_eq!(gone(&r), 6);
        // Six among a hundred: under a tenth.
        let r = make(7);
        assert_eq!(doctor::delete_orphans(&r, 100, false).unwrap(), 7);
        // Five among two: not over five.
        let r = make(5);
        assert_eq!(doctor::delete_orphans(&r, 2, false).unwrap(), 5);
        // Exactly a tenth is not over it.
        let r = make(6);
        assert_eq!(doctor::delete_orphans(&r, 60, false).unwrap(), 6);
        assert_eq!(
            doctor::delete_orphans(&Report::default(), 0, false).unwrap(),
            0
        );
    }

    #[test]
    fn repair_work_removes_the_entries_and_leaves_every_mapping_alone() {
        let w = World::new(4);
        fs::remove_file(w.file_of(dbid(2))).unwrap();
        fs::remove_file(w.file_of(ORPHAN)).unwrap();
        let report = examine(&w);
        let p = w.prepare();
        let work = doctor::repair_work(&p, &report);
        let removes: Vec<(u64, Option<i64>, u64)> = work
            .tracks
            .removes
            .iter()
            .map(|r| (r.dbid, r.track_id, r.size))
            .collect();
        assert_eq!(
            removes,
            vec![
                (
                    dbid(2),
                    Some(w.id_of(&w.files[1])),
                    w.track(dbid(2)).unwrap().adopt.size
                ),
                (ORPHAN, None, 1234),
            ]
        );
        assert!(work.tracks.copies.is_empty() && work.tracks.retags.is_empty());
        assert!(work.playlists.ops.is_empty() && work.playlists.deleted.is_empty());
        assert!(work.keep_playlist_mappings);
        // Managed playlists are expected as they are, less the lost tracks.
        let mut got: Vec<(String, Vec<u64>)> = work
            .playlists
            .managed
            .iter()
            .map(|t| (t.name.clone(), t.members.clone()))
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("Mix".to_owned(), vec![dbid(4)]),
                ("Road".to_owned(), vec![dbid(1), dbid(3), dbid(4)]),
            ]
        );
    }

    #[test]
    fn fix_repairs_the_database_the_files_and_the_index_and_keeps_the_playlist_mappings() {
        let mut w = World::new(4);
        // Entries without a file: a managed one (in Road and the unmanaged
        // Stray) and the unmanaged track.
        fs::remove_file(w.file_of(dbid(2))).unwrap();
        fs::remove_file(w.file_of(ORPHAN)).unwrap();
        // An orphan file.
        let orphan = music(&w, "F07/LOST.mp3");
        fs::write(&orphan, b"x").unwrap();
        // Playlist state the repair must not touch: Mix deleted in the
        // library (a tombstone, its iPod playlist still there), Road mapped
        // without an iPod id.
        playlists::delete(w.env.store.conn(), &w.env.cfg, "Mix").unwrap();
        let conn = w.env.store.conn();
        conn.execute(
            "UPDATE device_playlists SET ipod_id = NULL WHERE ipod_name = 'Road'",
            [],
        )
        .unwrap();
        // A forgotten track and a stale mapping.
        let ghost_id = ghost(&mut w, now_unix() - FORGET_AFTER_SECS - 10);
        let conn = w.env.store.conn();
        let maps_before = devices::playlist_mappings(conn, w.device()).unwrap();
        let tombstones = store::removed_playlists(conn).unwrap();
        assert_eq!(tombstones.len(), 1);
        assert!(maps_before.iter().any(|m| m.ipod_id.is_none()));
        let road_ipod = w.playlist("Road").unwrap();
        let mix_ipod = w.playlist("Mix").unwrap();

        let report = examine(&w);
        assert_eq!(report.missing_files.len(), 2);
        assert_eq!(report.orphan_files, vec![orphan.clone()]);
        assert_eq!(report.forgotten.len(), 1);
        let tracks_before = w.contents().tracks.len();

        // What `iprune doctor --fix` does, in its order.
        let prepared = w.prepare();
        let work = doctor::repair_work(&prepared, &report);
        let out = exec::sync(
            w.env.store.conn(),
            &w.env.cfg,
            &w.paths,
            &iprune_core::lock::ipod(&w.paths).unwrap(),
            &w.root,
            w.open(),
            &prepared,
            &work,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(out.removed, 2);
        assert_eq!(
            doctor::delete_orphans(&report, tracks_before, false).unwrap(),
            1
        );
        doctor::fix_index(w.env.store.conn(), Some(w.device()), &report).unwrap();

        // The database lost exactly those entries, verified by the sync.
        let c = w.contents();
        assert_eq!(c.tracks.len(), tracks_before - 2);
        assert!(w.track(dbid(2)).is_none() && w.track(ORPHAN).is_none());
        assert_eq!(
            w.playlist("Road"),
            Some(road_ipod.into_iter().filter(|d| *d != dbid(2)).collect())
        );
        assert_eq!(w.playlist("Stray"), Some(vec![]));
        assert_eq!(
            w.playlist("Mix"),
            Some(mix_ipod),
            "the deleted playlist is left for sync"
        );
        // A backup was taken first, and holds what was removed.
        assert_eq!(w.backups().len(), 1);
        let wf: serde_json::Value =
            serde_json::from_slice(&fs::read(out.backup.dir.join("work.json")).unwrap()).unwrap();
        assert_eq!(wf["tracks"]["removes"].as_array().unwrap().len(), 2);
        // The orphan is gone, the other files are not.
        assert!(!orphan.exists());
        for d in [dbid(1), dbid(3), dbid(4)] {
            assert!(w.file_of(d).is_file());
        }
        // The index: mapping of the removed track dropped, the ghost gone,
        // playlist mappings and tombstones exactly as before.
        let conn = w.env.store.conn();
        assert_eq!(w.mapping().len(), 3);
        assert_eq!(
            devices::playlist_mappings(conn, w.device()).unwrap(),
            maps_before
        );
        assert_eq!(store::removed_playlists(conn).unwrap(), tombstones);
        let ghosts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tracks WHERE id = ?1",
                [ghost_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ghosts, 0);
        // Nothing left to repair.
        assert!(examine(&w).is_clean(), "{:?}", examine(&w));
    }
}

// ---- restore: statistics, Play Counts and missing files

fn restore(
    w: &World,
    stamp: &str,
    discard_play_counts: bool,
) -> iprune_core::Result<exec::Restored> {
    // Tries again while the lock reports busy: a helper program forked by
    // another test thread can briefly hold a copy of a released lock file.
    for _ in 0..200 {
        match exec::restore(
            w.env.store.conn(),
            &w.paths,
            &w.root,
            GUID,
            stamp,
            discard_play_counts,
        ) {
            Err(Error::IpodBusy) => std::thread::sleep(std::time::Duration::from_millis(5)),
            other => return other,
        }
    }
    unreachable!("the iPod lock stayed busy")
}

/// A World where a sync removed track 1 (library file `s1.mp3` deleted), and
/// the iPod then played `plays` of its remaining tracks, in the database
/// order [d2, d3, orphan]. Returns the stamp of the backup taken before that
/// sync, whose database is [d1, d2, d3, orphan].
fn removed_then_played(w: &mut World) -> String {
    fs::remove_file(&w.files[0]).unwrap();
    w.rescan();
    let out = w.sync().unwrap();
    assert_eq!(out.removed, 1);
    let order: Vec<u64> = w.contents().tracks.iter().map(|t| t.adopt.dbid).collect();
    assert_eq!(order, vec![dbid(2), dbid(3), ORPHAN]);
    // d2: 5 plays, rating 100; d3: 7 plays, rating 20; the orphan: none.
    w.write_play_counts(&[
        (5, 1_700_000_000, 100, 0),
        (7, 1_700_000_100, 20, 0),
        (0, 0, 0, 0),
    ]);
    out.backup.stamp
}

fn stats_of(w: &World, file: &Path) -> Option<(i64, Option<i64>)> {
    let id = w.id_of(file);
    w.env
        .store
        .conn()
        .query_row(
            "SELECT plays, rating FROM stats WHERE track_id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
}

// Prevents (audit H2) a restore leaving a live Play Counts, which libgpod
// reads by position against the restored database, so that the plays of the
// database it replaced went to the wrong tracks or were lost.
#[test]
fn restore_pulls_the_plays_of_the_database_it_replaces_and_takes_play_counts_off_the_ipod() {
    let mut w = World::new(3);
    let stamp = removed_then_played(&mut w);
    let counts = fs::read(w.root.join("iPod_Control/iTunes/Play Counts")).unwrap();

    let r = restore(&w, &stamp, false).unwrap();

    // Credited to the tracks the plays belong to, by the current mappings.
    assert_eq!(stats_of(&w, &w.files[1]), Some((5, Some(100))));
    assert_eq!(stats_of(&w, &w.files[2]), Some((7, Some(20))));
    assert_eq!(
        stats_of(&w, &w.files[0]),
        None,
        "the removed track has none"
    );
    assert_eq!((r.pulled.tracks(), r.pulled.plays()), (2, 12));
    // The file left the iPod, whole, and is kept in the backup of what the
    // restore replaced; the restored database carries no recent plays.
    assert!(!w.root.join("iPod_Control/iTunes/Play Counts").exists());
    assert_eq!(r.set_aside, vec!["Play Counts"]);
    assert_eq!(fs::read(r.saved.dir.join("Play Counts")).unwrap(), counts);
    let restored = w.contents();
    assert_eq!(restored.tracks.len(), 4, "the backup's database is back");
    assert!(
        restored.tracks.iter().all(|t| t.recent_plays == 0),
        "{:?}",
        restored
            .tracks
            .iter()
            .map(|t| (t.adopt.dbid, t.recent_plays))
            .collect::<Vec<_>>()
    );
    assert_eq!(w.pending_pulls(), 0, "settled, not left pending");

    // The next sync does not credit them again. (Its pull takes the iPod's
    // rating, which the restored database has not got, so ratings are not
    // compared here.)
    w.sync().unwrap();
    assert_eq!(stats_of(&w, &w.files[1]).map(|s| s.0), Some(5));
    assert_eq!(stats_of(&w, &w.files[2]).map(|s| s.0), Some(7));
    assert_eq!(w.stats_total_plays(), 12);
}

// `--discard-play-counts` is for a database that cannot be read; with a
// readable one the plays are still pulled.
#[test]
fn discarding_play_counts_does_not_lose_the_plays_of_a_readable_database() {
    let mut w = World::new(3);
    let stamp = removed_then_played(&mut w);
    let r = restore(&w, &stamp, true).unwrap();
    assert_eq!(w.stats_total_plays(), 12);
    assert_eq!(r.pulled.plays(), 12);
    assert!(!w.root.join("iPod_Control/iTunes/Play Counts").exists());
}

// Prevents (audit M17) entries a restore brought back after a sync deleted
// their files staying in the database with no file: they are copied again.
#[test]
fn entries_a_restore_brings_back_without_their_files_are_copied_again() {
    let mut w = World::new(3);
    let s1 = w.files[0].clone();
    let gone_file = w.file_of(dbid(1));
    fs::remove_file(&s1).unwrap();
    w.rescan();
    let out = w.sync().unwrap();
    assert_eq!(out.removed, 1);
    assert!(!gone_file.exists(), "set up: the sync deleted the file");

    let r = restore(&w, &out.backup.stamp, false).unwrap();
    assert_eq!(r.dropped_tracks, 0, "the library track is still indexed");
    assert!(w.track(dbid(1)).is_some(), "the entry is back");
    assert!(!gone_file.exists(), "and has no file");

    // The library file comes back.
    w.env.add("s1.mp3", Kind::Mp3, 1);
    w.rescan();
    let p = w.prepare();
    assert_eq!(
        p.plan
            .replace
            .iter()
            .map(|r| (r.old_dbid, r.file_missing))
            .collect::<Vec<_>>(),
        vec![(dbid(1), true)],
        "{:?}",
        p.plan
    );
    let out = w.sync().unwrap();
    assert_eq!((out.copied, out.recopied), (0, 1));

    // Every entry of the database has a file, with the library's bytes for
    // the managed ones.
    let c = w.contents();
    for t in &c.tracks {
        let f = ipod::file_path(&w.root, t.ipod_path.as_deref().expect("a path")).unwrap();
        assert!(
            f.is_file(),
            "{:x} has no file {}",
            t.adopt.dbid,
            f.display()
        );
    }
    assert_eq!(
        fs::read(w.file_of(dbid(1))).unwrap(),
        fs::read(&s1).unwrap()
    );
    // Nothing is left to do.
    let p = w.prepare();
    assert!(exec::full_work(&p).is_empty(), "{:?}", p.plan);
}

// ---- the guard on copying many tracks again for a missing file

fn lose_ipod_files(w: &World, ks: std::ops::RangeInclusive<usize>) {
    for k in ks {
        fs::remove_file(w.file_of(dbid(k))).unwrap();
    }
}

fn every_entry_has_a_file(w: &World) {
    for t in &w.contents().tracks {
        let f = ipod::file_path(&w.root, t.ipod_path.as_deref().expect("a path")).unwrap();
        assert!(f.is_file(), "{:x} has no file", t.adopt.dbid);
    }
}

// Prevents copying most of the library again because the iPod's music
// folder read badly.
#[test]
fn many_files_missing_from_the_ipod_are_refused_unless_allowed() {
    let w = World::new(8);
    lose_ipod_files(&w, 1..=6);
    let tree_before = w.tree();
    let e = w.sync().unwrap_err();
    assert!(
        matches!(
            e,
            Error::MassMissingFiles {
                missing: 6,
                managed: 8
            }
        ),
        "{e}"
    );
    assert!(e.to_string().contains("--allow-mass-removal"), "{e}");
    assert_eq!(w.tree(), tree_before, "the iPod is untouched");
    assert!(w.backups().is_empty(), "no backup, so no write began");
    assert_eq!(w.mapping().len(), 8);

    let p = w.prepare();
    let work = exec::full_work(&p);
    let out = w
        .run(
            &p,
            &work,
            Options {
                allow_mass_removal: true,
            },
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!((out.recopied, out.removed), (6, 0));
    every_entry_has_a_file(&w);
    assert!(exec::full_work(&w.prepare()).is_empty());
}

#[test]
fn five_files_missing_from_the_ipod_are_copied_again_without_the_flag() {
    let w = World::new(8);
    lose_ipod_files(&w, 1..=5);
    let out = w.sync().unwrap();
    assert_eq!(out.recopied, 5);
    every_entry_has_a_file(&w);
}

// The refusal needs more than five and more than a tenth of the managed
// tracks; a replace for changed audio is not counted.
#[test]
fn the_missing_files_guard_counts_only_missing_files_and_a_tenth_of_the_managed_tracks() {
    use iprune_core::plan::{Plan, ReplaceOp};
    let plan = |missing: usize, changed: usize| Plan {
        replace: (0..missing + changed)
            .map(|i| ReplaceOp {
                track_id: i as i64,
                old_dbid: i as u64,
                path: "/x.mp3".into(),
                size: 1,
                old_size: 1,
                file_missing: i < missing,
            })
            .collect(),
        ..Plan::default()
    };
    let strict = Options::default();
    let allow = Options {
        allow_mass_removal: true,
    };
    // (missing, changed audio, managed, refused)
    let cases = [
        (6, 0, 59, true),
        (6, 0, 60, false),
        (5, 0, 1, false),
        (0, 20, 3, false),
        (6, 20, 59, true),
        (60, 0, 400, true),
        (40, 0, 400, false),
    ];
    for (missing, changed, managed, refused) in cases {
        let r = exec::check_missing_files(&plan(missing, changed), managed, strict);
        match (refused, r) {
            (
                true,
                Err(Error::MassMissingFiles {
                    missing: m,
                    managed: n,
                }),
            ) => {
                assert_eq!((m, n), (missing, managed));
            }
            (false, Ok(())) => {}
            (_, other) => panic!("{missing}/{changed}/{managed}: {other:?}"),
        }
        assert!(
            exec::check_missing_files(&plan(missing, changed), managed, allow).is_ok(),
            "the flag allows it"
        );
    }
}
