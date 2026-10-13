-- A playlist whose file is deleted while an iPod playlist is mapped to it
-- stays as a tombstone until a sync deletes the iPod playlist: the mapping
-- is what tells the sync which iPod playlist to delete.
ALTER TABLE playlists ADD COLUMN removed_at INTEGER;

-- The iPod playlist's persistent id, so two iPod playlists with the same
-- name cannot be confused. NULL for mappings saved before this column.
ALTER TABLE device_playlists ADD COLUMN ipod_id INTEGER;

-- The library file's duration at the last sync, to tell a changed recording
-- from a retag. The iPod's own durations are sometimes wrong. Mappings saved
-- before this column were made from the current files.
ALTER TABLE device_tracks ADD COLUMN synced_duration_ms INTEGER;
UPDATE device_tracks SET synced_duration_ms =
  (SELECT duration_ms FROM tracks WHERE tracks.id = device_tracks.track_id);
