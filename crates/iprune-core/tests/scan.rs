mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use common::{Env, Kind, chmod_000_blocks_reads, fixture};
use iprune_core::Error;
use iprune_core::tags::FileKind;

const NFC: &str = "J\u{f3}hann.mp3";
const NFD: &str = "Jo\u{301}hann.mp3";

fn bump_mtime(p: &std::path::Path) {
    let f = fs::File::options().write(true).open(p).unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(120))
        .unwrap();
}

fn only_track_ids(env: &Env) -> Vec<(i64, PathBuf, bool)> {
    env.tracks()
        .into_iter()
        .map(|t| (t.id, t.path, t.missing_since.is_some()))
        .collect()
}

#[test]
fn first_scan_adds_everything_and_second_finds_nothing_changed() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.add("Album/b.m4a", Kind::Aac, 2);

    let r = env.scan();
    assert_eq!((r.added, r.unchanged, r.updated), (2, 0, 0));
    let t = env.track(&a);
    assert_eq!(t.tags.title.as_deref(), Some("Title1"));
    assert_eq!(t.tags.artist.as_deref(), Some("Artist1"));
    assert_eq!(t.info.kind, FileKind::Mp3);
    assert_eq!(t.size, fs::metadata(&a).unwrap().len());
    assert_eq!(t.hash, *blake3::hash(&fs::read(&a).unwrap()).as_bytes());

    let ids = only_track_ids(&env);
    let r = env.scan();
    assert_eq!((r.added, r.unchanged, r.updated), (0, 2, 0));
    assert!(r.moved.is_empty() && r.missing.is_empty() && r.unreadable.is_empty());
    assert_eq!(only_track_ids(&env), ids);
}

#[test]
fn each_supported_format_gets_the_right_kind() {
    let mut env = Env::new();
    let cases = [
        ("a.mp3", Kind::Mp3, FileKind::Mp3),
        ("b.m4a", Kind::Aac, FileKind::Aac),
        ("c.m4a", Kind::Alac, FileKind::Alac),
        ("d.wav", Kind::Wav, FileKind::Wav),
        ("e.aiff", Kind::Aiff, FileKind::Aiff),
    ];
    for (i, (name, kind, _)) in cases.iter().enumerate() {
        env.add(name, *kind, i as u32);
    }
    let r = env.scan();
    assert_eq!(r.added, 5);
    assert!(r.unsupported.is_empty() && r.unreadable.is_empty());
    for (name, _, expected) in cases {
        assert_eq!(
            env.track(&env.music.join(name)).info.kind,
            expected,
            "{name}"
        );
    }
}

#[test]
fn uppercase_extension_is_indexed() {
    let mut env = Env::new();
    env.add("LOUD.MP3", Kind::Mp3, 1);
    assert_eq!(env.scan().added, 1);
}

#[test]
fn rewritten_file_is_updated_with_same_id() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    let before = env.track(&a);

    fs::copy(fixture(Kind::Mp3, 2), &a).unwrap();
    let r = env.scan();
    assert_eq!((r.updated, r.added, r.unchanged), (1, 0, 0));
    let after = env.track(&a);
    assert_eq!(after.id, before.id);
    assert_ne!(after.hash, before.hash);
    assert_eq!(after.tags.title.as_deref(), Some("Title2"));
    assert_eq!(env.tracks().len(), 1);
}

#[test]
fn touched_file_with_same_content_is_reread_not_added() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    let before = env.track(&a);
    bump_mtime(&a);
    let r = env.scan();
    assert_eq!((r.updated, r.added, r.moved.len()), (1, 0, 0));
    let after = env.track(&a);
    assert_eq!(after.id, before.id);
    assert_ne!(after.mtime_ns, before.mtime_ns);
    // and it settles again
    assert_eq!(env.scan().unchanged, 1);
}

