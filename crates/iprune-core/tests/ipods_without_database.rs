//! `device::ipods_without_database_in` on a crafted mountinfo whose mount
//! points are folders in a temp dir: nothing real is looked at.

use std::fs;
use std::path::{Path, PathBuf};

use iprune_core::device::ipods_without_database_in;

/// A mount folder holding `files` (paths under it).
fn mount(root: &Path, name: &str, files: &[&str]) -> PathBuf {
    let m = root.join(name);
    fs::create_dir_all(&m).unwrap();
    for f in files {
        let p = m.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "x").unwrap();
    }
    m
}

fn line(id: u32, mountpoint: &Path, fstype: &str) -> String {
    format!(
        "{id} 22 8:{id} / {} rw,relatime shared:1 - {fstype} /dev/sd{id} rw\n",
        mountpoint.display()
    )
}

const SYS: &str = "iPod_Control/Device/SysInfo";
const EXT: &str = "iPod_Control/Device/SysInfoExtended";
const DB: &str = "iPod_Control/iTunes/iTunesDB";

#[test]
fn only_vfat_mounts_with_device_files_and_no_database_are_found() {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    let sys_only = mount(r, "sys_only", &[SYS]);
    let ext_only = mount(r, "ext_only", &[EXT]);
    let with_db = mount(r, "with_db", &[SYS, DB]);
    let ext4 = mount(r, "ext4", &[SYS]);
    let bare = mount(r, "bare", &["music/a.mp3"]);
    let db_only = mount(r, "db_only", &[DB]);
    let text = format!(
        "{}{}{}{}{}{}{}",
        line(30, Path::new("/"), "ext4"),
        line(31, &sys_only, "vfat"),
        line(32, &ext_only, "vfat"),
        line(33, &with_db, "vfat"),
        line(34, &ext4, "ext4"),
        line(35, &bare, "vfat"),
        line(36, &db_only, "vfat"),
    );
    let found: Vec<PathBuf> = ipods_without_database_in(&text)
        .into_iter()
        .map(|m| m.mountpoint)
        .collect();
    assert_eq!(found, vec![sys_only, ext_only]);
}

#[test]
fn a_mountinfo_with_no_candidate_gives_nothing() {
    assert!(ipods_without_database_in("").is_empty());
    assert!(ipods_without_database_in("garbage line\n").is_empty());
}
