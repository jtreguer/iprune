//! Finding mounted iPods.

use std::path::{Path, PathBuf};

use crate::error::IoContext;
use crate::{Error, Result};

/// A mounted filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub mountpoint: PathBuf,
    pub fstype: String,
    pub read_only: bool,
}

/// Undoes the octal escapes the kernel uses in mountinfo (`\040` for space).
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // Computed wide: a corrupt `\777` must not overflow a byte.
        let value = (i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)))
            .then(|| {
                b[i + 1..i + 4]
                    .iter()
                    .fold(0u32, |v, c| v * 8 + u32::from(c - b'0'))
            })
            .filter(|&v| v <= 255);
        if let (b'\\', Some(v)) = (b[i], value) {
            out.push(v as u8);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses `/proc/self/mountinfo`. Malformed lines are skipped.
pub fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let left: Vec<&str> = left.split(' ').collect();
            let mountpoint = unescape(left.get(4)?);
            let options = left.get(5)?;
            let fstype = right.split(' ').next()?.to_owned();
            Some(Mount {
                mountpoint: PathBuf::from(mountpoint),
                fstype,
                read_only: options.split(',').any(|o| o == "ro"),
            })
        })
        .collect()
}

/// The database file of an iPod mounted at `mountpoint`.
pub fn itunesdb(mountpoint: &Path) -> PathBuf {
    mountpoint.join("iPod_Control/iTunes/iTunesDB")
}

/// Refuses to write to `mountpoint` when the filesystem holding it is
/// mounted read-only, or when it is itself a mounted filesystem other than
/// vfat (FAT32, the only format supported; a real iPod is always mounted at
/// its root). A folder inside another filesystem (a copy, a test) passes
/// the format check.
pub fn check_writable(mountpoint: &Path) -> Result<()> {
    let path = Path::new("/proc/self/mountinfo");
    let text = std::fs::read_to_string(path).at(path)?;
    check_mount(&parse_mountinfo(&text), mountpoint)
}

fn check_mount(mounts: &[Mount], mountpoint: &Path) -> Result<()> {
    // Symlinks and `..` resolved first. The filesystem holding the path is
    // the longest mountpoint it is under, the last such entry winning.
    let path = std::fs::canonicalize(mountpoint).unwrap_or_else(|_| mountpoint.to_owned());
    let holder = mounts
        .iter()
        .enumerate()
        .filter(|(_, m)| path.starts_with(&m.mountpoint))
        .max_by_key(|(i, m)| (m.mountpoint.components().count(), *i))
        .map(|(_, m)| m);
    match holder {
        Some(m) if m.read_only => Err(Error::NotWritable(format!(
            "{} is on a read-only mount",
            mountpoint.display()
        ))),
        Some(m) if m.mountpoint == path && m.fstype != "vfat" => Err(Error::NotWritable(format!(
            "{} is {}, and iprune writes only to FAT32 (vfat) iPods",
            mountpoint.display(),
            m.fstype
        ))),
        _ => Ok(()),
    }
}

/// A FireWire GUID as upper-case hex without a `0x` prefix, so GUIDs from
/// libgpod, SysInfo and SysInfoExtended compare equal.
pub fn normalize_guid(guid: &str) -> String {
    let g = guid.trim();
    g.strip_prefix("0x")
        .or_else(|| g.strip_prefix("0X"))
        .unwrap_or(g)
        .to_ascii_uppercase()
}

/// The FireWire GUID of the iPod at `mountpoint`, read from
/// `SysInfoExtended` or `SysInfo` without parsing the database, so it works
/// when the database is damaged. Upper-case hex without `0x`.
pub fn firewire_guid(mountpoint: &Path) -> Option<String> {
    let dir = mountpoint.join("iPod_Control/Device");
    let clean = |s: &str| {
        let s = s.trim();
        let s = s
            .strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .unwrap_or(s);
        (!s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit())).then(|| s.to_ascii_uppercase())
    };
    if let Ok(text) = std::fs::read_to_string(dir.join("SysInfoExtended")) {
        let after_key = text.split("<key>FireWireGUID</key>").nth(1);
        let value = after_key
            .and_then(|r| r.split("<string>").nth(1))
            .and_then(|r| r.split("</string>").next());
        if let Some(g) = value.and_then(clean) {
            return Some(g);
        }
    }
    let text = std::fs::read_to_string(dir.join("SysInfo")).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("FirewireGuid:"))
        .and_then(clean)
}

