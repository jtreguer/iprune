//! The autotag process and its protocol: JSON lines, one request per line
//! on its stdin and one answer per line on its stdout (technical document,
//! section 7.2; `tools/tests/protocol` holds examples both sides are tested
//! against).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{Config, Paths};
use crate::lock::{self, ImportLock};
use crate::tags::Tags;
use crate::{Error, Result};

/// The protocol version this iprune speaks.
pub const PROTOCOL: u64 = 1;

/// Lines of autotag's stderr kept, for the message when it fails.
const STDERR_LINES: usize = 20;

/// How long autotag gets to end by itself once its stdin is closed.
const GRACE: Duration = Duration::from_secs(2);

/// Tags as the protocol carries them, named as in [`Tags`]. A field left
/// out is left as the file has it, so an import cannot clear a field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_no: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_total: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disc_no: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disc_total: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compilation: Option<bool>,
}

impl Fields {
    /// `base` with these fields written over it, as autotag writes them.
    pub fn over(&self, base: &Tags) -> Tags {
        let text = |new: &Option<String>, old: &Option<String>| match new
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(s) => Some(s.to_owned()),
            None => old.clone(),
        };
        Tags {
            title: text(&self.title, &base.title),
            artist: text(&self.artist, &base.artist),
            album: text(&self.album, &base.album),
            album_artist: text(&self.album_artist, &base.album_artist),
            composer: text(&self.composer, &base.composer),
            genre: text(&self.genre, &base.genre),
            year: self.year.or(base.year),
            track_no: self.track_no.or(base.track_no),
            track_total: self.track_total.or(base.track_total),
            disc_no: self.disc_no.or(base.disc_no),
            disc_total: self.disc_total.or(base.disc_total),
            compilation: self.compilation.unwrap_or(base.compilation),
        }
    }

    /// The fields whose value in `tags` differs from the one sent: what a
    /// read-back after filing checks.
    pub fn differences(&self, tags: &Tags) -> Vec<&'static str> {
        let mut out = Vec::new();
        let mut text = |name, sent: &Option<String>, read: &Option<String>| {
            if let Some(s) = sent.as_deref().map(str::trim).filter(|s| !s.is_empty())
                && read.as_deref().map(str::trim) != Some(s)
            {
                out.push(name);
            }
        };
        text("title", &self.title, &tags.title);
        text("artist", &self.artist, &tags.artist);
        text("album", &self.album, &tags.album);
        text("album artist", &self.album_artist, &tags.album_artist);
        text("composer", &self.composer, &tags.composer);
        text("genre", &self.genre, &tags.genre);
        for (name, sent, read) in [
            ("year", self.year, tags.year),
            ("track", self.track_no, tags.track_no),
            ("track total", self.track_total, tags.track_total),
            ("disc", self.disc_no, tags.disc_no),
            ("disc total", self.disc_total, tags.disc_total),
        ] {
            if sent.is_some() && sent != read {
                out.push(name);
            }
        }
        if self.compilation.is_some_and(|c| c != tags.compilation) {
            out.push("compilation");
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum By {
    Fingerprint,
    Name,
}

/// Artist and title: what a lookup searched for, or what Search again asks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Query {
    pub artist: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Candidate {
    pub recording: String,
    pub release: String,
    pub label: String,
    pub length_ms: Option<u64>,
    /// The 10-second rule: found by fingerprint, or by name with a length
    /// within 10 seconds of the file's.
    pub certain: bool,
    pub tags: Fields,
    /// Relative to the library root; none when it is not mounted.
    pub destination: Option<String>,
    pub same_title: Vec<String>,
    /// The genre was not looked up (`"genre": null`); `tags` asks for it.
    #[serde(skip)]
    pub genre_unknown: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Lookup {
    pub by: By,
    pub query: Query,
    pub duration_ms: u64,
    pub candidates: Vec<Candidate>,
}

impl Lookup {
    /// A lookup answer, as autotag sent it or as stored in the index.
    pub fn parse(text: &str) -> serde_json::Result<Lookup> {
        let value: Value = serde_json::from_str(text)?;
        // A genre left out means MusicBrainz has none; null means it was
        // not looked up. serde reads both as None.
        let unknown: Vec<bool> = value["candidates"]
            .as_array()
            .map(|cs| {
                cs.iter()
                    .map(|c| c["tags"].get("genre").is_some_and(Value::is_null))
                    .collect()
            })
            .unwrap_or_default();
        let mut lookup: Lookup = serde_json::from_value(value)?;
        for (c, unknown) in lookup.candidates.iter_mut().zip(unknown) {
            c.genre_unknown = unknown;
        }
        Ok(lookup)
    }
}

/// What `apply` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// Filed at this path, relative to the library root.
    Filed { destination: String },
    /// A dry run: where it would go, and the files of the same title there.
    WouldFile {
        destination: String,
        same_title: Vec<String>,
    },
    /// The destination is taken, or files of the same title exist and
    /// duplicates were not allowed.
    Refused {
        reason: String,
        destination: Option<String>,
        same_title: Vec<String>,
    },
}

impl Applied {
    fn parse(v: &Value) -> Result<Applied> {
        let bad = || Error::Autotag(format!("unexpected answer to apply: {v}"));
        let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let same_title = match v.get("same_title") {
            None => Vec::new(),
            Some(s) => serde_json::from_value(s.clone()).map_err(|_| bad())?,
        };
        if let Some(reason) = text("refused") {
            return Ok(Applied::Refused {
                reason,
                destination: text("destination"),
                same_title,
            });
        }
        let destination = text("destination").ok_or_else(bad)?;
        match v.get("filed").and_then(Value::as_bool) {
            Some(true) => Ok(Applied::Filed { destination }),
            Some(false) => Ok(Applied::WouldFile {
                destination,
                same_title,
            }),
            None => Err(bad()),
        }
    }
}

/// autotag's first line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Hello {
    pub protocol: u64,
    pub formats: Vec<String>,
    /// It has an AcoustID key, so it can look songs up by fingerprint.
    pub acoustid: bool,
    pub fpcalc: bool,
}

