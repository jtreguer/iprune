//! `backup`: stamps, creating, listing, pruning and restoring, on temp
//! folders standing in for the data folder and the iPod.

use std::fs;
use std::path::{Path, PathBuf};

use iprune_core::Error;
use iprune_core::backup::{self, Backup};

const GUID: &str = "000A27001234ABCD";

struct Ipod {
    dir: tempfile::TempDir,
}

impl Ipod {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("iPod_Control/iTunes")).unwrap();
        fs::create_dir_all(dir.path().join("iPod_Control/Artwork")).unwrap();
        Ipod { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn db(&self) -> PathBuf {
        self.root().join("iPod_Control/iTunes/iTunesDB")
    }

    fn artwork(&self) -> PathBuf {
        self.root().join("iPod_Control/Artwork/ArtworkDB")
    }

    fn counts(&self) -> PathBuf {
        self.root().join("iPod_Control/iTunes/Play Counts")
    }

    fn full(&self, tag: &str) {
        fs::write(self.db(), format!("db {tag}")).unwrap();
        fs::write(self.artwork(), format!("art {tag}")).unwrap();
        fs::write(self.counts(), format!("counts {tag}")).unwrap();
    }
}

fn root() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn stamp_formats_known_dates_in_utc() {
    let cases: &[(i64, &str)] = &[
        (0, "19700101-000000"),
        (-1, "19691231-235959"),
        (951_782_400, "20000229-000000"), // a leap day (divisible by 400)
        (1_709_210_096, "20240229-123456"), // a leap day, mid-day
        (951_868_800, "20000301-000000"), // the day after
        (4_107_542_399, "21000228-235959"), // 2100 is not a leap year
        (4_107_542_400, "21000301-000000"),
        (1_700_000_000, "20231114-221320"),
        (1_792_000_000, "20261014-174640"),
    ];
    for (unix, want) in cases {
        assert_eq!(backup::stamp(*unix), *want, "for {unix}");
    }
}

#[test]
fn create_copies_the_files_and_writes_the_extras() {
    let ipod = Ipod::new();
    ipod.full("one");
    let data = root();
    let b = backup::create(
        data.path(),
        GUID,
        ipod.root(),
        1_700_000_000,
        &[("plan.json", b"{}"), ("snapshot.txt", b"snap")],
    )
    .unwrap();
    assert_eq!(b.stamp, "20231114-221320");
    assert_eq!(b.dir, data.path().join(GUID).join("20231114-221320"));
    let read = |n: &str| fs::read_to_string(b.dir.join(n)).unwrap();
    assert_eq!(read("iTunesDB"), "db one");
    assert_eq!(read("ArtworkDB"), "art one");
    assert_eq!(read("Play Counts"), "counts one");
    assert_eq!(read("plan.json"), "{}");
    assert_eq!(read("snapshot.txt"), "snap");
    // The iPod itself is untouched.
    assert_eq!(fs::read_to_string(ipod.db()).unwrap(), "db one");
}

#[test]
fn a_second_backup_in_the_same_second_gets_a_suffix_and_the_first_is_kept() {
    let ipod = Ipod::new();
    ipod.full("one");
    let data = root();
    let a = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    ipod.full("two");
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    ipod.full("three");
    let c = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    assert_eq!(a.stamp, "19700101-000140");
    assert_eq!(b.stamp, "19700101-000140-2");
    assert_eq!(c.stamp, "19700101-000140-3");
    let db = |x: &Backup| fs::read_to_string(x.dir.join("iTunesDB")).unwrap();
    assert_eq!(
        (db(&a), db(&b), db(&c)),
        ("db one".into(), "db two".into(), "db three".into())
    );
}

