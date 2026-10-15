//! Adding, copying, updating and removing tracks, on fake iPods in temp dirs
//! (`iPod_Control` tree whose iTunesDB is written by `tests/c/make_ipod.c`).
//! Nothing here opens a real iPod.

mod common;
use common::FakeIpod;

use std::fs;
use std::path::{Path, PathBuf};

use gpod::{AudioFields, Database, Error, TrackFields};

const T1: u64 = 0x1;
const T2: u64 = 0x2;
const T3: u64 = 0x3;
const NEW: u64 = 0xabcd_ef01_2345_6789;

const MANIFEST: &str = "M\tiPod de Test
T\t1\t1000\t1000\tOne\tArtA\tAlb\t0
T\t2\t2000\t1000\tTwo\tArtB\tAlb\t0
T\t3\t3000\t1000\tThree\tArtC\tAlb\t0
P\tAlpha\t1,2,2,3
P\tBeta\t2
S\tSmart\t1,2
R\tSmart\tBjörk
C\tPodcasts
";

fn audio(size: u32) -> AudioFields {
    AudioFields {
        size,
        tracklen: 123_456,
        bitrate: 192,
        samplerate: 44_100,
        filetype: "MPEG audio file".into(),
        time_added: 1_700_000_000,
        time_modified: 1_650_000_000,
    }
}

fn fields() -> TrackFields {
    TrackFields {
        title: Some("Nouveau Titre é".into()),
        artist: Some("Artiste".into()),
        album: Some("Album".into()),
        albumartist: Some("AA".into()),
        composer: Some("Comp".into()),
        genre: Some("Jazz".into()),
        year: Some(1999),
        track_nr: Some(4),
        tracks: Some(12),
        cd_nr: Some(1),
        cds: Some(2),
        compilation: Some(true),
    }
}

