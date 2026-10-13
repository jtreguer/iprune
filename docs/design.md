# iprune: design document

Status: draft, 2026-10-13

## 1. Purpose

iprune is a small Linux tool that manages playlists for a local music library
and keeps an iPod Classic in sync with that library. It replaces the parts of
iTunes that Linux players either lack or get wrong: deliberate, verifiable
sync of tracks and playlists to an iPod.

It does not play music, and it does not coordinate with music players.

## 2. Background

The current setup, as of this document:

| Item | Value |
|---|---|
| Library root | `/mnt/seagate-1tb/Music`, about 77 GB |
| Track files | 8,032 mp3, 1,561 m4a, 79 wav, 1 aiff, 2 m4v (video) |
| Layout | iTunes style, `Artist/Album/NN Title.ext` |
| Playlists on disk | 339 `.m3u8` files in `/mnt/seagate-1tb/Playlists`, absolute paths |
| Rhythmbox | 339 static playlists, 3 automatic ones |
| Strawberry | 341 playlists, 9,675 songs indexed from the same root |
| iTunes export | `/mnt/seagate-1tb/Bibliothèque.xml`, with play counts and dates added |
| iPod model | iPod Classic 7th generation, 160 GB, FAT32 (Windows formatted) |
| iPod | 9,669 tracks, 329 regular playlists, 7 smart playlists, 6,191 tracks with play counts |
| iPod backup | `/mnt/seagate-1tb/iPod-backup-2026-10` |

