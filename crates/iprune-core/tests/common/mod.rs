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
    fixture_secs(kind, variant, 1)
}

/// [`fixture`] lasting `secs` seconds, for tests that need a different
/// duration (the planner copies audio again when it changes by over a second).
pub fn fixture_secs(kind: Kind, variant: u32, secs: u32) -> PathBuf {
    let dir = target_tmp().join("audio-fixtures");
    let name = match secs {
        1 => format!("{kind:?}-{variant}.{}", kind.ext()),
        n => format!("{kind:?}-{variant}-{n}s.{}", kind.ext()),
    };
    let path = dir.join(&name);
    let _guard = GENERATE.lock().unwrap_or_else(|e| e.into_inner());
    if path.exists() {
        return path;
    }
    fs::create_dir_all(&dir).unwrap();
    let tmp = dir.join(format!(".{}-{name}", std::process::id()));
    let out = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!(
            "sine=frequency={}:duration={secs}",
            300 + variant * 37
        ))
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

/// One ID3v2.4 frame: id, syncsafe size, no flags, then `data`.
pub fn id3v24_frame(id: &str, data: &[u8]) -> Vec<u8> {
    assert_eq!(id.len(), 4);
    let n = data.len() as u32;
    assert!(n < 1 << 28);
    let mut f = id.as_bytes().to_vec();
    f.extend([
        (n >> 21) as u8 & 0x7f,
        (n >> 14) as u8 & 0x7f,
        (n >> 7) as u8 & 0x7f,
        n as u8 & 0x7f,
    ]);
    f.extend([0, 0]);
    f.extend(data);
    f
}

/// A text frame: encoding byte, then the raw text bytes.
pub fn id3v24_text(id: &str, encoding: u8, text: &[u8]) -> Vec<u8> {
    let mut d = vec![encoding];
    d.extend(text);
    id3v24_frame(id, d.as_slice())
}

/// The bytes of the cached Mp3 fixture `variant` with its ID3v2 tag replaced
/// by a hand-built ID3v2.4 tag made of `frames`. The audio is untouched.
pub fn mp3_with_id3v2(variant: u32, frames: &[Vec<u8>]) -> Vec<u8> {
    let raw = fs::read(fixture(Kind::Mp3, variant)).unwrap();
    let audio = if raw.starts_with(b"ID3") {
        let size = raw[6..10]
            .iter()
            .fold(0usize, |a, b| (a << 7) | *b as usize);
        &raw[10 + size..]
    } else {
        &raw[..]
    };
    let body: Vec<u8> = frames.concat();
    let n = body.len() as u32;
    let mut out = b"ID3\x04\x00\x00".to_vec();
    out.extend([
        (n >> 21) as u8 & 0x7f,
        (n >> 14) as u8 & 0x7f,
        (n >> 7) as u8 & 0x7f,
        n as u8 & 0x7f,
    ]);
    out.extend(body);
    out.extend(audio);
    out
}

