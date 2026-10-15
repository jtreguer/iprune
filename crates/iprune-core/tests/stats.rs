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

/// An iPod track with `plays` and `skips` since the last sync, that was
/// never played before: its lifetime counts (libgpod folds the recent ones
/// into them) are the recent ones.
fn ipod_track(dbid: u64, plays: u32, skips: u32, rating: u32, last_played: i64) -> Track {
    lifetime_track(dbid, (plays, skips), (plays, skips), rating, last_played)
}

/// As `ipod_track`, with `lifetime` `(plays, skips)` counts that include the
/// `recent` ones.
fn lifetime_track(
    dbid: u64,
    lifetime: (u32, u32),
    recent: (u32, u32),
    rating: u32,
    last_played: i64,
) -> Track {
    Track {
        adopt: IpodTrack {
            dbid,
            size: 1,
            duration_ms: 1000,
            title: None,
            artist: None,
            album: None,
            playlists: 0,
            playcount: lifetime.0,
            hash: None,
        },
        tags: Tags::default(),
        ipod_path: Some(":iPod_Control:Music:F00:A.mp3".into()),
        recent_plays: recent.0,
        recent_skips: recent.1,
        skips: lifetime.1,
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

/// Maps `maps` to `dev` in the index and records, for each `(dbid, rating)`,
/// the rating the iPod track held at the last pull.
fn record_ratings(conn: &Connection, dev: i64, maps: &[TrackMapping], seen: &[(u64, u32)]) {
    devices::replace_mappings(conn, dev, maps, &[]).unwrap();
    for (dbid, rating) in seen {
        devices::set_synced_rating(conn, dev, *dbid, Some(*rating)).unwrap();
    }
}

/// The recorded iPod rating of each mapped track of `dev`, by dbid.
fn recorded(conn: &Connection, dev: i64) -> std::collections::BTreeMap<u64, u32> {
    devices::synced_ratings(conn, dev)
        .unwrap()
        .into_iter()
        .collect()
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
    // The iPod's rating of track a was recorded at an earlier pull.
    record_ratings(conn, dev, &maps, &[(10, 60)]);
    let before = rows(conn);
    let c = contents(vec![
        // Same rating as recorded, older date, no recent counts: nothing new.
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

#[test]
fn all_is_empty_without_stats() {
    let env = Env::new();
    assert!(stats::all(env.store.conn()).unwrap().is_empty());
}

#[test]
fn all_returns_each_tracks_totals_by_id_and_leaves_out_tracks_without_a_row() {
    let mut env = Env::new();
    for i in 1..=3 {
        env.add(&format!("t{i}.mp3"), Kind::Mp3, i);
    }
    env.scan();
    let ids: Vec<i64> = env.tracks().iter().map(|t| t.id).collect();
    assert_eq!(ids.len(), 3);
    let conn = env.store.conn();
    put(conn, (ids[0], 12, 3, Some(80), Some(1_700_000_000)));
    // Never rated, never played: NULL stays None, and is not the same as 0.
    put(conn, (ids[1], 0, 0, None, None));

    let got = stats::all(conn).unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(
        got[&ids[0]],
        stats::TrackStats {
            plays: 12,
            skips: 3,
            rating: Some(80),
            last_played: Some(1_700_000_000),
        }
    );
    assert_eq!(got[&ids[1]], stats::TrackStats::default());
    assert!(!got.contains_key(&ids[2]));

    // A rating of 0 is a rating, not a missing one.
    conn.execute("UPDATE stats SET rating = 0 WHERE track_id = ?1", [ids[1]])
        .unwrap();
    assert_eq!(stats::all(conn).unwrap()[&ids[1]].rating, Some(0));
}

// ---- ratings: a rating is news only when it changed on that iPod track

/// The stored rating of `track_id`.
fn rating_of(conn: &Connection, track_id: i64) -> Option<i64> {
    row(conn, track_id).3
}

// Prevents (audit H4) a track copied again to the iPod, which starts with no
// rating, wiping the rating the user gave it.
#[test]
fn a_track_copied_again_under_a_new_dbid_keeps_its_rating() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    record_ratings(conn, dev, &maps, &[]);
    // The user rates it on the iPod; the pull stores 80 and records it.
    stats::pull(
        conn,
        dev,
        &maps,
        &contents(vec![ipod_track(10, 0, 0, 80, 0)]),
        None,
    )
    .unwrap();
    assert_eq!(rating_of(conn, ids[0]), Some(80), "set up");
    assert_eq!(recorded(conn, dev).get(&10), Some(&80), "set up");

    // The library track is copied again as dbid 11, unrated, the way a copy
    // always is, while its mapping still holds the old dbid (the iPod track
    // is gone but was never removed from the index).
    devices::apply_track_changes(conn, dev, &[mapping(ids[0], 11)], &[]).unwrap();
    let maps = devices::track_mappings(conn, dev).unwrap();
    assert!(maps.iter().any(|m| m.dbid == 11), "set up");
    assert!(!maps.iter().any(|m| m.dbid == 10), "set up: one mapping");
    assert_eq!(recorded(conn, dev).get(&11), None, "a new dbid knows none");
    let p = stats::pull(
        conn,
        dev,
        &maps,
        &contents(vec![ipod_track(11, 0, 0, 0, 0)]),
        None,
    )
    .unwrap();
    assert_eq!(p.tracks(), 0, "an unrated new copy is not news");
    assert_eq!(rating_of(conn, ids[0]), Some(80));

    // And it still follows the iPod afterwards: 80 given on the copy.
    stats::pull(
        conn,
        dev,
        &maps,
        &contents(vec![ipod_track(11, 0, 0, 60, 0)]),
        None,
    )
    .unwrap();
    assert_eq!(rating_of(conn, ids[0]), Some(60));
}

// Prevents (audit H4) a restored older database lowering ratings given since.
// `exec::restore` records the restored ratings; here, the baseline it leaves.
#[test]
fn an_older_rating_that_is_the_recorded_one_does_not_lower_the_stored_rating() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    put(conn, (ids[0], 0, 0, Some(80), None));
    // The restore saw rating 40 in the restored database and recorded it.
    record_ratings(conn, dev, &maps, &[(10, 40)]);
    let p = stats::pull(
        conn,
        dev,
        &maps,
        &contents(vec![ipod_track(10, 0, 0, 40, 0)]),
        None,
    )
    .unwrap();
    assert_eq!(p.tracks(), 0);
    assert_eq!(rating_of(conn, ids[0]), Some(80));
    // A rating given on the iPod after the restore still counts.
    stats::pull(
        conn,
        dev,
        &maps,
        &contents(vec![ipod_track(10, 0, 0, 100, 0)]),
        None,
    )
    .unwrap();
    assert_eq!(rating_of(conn, ids[0]), Some(100));
}

// Prevents (audit H4) one iPod without the rating clearing what another gave.
#[test]
fn a_second_ipod_without_the_rating_does_not_clear_it() {
    let (env, ids, maps_a) = world();
    let conn = env.store.conn();
    let a = device(conn);
    let b = devices::upsert_device(conn, "G2", None, None).unwrap();
    record_ratings(conn, a, &maps_a, &[]);
    stats::pull(
        conn,
        a,
        &maps_a,
        &contents(vec![ipod_track(10, 0, 0, 80, 0)]),
        None,
    )
    .unwrap();
    assert_eq!(rating_of(conn, ids[0]), Some(80), "set up");

    // The same library track, copied to iPod B as dbid 55, unrated.
    let maps_b = vec![mapping(ids[0], 55)];
    record_ratings(conn, b, &maps_b, &[]);
    let p = stats::pull(
        conn,
        b,
        &maps_b,
        &contents(vec![ipod_track(55, 2, 0, 0, 0)]),
        None,
    )
    .unwrap();
    assert_eq!(p.plays(), 2, "its plays are still counted");
    assert_eq!(rating_of(conn, ids[0]), Some(80));
    assert_eq!(row(conn, ids[0]).1, 2, "set up: row has B's two plays");
    // A's own record is untouched by B's pull.
    assert_eq!(recorded(conn, a).get(&10), Some(&80));
}

// Prevents (audit H4) the fix swallowing a real un-rate: 80, then 0 on the
// same dbid, clears the stored rating.
#[test]
fn clearing_a_rating_on_the_ipod_clears_the_stored_one() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    record_ratings(conn, dev, &maps, &[]);
    stats::pull(
        conn,
        dev,
        &maps,
        &contents(vec![ipod_track(10, 0, 0, 80, 0)]),
        None,
    )
    .unwrap();
    assert_eq!(rating_of(conn, ids[0]), Some(80), "set up");

    let p = stats::pull(
        conn,
        dev,
        &maps,
        &contents(vec![ipod_track(10, 0, 0, 0, 0)]),
        None,
    )
    .unwrap();
    assert_eq!(p.tracks(), 1);
    assert_eq!(rating_of(conn, ids[0]), None);
    assert_eq!(recorded(conn, dev).get(&10), Some(&0), "0 is recorded");

    // Seen again as 0, nothing more happens; rating once more is 80.
    let p = stats::pull(
        conn,
        dev,
        &maps,
        &contents(vec![ipod_track(10, 0, 0, 0, 0)]),
        None,
    )
    .unwrap();
    assert_eq!(p.tracks(), 0);
    stats::pull(
        conn,
        dev,
        &maps,
        &contents(vec![ipod_track(10, 0, 0, 80, 0)]),
        None,
    )
    .unwrap();
    assert_eq!(rating_of(conn, ids[0]), Some(80));
}

#[test]
fn a_mapping_with_no_recorded_rating_takes_a_rating_and_ignores_none() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    record_ratings(conn, dev, &maps, &[]);
    put(conn, (ids[0], 1, 0, Some(20), None));
    put(conn, (ids[1], 1, 0, Some(80), None));
    let c = contents(vec![
        ipod_track(10, 0, 0, 60, 0), // rated on the iPod: taken
        ipod_track(20, 0, 0, 0, 0),  // not rated, nothing recorded: ignored
    ]);
    let p = stats::pull(conn, dev, &maps, &c, None).unwrap();
    assert_eq!(p.tracks(), 1, "only the rated one is news");
    assert_eq!(rating_of(conn, ids[0]), Some(60));
    assert_eq!(rating_of(conn, ids[1]), Some(80));
    let rec = recorded(conn, dev);
    assert_eq!(rec.get(&10), Some(&60));
    assert_eq!(rec.get(&20), None, "an absent rating is not recorded");
}

// Same dbid, new library file (a retag, a replaced file): the iPod track and
// its recorded rating are the same ones.
#[test]
fn the_recorded_rating_survives_an_update_of_the_same_dbid_and_not_a_new_dbid() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    record_ratings(conn, dev, &maps, &[(10, 80), (20, 40)]);
    let mut same = mapping(ids[0], 10);
    same.synced_size = 99;
    let new = mapping(ids[1], 21);
    devices::apply_track_changes(conn, dev, &[same, new], &[]).unwrap();
    let rec = recorded(conn, dev);
    assert_eq!(rec.get(&10), Some(&80), "same dbid keeps it");
    assert_eq!(rec.get(&21), None, "a new dbid starts with none");
    assert_eq!(rec.get(&20), None, "and the old one is gone with its row");
    let m = devices::track_mappings(conn, dev).unwrap();
    assert_eq!(
        m.iter().find(|m| m.track_id == ids[0]).unwrap().synced_size,
        99,
        "set up: the row was updated"
    );
}

