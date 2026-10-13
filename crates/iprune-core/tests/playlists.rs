mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::{Env, Kind};
use iprune_core::{Error, playlists};

/// An env with three indexed tracks: Title1..Title3 (a, b, c).
fn env3() -> (Env, [PathBuf; 3]) {
    let mut env = Env::new();
    let t = [
        env.add("a.mp3", Kind::Mp3, 1),
        env.add("b.mp3", Kind::Mp3, 2),
        env.add("c.mp3", Kind::Mp3, 3),
    ];
    env.scan();
    (env, t)
}

fn s(p: &Path) -> String {
    p.to_str().unwrap().to_owned()
}

fn titles(env: &Env, name: &str) -> Vec<String> {
    playlists::show(env.store.conn(), &env.cfg, name)
        .unwrap()
        .into_iter()
        .map(|i| match i.track {
            Some(t) => t.display_title().into_owned(),
            None => format!("?{}", i.entry.location),
        })
        .collect()
}

#[test]
fn create_writes_empty_file_and_index_row() {
    let (env, _) = env3();
    playlists::create(env.store.conn(), &env.cfg, "mix").unwrap();
    assert_eq!(env.read_playlist("mix"), "#EXTM3U\n");
    assert!(env.playlist_id("mix").is_some());
    assert!(env.indexed_items("mix").is_empty());
}

#[test]
fn create_twice_fails_and_keeps_existing_content() {
    let (env, t) = env3();
    playlists::create(env.store.conn(), &env.cfg, "mix").unwrap();
    playlists::add(env.store.conn(), &env.cfg, "mix", &[t[0].clone()], None).unwrap();
    let before = env.read_playlist("mix");
    let err = playlists::create(env.store.conn(), &env.cfg, "mix").unwrap_err();
    assert!(
        matches!(err, Error::PlaylistExists(ref n) if n == "mix"),
        "{err:?}"
    );
    assert_eq!(env.read_playlist("mix"), before);
}

#[test]
fn invalid_names_are_rejected_and_nothing_is_created() {
    let (env, _) = env3();
    for bad in ["", "   ", "a/b", "../escape", ".hidden", "line\nbreak"] {
        let err = playlists::create(env.store.conn(), &env.cfg, bad).unwrap_err();
        assert!(matches!(err, Error::InvalidName(..)), "{bad:?}: {err:?}");
    }
    assert_eq!(fs::read_dir(&env.lists).unwrap().count(), 0);
    assert!(
        !env.lists.parent().unwrap().join("escape.m3u8").exists(),
        "no escape out of the playlists folder"
    );
}

#[test]
fn rename_keeps_row_id_and_moves_the_file() {
    let (env, t) = env3();
    let conn = env.store.conn();
    playlists::create(conn, &env.cfg, "old").unwrap();
    playlists::add(conn, &env.cfg, "old", &[t[0].clone(), t[1].clone()], None).unwrap();
    // A later row, so a delete-and-reinsert could not get the same id back.
    playlists::create(conn, &env.cfg, "later").unwrap();
    let id = env.playlist_id("old").unwrap();
    let text = env.read_playlist("old");

    playlists::rename(conn, &env.cfg, "old", "new").unwrap();

    assert!(!env.playlist_file("old").exists());
    assert_eq!(env.read_playlist("new"), text);
    assert_eq!(env.playlist_id("new"), Some(id));
    assert_eq!(env.playlist_id("old"), None);
    assert_eq!(env.indexed_items("new").len(), 2);
}

#[test]
fn rename_onto_existing_or_from_unknown_changes_nothing() {
    let (env, t) = env3();
    let conn = env.store.conn();
    playlists::create(conn, &env.cfg, "one").unwrap();
    playlists::create(conn, &env.cfg, "two").unwrap();
    playlists::add(conn, &env.cfg, "two", &[t[0].clone()], None).unwrap();
    let (one, two) = (env.read_playlist("one"), env.read_playlist("two"));

    let err = playlists::rename(conn, &env.cfg, "one", "two").unwrap_err();
    assert!(
        matches!(err, Error::PlaylistExists(ref n) if n == "two"),
        "{err:?}"
    );
    let err = playlists::rename(conn, &env.cfg, "nope", "x").unwrap_err();
    assert!(
        matches!(err, Error::NoSuchPlaylist(ref n) if n == "nope"),
        "{err:?}"
    );
    let err = playlists::rename(conn, &env.cfg, "one", "bad/name").unwrap_err();
    assert!(matches!(err, Error::InvalidName(..)), "{err:?}");

    assert_eq!(
        (env.read_playlist("one"), env.read_playlist("two")),
        (one, two)
    );
    assert!(!env.playlist_file("x").exists());
    assert!(env.playlist_id("one").is_some() && env.playlist_id("two").is_some());
}

