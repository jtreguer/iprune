//! Helpers shared by the integration tests. Fixtures are built by the C
//! programs in `tests/c`, which use only libgpod, so they do not depend on the
//! Rust code under test.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn compile_c(src: &str, out: &Path) {
    let flags = Command::new("pkg-config")
        .args(["--cflags", "--libs", "libgpod-1.0", "glib-2.0"])
        .output()
        .expect("pkg-config");
    assert!(flags.status.success());
    let flags = String::from_utf8(flags.stdout).unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/c")
        .join(src);
    let status = Command::new("gcc")
        .arg("-o")
        .arg(out)
        .arg(&src)
        .args(flags.split_whitespace())
        .status()
        .expect("gcc");
    assert!(status.success(), "compiling {}", src.display());
}

/// Builds the C fixture database at `db` using libgpod.
pub fn make_fixture_db(workdir: &Path, db: &Path) {
    let make = workdir.join("make_fixture");
    compile_c("make_fixture.c", &make);
    let out = Command::new(&make).arg(db).output().unwrap();
    assert!(
        out.status.success(),
        "make_fixture failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A fake iPod tree in a temp dir: `<root>/iPod_Control/iTunes/iTunesDB`
/// written by libgpod, and optionally `iPod_Control/Device/SysInfo`.
pub struct FakeIpod {
    pub dir: tempfile::TempDir,
}

impl FakeIpod {
    pub fn new(sysinfo: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctl = dir.path().join("iPod_Control");
        fs::create_dir_all(ctl.join("iTunes")).unwrap();
        fs::create_dir_all(ctl.join("Device")).unwrap();
        fs::create_dir_all(ctl.join("Music")).unwrap();
        // The C tool is built outside the mountpoint so the tree stays clean.
        let work = tempfile::tempdir().unwrap();
        make_fixture_db(work.path(), &ctl.join("iTunes/iTunesDB"));
        if let Some(s) = sysinfo {
            fs::write(ctl.join("Device/SysInfo"), s).unwrap();
        }
        FakeIpod { dir }
    }

    pub fn root(&self) -> PathBuf {
        self.dir.path().to_owned()
    }

    pub fn itunesdb(&self) -> PathBuf {
        self.dir.path().join("iPod_Control/iTunes/iTunesDB")
    }
}

/// Builds an iTunesDB at `db` from a `make_ipod.c` manifest (see that file for
/// the format) using libgpod.
pub fn make_ipod_db(workdir: &Path, db: &Path, manifest: &str) {
    let make = workdir.join("make_ipod");
    if !make.exists() {
        compile_c("make_ipod.c", &make);
    }
    let file = workdir.join("manifest.tsv");
    fs::write(&file, manifest).unwrap();
    let out = Command::new(&make).arg(db).arg(&file).output().unwrap();
    assert!(
        out.status.success(),
        "make_ipod failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

impl FakeIpod {
    /// A fake iPod whose iTunesDB is described by a `make_ipod.c` manifest.
    /// `iPod_Control/Music` holds the folders F00 to F49 of a real iPod, which
    /// libgpod needs to copy a file ("No 'F..' directories found" otherwise).
    pub fn from_manifest(manifest: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctl = dir.path().join("iPod_Control");
        fs::create_dir_all(ctl.join("iTunes")).unwrap();
        fs::create_dir_all(ctl.join("Device")).unwrap();
        for n in 0..50 {
            fs::create_dir_all(ctl.join(format!("Music/F{n:02}"))).unwrap();
        }
        let work = tempfile::tempdir().unwrap();
        make_ipod_db(work.path(), &ctl.join("iTunes/iTunesDB"), manifest);
        fs::write(
            ctl.join("Device/SysInfo"),
            "FirewireGuid: 0x000A27001234ABCD\n",
        )
        .unwrap();
        FakeIpod { dir }
    }
}
