//! A stable text dump of a database, for diffing before and after a write.
//!
//! The output is byte-identical to the C `snapshot` tool used to repair the
//! iPod on 2026-10-12 (`tests/c/snapshot.c`), so old and new snapshots can be
//! compared directly.

use std::borrow::Cow;
use std::io::{self, Write};

use crate::Database;

/// glibc prints NULL `%s` arguments as `(null)`.
fn s(v: Option<Cow<'_, str>>) -> Cow<'_, str> {
    v.unwrap_or(Cow::Borrowed("(null)"))
}

pub fn write(db: &Database, out: &mut impl Write) -> io::Result<()> {
    writeln!(
        out,
        "tracks {} playlists {}",
        db.track_count(),
        db.playlist_count()
    )?;
    for t in db.tracks() {
        writeln!(
            out,
            "T {:016x} {} | {} | {} | {} | {} | art {} mhii {} | rating {} plays {}",
            t.dbid(),
            s(t.ipod_path()),
            s(t.artist()),
            s(t.album()),
            s(t.title()),
            // The C tool prints the guint32 size with %d.
            t.size() as i32,
            t.has_thumbnails() as i32,
            t.mhii_link(),
            t.rating(),
            t.playcount(),
        )?;
    }
    for pl in db.playlists() {
        writeln!(
            out,
            "P {}{}{} {}",
            s(pl.name()),
            if pl.is_master() { " [master]" } else { "" },
            if pl.is_smart() { " [smart]" } else { "" },
            pl.len(),
        )?;
        if pl.is_master() || pl.is_smart() {
            continue;
        }
        for m in pl.members() {
            writeln!(out, "  {:016x}", m.dbid())?;
        }
    }
    Ok(())
}