// ---- pending pulls written before ratings were recorded

#[test]
fn a_pending_pull_recorded_without_the_rating_baseline_is_still_undone() {
    let (env, ids, _) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    put(conn, (ids[0], 8, 1, Some(60), Some(2000)));
    // As the previous version wrote it: no `synced` field.
    let old = format!(
        r#"[{{"track_id":{},"plays":3,"skips":1,"rating":60,"last_played":2000,"before":[40,1000]}}]"#,
        ids[0]
    );
    conn.execute(
        "INSERT INTO pending_pulls (device_id, play_counts_hash, changes, at) VALUES (?1, ?2, ?3, 0)",
        rusqlite::params![dev, &H1[..], old],
    )
    .unwrap();
    let r = stats::recover(conn, dev, pc(Some(H1), None)).unwrap();
    assert_eq!(undone(&r), 1, "{r:?}");
    assert_eq!(rows(conn), vec![(ids[0], 5, 0, Some(40), Some(1000))]);
}

// ---- undo is relative (audit H5)

/// The same library track on two iPods: A's pull stays pending, B's pull
/// settles, then A recovers. `initial` is the row before; `a` and `b` are
/// each iPod's view (dbids 111 and 222).
fn two_ipods(initial: Option<Row>, a: Track, b: Track) -> (Env, i64, i64, i64) {
    let mut env = Env::new();
    let p = env.add("t.mp3", Kind::Mp3, 1);
    env.scan();
    let t = env.track(&p).id;
    let conn = env.store.conn();
    let dev_a = devices::upsert_device(conn, "GUID-A", None, None).unwrap();
    let dev_b = devices::upsert_device(conn, "GUID-B", None, None).unwrap();
    if let Some(r) = initial {
        put(conn, (t, r.1, r.2, r.3, r.4));
    }
    let maps_a = vec![mapping(t, 111)];
    let maps_b = vec![mapping(t, 222)];
    record_ratings(conn, dev_a, &maps_a, &[]);
    record_ratings(conn, dev_b, &maps_b, &[]);
    // 1. A pulls and its write never happens: the pull stays pending.
    stats::pull(conn, dev_a, &maps_a, &contents(vec![a]), Some(H1)).unwrap();
    // 2. B pulls and its write succeeds.
    stats::pull(conn, dev_b, &maps_b, &contents(vec![b]), Some(H2)).unwrap();
    stats::settle(conn, dev_b).unwrap();
    (env, t, dev_a, dev_b)
}

