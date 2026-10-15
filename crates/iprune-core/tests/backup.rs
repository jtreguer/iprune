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
