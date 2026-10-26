//! Writing edited tags to library files (Get Info, `iprune tag`). The only
//! code that writes under the library roots.
//!
//! The new file is built in memory by iprune's own tag writers, which
//! change only the frames or atoms of the edited fields and copy every
//! other byte: lofty rewrites whole tags, converting ID3v2.2 and 2.3 and
//! dropping what it cannot parse, so it is used only to read the result
//! back. The new file goes to a temporary file in the same folder, is read
//! back and checked, then renamed over the original, which is never opened
//! for writing.

mod chunks;
mod id3;
mod mp4;

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::Connection;

use crate::store::{self, FileRecord, TrackRow};
use crate::tags::{self, FileKind, Tags};
use crate::{Result, paths};

/// Lowest and highest year accepted: the reader takes a year of four
/// digits only, so a smaller one would never read back.
pub const MIN_YEAR: u32 = 1000;
pub const MAX_YEAR: u32 = 9999;
/// Highest track or disc number or total accepted.
pub const MAX_NUMBER: u32 = 999;

/// The fields an edit changes. `None` leaves a field alone; `Some(None)`
/// clears it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    pub title: Option<Option<String>>,
    pub artist: Option<Option<String>>,
    pub album: Option<Option<String>>,
    pub album_artist: Option<Option<String>>,
    pub composer: Option<Option<String>>,
    pub genre: Option<Option<String>>,
    pub year: Option<Option<u32>>,
    pub track_no: Option<Option<u32>>,
    pub track_total: Option<Option<u32>>,
    pub disc_no: Option<Option<u32>>,
    pub disc_total: Option<Option<u32>>,
    pub compilation: Option<bool>,
}

/// Trimmed, and `None` when nothing is left.
fn clean(v: &Option<Option<String>>) -> Option<Option<String>> {
    v.as_ref().map(|v| {
        v.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    })
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        *self == Changes::default()
    }

    /// Text trimmed, an empty text turned into a clear.
    pub fn cleaned(&self) -> Changes {
        Changes {
            title: clean(&self.title),
            artist: clean(&self.artist),
            album: clean(&self.album),
            album_artist: clean(&self.album_artist),
            composer: clean(&self.composer),
            genre: clean(&self.genre),
            ..self.clone()
        }
    }

    /// Why the edit cannot be written, if it cannot: a year or a number out
    /// of range.
    pub fn check(&self) -> std::result::Result<(), String> {
        if let Some(Some(y)) = self.year
            && !(MIN_YEAR..=MAX_YEAR).contains(&y)
        {
            return Err(format!("year {y} is not between {MIN_YEAR} and {MAX_YEAR}"));
        }
        for (name, v) in [
            ("track number", self.track_no),
            ("track total", self.track_total),
            ("disc number", self.disc_no),
            ("disc total", self.disc_total),
        ] {
            if let Some(Some(n)) = v
                && !(1..=MAX_NUMBER).contains(&n)
            {
                return Err(format!("{name} {n} is not between 1 and {MAX_NUMBER}"));
            }
        }
        Ok(())
    }

    /// Only the changes that differ from `current`: a field set to the
    /// value it has is not written, so its frames keep their bytes (a full
    /// date stays when the same year is given).
    pub fn without_same(&self, current: &Tags) -> Changes {
        fn keep<T: PartialEq + Clone>(v: &Option<T>, now: &T) -> Option<T> {
            v.as_ref().filter(|v| *v != now).cloned()
        }
        Changes {
            title: keep(&self.title, &current.title),
            artist: keep(&self.artist, &current.artist),
            album: keep(&self.album, &current.album),
            album_artist: keep(&self.album_artist, &current.album_artist),
            composer: keep(&self.composer, &current.composer),
            genre: keep(&self.genre, &current.genre),
            year: keep(&self.year, &current.year),
            track_no: keep(&self.track_no, &current.track_no),
            track_total: keep(&self.track_total, &current.track_total),
            disc_no: keep(&self.disc_no, &current.disc_no),
            disc_total: keep(&self.disc_total, &current.disc_total),
            compilation: keep(&self.compilation, &current.compilation),
        }
    }

    /// `tags` with the changes made.
    pub fn apply(&self, tags: &Tags) -> Tags {
        let mut t = tags.clone();
        let set = |dst: &mut Option<String>, v: &Option<Option<String>>| {
            if let Some(v) = v {
                dst.clone_from(v);
            }
        };
        set(&mut t.title, &self.title);
        set(&mut t.artist, &self.artist);
        set(&mut t.album, &self.album);
        set(&mut t.album_artist, &self.album_artist);
        set(&mut t.composer, &self.composer);
        set(&mut t.genre, &self.genre);
        for (dst, v) in [
            (&mut t.year, self.year),
            (&mut t.track_no, self.track_no),
            (&mut t.track_total, self.track_total),
            (&mut t.disc_no, self.disc_no),
            (&mut t.disc_total, self.disc_total),
        ] {
            if let Some(v) = v {
                *dst = v;
            }
        }
        if let Some(c) = self.compilation {
            t.compilation = c;
        }
        t
    }
}

