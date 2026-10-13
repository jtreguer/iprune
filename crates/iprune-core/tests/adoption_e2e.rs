//! Scan a generated library, read an iTunesDB written by libgpod (from the C
//! helper `make_ipod.c`) and run adoption through the library API.
//!
//! The iPod tracks take their `size` from the generated files' byte size and
//! their `tracklen` from the scanned duration, as on a real iPod.

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use common::{
    Env, IpodSpec, Kind, PlaylistSpec, fake_ipod, id3v24_text, ipod_playlist, mp3_with_id3v2,
};
use iprune_core::adopt::Reason;
use iprune_core::store::devices;
use iprune_core::{Error, adoption, ipod, playlists};

const HIGH_BIT: u64 = 0xffff_ffff_ffff_fff0;

struct World {
    env: Env,
    /// Library files by short name.
    files: HashMap<&'static str, PathBuf>,
    /// The mounted fake iPod (kept alive).
    _ipod_dir: tempfile::TempDir,
    ipod_root: PathBuf,
}

impl World {
    fn id(&self, name: &str) -> i64 {
        self.env.track(&self.files[name]).id
    }

    fn spec(&self, file: &str, dbid: u64, title: &'static str, artist: &'static str) -> IpodSpec {
        let t = self.env.track(&self.files[file]);
        IpodSpec {
            dbid,
            size: fs::metadata(&t.path).unwrap().len(),
            ms: t.info.duration_ms.expect("duration"),
            title,
            artist,
            album: "",
            plays: 0,
        }
    }

    fn open(&self) -> gpod::Database {
        gpod::Database::open(&self.ipod_root).unwrap()
    }
}

/// A library of seven files and an iPod that holds most of them.
///
///  iPod dbid        file          expected
///  HIGH_BIT         mp3 one       accepted (shares its size with mp3 two)
///  2                mp3 two       weak (title edited on the iPod)
///  3                aac one       accepted
///  4                untagged.mp3  accepted (no title or artist, either side)
///  5                none          unmatched on the iPod
///  6 and 7          wav three     both claim it: shared
///  8                retag.mp3     one byte off in size: tags only
///  (nothing)        wav two, aac two, aiff  library files not on the iPod
fn world(sysinfo: Option<&str>) -> World {
    let mut env = Env::new();
    let mut files = HashMap::new();
    files.insert("mp3one", env.add("mp3one.mp3", Kind::Mp3, 1));
    files.insert("mp3two", env.add("mp3two.mp3", Kind::Mp3, 2));
    files.insert("aacone", env.add("aacone.m4a", Kind::Aac, 1));
    files.insert("wavtwo", env.add("wavtwo.wav", Kind::Wav, 2));
    files.insert("wavthree", env.add("wavthree.wav", Kind::Wav, 3));
    files.insert("aacnotonipod", env.add("aacnotonipod.m4a", Kind::Aac, 2));
    files.insert("aiff", env.add("aiff4.aiff", Kind::Aiff, 4));
    // Retagged after it was copied to the iPod: the iPod has other bytes.
    let frames = [
        id3v24_text("TIT2", 3, b"Retagged Song"),
        id3v24_text("TPE1", 3, b"Some Band"),
        id3v24_text("TALB", 3, b"Some Album"),
    ];
    files.insert(
        "retag",
        env.add_bytes("retag.mp3", &mp3_with_id3v2(6, &frames)),
    );
    files.insert(
        "untagged",
        env.add_bytes("Untagged Demo.mp3", &mp3_with_id3v2(5, &[])),
    );
    env.scan();
    let ipod_dir = tempfile::tempdir().unwrap();
    let mut w = World {
        env,
        files,
        _ipod_dir: ipod_dir,
        ipod_root: PathBuf::new(),
    };
    let mut wav6 = w.spec("wavthree", 6, "Title3", "Artist3");
    wav6.plays = 4;
    let wav7 = w.spec("wavthree", 7, "Title3", "Artist3");
    let mut retag = w.spec("retag", 8, "Retagged Song", "Some Band");
    retag.album = "Some Album";
    retag.size += 1;
    let mut gone = w.spec("mp3one", 5, "Gone", "Nobody");
    gone.size = 12345;
    let tracks = [
        w.spec("mp3one", HIGH_BIT, "Title1", "Artist1"),
        w.spec("mp3two", 2, "Renamed", "Artist2"),
        w.spec("aacone", 3, "Title1", "Artist1"),
        w.spec("untagged", 4, "Untagged Demo", ""),
        gone,
        wav6,
        wav7,
        retag,
    ];
    let smart = PlaylistSpec {
        name: "Top rated",
        smart: true,
        podcasts: false,
        members: vec![3],
    };
    let playlists = [
        ipod_playlist("Road", &[HIGH_BIT, 3]),
        ipod_playlist("MIX", &[HIGH_BIT]),
        ipod_playlist("OnlyIpod", &[5]),
        ipod_playlist("Empty", &[]),
        // dbid 7 is in a playlist, 6 is not, but 6 has the plays.
        ipod_playlist("Wavs", &[7, 7]),
        smart,
    ];
    w.ipod_root = fake_ipod(w._ipod_dir.path(), sysinfo, &tracks, &playlists);

    let abs = |n: &str| w.files[n].display().to_string();
    w.env
        .write_playlist("Road", &format!("{}\n{}\n", abs("mp3one"), abs("aacone")));
    w.env
        .write_playlist("Mix", &format!("{}\n{}\n", abs("mp3one"), abs("mp3two")));
    w.env.write_playlist("Empty", "");
    w.env
        .write_playlist("NewFile", &format!("{}\n", abs("wavtwo")));
    playlists::sync_from_disk(w.env.store.conn(), &w.env.cfg, true).unwrap();
    w
}

