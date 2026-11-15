//! Sets an iPod's clock to the computer's, as iTunes did when the iPod was
//! connected: a SCSI WRITE BUFFER in the vendor-specific mode, at buffer
//! offset 0x0C0000, of 12 bytes (libgpod's `tools/ipod-scsi.c`,
//! `sync_time`). Linux lets only root send that command, so this is a
//! separate, small program, installed root-owned, that `iprune` runs through
//! `pkexec` at each sync. It changes nothing but the clock, and only on an
//! Apple iPod.

use std::path::{Path, PathBuf};

/// WRITE BUFFER(10): mode 1 (vendor specific), buffer 0, offset 0x0C0000,
/// 12 bytes, as libgpod's `sg_ll_write_buffer(fd, 1, 0, 0x0c0000, buf, 12)`.
pub const CDB: [u8; 10] = [0x3b, 0x01, 0x00, 0x0c, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x00];

/// A local time, as `localtime(3)` gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i16,
    /// 1 for 1 January.
    pub day_of_year: i16,
    pub hour: i8,
    pub minute: i8,
    pub second: i8,
    /// Seconds east of UTC, daylight saving time included (`tm_gmtoff`).
    pub offset: i32,
    /// Daylight saving time is in effect (`tm_isdst`).
    pub dst: bool,
}

impl LocalTime {
    pub fn of(t: &jiff::ZonedDateTime) -> LocalTime {
        let info = t.time_zone().to_offset_info(t.timestamp());
        LocalTime {
            year: t.year(),
            day_of_year: t.day_of_year(),
            hour: t.hour(),
            minute: t.minute(),
            second: t.second(),
            offset: info.offset().seconds(),
            dst: info.dst().is_dst(),
        }
    }
}

/// The 12 bytes sent, laid out as libgpod's `struct iPodTime`: the year and
/// the day of the year counted from 0, both big-endian, the offset from UTC
/// in quarter hours as a signed byte (UTC+2 is 8), hour, minute, second, 1
/// when daylight saving time is in effect, and three zero bytes.
pub fn payload(t: &LocalTime) -> [u8; 12] {
    let mut p = [0u8; 12];
    p[0..2].copy_from_slice(&(t.year as u16).to_be_bytes());
    p[2..4].copy_from_slice(&((t.day_of_year - 1) as u16).to_be_bytes());
    p[4] = (t.offset / 900) as i8 as u8;
    p[5] = t.hour as u8;
    p[6] = t.minute as u8;
    p[7] = t.second as u8;
    p[8] = u8::from(t.dst);
    p
}

/// The whole disk holding `dev` (`/dev/sdc2`, or the disk itself), as
/// `/dev/sdc`, when it is an Apple iPod according to the sysfs tree at
/// `sys`: a USB device of vendor 05ac above it, and a SCSI model starting
/// with "iPod". Anything else is refused.
pub fn ipod_disk(sys: &Path, dev: &str) -> Result<PathBuf, String> {
    let name = dev
        .strip_prefix("/dev/")
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_alphanumeric()))
        .ok_or_else(|| format!("{dev} is not a disk name such as /dev/sdc2"))?;
    let not_ipod = || format!("{dev} is not an iPod");
    let sys = std::fs::canonicalize(sys).map_err(|e| format!("{}: {e}", sys.display()))?;
    let mut block =
        std::fs::canonicalize(sys.join("class/block").join(name)).map_err(|_| not_ipod())?;
    if block.join("partition").is_file() {
        block.pop();
    }
    let disk = block
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| n.bytes().all(|b| b.is_ascii_alphanumeric()))
        .ok_or_else(not_ipod)?
        .to_owned();
    let read = |p: PathBuf| std::fs::read_to_string(p).unwrap_or_default();
    let model = read(block.join("device/model"));
    let usb = block
        .ancestors()
        .take_while(|p| p.starts_with(&sys))
        .find(|p| p.join("idVendor").is_file())
        .ok_or_else(not_ipod)?;
    if read(usb.join("idVendor")).trim() != "05ac" || !model.trim().starts_with("iPod") {
        return Err(not_ipod());
    }
    Ok(Path::new("/dev").join(disk))
}

#[cfg(test)]
mod tests;
