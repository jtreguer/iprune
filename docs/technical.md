# iprune: technical document

Status: draft, 2026-10-13. Companion to [design.md](design.md).

## 1. Stack

| Concern | Choice | Notes |
|---|---|---|
| Language | Rust, stable, edition 2024 | |
| iPod database | libgpod 0.8.3 (Arch `extra`) through FFI | `pkg-config libgpod-1.0` |
| GLib types | `glib-sys` | GList, GError, g_free, g_strdup |
| Bindings | `bindgen` at build time | struct layouts for `Itdb_Track`, `Itdb_Playlist` |
| Tags | `lofty` | mp3, m4a, wav, aiff |
| Local database | `rusqlite` with the `bundled` feature | |
| File walking | `walkdir`, `rayon` for parallel tag reading | |
| Hashing | `blake3` | |
| CLI | `clap` (derive) | |
| Desktop UI | GTK4 through `gtk4-rs` | plain GTK4, not libadwaita, so the GTK theme set by Omarchy applies |
| Config | `serde`, `toml` | |
| Paths | `directories` | XDG locations |
| Errors and logs | `anyhow` in the binary, `thiserror` in libraries, `tracing` | |
| Device mount and eject | `udisksctl` subprocess | `zbus` later if needed |

## 2. Repository layout

```
iprune/
  Cargo.toml                workspace
  crates/
    gpod-sys/               raw bindgen output, links libgpod
      build.rs
      wrapper.h
      src/lib.rs
    gpod/                   safe wrapper, the only crate with unsafe code
      src/{lib,db,track,playlist,device,error}.rs
    iprune-core/            library, playlists, planner, executor; no UI
      src/
        config.rs
        store/              SQLite schema and queries
        scan.rs             walk roots, read tags, detect moves
        playlist.rs         .m3u8 read and write
        device.rs           find mounted iPods, read identity
        adopt.rs            match iPod tracks to library tracks
        plan.rs             pure sync planner
        exec.rs             applies a plan through gpod
        verify.rs           snapshot and compare
        stats.rs            play count pull and export
    iprune/                 binary `iprune`: the CLI
      src/main.rs
    iprune-gui/             binary `iprune-gui`: the GTK4 window
      src/{main,view_model,...}.rs
  docs/
  tests/
    fixtures/
```

`plan.rs` has no I/O and no libgpod dependency. It takes plain structs and
returns a `Plan`. This keeps the logic that decides what happens to the iPod
testable without a device or even libgpod.

## 3. libgpod bindings

### 3.1 gpod-sys

`build.rs` runs `pkg_config::probe_library("libgpod-1.0")` and feeds the
include paths to bindgen. An allowlist keeps the output small:

```rust
bindgen::Builder::default()
    .header("wrapper.h")                       // #include <gpod/itdb.h>
    .clang_args(include_paths.iter().map(|p| format!("-I{}", p.display())))
    .allowlist_function("itdb_.*")
    .allowlist_type("Itdb_.*")
    .allowlist_var("ITDB_.*")
    .blocklist_type("_?G.*")                    // taken from glib-sys instead
    .raw_line("use glib_sys::*;")
    .generate()
```

### 3.2 Functions used

| Purpose | Function |
|---|---|
| Open | `itdb_parse(mountpoint, &err)`, `itdb_parse_file(path, &err)` for tests and dry runs |
| Close | `itdb_free` |
| Write | `itdb_write(db, &err)`, `itdb_write_file(db, path, &err)` for dry runs |
| Sync markers | `itdb_start_sync`, `itdb_stop_sync` (shows "Sync in progress" on the iPod) |
| Device info | `itdb_device_get_sysinfo(dev, "FirewireGuid")`, `itdb_device_get_ipod_info` |
| Tracks | `itdb_track_new`, `itdb_track_add`, `itdb_track_remove`, `itdb_track_by_id` |
| Files | `itdb_cp_track_to_ipod(track, path, &err)`, `itdb_filename_on_ipod`, `itdb_get_mountpoint` |
| Playlists | `itdb_playlist_mpl`, `itdb_playlist_new`, `itdb_playlist_add`, `itdb_playlist_remove`, `itdb_playlist_add_track`, `itdb_playlist_remove_track`, `itdb_playlist_contains_track`; rename by replacing `name` with `g_free` and `g_strdup` |
| Tests | `itdb_init_ipod(mountpoint, model, name, &err)` |

