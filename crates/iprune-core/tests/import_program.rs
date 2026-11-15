//! Which autotag program `import::autotag::program` picks (technical
//! document, section 7.2): `import.autotag`, else `autotag` next to the
//! running binary, else the first `autotag` file on `PATH`.
//!
//! It depends on the running binary and on `PATH`, which a test cannot change
//! for itself (the process is shared and `set_var` is unsafe). So each case
//! copies this test binary into a temp folder, with or without an `autotag`
//! beside it, and runs only `child_program` in it with its own `PATH`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use iprune_core::Error;
use iprune_core::config::Config;
use iprune_core::import::autotag::program;

/// Runs in the copy: checks what `program` gives against what the parent
/// expects. Without the parent's variables it does nothing.
#[test]
fn child_program() {
    let Ok(expect) = std::env::var("IPRUNE_PROGRAM_EXPECT") else {
        return;
    };
    let mut cfg = Config::default();
    cfg.import.autotag = std::env::var("IPRUNE_PROGRAM_CONFIG")
        .unwrap_or_default()
        .into();
    match (program(&cfg), expect.as_str()) {
        (Err(Error::Autotag(m)), "ERR") => {
            assert!(m.contains("import.autotag") && m.contains("PATH"), "{m}");
        }
        (Ok(p), want) if want != "ERR" => assert_eq!(p, Path::new(want)),
        (got, want) => panic!("expected {want}, got {got:?}"),
    }
}

struct Case {
    dir: tempfile::TempDir,
}

impl Case {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["bin", "p1", "p2", "empty"] {
            fs::create_dir(dir.path().join(d)).unwrap();
        }
        // A copy of this very binary, to run from `bin/`.
        fs::copy(
            std::env::current_exe().unwrap(),
            dir.path().join("bin/tests"),
        )
        .unwrap();
        Case { dir }
    }

    fn at(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn script(&self, rel: &str) -> PathBuf {
        let p = self.at(rel);
        fs::write(&p, "#!/bin/sh\n").unwrap();
        p
    }

    /// Runs `program` in the copy with `PATH` made of `path` (relative
    /// folders) and the config's `import.autotag`; it must give `expect`.
    fn check(&self, config: Option<&Path>, path: &[&str], expect: Result<&Path, ()>) {
        let path_var = std::env::join_paths(path.iter().map(|p| self.at(p))).unwrap();
        let mut cmd = Command::new(self.at("bin/tests"));
        cmd.args([
            "child_program",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("PATH", path_var)
        .env(
            "IPRUNE_PROGRAM_EXPECT",
            match expect {
                Ok(p) => p.to_str().unwrap(),
                Err(()) => "ERR",
            },
        )
        .env("IPRUNE_PROGRAM_CONFIG", config.unwrap_or(Path::new("")));
        // The copy was just written: a child another test forked meanwhile may
        // still hold it open, which makes exec fail with "Text file busy".
        let mut tries = 0;
        let out = loop {
            match cmd.output() {
                Err(e) if e.raw_os_error() == Some(26) && tries < 20 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                r => break r.unwrap(),
            }
        };
        let text = format!(
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "{text}");
        assert!(text.contains("1 passed"), "the child test ran: {text}");
    }
}

#[test]
fn the_config_wins_even_when_the_program_is_not_there() {
    let c = Case::new();
    c.script("bin/autotag");
    c.script("p1/autotag");
    let named = c.at("elsewhere/my-autotag");
    c.check(Some(&named), &["p1"], Ok(&named));
}

#[test]
fn next_to_the_running_binary_comes_before_path() {
    let c = Case::new();
    let beside = c.script("bin/autotag");
    c.script("p1/autotag");
    c.check(None, &["p1"], Ok(&beside));
}

#[test]
fn else_the_first_autotag_file_on_path_in_path_order() {
    let c = Case::new();
    let first = c.script("p1/autotag");
    let second = c.script("p2/autotag");
    c.check(None, &["empty", "p1", "p2"], Ok(&first));
    c.check(None, &["p2", "p1"], Ok(&second));
}

#[test]
fn a_folder_named_autotag_is_not_the_program() {
    let c = Case::new();
    fs::create_dir(c.at("p1/autotag")).unwrap();
    let real = c.script("p2/autotag");
    c.check(None, &["p1", "p2"], Ok(&real));
    // A folder beside the binary is skipped too.
    fs::create_dir(c.at("bin/autotag")).unwrap();
    c.check(None, &["p1", "p2"], Ok(&real));
}

#[test]
fn with_no_program_anywhere_the_error_says_what_to_set() {
    let c = Case::new();
    c.check(None, &["empty", "p1"], Err(()));
}