/// One field to rewrite in a tag, with its new value (`None` removes it).
/// Track and disc numbers go with their totals, as tags store them.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Title(Option<String>),
    Artist(Option<String>),
    Album(Option<String>),
    AlbumArtist(Option<String>),
    Composer(Option<String>),
    Genre(Option<String>),
    Year(Option<u32>),
    Track(Option<u32>, Option<u32>),
    Disc(Option<u32>, Option<u32>),
    Compilation(bool),
}

/// The items to write for `changes`, whose result is `expected`. With
/// `all`, every field, for a new tag: the reader prefers it to any other
/// tag in the file, so it must hold the values the index has.
fn items(changes: &Changes, expected: &Tags, all: bool) -> Vec<Item> {
    let t = expected;
    let mut out = Vec::new();
    let mut text = |edited: bool, make: fn(Option<String>) -> Item, v: &Option<String>| {
        if all || edited {
            out.push(make(v.clone()));
        }
    };
    text(changes.title.is_some(), Item::Title, &t.title);
    text(changes.artist.is_some(), Item::Artist, &t.artist);
    text(changes.album.is_some(), Item::Album, &t.album);
    text(
        changes.album_artist.is_some(),
        Item::AlbumArtist,
        &t.album_artist,
    );
    text(changes.composer.is_some(), Item::Composer, &t.composer);
    text(changes.genre.is_some(), Item::Genre, &t.genre);
    if all || changes.year.is_some() {
        out.push(Item::Year(t.year));
    }
    if all || changes.track_no.is_some() || changes.track_total.is_some() {
        out.push(Item::Track(t.track_no, t.track_total));
    }
    if all || changes.disc_no.is_some() || changes.disc_total.is_some() {
        out.push(Item::Disc(t.disc_no, t.disc_total));
    }
    if all || changes.compilation.is_some() {
        out.push(Item::Compilation(t.compilation));
    }
    // A new tag holds only what has a value.
    if all {
        out.retain(|i| match i {
            Item::Title(v)
            | Item::Artist(v)
            | Item::Album(v)
            | Item::AlbumArtist(v)
            | Item::Composer(v)
            | Item::Genre(v) => v.is_some(),
            Item::Year(y) => y.is_some(),
            Item::Track(n, t) | Item::Disc(n, t) => n.is_some() || t.is_some(),
            Item::Compilation(c) => *c,
        });
    }
    out
}

/// What happened to one track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Written,
    /// The file already holds these values; not rewritten.
    Unchanged,
    Refused(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileResult {
    pub track_id: i64,
    /// Empty when the track is not in the index.
    pub path: PathBuf,
    pub outcome: Outcome,
}

/// Progress through an edit: `done` of `total` tracks, the next one at
/// `path`.
#[derive(Debug, Clone, Copy)]
pub struct Progress<'a> {
    pub done: usize,
    pub total: usize,
    pub path: &'a Path,
}

