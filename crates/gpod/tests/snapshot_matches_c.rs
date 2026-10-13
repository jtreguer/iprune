//! The Rust snapshot must be byte-identical to the C `snapshot` tool, which
//! proves the struct layouts, string handling and list walking of the
//! bindings. Needs gcc and the libgpod development headers.

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::{compile_c, make_fixture_db};

fn c_snapshot(bin: &Path, db: &Path) -> Vec<u8> {
    let out = Command::new(bin).arg(db).output().unwrap();
    assert!(
        out.status.success(),
        "C snapshot failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn rust_snapshot(db: &Path) -> Vec<u8> {
    let db = gpod::Database::open_file(db).unwrap();
    let mut out = Vec::new();
    gpod::snapshot::write(&db, &mut out).unwrap();
    out
}

fn assert_same(c: &[u8], rust: &[u8]) {
    if c != rust {
        let (c, rust) = (String::from_utf8_lossy(c), String::from_utf8_lossy(rust));
        let line = c.lines().zip(rust.lines()).position(|(a, b)| a != b);
        panic!("snapshots differ at line {line:?}\n--- C\n{c}\n--- Rust\n{rust}");
    }
}

#[test]
fn fixture_snapshot_matches_c() {
    let dir = tempfile::tempdir().unwrap();
    let (snap, db) = (dir.path().join("snapshot"), dir.path().join("iTunesDB"));
    make_fixture_db(dir.path(), &db);
    compile_c("snapshot.c", &snap);

    let c = c_snapshot(&snap, &db);
    let rust = rust_snapshot(&db);
    assert_same(&c, &rust);

    // Guard against both sides agreeing on an empty or truncated database.
    let text = String::from_utf8(rust).unwrap();
    assert!(text.starts_with("tracks 4 playlists 5\n"), "{text}");
    assert!(
        text.contains("| (null) | (null) | No artist or album |"),
        "{text}"
    );
    assert!(
        text.contains("| -1294967296 |"),
        "size above 2^31 must print as signed: {text}"
    );
    assert!(
        text.contains("P 80s 4\n  00ab00cd00ef0012\n  86536232fbbc81cf\n  00ab00cd00ef0012\n"),
        "{text}"
    );
    assert!(
        text.contains("P Top rated [smart] 1\nP -Last 1\n"),
        "{text}"
    );
}

/// Runs against the real iPod backup, read only. Run with
/// `cargo test -p gpod -- --ignored`, or set `IPRUNE_REAL_ITUNESDB`.
#[test]
#[ignore = "needs the iPod backup on /mnt/seagate-1tb"]
fn real_backup_snapshot_matches_c() {
    let db = std::env::var_os("IPRUNE_REAL_ITUNESDB")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/mnt/seagate-1tb/iPod-backup-2026-10/iTunes/iTunesDB".into());
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("snapshot");
    compile_c("snapshot.c", &snap);
    assert_same(&c_snapshot(&snap, &db), &rust_snapshot(&db));
}