Struct fields read or written directly on `Itdb_Track`: `title`, `artist`,
`album`, `albumartist`, `composer`, `genre`, `filetype`, `track_nr`,
`tracks`, `cd_nr`, `cds`, `year`, `tracklen` (ms), `bitrate`, `samplerate`,
`size`, `compilation`, `mediatype`, `time_added`, `time_played`, `rating`
(0 to 100, 20 per star), `playcount`, `recent_playcount`, `skipcount`,
`recent_skipcount`, `dbid`, `ipod_path`, `sort_*`.

On `Itdb_Playlist`: `name`, `members`, `num`, `is_spl`, `type`.

### 3.3 Safe wrapper (crate `gpod`)

```rust
pub struct Database { raw: NonNull<Itdb_iTunesDB> }   // !Send, !Sync
pub struct TrackRef<'db> { raw: NonNull<Itdb_Track>, _db: PhantomData<&'db Database> }
pub struct TrackMut<'db> { ... }                       // obtained from &mut Database
pub struct PlaylistRef<'db> { ... }
pub struct NewTrack { raw: NonNull<Itdb_Track> }       // owned until added

impl Database {
    pub fn open(mountpoint: &Path) -> Result<Self>;
    pub fn open_file(itunesdb: &Path) -> Result<Self>;
    pub fn tracks(&self) -> impl Iterator<Item = TrackRef<'_>>;
    pub fn playlists(&self) -> impl Iterator<Item = PlaylistRef<'_>>;
    pub fn track_by_dbid(&self, dbid: u64) -> Option<TrackRef<'_>>;
    pub fn add_track(&mut self, t: NewTrack) -> TrackMut<'_>;  // also adds to master playlist
    pub fn remove_track(&mut self, dbid: u64) -> Result<PathBuf>; // returns the iPod file to delete
    pub fn set_playlist_members(&mut self, name: &str, dbids: &[u64]) -> Result<()>;
    pub fn write(&mut self) -> Result<()>;
    pub fn write_to(&mut self, path: &Path) -> Result<()>;
    pub fn firewire_guid(&self) -> Option<String>;
}
impl Drop for Database { fn drop(&mut self) { unsafe { itdb_free(self.raw.as_ptr()) } } }
```

Rules the wrapper enforces:

- **Ownership.** A `NewTrack` owns its `Itdb_Track` and frees it with
  `itdb_track_free` if dropped before `add_track`. After `itdb_track_add`
  the database owns it. `remove_track` first removes the track from every
  playlist, then calls `itdb_track_remove`, which frees it.
- **Strings.** libgpod expects UTF-8 `gchar*` allocated by GLib. Setters
  `g_free` the old value and `g_strdup` the new one. Getters copy into
  `String`, replacing invalid UTF-8 rather than panicking.
- **Threads.** `Database` is neither `Send` nor `Sync`. All libgpod calls happen
  on one thread. Tag reading and hashing run in parallel before that.
- **Errors.** Every `GError` is converted to `gpod::Error` and freed.
- **dbid.** Read the `dbid` after `itdb_track_add` and assert it is non-zero.
  If it is zero, read it again after `itdb_write`. Either way the executor
  records the mapping only once the write succeeds.
- **Playlist membership.** `set_playlist_members` removes all members and adds
  the new list in order. There is no "append" operation in the public API.
  This is the direct answer to the Rhythmbox doubling bug.

### 3.4 Validating the bindings

The first milestone ports `snapshot.c` from
`/mnt/seagate-1tb/iPod-backup-2026-10/tools/` to Rust and runs both on the
backup `iTunesDB`. Their output must be byte-identical. That proves the struct
layouts, string handling and list walking before anything is written.

