//! Playlist changes and writing, on temp copies built by `tests/c/make_ipod.c`.
//! Nothing here opens a real iPod: databases live in temp dirs.

mod common;
use common::{FakeIpod, make_ipod_db};

use std::fs;
use std::path::{Path, PathBuf};

use gpod::{Database, Error};

const T1: u64 = 0x1;
const T2: u64 = 0x2;
const T3: u64 = 0x3;
const T4: u64 = 0xffff_ffff_ffff_fff0;
const MISSING: u64 = 0x99;

const MANIFEST: &str = "M\tiPod de Test
T\t1\t1000\t1000\tOne\tArtA\tAlb\t0
T\t2\t1000\t1000\tTwo\tArtB\tAlb\t0
T\t3\t1000\t1000\tThree\tArtC\tAlb\t0
T\tfffffffffffffff0\t1000\t1000\tFour\tArtD\tAlb\t0
P\tAlpha\t1,2,2
P\tBeta\t3
S\tSmart\t1,2
R\tSmart\tBjörk
C\tPodcasts
";

struct Fx {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("iTunesDB");
        make_ipod_db(dir.path(), &path, MANIFEST);
        Fx { _dir: dir, path }
    }

    fn db(&self) -> Database {
        Database::open_file(&self.path).unwrap()
    }

    fn out(&self, name: &str) -> PathBuf {
        self._dir.path().join(name)
    }
}

