//! What the grid shows, without GTK: rows, columns, sorting, searching and
//! formatting. Plain functions, tested without a display.

use std::cmp::Ordering;
use std::path::PathBuf;

use iprune_core::adopt::normalize;
use iprune_core::playlists::Item;
use iprune_core::stats::TrackStats;
use iprune_core::store::TrackRow;
use iprune_core::tags::{FileKind, Tags};

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

    /// Whether a playlist named `name` is kept by this filter: every word
    /// is in the name, folded like a track search (case, accents and
    /// punctuation ignored). An empty filter keeps every playlist.
    pub fn matches_name(&self, name: &str) -> bool {
        let folded = fold_keeping_the(name);
        self.words.iter().all(|w| folded.contains(w.as_str()))
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
    /// The background colour picked from the menu, `#rrggbb`; None follows
    /// the theme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    /// The sidebar's width as last dragged, in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar: Option<i32>,
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

/// The sidebar's width when none was saved: most playlist names (median 15
/// characters, nine in ten under 25) show in full.
pub const SIDEBAR_WIDTH: i32 = 200;

/// The playlists to offer in Show in Playlist: those holding the track,
/// but the one on screen.
pub fn other_playlists(holding: Vec<String>, current: Option<&str>) -> Vec<String> {
    holding
        .into_iter()
        .filter(|n| Some(n.as_str()) != current)
        .collect()
}

/// The playlist positions of the entries of library track `id` among
/// `rows`.
pub fn positions_of_track<'a>(rows: impl Iterator<Item = &'a Row>, id: i64) -> Vec<usize> {
    rows.filter(|r| r.track_id == Some(id))
        .filter_map(|r| r.position)
        .collect()
}

impl Layouts {
    /// Records the sidebar's width to save, only once it differs from the
    /// one it started at: a width never dragged follows `SIDEBAR_WIDTH`.
    pub fn note_sidebar(&mut self, width: i32) {
        if self.sidebar.is_some() || width != self.sidebar_width() {
            self.sidebar = Some(width);
        }
    }

    /// The sidebar's width: the one saved, unless it is out of reason
    /// (below 120 or above 600 pixels).
    pub fn sidebar_width(&self) -> i32 {
        self.sidebar
            .filter(|w| (120..=600).contains(w))
            .unwrap_or(SIDEBAR_WIDTH)
    }

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
            background: table
                .get("background")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            sidebar: table
                .get("sidebar")
                .and_then(|v| v.as_integer())
                .and_then(|w| i32::try_from(w).ok()),
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

/// A text or number field of the Get Info form, in the order of the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoField {
    Title,
    Artist,
    AlbumArtist,
    Album,
    Composer,
    Genre,
    Year,
    Track,
    TrackTotal,
    Disc,
    DiscTotal,
}

impl InfoField {
    pub const ALL: [InfoField; 11] = [
        InfoField::Title,
        InfoField::Artist,
        InfoField::AlbumArtist,
        InfoField::Album,
        InfoField::Composer,
        InfoField::Genre,
        InfoField::Year,
        InfoField::Track,
        InfoField::TrackTotal,
        InfoField::Disc,
        InfoField::DiscTotal,
    ];

    pub fn label(self) -> &'static str {
        match self {
            InfoField::Title => "Name",
            InfoField::Artist => "Artist",
            InfoField::AlbumArtist => "Album Artist",
            InfoField::Album => "Album",
            InfoField::Composer => "Composer",
            InfoField::Genre => "Genre",
            InfoField::Year => "Year",
            InfoField::Track => "Track",
            InfoField::TrackTotal => "Track total",
            InfoField::Disc => "Disc",
            InfoField::DiscTotal => "Disc total",
        }
    }

    pub fn numeric(self) -> bool {
        matches!(
            self,
            InfoField::Year
                | InfoField::Track
                | InfoField::TrackTotal
                | InfoField::Disc
                | InfoField::DiscTotal
        )
    }

    /// Whether the form shows it for several songs: one name or one track
    /// number for all of them makes no sense.
    pub fn for_several(self) -> bool {
        !matches!(self, InfoField::Title | InfoField::Track)
    }

    /// Whether it suggests the values already in the library.
    pub fn completes(self) -> bool {
        matches!(
            self,
            InfoField::Artist
                | InfoField::AlbumArtist
                | InfoField::Album
                | InfoField::Composer
                | InfoField::Genre
        )
    }

    fn index(self) -> usize {
        InfoField::ALL
            .iter()
            .position(|&f| f == self)
            .expect("in ALL")
    }

    /// The field's value in `t`, as the form shows it.
    pub fn text(self, t: &Tags) -> String {
        let s = |v: &Option<String>| v.clone().unwrap_or_default();
        let n = |v: Option<u32>| v.map(|n| n.to_string()).unwrap_or_default();
        match self {
            InfoField::Title => s(&t.title),
            InfoField::Artist => s(&t.artist),
            InfoField::AlbumArtist => s(&t.album_artist),
            InfoField::Album => s(&t.album),
            InfoField::Composer => s(&t.composer),
            InfoField::Genre => s(&t.genre),
            InfoField::Year => n(t.year),
            InfoField::Track => n(t.track_no),
            InfoField::TrackTotal => n(t.track_total),
            InfoField::Disc => n(t.disc_no),
            InfoField::DiscTotal => n(t.disc_total),
        }
    }
}

