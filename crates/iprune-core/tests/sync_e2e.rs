//! `sync::prepare` end to end: a generated library, a fake iPod whose
//! iTunesDB is written by libgpod (`make_ipod.c`) with a SysInfo holding a
//! FirewireGuid, adoption saved through `adoption::save`, then the plan.
//! Nothing here touches a real iPod or the real index.

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use common::{Env, IpodSpec, Kind, PlaylistSpec, fake_ipod, ipod_playlist};
use iprune_core::config::Scope as ConfigScope;
use iprune_core::plan::{
    Member, Plan, PlaylistDeleteReason, PlaylistOp, PlaylistSkipReason, RemoveReason, Space,
    SpaceCheck,
};
use iprune_core::sync::{self, Prepared};
use iprune_core::{Error, adoption, ipod, playlists};

const SYSINFO: &str = "FirewireGuid: 0x000A27001234ABCD\n";
const D1: u64 = 0xFFFF_FFFF_FFFF_FFF0;
const D2: u64 = 2;
const D3: u64 = 3;

struct World {
    env: Env,
    files: HashMap<&'static str, PathBuf>,
    _ipod_dir: tempfile::TempDir,
    root: PathBuf,
}

impl World {
    fn id(&self, name: &str) -> i64 {
        self.env.track(&self.files[name]).id
    }

    fn open(&self) -> gpod::Database {
        gpod::Database::open(&self.root).unwrap()
    }

    fn abs(&self, name: &str) -> String {
        self.files[name].display().to_string()
    }

    fn prepare(&self) -> iprune_core::Result<Prepared> {
        sync::prepare(self.env.store.conn(), &self.env.cfg, &self.open())
    }

    fn plan(&self) -> Plan {
        self.prepare().expect("prepare succeeds").plan
    }

    fn adopt(&self) {
        let conn = self.env.store.conn();
        let db = self.open();
        let run = adoption::run(conn, &db, false, &|_, _| {}).unwrap();
        assert_eq!(run.mapping.len(), 3, "clean adoption: {:?}", run.left);
        let guid = ipod::read(&db).firewire_guid.unwrap();
        adoption::save(conn, &run, &guid, false).unwrap();
    }

    fn reload_playlists(&self) {
        playlists::sync_from_disk(self.env.store.conn(), &self.env.cfg, true).unwrap();
    }
}

/// Library: mp3 one, aac one, wav three (plus whatever a test adds). The
/// iPod holds those three under D1..D3 with playlists Road [D1, D2] and
/// Mix [D3], and a Podcasts playlist. Files Road.m3u8 and Mix.m3u8 match.
fn world(sysinfo: Option<&str>, extra_ipod: Vec<PlaylistSpec>) -> World {
    let mut env = Env::new();
    let mut files = HashMap::new();
    files.insert("one", env.add("one.mp3", Kind::Mp3, 1));
    files.insert("two", env.add("two.m4a", Kind::Aac, 1));
    files.insert("three", env.add("three.wav", Kind::Wav, 3));
    env.scan();
    let dir = tempfile::tempdir().unwrap();
    let spec = |name: &str, dbid, title, artist| {
        let t = env.track(&files[name]);
        IpodSpec {
            dbid,
            size: fs::metadata(&t.path).unwrap().len(),
            ms: t.info.duration_ms.unwrap(),
            title,
            artist,
            album: "",
            plays: 0,
        }
    };
    let tracks = [
        spec("one", D1, "Title1", "Artist1"),
        spec("two", D2, "Title1", "Artist1"),
        spec("three", D3, "Title3", "Artist3"),
    ];
    let mut lists = vec![
        ipod_playlist("Road", &[D1, D2]),
        ipod_playlist("Mix", &[D3]),
    ];
    lists.extend(extra_ipod);
    let root = fake_ipod(dir.path(), sysinfo, &tracks, &lists);
    let w = World {
        env,
        files,
        _ipod_dir: dir,
        root,
    };
    w.env
        .write_playlist("Road", &format!("{}\n{}\n", w.abs("one"), w.abs("two")));
    w.env
        .write_playlist("Mix", &format!("{}\n", w.abs("three")));
    w.reload_playlists();
    w
}

