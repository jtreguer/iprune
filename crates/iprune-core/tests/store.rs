mod common;

use std::path::{Path, PathBuf};

use common::{Env, Kind};
use iprune_core::store::FileRecord;
use iprune_core::store::{self, Store};
use iprune_core::tags::{AudioInfo, FileKind, Tags};
use iprune_core::{Error, playlists};

fn names(store: &Store) -> Vec<String> {
    store::playlists(store.conn())
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect()
}

fn put(conn: &rusqlite::Connection, name: &str) -> iprune_core::Result<i64> {
    store::put_playlist(conn, name, Path::new(&format!("/p/{name}.m3u8")), 1, &[])
}

#[test]
fn savepoint_rolls_back_everything_on_error_outside_a_transaction() {
    let s = Store::open_in_memory().unwrap();
    let r: iprune_core::Result<()> = store::savepoint(s.conn(), || {
        put(s.conn(), "a")?;
        put(s.conn(), "b")?;
        Err(Error::NoSuchPlaylist("boom".into()))
    });
    assert!(matches!(r, Err(Error::NoSuchPlaylist(ref n)) if n == "boom"));
    assert!(names(&s).is_empty());
    // The connection is usable afterwards (savepoint released).
    put(s.conn(), "c").unwrap();
    assert_eq!(names(&s), ["c"]);
}

#[test]
fn savepoint_keeps_writes_on_success() {
    let s = Store::open_in_memory().unwrap();
    store::savepoint(s.conn(), || put(s.conn(), "a").map(|_| ())).unwrap();
    assert_eq!(names(&s), ["a"]);
}

#[test]
fn savepoint_inside_a_transaction_rolls_back_only_its_own_writes() {
    let mut s = Store::open_in_memory().unwrap();
    {
        let tx = s.transaction().unwrap();
        put(&tx, "before").unwrap();
        let r: iprune_core::Result<()> = store::savepoint(&tx, || {
            put(&tx, "inner")?;
            Err(Error::NoSuchPlaylist("boom".into()))
        });
        assert!(r.is_err());
        // A successful nested savepoint is kept with the outer transaction.
        store::savepoint(&tx, || put(&tx, "kept").map(|_| ())).unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(names(&s), ["before", "kept"]);
}

#[test]
fn successful_savepoint_is_still_undone_by_the_outer_rollback() {
    let mut s = Store::open_in_memory().unwrap();
    {
        let tx = s.transaction().unwrap();
        store::savepoint(&tx, || put(&tx, "a").map(|_| ())).unwrap();
        tx.rollback().unwrap();
    }
    assert!(names(&s).is_empty());
}

#[test]
fn failed_put_playlist_keeps_the_previous_items() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    let id = env.track(&a).id;
    let conn = env.store.conn();
    let file = Path::new("/p/x.m3u8");
    store::put_playlist(conn, "x", file, 1, &[(Some(id), "one".into())]).unwrap();
    // The second item references a track that does not exist: foreign key error
    // after the old items were deleted and the first new one inserted.
    let r = store::put_playlist(
        conn,
        "x",
        file,
        2,
        &[(Some(id), "new1".into()), (Some(987_654), "new2".into())],
    );
    assert!(matches!(r, Err(Error::Db(_))), "{r:?}");
    assert_eq!(
        env.indexed_items("x"),
        vec![(0, Some(id), "one".to_string())]
    );
    assert_eq!(store::playlists(conn).unwrap()[0].file_mtime_ns, 1);
}

#[test]
fn sync_from_disk_that_fails_midway_changes_nothing() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    env.write_playlist("old", &format!("{}\n", a.display()));
    playlists::sync_from_disk(env.store.conn(), &env.cfg, true).unwrap();
    let old_items = env.indexed_items("old");
    // "old" is deleted from disk, "a-new" loads fine, then "b-bad" fails in SQLite.
    std::fs::remove_file(env.playlist_file("old")).unwrap();
    env.write_playlist("a-new", &format!("{}\n", a.display()));
    env.write_playlist("b-bad", "/x/boom.mp3\n");
    env.store
        .conn()
        .execute_batch(
            "CREATE TRIGGER boom BEFORE INSERT ON playlist_items \
             WHEN NEW.raw_entry LIKE '%boom%' BEGIN SELECT RAISE(ABORT, 'boom'); END;",
        )
        .unwrap();

    let r = playlists::sync_from_disk(env.store.conn(), &env.cfg, true);
    assert!(matches!(r, Err(Error::Db(_))), "{r:?}");
    assert!(
        env.playlist_id("a-new").is_none(),
        "half-loaded playlist left behind"
    );
    assert!(env.playlist_id("b-bad").is_none());
    assert!(
        env.playlist_id("old").is_some(),
        "deletion must not be half-applied"
    );
    assert_eq!(env.indexed_items("old"), old_items);
}

