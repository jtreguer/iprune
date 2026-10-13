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

/// One mapped track: the library id, the iPod's dbid, the iPod's tag hash
/// and size, and the library file's duration at the last sync.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TrackMapping {
    pub track_id: i64,
    pub dbid: u64,
    pub synced_tags_hash: [u8; 32],
    pub synced_size: u64,
    pub synced_duration_ms: Option<u32>,
}

/// One mapped playlist. `ipod_id` is `None` for mappings saved before ids
/// were recorded; the name is then the only way to find the iPod playlist.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlaylistMapping {
    pub playlist_id: i64,
    pub ipod_id: Option<u64>,
    pub ipod_name: String,
}

/// Replaces every mapping of `device_id` with the given ones.
pub fn replace_mappings(
    conn: &Connection,
    device_id: i64,
    tracks: &[TrackMapping],
    playlists: &[PlaylistMapping],
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
        "INSERT INTO device_tracks (device_id, track_id, dbid, synced_tags_hash, synced_size, synced_duration_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for m in tracks {
        // dbid is a u64; SQLite stores it bit for bit as an i64.
        stmt.execute(params![
            device_id,
            m.track_id,
            m.dbid as i64,
            &m.synced_tags_hash[..],
            m.synced_size as i64,
            m.synced_duration_ms
        ])?;
    }
    let mut stmt = conn.prepare(
        "INSERT INTO device_playlists (device_id, playlist_id, ipod_name, ipod_id) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for m in playlists {
        stmt.execute(params![
            device_id,
            m.playlist_id,
            m.ipod_name,
            m.ipod_id.map(|id| id as i64)
        ])?;
    }
    // Tombstones no device maps any more have nothing left to delete.
    conn.execute(
        "DELETE FROM playlists WHERE removed_at IS NOT NULL \
         AND id NOT IN (SELECT playlist_id FROM device_playlists)",
        [],
    )?;
    Ok(())
}

/// Replaces the device's playlist mappings, then drops tombstones no
/// device maps any more: their iPod playlists are gone.
pub fn replace_playlist_mappings(
    conn: &Connection,
    device_id: i64,
    playlists: &[PlaylistMapping],
) -> Result<()> {
    conn.execute(
        "DELETE FROM device_playlists WHERE device_id = ?1",
        [device_id],
    )?;
    let mut stmt = conn.prepare(
        "INSERT INTO device_playlists (device_id, playlist_id, ipod_name, ipod_id) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for m in playlists {
        stmt.execute(params![
            device_id,
            m.playlist_id,
            m.ipod_name,
            m.ipod_id.map(|id| id as i64)
        ])?;
    }
    conn.execute(
        "DELETE FROM playlists WHERE removed_at IS NOT NULL \
         AND id NOT IN (SELECT playlist_id FROM device_playlists)",
        [],
    )?;
    Ok(())
}

pub fn set_last_sync(conn: &Connection, device_id: i64, at: i64) -> Result<()> {
    conn.execute(
        "UPDATE devices SET last_sync_at = ?2 WHERE id = ?1",
        params![device_id, at],
    )?;
    Ok(())
}

/// Every playlist mapped on the device, tombstones included.
pub fn playlist_mappings(conn: &Connection, device_id: i64) -> Result<Vec<PlaylistMapping>> {
    let mut stmt = conn.prepare(
        "SELECT playlist_id, ipod_id, ipod_name FROM device_playlists WHERE device_id = ?1 ORDER BY playlist_id",
    )?;
    let rows = stmt
        .query_map([device_id], |r| {
            Ok(PlaylistMapping {
                playlist_id: r.get(0)?,
                ipod_id: r.get::<_, Option<i64>>(1)?.map(|id| id as u64),
                ipod_name: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn track_mappings(conn: &Connection, device_id: i64) -> Result<Vec<TrackMapping>> {
    let mut stmt = conn.prepare(
        "SELECT track_id, dbid, synced_tags_hash, synced_size, synced_duration_ms FROM device_tracks \
         WHERE device_id = ?1 ORDER BY track_id",
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
                synced_duration_ms: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Every indexed playlist with its resolved members, by name.
pub fn library_playlists(conn: &Connection) -> Result<Vec<LibPlaylist>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.name, i.position, i.track_id FROM playlists p \
         LEFT JOIN playlist_items i ON i.playlist_id = p.id WHERE p.removed_at IS NULL \
         ORDER BY p.name, i.position",
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