## 4. Configuration

`~/.config/iprune/config.toml`:

```toml
[library]
roots = ["/mnt/seagate-1tb/Music"]
exclude_extensions = ["m4v", "mp4", "mov"]

[playlists]
dir = "/mnt/seagate-1tb/Playlists"
path_style = "absolute"          # or "relative" to the playlists dir

[sync]
scope = "all"                    # or "playlists"
playlists = []                   # used when scope = "playlists"
remove_unmanaged = false
keep_backups = 20

[device]
mountpoint = ""                  # empty means detect
```

## 5. State and files

| Path | Content |
|---|---|
| `~/.local/share/iprune/iprune.sqlite` | library index, mappings, statistics |
| `~/.local/share/iprune/stats.csv` | play statistics, rewritten after each sync |
| `~/.local/share/iprune/backups/<guid>/<timestamp>/` | `iTunesDB`, `ArtworkDB`, `Play Counts` if present, `plan.json`, `snapshot.txt` |
| `<ipod>/iPod_Control/iprune/manifest.json` | copy of the device mapping, written after each sync |
| `/mnt/seagate-1tb/Playlists/*.m3u8` | playlists, the source of truth |

The manifest on the iPod lets iprune rebuild the mapping if the local database
is lost, without running adoption again.

## 6. Database schema

```sql
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
```

Migrations are numbered SQL files applied in order, tracked with
`PRAGMA user_version`.

Migration 0003 adds `playlists.removed_at`. When a playlist file is deleted
while an iPod playlist is mapped to it, the row stays as a tombstone (items
dropped, `removed_at` set) so the next sync knows which iPod playlist to
delete; the executor drops the tombstone after that sync. A file that comes
back under the same name revives the row and its mapping. Renaming a
playlist onto a tombstone's name moves the tombstone aside (name and file
`.removed-<id>`, which no playlist file can have), so the next sync still
deletes its iPod playlist. Re-adoption drops tombstones no device maps any
more. The same migration adds `device_playlists.ipod_id`, the iPod
playlist's persistent id, and `device_tracks.synced_duration_ms`, the
library file's duration at the last sync, filled from the index for
existing mappings.

## 7. Library scan

1. Walk every root, skipping excluded extensions and hidden folders. A root
   that is not a readable folder stops the scan, and so does a walk that finds
   no audio at all while the index has tracks: an unmounted disk must never
   look like an empty library. Nothing under a folder that failed to read is
   marked missing.
2. For each file compare `(size, mtime_ns)` with the stored row. Unchanged
   files are skipped, so a rescan of 9,675 files takes about a second.
3. New or changed files are read in parallel with `lofty` (tags, duration,
   codec, embedded picture presence). New files are also hashed with BLAKE3.
   The first full hash of 77 GB on a USB hard disk takes 10 to 15 minutes and
   runs once; progress is shown.
4. Move detection: a new path is the same track as a path that disappeared
   in this scan, or one already marked missing, when exactly one such track
   and exactly one new file share the hash. Anything ambiguous is an
   addition plus a missing track. The row keeps its id, its path is
   updated, and every `.m3u8` that referenced the old path is rewritten.
5. Paths that disappeared without a match get `missing_since` set. They are
   deleted from the index after the next successful sync has removed them from
   the iPod, so the mapping is never orphaned.
6. Filetype for m4a comes from the codec (AAC or ALAC), not the extension.
7. Paths are stored exactly as on disk, so files can always be opened; the
   library has at least one file whose name is in NFD. Lookups from playlist
   entries and the command line go through `path_key`, the NFC form.
8. Files that cannot be read are reported and left out of the index; if they
   were indexed before, their row is kept as it was.

## 8. Playlists

- Read: UTF-8, `#EXTM3U` header optional, `#EXTINF` lines ignored on read,
  other `#` lines ignored, blank lines skipped. Relative entries resolve
  against the playlist's folder. Paths are NFC normalized before lookup.