fn snap(db: &Database) -> String {
    let mut out = Vec::new();
    gpod::snapshot::write(db, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn members(db: &Database, name: &str) -> Vec<u64> {
    db.playlists()
        .find(|p| p.name().as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no playlist {name}"))
        .members()
        .map(|t| t.dbid())
        .collect()
}

/// A source file outside the iPod, with distinctive bytes.
fn source(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, bytes).unwrap();
    p
}

fn music_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(&root.join("iPod_Control/Music"), &mut out);
    out.sort();
    out
}

#[test]
fn an_added_and_copied_track_has_its_file_and_survives_a_write_and_a_reread() {
    let ipod = FakeIpod::from_manifest(MANIFEST);
    let src_dir = tempfile::tempdir().unwrap();
    let bytes = b"not really audio, but distinctive 0123456789";
    let src = source(src_dir.path(), "song.mp3", bytes);
    let mut db = Database::open(&ipod.root()).unwrap();
    let before = db.track_count();

    db.add_track(NEW, &fields(), &audio(bytes.len() as u32))
        .unwrap();
    assert_eq!(db.track_count(), before + 1);
    assert_eq!(db.track_file(NEW).unwrap(), None, "no file before the copy");
    let old = db.copy_track_file(NEW, &src).unwrap();
    assert_eq!(old, None, "a new track has no older file");

    let file = db.track_file(NEW).unwrap().expect("a file after the copy");
    assert!(
        file.starts_with(ipod.root().join("iPod_Control/Music")),
        "{file:?}"
    );
    assert_eq!(fs::read(&file).unwrap(), bytes, "the bytes were copied");
    assert!(file.extension().is_some_and(|e| e == "mp3"), "{file:?}");

    // At the end of the master playlist, in no other.
    let mut master = members(&db, "iPod de Test");
    assert_eq!(master.pop(), Some(NEW));
    assert_eq!(master, [T1, T2, T3]);
    assert!(!members(&db, "Alpha").contains(&NEW));
    assert!(!members(&db, "Smart").contains(&NEW));

    db.write().unwrap();
    drop(db);
    let back = Database::open(&ipod.root()).unwrap();
    let t = back
        .track_by_dbid(NEW)
        .expect("the dbid survives the write");
    assert_eq!(t.dbid(), NEW, "libgpod kept the dbid");
    assert_eq!(t.title().as_deref(), Some("Nouveau Titre é"));
    assert_eq!(t.artist().as_deref(), Some("Artiste"));
    assert_eq!(t.album().as_deref(), Some("Album"));
    assert_eq!(t.albumartist().as_deref(), Some("AA"));
    assert_eq!(t.composer().as_deref(), Some("Comp"));
    assert_eq!(t.genre().as_deref(), Some("Jazz"));
    assert_eq!(
        (t.year(), t.track_nr(), t.tracks(), t.cd_nr(), t.cds()),
        (1999, 4, 12, 1, 2)
    );
    assert!(t.compilation());
    assert_eq!(t.size() as usize, bytes.len());
    assert_eq!(
        (t.tracklen(), t.bitrate(), t.samplerate()),
        (123_456, 192, 44_100)
    );
    assert_eq!(t.filetype().as_deref(), Some("MPEG audio file"));
    assert_eq!(t.time_added(), 1_700_000_000);
    assert_eq!(t.mediatype(), 1, "audio");
    let ipod_path = t.ipod_path().expect("ipod_path").into_owned();
    assert!(
        ipod_path.starts_with(":iPod_Control:Music:F"),
        "{ipod_path}"
    );
    assert_eq!(members(&back, "iPod de Test").last(), Some(&NEW));
}

#[test]
fn a_dbid_of_zero_or_in_use_is_refused_and_changes_nothing() {
    let ipod = FakeIpod::from_manifest(MANIFEST);
    let mut db = Database::open(&ipod.root()).unwrap();
    let before = snap(&db);
    for dbid in [0, T2] {
        let e = db.add_track(dbid, &fields(), &audio(10)).unwrap_err();
        assert!(matches!(e, Error::Write(_)), "{dbid}: {e}");
        assert_eq!(db.track_count(), 3);
        assert_eq!(snap(&db), before, "{dbid}");
    }
    assert_eq!(members(&db, "iPod de Test"), [T1, T2, T3]);
}

#[test]
fn a_nul_in_a_field_is_refused_when_adding_and_nothing_is_added() {
    let ipod = FakeIpod::from_manifest(MANIFEST);
    let mut db = Database::open(&ipod.root()).unwrap();
    let before = snap(&db);
    let bad = TrackFields {
        title: Some("fine".into()),
        genre: Some("Ja\0zz".into()),
        ..TrackFields::default()
    };
    let e = db.add_track(NEW, &bad, &audio(10)).unwrap_err();
    assert!(matches!(e, Error::NulInName(_)), "{e}");
    assert_eq!(db.track_count(), 3);
    assert!(db.track_by_dbid(NEW).is_none());
    assert_eq!(snap(&db), before);
    // The failed attempt did not use up the dbid.
    db.add_track(NEW, &fields(), &audio(10)).unwrap();
}

#[test]
fn a_nul_in_a_field_leaves_every_field_of_an_existing_track_as_it_was() {
    let ipod = FakeIpod::from_manifest(MANIFEST);
    let mut db = Database::open(&ipod.root()).unwrap();
    let before = snap(&db);
    // The title comes before the genre in the struct, and is fine: it must
    // not be set when the genre is refused.
    let bad = TrackFields {
        title: Some("Changed".into()),
        year: Some(2001),
        genre: Some("Ja\0zz".into()),
        ..TrackFields::default()
    };
    let e = db.set_track_fields(T1, &bad).unwrap_err();
    assert!(matches!(e, Error::NulInName(_)), "{e}");
    assert_eq!(snap(&db), before);
    assert_eq!(
        db.track_by_dbid(T1).unwrap().title().as_deref(),
        Some("One")
    );
    assert_eq!(db.track_by_dbid(T1).unwrap().year(), 0);
}

// Prevents a copy from starting with fields the track cannot hold: `check`
// is what lets the caller refuse before the file is copied.
#[test]
fn check_refuses_a_nul_in_any_text_field_and_accepts_the_rest() {
    assert!(fields().check().is_ok());
    assert!(TrackFields::default().check().is_ok());
    // A fresh field set per case, with only one text field bad.
    type Setter = fn(&mut TrackFields, String);
    let cases: [(&str, Setter); 6] = [
        ("title", |f, v| f.title = Some(v)),
        ("artist", |f, v| f.artist = Some(v)),
        ("album", |f, v| f.album = Some(v)),
        ("albumartist", |f, v| f.albumartist = Some(v)),
        ("composer", |f, v| f.composer = Some(v)),
        ("genre", |f, v| f.genre = Some(v)),
    ];
    for (name, set) in cases {
        let mut f = fields();
        set(&mut f, format!("bad\0{name}"));
        let e = f.check().unwrap_err();
        match e {
            Error::NulInName(v) => assert_eq!(v, format!("bad\0{name}")),
            other => panic!("{name}: {other}"),
        }
        // Without the NUL the same value passes.
        set(&mut f, format!("bad {name}"));
        assert!(f.check().is_ok(), "{name}");
    }
}

// `check` and the setters must agree: what `check` accepts, `add_track` takes.
#[test]
fn what_check_accepts_add_track_takes_and_what_it_refuses_add_track_refuses() {
    let ipod = FakeIpod::from_manifest(MANIFEST);
    let mut db = Database::open(&ipod.root()).unwrap();
    let bad = TrackFields {
        composer: Some("a\0b".into()),
        ..fields()
    };
    assert!(bad.check().is_err());
    assert!(db.add_track(NEW, &bad, &audio(10)).is_err());
    assert!(fields().check().is_ok());
    db.add_track(NEW, &fields(), &audio(10)).unwrap();
}

#[test]
fn set_track_fields_sets_what_is_given_and_leaves_none_alone() {
    let ipod = FakeIpod::from_manifest(MANIFEST);
    let mut db = Database::open(&ipod.root()).unwrap();
    db.set_track_fields(
        T1,
        &TrackFields {
            artist: Some("New Artist".into()),
            year: Some(1984),
            compilation: Some(true),
            ..TrackFields::default()
        },
    )
    .unwrap();
    let t = db.track_by_dbid(T1).unwrap();
    assert_eq!(t.artist().as_deref(), Some("New Artist"));
    assert_eq!(t.year(), 1984);
    assert!(t.compilation());
    // Untouched: the title, the album and everything else.
    assert_eq!(t.title().as_deref(), Some("One"));
    assert_eq!(t.album().as_deref(), Some("Alb"));
    assert_eq!(t.size(), 1000);
    assert_eq!(t.track_nr(), 0);
    // The other tracks did not move.
    let two = db.track_by_dbid(T2).unwrap();
    assert_eq!(
        (two.title().as_deref(), two.artist().as_deref(), two.year()),
        (Some("Two"), Some("ArtB"), 0)
    );

    // Setting nothing changes nothing.
    let before = snap(&db);
    db.set_track_fields(T1, &TrackFields::default()).unwrap();
    assert_eq!(snap(&db), before);

    // And an unknown track is an error, not a silent success.
    let e = db.set_track_fields(0x99, &fields()).unwrap_err();
    assert!(matches!(e, Error::NoSuchTrack(0x99)), "{e}");
}

#[test]
fn set_track_audio_sets_the_audio_facts_only() {
    let ipod = FakeIpod::from_manifest(MANIFEST);
    let mut db = Database::open(&ipod.root()).unwrap();
    db.set_track_audio(T3, &audio(777)).unwrap();
    let t = db.track_by_dbid(T3).unwrap();
    assert_eq!(
        (t.size(), t.tracklen(), t.bitrate(), t.samplerate()),
        (777, 123_456, 192, 44_100)
    );
    assert_eq!(t.title().as_deref(), Some("Three"), "tags untouched");
    assert!(matches!(
        db.set_track_audio(0x99, &audio(1)).unwrap_err(),
        Error::NoSuchTrack(0x99)
    ));
}

#[test]
fn remove_track_leaves_every_playlist_master_and_smart_included_repeats_too() {
    let ipod = FakeIpod::from_manifest(MANIFEST);
    // T2 has a file on the iPod (make_ipod.c names the second F01/T0001.mp3):
    // it must be returned, not deleted.
    let file_rel = "iPod_Control/Music/F01/T0001.mp3";
    fs::write(ipod.root().join(file_rel), b"audio of two").unwrap();
    let mut db = Database::open(&ipod.root()).unwrap();
    assert_eq!(
        members(&db, "Alpha"),
        [T1, T2, T2, T3],
        "repeat in the setup"
    );

    let file = db.remove_track(T2).unwrap();
    assert_eq!(file, Some(ipod.root().join(file_rel)));
    assert!(
        ipod.root().join(file_rel).is_file(),
        "the file is not deleted here"
    );

    assert!(db.track_by_dbid(T2).is_none());
    assert_eq!(db.track_count(), 2);
    assert_eq!(members(&db, "iPod de Test"), [T1, T3]);
    assert_eq!(members(&db, "Alpha"), [T1, T3], "both occurrences gone");
    assert_eq!(members(&db, "Beta"), Vec::<u64>::new());
    assert_eq!(members(&db, "Smart"), [T1]);

    db.write().unwrap();
    drop(db);
    let back = Database::open(&ipod.root()).unwrap();
    assert!(back.track_by_dbid(T2).is_none());
    assert_eq!(members(&back, "Alpha"), [T1, T3]);
    assert_eq!(members(&back, "iPod de Test"), [T1, T3]);
    assert_eq!(members(&back, "Smart"), [T1]);
    assert_eq!(
        back.track_by_dbid(T1).unwrap().title().as_deref(),
        Some("One")
    );
}

#[test]
fn remove_track_of_an_unknown_dbid_is_an_error_and_changes_nothing() {
    let ipod = FakeIpod::from_manifest(MANIFEST);
    let mut db = Database::open(&ipod.root()).unwrap();
    let before = snap(&db);
    let e = db.remove_track(0x99).unwrap_err();
    assert!(matches!(e, Error::NoSuchTrack(0x99)), "{e}");
    assert_eq!(snap(&db), before);
}

#[test]
fn removing_a_track_that_was_just_added_takes_it_out_of_the_master_playlist() {
    let ipod = FakeIpod::from_manifest(MANIFEST);
    let mut db = Database::open(&ipod.root()).unwrap();
    let before = snap(&db);
    db.add_track(NEW, &fields(), &audio(10)).unwrap();
    assert_eq!(db.remove_track(NEW).unwrap(), None, "no file yet");
    assert_eq!(snap(&db), before);
}

#[test]
fn a_copy_from_a_missing_source_is_a_copy_error_naming_the_source() {
    let ipod = FakeIpod::from_manifest(MANIFEST);
    let src_dir = tempfile::tempdir().unwrap();
    let missing = src_dir.path().join("gone.mp3");
    let mut db = Database::open(&ipod.root()).unwrap();
    db.add_track(NEW, &fields(), &audio(10)).unwrap();
    let e = db.copy_track_file(NEW, &missing).unwrap_err();
    match &e {
        Error::Copy { path, message } => {
            assert_eq!(path, &missing);
            assert!(!message.is_empty());
        }
        other => panic!("{other}"),
    }
    assert!(music_files(&ipod.root()).is_empty(), "nothing copied");
    assert_eq!(db.track_file(NEW).unwrap(), None, "still no file");
    // An unknown track is refused before any copy is tried.
    let real = source(src_dir.path(), "real.mp3", b"x");
    assert!(matches!(
        db.copy_track_file(0x99, &real).unwrap_err(),
        Error::NoSuchTrack(0x99)
    ));
}

/// Two copies of a track: the first from "a.mp3", the second from "b.mp3".
/// Returns what the second returned as the older file, and both paths.
fn copied_twice(ipod: &FakeIpod, db: &mut Database) -> (Option<PathBuf>, PathBuf, PathBuf) {
    let src_dir = tempfile::tempdir().unwrap();
    let first = source(src_dir.path(), "a.mp3", b"first version");
    let second = source(src_dir.path(), "b.mp3", b"second, longer version");
    db.add_track(NEW, &fields(), &audio(13)).unwrap();
    assert_eq!(db.copy_track_file(NEW, &first).unwrap(), None);
    let old_file = db.track_file(NEW).unwrap().unwrap();
    let returned = db.copy_track_file(NEW, &second).unwrap();
    let new_file = db.track_file(NEW).unwrap().unwrap();
    assert!(new_file.starts_with(ipod.root()));
    (returned, old_file, new_file)
}

// What exec relies on: the file returned by a second copy is deleted after
// the write, so it must not be the file the track now points at.
#[test]
fn the_older_file_returned_by_a_second_copy_is_not_the_tracks_new_file() {
    let ipod = FakeIpod::from_manifest(MANIFEST);
    let mut db = Database::open(&ipod.root()).unwrap();
    let (returned, old_file, new_file) = copied_twice(&ipod, &mut db);
    assert_eq!(returned.as_ref(), Some(&old_file), "the previous file");
    assert_ne!(
        returned.as_ref(),
        Some(&new_file),
        "deleting the returned file would delete the new audio"
    );
}

// The doc of `copy_track_file` says the existing file is left where it is,
// so the old database stays valid until the new one is written.
#[test]
fn a_second_copy_leaves_the_older_file_with_its_bytes() {
    let ipod = FakeIpod::from_manifest(MANIFEST);
    let mut db = Database::open(&ipod.root()).unwrap();
    let (_, old_file, new_file) = copied_twice(&ipod, &mut db);
    assert_eq!(fs::read(&new_file).unwrap(), b"second, longer version");
    assert_eq!(fs::read(&old_file).unwrap(), b"first version");
}

#[test]
fn a_failed_second_copy_keeps_the_track_pointing_at_its_old_file() {
    let ipod = FakeIpod::from_manifest(MANIFEST);
    let src_dir = tempfile::tempdir().unwrap();
    let first = source(src_dir.path(), "a.mp3", b"first version");
    let mut db = Database::open(&ipod.root()).unwrap();
    db.add_track(NEW, &fields(), &audio(13)).unwrap();
    db.copy_track_file(NEW, &first).unwrap();
    let old_file = db.track_file(NEW).unwrap().unwrap();
    let path_before = db
        .track_by_dbid(NEW)
        .unwrap()
        .ipod_path()
        .map(|p| p.into_owned());

    let e = db
        .copy_track_file(NEW, &src_dir.path().join("gone.mp3"))
        .unwrap_err();
    assert!(matches!(e, Error::Copy { .. }), "{e}");
    let path_after = db
        .track_by_dbid(NEW)
        .unwrap()
        .ipod_path()
        .map(|p| p.into_owned());
    assert_eq!(
        path_after, path_before,
        "the track still points at its file"
    );
    assert!(
        old_file.is_file(),
        "the failed copy deleted the file the track still points at"
    );
    assert_eq!(fs::read(&old_file).unwrap(), b"first version");
    // `track_file` still finds it: the failed copy must not leave the track
    // marked as not transferred, or a later removal would leak its file.
    assert_eq!(db.track_file(NEW).unwrap(), Some(old_file));
}

#[test]
fn set_track_audio_with_a_zero_date_keeps_the_date_added_and_a_date_sets_it() {
    let ipod = FakeIpod::from_manifest(MANIFEST);
    let mut db = Database::open(&ipod.root()).unwrap();
    db.set_track_audio(T1, &audio(10)).unwrap();
    assert_eq!(db.track_by_dbid(T1).unwrap().time_added(), 1_700_000_000);
    let keep = AudioFields {
        time_added: 0,
        time_modified: 1_600_000_000,
        ..audio(20)
    };
    db.set_track_audio(T1, &keep).unwrap();
    let t = db.track_by_dbid(T1).unwrap();
    assert_eq!(t.time_added(), 1_700_000_000, "0 leaves the date alone");
    assert_eq!(t.size(), 20, "the rest is still set");
}

// ---- reset_replaced_audio: what described the old file (audit M13)

/// The per-file audio data of a track as libgpod stores it, read back
/// through libgpod itself and not through the wrapper under test.
#[derive(Debug, PartialEq)]
struct Raw {
    pregap: u32,
    postgap: u32,
    samplecount: u64,
    gapless_data: u32,
    gapless_track_flag: u32,
    gapless_album_flag: u32,
    soundcheck: u32,
    starttime: u32,
    stoptime: u32,
    bookmark_time: u32,
    samplerate: u32,
    playcount: u32,
    skipcount: u32,
    rating: u32,
    time_added: i64,
    time_played: i64,
}

/// Parses the fake iPod at `root` with raw libgpod, applies `f` to the track
/// `dbid` and, when `write`, writes it back. Returns its fields.
fn with_raw_track(
    root: &Path,
    dbid: u64,
    write: bool,
    f: impl Fn(&mut gpod_sys::Itdb_Track),
) -> Raw {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let mp = CString::new(root.as_os_str().as_bytes()).unwrap();
    unsafe {
        let mut err = std::ptr::null_mut();
        let db = gpod_sys::itdb_parse(mp.as_ptr(), &mut err);
        assert!(!db.is_null(), "itdb_parse failed");
        let mut l = (*db).tracks;
        let mut found = None;
        while !l.is_null() {
            let t = (*l).data as *mut gpod_sys::Itdb_Track;
            if (*t).dbid == dbid {
                found = Some(t);
            }
            l = (*l).next;
        }
        let t = found.expect("no such track");
        f(&mut *t);
        if write {
            assert_ne!(gpod_sys::itdb_write(db, &mut err), 0, "itdb_write failed");
        }
        let t = &*t;
        let raw = Raw {
            pregap: t.pregap,
            postgap: t.postgap,
            samplecount: t.samplecount,
            gapless_data: t.gapless_data,
            gapless_track_flag: t.gapless_track_flag.into(),
            gapless_album_flag: t.gapless_album_flag.into(),
            soundcheck: t.soundcheck,
            starttime: t.starttime,
            stoptime: t.stoptime,
            bookmark_time: t.bookmark_time,
            samplerate: t.samplerate.into(),
            playcount: t.playcount,
            skipcount: t.skipcount,
            rating: t.rating,
            time_added: t.time_added,
            time_played: t.time_played,
        };
        gpod_sys::itdb_free(db);
        raw
    }
}

fn old_audio(t: &mut gpod_sys::Itdb_Track) {
    t.pregap = 576;
    t.postgap = 1100;
    t.samplecount = 8_820_000;
    t.gapless_data = 12345;
    t.gapless_track_flag = 1;
    t.gapless_album_flag = 1;
    t.soundcheck = 1200;
    t.starttime = 5000;
    t.stoptime = 190_000;
    t.bookmark_time = 7000;
    t.samplerate = 22_050;
    t.samplerate2 = 22_050.0;
    t.playcount = 7;
    t.skipcount = 2;
    t.rating = 80;
    t.time_added = 1_600_000_000;
    t.time_played = 1_650_000_000;
}

fn read_raw(root: &Path, dbid: u64) -> Raw {
    with_raw_track(root, dbid, false, |_| {})
}

#[test]
fn reset_replaced_audio_clears_the_old_files_data_and_keeps_plays_rating_and_dates() {
    let ipod = FakeIpod::from_manifest(MANIFEST);
    let root = ipod.root();
    with_raw_track(&root, T1, true, old_audio);
    with_raw_track(&root, T2, true, old_audio);
    let seeded = read_raw(&root, T2);
    assert_eq!(seeded.pregap, 576, "set up: the fields were stored");
    assert_eq!(seeded.stoptime, 190_000, "set up");

    let mut db = Database::open(&root).unwrap();
    db.reset_replaced_audio(T2).unwrap();
    db.write().unwrap();
    drop(db);

    let after = read_raw(&root, T2);
    assert_eq!(
        after,
        Raw {
            pregap: 0,
            postgap: 0,
            samplecount: 0,
            gapless_data: 0,
            gapless_track_flag: 0,
            soundcheck: 0,
            starttime: 0,
            stoptime: 0,
            bookmark_time: 0,
            // Kept: the album's flag, the rate (set by set_track_audio, not
            // here), plays, rating and dates.
            gapless_album_flag: 1,
            samplerate: 22_050,
            playcount: 7,
            skipcount: 2,
            rating: 80,
            time_added: 1_600_000_000,
            time_played: 1_650_000_000,
        }
    );
    // Another track is not touched.
    assert_eq!(read_raw(&root, T1), seeded);
}

#[test]
fn reset_replaced_audio_of_an_unknown_track_is_an_error_and_changes_nothing() {
    let ipod = FakeIpod::from_manifest(MANIFEST);
    let root = ipod.root();
    with_raw_track(&root, T1, true, old_audio);
    let seeded = read_raw(&root, T1);
    let mut db = Database::open(&root).unwrap();
    let before = snap(&db);
    let e = db.reset_replaced_audio(0x99).unwrap_err();
    assert!(matches!(e, Error::NoSuchTrack(0x99)), "{e}");
    assert_eq!(snap(&db), before);
    db.write().unwrap();
    drop(db);
    assert_eq!(read_raw(&root, T1), seeded);
}
