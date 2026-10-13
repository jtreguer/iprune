//! Playlist tombstones: a deleted playlist file whose playlist is mapped to an
//! iPod playlist stays in the index, hidden, so the next sync can delete the
//! iPod playlist. Plus migration 0003 on a version 2 database.

mod common;

use common::{Env, Kind};
use iprune_core::store::devices::{self, PlaylistMapping};
use iprune_core::store::{self, Store};
use iprune_core::{adopt, playlists};
use rusqlite::Connection;

fn env() -> Env {
    let mut env = Env::new();
    env.add("a.mp3", Kind::Mp3, 1);
    env.add("b.mp3", Kind::Mp3, 2);
    env.scan();
    env
}

fn pm(playlist_id: i64, ipod_id: u64, name: &str) -> PlaylistMapping {
    PlaylistMapping {
        playlist_id,
        ipod_id: Some(ipod_id),
        ipod_name: name.into(),
    }
}

/// Writes the playlist files `(name, track file)` and loads them.
fn load(env: &Env, lists: &[(&str, &str)]) {
    for (name, file) in lists {
        env.write_playlist(name, &format!("{}\n", env.music.join(file).display()));
    }
    playlists::sync_from_disk(env.store.conn(), &env.cfg, true).unwrap();
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn live_names(conn: &Connection) -> Vec<String> {
    store::playlists(conn)
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect()
}

/// An env where "mix" is mapped on a device as iPod playlist 0x77 named "Mix".
fn mapped_mix() -> (Env, i64, i64) {
    let env = env();
    load(&env, &[("mix", "a.mp3"), ("other", "b.mp3")]);
    let conn = env.store.conn();
    let id = env.playlist_id("mix").unwrap();
    let dev = devices::upsert_device(conn, "G1", None, None).unwrap();
    devices::replace_mappings(conn, dev, &[], &[pm(id, 0x77, "Mix")]).unwrap();
    (env, id, dev)
}

// Prevents: deleting a playlist file that is mapped to an iPod playlist
// failing on the foreign key of device_playlists, which made every later
// scan or sync of the playlists folder fail.
#[test]
fn deleting_a_mapped_playlist_file_keeps_a_hidden_tombstone() {
    let (env, id, dev) = mapped_mix();
    let conn = env.store.conn();
    std::fs::remove_file(env.playlist_file("mix")).unwrap();

    let report = playlists::sync_from_disk(conn, &env.cfg, false).unwrap();
    assert_eq!(report.removed, vec!["mix"]);

    // Hidden everywhere the user looks.
    assert_eq!(live_names(conn), vec!["other"]);
    let summaries: Vec<String> = store::playlist_summaries(conn)
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(summaries, vec!["other"]);
    let lib: Vec<String> = devices::library_playlists(conn)
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(lib, vec!["other"]);
    assert!(env.playlist_id("mix").is_none());

    // Kept for the sync, with its mapping and no members.
    assert_eq!(
        store::removed_playlists(conn).unwrap(),
        vec![(id, "mix".to_owned())]
    );
    assert_eq!(
        devices::playlist_mappings(conn, dev).unwrap(),
        vec![pm(id, 0x77, "Mix")]
    );
    assert_eq!(
        count(
            conn,
            &format!("SELECT COUNT(*) FROM playlist_items WHERE playlist_id = {id}")
        ),
        0
    );

    // A second scan does not trip over it.
    let again = playlists::sync_from_disk(conn, &env.cfg, true).unwrap();
    assert!(again.removed.is_empty(), "{:?}", again.removed);
    assert_eq!(store::removed_playlists(conn).unwrap().len(), 1);
}

#[test]
fn the_playlist_delete_command_leaves_a_tombstone_too() {
    let (env, id, _) = mapped_mix();
    let conn = env.store.conn();
    playlists::delete(conn, &env.cfg, "mix").unwrap();
    assert!(!env.playlist_file("mix").exists());
    assert_eq!(
        store::removed_playlists(conn).unwrap(),
        vec![(id, "mix".to_owned())]
    );
    assert_eq!(live_names(conn), vec!["other"]);
}

#[test]
fn deleting_an_unmapped_playlist_file_really_deletes_it() {
    let (env, _, _) = mapped_mix();
    let conn = env.store.conn();
    let other = env.playlist_id("other").unwrap();
    std::fs::remove_file(env.playlist_file("other")).unwrap();
    let report = playlists::sync_from_disk(conn, &env.cfg, false).unwrap();
    assert_eq!(report.removed, vec!["other"]);
    assert_eq!(
        count(
            conn,
            &format!("SELECT COUNT(*) FROM playlists WHERE id = {other}")
        ),
        0
    );
    assert_eq!(
        count(
            conn,
            &format!("SELECT COUNT(*) FROM playlist_items WHERE playlist_id = {other}")
        ),
        0
    );
    assert!(store::removed_playlists(conn).unwrap().is_empty());
    // The mapped one is untouched.
    assert_eq!(live_names(conn), vec!["mix"]);
}

#[test]
fn deleting_twice_keeps_one_tombstone() {
    let (env, id, _) = mapped_mix();
    let conn = env.store.conn();
    std::fs::remove_file(env.playlist_file("mix")).unwrap();
    store::delete_playlist(conn, "mix").unwrap();
    store::delete_playlist(conn, "mix").unwrap();
    assert_eq!(
        store::removed_playlists(conn).unwrap(),
        vec![(id, "mix".to_owned())]
    );
}

// Prevents: a file that comes back getting a new id, which would orphan the
// mapping and re-create the iPod playlist next to the old one.
#[test]
fn a_file_coming_back_revives_the_same_id_and_mapping() {
    let (env, id, dev) = mapped_mix();
    let conn = env.store.conn();
    std::fs::remove_file(env.playlist_file("mix")).unwrap();
    playlists::sync_from_disk(conn, &env.cfg, false).unwrap();
    assert_eq!(store::removed_playlists(conn).unwrap().len(), 1);

    load(&env, &[("mix", "b.mp3")]);
    assert_eq!(env.playlist_id("mix"), Some(id));
    assert!(store::removed_playlists(conn).unwrap().is_empty());
    assert_eq!(
        devices::playlist_mappings(conn, dev).unwrap(),
        vec![pm(id, 0x77, "Mix")]
    );
    let lib = devices::library_playlists(conn).unwrap();
    let mix = lib.iter().find(|p| p.name == "mix").unwrap();
    assert_eq!(mix.id, id);
    assert_eq!(mix.members.len(), 1);
    assert!(mix.members[0].is_some());
}

#[test]
fn creating_a_playlist_over_a_tombstone_revives_it_too() {
    let (env, id, dev) = mapped_mix();
    let conn = env.store.conn();
    playlists::delete(conn, &env.cfg, "mix").unwrap();
    playlists::create(conn, &env.cfg, "mix").unwrap();
    assert_eq!(env.playlist_id("mix"), Some(id));
    assert!(store::removed_playlists(conn).unwrap().is_empty());
    assert_eq!(devices::playlist_mappings(conn, dev).unwrap().len(), 1);
}

// Prevents: renaming a playlist onto a deleted one's name failing on the
// unique name, or silently dropping the tombstone and with it the only record
// of which iPod playlist to delete.
#[test]
fn renaming_onto_a_tombstones_name_sets_the_tombstone_aside_and_keeps_its_mapping() {
    let (env, mix, dev) = mapped_mix();
    let conn = env.store.conn();
    let other = env.playlist_id("other").unwrap();
    devices::replace_mappings(
        conn,
        dev,
        &[],
        &[pm(mix, 0x77, "Mix"), pm(other, 0x78, "other")],
    )
    .unwrap();
    playlists::delete(conn, &env.cfg, "mix").unwrap();

    playlists::rename(conn, &env.cfg, "other", "mix").unwrap();

    // The renamed playlist keeps its id, and so its iPod playlist.
    assert_eq!(env.playlist_id("mix"), Some(other));
    assert!(env.playlist_file("mix").exists());
    // The tombstone is still there under an internal name.
    let removed = store::removed_playlists(conn).unwrap();
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0].0, mix);
    assert_ne!(removed[0].1, "mix");
    assert!(removed[0].1.starts_with('.'), "{removed:?}");
    // Both mappings survive.
    assert_eq!(
        devices::playlist_mappings(conn, dev).unwrap(),
        vec![pm(mix, 0x77, "Mix"), pm(other, 0x78, "other")]
    );
    // The internal name is not a playlist anyone sees.
    assert_eq!(live_names(conn), vec!["mix"]);
    let lib: Vec<i64> = devices::library_playlists(conn)
        .unwrap()
        .into_iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(lib, vec![other]);

    // The name is free for another round: delete it and rename onto it again.
    playlists::delete(conn, &env.cfg, "mix").unwrap();
    assert_eq!(store::removed_playlists(conn).unwrap().len(), 2);
}