#[test]
fn reads_the_ipod_database_into_plain_data() {
    let w = world(Some("FirewireGuid: 0x000A27001234ABCD\n"));
    let c = ipod::read(&w.open());
    assert_eq!(c.name.as_deref(), Some("iPod de Test"));
    assert!(c.firewire_guid.unwrap().ends_with("000A27001234ABCD"));
    assert_eq!(c.tracks.len(), 8);
    assert_eq!(c.smart_playlists.len(), 1);
    // Master and smart playlists are not in the regular list.
    let names: Vec<&str> = c.playlists.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["Road", "MIX", "OnlyIpod", "Empty", "Wavs"]);
    // Repeats are kept in the members.
    let wavs = c.playlists.iter().find(|p| p.name == "Wavs").unwrap();
    assert_eq!(wavs.members, vec![7, 7]);

    let t = |dbid: u64| c.tracks.iter().find(|t| t.adopt.dbid == dbid).unwrap();
    // dbid with the high bit survives the trip through libgpod.
    assert_eq!(t(HIGH_BIT).adopt.title.as_deref(), Some("Title1"));
    // Playlist counts: repeats count once, smart and master do not count.
    assert_eq!(t(HIGH_BIT).adopt.playlists, 2);
    assert_eq!(t(3).adopt.playlists, 1);
    assert_eq!(t(7).adopt.playlists, 1);
    assert_eq!(t(6).adopt.playlists, 0);
    assert_eq!(t(6).adopt.playcount, 4);
    // Empty tags are None, not empty strings.
    assert_eq!(t(4).adopt.artist, None);
    assert_eq!(t(4).tags.artist, None);
    assert_eq!(t(4).tags.genre, None);
    assert_eq!(t(4).tags.year, None);
    assert!(t(4).ipod_path.as_deref().unwrap().contains("iPod_Control"));
}

