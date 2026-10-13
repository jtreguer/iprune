-- Why a file's tags may be missing, and whether it is FairPlay protected.
-- Both are kept so the scan can keep reporting them, and retry the tag read.
ALTER TABLE tracks ADD COLUMN tag_error TEXT;
ALTER TABLE tracks ADD COLUMN drm INTEGER NOT NULL DEFAULT 0;