#[test]
fn replace_mappings_drops_tombstones_no_device_maps_any_more() {
    let (env, mix, dev1) = mapped_mix();
    let conn = env.store.conn();
    let other = env.playlist_id("other").unwrap();
    let dev2 = devices::upsert_device(conn, "G2", None, None).unwrap();
    devices::replace_mappings(conn, dev2, &[], &[pm(other, 0x1, "other")]).unwrap();
    // Both playlists deleted; "other" is mapped on dev2 only, "mix" on dev1.
    playlists::delete(conn, &env.cfg, "mix").unwrap();
    playlists::delete(conn, &env.cfg, "other").unwrap();
    assert_eq!(store::removed_playlists(conn).unwrap().len(), 2);

    // Re-adopting dev1 without the mapping: "mix" has nothing left to delete,
    // "other" is still wanted by dev2.
    devices::replace_mappings(conn, dev1, &[], &[]).unwrap();
    assert_eq!(
        store::removed_playlists(conn).unwrap(),
        vec![(other, "other".to_owned())]
    );
    assert_eq!(
        count(
            conn,
            &format!("SELECT COUNT(*) FROM playlists WHERE id = {mix}")
        ),
        0
    );
}

#[test]
fn replace_mappings_keeps_live_playlists_that_are_not_mapped() {
    let (env, _, dev) = mapped_mix();
    let conn = env.store.conn();
    devices::replace_mappings(conn, dev, &[], &[]).unwrap();
    assert_eq!(live_names(conn), vec!["mix", "other"]);
}