#[test]
fn adoption_matches_the_scenario() {
    let w = world(None);
    let run = adoption::run(w.env.store.conn(), &w.open(), false, &|_, _| {}).unwrap();
    let a = &run.adoption;
    let (mp3one, aacone, untagged) = (w.id("mp3one"), w.id("aacone"), w.id("untagged"));

    // Accepted: same size and duration, tags agree (including the untagged
    // file, whose name stands in for its title).
    let mut want = vec![(HIGH_BIT, mp3one), (3, aacone), (4, untagged)];
    want.sort_unstable();
    assert_eq!(a.accepted, want);

    let by_dbid: HashMap<u64, _> = a.proposals.iter().map(|p| (p.dbid, p)).collect();
    assert_eq!(a.proposals.len(), 4, "{:?}", a.proposals);
    assert_eq!(by_dbid[&2].reason, Reason::WeakTags);
    assert_eq!(by_dbid[&2].default, Some(w.id("mp3two")));
    // Best first; the other same-size file with unrelated tags comes after.
    assert_eq!(by_dbid[&2].candidates, vec![w.id("mp3two"), w.id("mp3one")]);
    assert_eq!(by_dbid[&6].reason, Reason::SharedWithAnotherIpodTrack);
    assert_eq!(by_dbid[&7].reason, Reason::SharedWithAnotherIpodTrack);
    // 7 is in a playlist, 6 has the plays: playlists first.
    assert_eq!(by_dbid[&7].default, Some(w.id("wavthree")));
    assert_eq!(by_dbid[&6].default, None);
    assert_eq!(by_dbid[&8].reason, Reason::TagsOnly);
    assert_eq!(by_dbid[&8].default, Some(w.id("retag")));

    assert_eq!(a.unmatched_ipod, vec![5]);
    let mut lib_only = vec![w.id("wavtwo"), w.id("aacnotonipod"), w.id("aiff")];
    lib_only.sort_unstable();
    assert_eq!(a.unmatched_library, lib_only);

    // Without --accept-proposals only the accepted pairs are mapped.
    assert_eq!(run.mapping, want);
    assert_eq!(run.left.len(), 4);

    // With it, each default is taken, and the contested file goes to 7 only.
    let run = adoption::run(w.env.store.conn(), &w.open(), true, &|_, _| {}).unwrap();
    let mut want = vec![
        (HIGH_BIT, mp3one),
        (2, w.id("mp3two")),
        (3, aacone),
        (4, untagged),
        (7, w.id("wavthree")),
        (8, w.id("retag")),
    ];
    want.sort_unstable();
    assert_eq!(run.mapping, want);
    assert_eq!(run.left.iter().map(|p| p.dbid).collect::<Vec<_>>(), vec![6]);
}

#[test]
fn playlists_are_matched_by_name_and_compared_through_the_mapping() {
    let w = world(None);
    let run = adoption::run(w.env.store.conn(), &w.open(), false, &|_, _| {}).unwrap();
    let m = &run.playlists;
    let by_name: HashMap<&str, _> = m
        .matched
        .iter()
        .map(|m| (m.ipod_name.as_str(), m))
        .collect();
    assert_eq!(m.matched.len(), 3, "{m:?}");
    let road = by_name["Road"];
    assert_eq!(road.library_name, "Road");
    assert!(road.same_contents);
    // Loose match on case; the iPod has one track where the file has two.
    let mix = by_name["MIX"];
    assert_eq!(mix.library_name, "Mix");
    assert!(!mix.same_contents);
    assert!(by_name["Empty"].same_contents);
    assert_eq!(m.unmatched_ipod, vec!["OnlyIpod", "Wavs"]);
    assert_eq!(m.unmatched_library, vec!["NewFile"]);
    assert_eq!(run.ipod.smart_playlists.len(), 1);
}

#[test]
fn saved_hash_is_the_ipods_and_equals_the_librarys_when_tags_agree() {
    let w = world(Some("FirewireGuid: 0x000A27001234ABCD\n"));
    let conn = w.env.store.conn();
    let run = adoption::run(conn, &w.open(), true, &|_, _| {}).unwrap();
    adoption::save(conn, &run, "GUID-A", false).unwrap();

    let dev = devices::device_id(conn, "GUID-A").unwrap().unwrap();
    let maps = devices::track_mappings(conn, dev).unwrap();
    assert_eq!(maps.len(), run.mapping.len());
    let by_dbid: HashMap<u64, _> = maps.iter().map(|m| (m.dbid, m)).collect();

    // Identical tags: the stored hash is the library's, so the first sync
    // sees no tag change.
    for (dbid, file) in [(HIGH_BIT, "mp3one"), (3, "aacone")] {
        let m = by_dbid[&dbid];
        let row = w.env.track(&w.files[file]);
        assert_eq!(m.track_id, row.id);
        assert_eq!(m.synced_tags_hash, row.tags_hash, "dbid {dbid:x}");
        assert_eq!(m.synced_size, row.size);
    }
    // Retagged on the iPod: the stored hash is the iPod's, not the file's.
    let m = by_dbid[&2];
    let row = w.env.track(&w.files["mp3two"]);
    assert_ne!(m.synced_tags_hash, row.tags_hash);
    let ipod_tags = ipod::read(&w.open())
        .tracks
        .into_iter()
        .find(|t| t.adopt.dbid == 2)
        .unwrap()
        .tags;
    assert_eq!(m.synced_tags_hash, ipod_tags.hash());
    // A tags-only match has the iPod's size, one byte off the file.
    let m = by_dbid[&8];
    let row = w.env.track(&w.files["retag"]);
    assert_eq!(m.synced_size, row.size + 1);
}

