//! `stats::pull`, `undo` and `export_csv` against a real SQLite index. The
//! iPod side is a `Contents` built by hand: the C fixture cannot set recent
//! play counts, which libgpod reads from a `Play Counts` file.

mod common;

use common::{Env, Kind};
use iprune_core::adopt::IpodTrack;
use iprune_core::ipod::{Contents, Track};
use iprune_core::stats;
use iprune_core::store::devices::{self, TrackMapping};
use iprune_core::tags::Tags;
use rusqlite::Connection;

type Row = (i64, i64, i64, Option<i64>, Option<i64>);

fn rows(conn: &Connection) -> Vec<Row> {
    let mut stmt = conn
        .prepare("SELECT track_id, plays, skips, rating, last_played FROM stats ORDER BY track_id")
        .unwrap();
    stmt.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

fn row(conn: &Connection, track_id: i64) -> Row {
    rows(conn)
        .into_iter()
        .find(|r| r.0 == track_id)
        .unwrap_or_else(|| panic!("no stats row for track {track_id}"))
}

fn put(conn: &Connection, row: Row) {
    conn.execute(
        "INSERT INTO stats (track_id, plays, skips, rating, last_played) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![row.0, row.1, row.2, row.3, row.4],
    )
    .unwrap();
}

fn device(conn: &Connection) -> i64 {
    devices::upsert_device(conn, "G1", None, None).unwrap()
}

fn pending(conn: &Connection, dev: i64) -> Option<(Option<Vec<u8>>, String)> {
    conn.query_row(
        "SELECT play_counts_hash, changes FROM pending_pulls WHERE device_id = ?1",
        [dev],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .ok()
}

fn mapping(track_id: i64, dbid: u64) -> TrackMapping {
    TrackMapping {
        track_id,
        dbid,
        synced_tags_hash: [0; 32],
        synced_size: 1,
        synced_duration_ms: None,
    }
}

fn ipod_track(dbid: u64, plays: u32, skips: u32, rating: u32, last_played: i64) -> Track {
    Track {
        adopt: IpodTrack {
            dbid,
            size: 1,
            duration_ms: 1000,
            title: None,
            artist: None,
            album: None,
            playlists: 0,
            playcount: 0,
            hash: None,
        },
        tags: Tags::default(),
        ipod_path: Some(":iPod_Control:Music:F00:A.mp3".into()),
        recent_plays: plays,
        recent_skips: skips,
        rating,
        last_played,
    }
}

fn contents(tracks: Vec<Track>) -> Contents {
    Contents {
        tracks,
        ..Contents::default()
    }
}

/// Three tracks a, b, c with ids and mappings to dbids 10, 20, 30.
fn world() -> (Env, [i64; 3], Vec<TrackMapping>) {
    let mut env = Env::new();
    let paths = [
        env.add("a.mp3", Kind::Mp3, 1),
        env.add("b.mp3", Kind::Mp3, 2),
        env.add("c.mp3", Kind::Mp3, 3),
    ];
    env.scan();
    let ids = paths.map(|p| env.track(&p).id);
    let maps = vec![
        mapping(ids[0], 10),
        mapping(ids[1], 20),
        mapping(ids[2], 30),
    ];
    (env, ids, maps)
}

#[test]
fn pull_creates_a_row_for_a_track_with_no_stats() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    let c = contents(vec![ipod_track(10, 3, 1, 80, 5000)]);
    let p = stats::pull(conn, dev, &maps, &c, None).unwrap();
    assert_eq!((p.tracks(), p.plays()), (1, 3));
    assert_eq!(rows(conn), vec![(ids[0], 3, 1, Some(80), Some(5000))]);
}

#[test]
fn pull_adds_recent_counts_to_the_totals() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    put(conn, (ids[0], 5, 2, Some(40), Some(1000)));
    let c = contents(vec![ipod_track(10, 3, 4, 60, 2000)]);
    let p = stats::pull(conn, dev, &maps, &c, None).unwrap();
    assert_eq!((p.tracks(), p.plays()), (1, 3));
    assert_eq!(rows(conn), vec![(ids[0], 8, 6, Some(60), Some(2000))]);
}

#[test]
fn pull_keeps_the_latest_last_played() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    put(conn, (ids[0], 1, 0, None, Some(9000)));
    put(conn, (ids[1], 1, 0, None, Some(1000)));
    // a: the iPod's date is older than the index's. b: newer.
    let c = contents(vec![
        ipod_track(10, 1, 0, 0, 4000),
        ipod_track(20, 1, 0, 0, 7000),
    ]);
    stats::pull(conn, dev, &maps, &c, None).unwrap();
    assert_eq!(row(conn, ids[0]).4, Some(9000));
    assert_eq!(row(conn, ids[1]).4, Some(7000));
}

