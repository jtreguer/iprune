//! End to end tests of the `iprune` binary. Fixtures are built by the C
//! programs kept in `crates/gpod/tests/c` (libgpod only, no Rust involved), and
//! the expected snapshot is produced by the C `snapshot` tool.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn c_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../gpod/tests/c")
}

fn compile_c(src: &str, out: &Path) {
    let flags = Command::new("pkg-config")
        .args(["--cflags", "--libs", "libgpod-1.0", "glib-2.0"])
        .output()
        .expect("pkg-config");
    assert!(flags.status.success());
    let flags = String::from_utf8(flags.stdout).unwrap();
    let status = Command::new("gcc")
        .arg("-o")
        .arg(out)
        .arg(c_dir().join(src))
        .args(flags.split_whitespace())
        .status()
        .expect("gcc");
    assert!(status.success(), "compiling {src}");
}

fn iprune(args: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_iprune"))
        .args(args)
        .output()
        .unwrap()
}

fn iprune_str(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_iprune"))
        .args(args)
        .output()
        .unwrap()
}

struct Fixture {
    /// Holds the C tools and a standalone iTunesDB.
    work: tempfile::TempDir,
    /// A fake iPod tree whose iTunesDB is the same database.
    ipod: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let work = tempfile::tempdir().unwrap();
        let ipod = tempfile::tempdir().unwrap();
        compile_c("make_fixture.c", &work.path().join("make_fixture"));
        compile_c("snapshot.c", &work.path().join("snapshot"));
        let db = work.path().join("iTunesDB");
        let st = Command::new(work.path().join("make_fixture"))
            .arg(&db)
            .status()
            .unwrap();
        assert!(st.success());
        let itunes = ipod.path().join("iPod_Control/iTunes");
        fs::create_dir_all(&itunes).unwrap();
        fs::copy(&db, itunes.join("iTunesDB")).unwrap();
        Fixture { work, ipod }
    }

    fn db(&self) -> PathBuf {
        self.work.path().join("iTunesDB")
    }

    fn c_snapshot(&self, arg: &Path) -> Vec<u8> {
        let out = Command::new(self.work.path().join("snapshot"))
            .arg(arg)
            .output()
            .unwrap();
        assert!(out.status.success());
        out.stdout
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn snapshot_of_file_matches_c_tool() {
    let f = Fixture::new();
    let out = iprune(&[Path::new("snapshot"), &f.db()]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(out.stderr.is_empty(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), text(&f.c_snapshot(&f.db())));
    // Guard against both sides printing nothing.
    assert!(
        text(&out.stdout).starts_with("tracks 4 playlists 5\n"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn snapshot_of_mountpoint_matches_c_tool() {
    let f = Fixture::new();
    let out = iprune(&[Path::new("snapshot"), f.ipod.path()]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), text(&f.c_snapshot(f.ipod.path())));
    assert!(
        text(&out.stdout).contains("P 80s 4\n"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn snapshot_does_not_modify_the_database() {
    let f = Fixture::new();
    let db = f.ipod.path().join("iPod_Control/iTunes/iTunesDB");
    let before = fs::read(&db).unwrap();
    let out = iprune(&[Path::new("snapshot"), f.ipod.path()]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(fs::read(&db).unwrap(), before);
}

#[test]
fn missing_file_exits_1_with_message_and_no_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope/iTunesDB");
    let out = iprune(&[Path::new("snapshot"), &missing]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "{}", text(&out.stdout));
    let err = text(&out.stderr);
    assert!(err.starts_with("iprune:"), "{err}");
    assert!(err.contains(missing.to_str().unwrap()), "{err}");
}

#[test]
fn garbage_file_exits_1_with_no_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("iTunesDB");
    fs::write(&p, b"definitely not a database").unwrap();
    let out = iprune(&[Path::new("snapshot"), &p]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "{}", text(&out.stdout));
    assert!(text(&out.stderr).starts_with("iprune:"));
}

#[test]
fn directory_without_database_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let out = iprune(&[Path::new("snapshot"), dir.path()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "{}", text(&out.stdout));
    assert!(text(&out.stderr).contains(dir.path().to_str().unwrap()));
}

#[test]
fn usage_errors_exit_2() {
    for args in [
        &[][..],
        &["snapshot"][..],
        &["frobnicate"][..],
        &["snapshot", "a", "b"][..],
        &["snapshot", "--no-such-flag", "a"][..],
    ] {
        let out = iprune_str(args);
        assert_eq!(out.status.code(), Some(2), "args {args:?}");
        assert!(out.stdout.is_empty(), "args {args:?}");
        assert!(!out.stderr.is_empty(), "args {args:?}");
    }
}

#[test]
fn help_exits_0() {
    let out = iprune_str(&["--help"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(text(&out.stdout).contains("snapshot"));
}

/// Prevents `iprune snapshot X | head` from reporting an error: a closed
/// stdout (EPIPE) must exit 0 and stay silent.
#[test]
fn closed_stdout_is_not_an_error() {
    use std::process::Stdio;
    let f = Fixture::new();
    // The read end is closed before the child exists, so every write fails
    // with EPIPE regardless of timing or output size.
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let out = Command::new(env!("CARGO_BIN_EXE_iprune"))
        .arg("snapshot")
        .arg(f.ipod.path())
        .stdout(Stdio::from(writer))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(out.stderr.is_empty(), "{}", text(&out.stderr));
}
