//! The Import view's rows (design sections 7.6 and 8, technical document
//! section 7.2): what each inbox file shows in the grid and in the pane
//! under it, worked out from `iprune_core::import::view`. Plain and tested;
//! `import_view` draws it.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use iprune_core::import::{By, Entry, Inbox, Index, Status, View, view};
use iprune_core::tags::{AudioInfo, Tags};

use iprune_core::adopt::normalize;

use super::{Query, duration};

/// One candidate as the pane lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateLine {
    /// "a-ha – Take On Me [3:46]", then the release on a second line.
    pub label: String,
    /// The 10-second rule holds for it.
    pub certain: bool,
}

/// One inbox file.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportRow {
    /// Its row in `import_items`; none for a file that cannot be imported.
    pub item: Option<i64>,
    pub path: PathBuf,
    /// Relative to the inbox.
    pub file: String,
    pub status: Status,
    /// The tags it has now, and the ones it would get once filed.
    pub before: Option<Tags>,
    pub after: Option<Tags>,
    pub info: Option<AudioInfo>,
    /// Relative to the library root.
    pub destination: Option<String>,
    pub candidates: Vec<CandidateLine>,
    /// The candidate it takes.
    pub candidate: Option<usize>,
    /// How the lookup found the song, and the artist and title it searched.
    pub by: Option<By>,
    pub query: Option<(String, String)>,
    /// Library songs it may be, as paths; autotag's same-title files are
    /// relative to the library root.
    pub duplicates: Vec<(Option<i64>, String)>,
    /// Tags were typed for it.
    pub typed: bool,
    /// A possible duplicate the user accepted.
    pub confirmed: bool,
    /// The last lookup or filing error.
    pub error: Option<String>,
}

/// The grid's columns, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImportColumn {
    Status,
    File,
    Name,
    Artist,
    Album,
    AlbumArtist,
    Genre,
    Year,
    Track,
    Disc,
    Time,
    Destination,
}

impl ImportColumn {
    pub const ALL: [ImportColumn; 12] = [
        ImportColumn::Status,
        ImportColumn::File,
        ImportColumn::Name,
        ImportColumn::Artist,
        ImportColumn::Album,
        ImportColumn::AlbumArtist,
        ImportColumn::Genre,
        ImportColumn::Year,
        ImportColumn::Track,
        ImportColumn::Disc,
        ImportColumn::Time,
        ImportColumn::Destination,
    ];

