//! `device::firewire_guid`: read from SysInfoExtended or SysInfo, no database.

use std::fs;
use std::path::Path;

use iprune_core::device::{firewire_guid, firewire_guid_or};

fn ipod(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let dev = dir.path().join("iPod_Control/Device");
    fs::create_dir_all(&dev).unwrap();
    for (n, c) in files {
        fs::write(dev.join(n), c).unwrap();
    }
    dir
}

fn plist(guid: &str) -> String {
    format!(
        "<plist><dict><key>Other</key><string>zzz</string><key>FireWireGUID</key>\n<string>{guid}</string></dict></plist>"
    )
}

#[test]
fn sysinfo_gives_upper_case_hex_without_the_prefix() {
    let d = ipod(&[(
        "SysInfo",
        "ModelNumStr: xA623\nFirewireGuid: 0x000a27001234abcd\n",
    )]);
    assert_eq!(firewire_guid(d.path()).as_deref(), Some("000A27001234ABCD"));
    let d = ipod(&[("SysInfo", "FirewireGuid: 000A27001234ABCD\n")]);
    assert_eq!(firewire_guid(d.path()).as_deref(), Some("000A27001234ABCD"));
}

#[test]
fn sysinfo_extended_wins_over_sysinfo() {
    let d = ipod(&[
        ("SysInfoExtended", &plist("000A270000000001")),
        ("SysInfo", "FirewireGuid: 0x000A270000000002\n"),
    ]);
    assert_eq!(firewire_guid(d.path()).as_deref(), Some("000A270000000001"));
}

#[test]
fn an_unusable_extended_value_falls_back_to_sysinfo() {
    let d = ipod(&[
        ("SysInfoExtended", &plist("not hex")),
        ("SysInfo", "FirewireGuid: 0x000A270000000002\n"),
    ]);
    assert_eq!(firewire_guid(d.path()).as_deref(), Some("000A270000000002"));
}

#[test]
fn no_guid_is_none() {
    assert_eq!(firewire_guid(ipod(&[]).path()), None);
    assert_eq!(
        firewire_guid(ipod(&[("SysInfo", "FirewireGuid:\n")]).path()),
        None
    );
    assert_eq!(
        firewire_guid(ipod(&[("SysInfo", "FirewireGuid: 0xZZ\n")]).path()),
        None
    );
    assert_eq!(
        firewire_guid(ipod(&[("SysInfo", "Other: 0x12\n")]).path()),
        None
    );
    assert_eq!(firewire_guid(Path::new("/nonexistent/ipod")), None);
}

// ---- the USB serial, on fake sysfs trees

mod usb_serial {
    use std::os::unix::fs::symlink;

    use super::*;
    use iprune_core::device::usb_guid_in;

    /// A sysfs tree as the kernel lays it out: `class/block/<name>` are
    /// symlinks into `devices/.../usb1/1-3/1-3.4/1-3.4:1.0/host8/.../block`.
    /// `usb_files` are written in the USB device directory `1-3.4`.
    struct Sys {
        dir: tempfile::TempDir,
    }

    impl Sys {
        fn root(&self) -> std::path::PathBuf {
            self.dir.path().join("sys")
        }
    }

    fn sys(usb_files: &[(&str, &str)]) -> Sys {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sys");
        let usb = root.join("devices/pci0000:00/0000:00:14.0/usb1/1-3/1-3.4");
        let disk = usb.join("1-3.4:1.0/host8/target8:0:0/8:0:0:0/block/sdz");
        let part = disk.join("sdz2");
        fs::create_dir_all(&part).unwrap();
        for (n, c) in usb_files {
            fs::write(usb.join(n), c).unwrap();
        }
        fs::create_dir_all(root.join("class/block")).unwrap();
        symlink(&part, root.join("class/block/sdz2")).unwrap();
        symlink(&disk, root.join("class/block/sdz")).unwrap();
        Sys { dir }
    }

    const APPLE: &[(&str, &str)] = &[
        ("idVendor", "05ac\n"),
        ("idProduct", "1209\n"),
        ("serial", "000a270015bb7c98\n"),
    ];

