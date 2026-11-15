//! The default inbox (`~/Music/Inbox`) and a library that holds it
//! (technical document, sections 4 and 7.2): the config still loads, the
//! import is what refuses. The default depends on `HOME`, which a test cannot
//! change for itself (the process is shared and `set_var` is unsafe), so the
//! parent runs `child_default_inbox` of this same binary with `HOME` and
//! `XDG_CONFIG_HOME` pointing into a temp folder.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use iprune_core::Error;
use iprune_core::config::Config;
use iprune_core::import;
use iprune_core::store::Store;

fn config(roots: &[&Path], lists: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.library.roots = roots.iter().map(|p| p.to_path_buf()).collect();
    cfg.playlists.dir = lists.to_owned();
    cfg
}

fn refused<T: std::fmt::Debug>(r: iprune_core::Result<T>, inbox: &Path) {
    match r {
        Err(Error::NotWritable(m)) => {
            assert!(m.contains(inbox.to_str().unwrap()), "names the inbox: {m}");
            assert!(m.contains("library"), "{m}");
        }
        other => panic!("expected NotWritable, got {other:?}"),
    }
}

/// Runs in the child, where `HOME` is the parent's temp folder.
#[test]
fn child_default_inbox() {
    let Ok(home) = std::env::var("IPRUNE_DEFAULT_INBOX_HOME") else {
        return;
    };
    let home = PathBuf::from(home);
    let inbox = home.join("Music/Inbox");
    let lists = home.join("lists");
    fs::create_dir_all(home.join("Music/Rock")).unwrap();
    fs::create_dir_all(&lists).unwrap();
    let store = Store::open_in_memory().unwrap();

    // The default is where the doc says.
    let cfg = config(&[&home.join("Music")], &lists);
    assert_eq!(cfg.inbox(), Some(inbox.clone()));

    // A library that holds the default inbox: the config loads...
    cfg.validate(Path::new("config.toml")).unwrap();
    // ...and the import refuses, whichever way it reaches the inbox, and
    // makes nothing.
    refused(import::inbox_dir(&cfg), &inbox);
    refused(import::read_inbox(store.conn(), &cfg).map(|_| ()), &inbox);
    let src = home.join("song.mp3");
    fs::write(&src, b"x").unwrap();
    refused(
        import::add_to_inbox(&cfg, std::slice::from_ref(&src)),
        &inbox,
    );
    assert!(!inbox.exists(), "the refused import created nothing");

    // Also a library that is the home folder itself: the inbox is inside it.
    let cfg = config(&[&home], &lists);
    refused(import::inbox_dir(&cfg), &inbox);
    assert!(!inbox.exists());

    // A library elsewhere does not overlap: the default inbox is made.
    let other = home.join("elsewhere");
    fs::create_dir(&other).unwrap();
    let cfg = config(&[&other], &lists);
    assert_eq!(import::inbox_dir(&cfg).unwrap(), inbox);
    assert!(inbox.is_dir());

    // An inbox set in the config that overlaps stays a config error.
    let mut cfg = config(&[&home.join("Music")], &lists);
    cfg.import.inbox = home.join("Music/Mine");
    match cfg.validate(Path::new("config.toml")) {
        Err(Error::Config { message, .. }) => {
            assert!(message.contains("import.inbox"), "{message}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_library_that_holds_the_default_inbox_loads_and_only_the_import_refuses() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap().join("home");
    fs::create_dir(&home).unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "child_default_inbox",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", tmp.path().join("cfg"))
        .env_remove("XDG_MUSIC_DIR")
        .env("IPRUNE_DEFAULT_INBOX_HOME", &home)
        .output()
        .unwrap();
    let text = format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("1 passed"), "the child test ran: {text}");
}
