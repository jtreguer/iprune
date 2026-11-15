//! Helpers for the import tests: a temp library, inbox, playlists folder and
//! data folder, and a fake `autotag serve` (`fake_autotag.py`, standard
//! library only) named in `import.autotag`. Nothing here touches the network
//! or the real home.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use iprune_core::config::Paths;
use iprune_core::import::{self, Autotag, FileReport, Inbox, Index, View};
use serde_json::{Value, json};

use super::{Env, Kind, fixture_secs, target_tmp};

const SCRIPT: &str = include_str!("fake_autotag.py");

/// The fake autotag, written once (named after its content, renamed into
/// place) and shared by every test binary.
pub fn fake_autotag() -> PathBuf {
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(write_fake_autotag).clone()
}

fn write_fake_autotag() -> PathBuf {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    SCRIPT.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());
    let dir = target_tmp().join("fake-autotag");
    let path = dir.join(format!("autotag-{}.py", hash));
    if path.exists() {
        return path;
    }
    fs::create_dir_all(&dir).unwrap();
    let tmp = dir.join(format!(".{}-{}.tmp", std::process::id(), hash));
    fs::write(&tmp, SCRIPT).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::rename(&tmp, &path).unwrap();
    path
}

/// Where the fake looks for its scenario, and writes its log and pid: next
/// to the library root.
pub fn scenario_dir(root: &Path) -> PathBuf {
    root.parent().unwrap().to_owned()
}

pub fn write_scenario(root: &Path, scenario: &Value) {
    fs::write(
        scenario_dir(root).join("fake-autotag.json"),
        scenario.to_string(),
    )
    .unwrap();
}