/// What the form opens with for the songs edited: each field's value when
/// every song has the same, `None` when they differ ("Mixed").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub text: [Option<String>; 11],
    pub compilation: Option<bool>,
}

impl Opened {
    pub fn new(tags: &[&Tags]) -> Opened {
        let shared = |f: &dyn Fn(&Tags) -> String| {
            let mut values = tags.iter().map(|t| f(t));
            let first = values.next().unwrap_or_default();
            values.all(|v| v == first).then_some(first)
        };
        Opened {
            text: InfoField::ALL.map(|field| shared(&|t| field.text(t))),
            compilation: {
                let mut values = tags.iter().map(|t| t.compilation);
                let first = values.next().unwrap_or(false);
                values.all(|v| v == first).then_some(first)
            },
        }
    }

    pub fn get(&self, f: InfoField) -> Option<&str> {
        self.text[f.index()].as_deref()
    }
}

/// What the form holds when OK is pressed. `ticked` matters only for
/// several songs, where only ticked fields are written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entered {
    pub text: [String; 11],
    pub ticked: [bool; 11],
    /// `None` while the compilation box still shows "mixed".
    pub compilation: Option<bool>,
    pub compilation_ticked: bool,
}

impl Entered {
    /// The form as it opens: what `opened` shows, nothing ticked.
    pub fn new(opened: &Opened) -> Entered {
        Entered {
            text: opened.text.clone().map(Option::unwrap_or_default),
            ticked: [false; 11],
            compilation: opened.compilation,
            compilation_ticked: false,
        }
    }

    pub fn get(&self, f: InfoField) -> &str {
        &self.text[f.index()]
    }

    pub fn set(&mut self, f: InfoField, text: &str) {
        self.text[f.index()] = text.to_owned();
    }

    pub fn ticked(&self, f: InfoField) -> bool {
        self.ticked[f.index()]
    }

    pub fn tick(&mut self, f: InfoField, on: bool) {
        self.ticked[f.index()] = on;
    }
}

/// A number field's value: empty clears it. Digits only, in range.
fn number_field(f: InfoField, text: &str) -> Result<Option<u32>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let (min, max) = match f {
        InfoField::Year => (
            iprune_core::tag_edit::MIN_YEAR,
            iprune_core::tag_edit::MAX_YEAR,
        ),
        _ => (1, iprune_core::tag_edit::MAX_NUMBER),
    };
    match t.parse::<u32>() {
        Ok(n) if t.bytes().all(|b| b.is_ascii_digit()) && (min..=max).contains(&n) => Ok(Some(n)),
        _ => Err(format!("{} takes a number from {min} to {max}", f.label())),
    }
}

