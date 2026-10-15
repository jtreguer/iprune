mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use common::{
    Env, Kind, chmod_000_blocks_reads, fairplay_m4a, fixture, id3v24_text, mp3_with_id3v2,
};
use iprune_core::Error;
use iprune_core::store::Store;
use iprune_core::tags::{self, FileKind};

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

fn tit2(t: &str) -> Vec<u8> {
    id3v24_text("TIT2", 3, t.as_bytes())
}

/// UTF-16 text with an invalid BOM: not even Relaxed parsing accepts the tag.
fn unparseable_tag() -> Vec<Vec<u8>> {
    vec![
        tit2("Lost"),
        id3v24_text("TRCK", 1, &[0x31, 0x00, 0x32, 0x00]),
    ]
}

// Prevents: valid MP3s with a quirky tag landing in "unreadable" and never
// being indexed.
#[test]
fn file_with_unparseable_tags_is_indexed_without_tags_and_reported() {
    let mut env = Env::new();
    let good = env.add("good.mp3", Kind::Mp3, 1);
    let bad = env.add_bytes("bad-tag.mp3", &mp3_with_id3v2(2, &unparseable_tag()));
    let garbage = env.music.join("garbage.mp3");
    fs::write(&garbage, b"this is not audio at all, just text").unwrap();

    let r = env.scan();
    assert_eq!(r.added, 2);
    assert_eq!(r.untagged.len(), 1, "{:?}", r.untagged);
    assert_eq!(r.untagged[0].0, bad);
    assert!(!r.untagged[0].1.is_empty());
    assert_eq!(r.unreadable.len(), 1, "{:?}", r.unreadable);
    assert_eq!(r.unreadable[0].0, garbage, "garbage stays unreadable");
    assert!(r.drm.is_empty() && r.unsupported.is_empty());

    let row = env.track(&bad);
    assert_eq!(row.tags, iprune_core::tags::Tags::default());
    assert_eq!(row.info.kind, FileKind::Mp3);
    assert!(
        row.info.duration_ms.is_some_and(|ms| ms > 900),
        "{:?}",
        row.info
    );
    assert!(row.missing_since.is_none());
    assert_eq!(row.hash, *blake3::hash(&fs::read(&bad).unwrap()).as_bytes());
    assert!(env.tracks().iter().all(|t| t.path != garbage));
    assert!(!r.untagged.iter().any(|(p, _)| *p == good));

    // A file with a tag error is never "unchanged": it is read again.
    let r = env.scan();
    assert_eq!((r.added, r.unchanged, r.updated), (0, 1, 1));
    assert!(r.unreadable.len() == 1 && r.missing.is_empty());
}

// Prevents: a skippable frame being reported as a tag failure, or its file
// losing the tags that did parse.
#[test]
fn file_with_one_skippable_frame_is_indexed_with_its_tags_and_not_reported() {
    let mut env = Env::new();
    let p = env.add_bytes(
        "odd.mp3",
        &mp3_with_id3v2(1, &[tit2("Survivor"), id3v24_text("TDRC", 3, b"notadate")]),
    );
    let r = env.scan();
    assert_eq!(r.added, 1);
    assert!(r.untagged.is_empty() && r.unreadable.is_empty(), "{r:?}");
    assert_eq!(env.track(&p).tags.title.as_deref(), Some("Survivor"));
}

#[test]
fn file_whose_tags_break_later_is_updated_not_lost() {
    let mut env = Env::new();
    let p = env.add_bytes("a.mp3", &mp3_with_id3v2(1, &[tit2("Fine")]));
    env.add("b.mp3", Kind::Mp3, 2);
    env.scan();
    let before = env.track(&p);
    assert_eq!(before.tags.title.as_deref(), Some("Fine"));

    fs::write(&p, mp3_with_id3v2(1, &unparseable_tag())).unwrap();
    let r = env.scan();
    assert_eq!(r.updated, 1);
    assert_eq!(r.untagged.len(), 1);
    assert!(r.unreadable.is_empty() && r.missing.is_empty());
    let after = env.track(&p);
    assert_eq!(after.id, before.id);
    assert_eq!(after.tags.title, None);
    assert!(after.missing_since.is_none());
}