// Prevents (audit H5) undoing A's pull deleting or reverting what B added.
#[test]
fn undoing_a_pull_that_created_the_row_keeps_another_ipods_settled_stats() {
    // No row before. A: 2 plays. B: 3 plays, rating 80, played at 5000.
    let (env, t, dev_a, _) = two_ipods(
        None,
        ipod_track(111, 2, 0, 0, 0),
        lifetime_track(222, (3, 0), (3, 0), 80, 5000),
    );
    let conn = env.store.conn();
    assert_eq!(row(conn, t), (t, 5, 0, Some(80), Some(5000)), "set up");
    let r = stats::recover(conn, dev_a, pc(Some(H1), None)).unwrap();
    assert_eq!(undone(&r), 1);
    assert_eq!(
        row(conn, t),
        (t, 3, 0, Some(80), Some(5000)),
        "B's plays, rating and date survive; only A's two plays go"
    );
    assert!(pending(conn, dev_a).is_none());
}

#[test]
fn undoing_a_pull_keeps_the_rating_and_date_another_ipod_set_since() {
    // Row (5 plays, rating 40, at 1000). A: +3 plays, +1 skip, rating 60, at
    // 2000. B: +2 plays, rating 100, at 9000.
    let (env, t, dev_a, _) = two_ipods(
        Some((0, 5, 0, Some(40), Some(1000))),
        ipod_track(111, 3, 1, 60, 2000),
        ipod_track(222, 2, 0, 100, 9000),
    );
    let conn = env.store.conn();
    assert_eq!(row(conn, t), (t, 10, 1, Some(100), Some(9000)), "set up");
    let r = stats::recover(conn, dev_a, pc(Some(H1), None)).unwrap();
    assert_eq!(undone(&r), 1);
    assert_eq!(
        row(conn, t),
        (t, 7, 0, Some(100), Some(9000)),
        "A's plays and skip are taken back, B's rating and date stay"
    );
}