/// The edit OK sends: for one song, the fields that differ from what the
/// form opened with; for several, the ticked ones. An error names the
/// first field whose value cannot be written, and OK stays disabled.
pub fn info_changes(
    several: bool,
    opened: &Opened,
    entered: &Entered,
) -> Result<iprune_core::tag_edit::Changes, String> {
    let mut c = iprune_core::tag_edit::Changes::default();
    for f in InfoField::ALL {
        if several && !f.for_several() {
            continue;
        }
        let text = entered.get(f);
        let wanted = if several {
            entered.ticked(f)
        } else {
            opened.get(f) != Some(text)
        };
        if !wanted {
            continue;
        }
        if f.numeric() {
            // Only a value to write is checked: a song may hold one out of
            // range, which must not keep its other fields from being edited.
            let n = number_field(f, text)?;
            let slot = match f {
                InfoField::Year => &mut c.year,
                InfoField::Track => &mut c.track_no,
                InfoField::TrackTotal => &mut c.track_total,
                InfoField::Disc => &mut c.disc_no,
                _ => &mut c.disc_total,
            };
            *slot = Some(n);
            continue;
        }
        let v = Some(text.trim().to_owned()).filter(|s| !s.is_empty());
        let slot = match f {
            InfoField::Title => &mut c.title,
            InfoField::Artist => &mut c.artist,
            InfoField::AlbumArtist => &mut c.album_artist,
            InfoField::Album => &mut c.album,
            InfoField::Composer => &mut c.composer,
            _ => &mut c.genre,
        };
        *slot = Some(v);
    }
    let compilation_wanted = if several {
        entered.compilation_ticked
    } else {
        entered.compilation != opened.compilation
    };
    if compilation_wanted && let Some(on) = entered.compilation {
        c.compilation = Some(on);
    }
    Ok(c)
}

/// Whether OK would clear a field that has a value in a song: the form
/// then says the iPod keeps it.
pub fn info_clears(several: bool, opened: &Opened, entered: &Entered) -> bool {
    InfoField::ALL.iter().any(|&f| {
        if several && !f.for_several() {
            return false;
        }
        let emptied = entered.get(f).trim().is_empty();
        let had = opened.get(f) != Some("");
        emptied && had && (!several || entered.ticked(f))
    })
}

/// The values a completing field suggests, from every track in the library.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completions {
    pub artist: Vec<String>,
    pub album_artist: Vec<String>,
    pub album: Vec<String>,
    pub composer: Vec<String>,
    pub genre: Vec<String>,
}

impl Completions {
    /// Each field's distinct values among the present tracks, sorted as
    /// the grid sorts them.
    pub fn new(tracks: &[TrackRow]) -> Completions {
        let pool = |f: &dyn Fn(&Tags) -> &Option<String>| {
            let mut v: Vec<(String, String)> = tracks
                .iter()
                .filter(|t| t.missing_since.is_none())
                .filter_map(|t| f(&t.tags).clone())
                .map(|s| (fold_keeping_the(&s), s))
                .collect();
            v.sort();
            v.dedup_by(|a, b| a.1 == b.1);
            v.into_iter().map(|(_, s)| s).collect()
        };
        Completions {
            artist: pool(&|t| &t.artist),
            album_artist: pool(&|t| &t.album_artist),
            album: pool(&|t| &t.album),
            composer: pool(&|t| &t.composer),
            genre: pool(&|t| &t.genre),
        }
    }

    pub fn of(&self, f: InfoField) -> &[String] {
        match f {
            InfoField::Artist => &self.artist,
            InfoField::AlbumArtist => &self.album_artist,
            InfoField::Album => &self.album,
            InfoField::Composer => &self.composer,
            InfoField::Genre => &self.genre,
            _ => &[],
        }
    }
}

/// Up to `max` values of `pool` starting with `typed`, ignoring case,
/// accents and punctuation, but for `typed` itself; none for an empty
/// field.
pub fn suggestions(pool: &[String], typed: &str, max: usize) -> Vec<String> {
    let want = fold_keeping_the(typed);
    if want.is_empty() {
        return Vec::new();
    }
    pool.iter()
        .filter(|v| v.as_str() != typed.trim() && fold_keeping_the(v).starts_with(&want))
        .take(max)
        .cloned()
        .collect()
}

/// The read-only line under the form for one song: kind, time, bit rate,
/// sample rate and size, those known.
pub fn facts(t: &TrackRow) -> String {
    let mut parts = vec![kind_name(t.info.kind).to_string()];
    if let Some(ms) = t.info.duration_ms {
        parts.push(duration(ms));
    }
    if let Some(b) = t.info.bitrate {
        parts.push(format!("{b} kbit/s"));
    }
    if let Some(r) = t.info.samplerate {
        parts.push(format!("{} kHz", r as f64 / 1000.0));
    }
    parts.push(megabytes(t.size));
    parts.join(", ")
}