/// The program to run: `import.autotag`, else `autotag` next to the running
/// binary (where `packaging/install.sh` puts it), else on `PATH`.
pub fn program(cfg: &Config) -> Result<PathBuf> {
    if !cfg.import.autotag.as_os_str().is_empty() {
        return Ok(cfg.import.autotag.clone());
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("autotag")));
    let on_path: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("autotag"))
                .collect()
        })
        .unwrap_or_default();
    beside
        .into_iter()
        .chain(on_path)
        .find(|p| p.is_file())
        .ok_or_else(|| {
            Error::Autotag(
                "not found next to iprune nor on PATH; set import.autotag in the config, or \
                 install it with packaging/install.sh"
                    .into(),
            )
        })
}

/// An autotag process, with `import.lock` held while it runs. Dropping it
/// closes autotag's stdin, which ends it after the request it is on, and
/// kills it if it has not ended [`GRACE`] later.
pub struct Autotag {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    next_id: u64,
    pub hello: Hello,
    _lock: ImportLock,
}

/// A program just written can still be open for writing in a child another
/// thread forked meanwhile: try again a few times.
fn spawn(cmd: &mut Command) -> std::io::Result<Child> {
    let mut tries = 0;
    loop {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 10 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            r => return r,
        }
    }
}

impl Autotag {
    /// Starts `autotag serve` for the import root and reads its first line.
    pub fn start(cfg: &Config, paths: &Paths) -> Result<Autotag> {
        let lock = lock::import(paths)?;
        let program = program(cfg)?;
        let root = cfg
            .import_root()
            .ok_or_else(|| Error::Autotag("no library root to file into".into()))?;
        let mut child = spawn(
            Command::new(&program)
                .arg("serve")
                .arg("--library")
                .arg(root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                // Ctrl+C in the terminal reaches iprune only, which lets a
                // song being filed finish before it stops.
                .process_group(0),
        )
        .map_err(|e| Error::Autotag(format!("{} could not start: {e}", program.display())))?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            unreachable!("all three are piped");
        };
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let kept = Arc::clone(&tail);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr)
                .lines()
                .map_while(std::io::Result::ok)
            {
                let mut q = kept.lock().unwrap_or_else(|e| e.into_inner());
                if q.len() == STDERR_LINES {
                    q.pop_front();
                }
                q.push_back(line);
            }
        });
        let mut autotag = Autotag {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            stderr: tail,
            next_id: 1,
            hello: Hello::default(),
            _lock: lock,
        };
        let first = autotag.read_answer()?;
        let hello: Hello = serde_json::from_value(first.clone())
            .map_err(|_| Error::Autotag(format!("unexpected first line: {first}")))?;
        if hello.protocol != PROTOCOL {
            return Err(Error::Autotag(format!(
                "{} speaks protocol {}, this iprune {PROTOCOL}; install both again \
                 (packaging/install.sh)",
                program.display(),
                hello.protocol
            )));
        }
        autotag.hello = hello;
        Ok(autotag)
    }

    /// What autotag wrote to stderr lately, for an error message.
    fn stderr_tail(&self) -> String {
        // Its last lines may still be on their way.
        std::thread::sleep(Duration::from_millis(50));
        let q = self.stderr.lock().unwrap_or_else(|e| e.into_inner());
        q.iter().cloned().collect::<Vec<_>>().join("\n")
    }

    /// autotag is gone or broke the protocol: the error, with its stderr.
    fn failed(&mut self, what: String) -> Error {
        let status = match self.child.try_wait() {
            Ok(Some(s)) => format!(" ({s})"),
            _ => String::new(),
        };
        let tail = self.stderr_tail();
        let tail = if tail.is_empty() {
            String::new()
        } else {
            format!(":\n{tail}")
        };
        Error::Autotag(format!("{what}{status}{tail}"))
    }

    fn read_answer(&mut self) -> Result<Value> {
        let mut line = String::new();
        match self.stdout.read_line(&mut line) {
            Ok(0) => {
                // Gone: wait for its status, briefly.
                let deadline = Instant::now() + GRACE;
                while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(self.failed("stopped".into()))
            }
            Ok(_) => serde_json::from_str(&line).map_err(|_| {
                let shown: String = line.trim_end().chars().take(200).collect();
                self.failed(format!("wrote a line that is not JSON: {shown}"))
            }),
            Err(e) => Err(self.failed(format!("cannot be read: {e}"))),
        }
    }

    /// Sends one request and reads its answer. The outer error is autotag
    /// failing; the inner one, autotag answering this request with an
    /// error.
    fn request(&mut self, mut req: Value) -> Result<std::result::Result<Value, String>> {
        let id = self.next_id;
        self.next_id += 1;
        req["id"] = json!(id);
        let written = match self.stdin.as_mut() {
            Some(stdin) => writeln!(stdin, "{req}").and_then(|()| stdin.flush()),
            None => Err(std::io::Error::other("already stopped")),
        };
        if let Err(e) = written {
            return Err(self.failed(format!("cannot be written to: {e}")));
        }
        let mut answer = self.read_answer()?;
        if answer.get("id") != Some(&json!(id)) {
            return Err(self.failed(format!(
                "answered request {id} with id {}",
                answer.get("id").unwrap_or(&Value::Null)
            )));
        }
        if let Some(o) = answer.as_object_mut() {
            o.remove("id");
        }
        match answer.get("error") {
            Some(e) => Ok(Err(e.as_str().map_or_else(|| e.to_string(), str::to_owned))),
            None => Ok(Ok(answer)),
        }
    }

    /// Looks a file up, with the artist and title of Search again when
    /// given. On success, the answer parsed and as text, to be stored.
    pub fn lookup(
        &mut self,
        file: &Path,
        search: Option<&Query>,
    ) -> Result<std::result::Result<(Lookup, String), String>> {
        let mut req = json!({"op": "lookup", "file": utf8(file)?});
        if let Some(q) = search {
            for (k, v) in [("artist", &q.artist), ("title", &q.title)] {
                if !v.trim().is_empty() {
                    req[k] = json!(v.trim());
                }
            }
        }
        Ok(match self.request(req)? {
            Err(e) => Err(e),
            Ok(answer) => {
                let text = answer.to_string();
                let lookup = Lookup::parse(&text)
                    .map_err(|e| Error::Autotag(format!("unexpected answer to lookup ({e})")))?;
                Ok((lookup, text))
            }
        })
    }

    /// A candidate's full tags, genre included.
    pub fn tags(
        &mut self,
        recording: &str,
        release: &str,
    ) -> Result<std::result::Result<Fields, String>> {
        Ok(
            match self.request(json!({"op": "tags", "recording": recording, "release": release}))? {
                Err(e) => Err(e),
                Ok(answer) => Ok(serde_json::from_value(answer["tags"].clone())
                    .map_err(|e| Error::Autotag(format!("unexpected answer to tags ({e})")))?),
            },
        )
    }

    /// Files a tagged copy of `file` into the library and deletes `file`;
    /// with `dry_run`, only says where it would go. `ids` are the
    /// MusicBrainz recording and release, written with the tags.
    pub fn apply(
        &mut self,
        file: &Path,
        fields: &Fields,
        ids: Option<(&str, &str)>,
        dry_run: bool,
        allow_duplicate: bool,
    ) -> Result<std::result::Result<Applied, String>> {
        let mut req = json!({
            "op": "apply",
            "file": utf8(file)?,
            "tags": fields,
            "dry_run": dry_run,
            "allow_duplicate": allow_duplicate,
        });
        if let Some((recording, release)) = ids {
            req["recording"] = json!(recording);
            req["release"] = json!(release);
        }
        Ok(match self.request(req)? {
            Err(e) => Err(e),
            Ok(answer) => Ok(Applied::parse(&answer)?),
        })
    }
}

