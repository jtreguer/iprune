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
    ipod_playlist, mp3_secs_with_id3v2, mp3_with_id3v2, read_audio_data, seed_old_audio,
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
        World::new_with_plays(n, &[])
    }

    /// As `new`, with these `playcount`s in the iPod's iTunesDB for the first
    /// tracks, as built up under iTunes before adoption.
    pub fn new_with_plays(n: usize, plays: &[u32]) -> World {
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
                    plays: plays.get(k).copied().unwrap_or(0),
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
    // A folder is not a track's file: kept, with a warning.
    assert!(
        out.warnings[0].contains("kept") && out.warnings[0].contains("not a regular file"),
        "{:?}",
        out.warnings
    );
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

// One unmanaged removal among four tracks is under both guards' thresholds
// (the managed one, and the unmanaged one of audit L5: over five and over a
// tenth), so it goes through unasked.
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

// Prevents (audit L5) the unmanaged removals escaping every mass-removal
// guard: more than five and more than a tenth of ALL the iPod's tracks is
// refused; exactly five, or exactly a tenth, is not.
#[test]
fn unmanaged_removals_are_refused_over_five_and_over_a_tenth_of_the_ipod() {
    use iprune_core::transfer::{Remove, TrackWork};
    let work = |unmanaged: usize, managed: usize| TrackWork {
        removes: (0..unmanaged)
            .map(|i| Remove {
                dbid: 0x7000 + i as u64,
                track_id: None,
                size: 1,
            })
            .chain((0..managed).map(|i| Remove {
                dbid: 0x8000 + i as u64,
                track_id: Some(i as i64),
                size: 1,
            }))
            .collect(),
        ..TrackWork::default()
    };
    let strict = Options::default();
    let refused = |w: &TrackWork, on_ipod: usize, o: Options| {
        matches!(
            exec::check_unmanaged_removals(w, on_ipod, o),
            Err(Error::MassUnmanagedRemoval { .. })
        )
    };
    // Over five and over a tenth.
    let e = exec::check_unmanaged_removals(&work(6, 0), 10, strict).unwrap_err();
    assert!(
        matches!(
            e,
            Error::MassUnmanagedRemoval {
                removes: 6,
                on_ipod: 10
            }
        ),
        "{e}"
    );
    // Five of five: not over five.
    assert!(!refused(&work(5, 0), 5, strict));
    // Six of sixty: exactly a tenth; six of fifty-nine is over it.
    assert!(!refused(&work(6, 0), 60, strict));
    assert!(refused(&work(6, 0), 59, strict));
    // The flag lifts it.
    let allow = Options {
        allow_mass_removal: true,
    };
    assert!(!refused(&work(6, 0), 10, allow));
    // Managed removals are the other guard's: they do not count here, and
    // do not hide unmanaged ones either.
    assert!(!refused(&work(0, 9), 10, strict));
    assert!(!refused(&work(5, 9), 14, strict));
    assert!(refused(&work(6, 9), 15, strict));
    assert!(!refused(&TrackWork::default(), 0, strict));
}

// The refusal reaches `check_guards` (and so a sync and the window), counted
// against every track on the iPod, before anything is written.
#[test]
fn check_guards_refuses_many_unmanaged_removals() {
    let mut w = World::new(2);
    w.env.cfg.sync.remove_unmanaged = true;
    // Leave the unmanaged Orphan + five more: six, of eight.
    let mut db = w.open();
    for k in 0..5u64 {
        let fields = gpod::TrackFields {
            title: Some(format!("Extra{k}")),
            ..Default::default()
        };
        let audio = gpod::AudioFields {
            size: 10,
            tracklen: 1000,
            bitrate: 128,
            samplerate: 44100,
            filetype: "MPEG audio file".into(),
            time_added: 0,
            time_modified: 0,
        };
        db.add_track(0x5000 + k, &fields, &audio).unwrap();
    }
    db.write().unwrap();
    drop(db);
    w.reload();
    let p = w.prepare();
    let work = exec::full_work(&p);
    assert_eq!(work.tracks.removes.len(), 6, "{:?}", work.tracks.removes);
    assert!(matches!(
        exec::check_guards(&p, &work, Options::default()).unwrap_err(),
        Error::MassUnmanagedRemoval {
            removes: 6,
            on_ipod: 8
        }
    ));
    exec::check_guards(
        &p,
        &work,
        Options {
            allow_mass_removal: true,
        },
    )
    .unwrap();
}

// ---- the copy is checked before the write (audit L22)

/// Every file under the iPod's Music folder.
fn music_files(w: &World) -> std::collections::BTreeSet<PathBuf> {
    tree(&w.root.join("iPod_Control/Music"))
        .into_keys()
        .collect()
}

/// Runs the plan, cutting each file that appeared under Music in half once
/// the last copy is done: the iPod's USB link losing the end of the write.
/// (`Progress::Writing` comes after the checks, so too late for this; the
/// closing `Copying` event, with no name, comes before them.)
fn run_with_short_copies(w: &World, p: &Prepared, work: &FullWork) -> iprune_core::Result<Outcome> {
    let known = music_files(w);
    let cut = |e: Progress| {
        if let Progress::Copying { name, .. } = &e
            && name.is_empty()
        {
            for f in music_files(w).difference(&known) {
                let path = w.root.join("iPod_Control/Music").join(f);
                let len = fs::metadata(&path).unwrap().len();
                assert!(len > 1, "set up: a real copy");
                fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(len / 2)
                    .unwrap();
            }
        }
    };
    w.run(p, work, Options::default(), &cut, &AtomicBool::new(false))
}

// Prevents (audit L22) a short file on the iPod being written into the
// database as a good track (and, for a replaced track, the old file being
// deleted on its strength). A copy cut short is a PreWriteCheck problem
// naming both sizes; the database, the statistics pull and the mappings are
// as before, and the next sync copies it again.
#[test]
fn a_new_track_whose_file_is_short_is_a_prewrite_problem_and_nothing_is_written() {
    let mut w = World::new(3);
    let new = w.add_new("fresh.mp3", Kind::Mp3, 7);
    let want = fs::metadata(&new).unwrap().len();
    w.write_play_counts(&[
        (3, 1_700_000_000, 80, 1),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
    ]);
    let p = w.prepare();
    let work = exec::full_work(&p);
    assert_eq!(work.tracks.copies.len(), 1);
    let db_before = fs::read(w.root.join("iPod_Control/iTunes/iTunesDB")).unwrap();
    let counts_before = fs::read(w.root.join("iPod_Control/iTunes/Play Counts")).unwrap();
    let music_before = music_files(&w);
    let mapping_before = w.mapping();

    let e = run_with_short_copies(&w, &p, &work).unwrap_err();
    let Error::PreWriteCheck(problems) = &e else {
        panic!("expected PreWriteCheck, got {e}");
    };
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains(&(want / 2).to_string()) && problems[0].contains(&want.to_string()),
        "both sizes are named: {problems:?}"
    );
    // Nothing written: the database and Play Counts are as they were, the
    // pull is undone, and no mapping was recorded.
    assert_eq!(
        fs::read(w.root.join("iPod_Control/iTunes/iTunesDB")).unwrap(),
        db_before
    );
    assert_eq!(
        fs::read(w.root.join("iPod_Control/iTunes/Play Counts")).unwrap(),
        counts_before
    );
    assert_eq!(w.stats_total_plays(), 0, "the pull is undone");
    assert_eq!(w.pending_pulls(), 0);
    assert_eq!(w.mapping(), mapping_before);
    assert_eq!(w.contents().tracks.len(), 4);
    // Only the cut file is new on the iPod.
    assert_eq!(music_files(&w).difference(&music_before).count(), 1);

    // The next sync copies the track again, whole.
    let out = w.sync().unwrap();
    assert_eq!(out.copied, 1);
    let id = w.id_of(&new);
    let file = w.file_of(w.mapping()[&id]);
    assert_eq!(fs::metadata(file).unwrap().len(), want);
    assert_eq!(w.stats_total_plays(), 3, "the plays are pulled once");
}

// The same for new audio replacing a track's: the old file is not deleted and
// the track keeps pointing at it.
#[test]
fn a_replacement_whose_file_is_short_keeps_the_old_file_and_the_old_entry() {
    let mut w = World::new(3);
    let src = w.files[2].clone();
    fs::copy(fixture_secs(Kind::Mp3, 3, 3), &src).unwrap();
    w.rescan();
    let want = fs::metadata(&src).unwrap().len();
    let old_file = w.file_of(dbid(3));
    let old_bytes = fs::read(&old_file).unwrap();
    let p = w.prepare();
    let work = exec::full_work(&p);
    assert_eq!(work.tracks.copies.len(), 1);
    assert!(work.tracks.copies[0].replace, "set up: a replacement");
    let db_before = fs::read(w.root.join("iPod_Control/iTunes/iTunesDB")).unwrap();

    let e = run_with_short_copies(&w, &p, &work).unwrap_err();
    let Error::PreWriteCheck(problems) = &e else {
        panic!("expected PreWriteCheck, got {e}");
    };
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains(&want.to_string()), "{problems:?}");
    assert_eq!(
        fs::read(w.root.join("iPod_Control/iTunes/iTunesDB")).unwrap(),
        db_before
    );
    assert_eq!(
        fs::read(&old_file).unwrap(),
        old_bytes,
        "the old file stays"
    );
    assert_eq!(w.file_of(dbid(3)), old_file, "the track still points at it");

    // Once a whole copy is made the track has the new audio.
    let out = w.sync().unwrap();
    assert_eq!(out.recopied, 1);
    assert_eq!(fs::metadata(w.file_of(dbid(3))).unwrap().len(), want);
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

/// Replaces the library file of copy `i` with a directory, which opens
/// and passes the size check (the copy's size is set to the directory's) but
/// cannot be read: the stand-in for a file that fails mid-read. Returns where
/// the real file went.
fn make_unreadable(work: &mut FullWork, i: usize, keep: &Path) -> PathBuf {
    let path = work.tracks.copies[i].path.clone();
    let aside = keep.join(path.file_name().unwrap());
    fs::rename(&path, &aside).unwrap();
    fs::create_dir(&path).unwrap();
    work.tracks.copies[i].audio.size = fs::metadata(&path).unwrap().len();
    // Set up check: it opens, and reading fails.
    let mut f = fs::File::open(&path).unwrap();
    assert!(std::io::Read::read(&mut f, &mut [0u8; 8]).is_err());
    aside
}

// Prevents (audit L6) one file that opens but cannot be read to its end
// stopping every sync at the same place, with the retags and playlists
// after it never applied.
#[test]
fn a_file_that_opens_but_cannot_be_read_is_skipped_and_the_rest_of_the_sync_goes_on() {
    let mut w = World::new(3);
    // A retag of track 1, applied only once the copies are done.
    w.env
        .add_bytes("s1.mp3", &retagged_mp3(1, "Renamed", "Artist1", None, None));
    let news = with_three_new(&mut w);
    let p = w.prepare();
    let mut work = exec::full_work(&p);
    assert_eq!(work.tracks.copies.len(), 3);
    assert_eq!(work.tracks.retags.len(), 1);
    let planned: Vec<(PathBuf, u64)> = work
        .tracks
        .copies
        .iter()
        .map(|c| (c.path.clone(), c.dbid))
        .collect();
    let keep = tempfile::tempdir().unwrap();
    let aside = make_unreadable(&mut work, 1, keep.path());
    let (bad, bad_dbid) = planned[1].clone();
    let (first, second) = (planned[0].clone(), planned[2].clone());
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
    assert_eq!(
        out.stopped, None,
        "an unreadable source does not stop the sync"
    );
    assert_eq!(out.copied, 2);
    assert_eq!(out.retagged, 1, "the retag after the copies still applied");
    assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
    assert_eq!(out.skipped[0].0, bad);
    assert!(!out.skipped[0].1.is_empty());

    // Both good files are on the iPod, mapped and in the playlists; the bad
    // one is nowhere.
    for (f, d) in [&first, &second] {
        assert!(w.track(*d).is_some(), "{f:?}");
        assert!(w.mapping().contains_key(&w.id_of(f)));
    }
    assert!(w.track(bad_dbid).is_none());
    assert!(!w.mapping().values().any(|d| *d == bad_dbid));
    assert_eq!(
        w.track(dbid(1)).unwrap().tags.title.as_deref(),
        Some("Renamed")
    );
    // In the order of the playlist files, the bad one left out.
    let good: Vec<u64> = news
        .iter()
        .filter(|f| **f != bad)
        .map(|f| planned.iter().find(|(p, _)| p == f).unwrap().1)
        .collect();
    assert_eq!(good.len(), 2);
    let mut road = road_before;
    road.extend(&good);
    assert_eq!(w.playlist("Road"), Some(road));
    assert_eq!(w.playlist("Fresh"), Some(good));

    // The file comes back: the next plan holds that file and nothing else.
    fs::remove_dir(&bad).unwrap();
    fs::rename(&aside, &bad).unwrap();
    w.rescan();
    let p2 = w.prepare();
    let work2 = exec::full_work(&p2);
    let again: Vec<&Path> = work2
        .tracks
        .copies
        .iter()
        .map(|c| c.path.as_path())
        .collect();
    assert_eq!(again, [bad.as_path()], "{:?}", p2.plan);
    assert!(work2.tracks.retags.is_empty() && work2.tracks.removes.is_empty());
    let out = w
        .run(
            &p2,
            &work2,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!((out.copied, out.skipped.len(), out.stopped), (1, 0, None));
    assert_eq!(w.playlist("Fresh").unwrap().len(), 3);
    assert!(exec::full_work(&w.prepare()).is_empty());
    let _ = news;
}

// Prevents (audit L6) the same for new audio of a track already on the iPod:
// the old file and the track's places in the playlists stay.
#[test]
fn new_audio_that_cannot_be_read_leaves_the_existing_track_as_it_is() {
    let mut w = World::new(3);
    let src = w.files[2].clone();
    let old_file = w.file_of(dbid(3));
    let old_bytes = fs::read(&old_file).unwrap();
    fs::copy(fixture_secs(Kind::Mp3, 3, 3), &src).unwrap();
    w.rescan();
    let p = w.prepare();
    let mut work = exec::full_work(&p);
    assert!(work.tracks.copies.len() == 1 && work.tracks.copies[0].replace);
    let keep = tempfile::tempdir().unwrap();
    make_unreadable(&mut work, 0, keep.path());
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
    assert_eq!(out.stopped, None);
    assert_eq!((out.recopied, out.copied), (0, 0));
    assert_eq!(out.skipped.len(), 1);
    assert_eq!(out.skipped[0].0, src);
    assert_eq!(w.playlist("Road"), Some(road));
    assert_eq!(w.playlist("Mix"), Some(mix));
    assert_eq!(w.track(dbid(3)).unwrap().adopt.size, old_bytes.len() as u64);
    assert_eq!(w.file_of(dbid(3)), old_file);
    assert_eq!(fs::read(&old_file).unwrap(), old_bytes);
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

// ---- the backup before every write after the first (audit M8)

/// The backups a checkpoint took (they hold `checkpoint.txt`), oldest first.
fn checkpoint_backups(w: &World) -> Vec<backup::Backup> {
    w.backups()
        .into_iter()
        .filter(|b| b.dir.join("checkpoint.txt").is_file())
        .collect()
}

/// A world with 501 new files (a checkpoint after 500 copies) and the first
/// library file deleted, so a sync writes three times: the removal, the
/// checkpoint, the end. The removed file is gone from the iPod after the
/// first write, which the backup taken before the sync cannot give back.
fn removal_then_501_copies() -> World {
    let (mut w, _) = with_many_new(501);
    fs::remove_file(&w.files[0]).unwrap();
    w.rescan();
    w
}

// Prevents (audit M8) a sync that fails at its last write pointing to the
// backup taken before the sync, whose removed files are already deleted,
// instead of to the last verified checkpoint.
#[test]
#[cfg(unix)]
fn a_final_write_that_fails_after_a_checkpoint_names_the_checkpoint_backup_and_it_restores() {
    use std::os::unix::fs::PermissionsExt;
    let w = removal_then_501_copies();
    let removed_file = w.file_of(dbid(1));
    let p = w.prepare();
    let work = exec::full_work(&p);
    assert_eq!(work.tracks.copies.len(), 501);
    assert_eq!(work.tracks.removes.len(), 1);
    let itunes = w.root.join("iPod_Control/iTunes");
    let db_path = itunes.join("iTunesDB");
    struct Restore(PathBuf, u32);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(self.1));
        }
    }
    let guards: RefCell<Vec<Restore>> = RefCell::new(Vec::new());
    let writes = RefCell::new(0);
    // The database after the checkpoint write, read when the 501st copy
    // starts.
    let checkpointed: RefCell<Option<Vec<u8>>> = RefCell::new(None);
    let blocked = RefCell::new(false);
    let progress = |e: Progress| match e {
        Progress::Copying { done: 500, .. } if checkpointed.borrow().is_none() => {
            *checkpointed.borrow_mut() = Some(fs::read(&db_path).unwrap());
        }
        Progress::Writing => {
            *writes.borrow_mut() += 1;
            if *writes.borrow() == 3 {
                for (path, mode) in [(&db_path, 0o444), (&itunes, 0o555)] {
                    let old = fs::metadata(path).unwrap().permissions().mode() & 0o777;
                    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
                    guards.borrow_mut().push(Restore(path.clone(), old));
                }
                *blocked.borrow_mut() = fs::File::create(itunes.join(".probe")).is_err();
                let _ = fs::remove_file(itunes.join(".probe"));
            }
        }
        _ => {}
    };
    let res = w.run(
        &p,
        &work,
        Options::default(),
        &progress,
        &AtomicBool::new(false),
    );
    guards.borrow_mut().clear();
    if !*blocked.borrow() {
        eprintln!("running as root: permissions do not block writes, test skipped");
        return;
    }
    assert_eq!(*writes.borrow(), 3);
    let e = res.unwrap_err();
    let Error::WriteIncomplete { stamp, .. } = &e else {
        panic!("expected WriteIncomplete, got {e}");
    };

    // Two backups: the one before the sync and the checkpoint's.
    assert_eq!(w.backups().len(), 2);
    let cps = checkpoint_backups(&w);
    assert_eq!(cps.len(), 1);
    let cp = &cps[0];
    let pre = w
        .backups()
        .into_iter()
        .find(|b| b.stamp != cp.stamp)
        .unwrap();
    assert_eq!(stamp, &cp.stamp, "the error names the checkpoint backup");
    assert_ne!(stamp, &pre.stamp);
    assert!(
        fs::read_to_string(cp.dir.join("checkpoint.txt"))
            .unwrap()
            .contains(&pre.stamp),
        "it names the backup taken before the sync"
    );
    // Its database is what the last verified write put on the iPod.
    assert_eq!(
        fs::read(cp.dir.join("iTunesDB")).unwrap(),
        checkpointed.into_inner().unwrap()
    );
    // Its mappings are the recorded ones: the 3 adopted, minus the removed,
    // plus 500 copies. Not the 3 of the backup before the sync.
    let m: serde_json::Value =
        serde_json::from_slice(&fs::read(cp.dir.join("mappings.json")).unwrap()).unwrap();
    let saved: BTreeMap<i64, u64> = m["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["track_id"].as_i64().unwrap(), t["dbid"].as_u64().unwrap()))
        .collect();
    assert_eq!(saved.len(), 2 + 500);
    assert_eq!(saved, w.mapping());
    assert!(
        !fs::read_to_string(cp.dir.join("snapshot.txt"))
            .unwrap()
            .is_empty()
    );
    // The backup before the sync still lists the removed track, whose file
    // is gone: that is why it is not the one to restore.
    assert!(w.track(dbid(1)).is_none());
    assert!(!removed_file.exists());

    // Restoring the checkpoint gives a database whose every file exists.
    restore(&w, &cp.stamp, false).unwrap();
    let c = w.contents();
    assert_eq!(c.tracks.len(), 2 + 1 + 500);
    for t in &c.tracks {
        let f = ipod::file_path(&w.root, t.ipod_path.as_deref().expect("a path")).unwrap();
        assert!(
            f.is_file(),
            "{:x} has no file {}",
            t.adopt.dbid,
            f.display()
        );
    }
}

