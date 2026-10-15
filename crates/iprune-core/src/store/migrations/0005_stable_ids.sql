-- foreign_keys off: the tables are rebuilt, see `Store::init`.
--
-- Track and playlist ids are never handed out again once deleted
-- (AUTOINCREMENT): a backup's mappings name rows by id, and a restore must
-- not attach an old iPod entry to a new song or playlist that took a freed
-- id. SQLite cannot add AUTOINCREMENT to a table, so both are rebuilt with
-- the same ids, and the next id starts above the highest one used.
CREATE TABLE tracks_new (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  path          TEXT NOT NULL UNIQUE,
  path_key      TEXT NOT NULL UNIQUE,
  size          INTEGER NOT NULL,
  mtime_ns      INTEGER NOT NULL,
  hash          BLOB NOT NULL,
  duration_ms   INTEGER,
  filetype      TEXT NOT NULL,
  bitrate       INTEGER, samplerate INTEGER,
  title TEXT, artist TEXT, album TEXT, album_artist TEXT, composer TEXT,
  genre TEXT, year INTEGER,
  track_no INTEGER, track_total INTEGER, disc_no INTEGER, disc_total INTEGER,
  compilation   INTEGER NOT NULL DEFAULT 0,
  tags_hash     BLOB NOT NULL,
  added_at      INTEGER NOT NULL,
  missing_since INTEGER,
  tag_error     TEXT,
  drm           INTEGER NOT NULL DEFAULT 0
);
INSERT INTO tracks_new (id, path, path_key, size, mtime_ns, hash, duration_ms, filetype,
  bitrate, samplerate, title, artist, album, album_artist, composer, genre, year,
  track_no, track_total, disc_no, disc_total, compilation, tags_hash, added_at,
  missing_since, tag_error, drm)
SELECT id, path, path_key, size, mtime_ns, hash, duration_ms, filetype,
  bitrate, samplerate, title, artist, album, album_artist, composer, genre, year,
  track_no, track_total, disc_no, disc_total, compilation, tags_hash, added_at,
  missing_since, tag_error, drm FROM tracks;
DROP TABLE tracks;
ALTER TABLE tracks_new RENAME TO tracks;
CREATE INDEX tracks_hash ON tracks(hash);

CREATE TABLE playlists_new (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  name          TEXT NOT NULL UNIQUE,
  file          TEXT NOT NULL UNIQUE,
  file_mtime_ns INTEGER NOT NULL,
  removed_at    INTEGER
);
INSERT INTO playlists_new (id, name, file, file_mtime_ns, removed_at)
SELECT id, name, file, file_mtime_ns, removed_at FROM playlists;
DROP TABLE playlists;
ALTER TABLE playlists_new RENAME TO playlists;

-- Lookups by track, for doctor and the window.
CREATE INDEX playlist_items_track ON playlist_items(track_id);

-- The stamp of a restore that started putting files back on this iPod and
-- has not recorded its mappings yet. A sync refuses to run while it is set.
ALTER TABLE devices ADD COLUMN restoring TEXT;