/// The tracks that went through an edit, in the order given, each once. A
/// stop leaves the rest out.
#[derive(Debug, Default)]
pub struct Report {
    pub results: Vec<FileResult>,
    pub stopped: bool,
    /// Set when the index could not be updated after a file was written:
    /// that file is reported written, the edit stops there, and a scan
    /// brings the index up to date.
    pub index_error: Option<String>,
}

impl Report {
    pub fn written(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.outcome == Outcome::Written)
            .count()
    }

    pub fn refused(&self) -> impl Iterator<Item = (&FileResult, &str)> {
        self.results.iter().filter_map(|r| match &r.outcome {
            Outcome::Refused(why) => Some((r, why.as_str())),
            _ => None,
        })
    }
}

/// Why `t` cannot be edited, if it cannot. Checked before any file is
/// opened, and by the window to show a track read-only.
pub fn refusal(t: &TrackRow) -> Option<String> {
    if t.missing_since.is_some() {
        return Some("the file is missing".into());
    }
    if t.drm {
        return Some("FairPlay protected (an iTunes Store purchase)".into());
    }
    if let Some(e) = &t.tag_error {
        return Some(format!(
            "its tags could not be read ({e}); rewriting them could lose what was skipped"
        ));
    }
    None
}

/// Writes `changes` to the files of `track_ids`, one at a time, updating
/// their rows. A track that cannot be written is reported and the others
/// still go ahead; `stop`, checked between files, ends it early. Errors are
/// those of reading the index; one updating it ends the edit with
/// `index_error` set, keeping the report.
pub fn edit(
    conn: &Connection,
    track_ids: &[i64],
    changes: &Changes,
    progress: &mut dyn FnMut(Progress<'_>),
    stop: &AtomicBool,
) -> Result<Report> {
    let changes = changes.cleaned();
    let mut ids: Vec<i64> = Vec::with_capacity(track_ids.len());
    for &id in track_ids {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    let mut report = Report::default();
    let invalid = changes.check().err();
    let total = ids.len();
    for (done, &id) in ids.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            report.stopped = true;
            break;
        }
        let Some(row) = store::track(conn, id)? else {
            report.results.push(FileResult {
                track_id: id,
                path: PathBuf::new(),
                outcome: Outcome::Refused("not in the library index".into()),
            });
            continue;
        };
        progress(Progress {
            done,
            total,
            path: &row.path,
        });
        let result = match &invalid {
            Some(why) => Err(why.clone()),
            None => edit_one(&row, &changes),
        };
        let outcome = match result {
            Ok(Some(rec)) => {
                if let Err(e) = store::update_track(conn, row.id, &rec) {
                    report.index_error = Some(format!(
                        "{e}; the files written are fine, run a scan to bring the index up to date"
                    ));
                }
                Outcome::Written
            }
            Ok(None) => Outcome::Unchanged,
            Err(why) => Outcome::Refused(why),
        };
        report.results.push(FileResult {
            track_id: id,
            path: row.path,
            outcome,
        });
        if report.index_error.is_some() {
            report.stopped = true;
            break;
        }
    }
    Ok(report)
}

/// Each field where `got` differs from `want`, said in words.
fn differences(got: &Tags, want: &Tags) -> Vec<String> {
    fn shown<T: std::fmt::Display>(v: &Option<T>) -> String {
        v.as_ref().map_or("nothing".into(), |v| format!("\"{v}\""))
    }
    let mut out = Vec::new();
    let mut text = |name: &str, g: &Option<String>, w: &Option<String>| {
        if g != w {
            out.push(format!("{name} reads {} instead of {}", shown(g), shown(w)));
        }
    };
    text("title", &got.title, &want.title);
    text("artist", &got.artist, &want.artist);
    text("album", &got.album, &want.album);
    text("album artist", &got.album_artist, &want.album_artist);
    text("composer", &got.composer, &want.composer);
    text("genre", &got.genre, &want.genre);
    for (name, g, w) in [
        ("year", got.year, want.year),
        ("track number", got.track_no, want.track_no),
        ("track total", got.track_total, want.track_total),
        ("disc number", got.disc_no, want.disc_no),
        ("disc total", got.disc_total, want.disc_total),
    ] {
        if g != w {
            out.push(format!(
                "{name} reads {} instead of {}",
                shown(&g),
                shown(&w)
            ));
        }
    }
    if got.compilation != want.compilation {
        out.push(format!(
            "compilation reads {} instead of {}",
            if got.compilation { "yes" } else { "no" },
            if want.compilation { "yes" } else { "no" }
        ));
    }
    out
}

