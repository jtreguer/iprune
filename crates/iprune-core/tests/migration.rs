//! The index migrated from schema version 4 (before ids were made stable)
//! to the current one, on a file in a temp dir that holds data.

use std::collections::BTreeMap;
use std::path::Path;

use iprune_core::Error;
use iprune_core::store::Store;
use rusqlite::{Connection, types::Value};

const V1_TO_V4: [&str; 4] = [
    include_str!("../src/store/migrations/0001_init.sql"),
    include_str!("../src/store/migrations/0002_tag_state.sql"),
    include_str!("../src/store/migrations/0003_playlist_tombstones.sql"),
    include_str!("../src/store/migrations/0004_pending_pulls.sql"),
];

const TABLES: [&str; 8] = [
    "tracks",
    "playlists",
    "playlist_items",
    "stats",
    "devices",
    "device_tracks",
    "device_playlists",
    "pending_pulls",
];

type Row = BTreeMap<String, String>;

/// Every row of `table` by column name, so column order does not matter.
fn dump(conn: &Connection, table: &str) -> Vec<Row> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY 1, 2"))
        .unwrap();
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    stmt.query_map([], |r| {
        Ok(names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), format!("{:?}", r.get::<_, Value>(i).unwrap())))
            .collect::<Row>())
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

fn dump_all(conn: &Connection) -> BTreeMap<&'static str, Vec<Row>> {
    TABLES.iter().map(|t| (*t, dump(conn, t))).collect()
}

fn track(conn: &Connection, id: i64) {
    conn.execute(
        "INSERT INTO tracks (id, path, path_key, size, mtime_ns, hash, duration_ms, filetype, \
         title, artist, tags_hash, added_at, missing_since, tag_error, drm, compilation) \
         VALUES (?1, ?2, ?2, ?3, 9, x'0102', 1500, 'mp3', ?4, 'Artist', x'0a0b', 77, ?5, ?6, ?7, 1)",
        rusqlite::params![
            id,
            format!("/music/{id}.mp3"),
            1000 + id,
            format!("Title {id}"),
            (id == 2).then_some(5),
            (id == 2).then_some("bad tag"),
            id == 1
        ],
    )
    .unwrap();
}

