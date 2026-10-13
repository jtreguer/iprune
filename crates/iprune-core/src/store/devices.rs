//! Devices and the mapping between library and iPod tracks.

use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::adopt::LibPlaylist;

pub fn device_id(conn: &Connection, guid: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM devices WHERE firewire_guid = ?1",
            [guid],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn upsert_device(
    conn: &Connection,
    guid: &str,
    name: Option<&str>,
    model: Option<&str>,
) -> Result<i64> {
    Ok(conn.query_row(
        "INSERT INTO devices (firewire_guid, name, model) VALUES (?1, ?2, ?3) \
         ON CONFLICT(firewire_guid) DO UPDATE SET name = excluded.name, model = excluded.model RETURNING id",
        params![guid, name, model],
        |r| r.get(0),
    )?)
}

pub fn mapped_track_count(conn: &Connection, device_id: i64) -> Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM device_tracks WHERE device_id = ?1",
        [device_id],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

/// One adopted track: the library id, the iPod's dbid, and the iPod's tag
/// hash and size at adoption time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackMapping {
    pub track_id: i64,
    pub dbid: u64,
    pub synced_tags_hash: [u8; 32],
    pub synced_size: u64,
}

/// Replaces every mapping of `device_id` with the given ones.
pub fn replace_mappings(
    conn: &Connection,
    device_id: i64,
    tracks: &[TrackMapping],
    playlists: &[(i64, String)],
) -> Result<()> {
    conn.execute(
        "DELETE FROM device_tracks WHERE device_id = ?1",
        [device_id],
    )?;
    conn.execute(
        "DELETE FROM device_playlists WHERE device_id = ?1",
        [device_id],
    )?;
    let mut stmt = conn.prepare(
        "INSERT INTO device_tracks (device_id, track_id, dbid, synced_tags_hash, synced_size) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for m in tracks {
        // dbid is a u64; SQLite stores it bit for bit as an i64.
        stmt.execute(params![
            device_id,
            m.track_id,
            m.dbid as i64,
            &m.synced_tags_hash[..],
            m.synced_size as i64
        ])?;
    }
    let mut stmt = conn.prepare(
        "INSERT INTO device_playlists (device_id, playlist_id, ipod_name) VALUES (?1, ?2, ?3)",
    )?;
    for (playlist_id, name) in playlists {
        stmt.execute(params![device_id, playlist_id, name])?;
    }
    Ok(())
}

pub fn track_mappings(conn: &Connection, device_id: i64) -> Result<Vec<TrackMapping>> {
    let mut stmt = conn.prepare(
        "SELECT track_id, dbid, synced_tags_hash, synced_size FROM device_tracks WHERE device_id = ?1 ORDER BY track_id",
    )?;
    let rows = stmt
        .query_map([device_id], |r| {
            let hash: Vec<u8> = r.get(2)?;
            Ok(TrackMapping {
                track_id: r.get(0)?,
                dbid: r.get::<_, i64>(1)? as u64,
                synced_tags_hash: hash.try_into().map_err(|_| {
                    rusqlite::Error::InvalidColumnType(
                        2,
                        "synced_tags_hash".into(),
                        rusqlite::types::Type::Blob,
                    )
                })?,
                synced_size: r.get::<_, i64>(3)? as u64,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Every indexed playlist with its resolved members, by name.
pub fn library_playlists(conn: &Connection) -> Result<Vec<LibPlaylist>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.name, i.position, i.track_id FROM playlists p \
         LEFT JOIN playlist_items i ON i.playlist_id = p.id ORDER BY p.name, i.position",
    )?;
    let mut out: Vec<LibPlaylist> = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        if out.last().is_none_or(|p| p.id != id) {
            out.push(LibPlaylist {
                id,
                name: r.get(1)?,
                members: Vec::new(),
            });
        }
        // An empty playlist gives one row with no position: no member. An
        // unresolved entry has a position and no track: a `None` member.
        if r.get::<_, Option<i64>>(2)?.is_some() {
            out.last_mut()
                .expect("pushed above")
                .members
                .push(r.get(3)?);
        }
    }
    Ok(out)
}
