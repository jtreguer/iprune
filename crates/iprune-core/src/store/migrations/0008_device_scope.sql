-- What each iPod syncs, set from the window or `iprune scope`: 'all',
-- 'playlists' (only those named) or 'except' (all but those named). NULL
-- follows [sync] in the config.
ALTER TABLE devices ADD COLUMN scope TEXT;

CREATE TABLE device_scope_playlists (
  device_id INTEGER NOT NULL REFERENCES devices(id),
  name      TEXT NOT NULL,
  PRIMARY KEY (device_id, name)
);
