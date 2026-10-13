mod common;

use std::path::Path;

use common::{Env, Kind};
use iprune_core::store::{self, Store};
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