fn snap(db: &Database) -> String {
    let mut out = Vec::new();
    gpod::snapshot::write(db, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn id_of(db: &Database, name: &str) -> u64 {
    db.playlists()
        .find(|p| p.name().as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no playlist {name}"))
        .id()
}

fn members(db: &Database, id: u64) -> Vec<u64> {
    db.playlists()
        .find(|p| p.id() == id)
        .unwrap()
        .members()
        .map(|t| t.dbid())
        .collect()
}

fn names(db: &Database) -> Vec<String> {
    db.playlists()
        .map(|p| p.name().unwrap().into_owned())
        .collect()
}

/// Writes `db` to a new file and reads it back.
fn round_trip(fx: &Fx, db: &mut Database, name: &str) -> Database {
    let p = fx.out(name);
    db.write_file(&p).unwrap();
    Database::open_file(&p).unwrap()
}

#[test]
fn create_playlist_appends_an_empty_playlist_with_a_fresh_id() {
    let fx = Fx::new();
    let mut db = fx.db();
    let before: Vec<u64> = db.playlists().map(|p| p.id()).collect();
    let a = db.create_playlist("New é").unwrap();
    let b = db.create_playlist("Other").unwrap();
    assert_ne!(a, 0);
    assert_ne!(a, b);
    assert!(!before.contains(&a) && !before.contains(&b));
    let all = names(&db);
    assert_eq!(&all[all.len() - 2..], ["New é", "Other"]);
    assert_eq!(db.playlist_count(), before.len() + 2);
    assert!(members(&db, a).is_empty());

    let back = round_trip(&fx, &mut db, "out");
    assert_eq!(id_of(&back, "New é"), a, "the id survives a write");
    assert!(members(&back, a).is_empty());
}

#[test]
fn create_playlist_with_a_nul_in_the_name_is_refused_and_adds_nothing() {
    let fx = Fx::new();
    let mut db = fx.db();
    let before = snap(&db);
    let e = db.create_playlist("a\0b").unwrap_err();
    assert!(matches!(e, Error::NulInName(_)), "{e}");
    assert_eq!(snap(&db), before);
}

#[test]
fn rename_changes_only_the_name() {
    let fx = Fx::new();
    let mut db = fx.db();
    let (alpha, beta) = (id_of(&db, "Alpha"), id_of(&db, "Beta"));
    db.rename_playlist(alpha, "Alpha 2").unwrap();
    assert_eq!(
        names(&db),
        ["iPod de Test", "Alpha 2", "Beta", "Smart", "Podcasts"]
    );
    assert_eq!(members(&db, alpha), [T1, T2, T2]);
    assert_eq!(members(&db, beta), [T3]);

    let back = round_trip(&fx, &mut db, "out");
    assert_eq!(id_of(&back, "Alpha 2"), alpha);
    assert_eq!(members(&back, alpha), [T1, T2, T2]);
}

#[test]
fn rename_with_a_nul_leaves_the_old_name() {
    let fx = Fx::new();
    let mut db = fx.db();
    let alpha = id_of(&db, "Alpha");
    let before = snap(&db);
    let e = db.rename_playlist(alpha, "x\0y").unwrap_err();
    assert!(matches!(e, Error::NulInName(_)), "{e}");
    assert_eq!(snap(&db), before);
}

#[test]
fn remove_playlist_drops_it_and_keeps_the_tracks_and_the_others() {
    let fx = Fx::new();
    let mut db = fx.db();
    let (alpha, beta) = (id_of(&db, "Alpha"), id_of(&db, "Beta"));
    db.remove_playlist(alpha).unwrap();
    assert_eq!(names(&db), ["iPod de Test", "Beta", "Smart", "Podcasts"]);
    assert_eq!(db.track_count(), 4, "tracks stay in the database");
    assert_eq!(members(&db, beta), [T3]);
    assert_eq!(
        db.master_playlist().unwrap().len(),
        4,
        "the master still lists every track"
    );

    let back = round_trip(&fx, &mut db, "out");
    assert_eq!(names(&back), names(&db));
    assert_eq!(back.track_count(), 4);
}

#[test]
fn set_members_replaces_the_list_in_order_and_keeps_repeats() {
    let fx = Fx::new();
    let mut db = fx.db();
    let alpha = id_of(&db, "Alpha");
    db.set_playlist_members(alpha, &[T4, T2, T2, T1, T4])
        .unwrap();
    assert_eq!(members(&db, alpha), [T4, T2, T2, T1, T4]);
    // Setting the same list again must not double it.
    db.set_playlist_members(alpha, &[T4, T2, T2, T1, T4])
        .unwrap();
    assert_eq!(members(&db, alpha), [T4, T2, T2, T1, T4]);
    // A shorter list shrinks it, an empty one clears it.
    db.set_playlist_members(alpha, &[T3]).unwrap();
    assert_eq!(members(&db, alpha), [T3]);
    db.set_playlist_members(alpha, &[T1, T3]).unwrap();

    let back = round_trip(&fx, &mut db, "out");
    assert_eq!(members(&back, alpha), [T1, T3]);
    db.set_playlist_members(alpha, &[]).unwrap();
    assert!(members(&db, alpha).is_empty());
    let back = round_trip(&fx, &mut db, "empty");
    assert!(members(&back, alpha).is_empty());
    assert_eq!(back.track_count(), 4);
}

#[test]
fn set_members_with_an_unknown_track_changes_nothing() {
    let fx = Fx::new();
    let mut db = fx.db();
    let alpha = id_of(&db, "Alpha");
    let before = snap(&db);
    // The valid dbids come first: the check must happen before the clear.
    let e = db
        .set_playlist_members(alpha, &[T3, T1, MISSING])
        .unwrap_err();
    assert!(matches!(e, Error::NoSuchTrack(d) if d == MISSING), "{e}");
    assert_eq!(members(&db, alpha), [T1, T2, T2]);
    assert_eq!(snap(&db), before);
}

#[test]
fn master_smart_and_podcasts_playlists_are_refused_and_stay_as_they_were() {
    let fx = Fx::new();
    let mut db = fx.db();
    let master = db.master_playlist().unwrap().id();
    let smart = id_of(&db, "Smart");
    let podcasts = id_of(&db, "Podcasts");
    let before = snap(&db);
    for id in [master, smart, podcasts] {
        let e = db.rename_playlist(id, "Hacked").unwrap_err();
        assert!(matches!(e, Error::ProtectedPlaylist(i) if i == id), "{e}");
        let e = db.set_playlist_members(id, &[T1]).unwrap_err();
        assert!(matches!(e, Error::ProtectedPlaylist(i) if i == id), "{e}");
        let e = db.set_playlist_members(id, &[]).unwrap_err();
        assert!(matches!(e, Error::ProtectedPlaylist(i) if i == id), "{e}");
        let e = db.remove_playlist(id).unwrap_err();
        assert!(matches!(e, Error::ProtectedPlaylist(i) if i == id), "{e}");
    }
    assert_eq!(snap(&db), before);
    assert_eq!(names(&db)[0], "iPod de Test");
    assert!(db.playlists().any(|p| p.is_podcasts()));
}

#[test]
fn an_unknown_playlist_id_is_refused() {
    let fx = Fx::new();
    let mut db = fx.db();
    let before = snap(&db);
    for e in [
        db.rename_playlist(0xdead, "x").unwrap_err(),
        db.remove_playlist(0xdead).unwrap_err(),
        db.set_playlist_members(0xdead, &[T1]).unwrap_err(),
    ] {
        assert!(matches!(e, Error::NoSuchPlaylist(0xdead)), "{e}");
    }
    assert_eq!(snap(&db), before);
}

#[test]
fn a_write_file_round_trip_keeps_the_snapshot() {
    let fx = Fx::new();
    let mut db = fx.db();
    let alpha = id_of(&db, "Alpha");
    db.rename_playlist(alpha, "Alpha 2").unwrap();
    db.set_playlist_members(alpha, &[T4, T4, T1]).unwrap();
    let new = db.create_playlist("Fresh").unwrap();
    db.set_playlist_members(new, &[T2, T3]).unwrap();
    db.remove_playlist(id_of(&db, "Beta")).unwrap();
    let expected = snap(&db);
    let back = round_trip(&fx, &mut db, "out");
    assert_eq!(snap(&back), expected);
    assert!(expected.contains("P Alpha 2 3"), "{expected}");
    assert!(!expected.contains("P Beta"), "{expected}");
}

#[test]
fn write_file_to_a_missing_folder_fails_with_a_write_error() {
    let fx = Fx::new();
    let mut db = fx.db();
    let p = fx.out("no/such/dir/iTunesDB");
    let e = db.write_file(&p).unwrap_err();
    assert!(matches!(e, Error::Write(_)), "{e}");
    assert!(!p.exists());
}

// ---- raw smart playlist records

fn u32_at(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize
}

/// The raw bytes of every `mhod` of type 50 (smart preferences) and 51 (smart
/// rules) in an iTunesDB, in file order, found by walking
/// `mhbd` > `mhsd` > `mhlp` > `mhyp` > `mhod`.
fn smart_records(path: &Path) -> Vec<(u32, Vec<u8>)> {
    let b = fs::read(path).unwrap();
    assert_eq!(&b[..4], b"mhbd");
    let mut out = Vec::new();
    let mut at = u32_at(&b, 4);
    while at < b.len() {
        assert_eq!(&b[at..at + 4], b"mhsd");
        let (hl, total) = (u32_at(&b, at + 4), u32_at(&b, at + 8));
        let lp = at + hl;
        if &b[lp..lp + 4] == b"mhlp" {
            let (n, mut p) = (u32_at(&b, lp + 8), lp + u32_at(&b, lp + 4));
            for _ in 0..n {
                assert_eq!(&b[p..p + 4], b"mhyp");
                let (yhl, ytotal, mhods) =
                    (u32_at(&b, p + 4), u32_at(&b, p + 8), u32_at(&b, p + 12));
                let mut m = p + yhl;
                for _ in 0..mhods {
                    assert_eq!(&b[m..m + 4], b"mhod");
                    let (mtotal, ty) = (u32_at(&b, m + 8), u32_at(&b, m + 12) as u32);
                    if ty == 50 || ty == 51 {
                        out.push((ty, b[m..m + mtotal].to_vec()));
                    }
                    m += mtotal;
                }
                p += ytotal;
            }
        }
        at += total;
    }
    out
}

#[test]
fn smart_playlist_records_are_byte_identical_after_a_write() {
    let fx = Fx::new();
    let before = smart_records(&fx.path);
    // Not vacuous: preferences and the rule (UTF-16LE "Bj") are there.
    assert!(before.iter().any(|(t, _)| *t == 50), "{before:?}");
    let rule = before
        .iter()
        .find(|(t, _)| *t == 51)
        .expect("a rule record");
    assert!(
        rule.1.windows(4).any(|w| w == [b'B', 0, b'j', 0]),
        "{:?}",
        rule.1
    );

    let mut db = fx.db();
    let alpha = id_of(&db, "Alpha");
    db.rename_playlist(alpha, "Alpha 2").unwrap();
    db.set_playlist_members(alpha, &[T4, T1]).unwrap();
    let new = db.create_playlist("Fresh").unwrap();
    db.set_playlist_members(new, &[T2]).unwrap();
    db.remove_playlist(id_of(&db, "Beta")).unwrap();
    let out = fx.out("out");
    db.write_file(&out).unwrap();
    assert_eq!(smart_records(&out), before);
}

// ---- writing to a mountpoint

#[test]
fn write_persists_changes_to_the_ipod_database_in_the_mountpoint() {
    let ipod = FakeIpod::new(Some("FirewireGuid: 0x000A27001234ABCD\n"));
    let mut db = Database::open(&ipod.root()).unwrap();
    let id = db.create_playlist("Road").unwrap();
    let first = db.tracks().next().unwrap().dbid();
    db.set_playlist_members(id, &[first, first]).unwrap();
    let expected = snap(&db);
    let before = fs::read(ipod.itunesdb()).unwrap();
    db.start_sync();
    db.write().unwrap();
    db.stop_sync();
    drop(db);
    assert_ne!(fs::read(ipod.itunesdb()).unwrap(), before);
    let back = Database::open(&ipod.root()).unwrap();
    assert_eq!(members(&back, id), [first, first]);
    // libgpod renumbers the artwork link on write, nothing else may differ.
    let strip = |s: &str| -> String {
        s.lines()
            .map(|l| l.split(" mhii ").next().unwrap().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(strip(&snap(&back)), strip(&expected));
}
