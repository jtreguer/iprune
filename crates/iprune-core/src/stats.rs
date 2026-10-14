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
/// libgpod renames `Play Counts` to `Play Counts.bak` only once a write
/// succeeds, so:
/// - the `.bak` is the file pulled from: the write happened, the pull stands;
/// - a `Play Counts` is still there (the same, or grown since with new
///   plays): the write never happened, the pull is undone and the next pull
///   counts every play once;
/// - neither: the file pulled from was removed by a write, the pull stands.
///
/// A pull made with no `Play Counts` carried no plays and is undone; the
/// next pull takes its ratings and dates again.
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
/// takes its rating, and keeps the latest play date, and records the pull as
/// pending until [`settle`]. One savepoint. `play_counts` is the hash of the
/// iPod's `Play Counts` file now.
pub fn pull(
    conn: &Connection,
    device_id: i64,
    mappings: &[TrackMapping],
    ipod: &Contents,
    play_counts: Option<[u8; 32]>,
) -> Result<Pulled> {
    let by_dbid: HashMap<u64, i64> = mappings.iter().map(|m| (m.dbid, m.track_id)).collect();
    let mut changes = Vec::new();
    for t in &ipod.tracks {
        let Some(&track_id) = by_dbid.get(&t.adopt.dbid) else {
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
        let rating = (t.rating > 0).then_some(i64::from(t.rating));
        let last_played = match (old_played, (t.last_played > 0).then_some(t.last_played)) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        if t.recent_plays == 0
            && t.recent_skips == 0
            && rating == old_rating
            && last_played == old_played
        {
            continue;
        }
        changes.push(Change {
            track_id,
            plays: t.recent_plays,
            skips: t.recent_skips,
            rating,
            last_played,
            before,
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
pub fn undo(conn: &Connection, device_id: i64, pulled: &Pulled) -> Result<()> {
    store::savepoint(conn, || {
        settle(conn, device_id)?;
        for c in &pulled.changes {
            match c.before {
                None => {
                    conn.execute("DELETE FROM stats WHERE track_id = ?1", [c.track_id])?;
                }
                Some((rating, last_played)) => {
                    conn.execute(
                        "UPDATE stats SET plays = plays - ?2, skips = skips - ?3, rating = ?4, \
                         last_played = ?5 WHERE track_id = ?1",
                        params![c.track_id, c.plays, c.skips, rating, last_played],
                    )?;
                }
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

/// Writes every track with statistics to `path` as CSV, replacing it
/// atomically: path, plays, skips, rating, last_played (Unix time).
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
    let mode = match crate::playlist::mtime_ns(path) {
        Ok(mtime_ns) => WriteMode::Replace { mtime_ns },
        Err(_) => WriteMode::CreateNew,
    };
    write_atomic(path, &out, mode)?;
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