/// The bytes of the cached Aac fixture `variant` with the `mp4a` sample entry
/// renamed `drms`, which is how FairPlay protected iTunes Store files look.
pub fn fairplay_m4a(variant: u32) -> Vec<u8> {
    let mut b = fs::read(fixture(Kind::Aac, variant)).unwrap();
    let stsd = b.windows(4).position(|w| w == b"stsd").expect("stsd atom");
    // stsd: fourcc, version/flags (4), entry count (4), then entry size (4) and format.
    let at = stsd + 4 + 4 + 4 + 4;
    assert_eq!(&b[at..at + 4], b"mp4a");
    b[at..at + 4].copy_from_slice(b"drms");
    b
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
        // The fake iPods are folders in a temp dir.
        cfg.device.allow_folder = true;
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

    /// Writes `bytes` to `rel` under the music folder.
    pub fn add_bytes(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let dest = self.music.join(rel);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, bytes).unwrap();
        dest
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

// ---- iPod databases built by libgpod

/// Compiles `crates/gpod/tests/c/make_ipod.c` once per test binary. The C
/// program uses only libgpod, so the fixture does not depend on the Rust code
/// under test.
fn make_ipod_tool() -> PathBuf {
    static TOOL: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    TOOL.get_or_init(|| {
        let flags = Command::new("pkg-config")
            .args(["--cflags", "--libs", "libgpod-1.0", "glib-2.0"])
            .output()
            .expect("pkg-config");
        assert!(flags.status.success(), "pkg-config libgpod-1.0");
        let flags = String::from_utf8(flags.stdout).unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../gpod/tests/c/make_ipod.c");
        let dir = target_tmp().join("c-tools");
        fs::create_dir_all(&dir).unwrap();
        // Several test binaries may build it at once: private name, then rename.
        let tmp = dir.join(format!(".make_ipod-{}", std::process::id()));
        let status = Command::new("gcc")
            .arg("-o")
            .arg(&tmp)
            .arg(&src)
            .args(flags.split_whitespace())
            .status()
            .expect("gcc");
        assert!(status.success(), "compiling {}", src.display());
        let out = dir.join(format!("make_ipod-{}", std::process::id()));
        fs::rename(&tmp, &out).unwrap();
        out
    })
    .clone()
}

/// One iPod track for [`write_itunesdb`].
#[derive(Clone, Default)]
pub struct IpodSpec {
    pub dbid: u64,
    pub size: u64,
    pub ms: u32,
    pub title: &'static str,
    pub artist: &'static str,
    pub album: &'static str,
    pub plays: u32,
}

/// An iPod playlist (`smart` or regular) with members by dbid, in order.
pub struct PlaylistSpec {
    pub name: &'static str,
    pub smart: bool,
    /// The Podcasts playlist, which iprune never manages.
    pub podcasts: bool,
    pub members: Vec<u64>,
}

pub fn ipod_playlist(name: &'static str, members: &[u64]) -> PlaylistSpec {
    PlaylistSpec {
        name,
        smart: false,
        podcasts: false,
        members: members.to_vec(),
    }
}

/// Writes an iTunesDB at `db` with libgpod, from the given tracks and playlists.
pub fn write_itunesdb(db: &Path, master: &str, tracks: &[IpodSpec], playlists: &[PlaylistSpec]) {
    let mut manifest = format!("M\t{master}\n");
    for t in tracks {
        manifest += &format!(
            "T\t{:x}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            t.dbid, t.size, t.ms, t.title, t.artist, t.album, t.plays
        );
    }
    for p in playlists {
        let members: Vec<String> = p.members.iter().map(|d| format!("{d:x}")).collect();
        manifest += &format!(
            "{}\t{}\t{}\n",
            if p.podcasts {
                "C"
            } else if p.smart {
                "S"
            } else {
                "P"
            },
            p.name,
            members.join(",")
        );
    }
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("manifest.tsv");
    fs::write(&file, manifest).unwrap();
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let out = Command::new(make_ipod_tool())
        .arg(db)
        .arg(&file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "make_ipod failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A fake iPod tree: `<dir>/iPod_Control/iTunes/iTunesDB` from the given
/// tracks and playlists, plus a `SysInfo` when given. Returns the mountpoint.
pub fn fake_ipod(
    dir: &Path,
    sysinfo: Option<&str>,
    tracks: &[IpodSpec],
    playlists: &[PlaylistSpec],
) -> PathBuf {
    let ctl = dir.join("iPod_Control");
    fs::create_dir_all(ctl.join("Device")).unwrap();
    // The F00 to F49 folders of a real iPod, which libgpod copies files into.
    for n in 0..50 {
        fs::create_dir_all(ctl.join(format!("Music/F{n:02}"))).unwrap();
    }
    write_itunesdb(
        &ctl.join("iTunes/iTunesDB"),
        "iPod de Test",
        tracks,
        playlists,
    );
    // Each track's file, where `make_ipod.c` points it, of its size (sparse):
    // an entry with no file is a damaged iPod, which a sync repairs.
    for (serial, t) in tracks.iter().enumerate() {
        let f = fs::File::create(ctl.join(format!("Music/F{:02}/T{serial:04}.mp3", serial % 50)))
            .unwrap();
        f.set_len(t.size).unwrap();
    }
    if let Some(s) = sysinfo {
        fs::write(ctl.join("Device/SysInfo"), s).unwrap();
    }
    dir.to_owned()
}

/// Where fixtures and tools are built. Cargo sets `CARGO_TARGET_TMPDIR` for
/// integration tests; a unit-test binary that includes this file (the GUI
/// crate's) uses the workspace's `target/tmp` instead: removed by `cargo
/// clean`, and not shared with other users as `/tmp` would be.
fn target_tmp() -> &'static Path {
    Path::new(
        option_env!("CARGO_TARGET_TMPDIR")
            .unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/tmp")),
    )
}

// ---- cover art on a fake iPod

/// Compiles `c_art.c` once per test binary.
fn art_tool() -> PathBuf {
    static TOOL: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    TOOL.get_or_init(|| {
        let flags = Command::new("pkg-config")
            .args([
                "--cflags",
                "--libs",
                "libgpod-1.0",
                "gdk-pixbuf-2.0",
                "glib-2.0",
            ])
            .output()
            .expect("pkg-config");
        assert!(
            flags.status.success(),
            "pkg-config libgpod-1.0 gdk-pixbuf-2.0"
        );
        let flags = String::from_utf8(flags.stdout).unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/c_art.c");
        let dir = target_tmp().join("c-tools");
        fs::create_dir_all(&dir).unwrap();
        let out = dir.join(format!("c_art-{}", std::process::id()));
        let status = Command::new("gcc")
            .arg("-o")
            .arg(&out)
            .arg(&src)
            .args(flags.split_whitespace())
            .status()
            .expect("gcc");
        assert!(status.success(), "compiling {}", src.display());
        out
    })
    .clone()
}

fn run_art(verb: &str, root: &Path) -> String {
    let o = Command::new(art_tool())
        .arg(verb)
        .arg(root)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "c_art {verb}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

/// Turns the fake iPod at `root` into an artwork-capable one (a Classic model
/// in `SysInfo`, which keeps `sysinfo_rest`, the GUID line) and gives every
/// track but the one with dbid 0x9999 its own colour cover, written by
/// libgpod: an `ArtworkDB` and `F*_*.ithmb` files.
pub fn add_cover_art(root: &Path, sysinfo_rest: &str) {
    fs::write(
        root.join("iPod_Control/Device/SysInfo"),
        format!("ModelNumStr: xC297\n{sysinfo_rest}"),
    )
    .unwrap();
    run_art("add", root);
}

/// What libgpod reads back as each track's cover: one line per track,
/// `<dbid hex> <title> <rgb hex>` or `NONE`.
pub fn read_covers(root: &Path) -> String {
    run_art("show", root)
}

// ---- damaged databases

fn u32_at(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize
}

fn add_len(b: &mut [u8], at: usize, delta: isize) {
    let v = (u32_at(b, at) as isize + delta) as u32;
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Rewrites, in the iTunesDB file `db`, the iPod path of the `k`-th track in
/// database order (`:iPod_Control:Music:..`) to `path`, resizing the
/// containers that hold it. libgpod writes only paths it chose itself, so a
/// damaged database (a path with `..`, one naming iTunesDB, two tracks
/// sharing a file) has to be made by editing the bytes.
pub fn set_ipod_path(db: &Path, k: usize, path: &str) {
    let mut b = fs::read(db).unwrap();
    assert_eq!(&b[0..4], b"mhbd");
    let mut sd = u32_at(&b, 4);
    // The track list: the mhsd of type 1.
    while &b[sd..sd + 4] == b"mhsd" && u32_at(&b, sd + 12) != 1 {
        sd += u32_at(&b, sd + 8);
    }
    assert_eq!(&b[sd..sd + 4], b"mhsd", "no track list");
    let mhlt = sd + u32_at(&b, sd + 4);
    assert_eq!(&b[mhlt..mhlt + 4], b"mhlt");
    assert!(k < u32_at(&b, mhlt + 8), "no track {k}");
    let mut hit = mhlt + u32_at(&b, mhlt + 4);
    for _ in 0..k {
        hit += u32_at(&b, hit + 8);
    }
    assert_eq!(&b[hit..hit + 4], b"mhit");
    let mut od = hit + u32_at(&b, hit + 4);
    for _ in 0..u32_at(&b, hit + 12) {
        assert_eq!(&b[od..od + 4], b"mhod");
        if u32_at(&b, od + 12) == 2 {
            let old = u32_at(&b, od + 28);
            let units: Vec<u8> = path.encode_utf16().flat_map(u16::to_le_bytes).collect();
            let delta = units.len() as isize - old as isize;
            let start = od + 40;
            b.splice(start..start + old, units.iter().copied());
            b[od + 28..od + 32].copy_from_slice(&(units.len() as u32).to_le_bytes());
            for at in [od + 8, hit + 8, sd + 8, 8] {
                add_len(&mut b, at, delta);
            }
            fs::write(db, b).unwrap();
            return;
        }
        od += u32_at(&b, od + 8);
    }
    panic!("track {k} has no location");
}
