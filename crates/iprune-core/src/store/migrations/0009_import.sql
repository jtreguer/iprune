-- Songs waiting in the inbox to be imported (technical document, section
-- 7.2): one row per audio file found there, dropped when the file leaves.
-- `lookup` is autotag's answer as it came; `search`, `choice`, `tags` and
-- `confirmed` are what the user decided; a changed file loses them all.
CREATE TABLE import_items (
  id        INTEGER PRIMARY KEY AUTOINCREMENT,
  path      TEXT NOT NULL UNIQUE,
  size      INTEGER NOT NULL,
  mtime_ns  INTEGER NOT NULL,
  hash      BLOB NOT NULL,
  lookup    TEXT,
  search    TEXT,
  choice    INTEGER,
  tags      TEXT,
  confirmed INTEGER NOT NULL DEFAULT 0,
  error     TEXT,
  added_at  INTEGER NOT NULL
);