impl Drop for Autotag {
    fn drop(&mut self) {
        // End of input: autotag finishes the request it is on, then exits.
        self.stdin.take();
        let deadline = Instant::now() + GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                _ => {
                    // The whole group: `uv run` keeps python as its child.
                    // autotag leads its own group (`process_group(0)`).
                    if let Some(pid) = i32::try_from(self.child.id())
                        .ok()
                        .and_then(rustix::process::Pid::from_raw)
                    {
                        let _ =
                            rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                    }
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
            }
        }
    }
}

fn utf8(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| Error::NotUtf8(path.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: Value) -> Result<Applied> {
        Applied::parse(&v)
    }

    #[test]
    fn a_filed_answer_has_a_destination_and_filed_true() {
        let got = parse(json!({"destination": "A/B/01 C.mp3", "same_title": [], "filed": true}));
        assert_eq!(
            got.unwrap(),
            Applied::Filed {
                destination: "A/B/01 C.mp3".into()
            }
        );
    }

    #[test]
    fn a_dry_run_answer_says_where_and_what_of_the_same_title_is_there() {
        let got = parse(json!({
            "destination": "A/B/01 C.mp3", "same_title": ["A/X/02 C.mp3"], "filed": false
        }));
        assert_eq!(
            got.unwrap(),
            Applied::WouldFile {
                destination: "A/B/01 C.mp3".into(),
                same_title: vec!["A/X/02 C.mp3".into()],
            }
        );
        // A missing same_title is an empty list.
        let got = parse(json!({"destination": "d.mp3", "filed": false})).unwrap();
        assert_eq!(
            got,
            Applied::WouldFile {
                destination: "d.mp3".into(),
                same_title: vec![]
            }
        );
    }

    #[test]
    fn a_refusal_carries_its_reason_and_whatever_else_autotag_said() {
        let got = parse(json!({
            "refused": "already in the library: d.mp3", "destination": "d.mp3",
            "same_title": ["d.mp3"]
        }));
        assert_eq!(
            got.unwrap(),
            Applied::Refused {
                reason: "already in the library: d.mp3".into(),
                destination: Some("d.mp3".into()),
                same_title: vec!["d.mp3".into()],
            }
        );
        // A refusal is a refusal even without a destination or `filed`.
        let got = parse(json!({"refused": "no"})).unwrap();
        assert_eq!(
            got,
            Applied::Refused {
                reason: "no".into(),
                destination: None,
                same_title: vec![]
            }
        );
        // And it wins over a `filed` that should not be there.
        let got = parse(json!({"refused": "no", "destination": "d", "filed": true})).unwrap();
        assert!(matches!(got, Applied::Refused { .. }));
    }

    #[test]
    fn an_answer_of_no_known_shape_is_an_autotag_error_not_a_filed_song() {
        for bad in [
            json!({}),
            json!({"filed": true}),                      // no destination
            json!({"destination": "d.mp3"}),             // no filed
            json!({"destination": "d.mp3", "filed": 1}), // not a boolean
            json!({"destination": "d.mp3", "filed": "true"}),
            json!({"destination": 7, "filed": true}),
            json!({"destination": "d", "filed": true, "same_title": "x"}),
            json!({"destination": "d", "filed": true, "same_title": [1]}),
        ] {
            let e = parse(bad.clone()).unwrap_err();
            assert!(
                matches!(&e, Error::Autotag(m) if m.contains("unexpected answer to apply")),
                "{bad}: {e:?}"
            );
        }
    }

    /// Contract: every apply answer in tools/tests/protocol parses, to the
    /// variant its keys say, with its destination and same_title.
    #[test]
    fn every_apply_answer_of_the_protocol_contract_parses() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/tests/protocol");
        let mut seen = (0, 0, 0);
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        files.sort();
        for f in files {
            for (n, line) in std::fs::read_to_string(&f).unwrap().lines().enumerate() {
                let v: Value = serde_json::from_str(line).unwrap();
                if v["request"]["op"] != "apply" || v["answer"].get("error").is_some() {
                    continue;
                }
                let at = format!("{}:{}", f.display(), n + 1);
                let ans = &v["answer"];
                let same: Vec<String> = ans["same_title"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s.as_str().unwrap().to_owned())
                    .collect();
                let dest = ans["destination"].as_str().unwrap().to_owned();
                match parse(ans.clone()).unwrap_or_else(|e| panic!("{at}: {e}")) {
                    Applied::Filed { destination } => {
                        assert_eq!(ans["filed"], true, "{at}");
                        assert_eq!(destination, dest, "{at}");
                        seen.0 += 1;
                    }
                    Applied::WouldFile {
                        destination,
                        same_title,
                    } => {
                        assert_eq!(ans["filed"], false, "{at}");
                        assert_eq!((destination, same_title), (dest, same), "{at}");
                        seen.1 += 1;
                    }
                    Applied::Refused {
                        reason,
                        destination,
                        same_title,
                    } => {
                        assert_eq!(ans["refused"].as_str(), Some(reason.as_str()), "{at}");
                        assert_eq!((destination, same_title), (Some(dest), same), "{at}");
                        seen.2 += 1;
                    }
                }
            }
        }
        assert!(
            seen.0 >= 2 && seen.1 >= 2 && seen.2 >= 3,
            "filed, would file, refused: {seen:?}"
        );
    }
}