- Write: always `#EXTM3U`, then `#EXTINF:<seconds>,<artist> - <title>` and
  the path for each entry, LF line endings. Written to a temporary file in the
  same folder, `fsync`ed, then renamed over the original.
- External edits are detected by `mtime`. Every edit records the file's mtime
  when it reads it and refuses to replace the file if it changed meanwhile.
  The window watches the playlists folder and reloads a file changed by
  another program; an edit made against the old contents is refused and
  the window shows a message instead of overwriting.
- Creating and renaming never replace an existing file (no-clobber persist,
  hard link then unlink).
- Unresolved entries are kept verbatim in the file so iprune never deletes a
  line it does not understand.

## 9. Device detection and identity

- Read `/proc/self/mountinfo` and keep mountpoints containing
  `iPod_Control/iTunes/iTunesDB`. With more than one, ask.
- Identity is `itdb_device_get_sysinfo(device, "FirewireGuid")`. If missing,
  stop with a message telling the user to run `ipod-read-sysinfo-extended`,
  since libgpod cannot write a valid Classic database without it anyway.
- Before parsing, check the filesystem type. The supported iPod is FAT32
  (`vfat`); anything else is refused for writes, as is a read-only mount.

## 10. Adoption algorithm

Input: every iPod track (dbid, size, tracklen, title, artist, album) and every
library track.

1. Index library tracks by `size`.
2. For each iPod track, take candidates with equal size. Keep those with
   `|duration difference| <= 1000 ms`.
3. Score the remaining candidates on normalized title and artist (lowercase,
   NFKD without accents, punctuation removed, leading "the " dropped). Exact
   match scores 2, one of the two matches scores 1. A library file with no
   title tag is compared by its file name, which is what iTunes used as the
   title. An artist missing on both sides counts as a match when the titles
   match; a title missing on both sides is no evidence.
4. One candidate with the top score of 2: accepted. A single top candidate
   with a lower score, or several tied: proposed. The top candidate is the
   default when its score is above 0; for ties (duplicate library files) the
   next free tied copy is taken. A score of 0 never gets a default: equal
   size and duration alone are not enough. No candidate: unmatched.
5. A library track can be claimed by only one iPod track. If two iPod tracks
   claim it, both become proposals. This catches the duplicates already on the
   iPod.
6. Second pass for unmatched iPod tracks: same normalized artist, title and
   album, duration within two seconds, any size. These are proposals, covering
   files whose tags were edited after they were copied. A default needs a
   single candidate and either a non-empty album or durations within 10 ms
   (a live take or a remix is never that close).
7. Proposals are resolved strongest first (duplicates on the iPod, duplicate
   library files, weak tags, tags only; then by score), so a weak default
   never takes a file from a better match.
8. Content check, only when reading a mounted iPod. Every iPod track not
   accepted by steps 1 to 7 whose size equals some library file's size is
   read from the iPod and hashed (blake3, as in the index), one file at a
   time. Matching then runs again with an extra first step: an iPod file
   byte-identical to exactly one library file is accepted whatever its
   duration and tags say; identical to several (duplicates in the library),
   it is a duplicate-file proposal with the first copy as default. When an
   identical file and a tag match claim the same library file (step 5), the
   identical one gets the default. Files are opened with `O_NOATIME`, so
   reading them does not touch their access dates. A file
   that cannot be read is listed in the report and matched on tags as
   before. Durations are unreliable for some MP3s on both sides (the iPod
   and lofty were each off by a factor of two to six on real files), so
   content is the deciding evidence when it is available.

Output is written to `adopt-report.txt` in the data folder, summarised on
the terminal, and shown in the window's adoption review. From the command
line, nothing is saved while proposals are pending: the user reads the report and runs again
with `--accept-proposals`, which takes every default. Proposals left without
a usable default keep their iPod track unmanaged. A second adoption of the
same iPod needs `--force`. Accepted
and confirmed matches go into `device_tracks` with `synced_tags_hash` computed
from the iPod's current values, so the first sync updates tags only where they
really differ.