    pub fn title(self) -> &'static str {
        match self {
            ImportColumn::Status => "",
            ImportColumn::File => "File",
            ImportColumn::Name => "Name",
            ImportColumn::Artist => "Artist",
            ImportColumn::Album => "Album",
            ImportColumn::AlbumArtist => "Album Artist",
            ImportColumn::Genre => "Genre",
            ImportColumn::Year => "Year",
            ImportColumn::Track => "Track",
            ImportColumn::Disc => "Disc",
            ImportColumn::Time => "Time",
            ImportColumn::Destination => "Destination",
        }
    }

    pub fn width(self) -> i32 {
        match self {
            ImportColumn::Status => 32,
            ImportColumn::Year | ImportColumn::Track | ImportColumn::Disc => 56,
            ImportColumn::Time => 60,
            ImportColumn::Genre => 110,
            ImportColumn::File | ImportColumn::Destination => 240,
            _ => 170,
        }
    }

    pub fn numeric(self) -> bool {
        matches!(
            self,
            ImportColumn::Year | ImportColumn::Track | ImportColumn::Disc | ImportColumn::Time
        )
    }

    /// A tag column's value in `t`.
    fn tag_text(self, t: &Tags) -> String {
        let pair = |n: Option<u32>, total: Option<u32>| match (n, total) {
            (Some(n), Some(t)) => format!("{n} of {t}"),
            (Some(n), None) => n.to_string(),
            (None, Some(t)) => format!("of {t}"),
            (None, None) => String::new(),
        };
        let text = |v: &Option<String>| v.clone().unwrap_or_default();
        match self {
            ImportColumn::Name => text(&t.title),
            ImportColumn::Artist => text(&t.artist),
            ImportColumn::Album => text(&t.album),
            ImportColumn::AlbumArtist => text(&t.album_artist),
            ImportColumn::Genre => text(&t.genre),
            ImportColumn::Year => t.year.map(|y| y.to_string()).unwrap_or_default(),
            ImportColumn::Track => pair(t.track_no, t.track_total),
            ImportColumn::Disc => pair(t.disc_no, t.disc_total),
            _ => String::new(),
        }
    }

    fn is_tag(self) -> bool {
        !matches!(
            self,
            ImportColumn::Status
                | ImportColumn::File
                | ImportColumn::Time
                | ImportColumn::Destination
        )
    }

    /// What the cell shows: for a tag, the value the file would get once
    /// filed, else the one it has.
    pub fn text(self, r: &ImportRow) -> String {
        match self {
            ImportColumn::Status => status_symbol(&r.status).to_owned(),
            ImportColumn::File => r.file.clone(),
            ImportColumn::Time => r
                .info
                .as_ref()
                .and_then(|i| i.duration_ms)
                .map(duration)
                .unwrap_or_default(),
            ImportColumn::Destination => r.destination.clone().unwrap_or_default(),
            c => r
                .after
                .as_ref()
                .or(r.before.as_ref())
                .map(|t| c.tag_text(t))
                .unwrap_or_default(),
        }
    }

    /// For a tag the file would get changed: the value it has now, "(empty)"
    /// when none. Shown bold, with this in its tooltip.
    pub fn was(self, r: &ImportRow) -> Option<String> {
        if !self.is_tag() {
            return None;
        }
        let (Some(before), Some(after)) = (&r.before, &r.after) else {
            return None;
        };
        let old = self.tag_text(before);
        (old != self.tag_text(after)).then(|| {
            if old.is_empty() {
                "(empty)".to_owned()
            } else {
                old
            }
        })
    }

    /// The tooltip of a cell.
    pub fn tooltip(self, r: &ImportRow) -> Option<String> {
        match self {
            ImportColumn::Status => Some(status_words(&r.status).to_owned()),
            _ => self.was(r).map(|w| format!("Now: {w}")),
        }
    }

    /// The grid's order: by status first (the files waiting for the user at
    /// the top), then by file; clicking a header sorts by its text.
    pub fn compare(self, a: &ImportRow, b: &ImportRow) -> Ordering {
        match self {
            ImportColumn::Status => rank(&a.status)
                .cmp(&rank(&b.status))
                .then_with(|| a.path.cmp(&b.path)),
            ImportColumn::Time => a
                .info
                .as_ref()
                .and_then(|i| i.duration_ms)
                .cmp(&b.info.as_ref().and_then(|i| i.duration_ms))
                .then_with(|| a.path.cmp(&b.path)),
            c if c.numeric() => lead(&c.text(a))
                .cmp(&lead(&c.text(b)))
                .then_with(|| a.path.cmp(&b.path)),
            c => normalize(Some(&c.text(a)))
                .cmp(&normalize(Some(&c.text(b))))
                .then_with(|| a.path.cmp(&b.path)),
        }
    }
}

/// The number a cell starts with: "3 of 10" sorts by 3.
fn lead(s: &str) -> Option<u32> {
    s.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// The grid's symbol for a status.
pub fn status_symbol(s: &Status) -> &'static str {
    match s {
        Status::Ready => "✓",
        Status::Choose => "?",
        Status::NoMatch => "✗",
        Status::AlreadyInLibrary(_) => "≡",
        Status::PossibleDuplicate => "≈",
        Status::LookingUp => "⋯",
        Status::Error(_) => "!",
        Status::NotSupported(_) => "–",
    }
}

/// A status in a few words.
pub fn status_words(s: &Status) -> &'static str {
    match s {
        Status::Ready => "Ready",
        Status::Choose => "Choose a match",
        Status::NoMatch => "No match",
        Status::AlreadyInLibrary(_) => "Already in the library",
        Status::PossibleDuplicate => "Possible duplicate",
        Status::LookingUp => "Looking up",
        Status::Error(_) => "Error",
        Status::NotSupported(_) => "Not supported",
    }
}

/// The files waiting for the user first, the ready ones last.
fn rank(s: &Status) -> u8 {
    match s {
        Status::Choose => 0,
        Status::NoMatch => 1,
        Status::PossibleDuplicate => 2,
        Status::Error(_) => 3,
        Status::AlreadyInLibrary(_) => 4,
        Status::NotSupported(_) => 5,
        Status::LookingUp => 6,
        Status::Ready => 7,
    }
}

