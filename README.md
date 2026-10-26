# iprune

iprune manages the playlists of a local music library and syncs the
library to an iPod on Linux. It does what iTunes did for the iPod
Classic and what Linux music players either lack or get wrong: a sync
that you can read before it happens and that is checked after it is
written.

It has two parts:

- `iprune-gui`, a GTK4 window in the manner of iTunes: a sidebar of
  playlists, a sortable grid of tracks, drag and drop, and a Sync button.
- `iprune`, a command line tool that does everything the window does.

iprune does not play music or touch cover art. It edits the tags the iPod
uses (name, artist, album, genre, year and the like) in a Get Info form, as
iTunes did, for one song or several at once, or with `iprune tag`; only the
fields you change are rewritten in the file. Your music folder and its
`.m3u8` playlist files are the reference. The iPod is a
copy of them, kept up to date one way, from the library to the device.

## How a sync works

Every sync starts with a plan. Nothing touches the iPod until you accept
it, and afterwards the iPod is read again and compared with the plan.

## Features

- **Library index.** Scans one or more music folders (MP3, AAC, ALAC,
  WAV, AIFF) into a SQLite index. Files moved or renamed keep their
  identity, by content hash. An unmounted disk is never taken for an
  empty library.
- **Playlists.** Create, rename, delete, reorder and edit `.m3u8`
  playlists from the window or the command line. Writes are atomic, and
  entries iprune cannot resolve are kept as they are.
- **The window.** Sortable and resizable columns, a search field, a
  filter for the playlists, drag and drop into and within playlists,
  Add to Playlist and Show in Playlist in the right-click menu, and the
  connected iPod's state with its Sync, Selection, Adopt and Eject
  buttons.
- **Adoption.** Takes over an iPod as it is: its tracks are matched to
  library files by size, tags, duration and, when needed, identical
  content. Nothing is copied again, and doubtful matches are shown for
  you to accept or reject.
- **Planned sync.** Copies new tracks, removes those gone from the
  library or the selection, updates changed tags and rebuilds playlists.
  The full plan, with the space it needs, is shown before anything is
  written.
- **Per-iPod selection.** Each iPod syncs the whole library, only some
  playlists, or the whole library except some playlists, so a smaller
  iPod can hold part of it.
- **Play statistics.** Play counts, skips, ratings and last-played dates
  are read from the iPod and kept in the index before every write, and
  exported to a CSV file.
- **Safety.** Before every write, the iPod database, the cover art
  database and the play counts are backed up. After writing, the
  database is read again and compared with the plan; any difference
  stops the sync. iPod tracks and playlists that iprune does not manage
  are left alone, as are smart playlists.
- **Repair.** `iprune backups` and `iprune restore` put an earlier
  database back. `iprune doctor` finds what a crash or an interrupted
  sync left behind, and `doctor --fix` repairs it.
- **Desktop.** A desktop entry for the app launcher and a notification
  when a sync ends.

## Supported devices

iprune has been used with two iPods, both formatted FAT32 (by Windows
iTunes):

- iPod Classic 7th generation, 160 GB
- iPod Video 5th generation (5.5G), 80 GB

Other iPods that libgpod supports may work but have not been tried.
iprune does not write to HFS+ (Mac-formatted) iPods. iPhones and MTP players are out
of scope.

An iPod freshly restored by iTunes may have an empty
`iPod_Control/Device/SysInfo` file. libgpod then cannot tell the model,
and iprune refuses to write when that would lose the cover art. A
`SysInfo` naming the model fixes it, for example for an 80 GB black iPod
Video:

```
ModelNumStr: xA450
FirewireGuid: 0x000A27XXXXXXXXXX
```

## Requirements

- Linux, with `udisksctl` for mounting and ejecting.
- libgpod 0.8.3 (`pacman -S libgpod` on Arch), found through
  `pkg-config libgpod-1.0`.
- GTK 4.12 or later.
- Rust stable, 1.92 or later.
- For the tests: `gcc`, and `ffmpeg` with libmp3lame, aac, alac and
  libopus.

It was developed on Arch Linux with Omarchy (Hyprland), and follows the
GTK light or dark theme.

## Install

```sh
packaging/install.sh            # installs iprune, iprune-gui and the desktop entry
packaging/install.sh --uninstall
```

## Getting started

```sh
iprune init --library ~/Music --playlists ~/Playlists
iprune scan
iprune adopt                    # once per iPod, with the iPod mounted
iprune sync --plan-only         # see what a sync would do
iprune sync                     # do it, after confirmation
```

Or open iprune from the app launcher and use the Adopt and Sync buttons.
`iprune scope` (or the Selection button) chooses what each iPod holds.

The configuration lives in `~/.config/iprune/config.toml`, the index,
statistics and backups in `~/.local/share/iprune/`.

## Documentation

- [`docs/design.md`](docs/design.md): what iprune does and why.
- [`docs/technical.md`](docs/technical.md): how it does it.

## Built with AI

iprune was written with Claude Code, Anthropic's AI coding assistant,
working from a specification and under the direction of its author. Every
change was tested and reviewed before being committed, and the tool has
been used on real iPods, but it has not had the scrutiny of a large user
base.

## Use at your own risk

iprune writes to your iPod's database. It backs the database up before
every write and checks the result, but it comes with no warranty of any
kind. Keep your own copy of the iPod (at least `iPod_Control/iTunes`)
and of your playlists before you try it.

## License

MIT; see [LICENSE](LICENSE). You are free to use, modify and share it.
