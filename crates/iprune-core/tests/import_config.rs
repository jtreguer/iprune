//! The `[import]` section of the config (technical document, sections 4 and
//! 7.2): defaults, what is saved, and the checks of `Config::validate`. Every
//! folder is a temp one.

use std::fs;
use std::path::{Path, PathBuf};

use iprune_core::Error;
use iprune_core::config::{Config, Import};

struct Dirs {
    _tmp: tempfile::TempDir,
    base: PathBuf,
}

impl Dirs {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        for d in ["music", "music2", "lists"] {
            fs::create_dir(base.join(d)).unwrap();
        }
        Dirs { _tmp: tmp, base }
    }

    fn config(&self) -> Config {
        let mut cfg = Config::default();
        cfg.library.roots = vec![self.base.join("music")];
        cfg.playlists.dir = self.base.join("lists");
        // An inbox of its own, outside the root, so that the default one
        // (in the real home folder) is not looked at.
        cfg.import.inbox = self.base.join("inbox");
        cfg
    }

    fn path(&self) -> PathBuf {
        self.base.join("config.toml")
    }
}

/// The message of a refused config.
fn refused(cfg: &Config, path: &Path) -> String {
    match cfg.validate(path) {
        Err(Error::Config { path: p, message }) => {
            assert_eq!(p, path, "the error names the config file");
            message
        }
        other => panic!("expected the config to be refused, got {other:?}"),
    }
}

// -- Defaults and accessors --------------------------------------------------

#[test]
fn a_config_without_an_import_section_has_every_import_setting_unset() {
    let d = Dirs::new();
    let text = format!(
        "[library]\nroots = [\"{}\"]\n[playlists]\ndir = \"{}\"\n",
        d.base.join("music").display(),
        d.base.join("lists").display()
    );
    fs::write(d.path(), text).unwrap();
    let cfg = Config::load_unchecked(&d.path()).unwrap();
    assert_eq!(cfg.import, Import::default());
    assert!(cfg.import.inbox.as_os_str().is_empty());
    assert!(cfg.import.root.as_os_str().is_empty());
    assert!(cfg.import.autotag.as_os_str().is_empty());
}

#[test]
fn the_import_section_is_read_and_unknown_keys_in_it_are_refused() {
    let d = Dirs::new();
    let base = format!(
        "[library]\nroots = [\"{m}\"]\n[playlists]\ndir = \"{l}\"\n",
        m = d.base.join("music").display(),
        l = d.base.join("lists").display()
    );
    fs::write(
        d.path(),
        format!(
            "{base}[import]\ninbox = \"/x/inbox\"\nroot = \"/x/root\"\nautotag = \"/x/autotag\"\n"
        ),
    )
    .unwrap();
    let cfg = Config::load_unchecked(&d.path()).unwrap();
    assert_eq!(cfg.import.inbox, Path::new("/x/inbox"));
    assert_eq!(cfg.import.root, Path::new("/x/root"));
    assert_eq!(cfg.import.autotag, Path::new("/x/autotag"));

    fs::write(d.path(), format!("{base}[import]\ninbx = \"/x\"\n")).unwrap();
    let e = Config::load_unchecked(&d.path()).unwrap_err();
    assert!(matches!(e, Error::Config { .. }), "{e:?}");
}

#[test]
fn the_inbox_is_the_one_set_else_the_default_in_the_music_folder() {
    let d = Dirs::new();
    let mut cfg = d.config();
    assert_eq!(cfg.inbox(), Some(d.base.join("inbox")));
    cfg.import.inbox = PathBuf::new();
    assert_eq!(cfg.inbox(), Import::default_inbox());
    if let Some(p) = Import::default_inbox() {
        assert!(p.is_absolute());
        assert_eq!(p.file_name().unwrap(), "Inbox");
    }
}

#[test]
fn songs_are_filed_into_import_root_else_the_first_library_root() {
    let d = Dirs::new();
    let mut cfg = d.config();
    cfg.library.roots = vec![d.base.join("music2"), d.base.join("music")];
    assert_eq!(cfg.import_root(), Some(d.base.join("music2").as_path()));
    cfg.import.root = d.base.join("music/Filed");
    assert_eq!(
        cfg.import_root(),
        Some(d.base.join("music/Filed").as_path())
    );
    cfg.import.root = PathBuf::new();
    cfg.library.roots.clear();
    assert_eq!(cfg.import_root(), None);
}

// -- Saving ------------------------------------------------------------------

fn key_lines(text: &str, key: &str) -> usize {
    text.lines()
        .filter(|l| l.trim_start().starts_with(&format!("{key} =")))
        .count()
}

