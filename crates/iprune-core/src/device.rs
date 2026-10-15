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
    /// The folder of the filesystem mounted here: `/` for a whole
    /// filesystem, another path for a bind mount of a subfolder.
    pub root: PathBuf,
}

/// A mountinfo or fstab path with the kernel's octal escapes (`\040` for a
/// space) decoded.
pub fn unescape_mount(s: &str) -> String {
    unescape(s)
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

/// One mountinfo line: the mount and its source (`/dev/sdc2`).
fn parse_line(line: &str) -> Option<(Mount, String)> {
    let (left, right) = line.split_once(" - ")?;
    let left: Vec<&str> = left.split(' ').collect();
    let root = unescape(left.get(3)?);
    let mountpoint = unescape(left.get(4)?);
    let options = left.get(5)?;
    let mut right = right.split(' ');
    let fstype = right.next()?.to_owned();
    let source = unescape(right.next().unwrap_or(""));
    Some((
        Mount {
            mountpoint: PathBuf::from(mountpoint),
            fstype,
            read_only: options.split(',').any(|o| o == "ro"),
            root: PathBuf::from(root),
        },
        source,
    ))
}

/// Parses `/proc/self/mountinfo`. Malformed lines are skipped.
pub fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|l| Some(parse_line(l)?.0))
        .collect()
}

/// The block device mounted exactly at `mountpoint`, from mountinfo
/// `text`: the last such entry, as for [`check_writable`]. `None` when
/// `mountpoint` is not itself a mount of a `/dev/` device.
pub fn block_device(text: &str, mountpoint: &Path) -> Option<String> {
    let path = std::fs::canonicalize(mountpoint).unwrap_or_else(|_| mountpoint.to_owned());
    text.lines()
        .filter_map(parse_line)
        .rfind(|(m, _)| m.mountpoint == path)
        .map(|(_, source)| source)
        .filter(|s| s.starts_with("/dev/"))
}

/// Unmounts the iPod at `mountpoint` and powers it off, through
/// `udisksctl`, as the file manager would. Returns a warning when it was
/// unmounted but could not be powered off: it can be unplugged then too,
/// since unmounting writes everything out.
pub fn eject(mountpoint: &Path) -> Result<Option<String>> {
    let path = Path::new("/proc/self/mountinfo");
    let text = std::fs::read_to_string(path).at(path)?;
    let Some(dev) = block_device(&text, mountpoint) else {
        return Err(Error::Eject(format!(
            "{} is not a mounted device",
            mountpoint.display()
        )));
    };
    eject_device(&dev, &mut udisksctl)
}

/// The two steps of [`eject`] on `dev`, with `run` standing for `udisksctl`
/// (a seam for tests).
fn eject_device(
    dev: &str,
    run: &mut dyn FnMut(&[&str]) -> std::result::Result<(), String>,
) -> Result<Option<String>> {
    run(&["unmount", "--block-device", dev])
        .map_err(|e| Error::Eject(format!("could not unmount {dev}: {e}")))?;
    Ok(run(&["power-off", "--block-device", dev])
        .err()
        .map(|e| format!("unmounted, but not powered off ({e}); it can be unplugged")))
}

/// Runs `udisksctl` with `args`, without asking anything; its error output
/// on failure.
fn udisksctl(args: &[&str]) -> std::result::Result<(), String> {
    run_tool("udisksctl", args)
}

/// [`udisksctl`] with the program named, so tests can use a fake one.
fn run_tool(program: &str, args: &[&str]) -> std::result::Result<(), String> {
    let out = std::process::Command::new(program)
        .args(args)
        .arg("--no-user-interaction")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("udisksctl: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    Err(if err.is_empty() {
        format!("udisksctl {}: {}", args[0], out.status)
    } else {
        err
    })
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
pub fn check_writable(mountpoint: &Path, allow_folder: bool) -> Result<()> {
    let path = Path::new("/proc/self/mountinfo");
    let text = std::fs::read_to_string(path).at(path)?;
    if !allow_folder {
        check_device(&text, mountpoint)?;
    }
    check_mount(&parse_mountinfo(&text), mountpoint)
}

