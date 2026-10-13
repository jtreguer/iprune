# CLAUDE.md

iprune is a Linux terminal tool that manages `.m3u8` playlists for a local
music library and syncs tracks and playlists one way, library to device, to an
iPod Classic through libgpod. It does not play music or edit tags.

Read `docs/design.md` (what and why) and `docs/technical.md` (how) before
changing behaviour. They are the spec. If code and docs disagree, ask which one
is right rather than silently picking one.

## Status

M0 and M1 done (bindings, read-only `gpod` wrapper, `iprune snapshot`; config,
scan, SQLite index, `.m3u8` editing, `playlist` commands). Work follows the milestones in `docs/technical.md` section 17:

| # | Deliverable | Writes to iPod |
|---|---|---|
| M0 | `gpod-sys`, `gpod`, `iprune snapshot` byte-identical to the C tool | no |
| M1 | config, scan, SQLite, `.m3u8` read and write, `playlist` commands | no |
| M2 | `adopt --file` on the backup, then on the device | no |
| M3 | planner and `sync --plan-only` | no |
| M4 | executor for playlists only, backups, checks, verify, restore | yes |
| M5 | track copy, remove, tag update, stats pull, `doctor` | yes |
| M6 | terminal UI | yes |
| M7 | desktop entry, notifications, packaging | yes |

Do not start work that writes to an iPod before the earlier milestones are done
and tested.

## Decisions

- Target device: iPod Classic 7th generation, 160 GB, FAT32. No HFS+ support.
- No integration with Rhythmbox, Strawberry or any player. iprune owns the
  `.m3u8` folder.
- No cover art work of any kind. Leave existing iPod artwork alone.
- Play statistics come from the iPod only; no iTunes XML import.

## Stack

Rust stable, edition 2024, Cargo workspace. libgpod 0.8.3 (Arch `extra`) via
FFI with `bindgen` at build time and `glib-sys` for GLib types. `lofty` for
tags, `rusqlite` (bundled), `walkdir` + `rayon`, `blake3`, `clap` (derive),
`ratatui` + `crossterm`, `nucleo-matcher`, `serde` + `toml`, `directories`,
`anyhow` in the binary, `thiserror` in libraries, `tracing`. Mount and eject
through `udisksctl`.

## Layout

```
crates/gpod-sys/     raw bindgen output, links libgpod-1.0 via pkg-config
crates/gpod/         safe wrapper; the only crate allowed to contain unsafe
crates/iprune-core/  config, store, scan, playlist (.m3u8 format), playlists (operations);
                     later device, adopt, plan, exec, verify, stats
crates/iprune/       binary: CLI and TUI
crates/gpod/tests/c/ snapshot.c (copy of the C repair tool) and make_fixture.c
```

## Commands

```
cargo build
cargo test                                  # needs gcc (C snapshot tool) and ffmpeg with
                                            # libmp3lame, aac, alac, libopus (audio fixtures)
cargo test -p gpod -- --include-ignored     # also compares against the real backup, read only
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Requires `libgpod` installed (`pacman -S libgpod`) and `pkg-config libgpod-1.0`
to resolve.

## Review and tests before every commit

No code is committed or pushed until both of these are done, for every
change, however small:

1. **Tests by Aldo.** Launch the `aldo-test-specialist` agent on the change. It
   writes or extends the tests, shows them failing against broken code, and
   passing against the change.
2. **Review by Vera.** Launch the `vera` agent on the diff (code and Aldo's
   tests). Fix what she finds, or record why a finding was rejected, then have
   her check the fixes.

`cargo test`, `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`
must pass at the end. Mention in the commit message that both steps ran.

## Rules that must hold

- **Never write to a real iPod** from code, tests or experiments unless the user
  explicitly asks in that moment. Use `itdb_parse_file` / `itdb_write_file` on
  copies, or a fake iPod made with `itdb_init_ipod` in a temp dir.
- **Never modify `/mnt/seagate-1tb/iPod-backup-2026-10`.** It is the only
  backup of the repaired iPod. Read from it, or copy it to a temp dir first.
- **Never modify files under `/mnt/seagate-1tb/Music` or
  `/mnt/seagate-1tb/Playlists`** during development. Tests use fixtures or temp
  copies.
- `plan.rs` is pure: no I/O, no libgpod. Same input gives the same plan in the
  same order.
- Playlist membership is only ever set as a whole (`set_playlist_members`
  clears then adds in order). No append operation in the public API. This
  prevents the playlist doubling bug Rhythmbox caused.
- Only touch iPod tracks and playlists iprune manages. Unmanaged ones are
  reported and kept unless the user asks for removal.
- Never edit the master playlist or smart playlists.
- Pull play statistics and commit them to SQLite before any iPod write
  (libgpod resets recent counts on write).
- Back up `iTunesDB`, `ArtworkDB` and `Play Counts` before every write. Run the
  pre-write checks, then re-parse and compare the snapshot after writing. Any
  difference stops the sync and exits with code 3.
- An empty library tag never overwrites a non-empty iPod value.
- `.m3u8` writes go to a temp file in the same folder, `fsync`, then rename.
  Unresolved entries are kept verbatim.
- Paths are stored exactly as on disk and looked up by their NFC form
  (`path_key`). At least one library file has an NFD name.
- A scan never marks tracks missing when a root is absent, a folder failed to
  read, or the walk found nothing: an unmounted disk is not an empty library.

## libgpod wrapper rules

- `Database` is neither `Send` nor `Sync`; all libgpod calls run on one thread.
- `NewTrack` owns its `Itdb_Track` and frees it if dropped before
  `add_track`. After `itdb_track_add` the database owns it.
- `remove_track` removes the track from every playlist before
  `itdb_track_remove`.
- String setters `g_free` the old value and `g_strdup` the new one. Getters
  copy into `String` with lossy UTF-8.
- Every `GError` is converted to `gpod::Error` and freed.
- Record a `dbid` mapping only after `itdb_write` succeeds.

## Testing

- Unit: planner, playlist parsing, adoption scoring, tag diffing, table driven.
- Bindings (M0): `gpod::snapshot` output must match the C `snapshot` tool byte
  for byte, on a fixture built by `make_fixture.c` (always) and on the backup
  `iTunesDB` (ignored test). Do not change the snapshot format.
- Integration: full sync against a fake iPod from `itdb_init_ipod` with a
  `SysInfo` containing a `FirewireGuid`; audio fixtures generated with ffmpeg.
- Regression: syncing the same plan twice must give an empty second plan and an
  unchanged snapshot. Also a `proptest` version of this with random edits.

## CLI exit codes

0 success, 1 error, 2 usage, 3 verification failed, 4 user declined the plan.
