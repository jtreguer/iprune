//! What the grid shows, without GTK: rows, columns, sorting, searching and
//! formatting. Plain functions, tested without a display.

use std::cmp::Ordering;
use std::path::PathBuf;

use iprune_core::adopt::normalize;
use iprune_core::playlists::Item;
use iprune_core::stats::TrackStats;
use iprune_core::store::TrackRow;
use iprune_core::tags::FileKind;

/// One line of the grid: a library track, or a playlist entry that matches
/// no file.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// 1-based position in the playlist; `None` in the library view.
    pub position: Option<usize>,
    pub track_id: Option<i64>,
    pub name: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub composer: String,
    pub genre: String,
    pub time_ms: Option<u32>,
    pub year: Option<u32>,
    pub disc: Option<u32>,
    pub track: Option<u32>,
    pub plays: u32,
    pub skips: u32,
    /// 0 to 100.
    pub rating: Option<u32>,
    pub last_played: Option<i64>,
    pub added: Option<i64>,
    pub kind: &'static str,
    pub size: Option<u64>,
    pub path: PathBuf,
    /// A playlist entry that matches no indexed file.
    pub unresolved: bool,
    /// Title, artists, album and composer, folded for searching.
    search: String,
    /// Text columns folded once for sorting (case, accents, leading "The").
    keys: Keys,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Keys {
    name: String,
    artist: String,
    album: String,
    album_artist: String,
    composer: String,
    genre: String,
}

/// Folded like the sort keys, but a leading "the" is kept, so a query with
/// "the" inside it ("floyd the wall") still finds "The Wall". `normalize`
/// drops only a first word "the"; a throwaway first word keeps it.
fn fold_keeping_the(s: &str) -> String {
    let folded = normalize(Some(&format!("x {s}")));
    folded
        .strip_prefix('x')
        .map_or(folded.as_str(), str::trim_start)
        .to_owned()
}

/// A search, folded once per keystroke rather than once per row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    words: Vec<String>,
}

impl Query {
    pub fn new(text: &str) -> Query {
        Query {
            words: normalize(Some(text))
                .split(' ')
                .filter(|w| !w.is_empty())
                .map(str::to_owned)
                .collect(),
        }
    }
}

fn kind_name(k: FileKind) -> &'static str {
    match k {
        FileKind::Mp3 => "MP3",
        FileKind::Aac => "AAC",
        FileKind::Alac => "ALAC",
        FileKind::Wav => "WAV",
        FileKind::Aiff => "AIFF",
    }
}

impl Row {
    pub fn from_track(t: &TrackRow, stats: Option<&TrackStats>, position: Option<usize>) -> Row {
        let s = stats.copied().unwrap_or_default();
        let text = |v: &Option<String>| v.clone().unwrap_or_default();
        let mut row = Row {
            position,
            track_id: Some(t.id),
            name: t.display_title().into_owned(),
            artist: text(&t.tags.artist),
            album: text(&t.tags.album),
            album_artist: text(&t.tags.album_artist),
            composer: text(&t.tags.composer),
            genre: text(&t.tags.genre),
            time_ms: t.info.duration_ms,
            year: t.tags.year,
            disc: t.tags.disc_no,
            track: t.tags.track_no,
            plays: s.plays,
            skips: s.skips,
            rating: s.rating,
            last_played: s.last_played,
            added: Some(t.added_at),
            kind: kind_name(t.info.kind),
            size: Some(t.size),
            path: t.path.clone(),
            unresolved: false,
            search: String::new(),
            keys: Keys::default(),
        };
        row.fold();
        row
    }

    /// A playlist entry with no file behind it, shown by its location.
    pub fn unresolved(location: &str, position: usize) -> Row {
        let path = PathBuf::from(location);
        let name = path
            .file_stem()
            .map_or_else(|| location.to_owned(), |s| s.to_string_lossy().into_owned());
        let mut row = Row {
            position: Some(position),
            track_id: None,
            name,
            artist: String::new(),
            album: String::new(),
            album_artist: String::new(),
            composer: String::new(),
            genre: String::new(),
            time_ms: None,
            year: None,
            disc: None,
            track: None,
            plays: 0,
            skips: 0,
            rating: None,
            last_played: None,
            added: None,
            kind: "",
            size: None,
            path,
            unresolved: true,
            search: String::new(),
            keys: Keys::default(),
        };
        row.fold();
        row
    }

    /// Folds the text columns once, for sorting and searching.
    fn fold(&mut self) {
        let f = |s: &str| normalize(Some(s));
        self.keys = Keys {
            name: f(&self.name),
            artist: f(&self.artist),
            album: f(&self.album),
            album_artist: f(&self.album_artist),
            composer: f(&self.composer),
            genre: f(&self.genre),
        };
        self.search = [
            &self.name,
            &self.artist,
            &self.album,
            &self.album_artist,
            &self.composer,
        ]
        .iter()
        .map(|s| fold_keeping_the(s))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    }

