//! `clock::set_with` with fake `pkexec` scripts in a temp dir: the real
//! `pkexec`, the real helper and `sg_raw` are never run, and the mountinfo
//! text is written by hand. The mountpoint is a real temp folder, since the
//! code canonicalizes it.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use iprune_core::clock::{self, set_with};

/// Writing a script and running it while another test forks can fail with
/// "Text file busy"; these tests take turns.
static SPAWN: Mutex<()> = Mutex::new(());

struct Rig {
    dir: tempfile::TempDir,
    _turn: std::sync::MutexGuard<'static, ()>,
}

impl Rig {
    fn new() -> Rig {
        let turn = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("mp")).unwrap();
        let rig = Rig { dir, _turn: turn };
        fs::write(rig.helper(), "not run: only its path is passed\n").unwrap();
        rig
    }

    fn base(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn mountpoint(&self) -> PathBuf {
        self.base().join("mp")
    }

    fn helper(&self) -> PathBuf {
        self.base().join("iprune-clock")
    }

    fn pkexec(&self) -> PathBuf {
        self.base().join("pkexec")
    }

    fn log(&self) -> PathBuf {
        self.base().join("pkexec.log")
    }

    /// A pkexec that logs its arguments, one per line, then runs `body`.
    fn fake_pkexec(&self, body: &str) {
        let script = format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{}'\n{body}\n",
            self.log().display()
        );
        fs::write(self.pkexec(), script).unwrap();
        fs::set_permissions(self.pkexec(), fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Mountinfo with the mountpoint mounted from `source`.
    fn mounts(&self, source: &str) -> String {
        format!(
            "22 1 8:1 / / rw - ext4 /dev/sda1 rw\n\
             95 22 8:34 / {} rw,nosuid - vfat {source} rw\n",
            self.mountpoint().display()
        )
    }

    fn set(&self, mountinfo: &str) -> Result<Option<String>, String> {
        set_with(
            mountinfo,
            &self.mountpoint(),
            &self.pkexec(),
            &self.helper(),
        )
    }

    fn args(&self) -> Option<Vec<String>> {
        fs::read_to_string(self.log())
            .ok()
            .map(|t| t.lines().map(str::to_owned).collect())
    }
}

// Prevents running the wrong program, a password prompt in the terminal
// (--disable-internal-agent), the wrong helper, or the wrong device (the
// partition the mount names is what the helper resolves to its disk).
#[test]
fn pkexec_gets_no_agent_the_helper_and_the_mounted_device() {
    let rig = Rig::new();
    rig.fake_pkexec("echo '2026-11-15 23:41:07'");
    let got = rig.set(&rig.mounts("/dev/sdc2")).unwrap();
    assert_eq!(got.as_deref(), Some("2026-11-15 23:41:07"));
    assert_eq!(
        rig.args().unwrap(),
        [
            "--disable-internal-agent".to_owned(),
            rig.helper().display().to_string(),
            "/dev/sdc2".to_owned()
        ]
    );
}

// Prevents pkexec reading the terminal's input (or a pipe of the caller's):
// its stdin is /dev/null.
#[test]
fn pkexec_is_run_with_stdin_on_dev_null() {
    let rig = Rig::new();
    rig.fake_pkexec("readlink /proc/$$/fd/0");
    let got = rig.set(&rig.mounts("/dev/sdc2")).unwrap();
    assert_eq!(got.as_deref(), Some("/dev/null"));
}

#[test]
fn the_time_is_the_trimmed_stdout() {
    let rig = Rig::new();
    rig.fake_pkexec("printf '\\n  Sun 15 Nov 2026 23:41:07 CET \\n\\n'; echo ignored >&2");
    assert_eq!(
        rig.set(&rig.mounts("/dev/sdc2")).unwrap().as_deref(),
        Some("Sun 15 Nov 2026 23:41:07 CET")
    );
}

// Prevents "set" being reported when the helper printed nothing.
#[test]
fn success_with_no_output_is_an_error() {
    let rig = Rig::new();
    for body in ["exit 0", "printf '  \\n'; echo warn >&2"] {
        rig.fake_pkexec(body);
        let e = rig.set(&rig.mounts("/dev/sdc2")).unwrap_err();
        assert!(e.contains("nothing"), "{body}: {e}");
    }
}

// Prevents a dismissed prompt (126) being taken for a refusal (127) or for
// success.
#[test]
fn exit_126_is_a_dismissed_prompt_whatever_stderr_says() {
    let rig = Rig::new();
    rig.fake_pkexec("echo 'Error executing command as another user' >&2; exit 126");
    let e = rig.set(&rig.mounts("/dev/sdc2")).unwrap_err();
    assert!(e.contains("dismissed"), "{e}");
    assert!(
        !e.contains("refused") && !e.contains("Error executing"),
        "{e}"
    );
}

#[test]
fn exit_127_is_a_refusal_that_carries_stderr() {
    let rig = Rig::new();
    rig.fake_pkexec("echo 'Not authorized: policy says no' >&2; echo '2026' ; exit 127");
    let e = rig.set(&rig.mounts("/dev/sdc2")).unwrap_err();
    assert!(e.contains("refused"), "{e}");
    assert!(e.contains("Not authorized: policy says no"), "{e}");
    assert!(!e.contains("dismissed"), "{e}");
}

// Prevents a failure of the helper itself (not an iPod, sg_raw failing)
// losing the reason it gave.
#[test]
fn another_failure_is_its_trimmed_stderr() {
    let rig = Rig::new();
    rig.fake_pkexec("printf '\\n/dev/sdc is not an iPod\\n\\n' >&2; echo '2026'; exit 1");
    assert_eq!(
        rig.set(&rig.mounts("/dev/sdc2")).unwrap_err(),
        "/dev/sdc is not an iPod"
    );
}

#[test]
fn a_failure_with_no_stderr_names_the_status_and_a_signal_death_is_one_too() {
    let rig = Rig::new();
    rig.fake_pkexec("exit 3");
    let e = rig.set(&rig.mounts("/dev/sdc2")).unwrap_err();
    assert!(e.contains("failed") && e.contains('3'), "{e}");
    rig.fake_pkexec("kill -9 $$");
    let e = rig.set(&rig.mounts("/dev/sdc2")).unwrap_err();
    assert!(e.contains("failed"), "{e}");
}

// Prevents trying to set the clock of a folder (a copy, a test fixture, a
// disk image): nothing runs, there is no error to warn about.
#[test]
fn a_folder_that_is_not_a_mount_is_ok_none_and_runs_nothing() {
    let rig = Rig::new();
    rig.fake_pkexec("echo 'should not run'");
    // Another mount only, a sub folder of a mount, and a mount that is not
    // a device.
    let others = "22 1 8:1 / / rw - ext4 /dev/sda1 rw\n";
    assert_eq!(rig.set(others), Ok(None));
    let nested = format!(
        "95 22 8:34 / {} rw - vfat /dev/sdc2 rw\n",
        rig.base().display()
    );
    assert_eq!(rig.set(&nested), Ok(None));
    assert_eq!(rig.set(&rig.mounts("tmpfs")), Ok(None));
    assert_eq!(rig.set(""), Ok(None));
    assert_eq!(rig.args(), None, "pkexec was run");
}

// Prevents an unhelpful error (or a run of pkexec on a missing path) when
// install.sh was not run.
#[test]
fn a_missing_helper_is_an_error_naming_the_installer_and_runs_nothing() {
    let rig = Rig::new();
    rig.fake_pkexec("echo '2026'");
    fs::remove_file(rig.helper()).unwrap();
    let e = rig.set(&rig.mounts("/dev/sdc2")).unwrap_err();
    assert!(e.contains("packaging/install.sh"), "{e}");
    assert!(e.contains(&rig.helper().display().to_string()), "{e}");
    assert_eq!(rig.args(), None, "pkexec was run");
    // A folder in its place is no helper either.
    fs::create_dir(rig.helper()).unwrap();
    assert!(rig.set(&rig.mounts("/dev/sdc2")).is_err());
    assert_eq!(rig.args(), None);
}

#[test]
fn a_pkexec_that_cannot_start_is_an_error() {
    let rig = Rig::new();
    let e = rig.set(&rig.mounts("/dev/sdc2")).unwrap_err();
    assert!(e.contains("pkexec"), "{e}");
}

// The installer and the polkit policy both name this path.
#[test]
fn the_helper_path_is_the_one_the_installer_and_the_policy_use() {
    assert_eq!(clock::HELPER, "/usr/local/lib/iprune/iprune-clock");
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let policy = fs::read_to_string(repo.join("packaging/dev.iprune.clock.policy")).unwrap();
    assert!(policy.contains(clock::HELPER), "{policy}");
    let install = fs::read_to_string(repo.join("packaging/install.sh")).unwrap();
    assert!(
        install.contains("helper_dir=/usr/local/lib/iprune"),
        "{install}"
    );
}

// Prevents a stuck polkit daemon (or a hung pkexec) freezing the end of a
// sync: the call gives up at the timeout, kills pkexec and reaps it.
#[test]
fn a_pkexec_that_does_not_answer_is_killed_at_the_timeout() {
    let rig = Rig::new();
    let pid_file = rig.base().join("pkexec.pid");
    rig.fake_pkexec(&format!(
        "echo $$ > '{}'\nexec sleep 30",
        pid_file.display()
    ));
    let start = std::time::Instant::now();
    let e = clock::set_within(
        &rig.mounts("/dev/sdc2"),
        &rig.mountpoint(),
        &rig.pkexec(),
        &rig.helper(),
        std::time::Duration::from_millis(300),
    )
    .unwrap_err();
    let took = start.elapsed();
    assert!(e.contains("did not answer") && e.contains("0.3 s"), "{e}");
    assert!(took.as_millis() >= 300, "gave up early: {took:?}");
    assert!(took.as_secs() < 5, "waited for the sleep: {took:?}");
    let pid = fs::read_to_string(&pid_file)
        .expect("the fake pkexec ran and wrote its pid")
        .trim()
        .to_owned();
    // Killed and reaped: not even a zombie is left.
    assert!(
        !Path::new("/proc").join(&pid).exists(),
        "process {pid} is still there"
    );
}

// The timeout does not cut a pkexec that answers in time.
#[test]
fn a_pkexec_that_answers_before_the_timeout_gives_its_answer() {
    let rig = Rig::new();
    rig.fake_pkexec("sleep 0.2; echo '2026-11-15 23:41:07'");
    let got = clock::set_within(
        &rig.mounts("/dev/sdc2"),
        &rig.mountpoint(),
        &rig.pkexec(),
        &rig.helper(),
        std::time::Duration::from_secs(10),
    );
    assert_eq!(got, Ok(Some("2026-11-15 23:41:07".to_owned())));
}