    #[test]
    fn an_apple_serial_gives_the_upper_case_guid_for_a_partition_and_a_disk() {
        let s = sys(APPLE);
        assert_eq!(
            usb_guid_in(&s.root(), "/dev/sdz2").as_deref(),
            Some("000A270015BB7C98")
        );
        assert_eq!(
            usb_guid_in(&s.root(), "/dev/sdz").as_deref(),
            Some("000A270015BB7C98")
        );
    }

    #[test]
    fn a_device_missing_from_class_block_gives_none() {
        let s = sys(APPLE);
        assert_eq!(usb_guid_in(&s.root(), "/dev/sdz9"), None);
        assert_eq!(
            usb_guid_in(&s.dir.path().join("nowhere"), "/dev/sdz2"),
            None
        );
    }

    #[test]
    fn a_vendor_other_than_apple_gives_none() {
        let s = sys(&[("idVendor", "0781\n"), ("serial", "000A270015BB7C98\n")]);
        assert_eq!(usb_guid_in(&s.root(), "/dev/sdz2"), None);
    }

    #[test]
    fn a_missing_empty_or_non_hex_serial_gives_none() {
        let no_serial = sys(&[("idVendor", "05ac\n")]);
        assert_eq!(usb_guid_in(&no_serial.root(), "/dev/sdz2"), None);
        for serial in [
            "",
            "\n",
            "  \n",
            "000A27ZZ15BB7C98\n",
            "0x000A270015BB7C98\n",
        ] {
            let s = sys(&[("idVendor", "05ac\n"), ("serial", serial)]);
            assert_eq!(
                usb_guid_in(&s.root(), "/dev/sdz2"),
                None,
                "serial {serial:?}"
            );
        }
    }

    #[test]
    fn it_never_climbs_above_the_sys_root() {
        // No USB device under the root; an Apple one above it.
        let s = sys(&[]);
        fs::write(s.dir.path().join("idVendor"), "05ac\n").unwrap();
        fs::write(s.dir.path().join("serial"), "000A270015BB7C98\n").unwrap();
        assert_eq!(usb_guid_in(&s.root(), "/dev/sdz2"), None);
    }

    #[test]
    fn a_vendor_file_on_the_way_up_stops_the_walk_at_the_first_one() {
        // The nearest idVendor belongs to a non-Apple device; an Apple one
        // further up must not be taken instead.
        let s = sys(&[("idVendor", "0781\n"), ("serial", "AAAAAAAAAAAAAAAA\n")]);
        let hub = s.root().join("devices/pci0000:00/0000:00:14.0/usb1/1-3");
        fs::write(hub.join("idVendor"), "05ac\n").unwrap();
        fs::write(hub.join("serial"), "000A270015BB7C98\n").unwrap();
        assert_eq!(usb_guid_in(&s.root(), "/dev/sdz2"), None);
    }

    #[test]
    fn a_folder_that_is_not_a_mount_with_no_sysinfo_has_no_guid() {
        let d = ipod(&[]);
        assert_eq!(firewire_guid(d.path()), None);
    }

    #[test]
    fn device_files_still_win_over_the_serial_lookup() {
        let serial = |_: &Path| Some("000A270000000009".to_owned());
        let d = ipod(&[
            ("SysInfoExtended", &plist("000A270000000001")),
            ("SysInfo", "FirewireGuid: 0x000A270000000002\n"),
        ]);
        assert_eq!(
            firewire_guid_or(d.path(), serial).as_deref(),
            Some("000A270000000001")
        );
        let d = ipod(&[("SysInfo", "FirewireGuid: 0x000A270000000002\n")]);
        assert_eq!(
            firewire_guid_or(d.path(), serial).as_deref(),
            Some("000A270000000002")
        );
        // A restored iPod: an empty SysInfo, no SysInfoExtended.
        let d = ipod(&[("SysInfo", "")]);
        assert_eq!(
            firewire_guid_or(d.path(), serial).as_deref(),
            Some("000A270000000009")
        );
    }
}