Playlist adoption: iPod playlists are matched to `.m3u8` files by exact name,
then by NFC-normalized, case-insensitive name, then with the characters a
file name cannot hold (`/ \ : * ? " < > |`) replaced by `_`, as exporters
did: "Dance/Trance" is the file `Dance_Trance.m3u8`. Matches go into
`device_playlists`.

This step only reads the iPod. It can also run against the backup with
`iprune adopt --file /mnt/seagate-1tb/iPod-backup-2026-10/iTunes/iTunesDB`,
which is how the algorithm gets tuned before the device is involved; with
`--file` nothing is saved.

Results on the backup (2026-10-13): 9,534 of 9,669 iPod tracks matched,
87 weak proposals (mostly WAV files whose artist exists only in the iTunes
database), 11 duplicate-file proposals, 6 tag-only proposals, 31 iPod tracks
with no file, 35 library files not on the iPod. With `--accept-proposals`,
9,630 tracks are mapped and 8 proposals stay open (score 0, or tags only
without an album). The backup's `iTunesDB`
predates the 2026-10-12 repair: 322 of its playlists are exactly double their
`.m3u8` file and 4 follow the repair tool's other pattern, so on the backup
every playlist reports a difference.

Results on the device (2026-10-13, iPod Classic 160 GB, GUID read from
`SysInfoExtended`): 9,672 tracks; before the content check, 9,537 matched,
104 proposals and 31 iPod tracks with no library file. All 31 have a
byte-identical library file whose duration disagrees with the iPod's.
With the content check (130 iPod files read, 79 s): 9,657 matched, 121 of
them by identical file; 15 proposals (8 among identical duplicate WAVs, 2
duplicates on the iPod, 5 tags-only MP3s about 8 KB smaller in the library,
same duration, no album); no iPod track without a library file; 2 library
files not on the iPod; 13 playlists differ from their file.

## 11. Sync

### 11.1 Planner