fn podcasts() -> PlaylistSpec {
    PlaylistSpec {
        name: "Podcasts",
        smart: false,
        podcasts: true,
        members: vec![],
    }
}

fn adopted() -> World {
    let w = world(Some(SYSINFO), vec![]);
    w.adopt();
    w
}

/// A mono 8 kHz 8 bit PCM WAV of `secs` seconds of silence.
fn wav(secs: u32) -> Vec<u8> {
    let n = 8000 * secs;
    let mut b = b"RIFF".to_vec();
    b.extend((36 + n).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes()); // PCM
    b.extend(1u16.to_le_bytes()); // mono
    b.extend(8000u32.to_le_bytes());
    b.extend(8000u32.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(8u16.to_le_bytes());
    b.extend(b"data");
    b.extend(n.to_le_bytes());
    b.extend(vec![128u8; n as usize]);
    b
}

// ---- refusing

#[test]
fn an_ipod_that_was_not_adopted_is_refused() {
    let w = world(Some(SYSINFO), vec![]);
    let err = w.prepare().err().expect("must be refused");
    assert!(
        matches!(err, Error::NotAdopted(ref g) if g.ends_with("000A27001234ABCD")),
        "{err:?}"
    );
}

#[test]
fn a_device_known_but_with_no_mapped_track_is_refused() {
    let w = world(Some(SYSINFO), vec![]);
    let guid = ipod::read(&w.open()).firewire_guid.unwrap();
    iprune_core::store::devices::upsert_device(w.env.store.conn(), &guid, None, None).unwrap();
    let err = w.prepare().err().expect("must be refused");
    assert!(matches!(err, Error::NotAdopted(_)), "{err:?}");
}

#[test]
fn an_ipod_without_a_firewire_guid_is_refused() {
    let w = world(None, vec![]);
    let err = w.prepare().err().expect("must be refused");
    assert!(matches!(err, Error::NoFirewireGuid), "{err:?}");
}

// ---- planning

#[test]
fn a_library_identical_to_the_ipod_gives_an_empty_plan() {
    let w = adopted();
    let p = w.prepare().unwrap();
    assert!(p.plan.is_empty(), "{:?}", p.plan);
    assert!(p.plan.unmanaged.tracks.is_empty() && p.plan.unmanaged.playlists.is_empty());
    assert!(p.plan.skipped.is_empty() && p.plan.entries_skipped.is_empty());
    assert!(p.plan.stale_mappings.is_empty());
    assert_eq!(p.plan.expected.track_count, 3);
    let names: Vec<&str> = p
        .plan
        .expected
        .playlists
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(names, vec!["Mix", "Road"]);
    assert!(p.guid.ends_with("000A27001234ABCD"));
    // The space of the temp dir holding the fake iPod was read.
    let d = p.plan.space.device.expect("space read from the mountpoint");
    assert!(d.capacity > 0 && d.free <= d.capacity);
    assert!(p.plan.space.fits);
    assert_eq!(p.library.len(), 3);
}

#[test]
fn preparing_twice_gives_the_same_plan_apart_from_the_free_space() {
    let w = adopted();
    w.env
        .write_playlist("New", &format!("{}\n{}\n", w.abs("three"), w.abs("one")));
    w.reload_playlists();
    // Free space moves under the test's feet; everything else must not.
    let strip = |mut p: Plan| {
        p.space.device = None;
        p
    };
    assert_eq!(strip(w.plan()), strip(w.plan()));
}

#[test]
fn one_new_library_file_gives_one_copy_and_nothing_else() {
    let mut w = adopted();
    let new = w.env.add("four.mp3", Kind::Mp3, 4);
    w.env.scan();
    let p = w.plan();
    assert_eq!(p.copy.len(), 1, "{p:?}");
    assert_eq!(p.copy[0].path, new);
    assert_eq!(p.copy[0].size, fs::metadata(&new).unwrap().len());
    assert_eq!(p.copy[0].track_id, w.env.track(&new).id);
    assert!(p.replace.is_empty() && p.remove.is_empty() && p.update_tags.is_empty());
    assert!(p.playlists.is_empty(), "{:?}", p.playlists);
    assert_eq!(p.expected.track_count, 4);
    assert_eq!(p.space.needed, p.copy[0].size as i64);
    assert!(!p.is_empty());
}

#[test]
fn an_edited_playlist_file_gives_set_members() {
    let w = adopted();
    w.env
        .write_playlist("Road", &format!("{}\n{}\n", w.abs("two"), w.abs("one")));
    w.reload_playlists();
    let p = w.plan();
    assert_eq!(
        p.playlists,
        vec![PlaylistOp::SetMembers {
            playlist_id: w.env.playlist_id("Road").unwrap(),
            ipod_id: p
                .playlists
                .iter()
                .find_map(|o| match o {
                    PlaylistOp::SetMembers { ipod_id, .. } => Some(*ipod_id),
                    _ => None,
                })
                .unwrap(),
            name: "Road".into(),
            members: vec![Member::Track(D2), Member::Track(D1)],
        }]
    );
    assert!(p.copy.is_empty() && p.remove.is_empty() && p.update_tags.is_empty());
}

#[test]
fn a_new_file_added_to_a_playlist_is_copied_and_set_as_a_new_member() {
    let mut w = adopted();
    let new = w.env.add("four.mp3", Kind::Mp3, 4);
    w.env.scan();
    w.env
        .write_playlist("Mix", &format!("{}\n{}\n", w.abs("three"), new.display()));
    w.reload_playlists();
    let p = w.plan();
    let new_id = w.env.track(&new).id;
    assert_eq!(p.copy.len(), 1);
    match p.playlists.as_slice() {
        [PlaylistOp::SetMembers { name, members, .. }] => {
            assert_eq!(name, "Mix");
            assert_eq!(members, &vec![Member::Track(D3), Member::New(new_id)]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_file_gone_from_the_library_is_removed_from_the_ipod_and_its_playlist() {
    let mut w = adopted();
    fs::remove_file(&w.files["two"]).unwrap();
    w.env.scan();
    let p = w.plan();
    assert_eq!(p.remove.len(), 1, "{p:?}");
    assert_eq!(p.remove[0].dbid, D2);
    assert_eq!(p.remove[0].track_id, Some(w.id("two")));
    assert_eq!(p.remove[0].reason, RemoveReason::MissingFromLibrary);
    // The file still lists it, unresolved or missing: left out and reported.
    assert_eq!(p.entries_skipped.len(), 1, "{:?}", p.entries_skipped);
    assert_eq!(p.entries_skipped[0].playlist, "Road");
    assert_eq!(p.entries_skipped[0].position, 2);
    assert!(p.playlists.iter().any(|o| matches!(
        o,
        PlaylistOp::SetMembers { name, members, .. }
            if name == "Road" && *members == vec![Member::Track(D1)]
    )));
    assert_eq!(p.expected.track_count, 2);
}

#[test]
fn changed_audio_is_a_replace_and_the_peak_counts_both_files() {
    let w = adopted();
    // A 3 second recording where there was a 1 second one.
    fs::write(&w.files["three"], wav(3)).unwrap();
    let mut w = w;
    w.env.scan();
    let p = w.plan();
    assert_eq!(p.replace.len(), 1, "{p:?}");
    let r = &p.replace[0];
    assert_eq!((r.track_id, r.old_dbid), (w.id("three"), D3));
    assert!(p.copy.is_empty() && p.remove.is_empty());
    // Written before the old one goes: the peak is the whole new file.
    assert_eq!(p.space.peak, r.size as i64);
    assert_eq!(p.space.needed, r.size as i64 - r.old_size as i64);
    assert_ne!(p.space.peak, p.space.needed);
    let text = sync::describe(&w.prepare().unwrap(), false);
    assert!(text.contains("at the peak"), "{text}");
    assert!(text.contains("once done"), "{text}");
}

// Prevents: a playlist file deleted after adoption failing the index with a
// foreign key error, or the iPod playlist being kept for ever.
#[test]
fn a_deleted_playlist_file_deletes_its_ipod_playlist() {
    let w = adopted();
    let before = w.plan();
    assert!(before.is_empty());
    fs::remove_file(w.env.playlist_file("Mix")).unwrap();
    w.reload_playlists();
    let p = w.plan();
    match p.playlists.as_slice() {
        [
            PlaylistOp::Delete {
                playlist_id: Some(_),
                name,
                reason: PlaylistDeleteReason::FileDeleted,
                ..
            },
        ] => assert_eq!(name, "Mix"),
        other => panic!("{other:?}"),
    }
    // The track in it stays: only the playlist goes.
    assert!(p.remove.is_empty());
    assert!(p.unmanaged.playlists.is_empty());
    assert!(p.expected.playlists.iter().all(|e| e.name != "Mix"));
}

#[test]
fn renaming_a_playlist_onto_a_deleted_ones_name_renames_one_and_deletes_the_other() {
    let w = adopted();
    let conn = w.env.store.conn();
    let road_ipod = ipod::read(&w.open())
        .playlists
        .iter()
        .find(|p| p.name == "Road")
        .unwrap()
        .id;
    let mix_ipod = ipod::read(&w.open())
        .playlists
        .iter()
        .find(|p| p.name == "Mix")
        .unwrap()
        .id;
    playlists::delete(conn, &w.env.cfg, "Road").unwrap();
    playlists::rename(conn, &w.env.cfg, "Mix", "Road").unwrap();
    let p = w.plan();
    let mix_id = w.env.playlist_id("Road").unwrap();
    assert_eq!(
        p.playlists,
        vec![
            PlaylistOp::Delete {
                playlist_id: p.playlists.iter().find_map(|o| match o {
                    PlaylistOp::Delete { playlist_id, .. } => *playlist_id,
                    _ => None,
                }),
                ipod_id: road_ipod,
                name: "Road".into(),
                reason: PlaylistDeleteReason::FileDeleted,
            },
            PlaylistOp::Rename {
                playlist_id: mix_id,
                ipod_id: mix_ipod,
                from: "Mix".into(),
                to: "Road".into(),
            },
        ]
    );
    // The deleted playlist's id is not the renamed one's.
    assert!(
        matches!(&p.playlists[0], PlaylistOp::Delete { playlist_id: Some(id), .. } if *id != mix_id)
    );
}

#[test]
fn two_ipod_playlists_with_one_name_keep_the_mapped_one_by_id() {
    // The mapped playlist is the second "Mix" on the iPod.
    let w = world(Some(SYSINFO), vec![ipod_playlist("Mix", &[])]);
    // Adoption maps the first "Mix" by name, in iPod order. Re-map by hand to
    // the second one, as a sync of an earlier version would have recorded.
    let conn = w.env.store.conn();
    let db = w.open();
    let run = adoption::run(conn, &db, false, &|_, _| {}).unwrap();
    let guid = ipod::read(&db).firewire_guid.unwrap();
    adoption::save(conn, &run, &guid, false).unwrap();
    let dev = iprune_core::store::devices::device_id(conn, &guid)
        .unwrap()
        .unwrap();
    let c = ipod::read(&db);
    let mixes: Vec<u64> = c
        .playlists
        .iter()
        .filter(|p| p.name == "Mix")
        .map(|p| p.id)
        .collect();
    assert_eq!(mixes.len(), 2);
    let mut maps = iprune_core::store::devices::playlist_mappings(conn, dev).unwrap();
    let mix = maps.iter_mut().find(|m| m.ipod_name == "Mix").unwrap();
    mix.ipod_id = Some(mixes[1]);
    let tracks = iprune_core::store::devices::track_mappings(conn, dev).unwrap();
    iprune_core::store::devices::replace_mappings(conn, dev, &tracks, &maps).unwrap();

    let p = w.plan();
    // The second Mix holds nothing, so it is the one set to [D3]. The first
    // already holds [D3], but it is not ours: unmanaged and untouched.
    match p.playlists.as_slice() {
        [PlaylistOp::SetMembers { ipod_id, name, .. }] => {
            assert_eq!(*ipod_id, mixes[1]);
            assert_eq!(name, "Mix");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(p.unmanaged.playlists, vec!["Mix"]);
}

// ---- the Podcasts playlist

#[test]
fn the_podcasts_playlist_is_never_planned_not_even_for_removal() {
    let w = world(Some(SYSINFO), vec![podcasts()]);
    w.adopt();
    let c = ipod::read(&w.open());
    assert!(
        c.playlists.iter().all(|p| p.name != "Podcasts"),
        "{:?}",
        c.playlists
    );
    let mut w = w;
    w.env.cfg.sync.remove_unmanaged = true;
    let p = w.plan();
    assert!(p.is_empty(), "{p:?}");
    assert!(p.unmanaged.playlists.is_empty());
}

// ---- smart playlist exports

fn smart(name: &'static str) -> PlaylistSpec {
    PlaylistSpec {
        name,
        smart: true,
        podcasts: false,
        members: vec![D1],
    }
}

/// The world plus a file `<name>.m3u8` holding track one, then adopted.
fn adopted_with_export(ipod_extra: Vec<PlaylistSpec>, name: &str) -> World {
    let w = world(Some(SYSINFO), ipod_extra);
    w.env.write_playlist(name, &format!("{}\n", w.abs("one")));
    w.reload_playlists();
    w.adopt();
    w
}

#[test]
fn a_file_named_like_a_smart_playlist_is_skipped_not_created() {
    let w = adopted_with_export(vec![smart("Top rated")], "Top rated");
    let c = ipod::read(&w.open());
    assert_eq!(c.smart_playlists, vec!["Top rated".to_owned()]);
    let p = w.plan();
    assert_eq!(p.playlists_skipped.len(), 1, "{:?}", p.playlists_skipped);
    assert_eq!(p.playlists_skipped[0].name, "Top rated");
    assert_eq!(
        p.playlists_skipped[0].reason,
        PlaylistSkipReason::SmartOnIpod
    );
    assert!(p.playlists.is_empty(), "{:?}", p.playlists);
    assert!(p.entries_skipped.is_empty());
    let expected: Vec<&str> = p
        .expected
        .playlists
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(expected, vec!["Mix", "Road"]);
    // Nothing else to do: the skip is the only thing in the plan.
    assert!(p.is_empty(), "{p:?}");
    // The smart playlist itself is untouched and not offered for removal.
    assert!(p.unmanaged.playlists.is_empty());
}

#[test]
fn the_same_file_is_created_when_the_ipod_has_no_such_smart_playlist() {
    let w = adopted_with_export(vec![smart("Something else")], "Top rated");
    let p = w.plan();
    assert!(p.playlists_skipped.is_empty(), "{:?}", p.playlists_skipped);
    assert!(
        p.playlists
            .iter()
            .any(|o| matches!(o, PlaylistOp::Create { name, .. } if name == "Top rated")),
        "{:?}",
        p.playlists
    );
}

#[test]
fn a_skipped_export_stays_skipped_on_the_next_prepare() {
    let w = adopted_with_export(vec![smart("Top rated")], "Top rated");
    let (a, b) = (w.plan(), w.plan());
    assert_eq!(a.playlists_skipped, b.playlists_skipped);
    assert_eq!(a.playlists_skipped.len(), 1);
}

#[test]
fn describe_lists_the_playlist_files_not_synced() {
    let w = adopted_with_export(vec![smart("Top rated")], "Top rated");
    // The section is listed in the verbose output; the summary line always.
    let brief = sync::describe(&w.prepare().unwrap(), false);
    assert!(brief.contains("skipped: 0 tracks, 1 playlist,"), "{brief}");
    assert!(!brief.contains("Playlist files not synced"), "{brief}");
    let text = sync::describe(&w.prepare().unwrap(), true);
    assert!(text.contains("Playlist files not synced"), "{text}");
    assert!(text.contains("Top rated"), "{text}");
    // The summary line counts it: 0 tracks, 1 playlist, 0 entries.
    assert!(text.contains("skipped: 0 tracks, 1 playlist,"), "{text}");
    assert!(text.contains("playlists: 0 new"), "{text}");
}

#[test]
fn describe_has_no_such_section_when_nothing_was_skipped() {
    let w = adopted();
    let text = sync::describe(&w.prepare().unwrap(), true);
    assert!(!text.contains("Playlist files not synced"), "{text}");
    assert!(!text.contains("skipped:"), "{text}");
}

// ---- scope

#[test]
fn a_playlists_scope_with_no_playlists_is_refused() {
    let mut w = adopted();
    w.env.cfg.sync.scope = ConfigScope::Playlists;
    w.env.cfg.sync.playlists = vec![];
    let err = w.prepare().err().expect("must be refused");
    // The empty list is its own error, not "holds no track".
    match err {
        Error::BadScope(m) => assert!(m.contains("empty"), "{m}"),
        e => panic!("{e:?}"),
    }
}

#[test]
fn a_playlists_scope_naming_a_missing_playlist_is_refused() {
    let mut w = adopted();
    w.env.cfg.sync.scope = ConfigScope::Playlists;
    w.env.cfg.sync.playlists = vec!["Road".into(), "Typo".into()];
    let err = w.prepare().err().expect("must be refused");
    match err {
        Error::BadScope(m) => assert!(m.contains("Typo") && !m.contains("Road,"), "{m}"),
        e => panic!("{e:?}"),
    }
}

#[test]
fn a_playlists_scope_whose_playlists_hold_no_present_track_is_refused() {
    let mut w = adopted();
    w.env.cfg.sync.scope = ConfigScope::Playlists;
    w.env.cfg.sync.playlists = vec!["Empty".into()];
    // Emptied.
    w.env.write_playlist("Empty", "");
    w.reload_playlists();
    let err = w.prepare().err().expect("emptied playlist");
    assert!(matches!(err, Error::BadScope(_)), "{err:?}");

    // Entries that point at nothing.
    w.env
        .write_playlist("Empty", &format!("{}/nowhere.mp3\n", w.env.music.display()));
    w.reload_playlists();
    let err = w.prepare().err().expect("unresolved entries");
    assert!(matches!(err, Error::BadScope(_)), "{err:?}");

    // Entries whose files are all gone.
    w.env.cfg.sync.playlists = vec!["Mix".into()];
    fs::remove_file(&w.files["three"]).unwrap();
    w.env.scan();
    let err = w.prepare().err().expect("missing files");
    assert!(matches!(err, Error::BadScope(_)), "{err:?}");
}

#[test]
fn a_valid_playlists_scope_plans_only_those_playlists() {
    let mut w = adopted();
    w.env.add("four.mp3", Kind::Mp3, 4);
    w.env.scan();
    w.env.cfg.sync.scope = ConfigScope::Playlists;
    w.env.cfg.sync.playlists = vec!["Road".into()];
    let p = w.plan();
    // Mix and its track leave the scope; the new file is in no listed playlist.
    assert!(p.copy.is_empty(), "{:?}", p.copy);
    assert_eq!(p.remove.len(), 1);
    assert_eq!(
        (p.remove[0].dbid, p.remove[0].reason),
        (D3, RemoveReason::OutOfScope)
    );
    assert!(p.playlists.iter().any(|o| matches!(
        o,
        PlaylistOp::Delete { name, reason: PlaylistDeleteReason::OutOfScope, .. } if name == "Mix"
    )));
}

// ---- describe, space, bytes

#[test]
fn describe_mentions_the_counts() {
    let mut w = adopted();
    let new = w.env.add("four.mp3", Kind::Mp3, 4);
    w.env.scan();
    w.env.write_playlist("Road", &format!("{}\n", w.abs("one")));
    w.reload_playlists();
    let p = w.prepare().unwrap();
    let text = sync::describe(&p, false);
    assert!(text.contains("1 track to copy"), "{text}");
    assert!(text.contains("0 tracks to remove"), "{text}");
    assert!(text.contains("1 changed"), "{text}");
    assert!(text.contains("0 new"), "{text}");
    assert!(text.contains("iPod de Test"), "{text}");
    assert!(text.contains("000A27001234ABCD"), "{text}");
    assert!(
        !text.contains(&new.display().to_string()),
        "not verbose: {text}"
    );
    // The net and the peak are equal here: one value.
    assert!(!text.contains("at the peak"), "{text}");
    assert!(!text.contains("DOES NOT FIT"), "{text}");

    let verbose = sync::describe(&p, true);
    assert!(verbose.contains(&new.display().to_string()), "{verbose}");
    assert!(verbose.contains("Road"), "{verbose}");
}

#[test]
fn describe_on_an_empty_plan_says_nothing_to_do_in_counts() {
    let w = adopted();
    let text = sync::describe(&w.prepare().unwrap(), true);
    assert!(text.contains("0 tracks to copy"), "{text}");
    assert!(text.contains("0 tracks to remove"), "{text}");
    assert!(
        text.contains("0 new, 0 changed, 0 renamed, 0 removed"),
        "{text}"
    );
    assert!(!text.contains("=="), "no sections in an empty plan: {text}");
}

fn bare(space: SpaceCheck) -> Prepared {
    Prepared {
        plan: Plan {
            space,
            ..Plan::default()
        },
        ipod: Default::default(),
        guid: "G".into(),
        tracks_map: vec![],
        playlists_map: vec![],
        library: HashMap::new(),
        recovered: 0,
    }
}

#[test]
fn describe_shows_the_peak_only_when_it_differs_from_the_net() {
    let dev = Some(Space {
        capacity: 10_000_000,
        free: 5_000_000,
    });
    let same = sync::describe(
        &bare(SpaceCheck {
            needed: 2_000_000,
            peak: 2_000_000,
            device: dev,
            fits: true,
        }),
        false,
    );
    let space_line = same.lines().find(|l| l.contains("space:")).unwrap();
    assert!(space_line.contains("needs 2.0 MB"), "{space_line}");
    assert!(
        !space_line.contains("peak") && !space_line.contains("once done"),
        "{space_line}"
    );

    let differs = sync::describe(
        &bare(SpaceCheck {
            needed: -1_000_000,
            peak: 3_000_000,
            device: dev,
            fits: true,
        }),
        false,
    );
    let line = differs.lines().find(|l| l.contains("space:")).unwrap();
    assert!(line.contains("needs 3.0 MB at the peak"), "{line}");
    assert!(line.contains("frees 1.0 MB once done"), "{line}");
    assert!(line.contains("5.0 MB free"), "{line}");

    let full = sync::describe(
        &bare(SpaceCheck {
            needed: 9_000_000,
            peak: 9_000_000,
            device: dev,
            fits: false,
        }),
        false,
    );
    assert!(full.contains("DOES NOT FIT"), "{full}");
}

#[test]
fn space_reads_a_real_directory_and_fails_on_a_missing_one() {
    let dir = tempfile::tempdir().unwrap();
    let s = sync::space(dir.path()).unwrap();
    assert!(s.capacity > 0);
    assert!(s.free <= s.capacity);
    let err = sync::space(&dir.path().join("nope")).unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "{err:?}");
}

#[test]
fn bytes_use_decimal_units() {
    for (n, want) in [
        (0, "0 B"),
        (999, "999 B"),
        (1_000, "1.0 kB"),
        (1_500, "1.5 kB"),
        (999_999, "1000.0 kB"),
        (1_000_000, "1.0 MB"),
        (2_500_000, "2.5 MB"),
        (1_000_000_000, "1.0 GB"),
        (160_000_000_000, "160.0 GB"),
    ] {
        assert_eq!(sync::bytes(n), want, "{n}");
    }
}