/// Refuses a `mountpoint` that is not the root of a mounted FAT32 device:
/// a folder (a copy of an iPod, a backup) must never be written as if it
/// were one. `device.allow_folder` lifts this for tests.
fn check_device(mountinfo: &str, mountpoint: &Path) -> Result<()> {
    let path = std::fs::canonicalize(mountpoint).unwrap_or_else(|_| mountpoint.to_owned());
    match mountinfo
        .lines()
        .filter_map(parse_line)
        .rfind(|(m, _)| m.mountpoint == path)
    {
        Some((m, source))
            if m.fstype == "vfat" && source.starts_with("/dev/") && m.root == Path::new("/") =>
        {
            Ok(())
        }
        Some((m, _)) if m.root != Path::new("/") => Err(Error::NotWritable(format!(
            "{} is a folder of a device mounted elsewhere, not the iPod itself",
            mountpoint.display()
        ))),
        Some((m, source)) => Err(Error::NotWritable(format!(
            "{} is {} from {source}, and iprune writes only to FAT32 (vfat) iPods",
            mountpoint.display(),
            m.fstype
        ))),
        None => Err(Error::NotWritable(format!(
            "{} is a folder, not a mounted iPod; iprune writes only to the iPod itself",
            mountpoint.display()
        ))),
    }
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

/// The hashing scheme in an `iTunesDB` header (0 when unsigned, 1 for the
/// hash the iPod Classic checks) and whether its hash is set. A file too
/// short to hold them reads as unsigned.
pub fn signature(itunesdb: &Path) -> Result<(u16, bool)> {
    use std::io::Read as _;
    let mut head = Vec::with_capacity(0x6c);
    std::fs::File::open(itunesdb)
        .and_then(|f| f.take(0x6c).read_to_end(&mut head))
        .at(itunesdb)?;
    if head.len() < 0x6c {
        return Ok((0, false));
    }
    let scheme = u16::from_le_bytes([head[0x30], head[0x31]]);
    Ok((scheme, head[0x58..0x6c].iter().any(|b| *b != 0)))
}

/// What is wrong with a database written with signature `now` over one
/// signed as `was` (see [`signature`]): it must keep the scheme of a signed
/// one and carry a hash.
pub fn signature_lost(was: (u16, bool), now: (u16, bool)) -> Option<String> {
    if was.0 == 0 || (now.0 == was.0 && now.1) {
        return None;
    }
    Some(format!(
        "the database was signed (scheme {}) and was written {}",
        was.0,
        match now {
            (0, _) => "unsigned".to_owned(),
            (s, false) if s == was.0 => "without its hash".to_owned(),
            (s, _) => format!("with scheme {s}"),
        }
    ))
}

/// Why libgpod would write the iPod at `mountpoint` unsigned, if it would.
/// It signs by `DBVersion` in `SysInfoExtended` (3 or more), or, with no
/// such key, by the model it recognises from `SysInfo` (`model`, as
/// [`crate::ipod::read`] gives it).
pub fn unsigned_reason(mountpoint: &Path, model: Option<&str>) -> Option<String> {
    let ext = mountpoint.join("iPod_Control/Device/SysInfoExtended");
    let version = std::fs::read_to_string(ext).ok().and_then(|t| {
        let after = &t[t.find("<key>DBVersion</key>")? + 20..];
        let start = after.find("<integer>")? + 9;
        let end = after[start..].find("</integer>")? + start;
        after[start..end].trim().parse::<u32>().ok()
    });
    match (version, model) {
        (Some(v), _) if v >= 3 => None,
        (Some(v), _) => Some(format!("SysInfoExtended gives DBVersion {v}")),
        (None, Some(_)) => None,
        (None, None) => Some(
            "it has no SysInfoExtended with a DBVersion and libgpod does not recognise its model"
                .into(),
        ),
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

/// The iPod to use: `device.mountpoint` from the config when set, else the
/// only mounted iPod.
pub fn mountpoint(cfg: &crate::config::Config) -> Result<std::path::PathBuf> {
    if !cfg.device.mountpoint.as_os_str().is_empty() {
        return Ok(cfg.device.mountpoint.clone());
    }
    pick(&find_ipods()?)
}

/// The one iPod among the mounted ones.
fn pick(found: &[Mount]) -> Result<std::path::PathBuf> {
    match found {
        [] => Err(Error::NoIpod),
        [one] => Ok(one.mountpoint.clone()),
        many => Err(Error::SeveralIpods(
            many.iter()
                .map(|m| m.mountpoint.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        )),
    }
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

    const INFO: &str = "\
22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw
95 22 8:33 / /run/media/julien/iPod\\040de\\040Julien rw,nosuid shared:50 - vfat /dev/sdc1 rw,uid=1000
96 22 0:50 / /run/media/julien/tmpfs rw - tmpfs tmpfs rw
97 22 0:51 / /run/media/julien/fuse rw - fuse.x /dev rw
";

    #[test]
    fn parse_line_returns_the_source_with_escapes_decoded() {
        let (m, src) =
            parse_line("95 22 8:33 / /mnt/a\\040b ro,nosuid - vfat /dev/disk/by-label/A\\040B rw")
                .unwrap();
        assert_eq!(m.mountpoint, Path::new("/mnt/a b"));
        assert!(m.read_only);
        assert_eq!(src, "/dev/disk/by-label/A B");
        assert!(parse_line("garbage").is_none());
    }

    #[test]
    fn block_device_decodes_octal_escapes_in_the_mountpoint() {
        assert_eq!(
            block_device(INFO, Path::new("/run/media/julien/iPod de Julien")).as_deref(),
            Some("/dev/sdc1")
        );
    }

    #[test]
    fn block_device_is_none_for_a_folder_that_is_not_a_mount() {
        // A folder inside a mount, and the parent of one.
        assert_eq!(
            block_device(
                INFO,
                Path::new("/run/media/julien/iPod de Julien/iPod_Control")
            ),
            None
        );
        assert_eq!(block_device(INFO, Path::new("/run/media/julien")), None);
    }

    #[test]
    fn block_device_ignores_a_sibling_that_only_shares_a_prefix() {
        let text = "95 22 8:33 / /mnt/ipod2 rw - vfat /dev/sdc1 rw\n";
        assert_eq!(block_device(text, Path::new("/mnt/ipod")), None);
        assert_eq!(
            block_device(text, Path::new("/mnt/ipod2")).as_deref(),
            Some("/dev/sdc1")
        );
    }

    #[test]
    fn block_device_is_none_when_the_source_is_not_under_dev() {
        assert_eq!(
            block_device(INFO, Path::new("/run/media/julien/tmpfs")),
            None
        );
        // "/dev" itself is not a device node either.
        assert_eq!(
            block_device(INFO, Path::new("/run/media/julien/fuse")),
            None
        );
    }

    #[test]
    fn block_device_takes_the_last_entry_for_a_mountpoint() {
        let text = "\
1 0 8:1 / /mnt/x rw - vfat /dev/sdb1 rw
2 0 8:2 / /mnt/x rw - vfat /dev/sdc1 rw
";
        assert_eq!(
            block_device(text, Path::new("/mnt/x")).as_deref(),
            Some("/dev/sdc1")
        );
        // A later non-device entry hides the earlier device.
        let text = format!("{text}3 0 0:5 / /mnt/x rw - tmpfs tmpfs rw\n");
        assert_eq!(block_device(&text, Path::new("/mnt/x")), None);
    }

    #[test]
    fn block_device_follows_symlinks_to_the_mountpoint() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let text = format!("1 0 8:1 / {} rw - vfat /dev/sdb1 rw\n", real.display());
        assert_eq!(block_device(&text, &link).as_deref(), Some("/dev/sdb1"));
    }

    #[test]
    fn eject_refuses_a_folder_that_is_not_a_mount() {
        let dir = tempfile::tempdir().unwrap();
        let err = eject(dir.path()).unwrap_err();
        match err {
            Error::Eject(m) => assert!(m.ends_with("is not a mounted device"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    type Calls = Vec<Vec<String>>;

    fn recorded(
        fail: &'static [&'static str],
    ) -> (Calls, impl FnMut(&[&str]) -> Result<(), String>) {
        (Vec::new(), move |args: &[&str]| {
            if fail.contains(&args[0]) {
                Err(format!("{} failed", args[0]))
            } else {
                Ok(())
            }
        })
    }

    fn run_recording(fail: &'static [&'static str]) -> (Calls, Result<Option<String>>) {
        let (_, mut inner) = recorded(fail);
        let mut calls: Calls = Vec::new();
        let r = eject_device("/dev/sdc1", &mut |a| {
            calls.push(a.iter().map(|s| s.to_string()).collect());
            inner(a)
        });
        (calls, r)
    }

    #[test]
    fn eject_unmounts_then_powers_off_the_same_device() {
        let (calls, r) = run_recording(&[]);
        assert_eq!(r.unwrap(), None);
        assert_eq!(
            calls,
            vec![
                vec!["unmount", "--block-device", "/dev/sdc1"],
                vec!["power-off", "--block-device", "/dev/sdc1"],
            ]
        );
    }

    #[test]
    fn eject_does_not_power_off_when_unmounting_fails() {
        let (calls, r) = run_recording(&["unmount"]);
        match r.unwrap_err() {
            Error::Eject(m) => assert!(m.contains("could not unmount /dev/sdc1"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0][0], "unmount");
    }

    #[test]
    fn eject_warns_when_only_power_off_fails() {
        let (calls, r) = run_recording(&["power-off"]);
        let warning = r.unwrap().expect("a warning");
        assert!(warning.contains("unmounted"), "{warning}");
        assert!(warning.contains("power-off failed"), "{warning}");
        assert_eq!(calls.len(), 2);
    }

    /// A script that appends its arguments to `log` and exits with `code`,
    /// writing `err` to stderr.
    fn fake_tool(dir: &Path, code: i32, err: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let (tool, log) = (dir.join("fake-udisksctl"), dir.join("log"));
        std::fs::write(
            &tool,
            format!(
                "#!/bin/sh\necho \"$@\" >> '{}'\necho '{err}' >&2\nexit {code}\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        (tool, log)
    }

    use crate::SPAWN;

    #[test]
    fn run_tool_adds_no_user_interaction_last() {
        let _turn = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let (tool, log) = fake_tool(dir.path(), 0, "");
        run_tool(
            tool.to_str().unwrap(),
            &["unmount", "--block-device", "/dev/sdc1"],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(log).unwrap().trim(),
            "unmount --block-device /dev/sdc1 --no-user-interaction"
        );
    }

    #[test]
    fn run_tool_failure_carries_stderr_or_the_status() {
        let _turn = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = fake_tool(dir.path(), 1, "Device is busy");
        assert_eq!(
            run_tool(tool.to_str().unwrap(), &["unmount"]).unwrap_err(),
            "Device is busy"
        );
        let (tool, _) = fake_tool(dir.path(), 3, "");
        // The fake echoes an empty line to stderr, which trims to nothing.
        let e = run_tool(tool.to_str().unwrap(), &["unmount"]).unwrap_err();
        assert!(
            e.starts_with("udisksctl unmount:") && e.contains('3'),
            "{e}"
        );
        assert!(
            run_tool("/nonexistent/udisksctl", &["x"])
                .unwrap_err()
                .starts_with("udisksctl:")
        );
    }

    #[test]
    fn mountpoint_uses_the_configured_path_without_looking_for_ipods() {
        let mut cfg = crate::config::Config::default();
        cfg.device.mountpoint = PathBuf::from("/nonexistent/chosen");
        assert_eq!(mountpoint(&cfg).unwrap(), Path::new("/nonexistent/chosen"));
    }

    #[test]
    fn pick_takes_the_only_ipod() {
        let one = [mount("/run/media/x/IPOD", "vfat", false)];
        assert_eq!(pick(&one).unwrap(), Path::new("/run/media/x/IPOD"));
    }

    #[test]
    fn pick_refuses_none_and_several() {
        let e = pick(&[]).unwrap_err();
        assert!(matches!(e, Error::NoIpod), "{e}");
        assert!(e.to_string().contains("no mounted iPod found"), "{e}");
        let two = [
            mount("/run/media/x/A", "vfat", false),
            mount("/run/media/x/B", "vfat", false),
        ];
        let e = pick(&two).unwrap_err();
        assert!(
            matches!(e, Error::SeveralIpods(ref l) if l == "/run/media/x/A, /run/media/x/B"),
            "{e}"
        );
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
            root: PathBuf::from("/"),
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
    // ---- check_device (audit M6): only the root of a vfat /dev mount

    const IPOD_ROOT: &str = "/run/media/julien/IPOD";

    fn device_info() -> String {
        format!(
            "\
22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw
95 22 8:33 / {IPOD_ROOT} rw,nosuid shared:50 - vfat /dev/sdc1 rw
96 22 8:34 / /mnt/ext rw - ext4 /dev/sdd1 rw
97 22 0:50 / /mnt/ram rw - tmpfs tmpfs rw
98 22 0:51 / /mnt/fuse rw - vfat /devices-are-not-dev rw
"
        )
    }

    fn refused(path: &str) -> String {
        match check_device(&device_info(), Path::new(path)) {
            Err(Error::NotWritable(m)) => m,
            other => panic!("{path}: expected NotWritable, got {other:?}"),
        }
    }

    #[test]
    fn check_device_accepts_the_root_of_a_vfat_device_mount() {
        check_device(&device_info(), Path::new(IPOD_ROOT)).unwrap();
    }

    #[test]
    fn check_device_refuses_a_folder_inside_the_mount_and_the_folder_above() {
        assert!(
            refused("/run/media/julien/IPOD/iPod_Control").contains("not a mounted iPod"),
            "inside the mount"
        );
        refused("/run/media/julien");
        // A sibling sharing only a name prefix is not the mount either.
        refused("/run/media/julien/IPOD2");
    }

    #[test]
    fn check_device_refuses_a_mount_that_is_not_vfat_or_not_a_dev_node() {
        assert!(refused("/mnt/ext").contains("ext4"));
        assert!(refused("/mnt/ram").contains("tmpfs"));
        // vfat, but the source is not a node under /dev/ (a loop file
        // through fuse, a bind of something else).
        refused("/mnt/fuse");
    }

    #[test]
    fn check_device_takes_the_last_entry_for_the_mountpoint() {
        let over = format!(
            "{}99 22 0:60 / {IPOD_ROOT} rw - tmpfs tmpfs rw\n",
            device_info()
        );
        assert!(check_device(&over, Path::new(IPOD_ROOT)).is_err());
        let back = format!("{over}100 22 8:35 / {IPOD_ROOT} rw - vfat /dev/sde1 rw\n");
        check_device(&back, Path::new(IPOD_ROOT)).unwrap();
    }

    #[test]
    fn check_device_refuses_everything_without_mounts() {
        assert!(check_device("", Path::new(IPOD_ROOT)).is_err());
    }

    #[test]
    fn check_writable_refuses_a_plain_folder_unless_folders_are_allowed() {
        let dir = tempfile::tempdir().unwrap();
        match check_writable(dir.path(), false) {
            Err(Error::NotWritable(_)) => {}
            other => panic!("expected NotWritable, got {other:?}"),
        }
        check_writable(dir.path(), true).unwrap();
    }

    // ---- signature (audit M12)

    /// An iTunesDB header of `len` bytes: `scheme` at 0x30, `hash` at 0x58.
    fn header(len: usize, scheme: u16, hash: &[(usize, u8)]) -> tempfile::NamedTempFile {
        let mut b = vec![0u8; len];
        if len >= 4 {
            b[..4].copy_from_slice(b"mhbd");
        }
        if len > 0x32 {
            b[0x30..0x32].copy_from_slice(&scheme.to_le_bytes());
        }
        for (at, v) in hash {
            b[*at] = *v;
        }
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), b).unwrap();
        f
    }

    #[test]
    fn signature_of_a_file_too_short_for_the_hash_is_unsigned() {
        // Everything set, but one byte short of the end of the hash field.
        let f = header(0x6b, 1, &[(0x58, 9), (0x6a, 9)]);
        assert_eq!(signature(f.path()).unwrap(), (0, false));
        let f = header(0, 0, &[]);
        assert_eq!(signature(f.path()).unwrap(), (0, false));
    }

    #[test]
    fn signature_reads_the_scheme_little_endian_and_whether_the_hash_is_set() {
        let f = header(0x6c, 1, &[]);
        assert_eq!(signature(f.path()).unwrap(), (1, false), "scheme, no hash");
        let f = header(0x6c, 1, &[(0x58, 1)]);
        assert_eq!(signature(f.path()).unwrap(), (1, true), "first hash byte");
        let f = header(0x6c, 1, &[(0x6b, 1)]);
        assert_eq!(signature(f.path()).unwrap(), (1, true), "last hash byte");
        let f = header(0x6c, 0x0102, &[(0x60, 7)]);
        assert_eq!(signature(f.path()).unwrap(), (0x0102, true));
        let f = header(0x6c, 0, &[(0x60, 7)]);
        assert_eq!(
            signature(f.path()).unwrap(),
            (0, true),
            "hash but no scheme"
        );
    }

    #[test]
    fn signature_ignores_bytes_next_to_the_hash_field() {
        // 0x57 is before the field and 0x6c after it.
        let f = header(0x80, 1, &[(0x57, 5), (0x6c, 5), (0x7f, 5)]);
        assert_eq!(signature(f.path()).unwrap(), (1, false));
    }

    #[test]
    fn signature_of_a_missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(signature(&dir.path().join("iTunesDB")).is_err());
    }

    // ---- unsigned_reason (audit M12)

    fn ipod_with_ext(ext: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("iPod_Control/Device");
        std::fs::create_dir_all(&device).unwrap();
        if let Some(text) = ext {
            std::fs::write(device.join("SysInfoExtended"), text).unwrap();
        }
        dir
    }

    fn plist(body: &str) -> String {
        format!("<plist><dict>{body}</dict></plist>")
    }

    fn db_version(v: &str) -> String {
        plist(&format!(
            "<key>FireWireGUID</key><string>000A27001234ABCD</string>\n<key>DBVersion</key>\n<integer>{v}</integer>"
        ))
    }

    #[test]
    fn unsigned_reason_follows_the_db_version_when_there_is_one() {
        for (v, signs) in [
            ("2", false),
            ("0", false),
            ("3", true),
            ("4", true),
            (" 3 ", true),
        ] {
            let dir = ipod_with_ext(Some(&db_version(v)));
            // The model does not matter once the version is known.
            for model in [None, Some("MC297")] {
                let r = unsigned_reason(dir.path(), model);
                assert_eq!(
                    r.is_none(),
                    signs,
                    "DBVersion {v:?}, model {model:?}: {r:?}"
                );
            }
        }
        let dir = ipod_with_ext(Some(&db_version("2")));
        let why = unsigned_reason(dir.path(), Some("MC297")).unwrap();
        assert!(why.contains("DBVersion 2"), "{why}");
    }

    #[test]
    fn unsigned_reason_without_a_version_depends_on_the_model_libgpod_knows() {
        // No SysInfoExtended at all.
        let dir = ipod_with_ext(None);
        assert_eq!(unsigned_reason(dir.path(), Some("MC297")), None);
        assert!(unsigned_reason(dir.path(), None).is_some());
        // One with other keys, or an unreadable version, is no version.
        for text in [
            plist("<key>FireWireGUID</key><string>x</string>"),
            plist("<key>DBVersion</key><integer>three</integer>"),
            plist("<key>DBVersion</key>"),
            String::new(),
        ] {
            let dir = ipod_with_ext(Some(&text));
            assert_eq!(unsigned_reason(dir.path(), Some("MC297")), None, "{text}");
            assert!(unsigned_reason(dir.path(), None).is_some(), "{text}");
        }
    }
    // ---- bind mounts (check_device) and signature_lost

    #[test]
    fn check_device_accepts_the_line_a_real_ipod_gives_and_refuses_a_bind_mount() {
        let real = "95 22 8:33 / /run/media/julien/IPOD\\040DE\\040JUL rw,nosuid,nodev shared:50 - vfat /dev/sdc1 rw,uid=1000\n";
        check_device(real, Path::new("/run/media/julien/IPOD DE JUL")).unwrap();
        // A subfolder of the device mounted elsewhere: same fstype and source.
        let bind = "96 22 8:33 /iPod_Copy /mnt/copy rw - vfat /dev/sdc1 rw\n";
        match check_device(bind, Path::new("/mnt/copy")) {
            Err(Error::NotWritable(m)) => assert!(m.contains("folder"), "{m}"),
            other => panic!("expected NotWritable, got {other:?}"),
        }
        // The bind mount mounted last hides the real root at the same place.
        let both = format!(
            "{real}97 22 8:33 /sub /run/media/julien/IPOD\\040DE\\040JUL rw - vfat /dev/sdc1 rw\n"
        );
        assert!(check_device(&both, Path::new("/run/media/julien/IPOD DE JUL")).is_err());
    }

    #[test]
    fn signature_lost_has_a_verdict_for_every_pair() {
        type Sig = (u16, bool);
        let cases: [(Sig, Sig, Option<&str>); 9] = [
            ((0, false), (0, false), None),
            ((0, false), (1, true), None),
            ((0, true), (0, false), None),
            ((1, true), (1, true), None),
            ((1, true), (0, false), Some("unsigned")),
            ((1, true), (0, true), Some("unsigned")),
            ((1, true), (1, false), Some("without its hash")),
            ((1, true), (2, true), Some("with scheme 2")),
            ((1, false), (2, false), Some("with scheme 2")),
        ];
        for (was, now, want) in cases {
            let got = signature_lost(was, now);
            match want {
                None => assert_eq!(got, None, "{was:?} -> {now:?}"),
                Some(frag) => {
                    let m = got.unwrap_or_else(|| panic!("{was:?} -> {now:?}: no verdict"));
                    assert!(m.ends_with(frag), "{was:?} -> {now:?}: {m}");
                    assert!(m.contains(&format!("scheme {}", was.0)), "{m}");
                }
            }
        }
    }
}
