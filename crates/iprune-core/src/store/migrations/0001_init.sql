CREATE TABLE tracks (
  id            INTEGER PRIMARY KEY,
  path          TEXT NOT NULL UNIQUE,        -- exact on-disk path, absolute
  path_key      TEXT NOT NULL UNIQUE,        -- NFC form of path, for lookups
  size          INTEGER NOT NULL,
  mtime_ns      INTEGER NOT NULL,
  hash          BLOB NOT NULL,               -- blake3 of the whole file
  duration_ms   INTEGER,
  filetype      TEXT NOT NULL,               -- mp3, aac, alac, wav, aiff
  bitrate       INTEGER, samplerate INTEGER,
  title TEXT, artist TEXT, album TEXT, album_artist TEXT, composer TEXT,
  genre TEXT, year INTEGER,
  track_no INTEGER, track_total INTEGER, disc_no INTEGER, disc_total INTEGER,
  compilation   INTEGER NOT NULL DEFAULT 0,
  tags_hash     BLOB NOT NULL,               -- hash of the fields synced to the iPod
  added_at      INTEGER NOT NULL,
  missing_since INTEGER                      -- set when the file disappears
);
CREATE INDEX tracks_hash ON tracks(hash);

CREATE TABLE playlists (
  id         INTEGER PRIMARY KEY,
  name       TEXT NOT NULL UNIQUE,
  file       TEXT NOT NULL UNIQUE,
  file_mtime_ns INTEGER NOT NULL
);

CREATE TABLE playlist_items (
  playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
  position    INTEGER NOT NULL,
  track_id    INTEGER REFERENCES tracks(id),   -- NULL when the entry is unresolved
  raw_entry   TEXT NOT NULL,                   -- the line from the file
  PRIMARY KEY (playlist_id, position)
);

CREATE TABLE devices (
  id            INTEGER PRIMARY KEY,
  firewire_guid TEXT NOT NULL UNIQUE,
  name TEXT, model TEXT,
  last_sync_at  INTEGER
);

CREATE TABLE device_tracks (
  device_id   INTEGER NOT NULL REFERENCES devices(id),
  track_id    INTEGER NOT NULL REFERENCES tracks(id),
  dbid        INTEGER NOT NULL,               -- u64 stored as i64
  synced_tags_hash BLOB NOT NULL,
  synced_size INTEGER NOT NULL,
  PRIMARY KEY (device_id, track_id),
  UNIQUE (device_id, dbid)
);

CREATE TABLE device_playlists (
  device_id   INTEGER NOT NULL REFERENCES devices(id),
  playlist_id INTEGER NOT NULL REFERENCES playlists(id),
  ipod_name   TEXT NOT NULL,
  PRIMARY KEY (device_id, playlist_id)
);

CREATE TABLE stats (
  track_id    INTEGER PRIMARY KEY REFERENCES tracks(id),
  plays       INTEGER NOT NULL DEFAULT 0,
  skips       INTEGER NOT NULL DEFAULT 0,
  rating      INTEGER,                         -- 0..100
  last_played INTEGER
);
