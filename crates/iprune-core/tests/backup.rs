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
    assert_eq!(done, vec!["iTunesDB", "ArtworkDB"]);
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
    assert_eq!(done, vec!["iTunesDB", "ArtworkDB"]);
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