fn record(name: &str, tag_error: Option<&str>, drm: bool) -> FileRecord {
    FileRecord {
        path: PathBuf::from(format!("/m/{name}")),
        path_key: format!("/m/{name}"),
        size: 10,
        mtime_ns: 20,
        hash: [7; 32],
        info: AudioInfo {
            kind: FileKind::Aac,
            duration_ms: Some(1000),
            bitrate: None,
            samplerate: None,
        },
        tags: Tags::default(),
        tag_error: tag_error.map(String::from),
        drm,
    }
}

fn row(s: &Store, name: &str) -> store::TrackRow {
    store::all_tracks(s.conn())
        .unwrap()
        .into_iter()
        .find(|t| t.path == Path::new("/m").join(name))
        .unwrap()
}

#[test]
fn track_row_round_trips_tag_error_and_drm_through_insert_and_update() {
    let s = Store::open_in_memory().unwrap();
    let a = store::insert_track(s.conn(), &record("a", Some("bad tag"), true), 1).unwrap();
    let b = store::insert_track(s.conn(), &record("b", None, false), 1).unwrap();
    let (ra, rb) = (row(&s, "a"), row(&s, "b"));
    assert_eq!((ra.tag_error.as_deref(), ra.drm), (Some("bad tag"), true));
    assert_eq!((rb.tag_error.as_deref(), rb.drm), (None, false));

    // Update writes both fields in both directions.
    store::update_track(s.conn(), a, &record("a", None, false)).unwrap();
    store::update_track(s.conn(), b, &record("b", Some("later"), true)).unwrap();
    let (ra, rb) = (row(&s, "a"), row(&s, "b"));
    assert_eq!((ra.tag_error, ra.drm), (None, false));
    assert_eq!((rb.tag_error.as_deref(), rb.drm), (Some("later"), true));
}

#[test]
fn flagged_tracks_lists_present_flagged_rows_only() {
    let s = Store::open_in_memory().unwrap();
    store::insert_track(s.conn(), &record("clean", None, false), 1).unwrap();
    store::insert_track(s.conn(), &record("untagged", Some("why"), false), 1).unwrap();
    store::insert_track(s.conn(), &record("drm", None, true), 1).unwrap();
    let gone = store::insert_track(s.conn(), &record("gone", Some("x"), true), 1).unwrap();
    store::set_missing(s.conn(), gone, Some(5)).unwrap();
    let f = store::flagged_tracks(s.conn()).unwrap();
    assert_eq!(
        f.untagged,
        vec![(PathBuf::from("/m/untagged"), "why".to_string())]
    );
    assert_eq!(f.drm, vec![PathBuf::from("/m/drm")]);
}