#[test]
fn a_never_played_track_keeps_its_stored_date_and_gets_none_when_it_has_none() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    put(conn, (ids[0], 1, 0, None, Some(9000)));
    let c = contents(vec![ipod_track(10, 2, 0, 0, 0), ipod_track(20, 2, 0, 0, 0)]);
    stats::pull(conn, dev, &maps, &c, None).unwrap();
    assert_eq!(row(conn, ids[0]).4, Some(9000));
    assert_eq!(row(conn, ids[1]).4, None);
}

#[test]
fn pull_skips_tracks_with_nothing_new_and_unmapped_dbids() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    put(conn, (ids[0], 5, 1, Some(60), Some(3000)));
    let before = rows(conn);
    let c = contents(vec![
        // Same rating, older date, no recent counts: nothing new.
        ipod_track(10, 0, 0, 60, 2000),
        // No stats at all and none stored: nothing new, so no row either.
        ipod_track(20, 0, 0, 0, 0),
        // Plays, but the dbid is not mapped.
        ipod_track(999, 7, 7, 100, 8000),
    ]);
    let p = stats::pull(conn, dev, &maps, &c, None).unwrap();
    assert_eq!((p.tracks(), p.plays()), (0, 0));
    assert_eq!(rows(conn), before);
}

#[test]
fn a_rating_or_date_change_alone_is_a_change_with_zero_plays() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    put(conn, (ids[0], 5, 1, Some(60), Some(3000)));
    let c = contents(vec![ipod_track(10, 0, 0, 100, 3000)]);
    let p = stats::pull(conn, dev, &maps, &c, None).unwrap();
    assert_eq!((p.tracks(), p.plays()), (1, 0));
    assert_eq!(rows(conn), vec![(ids[0], 5, 1, Some(100), Some(3000))]);
}

#[test]
fn pulling_the_same_contents_twice_counts_the_plays_twice() {
    // Why the pull must be undone when the write fails, and why it happens
    // once per write: the iPod still holds the recent counts until it is
    // written, and a second pull would add them again.
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    let c = contents(vec![ipod_track(10, 3, 0, 0, 0)]);
    stats::pull(conn, dev, &maps, &c, None).unwrap();
    stats::pull(conn, dev, &maps, &c, None).unwrap();
    assert_eq!(row(conn, ids[0]).1, 6);
}

#[test]
fn undo_restores_the_previous_rows_exactly_and_deletes_the_rows_the_pull_created() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    put(conn, (ids[0], 5, 2, Some(40), Some(1000)));
    put(conn, (ids[2], 9, 9, None, None));
    let before = rows(conn);
    let c = contents(vec![
        ipod_track(10, 3, 4, 60, 2000), // existing row, changed
        ipod_track(20, 2, 0, 20, 500),  // no row yet: created
        ipod_track(30, 0, 0, 0, 0),     // nothing new: untouched
    ]);
    let p = stats::pull(conn, dev, &maps, &c, None).unwrap();
    assert_eq!(p.tracks(), 2);
    assert_ne!(rows(conn), before);
    assert_eq!(rows(conn).len(), 3);

    stats::undo(conn, dev, &p).unwrap();
    assert_eq!(rows(conn), before);
}

#[test]
fn undo_of_an_empty_pull_changes_nothing() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    put(conn, (ids[0], 5, 2, Some(40), Some(1000)));
    let before = rows(conn);
    let p = stats::pull(conn, dev, &maps, &contents(vec![]), None).unwrap();
    stats::undo(conn, dev, &p).unwrap();
    assert_eq!(rows(conn), before);
}

// ---- CSV

#[test]
fn export_csv_sorts_by_path_escapes_fields_and_leaves_null_columns_empty() {
    let mut env = Env::new();
    let z = env.add("z.mp3", Kind::Mp3, 1);
    let comma = env.add("with,comma.mp3", Kind::Mp3, 2);
    let quote = env.add("quo\"te.mp3", Kind::Mp3, 3);
    let none = env.add("no stats.mp3", Kind::Mp3, 4);
    env.scan();
    let conn = env.store.conn();
    put(conn, (env.track(&z).id, 1, 2, Some(80), Some(1700000000)));
    put(conn, (env.track(&comma).id, 3, 0, None, None));
    put(conn, (env.track(&quote).id, 0, 4, Some(20), None));
    let _ = none;
    let out = env.music.join("../stats.csv");
    let n = stats::export_csv(conn, &out).unwrap();
    assert_eq!(n, 3);
    let text = std::fs::read_to_string(&out).unwrap();
    let m = env.music.display();
    let expected = format!(
        "path,plays,skips,rating,last_played\n\
         \"{m}/quo\"\"te.mp3\",0,4,20,\n\
         \"{m}/with,comma.mp3\",3,0,,\n\
         {m}/z.mp3,1,2,80,1700000000\n"
    );
    assert_eq!(text, expected);
}