#[test]
fn save_writes_the_device_and_playlists_then_refuses_a_second_adoption() {
    let w = world(None);
    let conn = w.env.store.conn();
    let run = adoption::run(conn, &w.open(), false, &|_, _| {}).unwrap();
    adoption::save(conn, &run, "GUID-A", false).unwrap();

    let dev = devices::device_id(conn, "GUID-A").unwrap().unwrap();
    let (name, model): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT name, model FROM devices WHERE id = ?1",
            [dev],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(name.as_deref(), Some("iPod de Test"));
    assert_eq!(model, run.ipod.model);
    let mut stmt = conn
        .prepare("SELECT p.name, d.ipod_name FROM device_playlists d JOIN playlists p ON p.id = d.playlist_id WHERE d.device_id = ?1 ORDER BY p.name")
        .unwrap();
    let rows: Vec<(String, String)> = stmt
        .query_map([dev], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        rows,
        vec![
            ("Empty".into(), "Empty".into()),
            ("Mix".into(), "MIX".into()),
            ("Road".into(), "Road".into())
        ]
    );
    let before = devices::track_mappings(conn, dev).unwrap();
    assert_eq!(before.len(), 3);

    // Same iPod again, now with the proposals accepted: refused, nothing changed.
    let run2 = adoption::run(conn, &w.open(), true, &|_, _| {}).unwrap();
    let err = adoption::save(conn, &run2, "GUID-A", false).unwrap_err();
    assert!(
        matches!(&err, Error::AlreadyAdopted(g) if g == "GUID-A"),
        "{err}"
    );
    assert_eq!(devices::track_mappings(conn, dev).unwrap(), before);

    // With force the mapping is replaced, not merged.
    adoption::save(conn, &run2, "GUID-A", true).unwrap();
    let after = devices::track_mappings(conn, dev).unwrap();
    assert_eq!(after.len(), 6);
    assert!(
        before
            .iter()
            .all(|b| after.iter().any(|a| a.dbid == b.dbid))
    );
}

#[test]
fn adoption_only_reads_the_ipod() {
    let w = world(Some("FirewireGuid: 0x000A27001234ABCD\n"));
    let db_file = w.ipod_root.join("iPod_Control/iTunes/iTunesDB");
    let before = fs::read(&db_file).unwrap();
    let tree = |root: &std::path::Path| {
        let mut v: Vec<_> = walk(root);
        v.sort();
        v
    };
    let tree_before = tree(&w.ipod_root);
    let run = adoption::run(w.env.store.conn(), &w.open(), true, &|_, _| {}).unwrap();
    let _ = adoption::report(&run);
    let _ = adoption::summary(&run);
    assert_eq!(fs::read(&db_file).unwrap(), before);
    assert_eq!(tree(&w.ipod_root), tree_before);
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        }
        out.push(p);
    }
    out
}

#[test]
fn an_empty_index_is_an_error_not_an_empty_library() {
    let env = Env::new();
    let dir = tempfile::tempdir().unwrap();
    let root = fake_ipod(dir.path(), None, &[], &[]);
    let db = gpod::Database::open(&root).unwrap();
    let err = adoption::run(env.store.conn(), &db, false, &|_, _| {})
        .err()
        .expect("empty index must fail");
    assert!(matches!(err, Error::EmptyIndex), "{err}");
}

#[test]
fn tracks_missing_from_disk_are_not_candidates() {
    let mut w = world(None);
    let gone = w.id("mp3one");
    fs::remove_file(&w.files["mp3one"]).unwrap();
    w.env.scan();
    let run = adoption::run(w.env.store.conn(), &w.open(), true, &|_, _| {}).unwrap();
    let a = &run.adoption;
    assert!(!a.accepted.iter().any(|&(_, id)| id == gone));
    assert!(!run.mapping.iter().any(|&(_, id)| id == gone));
    assert!(!a.unmatched_library.contains(&gone));
    assert!(a.proposals.iter().all(|p| !p.candidates.contains(&gone)));
    // The iPod track is not accepted anywhere: at best a weak proposal for
    // the other file of the same size.
    assert!(!a.accepted.iter().any(|&(d, _)| d == HIGH_BIT));
}

