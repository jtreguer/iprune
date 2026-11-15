//! `iprune-clock /dev/sdX`: sets the clock of the iPod on that disk (or the
//! disk holding that partition) to the computer's local time, and prints
//! the time set. Runs as root (`pkexec`); see the library for the command.

use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

use iprune_clock::{CDB, LocalTime, ipod_disk, payload};

/// From sg3_utils, which Arch's libgpod depends on.
const SG_RAW: &str = "/usr/bin/sg_raw";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [dev] = args.as_slice() else {
        eprintln!("usage: iprune-clock /dev/sdX");
        return ExitCode::from(2);
    };
    match set(dev) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("iprune-clock: {e}");
            ExitCode::FAILURE
        }
    }
}

fn set(dev: &str) -> Result<String, String> {
    let disk = ipod_disk(Path::new("/sys"), dev)?;
    // The system time zone: pkexec does not pass TZ on.
    let now = jiff::ZonedDateTime::now();
    let data = payload(&LocalTime::of(&now));
    let mut child = Command::new(SG_RAW)
        .args(["--send=12", "--timeout=10"])
        .arg(&disk)
        .args(CDB.iter().map(|b| format!("{b:02x}")))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("running {SG_RAW}: {e}"))?;
    // Dropped after the write, so sg_raw sees the end of its input.
    let written = child.stdin.take().map(|mut i| i.write_all(&data));
    let out = child
        .wait_with_output()
        .map_err(|e| format!("running {SG_RAW}: {e}"))?;
    if !out.status.success() {
        let said = [&out.stderr[..], &out.stdout[..]]
            .iter()
            .map(|b| String::from_utf8_lossy(b).trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!(
            "sg_raw on {} failed ({}): {said}",
            disk.display(),
            out.status
        ));
    }
    if let Some(Err(e)) = written {
        return Err(format!("sending the time to sg_raw: {e}"));
    }
    Ok(now.strftime("%Y-%m-%d %H:%M:%S %:z").to_string())
}