// Prevents: iTunes Store purchases (`drms` sample entry) being rejected as an
// unsupported codec.
#[test]
fn fairplay_m4a_is_indexed_as_aac_and_reported() {
    let mut env = Env::new();
    let store = env.add_bytes("Store/song.m4a", &fairplay_m4a(1));
    let plain = env.add("plain.m4a", Kind::Aac, 2);

    let r = env.scan();
    assert_eq!(r.added, 2);
    assert_eq!(r.drm, vec![store.clone()]);
    assert!(r.unsupported.is_empty() && r.unreadable.is_empty(), "{r:?}");
    assert!(r.untagged.is_empty());
    let row = env.track(&store);
    assert_eq!(row.info.kind, FileKind::Aac);
    assert_eq!(row.tags.title.as_deref(), Some("Title1"));
    assert_eq!(env.track(&plain).info.kind, FileKind::Aac);
}

const GOOD_TRCK_ENC3: &[u8] = b"1/23";

/// Same byte length as the bad frame: UTF-16 text with a bad BOM is 1 + 4 bytes.
fn good_track() -> Vec<u8> {
    id3v24_text("TRCK", 3, GOOD_TRCK_ENC3)
}

fn set_mtime(p: &std::path::Path, t: SystemTime) {
    fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

// Prevents: a file with unparseable tags being treated as unchanged (and so
// never retried or reported again) once its size and mtime match the index.
#[test]
fn untagged_file_is_listed_and_retried_on_every_scan() {
    let mut env = Env::new();
    let bad = env.add_bytes("bad.mp3", &mp3_with_id3v2(2, &unparseable_tag()));
    env.add("ok.mp3", Kind::Mp3, 1);
    env.scan();
    let id = env.track(&bad).id;
    for round in 1..=3 {
        let r = env.scan();
        assert_eq!(
            (r.added, r.unchanged, r.updated),
            (0, 1, 1),
            "round {round}"
        );
        assert_eq!(r.untagged.len(), 1, "round {round}: {:?}", r.untagged);
        assert_eq!(r.untagged[0].0, bad);
        assert!(!r.untagged[0].1.is_empty());
        assert!(r.unreadable.is_empty() && r.missing.is_empty());
        assert_eq!(env.track(&bad).id, id, "same row each time");
    }
}

// Prevents: a repaired file staying flagged forever. The repaired file has the
// same size and mtime as the broken one, so only the retry rule can heal it.
#[test]
fn repairing_the_tags_clears_the_flag_even_with_same_size_and_mtime() {
    let mut env = Env::new();
    let bad_frames = unparseable_tag();
    let mut good_frames = bad_frames.clone();
    good_frames[1] = good_track();
    let broken = mp3_with_id3v2(2, &bad_frames);
    let fixed = mp3_with_id3v2(2, &good_frames);
    assert_eq!(broken.len(), fixed.len(), "fixture precondition");

    let p = env.add_bytes("a.mp3", &broken);
    env.add("b.mp3", Kind::Mp3, 1);
    let when = SystemTime::now() - Duration::from_secs(3600);
    set_mtime(&p, when);
    let r = env.scan();
    assert_eq!(r.untagged.len(), 1);
    let before = env.track(&p);
    assert!(before.tag_error.is_some());
    assert_eq!(before.tags.title, None);

    fs::write(&p, &fixed).unwrap();
    set_mtime(&p, when);
    let r = env.scan();
    assert!(r.untagged.is_empty(), "{:?}", r.untagged);
    assert!(r.unreadable.is_empty());
    let after = env.track(&p);
    assert_eq!(after.id, before.id);
    assert_eq!(after.tag_error, None);
    assert_eq!(after.tags.title.as_deref(), Some("Lost"));
    assert_eq!(after.tags.track_no, Some(1));

    // Healed means unchanged from now on.
    let r = env.scan();
    assert_eq!((r.unchanged, r.updated), (2, 0));
    assert!(r.untagged.is_empty());
}

#[test]
fn repairing_the_tags_with_a_new_mtime_also_clears_the_flag() {
    let mut env = Env::new();
    let p = env.add_bytes("a.mp3", &mp3_with_id3v2(2, &unparseable_tag()));
    assert_eq!(env.scan().untagged.len(), 1);
    fs::write(&p, mp3_with_id3v2(2, &[tit2("Fixed")])).unwrap();
    bump_mtime(&p);
    let r = env.scan();
    assert!(r.untagged.is_empty());
    let t = env.track(&p);
    assert_eq!(t.tag_error, None);
    assert_eq!(t.tags.title.as_deref(), Some("Fixed"));
}

#[test]
fn a_deleted_untagged_file_drops_out_of_the_list() {
    let mut env = Env::new();
    let p = env.add_bytes("a.mp3", &mp3_with_id3v2(2, &unparseable_tag()));
    env.add("b.mp3", Kind::Mp3, 1);
    assert_eq!(env.scan().untagged.len(), 1);
    fs::remove_file(&p).unwrap();
    let r = env.scan();
    assert_eq!(r.missing, vec![p.clone()]);
    assert!(r.untagged.is_empty(), "{:?}", r.untagged);
    // It comes back flagged if the file returns.
    fs::write(&p, mp3_with_id3v2(2, &unparseable_tag())).unwrap();
    let r = env.scan();
    assert_eq!(r.untagged.len(), 1);
    assert!(env.track(&p).missing_since.is_none());
}

// Prevents: the FairPlay list only showing on the scan that first read the file.
#[test]
fn fairplay_file_stays_listed_on_rescans_and_leaves_when_deleted() {
    let mut env = Env::new();
    let store = env.add_bytes("Store/song.m4a", &fairplay_m4a(1));
    let plain = env.add("plain.m4a", Kind::Aac, 2);
    assert_eq!(env.scan().drm, vec![store.clone()]);
    for round in 1..=2 {
        let r = env.scan();
        assert_eq!((r.added, r.unchanged), (0, 2), "round {round}");
        assert_eq!(r.drm, vec![store.clone()], "round {round}");
    }
    assert!(env.track(&store).drm);
    assert!(!env.track(&plain).drm);

    fs::remove_file(&store).unwrap();
    let r = env.scan();
    assert_eq!(r.missing, vec![store.clone()]);
    assert!(r.drm.is_empty(), "{:?}", r.drm);
    assert!(
        env.track(&store).drm,
        "the row remembers, the report skips it"
    );
}

#[test]
fn replacing_a_fairplay_file_with_an_ordinary_one_clears_the_flag() {
    let mut env = Env::new();
    let p = env.add_bytes("a.m4a", &fairplay_m4a(1));
    env.add("b.mp3", Kind::Mp3, 2);
    assert_eq!(env.scan().drm.len(), 1);
    fs::copy(fixture(Kind::Aac, 3), &p).unwrap();
    bump_mtime(&p);
    let r = env.scan();
    assert!(r.drm.is_empty());
    assert!(!env.track(&p).drm);
}

// ---- tags read by an older reader (audit M15)

fn index_path(env: &Env) -> PathBuf {
    env.music.parent().unwrap().join("db/iprune.sqlite")
}

fn versions(env: &Env) -> Vec<(String, i64)> {
    let mut stmt = env
        .store
        .conn()
        .prepare("SELECT path, tags_version FROM tracks ORDER BY path")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Turns the index into one that the previous version of iprune left: no
/// `tags_version` column, schema version 6. Opening it again migrates it.
fn downgrade_to_version_6(env: &mut Env) {
    let conn = env.store.conn();
    conn.execute_batch("ALTER TABLE tracks DROP COLUMN tags_version")
        .unwrap();
    conn.pragma_update(None, "user_version", 6).unwrap();
    env.store = Store::open(&index_path(env)).unwrap();
}

fn multi_value_mp3() -> Vec<u8> {
    mp3_with_id3v2(
        1,
        &[
            id3v24_text("TIT2", 3, b"Multi"),
            id3v24_text("TPE1", 3, b"Artist A\0Artist B"),
            id3v24_text("TCON", 3, b"Rock\0Pop"),
        ],
    )
}

// Prevents: a changed tag reader never reaching files already indexed (the
// scan skips them by size and mtime), and a tags-only re-read replacing the
// content hash that move detection depends on.
#[test]
fn rows_read_by_an_older_reader_get_their_tags_read_again_but_keep_their_hash_once() {
    let mut env = Env::new();
    let multi = env.add_bytes("multi.mp3", &multi_value_mp3());
    let plain = env.add("plain.mp3", Kind::Mp3, 2);
    let current = env.add("current.mp3", Kind::Mp3, 3);
    let r = env.scan();
    assert_eq!((r.added, r.updated, r.unchanged), (3, 0, 0));
    assert!(
        versions(&env)
            .iter()
            .all(|(_, v)| *v == tags::READER_VERSION),
        "a new row records the reader that read it: {:?}",
        versions(&env)
    );
    let ids: Vec<i64> = [&multi, &plain, &current]
        .iter()
        .map(|p| env.track(p).id)
        .collect();

    // The index of the previous version: the multi-value tags were cut to the
    // first value, and the hashes are not the files' (so a re-hash shows).
    downgrade_to_version_6(&mut env);
    let conn = env.store.conn();
    let mut cut = env.track(&multi).tags;
    cut.artist = Some("Artist A".into());
    cut.genre = Some("Rock".into());
    conn.execute(
        "UPDATE tracks SET artist = 'Artist A', genre = 'Rock', tags_hash = ?1 WHERE path = ?2",
        rusqlite::params![&cut.hash()[..], multi.to_str().unwrap()],
    )
    .unwrap();
    for (k, p) in [&multi, &plain, &current].iter().enumerate() {
        conn.execute(
            "UPDATE tracks SET hash = ?1 WHERE path = ?2",
            rusqlite::params![&[k as u8 + 1; 32][..], p.to_str().unwrap()],
        )
        .unwrap();
    }
    // One row was already read by the current reader, whatever its tags say.
    conn.execute(
        "UPDATE tracks SET tags_version = ?1, title = 'Marker' WHERE path = ?2",
        rusqlite::params![tags::READER_VERSION, current.to_str().unwrap()],
    )
    .unwrap();
    let marker_hash = env.track(&current).tags_hash;
    let versions_before: Vec<i64> = versions(&env).into_iter().map(|(_, v)| v).collect();
    assert_eq!(
        versions_before.iter().filter(|v| **v == 0).count(),
        2,
        "set up"
    );

    let r = env.scan();
    assert_eq!(
        (r.updated, r.unchanged, r.added, r.returned),
        (2, 1, 0, 0),
        "{r:?}"
    );
    let t = env.track(&multi);
    assert_eq!(t.tags.artist.as_deref(), Some("Artist A/Artist B"));
    assert_eq!(t.tags.genre.as_deref(), Some("Rock/Pop"));
    assert_eq!(t.tags_hash, t.tags.hash(), "the tag hash follows the tags");
    assert_ne!(t.tags_hash, cut.hash());
    assert_eq!(t.hash, [1; 32], "the content hash is kept");
    assert_eq!(env.track(&plain).hash, [2; 32], "the content hash is kept");
    assert_eq!(env.track(&plain).tags.title.as_deref(), Some("Title2"));
    // The row of the current reader was not read again.
    let c = env.track(&current);
    assert_eq!(c.tags.title.as_deref(), Some("Marker"));
    assert_eq!(c.tags_hash, marker_hash);
    assert_eq!(c.hash, [3; 32]);
    // Same rows, all at the current version.
    assert_eq!(
        [&multi, &plain, &current].map(|p| env.track(p).id).to_vec(),
        ids
    );
    assert!(
        versions(&env)
            .iter()
            .all(|(_, v)| *v == tags::READER_VERSION),
        "{:?}",
        versions(&env)
    );

    // Once: the next scan finds everything unchanged.
    let r = env.scan();
    assert_eq!((r.updated, r.unchanged, r.added), (0, 3, 0), "{r:?}");
}

// Prevents: a stale row whose file also changed being read tags-only, which
// would keep the hash of the old content.
#[test]
fn a_stale_row_whose_file_changed_is_read_in_full_and_gets_the_new_hash() {
    let mut env = Env::new();
    let p = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    downgrade_to_version_6(&mut env);
    env.store
        .conn()
        .execute("UPDATE tracks SET hash = ?1", [&[9u8; 32][..]])
        .unwrap();
    fs::copy(fixture(Kind::Mp3, 5), &p).unwrap();
    bump_mtime(&p);
    let r = env.scan();
    assert_eq!((r.updated, r.unchanged, r.added), (1, 0, 0), "{r:?}");
    let t = env.track(&p);
    assert_eq!(t.tags.title.as_deref(), Some("Title5"));
    assert_ne!(t.hash, [9; 32], "the content changed, so did its hash");
    // The hash is the new file's: the same file in a fresh index.
    let mut other = Env::new();
    let q = other.add("a.mp3", Kind::Mp3, 5);
    other.scan();
    assert_eq!(t.hash, other.track(&q).hash);
    assert_eq!(versions(&env)[0].1, tags::READER_VERSION);
}
