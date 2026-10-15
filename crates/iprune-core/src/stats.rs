//! Play statistics. They come from the iPod only. They are pulled into SQLite
//! before every write, because libgpod resets the recent counts when it
//! writes, and exported to `stats.csv` after each sync so they outlive the
//! index.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::ipod::Contents;
use crate::playlist::{WriteMode, write_atomic};
use crate::store::{self, devices::TrackMapping};

/// One track's change, kept so a pull can be undone if the write fails: the
/// iPod still holds the same recent counts and would give them again.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Change {
    track_id: i64,
    plays: u32,
    skips: u32,
    rating: Option<i64>,
    last_played: Option<i64>,
    /// The row before the pull, `None` when there was none.
    before: Option<(Option<i64>, Option<i64>)>,
    /// The iPod track's rating recorded as seen, `(dbid, before, after)`,
    /// when this pull changed it. Absent from pulls recorded before it was
    /// kept.
    #[serde(default)]
    synced: Option<(u64, Option<u32>, u32)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pulled {
    changes: Vec<Change>,
}

impl Pulled {
    /// Tracks whose statistics changed.
    pub fn tracks(&self) -> usize {
        self.changes.len()
    }

    pub fn plays(&self) -> u64 {
        self.changes.iter().map(|c| u64::from(c.plays)).sum()
    }
}

/// The iPod's `Play Counts` file and the `Play Counts.bak` libgpod renames
/// it to on a successful write, each hashed, `None` when absent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlayCounts {
    pub live: Option<[u8; 32]>,
    pub renamed: Option<[u8; 32]>,
}

fn hash_file(path: &Path) -> Result<Option<[u8; 32]>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(*blake3::hash(&bytes).as_bytes())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(crate::Error::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

pub fn play_counts(mountpoint: &Path) -> Result<PlayCounts> {
    let dir = mountpoint.join("iPod_Control/iTunes");
    Ok(PlayCounts {
        live: hash_file(&dir.join("Play Counts"))?,
        renamed: hash_file(&dir.join("Play Counts.bak"))?,
    })
}

/// What [`recover`] did with a pull left pending.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovery {
    /// Tracks whose pull was undone because the write never happened.
    pub undone: usize,
    /// Set when the pending record could not be read and was dropped.
    pub dropped: Option<String>,
}

