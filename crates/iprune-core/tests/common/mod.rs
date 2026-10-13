//! Shared helpers for the integration tests: ffmpeg-generated audio fixtures
//! (built once per test binary, then copied), and a throwaway library with its
//! own playlists folder and SQLite index. Nothing here touches the real home
//! or the real music library.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use iprune_core::config::{Config, PathStyle};
use iprune_core::scan::{self, ScanReport};
use iprune_core::store::{self, Store, TrackRow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Mp3,
    Aac,
    Alac,
    Wav,
    Aiff,
    Flac,
    /// FLAC audio inside an MP4 container, named .m4a.
    FlacInM4a,
    /// Opus audio inside an MP4 container, named .m4a.
    OpusInM4a,
}

impl Kind {
    pub fn ext(self) -> &'static str {
        match self {
            Kind::Mp3 => "mp3",
            Kind::Aac | Kind::Alac | Kind::FlacInM4a | Kind::OpusInM4a => "m4a",
            Kind::Wav => "wav",
            Kind::Aiff => "aiff",
            Kind::Flac => "flac",
        }
    }

    fn ffmpeg_args(self) -> &'static [&'static str] {
        match self {
            Kind::Mp3 => &["-c:a", "libmp3lame"],
            Kind::Aac => &["-c:a", "aac"],
            Kind::Alac => &["-c:a", "alac"],
            Kind::Wav | Kind::Aiff => &[],
            Kind::Flac => &["-c:a", "flac"],
            Kind::FlacInM4a => &["-c:a", "flac", "-strict", "-2"],
            Kind::OpusInM4a => &["-c:a", "libopus"],
        }
    }
}

static GENERATE: Mutex<()> = Mutex::new(());

/// A cached audio file of `kind`. Each `variant` has a different tone, title
/// ("Title<variant>") and artist ("Artist<variant>"), so its bytes differ.
pub fn fixture(kind: Kind, variant: u32) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("audio-fixtures");
    let name = format!("{kind:?}-{variant}.{}", kind.ext());
    let path = dir.join(&name);
    let _guard = GENERATE.lock().unwrap_or_else(|e| e.into_inner());
    if path.exists() {
        return path;
    }
    fs::create_dir_all(&dir).unwrap();
    let tmp = dir.join(format!(".{}-{name}", std::process::id()));
    let out = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("sine=frequency={}:duration=1", 300 + variant * 37))
        .arg("-metadata")
        .arg(format!("title=Title{variant}"))
        .arg("-metadata")
        .arg(format!("artist=Artist{variant}"))
        .args(kind.ffmpeg_args())
        // The output name must keep its extension for ffmpeg to pick a muxer.
        .arg("-f")
        .arg(match kind {
            Kind::Mp3 => "mp3",
            Kind::Aac | Kind::Alac => "ipod",
            Kind::Wav => "wav",
            Kind::Aiff => "aiff",
            Kind::Flac => "flac",
            Kind::FlacInM4a | Kind::OpusInM4a => "mp4",
        })
        .arg(&tmp)
        .output()
        .expect("ffmpeg must be installed");
    assert!(
        out.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::rename(&tmp, &path).unwrap();
    path
}

pub struct Env {
    _dir: tempfile::TempDir,
    pub music: PathBuf,
    pub lists: PathBuf,
    pub cfg: Config,
    pub store: Store,
}

impl Env {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let music = base.join("music");
        let lists = base.join("lists");
        fs::create_dir(&music).unwrap();
        fs::create_dir(&lists).unwrap();
        let mut cfg = Config::default();
        cfg.library.roots = vec![music.clone()];
        cfg.playlists.dir = lists.clone();
        Env {
            _dir: dir,
            music,
            lists,
            cfg,
            store: Store::open(&base.join("db/iprune.sqlite")).unwrap(),
        }
    }

    pub fn relative_playlists(mut self) -> Self {
        self.cfg.playlists.path_style = PathStyle::Relative;
        self
    }

    /// Copies a fixture to `rel` under the music folder.
    pub fn add(&self, rel: &str, kind: Kind, variant: u32) -> PathBuf {
        self.add_at(&self.music, rel, kind, variant)
    }

    pub fn add_at(&self, root: &Path, rel: &str, kind: Kind, variant: u32) -> PathBuf {
        let dest = root.join(rel);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::copy(fixture(kind, variant), &dest).unwrap();
        dest
    }

    pub fn scan(&mut self) -> ScanReport {
        self.try_scan().expect("scan succeeds")
    }

    pub fn try_scan(&mut self) -> iprune_core::Result<ScanReport> {
        scan::scan(&mut self.store, &self.cfg, &|_| {})
    }

    pub fn tracks(&self) -> Vec<TrackRow> {
        store::all_tracks(self.store.conn()).unwrap()
    }

    pub fn track(&self, path: &Path) -> TrackRow {
        self.tracks()
            .into_iter()
            .find(|t| t.path == path)
            .unwrap_or_else(|| panic!("no track at {}", path.display()))
    }

    pub fn playlist_file(&self, name: &str) -> PathBuf {
        self.lists.join(format!("{name}.m3u8"))
    }

    pub fn write_playlist(&self, name: &str, text: &str) -> PathBuf {
        let p = self.playlist_file(name);
        fs::write(&p, text).unwrap();
        p
    }

    pub fn read_playlist(&self, name: &str) -> String {
        fs::read_to_string(self.playlist_file(name)).unwrap()
    }

    /// The location lines of a playlist file, in order.
    pub fn locations(&self, name: &str) -> Vec<String> {
        self.read_playlist(name)
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(String::from)
            .collect()
    }

    /// `(position, track_id, raw_entry)` rows of a playlist in the index.
    pub fn indexed_items(&self, name: &str) -> Vec<(i64, Option<i64>, String)> {
        let mut stmt = self
            .store
            .conn()
            .prepare(
                "SELECT i.position, i.track_id, i.raw_entry FROM playlist_items i \
                 JOIN playlists p ON p.id = i.playlist_id WHERE p.name = ?1 ORDER BY i.position",
            )
            .unwrap();
        stmt.query_map([name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    pub fn playlist_id(&self, name: &str) -> Option<i64> {
        store::playlists(self.store.conn())
            .unwrap()
            .into_iter()
            .find(|p| p.name == name)
            .map(|p| p.id)
    }
}

/// Whether directory permission bits stop us reading (false when running as root).
pub fn chmod_000_blocks_reads(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).unwrap();
    let blocked = fs::read_dir(dir).is_err();
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
    blocked
}
