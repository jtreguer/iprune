//! The iPod's clock, set to the computer's at each sync (iTunes set it each
//! time the iPod was connected). Linux lets only root send the SCSI command
//! that sets it, so `iprune-clock` (crate `iprune-clock`) sends it,
//! installed root-owned by `packaging/install.sh` and run through `pkexec`;
//! the polkit policy installed with it lets the active local session run
//! it without a password. A clock that could not be set is a warning: the
//! sync stands.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::device;

/// Where `packaging/install.sh` puts the helper. The polkit policy names
/// this path.
pub const HELPER: &str = "/usr/local/lib/iprune/iprune-clock";
const PKEXEC: &str = "/usr/bin/pkexec";

/// How long `pkexec` and the helper may take. Allowed or refused, pkexec
/// answers at once, and the helper gives `sg_raw` 10 s; only a stuck polkit
/// daemon would take longer, and the sync's end must not wait for it.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// Sets the clock of the iPod mounted at `mountpoint`. `Ok(Some(time))`
/// gives the time set, `Ok(None)` means `mountpoint` is not the root of a
/// mounted device (a folder: a copy, a test), so there is no clock, and
/// `Err` says why the clock could not be set.
pub fn set(mountpoint: &Path) -> Result<Option<String>, String> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("reading /proc/self/mountinfo: {e}"))?;
    set_with(&mountinfo, mountpoint, Path::new(PKEXEC), Path::new(HELPER))
}

/// [`set`] with the mounts as `mountinfo` gives them, `pkexec` and the
/// helper given.
pub fn set_with(
    mountinfo: &str,
    mountpoint: &Path,
    pkexec: &Path,
    helper: &Path,
) -> Result<Option<String>, String> {
    set_within(mountinfo, mountpoint, pkexec, helper, TIMEOUT)
}

/// [`set_with`], giving up on `pkexec` after `timeout`.
pub fn set_within(
    mountinfo: &str,
    mountpoint: &Path,
    pkexec: &Path,
    helper: &Path,
    timeout: Duration,
) -> Result<Option<String>, String> {
    let Some(dev) = device::block_device(mountinfo, mountpoint) else {
        return Ok(None);
    };
    if !helper.is_file() {
        return Err(format!(
            "{} is not installed (packaging/install.sh installs it)",
            helper.display()
        ));
    }
    // No password prompt in the terminal: the policy lets the active
    // session run the helper without one, and refuses any other.
    let mut child = Command::new(pkexec)
        .arg("--disable-internal-agent")
        .arg(helper)
        .arg(&dev)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("running {}: {e}", pkexec.display()))?;
    // Both say a line at most, well within a pipe's buffer, so neither
    // waits on a full pipe while this waits on it.
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                // pkexec is still ours to kill until it runs the helper as
                // root; past that point the kill fails and nothing waits.
                if child.kill().is_ok() {
                    let _ = child.wait();
                }
                return Err(format!(
                    "pkexec did not answer within {} s",
                    timeout.as_secs_f32()
                ));
            }
            Err(e) => return Err(format!("waiting for {}: {e}", pkexec.display())),
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("reading what {} said: {e}", pkexec.display()))?;
    let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_owned();
    let said = text(&out.stderr);
    match out.status.code() {
        Some(0) => match text(&out.stdout) {
            t if t.is_empty() => Err("the clock helper said nothing".into()),
            t => Ok(Some(t)),
        },
        Some(126) => Err("the password prompt was dismissed".into()),
        Some(127) => Err(format!("pkexec refused to run the clock helper: {said}")),
        _ if said.is_empty() => Err(format!("the clock helper failed ({})", out.status)),
        _ => Err(said),
    }
}