/// Settles a pull left pending by a sync that stopped between the pull and
/// the end of the write (a crash, Ctrl+C, a write that failed part way).
/// Just before the first write iprune renames `Play Counts` to
/// `Play Counts.bak` itself (libgpod would only after replacing `iTunesDB`),
/// and puts it back when a refused write left the iPod unchanged, so:
/// - the `.bak` is the file pulled from: the write was reached, the pull
///   stands;
/// - a `Play Counts` is still there (the same, or grown since with new
///   plays): the write never happened, the pull is undone and the next pull
///   counts every play once;
/// - neither: the file pulled from was removed by a write, the pull stands.
///
/// A pull made with no `Play Counts` carried no recent plays and is undone;
/// the next pull takes them again.
pub fn recover(conn: &Connection, device_id: i64, now: PlayCounts) -> Result<Recovery> {
    let pending: Option<(Option<Vec<u8>>, String)> = conn
        .query_row(
            "SELECT play_counts_hash, changes FROM pending_pulls WHERE device_id = ?1",
            [device_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((hash, changes)) = pending else {
        return Ok(Recovery::default());
    };
    let changes: Vec<Change> = match serde_json::from_str(&changes) {
        Ok(c) => c,
        Err(e) => {
            settle(conn, device_id)?;
            return Ok(Recovery {
                undone: 0,
                dropped: Some(format!(
                    "an unreadable pending statistics pull was dropped: {e}"
                )),
            });
        }
    };
    let pulled_from = hash.as_deref();
    let written = match pulled_from {
        None => false,
        Some(h) if now.renamed.as_ref().map(|r| &r[..]) == Some(h) => true,
        Some(_) => now.live.is_none(),
    };
    if written {
        settle(conn, device_id)?;
        return Ok(Recovery::default());
    }
    let undone = changes.len();
    undo(conn, device_id, &Pulled { changes })?;
    Ok(Recovery {
        undone,
        dropped: None,
    })
}

/// Marks the pull for `device_id` as settled: the write happened.
pub fn settle(conn: &Connection, device_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM pending_pulls WHERE device_id = ?1",
        [device_id],
    )?;
    Ok(())
}

/// Adds the iPod's recent plays and skips to each mapped track's totals,
/// and keeps the latest play date. A track with no statistics yet starts
/// from the iPod's lifetime counts (the recent ones included), so plays
/// made before adoption, under iTunes, are kept. The iPod's rating is taken
/// only when it changed on the iPod since the last pull of that iPod track
/// (`device_tracks.synced_rating`): a new copy, a re-copy, a restored
/// database or another iPod showing no rating or an older one does not
/// overwrite the stored rating, while clearing it on the iPod does. With no
/// rating recorded yet, a rating on the iPod is taken and none is not. The
/// pull is recorded as pending until [`settle`]. One savepoint.
/// `play_counts` is the hash of the iPod's `Play Counts` file now.
pub fn pull(
    conn: &Connection,
    device_id: i64,
    mappings: &[TrackMapping],
    ipod: &Contents,
    play_counts: Option<[u8; 32]>,
) -> Result<Pulled> {
    let by_dbid: HashMap<u64, i64> = mappings.iter().map(|m| (m.dbid, m.track_id)).collect();
    let seen = store::devices::synced_ratings(conn, device_id)?;
    let mut changes = Vec::new();
    for t in &ipod.tracks {
        let dbid = t.adopt.dbid;
        let Some(&track_id) = by_dbid.get(&dbid) else {
            continue;
        };
        let before: Option<(Option<i64>, Option<i64>)> = conn
            .query_row(
                "SELECT rating, last_played FROM stats WHERE track_id = ?1",
                [track_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (old_rating, old_played) = before.unwrap_or((None, None));
        let on_ipod = (t.rating > 0).then_some(i64::from(t.rating));
        let (rating, synced) = match seen.get(&dbid) {
            // A track with no statistics yet takes the iPod's rating.
            Some(&was) if before.is_none() && t.rating > 0 => (
                on_ipod,
                (was != t.rating).then_some((dbid, Some(was), t.rating)),
            ),
            Some(&was) if was == t.rating => (old_rating, None),
            Some(&was) => (on_ipod, Some((dbid, Some(was), t.rating))),
            None if t.rating > 0 => (on_ipod, Some((dbid, None, t.rating))),
            None => (old_rating, None),
        };
        let last_played = match (old_played, (t.last_played > 0).then_some(t.last_played)) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        // libgpod folds the recent counts into the lifetime ones it reads.
        let (plays, skips) = match before {
            None => (t.adopt.playcount, t.skips),
            Some(_) => (t.recent_plays, t.recent_skips),
        };
        if plays == 0
            && skips == 0
            && rating == old_rating
            && last_played == old_played
            && synced.is_none()
        {
            continue;
        }
        changes.push(Change {
            track_id,
            plays,
            skips,
            rating,
            last_played,
            before,
            synced,
        });
    }
    store::savepoint(conn, || {
        let mut stmt = conn.prepare(
            "INSERT INTO stats (track_id, plays, skips, rating, last_played) VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(track_id) DO UPDATE SET plays = plays + excluded.plays, \
             skips = skips + excluded.skips, rating = excluded.rating, last_played = excluded.last_played",
        )?;
        for c in &changes {
            stmt.execute(params![
                c.track_id,
                c.plays,
                c.skips,
                c.rating,
                c.last_played
            ])?;
            if let Some((dbid, _, now)) = c.synced {
                store::devices::set_synced_rating(conn, device_id, dbid, Some(now))?;
            }
        }
        conn.execute(
            "INSERT OR REPLACE INTO pending_pulls (device_id, play_counts_hash, changes, at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                device_id,
                play_counts.as_ref().map(|h| &h[..]),
                serde_json::to_string(&changes).expect("plain data serializes"),
                crate::now_unix()
            ],
        )?;
        Ok(())
    })?;
    changes.sort_by_key(|c| c.track_id);
    Ok(Pulled { changes })
}

/// Reverses a pull whose write did not happen, and clears it as pending.
/// Only this pull's part is taken back: its plays and skips are subtracted,
/// and the rating, last play and recorded iPod rating return to what they
/// were only where they still hold what this pull set. Another iPod's pull
/// made since is kept. A row this pull created is deleted only when nothing
/// else is left in it.
pub fn undo(conn: &Connection, device_id: i64, pulled: &Pulled) -> Result<()> {
    store::savepoint(conn, || {
        settle(conn, device_id)?;
        for c in &pulled.changes {
            let (rating, last_played) = c.before.unwrap_or((None, None));
            conn.execute(
                "UPDATE stats SET plays = max(plays - ?2, 0), skips = max(skips - ?3, 0) \
                 WHERE track_id = ?1",
                params![c.track_id, c.plays, c.skips],
            )?;
            conn.execute(
                "UPDATE stats SET rating = ?2 WHERE track_id = ?1 AND rating IS ?3",
                params![c.track_id, rating, c.rating],
            )?;
            conn.execute(
                "UPDATE stats SET last_played = ?2 WHERE track_id = ?1 AND last_played IS ?3",
                params![c.track_id, last_played, c.last_played],
            )?;
            if c.before.is_none() {
                conn.execute(
                    "DELETE FROM stats WHERE track_id = ?1 AND plays = 0 AND skips = 0 \
                     AND rating IS NULL AND last_played IS NULL",
                    [c.track_id],
                )?;
            }
            if let Some((dbid, was, now)) = c.synced {
                conn.execute(
                    "UPDATE device_tracks SET synced_rating = ?4 \
                     WHERE device_id = ?1 AND dbid = ?2 AND synced_rating IS ?3",
                    params![device_id, dbid as i64, now, was],
                )?;
            }
        }
        Ok(())
    })
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

/// Kept copies of `stats.csv` beyond the newest this many are deleted.
const KEEP_STATS: usize = 5;

/// Deletes the oldest `stats-*.csv` kept beside `path`, keeping `keep`.
fn prune_kept(path: &Path, keep: usize) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut kept: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map_err(|source| crate::Error::Io {
            path: dir.to_owned(),
            source,
        })?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("stats-") && n.ends_with(".csv"))
        })
        .collect();
    // The stamp sorts by time; `-N` copies of one second follow it.
    kept.sort_by_key(|p| {
        let name = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let stem = name.trim_end_matches(".csv").to_owned();
        let (base, n) = match stem.get(21..).and_then(|r| r.strip_prefix('-')) {
            Some(n) => (stem[..21].to_owned(), n.parse::<u32>().unwrap_or(u32::MAX)),
            None => (stem, 1),
        };
        (base, n)
    });
    let excess = kept.len().saturating_sub(keep);
    for p in kept.into_iter().take(excess) {
        std::fs::remove_file(&p).map_err(|source| crate::Error::Io {
            path: p.clone(),
            source,
        })?;
    }
    Ok(())
}