impl ImportRow {
    pub fn new(entry: &Entry, v: &View, index: &Index, inbox: &Path) -> ImportRow {
        let item = entry.item.as_ref();
        let file = entry.file.as_ref().ok();
        let mut duplicates: Vec<(Option<i64>, String)> = v
            .duplicates
            .iter()
            .map(|&t| {
                let path = index
                    .path(t)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                (Some(t), path)
            })
            .collect();
        duplicates.extend(v.same_title.iter().map(|s| (None, s.clone())));
        let candidates = v
            .lookup
            .as_ref()
            .map(|l| {
                l.candidates
                    .iter()
                    .map(|c| CandidateLine {
                        label: c.label.clone(),
                        certain: c.certain,
                    })
                    .collect()
            })
            .unwrap_or_default();
        ImportRow {
            item: item.map(|i| i.id),
            path: entry.path.clone(),
            file: entry
                .path
                .strip_prefix(inbox)
                .unwrap_or(&entry.path)
                .display()
                .to_string(),
            status: v.status.clone(),
            before: file.map(|f| f.tags.clone()),
            after: v.tags.clone(),
            info: file.map(|f| f.info.clone()),
            destination: v.destination.clone(),
            candidates,
            candidate: v.candidate,
            by: v.lookup.as_ref().map(|l| l.by),
            query: v
                .lookup
                .as_ref()
                .map(|l| (l.query.artist.clone(), l.query.title.clone()))
                .or_else(|| {
                    item.and_then(|i| i.search.as_ref())
                        .map(|q| (q.artist.clone(), q.title.clone()))
                }),
            duplicates,
            typed: item.is_some_and(|i| i.tags.is_some()),
            confirmed: item.is_some_and(|i| i.confirmed),
            error: v.error.clone(),
        }
    }

    pub fn ready(&self) -> bool {
        self.status == Status::Ready
    }

    /// Kept by the window's search: the file name, the name, artist and
    /// album it would get.
    pub fn matches(&self, query: &Query) -> bool {
        let tags = self.after.as_ref().or(self.before.as_ref());
        let fields = [
            Some(self.file.as_str()),
            tags.and_then(|t| t.title.as_deref()),
            tags.and_then(|t| t.artist.as_deref()),
            tags.and_then(|t| t.album.as_deref()),
        ];
        let folded: String = fields
            .iter()
            .flatten()
            .map(|s| normalize(Some(s)))
            .collect::<Vec<_>>()
            .join(" ");
        query.words.iter().all(|w| folded.contains(w.as_str()))
    }

    /// The pane's first line: the file, how it was found and its length.
    pub fn heading(&self) -> String {
        let length = self.info.as_ref().and_then(|i| i.duration_ms).map(duration);
        let found = match self.by {
            Some(By::Fingerprint) => Some("found by fingerprint"),
            Some(By::Name) => Some("found by name"),
            None => None,
        };
        let tail: Vec<&str> = [found, length.as_deref()].into_iter().flatten().collect();
        if tail.is_empty() {
            self.file.clone()
        } else {
            format!("{}    {}", self.file, tail.join(", "))
        }
    }

    /// What the pane says about the file's state, in a sentence or two.
    pub fn explanation(&self) -> String {
        let mut out = match &self.status {
            Status::Ready if self.typed => "Ready, with the tags typed.".to_owned(),
            Status::Ready if self.confirmed => {
                "Ready: accepted although it may already be in the library.".to_owned()
            }
            Status::Ready => "Ready to import.".to_owned(),
            Status::Choose => {
                "No certain match: choose one, search again, or type its tags.".to_owned()
            }
            Status::NoMatch => {
                "No match on MusicBrainz: search again with another artist or title, or \
                 type its tags."
                    .to_owned()
            }
            Status::AlreadyInLibrary(_) => {
                "The same file is already in the library; it is not imported. Delete \
                 moves it to the trash."
                    .to_owned()
            }
            Status::PossibleDuplicate => {
                "It may already be in the library. Tick Import anyway to import it all \
                 the same."
                    .to_owned()
            }
            Status::LookingUp => "Waiting to be looked up.".to_owned(),
            Status::Error(e) => e.clone(),
            Status::NotSupported(why) => format!("Not supported: {why}."),
        };
        if let Some(e) = &self.error
            && !matches!(self.status, Status::Error(_))
        {
            out.push_str(&format!("\nLast try: {e}"));
        }
        out
    }
}

