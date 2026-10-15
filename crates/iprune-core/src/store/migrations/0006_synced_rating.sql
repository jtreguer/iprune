-- The rating the iPod held for this track at the last statistics pull, to
-- tell a rating changed on the iPod from one that differs because the
-- iPod track is new (a copy, a re-copy), restored or on another iPod.
-- NULL when not known yet.
ALTER TABLE device_tracks ADD COLUMN synced_rating INTEGER;