#[test]
fn renamed_file_keeps_its_track_id_and_playlists_follow() {
    let mut env = Env::new();
    let old = env.add("a.mp3", Kind::Mp3, 1);
    let other = env.add("b.mp3", Kind::Mp3, 2);
    env.scan();
    let id = env.track(&old).id;
    env.write_playlist(
        "mix",
        &format!(
            "#EXTM3U\n#EXTINF:1,Artist1 - Title1\n{}\n{}\n/elsewhere/ghost.mp3\n",
            old.display(),
            other.display()
        ),
    );
    env.write_playlist("untouched", &format!("#EXTM3U\n{}\n", other.display()));
    let untouched_before = env.read_playlist("untouched");
    env.scan();

    let new = env.music.join("Renamed/a2.mp3");
    fs::create_dir(env.music.join("Renamed")).unwrap();
    fs::rename(&old, &new).unwrap();
    let r = env.scan();

    assert_eq!(r.moved, vec![(old.clone(), new.clone())]);
    assert_eq!((r.added, r.missing.len()), (0, 0));
    assert_eq!(r.rewritten_playlists, vec!["mix".to_string()]);
    let t = env.track(&new);
    assert_eq!(t.id, id);
    assert!(t.missing_since.is_none());
    assert_eq!(env.tracks().len(), 2, "no duplicate row for the old path");
    assert_eq!(
        env.locations("mix"),
        vec![
            new.to_str().unwrap().to_owned(),
            other.to_str().unwrap().to_owned(),
            "/elsewhere/ghost.mp3".to_owned()
        ]
    );
    assert!(!env.read_playlist("mix").contains("a.mp3"));
    assert_eq!(env.read_playlist("untouched"), untouched_before);
    // The index agrees: first entry now resolves to the same track id.
    let items = env.indexed_items("mix");
    assert_eq!(items[0].1, Some(id));
    assert_eq!(items[2].1, None);
}

#[test]
fn moved_file_rewrites_relative_playlist_entries() {
    let mut env = Env::new().relative_playlists();
    let old = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    env.write_playlist("rel", "#EXTM3U\n../music/a.mp3\n");
    env.scan();
    let new = env.music.join("sub/a.mp3");
    fs::create_dir(env.music.join("sub")).unwrap();
    fs::rename(&old, &new).unwrap();
    let r = env.scan();
    assert_eq!(r.rewritten_playlists, vec!["rel".to_string()]);
    assert_eq!(env.locations("rel"), vec!["../music/sub/a.mp3".to_string()]);
}

#[test]
fn deleted_file_is_marked_missing_and_comes_back_with_same_id() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.add("b.mp3", Kind::Mp3, 2);
    env.scan();
    let id = env.track(&a).id;
    env.write_playlist("p", &format!("#EXTM3U\n{}\n", a.display()));
    env.scan();
    let saved = fs::read(&a).unwrap();
    fs::remove_file(&a).unwrap();

    let r = env.scan();
    assert_eq!(r.missing, vec![a.clone()]);
    assert_eq!(r.unchanged, 1);
    let t = env.track(&a);
    assert_eq!(t.id, id);
    assert!(t.missing_since.is_some());
    // The playlist file keeps the line, the index no longer resolves it.
    assert_eq!(env.locations("p"), vec![a.to_str().unwrap().to_owned()]);
    assert_eq!(env.indexed_items("p")[0].1, None);
    assert_eq!(r.playlists.unresolved.len(), 1);

    // A second scan does not report it missing again.
    assert!(env.scan().missing.is_empty());

    fs::write(&a, saved).unwrap();
    let r = env.scan();
    assert_eq!((r.returned, r.added, r.updated), (1, 0, 0));
    let t = env.track(&a);
    assert_eq!(t.id, id);
    assert!(t.missing_since.is_none());
    assert_eq!(env.indexed_items("p")[0].1, Some(id));
    assert_eq!(env.tracks().len(), 2);
}