#[test]
fn undo_puts_the_rating_and_date_back_where_nothing_has_changed_them_since() {
    // Only plays came from B, so the rating and date are still A's and go.
    let (env, t, dev_a, _) = two_ipods(
        Some((0, 5, 0, Some(40), Some(1000))),
        ipod_track(111, 3, 1, 60, 2000),
        ipod_track(222, 2, 0, 0, 0),
    );
    let conn = env.store.conn();
    assert_eq!(row(conn, t), (t, 10, 1, Some(60), Some(2000)), "set up");
    stats::recover(conn, dev_a, pc(Some(H1), None)).unwrap();
    assert_eq!(row(conn, t), (t, 7, 0, Some(40), Some(1000)));
}

#[test]
fn undo_never_takes_plays_below_zero_and_deletes_only_a_row_back_to_defaults() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    // A row the pull created, with another source having removed plays.
    let created = stats::pull(
        conn,
        dev,
        &maps,
        &contents(vec![ipod_track(10, 3, 2, 0, 0)]),
        None,
    )
    .unwrap();
    conn.execute(
        "UPDATE stats SET plays = 1, skips = 0 WHERE track_id = ?1",
        [ids[0]],
    )
    .unwrap();
    stats::undo(conn, dev, &created).unwrap();
    assert!(rows(conn).is_empty(), "nothing left: the row goes");

    // A row that existed: the totals stop at zero.
    put(conn, (ids[1], 5, 5, None, None));
    let existing = stats::pull(
        conn,
        dev,
        &maps,
        &contents(vec![ipod_track(20, 3, 2, 0, 0)]),
        None,
    )
    .unwrap();
    conn.execute(
        "UPDATE stats SET plays = 1, skips = 1 WHERE track_id = ?1",
        [ids[1]],
    )
    .unwrap();
    stats::undo(conn, dev, &existing).unwrap();
    assert_eq!(rows(conn), vec![(ids[1], 0, 0, None, None)]);
}