/// A version 4 index. Tracks 1, 2 and 7 (the highest id; 7 is in no
/// playlist and no mapping), playlists 1 and 4 (the highest; 4 is empty and
/// unmapped), one iPod with track and playlist mappings, statistics and a
/// pending pull.
fn version_4(path: &Path) {
    let conn = Connection::open(path).unwrap();
    for sql in V1_TO_V4 {
        conn.execute_batch(sql).unwrap();
    }
    for id in [1, 2, 7] {
        track(&conn, id);
    }
    conn.execute_batch(
        "INSERT INTO playlists (id, name, file, file_mtime_ns, removed_at) VALUES \
           (1, 'Road', '/p/Road.m3u8', 11, NULL), (4, 'Gone', '/p/Gone.m3u8', 12, 99);
         INSERT INTO playlist_items (playlist_id, position, track_id, raw_entry) VALUES \
           (1, 0, 2, '/music/2.mp3'), (1, 1, NULL, '/music/unknown.mp3'), (1, 2, 1, '/music/1.mp3');
         INSERT INTO devices (id, firewire_guid, name, model, last_sync_at) VALUES \
           (1, '0x000A27001234ABCD', 'iPod de Test', 'MC297', 1700000000);
         INSERT INTO device_tracks (device_id, track_id, dbid, synced_tags_hash, synced_size) VALUES \
           (1, 1, -16, x'01', 1001), (1, 2, 2, x'02', 1002);
         INSERT INTO device_playlists (device_id, playlist_id, ipod_name, ipod_id) VALUES \
           (1, 1, 'Road', 4242);
         INSERT INTO stats (track_id, plays, skips, rating, last_played) VALUES \
           (1, 5, 1, 80, 1700000500), (2, 0, 0, NULL, NULL);
         INSERT INTO pending_pulls (device_id, play_counts_hash, changes, at) VALUES \
           (1, x'ff', '[]', 1700000600);",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 4).unwrap();
}

fn sequence(conn: &Connection, table: &str) -> Option<i64> {
    conn.query_row(
        "SELECT seq FROM sqlite_sequence WHERE name = ?1",
        [table],
        |r| r.get(0),
    )
    .ok()
}

#[test]
fn a_version_4_index_keeps_every_row_and_id_and_gets_an_id_that_is_never_reused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("iprune.sqlite");
    version_4(&path);
    let before = {
        let conn = Connection::open(&path).unwrap();
        assert_eq!(dump(&conn, "tracks").len(), 3, "set up");
        dump_all(&conn)
    };

    let store = Store::open(&path).unwrap();
    let conn = store.conn();

    // Rows and ids are kept, field for field. (`devices` and `device_tracks`
    // gained a column, unknown for the rows that were there.)
    let mut after = dump_all(conn);
    for row in after.get_mut("devices").unwrap() {
        assert_eq!(row.remove("restoring").as_deref(), Some("Null"));
    }
    let mapped = after.get_mut("device_tracks").unwrap();
    assert_eq!(mapped.len(), 2, "set up");
    for row in mapped {
        assert_eq!(row.remove("synced_rating").as_deref(), Some("Null"));
    }
    assert_eq!(after, before);
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 6);

    // The next id starts above the highest one used, also after that
    // highest row is deleted.
    assert_eq!(sequence(conn, "tracks"), Some(7));
    assert_eq!(sequence(conn, "playlists"), Some(4));
    conn.execute("DELETE FROM tracks WHERE id = 7", []).unwrap();
    conn.execute("DELETE FROM playlists WHERE id = 4", [])
        .unwrap();
    track_without_id(conn, "/music/new.mp3");
    conn.execute(
        "INSERT INTO playlists (name, file, file_mtime_ns) VALUES ('Fresh', '/p/Fresh.m3u8', 1)",
        [],
    )
    .unwrap();
    let new_track: i64 = conn
        .query_row(
            "SELECT id FROM tracks WHERE path = '/music/new.mp3'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let new_playlist: i64 = conn
        .query_row("SELECT id FROM playlists WHERE name = 'Fresh'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!((new_track, new_playlist), (8, 5));

    // Foreign keys are enforced again afterwards, and the references of the
    // tables that point at the rebuilt ones still work.
    let on: i64 = conn
        .pragma_query_value(None, "foreign_keys", |r| r.get(0))
        .unwrap();
    assert_eq!(on, 1);
    let e = conn.execute("DELETE FROM tracks WHERE id = 1", []);
    assert!(e.is_err(), "track 1 is in a playlist and mapped");
    conn.execute("DELETE FROM playlists WHERE id = 1", [])
        .unwrap_err(); // mapped on the iPod
    conn.execute("DELETE FROM device_playlists", []).unwrap();
    conn.execute("DELETE FROM playlists WHERE id = 1", [])
        .unwrap();
    let items: i64 = conn
        .query_row("SELECT COUNT(*) FROM playlist_items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(items, 0, "the playlist's items went with it");
    let broken: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(broken, 0);

    // The indexes: the old one on the hash is back, the new one on items.
    let indexes: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND name NOT LIKE 'sqlite_%'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(indexes.contains(&"tracks_hash".to_owned()), "{indexes:?}");
    assert!(
        indexes.contains(&"playlist_items_track".to_owned()),
        "{indexes:?}"
    );
}

fn track_without_id(conn: &Connection, path: &str) {
    conn.execute(
        "INSERT INTO tracks (path, path_key, size, mtime_ns, hash, filetype, tags_hash, added_at) \
         VALUES (?1, ?1, 1, 1, x'01', 'mp3', x'01', 1)",
        [path],
    )
    .unwrap();
}

#[test]
fn a_migration_that_would_leave_broken_references_is_not_committed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("iprune.sqlite");
    version_4(&path);
    // A mapping of a track that does not exist (foreign keys were off when
    // it was written).
    {
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        conn.execute(
            "INSERT INTO device_tracks (device_id, track_id, dbid, synced_tags_hash, synced_size) \
             VALUES (1, 99, 99, x'01', 1)",
            [],
        )
        .unwrap();
    }
    let before = {
        let conn = Connection::open(&path).unwrap();
        dump_all(&conn)
    };

    let e = Store::open(&path).err().expect("refused");
    assert!(
        matches!(&e, Error::Migration(_)) && e.to_string().contains("broken references"),
        "{e}"
    );

    // Nothing was changed: still version 4, the old tables, the same rows.
    let conn = Connection::open(&path).unwrap();
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 4);
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'tracks'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!sql.contains("AUTOINCREMENT"), "{sql}");
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('tracks_new', 'playlists_new')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "no half built table is left");
    assert_eq!(dump_all(&conn), before);
}

#[test]
fn a_new_index_is_created_at_the_current_version_with_stable_ids_and_foreign_keys_on() {
    let s = Store::open_in_memory().unwrap();
    let conn = s.conn();
    let on: i64 = conn
        .pragma_query_value(None, "foreign_keys", |r| r.get(0))
        .unwrap();
    assert_eq!(on, 1);
    track_without_id(conn, "/a.mp3");
    conn.execute("DELETE FROM tracks", []).unwrap();
    track_without_id(conn, "/b.mp3");
    let id: i64 = conn
        .query_row("SELECT id FROM tracks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(id, 2, "an id freed is not given again");
}