#[test]
fn export_csv_replaces_an_existing_file_and_an_empty_index_gives_only_the_header() {
    let env = Env::new();
    let out = env.music.join("../stats.csv");
    std::fs::write(&out, "old contents\n").unwrap();
    let n = stats::export_csv(env.store.conn(), &out).unwrap();
    assert_eq!(n, 0);
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "path,plays,skips,rating,last_played\n"
    );
}

// ---- pending pulls: settle, recover, play_counts

const H1: [u8; 32] = [1; 32];
const H2: [u8; 32] = [2; 32];

#[test]
fn a_pull_is_recorded_as_pending_with_the_play_counts_hash() {
    let (env, _, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    let c = contents(vec![ipod_track(10, 3, 0, 0, 0)]);
    stats::pull(conn, dev, &maps, &c, Some(H1)).unwrap();
    let (hash, _) = pending(conn, dev).expect("a pending row");
    assert_eq!(hash.as_deref(), Some(&H1[..]));
    // Without a Play Counts file the hash is NULL.
    let other = devices::upsert_device(conn, "G2", None, None).unwrap();
    stats::pull(conn, other, &maps, &c, None).unwrap();
    assert_eq!(pending(conn, other).unwrap().0, None);
}

#[test]
fn undo_and_settle_clear_the_pending_row() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    let c = contents(vec![ipod_track(10, 3, 0, 0, 0)]);
    let p = stats::pull(conn, dev, &maps, &c, Some(H1)).unwrap();
    stats::undo(conn, dev, &p).unwrap();
    assert!(pending(conn, dev).is_none());
    assert!(rows(conn).is_empty());

    stats::pull(conn, dev, &maps, &c, Some(H1)).unwrap();
    stats::settle(conn, dev).unwrap();
    assert!(pending(conn, dev).is_none());
    assert_eq!(row(conn, ids[0]).1, 3, "settling keeps the statistics");
}

use iprune_core::stats::{PlayCounts, Recovery};

fn pc(live: Option<[u8; 32]>, renamed: Option<[u8; 32]>) -> PlayCounts {
    PlayCounts { live, renamed }
}

/// One pull of 3 plays on track `a` (and one new row on `b`), made while the
/// iPod's `Play Counts` hashed to `pulled_from`.
fn pending_pull(pulled_from: Option<[u8; 32]>) -> (Env, [i64; 3], i64) {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    put(conn, (ids[0], 5, 0, Some(40), Some(1000)));
    let c = contents(vec![
        ipod_track(10, 3, 1, 60, 2000),
        ipod_track(20, 2, 0, 0, 0),
    ]);
    stats::pull(conn, dev, &maps, &c, pulled_from).unwrap();
    assert!(pending(conn, dev).is_some());
    (env, ids, dev)
}

fn undone(r: &Recovery) -> usize {
    assert_eq!(r.dropped, None);
    r.undone
}

/// The state before the pull in `pending_pull`.
fn untouched_rows(ids: [i64; 3]) -> Vec<Row> {
    vec![(ids[0], 5, 0, Some(40), Some(1000))]
}

#[test]
fn recover_table_the_write_never_happened_when_play_counts_is_still_there() {
    // Same file, or grown with new plays since (a different hash): either
    // way libgpod never renamed it, so the pull is undone.
    for live in [H1, H2] {
        let (env, ids, dev) = pending_pull(Some(H1));
        let conn = env.store.conn();
        let r = stats::recover(conn, dev, pc(Some(live), None)).unwrap();
        assert_eq!(undone(&r), 2, "live {live:?}");
        assert_eq!(
            rows(conn),
            untouched_rows(ids),
            "the new row is deleted too"
        );
        assert!(pending(conn, dev).is_none());
    }
}