#[test]
fn undo_leaves_a_recorded_rating_that_a_later_pull_changed() {
    let (env, _, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    record_ratings(conn, dev, &maps, &[]);
    let p = stats::pull(
        conn,
        dev,
        &maps,
        &contents(vec![ipod_track(10, 0, 0, 80, 0)]),
        Some(H1),
    )
    .unwrap();
    assert_eq!(recorded(conn, dev).get(&10), Some(&80), "set up");
    // The same iPod track was pulled again since and now shows 100.
    devices::set_synced_rating(conn, dev, 10, Some(100)).unwrap();
    stats::undo(conn, dev, &p).unwrap();
    assert_eq!(recorded(conn, dev).get(&10), Some(&100));

    // With nothing in between, undo forgets what the pull recorded, so the
    // rating is read as news again once the write has happened.
    devices::set_synced_rating(conn, dev, 10, Some(80)).unwrap();
    let p2 = stats::pull(
        conn,
        dev,
        &maps,
        &contents(vec![ipod_track(10, 0, 0, 60, 0)]),
        Some(H1),
    )
    .unwrap();
    assert_eq!(recorded(conn, dev).get(&10), Some(&60), "set up");
    stats::undo(conn, dev, &p2).unwrap();
    assert_eq!(recorded(conn, dev).get(&10), Some(&80));
    assert!(
        rows(conn).is_empty(),
        "and no rating of either pull is left"
    );
}

// ---- lifetime counts (audit M5)

// Prevents (audit M5) plays made under iTunes, before adoption, being lost
// once the iPod track goes: a track with no row starts from the iPod's
// lifetime counts, which already include the recent ones.
#[test]
fn a_track_with_no_row_starts_from_the_lifetime_counts_not_the_recent_ones() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    let c = contents(vec![lifetime_track(10, (10, 4), (2, 1), 0, 0)]);
    let p = stats::pull(conn, dev, &maps, &c, None).unwrap();
    assert_eq!(p.plays(), 10);
    assert_eq!(
        rows(conn),
        vec![(ids[0], 10, 4, None, None)],
        "10 and 4, not 12 and 5, not 2 and 1"
    );
}

#[test]
fn a_track_that_has_a_row_gets_only_the_recent_counts() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    put(conn, (ids[0], 10, 4, None, None));
    let c = contents(vec![lifetime_track(10, (120, 40), (3, 1), 0, 0)]);
    let p = stats::pull(conn, dev, &maps, &c, None).unwrap();
    assert_eq!(p.plays(), 3);
    assert_eq!(rows(conn), vec![(ids[0], 13, 5, None, None)]);
}

#[test]
fn a_row_with_zero_counts_is_still_a_row_so_lifetime_counts_are_not_added_twice() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    put(conn, (ids[0], 0, 0, Some(40), None));
    let c = contents(vec![lifetime_track(10, (30, 2), (1, 0), 40, 0)]);
    stats::pull(conn, dev, &maps, &c, None).unwrap();
    assert_eq!(rows(conn), vec![(ids[0], 1, 0, Some(40), None)]);
}

// A pull that is undone and made again must give the same result: the row
// the first one created is gone, so the second starts from lifetime again.
#[test]
fn an_undone_first_pull_starts_from_the_lifetime_counts_again() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    let c = contents(vec![lifetime_track(10, (10, 0), (2, 0), 0, 0)]);
    let p = stats::pull(conn, dev, &maps, &c, None).unwrap();
    stats::undo(conn, dev, &p).unwrap();
    assert!(rows(conn).is_empty());
    stats::pull(conn, dev, &maps, &c, None).unwrap();
    assert_eq!(row(conn, ids[0]).1, 10);
}

// ---- stats.csv (audit M4)

fn kept_csvs(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy().into_owned();
            n.starts_with("stats-") && n.ends_with(".csv")
        })
        .collect();
    v.sort();
    v
}

const HEADER: &str = "path,plays,skips,rating,last_played\n";

