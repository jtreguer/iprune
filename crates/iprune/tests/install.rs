//! `packaging/install.sh` against a temp install root and data home, with a
//! fake `cargo` first on the PATH that logs its arguments: nothing is built
//! and nothing outside the temp dir is written.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Writing a script and running it while another test forks can fail with
/// "Text file busy"; these tests take turns.
static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Env {
    dir: tempfile::TempDir,
    /// The install root's folder name in the temp dir.
    root: &'static str,
    _turn: std::sync::MutexGuard<'static, ()>,
}

impl Env {
    fn new() -> Env {
        Env::with_root("root")
    }

    fn with_root(root: &'static str) -> Env {
        let turn = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        fs::create_dir(base.join("bin")).unwrap();
        let tool = base.join("bin/cargo");
        fs::write(
            &tool,
            format!(
                "#!/bin/sh\necho \"$@\" >> '{}'\n",
                base.join("cargo.log").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        Env {
            dir,
            root,
            _turn: turn,
        }
    }

    fn base(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn root(&self) -> PathBuf {
        self.base().join(self.root)
    }

    fn entry(&self) -> PathBuf {
        self.base().join("share/applications/iprune.desktop")
    }

    fn run(&self, args: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.base().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new("sh")
            .arg(repo().join("packaging/install.sh"))
            .args(args)
            .env("PATH", path)
            .env("HOME", self.base().join("home"))
            .env("CARGO_INSTALL_ROOT", self.root())
            .env("XDG_DATA_HOME", self.base().join("share"))
            .output()
            .unwrap()
    }

    fn cargo_calls(&self) -> Vec<String> {
        fs::read_to_string(self.base().join("cargo.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

#[test]
fn install_names_the_installed_binary_in_full_in_the_entry() {
    let env = Env::new();
    let o = env.run(&[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let entry = fs::read_to_string(env.entry()).unwrap();
    let exec: Vec<&str> = entry.lines().filter(|l| l.starts_with("Exec=")).collect();
    assert_eq!(
        exec,
        [format!("Exec=\"{}/bin/iprune-gui\"", env.root().display())]
    );
    // Everything else is the packaged entry, and no temp file is left.
    let packaged = fs::read_to_string(repo().join("packaging/iprune.desktop")).unwrap();
    let rest = |s: &str| -> Vec<String> {
        s.lines()
            .filter(|l| !l.starts_with("Exec="))
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(rest(&entry), rest(&packaged));
    assert!(!env.entry().with_extension("desktop.tmp").exists());
    // Both crates installed into the chosen root.
    let calls = env.cargo_calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    for (call, krate) in calls.iter().zip(["crates/iprune", "crates/iprune-gui"]) {
        assert!(call.starts_with("install "), "{call}");
        assert!(
            call.contains(&format!("--root {}", env.root().display())),
            "{call}"
        );
        assert!(call.ends_with(krate), "{call}");
    }
}

#[test]
fn uninstall_removes_the_entry_and_only_the_entry_it_owns() {
    let env = Env::new();
    env.run(&[]);
    let other = env.entry().with_file_name("other.desktop");
    fs::write(&other, "[Desktop Entry]\n").unwrap();
    let o = env.run(&["--uninstall"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!env.entry().exists());
    assert!(other.exists(), "another application's entry is kept");
    let calls = env.cargo_calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(
        calls[2].starts_with("uninstall --root") && calls[2].ends_with("iprune iprune-gui"),
        "{}",
        calls[2]
    );
    // Uninstalling again, with nothing there, still succeeds.
    assert!(env.run(&["--uninstall"]).status.success());
}

#[test]
fn the_installed_entry_passes_desktop_file_validate() {
    let env = Env::new();
    env.run(&[]);
    // The validator is not on every machine.
    let Ok(o) = Command::new("desktop-file-validate")
        .arg(env.entry())
        .output()
    else {
        return;
    };
    assert!(
        o.status.success(),
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
}

#[test]
fn install_quotes_an_install_root_with_spaces_and_special_characters() {
    let env = Env::with_root(r#"odd root|&"1" $x `y` %z \w"#);
    let o = env.run(&[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let entry = fs::read_to_string(env.entry()).unwrap();
    let exec: Vec<&str> = entry.lines().filter(|l| l.starts_with("Exec=")).collect();
    // Inside the quotes " ` $ and \ get a backslash; then every backslash
    // is doubled for the key file, and % is doubled for the field codes.
    assert_eq!(
        exec,
        [format!(
            r#"Exec="{}/odd root|&\\"1\\" \\$x \\`y\\` %%z \\\\w/bin/iprune-gui""#,
            env.base().display()
        )]
    );
    let v = Command::new("desktop-file-validate")
        .arg(env.entry())
        .output()
        .unwrap();
    assert!(v.status.success(), "{}", String::from_utf8_lossy(&v.stdout));
}

#[test]
fn a_relative_install_root_gives_an_absolute_exec() {
    let env = Env::new();
    let path = format!(
        "{}:{}",
        env.base().join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let o = Command::new("sh")
        .arg(repo().join("packaging/install.sh"))
        .current_dir(env.base())
        .env("PATH", path)
        .env("HOME", env.base().join("home"))
        .env("CARGO_INSTALL_ROOT", "root")
        .env("XDG_DATA_HOME", env.base().join("share"))
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let entry = fs::read_to_string(env.entry()).unwrap();
    assert!(
        entry.contains(&format!("Exec=\"{}/bin/iprune-gui\"", env.root().display())),
        "{entry}"
    );
    assert!(
        env.cargo_calls()
            .iter()
            .all(|c| c.contains(&format!("--root {}", env.root().display()))),
        "{:?}",
        env.cargo_calls()
    );
}