#[test]
fn a_missing_itunesdb_is_an_error_and_leaves_no_listed_backup() {
    let ipod = Ipod::new();
    fs::write(ipod.artwork(), "art").unwrap();
    let data = root();
    let e =
        backup::create(data.path(), GUID, ipod.root(), 100, &[("plan.json", b"{}")]).unwrap_err();
    match e {
        Error::Io { path, source } => {
            assert_eq!(path, ipod.db());
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("{other}"),
    }
    assert!(backup::list(data.path(), GUID).unwrap().is_empty());
}

#[test]
fn artwork_and_play_counts_are_optional() {
    let ipod = Ipod::new();
    fs::write(ipod.db(), "db").unwrap();
    let data = root();
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    assert!(b.dir.join("iTunesDB").is_file());
    assert!(!b.dir.join("ArtworkDB").exists());
    assert!(!b.dir.join("Play Counts").exists());
    assert_eq!(backup::list(data.path(), GUID).unwrap(), vec![b]);
}

fn make(data: &Path, ipod: &Ipod, times: &[i64]) -> Vec<Backup> {
    times
        .iter()
        .map(|t| backup::create(data, GUID, ipod.root(), *t, &[]).unwrap())
        .collect()
}

#[test]
fn list_returns_the_backups_oldest_first_and_ignores_other_entries() {
    let ipod = Ipod::new();
    ipod.full("x");
    let data = root();
    // Created out of order on purpose; one pair shares a second.
    let made = make(data.path(), &ipod, &[3000, 1000, 2000, 1000]);
    let base = data.path().join(GUID);
    fs::create_dir(base.join("19990101-000000")).unwrap(); // no iTunesDB inside
    fs::write(base.join("stray.txt"), "x").unwrap();
    let listed: Vec<String> = backup::list(data.path(), GUID)
        .unwrap()
        .into_iter()
        .map(|b| b.stamp)
        .collect();
    assert_eq!(
        listed,
        vec![
            backup::stamp(1000),
            format!("{}-2", backup::stamp(1000)),
            backup::stamp(2000),
            backup::stamp(3000),
        ]
    );
    assert_eq!(made.len(), 4);
}

#[test]
fn list_of_an_unknown_ipod_is_empty() {
    let data = root();
    assert!(backup::list(data.path(), "nobody").unwrap().is_empty());
    assert!(
        backup::list(&data.path().join("absent"), GUID)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn prune_keeps_the_newest_and_only_touches_its_own_ipod() {
    let ipod = Ipod::new();
    ipod.full("x");
    let data = root();
    let made = make(data.path(), &ipod, &[1000, 2000, 3000, 4000]);
    let other = backup::create(data.path(), "OTHER", ipod.root(), 500, &[]).unwrap();

    assert!(backup::prune(data.path(), GUID, 10).unwrap().is_empty());
    assert_eq!(backup::list(data.path(), GUID).unwrap().len(), 4);

    let removed = backup::prune(data.path(), GUID, 2).unwrap();
    assert_eq!(removed, vec![made[0].dir.clone(), made[1].dir.clone()]);
    assert_eq!(
        backup::list(data.path(), GUID).unwrap(),
        vec![made[2].clone(), made[3].clone()]
    );
    assert!(!made[0].dir.exists() && !made[1].dir.exists());
    assert!(
        other.dir.join("iTunesDB").is_file(),
        "another iPod's backups stay"
    );

    // Pruning again changes nothing.
    assert!(backup::prune(data.path(), GUID, 2).unwrap().is_empty());
}

#[test]
fn restore_puts_back_itunesdb_and_artworkdb_but_never_play_counts() {
    let ipod = Ipod::new();
    ipod.full("old");
    let data = root();
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    ipod.full("new");
    let done = backup::restore_files(&b, ipod.root()).unwrap();
    // ArtworkDB first, iTunesDB last: see `restore_renames_the_itunesdb_last`.
    assert_eq!(done, vec!["ArtworkDB", "iTunesDB"]);
    assert_eq!(fs::read_to_string(ipod.db()).unwrap(), "db old");
    assert_eq!(fs::read_to_string(ipod.artwork()).unwrap(), "art old");
    assert_eq!(
        fs::read_to_string(ipod.counts()).unwrap(),
        "counts new",
        "Play Counts is not restored"
    );
    // No temporary file is left behind.
    for dir in ["iPod_Control/iTunes", "iPod_Control/Artwork"] {
        let leftovers: Vec<_> = fs::read_dir(ipod.root().join(dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with(".iprune"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}

#[test]
fn restore_skips_a_file_the_backup_does_not_have() {
    let ipod = Ipod::new();
    fs::write(ipod.db(), "db old").unwrap();
    let data = root();
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    ipod.full("new");
    let done = backup::restore_files(&b, ipod.root()).unwrap();
    assert_eq!(done, vec!["iTunesDB"]);
    assert_eq!(fs::read_to_string(ipod.db()).unwrap(), "db old");
    assert_eq!(fs::read_to_string(ipod.artwork()).unwrap(), "art new");
}

#[test]
fn on_the_go_playlist_info_is_backed_up_but_never_restored() {
    let ipod = Ipod::new();
    ipod.full("old");
    let otg = ipod.root().join("iPod_Control/iTunes/OTGPlaylistInfo");
    fs::write(&otg, "otg old").unwrap();
    let data = root();
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    assert_eq!(
        fs::read_to_string(b.dir.join("OTGPlaylistInfo")).unwrap(),
        "otg old"
    );
    // libgpod removes it on write; a restore must not bring it back.
    fs::remove_file(&otg).unwrap();
    ipod.full("new");
    let done = backup::restore_files(&b, ipod.root()).unwrap();
    // ArtworkDB first, iTunesDB last: see `restore_renames_the_itunesdb_last`.
    assert_eq!(done, vec!["ArtworkDB", "iTunesDB"]);
    assert!(!otg.exists());
}

#[test]
fn a_backup_is_filled_under_a_hidden_name_and_nothing_partial_is_listed() {
    let ipod = Ipod::new();
    ipod.full("x");
    let data = root();
    // A leftover from an interrupted run, even with an iTunesDB inside.
    let stale = data
        .path()
        .join(GUID)
        .join(format!(".partial-{}", backup::stamp(100)));
    fs::create_dir_all(&stale).unwrap();
    fs::write(stale.join("iTunesDB"), "half").unwrap();
    assert!(backup::list(data.path(), GUID).unwrap().is_empty());
    // A new backup at the same second clears the leftover and completes.
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    assert_eq!(b.stamp, backup::stamp(100));
    assert_eq!(fs::read_to_string(b.dir.join("iTunesDB")).unwrap(), "db x");
    assert!(!stale.exists());
    assert_eq!(backup::list(data.path(), GUID).unwrap(), vec![b]);
}

#[test]
fn a_failed_create_leaves_nothing_listed_and_does_not_use_up_the_stamp() {
    let ipod = Ipod::new(); // no iTunesDB
    let data = root();
    assert!(backup::create(data.path(), GUID, ipod.root(), 100, &[]).is_err());
    fs::write(ipod.db(), "db").unwrap();
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    assert_eq!(b.stamp, backup::stamp(100), "no -2 suffix after a failure");
}

#[test]
fn list_orders_numeric_suffixes_by_value_not_by_text() {
    let ipod = Ipod::new();
    ipod.full("x");
    let data = root();
    let made = make(data.path(), &ipod, &[100; 11]);
    let stamps: Vec<String> = backup::list(data.path(), GUID)
        .unwrap()
        .into_iter()
        .map(|b| b.stamp)
        .collect();
    let want: Vec<String> = made.iter().map(|b| b.stamp.clone()).collect();
    assert_eq!(want.last().unwrap(), &format!("{}-11", backup::stamp(100)));
    assert_eq!(stamps, want, "-10 and -11 come after -9");
    // So pruning drops the oldest ones, not -10 and -11.
    backup::prune(data.path(), GUID, 2).unwrap();
    let left: Vec<String> = backup::list(data.path(), GUID)
        .unwrap()
        .into_iter()
        .map(|b| b.stamp)
        .collect();
    assert_eq!(left, want[9..]);
}

// BUG (reported, not fixed here): `create` takes the first unused name, so
// after a prune leaves a hole at the plain stamp, the next backup of that
// second reuses it, sorts as the oldest, and the next `prune` deletes the
// backup that was just taken. Only backups within one second can collide.
#[test]
fn the_newest_backup_sorts_last_even_after_a_prune_left_a_hole_in_its_second() {
    let ipod = Ipod::new();
    ipod.full("x");
    let data = root();
    make(data.path(), &ipod, &[100, 100, 100]); // 100, 100-2, 100-3
    backup::prune(data.path(), GUID, 2).unwrap(); // removes the plain stamp
    ipod.full("newest");
    let newest = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    let listed = backup::list(data.path(), GUID).unwrap();
    assert_eq!(listed.last(), Some(&newest), "{listed:?}");
    backup::prune(data.path(), GUID, 1).unwrap();
    assert_eq!(backup::list(data.path(), GUID).unwrap(), vec![newest]);
}

#[test]
fn create_removes_stale_partial_folders_of_any_stamp() {
    let ipod = Ipod::new();
    ipod.full("x");
    let data = root();
    let base = data.path().join(GUID);
    fs::create_dir_all(base.join(".partial-19990101-000000")).unwrap();
    fs::write(base.join(".partial-19990101-000000/iTunesDB"), "half").unwrap();
    let keep = base.join(".keepme");
    fs::create_dir_all(&keep).unwrap();
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    assert!(!base.join(".partial-19990101-000000").exists());
    assert!(keep.exists(), "only .partial-* folders are removed");
    assert!(b.dir.join("iTunesDB").is_file());
}

// ---- restore: staging, order and failure

// Prevents (audit M11) a restore that fails while renaming leaving the new
// `iTunesDB` next to an old `ArtworkDB`, or the reverse: `iTunesDB` goes
// last, so a failure before it leaves the old database whole.
#[test]
fn restore_renames_the_itunesdb_last() {
    let ipod = Ipod::new();
    ipod.full("old");
    let data = root();
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    ipod.full("new");
    // The ArtworkDB cannot be replaced: a folder (not empty) stands in its place.
    fs::remove_file(ipod.artwork()).unwrap();
    fs::create_dir(ipod.artwork()).unwrap();
    fs::write(ipod.artwork().join("inside"), "x").unwrap();

    let e = backup::restore_files(&b, ipod.root()).unwrap_err();
    assert!(
        matches!(&e, Error::Io { path, .. } if *path == ipod.artwork()),
        "{e:?}"
    );
    assert_eq!(
        fs::read_to_string(ipod.db()).unwrap(),
        "db new",
        "the database was not replaced before the artwork"
    );
    assert!(ipod.artwork().join("inside").is_file());
}

// Prevents (audit M11) a copy that fails half way having already replaced
// the first file: every file is copied and compared before any is renamed.
#[test]
fn a_restore_that_cannot_copy_one_file_changes_nothing_and_leaves_no_temporary_file() {
    let ipod = Ipod::new();
    ipod.full("old");
    let data = root();
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    ipod.full("new");
    // The iTunesDB is staged under this name; a folder is there already.
    let blocked = ipod
        .root()
        .join("iPod_Control/iTunes/.iprune-restore-iTunesDB");
    fs::create_dir(&blocked).unwrap();

    assert!(backup::restore_files(&b, ipod.root()).is_err());
    assert_eq!(fs::read_to_string(ipod.db()).unwrap(), "db new");
    assert_eq!(
        fs::read_to_string(ipod.artwork()).unwrap(),
        "art new",
        "the artwork, copied first, was not renamed over"
    );
    let staged = ipod
        .root()
        .join("iPod_Control/Artwork/.iprune-restore-ArtworkDB");
    assert!(!staged.exists(), "the earlier temporary copy is removed");
}

// ---- the copy a restore takes of what it replaces

#[test]
fn a_backup_before_a_restore_may_lack_the_itunesdb_but_keeps_the_rest_and_is_not_listed() {
    let ipod = Ipod::new();
    fs::write(ipod.counts(), "counts").unwrap();
    let data = root();
    // `create` still refuses: a sync backup without a database is useless.
    assert!(backup::create(data.path(), GUID, ipod.root(), 100, &[]).is_err());
    let b =
        backup::create_before_restore(data.path(), GUID, ipod.root(), 100, &[("x", b"1")]).unwrap();
    assert!(!b.dir.join("iTunesDB").exists());
    assert_eq!(
        fs::read_to_string(b.dir.join("Play Counts")).unwrap(),
        "counts"
    );
    assert_eq!(fs::read_to_string(b.dir.join("x")).unwrap(), "1");
    assert!(backup::list(data.path(), GUID).unwrap().is_empty());
}

// ---- set_aside

fn otg(ipod: &Ipod) -> PathBuf {
    ipod.root().join("iPod_Control/iTunes/OTGPlaylistInfo")
}

#[test]
fn set_aside_deletes_play_counts_and_otg_info_when_the_backup_holds_the_same_bytes() {
    let ipod = Ipod::new();
    ipod.full("x");
    fs::write(otg(&ipod), "otg").unwrap();
    let data = root();
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();

    let done = backup::set_aside(&b, ipod.root()).unwrap();
    assert_eq!(done, vec!["Play Counts", "OTGPlaylistInfo"]);
    assert!(!ipod.counts().exists() && !otg(&ipod).exists());
    // The database and the artwork are not touched, the copies are kept.
    assert!(ipod.db().is_file() && ipod.artwork().is_file());
    assert_eq!(
        fs::read_to_string(b.dir.join("Play Counts")).unwrap(),
        "counts x"
    );
    assert_eq!(
        fs::read_to_string(b.dir.join("OTGPlaylistInfo")).unwrap(),
        "otg"
    );
}

#[test]
fn set_aside_keeps_a_file_the_backup_does_not_hold_the_same_bytes_of() {
    let ipod = Ipod::new();
    ipod.full("x");
    let data = root();
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    // Changed after the backup was taken.
    fs::write(ipod.counts(), "counts changed").unwrap();
    let e = backup::set_aside(&b, ipod.root()).unwrap_err();
    assert!(
        matches!(&e, Error::BackupMismatch(p) if *p == b.dir.join("Play Counts")),
        "{e:?}"
    );
    assert_eq!(fs::read_to_string(ipod.counts()).unwrap(), "counts changed");

    // A file the backup has no copy of at all is kept too.
    fs::write(otg(&ipod), "new otg").unwrap();
    fs::write(ipod.counts(), "counts x").unwrap();
    let e = backup::set_aside(&b, ipod.root()).unwrap_err();
    assert!(
        matches!(&e, Error::BackupMismatch(p) if *p == b.dir.join("OTGPlaylistInfo")),
        "{e:?}"
    );
    assert_eq!(fs::read_to_string(otg(&ipod)).unwrap(), "new otg");
}

#[test]
fn set_aside_with_nothing_to_set_aside_does_nothing() {
    let ipod = Ipod::new();
    fs::write(ipod.db(), "db").unwrap();
    let data = root();
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    assert!(backup::set_aside(&b, ipod.root()).unwrap().is_empty());
    assert_eq!(fs::read_to_string(ipod.db()).unwrap(), "db");
}

// ---- summary

fn backup_with_snapshot(data: &Path, snapshot: Option<&str>) -> Backup {
    let ipod = Ipod::new();
    fs::write(ipod.db(), "db").unwrap();
    let extra: Vec<(&str, &[u8])> = snapshot
        .map(|s| ("snapshot.txt", s.as_bytes()))
        .into_iter()
        .collect();
    backup::create(data, GUID, ipod.root(), 100, &extra).unwrap()
}

const SNAPSHOT: &str = "tracks 3 playlists 2\n\
T 0000000000000001 :iPod_Control:Music:F00:T0000.mp3 | A | B | C | 10 | art 0 mhii 0 | rating 0 plays 0\n\
P iPod de Julien [master] 3\n\
P Road 1\n  0000000000000001\n";

#[test]
fn summary_gives_the_ipod_name_and_the_counts_from_the_snapshot() {
    let data = root();
    let b = backup_with_snapshot(data.path(), Some(SNAPSHOT));
    assert_eq!(
        backup::summary(&b).as_deref(),
        Some("iPod de Julien, 3 tracks, 2 playlists")
    );
}

#[test]
fn summary_takes_the_name_of_the_master_playlist_wherever_it_is() {
    let data = root();
    let snap = "tracks 0 playlists 2\nP Road 0\nP Julien's [x] iPod [master] 0\n";
    let b = backup_with_snapshot(data.path(), Some(snap));
    assert_eq!(
        backup::summary(&b).as_deref(),
        Some("Julien's [x] iPod, 0 tracks, 2 playlists")
    );
}

#[test]
fn summary_of_a_snapshot_without_a_master_playlist_still_gives_the_counts() {
    let data = root();
    let b = backup_with_snapshot(data.path(), Some("tracks 5 playlists 1\nP Road 0\n"));
    let s = backup::summary(&b).unwrap();
    assert!(s.ends_with(", 5 tracks, 1 playlists"), "{s}");
    assert!(
        !s.contains("Road"),
        "a plain playlist is not the iPod's name: {s}"
    );
}

#[test]
fn summary_is_none_without_a_readable_snapshot() {
    let data = root();
    let none = backup_with_snapshot(data.path(), None);
    assert_eq!(backup::summary(&none), None);
    for bad in ["", "\n", "garbage\n", "tracks 3\n"] {
        let data = root();
        let b = backup_with_snapshot(data.path(), Some(bad));
        assert_eq!(backup::summary(&b), None, "{bad:?}");
    }
}

// ---- cover art: the .ithmb files (audit H3)

fn hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

impl Ipod {
    fn ithmb(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.root().join("iPod_Control/Artwork").join(name);
        fs::write(&p, bytes).unwrap();
        p
    }

    fn ithmb_names(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(self.root().join("iPod_Control/Artwork"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }
}

fn store(data: &Path) -> PathBuf {
    data.join(GUID).join(".ithmb")
}

fn stored_names(data: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(store(data))
        .map(|rd| {
            rd.map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn ithmb_files_are_listed_whatever_their_case_and_dot_files_and_other_names_are_not() {
    let ipod = Ipod::new();
    ipod.ithmb("F1060_1.ithmb", b"a");
    ipod.ithmb("F1055_1.ITHMB", b"b");
    ipod.ithmb("f1068_1.Ithmb", b"c");
    ipod.ithmb(".iprune-restore-F1060_1.ithmb", b"temp");
    ipod.ithmb("._F1060_1.ithmb", b"appledouble");
    ipod.ithmb("F1060_1.ithmb.bak", b"x");
    ipod.ithmb("ithmb", b"x");
    fs::write(ipod.artwork(), "db").unwrap();
    fs::create_dir(ipod.root().join("iPod_Control/Artwork/F9_9.ithmb")).unwrap();

    let names: Vec<String> = backup::ithmb_files(ipod.root())
        .unwrap()
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["F1055_1.ITHMB", "F1060_1.ithmb", "f1068_1.Ithmb"]);
}

#[test]
fn ithmb_files_of_an_ipod_without_an_artwork_folder_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    assert!(backup::ithmb_files(dir.path()).unwrap().is_empty());
}

#[test]
fn a_backup_stores_each_ithmb_file_under_its_hash_and_lists_it() {
    let ipod = Ipod::new();
    ipod.full("x");
    ipod.ithmb("F1060_1.ithmb", b"big thumbnails");
    ipod.ithmb("F1055_1.ithmb", b"small ones");
    let data = root();
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();

    let list = backup::artwork_list(&b.dir).expect("an artwork list");
    assert_eq!(list.len(), 2);
    for (name, bytes) in [
        ("F1055_1.ithmb", &b"small ones"[..]),
        ("F1060_1.ithmb", &b"big thumbnails"[..]),
    ] {
        let item = list.iter().find(|i| i.name == name).expect(name);
        assert_eq!(item.size, bytes.len() as u64);
        assert_eq!(item.hash, hash(bytes));
        assert_eq!(
            fs::read(store(data.path()).join(&item.hash)).unwrap(),
            bytes
        );
    }
    let mut want = vec![hash(b"small ones"), hash(b"big thumbnails")];
    want.sort();
    assert_eq!(stored_names(data.path()), want, "no temporary file left");
}

#[test]
fn an_ipod_without_ithmb_files_gets_an_empty_list_and_no_store() {
    let ipod = Ipod::new();
    ipod.full("x");
    let data = root();
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    assert_eq!(backup::artwork_list(&b.dir), Some(vec![]));
    assert!(!store(data.path()).exists());
}

#[test]
fn files_with_the_same_content_are_stored_once_and_so_are_unchanged_files_of_a_later_backup() {
    let ipod = Ipod::new();
    ipod.full("x");
    ipod.ithmb("F1_1.ithmb", b"same");
    ipod.ithmb("F2_1.ithmb", b"same");
    ipod.ithmb("F3_1.ithmb", b"other");
    let data = root();
    let first = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    assert_eq!(stored_names(data.path()).len(), 2, "two contents");
    let mtime = |h: &str| {
        fs::metadata(store(data.path()).join(h))
            .unwrap()
            .modified()
            .unwrap()
    };
    let m = mtime(&hash(b"same"));

    // Nothing changes: the second backup lists the same hashes, adds nothing.
    let second = backup::create(data.path(), GUID, ipod.root(), 200, &[]).unwrap();
    assert_eq!(
        backup::artwork_list(&second.dir),
        backup::artwork_list(&first.dir)
    );
    assert_eq!(stored_names(data.path()).len(), 2);
    assert_eq!(mtime(&hash(b"same")), m, "the stored file is not rewritten");

    // One file changes: its new content is stored, the old one is kept for
    // the first backup.
    ipod.ithmb("F3_1.ithmb", b"changed");
    let third = backup::create(data.path(), GUID, ipod.root(), 300, &[]).unwrap();
    let names = stored_names(data.path());
    assert_eq!(names.len(), 3, "{names:?}");
    for h in [hash(b"same"), hash(b"other"), hash(b"changed")] {
        assert!(names.contains(&h));
    }
    let item = |b: &Backup, n: &str| {
        backup::artwork_list(&b.dir)
            .unwrap()
            .into_iter()
            .find(|i| i.name == n)
            .unwrap()
            .hash
    };
    assert_eq!(item(&first, "F3_1.ithmb"), hash(b"other"));
    assert_eq!(item(&third, "F3_1.ithmb"), hash(b"changed"));
}

// A file with the name, size and time of the previous backup's is not read
// again: here it cannot be read at all, and the backup still succeeds.
#[test]
#[cfg(unix)]
fn an_ithmb_file_unchanged_since_the_last_backup_is_not_read_again() {
    use std::os::unix::fs::PermissionsExt;
    let ipod = Ipod::new();
    ipod.full("x");
    let f = ipod.ithmb("F1_1.ithmb", b"large and slow to read");
    let data = root();
    let first = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o000)).unwrap();
    let readable = fs::File::open(&f).is_ok();
    let second = backup::create(data.path(), GUID, ipod.root(), 200, &[]);
    fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
    if readable {
        eprintln!("running as root: permissions do not block reads, test skipped");
        return;
    }
    let second = second.unwrap();
    assert_eq!(
        backup::artwork_list(&second.dir),
        backup::artwork_list(&first.dir)
    );
}

fn two_backups_sharing_a_file(ipod: &Ipod, data: &Path) -> (Backup, Backup) {
    ipod.full("x");
    ipod.ithmb("F1_1.ithmb", b"only in the first");
    ipod.ithmb("F2_1.ithmb", b"in both");
    let first = backup::create(data, GUID, ipod.root(), 100, &[]).unwrap();
    fs::remove_file(ipod.root().join("iPod_Control/Artwork/F1_1.ithmb")).unwrap();
    ipod.ithmb("F3_1.ithmb", b"only in the second");
    let second = backup::create(data, GUID, ipod.root(), 200, &[]).unwrap();
    (first, second)
}

#[test]
fn prune_deletes_only_the_stored_files_no_backup_lists() {
    let ipod = Ipod::new();
    let data = root();
    let (first, second) = two_backups_sharing_a_file(&ipod, data.path());
    assert_eq!(stored_names(data.path()).len(), 3);

    // Nothing pruned, nothing deleted.
    assert!(backup::prune(data.path(), GUID, 2).unwrap().is_empty());
    assert_eq!(stored_names(data.path()).len(), 3);

    assert_eq!(
        backup::prune(data.path(), GUID, 1).unwrap(),
        vec![first.dir]
    );
    let mut want = vec![hash(b"in both"), hash(b"only in the second")];
    want.sort();
    assert_eq!(stored_names(data.path()), want);
    // What the kept backup lists is all there, byte for byte.
    for i in backup::artwork_list(&second.dir).unwrap() {
        assert_eq!(
            fs::read(store(data.path()).join(&i.hash)).unwrap().len() as u64,
            i.size
        );
    }
}

#[test]
fn prune_does_not_touch_another_ipods_store() {
    let ipod = Ipod::new();
    ipod.full("x");
    ipod.ithmb("F1_1.ithmb", b"theirs");
    let data = root();
    backup::create(data.path(), "OTHER", ipod.root(), 100, &[]).unwrap();
    two_backups_sharing_a_file(&ipod, data.path());
    backup::prune(data.path(), GUID, 1).unwrap();
    assert!(
        data.path()
            .join("OTHER/.ithmb")
            .join(hash(b"theirs"))
            .is_file()
    );
}

#[test]
fn prune_keeps_every_stored_file_when_a_backups_list_cannot_be_read() {
    let ipod = Ipod::new();
    let data = root();
    let (first, second) = two_backups_sharing_a_file(&ipod, data.path());
    fs::write(second.dir.join("artwork.json"), "{ not json").unwrap();
    backup::prune(data.path(), GUID, 1).unwrap();
    assert!(!first.dir.exists());
    assert_eq!(
        stored_names(data.path()).len(),
        3,
        "an unreadable list may need any of them"
    );
}

#[test]
fn prune_keeps_the_files_of_a_backup_that_is_not_listed() {
    let ipod = Ipod::new();
    let data = root();
    // The copy a restore takes of an iPod with no iTunesDB: not listed.
    ipod.ithmb("F9_1.ithmb", b"kept by the unlisted one");
    let unlisted = backup::create_before_restore(data.path(), GUID, ipod.root(), 50, &[]).unwrap();
    // The file leaves the iPod: only the unlisted backup refers to it now.
    fs::remove_file(ipod.root().join("iPod_Control/Artwork/F9_1.ithmb")).unwrap();
    assert!(backup::list(data.path(), GUID).unwrap().is_empty());
    let (first, _second) = two_backups_sharing_a_file(&ipod, data.path());
    backup::prune(data.path(), GUID, 1).unwrap();
    assert!(!first.dir.exists());
    assert!(unlisted.dir.exists());
    assert!(
        store(data.path())
            .join(hash(b"kept by the unlisted one"))
            .is_file()
    );
}

#[test]
fn prune_ignores_a_backup_taken_before_artwork_lists_existed() {
    let ipod = Ipod::new();
    let data = root();
    let (first, second) = two_backups_sharing_a_file(&ipod, data.path());
    // Make the second look like an old backup, with no list at all.
    fs::remove_file(second.dir.join("artwork.json")).unwrap();
    backup::prune(data.path(), GUID, 1).unwrap();
    assert!(!first.dir.exists());
    assert!(
        stored_names(data.path()).is_empty(),
        "nothing lists them, and the old backup does not need them"
    );
}

fn restore_setup() -> (Ipod, tempfile::TempDir, Backup) {
    let ipod = Ipod::new();
    ipod.full("old");
    ipod.ithmb("F1_1.ithmb", b"old thumbnails one");
    ipod.ithmb("F2_1.ithmb", b"old thumbnails two");
    let data = root();
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    ipod.full("new");
    ipod.ithmb("F1_1.ithmb", b"rearranged");
    fs::remove_file(ipod.root().join("iPod_Control/Artwork/F2_1.ithmb")).unwrap();
    (ipod, data, b)
}

fn read(ipod: &Ipod, name: &str) -> Vec<u8> {
    fs::read(ipod.root().join("iPod_Control/Artwork").join(name)).unwrap()
}

#[test]
fn restore_puts_the_ithmb_files_back_with_the_artworkdb() {
    let (ipod, data, b) = restore_setup();
    let done = backup::restore_files(&b, ipod.root()).unwrap();
    assert!(
        done.contains(&"ArtworkDB") && done.contains(&"iTunesDB"),
        "{done:?}"
    );
    assert_eq!(read(&ipod, "F1_1.ithmb"), b"old thumbnails one");
    assert_eq!(read(&ipod, "F2_1.ithmb"), b"old thumbnails two");
    assert_eq!(read(&ipod, "ArtworkDB"), b"art old");
    assert_eq!(fs::read_to_string(ipod.db()).unwrap(), "db old");
    // The store is only read.
    assert_eq!(stored_names(data.path()).len(), 2);
    assert!(
        ipod.ithmb_names().iter().all(|n| !n.starts_with(".iprune")),
        "{:?}",
        ipod.ithmb_names()
    );
}

// Prevents (audit H3) restoring cover art from a store file that rotted:
// the restore stops before it renames anything.
#[test]
fn a_corrupted_store_file_makes_restore_refuse_before_any_rename() {
    let (ipod, data, b) = restore_setup();
    let victim = store(data.path()).join(hash(b"old thumbnails two"));
    fs::write(&victim, b"old thumbnails twp").unwrap();
    let e = backup::restore_files(&b, ipod.root()).unwrap_err();
    assert!(
        matches!(&e, Error::BackupMismatch(p) if *p == victim),
        "{e:?}"
    );
    // Nothing on the iPod changed, the first file (good in the store) included.
    assert_eq!(read(&ipod, "F1_1.ithmb"), b"rearranged");
    assert!(!ipod.root().join("iPod_Control/Artwork/F2_1.ithmb").exists());
    assert_eq!(read(&ipod, "ArtworkDB"), b"art new");
    assert_eq!(fs::read_to_string(ipod.db()).unwrap(), "db new");
    assert!(
        ipod.ithmb_names().iter().all(|n| !n.starts_with(".iprune")),
        "temporary copies are removed: {:?}",
        ipod.ithmb_names()
    );
}

#[test]
fn a_missing_store_file_makes_restore_refuse_before_any_rename() {
    let (ipod, data, b) = restore_setup();
    fs::remove_file(store(data.path()).join(hash(b"old thumbnails one"))).unwrap();
    assert!(backup::restore_files(&b, ipod.root()).is_err());
    assert_eq!(read(&ipod, "F1_1.ithmb"), b"rearranged");
    assert_eq!(read(&ipod, "ArtworkDB"), b"art new");
    assert_eq!(fs::read_to_string(ipod.db()).unwrap(), "db new");
    assert!(ipod.ithmb_names().iter().all(|n| !n.starts_with(".iprune")));
}

// The order of the renames: .ithmb files, then ArtworkDB, then iTunesDB. When
// the ArtworkDB cannot be replaced, the files it needs are back already and
// the iTunesDB is still the new one.
#[test]
fn restore_renames_the_ithmb_files_before_the_artworkdb_and_the_itunesdb_last() {
    let (ipod, _data, b) = restore_setup();
    fs::remove_file(ipod.artwork()).unwrap();
    fs::create_dir(ipod.artwork()).unwrap();
    fs::write(ipod.artwork().join("inside"), "x").unwrap();
    assert!(backup::restore_files(&b, ipod.root()).is_err());
    assert_eq!(read(&ipod, "F1_1.ithmb"), b"old thumbnails one");
    assert_eq!(fs::read_to_string(ipod.db()).unwrap(), "db new");
}

#[test]
fn restoring_a_backup_without_an_artwork_list_leaves_the_ithmb_files_alone() {
    let (ipod, _data, b) = restore_setup();
    fs::remove_file(b.dir.join("artwork.json")).unwrap();
    let done = backup::restore_files(&b, ipod.root()).unwrap();
    assert_eq!(done, vec!["ArtworkDB", "iTunesDB"]);
    assert_eq!(read(&ipod, "F1_1.ithmb"), b"rearranged");
    assert_eq!(read(&ipod, "ArtworkDB"), b"art old");
}

#[test]
fn artwork_unchanged_compares_names_and_content_with_the_backups_list() {
    let ipod = Ipod::new();
    ipod.full("x");
    let f1 = ipod.ithmb("F1_1.ithmb", b"one one");
    ipod.ithmb("F2_1.ithmb", b"two two");
    let data = root();
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    assert!(backup::artwork_unchanged(&b.dir, ipod.root()));

    // Rewritten with the same bytes (new time): still the same cover art.
    fs::write(&f1, b"one one").unwrap();
    assert!(backup::artwork_unchanged(&b.dir, ipod.root()));

    // Same size, one byte different.
    fs::write(&f1, b"one onf").unwrap();
    assert!(!backup::artwork_unchanged(&b.dir, ipod.root()), "content");
    // Truncated, as libgpod does after moving thumbnails.
    fs::write(&f1, b"one").unwrap();
    assert!(!backup::artwork_unchanged(&b.dir, ipod.root()), "size");
    // Back as it was.
    fs::write(&f1, b"one one").unwrap();
    assert!(backup::artwork_unchanged(&b.dir, ipod.root()));
    // A file libgpod removed.
    fs::remove_file(&f1).unwrap();
    assert!(!backup::artwork_unchanged(&b.dir, ipod.root()), "removed");
    // A file libgpod created.
    fs::write(&f1, b"one one").unwrap();
    ipod.ithmb("F9_1.ithmb", b"new");
    assert!(!backup::artwork_unchanged(&b.dir, ipod.root()), "added");
}

#[test]
fn artwork_unchanged_is_true_for_a_backup_taken_before_artwork_lists_existed() {
    let ipod = Ipod::new();
    ipod.full("x");
    ipod.ithmb("F1_1.ithmb", b"one");
    let data = root();
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    fs::remove_file(b.dir.join("artwork.json")).unwrap();
    ipod.ithmb("F1_1.ithmb", b"completely different");
    assert!(backup::artwork_unchanged(&b.dir, ipod.root()));
}

// ---- artwork_left_behind (audit H3)

fn left_behind_setup() -> (Ipod, tempfile::TempDir, Backup) {
    let ipod = Ipod::new();
    ipod.full("x");
    ipod.ithmb("F1_1.ithmb", b"one one one");
    ipod.ithmb("F2_1.ithmb", b"two two two");
    let data = root();
    let b = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    (ipod, data, b)
}

#[test]
fn artwork_left_behind_is_false_for_a_backup_without_a_list() {
    let (ipod, _d, b) = left_behind_setup();
    fs::remove_file(b.dir.join("artwork.json")).unwrap();
    ipod.ithmb("F1_1.ithmb", b"short");
    assert!(!backup::artwork_left_behind(&b.dir, ipod.root()));
}

#[test]
fn artwork_left_behind_is_false_when_nothing_changed() {
    let (ipod, _d, b) = left_behind_setup();
    assert!(!backup::artwork_left_behind(&b.dir, ipod.root()));
    // Same size, other bytes: not a rearrangement the size check sees.
    ipod.ithmb("F1_1.ithmb", b"ONE ONE ONE");
    assert!(!backup::artwork_left_behind(&b.dir, ipod.root()));
}

#[test]
fn artwork_left_behind_is_false_when_artworkdb_was_rewritten_too() {
    let (ipod, _d, b) = left_behind_setup();
    ipod.ithmb("F1_1.ithmb", b"short");
    fs::write(ipod.artwork(), "art rewritten").unwrap();
    assert!(!backup::artwork_left_behind(&b.dir, ipod.root()));
}

#[test]
fn artwork_left_behind_is_true_when_sizes_changed_and_artworkdb_is_the_same() {
    let (ipod, _d, b) = left_behind_setup();
    ipod.ithmb("F1_1.ithmb", b"short");
    assert!(backup::artwork_left_behind(&b.dir, ipod.root()));
}

#[test]
fn artwork_left_behind_is_true_when_a_file_is_added_and_artworkdb_is_the_same() {
    let (ipod, _d, b) = left_behind_setup();
    let extra = ipod.ithmb("F9_1.ithmb", b"new");
    assert!(backup::artwork_left_behind(&b.dir, ipod.root()), "added");
    fs::remove_file(extra).unwrap();
    assert!(!backup::artwork_left_behind(&b.dir, ipod.root()));
    // A file no thumbnail used is deleted by libgpod with ArtworkDB as it
    // was: a correct write, not a failed one.
    fs::remove_file(ipod.root().join("iPod_Control/Artwork/F2_1.ithmb")).unwrap();
    assert!(!backup::artwork_left_behind(&b.dir, ipod.root()), "removed");
}

// ---- review changes: hashing first, unchanged files, space, unreadable list, prune

// The store already holds this content: nothing is written to it (here it
// could not be: the folder is read-only), even though the file's time moved.
#[test]
#[cfg(unix)]
fn a_file_whose_content_is_in_the_store_is_not_copied_again_when_only_its_time_changed() {
    use std::os::unix::fs::PermissionsExt;
    let ipod = Ipod::new();
    ipod.full("x");
    let f = ipod.ithmb("F1_1.ithmb", b"thumbnails");
    let data = root();
    let first = backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    // Same bytes, new time: the name/size/time shortcut does not apply.
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(&f, b"thumbnails").unwrap();
    let st = store(data.path());
    fs::set_permissions(&st, fs::Permissions::from_mode(0o555)).unwrap();
    let blocked = fs::File::create(st.join(".probe")).is_err();
    let second = backup::create(data.path(), GUID, ipod.root(), 200, &[]);
    fs::set_permissions(&st, fs::Permissions::from_mode(0o755)).unwrap();
    if !blocked {
        eprintln!("running as root: permissions do not block writes, test skipped");
        return;
    }
    let second = second.unwrap();
    let hashes = |b: &Backup| -> Vec<String> {
        backup::artwork_list(&b.dir)
            .unwrap()
            .into_iter()
            .map(|i| i.hash)
            .collect()
    };
    assert_eq!(hashes(&second), hashes(&first));
    assert_eq!(stored_names(data.path()), vec![hash(b"thumbnails")]);
}

// Restoring leaves a cover art file alone when the iPod already holds the
// listed content: here its store copy is gone, and the restore still works.
#[test]
fn restore_does_not_touch_an_ithmb_file_that_already_has_the_listed_content() {
    let (ipod, data, b) = restore_setup();
    // F2 comes back to its backed up content by itself; F1 is still wrong.
    let f2 = ipod.ithmb("F2_1.ithmb", b"old thumbnails two");
    let before = fs::metadata(&f2).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::remove_file(store(data.path()).join(hash(b"old thumbnails two"))).unwrap();

    backup::restore_files(&b, ipod.root()).unwrap();
    assert_eq!(read(&ipod, "F1_1.ithmb"), b"old thumbnails one");
    assert_eq!(read(&ipod, "F2_1.ithmb"), b"old thumbnails two");
    assert_eq!(
        fs::metadata(&f2).unwrap().modified().unwrap(),
        before,
        "not replaced"
    );
}

fn set_listed_size(b: &Backup, name: &str, size: u64) {
    let path = b.dir.join("artwork.json");
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for item in v.as_array_mut().unwrap() {
        if item["name"] == name {
            item["size"] = size.into();
        }
    }
    fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
}

// A restore that cannot fit its temporary copies is refused up front. The
// list claims a file larger than any disk can have free.
#[test]
fn restore_refuses_when_the_temporary_copies_do_not_fit_and_changes_nothing() {
    let (ipod, _data, b) = restore_setup();
    set_listed_size(&b, "F1_1.ithmb", 1 << 62);
    let e = backup::restore_files(&b, ipod.root()).unwrap_err();
    assert!(
        matches!(&e, Error::NoRoomToRestore { needed, .. } if *needed >= 1 << 62),
        "{e:?}"
    );
    assert_eq!(read(&ipod, "F1_1.ithmb"), b"rearranged");
    assert_eq!(read(&ipod, "ArtworkDB"), b"art new");
    assert_eq!(fs::read_to_string(ipod.db()).unwrap(), "db new");
    assert!(ipod.ithmb_names().iter().all(|n| !n.starts_with(".iprune")));
}

// A list that exists but cannot be read must not be taken for a backup made
// before lists were kept: that would restore ArtworkDB without its files.
#[test]
fn restore_refuses_an_unreadable_artwork_list_and_changes_nothing() {
    let (ipod, _data, b) = restore_setup();
    fs::write(b.dir.join("artwork.json"), "{ not json").unwrap();
    let e = backup::restore_files(&b, ipod.root()).unwrap_err();
    assert!(
        matches!(&e, Error::Io { path, .. } if path.ends_with("artwork.json")),
        "{e:?}"
    );
    assert_eq!(read(&ipod, "F1_1.ithmb"), b"rearranged");
    assert_eq!(read(&ipod, "ArtworkDB"), b"art new");
    assert_eq!(fs::read_to_string(ipod.db()).unwrap(), "db new");
}

#[test]
fn an_unreadable_artwork_list_is_an_error_a_missing_one_is_none_and_unchanged_is_false() {
    let (ipod, _data, b) = restore_setup();
    assert!(backup::artwork_list_checked(&b.dir).unwrap().is_some());
    fs::write(b.dir.join("artwork.json"), "[1, 2").unwrap();
    assert!(backup::artwork_list_checked(&b.dir).is_err());
    assert_eq!(backup::artwork_list(&b.dir), None);
    assert!(!backup::artwork_unchanged(&b.dir, ipod.root()));
    fs::remove_file(b.dir.join("artwork.json")).unwrap();
    assert!(backup::artwork_list_checked(&b.dir).unwrap().is_none());
    assert!(backup::artwork_unchanged(&b.dir, ipod.root()));
}

#[test]
fn prune_deletes_unlisted_and_leftover_partial_store_files_even_when_it_removes_no_backup() {
    let ipod = Ipod::new();
    ipod.full("x");
    ipod.ithmb("F1_1.ithmb", b"listed");
    let data = root();
    backup::create(data.path(), GUID, ipod.root(), 100, &[]).unwrap();
    let st = store(data.path());
    fs::write(st.join(".partial-F2_1.ithmb"), "half a copy").unwrap();
    fs::write(st.join(hash(b"orphan")), "orphan").unwrap();

    assert!(backup::prune(data.path(), GUID, 5).unwrap().is_empty());
    assert_eq!(
        stored_names(data.path()),
        vec![hash(b"listed")],
        "only what a backup lists is left"
    );
}

// A truncated store file is found by the plan, before anything is staged
// (checking its content only when copying would come after the caller's
// first changes).
#[test]
fn a_truncated_store_file_is_refused_by_the_plan_and_by_restore() {
    let (ipod, data, b) = restore_setup();
    let victim = store(data.path()).join(hash(b"old thumbnails one"));
    fs::write(&victim, b"old thumb").unwrap();
    let e = backup::plan_restore(&b, ipod.root()).unwrap_err();
    assert!(
        matches!(&e, Error::BackupMismatch(p) if *p == victim),
        "{e:?}"
    );
    let e = backup::restore_files(&b, ipod.root()).unwrap_err();
    assert!(
        matches!(&e, Error::BackupMismatch(p) if *p == victim),
        "{e:?}"
    );
    assert_eq!(read(&ipod, "F1_1.ithmb"), b"rearranged");
    assert_eq!(read(&ipod, "ArtworkDB"), b"art new");
    assert_eq!(fs::read_to_string(ipod.db()).unwrap(), "db new");
    assert!(ipod.ithmb_names().iter().all(|n| !n.starts_with(".iprune")));
}