#[test]
fn report_and_summary_name_what_was_decided() {
    let w = world(None);
    let run = adoption::run(w.env.store.conn(), &w.open(), false, &|_, _| {}).unwrap();
    let report = adoption::report(&run);
    // Stable fragments only: the iPod name, library paths, the unmatched dbid
    // in the 16 digit form the report uses, and the playlist names.
    assert!(report.contains("iPod de Test"), "{report}");
    assert!(report.contains(&w.files["wavthree"].display().to_string()));
    assert!(report.contains(&w.files["wavtwo"].display().to_string()));
    assert!(
        report.contains(&format!("{:016x}", 5)),
        "unmatched iPod track listed"
    );
    assert!(report.contains("NewFile") && report.contains("OnlyIpod"));
    let summary = adoption::summary(&run);
    assert!(summary.contains("iPod tracks: 8"), "{summary}");
    assert!(summary.contains("1 smart playlists"), "{summary}");
}

#[test]
fn model_text_names_the_model_and_capacity() {
    let w = world(Some(
        "ModelNumStr: MC297\nFirewireGuid: 0x000A27001234ABCD\n",
    ));
    let model = ipod::read(&w.open()).model.expect("model");
    assert!(model.contains("Classic"), "{model}");
    assert!(model.contains("160 GB"), "{model}");
}

/// Read only: opens the backup's iTunesDB and reads it into plain data.
#[test]
#[ignore = "needs the iPod backup on /mnt/seagate-1tb"]
fn reads_the_real_backup_database() {
    let path = std::env::var("IPRUNE_BACKUP_ITUNESDB")
        .unwrap_or_else(|_| "/mnt/seagate-1tb/iPod-backup-2026-10/iTunes/iTunesDB".into());
    let db = gpod::Database::open_file(std::path::Path::new(&path)).unwrap();
    let c = ipod::read(&db);
    assert_eq!(c.tracks.len(), db.track_count());
    assert!(c.tracks.len() > 1000);
    assert!(c.name.is_some());
    let dbids: std::collections::HashSet<u64> = c.tracks.iter().map(|t| t.adopt.dbid).collect();
    assert_eq!(dbids.len(), c.tracks.len(), "dbids are unique");
    // Every regular playlist member is a track of the database.
    for p in &c.playlists {
        assert!(p.members.iter().all(|d| dbids.contains(d)), "{}", p.name);
    }
    assert_eq!(
        c.playlists.len() + c.smart_playlists.len() + 1,
        db.playlist_count()
    );
}

#[test]
fn empty_and_blank_ipod_strings_read_as_missing_tags() {
    let dir = tempfile::tempdir().unwrap();
    let blank = IpodSpec {
        dbid: 1,
        size: 10,
        ms: 1000,
        title: "<empty>",
        artist: "<space>",
        album: "",
        plays: 0,
    };
    let root = fake_ipod(dir.path(), None, &[blank], &[]);
    let c = ipod::read(&gpod::Database::open(&root).unwrap());
    let t = &c.tracks[0];
    assert_eq!(
        (
            t.adopt.title.clone(),
            t.adopt.artist.clone(),
            t.adopt.album.clone()
        ),
        (None, None, None)
    );
    assert_eq!(t.tags.title, None);
    assert_eq!(t.tags.artist, None);
    assert_eq!(t.tags.hash(), iprune_core::tags::Tags::default().hash());
}

#[test]
fn model_is_none_when_libgpod_only_has_a_placeholder() {
    for sysinfo in [
        None,
        Some("FirewireGuid: 0x000A27001234ABCD\n"),
        Some("ModelNumStr: ZZZ\n"),
    ] {
        let w = world(sysinfo);
        assert_eq!(ipod::read(&w.open()).model, None, "{sysinfo:?}");
    }
}

// ---- content check against files on a mounted fake iPod

/// Where `make_ipod.c` puts the file of the track at `index` in the manifest.
fn ipod_file(root: &std::path::Path, index: usize) -> PathBuf {
    root.join(format!(
        "iPod_Control/Music/F{:02}/T{:04}.mp3",
        index % 50,
        index
    ))
}

fn put(root: &std::path::Path, index: usize, bytes: &[u8]) -> PathBuf {
    let p = ipod_file(root, index);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, bytes).unwrap();
    p
}

