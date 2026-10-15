-- The version of the tag reader that read each row's tags. A scan reads
-- the tags of rows read by an older reader again (not the audio, whose hash
-- is kept), so a change in how tags are read reaches files already indexed.
ALTER TABLE tracks ADD COLUMN tags_version INTEGER NOT NULL DEFAULT 0;