/// The file's size and modification time.
fn stamp(path: &Path) -> std::io::Result<(u64, i64)> {
    let m = std::fs::metadata(path)?;
    Ok((m.size(), m.mtime() * 1_000_000_000 + m.mtime_nsec()))
}

/// The file `data` with `changes` written into its tag. `current` is what
/// the index read from it.
fn rewrite(
    data: &[u8],
    kind: FileKind,
    current: &Tags,
    changes: &Changes,
) -> std::result::Result<Vec<u8>, String> {
    let expected = changes.apply(current);
    match kind {
        FileKind::Aac | FileKind::Alac => mp4::rewrite(data, &items(changes, &expected, false)),
        FileKind::Mp3 => id3::rewrite_mp3(data, |all| items(changes, &expected, all)),
        FileKind::Wav | FileKind::Aiff => chunks::rewrite(data, |tag| {
            id3::rewrite_tag(tag, |all| items(changes, &expected, all))
        }),
    }
}

/// Writes `changes` to the file of `row`. `Ok(None)` when they change
/// nothing; otherwise what the index should now hold for it.
fn edit_one(row: &TrackRow, changes: &Changes) -> std::result::Result<Option<FileRecord>, String> {
    if let Some(why) = refusal(row) {
        return Err(why);
    }
    // The file is replaced by a rename, which needs only its folder: a
    // read-only file is refused here, as a tag editor would.
    match std::fs::metadata(&row.path) {
        Ok(m) if m.permissions().readonly() => return Err("the file is read-only".into()),
        _ => {}
    }
    let changes = &changes.without_same(&row.tags);
    if changes.is_empty() {
        return Ok(None);
    }
    let expected = changes.apply(&row.tags);
    let path = &row.path;
    let unchanged_since_scan = |what: &str| match stamp(path) {
        Ok(s) if s == (row.size, row.mtime_ns) => Ok(()),
        Ok(_) => Err(format!(
            "the file changed {what}; run a scan, then edit it again"
        )),
        Err(e) => Err(format!("cannot read the file: {e}")),
    };
    unchanged_since_scan("since the last scan")?;
    let data = std::fs::read(path).map_err(|e| format!("cannot read the file: {e}"))?;
    let new = rewrite(&data, row.info.kind, &row.tags, changes)?;
    drop(data);

    let dir = path
        .parent()
        .ok_or_else(|| "the file has no folder".to_string())?;
    // Hidden, so a scan skips it, and with the file's extension, from
    // which the reader takes the format before looking at the content.
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let mut tmp = tempfile::Builder::new()
        .prefix(".iprune-")
        .suffix(&format!(".tmp{ext}"))
        .tempfile_in(dir)
        .map_err(|e| format!("cannot create a file in {}: {e}", dir.display()))?;
    let perms = std::fs::metadata(path)
        .map_err(|e| format!("cannot read the file: {e}"))?
        .permissions();
    tmp.write_all(&new)
        .and_then(|()| tmp.as_file().set_permissions(perms))
        .and_then(|()| tmp.as_file().sync_all())
        .map_err(|e| format!("cannot write the new file: {e}"))?;
    let hash = crate::scan::hash_reader(&new[..]).expect("hashing memory cannot fail");
    drop(new);

    let back = tags::read(tmp.path()).map_err(|e| format!("the new file cannot be read: {e:?}"))?;
    let mismatch = if let Some(e) = &back.tag_error {
        Some(format!("its tags cannot be parsed ({e})"))
    } else if back.tags != expected {
        Some(differences(&back.tags, &expected).join(", "))
    } else if back.info.kind != row.info.kind || back.info.duration_ms != row.info.duration_ms {
        Some("the audio no longer reads the same".into())
    } else {
        None
    };
    if let Some(m) = mismatch {
        return Err(format!(
            "the written tags did not check out, file left as it was: {m}"
        ));
    }

    // Taken now: a rename keeps them, and nothing after it may fail, or
    // the index would be left behind a file already replaced.
    let (size, mtime_ns) =
        stamp(tmp.path()).map_err(|e| format!("cannot read the new file: {e}"))?;
    unchanged_since_scan("while it was being edited")?;
    tmp.persist(path)
        .map_err(|e| format!("cannot replace the file: {}", e.error))?;
    // The rename is done; syncing the folder only makes it durable sooner.
    let _ = File::open(dir).and_then(|d| d.sync_all());
    Ok(Some(FileRecord {
        path: path.clone(),
        path_key: paths::key(path).unwrap_or_else(|| row.path_key.clone()),
        size,
        mtime_ns,
        hash,
        info: back.info,
        tags: back.tags,
        tag_error: None,
        drm: back.drm,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleaned_trims_and_turns_empty_into_clear() {
        let c = Changes {
            artist: Some(Some("  a-ha ".into())),
            album: Some(Some("   ".into())),
            genre: Some(None),
            ..Changes::default()
        }
        .cleaned();
        assert_eq!(c.artist, Some(Some("a-ha".into())));
        assert_eq!(c.album, Some(None));
        assert_eq!(c.genre, Some(None));
        assert_eq!(c.title, None);
    }

    #[test]
    fn check_rejects_out_of_range_values() {
        let ok = Changes {
            year: Some(Some(1985)),
            track_no: Some(Some(999)),
            disc_total: Some(None),
            ..Changes::default()
        };
        assert_eq!(ok.check(), Ok(()));
        for bad in [
            Changes {
                year: Some(Some(0)),
                ..Changes::default()
            },
            Changes {
                year: Some(Some(10_000)),
                ..Changes::default()
            },
            Changes {
                track_total: Some(Some(1000)),
                ..Changes::default()
            },
            Changes {
                disc_no: Some(Some(0)),
                ..Changes::default()
            },
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn apply_changes_only_the_listed_fields() {
        let before = Tags {
            title: Some("Take On Me".into()),
            artist: Some("A-Ha".into()),
            year: Some(1984),
            track_no: Some(1),
            track_total: Some(10),
            ..Tags::default()
        };
        let after = Changes {
            artist: Some(Some("a-ha".into())),
            year: Some(None),
            track_total: Some(Some(11)),
            compilation: Some(true),
            ..Changes::default()
        }
        .apply(&before);
        assert_eq!(after.title.as_deref(), Some("Take On Me"));
        assert_eq!(after.artist.as_deref(), Some("a-ha"));
        assert_eq!(after.year, None);
        assert_eq!((after.track_no, after.track_total), (Some(1), Some(11)));
        assert!(after.compilation);
    }

    #[test]
    fn items_cover_the_edited_fields_or_every_value() {
        let tags = Tags {
            title: Some("T".into()),
            track_no: Some(2),
            track_total: Some(9),
            ..Tags::default()
        };
        let c = Changes {
            track_total: Some(Some(9)),
            genre: Some(None),
            ..Changes::default()
        };
        assert_eq!(
            items(&c, &tags, false),
            vec![Item::Genre(None), Item::Track(Some(2), Some(9))]
        );
        assert_eq!(
            items(&c, &tags, true),
            vec![Item::Title(Some("T".into())), Item::Track(Some(2), Some(9))]
        );
    }
}
