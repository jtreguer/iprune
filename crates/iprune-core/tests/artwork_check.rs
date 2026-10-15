//! `exec::check_artwork` on real databases: a fake iPod written by libgpod
//! with cover art (a solid colour per track, `.ithmb` files and an
//! `ArtworkDB`), then damaged. Everything is in temp folders.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{IpodSpec, add_cover_art, fake_ipod};
use iprune_core::{Error, exec};

const SYSINFO: &str = "FirewireGuid: 0x000A27001234ABCD\n";

fn ipod_with_art(n: usize) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let tracks: Vec<IpodSpec> = (1..=n)
        .map(|i| IpodSpec {
            dbid: 0x1000 + i as u64,
            size: 1000,
            ms: 5000,
            title: Box::leak(format!("Title{i}").into_boxed_str()),
            artist: "A",
            album: "",
            plays: 0,
        })
        .collect();
    let root = fake_ipod(dir.path(), Some(SYSINFO), &tracks, &[]);
    add_cover_art(&root, SYSINFO);
    (dir, root)
}

fn check(root: &Path) -> iprune_core::Result<()> {
    let db = gpod::Database::open(root).unwrap();
    exec::check_artwork(&db, root)
}

fn artwork(root: &Path) -> PathBuf {
    root.join("iPod_Control/Artwork")
}

fn remove_ithmbs(root: &Path) {
    let mut n = 0;
    for e in fs::read_dir(artwork(root)).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "ithmb") {
            fs::remove_file(p).unwrap();
            n += 1;
        }
    }
    assert!(n > 0, "set up: there were .ithmb files");
}

#[test]
fn a_healthy_artwork_database_passes() {
    let (_d, root) = ipod_with_art(12);
    check(&root).unwrap();
}

#[test]
fn an_ipod_with_no_artwork_at_all_passes() {
    let dir = tempfile::tempdir().unwrap();
    let root = fake_ipod(dir.path(), Some(SYSINFO), &[], &[]);
    // No tracks, no .ithmb file, no ArtworkDB.
    check(&root).unwrap();
}

// Prevents (audit H9): the .ithmb files are there, no track holds a thumbnail.
#[test]
fn ithmb_files_with_no_thumbnail_on_any_track_are_refused_even_for_one_track() {
    let (_d, root) = ipod_with_art(1);
    fs::remove_file(artwork(&root).join("ArtworkDB")).unwrap();
    match check(&root).unwrap_err() {
        Error::ArtworkUnreadable { files, lost } => {
            assert!(files > 0);
            assert_eq!(lost, 1);
        }
        other => panic!("{other}"),
    }
}

// Prevents (audit H9): the .ithmb files are gone as well, the tracks still
// point at artwork. At most 5 such tracks is tolerated, 6 is not.
#[test]
fn six_tracks_whose_cover_is_lost_are_refused_and_five_are_not() {
    let (_d, five) = ipod_with_art(5);
    fs::remove_file(artwork(&five).join("ArtworkDB")).unwrap();
    remove_ithmbs(&five);
    check(&five).expect("5 lost tracks is within the tolerance");

    let (_d, six) = ipod_with_art(6);
    fs::remove_file(artwork(&six).join("ArtworkDB")).unwrap();
    remove_ithmbs(&six);
    match check(&six).unwrap_err() {
        Error::ArtworkUnreadable { files, lost } => {
            assert_eq!((files, lost), (0, 6));
        }
        other => panic!("{other}"),
    }
}

// Leftover .ithmb files that no track points at (the iPod's covers were
// removed, or never made by this database) are not a reason to refuse.
#[test]
fn leftover_ithmb_files_no_track_points_at_do_not_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let tracks: Vec<IpodSpec> = (1..=3)
        .map(|i| IpodSpec {
            dbid: 0x1000 + i,
            size: 1000,
            ms: 5000,
            title: "T",
            artist: "A",
            album: "",
            plays: 0,
        })
        .collect();
    let root = fake_ipod(dir.path(), Some(SYSINFO), &tracks, &[]);
    fs::create_dir_all(artwork(&root)).unwrap();
    fs::write(artwork(&root).join("F1060_1.ithmb"), vec![7u8; 4096]).unwrap();
    check(&root).unwrap();
}