#[test]
fn recover_table_the_write_happened_when_the_pulled_file_was_renamed() {
    // Pulled from H1, now H1 is the .bak. A fresh Play Counts may have
    // appeared since; it does not matter.
    for live in [None, Some(H2)] {
        let (env, ids, dev) = pending_pull(Some(H1));
        let conn = env.store.conn();
        let before = rows(conn);
        let r = stats::recover(conn, dev, pc(live, Some(H1))).unwrap();
        assert_eq!(undone(&r), 0, "live {live:?}");
        assert_eq!(rows(conn), before, "the pull stands");
        assert_eq!(row(conn, ids[0]).1, 8);
        assert!(pending(conn, dev).is_none());
    }
}

#[test]
fn recover_table_neither_file_means_the_pull_stands() {
    let (env, _, dev) = pending_pull(Some(H1));
    let conn = env.store.conn();
    let before = rows(conn);
    let r = stats::recover(conn, dev, pc(None, None)).unwrap();
    assert_eq!(undone(&r), 0);
    assert_eq!(rows(conn), before);
    assert!(pending(conn, dev).is_none());
}

#[test]
fn recover_table_an_unrelated_bak_does_not_prove_a_write() {
    // A .bak from some earlier write (a different hash) next to a live file.
    let (env, ids, dev) = pending_pull(Some(H1));
    let conn = env.store.conn();
    let r = stats::recover(conn, dev, pc(Some(H1), Some(H2))).unwrap();
    assert_eq!(undone(&r), 2);
    assert_eq!(rows(conn), untouched_rows(ids));
}

#[test]
fn recover_table_a_pull_made_without_play_counts_is_undone() {
    // It carried no plays, so undoing loses nothing and the next pull takes
    // the ratings and dates again. Whatever the files look like now.
    for now in [
        pc(None, None),
        pc(Some(H1), None),
        pc(None, Some(H1)),
        pc(Some(H2), Some(H1)),
    ] {
        let (env, ids, dev) = pending_pull(None);
        let conn = env.store.conn();
        let r = stats::recover(conn, dev, now).unwrap();
        assert_eq!(undone(&r), 2, "{now:?}");
        assert_eq!(rows(conn), untouched_rows(ids), "{now:?}");
        assert!(pending(conn, dev).is_none());
    }
}

#[test]
fn recover_drops_a_pending_row_it_cannot_read_and_says_so() {
    let (env, ids, dev) = pending_pull(Some(H1));
    let conn = env.store.conn();
    conn.execute("UPDATE pending_pulls SET changes = 'not json'", [])
        .unwrap();
    let before = rows(conn);
    let r = stats::recover(conn, dev, pc(Some(H1), None)).unwrap();
    assert_eq!(r.undone, 0);
    let msg = r.dropped.expect("a message for the warning");
    assert!(msg.contains("pending"), "{msg}");
    assert!(pending(conn, dev).is_none());
    assert_eq!(rows(conn), before, "the statistics are left as they are");
    assert_eq!(row(conn, ids[0]).1, 8);
}

#[test]
fn recover_with_nothing_pending_changes_nothing() {
    let (env, ids, _) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    put(conn, (ids[0], 5, 0, None, None));
    assert_eq!(
        stats::recover(conn, dev, pc(Some(H1), Some(H1))).unwrap(),
        Recovery::default()
    );
    assert_eq!(rows(conn), vec![(ids[0], 5, 0, None, None)]);
}

#[test]
fn recover_only_looks_at_its_own_device() {
    let (env, _, dev) = pending_pull(Some(H1));
    let conn = env.store.conn();
    let other = devices::upsert_device(conn, "G2", None, None).unwrap();
    let before = rows(conn);
    assert_eq!(
        stats::recover(conn, other, pc(Some(H1), None)).unwrap(),
        Recovery::default()
    );
    assert_eq!(rows(conn), before);
    assert!(pending(conn, dev).is_some());
}

#[test]
fn play_counts_hashes_the_live_file_and_the_renamed_one_separately() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        stats::play_counts(dir.path()).unwrap(),
        PlayCounts::default()
    );
    let itunes = dir.path().join("iPod_Control/iTunes");
    std::fs::create_dir_all(&itunes).unwrap();
    std::fs::write(itunes.join("Play Counts"), b"abc").unwrap();
    let a = stats::play_counts(dir.path()).unwrap();
    assert_eq!(a, pc(Some(*blake3::hash(b"abc").as_bytes()), None));
    std::fs::write(itunes.join("Play Counts"), b"abd").unwrap();
    assert_ne!(stats::play_counts(dir.path()).unwrap().live, a.live);
    // libgpod renames it after a write.
    std::fs::rename(itunes.join("Play Counts"), itunes.join("Play Counts.bak")).unwrap();
    assert_eq!(
        stats::play_counts(dir.path()).unwrap(),
        pc(None, Some(*blake3::hash(b"abd").as_bytes()))
    );
}