// Prevents (audit M4) a rebuilt index replacing the fuller stats.csv.
#[test]
fn an_export_that_would_shrink_stats_csv_keeps_the_old_file_beside_it() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    let conn = env.store.conn();
    put(conn, (env.track(&a).id, 2, 0, None, None));
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("stats.csv");
    let old = format!(
        "{HEADER}/gone/one.mp3,40,0,,\n/gone/two.mp3,7,1,80,\n{}\n",
        "x,1,0,,"
    );
    std::fs::write(&csv, &old).unwrap();

    let n = stats::export_csv(conn, &csv).unwrap();
    assert_eq!(n, 1);
    let kept = kept_csvs(dir.path());
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(std::fs::read_to_string(&kept[0]).unwrap(), old);
    let new = std::fs::read_to_string(&csv).unwrap();
    assert!(new.starts_with(HEADER) && new.contains(",2,0,,"), "{new}");
    assert!(!new.contains("gone"), "{new}");
}

#[test]
fn an_export_with_more_plays_in_fewer_rows_also_keeps_the_old_file() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    let conn = env.store.conn();
    put(conn, (env.track(&a).id, 2, 0, None, None));
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("stats.csv");
    // One row like the new one, with more plays.
    std::fs::write(&csv, format!("{HEADER}/old/a.mp3,9,0,,\n")).unwrap();
    stats::export_csv(conn, &csv).unwrap();
    assert_eq!(kept_csvs(dir.path()).len(), 1);
}

#[test]
fn an_export_that_grows_stats_csv_keeps_no_copy() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    let b = env.add("b.mp3", Kind::Mp3, 2);
    env.scan();
    let conn = env.store.conn();
    put(conn, (env.track(&a).id, 2, 0, None, None));
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("stats.csv");
    stats::export_csv(conn, &csv).unwrap();
    // More plays, then more rows, and the same again.
    conn.execute("UPDATE stats SET plays = 5", []).unwrap();
    stats::export_csv(conn, &csv).unwrap();
    put(conn, (env.track(&b).id, 1, 0, None, None));
    stats::export_csv(conn, &csv).unwrap();
    stats::export_csv(conn, &csv).unwrap();
    assert!(
        kept_csvs(dir.path()).is_empty(),
        "{:?}",
        kept_csvs(dir.path())
    );
    let text = std::fs::read_to_string(&csv).unwrap();
    assert_eq!(text.lines().count(), 3, "{text}");
}

// A quoted path with commas must not hide its plays from the comparison.
#[test]
fn the_plays_of_a_quoted_path_with_commas_count_when_comparing() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    let conn = env.store.conn();
    put(conn, (env.track(&a).id, 2, 0, None, None));
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("stats.csv");
    // Same row count as the new export; only the plays differ, and the
    // commas of the path are in front of them.
    std::fs::write(&csv, format!("{HEADER}\"/m/Doe, Jane/1,2,3.mp3\",9,0,,\n")).unwrap();
    stats::export_csv(conn, &csv).unwrap();
    assert_eq!(kept_csvs(dir.path()).len(), 1);

    // The same file with fewer plays than the new export keeps nothing.
    std::fs::remove_file(kept_csvs(dir.path())[0].clone()).unwrap();
    std::fs::write(&csv, format!("{HEADER}\"/m/Doe, Jane/1,2,3.mp3\",1,0,,\n")).unwrap();
    stats::export_csv(conn, &csv).unwrap();
    assert!(kept_csvs(dir.path()).is_empty());
}

#[test]
fn a_stats_csv_that_cannot_be_read_as_text_is_replaced_without_a_copy() {
    let env = Env::new();
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("stats.csv");
    std::fs::write(&csv, [0xff, 0xfe, 0x00, 0xff]).unwrap();
    stats::export_csv(env.store.conn(), &csv).unwrap();
    assert_eq!(std::fs::read_to_string(&csv).unwrap(), HEADER);
    assert!(kept_csvs(dir.path()).is_empty());
}