// ---- migration 0003 on a version 2 database

#[test]
fn migration_0003_upgrades_a_version_2_database_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("old/iprune.sqlite");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    {
        let c = Connection::open(&db).unwrap();
        c.execute_batch(include_str!("../src/store/migrations/0001_init.sql"))
            .unwrap();
        c.execute_batch(include_str!("../src/store/migrations/0002_tag_state.sql"))
            .unwrap();
        c.execute_batch(
            "INSERT INTO tracks (id, path, path_key, size, mtime_ns, hash, duration_ms, filetype, tags_hash, added_at) \
               VALUES (1, '/m/a.mp3', '/m/a.mp3', 5, 6, zeroblob(32), 215000, 'mp3', zeroblob(32), 9), \
                      (2, '/m/b.mp3', '/m/b.mp3', 5, 6, zeroblob(32), NULL, 'mp3', zeroblob(32), 9); \
             INSERT INTO devices (id, firewire_guid) VALUES (1, 'G1'); \
             INSERT INTO device_tracks (device_id, track_id, dbid, synced_tags_hash, synced_size) \
               VALUES (1, 1, 11, zeroblob(32), 5), (1, 2, 12, zeroblob(32), 5); \
             INSERT INTO playlists (id, name, file, file_mtime_ns) VALUES (1, 'Road', '/p/Road.m3u8', 3); \
             INSERT INTO device_playlists (device_id, playlist_id, ipod_name) VALUES (1, 1, 'Road');",
        )
        .unwrap();
        c.pragma_update(None, "user_version", 2).unwrap();
        let cols: Vec<String> = c
            .prepare("SELECT name FROM pragma_table_info('device_tracks')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            !cols.iter().any(|c| c == "synced_duration_ms"),
            "precondition"
        );
    }

    let s = Store::open(&db).unwrap();
    let conn = s.conn();
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert!(version >= 3, "user_version is {version}");

    // The synced duration is filled from the file's, NULL stays NULL.
    let maps = devices::track_mappings(conn, 1).unwrap();
    assert_eq!(maps.len(), 2);
    assert_eq!(maps[0].track_id, 1);
    assert_eq!(maps[0].synced_duration_ms, Some(215_000));
    assert_eq!(maps[1].synced_duration_ms, None);
    // The playlist mapping has no id yet; the name stands in.
    assert_eq!(
        devices::playlist_mappings(conn, 1).unwrap(),
        vec![PlaylistMapping {
            playlist_id: 1,
            ipod_id: None,
            ipod_name: "Road".into()
        }]
    );
    // The existing playlist is live, not a tombstone.
    assert_eq!(live_names(conn), vec!["Road"]);
    assert!(store::removed_playlists(conn).unwrap().is_empty());
    // And the new tombstone path works on it.
    store::delete_playlist(conn, "Road").unwrap();
    assert_eq!(
        store::removed_playlists(conn).unwrap(),
        vec![(1, "Road".to_owned())]
    );

    // Reopening is a no-op, not a second ALTER.
    drop(s);
    Store::open(&db).unwrap();
}

// A playlist that no iPod maps is simply the adoption matcher's business:
// tombstones must never be offered to it.
#[test]
fn adoption_never_sees_tombstones() {
    let (env, _, _) = mapped_mix();
    let conn = env.store.conn();
    playlists::delete(conn, &env.cfg, "mix").unwrap();
    let ipod = [adopt::IpodPlaylist {
        id: 5,
        name: "mix".into(),
        members: vec![],
    }];
    let m = adopt::match_playlists(&ipod, &devices::library_playlists(conn).unwrap(), &[]);
    assert!(m.matched.is_empty(), "{:?}", m.matched);
    assert_eq!(m.unmatched_ipod, vec!["mix"]);
}