    /// True when every word of `query` appears in the title, artists, album
    /// or composer, ignoring case, accents and punctuation. An empty query
    /// matches everything.
    pub fn matches(&self, query: &Query) -> bool {
        query.words.iter().all(|w| self.search.contains(w.as_str()))
    }
}

/// The library's rows, by track: present tracks only.
pub fn library_rows(
    tracks: &[TrackRow],
    stats: &std::collections::HashMap<i64, TrackStats>,
) -> Vec<Row> {
    tracks
        .iter()
        .filter(|t| t.missing_since.is_none())
        .map(|t| Row::from_track(t, stats.get(&t.id), None))
        .collect()
}

/// A playlist's rows, in file order, numbered from 1.
pub fn playlist_rows(
    items: &[Item],
    stats: &std::collections::HashMap<i64, TrackStats>,
) -> Vec<Row> {
    items
        .iter()
        .enumerate()
        .map(|(i, item)| match &item.track {
            Some(t) => Row::from_track(t, stats.get(&t.id), Some(i + 1)),
            None => Row::unresolved(&item.entry.location, i + 1),
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Column {
    Position,
    Name,
    Time,
    Artist,
    Album,
    AlbumArtist,
    Composer,
    Genre,
    Year,
    Disc,
    Track,
    Plays,
    Skips,
    Rating,
    LastPlayed,
    Added,
    Kind,
    Size,
}

impl Column {
    pub const ALL: [Column; 18] = [
        Column::Position,
        Column::Name,
        Column::Time,
        Column::Artist,
        Column::Album,
        Column::AlbumArtist,
        Column::Composer,
        Column::Genre,
        Column::Year,
        Column::Disc,
        Column::Track,
        Column::Plays,
        Column::Skips,
        Column::Rating,
        Column::LastPlayed,
        Column::Added,
        Column::Kind,
        Column::Size,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Column::Position => "#",
            Column::Name => "Name",
            Column::Time => "Time",
            Column::Artist => "Artist",
            Column::Album => "Album",
            Column::AlbumArtist => "Album Artist",
            Column::Composer => "Composer",
            Column::Genre => "Genre",
            Column::Year => "Year",
            Column::Disc => "Disc",
            Column::Track => "Track",
            Column::Plays => "Plays",
            Column::Skips => "Skips",
            Column::Rating => "Rating",
            Column::LastPlayed => "Last Played",
            Column::Added => "Date Added",
            Column::Kind => "Kind",
            Column::Size => "Size",
        }
    }

    /// A stable name for saving the layout.
    pub fn key(self) -> &'static str {
        match self {
            Column::Position => "position",
            Column::Name => "name",
            Column::Time => "time",
            Column::Artist => "artist",
            Column::Album => "album",
            Column::AlbumArtist => "album_artist",
            Column::Composer => "composer",
            Column::Genre => "genre",
            Column::Year => "year",
            Column::Disc => "disc",
            Column::Track => "track",
            Column::Plays => "plays",
            Column::Skips => "skips",
            Column::Rating => "rating",
            Column::LastPlayed => "last_played",
            Column::Added => "added",
            Column::Kind => "kind",
            Column::Size => "size",
        }
    }

    pub fn from_key(key: &str) -> Option<Column> {
        Column::ALL.into_iter().find(|c| c.key() == key)
    }

    /// Initial width in pixels.
    pub fn width(self) -> i32 {
        match self {
            Column::Position | Column::Disc | Column::Track => 48,
            Column::Time | Column::Year | Column::Plays | Column::Skips | Column::Kind => 64,
            Column::Rating | Column::Size => 80,
            Column::LastPlayed | Column::Added => 100,
            Column::Genre => 120,
            Column::Name => 280,
            Column::Artist | Column::Album | Column::AlbumArtist | Column::Composer => 200,
        }
    }

    /// Shown until the user changes it.
    pub fn visible_by_default(self) -> bool {
        matches!(
            self,
            Column::Position
                | Column::Name
                | Column::Time
                | Column::Artist
                | Column::Album
                | Column::Genre
                | Column::Year
                | Column::Plays
                | Column::Rating
        )
    }

    /// Numbers are right-aligned.
    pub fn numeric(self) -> bool {
        matches!(
            self,
            Column::Position
                | Column::Time
                | Column::Year
                | Column::Disc
                | Column::Track
                | Column::Plays
                | Column::Skips
                | Column::Size
        )
    }

    /// The text of the cell. Dates are left to the caller, which knows the
    /// local time zone: [`Column::LastPlayed`] and [`Column::Added`] give "".
    pub fn text(self, r: &Row) -> String {
        let num = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
        match self {
            Column::Position => r.position.map(|p| p.to_string()).unwrap_or_default(),
            Column::Name => r.name.clone(),
            Column::Time => r.time_ms.map(duration).unwrap_or_default(),
            Column::Artist => r.artist.clone(),
            Column::Album => r.album.clone(),
            Column::AlbumArtist => r.album_artist.clone(),
            Column::Composer => r.composer.clone(),
            Column::Genre => r.genre.clone(),
            Column::Year => num(r.year),
            Column::Disc => num(r.disc),
            Column::Track => num(r.track),
            Column::Plays => match r.plays {
                0 => String::new(),
                n => n.to_string(),
            },
            Column::Skips => match r.skips {
                0 => String::new(),
                n => n.to_string(),
            },
            Column::Rating => r.rating.map(stars).unwrap_or_default(),
            Column::LastPlayed | Column::Added => String::new(),
            Column::Kind => r.kind.to_owned(),
            Column::Size => r.size.map(megabytes).unwrap_or_default(),
        }
    }

    /// The Unix time a date column shows, if any.
    pub fn time(self, r: &Row) -> Option<i64> {
        match self {
            Column::LastPlayed => r.last_played,
            Column::Added => r.added,
            _ => None,
        }
    }

    /// Ascending order for this column. Empty values sort last, text ignores
    /// case and accents, and ties fall back to artist, album, disc, track and
    /// name, then to the position. A descending sort reverses all of it,
    /// empty values and ties included.
    pub fn compare(self, a: &Row, b: &Row) -> Ordering {
        self.primary(a, b).then_with(|| tie_break(a, b))
    }

    fn primary(self, a: &Row, b: &Row) -> Ordering {
        match self {
            Column::Position => opt(a.position, b.position),
            Column::Name => text(&a.keys.name, &b.keys.name),
            Column::Time => opt(a.time_ms, b.time_ms),
            Column::Artist => text(&a.keys.artist, &b.keys.artist),
            Column::Album => text(&a.keys.album, &b.keys.album),
            Column::AlbumArtist => text(&a.keys.album_artist, &b.keys.album_artist),
            Column::Composer => text(&a.keys.composer, &b.keys.composer),
            Column::Genre => text(&a.keys.genre, &b.keys.genre),
            Column::Year => opt(a.year, b.year),
            Column::Disc => opt(a.disc, b.disc),
            Column::Track => opt(a.track, b.track),
            Column::Plays => a.plays.cmp(&b.plays),
            Column::Skips => a.skips.cmp(&b.skips),
            Column::Rating => opt(a.rating, b.rating),
            Column::LastPlayed => opt(a.last_played, b.last_played),
            Column::Added => opt(a.added, b.added),
            Column::Kind => text(a.kind, b.kind),
            Column::Size => opt(a.size, b.size),
        }
    }
}

/// Values first in their order, missing ones last.
fn opt<T: Ord>(a: Option<T>, b: Option<T>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Two folded keys (case, accents and a leading "The" removed), empty last.
fn text(x: &str, y: &str) -> Ordering {
    match (x.is_empty(), y.is_empty()) {
        (false, true) => Ordering::Less,
        (true, false) => Ordering::Greater,
        _ => x.cmp(y),
    }
}

fn tie_break(a: &Row, b: &Row) -> Ordering {
    text(&a.keys.artist, &b.keys.artist)
        .then_with(|| text(&a.keys.album, &b.keys.album))
        .then_with(|| opt(a.disc, b.disc))
        .then_with(|| opt(a.track, b.track))
        .then_with(|| text(&a.keys.name, &b.keys.name))
        .then_with(|| opt(a.position, b.position))
        .then_with(|| a.path.cmp(&b.path))
}

/// `m:ss`, or `h:mm:ss` from one hour.
pub fn duration(ms: u32) -> String {
    let s = ms / 1000;
    match (s / 3600, s / 60 % 60, s % 60) {
        (0, m, s) => format!("{m}:{s:02}"),
        (h, m, s) => format!("{h}:{m:02}:{s:02}"),
    }
}

/// Total playing time as "N h M min", or "M min" under an hour.
pub fn total_time(ms: u64) -> String {
    let min = ms / 60_000;
    match (min / 60, min % 60) {
        (0, m) => format!("{m} min"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// One to five stars out of a 0 to 100 rating, each star 20.
pub fn stars(rating: u32) -> String {
    let n = (rating.min(100) + 10) / 20;
    "★".repeat(n as usize)
}

pub fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

/// "214 songs, 14 h 3 min, 1.2 GB" for the rows shown.
pub fn summary<'a>(rows: impl Iterator<Item = &'a Row>) -> String {
    let (mut n, mut ms, mut bytes) = (0usize, 0u64, 0u64);
    for r in rows {
        n += 1;
        ms += u64::from(r.time_ms.unwrap_or(0));
        bytes += r.size.unwrap_or(0);
    }
    let songs = if n == 1 {
        "1 song".to_owned()
    } else {
        format!("{n} songs")
    };
    format!(
        "{songs}, {}, {}",
        total_time(ms),
        iprune_core::sync::bytes(bytes)
    )
}

#[cfg(test)]
mod tests;

/// How one column is shown.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ColumnState {
    pub id: String,
    pub width: i32,
    pub visible: bool,
}

/// The column layout of one kind of view (the library, or playlists), in
/// display order.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Layout {
    pub columns: Vec<ColumnState>,
}

/// The saved layouts, `~/.local/state/iprune/ui.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Layouts {
    #[serde(default)]
    pub library: Layout,
    #[serde(default)]
    pub playlist: Layout,
}

impl Layout {
    /// The default layout. In the library the position column is hidden:
    /// there is no position.
    pub fn default_for(playlist: bool) -> Layout {
        Layout {
            columns: Column::ALL
                .iter()
                .map(|c| ColumnState {
                    id: c.key().to_owned(),
                    width: c.width(),
                    visible: c.visible_by_default() && (playlist || *c != Column::Position),
                })
                .collect(),
        }
    }

    /// A saved layout made whole: unknown or repeated columns dropped, a
    /// width below 24 pixels raised to 24, and columns it lacks (added in a
    /// later version) appended with their defaults. In the library the
    /// position column stays hidden.
    pub fn complete(&self, playlist: bool) -> Layout {
        let defaults = Layout::default_for(playlist);
        let mut seen = std::collections::HashSet::new();
        let mut columns: Vec<ColumnState> = self
            .columns
            .iter()
            .filter(|c| Column::from_key(&c.id).is_some() && seen.insert(c.id.clone()))
            .map(|c| ColumnState {
                id: c.id.clone(),
                width: c.width.max(24),
                visible: c.visible && (playlist || c.id != Column::Position.key()),
            })
            .collect();
        for d in defaults.columns {
            if !seen.contains(&d.id) {
                columns.push(d);
            }
        }
        Layout { columns }
    }
}

/// The 1-based positions the entries at `moved` end up at after
/// [`iprune_core::playlists::reorder`] gave `order`, sorted.
pub fn new_positions(order: &[usize], moved: &[usize]) -> Vec<usize> {
    let mut out: Vec<usize> = order
        .iter()
        .enumerate()
        .filter(|(_, p)| moved.contains(p))
        .map(|(i, _)| i + 1)
        .collect();
    out.sort_unstable();
    out
}

/// Where a drop on the row at 1-based `position` inserts: before it in its
/// upper half, after it in its lower half.
pub fn drop_before(position: usize, y: f64, height: f64) -> usize {
    if y < height / 2.0 {
        position
    } else {
        position + 1
    }
}

impl Layouts {
    /// Reads `ui.toml`, keeping whatever is readable: a malformed column is
    /// dropped on its own, and a view that cannot be read gets the default.
    pub fn parse(text: &str) -> Layouts {
        let Ok(table) = text.parse::<toml::Table>() else {
            return Layouts::default();
        };
        let view = |key: &str| -> Layout {
            let columns = table
                .get(key)
                .and_then(|v| v.get("columns"))
                .and_then(|c| c.as_array())
                .map(|cols| {
                    cols.iter()
                        .filter_map(|c| c.clone().try_into::<ColumnState>().ok())
                        .collect()
                })
                .unwrap_or_default();
            Layout { columns }
        };
        Layouts {
            library: view("library"),
            playlist: view("playlist"),
        }
    }
}

/// What Ctrl+Up or Ctrl+Down does with the selected positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nudge {
    /// Move them before this 1-based position (see `playlists::reorder`).
    Before(usize),
    /// The selection has gaps: moving it would gather it.
    NotAdjacent,
    /// Nothing selected, or already at the top or bottom.
    Nothing,
}

/// The move for Ctrl+Up (`up`) or Ctrl+Down on 1-based `positions` in a
/// playlist of `len` entries: one place, adjacent entries only.
pub fn nudge_before(positions: &[usize], len: usize, up: bool) -> Nudge {
    let mut p = positions.to_vec();
    p.sort_unstable();
    p.dedup();
    let (Some(&first), Some(&last)) = (p.first(), p.last()) else {
        return Nudge::Nothing;
    };
    if last - first + 1 != p.len() {
        return Nudge::NotAdjacent;
    }
    match up {
        true if first > 1 => Nudge::Before(first - 1),
        false if last < len => Nudge::Before(last + 2),
        _ => Nudge::Nothing,
    }
}