// Prevents (audit M8) a checkpoint backup that cannot be made (backups disk
// full) being a bare I/O error, or the write going ahead without it, or the
// pull being undone though the iPod holds the first writes.
#[test]
#[cfg(unix)]
fn a_checkpoint_backup_that_fails_stops_before_the_write_and_the_next_sync_finishes() {
    use std::os::unix::fs::PermissionsExt;
    let w = removal_then_501_copies();
    w.write_play_counts(&[
        (3, 1_700_000_000, 80, 1),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
    ]);
    let p = w.prepare();
    let work = exec::full_work(&p);
    let db_path = w.root.join("iPod_Control/iTunes/iTunesDB");
    let guid_dir = w.paths.data_dir.join("backups").join(GUID);
    let at_checkpoint: RefCell<Option<Vec<u8>>> = RefCell::new(None);
    let blocked = RefCell::new(false);
    let progress = |e: Progress| {
        if matches!(e, Progress::Copying { done: 500, .. }) && at_checkpoint.borrow().is_none() {
            *at_checkpoint.borrow_mut() = Some(fs::read(&db_path).unwrap());
            fs::set_permissions(&guid_dir, fs::Permissions::from_mode(0o555)).unwrap();
            *blocked.borrow_mut() = fs::create_dir(guid_dir.join(".probe")).is_err();
            let _ = fs::remove_dir(guid_dir.join(".probe"));
        }
    };
    let res = w.run(
        &p,
        &work,
        Options::default(),
        &progress,
        &AtomicBool::new(false),
    );
    fs::set_permissions(&guid_dir, fs::Permissions::from_mode(0o755)).unwrap();
    if !*blocked.borrow() {
        eprintln!("running as root: permissions do not block writes, test skipped");
        return;
    }
    let e = res.unwrap_err();
    assert!(
        matches!(e, Error::CheckpointBackupFailed(_)),
        "expected CheckpointBackupFailed, got {e}"
    );
    assert!(!e.is_check_failure(), "exit 1, not 3");
    // The iPod is as the last verified write left it; the pull stands.
    assert_eq!(
        fs::read(&db_path).unwrap(),
        at_checkpoint.into_inner().unwrap()
    );
    assert_eq!(w.contents().tracks.len(), 2 + 1 + 500);
    assert_eq!(w.mapping().len(), 2 + 500);
    assert_eq!(w.stats_total_plays(), 3);
    assert_eq!(w.pending_pulls(), 0);

    // The next sync copies the rest, plays counted once.
    let p = w.prepare();
    let work = exec::full_work(&p);
    assert_eq!(
        work.tracks.copies.len(),
        1,
        "{:?}",
        work.tracks.copies.len()
    );
    let out = w
        .run(
            &p,
            &work,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(out.copied, 1);
    assert_eq!(w.contents().tracks.len(), 2 + 1 + 501);
    assert_eq!(w.mapping().len(), 2 + 501);
    assert_eq!(w.stats_total_plays(), 3);
    assert!(exec::full_work(&w.prepare()).is_empty());
}

// Prevents (audit M9) the pre-prompt guards applying to work that does not
// copy or change playlists, as doctor's repair and a write of tracks only.
#[test]
fn check_guards_apply_each_guard_only_to_the_work_it_is_about() {
    let w = World::new(8);
    for k in 1..=6 {
        fs::remove_file(w.file_of(dbid(k))).unwrap();
    }
    w.write_list("Stray", &[w.files[0].clone()]);
    w.reload();
    let p = w.prepare();
    let opts = Options::default();
    let full = exec::full_work(&p);
    assert!(!full.tracks.copies.is_empty() && !full.playlists.ops.is_empty());
    // With copies the missing files are caught first.
    assert!(matches!(
        exec::check_guards(&p, &full, opts).unwrap_err(),
        Error::MassMissingFiles { .. }
    ));
    // With the copies gone from the work, the takeover is next.
    let mut no_copies = exec::full_work(&p);
    no_copies.tracks.copies.clear();
    assert!(matches!(
        exec::check_guards(&p, &no_copies, opts).unwrap_err(),
        Error::PlaylistTakeover(_)
    ));
    // No copies and no playlist ops: neither guard applies.
    no_copies.playlists.ops.clear();
    exec::check_guards(&p, &no_copies, opts).unwrap();
    // And the flag lifts both.
    exec::check_guards(
        &p,
        &full,
        Options {
            allow_mass_removal: true,
        },
    )
    .unwrap();
}

// Prevents (audit M8) checkpoint backups piling up, one per write.
#[test]
fn only_the_latest_checkpoint_backup_exists_and_none_is_left_after_a_good_sync() {
    let w = removal_then_501_copies();
    let p = w.prepare();
    let work = exec::full_work(&p);
    // Per write: the checkpoint backups there are as it starts, and their
    // stamps.
    let seen: RefCell<Vec<Vec<String>>> = RefCell::new(Vec::new());
    let progress = |e: Progress| {
        if matches!(e, Progress::Writing) {
            seen.borrow_mut().push(
                checkpoint_backups(&w)
                    .into_iter()
                    .map(|b| b.stamp)
                    .collect(),
            );
        }
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
    let seen = seen.into_inner();
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert!(seen[0].is_empty(), "the first write takes none: {seen:?}");
    assert_eq!(seen[1].len(), 1, "{seen:?}");
    assert_eq!(seen[2].len(), 1, "the earlier one went: {seen:?}");
    assert_ne!(seen[1], seen[2]);
    // After the sync only the backup taken before it is left.
    assert!(checkpoint_backups(&w).is_empty());
    let left = w.backups();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].stamp, out.backup.stamp);
    assert!(left[0].dir.join("work.json").is_file());
    let dirs: Vec<_> = fs::read_dir(w.paths.data_dir.join("backups").join(GUID))
        .unwrap()
        .collect();
    assert_eq!(dirs.len(), 1, "no stray folder: {dirs:?}");
}

