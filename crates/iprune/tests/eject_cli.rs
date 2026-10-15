//! `iprune eject` against a fake iPod: a plain temp folder, which is not a
//! mount, so the command must refuse before it runs `udisksctl`. A fake
//! `udisksctl` first on PATH records any call; the real one is never reached.
//! Every run gets its own HOME and XDG folders and a closed stdin.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Sandbox {
    dir: tempfile::TempDir,
    ipod: PathBuf,
    bin: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let (music, lists, ipod, bin) = (
            base.join("music"),
            base.join("lists"),
            base.join("ipod"),
            base.join("bin"),
        );
        for d in [&music, &lists, &ipod, &bin] {
            fs::create_dir(d).unwrap();
        }
        let sb = Sandbox { dir, ipod, bin };
        let init = sb.run(&[
            "init",
            "--library",
            music.to_str().unwrap(),
            "--playlists",
            lists.to_str().unwrap(),
        ]);
        assert_eq!(init.status.code(), Some(0));
        let cfg = base.join("config/iprune/config.toml");
        let text = fs::read_to_string(&cfg).unwrap();
        let empty = "mountpoint = \"\"";
        assert!(text.contains(empty), "{text}");
        fs::write(
            cfg,
            text.replace(
                empty,
                &format!("mountpoint = {:?}\nallow_folder = true", sb.ipod),
            ),
        )
        .unwrap();
        sb
    }

    fn base(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// A `udisksctl` that appends its arguments to `<base>/udisksctl.log`.
    fn fake_udisksctl(&self) -> PathBuf {
        let log = self.base().join("udisksctl.log");
        let tool = self.bin.join("udisksctl");
        fs::write(
            &tool,
            format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 0\n", log.display()),
        )
        .unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        log
    }

    fn run(&self, args: &[&str]) -> Output {
        let base = self.base();
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(env!("CARGO_BIN_EXE_iprune"))
            .args(args)
            .env("HOME", base.join("home"))
            .env("XDG_CONFIG_HOME", base.join("config"))
            .env("XDG_DATA_HOME", base.join("data"))
            .env("PATH", path)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

#[test]
fn eject_refuses_a_folder_that_is_not_a_mount_and_never_calls_udisksctl() {
    let sb = Sandbox::new();
    let log = sb.fake_udisksctl();
    let o = sb.run(&["eject"]);
    let (out, err) = (
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr),
    );
    assert_eq!(o.status.code(), Some(1), "stdout: {out}\nstderr: {err}");
    assert!(
        err.contains(&format!("{} is not a mounted device", sb.ipod.display())),
        "{err}"
    );
    assert!(!out.contains("ejected"), "{out}");
    assert!(!log.exists(), "udisksctl was called: {:?}", fs::read(&log));
}

#[test]
fn eject_while_the_ipod_lock_is_held_is_busy_and_never_calls_udisksctl() {
    let sb = Sandbox::new();
    let log = sb.fake_udisksctl();
    // The CLI's data folder is $XDG_DATA_HOME/iprune.
    let paths = iprune_core::config::Paths {
        config_file: sb.base().join("config/iprune/config.toml"),
        data_dir: sb.base().join("data/iprune"),
    };
    let lock = iprune_core::lock::ipod(&paths).unwrap();
    let o = sb.run(&["eject"]);
    let (out, err) = (
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr),
    );
    assert_eq!(o.status.code(), Some(1), "stdout: {out}\nstderr: {err}");
    assert!(err.contains("another iprune is"), "{err}");
    assert!(!err.contains("not a mounted device"), "{err}");
    assert!(!log.exists(), "udisksctl was called: {:?}", fs::read(&log));

    // Without the lock the command goes on to the mount check.
    drop(lock);
    let o = sb.run(&["eject"]);
    assert!(String::from_utf8_lossy(&o.stderr).contains("not a mounted device"));
}