#[test]
fn delete_removes_file_and_row_but_not_tracks() {
    let (env, t) = env3();
    let conn = env.store.conn();
    playlists::create(conn, &env.cfg, "gone").unwrap();
    playlists::create(conn, &env.cfg, "stays").unwrap();
    playlists::add(conn, &env.cfg, "gone", &[t[0].clone()], None).unwrap();

    playlists::delete(conn, &env.cfg, "gone").unwrap();
    assert!(!env.playlist_file("gone").exists());
    assert_eq!(env.playlist_id("gone"), None);
    assert!(env.indexed_items("gone").is_empty());
    assert!(t[0].exists(), "the audio file is untouched");
    assert_eq!(env.tracks().len(), 3);
    assert!(env.playlist_file("stays").exists() && env.playlist_id("stays").is_some());

    let err = playlists::delete(conn, &env.cfg, "gone").unwrap_err();
    assert!(matches!(err, Error::NoSuchPlaylist(_)), "{err:?}");
}

#[test]
fn add_appends_in_order_with_extinf_and_updates_index() {
    let (env, t) = env3();
    let conn = env.store.conn();
    playlists::create(conn, &env.cfg, "mix").unwrap();
    playlists::add(conn, &env.cfg, "mix", &[t[2].clone(), t[0].clone()], None).unwrap();
    playlists::add(conn, &env.cfg, "mix", &[t[1].clone()], None).unwrap();

    assert_eq!(env.locations("mix"), vec![s(&t[2]), s(&t[0]), s(&t[1])]);
    let text = env.read_playlist("mix");
    assert!(
        text.starts_with("#EXTM3U\n#EXTINF:1,Artist3 - Title3\n"),
        "{text}"
    );
    let ids: Vec<_> = env.indexed_items("mix").into_iter().map(|i| i.1).collect();
    assert_eq!(
        ids,
        vec![
            Some(env.track(&t[2]).id),
            Some(env.track(&t[0]).id),
            Some(env.track(&t[1]).id)
        ]
    );
}

#[test]
fn add_at_position_inserts_before_that_entry() {
    let (env, t) = env3();
    let conn = env.store.conn();
    playlists::create(conn, &env.cfg, "mix").unwrap();
    playlists::add(conn, &env.cfg, "mix", &[t[0].clone(), t[1].clone()], None).unwrap();
    playlists::add(conn, &env.cfg, "mix", &[t[2].clone()], Some(2)).unwrap();
    assert_eq!(titles(&env, "mix"), ["Title1", "Title3", "Title2"]);
    playlists::add(conn, &env.cfg, "mix", &[t[2].clone()], Some(1)).unwrap();
    assert_eq!(
        titles(&env, "mix"),
        ["Title3", "Title1", "Title3", "Title2"]
    );
    // len + 1 means the end
    playlists::add(conn, &env.cfg, "mix", &[t[0].clone()], Some(5)).unwrap();
    assert_eq!(
        titles(&env, "mix"),
        ["Title3", "Title1", "Title3", "Title2", "Title1"]
    );
}

#[test]
fn add_to_empty_playlist_at_one_works() {
    let (env, t) = env3();
    playlists::create(env.store.conn(), &env.cfg, "mix").unwrap();
    playlists::add(env.store.conn(), &env.cfg, "mix", &[t[0].clone()], Some(1)).unwrap();
    assert_eq!(titles(&env, "mix"), ["Title1"]);
}

#[test]
fn add_of_a_path_not_in_the_index_fails_and_writes_nothing() {
    let (env, t) = env3();
    let conn = env.store.conn();
    playlists::create(conn, &env.cfg, "mix").unwrap();
    playlists::add(conn, &env.cfg, "mix", &[t[0].clone()], None).unwrap();
    let before = env.read_playlist("mix");
    let items_before = env.indexed_items("mix");
    let stranger = env.add("new-not-scanned.mp3", Kind::Mp3, 8);

    // The good file comes first: it must not be added either.
    let err = playlists::add(
        conn,
        &env.cfg,
        "mix",
        &[t[1].clone(), stranger.clone()],
        None,
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::NotIndexed(ref p) if *p == stranger),
        "{err:?}"
    );
    assert_eq!(env.read_playlist("mix"), before);
    assert_eq!(env.indexed_items("mix"), items_before);
}

