//! Keeps the machine from suspending, shutting down or acting on the lid
//! while the iPod is written. A logind block inhibitor, taken through
//! `systemd-inhibit` (the desktop's own inhibit, through its portal, does
//! nothing on Hyprland). The inhibitor lives as long as a `cat` that
//! `systemd-inhibit` runs and that reads our end of a pipe: it ends when the
//! [`Inhibitor`] is dropped, or when iprune dies.

use std::process::{Child, Command, Stdio};

/// What is inhibited: suspend and hibernate, shutdown and reboot, idle, and
/// the lid switch.
const WHAT: &str = "sleep:shutdown:idle:handle-lid-switch";

/// Held while it lives.
pub struct Inhibitor {
    child: Child,
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        // Ended at once: closing the pipe would wait for `cat`, which a
        // `systemd-inhibit` still stuck on logind never started.
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Takes the inhibitor, saying `why`. `None` when it cannot be taken (no
/// systemd-inhibit, or it ended at once: no logind, refused): the write goes
/// ahead unprotected, and the caller says so.
pub fn hold(why: &str) -> Option<Inhibitor> {
    hold_with("systemd-inhibit", why)
}

fn hold_with(program: &str, why: &str) -> Option<Inhibitor> {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new(program)
        .args([
            &format!("--what={WHAT}"),
            "--mode=block",
            "--who=iprune",
            &format!("--why={why}"),
            "cat",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Its own process group: the Ctrl+C or hang-up a terminal sends to
        // iprune's group must not end it, since the write it covers goes on.
        .process_group(0)
        .spawn()
        .ok()?;
    // Given a moment to take the lock, or to fail at it.
    std::thread::sleep(std::time::Duration::from_millis(100));
    match child.try_wait() {
        Ok(None) => Some(Inhibitor { child }),
        _ => {
            let _ = child.wait();
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SPAWN;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// A stand-in for `systemd-inhibit`: records its options (everything
    /// before the command), then its pid, and runs the command it is given.
    fn fake_program(dir: &Path) -> std::path::PathBuf {
        let tool = dir.join("fake-inhibit");
        std::fs::write(
            &tool,
            format!(
                "#!/bin/sh\nwhile [ \"${{1#--}}\" != \"$1\" ]; do printf '%s\\n' \"$1\" >> '{log}'; shift; done\n\
                 echo \"$@\" > '{cmd}'\necho $$ > '{pid}.tmp'\nmv '{pid}.tmp' '{pid}'\nexec \"$@\"\n",
                log = dir.join("opts").display(),
                cmd = dir.join("cmd").display(),
                pid = dir.join("pid").display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        tool
    }

    fn wait_for_pid(dir: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(s) = std::fs::read_to_string(dir.join("pid")) {
                return s.trim().parse().unwrap();
            }
            assert!(Instant::now() < deadline, "the program never started");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // Prevents (audit M7) a write with no inhibitor, or one that lets the
    // machine sleep, shut down, idle or close the lid, or that outlives us.
    #[test]
    fn hold_with_takes_a_blocking_inhibitor_that_ends_when_dropped() {
        let _turn = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let tool = fake_program(dir.path());
        let held = hold_with(tool.to_str().unwrap(), "Writing to the iPod").expect("spawned");
        let pid = wait_for_pid(dir.path());
        let proc = std::path::PathBuf::from(format!("/proc/{pid}"));

        let opts = std::fs::read_to_string(dir.path().join("opts")).unwrap();
        assert_eq!(
            opts.lines().collect::<Vec<_>>(),
            [
                "--what=sleep:shutdown:idle:handle-lid-switch",
                "--mode=block",
                "--who=iprune",
                "--why=Writing to the iPod",
            ]
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cmd"))
                .unwrap()
                .trim(),
            "cat",
            "it holds by running cat on our pipe"
        );
        // Held: the process lives for as long as the Inhibitor does.
        std::thread::sleep(Duration::from_millis(200));
        assert!(proc.exists(), "the inhibitor ended while held");

        drop(held);
        // Drop waits for the child, so it is gone (and reaped) at once.
        assert!(!proc.exists(), "the inhibitor outlived its Inhibitor");
    }

    #[test]
    fn hold_with_a_missing_program_gives_none() {
        // It still forks.
        let _turn = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        assert!(hold_with("/nonexistent/systemd-inhibit", "x").is_none());
    }

    fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let tool = dir.join(name);
        std::fs::write(&tool, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        tool
    }

    // A systemd-inhibit that fails at once (no logind, refused) holds nothing:
    // None, so the caller can say so, and the child is reaped.
    #[test]
    fn hold_with_a_program_that_exits_at_once_gives_none() {
        let _turn = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        for body in ["exit 1", "exit 0"] {
            let tool = script(dir.path(), "quits", body);
            assert!(hold_with(tool.to_str().unwrap(), "x").is_none(), "{body}");
        }
    }

    // Prevents a drop hanging for ever on a systemd-inhibit that is stuck (on
    // logind) and ignores the closed pipe: it is killed.
    #[test]
    fn drop_kills_a_child_that_ignores_the_end_of_its_input() {
        let _turn = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let tool = script(
            dir.path(),
            "stuck",
            &format!(
                "echo $$ > '{0}.tmp'; mv '{0}.tmp' '{0}'; exec sleep 30",
                dir.path().join("pid").display()
            ),
        );
        let held = hold_with(tool.to_str().unwrap(), "x").expect("alive after 100 ms");
        let pid = wait_for_pid(dir.path());
        let proc = std::path::PathBuf::from(format!("/proc/{pid}"));
        assert!(proc.exists());
        let start = Instant::now();
        drop(held);
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "drop waited for it"
        );
        assert!(!proc.exists(), "the stuck child was left running");
    }

    // Prevents the inhibitor sharing iprune's process group, where the
    // terminal's Ctrl+C or hang-up would end it mid-write.
    #[test]
    fn the_child_is_in_its_own_process_group() {
        let _turn = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let tool = fake_program(dir.path());
        let held = hold_with(tool.to_str().unwrap(), "x").expect("spawned");
        let pid = wait_for_pid(dir.path());
        let pgid = |pid: u32| -> u32 {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            stat.rsplit(')')
                .next()
                .unwrap()
                .split_whitespace()
                .nth(2)
                .unwrap()
                .parse()
                .unwrap()
        };
        assert_eq!(pgid(pid), pid, "its own group, led by itself");
        assert_ne!(pgid(pid), pgid(std::process::id()));
        drop(held);
    }
}