#[test]
fn two_new_copies_of_a_vanished_file_are_not_a_move() {
    let mut env = Env::new();
    let old = env.add("a.mp3", Kind::Mp3, 1);
    env.add("keep.mp3", Kind::Mp3, 9);
    env.scan();
    let id = env.track(&old).id;
    env.write_playlist("p", &format!("#EXTM3U\n{}\n", old.display()));
    env.scan();
    let playlist_before = env.read_playlist("p");

    fs::remove_file(&old).unwrap();
    let c1 = env.add("copy1.mp3", Kind::Mp3, 1);
    let c2 = env.add("copy2.mp3", Kind::Mp3, 1);
    let r = env.scan();

    assert!(r.moved.is_empty());
    assert_eq!((r.added, r.missing.clone()), (2, vec![old.clone()]));
    assert!(r.rewritten_playlists.is_empty());
    assert!(env.track(&old).missing_since.is_some());
    assert_eq!(env.track(&old).id, id);
    assert_ne!(env.track(&c1).id, id);
    assert_ne!(env.track(&c2).id, id);
    assert_eq!(env.read_playlist("p"), playlist_before);
}

#[test]
fn two_vanished_duplicates_and_one_new_copy_is_not_a_move() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    let b = env.add("b.mp3", Kind::Mp3, 1);
    env.add("keep.mp3", Kind::Mp3, 9);
    env.scan();
    fs::remove_file(&a).unwrap();
    fs::remove_file(&b).unwrap();
    let c = env.add("c.mp3", Kind::Mp3, 1);
    let r = env.scan();
    assert!(r.moved.is_empty());
    assert_eq!(r.added, 1);
    assert_eq!(r.missing.len(), 2);
    assert!(env.track(&a).missing_since.is_some());
    assert!(env.track(&b).missing_since.is_some());
    assert!(env.track(&c).missing_since.is_none());
}

#[test]
fn garbage_mp3_is_reported_and_not_added() {
    let mut env = Env::new();
    env.add("ok.mp3", Kind::Mp3, 1);
    let bad = env.music.join("bad.mp3");
    fs::write(&bad, b"this is not audio at all, just text").unwrap();
    let r = env.scan();
    assert_eq!(r.added, 1);
    assert_eq!(r.unreadable.len(), 1);
    assert_eq!(r.unreadable[0].0, bad);
    assert!(env.tracks().iter().all(|t| t.path != bad));
}

#[test]
fn indexed_file_that_turns_unreadable_is_not_marked_missing() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.add("b.mp3", Kind::Mp3, 2);
    env.scan();
    let before = env.track(&a);
    fs::write(&a, b"corrupted").unwrap();

    let r = env.scan();
    assert_eq!(r.unreadable.len(), 1);
    assert_eq!(r.unreadable[0].0, a);
    assert!(r.missing.is_empty(), "a corrupt file is not a missing file");
    let after = env.track(&a);
    assert!(after.missing_since.is_none());
    assert_eq!(after.id, before.id);
    assert_eq!(after.hash, before.hash, "old record kept until readable");
}

#[test]
fn unsupported_formats_are_reported_and_not_indexed() {
    let mut env = Env::new();
    env.add("ok.mp3", Kind::Mp3, 1);
    env.add("song.flac", Kind::Flac, 2);
    env.add("hidden-flac.m4a", Kind::FlacInM4a, 3);
    env.add("opus.m4a", Kind::OpusInM4a, 4);
    let r = env.scan();
    assert_eq!(r.added, 1);
    let mut names: Vec<_> = r
        .unsupported
        .iter()
        .map(|(p, _)| p.file_name().unwrap().to_str().unwrap().to_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["hidden-flac.m4a", "opus.m4a", "song.flac"]);
    assert!(r.unreadable.is_empty(), "{:?}", r.unreadable);
    assert_eq!(env.tracks().len(), 1);
}

#[test]
fn indexed_m4a_replaced_by_unsupported_codec_is_not_marked_missing() {
    let mut env = Env::new();
    let a = env.add("a.m4a", Kind::Aac, 1);
    env.add("b.mp3", Kind::Mp3, 2);
    env.scan();
    fs::copy(fixture(Kind::FlacInM4a, 1), &a).unwrap();
    let r = env.scan();
    assert_eq!(r.unsupported.len(), 1);
    assert!(r.missing.is_empty());
    assert!(env.track(&a).missing_since.is_none());
}