struct Content {
    env: Env,
    dir: tempfile::TempDir,
    root: PathBuf,
    lib: Vec<PathBuf>,
}

/// Library of `n` mp3 files; the iPod has one track per file, in order, with
/// the library's size but a wrong duration (x3) and unrelated tags, so only
/// the content can match them. No iPod file exists yet.
fn content_world(n: usize) -> Content {
    let mut env = Env::new();
    let lib: Vec<PathBuf> = (0..n)
        .map(|i| env.add(&format!("f{i}.mp3"), Kind::Mp3, i as u32 + 1))
        .collect();
    env.scan();
    let dir = tempfile::tempdir().unwrap();
    let tracks: Vec<IpodSpec> = lib
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let t = env.track(p);
            IpodSpec {
                dbid: i as u64 + 1,
                size: fs::metadata(p).unwrap().len(),
                ms: t.info.duration_ms.unwrap() * 3,
                title: "Unrelated",
                artist: "Nobody",
                album: "",
                plays: 0,
            }
        })
        .collect();
    let root = fake_ipod(dir.path(), None, &tracks, &[]);
    Content {
        env,
        dir,
        root,
        lib,
    }
}

impl Content {
    fn run(&self) -> adoption::Run {
        let db = gpod::Database::open(&self.root).unwrap();
        adoption::run(self.env.store.conn(), &db, false, &|_, _| {}).unwrap()
    }
}

#[test]
fn identical_files_are_matched_by_content_despite_wrong_durations_and_tags() {
    let c = content_world(3);
    let _keep = &c.dir;
    // Tracks 1 and 2 hold the library files' bytes. Track 3's file has the
    // same size as its library file but one byte changed.
    put(&c.root, 0, &fs::read(&c.lib[0]).unwrap());
    put(&c.root, 1, &fs::read(&c.lib[1]).unwrap());
    let mut other = fs::read(&c.lib[2]).unwrap();
    let last = other.len() - 1;
    other[last] ^= 0xff;
    put(&c.root, 2, &other);

    let calls = std::cell::RefCell::new(Vec::new());
    let db = gpod::Database::open(&c.root).unwrap();
    let run = adoption::run(c.env.store.conn(), &db, false, &|d, t| {
        calls.borrow_mut().push((d, t))
    })
    .unwrap();

    let id = |i: usize| c.env.track(&c.lib[i]).id;
    assert_eq!(run.adoption.accepted, vec![(1, id(0)), (2, id(1))]);
    assert_eq!(run.adoption.identical, vec![1, 2]);
    assert_eq!(run.mapping, vec![(1, id(0)), (2, id(1))]);
    // The changed file is not taken for the library file on size alone.
    assert_eq!(run.adoption.unmatched_ipod, vec![3]);
    assert!(run.unreadable.is_empty(), "{:?}", run.unreadable);
    assert_eq!(run.compared, 3);
    assert_eq!(*calls.borrow(), vec![(1, 3), (2, 3), (3, 3)]);
}

#[test]
fn settled_tracks_are_not_read_and_unreadable_files_do_not_stop_the_run() {
    let c = content_world(3);
    let _keep = &c.dir;
    // Track 1: file absent. Track 2: file present. Track 3: path is a directory.
    put(&c.root, 1, &fs::read(&c.lib[1]).unwrap());
    fs::create_dir_all(ipod_file(&c.root, 2)).unwrap();
    let run = c.run();

    assert_eq!(run.compared, 1);
    let bad: Vec<u64> = run.unreadable.iter().map(|(d, _)| *d).collect();
    assert_eq!(bad, vec![1, 3]);
    assert!(run.unreadable.iter().all(|(_, why)| !why.is_empty()));
    assert_eq!(run.adoption.identical, vec![2]);
    assert_eq!(run.mapping, vec![(2, c.env.track(&c.lib[1]).id)]);
    assert_eq!(run.adoption.unmatched_ipod, vec![1, 3]);
    let report = adoption::report(&run);
    assert!(report.contains("could not be read"), "{report}");
    assert!(report.contains(&format!("{:016x}", 3)), "{report}");
}

#[test]
fn no_unreadable_section_when_everything_was_read() {
    let c = content_world(1);
    let _keep = &c.dir;
    put(&c.root, 0, &fs::read(&c.lib[0]).unwrap());
    let run = c.run();
    assert!(run.unreadable.is_empty());
    assert!(!adoption::report(&run).contains("could not be read"));
}