// A track with no row takes the iPod's rating whatever is recorded for it
// (a lost row, a restored baseline), and 0 is still ignored.
#[test]
fn a_track_with_no_row_takes_the_ipods_rating_even_when_one_is_recorded() {
    let (env, ids, maps) = world();
    let conn = env.store.conn();
    let dev = device(conn);
    record_ratings(conn, dev, &maps, &[(10, 80), (20, 60), (30, 40)]);
    let c = contents(vec![
        ipod_track(10, 0, 0, 80, 0),  // same as recorded
        ipod_track(20, 0, 0, 100, 0), // different from recorded
        ipod_track(30, 0, 0, 0, 0),   // none on the iPod
    ]);
    stats::pull(conn, dev, &maps, &c, None).unwrap();
    assert_eq!(rating_of(conn, ids[0]), Some(80));
    assert_eq!(rating_of(conn, ids[1]), Some(100));
    assert!(
        rows(conn).iter().all(|r| r.0 != ids[2] || r.3.is_none()),
        "0 on the iPod gives no rating"
    );
    let rec = recorded(conn, dev);
    assert_eq!(rec.get(&10), Some(&80));
    assert_eq!(rec.get(&20), Some(&100), "a differing one is updated");
}

#[test]
fn kept_copies_of_stats_csv_made_in_the_same_second_do_not_overwrite_each_other() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    let conn = env.store.conn();
    put(conn, (env.track(&a).id, 2, 0, None, None));
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("stats.csv");
    let mut wrote = Vec::new();
    for plays in [40, 50, 60] {
        let text = format!("{HEADER}/old/{plays}.mp3,{plays},0,,\n");
        std::fs::write(&csv, &text).unwrap();
        stats::export_csv(conn, &csv).unwrap();
        wrote.push(text);
    }
    let mut kept: Vec<String> = kept_csvs(dir.path())
        .iter()
        .map(|p| std::fs::read_to_string(p).unwrap())
        .collect();
    kept.sort();
    wrote.sort();
    assert_eq!(kept, wrote, "every replaced file is kept");
}

// ---- kept copies are bounded

#[test]
fn seven_shrinking_exports_leave_the_five_newest_copies() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    let conn = env.store.conn();
    put(conn, (env.track(&a).id, 2, 0, None, None));
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("stats.csv");
    let mut wrote = Vec::new();
    for plays in 40..47 {
        let text = format!("{HEADER}/old/{plays}.mp3,{plays},0,,\n");
        std::fs::write(&csv, &text).unwrap();
        stats::export_csv(conn, &csv).unwrap();
        wrote.push(text);
    }
    let mut kept: Vec<String> = kept_csvs(dir.path())
        .iter()
        .map(|p| std::fs::read_to_string(p).unwrap())
        .collect();
    kept.sort();
    assert_eq!(kept, wrote[2..], "the two oldest are gone");
    assert!(csv.is_file());
}

#[test]
fn pruning_orders_copies_of_one_second_by_number_not_by_text_and_leaves_other_files() {
    let mut env = Env::new();
    let a = env.add("a.mp3", Kind::Mp3, 1);
    env.scan();
    let conn = env.store.conn();
    put(conn, (env.track(&a).id, 2, 0, None, None));
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("stats.csv");
    // Copies of an old second: no suffix, then -2 to -11; and an older one.
    let mut old = vec!["stats-20200101-000000.csv".to_owned()];
    old.extend((2..=11).map(|n| format!("stats-20200101-000000-{n}.csv")));
    old.push("stats-20190101-000000.csv".to_owned());
    for n in &old {
        std::fs::write(dir.path().join(n), n).unwrap();
    }
    let others = [
        "notes.txt",
        "stats-summary.txt",
        "stats.csv.bak",
        "other.csv",
    ];
    for n in others {
        std::fs::write(dir.path().join(n), n).unwrap();
    }
    std::fs::write(&csv, format!("{HEADER}/old/x.mp3,99,0,,\n")).unwrap();
    stats::export_csv(conn, &csv).unwrap();

    let names: Vec<String> = kept_csvs(dir.path())
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    // The new copy (today's stamp) and the four newest of the old second:
    // -11, -10, -9, -8. Sorted as text, -10 and -11 would be dropped.
    let mut want: Vec<String> = vec![
        "stats-20200101-000000-11.csv".into(),
        "stats-20200101-000000-10.csv".into(),
        "stats-20200101-000000-9.csv".into(),
        "stats-20200101-000000-8.csv".into(),
    ];
    let new: Vec<&String> = names
        .iter()
        .filter(|n| !n.starts_with("stats-2020"))
        .collect();
    assert_eq!(new.len(), 1, "{names:?}");
    want.push(new[0].clone());
    want.sort();
    assert_eq!(names, want);
    for n in others {
        assert!(dir.path().join(n).is_file(), "{n} is left alone");
    }
    assert!(csv.is_file());
}