// Prevents (audit M8) an extra backup for a sync with a single write.
#[test]
fn a_sync_with_one_write_takes_one_backup() {
    let mut w = World::new(3);
    w.add_new("one.mp3", Kind::Mp3, 21);
    let p = w.prepare();
    let work = exec::full_work(&p);
    let during = RefCell::new(Vec::new());
    let progress = |e: Progress| {
        if matches!(e, Progress::Writing) {
            during.borrow_mut().push(w.backups().len());
        }
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
    assert_eq!(during.into_inner(), vec![1]);
    assert_eq!(w.backups().len(), 1);
    assert_eq!(w.backups()[0].stamp, out.backup.stamp);
    assert!(checkpoint_backups(&w).is_empty());
}

// ---- playlist takeovers (audit M9)

/// A file "Stray" for the iPod playlist of that name, which the iPod alone
/// holds as [track 2]; the file has `files`.
fn with_stray_file(w: &World, files: &[usize]) {
    let list: Vec<PathBuf> = files.iter().map(|&i| w.files[i].clone()).collect();
    w.write_list("Stray", &list);
    w.reload();
}

// Prevents (audit M9) a new playlist file silently taking the tracks out of
// an iPod playlist iprune never managed.
#[test]
fn a_takeover_that_drops_tracks_is_refused_before_anything_is_written_unless_allowed() {
    let w = World::new(3);
    with_stray_file(&w, &[0, 2]); // the iPod's has track 2 only
    let p = w.prepare();
    let work = exec::full_work(&p);
    assert_eq!(p.plan.takeovers.len(), 1);
    let tree = w.tree();
    let e = w
        .run(
            &p,
            &work,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
    match &e {
        Error::PlaylistTakeover(names) => {
            assert_eq!(names.len(), 1, "{names:?}");
            assert!(names[0].contains("Stray"), "{names:?}");
        }
        other => panic!("expected PlaylistTakeover, got {other}"),
    }
    assert_eq!(w.tree(), tree, "the iPod is as it was");
    assert!(w.backups().is_empty(), "no backup: no write began");
    assert_eq!(w.playlist("Stray"), Some(vec![dbid(2)]));

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
    assert!(out.applied > 0);
    assert_eq!(w.playlist("Stray"), Some(vec![dbid(1), dbid(3)]));
}

#[test]
fn a_takeover_that_only_adds_tracks_is_not_refused() {
    let w = World::new(3);
    with_stray_file(&w, &[1, 0]); // track 2 kept, track 1 added
    let p = w.prepare();
    assert_eq!(p.plan.takeovers.len(), 1);
    assert_eq!(p.plan.takeovers[0].dropped, 0);
    exec::check_takeovers(&p.plan, Options::default()).unwrap();
    w.sync().unwrap();
    assert_eq!(w.playlist("Stray"), Some(vec![dbid(2), dbid(1)]));
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
        &w.env.cfg,
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

    // Prevents (audit L38) a track whose file is back at its path, but which
    // the last scan has not seen yet, being forgotten with its play counts.
    #[test]
    fn a_long_missing_track_whose_file_is_back_is_not_forgotten() {
        let mut w = World::new(3);
        let now = now_unix();
        let id = ghost(&mut w, now - FORGET_AFTER_SECS - 1);
        let path: PathBuf = w
            .env
            .store
            .conn()
            .query_row("SELECT path FROM tracks WHERE id = ?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
            .into();
        let contents = w.contents();
        let found = |w: &World| {
            doctor::examine(
                w.env.store.conn(),
                &w.root,
                &contents,
                Some(w.device()),
                now,
            )
            .unwrap()
            .forgotten
            .iter()
            .map(|(i, _)| *i)
            .collect::<Vec<_>>()
        };
        assert_eq!(
            found(&w),
            vec![id],
            "set up: forgotten while the file is gone"
        );
        fs::write(&path, b"back").unwrap();
        assert!(found(&w).is_empty(), "the file is back: kept");
    }

    // Prevents (audit L38) a repair acting on a report that went stale
    // between the examination and the fix: a scan since found the file.
    #[test]
    fn fix_index_leaves_a_candidate_a_scan_found_again() {
        let mut w = World::new(3);
        let now = now_unix();
        let id = ghost(&mut w, now - FORGET_AFTER_SECS - 1);
        let (with, all_items) = items_of(&w, id);
        let contents = w.contents();
        let r = doctor::examine(
            w.env.store.conn(),
            &w.root,
            &contents,
            Some(w.device()),
            now,
        )
        .unwrap();
        assert_eq!(r.forgotten.len(), 1, "set up");
        let conn = w.env.store.conn();
        conn.execute("UPDATE tracks SET missing_since = NULL WHERE id = ?1", [id])
            .unwrap();
        doctor::fix_index(conn, Some(w.device()), &r).unwrap();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [id], |r| r.get(0)).unwrap() };
        assert_eq!(count("SELECT COUNT(*) FROM tracks WHERE id = ?1"), 1);
        assert_eq!(
            count("SELECT plays FROM stats WHERE track_id = ?1"),
            4,
            "its play counts stay"
        );
        assert_eq!(
            items_of(&w, id),
            (with, all_items),
            "and its playlist entry"
        );

        // Still missing, but mapped to a device since: kept too.
        let conn = w.env.store.conn();
        conn.execute(
            "UPDATE tracks SET missing_since = ?2 WHERE id = ?1",
            [id, now - FORGET_AFTER_SECS - 1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO device_tracks (device_id, track_id, dbid, synced_tags_hash, synced_size, synced_duration_ms) \
             VALUES (?1, ?2, 77, x'00', 1, 1)",
            [w.device(), id],
        )
        .unwrap();
        doctor::fix_index(conn, Some(w.device()), &r).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM tracks WHERE id = ?1", [id], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
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
    fn orphans_are_moved_aside_unless_there_are_many_compared_with_the_tracks() {
        // A fake iPod: only the Music layout matters to the move.
        let dir = tempfile::tempdir().unwrap();
        let ipod = dir.path();
        let folder = ipod.join("iPod_Control/Music/F07");
        fs::create_dir_all(&folder).unwrap();
        let content = |n: usize, i: usize| format!("bytes of orphan {n}-{i}").into_bytes();
        let round = std::cell::Cell::new(0);
        let make = |n: usize| -> Report {
            round.set(round.get() + 1);
            Report {
                orphan_files: (0..n)
                    .map(|i| {
                        // Names differ per round: two moves in the same
                        // second share a stamp folder.
                        let f = folder.join(format!("o{n}-{i}-r{}.mp3", round.get()));
                        fs::write(&f, content(n, i)).unwrap();
                        f
                    })
                    .collect(),
                ..Report::default()
            }
        };
        let gone = |r: &Report| r.orphan_files.iter().filter(|f| !f.exists()).count();
        // Every file under the quarantine folder, by name, with its bytes and
        // the folder it sits in relative to the stamp.
        let quarantined = || -> Vec<(String, String, Vec<u8>)> {
            let mut out = Vec::new();
            let root = ipod.join(doctor::ORPHANS);
            for stamp in fs::read_dir(&root).into_iter().flatten() {
                for fold in fs::read_dir(stamp.unwrap().path()).unwrap() {
                    let fold = fold.unwrap().path();
                    for f in fs::read_dir(&fold).unwrap() {
                        let f = f.unwrap().path();
                        out.push((
                            fold.file_name().unwrap().to_string_lossy().into_owned(),
                            f.file_name().unwrap().to_string_lossy().into_owned(),
                            fs::read(&f).unwrap(),
                        ));
                    }
                }
            }
            out.sort();
            out
        };
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
        assert_eq!(gone(&r), 0, "nothing moved when refused");
        assert!(!ipod.join(doctor::ORPHANS).exists(), "no folder made");
        // Allowed, they all move, keeping their bytes and their folder name.
        assert_eq!(doctor::delete_orphans(&r, 10, true).unwrap(), 6);
        assert_eq!(gone(&r), 6);
        let want: Vec<_> = (0..6)
            .map(|i| ("F07".to_owned(), format!("o6-{i}-r1.mp3"), content(6, i)))
            .collect();
        assert_eq!(quarantined(), want);
        // Six among a hundred: under a tenth.
        let r = make(7);
        assert_eq!(doctor::delete_orphans(&r, 100, false).unwrap(), 7);
        assert_eq!(gone(&r), 7);
        // Five among two: not over five.
        let r = make(5);
        assert_eq!(doctor::delete_orphans(&r, 2, false).unwrap(), 5);
        assert_eq!(gone(&r), 5);
        // Exactly a tenth is not over it.
        let r = make(6);
        assert_eq!(doctor::delete_orphans(&r, 60, false).unwrap(), 6);
        assert_eq!(gone(&r), 6);
        assert_eq!(quarantined().len(), 6 + 7 + 5 + 6, "none lost on the way");
        assert_eq!(
            doctor::delete_orphans(&Report::default(), 0, false).unwrap(),
            0
        );
    }

    // Prevents (audit L16) orphan files being deleted outright, and the
    // files set aside being reported (and moved again) by the next run.
    #[test]
    fn moved_orphans_keep_their_bytes_and_are_not_orphans_again() {
        let w = World::new(3);
        fs::write(music(&w, "F05/LOST.mp3"), b"lost one").unwrap();
        fs::write(music(&w, "F49/also lost.m4a"), b"lost two").unwrap();
        let used = w.file_of(dbid(2));
        let used_bytes = fs::read(&used).unwrap();
        let r = examine(&w);
        assert_eq!(r.orphan_files.len(), 2, "{r:?}");

        let tracks = w.contents().tracks.len();
        assert_eq!(doctor::delete_orphans(&r, tracks, false).unwrap(), 2);

        assert!(!music(&w, "F05/LOST.mp3").exists());
        assert!(!music(&w, "F49/also lost.m4a").exists());
        let stamps: Vec<_> = fs::read_dir(w.root.join(doctor::ORPHANS))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(stamps.len(), 1, "{stamps:?}");
        assert_eq!(
            fs::read(stamps[0].join("F05/LOST.mp3")).unwrap(),
            b"lost one"
        );
        assert_eq!(
            fs::read(stamps[0].join("F49/also lost.m4a")).unwrap(),
            b"lost two"
        );
        // A file a track uses is not touched.
        assert_eq!(fs::read(&used).unwrap(), used_bytes);
        // And the next run finds nothing more to move.
        let again = examine(&w);
        assert!(again.orphan_files.is_empty(), "{again:?}");
        assert_eq!(doctor::delete_orphans(&again, tracks, false).unwrap(), 0);
        assert_eq!(
            fs::read_dir(w.root.join(doctor::ORPHANS)).unwrap().count(),
            1
        );
    }

    // Prevents (audit L16) two moves in the same second sharing a folder, the
    // second renaming over the first's file of the same name.
    #[test]
    fn moves_in_the_same_second_go_to_different_folders() {
        let dir = tempfile::tempdir().unwrap();
        let ipod = dir.path();
        let folder = ipod.join("iPod_Control/Music/F07");
        fs::create_dir_all(&folder).unwrap();
        const ROUNDS: usize = 20; // well inside one second, most of the time
        for i in 0..ROUNDS {
            let f = folder.join("same.mp3");
            fs::write(&f, format!("round {i}")).unwrap();
            let r = Report {
                orphan_files: vec![f],
                ..Report::default()
            };
            assert_eq!(doctor::delete_orphans(&r, 1, false).unwrap(), 1);
        }
        let mut seen = Vec::new();
        for stamp in fs::read_dir(ipod.join(doctor::ORPHANS)).unwrap() {
            let f = stamp.unwrap().path().join("F07/same.mp3");
            seen.push(String::from_utf8(fs::read(&f).unwrap()).unwrap());
        }
        seen.sort();
        let mut want: Vec<String> = (0..ROUNDS).map(|i| format!("round {i}")).collect();
        want.sort();
        assert_eq!(seen, want, "every round kept in a folder of its own");
    }

    // Prevents (audit L16) a file counted as moved that was not.
    #[test]
    fn an_orphan_with_an_unexpected_path_is_neither_moved_nor_counted() {
        let dir = tempfile::tempdir().unwrap();
        let ipod = dir.path();
        let folder = ipod.join("iPod_Control/Music/F07");
        fs::create_dir_all(&folder).unwrap();
        let good = folder.join("good.mp3");
        fs::write(&good, b"good").unwrap();
        // Not under <mountpoint>/iPod_Control/Music/<folder>/.
        let odd = ipod.join("odd.mp3");
        fs::write(&odd, b"odd").unwrap();
        let r = Report {
            orphan_files: vec![odd.clone(), good.clone()],
            ..Report::default()
        };
        assert_eq!(doctor::delete_orphans(&r, 2, false).unwrap(), 1);
        assert_eq!(fs::read(&odd).unwrap(), b"odd", "left where it was");
        assert!(!good.exists());
    }

    // Prevents (audit M9) doctor's repair, which copies nothing, being
    // refused as a mass re-copy of tracks whose files look missing.
    #[test]
    fn a_repair_that_copies_nothing_is_not_blocked_by_many_missing_files_in_the_plan() {
        let w = World::new(8);
        for k in 1..=6 {
            fs::remove_file(w.file_of(dbid(k))).unwrap();
        }
        fs::remove_file(w.file_of(ORPHAN)).unwrap();
        let p = w.prepare();
        assert_eq!(
            p.plan.replace.iter().filter(|r| r.file_missing).count(),
            6,
            "set up: the plan would copy six tracks again"
        );
        assert!(
            exec::check_missing_files(&p.plan, p.tracks_map.len(), Options::default()).is_err()
        );
        // The report names only the unmanaged entry: what doctor would remove
        // without touching a managed track.
        let mut report = examine(&w);
        report.missing_files.retain(|(d, _)| *d == ORPHAN);
        assert_eq!(report.missing_files.len(), 1);
        let work = doctor::repair_work(&p, &report);
        assert!(work.tracks.copies.is_empty());
        let out = w
            .run(
                &p,
                &work,
                Options::default(),
                &|_| {},
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(out.removed, 1);
        assert!(w.track(ORPHAN).is_none());
        assert!(w.track(dbid(1)).is_some(), "managed entries are kept");
        assert_eq!(w.mapping().len(), 8);
    }

    // Prevents (audit M9) doctor's repair, which changes no playlist, being
    // refused because the library has a playlist that would take one over.
    #[test]
    fn a_repair_is_not_blocked_by_a_takeover_pending_elsewhere_in_the_plan() {
        let w = World::new(4);
        // "Stray" [2] is only on the iPod; a file of that name would drop it.
        w.write_list("Stray", &[w.files[2].clone()]);
        w.reload();
        fs::remove_file(w.file_of(ORPHAN)).unwrap();
        let p = w.prepare();
        assert_eq!(p.plan.takeovers.len(), 1, "set up: {:?}", p.plan.takeovers);
        assert_eq!(p.plan.takeovers[0].dropped, 1);
        let report = examine(&w);
        let work = doctor::repair_work(&p, &report);
        assert!(work.playlists.ops.is_empty());
        w.run(
            &p,
            &work,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(w.track(ORPHAN).is_none());
        assert_eq!(w.playlist("Stray"), Some(vec![dbid(2)]), "not taken over");
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
    exec::restore(
        w.env.store.conn(),
        &w.env.cfg,
        &w.paths,
        &w.root,
        GUID,
        stamp,
        discard_play_counts,
    )
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

// ---- cover art: ArtworkDB, the .ithmb files and backups of them (audit H9, H3)

mod artwork_tests {
    use super::*;
    use common::{add_cover_art, read_covers};

    /// A World whose iPod has a cover on every track but the unmanaged one.
    fn art_world(n: usize) -> World {
        let w = World::new(n);
        add_cover_art(&w.root, SYSINFO);
        w
    }

    fn artwork_dir(w: &World) -> BTreeMap<PathBuf, Vec<u8>> {
        tree(&w.root.join("iPod_Control/Artwork"))
    }

    fn ithmbs(w: &World) -> BTreeMap<PathBuf, Vec<u8>> {
        artwork_dir(w)
            .into_iter()
            .filter(|(p, _)| p.extension().is_some_and(|e| e == "ithmb"))
            .collect()
    }

    fn mix_gets_a_second_track(w: &World) {
        w.write_list("Mix", &[w.files[0].clone(), w.files[1].clone()]);
        w.reload();
    }

    fn backups_dir(w: &World) -> PathBuf {
        w.paths.data_dir.join("backups")
    }

    // Prevents (audit H9) a sync deleting every cover on the iPod because
    // its ArtworkDB was gone or unreadable: libgpod then sees no thumbnails
    // and deletes the .ithmb files nothing refers to.
    fn damaged_artworkdb_is_refused_and_nothing_changes(damage: impl Fn(&Path)) {
        let w = art_world(4);
        let artdb = w.root.join("iPod_Control/Artwork/ArtworkDB");
        assert!(artdb.is_file());
        assert!(!ithmbs(&w).is_empty(), "set up: covers exist");
        w.write_play_counts(&[
            (2, 1_700_000_000, 80, 0),
            (0, 0, 0, 0),
            (0, 0, 0, 0),
            (0, 0, 0, 0),
            (0, 0, 0, 0),
        ]);
        damage(&artdb);
        mix_gets_a_second_track(&w);
        let before = w.tree();

        let e = w.sync().unwrap_err();
        match &e {
            Error::ArtworkUnreadable { files, lost } => {
                assert!(*files > 0, "{e}");
                assert_eq!(*lost, 4, "every track with a cover lost it: {e}");
            }
            other => panic!("expected ArtworkUnreadable, got {other}"),
        }
        // Not a check failure of a written database: nothing was written.
        assert!(!e.is_check_failure(), "no write, so not exit code 3");
        // Every file on the iPod is as it was, the .ithmb files included.
        assert_eq!(w.tree(), before);
        // No backup, no pull, no changed playlist.
        assert!(w.backups().is_empty());
        assert!(!backups_dir(&w).exists(), "no backup folder at all");
        assert_eq!(w.stats_total_plays(), 0);
        assert_eq!(w.pending_pulls(), 0);
    }

    #[test]
    fn a_sync_refuses_when_artworkdb_is_missing_and_deletes_no_cover() {
        damaged_artworkdb_is_refused_and_nothing_changes(|artdb| fs::remove_file(artdb).unwrap());
    }

    #[test]
    fn a_sync_refuses_when_artworkdb_is_garbage_and_deletes_no_cover() {
        damaged_artworkdb_is_refused_and_nothing_changes(|artdb| {
            fs::write(artdb, b"this is not an ArtworkDB, not even close\0\0\0").unwrap();
        });
    }

    #[test]
    fn a_sync_refuses_when_artworkdb_is_empty_and_deletes_no_cover() {
        damaged_artworkdb_is_refused_and_nothing_changes(|artdb| fs::write(artdb, b"").unwrap());
    }

    #[test]
    fn a_sync_goes_ahead_when_the_artwork_is_healthy_and_keeps_every_cover() {
        let w = art_world(4);
        let covers = read_covers(&w.root);
        let ithmb_before = ithmbs(&w);
        mix_gets_a_second_track(&w);
        let out = w.sync().unwrap();
        assert_eq!(out.stopped, None);
        assert_eq!(w.playlist("Mix").unwrap(), vec![dbid(1), dbid(2)]);
        assert_eq!(read_covers(&w.root), covers, "every cover is still found");
        assert_eq!(
            ithmbs(&w),
            ithmb_before,
            "a playlist edit leaves the .ithmb files alone"
        );
        assert_eq!(w.backups().len(), 1);
    }

    #[test]
    fn an_ipod_without_any_artwork_still_syncs() {
        // The plain fake iPod: no ArtworkDB, no .ithmb file, no thumbnails.
        let w = World::new(3);
        mix_gets_a_second_track(&w);
        w.sync().unwrap();
        assert_eq!(w.playlist("Mix").unwrap(), vec![dbid(1), dbid(2)]);
    }

    // ---- audit H3: the .ithmb files libgpod rewrites on every write

    /// Removes the first managed track (its cover is first in the .ithmb
    /// files) from the library and syncs with removals allowed.
    fn sync_removing_the_first_track(w: &mut World) -> iprune_core::Result<Outcome> {
        sync_removing_the_first_track_with(w, &|_| {})
    }

    fn sync_removing_the_first_track_with(
        w: &mut World,
        progress: &dyn Fn(Progress),
    ) -> iprune_core::Result<Outcome> {
        fs::remove_file(&w.files[0]).unwrap();
        w.rescan();
        let p = w.prepare();
        let work = exec::full_work(&p);
        w.run(
            &p,
            &work,
            Options {
                allow_mass_removal: true,
            },
            progress,
            &AtomicBool::new(false),
        )
    }

    fn store_of(w: &World) -> PathBuf {
        backups_dir(w).join(GUID).join(".ithmb")
    }

    fn stored(w: &World) -> BTreeMap<String, Vec<u8>> {
        fs::read_dir(store_of(w))
            .map(|rd| {
                rd.map(|e| e.unwrap())
                    .map(|e| {
                        (
                            e.file_name().to_string_lossy().into_owned(),
                            fs::read(e.path()).unwrap(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    // Prevents (audit H3) a backup that holds the old ArtworkDB but not the
    // .ithmb files it points into: libgpod moves thumbnails around on a write.
    #[test]
    fn removing_a_track_with_a_cover_rearranges_the_ithmb_files_and_the_backup_keeps_the_old_ones()
    {
        let mut w = art_world(4);
        let ithmb_before = ithmbs(&w);
        let artdb_before = artwork_dir(&w);
        let covers_before = read_covers(&w.root);
        let out = sync_removing_the_first_track(&mut w).unwrap();
        assert_eq!(out.removed, 1);

        // The scenario is real: libgpod did rewrite the files.
        assert_ne!(
            ithmbs(&w),
            ithmb_before,
            "set up: the removal must rearrange the .ithmb files"
        );

        // The backup's store holds exactly the content from before the write,
        // under its hash, and the list says which file is which.
        let b = w.backups().pop().unwrap();
        let list = backup::artwork_list(&b.dir).expect("an artwork list");
        assert_eq!(list.len(), ithmb_before.len());
        let store = stored(&w);
        let distinct: std::collections::BTreeSet<_> = ithmb_before.values().collect();
        assert_eq!(store.len(), distinct.len(), "one stored file per content");
        for (path, bytes) in &ithmb_before {
            let name = path.file_name().unwrap().to_string_lossy();
            let item = list
                .iter()
                .find(|i| i.name == *name)
                .unwrap_or_else(|| panic!("{name} not listed"));
            assert_eq!(item.size, bytes.len() as u64);
            assert_eq!(
                item.hash,
                blake3::hash(bytes).to_hex().to_string(),
                "{name}"
            );
            assert_eq!(&store[&item.hash], bytes, "{name} stored as it was");
        }
        assert_eq!(
            fs::read(b.dir.join("ArtworkDB")).unwrap(),
            artdb_before[Path::new("ArtworkDB")]
        );

        // Restoring that backup makes the covers consistent again: same
        // .ithmb bytes, same ArtworkDB, every track shows its own colour.
        assert_ne!(
            read_covers(&w.root),
            covers_before,
            "set up: after the write"
        );
        backup::restore_files(&b, &w.root).unwrap();
        assert_eq!(ithmbs(&w), ithmb_before);
        assert_eq!(artwork_dir(&w), artdb_before);
        assert_eq!(read_covers(&w.root), covers_before);
    }

    #[test]
    fn a_second_sync_that_changes_no_cover_stores_no_new_ithmb_file() {
        let w = art_world(4);
        mix_gets_a_second_track(&w);
        w.sync().unwrap();
        let store1 = stored(&w);
        let list1 = backup::artwork_list(&w.backups()[0].dir).unwrap();
        assert!(!store1.is_empty());
        let mtimes = |w: &World| -> BTreeMap<String, std::time::SystemTime> {
            fs::read_dir(store_of(w))
                .unwrap()
                .map(|e| e.unwrap())
                .map(|e| {
                    (
                        e.file_name().to_string_lossy().into_owned(),
                        e.metadata().unwrap().modified().unwrap(),
                    )
                })
                .collect()
        };
        let mtimes1 = mtimes(&w);

        // Another playlist-only change: a second backup is taken.
        w.write_list("Mix", &[w.files[2].clone()]);
        w.reload();
        w.sync().unwrap();
        let backups = w.backups();
        assert_eq!(backups.len(), 2);
        let list2 = backup::artwork_list(&backups[1].dir).unwrap();
        assert_eq!(
            list2.iter().map(|i| (&i.name, &i.hash)).collect::<Vec<_>>(),
            list1.iter().map(|i| (&i.name, &i.hash)).collect::<Vec<_>>(),
            "the same covers, the same hashes"
        );
        assert_eq!(stored(&w), store1, "no new file in the store");
        assert_eq!(mtimes(&w), mtimes1, "and none rewritten");
        let leftovers: Vec<_> = fs::read_dir(store_of(&w))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    // Prevents (audit H3c) a write that fails after libgpod rearranged the
    // .ithmb files, but before ArtworkDB and iTunesDB changed, being reported
    // as "nothing was written" and the stats pull being undone.
    #[test]
    #[cfg(unix)]
    fn a_write_that_fails_after_the_ithmb_files_moved_is_reported_as_incomplete() {
        use std::os::unix::fs::PermissionsExt;
        let mut w = art_world(4);
        let ithmb_before = ithmbs(&w);
        let artdb = fs::read(w.root.join("iPod_Control/Artwork/ArtworkDB")).unwrap();
        let itunesdb = fs::read(w.root.join("iPod_Control/iTunes/iTunesDB")).unwrap();
        w.write_play_counts(&[
            (3, 1_700_000_000, 80, 0),
            (0, 0, 0, 0),
            (0, 0, 0, 0),
            (0, 0, 0, 0),
            (0, 0, 0, 0),
        ]);
        let counts_path = w.root.join("iPod_Control/iTunes/Play Counts");
        let counts = fs::read(&counts_path).unwrap();
        // libgpod rewrites the .ithmb files first, then ArtworkDB, then
        // iTunesDB. ArtworkDB can be neither written in place (read-only
        // file) nor replaced (read-only folder) and libgpod does not fail for
        // that; the iTunes folder is read-only too, which makes the write
        // fail. Only the .ithmb files (rewritten in place) change.
        //
        // The folders are locked as the write starts, not before: iprune
        // moves `Play Counts` aside first, in the iTunes folder, and a sync
        // that cannot do that never reaches libgpod.
        struct Restore(PathBuf, u32);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(self.1));
            }
        }
        let target = w.root.join("iPod_Control/Artwork/ArtworkDB");
        let itunes = w.root.join("iPod_Control/iTunes");
        let art_dir = w.root.join("iPod_Control/Artwork");
        let guards: RefCell<Vec<Restore>> = RefCell::new(Vec::new());
        let blocked = RefCell::new(true);
        let lock = |progress: Progress| {
            if !matches!(progress, Progress::Writing) {
                return;
            }
            fs::set_permissions(&target, fs::Permissions::from_mode(0o444)).unwrap();
            guards.borrow_mut().push(Restore(target.clone(), 0o644));
            fs::set_permissions(&itunes, fs::Permissions::from_mode(0o555)).unwrap();
            guards.borrow_mut().push(Restore(itunes.clone(), 0o755));
            fs::set_permissions(&art_dir, fs::Permissions::from_mode(0o555)).unwrap();
            guards.borrow_mut().push(Restore(art_dir.clone(), 0o755));
            if fs::OpenOptions::new().append(true).open(&target).is_ok()
                || fs::File::create(itunes.join(".probe")).is_ok()
            {
                let _ = fs::remove_file(itunes.join(".probe"));
                *blocked.borrow_mut() = false;
            }
        };
        let res = sync_removing_the_first_track_with(&mut w, &lock);
        guards.borrow_mut().clear();
        if !*blocked.borrow() {
            eprintln!("running as root: permissions do not block writes, test skipped");
            return;
        }

        // Set up: ArtworkDB and iTunesDB did not change, the .ithmb files did.
        assert_eq!(
            fs::read(w.root.join("iPod_Control/Artwork/ArtworkDB")).unwrap(),
            artdb
        );
        assert_eq!(
            fs::read(w.root.join("iPod_Control/iTunes/iTunesDB")).unwrap(),
            itunesdb
        );
        assert_ne!(ithmbs(&w), ithmb_before, "set up: libgpod moved thumbnails");

        let e = res.unwrap_err();
        let stamp = w.backups().last().unwrap().stamp.clone();
        match &e {
            Error::WriteIncomplete { stamp: s, .. } => assert_eq!(*s, stamp),
            other => panic!("expected WriteIncomplete, got {other}"),
        }
        // The pull is left pending, not undone as for an untouched iPod, and
        // `Play Counts` stays where iprune moved it, so its plays are not
        // pulled a second time.
        assert_eq!(w.pending_pulls(), 1);
        assert!(!counts_path.exists());
        assert_eq!(
            fs::read(counts_path.with_file_name("Play Counts.bak")).unwrap(),
            counts
        );
    }

    // Prevents (audit H3) a sync reporting success when libgpod rearranged the
    // .ithmb files but silently failed to write ArtworkDB: the covers would be
    // scrambled and the music files of removed tracks deleted.
    #[test]
    #[cfg(unix)]
    fn a_silently_unwritten_artworkdb_after_a_rearrangement_fails_verification_and_restore_repairs_it()
     {
        use std::os::unix::fs::PermissionsExt;
        let mut w = art_world(4);
        let ithmb_before = ithmbs(&w);
        let art_before = artwork_dir(&w);
        let covers_before = read_covers(&w.root);
        let doomed = w.file_of(dbid(1));
        assert!(doomed.is_file());
        let target = w.root.join("iPod_Control/Artwork/ArtworkDB");
        // The file read-only is not enough (libgpod replaces it); the folder
        // is read-only too, so ArtworkDB can be neither written nor replaced,
        // while the existing .ithmb files are still rewritten in place.
        let art_dir = w.root.join("iPod_Control/Artwork");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(&art_dir, fs::Permissions::from_mode(0o555)).unwrap();
        if fs::OpenOptions::new().append(true).open(&target).is_ok()
            || fs::File::create(art_dir.join(".probe")).is_ok()
        {
            let _ = fs::remove_file(art_dir.join(".probe"));
            fs::set_permissions(&art_dir, fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("running as root: permissions do not block writes, test skipped");
            return;
        }
        let res = sync_removing_the_first_track(&mut w);
        fs::set_permissions(&art_dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();

        // Set up: the .ithmb files moved, ArtworkDB did not.
        assert_ne!(ithmbs(&w), ithmb_before, "set up: libgpod moved thumbnails");
        assert_eq!(
            fs::read(&target).unwrap(),
            art_before[Path::new("ArtworkDB")],
            "set up: ArtworkDB was not rewritten"
        );

        let e = res.unwrap_err();
        let stamp = w.backups().last().unwrap().stamp.clone();
        match &e {
            Error::Verification { stamp: s, problems } => {
                assert_eq!(*s, stamp);
                assert!(!problems.is_empty());
            }
            other => panic!("expected Verification, got {other}"),
        }
        assert!(e.is_check_failure(), "exit code 3");
        assert!(doomed.is_file(), "the removed track's file was not deleted");

        // The backup puts the covers right.
        assert_ne!(read_covers(&w.root), covers_before, "set up: scrambled");
        let b = w.backups().pop().unwrap();
        backup::restore_files(&b, &w.root).unwrap();
        assert_eq!(ithmbs(&w), ithmb_before);
        assert_eq!(read_covers(&w.root), covers_before);
    }

    // An iPod with .ithmb files nothing points at (no track has a cover) is
    // not refused, and the files are left as they are by a playlist edit.
    #[test]
    fn leftover_ithmb_files_no_track_points_at_do_not_stop_a_sync() {
        let w = World::new(3);
        let dir = w.root.join("iPod_Control/Artwork");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("F1060_1.ithmb"), vec![9u8; 2048]).unwrap();
        mix_gets_a_second_track(&w);
        w.sync().unwrap();
        assert_eq!(w.playlist("Mix").unwrap(), vec![dbid(1), dbid(2)]);
        assert_eq!(
            fs::read(dir.join("F1060_1.ithmb")).unwrap(),
            vec![9u8; 2048]
        );
    }

    // ---- exec::restore refuses before it changes anything (audit H3)

    /// A sync removed a track with a cover (so the live `.ithmb` files differ
    /// from the backup's), and the iPod has played since: a live Play Counts.
    fn removed_with_art_and_played() -> (World, backup::Backup) {
        let mut w = art_world(4);
        sync_removing_the_first_track(&mut w).unwrap();
        w.write_play_counts(&[
            (2, 1_700_000_000, 0, 0),
            (1, 1_700_000_100, 0, 0),
            (0, 0, 0, 0),
            (0, 0, 0, 0),
        ]);
        let b = w.backups().pop().unwrap();
        (w, b)
    }

    /// The listed files whose live content differs, with their store paths.
    fn needed_store_files(w: &World, b: &backup::Backup) -> Vec<(String, PathBuf)> {
        backup::artwork_list(&b.dir)
            .unwrap()
            .into_iter()
            .filter(|i| {
                fs::read(w.root.join("iPod_Control/Artwork").join(&i.name))
                    .map(|live| blake3::hash(&live).to_hex().to_string() != i.hash)
                    .unwrap_or(true)
            })
            .map(|i| (i.name, store_of(w).join(i.hash)))
            .collect()
    }

    fn backup_folders(w: &World) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(backups_dir(w).join(GUID))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// Runs `exec::restore` and checks that it refused and changed nothing.
    fn refused_restore_changes_nothing(w: &World, b: &backup::Backup) -> Error {
        let counts = w.root.join("iPod_Control/iTunes/Play Counts");
        let counts_before = fs::read(&counts).unwrap();
        let tree_before = w.tree();
        let folders_before = backup_folders(w);
        let e = restore(w, &b.stamp, false).unwrap_err();
        assert_eq!(
            devices::restoring(w.env.store.conn(), w.device()).unwrap(),
            None,
            "no restore marker"
        );
        assert_eq!(
            fs::read(&counts).unwrap(),
            counts_before,
            "Play Counts stays"
        );
        assert_eq!(w.stats_total_plays(), 0, "no stats pulled");
        assert_eq!(w.pending_pulls(), 0);
        assert_eq!(backup_folders(w), folders_before, "no new backup folder");
        assert_eq!(w.tree(), tree_before, "the iPod is unchanged");
        e
    }

    fn a_sync_still_goes_through(w: &World) {
        w.write_list("Mix", &[w.files[2].clone(), w.files[3].clone()]);
        w.reload();
        w.sync()
            .expect("a sync is not blocked after a refused restore");
    }

    #[test]
    fn restore_with_an_unreadable_artwork_list_refuses_before_changing_anything() {
        let (w, b) = removed_with_art_and_played();
        assert!(!needed_store_files(&w, &b).is_empty(), "set up");
        fs::write(b.dir.join("artwork.json"), "{ not json").unwrap();
        let e = refused_restore_changes_nothing(&w, &b);
        assert!(
            matches!(&e, Error::Io { path, .. } if path.ends_with("artwork.json")),
            "{e}"
        );
        a_sync_still_goes_through(&w);
    }

    #[test]
    fn restore_with_a_deleted_store_file_refuses_before_changing_anything() {
        let (w, b) = removed_with_art_and_played();
        let needed = needed_store_files(&w, &b);
        assert!(!needed.is_empty(), "set up");
        let gone = &needed[0].1;
        fs::remove_file(gone).unwrap();
        let e = refused_restore_changes_nothing(&w, &b);
        match &e {
            Error::Io { path, source } => {
                assert_eq!(path, gone);
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected Io NotFound, got {other}"),
        }
        a_sync_still_goes_through(&w);
    }

    #[test]
    fn restore_without_room_for_its_copies_refuses_before_changing_anything() {
        let (w, b) = removed_with_art_and_played();
        let (name, _) = needed_store_files(&w, &b).remove(0);
        let path = b.dir.join("artwork.json");
        let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for item in v.as_array_mut().unwrap() {
            if item["name"] == name.as_str() {
                item["size"] = (1u64 << 62).into();
            }
        }
        fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
        let e = refused_restore_changes_nothing(&w, &b);
        match &e {
            Error::NoRoomToRestore { needed, free } => assert!(needed > free, "{e}"),
            other => panic!("expected NoRoomToRestore, got {other}"),
        }
        a_sync_still_goes_through(&w);
    }

    #[test]
    fn restore_with_a_truncated_store_file_refuses_before_changing_anything() {
        let (w, b) = removed_with_art_and_played();
        let needed = needed_store_files(&w, &b);
        assert!(!needed.is_empty(), "set up");
        let victim = &needed[0].1;
        let bytes = fs::read(victim).unwrap();
        assert!(bytes.len() > 1, "set up");
        fs::write(victim, &bytes[..bytes.len() / 2]).unwrap();
        let e = refused_restore_changes_nothing(&w, &b);
        assert!(
            matches!(&e, Error::BackupMismatch(p) if p == victim),
            "expected BackupMismatch, got {e}"
        );
        a_sync_still_goes_through(&w);
    }
}

// ---- audit step 3: a prompt that waited (H1), signed databases (M12),
// deletions that follow a damaged path (L1, L2) and folders as iPods (M6)

mod step3 {
    use super::*;
    use iprune_core::config::Config;

    fn sysinfo_path(w: &World) -> PathBuf {
        w.root.join("iPod_Control/Device/SysInfo")
    }

    fn itunesdb_path(w: &World) -> PathBuf {
        w.root.join("iPod_Control/iTunes/iTunesDB")
    }

    /// Mix cycles through four different member lists, so each call leaves
    /// the iPod with a playlist to rewrite.
    fn nudge(w: &World, variant: usize) {
        let f = &w.files;
        let lists = [
            vec![f[2].clone()],
            vec![f[2].clone(), f[0].clone()],
            vec![f[0].clone(), f[2].clone()],
            vec![f[0].clone()],
        ];
        w.write_list("Mix", &lists[variant % 4]);
        w.reload();
    }

    /// The iPod and the index as they are, for "nothing changed" checks.
    #[derive(Debug, PartialEq)]
    struct State {
        tree: BTreeMap<PathBuf, Vec<u8>>,
        plays: i64,
        pending: i64,
        backups: usize,
        mapping: BTreeMap<i64, u64>,
    }

    fn state(w: &World) -> State {
        State {
            tree: w.tree(),
            plays: w.stats_total_plays(),
            pending: w.pending_pulls(),
            backups: w.backups().len(),
            mapping: w.mapping(),
        }
    }

    fn sync_with_cfg(
        w: &World,
        cfg: &Config,
        p: &Prepared,
        work: &FullWork,
    ) -> iprune_core::Result<Outcome> {
        exec::sync(
            w.env.store.conn(),
            cfg,
            &w.paths,
            &iprune_core::lock::ipod(&w.paths).unwrap(),
            &w.root,
            w.open(),
            p,
            work,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
    }

    // ---- H1

    /// What the CLI does around its prompt: takes the lock, opens and plans
    /// (the work to do is a rewritten Mix), lets `change` alter the iPod the
    /// way something else would while the prompt waits, then applies the
    /// plan with the database parsed before. It must be refused with nothing
    /// changed; `undo`, then the same sync run again from scratch, goes through.
    fn refused_after(setup: impl Fn(&World), change: impl Fn(&World), undo: impl Fn(&World)) {
        let w = World::new(3);
        setup(&w);
        nudge(&w, 1);
        let lock = iprune_core::lock::ipod(&w.paths).unwrap();
        let db = w.open();
        let p = sync::prepare(w.env.store.conn(), &w.env.cfg, &db).unwrap();
        let work = exec::full_work(&p);
        assert!(!work.is_empty(), "set up: there is something to write");
        change(&w);
        let before = state(&w);

        let e = exec::sync(
            w.env.store.conn(),
            &w.env.cfg,
            &w.paths,
            &lock,
            &w.root,
            db,
            &p,
            &work,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(matches!(e, Error::IpodChanged), "{e:?}");
        assert_eq!(state(&w), before, "the refusal changed something");
        assert_eq!(w.stats_total_plays(), 0, "plays were pulled");

        // The iPod as it was, run again: it reads the iPod and writes it.
        undo(&w);
        drop(lock);
        let out = w.sync().unwrap();
        assert!(out.applied > 0);
        assert_eq!(w.backups().len(), 1);
    }

    // Prevents (audit H1) writing a database parsed before a prompt over an
    // iTunesDB another program rewrote while the prompt waited.
    #[test]
    fn a_changed_itunesdb_since_it_was_read_is_refused_and_nothing_changes() {
        refused_after(
            |_| {},
            |w| {
                let mut other = w.open();
                other.create_playlist("Made by someone else").unwrap();
                other.write().unwrap();
            },
            |_| {},
        );
    }

    // Prevents (audit H1) a Play Counts that appeared during the prompt being
    // overwritten without its plays pulled: they would be lost.
    #[test]
    fn a_play_counts_that_appeared_since_it_was_read_is_refused_and_not_pulled() {
        refused_after(
            |_| {},
            |w| w.write_play_counts(&[(4, 1_700_000_000, 80, 0)]),
            |_| {},
        );
    }

    #[test]
    fn a_play_counts_that_grew_since_it_was_read_is_refused_and_not_pulled() {
        refused_after(
            |w| w.write_play_counts(&[(4, 1_700_000_000, 80, 0)]),
            |w| w.write_play_counts(&[(9, 1_700_000_500, 80, 0)]),
            |_| {},
        );
    }

    #[test]
    fn a_play_counts_removed_since_it_was_read_is_refused() {
        refused_after(
            |w| w.write_play_counts(&[(4, 1_700_000_000, 80, 0)]),
            |w| fs::remove_file(w.root.join("iPod_Control/iTunes/Play Counts")).unwrap(),
            |_| {},
        );
    }

    // Prevents (audit H1) iPod A's database being written over iPod B plugged
    // in at the same mountpoint. B has the same iTunesDB bytes here, so only
    // the GUID tells them apart.
    #[test]
    fn a_different_firewire_guid_since_it_was_read_is_refused() {
        refused_after(
            |_| {},
            |w| fs::write(sysinfo_path(w), "FirewireGuid: 0x000A2700BBBBBBBB\n").unwrap(),
            |w| fs::write(sysinfo_path(w), SYSINFO).unwrap(),
        );
    }

    #[test]
    fn a_vanished_guid_since_it_was_read_is_refused() {
        refused_after(
            |_| {},
            |w| fs::remove_file(sysinfo_path(w)).unwrap(),
            |w| fs::write(sysinfo_path(w), SYSINFO).unwrap(),
        );
    }

    // The identity is the iPod's content, not when it was looked at: putting
    // the same bytes back is the same iPod.
    #[test]
    fn identity_follows_the_content_of_each_part_and_nothing_else() {
        let w = World::new(3);
        let first = sync::identity(&w.root).unwrap();
        assert_eq!(sync::identity(&w.root).unwrap(), first);
        sync::check_identity(&w.root, &first).unwrap();
        assert_eq!(w.prepare().identity.as_ref(), Some(&first));

        let db = fs::read(itunesdb_path(&w)).unwrap();
        let sysinfo = fs::read(sysinfo_path(&w)).unwrap();
        let mut seen = vec![first.clone()];
        let mut differs = |what: &str, id: sync::Identity| {
            assert!(!seen.contains(&id), "{what} did not change the identity");
            seen.push(id);
        };

        let mut other = w.open();
        other.create_playlist("Elsewhere").unwrap();
        other.write().unwrap();
        drop(other);
        differs("the iTunesDB", sync::identity(&w.root).unwrap());
        fs::write(itunesdb_path(&w), &db).unwrap();
        assert_eq!(sync::identity(&w.root).unwrap(), first, "same bytes back");

        w.write_play_counts(&[(1, 1_700_000_000, 0, 0)]);
        differs("a Play Counts", sync::identity(&w.root).unwrap());
        w.write_play_counts(&[(2, 1_700_000_000, 0, 0)]);
        differs("other plays", sync::identity(&w.root).unwrap());
        fs::remove_file(w.root.join("iPod_Control/iTunes/Play Counts")).unwrap();
        assert_eq!(sync::identity(&w.root).unwrap(), first);

        fs::write(sysinfo_path(&w), "FirewireGuid: 0x000A2700BBBBBBBB\n").unwrap();
        differs("the GUID", sync::identity(&w.root).unwrap());
        fs::write(sysinfo_path(&w), &sysinfo).unwrap();
        assert_eq!(sync::identity(&w.root).unwrap(), first);
        assert!(matches!(
            {
                fs::write(sysinfo_path(&w), "FirewireGuid: 0x000A2700BBBBBBBB\n").unwrap();
                sync::check_identity(&w.root, &first)
            },
            Err(Error::IpodChanged)
        ));
    }

    // ---- M12

    /// `(scheme at 0x30, hash58 set)` of the iPod's iTunesDB, read here
    /// from the bytes rather than through the code under test.
    fn signature(w: &World) -> (u16, bool) {
        let b = fs::read(itunesdb_path(w)).unwrap();
        (
            u16::from_le_bytes([b[0x30], b[0x31]]),
            b[0x58..0x6c].iter().any(|x| *x != 0),
        )
    }

    const CLASSIC: &str = "ModelNumStr: xC297\nFirewireGuid: 0x000A27001234ABCD\n";

    fn ext(version: &str) -> String {
        format!(
            "<plist><dict><key>FireWireGUID</key><string>0x000A27001234ABCD</string>\n<key>DBVersion</key>\n<integer>{version}</integer></dict></plist>"
        )
    }

    /// A World whose database libgpod has signed: a Classic model in SysInfo,
    /// one sync done.
    fn signed_world() -> World {
        let w = World::new(3);
        assert_eq!(signature(&w), (0, false), "set up: the fake is unsigned");
        fs::write(sysinfo_path(&w), CLASSIC).unwrap();
        nudge(&w, 1);
        w.sync().unwrap();
        assert_eq!(signature(&w), (1, true), "set up: the first sync signed");
        w
    }

    fn refused_unsigned(w: &World) -> String {
        nudge(w, 2);
        let before = state(w);
        let p = w.prepare();
        let work = exec::full_work(&p);
        assert!(!work.is_empty(), "set up: there is something to write");
        let e = w
            .run(
                &p,
                &work,
                Options::default(),
                &|_| {},
                &AtomicBool::new(false),
            )
            .unwrap_err();
        let Error::WouldNotSign(why) = &e else {
            panic!("expected WouldNotSign, got {e:?}")
        };
        assert_eq!(state(w), before, "the refusal changed something");
        assert_eq!(signature(w), (1, true), "still signed");
        why.clone()
    }

    // Prevents (audit M12) a sync reporting success after replacing a signed
    // database with an unsigned one the iPod then shows as empty.
    #[test]
    fn a_signed_database_is_not_written_when_libgpod_would_not_sign_it() {
        let w = signed_world();
        // As after a hand edit or a restore: the GUID, no model.
        fs::write(sysinfo_path(&w), "FirewireGuid: 0x000A27001234ABCD\n").unwrap();
        let why = refused_unsigned(&w);
        assert!(why.contains("DBVersion") && why.contains("model"), "{why}");

        // Model back: the same sync goes through and the result is signed.
        fs::write(sysinfo_path(&w), CLASSIC).unwrap();
        w.sync().unwrap();
        assert_eq!(signature(&w), (1, true));
    }

    #[test]
    fn a_signed_database_is_not_written_when_sysinfoextended_says_dbversion_2() {
        let w = signed_world();
        // The model is known, but the version says the older, unsigned format.
        fs::write(w.root.join("iPod_Control/Device/SysInfoExtended"), ext("2")).unwrap();
        let why = refused_unsigned(&w);
        assert!(why.contains("DBVersion 2"), "{why}");
    }

    #[test]
    fn dbversion_3_in_sysinfoextended_is_allowed_to_write_a_signed_database() {
        let w = signed_world();
        fs::write(sysinfo_path(&w), "FirewireGuid: 0x000A27001234ABCD\n").unwrap();
        fs::write(w.root.join("iPod_Control/Device/SysInfoExtended"), ext("3")).unwrap();
        nudge(&w, 2);
        // No model, but the version is enough.
        w.sync().unwrap();
        assert_eq!(signature(&w), (1, true));
    }

    // An unsigned database stays unsigned without a fuss: the refusal is only
    // for downgrading a signed one.
    #[test]
    fn an_unsigned_database_is_written_even_when_libgpod_would_not_sign() {
        let w = World::new(3);
        nudge(&w, 1);
        let out = w.sync().unwrap();
        assert!(out.applied > 0);
        assert_eq!(signature(&w), (0, false));
    }

    // Prevents (audit M12) the check on the written file being lost: the
    // pre-write check trusts the model, libgpod does not sign for it.
    #[test]
    fn a_signed_database_written_unsigned_fails_the_verification() {
        let w = signed_world();
        // libgpod knows this model (so the pre-write check passes) but writes
        // no hash for it: an iPod Video, whose firmware checks nothing.
        fs::write(
            sysinfo_path(&w),
            "ModelNumStr: MA002\nFirewireGuid: 0x000A27001234ABCD\n",
        )
        .unwrap();
        nudge(&w, 2);
        let e = w.sync().unwrap_err();
        match &e {
            Error::Verification { problems, .. } => assert!(
                problems.iter().any(|p| p.contains("signed")),
                "{problems:?}"
            ),
            other => panic!("expected Verification, got {other:?}"),
        }
        assert!(!signature(&w).1, "set up: it was written unsigned");
    }

    // ---- L1, L2: deleting what a damaged database points at

    #[derive(Clone, Copy, Debug)]
    enum Target {
        /// The iPod's own database.
        ItunesDb,
        /// A file outside the iPod, reached through `..`.
        Outside,
        /// The file of another track that stays.
        Shared,
        /// Directly in the music folder, not in one of its F00 to F49.
        MusicRoot,
        /// Below a music subfolder.
        TooDeep,
    }

    #[derive(Clone, Copy, Debug)]
    enum How {
        Removed,
        Recopied,
    }

    struct Damaged {
        w: World,
        out: Outcome,
        /// What the track's path named.
        file: PathBuf,
        /// Its bytes before the sync, for a file that must not change.
        bytes: Vec<u8>,
        _victim: Option<tempfile::NamedTempFile>,
    }

    /// Track 3's path is made to name `target`, then the track is removed (its
    /// library file deleted) or recopied (new audio), and the sync run.
    fn damaged(target: Target, how: How) -> Damaged {
        let mut w = World::new(4);
        let victim = tempfile::Builder::new()
            .prefix("iprune-victim-")
            .tempfile_in(w.root.parent().unwrap())
            .unwrap();
        fs::write(victim.path(), b"outside the iPod").unwrap();
        let name = victim.path().file_name().unwrap().to_str().unwrap();
        let (path, file) = match target {
            Target::ItunesDb => (
                ":iPod_Control:iTunes:iTunesDB".to_owned(),
                itunesdb_path(&w),
            ),
            // Four steps up from F00 leave the iPod's folder.
            Target::Outside => (
                format!(":iPod_Control:Music:F00:..:..:..:..:{name}"),
                victim.path().to_owned(),
            ),
            Target::Shared => (
                ":iPod_Control:Music:F00:T0000.mp3".to_owned(),
                w.file_of(dbid(1)),
            ),
            Target::MusicRoot => (
                ":iPod_Control:Music:stray.mp3".to_owned(),
                w.root.join("iPod_Control/Music/stray.mp3"),
            ),
            Target::TooDeep => (
                ":iPod_Control:Music:F05:sub:deep.mp3".to_owned(),
                w.root.join("iPod_Control/Music/F05/sub/deep.mp3"),
            ),
        };
        if matches!(target, Target::MusicRoot | Target::TooDeep) {
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, b"in the music folder, but not a track's place").unwrap();
        }
        common::set_ipod_path(&itunesdb_path(&w), 2, &path);
        assert_eq!(
            w.track(dbid(3)).unwrap().ipod_path.as_deref(),
            Some(path.as_str()),
            "set up: the database holds the damaged path"
        );
        match how {
            How::Removed => fs::remove_file(&w.files[2]).unwrap(),
            How::Recopied => {
                fs::copy(fixture_secs(Kind::Mp3, 3, 3), &w.files[2]).unwrap();
            }
        }
        w.rescan();
        let bytes = fs::read(&file).unwrap();
        let out = w
            .sync()
            .unwrap_or_else(|e| panic!("{target:?} {how:?}: {e:?}"));
        Damaged {
            w,
            out,
            file,
            bytes,
            _victim: Some(victim),
        }
    }

    fn assert_kept(d: &Damaged, target: Target, how: How) {
        let ctx = format!("{target:?} {how:?}");
        let kept: Vec<_> = d
            .out
            .warnings
            .iter()
            .filter(|m| m.contains("kept"))
            .collect();
        assert_eq!(kept.len(), 1, "{ctx}: {:?}", d.out.warnings);
        assert_eq!(d.out.warnings.len(), 1, "{ctx}: {:?}", d.out.warnings);
        let leaf = d.file.file_name().unwrap().to_str().unwrap();
        assert!(kept[0].contains(leaf), "{ctx}: {}", kept[0]);
        assert!(d.file.is_file(), "{ctx}: the target is gone");
        match how {
            How::Removed => assert_eq!((d.out.removed, d.out.recopied), (1, 0), "{ctx}"),
            How::Recopied => assert_eq!((d.out.removed, d.out.recopied), (0, 1), "{ctx}"),
        }
        match target {
            Target::ItunesDb => {
                // What is there is the database the sync wrote and verified.
                let c = d.w.contents();
                let track = c.tracks.iter().find(|t| t.adopt.dbid == dbid(3));
                match how {
                    How::Removed => assert!(track.is_none(), "{ctx}"),
                    How::Recopied => assert_ne!(
                        track.expect("recopied").ipod_path.as_deref(),
                        Some(":iPod_Control:iTunes:iTunesDB"),
                        "{ctx}"
                    ),
                }
            }
            Target::Outside | Target::Shared | Target::MusicRoot | Target::TooDeep => {
                assert_eq!(fs::read(&d.file).unwrap(), d.bytes, "{ctx}: modified");
            }
        }
        if let Target::Shared = target {
            let t = d.w.track(dbid(1)).expect("the other track stays");
            assert_eq!(
                t.ipod_path.as_deref(),
                Some(":iPod_Control:Music:F00:T0000.mp3")
            );
        }
        let p = d.w.prepare();
        assert!(exec::full_work(&p).is_empty(), "{ctx}: {:?}", p.plan);
    }

    // Prevents (audit L2) a removed or replaced track's path, read from a
    // damaged database, deleting the iTunesDB, a file outside the iPod or
    // the file another track plays.
    #[test]
    fn a_removed_track_pointing_at_itunesdb_keeps_it() {
        let d = damaged(Target::ItunesDb, How::Removed);
        assert_kept(&d, Target::ItunesDb, How::Removed);
    }

    #[test]
    fn a_recopied_track_pointing_at_itunesdb_keeps_it() {
        let d = damaged(Target::ItunesDb, How::Recopied);
        assert_kept(&d, Target::ItunesDb, How::Recopied);
    }

    #[test]
    fn a_removed_track_pointing_outside_the_ipod_keeps_that_file() {
        let d = damaged(Target::Outside, How::Removed);
        assert_kept(&d, Target::Outside, How::Removed);
    }

    #[test]
    fn a_recopied_track_pointing_outside_the_ipod_keeps_that_file() {
        let d = damaged(Target::Outside, How::Recopied);
        assert_kept(&d, Target::Outside, How::Recopied);
    }

    // The only place a track's file lives is a folder directly under Music.
    #[test]
    fn a_removed_track_pointing_at_the_music_folder_itself_or_below_a_subfolder_keeps_the_file() {
        for target in [Target::MusicRoot, Target::TooDeep] {
            let d = damaged(target, How::Removed);
            assert_kept(&d, target, How::Removed);
        }
    }

    // Prevents (audit L1) removing one track deleting the file of another.
    #[test]
    fn a_removed_track_sharing_a_file_with_a_kept_track_keeps_the_file() {
        let d = damaged(Target::Shared, How::Removed);
        assert_kept(&d, Target::Shared, How::Removed);
    }

    #[test]
    fn a_recopied_track_sharing_a_file_with_a_kept_track_keeps_the_file() {
        let d = damaged(Target::Shared, How::Recopied);
        assert_kept(&d, Target::Shared, How::Recopied);
    }

    // The normal case, to show the check is not a blanket refusal.
    #[test]
    fn a_removed_tracks_own_file_in_the_music_folder_is_still_deleted() {
        let mut w = World::new(4);
        let file = w.file_of(dbid(3));
        let other = w.file_of(dbid(4));
        fs::remove_file(&w.files[2]).unwrap();
        w.rescan();
        let out = w.sync().unwrap();
        assert_eq!(out.removed, 1);
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
        assert!(!file.exists());
        assert!(other.is_file());
    }

    // A symlink in the music folder is not a regular file: neither it nor
    // what it points at (a file in the music folder no track uses) is deleted.
    #[test]
    fn a_symlink_in_place_of_the_tracks_file_is_kept_and_its_target_too() {
        let mut w = World::new(4);
        let file = w.file_of(dbid(3));
        let target = w.root.join("iPod_Control/Music/F05/unused.mp3");
        fs::write(&target, b"precious").unwrap();
        fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&target, &file).unwrap();
        fs::remove_file(&w.files[2]).unwrap();
        w.rescan();
        let out = w.sync().unwrap();
        assert_eq!(out.removed, 1);
        assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
        assert!(out.warnings[0].contains("kept"), "{:?}", out.warnings);
        assert_eq!(fs::read(&target).unwrap(), b"precious");
        assert!(file.symlink_metadata().unwrap().is_symlink());
    }

    // ---- doctor and damaged paths

    #[test]
    fn doctor_reports_paths_that_leave_the_ipod_and_proposes_no_repair_for_them() {
        use iprune_core::doctor;
        let w = World::new(4);
        let db = itunesdb_path(&w);
        let bad = [
            (1, ":iPod_Control:Music:F00:..:..:..:..:elsewhere.mp3"),
            (2, "iPod_Control:Music:F01:T0001.mp3"),
            (3, ":..:outside.mp3"),
        ];
        for (k, path) in bad {
            common::set_ipod_path(&db, k, path);
        }
        let victim = w.root.parent().unwrap().join("elsewhere.mp3");
        let existed = victim.exists();
        let contents = w.contents();
        let r = doctor::examine(
            w.env.store.conn(),
            &w.root,
            &contents,
            Some(w.device()),
            iprune_core::now_unix(),
        )
        .unwrap();
        assert_eq!(
            r.bad_paths,
            vec![
                (dbid(2), bad[0].1.to_owned()),
                (dbid(3), bad[1].1.to_owned()),
                (dbid(4), bad[2].1.to_owned()),
            ]
        );
        assert!(
            r.missing_files.is_empty(),
            "a damaged path is not a missing file: {:?}",
            r.missing_files
        );
        assert!(!r.is_clean());
        let p = w.prepare();
        let work = doctor::repair_work(&p, &r);
        assert!(work.tracks.removes.is_empty(), "{:?}", work.tracks.removes);
        assert_eq!(victim.exists(), existed);
    }

    #[test]
    fn doctor_calls_an_empty_path_a_missing_file_not_a_damaged_one() {
        let w = World::new(3);
        common::set_ipod_path(&itunesdb_path(&w), 1, "");
        let contents = w.contents();
        let r = iprune_core::doctor::examine(
            w.env.store.conn(),
            &w.root,
            &contents,
            Some(w.device()),
            iprune_core::now_unix(),
        )
        .unwrap();
        assert!(r.bad_paths.is_empty(), "{:?}", r.bad_paths);
        // An entry with no path at all is an entry with no file.
        let missing: Vec<u64> = r.missing_files.iter().map(|(d, _)| *d).collect();
        assert_eq!(missing, vec![dbid(2)]);
    }

    // ---- M6: a folder is not an iPod

    fn folder_refused(cfg_for_refused: &Config) {
        let w = World::new(3);
        nudge(&w, 1);
        w.write_play_counts(&[(3, 1_700_000_000, 60, 0)]);
        let p = w.prepare();
        let work = exec::full_work(&p);
        assert!(!work.is_empty());
        let before = state(&w);
        let e = sync_with_cfg(&w, cfg_for_refused, &p, &work).unwrap_err();
        assert!(matches!(e, Error::NotWritable(_)), "{e:?}");
        assert_eq!(state(&w), before, "a refused sync changed something");
        assert_eq!(w.stats_total_plays(), 0, "the plays were not pulled");

        // The same plan, folders allowed, goes through.
        let out = sync_with_cfg(&w, &w.env.cfg, &p, &work).unwrap();
        assert!(out.applied > 0);
    }

    // Prevents (audit M6) a sync writing a copy of an iPod, or the backup.
    #[test]
    fn a_sync_to_a_plain_folder_is_refused_unless_folders_are_allowed() {
        let w = World::new(3);
        assert!(w.env.cfg.device.allow_folder, "set up: the tests allow it");
        let mut cfg = w.env.cfg.clone();
        cfg.device.allow_folder = false;
        drop(w);
        folder_refused(&cfg);
    }

    #[test]
    fn a_restore_to_a_plain_folder_is_refused_unless_folders_are_allowed() {
        let w = World::new(3);
        nudge(&w, 1);
        let stamp = w.sync().unwrap().backup.stamp;
        nudge(&w, 2);
        w.sync().unwrap();
        w.write_play_counts(&[(3, 1_700_000_000, 60, 0)]);
        let before = state(&w);
        let mut cfg = w.env.cfg.clone();
        cfg.device.allow_folder = false;
        let e = exec::restore(
            w.env.store.conn(),
            &cfg,
            &w.paths,
            &w.root,
            GUID,
            &stamp,
            false,
        )
        .unwrap_err();
        assert!(matches!(e, Error::NotWritable(_)), "{e:?}");
        assert_eq!(state(&w), before, "a refused restore changed something");
        assert_eq!(w.stats_total_plays(), 0);

        assert_eq!(w.playlist("Mix"), Some(vec![dbid(1), dbid(3)]), "set up");
        restore(&w, &stamp, false).unwrap();
        assert_ne!(state(&w), before);
        assert_eq!(w.playlist("Mix"), Some(vec![dbid(3)]), "the backup's Mix");
    }
    // Prevents (found by Aldo) a sync failing with NotAdopted once the iPod
    // gains a SysInfoExtended, where libgpod gives the GUID without "0x".
    #[test]
    fn an_ipod_that_gains_a_sysinfoextended_after_adoption_is_still_the_same_device() {
        let w = World::new(3);
        // Adopted as "0x000A27001234ABCD"; now reported bare and lower case.
        fs::write(
            w.root.join("iPod_Control/Device/SysInfoExtended"),
            "<plist><dict><key>FireWireGUID</key><string>000a27001234abcd</string></dict></plist>",
        )
        .unwrap();
        assert_eq!(
            w.contents().firewire_guid.as_deref(),
            Some("000a27001234abcd")
        );
        nudge(&w, 1);
        let out = w.sync().unwrap();
        assert!(out.applied > 0);
        assert_eq!(w.playlist("Mix"), Some(vec![dbid(3), dbid(1)]));
        assert_eq!(
            devices::devices(w.env.store.conn()).unwrap().len(),
            1,
            "no second device row"
        );
    }
    // Prevents (review of step 3) a restore of iPod A's backup onto iPod B,
    // swapped in at the same mountpoint while a prompt waited.
    #[test]
    fn a_restore_onto_an_ipod_with_another_guid_than_the_backups_is_refused() {
        let w = World::new(3);
        nudge(&w, 1);
        let stamp = w.sync().unwrap().backup.stamp;
        w.write_play_counts(&[(3, 1_700_000_000, 60, 0)]);
        // iPod B: same files, another GUID.
        fs::write(sysinfo_path(&w), "FirewireGuid: 0x000A2700BBBBBBBB\n").unwrap();
        let before = state(&w);
        let data = tree(&w.paths.data_dir);

        let e = restore(&w, &stamp, false).unwrap_err();
        match &e {
            Error::GuidMismatch { ipod, given } => {
                assert!(ipod.to_uppercase().ends_with("BBBBBBBB"), "{e}");
                assert!(given.to_uppercase().ends_with("1234ABCD"), "{e}");
            }
            other => panic!("expected GuidMismatch, got {other:?}"),
        }
        assert_eq!(state(&w), before, "B was changed");
        assert_eq!(tree(&w.paths.data_dir), data, "a backup was taken");
        assert_eq!(w.stats_total_plays(), 0, "plays were pulled");
        assert_eq!(
            devices::restoring(w.env.store.conn(), w.device()).unwrap(),
            None
        );

        // The right iPod back: the restore goes through.
        fs::write(sysinfo_path(&w), SYSINFO).unwrap();
        restore(&w, &stamp, false).unwrap();
    }

    // The same iPod spelled differently is the same iPod.
    #[test]
    fn a_restore_accepts_the_same_guid_in_another_spelling() {
        let w = World::new(3);
        nudge(&w, 1);
        let stamp = w.sync().unwrap().backup.stamp;
        fs::write(sysinfo_path(&w), "FirewireGuid: 000a27001234abcd\n").unwrap();
        restore(&w, &stamp, false).unwrap();
    }

    /// Backup folders of the iPod (not the hidden store), by name.
    fn backup_folders(w: &World) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(w.paths.data_dir.join("backups"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| !n.starts_with('.'))
            .collect();
        v.sort();
        v
    }

    // Prevents (review of step 3) one iPod's backups being split in two
    // folders when its GUID is read from SysInfoExtended after SysInfo:
    // pruning would count each half.
    #[test]
    fn syncs_under_two_spellings_of_the_guid_share_one_folder_and_one_prune() {
        let mut w = World::new(3);
        w.env.cfg.sync.keep_backups = 2;
        let root = w.paths.data_dir.join("backups");
        nudge(&w, 1);
        w.sync().unwrap();
        assert_eq!(backup_folders(&w), vec![GUID.to_owned()]);

        // The GUID now comes from SysInfoExtended, bare.
        fs::write(
            w.root.join("iPod_Control/Device/SysInfoExtended"),
            "<plist><dict><key>FireWireGUID</key><string>000A27001234ABCD</string></dict></plist>",
        )
        .unwrap();
        nudge(&w, 2);
        w.sync().unwrap();
        assert_eq!(backup_folders(&w), vec![GUID.to_owned()], "a second folder");
        assert_eq!(backup::list(&root, "000A27001234ABCD").unwrap().len(), 2);

        nudge(&w, 3);
        w.sync().unwrap();
        assert_eq!(backup_folders(&w), vec![GUID.to_owned()]);
        assert_eq!(
            backup::list(&root, "000A27001234ABCD").unwrap().len(),
            2,
            "pruned to keep_backups across both spellings"
        );
    }

    // The artwork store is per iPod: a backup under the other spelling must
    // find the covers the first one stored, not copy them again elsewhere.
    #[test]
    fn a_backup_under_another_spelling_of_the_guid_reuses_the_first_ones_cover_store() {
        let w = World::new(3);
        common::add_cover_art(&w.root, SYSINFO);
        nudge(&w, 1);
        let first = w.sync().unwrap().backup;
        let root = w.paths.data_dir.join("backups");
        let store = root.join(GUID).join(".ithmb");
        let stored = fs::read_dir(&store).unwrap().count();
        assert!(stored > 0, "set up: the covers are stored");

        let second =
            backup::create(&root, "000A27001234ABCD", &w.root, 1_800_000_000, &[]).unwrap();
        assert_eq!(second.dir.parent(), first.dir.parent(), "same folder");
        assert_eq!(backup_folders(&w), vec![GUID.to_owned()]);
        assert_eq!(
            fs::read_dir(&store).unwrap().count(),
            stored,
            "copied again"
        );
        // The first backup was taken before libgpod rewrote the files, so
        // only their content (hash) is compared, not their times.
        let hashes = |b: &backup::Backup| -> Vec<(String, String)> {
            let v: serde_json::Value =
                serde_json::from_slice(&fs::read(b.dir.join("artwork.json")).unwrap()).unwrap();
            v.as_array()
                .unwrap()
                .iter()
                .map(|i| {
                    (
                        i["name"].as_str().unwrap().to_owned(),
                        i["hash"].as_str().unwrap().to_owned(),
                    )
                })
                .collect()
        };
        assert_eq!(hashes(&second), hashes(&first), "the same covers");
    }
}

// ---- statistics: lifetime counts, ratings (audit M5, H4)

fn full_stats_of(w: &World, file: &Path) -> Option<(i64, i64, Option<i64>, Option<i64>)> {
    w.env
        .store
        .conn()
        .query_row(
            "SELECT plays, skips, rating, last_played FROM stats WHERE track_id = ?1",
            [w.id_of(file)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok()
}

// Prevents (audit M5) the plays made under iTunes before adoption being left
// out of iprune's totals. libgpod folds the recent counts into the lifetime
// ones it reads: 8 in the iTunesDB and 2 in Play Counts read as 10.
#[test]
fn an_adopted_ipods_lifetime_plays_are_imported_once_and_later_pulls_add_the_new_ones() {
    let mut w = World::new_with_plays(3, &[8]);
    w.write_play_counts(&[
        (2, 1_700_000_000, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
    ]);
    let t = w.track(dbid(1)).unwrap();
    assert_eq!((t.adopt.playcount, t.recent_plays), (10, 2), "set up");
    let file = w.files[0].clone();
    assert_eq!(full_stats_of(&w, &file), None, "set up: no row yet");

    w.add_new("n1.mp3", Kind::Mp3, 11);
    w.sync().unwrap();
    let (plays, ..) = full_stats_of(&w, &file).expect("a row");
    assert_eq!(plays, 10, "the lifetime count, not 12 and not 2");
    assert_eq!(w.track(dbid(1)).unwrap().adopt.playcount, 10, "set up");
    let csv = fs::read_to_string(w.paths.data_dir.join("stats.csv")).unwrap();
    let line = csv
        .lines()
        .find(|l| l.contains("s1.mp3"))
        .unwrap_or_else(|| panic!("s1.mp3 is listed: {csv}"));
    assert!(line.contains(",10,0,"), "{line}");

    // Three more plays; the iPod starts a new Play Counts after the write.
    w.write_play_counts(&[
        (3, 1_700_001_000, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
    ]);
    w.add_new("n2.mp3", Kind::Mp3, 12);
    w.sync().unwrap();
    assert_eq!(full_stats_of(&w, &file).unwrap().0, 13);
    assert_eq!(w.track(dbid(1)).unwrap().adopt.playcount, 13, "set up");
}

// The reason for the import: the iPod track goes, its history must not.
#[test]
fn lifetime_plays_survive_the_removal_of_the_ipod_track() {
    let mut w = World::new_with_plays(3, &[150]);
    w.write_play_counts(&[
        (3, 1_700_000_000, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
    ]);
    let file = w.files[0].clone();
    w.add_new("n1.mp3", Kind::Mp3, 11);
    w.sync().unwrap();
    assert_eq!(full_stats_of(&w, &file).unwrap().0, 153, "set up");

    // The library file goes away; the next sync removes the iPod track.
    fs::remove_file(&file).unwrap();
    w.rescan();
    w.sync().unwrap();
    assert!(w.track(dbid(1)).is_none(), "set up: removed from the iPod");
    assert_eq!(w.stats_total_plays(), 153);
    let csv = fs::read_to_string(w.paths.data_dir.join("stats.csv")).unwrap();
    assert!(csv.contains("s1.mp3,153,"), "{csv}");
}

// A track that already has a row (counted from an earlier pull) is not
// started over from the lifetime count.
#[test]
fn a_track_with_a_row_gets_its_total_plus_the_recent_plays_only() {
    let mut w = World::new_with_plays(3, &[8]);
    w.write_play_counts(&[
        (2, 1_700_000_000, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
    ]);
    let file = w.files[0].clone();
    let id = w.id_of(&file);
    w.env
        .store
        .conn()
        .execute(
            "INSERT INTO stats (track_id, plays, skips) VALUES (?1, 5, 0)",
            [id],
        )
        .unwrap();
    w.add_new("n1.mp3", Kind::Mp3, 11);
    w.sync().unwrap();
    assert_eq!(full_stats_of(&w, &file).unwrap().0, 7, "5 + 2, not 10");
}

// Prevents (audit H4) a track copied again to the iPod, which starts with no
// rating, wiping the rating pulled earlier.
#[test]
fn a_recopied_track_keeps_the_rating_pulled_earlier() {
    let mut w = World::new(3);
    let s1 = w.files[0].clone();
    let id = w.id_of(&s1);
    let rating = |w: &World| full_stats_of(w, &s1).and_then(|r| r.2);

    // Sync 1: the user rated track 1 on the iPod (80 = 4 stars).
    w.write_play_counts(&[
        (2, 1_700_000_000, 80, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
    ]);
    w.add_new("n1.mp3", Kind::Mp3, 11);
    w.sync().unwrap();
    assert_eq!(rating(&w), Some(80), "set up: rating pulled");
    let first_dbid = w.mapping()[&id];

    // Sync 2: the library file is briefly missing: the iPod track goes.
    let bytes = fs::read(&s1).unwrap();
    fs::remove_file(&s1).unwrap();
    w.rescan();
    assert_eq!(w.sync().unwrap().removed, 1, "set up");
    assert_eq!(rating(&w), Some(80), "still in the index");

    // Sync 3: the file is back and is copied again, unrated.
    fs::write(&s1, &bytes).unwrap();
    w.rescan();
    assert_eq!(w.sync().unwrap().copied, 1, "set up");
    let new_dbid = w.mapping()[&id];
    assert_ne!(new_dbid, first_dbid, "set up: a new iPod track");
    assert_eq!(w.track(new_dbid).unwrap().rating, 0, "set up: unrated");

    // Sync 4: a later sync pulls statistics again.
    w.add_new("n2.mp3", Kind::Mp3, 12);
    w.sync().unwrap();
    assert_eq!(rating(&w), Some(80), "the rating survives the re-copy");
}

// ---- Play Counts is moved aside before libgpod writes (audit M3)

/// The state a sync is in when the database write starts.
#[derive(Debug)]
struct AtWrite {
    live_counts: bool,
    counts_bak: Option<Vec<u8>>,
    itunesdb: Vec<u8>,
}

fn counts_files(root: &Path) -> (bool, Option<Vec<u8>>) {
    let dir = root.join("iPod_Control/iTunes");
    (
        dir.join("Play Counts").exists(),
        fs::read(dir.join("Play Counts.bak")).ok(),
    )
}

/// A world with 4 tracks, plays on tracks 1 and 3, and track 1 gone from the
/// library, so the sync removes it and every later track moves up.
fn removal_world() -> (World, Vec<i64>) {
    let mut w = World::new(4);
    let ids: Vec<i64> = w.files.iter().map(|f| w.id_of(f)).collect();
    w.write_play_counts(&[
        (5, 1_700_000_000, 0, 0),
        (0, 0, 0, 0),
        (7, 1_700_000_100, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
    ]);
    fs::remove_file(&w.files[0]).unwrap();
    w.rescan();
    (w, ids)
}

fn plays_by_track(w: &World) -> BTreeMap<i64, i64> {
    let conn = w.env.store.conn();
    let mut st = conn.prepare("SELECT track_id, plays FROM stats").unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

// Prevents (audit M3) a crash between libgpod replacing iTunesDB and
// renaming Play Counts leaving counts indexed by the old track order next to
// the new database. iprune renames the file itself, before libgpod writes.
#[test]
fn play_counts_is_moved_aside_before_the_database_is_written_and_stays_aside() {
    let (w, ids) = removal_world();
    let counts_before = fs::read(w.root.join("iPod_Control/iTunes/Play Counts")).unwrap();
    let itunesdb_before = fs::read(w.root.join("iPod_Control/iTunes/iTunesDB")).unwrap();
    let seen: RefCell<Vec<AtWrite>> = RefCell::new(Vec::new());
    let p = w.prepare();
    let work = exec::full_work(&p);
    let out = w
        .run(
            &p,
            &work,
            Options::default(),
            &|e| {
                if matches!(e, Progress::Writing) {
                    let (live_counts, counts_bak) = counts_files(&w.root);
                    seen.borrow_mut().push(AtWrite {
                        live_counts,
                        counts_bak,
                        itunesdb: fs::read(w.root.join("iPod_Control/iTunes/iTunesDB")).unwrap(),
                    });
                }
            },
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(out.removed, 1, "set up");
    let seen = seen.into_inner();
    assert!(!seen.is_empty(), "set up: the write was announced");
    // Whenever a write starts, no live Play Counts sits next to the old or the
    // new iTunesDB, and the first write starts from the old iTunesDB.
    for at in &seen {
        assert!(!at.live_counts, "{at:?}");
    }
    assert_eq!(
        seen[0].itunesdb, itunesdb_before,
        "the first write had not started"
    );
    assert_eq!(seen[0].counts_bak.as_ref(), Some(&counts_before));
    // And after it.
    let (live, bak) = counts_files(&w.root);
    assert!(!live);
    assert_eq!(bak.as_ref(), Some(&counts_before));
    assert_ne!(
        fs::read(w.root.join("iPod_Control/iTunes/iTunesDB")).unwrap(),
        itunesdb_before,
        "set up: the database was replaced"
    );
    let plays = plays_by_track(&w);
    assert_eq!(
        (plays.get(&ids[0]), plays.get(&ids[2]), plays.len()),
        (Some(&5), Some(&7), 2),
        "{plays:?}"
    );
}

/// Every file under `root`, deleted, then `files` written back.
fn put_tree_back(root: &Path, files: &BTreeMap<PathBuf, Vec<u8>>) {
    for rel in tree(root).keys() {
        fs::remove_file(root.join(rel)).unwrap();
    }
    for (rel, bytes) in files {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
}

// A kill right after Play Counts was moved aside and the pull committed, before
// libgpod wrote anything: the pull stands (its plays are in the index and the
// file is in the backup), nothing is pulled twice, nothing is credited to a
// track that moved up in the order.
#[test]
fn a_kill_after_play_counts_was_moved_aside_counts_every_play_once_on_the_next_sync() {
    let (w, ids) = removal_world();
    let at_write_ipod: RefCell<Option<BTreeMap<PathBuf, Vec<u8>>>> = RefCell::new(None);
    let index = w.env.store.conn().path().unwrap().to_owned() + ".at-write";
    let p = w.prepare();
    let work = exec::full_work(&p);
    w.run(
        &p,
        &work,
        Options::default(),
        &|e| {
            if matches!(e, Progress::Writing) && at_write_ipod.borrow().is_none() {
                *at_write_ipod.borrow_mut() = Some(w.tree());
                w.env
                    .store
                    .conn()
                    .execute("VACUUM INTO ?1", [&index])
                    .unwrap();
            }
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    let clean_plays = plays_by_track(&w);

    // The process died at that point: the iPod and the index are as they
    // were when the write started.
    let crashed_ipod = at_write_ipod.into_inner().unwrap();
    assert!(
        crashed_ipod.contains_key(Path::new("iPod_Control/iTunes/Play Counts.bak")),
        "set up"
    );
    assert!(
        !crashed_ipod.contains_key(Path::new("iPod_Control/iTunes/Play Counts")),
        "set up"
    );
    put_tree_back(&w.root, &crashed_ipod);
    {
        let c = w.env.store.conn();
        c.execute("ATTACH DATABASE ?1 AS snap", [&index]).unwrap();
        for t in ["stats", "pending_pulls", "device_tracks"] {
            c.execute_batch(&format!(
                "DELETE FROM main.{t}; INSERT INTO main.{t} SELECT * FROM snap.{t};"
            ))
            .unwrap();
        }
        c.execute("DETACH DATABASE snap", []).unwrap();
    }
    assert_eq!(w.pending_pulls(), 1, "set up: the pull is pending");

    let out = w.sync().unwrap();
    assert_eq!(out.recovered, 0, "the pull stands");
    assert_eq!(out.pulled.plays(), 0, "no live Play Counts to pull again");
    assert_eq!(out.removed, 1);
    assert_eq!(plays_by_track(&w), clean_plays);
    let plays = plays_by_track(&w);
    assert_eq!(
        (plays.get(&ids[1]), plays.get(&ids[3])),
        (None, None),
        "tracks 2 and 4 never played"
    );
    assert_eq!(w.pending_pulls(), 0);
}

// ---- audit step 6: library identity

mod library_identity {
    use super::*;

    /// Track `variant`'s audio with its title replaced; the artist stays and
    /// the album is always "Album A".
    fn album_mp3(variant: u32, title: &str) -> Vec<u8> {
        retagged_mp3(
            variant,
            title,
            &format!("Artist{variant}"),
            Some("Album A"),
            None,
        )
    }

    // Prevents (audit H6) a file moved and retagged in one go (Picard, beets)
    // becoming a new track: its iPod entry removed, its stats and playlist
    // places lost.
    #[test]
    fn a_file_moved_and_retagged_together_keeps_its_track_playlists_stats_and_ipod_entry() {
        let mut w = World::new(3);
        w.write_play_counts(&[
            (4, 1_700_000_000, 80, 2),
            (0, 0, 0, 0),
            (0, 0, 0, 0),
            (0, 0, 0, 0),
        ]);
        // Track 1 gets an album in the library; one sync settles that and
        // pulls the plays into SQLite.
        let src = w.files[0].clone();
        fs::write(&src, album_mp3(1, "Title1")).unwrap();
        w.rescan();
        w.sync().unwrap();
        let id = w.id_of(&src);
        assert_eq!(stats_of(&w, &src), Some((4, Some(80))));
        let before = w.track(dbid(1)).unwrap();
        assert_eq!((before.adopt.playcount, before.rating), (4, 80));
        assert_eq!(before.tags.album.as_deref(), Some("Album A"));
        let road_before = w.playlist("Road").unwrap();
        assert!(w.prepare().plan.is_empty());

        // Same audio, new title and path in one operation.
        fs::remove_file(&src).unwrap();
        let new_path = w
            .env
            .add_bytes("Artist1/Album A/01 Renamed.mp3", &album_mp3(1, "Renamed"));
        let scan = w.env.scan();
        w.reload();
        assert_eq!(scan.moved, vec![(src.clone(), new_path.clone())]);
        assert_eq!((scan.added, scan.missing.len()), (0, 0), "{scan:?}");
        assert_eq!(w.id_of(&new_path), id, "the track keeps its id");
        // The .m3u8 follows the file and every entry still resolves.
        let road = w.env.locations("Road");
        assert_eq!(road.first().map(String::as_str), new_path.to_str());
        assert!(!road.iter().any(|l| Path::new(l) == src), "{road:?}");
        let items = w.env.indexed_items("Road");
        assert_eq!(items.len(), 3);
        assert!(items.iter().all(|i| i.1.is_some()), "{items:?}");
        assert_eq!(items[0].1, Some(id));
        assert_eq!(stats_of(&w, &new_path), Some((4, Some(80))));

        // The plan replaces the audio on the same iPod entry.
        let p = w.prepare();
        assert_eq!(p.plan.replace.len(), 1, "{:?}", p.plan);
        assert_eq!(
            (p.plan.replace[0].track_id, p.plan.replace[0].old_dbid),
            (id, dbid(1))
        );
        assert!(
            p.plan.copy.is_empty() && p.plan.remove.is_empty(),
            "{:?}",
            p.plan
        );

        let count_before = w.contents().tracks.len();
        let out = w.sync().unwrap();
        assert_eq!((out.copied, out.recopied, out.removed), (0, 1, 0));
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);

        assert_eq!(w.mapping()[&id], dbid(1), "same iPod entry");
        assert_eq!(w.contents().tracks.len(), count_before);
        assert_eq!(w.playlist("Road").unwrap(), road_before);
        let after = w.track(dbid(1)).unwrap();
        assert_eq!(after.tags.title.as_deref(), Some("Renamed"));
        assert_eq!((after.adopt.playcount, after.rating), (4, 80));
        assert_eq!(
            fs::read(w.file_of(dbid(1))).unwrap(),
            fs::read(&new_path).unwrap(),
            "the iPod holds the library file's bytes"
        );
        assert_eq!(stats_of(&w, &new_path), Some((4, Some(80))));
        // The mapping records the real audio now: settled.
        assert!(exec::full_work(&w.prepare()).is_empty());
    }

    // A wrong pair must never leave old audio under new tags: a different
    // recording of the same length and bitrate that is paired by mistake
    // (same artist and album) gets its audio replaced, not just retagged.
    #[test]
    fn a_wrong_pair_by_audio_properties_gets_its_audio_replaced_not_just_retagged() {
        let mut w = World::new(3);
        let src = w.files[1].clone();
        fs::write(&src, album_mp3(2, "Title2")).unwrap();
        w.rescan();
        w.sync().unwrap();
        // Another recording of the same length, same artist and album, new
        // title and path (the tags say it is the same song, the bytes of the
        // audio are different).
        let old_audio = fs::read(w.file_of(dbid(2))).unwrap();
        fs::remove_file(&src).unwrap();
        let new_bytes = retagged_mp3(7, "Take Two", "Artist2", Some("Album A"), None);
        let new_path = w.env.add_bytes("Take Two.mp3", &new_bytes);
        let scan = w.env.scan();
        w.reload();
        assert_eq!(scan.moved.len(), 1, "{scan:?}");
        let p = w.prepare();
        assert_eq!(p.plan.replace.len(), 1, "{:?}", p.plan);
        w.sync().unwrap();
        let now = fs::read(w.file_of(dbid(2))).unwrap();
        assert_ne!(now, old_audio);
        assert_eq!(now, fs::read(&new_path).unwrap());
        assert_eq!(w.mapping()[&w.id_of(&new_path)], dbid(2));
    }

    /// A 600 ms Mp3 with these tags, and the same audio under other tags.
    fn short_mp3_pair(dir: &Path) -> (Vec<u8>, Vec<u8>) {
        let run = |args: &[&str]| {
            let o = std::process::Command::new("ffmpeg")
                .args(["-loglevel", "error", "-y"])
                .args(args)
                .output()
                .expect("ffmpeg must be installed");
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        };
        let a = dir.join("a.mp3");
        let b = dir.join("b.mp3");
        let a_s = a.to_str().unwrap();
        run(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.6",
            "-c:a",
            "libmp3lame",
            "-id3v2_version",
            "3",
            "-metadata",
            "title=Short",
            "-metadata",
            "artist=Artist1",
            "-metadata",
            "album=Album A",
            a_s,
        ]);
        run(&[
            "-i",
            a_s,
            "-c",
            "copy",
            "-id3v2_version",
            "3",
            "-metadata",
            "title=Short Renamed",
            b.to_str().unwrap(),
        ]);
        (fs::read(a).unwrap(), fs::read(b).unwrap())
    }

    // A paired track of a second or less: the duration rule cannot fire (the
    // difference to the reset mark is under the tolerance), so only the mark
    // makes the plan replace the audio.
    #[test]
    fn a_wrong_pair_of_a_track_under_a_second_is_planned_as_a_replace() {
        let mut w = World::new(3);
        let tmp = tempfile::tempdir().unwrap();
        let (old_bytes, new_bytes) = short_mp3_pair(tmp.path());
        let src = w.files[0].clone();
        fs::write(&src, &old_bytes).unwrap();
        w.rescan();
        assert!(w.env.track(&src).info.duration_ms.unwrap() < 1000);
        w.sync().unwrap();
        assert!(w.prepare().plan.is_empty(), "settled before the move");
        let id = w.id_of(&src);

        fs::remove_file(&src).unwrap();
        let new_path = w.env.add_bytes("Short Renamed.mp3", &new_bytes);
        let scan = w.env.scan();
        w.reload();
        assert_eq!(scan.moved.len(), 1, "{scan:?}");
        assert_eq!(w.id_of(&new_path), id);

        let p = w.prepare();
        assert_eq!(p.plan.replace.len(), 1, "{:?}", p.plan);
        assert_eq!(p.plan.replace[0].track_id, id);
        assert_eq!(p.plan.replace[0].old_dbid, dbid(1));
        w.sync().unwrap();
        assert_eq!(
            fs::read(w.file_of(dbid(1))).unwrap(),
            fs::read(&new_path).unwrap()
        );
        assert!(w.prepare().plan.is_empty());
    }
}

// ---- audit step 7: tags (M13, M15, M16)

/// A 3 second copy of Mp3 fixture `variant` whose tag holds only `frames`.
fn long_mp3_with(variant: u32, frames: &[Vec<u8>]) -> Vec<u8> {
    mp3_secs_with_id3v2(variant, 3, frames)
}

fn ipod_title(w: &World, d: u64) -> Option<String> {
    w.open()
        .track_by_dbid(d)
        .unwrap()
        .title()
        .map(|s| s.into_owned())
}

// Prevents (audit M16): a replace putting the file name over a title that
// exists only on the iPod, when the new file has no title tag.
#[test]
fn a_replace_keeps_the_ipod_title_when_the_new_file_has_no_title_tag() {
    let mut w = World::new(3);
    let src = w.files[1].clone();
    // Same audio, no title: a retag leaves the iPod title.
    w.env.add_bytes(
        "s2.mp3",
        &mp3_with_id3v2(2, &[id3v24_text("TPE1", 3, b"Artist2")]),
    );
    w.rescan();
    assert_eq!(w.env.track(&src).tags.title, None, "set up: no title tag");
    w.sync().unwrap();
    assert_eq!(ipod_title(&w, dbid(2)).as_deref(), Some("Title2"));

    // New, longer audio, a new artist, still no title tag.
    let bytes = long_mp3_with(2, &[id3v24_text("TPE1", 3, b"New Artist")]);
    w.env.add_bytes("s2.mp3", &bytes);
    w.rescan();
    let p = w.prepare();
    assert_eq!(p.plan.replace.len(), 1, "{:?}", p.plan);
    let work = exec::full_work(&p);
    assert_eq!(work.tracks.copies.len(), 1);
    assert!(work.tracks.copies[0].replace);
    let out = w.sync().unwrap();
    assert_eq!((out.copied, out.recopied), (0, 1));

    assert_eq!(
        ipod_title(&w, dbid(2)).as_deref(),
        Some("Title2"),
        "the replace overwrote the iPod-only title"
    );
    // The replace did happen, and the other tags of the file were applied.
    assert_eq!(fs::read(w.file_of(dbid(2))).unwrap(), bytes);
    let t = w.open();
    assert_eq!(
        t.track_by_dbid(dbid(2)).unwrap().artist().as_deref(),
        Some("New Artist")
    );
    assert!(w.prepare().plan.is_empty(), "settled");
}

// Prevents (audit M16): the same loss for a file whose tags fail to parse.
#[test]
fn a_replace_keeps_the_ipod_title_when_the_new_files_tags_fail_to_parse() {
    let mut w = World::new(3);
    let src = w.files[1].clone();
    // A TRCK frame in UTF-16 with a byte order mark that is neither FF FE nor
    // FE FF: not even Relaxed parsing reads the tag.
    let bad = id3v24_text("TRCK", 1, &[0x31, 0x00, 0x32, 0x00]);
    let bytes = long_mp3_with(2, &[id3v24_text("TIT2", 3, b"Lost"), bad]);
    w.env.add_bytes("s2.mp3", &bytes);
    w.rescan();
    let row = w.env.track(&src);
    assert!(row.tag_error.is_some(), "set up: the tags fail to parse");
    assert_eq!(row.tags.title, None, "set up: nothing half-parsed is kept");
    let p = w.prepare();
    assert_eq!(p.plan.replace.len(), 1, "{:?}", p.plan);
    let out = w.sync().unwrap();
    assert_eq!((out.copied, out.recopied), (0, 1));

    assert_eq!(ipod_title(&w, dbid(2)).as_deref(), Some("Title2"));
    assert_eq!(fs::read(w.file_of(dbid(2))).unwrap(), bytes);
    assert_eq!(w.mapping()[&w.id_of(&src)], dbid(2));
}

// A new track still gets the file name when the file has no title.
#[test]
fn a_new_copy_of_a_file_without_a_title_tag_gets_the_file_name() {
    let mut w = World::new(3);
    let new = w.env.add_bytes(
        "Untitled Song.mp3",
        &mp3_with_id3v2(21, &[id3v24_text("TPE1", 3, b"Someone")]),
    );
    w.rescan();
    assert_eq!(w.env.track(&new).tags.title, None, "set up: no title tag");
    let mut road = w.files.clone();
    road.push(new.clone());
    w.write_list("Road", &road);
    w.reload();
    let p = w.prepare();
    let work = exec::full_work(&p);
    assert_eq!(work.tracks.copies.len(), 1);
    assert!(!work.tracks.copies[0].replace);
    let d = work.tracks.copies[0].dbid;
    let out = w.run(
        &p,
        &work,
        Options::default(),
        &|_| {},
        &AtomicBool::new(false),
    );
    assert_eq!(out.unwrap().copied, 1);
    assert_eq!(ipod_title(&w, d).as_deref(), Some("Untitled Song"));
}

const OLD_AUDIO_DATA: [(&str, &str); 9] = [
    ("pregap", "576"),
    ("postgap", "1100"),
    ("samplecount", "8820000"),
    ("gapless_data", "12345"),
    ("gapless_track_flag", "1"),
    ("soundcheck", "1200"),
    ("starttime", "5000"),
    ("stoptime", "190000"),
    ("bookmark_time", "7000"),
];

// Prevents (audit M13): a replace leaving the old file's gapless data, start
// and stop times, Sound Check and bookmark on the new audio.
#[test]
fn a_replace_clears_what_described_the_old_audio_and_keeps_plays_rating_and_dates() {
    let mut w = World::new(3);
    seed_old_audio(&w.root, dbid(3));
    let before = read_audio_data(&w.root, dbid(3));
    for (k, v) in OLD_AUDIO_DATA {
        assert_eq!(before[k], v, "set up: {k}");
    }
    let src = w.files[2].clone();
    fs::copy(fixture_secs(Kind::Mp3, 3, 3), &src).unwrap();
    w.rescan();
    let p = w.prepare();
    assert_eq!(p.plan.replace.len(), 1, "{:?}", p.plan);
    let out = w.sync().unwrap();
    assert_eq!((out.copied, out.recopied), (0, 1));

    let after = read_audio_data(&w.root, dbid(3));
    for (k, _) in OLD_AUDIO_DATA {
        assert_eq!(after[k], "0", "{k} still describes the old file");
    }
    // The new audio's rate.
    let rate = w.open().track_by_dbid(dbid(3)).unwrap().samplerate();
    assert_ne!(rate, 22050, "set up: the replace set the new rate");
    assert_eq!(after["samplerate"], rate.to_string());
    // What belongs to the user, or to the album, stays.
    for k in [
        "playcount",
        "skipcount",
        "rating",
        "time_added",
        "time_played",
        "gapless_album_flag",
    ] {
        assert_eq!(after[k], before[k], "{k} was changed by the replace");
    }
    // The audio really was replaced.
    assert!(w.track(dbid(3)).unwrap().adopt.duration_ms > 2500);
}

// A tag update does not touch the audio data: only a replace resets it.
#[test]
fn a_retag_leaves_the_audio_data_of_the_track_alone() {
    let mut w = World::new(3);
    seed_old_audio(&w.root, dbid(2));
    let before = read_audio_data(&w.root, dbid(2));
    w.env.add_bytes(
        "s2.mp3",
        &retagged_mp3(2, "Other Title", "Artist2", None, None),
    );
    w.rescan();
    let p = w.prepare();
    assert!(p.plan.replace.is_empty(), "{:?}", p.plan);
    let out = w.sync().unwrap();
    assert_eq!((out.retagged, out.recopied), (1, 0));
    assert_eq!(ipod_title(&w, dbid(2)).as_deref(), Some("Other Title"));
    assert_eq!(read_audio_data(&w.root, dbid(2)), before);
}

// Prevents (audit M15): a multi-value tag reaching the iPod cut to its first
// value, and the library's stored tags disagreeing with what was sent.
#[test]
fn a_file_with_several_values_per_tag_puts_the_joined_values_on_the_ipod() {
    let mut w = World::new(3);
    let multi = |variant: u32| {
        mp3_with_id3v2(
            variant,
            &[
                id3v24_text("TIT2", 3, b"Title2"),
                id3v24_text("TPE1", 3, b"Artist A\0Artist B"),
                id3v24_text("TALB", 3, b"Album\0Second"),
                id3v24_text("TCON", 3, b"Rock\0Pop"),
            ],
        )
    };
    // An existing track is retagged...
    w.env.add_bytes("s2.mp3", &multi(2));
    // ... and a new one is copied.
    let new = w.env.add_bytes("new.mp3", &multi(21));
    w.rescan();
    let mut road = w.files.clone();
    road.push(new.clone());
    w.write_list("Road", &road);
    w.reload();
    let p = w.prepare();
    let work = exec::full_work(&p);
    assert_eq!(work.tracks.copies.len(), 1);
    let d = work.tracks.copies[0].dbid;
    let out = w.run(
        &p,
        &work,
        Options::default(),
        &|_| {},
        &AtomicBool::new(false),
    );
    let out = out.unwrap();
    assert_eq!((out.copied, out.retagged), (1, 1));
    for dbid in [dbid(2), d] {
        let db = w.open();
        let t = db.track_by_dbid(dbid).unwrap();
        assert_eq!(t.artist().as_deref(), Some("Artist A/Artist B"), "{dbid:x}");
        assert_eq!(t.album().as_deref(), Some("Album/Second"), "{dbid:x}");
        assert_eq!(t.genre().as_deref(), Some("Rock/Pop"), "{dbid:x}");
    }
    assert!(w.prepare().plan.is_empty(), "the second plan is empty");
}