/// The Import view's rows for an inbox, in the grid's default order.
pub fn rows(inbox: &Inbox, index: &Index) -> Vec<ImportRow> {
    let mut out: Vec<ImportRow> = inbox
        .entries
        .iter()
        .map(|e| ImportRow::new(e, &view(e, index), index, &inbox.dir))
        .collect();
    out.sort_by(|a, b| ImportColumn::Status.compare(a, b));
    out
}

/// The Import button's label.
pub fn import_label(ready: usize) -> String {
    match ready {
        0 => "Nothing Ready to Import".to_owned(),
        1 => "Import 1 Ready Song".to_owned(),
        n => format!("Import {n} Ready Songs"),
    }
}

/// The status line while the Import view is shown.
pub fn summary(rows: &[ImportRow]) -> String {
    let files = |n: usize| match n {
        1 => "1 file".to_owned(),
        n => format!("{n} files"),
    };
    match rows.len() {
        0 => "The inbox is empty".to_owned(),
        n => format!(
            "{} in the inbox, {} ready",
            files(n),
            rows.iter().filter(|r| r.ready()).count()
        ),
    }
}

/// Whether lookups start by themselves after the rows are read: files wait
/// for one, the view is shown, nothing runs, and the user has not stopped
/// them (nor has autotag failed) since they last acted.
pub fn auto_look_up(waiting: bool, shown: bool, idle: bool, held: bool) -> bool {
    waiting && shown && idle && !held
}

/// Whether the pane keeps its candidate radios, and the tick the user may
/// have moved, on a reload: the same file, with the same candidates.
pub fn keep_radios(same_file: bool, shown: &[String], candidates: &[CandidateLine]) -> bool {
    same_file
        && !shown.is_empty()
        && shown.len() == candidates.len()
        && shown.iter().zip(candidates).all(|(s, c)| *s == c.label)
}

/// The sidebar's row: "Import", then "Import (6)" while files wait.
pub fn sidebar_label(files: usize) -> String {
    match files {
        0 => "Import".to_owned(),
        n => format!("Import ({n})"),
    }
}

/// The line under the grid while autotag works.
pub fn progress_line(filing: bool, done: usize, total: usize, file: &str) -> String {
    let what = if filing { "Importing" } else { "Looking up" };
    if file.is_empty() {
        format!("{what}…")
    } else {
        format!("{what} {} of {total}: {file}…", (done + 1).min(total))
    }
}

/// The tags Type tags opens with: what the file would get, else what it
/// has.
pub fn proposal(r: &ImportRow) -> Option<Tags> {
    r.after.clone().or_else(|| r.before.clone())
}

/// The fields Type tags stores, from the tags the form ends with: every
/// field with a value, so the typed tags stand for the whole proposal.
pub fn typed_fields(t: &Tags) -> iprune_core::import::Fields {
    iprune_core::import::Fields {
        title: t.title.clone(),
        artist: t.artist.clone(),
        album: t.album.clone(),
        album_artist: t.album_artist.clone(),
        composer: t.composer.clone(),
        genre: t.genre.clone(),
        year: t.year,
        track_no: t.track_no,
        track_total: t.track_total,
        disc_no: t.disc_no,
        disc_total: t.disc_total,
        compilation: Some(t.compilation),
    }
}

/// `t` with Get Info's changes applied: the tags the form ends with.
pub fn with_changes(t: &Tags, c: &iprune_core::tag_edit::Changes) -> Tags {
    let text = |old: &Option<String>, new: &Option<Option<String>>| match new {
        Some(v) => v.clone(),
        None => old.clone(),
    };
    let num = |old: Option<u32>, new: Option<Option<u32>>| new.unwrap_or(old);
    Tags {
        title: text(&t.title, &c.title),
        artist: text(&t.artist, &c.artist),
        album: text(&t.album, &c.album),
        album_artist: text(&t.album_artist, &c.album_artist),
        composer: text(&t.composer, &c.composer),
        genre: text(&t.genre, &c.genre),
        year: num(t.year, c.year),
        track_no: num(t.track_no, c.track_no),
        track_total: num(t.track_total, c.track_total),
        disc_no: num(t.disc_no, c.disc_no),
        disc_total: num(t.disc_total, c.disc_total),
        compilation: c.compilation.unwrap_or(t.compilation),
    }
}

#[cfg(test)]
mod tests;