/// Mounted filesystems that hold an iPod database.
pub fn find_ipods() -> Result<Vec<Mount>> {
    let path = Path::new("/proc/self/mountinfo");
    let text = std::fs::read_to_string(path).at(path)?;
    Ok(parse_mountinfo(&text)
        .into_iter()
        .filter(|m| itunesdb(&m.mountpoint).is_file())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mountinfo_lines() {
        let text = "\
22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw
95 22 8:33 / /run/media/julien/iPod\\040de\\040Julien rw,nosuid,nodev,relatime shared:50 - vfat /dev/sdc1 rw,uid=1000
96 22 8:34 / /mnt/ro ro,relatime - ext4 /dev/sdd1 ro
garbage line
";
        let m = parse_mountinfo(text);
        assert_eq!(m.len(), 3);
        assert_eq!(
            m[1].mountpoint,
            Path::new("/run/media/julien/iPod de Julien")
        );
        assert_eq!((m[1].fstype.as_str(), m[1].read_only), ("vfat", false));
        assert!(m[2].read_only);
    }

    #[test]
    fn unescape_handles_edge_cases() {
        assert_eq!(unescape("a\\134b"), "a\\b");
        assert_eq!(unescape("tail\\04"), "tail\\04");
        assert_eq!(unescape("x\\0"), "x\\0");
    }

    fn mount(path: &str, fstype: &str, read_only: bool) -> Mount {
        Mount {
            mountpoint: PathBuf::from(path),
            fstype: fstype.into(),
            read_only,
        }
    }

    #[test]
    fn check_mount_accepts_a_writable_vfat_mount() {
        let mounts = [
            mount("/", "ext4", false),
            mount("/run/media/x/IPOD", "vfat", false),
        ];
        assert!(check_mount(&mounts, Path::new("/run/media/x/IPOD")).is_ok());
    }

    #[test]
    fn check_mount_refuses_a_read_only_mount() {
        let mounts = [mount("/run/media/x/IPOD", "vfat", true)];
        let e = check_mount(&mounts, Path::new("/run/media/x/IPOD")).unwrap_err();
        assert!(
            matches!(e, Error::NotWritable(ref m) if m.contains("read-only")),
            "{e}"
        );
    }

    #[test]
    fn check_mount_refuses_a_filesystem_that_is_not_vfat() {
        for fstype in ["ext4", "hfsplus", "exfat"] {
            let mounts = [mount("/run/media/x/IPOD", fstype, false)];
            let e = check_mount(&mounts, Path::new("/run/media/x/IPOD")).unwrap_err();
            assert!(
                matches!(e, Error::NotWritable(ref m) if m.contains(fstype)),
                "{fstype}: {e}"
            );
        }
    }

    #[test]
    fn check_mount_passes_a_path_that_is_under_no_mount() {
        let mounts = [
            mount("/mnt/other", "ext4", true),
            mount("/mnt/third", "vfat", false),
        ];
        assert!(check_mount(&mounts, Path::new("/tmp/fake-ipod")).is_ok());
        assert!(check_mount(&[], Path::new("/tmp/fake-ipod")).is_ok());
    }

    #[test]
    fn check_mount_refuses_anything_under_a_read_only_mount() {
        let mounts = [
            mount("/", "ext4", false),
            mount("/run/media/x/IPOD", "vfat", false),
            mount("/run/media/x/IPOD/ro", "vfat", true),
            mount("/mnt/rodisk", "ext4", true),
        ];
        let refused = |p: &str| matches!(check_mount(&mounts, Path::new(p)), Err(Error::NotWritable(ref m)) if m.contains("read-only"));
        assert!(refused("/run/media/x/IPOD/ro"));
        assert!(refused("/run/media/x/IPOD/ro/sub/deeper"));
        assert!(refused("/mnt/rodisk/copy"));
        // The writable vfat mount around the ro one is still fine.
        assert!(check_mount(&mounts, Path::new("/run/media/x/IPOD")).is_ok());
        assert!(check_mount(&mounts, Path::new("/run/media/x/IPOD/sub")).is_ok());
    }

    #[test]
    fn check_mount_refuses_the_root_of_a_non_vfat_mount_but_not_a_folder_inside_it() {
        let mounts = [
            mount("/", "ext4", false),
            mount("/mnt/disk", "ext4", false),
            mount("/tmp", "tmpfs", false),
        ];
        for root in ["/mnt/disk", "/tmp"] {
            let e = check_mount(&mounts, Path::new(root)).unwrap_err();
            assert!(
                matches!(e, Error::NotWritable(ref m) if m.contains("vfat")),
                "{root}: {e}"
            );
        }
        for inside in ["/mnt/disk/copy", "/tmp/fake-ipod", "/home/x/ipod-copy"] {
            assert!(check_mount(&mounts, Path::new(inside)).is_ok(), "{inside}");
        }
        // A sibling that merely shares a name prefix is not inside the mount.
        assert!(check_mount(&mounts, Path::new("/mnt/disk2")).is_ok());
    }

    #[test]
    fn check_mount_uses_the_last_entry_for_a_path() {
        // Something mounted over it later wins.
        let p = "/run/media/x/IPOD";
        let ro_then_rw = [mount(p, "vfat", true), mount(p, "vfat", false)];
        assert!(check_mount(&ro_then_rw, Path::new(p)).is_ok());
        let rw_then_ro = [mount(p, "vfat", false), mount(p, "vfat", true)];
        assert!(check_mount(&rw_then_ro, Path::new(p)).is_err());
    }
}