#[test]
fn add_of_a_missing_track_is_refused() {
    let (mut env, t) = env3();
    playlists::create(env.store.conn(), &env.cfg, "mix").unwrap();
    fs::remove_file(&t[1]).unwrap();
    assert_eq!(env.scan().missing, vec![t[1].clone()]);
    let err = playlists::add(env.store.conn(), &env.cfg, "mix", &[t[1].clone()], None).unwrap_err();
    assert!(matches!(err, Error::NotIndexed(_)), "{err:?}");
    assert_eq!(env.read_playlist("mix"), "#EXTM3U\n");
}

#[test]
fn add_to_unknown_playlist_fails_and_creates_no_file() {
    let (env, t) = env3();
    let err =
        playlists::add(env.store.conn(), &env.cfg, "nope", &[t[0].clone()], None).unwrap_err();
    assert!(matches!(err, Error::NoSuchPlaylist(_)), "{err:?}");
    assert!(!env.playlist_file("nope").exists());
}

#[test]
fn add_with_bad_position_leaves_file_unchanged() {
    let (env, t) = env3();
    let conn = env.store.conn();
    playlists::create(conn, &env.cfg, "mix").unwrap();
    playlists::add(conn, &env.cfg, "mix", &[t[0].clone(), t[1].clone()], None).unwrap();
    let before = env.read_playlist("mix");
    for (pos, expected_len) in [(0, 3), (4, 3), (99, 3)] {
        let err = playlists::add(conn, &env.cfg, "mix", &[t[2].clone()], Some(pos)).unwrap_err();
        assert!(
            matches!(err, Error::BadPosition { position, len } if position == pos && len == expected_len),
            "{pos}: {err:?}"
        );
        assert_eq!(env.read_playlist("mix"), before);
    }
}

fn five(env: &Env, t: &[PathBuf; 3]) {
    let conn = env.store.conn();
    playlists::create(conn, &env.cfg, "mix").unwrap();
    // Title1 Title2 Title3 Title1 Title2
    playlists::add(
        conn,
        &env.cfg,
        "mix",
        &[
            t[0].clone(),
            t[1].clone(),
            t[2].clone(),
            t[0].clone(),
            t[1].clone(),
        ],
        None,
    )
    .unwrap();
}

#[test]
fn remove_several_positions_removes_exactly_those_entries() {
    let (env, t) = env3();
    five(&env, &t);
    playlists::remove(env.store.conn(), &env.cfg, "mix", &[4, 1, 5]).unwrap();
    assert_eq!(titles(&env, "mix"), ["Title2", "Title3"]);
    assert_eq!(env.indexed_items("mix").len(), 2);
}

#[test]
fn remove_duplicate_positions_removes_the_entry_once() {
    let (env, t) = env3();
    five(&env, &t);
    playlists::remove(env.store.conn(), &env.cfg, "mix", &[2, 2, 2]).unwrap();
    assert_eq!(
        titles(&env, "mix"),
        ["Title1", "Title3", "Title1", "Title2"]
    );
}

#[test]
fn remove_with_any_bad_position_changes_nothing() {
    let (env, t) = env3();
    five(&env, &t);
    let before = env.read_playlist("mix");
    for bad in [vec![1, 6], vec![0], vec![2, 0, 3]] {
        let err = playlists::remove(env.store.conn(), &env.cfg, "mix", &bad).unwrap_err();
        assert!(
            matches!(err, Error::BadPosition { len: 5, .. }),
            "{bad:?}: {err:?}"
        );
        assert_eq!(env.read_playlist("mix"), before, "{bad:?}");
    }
    assert_eq!(env.indexed_items("mix").len(), 5);
}

