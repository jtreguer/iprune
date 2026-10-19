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
        notify.rs           desktop notifications at the end of a sync
    iprune/                 binary `iprune`: the CLI
      src/main.rs
    iprune-gui/             binary `iprune-gui`: the GTK4 window
      src/{main,view_model,...}.rs
  packaging/                iprune.desktop, install.sh
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
roots = ["/mnt/seagate-1tb/Music"]  # absolute, never /
exclude_extensions = ["m4v", "mp4", "mov"]

[playlists]
dir = "/mnt/seagate-1tb/Playlists"
path_style = "absolute"          # or "relative" to the playlists dir

[sync]                           # for an iPod with no selection of its own
scope = "all"                    # or "playlists", or "except"
playlists = []                   # kept ("playlists") or left out ("except")
remove_unmanaged = false
keep_backups = 20

[device]
mountpoint = ""                  # empty means detect; absolute otherwise,
                                 # and neither inside a root nor holding one
# allow_folder = true            # tests only: write to a plain folder
```

## 5. State and files

| Path | Content |
|---|---|
| `~/.local/share/iprune/iprune.sqlite` | library index, mappings, statistics |
| `~/.local/share/iprune/stats.csv` | play statistics, rewritten after each sync; a previous file with more rows or plays is kept as `stats-<stamp>.csv` (the newest five), and each backup holds the one from before its sync |
| `~/.local/share/iprune/backups/<guid>/<timestamp>/` | `iTunesDB`, `ArtworkDB`, `Play Counts` if present, `artwork.json`, `plan.json`, `work.json`, `snapshot.txt`, `mappings.json`, `stats.csv` |
| `~/.local/share/iprune/backups/<guid>/.ithmb/<blake3>` | the iPod's `.ithmb` cover art files, one copy per content, shared by the backups that list them |
| `<ipod>/iPod_Control/iprune/manifest.json` | copy of the device mapping, written after each sync |
| `/mnt/seagate-1tb/Playlists/*.m3u8` | playlists, the source of truth |

The manifest on the iPod is a copy of the mapping for recovery by hand if the
local database is lost; iprune does not read it yet, and a lost index is
rebuilt by adopting again (whose first pull takes the iPod's lifetime
counts).

## 6. Database schema

```sql
CREATE TABLE tracks (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
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
  missing_since INTEGER,                     -- set when the file disappears
  tag_error     TEXT,                        -- why the tags could not be read
  drm           INTEGER NOT NULL DEFAULT 0,  -- FairPlay protected
  tags_version  INTEGER NOT NULL DEFAULT 0   -- tag reader version that read it
);

CREATE TABLE playlists (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  name       TEXT NOT NULL UNIQUE,
  file       TEXT NOT NULL UNIQUE,
  file_mtime_ns INTEGER NOT NULL,
  removed_at INTEGER                         -- a tombstone, see below
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
  last_sync_at  INTEGER,
  restoring     TEXT,                         -- stamp of an unfinished restore
  scope         TEXT                          -- 'all', 'playlists', 'except';
                                              -- NULL follows [sync]
);

CREATE TABLE device_scope_playlists (           -- the playlists a scope names
  device_id INTEGER NOT NULL REFERENCES devices(id),
  name      TEXT NOT NULL,
  PRIMARY KEY (device_id, name)
);

CREATE TABLE device_tracks (
  device_id   INTEGER NOT NULL REFERENCES devices(id),
  track_id    INTEGER NOT NULL REFERENCES tracks(id),
  dbid        INTEGER NOT NULL,               -- u64 stored as i64
  synced_tags_hash BLOB NOT NULL,
  synced_size INTEGER NOT NULL,
  synced_duration_ms INTEGER,
  PRIMARY KEY (device_id, track_id),
  UNIQUE (device_id, dbid)
);

CREATE TABLE device_playlists (
  device_id   INTEGER NOT NULL REFERENCES devices(id),
  playlist_id INTEGER NOT NULL REFERENCES playlists(id),
  ipod_name   TEXT NOT NULL,
  ipod_id     INTEGER,                        -- the iPod playlist's persistent id
  PRIMARY KEY (device_id, playlist_id)
);

CREATE TABLE stats (
  track_id    INTEGER PRIMARY KEY REFERENCES tracks(id),
  plays       INTEGER NOT NULL DEFAULT 0,
  skips       INTEGER NOT NULL DEFAULT 0,
  rating      INTEGER,                         -- 0..100
  last_played INTEGER
);

CREATE TABLE pending_pulls (                   -- see 11.2
  device_id        INTEGER PRIMARY KEY REFERENCES devices(id),
  play_counts_hash BLOB,
  changes          TEXT NOT NULL,
  at               INTEGER NOT NULL
);
```

This is the schema after every migration in `store/migrations`.

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
   marked missing. Symbolic links are not followed: each is reported, and
   nothing under it is marked missing. A folder holding an iPod
   (`iPod_Control/iTunes/iTunesDB`) is skipped and reported, so the iPod's
   own files never become library tracks. A folder below a root that is
   now empty, and is a mount target in `/etc/fstab` or held more than 50
   indexed tracks, counts as unreadable: a disk or share not mounted there
   must not look like deleted music. When the music was deleted on purpose,
   removing the empty folder lets the next scan mark it missing. Rows an
   older scan indexed from an iPod folder are left as they were.
2. For each file compare `(size, mtime_ns)` with the stored row. Unchanged
   files are skipped, so a rescan of 9,675 files takes about a second.
3. New or changed files are read in parallel with `lofty` (tags, duration,
   codec, embedded picture presence). New files are also hashed with BLAKE3.
   Every value of a multi-value text tag (ID3v2.4, Vorbis comments) is read,
   trimmed, and joined with "/". Rows read by an older tag reader
   (`tags_version`, migration 7) have their tags read again, not their
   audio, whose hash is kept; the scan reports them as updated.
   The first full hash of 77 GB on a USB hard disk takes 10 to 15 minutes and
   runs once; progress is shown.
4. Move detection: a new path is the same track as a path that disappeared
   in this scan, or one already marked missing, when exactly one such track
   and exactly one new file share the hash. Anything ambiguous is an
   addition plus a missing track. The row keeps its id, its path is
   updated, and every `.m3u8` that referenced the old path is rewritten.
   A file moved and retagged in one go (Picard, beets) has another hash, so
   a second pass pairs the rest by audio: the same kind, sample rate,
   bitrate and duration to the millisecond, which tags do not change, plus
   the same title or the same artist and album, again only one-to-one. Its
   audio is not proven the same, so its mappings are marked
   (`synced_size` and `synced_duration_ms` 0) and the next sync copies it
   again onto the same iPod entry, which keeps its play count and rating.
   A restore before that sync puts back the backup's mappings, and the
   mark with them is gone.
5. Paths that disappeared without a match get `missing_since` set. Their rows
   are kept, with their statistics and playlist links, so a file that comes
   back or turns out to have moved keeps its history. `doctor --fix` drops a
   row once it has been missing for 30 days and no device maps it (section
   12).
6. Filetype for m4a comes from the codec (AAC or ALAC), not the extension.
7. Paths are stored exactly as on disk, so files can always be opened; the
   library has at least one file whose name is in NFD. Lookups from playlist
   entries and the command line go through `path_key`, the NFC form.
8. Files that cannot be read are reported and left out of the index; if they
   were indexed before, their row is kept as it was.

## 8. Playlists

- Read: UTF-8, `#EXTM3U` header optional, lines ending in LF, CRLF or a
  lone CR, blank lines skipped. Each entry keeps its `#EXTINF` line. The
  other `#` lines (`#PLAYLIST`, `#EXTGRP`, comments) are kept where they are
  in the file whatever an edit does to the entries: those before the n-th
  entry stay before the n-th, the others at the end, and a file with no
  entry keeps them after the header. No line is lost, but a line meant for
  one track (`#EXTVLCOPT`, `#EXTGRP`) can end up next to another after a
  move or a removal. An `#EXTINF` stays with its location across other `#`
  lines between them. Relative entries resolve against the
  playlist's folder. Paths are NFC normalized before lookup.
- Write: always `#EXTM3U`, each entry's other `#` lines as they were, then
  `#EXTINF:<seconds>,<artist> - <title>` and
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
- A reload whose folder listing had errors (an entry or a file's metadata
  that could not be read, a `.m3u8` name that is not UTF-8) reports them and
  takes no playlist as deleted in that pass.

## 9. Device detection and identity

- Read `/proc/self/mountinfo` and keep mountpoints containing
  `iPod_Control/iTunes/iTunesDB`. With more than one, ask.
- Identity is `itdb_device_get_sysinfo(device, "FirewireGuid")`, else the
  GUID in `SysInfoExtended` or `SysInfo` read directly, else the USB serial
  number of the device mounted there, which is the same number: walk up
  from `/sys/class/block/<dev>` to the first USB device and take its
  `serial` if `idVendor` is Apple's `05ac` and the serial is hex. A
  restored iPod has an empty `SysInfo` and no `SysInfoExtended`, so only
  the serial identifies it. A folder has none. With no GUID at all, stop
  with a message telling the user to run `ipod-read-sysinfo-extended`.
- libgpod signs a database with the GUID it reads in the device files, not
  the serial. A write to a signed database is refused before it starts
  when libgpod finds no GUID there.
- Before parsing, check the filesystem type. The supported iPods are FAT32
  (`vfat`); anything else is refused for writes, as is a read-only mount.
- Every write (sync, `doctor --fix`, restore) needs the mountpoint to be
  the root of a `vfat` mount of a `/dev` device in mountinfo. A folder,
  such as a copy of the iPod or its backup, is refused even when
  `device.mountpoint` names it; `device.allow_folder = true` lifts this
  for tests. Reading a folder (`--plan-only`, `adopt`) still works.
- libgpod signs the Classic's `iTunesDB` (hashing scheme 1 at 0x30, hash
  at 0x58) only when `SysInfoExtended` gives `DBVersion` 3 or more, or,
  without one, when it recognises the model from `SysInfo`; the GUID
  alone is not enough. The iPod shows no music from an unsigned database,
  so when the current one is signed and libgpod would not sign, a write is
  refused before it starts, and after it the new header must carry the
  same scheme and a hash, or the sync fails verification. The Video's
  database is not signed (scheme 0), and a restored Video has no model in
  `SysInfo`, so libgpod writes it unsigned and neither check applies.
  Without a model libgpod cannot read the Video's `ArtworkDB` either, so
  once tracks with cover art are on it `exec::check_artwork` refuses a
  sync; a `SysInfo` naming the model (`ModelNumStr: xA450` for the 80 GB
  black one) fixes that. Capacity and free space always come from the
  filesystem (`statvfs`); libgpod's capacity is only a label, missing
  when it knows no model.
- What the iPod is when read (blake3 of `iTunesDB` and `Play Counts`, the
  GUID in its device files) is kept with the plan (`sync::Identity`), and
  `exec::sync` refuses with nothing changed when it differs at write
  time: an iPod swapped, played or changed by another program while a
  prompt waited. `doctor --fix` checks it again after its prompt.

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

1. Scope: every present track; the present members of the playlists
   kept; or every present track except the members of the playlists left
   out, even when another playlist holds them. The scope is the iPod's
   own (`devices.scope`), else `[sync]` in the config. Only the playlists
   kept, or all but those left out, go to the iPod.
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
   compared and sent trimmed and in NFC; values separated by NUL (ID3v2.4)
   are joined with "/", since the iPod's strings cannot hold one. An empty library field, spaces only
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
   A free iPod playlist matched this way was not managed until now: when
   it holds tracks and gets other members, the plan lists it as a takeover
   (`Plan.takeovers`, with its track count and how many the sync takes out
   of it), and one that takes tracks out is refused without
   `--allow-mass-removal` (the window's "Apply it anyway"), since that list
   exists only on the iPod (the owner's decision: matching by name stays,
   and says so).
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

`prepare` refuses a `playlists` scope that is empty, holds no present
track or names a playlist that does not exist, since it would plan the
removal of every managed track, and an `except` scope that names a
playlist that does not exist or leaves out every present track. Renaming
a playlist with iprune renames it in every iPod's scope. A deleted
playlist, or one renamed by another program, keeps its old name there,
and the iPod is refused until its scope is changed: dropping the name
silently would remove the playlist's tracks from an iPod that kept only
some playlists. For
M4: the executor also asks for confirmation, and a plan that removes more
than a tenth of the managed tracks needs an explicit flag.

`iprune sync --plan-only` scans the library, reads the iPod, prints the
summary of section 7.3 of the design (every change with `--verbose`, the
plan as JSON with `--json`) and saves the plan to `plan.json` in the data
folder. It refuses an iPod that has not been adopted, since every track
would otherwise be copied again. An adopted iPod with no mapped track is
refused too, unless it holds no track at all: an empty iPod, adopted
with nothing to match, gets the whole sync scope copied to it.

### 11.2 Executor

`exec.rs` applies a `FullWork` (`full_work`): the track part from
`transfer::track_work` (removals, copies, copies of new audio into an
existing track, tag updates, with the fields to set) and the playlist part
from `playlist_work_with`. A new track's dbid is derived from the iPod's
GUID and the library track (blake3, the first one not already on the
iPod), so the same plan always gives the same work, which is how Apply in
the window checks that it applies what was shown. Playlist members the plan
copies take that dbid; a member update that leaves the iPod list unchanged
is dropped. New audio for a track already on the iPod (a replace) is
copied into the same iPod track: it keeps its dbid and its place in every
playlist, unmanaged ones included. `iprune sync` prints the plan and what
it applies, and asks for confirmation (`--yes` skips it; declining exits
with code 4). `sync_playlists` (M4) is the same executor with no track
part.

Order of operations:

1. Refuse a mountpoint on a read-only mount, or one that is itself a
   mounted filesystem other than vfat (a real iPod is mounted at its root; a
   folder inside another filesystem, such as a copy or a test, passes the
   format check). Symlinks and `..` are resolved first. Refuse deleting more than five
   managed playlists when that is more than a tenth of them, unless
   `--allow-mass-removal` is given: an emptied playlists folder is likelier
   than a decision.
2. Pull statistics: for each mapped track add `recent_playcount` and
   `recent_skipcount` to `stats` and keep the latest `time_played`. A track
   with no `stats` row yet starts from the iPod's lifetime `playcount` and
   `skipcount` instead (libgpod folds the recent counts into them), so the
   plays counted under iTunes before adoption are kept (design goal 6; the
   owner's decision). The rating is taken only when it changed on the iPod:
   `device_tracks.synced_rating` (migration 0006) holds the rating that iPod
   track had at the last pull, and only a different value, a cleared one
   included, changes `stats.rating`. With none recorded yet, a rating on the
   iPod is taken and no rating is not. A new copy or re-copy (a new dbid)
   starts with none recorded, and a restore, or a forced re-adoption of
   tracks that have statistics, records the iPod's ratings as seen, so
   neither erases or lowers a stored rating, nor does another iPod. A track
   with no statistics row takes the iPod's rating whatever is recorded. A
   database whose ratings were reset outside iprune (iTunes, by hand) reads
   as every rating cleared on the iPod, and clears the stored ones.
   Commit to SQLite. This happens first because libgpod resets
   the recent counts on the next write. The pull is recorded in
   `pending_pulls` with the hash of `Play Counts` (migration 0004). If
   anything fails before the write, the pull is undone, since the iPod still
   holds the same counts. Undoing takes back only this pull's part: its
   plays and skips are subtracted, and the rating, last play and recorded
   rating go back only where they still hold what this pull set, so a pull
   of another iPod made since is kept. A sync that stops between the pull
   and the end of the write (killed, or a write that failed part way) leaves
   the record, and the next sync settles it before pulling again. Just
   before the first write, iprune renames `Play Counts` to `Play Counts.bak`
   itself (libgpod does it only after replacing `iTunesDB`, and a crash in
   between would leave counts indexed by the old track order next to the new
   database); a write that fails with the iPod unchanged gets it back. So: a
   `.bak` equal to the file pulled from means the write was reached and the
   pull stands; a `Play Counts` still present (the same, or grown with new
   plays) means it was not, and the pull is undone; neither means a write
   removed it, and the pull stands. A process killed after the rename and
   before libgpod wrote leaves the pull standing while the iPod's own
   database lacks those recent plays in its totals: iprune's counts are
   right, the iPod's are short. A write refused with the iPod unchanged but
   `Play Counts` impossible to put back keeps the pull and says so. A record
   that cannot be read is dropped with a warning. Plays of tracks iprune does not manage are not pulled,
   and the write resets them.
3. Back up `iTunesDB`, `ArtworkDB`, `Play Counts` and `OTGPlaylistInfo`
   (each copy compared with the original) to
   `backups/<guid>/<YYYYMMDD-HHMMSS>/`, with `plan.json`, `work.json` (the
   operations applied), `snapshot.txt` (before) and `mappings.json` (the
   device's mappings). The folder is filled under a hidden name and renamed
   when complete. The `F*_*.ithmb` files of `iPod_Control/Artwork` go to
   the iPod's `.ithmb` store, named by their blake3, and `artwork.json`
   lists them (name, size, time, hash): `ArtworkDB` holds offsets into
   them, and libgpod rewrites them in place on a write (it moves
   thumbnails into the slots of removed ones and truncates). A file with
   the name, size and time it had in the previous backup is not read
   again; the others are hashed, and copied only when their content is not
   stored yet. Pruning backups deletes stored files no backup lists. After
   the write, `.ithmb` files that changed size, or a new one, beside an
   `ArtworkDB` still equal to the backup's count as a verification failure:
   libgpod does not report a failed `ArtworkDB` write.
   Before that, the write is refused if libgpod could not read the artwork
   (`check_artwork`): `.ithmb` files while no track holds a thumbnail and
   some point at an artwork entry without one, or more than 5 tracks, and
   more than a tenth of those with artwork, that point at an entry without
   one. `.ithmb` files no track points at are leftovers libgpod deletes;
   they are in the backup. A missing or damaged `ArtworkDB`
   parses that way, and the write would then delete every `.ithmb` file.
4. `itdb_start_sync`.
5. Remove tracks: drop from all playlists, master and smart ones included,
   then `itdb_track_remove`. The file is deleted only once a write is
   verified, so a failed write never leaves the old database pointing at a
   deleted file; a crash in between leaves an orphan for `iprune doctor`.
   Only a regular file directly in a folder of `iPod_Control/Music` that no
   track of the written database still uses is deleted; any other path a
   damaged database names is kept with a warning.
   When the plan also copies, the removals are written, verified and their
   files deleted before the first copy, so the space they free (which the
   space check counts) really is free.
6. Copy: a new track is built (`NewTrack`, freed if never added) with every
   library tag, the title falling back to the file name, then `add_track`
   and `itdb_cp_track_to_ipod`. A library file gone, unreadable or changed
   since the scan is skipped and reported, left out of the playlists, and
   copied by a later sync; a copy that fails with a readable file (the iPod
   full or gone) takes its half-added track out and stops the sync as below.
   A replace copies the new file into the existing track and sets its audio
   facts and tags, keeping its date added; the old file is deleted after the
   verified write. Its tags are set like a retag: a tag the file lacks keeps
   the iPod's value, so a file with no title never puts its file name over
   an iPod title (the file name stands in only for a new track). What
   described the old file is cleared: gapless data (pregap, postgap,
   sample count, the track's gapless flag), start and stop times, Sound
   Check and the resume position. Play counts, rating, dates and the album's
   gapless flag stay. A file that is not marked as part of a compilation never
   clears the iPod's mark. Copies run one at a time with progress in bytes;
   the USB link is the limit. Files sampled above 48 kHz, which neither
   iPod plays, and files of 4 GiB or more, which FAT32 cannot
   hold, are skipped by the planner like FairPlay files. A copy that fails
   is checked by reading the library file to its end: if that fails too (a
   bad sector), the file is skipped like one gone and the sync goes on;
   otherwise the iPod failed and the sync stops as below.
7. Update tags on existing tracks: only the fields the plan lists, so an
   empty library field never clears an iPod value.
8. Apply playlist operations: deletes, renames, creates, then member updates
   with `set_playlist_members`.
9. Run the pre-write checks (section 11.3). On failure, stop without writing
   and exit with code 3.
10. Take the snapshot of the changed database, then `itdb_write`. libgpod
    writes several files; if it fails, the iPod's `iTunesDB`, `ArtworkDB`
    and `.ithmb` files are compared with the backup. Unchanged: the pull is undone and the error
    reported. Changed: the error names the backup to restore (exit code 3),
    the latest checkpoint's when one was written, and the pull stays pending
    for the next sync to settle as above. A checkpoint backup that cannot be
    taken stops the sync before that write, saying the iPod holds the last
    checkpoint.
11. `itdb_stop_sync`.
12. Verify (section 11.4).
13. Record the playlist mappings (with the ids of created playlists), drop
    the tombstones whose iPod playlist was deleted and set `last_sync_at`.
    Then export `stats.csv`, write the manifest to the iPod and keep the
    newest `keep_backups` backups, never deleting the one this sync took;
    a failure in these last three is a warning, since the sync itself is
    done. A new backup's stamp always sorts after the existing ones, even
    with the clock set back (it then takes the latest stamp with a higher
    `-N`). Only folders named like a stamp (`YYYYMMDD-HHMMSS`, with an
    optional `-N`) count as backups; anything else in the folder is
    ignored.

Mappings are recorded right after each verified write. If a crash comes
in between, the next `prepare` takes them back: an unmapped library
track whose own deterministic dbid (one of the first four `new_dbid`
would try) is on the iPod, unmapped and the file's size, is that track.
Its synced tag hash is left blank, so the next plan compares its iPod tags
with the library's. A file edited after the crash has another content hash,
so it is not taken back: it is copied again, and the earlier copy stays on
the iPod as an unmanaged track, reported by the next sync.

Checkpoints: during a long copy the database is written every 500 tracks
or 10 minutes, after the checks of section 11.3 on what is done so far
(removals and copies; playlists untouched, less the removed tracks), then
verified, and the mappings of what it holds are recorded. An interrupted
sync loses at most the copies since the last checkpoint, whose files
`iprune doctor` finds. Before each write after the first, what the iPod
holds (the last verified checkpoint) is backed up with the mappings
recorded for it, so a later write that fails names that backup rather than
the one from before the sync, whose removed files are already deleted (the
owner's rule: a backup before every write). Only the latest checkpoint's
backup is kept, and it is deleted once the sync is written and verified.

Ctrl+C, SIGTERM (logout, shutdown) or SIGHUP (the terminal closed) in the
CLI, or the same signals or Stop (shown only while there are copies) in the
window, during the copies finishes the current file,
writes what was done as a checkpoint and stops; a copy that fails on the
iPod side does the same. Until a command starts applying (after its
confirmation) a signal ends it at once; from then on, in `sync`, `doctor
--fix` and `restore` alike, it only asks it to stop and it never exits in
the middle of a write, a second signal included (a copy that hangs is ended
with `kill -9`, leaving an orphan for `iprune doctor`). Output to a
terminal that has gone is ignored. While a command applies, and while the
window's sync runs, a logind block inhibitor (`systemd-inhibit`, module
`inhibit`, in its own process group so the terminal's Ctrl+C or hang-up
does not end it) keeps the machine from suspending, shutting down, going
idle or acting on the lid; the desktop's own inhibit does nothing on
Hyprland. When it cannot be taken, the CLI says so. A
forced shutdown or power loss still ends it, and the checkpoint, the
pending pull, `restore` and `doctor` recover from that. If the checks before the final write fail after a
checkpoint, that write is not made and the checkpoint stands. Playlists and tag
updates are then left for the next sync, which plans them again. A plan
whose copies do not fit is refused before anything is written, and so is
one removing more than five managed tracks when that is more than a tenth
of them, or more than five tracks iprune does not manage
(`remove_unmanaged`) when that is more than a tenth of the iPod's tracks,
unless `--allow-mass-removal` is given (`exec::check_guards`).

### 11.3 Pre-write checks

The file of each track copied or copied again must exist and have the
library file's size, or the write is not made: a USB error can leave a
short file, and the old file of a replaced track would be deleted on its
strength.

`verify::check`, on the in-memory database after the changes, against an
`Expect` of what was meant to change:

- The tracks are those before, less the removed, plus the added. Every
  other track is exactly as before (apart from the count of playlists it
  is in), except that a retagged track has exactly the tags set and the
  same file and size, and a recopied one has a file.
- Added and recopied tracks have a file on the iPod.
- The master playlist's members are those before, less the removed, plus
  the added at the end.
- The number of regular playlists is the count before, plus those created,
  minus those deleted, and no playlist appears that is neither managed nor
  there before.
- Every managed playlist, found by its iPod id, has the expected name and
  exactly the expected dbids in the expected order, repeats included.
- Every playlist member is a track of the database.
- Unmanaged playlists are unchanged (same id, name and members) apart from
  removed tracks, and the deleted ones are gone.
- The smart playlists and the iPod's name are unchanged.

### 11.4 Verification

After `itdb_write`, close the database, parse it again from the iPod,
produce a snapshot in the same text format as the C `snapshot` tool, and
compare it with the snapshot of the in-memory database taken just before
the write. A database that cannot be read back counts as a difference. The artwork link (`mhii`) is excluded, since libgpod renumbers it
on every artwork write; it is found from the end of a track line, since a
tag may contain the same text. The members of the playlists libgpod flags
as Podcasts, taken by position so a regular playlist of the same name keeps
its order checked, are compared as a multiset: libgpod writes it grouped by show through a hash
table, so its order read back is not the order in memory. The header's signature is checked as in section 9.
Any difference is printed (up to 20 lines) and the
command exits with code 3; the backup stays in place for `iprune restore`
and the mappings are not updated.

### 11.5 Restore

`iprune restore <stamp>` (confirmation, or `--yes`) identifies the iPod from
`SysInfoExtended` or `SysInfo`, not from its database, which may be the
damaged part. With no readable GUID it refuses unless `--guid` names the
iPod's backup folder, as `iprune backups` lists it: a backup of another
iPod must never be written onto this one. Just before the first change,
the GUID in the iPod's device files is read again and must still be the
backup's (spelled with or without `0x`, in either case): the iPod may have
been swapped while the prompt waited. Backups of one iPod share one folder
whatever the spelling libgpod gives its GUID. The confirmation and `iprune
backups` show each backup's iPod name and track and playlist counts, from
its `snapshot.txt`. Under the iPod lock, in order:

1. The backup's `mappings.json` is read and its `iTunesDB` parsed; if
   either fails, nothing changes.
2. What the iPod holds now is backed up like before a sync (its
   `iTunesDB` may be missing), with the current mappings, so a restore can
   itself be undone.
3. When the current database can be read, a pending pull is settled and
   the statistics are pulled, as before a sync: `Play Counts` belongs to
   the current database's track order. When it cannot be read and a
   non-empty `Play Counts` is there, the restore is refused unless
   `--discard-play-counts`.
4. The device is marked as being restored; `sync`, the window's Sync and
   `doctor --fix` refuse to run until the mark is cleared, so a restore
   stopped part way is run again. `Play Counts` and `OTGPlaylistInfo`,
   which index tracks by position, are deleted from the iPod once the
   backup of step 2 is checked to hold them, and the pull is settled.
   On-The-Go lists are not restored.
5. The `.ithmb` files the backup lists (from the store), `ArtworkDB`, then
   `iTunesDB` are copied next to their targets under
   temporary names and compared, then renamed over them, `iTunesDB` last.
6. The iPod's database is read back and compared with the backup's
   `iTunesDB` read from a copy on its own, so no `Play Counts` is applied
   (artwork fields left out). A difference exits 3, leaves the mark set,
   and names both ways out: the same restore again, or a restore of the
   backup of step 2.
7. The mappings are replaced with the backup's and the mark cleared, in
   one transaction. Mappings of library tracks gone since are dropped. A
   playlist mapping whose library playlist is gone gets a tombstone row,
   so the next sync deletes that iPod playlist again.

Track and playlist ids are `AUTOINCREMENT` (migration 5): an id is never
given to another track or playlist, so a backup's mappings cannot attach an
old iPod entry to a different song or playlist. Ids freed before migration
5 below the highest id then in use can still have been given out again; a
backup taken before it may hold such mappings. Entries the backup brings
back whose files a later sync deleted have no file: the next sync copies
mapped tracks in scope again (`files_missing` in the planner input), and
`doctor` reports the others. More than 5 such tracks that are also more
than a tenth of the managed ones are refused without
`--allow-mass-removal` (the window's "Apply it anyway"): that many more
likely means the iPod's music folder reads badly. After restoring a backup
older than a sync that removed many tracks it is expected, and the flag is
the way through. Ratings pulled from the
replaced database are overwritten by the restored database's own on the
next sync. Files added after the backup become orphans
until `doctor --fix` moves them aside.

## 12. Doctor

`iprune doctor [--fix] [--yes] [--allow-mass-removal]` reports, and with
`--fix` repairs (asking first):

- files under `iPod_Control/Music` with no database entry (moved to
  `iPod_Control/iprune/orphans/<stamp>/` on the iPod, by rename: a file the
  database lost may be music that exists nowhere else; deleting that folder
  frees the space),
- database entries whose file is missing (remove entry),
- database entries whose iPod path is not a plain path under the iPod
  (`..`, empty parts): reported only, never repaired, so nothing is
  deleted by following them,
- regular playlists with repeated tracks not present in their source file
  (rebuild membership),
- mappings pointing to dbids not on the iPod (drop mapping),
- library tracks marked missing for more than 30 days and not on any device
  (drop from index: their statistics go, and playlist entries pointing to
  them stay as unresolved lines).

Database entries are removed through the sync's executor (statistics pull,
backup, checks, write, verification), with the playlist mappings left as
they are. Orphan files are moved aside after that. More than five orphans that
are over a tenth of the iPod's tracks more likely mean a damaged database
than leftovers (restoring a backup is the better answer), and as many
entries without a file more likely mean an unreadable music folder than lost
files: both need `--allow-mass-removal`, and are refused before anything is
repaired. A managed playlist holding a track more than once is
rebuilt by the next sync; an unmanaged one is only reported.

## 13. CLI

```
iprune init --library DIR... --playlists DIR [--path-style S] [--force]
iprune scan
iprune adopt [--file <iTunesDB>] [--accept-proposals]
iprune sync [--plan-only] [--verbose] [--json] [--yes] [--allow-mass-removal]
iprune playlist list|show|new|rm|rename|add|remove|move ...
iprune scope [all | only PLAYLIST... | except PLAYLIST... | default] [--guid GUID]
iprune device info
iprune backups
iprune restore <stamp> [--guid GUID] [--mountpoint DIR] [--discard-play-counts] [--yes]
iprune doctor [--fix]
iprune snapshot <mountpoint|iTunesDB>
iprune eject
iprune                    (no arguments: prints help; the window is iprune-gui)
```

`scope` shows or sets what an adopted iPod syncs, the connected one or
the one `--guid` names, with the number of tracks and bytes it selects
and, when connected, the iPod's size. `default` makes it follow `[sync]`
again. A scope that `prepare` would refuse is not saved.

`init --force` changes an existing config: the library folders and the
playlists folder given replace the old ones, `--path-style` when given, and
every other setting is kept. A config that no longer passes its checks is
mended this way; one that cannot be parsed is refused, to be fixed or
removed by hand, and the result is checked before it is written. It warns
when indexed tracks lie outside the new folders (compared in NFC), since
the next scan marks them missing. `doctor --fix` finds the iPod, then scans
the library before examining it; when the scan fails, no track is forgotten that time, and
a track whose file is back is never forgotten. `restore` that finds no iPod
with a database looks for a FAT32 mount with the iPod's device files
(`SysInfo` or `SysInfoExtended`), the state a lost `iTunesDB` leaves, or
takes `--mountpoint`.

Exit codes: 0 success, 1 error, 2 usage, 3 verification failed, 4 user
declined the plan.

## 14. Desktop UI

Crate `iprune-gui`, binary `iprune-gui`, GTK4 (4.12 or later) through
`gtk4-rs`, without libadwaita. Omarchy switches GTK applications between
light and dark with the `gtk-theme` (Adwaita or Adwaita-dark) and
`color-scheme` settings, which plain GTK4 follows. The window needs none of
libadwaita's widgets.

- **Grid.** `gtk::ColumnView` over a `gio::ListStore` of
  `glib::BoxedAnyObject`s holding plain `view_model::Row`s, wrapped in a
  `gtk::FilterListModel` (search) and a `gtk::SortListModel` fed by the
  column sorters (`gtk::CustomSorter` calling `Column::compare`). List widgets are recycled,
  so widgets exist only for the visible rows; the ~9,700 track objects are
  plain data that sorting and filtering go through. Column visibility, width
  and order are saved per view in `~/.local/state/iprune/ui.toml`.
- **Badge.** An "IPRUNE" label (`app-mark`, a fixed orange with a border,
  clear on dark and most mid backgrounds, weaker on very light ones)
  follows the Sync button in the header, so the window is not taken for
  fiio-tunes', its fork, which shows "FIIO-TUNES".
- **Sidebar.** A `gtk::ListBox` (about 340 rows) with headers for Library
  and Playlists, and the iPod's state below it. It starts 200 pixels wide,
  enough for nine playlist names in ten. Once dragged, its width is saved
  in `ui.toml` (`sidebar`) and used next time if between 120 and 600
  pixels. Playlists are drop targets
  for tracks. A filter field above it hides, as you type, the playlists
  whose name does not hold every word typed (case, accents and punctuation
  ignored, `Query::matches_name`), except the playlist on screen; Enter
  shows the first one kept, Escape empties the field.
- **Add to Playlist.** The grid's menu item opens a popover of its own, not
  a submenu (a GTK submenu is drawn at the height of the menu it replaces,
  three rows here): a filter field over a list of every playlist but the
  one shown, up to 480 pixels high. Typing filters it the same way, the
  first playlist kept is selected, and Enter or a click adds the selected
  tracks to it; Down moves into the list.
- **Show in Playlist.** The same popover, listing the playlists that hold
  the library track of the row right-clicked (`store::playlists_holding`),
  but the one shown; the menu item is disabled when there is none or the
  row has no file. Picking one empties the grid's search, selects the
  playlist in the sidebar, which shows it, and selects every entry of the
  track there, scrolled to the first (`positions_of_track`).
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

Built in steps: (1) the read-only window: sidebar, grid over the library or a
playlist (in file order, unresolved entries dimmed), every column sortable
with iTunes-style ties (artist, album, disc, track, name), a column menu,
search, a status line with totals, and the iPod's state; (2) editing:
dragging rows onto a playlist in the sidebar appends them, dragging inside a
playlist sorted by `#` (ascending) reorders it (`playlists::move_entries`,
one write for several rows), Delete removes the selected entries, Ctrl+Up
and Ctrl+Down move them, a context menu offers Remove from Playlist and Show
in Folder, and playlists are created (Ctrl+N or the + button), renamed (F2
or the sidebar's context menu) and deleted (context menu, with
confirmation). A `gio::FileMonitor` on the playlists folder reloads changes
made by other programs, the CLI included, by comparing the file on screen
with its modification time when it was read. The column order, widths and
visibility are saved in `~/.local/state/iprune/ui.toml`, one layout for the
library and one for playlists; (3) the Sync button, enabled when an
adopted iPod is mounted (the panel follows `gio::VolumeMonitor`): a worker
thread (`sync_job`, no GTK) scans the library, reads the iPod and plans; the
dialog shows the summary, every change in an expander, and what this
version applies; Apply reads the index and the iPod again on a worker,
refuses if the playlist changes it would apply are no longer the ones
shown (a change in free space or in the planned copies does not count),
and runs the M4 playlist sync. It compares against the index, without
reading the playlists folder again: a file edited after the plan but not
yet read is left for the next sync, and the window reads such edits
within half a second anyway. While it writes, neither the dialog nor the window can close, the
application is held, and suspend, shutdown and the lid are inhibited (see
section 11.2); a signal acts as Stop, and the window quits once the sync
is done. The result shows
the backup stamp, or the error with the restore command. libgpod objects
never cross threads: each worker opens its own database and its own
connection to the index (SQLite waits up to 10 s for the other's writes).
With M5 the same dialog runs the full sync, with a progress bar and Stop;
(4) the iPod panel's buttons and the grid's Add to Playlist menu. An iPod
mounted but not adopted gets an Adopt button: a worker (`adopt_job`, no
GTK) scans the library, reads the iPod and runs `adoption::run`, writing
`adopt-report.txt`; the dialog shows the summary and a grid, one row per
proposal, whose last column is a drop-down of "leave it unmanaged" and the
candidates, set to what `--accept-proposals` would take. Picking one
library file for two iPod tracks, or one an accepted match holds, is shown
and Save refused until resolved (`adopt::resolve_choices`). Save checks the
iPod mounted is still the one reviewed and calls `adoption::choose` and
`adoption::save`; an iPod already adopted is refused, as without `--force`.
An adopted iPod, connected or not, gets a Selection button: a dialog of
three choices (the whole library, only the playlists checked, the whole
library except them), the playlists with a filter, and the number of
tracks and bytes selected against the iPod's size, updated as boxes are
checked. Save stores the scope for that iPod as `iprune scope` does; the
panel shows it in a few words.
Eject runs `device::eject` on a worker: the block device mounted at the
mountpoint, from mountinfo, is unmounted then powered off with `udisksctl`.
Sync, Adopt and Eject share one busy flag: one of them at a time. Across
processes, an advisory `flock` on `ipod.lock` in the data folder, taken
without waiting, keeps the CLI and the window from writing or unmounting
the iPod under each other; the second is refused and changes nothing. A
sync (CLI, window) and `doctor --fix` take it before reading the iPod and
hold it until the write is verified, so none writes back a database
another changed in between: `exec::sync` takes the held lock as an
argument. `exec::restore` and both ejects take it themselves.
`iprune-gui` is a single-instance application: starting it again brings
the running one's window to the front.

## 15. Omarchy packaging

- `packaging/install.sh` runs `cargo install --locked` for `crates/iprune`
  and `crates/iprune-gui` (into `$CARGO_INSTALL_ROOT`, by default
  `~/.cargo`) and writes the desktop entry
  `~/.local/share/applications/iprune.desktop` from
  `packaging/iprune.desktop`, with `Exec` set to the installed
  `iprune-gui` in full, since the launcher may not have cargo's folder on
  its PATH. `StartupWMClass` is the application id, `dev.iprune.Iprune`.
  `install.sh --uninstall` removes the binaries and the entry. A PKGBUILD
  may come later.
- Notifications through `notify-send` (module `notify`), once a plan is
  confirmed (`--yes`, the prompt, or Apply in the window): "iPod synced"
  with what changed, "iPod sync stopped early" with why, or "iPod sync
  failed" with the error, shown until dismissed. A failure after the
  confirmation notifies even when it came before any write, such as a
  plan that changed since the window showed it, since the user may have
  walked away. The CLI and the window use the same wording. `notify-send`
  is started without waiting for it, and a notification that cannot be
  shown is ignored. A plan that is only shown, declined or found empty gives
  none. Plugging in the iPod gives none either.
- Optional binding, for example in `~/.config/hypr/bindings.conf`:
  `bindd = SUPER SHIFT, I, iPod, exec, iprune-gui`. Pressing it again
  brings the open window to the front.

## 16. Testing

| Level | What | How |
|---|---|---|
| Unit | planner, playlist parsing, adoption scoring, tag diffing | pure functions, table-driven tests |
| Bindings | struct layouts, strings, lists | Rust snapshot vs C `snapshot` on the backup `iTunesDB` |
| Integration | full sync on a fake iPod | `itdb_init_ipod` into a temp dir with a `SysInfo` containing a `FirewireGuid`, small audio fixtures generated with ffmpeg. With no model in `SysInfo`, libgpod writes an unsigned database, as on the Video; tests of signing and artwork add a Classic model number |
| Regression | playlist doubling | sync the same plan twice; the second plan must be empty and the snapshot unchanged |
| Real data, read only | adoption on 9,669 tracks | `adopt --file` on the backup, compare match counts between versions |
| Device | first write | playlists-only sync on the real iPod after a manual backup, then `snapshot` diff |

Property test with `proptest`: random library and playlist edits, plan,
apply to a fake iPod, plan again; the second plan is always empty.

Everything cargo runs has `IPRUNE_NO_DETECT=1` (`.cargo/config.toml`): iPod
detection refuses, so no test can find the real iPod through the mounts.
Every test names its fake iPod in `device.mountpoint`. `cargo run` needs a
mountpoint set too; the installed binaries are not affected.

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
| libgpod does not understand some iTunes smart playlist rules ("Unknown action type 1" on write) and may rewrite them | checked in M4 on a copy of the real `iTunesDB` (2026-10-13): parsed and written back unchanged, the 12 smart playlists' header and rule records (`mhod` 50 and 51) are byte-identical and the snapshot is identical. The only bytes that change are the album and artist list ids (`mhia`, `mhii`), regenerated on every write, and a few `mhbd` header fields. Parse 33 ms, write 255 ms |
| Artwork database corruption | iprune adds and edits no artwork, but libgpod rewrites `ArtworkDB` and the `.ithmb` files on every write; both are backed up together and restored together, and a write is refused when the artwork could not be read |
| Losing play counts | pulled and committed before any write, exported to CSV |
| FairPlay files (one in the library, "Bailemos") may not play on an iPod that is not authorised for that Apple account | the planner skips unmapped ones and leaves mapped ones in place; M5 decides whether copying them is worth offering |
