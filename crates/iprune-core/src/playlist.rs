//! The `.m3u8` file format.
//!
//! Reading keeps every location line verbatim, with the `#EXTINF` line just
//! before it, so an entry iprune cannot resolve is written back unchanged.
//! Other `#` lines (`#PLAYLIST`, `#EXTGRP`, comments) are kept with the entry
//! they come before, or after the last one, and written back as they were.
//! Blank lines and the `#EXTM3U` header are not kept; the header is always
//! written. Lines may end with LF, CRLF or a lone CR.

use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use crate::error::IoContext;
use crate::{Error, Result};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    /// The location line exactly as in the file.
    pub location: String,
    /// The `#EXTINF` line that preceded it, if any.
    pub extinf: Option<String>,
    /// Other `#` lines just before it, in order.
    pub comments: Vec<String>,
    /// `#` lines after it, for the last entry of the file.
    pub trailing: Vec<String>,
}

pub fn parse(text: &str) -> Vec<Entry> {
    parse_all(text).0
}

/// The entries, and the `#` lines of a file that has no entry to keep them
/// with.
pub fn parse_all(text: &str) -> (Vec<Entry>, Vec<String>) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut entries: Vec<Entry> = Vec::new();
    // The pending `#EXTINF`, and where it stood among the lines since the
    // last location, should it turn out to have none.
    let mut extinf: Option<(String, usize)> = None;
    let mut comments: Vec<String> = Vec::new();
    // LF, CRLF (its empty piece is skipped as blank) and a lone CR.
    for line in text.split(['\n', '\r']) {
        if line.trim().is_empty() || line.starts_with("#EXTM3U") {
            continue;
        }
        if line.starts_with("#EXTINF:") {
            // One with no location after it is kept as a plain line, where
            // it stood.
            if let Some((x, at)) = extinf.take() {
                comments.insert(at, x);
            }
            extinf = Some((line.to_owned(), comments.len()));
        } else if line.starts_with('#') {
            // A directive between an `#EXTINF` and its location (VLC's
            // `#EXTVLCOPT`) leaves the `#EXTINF` with that location.
            comments.push(line.to_owned());
        } else {
            entries.push(Entry {
                location: line.to_owned(),
                extinf: extinf.take().map(|(x, _)| x),
                comments: std::mem::take(&mut comments),
                trailing: Vec::new(),
            });
        }
    }
    if let Some((x, at)) = extinf {
        comments.insert(at, x);
    }
    match entries.last_mut() {
        Some(last) => {
            last.trailing = comments;
            (entries, Vec::new())
        }
        None => (entries, comments),
    }
}

pub fn render(entries: &[Entry]) -> String {
    render_all(entries, &[])
}

/// [`render`], with `loose` lines, those of a file with no entry, after the
/// header.
pub fn render_all(entries: &[Entry], loose: &[String]) -> String {
    let mut out = String::from("#EXTM3U\n");
    for l in loose {
        out.push_str(l);
        out.push('\n');
    }
    for e in entries {
        for c in &e.comments {
            out.push_str(c);
            out.push('\n');
        }
        if let Some(x) = &e.extinf {
            out.push_str(x);
            out.push('\n');
        }
        out.push_str(&e.location);
        out.push('\n');
        for c in &e.trailing {
            out.push_str(c);
            out.push('\n');
        }
    }
    out
}

/// `#EXTINF:<seconds>,<artist> - <title>`, with -1 for an unknown duration.
pub fn extinf(duration_ms: Option<u32>, artist: Option<&str>, title: &str) -> String {
    let secs = duration_ms.map_or(-1, |ms| i64::from(ms.saturating_add(500) / 1000));
    let one_line = |s: &str| s.replace(['\n', '\r'], " ");
    match artist {
        Some(a) => format!("#EXTINF:{secs},{} - {}", one_line(a), one_line(title)),
        None => format!("#EXTINF:{secs},{}", one_line(title)),
    }
}

pub fn read(path: &Path) -> Result<Vec<Entry>> {
    Ok(read_all(path)?.0)
}

/// [`read`] with the loose lines of [`parse_all`].
pub fn read_all(path: &Path) -> Result<(Vec<Entry>, Vec<String>)> {
    let bytes = std::fs::read(path).at(path)?;
    let text = String::from_utf8(bytes).map_err(|_| Error::NotUtf8(path.to_owned()))?;
    Ok(parse_all(&text))
}

/// How [`write_atomic`] may treat an existing file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// The file must not exist yet.
    CreateNew,
    /// Replace the file, which must still have this mtime (ns): it was read at
    /// that time, and a newer version was saved by someone else.
    Replace { mtime_ns: i64 },
}