#[test]
fn move_entry_forward_and_backward() {
    let (env, t) = env3();
    let conn = env.store.conn();
    playlists::create(conn, &env.cfg, "mix").unwrap();
    playlists::add(conn, &env.cfg, "mix", &t.clone(), None).unwrap();
    playlists::move_entry(conn, &env.cfg, "mix", 1, 3).unwrap();
    assert_eq!(titles(&env, "mix"), ["Title2", "Title3", "Title1"]);
    playlists::move_entry(conn, &env.cfg, "mix", 3, 1).unwrap();
    assert_eq!(titles(&env, "mix"), ["Title1", "Title2", "Title3"]);
    playlists::move_entry(conn, &env.cfg, "mix", 2, 2).unwrap();
    assert_eq!(titles(&env, "mix"), ["Title1", "Title2", "Title3"]);
    let ids: Vec<_> = env.indexed_items("mix").into_iter().map(|i| i.0).collect();
    assert_eq!(ids, [0, 1, 2]);
}

#[test]
fn move_with_bad_positions_changes_nothing() {
    let (env, t) = env3();
    let conn = env.store.conn();
    playlists::create(conn, &env.cfg, "mix").unwrap();
    playlists::add(conn, &env.cfg, "mix", &t.clone(), None).unwrap();
    let before = env.read_playlist("mix");
    for (from, to) in [(0, 1), (1, 0), (4, 1), (1, 4)] {
        let err = playlists::move_entry(conn, &env.cfg, "mix", from, to).unwrap_err();
        assert!(
            matches!(err, Error::BadPosition { len: 3, .. }),
            "{from}->{to}: {err:?}"
        );
        assert_eq!(env.read_playlist("mix"), before);
    }
}

#[test]
fn unresolved_entries_survive_edits_verbatim_with_their_extinf() {
    let (env, t) = env3();
    let original = format!(
        "#EXTM3U\n#EXTINF:241,Ghost - Gone Song\n/old/place/ghost.mp3\n{}\n#EXTINF:7,Odd\nhttp://example.com/stream one\n  spaced entry.mp3\n",
        t[0].display()
    );
    env.write_playlist("mix", &original);
    playlists::sync_from_disk(env.store.conn(), &env.cfg, true).unwrap();
    let conn = env.store.conn();

    playlists::add(conn, &env.cfg, "mix", &[t[1].clone()], Some(2)).unwrap();
    playlists::move_entry(conn, &env.cfg, "mix", 1, 5).unwrap();
    playlists::remove(conn, &env.cfg, "mix", &[2]).unwrap();

    let text = env.read_playlist("mix");
    assert!(
        text.contains("#EXTINF:241,Ghost - Gone Song\n/old/place/ghost.mp3\n"),
        "{text}"
    );
    assert!(
        text.contains("#EXTINF:7,Odd\nhttp://example.com/stream one\n"),
        "{text}"
    );
    assert!(text.contains("\n  spaced entry.mp3\n"), "{text:?}");
    // [ghost a http spaced] -> add b at 2 -> move 1 to 5 -> [b a http spaced ghost]
    // -> remove 2 drops a.
    assert_eq!(
        env.locations("mix"),
        vec![
            s(&t[1]),
            "http://example.com/stream one".to_owned(),
            "  spaced entry.mp3".to_owned(),
            "/old/place/ghost.mp3".to_owned(),
        ]
    );
    let unresolved = env
        .indexed_items("mix")
        .iter()
        .filter(|i| i.1.is_none())
        .count();
    assert_eq!(unresolved, 3);
}

#[test]
fn relative_style_writes_relative_locations() {
    let mut env = Env::new().relative_playlists();
    let a = env.add("Album/a.mp3", Kind::Mp3, 1);
    env.scan();
    playlists::create(env.store.conn(), &env.cfg, "mix").unwrap();
    playlists::add(
        env.store.conn(),
        &env.cfg,
        "mix",
        std::slice::from_ref(&a),
        None,
    )
    .unwrap();
    assert_eq!(
        env.locations("mix"),
        vec!["../music/Album/a.mp3".to_string()]
    );
    // And a relative entry resolves back to the track.
    let shown = playlists::show(env.store.conn(), &env.cfg, "mix").unwrap();
    assert_eq!(shown[0].track.as_ref().unwrap().path, a);
}

#[test]
fn absolute_style_writes_absolute_locations() {
    let (env, t) = env3();
    playlists::create(env.store.conn(), &env.cfg, "mix").unwrap();
    playlists::add(env.store.conn(), &env.cfg, "mix", &[t[0].clone()], None).unwrap();
    assert_eq!(env.locations("mix"), vec![s(&t[0])]);
}