// Prevents: an index made before migration 0002 failing to open, or its rows
// coming out flagged.
#[test]
fn migration_0002_upgrades_a_version_1_database_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("old/iprune.sqlite");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(include_str!("../src/store/migrations/0001_init.sql"))
            .unwrap();
        c.execute(
            "INSERT INTO tracks (path, path_key, size, mtime_ns, hash, filetype, tags_hash, added_at) \
             VALUES ('/m/old.mp3', '/m/old.mp3', 5, 6, zeroblob(32), 'mp3', zeroblob(32), 9)",
            [],
        )
        .unwrap();
        c.pragma_update(None, "user_version", 1).unwrap();
        let cols: Vec<String> = c
            .prepare("SELECT name FROM pragma_table_info('tracks')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            !cols.iter().any(|c| c == "tag_error" || c == "drm"),
            "precondition"
        );
    }

    let s = Store::open(&db).unwrap();
    let version: i64 = s
        .conn()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert!(version >= 2, "user_version is {version}");
    let cols: Vec<String> = s
        .conn()
        .prepare("SELECT name FROM pragma_table_info('tracks')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(cols.iter().any(|c| c == "tag_error") && cols.iter().any(|c| c == "drm"));

    let rows = store::all_tracks(s.conn()).unwrap();
    assert_eq!(rows.len(), 1, "existing row survives");
    assert_eq!(rows[0].path, Path::new("/m/old.mp3"));
    assert_eq!((rows[0].tag_error.clone(), rows[0].drm), (None, false));
    let (null_err, drm): (bool, i64) = s
        .conn()
        .query_row("SELECT tag_error IS NULL, drm FROM tracks", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert!(null_err && drm == 0);
    let f = store::flagged_tracks(s.conn()).unwrap();
    assert!(f.untagged.is_empty() && f.drm.is_empty());

    // Reopening is a no-op, not a second ALTER.
    drop(s);
    Store::open(&db).unwrap();
}

fn track_id(s: &Store, name: &str) -> i64 {
    store::insert_track(s.conn(), &record(name, None, false), 0).unwrap()
}

fn holding(s: &Store, id: i64) -> Vec<String> {
    store::playlists_holding(s.conn(), id).unwrap()
}

fn put_items(s: &Store, name: &str, items: &[(Option<i64>, &str)]) {
    let items: Vec<(Option<i64>, String)> =
        items.iter().map(|(i, t)| (*i, (*t).to_string())).collect();
    store::put_playlist(
        s.conn(),
        name,
        Path::new(&format!("/p/{name}.m3u8")),
        1,
        &items,
    )
    .unwrap();
}

#[test]
fn playlists_holding_lists_every_holder_in_name_order_and_skips_the_others() {
    let s = Store::open_in_memory().unwrap();
    let a = track_id(&s, "a.m4a");
    let b = track_id(&s, "b.m4a");
    // inserted out of name order on purpose
    put_items(&s, "zeta", &[(Some(a), "a")]);
    put_items(&s, "alpha", &[(Some(b), "b"), (Some(a), "a")]);
    put_items(&s, "mid", &[(Some(a), "a")]);
    put_items(&s, "only-b", &[(Some(b), "b")]);
    assert_eq!(holding(&s, a), ["alpha", "mid", "zeta"]);
    assert_eq!(holding(&s, b), ["alpha", "only-b"]);
}

#[test]
fn playlists_holding_lists_a_playlist_once_however_often_it_holds_the_track() {
    let s = Store::open_in_memory().unwrap();
    let a = track_id(&s, "a.m4a");
    put_items(
        &s,
        "twice",
        &[(Some(a), "a"), (Some(a), "a"), (Some(a), "a")],
    );
    assert_eq!(holding(&s, a), ["twice"]);
}

#[test]
fn playlists_holding_ignores_a_tombstoned_playlist_even_if_its_items_remain() {
    let s = Store::open_in_memory().unwrap();
    let a = track_id(&s, "a.m4a");
    put_items(&s, "live", &[(Some(a), "a")]);
    put_items(&s, "gone", &[(Some(a), "a")]);
    s.conn()
        .execute(
            "UPDATE playlists SET removed_at = 5 WHERE name = 'gone'",
            [],
        )
        .unwrap();
    let kept: i64 = s
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM playlist_items i JOIN playlists p ON p.id = i.playlist_id \
             WHERE p.name = 'gone'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, 1, "the fixture must keep the tombstone's item");
    assert_eq!(holding(&s, a), ["live"]);
}

#[test]
fn playlists_holding_never_matches_unresolved_entries() {
    let s = Store::open_in_memory().unwrap();
    let a = track_id(&s, "a.m4a");
    put_items(
        &s,
        "unresolved",
        &[(None, "missing.mp3"), (None, "other.mp3")],
    );
    put_items(&s, "mixed", &[(None, "missing.mp3"), (Some(a), "a")]);
    assert_eq!(holding(&s, a), ["mixed"]);
    // a track id that exists nowhere matches nothing, NULL rows included
    assert!(holding(&s, 987_654).is_empty());
}

#[test]
fn playlists_holding_is_empty_for_a_track_in_no_playlist() {
    let s = Store::open_in_memory().unwrap();
    let a = track_id(&s, "a.m4a");
    let b = track_id(&s, "b.m4a");
    assert!(holding(&s, a).is_empty(), "no playlist at all");
    put_items(&s, "x", &[(Some(b), "b")]);
    assert!(
        holding(&s, a).is_empty(),
        "a playlist holding another track"
    );
}
