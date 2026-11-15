//! The inbox and the import state (technical document, section 7.2,
//! "The inbox" and "State"): `read_inbox`, `add_to_inbox`, the setters and
//! `inbox_dir`. No autotag is started; every test has its own temp inbox and
//! index.

mod common;

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::import_env::{Imp, answer, candidate, files_under};
use common::{Kind, chmod_000_blocks_reads, fairplay_m4a, fixture};
use iprune_core::Error;
use iprune_core::import::{self, Fields, Query};
use iprune_core::tags::ReadError;

fn bump_mtime(p: &Path) {
    let f = fs::File::options().write(true).open(p).unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(120))
        .unwrap();
}

fn names(imp: &Imp) -> Vec<String> {
    imp.read()
        .entries
        .iter()
        .map(|e| {
            e.path
                .strip_prefix(&imp.inbox)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

fn rows(imp: &Imp) -> Vec<String> {
    import::items(imp.env.store.conn())
        .unwrap()
        .into_iter()
        .map(|i| {
            i.path
                .strip_prefix(&imp.inbox)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

fn lookup_text() -> String {
    answer(vec![candidate("r1", "Title2", "Artist2", true, "A/t.mp3")]).to_string()
}

// -- read_inbox --------------------------------------------------------------

#[test]
fn the_inbox_is_created_when_missing() {
    let imp = Imp::new();
    assert!(!imp.inbox.exists(), "set up");
    let inbox = imp.read();
    assert!(imp.inbox.is_dir());
    assert!(inbox.entries.is_empty() && inbox.unreadable.is_empty());
    assert_eq!(inbox.dir, imp.inbox);
    assert_eq!(import::inbox_dir(&imp.env.cfg).unwrap(), imp.inbox);
}

#[test]
fn a_new_audio_file_gets_a_row_with_its_size_time_and_hash() {
    let imp = Imp::new();
    let p = imp.put("a.mp3", 2);
    let inbox = imp.read();
    assert_eq!(inbox.entries.len(), 1);
    let e = &inbox.entries[0];
    let item = e.item.as_ref().expect("a row");
    let meta = fs::metadata(&p).unwrap();
    assert_eq!(item.path, p);
    assert_eq!(item.size, meta.len());
    assert_eq!(item.hash, *blake3::hash(&fs::read(&p).unwrap()).as_bytes());
    assert!(item.mtime_ns > 0 && item.added_at > 0);
    assert_eq!(
        (
            &item.lookup,
            &item.search,
            item.choice,
            &item.tags,
            item.confirmed,
            &item.error
        ),
        (&None, &None, None, &None, false, &None)
    );
    // What tags::read gave.
    let file = e.file.as_ref().unwrap();
    assert_eq!(file.tags.title.as_deref(), Some("Title2"));
    // And the row is in the index, once.
    assert_eq!(
        import::items(imp.env.store.conn()).unwrap(),
        vec![item.clone()]
    );
}

#[test]
fn entries_come_in_path_order_whatever_the_order_they_were_made_in() {
    let imp = Imp::new();
    for (rel, n) in [
        ("b.mp3", 2),
        ("sub/z.mp3", 3),
        ("a-b.mp3", 4),
        ("a/x.mp3", 5),
        ("a.mp3", 6),
        ("Zed/y.mp3", 2),
    ] {
        imp.put(rel, n);
    }
    let got: Vec<PathBuf> = imp.read().entries.into_iter().map(|e| e.path).collect();
    let mut sorted = got.clone();
    sorted.sort();
    assert_eq!(got, sorted);
    assert_eq!(got.len(), 6);
    // Reading again, with rows existing, gives the same order.
    let again: Vec<PathBuf> = imp.read().entries.into_iter().map(|e| e.path).collect();
    assert_eq!(again, got);
    // The index lists its rows by path as text.
    let mut as_text = names(&imp);
    as_text.sort();
    assert_eq!(rows(&imp), as_text);
}

#[test]
fn a_file_that_did_not_change_keeps_its_row_and_everything_stored_in_it() {
    let imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.read();
    let id = imp.item_id("a.mp3");
    let conn = imp.env.store.conn();
    // The search first: Search again starts a file over.
    import::look_up_again(
        conn,
        id,
        Some(&Query {
            artist: "A".into(),
            title: "T".into(),
        }),
    )
    .unwrap();
    import::set_lookup(conn, id, &lookup_text()).unwrap();
    import::set_choice(conn, id, Some(0)).unwrap();
    let typed = Fields {
        title: Some("Typed".into()),
        ..Fields::default()
    };
    import::set_tags(conn, id, Some(&typed)).unwrap();
    import::set_confirmed(conn, id, true).unwrap();
    import::set_error(conn, id, Some("an earlier filing error")).unwrap();
    let before = imp.item("a.mp3");

    imp.read();
    imp.read();
    assert_eq!(imp.item("a.mp3"), before);
    assert_eq!(before.id, id);
    assert!(before.lookup.is_some() && before.search.is_some() && before.tags.is_some());
    assert!(before.choice.is_some() && before.confirmed && before.error.is_some());
}

#[test]
fn search_again_and_look_up_again_start_the_file_over_and_keep_only_the_new_search() {
    let imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.put("b.mp3", 3);
    imp.read();
    decorate(&imp, "a.mp3");
    decorate(&imp, "b.mp3");
    let conn = imp.env.store.conn();
    let (a, b) = (imp.item_id("a.mp3"), imp.item_id("b.mp3"));
    let q = Query {
        artist: "New".into(),
        title: "Search".into(),
    };
    import::look_up_again(conn, a, Some(&q)).unwrap();
    let it = imp.item("a.mp3");
    assert_eq!(it.search, Some(q));
    assert_eq!(
        (it.lookup, it.choice, it.tags, it.confirmed, it.error),
        (None, None, None, false, None)
    );
    // The file itself is the same row: size, time and hash are kept.
    assert_eq!(it.id, a);

    // Look up again with no search drops the old search too.
    import::look_up_again(conn, b, None).unwrap();
    let it = imp.item("b.mp3");
    assert_eq!(it.search, None);
    assert_eq!(
        (it.lookup, it.choice, it.tags, it.confirmed, it.error),
        (None, None, None, false, None)
    );
}

/// Gives the row of `rel` every kind of state a user or a lookup can leave.
fn decorate(imp: &Imp, rel: &str) {
    let id = imp.item_id(rel);
    let conn = imp.env.store.conn();
    import::set_lookup(conn, id, &lookup_text()).unwrap();
    import::set_choice(conn, id, Some(0)).unwrap();
    import::set_tags(
        conn,
        id,
        Some(&Fields {
            title: Some("Typed".into()),
            ..Fields::default()
        }),
    )
    .unwrap();
    import::set_confirmed(conn, id, true).unwrap();
    conn.execute(
        "UPDATE import_items SET search = '{\"artist\":\"A\",\"title\":\"T\"}' WHERE id = ?1",
        [id],
    )
    .unwrap();
    import::set_error(conn, id, Some("old error")).unwrap();
    let it = imp.item(rel);
    assert!(
        it.lookup.is_some()
            && it.search.is_some()
            && it.choice.is_some()
            && it.tags.is_some()
            && it.confirmed
            && it.error.is_some(),
        "set up: {it:?}"
    );
}

#[test]
fn a_file_whose_size_changed_starts_over_and_is_hashed_again() {
    let imp = Imp::new();
    let p = imp.put("a.mp3", 2);
    imp.read();
    decorate(&imp, "a.mp3");
    let old = imp.item("a.mp3");

    let mut bytes = fs::read(&p).unwrap();
    bytes.extend_from_slice(b"padding");
    fs::write(&p, &bytes).unwrap();
    imp.read();

    let it = imp.item("a.mp3");
    assert_eq!(it.size, bytes.len() as u64);
    assert_eq!(it.hash, *blake3::hash(&bytes).as_bytes());
    assert_ne!(it.hash, old.hash);
    assert_eq!(
        (
            &it.lookup,
            &it.search,
            it.choice,
            &it.tags,
            it.confirmed,
            &it.error
        ),
        (&None, &None, None, &None, false, &None)
    );
    assert_eq!(rows(&imp), ["a.mp3"], "still one row, not two");
}

#[test]
fn a_file_whose_modification_time_changed_starts_over_even_when_the_bytes_are_the_same() {
    let imp = Imp::new();
    let p = imp.put("a.mp3", 2);
    imp.read();
    decorate(&imp, "a.mp3");
    let old = imp.item("a.mp3");

    bump_mtime(&p);
    imp.read();

    let it = imp.item("a.mp3");
    assert_ne!(it.mtime_ns, old.mtime_ns, "the new time is recorded");
    assert_eq!(it.hash, old.hash);
    assert!(it.lookup.is_none() && it.search.is_none() && it.choice.is_none());
    assert!(it.tags.is_none() && !it.confirmed && it.error.is_none());
}

#[test]
fn the_row_of_a_file_that_left_the_inbox_is_dropped_and_the_others_stay() {
    let imp = Imp::new();
    let gone = imp.put("a.mp3", 2);
    imp.put("sub/b.mp3", 3);
    imp.put("sub/c.mp3", 4);
    imp.read();
    assert_eq!(rows(&imp).len(), 3);

    fs::remove_file(&gone).unwrap();
    imp.read();
    assert_eq!(rows(&imp), ["sub/b.mp3", "sub/c.mp3"]);

    // A whole folder moved away by hand.
    fs::rename(imp.inbox.join("sub"), imp.base.join("elsewhere")).unwrap();
    imp.read();
    assert!(rows(&imp).is_empty());
}

#[test]
fn a_file_under_a_folder_that_cannot_be_read_keeps_its_row() {
    let imp = Imp::new();
    imp.put("locked/a.mp3", 2);
    imp.put("open/b.mp3", 3);
    let gone = imp.put("open/c.mp3", 4);
    imp.read();
    assert_eq!(rows(&imp).len(), 3);
    let locked = imp.inbox.join("locked");
    if !chmod_000_blocks_reads(&locked) {
        eprintln!("skipping: permissions are not enforced (running as root?)");
        return;
    }
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    fs::remove_file(&gone).unwrap();
    let inbox = imp.read();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        inbox.unreadable.iter().any(|(p, _)| *p == locked),
        "{:?}",
        inbox.unreadable
    );
    // The file under the locked folder may still be there; the one deleted
    // in a folder that could be read is gone.
    assert_eq!(rows(&imp), ["locked/a.mp3", "open/b.mp3"]);
    // Readable again: unchanged, so its state is intact.
    let inbox = imp.read();
    assert!(inbox.unreadable.is_empty());
    assert_eq!(rows(&imp), ["locked/a.mp3", "open/b.mp3"]);
}

#[test]
fn hidden_files_and_folders_are_skipped_and_get_no_row() {
    let imp = Imp::new();
    imp.put("visible.mp3", 2);
    imp.put(".hidden.mp3", 3);
    imp.put(".autotag-123.mp3", 4);
    imp.put(".folder/inside.mp3", 5);
    imp.put("sub/.iprune-abc.tmp", 6);
    fs::write(imp.inbox.join(".DS_Store"), b"x").unwrap();
    let inbox = imp.read();
    assert_eq!(names(&imp), ["visible.mp3"]);
    assert_eq!(rows(&imp), ["visible.mp3"]);
    assert!(inbox.unreadable.is_empty());
}

#[test]
fn symbolic_links_are_not_followed_and_are_reported() {
    let imp = Imp::new();
    imp.put("real.mp3", 2);
    let outside = imp.base.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::copy(fixture(Kind::Mp3, 3), outside.join("o.mp3")).unwrap();
    std::os::unix::fs::symlink(outside.join("o.mp3"), imp.inbox.join("link.mp3")).unwrap();
    std::os::unix::fs::symlink(&outside, imp.inbox.join("linked-folder")).unwrap();
    let inbox = imp.read();
    assert_eq!(names(&imp), ["real.mp3"]);
    assert_eq!(rows(&imp), ["real.mp3"]);
    let mut reported: Vec<_> = inbox
        .unreadable
        .iter()
        .map(|(p, why)| {
            (
                p.file_name().unwrap().to_str().unwrap().to_owned(),
                why.clone(),
            )
        })
        .collect();
    reported.sort();
    assert_eq!(reported.len(), 2, "{reported:?}");
    assert_eq!(reported[0].0, "link.mp3");
    assert_eq!(reported[1].0, "linked-folder");
    assert!(
        reported
            .iter()
            .all(|(_, why)| why.contains("symbolic link"))
    );
    // The target was not touched.
    assert!(outside.join("o.mp3").is_file());
}

#[test]
fn files_that_cannot_be_imported_are_listed_as_not_supported_with_no_row() {
    let imp = Imp::new();
    imp.put("good.mp3", 2);
    fs::write(imp.inbox.join("notes.txt"), b"text").unwrap();
    fs::write(imp.inbox.join("README"), b"text").unwrap();
    fs::write(imp.inbox.join("song.flac"), b"fLaC").unwrap();
    let bad = imp.inbox.join(OsStr::from_bytes(b"bad\xff.mp3"));
    fs::copy(fixture(Kind::Mp3, 3), &bad).unwrap();

    let inbox = imp.read();
    assert_eq!(
        rows(&imp),
        ["good.mp3"],
        "only the audio it can import has a row"
    );
    let by_name = |n: &OsStr| {
        inbox
            .entries
            .iter()
            .find(|e| e.path.file_name() == Some(n))
            .unwrap_or_else(|| panic!("{n:?} is listed"))
    };
    for (name, fragment) in [
        ("notes.txt", "txt"),
        ("README", "not an audio file"),
        ("song.flac", "flac"),
    ] {
        let e = by_name(OsStr::new(name));
        assert!(e.item.is_none(), "{name}");
        assert!(
            matches!(&e.file, Err(ReadError::Unsupported(why)) if why.contains(fragment)),
            "{name}: {:?}",
            e.file
        );
    }
    let e = by_name(bad.file_name().unwrap());
    assert!(e.item.is_none());
    assert!(
        matches!(&e.file, Err(ReadError::Unsupported(why)) if why.contains("UTF-8")),
        "{:?}",
        e.file
    );
    assert!(bad.exists(), "listed, not touched");
}

#[test]
fn the_extension_is_read_in_lowercase_and_a_name_is_kept_exactly_as_on_disk() {
    let imp = Imp::new();
    let upper = imp.put("LOUD.MP3", 2);
    let nfd = imp.put("Jo\u{301}hann.mp3", 3);
    imp.read();
    let mut paths: Vec<PathBuf> = import::items(imp.env.store.conn())
        .unwrap()
        .into_iter()
        .map(|i| i.path)
        .collect();
    paths.sort();
    let mut want = vec![upper, nfd];
    want.sort();
    assert_eq!(paths, want, "NFD kept as is");
}

#[test]
fn a_file_with_an_audio_extension_that_cannot_be_read_has_a_row_and_an_error() {
    let imp = Imp::new();
    fs::create_dir_all(&imp.inbox).unwrap();
    fs::write(imp.inbox.join("broken.mp3"), b"this is not audio at all").unwrap();
    let protected = imp.inbox.join("store.m4a");
    fs::write(&protected, fairplay_m4a(2)).unwrap();
    let inbox = imp.read();
    assert_eq!(rows(&imp), ["broken.mp3", "store.m4a"]);
    let broken = &inbox.entries[0];
    assert!(broken.item.is_some());
    assert!(
        matches!(broken.file, Err(ReadError::Unreadable(_))),
        "{:?}",
        broken.file
    );
    let drm = &inbox.entries[1];
    assert!(drm.item.is_some());
    assert!(drm.file.as_ref().unwrap().drm);
}

// -- Setters -----------------------------------------------------------------

#[test]
fn each_setter_changes_its_field_and_only_that_one() {
    let imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.put("b.mp3", 3);
    imp.read();
    let (a, b) = (imp.item_id("a.mp3"), imp.item_id("b.mp3"));
    let conn = imp.env.store.conn();
    let other = imp.item("b.mp3");

    import::set_error(conn, a, Some("boom")).unwrap();
    assert_eq!(imp.item("a.mp3").error.as_deref(), Some("boom"));
    import::set_error(conn, a, None).unwrap();
    assert_eq!(imp.item("a.mp3").error, None);

    import::set_choice(conn, a, Some(3)).unwrap();
    assert_eq!(imp.item("a.mp3").choice, Some(3));
    import::set_choice(conn, a, None).unwrap();
    assert_eq!(imp.item("a.mp3").choice, None);

    let typed = Fields {
        title: Some("T".into()),
        year: Some(1999),
        compilation: Some(true),
        ..Fields::default()
    };
    import::set_tags(conn, a, Some(&typed)).unwrap();
    assert_eq!(imp.item("a.mp3").tags, Some(typed));
    import::set_tags(conn, a, None).unwrap();
    assert_eq!(imp.item("a.mp3").tags, None);

    import::set_confirmed(conn, a, true).unwrap();
    assert!(imp.item("a.mp3").confirmed);
    import::set_confirmed(conn, a, false).unwrap();
    assert!(!imp.item("a.mp3").confirmed);

    assert_eq!(imp.item("b.mp3"), other, "the other row is untouched");
    let _ = b;
}

#[test]
fn a_successful_lookup_clears_the_error_and_search_again_clears_the_lookup_choice_and_error() {
    let imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.read();
    let id = imp.item_id("a.mp3");
    let conn = imp.env.store.conn();

    import::set_error(conn, id, Some("network")).unwrap();
    import::set_lookup(conn, id, &lookup_text()).unwrap();
    let it = imp.item("a.mp3");
    assert_eq!(
        it.lookup.as_deref(),
        Some(lookup_text().as_str()),
        "stored as it came"
    );
    assert_eq!(it.error, None);

    import::set_choice(conn, id, Some(0)).unwrap();
    import::set_error(conn, id, Some("filing failed")).unwrap();
    let q = Query {
        artist: "Someone".into(),
        title: "Something".into(),
    };
    import::look_up_again(conn, id, Some(&q)).unwrap();
    let it = imp.item("a.mp3");
    assert_eq!(it.search, Some(q));
    assert_eq!((it.lookup, it.choice, it.error), (None, None, None));

    // Look up again with no search goes back to the file's own tags.
    import::look_up_again(conn, id, None).unwrap();
    assert_eq!(imp.item("a.mp3").search, None);
}

#[test]
fn candidate_tags_go_into_the_stored_lookup_for_that_candidate_only() {
    let imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.read();
    let id = imp.item_id("a.mp3");
    let conn = imp.env.store.conn();
    let mut second = candidate("r2", "Other", "Artist2", false, "B/o.mp3");
    second["tags"]["genre"] = serde_json::Value::Null;
    let text = answer(vec![
        candidate("r1", "Title2", "Artist2", true, "A/t.mp3"),
        second,
    ])
    .to_string();
    import::set_lookup(conn, id, &text).unwrap();
    let full = Fields {
        title: Some("Other".into()),
        artist: Some("Artist2".into()),
        genre: Some("Live Rock".into()),
        year: Some(2001),
        ..Fields::default()
    };
    import::set_candidate_tags(conn, id, 1, &full).unwrap();

    let l = import::Lookup::parse(imp.item("a.mp3").lookup.as_deref().unwrap()).unwrap();
    assert_eq!(l.candidates[1].tags, full);
    assert!(!l.candidates[1].genre_unknown, "the genre is known now");
    assert_eq!(
        l.candidates[1].destination.as_deref(),
        Some("B/o.mp3"),
        "the rest of it stays"
    );
    assert_eq!(l.candidates[0].tags.genre, None);
    assert_eq!(l.candidates[0].recording, "r1");

    // A candidate that is not there, or a row with no lookup, changes nothing.
    let before = imp.item("a.mp3");
    import::set_candidate_tags(conn, id, 5, &full).unwrap();
    assert_eq!(imp.item("a.mp3"), before);
    import::set_candidate_tags(conn, 9999, 0, &full).unwrap();
}

#[test]
fn candidate_tags_for_a_row_with_no_lookup_or_an_unreadable_one() {
    let imp = Imp::new();
    imp.put("a.mp3", 2);
    imp.read();
    let id = imp.item_id("a.mp3");
    let conn = imp.env.store.conn();
    let tags = Fields {
        genre: Some("Pop".into()),
        ..Fields::default()
    };
    import::set_candidate_tags(conn, id, 0, &tags).unwrap();
    assert_eq!(imp.item("a.mp3").lookup, None, "nothing to put them in");

    import::set_lookup(conn, id, "not json").unwrap();
    let e = import::set_candidate_tags(conn, id, 0, &tags).unwrap_err();
    assert!(
        matches!(e, Error::Autotag(ref m) if m.contains("cannot be read")),
        "{e:?}"
    );
    assert_eq!(
        imp.item("a.mp3").lookup.as_deref(),
        Some("not json"),
        "left as it was"
    );
}

// -- add_to_inbox ------------------------------------------------------------

fn source_dir(imp: &Imp) -> PathBuf {
    let d = imp.base.join("source");
    fs::create_dir_all(&d).unwrap();
    d
}

fn inbox_files(imp: &Imp) -> Vec<String> {
    let mut v = Vec::new();
    if imp.inbox.exists() {
        files_under(&imp.inbox, &imp.inbox, &mut v);
    }
    v.sort();
    v
}

#[test]
fn a_file_is_copied_in_and_the_original_is_left_alone() {
    let imp = Imp::new();
    let src = source_dir(&imp).join("song.mp3");
    fs::copy(fixture(Kind::Mp3, 2), &src).unwrap();
    let bytes = fs::read(&src).unwrap();
    let mtime = fs::metadata(&src).unwrap().modified().unwrap();

    let got = import::add_to_inbox(&imp.env.cfg, std::slice::from_ref(&src)).unwrap();
    assert_eq!(got, vec![imp.inbox.join("song.mp3")]);
    assert_eq!(fs::read(&got[0]).unwrap(), bytes);
    assert_eq!(fs::read(&src).unwrap(), bytes);
    assert_eq!(fs::metadata(&src).unwrap().modified().unwrap(), mtime);
    assert_eq!(
        inbox_files(&imp),
        ["song.mp3"],
        "the hidden temporary file is gone"
    );
}

#[test]
fn a_name_already_taken_gets_a_number_and_nothing_is_ever_replaced() {
    let imp = Imp::new();
    fs::create_dir_all(&imp.inbox).unwrap();
    fs::write(imp.inbox.join("song.mp3"), b"first").unwrap();
    fs::write(imp.inbox.join("song (2).mp3"), b"second").unwrap();
    let src = source_dir(&imp).join("song.mp3");
    fs::write(&src, b"third").unwrap();

    let got = import::add_to_inbox(&imp.env.cfg, std::slice::from_ref(&src)).unwrap();
    assert_eq!(got, vec![imp.inbox.join("song (3).mp3")]);
    assert_eq!(fs::read(imp.inbox.join("song.mp3")).unwrap(), b"first");
    assert_eq!(fs::read(imp.inbox.join("song (2).mp3")).unwrap(), b"second");
    assert_eq!(fs::read(imp.inbox.join("song (3).mp3")).unwrap(), b"third");

    // The same source again: another copy, the next number.
    let got = import::add_to_inbox(&imp.env.cfg, &[src]).unwrap();
    assert_eq!(got, vec![imp.inbox.join("song (4).mp3")]);
    assert_eq!(
        inbox_files(&imp),
        ["song (2).mp3", "song (3).mp3", "song (4).mp3", "song.mp3"]
    );
}

#[test]
fn a_name_without_an_extension_is_numbered_at_its_end() {
    let imp = Imp::new();
    fs::create_dir_all(&imp.inbox).unwrap();
    fs::write(imp.inbox.join("track"), b"1").unwrap();
    let src = source_dir(&imp).join("track");
    fs::write(&src, b"2").unwrap();
    let got = import::add_to_inbox(&imp.env.cfg, &[src]).unwrap();
    assert_eq!(got, vec![imp.inbox.join("track (2)")]);
}

#[test]
fn several_sources_come_back_in_the_order_given() {
    let imp = Imp::new();
    let d = source_dir(&imp);
    let (b, a) = (d.join("b.mp3"), d.join("a.mp3"));
    fs::write(&b, b"b").unwrap();
    fs::write(&a, b"a").unwrap();
    let got = import::add_to_inbox(&imp.env.cfg, &[b, a]).unwrap();
    assert_eq!(got, vec![imp.inbox.join("b.mp3"), imp.inbox.join("a.mp3")]);
}

#[test]
fn a_folder_keeps_its_name_and_brings_its_audio_files_and_nothing_hidden() {
    let imp = Imp::new();
    let album = source_dir(&imp).join("Album");
    fs::create_dir_all(album.join("Disc 2")).unwrap();
    fs::create_dir_all(album.join(".hidden-dir")).unwrap();
    fs::create_dir_all(album.join("Artwork")).unwrap();
    fs::copy(fixture(Kind::Mp3, 2), album.join("01.mp3")).unwrap();
    fs::copy(fixture(Kind::Aac, 3), album.join("Disc 2/02.m4a")).unwrap();
    fs::write(album.join("03.FLAC"), b"fLaC").unwrap(); // listed as not supported later
    fs::write(album.join("cover.jpg"), b"jpg").unwrap();
    fs::write(album.join("notes.txt"), b"txt").unwrap();
    fs::write(album.join(".hidden.mp3"), b"h").unwrap();
    fs::write(album.join(".hidden-dir/inside.mp3"), b"h").unwrap();
    fs::write(album.join("Artwork/front.png"), b"png").unwrap();

    let got = import::add_to_inbox(&imp.env.cfg, std::slice::from_ref(&album)).unwrap();
    assert_eq!(
        inbox_files(&imp),
        ["Album/01.mp3", "Album/03.FLAC", "Album/Disc 2/02.m4a"]
    );
    let mut got_rel: Vec<_> = got
        .iter()
        .map(|p| p.strip_prefix(&imp.inbox).unwrap().to_owned())
        .collect();
    got_rel.sort();
    assert_eq!(
        got_rel,
        [
            PathBuf::from("Album/01.mp3"),
            PathBuf::from("Album/03.FLAC"),
            PathBuf::from("Album/Disc 2/02.m4a")
        ]
    );
    // The originals are all still there.
    assert!(album.join("cover.jpg").is_file() && album.join(".hidden.mp3").is_file());
    assert_eq!(
        fs::read(album.join("01.mp3")).unwrap(),
        fs::read(imp.inbox.join("Album/01.mp3")).unwrap()
    );
}

#[test]
fn a_folder_given_twice_does_not_replace_the_files_of_the_first_copy() {
    let imp = Imp::new();
    let album = source_dir(&imp).join("Album");
    fs::create_dir_all(&album).unwrap();
    fs::write(album.join("a.mp3"), b"v1").unwrap();
    import::add_to_inbox(&imp.env.cfg, std::slice::from_ref(&album)).unwrap();
    fs::write(album.join("a.mp3"), b"v2").unwrap();
    import::add_to_inbox(&imp.env.cfg, &[album]).unwrap();
    assert_eq!(inbox_files(&imp), ["Album/a (2).mp3", "Album/a.mp3"]);
    assert_eq!(fs::read(imp.inbox.join("Album/a.mp3")).unwrap(), b"v1");
    assert_eq!(fs::read(imp.inbox.join("Album/a (2).mp3")).unwrap(), b"v2");
}

#[test]
fn a_file_already_in_the_inbox_stays_where_it_is() {
    let imp = Imp::new();
    let inside = imp.put("sub/song.mp3", 2);
    let got = import::add_to_inbox(&imp.env.cfg, std::slice::from_ref(&inside)).unwrap();
    assert_eq!(got, vec![inside.clone()]);
    assert_eq!(inbox_files(&imp), ["sub/song.mp3"]);

    // Also when reached through a symbolic link to the inbox.
    let via = imp.base.join("via-link");
    std::os::unix::fs::symlink(&imp.inbox, &via).unwrap();
    let got = import::add_to_inbox(&imp.env.cfg, &[via.join("sub/song.mp3")]).unwrap();
    assert_eq!(got, vec![via.join("sub/song.mp3")]);
    assert_eq!(inbox_files(&imp), ["sub/song.mp3"]);
}

#[test]
fn a_missing_source_is_an_error_naming_it_and_copies_nothing() {
    let imp = Imp::new();
    let missing = source_dir(&imp).join("absent.mp3");
    let e = import::add_to_inbox(&imp.env.cfg, std::slice::from_ref(&missing)).unwrap_err();
    assert!(
        matches!(&e, Error::Io { path, .. } if *path == missing),
        "{e:?}"
    );
    assert!(inbox_files(&imp).is_empty());
}

#[test]
fn a_failed_copy_leaves_no_hidden_temporary_file() {
    let imp = Imp::new();
    let d = source_dir(&imp);
    let ok = d.join("ok.mp3");
    fs::write(&ok, b"ok").unwrap();
    let unreadable = d.join("locked.mp3");
    fs::write(&unreadable, b"no").unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::File::open(&unreadable).is_ok() {
        eprintln!("skipping: permissions are not enforced (running as root?)");
        return;
    }
    let result = import::add_to_inbox(&imp.env.cfg, &[ok, unreadable.clone()]);
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        matches!(&result, Err(Error::Io { path, .. }) if *path == unreadable),
        "{result:?}"
    );
    let left = inbox_files(&imp);
    assert!(
        left.iter().all(|n| !n.starts_with('.')),
        "no temporary file is left: {left:?}"
    );
}

#[test]
fn a_folder_that_holds_the_inbox_does_not_bring_the_inbox_into_itself() {
    let mut imp = Imp::new();
    let top = imp.base.join("top");
    let inbox = top.join("Inbox");
    imp.env.cfg.import.inbox = inbox.clone();
    fs::create_dir_all(top.join("sub")).unwrap();
    fs::create_dir_all(&inbox).unwrap();
    fs::copy(fixture(Kind::Mp3, 2), top.join("one.mp3")).unwrap();
    fs::copy(fixture(Kind::Mp3, 3), top.join("sub/two.mp3")).unwrap();
    fs::copy(fixture(Kind::Mp3, 4), inbox.join("already.mp3")).unwrap();
    fs::copy(fixture(Kind::Mp3, 5), inbox.join("also.mp3")).unwrap();
    let list = |inbox: &Path| {
        let mut v = Vec::new();
        files_under(inbox, inbox, &mut v);
        v.sort();
        v
    };

    let got = import::add_to_inbox(&imp.env.cfg, std::slice::from_ref(&top)).unwrap();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(
        list(&inbox),
        ["already.mp3", "also.mp3", "top/one.mp3", "top/sub/two.mp3"],
        "the folder's own files came; the inbox's were not copied again"
    );

    // Again: the folder's files are copied once more under new names (a name
    // taken gets a number), and still nothing of the inbox, so no nesting.
    let got = import::add_to_inbox(&imp.env.cfg, &[top]).unwrap();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(
        list(&inbox),
        [
            "already.mp3",
            "also.mp3",
            "top/one (2).mp3",
            "top/one.mp3",
            "top/sub/two (2).mp3",
            "top/sub/two.mp3"
        ]
    );
}

// The window's worker and `iprune import` read the inbox at the same time:
// both see no row for a new file, and both insert it. The second must keep
// the first's row, not fail on the duplicate path.
#[test]
fn two_readers_of_the_same_new_inbox_both_succeed_and_share_one_row_per_file() {
    use std::sync::Barrier;

    let imp = Imp::new();
    for n in 1..=6 {
        imp.put(&format!("f{n}.mp3"), n);
    }
    let db = imp.env.music.parent().unwrap().join("db/iprune.sqlite");
    for round in 0..10 {
        imp.env
            .store
            .conn()
            .execute("DELETE FROM import_items", [])
            .unwrap();
        let barrier = Barrier::new(2);
        let read = || {
            let store = iprune_core::store::Store::open(&db).unwrap();
            barrier.wait();
            import::read_inbox(store.conn(), &imp.env.cfg).map(|inbox| {
                inbox
                    .entries
                    .iter()
                    .map(|e| (e.path.clone(), e.item.as_ref().unwrap().id))
                    .collect::<Vec<_>>()
            })
        };
        let (a, b) = std::thread::scope(|s| {
            let a = s.spawn(read);
            let b = s.spawn(read);
            (a.join().unwrap(), b.join().unwrap())
        });
        let (a, b) = (
            a.unwrap_or_else(|e| panic!("round {round}, first reader: {e}")),
            b.unwrap_or_else(|e| panic!("round {round}, second reader: {e}")),
        );
        assert_eq!(a, b, "round {round}: both readers name the same rows");
        assert_eq!(a.len(), 6);
        let items = import::items(imp.env.store.conn()).unwrap();
        assert_eq!(items.len(), 6, "round {round}: one row per file");
        assert_eq!(
            items.iter().map(|i| i.path.clone()).collect::<Vec<_>>(),
            a.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>()
        );
    }
}
