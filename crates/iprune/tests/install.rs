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

// ---- autotag (M9 step 4) ----

use std::os::unix::fs::MetadataExt;

/// The tools the script uses, found on the real PATH.
const SCRIPT_TOOLS: [&str; 8] = ["sh", "cp", "chmod", "mv", "mkdir", "sed", "rm", "dirname"];

fn find_on_path(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("{name} is not on the PATH"))
}

impl Env {
    /// A PATH of links to the script's own tools and the fake cargo, and
    /// nothing else; with `uv`, a fake one too. printf is a shell builtin.
    fn limited_path(&self, with_uv: bool) -> String {
        let tools = self.base().join("tools");
        fs::create_dir_all(&tools).unwrap();
        for t in SCRIPT_TOOLS {
            let link = tools.join(t);
            if !link.exists() {
                std::os::unix::fs::symlink(find_on_path(t), link).unwrap();
            }
        }
        let cargo = tools.join("cargo");
        if !cargo.exists() {
            std::os::unix::fs::symlink(self.base().join("bin/cargo"), cargo).unwrap();
        }
        if with_uv {
            let uv = tools.join("uv");
            fs::write(&uv, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&uv, fs::Permissions::from_mode(0o755)).unwrap();
        }
        tools.display().to_string()
    }

    fn run_with(&self, path: &str, umask: Option<&str>, args: &[&str]) -> Output {
        let prefix = format!("umask {}; ", umask.unwrap_or("022"));
        Command::new(find_on_path("sh"))
            .arg("-c")
            .arg(format!("{prefix}exec sh \"$@\""))
            .arg("sh")
            .arg(repo().join("packaging/install.sh"))
            .args(args)
            .env("PATH", path)
            .env("HOME", self.base().join("home"))
            .env("CARGO_INSTALL_ROOT", self.root())
            .env("XDG_DATA_HOME", self.base().join("share"))
            .output()
            .unwrap()
    }

    fn autotag(&self) -> PathBuf {
        self.root().join("bin/autotag")
    }
}

fn ok(o: &Output) {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

// Prevents an install that forgets autotag, copies it altered, leaves it
// unexecutable (umask 077 makes cp alone give 0700), or leaves the temp file.
#[test]
fn install_copies_autotag_executable_and_leaves_no_temp_file() {
    let env = Env::new();
    let o = env.run_with(&env.limited_path(true), Some("077"), &[]);
    ok(&o);
    assert_eq!(
        fs::read(env.autotag()).unwrap(),
        fs::read(repo().join("tools/autotag")).unwrap()
    );
    assert_eq!(
        fs::metadata(env.autotag()).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(!env.root().join("bin/.autotag.tmp").exists());
    let names: Vec<_> = fs::read_dir(env.root().join("bin"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["autotag"], "nothing else in bin: {names:?}");
}

// Prevents writing into the installed file: a running autotag would see it
// change under it. The file must be replaced by rename.
#[test]
fn a_second_install_replaces_autotag_by_rename() {
    use std::io::Read;
    let env = Env::new();
    let path = env.limited_path(true);
    ok(&env.run_with(&path, None, &[]));
    // Stand in for an older autotag, written in place so the inode is kept.
    fs::write(env.autotag(), b"old autotag\n").unwrap();
    let before = fs::metadata(env.autotag()).unwrap().ino();
    let mut running = fs::File::open(env.autotag()).unwrap();

    ok(&env.run_with(&path, None, &[]));

    assert_ne!(fs::metadata(env.autotag()).unwrap().ino(), before);
    let mut seen = String::new();
    running.read_to_string(&mut seen).unwrap();
    assert_eq!(seen, "old autotag\n", "the open file kept its content");
    assert_eq!(
        fs::read(env.autotag()).unwrap(),
        fs::read(repo().join("tools/autotag")).unwrap()
    );
    assert!(!env.root().join("bin/.autotag.tmp").exists());
}

// Prevents an install that fails when cargo made no bin folder.
#[test]
fn install_creates_the_bin_folder_when_cargo_did_not() {
    let env = Env::new();
    assert!(!env.root().exists(), "the fake cargo creates nothing");
    let o = env.run_with(&env.limited_path(true), None, &[]);
    ok(&o);
    assert!(env.autotag().is_file());
}

// Prevents a silent install of a tool that cannot start without uv.
#[test]
fn a_note_on_stderr_says_when_uv_is_missing_and_only_then() {
    let env = Env::new();
    let without = env.limited_path(false);
    assert!(
        !Path::new(&without).join("uv").exists(),
        "no uv in the limited PATH"
    );
    let o = env.run_with(&without, None, &[]);
    ok(&o);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("uv"), "{err}");
    assert!(!String::from_utf8_lossy(&o.stdout).contains("uv"));
    // The note does not stop the install.
    assert!(env.autotag().is_file());
    drop(env); // the tests take turns

    let env = Env::new();
    let o = env.run_with(&env.limited_path(true), None, &[]);
    ok(&o);
    assert_eq!(String::from_utf8_lossy(&o.stderr), "");
}

// Prevents --uninstall leaving autotag behind, or reaching past it.
#[test]
fn uninstall_removes_autotag_and_leaves_the_music_link_and_other_files() {
    let env = Env::new();
    let path = env.limited_path(true);
    ok(&env.run_with(&path, None, &[]));
    let music = env.base().join("home/Music");
    fs::create_dir_all(&music).unwrap();
    let link = music.join("autotag");
    let target = env.base().join("repo-autotag");
    fs::write(&target, "#!/bin/sh\n").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let neighbour = env.root().join("bin/other-tool");
    fs::write(&neighbour, "x").unwrap();

    ok(&env.run_with(&path, None, &["--uninstall"]));

    assert!(!env.autotag().exists());
    assert!(env.autotag().symlink_metadata().is_err());
    assert!(!env.entry().exists());
    assert!(neighbour.exists(), "another tool in bin is kept");
    assert!(link.symlink_metadata().unwrap().is_symlink());
    assert_eq!(fs::read(&target).unwrap(), b"#!/bin/sh\n");
}

// Prevents the closing message forgetting what was installed.
#[test]
fn the_final_line_names_the_installed_autotag() {
    let env = Env::new();
    let o = env.run_with(&env.limited_path(true), None, &[]);
    ok(&o);
    let out = String::from_utf8_lossy(&o.stdout);
    let last = out.lines().last().unwrap();
    assert!(
        last.contains(&format!("{}/bin/autotag", env.root().display())),
        "{last}"
    );
}