`plan.rs`, pure. `sync.rs` gathers its input from the index and the iPod
(`ipod::read`, the device's mappings, `statvfs` on the mountpoint) and
describes the result.

```rust
pub struct PlanInput<'a> {
    pub library: &'a [TrackRow],             // every indexed track, missing ones too
    pub playlists: &'a [LibPlaylist],        // playlists with a file, resolved to track ids
    pub removed_playlists: &'a [(i64, String)], // tombstones still mapped to the iPod
    pub device: &'a Contents,                // tracks and regular playlists read from the iPod
    pub tracks_map: &'a [TrackMapping],      // track_id <-> dbid, synced tag hash and size
    pub playlists_map: &'a [(i64, String)],  // playlist_id <-> iPod playlist name
    pub scope: Scope<'a>,                    // All | Playlists(names)
    pub remove_unmanaged: bool,
    pub space: Option<Space>,                // capacity and free bytes
}

pub struct Plan {
    pub copy: Vec<CopyOp>,                   // track_id, path, size
    pub replace: Vec<ReplaceOp>,             // audio changed: copy new, remove old_dbid
    pub update_tags: Vec<TagOp>,             // dbid, field changes
    pub remove: Vec<RemoveOp>,               // dbid, reason
    pub playlists: Vec<PlaylistOp>,          // Delete | Rename | Create | SetMembers
    pub unmanaged: Unmanaged,                // kept, reported
    pub skipped: Vec<Skipped>,               // FairPlay files not on the iPod yet
    pub entries_skipped: Vec<EntrySkipped>,  // unresolved, missing or skipped entries
    pub stale_mappings: Vec<StaleMapping>,   // mapped dbid no longer on the iPod
    pub stats_to_pull: usize,
    pub space: SpaceCheck,
    pub expected: Expected,                  // track count and managed playlists
}

pub fn plan(input: &PlanInput) -> Plan;
```

Rules, in the order the planner applies them:

1. Scope: every present track, or the present members of the configured
   playlists.
2. A track in scope with no mapping, or whose mapped dbid is gone from the
   iPod (a stale mapping), is copied. An unmapped FairPlay file is skipped:
   the iPod may not be authorised to play it. Section 18 has the open
   question.
3. A mapped track whose size differs from the synced size and whose duration
   differs by more than one second from the file's duration at the last sync
   (`synced_duration_ms`) is replaced. The iPod's own durations are not used,
   since some are wrong by a factor of two to six. Otherwise,
   when its tag hash differs from the synced one, the fields where the
   library has a value the iPod lacks or differs from are updated. Text is
   compared and sent trimmed and in NFC. An empty library field, spaces only
   included, never clears an iPod value; a missing compilation mark never clears the iPod's.
4. A mapped iPod track whose library file is gone, or that left the scope, is
   removed. Unmapped iPod tracks are kept and reported, or removed when
   `remove_unmanaged` is set.
5. Playlists. A mapped playlist claims the iPod playlist with its stored
   id (`device_playlists.ipod_id`), or by stored name for a mapping saved
   without an id. A recorded id no longer on the iPod claims nothing; the
   playlist then goes through the name passes below like an unmapped one. A tombstone's iPod playlist is deleted, and so is a mapped playlist
   that left the scope. Unmapped playlists in scope then match free iPod
   playlists by the three name passes of adoption (exact, NFC
   case-insensitive, file-name characters). Unmatched ones are created.
   A playlist still unmatched whose name matches an iPod smart playlist by
   one of those passes is an export of it: it goes to `playlists_skipped`
   and is not created. Remaining regular iPod playlists are unmanaged: kept,
   or deleted with
   `remove_unmanaged`. The master, Podcasts and smart playlists are never
   part of the plan.
6. A matched playlist keeps its iPod name when that name differs from the
   file's only by file-name characters or Unicode form (`BO - 2001: a Space
   Odyssey` for `BO - 2001_ a Space Odyssey.m3u8`); otherwise, a change of
   case included, it is renamed to the file's name. Its members are set as a whole when the iPod's list of dbids differs
   from the file's, or when a member is copied or replaced. Entries that
   resolve to nothing, to a missing file or to a skipped track are left out
   and listed.
7. Playlist operations run deletes first, then renames, creates and member
   updates, each group by name, so no operation meets a name about to be
   freed.

The planner is deterministic: same input, same plan, same ordering. `Plan`
serializes to JSON for the backup folder and for `iprune sync --plan-only`.

Free space check: removals run before copies, but a replaced file is
removed only after its new copy is written. The peak, bytes copied minus
bytes removed, must fit in the iPod's free space with a margin of 1% of its
capacity; otherwise `fits` is false and the plan must not be applied.

`prepare` refuses a `playlists` scope that is empty or names a playlist that
does not exist, since it would plan the removal of every managed track. For
M4: the executor also asks for confirmation, and a plan that removes more
than a tenth of the managed tracks needs an explicit flag.

`iprune sync --plan-only` scans the library, reads the iPod, prints the
summary of section 7.3 of the design (every change with `--verbose`, the
plan as JSON with `--json`) and saves the plan to `plan.json` in the data
folder. It refuses an iPod that has not been adopted, since every track
would otherwise be copied again.

### 11.2 Executor

Order of operations:

1. Pull statistics: for each mapped track add `recent_playcount` and
   `recent_skipcount` to `stats`, copy `rating` and `time_played`. Commit to
   SQLite. This happens first because libgpod resets the recent counts on the
   next write.
2. Back up `iTunesDB`, `ArtworkDB` and `Play Counts` to the backup folder,
   with the plan and a snapshot.
3. `itdb_start_sync`.
4. Remove tracks: drop from all playlists, `itdb_track_remove`, delete the
   file.
5. Copy and replace tracks: build a `NewTrack` from library tags,
   `add_track`, `itdb_cp_track_to_ipod`. Show progress in bytes. Copy runs sequentially; the USB link
   is the limit, so parallel copies would not help.
6. Update tags on existing tracks. Fields empty in the library leave the iPod
   value alone.
7. Apply playlist operations with `set_playlist_members`.
8. Run the pre-write checks (section 11.3). On failure, stop without writing.
9. `itdb_write`.
10. `itdb_stop_sync`.
11. Verify (section 11.4).
12. Commit mappings to SQLite, write the manifest to the iPod, write
    `stats.csv`.

Checkpoints: during a long copy the database is written every 500 tracks or 10
minutes, after running the pre-write checks. An interrupted sync then loses at
most one checkpoint of copied files, and those files are found by
`iprune doctor`.

Ctrl+C during the copy finishes the current file, writes a checkpoint and
exits.

### 11.3 Pre-write checks

All on the in-memory database:

- Track count equals `expected.track_count`.
- Every regular playlist managed by iprune has exactly the expected dbids in
  the expected order.
- No managed playlist contains a repeated dbid unless the source file does.
- Every playlist member is in the master playlist.
- Unmanaged tracks and playlists are unchanged.
- Every track has an `ipod_path` and the file exists.

### 11.4 Verification

Close the database, parse it again from the iPod, produce a snapshot in the
same text format as the C `snapshot` tool, and compare it with the expected
state. Volatile parts (artwork `mhii` ids, which change on every artwork write)
are excluded. Any difference is printed and the command exits non-zero; the
backup stays in place for `iprune restore`.

### 11.5 Restore

`iprune restore <timestamp>` copies the backed up `iTunesDB` and `ArtworkDB`
onto the iPod, then rolls the SQLite mappings back to the matching
`plan.json` state. Files added after the backup become orphans until
`doctor --fix` deletes them.

## 12. Doctor

`iprune doctor [--fix]` reports, and with `--fix` repairs:

- files under `iPod_Control/Music` with no database entry (delete),
- database entries whose file is missing (remove entry),
- regular playlists with repeated tracks not present in their source file
  (rebuild membership),
- mappings pointing to dbids not on the iPod (drop mapping),
- library tracks marked missing for more than 30 days and not on any device
  (drop from index).

## 13. CLI

```
iprune init
iprune scan
iprune adopt [--file <iTunesDB>] [--accept-proposals]
iprune sync [--plan-only] [--verbose] [--json] [--yes]
iprune playlist list|show|new|rm|rename|add|remove|move ...
iprune device info
iprune backups
iprune restore <timestamp>
iprune doctor [--fix]
iprune snapshot <mountpoint|iTunesDB>
iprune eject
iprune                    (no arguments: prints help; the window is iprune-gui)
```

Exit codes: 0 success, 1 error, 2 usage, 3 verification failed, 4 user
declined the plan.

## 14. Desktop UI

Crate `iprune-gui`, binary `iprune-gui`, GTK4 (4.12 or later) through
`gtk4-rs`, without libadwaita. Omarchy switches GTK applications between
light and dark with the `gtk-theme` (Adwaita or Adwaita-dark) and
`color-scheme` settings, which plain GTK4 follows. The window needs none of
libadwaita's widgets.

- **Grid.** `gtk::ColumnView` over a `gio::ListStore` of a `TrackObject`
  GObject subclass, wrapped in a `gtk::FilterListModel` (search) and a
  `gtk::SortListModel` fed by the column sorters. List widgets are recycled,
  so widgets exist only for the visible rows; the ~9,700 track objects are
  plain data that sorting and filtering go through. Column visibility, width
  and order are saved per view in `~/.local/state/iprune/ui.toml`.
- **Sidebar.** `gtk::ListView` with sections for Library, Playlists and
  Device (section headers need GTK 4.12). Playlists are drop targets for
  tracks.
- **Drag and drop.** Rows carry library track ids (`gdk::ContentProvider`
  with an application-specific type). A drop on a sidebar playlist calls
  `playlists::add`; a drop inside a playlist grid calls `add` with the drop
  position, or `move_entry` when the rows come from the same playlist.
  `ColumnView` has no drop-at-row support of its own: each row widget gets a
  `gtk::DropTarget`, and the drop position is computed from the row and the
  pointer's half of it.
- **Threads.** The GTK main thread owns the widgets and its own SQLite
  connection for playlist edits, which are small and quick. Scan, plan and
  sync run on one worker thread that owns the libgpod `Database`; it sends
  progress and results back through an async channel read by
  `glib::MainContext::spawn_local`.
- **Same code as the CLI.** Every edit and sync goes through `iprune-core`
  (`playlists`, `scan`, `adoption`, `plan`, `exec`); the window holds no
  logic of its own beyond presentation.
- **Logic without a display.** Sorting keys, filtering, drop index
  arithmetic and the plan's presentation live in `view_model.rs` as plain
  functions with unit tests. Widget code stays thin.
- **Playlists folder.** A `gio::FileMonitor` reloads files changed by other
  programs.

## 15. Omarchy packaging

- `cargo install --path crates/iprune` and `--path crates/iprune-gui`, or a
  PKGBUILD later.
- Desktop entry `~/.local/share/applications/iprune.desktop` that starts
  `iprune-gui`.
- Notifications through `notify-send` at the end of a sync.
- Optional binding, for example in `~/.config/hypr/bindings.conf`:
  `bindd = SUPER SHIFT, I, iPod, exec, iprune-gui`.

## 16. Testing

| Level | What | How |
|---|---|---|
| Unit | planner, playlist parsing, adoption scoring, tag diffing | pure functions, table-driven tests |
| Bindings | struct layouts, strings, lists | Rust snapshot vs C `snapshot` on the backup `iTunesDB` |
| Integration | full sync on a fake iPod | `itdb_init_ipod` into a temp dir with a `SysInfo` containing a `FirewireGuid`, small audio fixtures generated with ffmpeg |
| Regression | playlist doubling | sync the same plan twice; the second plan must be empty and the snapshot unchanged |
| Real data, read only | adoption on 9,669 tracks | `adopt --file` on the backup, compare match counts between versions |
| Device | first write | playlists-only sync on the real iPod after a manual backup, then `snapshot` diff |

Property test with `proptest`: random library and playlist edits, plan,
apply to a fake iPod, plan again; the second plan is always empty.

## 17. Milestones

| # | Deliverable | Writes to iPod |
|---|---|---|
| M0 | `gpod-sys`, `gpod`, `iprune snapshot` identical to the C tool | no |
| M1 | config, scan, SQLite, `.m3u8` read and write, `playlist` commands | no |
| M2 | `adopt --file` on the backup, then on the device | no |
| M3 | planner and `sync --plan-only` | no |
| M4 | executor for playlists only, backups, checks, verify, restore | yes |
| M5 | track copy, remove, tag update, statistics pull, `doctor` | yes |
| M6 | desktop UI (GTK4) | yes |
| M7 | desktop entry, notifications, packaging | yes |

M4 comes before M5 on purpose. Playlist writes are the operation that failed
with Rhythmbox, the tracks are already on the iPod, and a playlist-only sync
can be checked against the backup with the existing C tools.

## 18. Risks

| Risk | Mitigation |
|---|---|
| libgpod is unmaintained since 2015 | it is stable and still packaged; the wrapper keeps it behind one crate so a pure Rust writer could replace it later |
| Struct layout mismatch with the installed headers | bindgen at build time against the system headers; M0 test against the real database |
| Large database write is slow (22 MB `iTunesDB`) | measured in M0; checkpoints sized accordingly |
| libgpod does not understand some iTunes smart playlist rules ("Unknown action type 1" on write) and may rewrite them | check in M4 whether a write changes the 7 smart playlists, by comparing the raw `mhyp` records before and after |
| Artwork database corruption | iprune never touches artwork; `ArtworkDB` backed up with every sync |
| Losing play counts | pulled and committed before any write, exported to CSV |
| FairPlay files (one in the library, "Bailemos") may not play on an iPod that is not authorised for that Apple account | the planner skips unmapped ones and leaves mapped ones in place; M5 decides whether copying them is worth offering |