#[test]
fn relative_input_path_is_resolved_against_cwd() {
    // add() resolves relative arguments against the process cwd; use an
    // absolute path with `..` segments to exercise normalization without
    // changing the cwd of the whole test binary.
    let (env, t) = env3();
    playlists::create(env.store.conn(), &env.cfg, "mix").unwrap();
    let weird = env.music.join("sub/../a.mp3");
    playlists::add(env.store.conn(), &env.cfg, "mix", &[weird], None).unwrap();
    assert_eq!(env.locations("mix"), vec![s(&t[0])]);
}

#[test]
fn edits_keep_file_permissions() {
    let (env, t) = env3();
    playlists::create(env.store.conn(), &env.cfg, "mix").unwrap();
    let f = env.playlist_file("mix");
    fs::set_permissions(&f, fs::Permissions::from_mode(0o640)).unwrap();
    playlists::add(env.store.conn(), &env.cfg, "mix", &[t[0].clone()], None).unwrap();
    playlists::move_entry(env.store.conn(), &env.cfg, "mix", 1, 1).unwrap();
    assert_eq!(
        fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert_eq!(
        fs::read_dir(&env.lists).unwrap().count(),
        1,
        "no temp files left in the playlists folder"
    );
}

fn touch_future(p: &Path, secs: u64) {
    fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(secs))
        .unwrap();
}

#[test]
fn sync_from_disk_picks_up_external_edits_and_deletions() {
    let (env, t) = env3();
    let conn = env.store.conn();
    playlists::create(conn, &env.cfg, "edited").unwrap();
    playlists::create(conn, &env.cfg, "deleted").unwrap();
    playlists::create(conn, &env.cfg, "same").unwrap();
    playlists::add(conn, &env.cfg, "edited", &[t[0].clone()], None).unwrap();
    assert_eq!(env.indexed_items("edited").len(), 1);

    let f = env.playlist_file("edited");
    fs::write(
        &f,
        format!(
            "#EXTM3U\n{}\n{}\n/nowhere.mp3\n",
            t[1].display(),
            t[2].display()
        ),
    )
    .unwrap();
    touch_future(&f, 100);
    fs::remove_file(env.playlist_file("deleted")).unwrap();
    fs::write(
        env.lists.join("fresh.m3u8"),
        format!("{}\n", t[0].display()),
    )
    .unwrap();
    fs::write(env.lists.join(".hidden.m3u8"), "x\n").unwrap();
    fs::write(env.lists.join("notes.txt"), "x\n").unwrap();

    let r = playlists::sync_from_disk(conn, &env.cfg, false).unwrap();
    assert_eq!(r.removed, vec!["deleted".to_string()]);
    assert_eq!(r.loaded, 2, "edited and fresh only: {r:?}");
    assert_eq!(
        r.unresolved,
        vec![("edited".to_string(), "/nowhere.mp3".to_string())]
    );
    let ids: Vec<_> = env
        .indexed_items("edited")
        .into_iter()
        .map(|i| i.1)
        .collect();
    assert_eq!(
        ids,
        vec![Some(env.track(&t[1]).id), Some(env.track(&t[2]).id), None]
    );
    assert_eq!(env.playlist_id("deleted"), None);
    assert!(env.playlist_id("fresh").is_some());
    assert!(env.playlist_id(".hidden").is_none());
    assert!(env.playlist_id("notes").is_none());
    assert!(env.playlist_id("same").is_some());
}

#[test]
fn sync_from_disk_without_force_skips_unchanged_files() {
    let (env, t) = env3();
    let conn = env.store.conn();
    playlists::create(conn, &env.cfg, "p").unwrap();
    playlists::add(conn, &env.cfg, "p", &[t[0].clone()], None).unwrap();
    assert_eq!(
        playlists::sync_from_disk(conn, &env.cfg, false)
            .unwrap()
            .loaded,
        0
    );
    assert_eq!(
        playlists::sync_from_disk(conn, &env.cfg, true)
            .unwrap()
            .loaded,
        1
    );
}

#[test]
fn sync_from_disk_skips_a_non_utf8_playlist_and_reports_it() {
    let (env, t) = env3();
    let conn = env.store.conn();
    playlists::create(conn, &env.cfg, "good").unwrap();
    playlists::add(conn, &env.cfg, "good", &[t[0].clone()], None).unwrap();
    fs::write(env.lists.join("bad.m3u8"), [0xff, 0xfe, b'\n']).unwrap();
    let r = playlists::sync_from_disk(conn, &env.cfg, true).unwrap();
    assert_eq!(r.skipped.len(), 1);
    assert!(r.skipped[0].0.ends_with("bad.m3u8"));
    assert!(env.playlist_id("bad").is_none());
    assert_eq!(env.indexed_items("good").len(), 1);
}