The iPod already holds almost the whole library. It was filled by iTunes and
later modified by Rhythmbox, whose sync doubled every playlist on the device.
That was repaired on 2026-10-12 with small libgpod programs (`repair`,
`snapshot`, `rmtracks`, kept in the backup's `tools/` folder). That incident
shapes this design more than anything else: **a sync tool that is not
predictable is worse than no sync tool.**

## 3. Goals

1. Point at one or more local music folders and index them.
2. Create, edit, reorder, rename and delete playlists.
3. Sync tracks and playlists to an iPod Classic, one way, library to device.
4. Show exactly what a sync will do before it does it, and verify afterwards
   that the iPod matches what was planned.
5. Take over the iPod as it is today without copying 77 GB again.
6. Bring play counts, skip counts and ratings back from the iPod so they are
   not lost.
7. Fit Omarchy: keyboard driven, runs in the terminal, follows the theme.

## 4. Non-goals

- Playback, and any integration with music players (Rhythmbox, Strawberry).
- Cover art. Artwork already on the iPod is left as it is; iprune never adds
  or changes artwork.
- Importing statistics from the iTunes export. The iPod's counts are used.
- Tag editing. Use a tag editor (Picard, Kid3, beets); iprune rereads tags.
- Video, podcasts, audiobooks, photos.
- Smart playlists on the iPod. Existing ones are left untouched.
- iPods other than the Classic, iPhones, MTP players.
- Syncing back from the iPod to the library (except play statistics).
- Automatic sync on plug-in. Sync always starts from a command.

## 5. Principles

**The plan is the product.** Every sync starts by computing a plan: tracks to
add, remove and update, playlists to create, change and delete. The plan is
shown in full and nothing touches the iPod until it is confirmed. The same plan
is then executed and checked.

**Files are the truth for content, the iPod is the truth for listening
history.** Tags come from the audio files. Playlists come from the `.m3u8`
files. Play counts, skips, ratings and last-played dates come from the iPod.

**Leave alone what you do not own.** iprune only changes iPod tracks and
playlists it knows it manages. Anything else is reported and kept unless the
user asks for removal.

**Always reversible.** The iPod database is backed up before every write.
Restoring the previous state is one command.

**Playlists are plain files.** Each playlist is one `.m3u8` file, edited
through iprune. iprune's own database is a cache and an
index; deleting it loses no user data except accumulated play statistics,
which are also exported.

## 6. Concepts

**Library.** The set of audio files under the configured roots. Each file is a
track. A track has a stable id in iprune's database that survives renames and
moves as long as the file content does not change at the same time.

**Playlist.** An ordered list of tracks stored as one `.m3u8` file in the
playlists folder. The file name, minus extension, is the playlist name. A track
may appear twice in a playlist only if the file says so.

**Device.** One iPod, identified by its FireWire GUID. iprune keeps a mapping
between library tracks and the iPod's own track ids (`dbid`) for each device.

**Sync scope.** What goes on the device. Either the whole library (today's
situation, and the default), or only the tracks in a chosen set of playlists.

**Adoption.** The one-time step that matches the tracks already on an iPod to
library files, so the first sync updates the device instead of refilling it.

## 7. User workflows

### 7.1 First run

1. `iprune init` writes a config pointing at `/mnt/seagate-1tb/Music` and
   `/mnt/seagate-1tb/Playlists`.
2. `iprune scan` indexes the library and loads the playlists. It reports
   playlist entries that point to missing files.
3. With the iPod connected, `iprune adopt` matches the 9,669 iPod tracks to
   library files and shows the result: matched, ambiguous, unmatched on either
   side. The user resolves ambiguous cases in the interface or accepts the
   defaults. Nothing on the iPod changes at this step.
4. `iprune sync` shows the first plan. After adoption it should be small: the
   few library tracks missing from the iPod, and any playlist differences.

### 7.2 Editing playlists

In the interface: pick a playlist, search the library, add tracks, reorder
them, remove them, rename or delete the playlist. Each change is written to
the `.m3u8` file straight away. Changes made to those files by another program
are picked up on the next scan or when the interface regains focus.

From the command line, for scripts:
`iprune playlist add "80s" <path>...`, `iprune playlist new`, `rm`, `rename`.

### 7.3 Syncing

1. Connect the iPod. iprune finds it by looking for a mounted filesystem with
   `iPod_Control/iTunes/iTunesDB`.
2. `iprune sync` (or `s` in the interface) scans for library changes, reads the
   iPod, pulls play statistics, and shows the plan:

   ```
   iPod "iPod de Julien" (FireWire GUID 000A27...)
     + 4 tracks to copy (38.2 MB)
     - 1 track to remove (not in library any more)
     ~ 12 tracks with changed tags
     playlists: 1 new, 3 changed, 0 removed
     unmanaged on iPod: 0 tracks, 0 playlists
     play counts pulled: 57 tracks
   Apply? [y/N]
   ```

   Any section can be expanded to the full list.
3. On confirmation: back up the database, copy files, update the database,
   write it, then read it back and compare it to the plan.
4. `iprune eject` unmounts and powers off the iPod.

### 7.4 Recovering

- `iprune backups` lists saved iPod databases.
- `iprune restore <backup>` puts one back. Files copied after that backup stay
  on the iPod as orphans; `iprune doctor` finds and removes them.
- `iprune doctor` also checks for playlists with repeated tracks, database
  entries whose files are gone, and files on the iPod with no database entry.

## 8. Interface

A terminal interface built for the keyboard, with three panes:

```
┌ Playlists ─────────┐┌ 80s ─────────────────────────────── 214 tracks ┐
│ 1984               ││  1  Tears for Fears   Everybody Wants to Rule… │
│ 2000s              ││  2  a-ha              Take On Me               │
│ 70s                ││  3  Pet Shop Boys     It's a Sin               │
│▸80s                ││  …                                             │
│ 8D                 ││                                                │
│ +Abel Korneziowski ││                                                │
│ …                  ││                                                │
├ Device ────────────┤│                                                │
│ iPod de Julien     ││                                                │
│ 9,669 tracks       ││                                                │
│ 71 GB used / 160 GB││                                                │
│ last sync 2 d ago  ││                                                │
└────────────────────┘└────────────────────────────────────────────────┘
 / search   a add   d remove   J/K move   n new   r rename   s sync   q quit
```

- `/` opens a fuzzy search over the whole library (artist, album, title).
  Results can be added to the current playlist.
- The sync plan opens as a full-screen view with expandable sections.
- Colors use the terminal's 16-color palette, so Omarchy theme changes apply
  without configuration.
- The playlist pane shows names exactly as the files are named, so the `+` and
  `-` prefixes already in use keep working as a sorting convention.

Every action in the interface is also a command, so syncing can be bound to a
Hyprland key or run from a script.

## 9. Sync rules

### Tracks

| Library | iPod | Action |
|---|---|---|
| In scope, not on iPod | | copy |
| | managed, no longer in scope or deleted | remove from iPod |
| | not managed by iprune | report, keep |
| Tags changed | managed | update iPod database fields, no file copy |
| Audio changed (size and duration differ) | managed | copy new file, remove old one |

When local tags are empty for a field that has a value on the iPod, the iPod
value is kept. This protects metadata that iTunes stored only in its database,
which matters for the 79 WAV files.

Files the iPod Classic cannot play (anything other than MP3, AAC, ALAC, WAV,
AIFF) are skipped and reported. The current library has none apart from the
two videos. Transcoding is planned for later (section 12).

### Playlists

- A playlist on the iPod gets exactly the members of its `.m3u8` file, in the
  same order. Its contents are rebuilt from scratch on every change, never
  appended to.
- Playlist entries that point to files outside the sync scope or missing from
  the library are skipped and reported.
- iPod playlists with no matching file are reported and kept, unless the user
  chooses to remove them.
- The master playlist and smart playlists are never edited directly.

### Play statistics

On every sync, before anything is written, iprune reads plays and skips since
the last sync, the current rating and the last-played date for every managed
track, and adds them to its own totals. These totals are exported to
`~/.local/share/iprune/stats.csv` after each sync so they outlive the
database.

### Safety checks

Before writing, iprune checks the planned database: every playlist has the
expected length; no playlist contains a repeated track unless its file does;
every playlist member is in the master playlist; the track count equals the
expected count. After writing, it parses the database again and compares a
snapshot with the expected result. Any difference stops the sync, leaves the
backup in place, and prints the difference.

## 10. Adoption

Paths on the iPod are scrambled (`iPod_Control/Music/F12/ABCD.mp3`), so
tracks are matched on content:

1. File size, which iTunes and libgpod preserve when copying. On the current
   iPod, 96 sizes are shared by more than one track, so size alone is not
   enough.
2. Duration within one second.
3. Normalized title and artist.

A match on all three is accepted. Size and duration without matching tags is
proposed for confirmation. Anything else is listed as unmatched. Unmatched iPod
tracks are left on the device as unmanaged; unmatched library tracks are copied
on the next sync.

Adoption also matches iPod playlists to `.m3u8` files by name, and reports
playlists whose contents differ.

## 11. Omarchy integration

- Installed as a single binary, plus a desktop entry that opens it in a
  terminal so it shows in the Omarchy app launcher.
- An optional Hyprland binding in `~/.config/hypr/bindings.conf`.
- A desktop notification when a sync finishes or fails.
- No udev rule that syncs automatically. Plugging in the iPod may show a
  notification with the pending plan size, nothing more.

## 12. Later

In rough order of value:

1. Transcoding on the fly (FLAC or Opus to ALAC or AAC) with a cache, for when
   the library gains formats the iPod cannot play.
2. Sync scope by rules (genre, rating, added in the last N months) to fit a
   smaller iPod.
3. Watching the library folders for changes while the interface is open.
4. A second iPod.

## 13. Decisions

Answers to the questions left open in the first draft (2026-10-13):

1. The iPod is a Classic 7th generation, 160 GB, formatted FAT32. HFS+ is not
   supported.
2. Rhythmbox and Strawberry are out of scope. The `.m3u8` folder is the only
   place playlists live, and iprune is the tool that edits them.
3. No cover art. iprune does not add, change or remove artwork.
4. Play statistics come from the iPod only. The iTunes export is not imported.
