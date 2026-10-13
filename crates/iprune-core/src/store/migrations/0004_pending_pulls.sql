-- A statistics pull not yet settled by a successful write. libgpod renames
-- `Play Counts` to `Play Counts.bak` when it writes; if the next sync still
-- finds the same `Play Counts` file, the write never happened and the pull
-- is undone so its plays are not counted twice.
CREATE TABLE pending_pulls (
  device_id        INTEGER PRIMARY KEY REFERENCES devices(id),
  play_counts_hash BLOB,                 -- blake3 of `Play Counts`, NULL when absent
  changes          TEXT NOT NULL,        -- JSON: what the pull changed, to undo it
  at               INTEGER NOT NULL
);