pub fn mtime_ns(path: &Path) -> Result<i64> {
    let m = std::fs::metadata(path).at(path)?;
    Ok(m.mtime() * 1_000_000_000 + m.mtime_nsec())
}

/// Writes `contents` to a temporary file next to `path`, syncs it and renames
/// it into place, so a crash leaves either the old file or the new one.
pub fn write_atomic(path: &Path, contents: &str, mode: WriteMode) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let perm = std::fs::metadata(path).map_or(0o644, |m| m.permissions().mode() & 0o7777);
    let mut tmp = tempfile::Builder::new()
        .prefix(".iprune-")
        .suffix(".tmp")
        .tempfile_in(dir)
        .at(dir)?;
    tmp.write_all(contents.as_bytes()).at(tmp.path())?;
    tmp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(perm))
        .at(tmp.path())?;
    tmp.as_file().sync_all().at(tmp.path())?;
    let persisted = match mode {
        WriteMode::CreateNew => tmp.persist_noclobber(path),
        WriteMode::Replace { mtime_ns: expected } => {
            if mtime_ns(path)? != expected {
                return Err(Error::ChangedOnDisk(path.to_owned()));
            }
            tmp.persist(path)
        }
    };
    persisted.map_err(|e| Error::Io {
        path: path.to_owned(),
        source: e.error,
    })?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// `path` relative to `base`, both absolute and normalized.
pub fn relative_to(path: &Path, base: &Path) -> PathBuf {
    let p: Vec<Component> = path.components().collect();
    let b: Vec<Component> = base.components().collect();
    let common = p.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out = PathBuf::new();
    for _ in common..b.len() {
        out.push("..");
    }
    for c in &p[common..] {
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keeps_locations_verbatim_and_pairs_extinf() {
        let text = "\u{feff}#EXTM3U\r\n#EXTINF:513,A - B\r\n/m/a.mp3\r\n\n#PLAYLIST:x\nrel/b.mp3\n#EXTINF:1,C\nhttp://x y\n";
        let e = parse(text);
        assert_eq!(e.len(), 3);
        assert_eq!(
            e[0],
            Entry {
                location: "/m/a.mp3".into(),
                extinf: Some("#EXTINF:513,A - B".into()),
                ..Default::default()
            }
        );
        assert_eq!(
            e[1],
            Entry {
                location: "rel/b.mp3".into(),
                extinf: None,
                comments: vec!["#PLAYLIST:x".into()],
                trailing: Vec::new(),
            }
        );
        assert_eq!(e[2].location, "http://x y");
    }

    #[test]
    fn render_parse_round_trip() {
        let e = vec![
            Entry {
                location: "/m/a.mp3".into(),
                extinf: Some(extinf(Some(512_600), Some("A"), "B")),
                ..Default::default()
            },
            Entry {
                location: "  odd entry ".into(),
                extinf: None,
                ..Default::default()
            },
        ];
        let text = render(&e);
        assert_eq!(text, "#EXTM3U\n#EXTINF:513,A - B\n/m/a.mp3\n  odd entry \n");
        assert_eq!(parse(&text), e);
    }

    #[test]
    fn extinf_formats() {
        assert_eq!(extinf(None, None, "T"), "#EXTINF:-1,T");
        assert_eq!(extinf(Some(499), Some("a\nb"), "t\r"), "#EXTINF:0,a b - t ");
    }

    #[test]
    fn write_atomic_replaces_and_keeps_mode() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.m3u8");
        std::fs::write(&p, "old").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o640)).unwrap();
        let t = mtime_ns(&p).unwrap();
        write_atomic(&p, "new", WriteMode::Replace { mtime_ns: t }).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temp file left behind"
        );
    }

    #[test]
    fn write_atomic_refuses_to_clobber_or_overwrite_newer_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.m3u8");
        std::fs::write(&p, "theirs").unwrap();
        let err = write_atomic(&p, "mine", WriteMode::CreateNew).unwrap_err();
        assert!(
            matches!(&err, Error::Io { source, .. } if source.kind() == std::io::ErrorKind::AlreadyExists),
            "{err}"
        );
        let stale = mtime_ns(&p).unwrap() - 1;
        let err = write_atomic(&p, "mine", WriteMode::Replace { mtime_ns: stale }).unwrap_err();
        assert!(matches!(err, Error::ChangedOnDisk(_)), "{err}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "theirs");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temp file left behind"
        );
    }

    #[test]
    fn relative_paths() {
        let base = Path::new("/mnt/x/Playlists");
        assert_eq!(
            relative_to(Path::new("/mnt/x/Music/a.mp3"), base),
            Path::new("../Music/a.mp3")
        );
        assert_eq!(
            relative_to(Path::new("/mnt/x/Playlists/a.mp3"), base),
            Path::new("a.mp3")
        );
    }
}