// Safe writes.

#[test]
fn create_over_an_unindexed_existing_file_fails_and_leaves_it_alone() {
    let (env, _) = env3();
    let f = env.write_playlist("hand", "#EXTM3U\n/some/where.mp3\n");
    let mtime = fs::metadata(&f).unwrap().modified().unwrap();
    let err = playlists::create(env.store.conn(), &env.cfg, "hand").unwrap_err();
    assert!(
        matches!(err, Error::PlaylistExists(ref n) if n == "hand"),
        "{err:?}"
    );
    assert_eq!(
        fs::read_to_string(&f).unwrap(),
        "#EXTM3U\n/some/where.mp3\n"
    );
    assert_eq!(fs::metadata(&f).unwrap().modified().unwrap(), mtime);
    assert!(env.playlist_id("hand").is_none());
    assert_eq!(fs::read_dir(&env.lists).unwrap().count(), 1, "no temp file");
}

#[test]
fn write_atomic_create_new_never_clobbers() {
    use iprune_core::playlist::{WriteMode, write_atomic};
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.m3u8");
    write_atomic(&p, "first", WriteMode::CreateNew).unwrap();
    let err = write_atomic(&p, "second", WriteMode::CreateNew).unwrap_err();
    assert!(
        matches!(&err, Error::Io { source, .. } if source.kind() == std::io::ErrorKind::AlreadyExists),
        "{err:?}"
    );
    assert_eq!(fs::read_to_string(&p).unwrap(), "first");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1, "no temp file");
}

#[test]
fn write_atomic_replace_refuses_when_the_file_changed_since_it_was_read() {
    use iprune_core::playlist::{WriteMode, mtime_ns, write_atomic};
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.m3u8");
    fs::write(&p, "mine").unwrap();
    let read_at = mtime_ns(&p).unwrap();
    // Someone else saves the file after we read it.
    fs::write(&p, "theirs").unwrap();
    touch_future(&p, 50);

    let err = write_atomic(&p, "overwrite", WriteMode::Replace { mtime_ns: read_at }).unwrap_err();
    assert!(
        matches!(&err, Error::ChangedOnDisk(q) if *q == p),
        "{err:?}"
    );
    assert_eq!(fs::read_to_string(&p).unwrap(), "theirs");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1, "no temp file");

    // With the current mtime it goes through.
    let now = mtime_ns(&p).unwrap();
    write_atomic(&p, "ok", WriteMode::Replace { mtime_ns: now }).unwrap();
    assert_eq!(fs::read_to_string(&p).unwrap(), "ok");
}

#[test]
fn rename_never_replaces_an_unindexed_existing_target() {
    let (env, t) = env3();
    let conn = env.store.conn();
    playlists::create(conn, &env.cfg, "src").unwrap();
    playlists::add(conn, &env.cfg, "src", &[t[0].clone()], None).unwrap();
    let src_text = env.read_playlist("src");
    // Target exists on disk but the index has never seen it.
    env.write_playlist("dst", "#EXTM3U\n/precious.mp3\n");

    let err = playlists::rename(conn, &env.cfg, "src", "dst").unwrap_err();
    assert!(
        matches!(err, Error::PlaylistExists(ref n) if n == "dst"),
        "{err:?}"
    );
    assert_eq!(env.read_playlist("dst"), "#EXTM3U\n/precious.mp3\n");
    assert_eq!(env.read_playlist("src"), src_text);
    assert!(env.playlist_id("src").is_some());
}

#[test]
fn rename_leaves_a_single_file_with_one_link() {
    use std::os::unix::fs::MetadataExt;
    let (env, t) = env3();
    let conn = env.store.conn();
    playlists::create(conn, &env.cfg, "src").unwrap();
    playlists::add(conn, &env.cfg, "src", &[t[0].clone()], None).unwrap();
    playlists::rename(conn, &env.cfg, "src", "dst").unwrap();
    assert_eq!(fs::read_dir(&env.lists).unwrap().count(), 1);
    assert_eq!(fs::metadata(env.playlist_file("dst")).unwrap().nlink(), 1);
}