#[test]
fn nfd_file_resolves_from_nfc_playlist_entry_and_back() {
    let mut env = Env::new();
    let nfd_file = env.add(NFD, Kind::Mp3, 1);
    let nfc_file = env.add("Caf\u{e9}.mp3", Kind::Mp3, 2);
    assert_ne!(NFC, NFD);
    // The on-disk spelling is kept exactly.
    let r = env.scan();
    assert_eq!(r.added, 2);
    assert!(env.tracks().iter().any(|t| t.path == nfd_file));

    let dir = env.music.display().to_string();
    env.write_playlist(
        "unicode",
        &format!("#EXTM3U\n{dir}/{NFC}\n{dir}/Cafe\u{301}.mp3\n"),
    );
    let r = env.scan();
    assert!(
        r.playlists.unresolved.is_empty(),
        "{:?}",
        r.playlists.unresolved
    );
    let items = env.indexed_items("unicode");
    assert_eq!(items[0].1, Some(env.track(&nfd_file).id));
    assert_eq!(items[1].1, Some(env.track(&nfc_file).id));
    // Entries are kept as the user wrote them until the playlist is edited.
    assert!(
        env.read_playlist("unicode")
            .contains(&format!("{dir}/{NFC}"))
    );
}

#[test]
fn hidden_folders_and_files_are_skipped() {
    let mut env = Env::new();
    env.add("shown.mp3", Kind::Mp3, 1);
    env.add(".Trash/gone.mp3", Kind::Mp3, 2);
    env.add("Album/.hidden.mp3", Kind::Mp3, 3);
    env.add(".a/.b/deep.mp3", Kind::Mp3, 4);
    let r = env.scan();
    assert_eq!(r.added, 1);
    assert_eq!(env.tracks().len(), 1);
}

#[test]
fn excluded_extensions_are_skipped_case_insensitively() {
    let mut env = Env::new();
    env.cfg.library.exclude_extensions = vec!["WAV".into()];
    env.add("a.mp3", Kind::Mp3, 1);
    env.add("b.wav", Kind::Wav, 2);
    env.add("c.flac", Kind::Flac, 3);
    env.cfg.library.exclude_extensions.push("flac".into());
    let r = env.scan();
    assert_eq!(r.added, 1);
    assert!(r.unsupported.is_empty(), "excluded beats unsupported");
    assert_eq!(env.tracks().len(), 1);
}

#[test]
fn non_audio_files_are_ignored_silently() {
    let mut env = Env::new();
    env.add("a.mp3", Kind::Mp3, 1);
    fs::write(env.music.join("cover.jpg"), b"x").unwrap();
    fs::write(env.music.join("notes.txt"), b"x").unwrap();
    let r = env.scan();
    assert_eq!(r.added, 1);
    assert!(r.unsupported.is_empty() && r.unreadable.is_empty());
}

// Safety guards.

#[test]
fn missing_root_is_an_error_and_changes_nothing() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    let before = only_track_ids(&env);
    let moved_away = env.music.with_file_name("music-gone");
    fs::rename(&env.music, &moved_away).unwrap();

    let err = env.try_scan().unwrap_err();
    assert!(
        matches!(&err, Error::RootMissing(p) if *p == env.music),
        "{err:?}"
    );
    assert_eq!(only_track_ids(&env), before);
    assert!(env.track(&a).missing_since.is_none());
}

#[test]
fn one_missing_root_among_several_aborts_the_whole_scan() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    let second = env.music.with_file_name("second");
    env.cfg.library.roots.push(second.clone());
    let err = env.try_scan().unwrap_err();
    assert!(
        matches!(&err, Error::RootMissing(p) if *p == second),
        "{err:?}"
    );
    assert!(env.track(&a).missing_since.is_none());
}