#[test]
fn settings_left_unset_are_not_written_and_those_set_are() {
    let d = Dirs::new();
    let mut cfg = d.config();
    cfg.import.inbox = PathBuf::new();
    cfg.save(&d.path()).unwrap();
    let text = fs::read_to_string(d.path()).unwrap();
    for k in ["inbox", "root", "autotag"] {
        assert_eq!(key_lines(&text, k), 0, "{k} in\n{text}");
    }
    assert_eq!(Config::load_unchecked(&d.path()).unwrap(), cfg);

    cfg.import.inbox = d.base.join("inbox");
    cfg.save(&d.path()).unwrap();
    let text = fs::read_to_string(d.path()).unwrap();
    assert_eq!(key_lines(&text, "inbox"), 1, "{text}");
    assert!(text.contains(d.base.join("inbox").to_str().unwrap()));
    assert_eq!(
        (key_lines(&text, "root"), key_lines(&text, "autotag")),
        (0, 0)
    );

    cfg.import.root = d.base.join("music/Filed");
    cfg.import.autotag = PathBuf::from("/opt/autotag");
    cfg.save(&d.path()).unwrap();
    assert_eq!(Config::load_unchecked(&d.path()).unwrap(), cfg);
    let text = fs::read_to_string(d.path()).unwrap();
    assert_eq!(
        (key_lines(&text, "root"), key_lines(&text, "autotag")),
        (1, 1)
    );
}

// -- Validation --------------------------------------------------------------

#[test]
fn a_config_with_sound_import_settings_is_accepted() {
    let d = Dirs::new();
    let mut cfg = d.config();
    cfg.validate(&d.path()).unwrap();
    // Unset ones are accepted too (the default inbox is the real one's, in
    // the home folder, which is not in the temp library).
    cfg.import.root = d.base.join("music"); // a root itself
    cfg.validate(&d.path()).unwrap();
    cfg.import.root = d.base.join("music/Filed/Deep"); // inside one, not made yet
    cfg.validate(&d.path()).unwrap();
    cfg.import.autotag = PathBuf::from("/no/such/autotag"); // not looked for here
    cfg.validate(&d.path()).unwrap();
}

#[test]
fn the_import_paths_must_be_absolute() {
    let d = Dirs::new();
    for (name, set) in [
        (
            "import.inbox",
            (|c: &mut Config| c.import.inbox = "inbox".into()) as fn(&mut Config),
        ),
        ("import.root", |c| c.import.root = "music/Filed".into()),
        ("import.autotag", |c| {
            c.import.autotag = "tools/autotag".into()
        }),
    ] {
        let mut cfg = d.config();
        set(&mut cfg);
        let m = refused(&cfg, &d.path());
        assert!(m.contains(name) && m.contains("absolute"), "{name}: {m}");
    }
}

#[test]
fn the_root_to_file_into_must_be_a_library_root_or_inside_one() {
    let d = Dirs::new();
    let mut cfg = d.config();
    cfg.import.root = d.base.join("elsewhere");
    let m = refused(&cfg, &d.path());
    assert!(m.contains("import.root"), "{m}");
    // Another library root is fine.
    cfg.library.roots = vec![d.base.join("music"), d.base.join("music2")];
    cfg.import.root = d.base.join("music2/new");
    cfg.validate(&d.path()).unwrap();
    // A sibling whose name starts with a root's name is not inside it.
    cfg.library.roots = vec![d.base.join("music")];
    cfg.import.root = d.base.join("music2");
    assert!(refused(&cfg, &d.path()).contains("import.root"));
    // Nor is the parent of a root.
    cfg.import.root = d.base.clone();
    assert!(refused(&cfg, &d.path()).contains("import.root"));
}

#[test]
fn the_inbox_must_be_neither_inside_a_library_root_nor_hold_one() {
    let d = Dirs::new();
    let mut cfg = d.config();
    for (what, inbox) in [
        ("a root itself", d.base.join("music")),
        ("inside a root", d.base.join("music/Inbox")),
        ("deep inside a root", d.base.join("music/a/b/Inbox")),
        ("holding a root", d.base.clone()),
    ] {
        cfg.import.inbox = inbox;
        let m = refused(&cfg, &d.path());
        assert!(m.contains("import.inbox"), "{what}: {m}");
    }
    // Beside the root is fine, also with a name that starts like it.
    for inbox in ["inbox", "music2", "music-inbox"] {
        cfg.import.inbox = d.base.join(inbox);
        cfg.validate(&d.path()).unwrap();
    }
}

#[test]
fn a_symbolic_link_into_the_library_does_not_hide_an_inbox_inside_it() {
    let d = Dirs::new();
    let mut cfg = d.config();
    fs::create_dir(d.base.join("music/real-inbox")).unwrap();
    std::os::unix::fs::symlink(d.base.join("music/real-inbox"), d.base.join("link")).unwrap();
    cfg.import.inbox = d.base.join("link");
    assert!(refused(&cfg, &d.path()).contains("import.inbox"));
    // And a link to the library root itself, from outside it.
    cfg.import.inbox = d.base.join("link2");
    std::os::unix::fs::symlink(d.base.join("music"), d.base.join("link2")).unwrap();
    assert!(refused(&cfg, &d.path()).contains("import.inbox"));
}

#[test]
fn the_inbox_of_a_library_that_is_a_symbolic_link_is_checked_by_where_it_really_is() {
    let d = Dirs::new();
    let mut cfg = d.config();
    // The root is a link to music2; an inbox in music2 is inside the library.
    std::os::unix::fs::symlink(d.base.join("music2"), d.base.join("lib-link")).unwrap();
    cfg.library.roots = vec![d.base.join("lib-link")];
    cfg.import.inbox = d.base.join("music2/Inbox");
    assert!(refused(&cfg, &d.path()).contains("import.inbox"));
}
