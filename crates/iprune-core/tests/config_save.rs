//! `Config::save` writes through a temporary file and renames it over the
//! old config (audit L41), in a temp folder.

use std::fs;

use iprune_core::config::Config;

/// A config that passes validation: its folders exist.
fn config(dir: &std::path::Path, keep: u32) -> Config {
    let mut cfg = Config::default();
    cfg.library.roots = vec![dir.to_owned()];
    cfg.playlists.dir = dir.to_owned();
    cfg.sync.keep_backups = keep;
    cfg
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    v.sort();
    v
}

#[test]
fn save_writes_the_config_and_leaves_no_temporary_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub/config.toml");
    let mut cfg = config(dir.path(), 9);
    cfg.save(&path).unwrap();
    assert_eq!(names(path.parent().unwrap()), ["config.toml"]);
    assert_eq!(Config::load(&path).unwrap(), cfg);

    // Over an existing one: replaced whole, still no leftover.
    cfg.sync.keep_backups = 2;
    cfg.save(&path).unwrap();
    assert_eq!(names(path.parent().unwrap()), ["config.toml"]);
    assert_eq!(Config::load(&path).unwrap().sync.keep_backups, 2);
}

// Prevents (audit L41) a write failing part way leaving a truncated config:
// the new text goes to a temporary file first, so when that cannot be
// created the old config is untouched.
#[test]
fn a_save_that_cannot_write_its_temporary_file_leaves_the_old_config_intact() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let old = config(dir.path(), 4);
    old.save(&path).unwrap();
    let before = fs::read(&path).unwrap();

    // A folder where the temporary file would go.
    fs::create_dir(dir.path().join("config.toml.tmp")).unwrap();
    let new = config(dir.path(), 11);
    assert!(new.save(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(Config::load(&path).unwrap(), old);
}
