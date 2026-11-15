use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use super::*;

fn at(zone: &str, y: i16, mo: i8, d: i8, h: i8, mi: i8, s: i8) -> LocalTime {
    let tz = jiff::tz::TimeZone::get(zone).unwrap();
    let t = jiff::civil::date(y, mo, d)
        .at(h, mi, s, 0)
        .to_zoned_datetime(tz)
        .unwrap();
    LocalTime::of(&t)
}

// ---- the 12 bytes

#[test]
fn winter_in_paris_is_four_quarter_hours_without_dst() {
    let t = at("Europe/Paris", 2026, 11, 15, 23, 41, 7);
    assert_eq!(t.offset, 3600);
    assert!(!t.dst);
    // 15 November is day 319, sent as 318.
    assert_eq!(
        payload(&t),
        [0x07, 0xea, 0x01, 0x3e, 4, 23, 41, 7, 0, 0, 0, 0]
    );
}

#[test]
fn summer_in_paris_is_eight_quarter_hours_with_dst() {
    // libgpod's own example: UTC+2 is 8.
    let t = at("Europe/Paris", 2026, 7, 1, 12, 0, 0);
    assert_eq!(
        payload(&t),
        [0x07, 0xea, 0x00, 0xb5, 8, 12, 0, 0, 1, 0, 0, 0]
    );
}

#[test]
fn west_of_utc_is_a_negative_signed_byte() {
    // St John's: UTC-3:30 in winter, UTC-2:30 in summer.
    let winter = at("America/St_Johns", 2026, 1, 1, 0, 0, 0);
    assert_eq!(
        payload(&winter),
        [0x07, 0xea, 0, 0, (-14i8) as u8, 0, 0, 0, 0, 0, 0, 0]
    );
    let summer = at("America/St_Johns", 2026, 7, 1, 0, 0, 0);
    assert_eq!(payload(&summer)[4], (-10i8) as u8);
    assert_eq!(payload(&summer)[8], 1);
}

#[test]
fn the_last_day_of_a_leap_year_is_365() {
    let t = at("UTC", 2028, 12, 31, 23, 59, 59);
    assert_eq!(
        payload(&t),
        [0x07, 0xec, 0x01, 0x6d, 0, 23, 59, 59, 0, 0, 0, 0]
    );
}

#[test]
fn the_command_is_write_buffer_mode_1_at_0x0c0000_for_12_bytes() {
    assert_eq!(CDB[0], 0x3b);
    assert_eq!(CDB[1] & 0x1f, 1);
    assert_eq!(CDB[2], 0);
    assert_eq!(&CDB[3..6], &[0x0c, 0, 0]);
    assert_eq!(&CDB[6..9], &[0, 0, 12]);
}

// ---- which disk, on fake sysfs trees

/// A sysfs tree as the kernel lays it out, with `class/block/sdz` and
/// `class/block/sdz2` linking into `devices/.../usb1/1-3/1-3.4/...`.
fn sys(id_vendor: &str, model: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sys");
    let usb = root.join("devices/pci0000:00/0000:00:14.0/usb1/1-3/1-3.4");
    let scsi = usb.join("1-3.4:1.0/host8/target8:0:0/8:0:0:0");
    let disk = scsi.join("block/sdz");
    let part = disk.join("sdz2");
    fs::create_dir_all(&part).unwrap();
    fs::write(part.join("partition"), "2\n").unwrap();
    fs::write(usb.join("idVendor"), format!("{id_vendor}\n")).unwrap();
    fs::write(scsi.join("vendor"), "Apple   \n").unwrap();
    fs::write(scsi.join("model"), format!("{model}\n")).unwrap();
    symlink("../../../8:0:0:0", disk.join("device")).unwrap();
    fs::create_dir_all(root.join("class/block")).unwrap();
    symlink(&part, root.join("class/block/sdz2")).unwrap();
    symlink(&disk, root.join("class/block/sdz")).unwrap();
    (dir, root)
}

#[test]
fn a_partition_or_the_disk_of_an_ipod_gives_the_disk() {
    let (_d, root) = sys("05ac", "iPod            ");
    assert_eq!(ipod_disk(&root, "/dev/sdz2"), Ok(PathBuf::from("/dev/sdz")));
    assert_eq!(ipod_disk(&root, "/dev/sdz"), Ok(PathBuf::from("/dev/sdz")));
}

#[test]
fn another_vendor_or_model_is_refused() {
    let (_d, root) = sys("0781", "iPod");
    assert!(ipod_disk(&root, "/dev/sdz2").is_err());
    let (_d, root) = sys("05ac", "SuperDrive");
    assert!(ipod_disk(&root, "/dev/sdz2").is_err());
}

#[test]
fn names_outside_dev_or_with_other_characters_are_refused() {
    let (_d, root) = sys("05ac", "iPod");
    for dev in [
        "sdz2",
        "/dev/",
        "/dev/../sdz2",
        "/dev/mapper/sdz2",
        "/tmp/sdz2",
        "/dev/sdz2 ",
    ] {
        assert!(ipod_disk(&root, dev).is_err(), "{dev}");
    }
}

#[test]
fn a_device_missing_from_sysfs_is_refused() {
    let (_d, root) = sys("05ac", "iPod");
    assert!(ipod_disk(&root, "/dev/sdz9").is_err());
}
