//! Finding mounted iPods.

use std::path::{Path, PathBuf};

use crate::Result;
use crate::error::IoContext;

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
}