/// Rows and total plays of a `stats.csv`; `None` when it cannot be read.
fn csv_totals(path: &Path) -> Option<(usize, u64)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut rows = 0;
    let mut plays = 0u64;
    for line in text.lines().skip(1) {
        rows += 1;
        // path,plays,skips,rating,last_played: the path may be quoted and
        // hold commas, the four last fields cannot.
        let fields: Vec<&str> = line.rsplitn(5, ',').collect();
        plays += fields
            .get(3)
            .and_then(|p| p.parse::<u64>().ok())
            .unwrap_or(0);
    }
    Some((rows, plays))
}

/// Writes every track with statistics to `path` as CSV, replacing it
/// atomically: path, plays, skips, rating, last_played (Unix time). A file
/// there with more rows or more plays than the new one (the index was lost
/// and rebuilt, or a bad pull) is not lost: it is kept beside it as
/// `stats-<stamp>.csv`.
pub fn export_csv(conn: &Connection, path: &Path) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT t.path, s.plays, s.skips, s.rating, s.last_played FROM stats s \
         JOIN tracks t ON t.id = s.track_id ORDER BY t.path",
    )?;
    let mut out = String::from("path,plays,skips,rating,last_played\n");
    let mut n = 0;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let opt = |v: Option<i64>| v.map(|v| v.to_string()).unwrap_or_default();
        let _ = writeln!(
            out,
            "{},{},{},{},{}",
            csv_field(&r.get::<_, String>(0)?),
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            opt(r.get(3)?),
            opt(r.get(4)?)
        );
        n += 1;
    }
    let plays: u64 = {
        let mut stmt = conn.prepare(
            "SELECT COALESCE(SUM(s.plays), 0) FROM stats s JOIN tracks t ON t.id = s.track_id",
        )?;
        stmt.query_row([], |r| r.get::<_, i64>(0))? as u64
    };
    if let Some((old_rows, old_plays)) = csv_totals(path)
        && (old_rows > n || old_plays > plays)
    {
        // After the highest copy of this second, never in a gap a prune
        // left: the new copy must sort last.
        let stamp = crate::backup::stamp(crate::now_unix());
        let dir = path.parent().unwrap_or(Path::new("."));
        let taken = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter_map(|n| {
                match n
                    .strip_prefix(&format!("stats-{stamp}"))?
                    .strip_suffix(".csv")?
                {
                    "" => Some(1),
                    rest => rest.strip_prefix('-')?.parse::<u32>().ok(),
                }
            })
            .max();
        let kept = match taken {
            None => path.with_file_name(format!("stats-{stamp}.csv")),
            Some(k) => path.with_file_name(format!("stats-{stamp}-{}.csv", k + 1)),
        };
        std::fs::rename(path, &kept).map_err(|source| crate::Error::Io {
            path: kept.clone(),
            source,
        })?;
    }
    let mode = match crate::playlist::mtime_ns(path) {
        Ok(mtime_ns) => WriteMode::Replace { mtime_ns },
        Err(_) => WriteMode::CreateNew,
    };
    write_atomic(path, &out, mode)?;
    // Old copies are only tidied: a failure here loses nothing.
    let _ = prune_kept(path, KEEP_STATS);
    Ok(n)
}

/// One track's totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrackStats {
    pub plays: u32,
    pub skips: u32,
    /// 0 to 100, `None` when unrated.
    pub rating: Option<u32>,
    pub last_played: Option<i64>,
}

/// Every track's statistics, by track id.
pub fn all(conn: &Connection) -> Result<HashMap<i64, TrackStats>> {
    let mut stmt = conn.prepare("SELECT track_id, plays, skips, rating, last_played FROM stats")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                TrackStats {
                    plays: r.get(1)?,
                    skips: r.get(2)?,
                    rating: r.get(3)?,
                    last_played: r.get(4)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}