#[test]
fn empty_library_with_indexed_tracks_is_refused_and_nothing_is_marked_missing() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.add("b.mp3", Kind::Mp3, 2);
    env.scan();
    fs::remove_file(&a).unwrap();
    fs::remove_file(env.music.join("b.mp3")).unwrap();

    let err = env.try_scan().unwrap_err();
    assert!(matches!(err, Error::EmptyRoot { tracks: 2, .. }), "{err:?}");
    assert!(env.tracks().iter().all(|t| t.missing_since.is_none()));
}

#[test]
fn empty_library_with_nothing_indexed_is_fine() {
    let mut env = Env::new();
    let r = env.scan();
    assert_eq!((r.added, r.unchanged, r.missing.len()), (0, 0, 0));
}

#[test]
fn empty_library_when_everything_is_already_missing_is_fine() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    iprune_core::store::set_missing(env.store.conn(), env.track(&a).id, Some(5)).unwrap();
    fs::remove_file(&a).unwrap();
    let r = env.scan();
    assert!(r.missing.is_empty() && r.added == 0);
    assert_eq!(env.track(&a).missing_since, Some(5), "timestamp kept");
}

#[test]
fn unreadable_subfolder_does_not_mark_its_tracks_missing() {
    let mut env = Env::new();
    let inside = env.add("locked/a.mp3", Kind::Mp3, 1);
    let outside = env.add("open/b.mp3", Kind::Mp3, 2);
    env.add("locked/deep/c.mp3", Kind::Mp3, 3);
    env.scan();
    let locked = env.music.join("locked");
    if !chmod_000_blocks_reads(&locked) {
        eprintln!("skipping: permissions are not enforced (running as root?)");
        return;
    }
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let result = env.try_scan();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let r = result.unwrap();
    assert!(r.missing.is_empty(), "{:?}", r.missing);
    assert!(r.unreadable.iter().any(|(p, _)| *p == locked));
    assert!(env.track(&inside).missing_since.is_none());
    assert!(
        env.track(&env.music.join("locked/deep/c.mp3"))
            .missing_since
            .is_none()
    );
    assert!(env.track(&outside).missing_since.is_none());
    // Once readable again, nothing has changed.
    let r = env.scan();
    assert_eq!((r.unchanged, r.added, r.updated), (3, 0, 0));
}

#[test]
fn files_deleted_elsewhere_are_still_missing_when_another_folder_is_unreadable() {
    let mut env = Env::new();
    env.add("locked/a.mp3", Kind::Mp3, 1);
    let gone = env.add("open/b.mp3", Kind::Mp3, 2);
    env.add("open/c.mp3", Kind::Mp3, 3);
    env.scan();
    let locked = env.music.join("locked");
    if !chmod_000_blocks_reads(&locked) {
        return;
    }
    fs::remove_file(&gone).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let result = env.try_scan();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(result.unwrap().missing, vec![gone]);
}

// Move detection across a missing period.

#[test]
fn file_that_was_missing_and_reappears_elsewhere_is_a_move() {
    let mut env = Env::new();
    let old = env.add("a.mp3", Kind::Mp3, 1);
    env.add("keep.mp3", Kind::Mp3, 9);
    env.scan();
    let id = env.track(&old).id;
    env.write_playlist("p", &format!("#EXTM3U\n{}\n", old.display()));
    env.scan();
    fs::remove_file(&old).unwrap();
    assert_eq!(env.scan().missing, vec![old.clone()]);
    assert!(env.track(&old).missing_since.is_some());

    let new = env.add("Elsewhere/a.mp3", Kind::Mp3, 1);
    let r = env.scan();
    assert_eq!(r.moved, vec![(old.clone(), new.clone())]);
    assert_eq!((r.added, r.returned, r.missing.len()), (0, 0, 0));
    assert_eq!(r.rewritten_playlists, vec!["p".to_string()]);
    let t = env.track(&new);
    assert_eq!(t.id, id);
    assert!(t.missing_since.is_none());
    assert_eq!(env.tracks().len(), 2);
    assert_eq!(env.locations("p"), vec![s_path(&new)]);
    assert_eq!(env.indexed_items("p")[0].1, Some(id));
}