/// The requests the fake received, as parsed JSON.
pub fn requests(root: &Path) -> Vec<Value> {
    match fs::read_to_string(scenario_dir(root).join("fake-autotag.log")) {
        Ok(text) => text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// `(pid, process group, parent pid)` of the last fake started.
pub fn fake_pids(root: &Path) -> (u32, u32, u32) {
    let text = fs::read_to_string(scenario_dir(root).join("fake-autotag.pid")).unwrap();
    let v: Vec<u32> = text
        .split_whitespace()
        .map(|n| n.parse().unwrap())
        .collect();
    (v[0], v[1], v[2])
}

pub fn process_exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// A candidate of a lookup answer, with the tags a title and artist give.
pub fn candidate(recording: &str, title: &str, artist: &str, certain: bool, dest: &str) -> Value {
    json!({
        "recording": recording,
        "release": format!("rel-{recording}"),
        "label": format!("{artist} - {title}"),
        "length_ms": 2000,
        "certain": certain,
        "tags": {"title": title, "artist": artist},
        "destination": dest,
        "same_title": [],
    })
}

/// A lookup answer holding `candidates`.
pub fn answer(candidates: Vec<Value>) -> Value {
    json!({
        "by": "name",
        "query": {"artist": "", "title": ""},
        "duration_ms": 2000,
        "candidates": candidates,
    })
}

/// Where the fake files `Title<n>` by `Artist<n>`.
pub fn dest(n: u32) -> String {
    format!("Artist{n}/Album{n}/{n:02} Title{n}.mp3")
}

/// A scenario in which each `(file, n)` has one certain candidate, the song
/// `Title<n>` by `Artist<n>` (the tags ffmpeg gave the fixture), filed at
/// `dest(n)`.
pub fn scenario(files: &[(&str, u32)]) -> Value {
    let mut lookups = serde_json::Map::new();
    let mut destinations = serde_json::Map::new();
    for (name, n) in files {
        // The fake knows files by their name alone.
        let name = Path::new(name).file_name().unwrap().to_str().unwrap();
        let c = candidate(
            &format!("rec-{n}"),
            &format!("Title{n}"),
            &format!("Artist{n}"),
            true,
            &dest(*n),
        );
        lookups.insert(name.to_owned(), answer(vec![c]));
        destinations.insert(name.to_owned(), json!(dest(*n)));
    }
    json!({"lookups": lookups, "destinations": destinations})
}

/// A throwaway library (one 1 s MP3 in `music/` so that it is not empty),
/// playlists folder, inbox and data folder, with the fake autotag configured.
pub struct Imp {
    pub env: Env,
    pub base: PathBuf,
    pub inbox: PathBuf,
    pub paths: Paths,
    /// The library file that keeps the root from being empty.
    pub seed: PathBuf,
}

impl Imp {
    pub fn new() -> Self {
        let mut env = Env::new();
        let base = env._dir.path().canonicalize().unwrap();
        let inbox = base.join("inbox");
        env.cfg.import.inbox = inbox.clone();
        env.cfg.import.autotag = fake_autotag();
        let seed = env.add("Seed/Seed.mp3", Kind::Mp3, 90);
        let paths = Paths {
            config_file: base.join("config/config.toml"),
            data_dir: base.join("data"),
        };
        Imp {
            env,
            base,
            inbox,
            paths,
            seed,
        }
    }

    pub fn music(&self) -> &Path {
        &self.env.music
    }

    pub fn scenario(&self, scenario: Value) {
        write_scenario(&self.env.music, &scenario);
    }

    pub fn requests(&self) -> Vec<Value> {
        requests(&self.env.music)
    }

    /// Only the requests of one op.
    pub fn requests_of(&self, op: &str) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|r| r["op"] == op)
            .collect()
    }

    /// Copies an MP3 fixture into the inbox: tone, `Title<n>` and `Artist<n>`
    /// of variant `n`, lasting `n + 1` seconds, so that no two have the same
    /// audio properties, nor those of the 1 s library files.
    pub fn put(&self, rel: &str, n: u32) -> PathBuf {
        let dest = self.inbox.join(rel);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::copy(fixture_secs(Kind::Mp3, n, n + 1), &dest).unwrap();
        dest
    }

    pub fn start(&self) -> iprune_core::Result<Autotag> {
        Autotag::start(&self.env.cfg, &self.paths)
    }

    pub fn read(&self) -> Inbox {
        import::read_inbox(self.env.store.conn(), &self.env.cfg).unwrap()
    }

    pub fn index(&self) -> Index {
        Index::load(self.env.store.conn()).unwrap()
    }

    /// `(file name relative to the inbox, view)` of every entry.
    pub fn views(&self) -> Vec<(String, View)> {
        let inbox = self.read();
        let index = self.index();
        inbox
            .entries
            .iter()
            .map(|e| {
                (
                    e.path
                        .strip_prefix(&inbox.dir)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                    import::view(e, &index),
                )
            })
            .collect()
    }

    pub fn view_of(&self, rel: &str) -> View {
        self.views()
            .into_iter()
            .find(|(n, _)| n == rel)
            .unwrap_or_else(|| panic!("{rel} is not in the inbox"))
            .1
    }

    pub fn item_id(&self, rel: &str) -> i64 {
        let path = self.inbox.join(rel);
        import::items(self.env.store.conn())
            .unwrap()
            .into_iter()
            .find(|i| i.path == path)
            .unwrap_or_else(|| panic!("no row for {rel}"))
            .id
    }

    pub fn item(&self, rel: &str) -> import::Item {
        import::item(self.env.store.conn(), self.item_id(rel))
            .unwrap()
            .unwrap()
    }

    /// Reads the inbox and looks every pending file up.
    pub fn look_up_all(&self, autotag: &mut Autotag, retry: bool) -> usize {
        let inbox = self.read();
        let index = self.index();
        let pending = import::pending(&inbox, &index, retry);
        import::look_up(
            self.env.store.conn(),
            &pending,
            autotag,
            &mut |_, _, _| {},
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    pub fn file(&mut self, playlist: Option<&str>) -> iprune_core::Result<FileReport> {
        self.file_with(playlist, &mut |_, _, _| {}, &AtomicBool::new(false))
    }

    pub fn file_with(
        &mut self,
        playlist: Option<&str>,
        progress: &mut dyn FnMut(usize, usize, &Path),
        stop: &AtomicBool,
    ) -> iprune_core::Result<FileReport> {
        let mut autotag = self.start()?;
        import::file(
            &mut self.env.store,
            &self.env.cfg,
            &mut autotag,
            playlist,
            progress,
            stop,
        )
    }

    /// The present tracks' paths, relative to the music folder, sorted.
    pub fn library(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .env
            .tracks()
            .into_iter()
            .filter(|t| t.missing_since.is_none())
            .map(|t| {
                t.path
                    .strip_prefix(&self.env.music)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        v.sort();
        v
    }

    /// Files under the inbox, relative, sorted.
    pub fn inbox_files(&self) -> Vec<String> {
        let mut v = Vec::new();
        files_under(&self.inbox, &self.inbox, &mut v);
        v.sort();
        v
    }
}

/// Every file under `dir` (hidden ones too), relative to `root`.
pub fn files_under(root: &Path, dir: &Path, out: &mut Vec<String>) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            files_under(root, &p, out);
        } else {
            out.push(p.strip_prefix(root).unwrap().to_str().unwrap().to_owned());
        }
    }
}