#[test]
fn a_database_file_without_a_mountpoint_hashes_nothing() {
    let c = content_world(2);
    let _keep = &c.dir;
    // The files exist, so a pass that wrongly ran would match them.
    for i in 0..2 {
        put(&c.root, i, &fs::read(&c.lib[i]).unwrap());
    }
    let db = gpod::Database::open_file(&c.root.join("iPod_Control/iTunes/iTunesDB")).unwrap();
    assert_eq!(db.mountpoint(), None);
    let called = std::cell::Cell::new(false);
    let run = adoption::run(c.env.store.conn(), &db, false, &|_, _| called.set(true)).unwrap();
    assert_eq!(run.compared, 0);
    assert!(run.unreadable.is_empty());
    assert!(!called.get());
    assert!(run.adoption.accepted.is_empty() && run.adoption.identical.is_empty());
    assert_eq!(run.adoption.unmatched_ipod, vec![1, 2]);
}

#[test]
fn a_content_identical_file_wins_the_shared_library_file_over_a_tag_match() {
    // Track 1 holds the bytes with a wrong duration; track 2 has the library
    // file's tags, size and duration, and is in a playlist.
    let mut env = Env::new();
    let lib = env.add("f.mp3", Kind::Mp3, 1);
    env.scan();
    let row = env.track(&lib);
    let size = fs::metadata(&lib).unwrap().len();
    let title: &'static str = Box::leak(row.display_title().into_owned().into_boxed_str());
    let artist: &'static str =
        Box::leak(row.tags.artist.clone().unwrap_or_default().into_boxed_str());
    let tracks = [
        IpodSpec {
            dbid: 1,
            size,
            ms: row.info.duration_ms.unwrap() * 3,
            title: "Unrelated",
            artist: "Nobody",
            ..IpodSpec::default()
        },
        IpodSpec {
            dbid: 2,
            size,
            ms: row.info.duration_ms.unwrap(),
            title,
            artist,
            ..IpodSpec::default()
        },
    ];
    let dir = tempfile::tempdir().unwrap();
    let root = fake_ipod(dir.path(), None, &tracks, &[ipod_playlist("P", &[2])]);
    put(&root, 0, &fs::read(&lib).unwrap());
    let db = gpod::Database::open(&root).unwrap();
    let run = adoption::run(env.store.conn(), &db, true, &|_, _| {}).unwrap();
    // Track 2 is settled on tags alone, so it is accepted before the content
    // pass and track 1 is compared: both claim the file.
    let defaults: Vec<_> = run
        .adoption
        .proposals
        .iter()
        .map(|p| (p.dbid, p.default))
        .collect();
    assert_eq!(defaults, vec![(1, Some(row.id)), (2, None)]);
    assert_eq!(run.mapping, vec![(1, row.id)]);
}

/// Reading the iPod must not touch its access times (a flash iPod would
/// otherwise be written to by every read).
#[test]
fn hashing_the_ipod_files_leaves_their_access_time_alone() {
    let c = content_world(1);
    let _keep = &c.dir;
    let f = put(&c.root, 0, &fs::read(&c.lib[0]).unwrap());
    let old = "2001-02-03 04:05:06";
    let status = std::process::Command::new("touch")
        .args(["-a", "-d", old])
        .arg(&f)
        .status()
        .unwrap();
    assert!(status.success());
    let before = fs::metadata(&f).unwrap().accessed().unwrap();
    // Control: an ordinary read does move the old access time on this
    // filesystem, otherwise the test below proves nothing.
    let probe = c.dir.path().join("probe");
    fs::write(&probe, b"x").unwrap();
    std::process::Command::new("touch")
        .args(["-a", "-d", old])
        .arg(&probe)
        .status()
        .unwrap();
    let probe_before = fs::metadata(&probe).unwrap().accessed().unwrap();
    let _ = fs::read(&probe).unwrap();
    if fs::metadata(&probe).unwrap().accessed().unwrap() == probe_before {
        eprintln!("skipped: this filesystem does not update access times on read");
        return;
    }

    let run = c.run();
    assert_eq!(run.compared, 1);
    assert_eq!(run.adoption.identical, vec![1]);
    assert_eq!(fs::metadata(&f).unwrap().accessed().unwrap(), before);
}