fn s_path(p: &std::path::Path) -> String {
    p.to_str().unwrap().to_owned()
}

#[test]
fn missing_track_and_two_new_copies_is_not_a_move() {
    let mut env = Env::new();
    let old = env.add("a.mp3", Kind::Mp3, 1);
    env.add("keep.mp3", Kind::Mp3, 9);
    env.scan();
    fs::remove_file(&old).unwrap();
    env.scan();
    let id = env.track(&old).id;
    let c1 = env.add("c1.mp3", Kind::Mp3, 1);
    let c2 = env.add("c2.mp3", Kind::Mp3, 1);
    let r = env.scan();
    assert!(r.moved.is_empty());
    assert_eq!(r.added, 2);
    assert!(env.track(&old).missing_since.is_some());
    assert_eq!(env.track(&old).id, id);
    assert_ne!(env.track(&c1).id, id);
    assert_ne!(env.track(&c2).id, id);
}

#[test]
fn two_missing_duplicates_and_one_new_copy_is_not_a_move() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    let b = env.add("b.mp3", Kind::Mp3, 1);
    env.add("keep.mp3", Kind::Mp3, 9);
    env.scan();
    fs::remove_file(&a).unwrap();
    fs::remove_file(&b).unwrap();
    env.scan();
    let c = env.add("c.mp3", Kind::Mp3, 1);
    let r = env.scan();
    assert!(r.moved.is_empty());
    assert_eq!(r.added, 1);
    assert!(env.track(&a).missing_since.is_some());
    assert!(env.track(&b).missing_since.is_some());
    assert!(env.track(&c).missing_since.is_none());
}

#[test]
fn newly_vanished_and_already_missing_with_same_content_are_ambiguous() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    let b = env.add("b.mp3", Kind::Mp3, 1);
    env.add("keep.mp3", Kind::Mp3, 9);
    env.scan();
    fs::remove_file(&a).unwrap();
    env.scan();
    fs::remove_file(&b).unwrap();
    let c = env.add("c.mp3", Kind::Mp3, 1);
    let r = env.scan();
    assert!(r.moved.is_empty());
    assert_eq!((r.added, r.missing.clone()), (1, vec![b.clone()]));
    assert!(env.track(&a).missing_since.is_some());
    assert!(env.track(&c).missing_since.is_none());
}

// Per-root guard.

#[test]
fn one_emptied_root_is_refused_even_when_another_root_has_files() {
    let mut env = Env::new();
    let second = env.music.with_file_name("second");
    fs::create_dir(&second).unwrap();
    env.cfg.library.roots.push(second.clone());
    let a = env.add("a.mp3", Kind::Mp3, 1);
    let b = env.add_at(&second, "b.mp3", Kind::Mp3, 2);
    env.add_at(&second, "c.mp3", Kind::Mp3, 3);
    env.scan();
    // The second disk "mounts" as an empty folder; the first loses a file.
    fs::remove_file(&b).unwrap();
    fs::remove_file(second.join("c.mp3")).unwrap();
    env.add("a2.mp3", Kind::Mp3, 4);
    fs::remove_file(&a).unwrap();

    let err = env.try_scan().unwrap_err();
    assert!(
        matches!(&err, Error::EmptyRoot { root, tracks: 2 } if *root == second),
        "{err:?}"
    );
    assert!(env.tracks().iter().all(|t| t.missing_since.is_none()));
    assert_eq!(env.tracks().len(), 3, "a2.mp3 was not indexed either");
}

#[test]
fn empty_root_with_nothing_indexed_under_it_is_fine() {
    let mut env = Env::new();
    let second = env.music.with_file_name("second");
    fs::create_dir(&second).unwrap();
    env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    env.cfg.library.roots.push(second);
    let r = env.scan();
    assert_eq!((r.unchanged, r.added, r.missing.len()), (1, 0, 0));
}
