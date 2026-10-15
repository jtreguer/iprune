//! `exec::sync` on a fake iPod whose Podcasts playlist holds many shows, and
//! on a track whose title contains " mhii " (audit H8, L50). The iPod, the
//! index, the music folder and the data folder are all in temp dirs.
//!
//! libgpod writes the Podcasts playlist grouped by show through a hash table,
//! so its member order read back differs from the order in memory; the
//! verification after the write must not count that as a difference.

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use common::{
    Env, IpodSpec, Kind, PlaylistSpec, add_cover_art, fake_ipod, id3v24_text, ipod_playlist,
    mp3_with_id3v2,
};
use iprune_core::config::Paths;
use iprune_core::exec::{self, Options, Outcome};
use iprune_core::{adoption, ipod, playlists, sync};

const SYSINFO: &str = "FirewireGuid: 0x000A27001234ABCD\n";
/// The dbid of the first managed track; the second is one more.
const FIRST: u64 = 0x1001;
const SECOND: u64 = 0x1002;
const EPISODE_BASE: u64 = 0x5000;

struct Setup {
    /// The album (show) of each podcast episode, one episode per show.
    shows: Vec<String>,
    /// The first managed track is the only episode of `shows[0]`, instead of
    /// a separate unmanaged episode.
    managed_episode: bool,
    /// The title of the second managed track, in its file and on the iPod.
    title: &'static str,
    /// Every track gets a cover.
    art: bool,
}

struct World {
    env: Env,
    files: Vec<PathBuf>,
    _ipod_dir: tempfile::TempDir,
    _data_dir: tempfile::TempDir,
    root: PathBuf,
    paths: Paths,
    /// The dbids of the Podcasts playlist, as written.
    podcasts: Vec<u64>,
}

impl World {
    fn new(s: Setup) -> World {
        let mut env = Env::new();
        let first = env.add("s1.mp3", Kind::Mp3, 1);
        let second = env.add_bytes(
            "s2.mp3",
            &mp3_with_id3v2(
                2,
                &[
                    id3v24_text("TIT2", 3, s.title.as_bytes()),
                    id3v24_text("TPE1", 3, b"Artist2"),
                ],
            ),
        );
        let files = vec![first, second];
        env.scan();
        let mut tracks: Vec<IpodSpec> = Vec::new();
        for (k, f) in files.iter().enumerate() {
            let t = env.track(f);
            tracks.push(IpodSpec {
                dbid: FIRST + k as u64,
                size: t.size,
                ms: t.info.duration_ms.unwrap(),
                title: if k == 0 { "Title1" } else { s.title },
                artist: if k == 0 { "Artist1" } else { "Artist2" },
                album: if k == 0 && s.managed_episode {
                    Box::leak(s.shows[0].clone().into_boxed_str())
                } else {
                    ""
                },
                plays: 0,
            });
        }
        let mut podcasts = Vec::new();
        for (k, show) in s.shows.iter().enumerate() {
            if k == 0 && s.managed_episode {
                podcasts.push(FIRST);
                continue;
            }
            let dbid = EPISODE_BASE + k as u64;
            podcasts.push(dbid);
            tracks.push(IpodSpec {
                dbid,
                size: 1000,
                ms: 60_000,
                title: Box::leak(format!("Episode {k}").into_boxed_str()),
                artist: "Host",
                album: Box::leak(show.clone().into_boxed_str()),
                plays: 0,
            });
        }
        let lists = vec![
            ipod_playlist("Road", &[FIRST, SECOND]),
            PlaylistSpec {
                name: "Podcasts",
                smart: false,
                podcasts: true,
                members: podcasts.clone(),
            },
        ];
        let ipod_dir = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let root = fake_ipod(ipod_dir.path(), Some(SYSINFO), &tracks, &lists);
        // make_ipod.c names the k-th track F(k mod 50)/T(k).mp3: put the
        // audio of the managed ones there.
        for (k, f) in files.iter().enumerate() {
            let dest = root.join(format!("iPod_Control/Music/F{:02}/T{k:04}.mp3", k % 50));
            fs::copy(f, dest).unwrap();
        }
        if s.art {
            add_cover_art(&root, SYSINFO);
        }
        let paths = Paths {
            config_file: data_dir.path().join("config.toml"),
            data_dir: data_dir.path().join("data"),
        };
        let w = World {
            env,
            files,
            _ipod_dir: ipod_dir,
            _data_dir: data_dir,
            root,
            paths,
            podcasts,
        };
        w.road(&[0, 1]);
        let conn = w.env.store.conn();
        let db = w.open();
        let run = adoption::run(conn, &db, false, &|_, _| {}).unwrap();
        assert_eq!(run.mapping.len(), 2, "clean adoption: {:?}", run.left);
        adoption::save(conn, &run, &ipod::read(&db).firewire_guid.unwrap(), false).unwrap();
        drop(db);
        w
    }

    /// Rewrites the playlist file Road with the given library files, in order.
    fn road(&self, order: &[usize]) {
        self.list("Road", order);
    }

    /// Rewrites the playlist file `name` with the given library files.
    fn list(&self, name: &str, order: &[usize]) {
        let text: String = order
            .iter()
            .map(|&i| format!("{}\n", self.files[i].display()))
            .collect();
        self.env.write_playlist(name, &text);
        playlists::sync_from_disk(self.env.store.conn(), &self.env.cfg, true).unwrap();
    }

    fn open(&self) -> gpod::Database {
        gpod::Database::open(&self.root).unwrap()
    }

    fn prepare(&self) -> sync::Prepared {
        sync::prepare(self.env.store.conn(), &self.env.cfg, &self.open()).unwrap()
    }

    fn sync(&self) -> iprune_core::Result<Outcome> {
        let p = self.prepare();
        let work = exec::full_work(&p);
        exec::sync(
            self.env.store.conn(),
            &self.env.cfg,
            &self.paths,
            &iprune_core::lock::ipod(&self.paths).unwrap(),
            &self.root,
            self.open(),
            &p,
            &work,
            Options::default(),
            &|_| {},
            &AtomicBool::new(false),
        )
    }

    /// The snapshot text of the iPod's database as it is on disk.
    fn snapshot(&self) -> String {
        let mut out = Vec::new();
        gpod::snapshot::write(&self.open(), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// The members of the Podcasts playlist (dbid hex), sorted.
    fn podcast_members(&self) -> Vec<String> {
        let snap = self.snapshot();
        let mut lines = snap.lines();
        let mut members = Vec::new();
        while let Some(l) = lines.next() {
            if l.starts_with("P Podcasts ") {
                for m in lines.by_ref().take_while(|m| m.starts_with("  ")) {
                    members.push(m.trim().to_owned());
                }
                break;
            }
        }
        members.sort();
        members
    }

    /// Asserts that the second plan is empty and that applying it changes
    /// nothing.
    fn assert_second_sync_is_empty(&self, what: &str) {
        let p = self.prepare();
        assert!(
            p.plan.copy.is_empty()
                && p.plan.replace.is_empty()
                && p.plan.update_tags.is_empty()
                && p.plan.remove.is_empty()
                && p.plan.playlists.is_empty(),
            "{what}: the second plan is not empty: {:?}",
            p.plan
        );
        assert!(exec::full_work(&p).is_empty(), "{what}: work left");
    }
}

fn hex(ids: &[u64]) -> Vec<String> {
    let mut v: Vec<String> = ids.iter().map(|d| format!("{d:016x}")).collect();
    v.sort();
    v
}

fn shows(pattern: &str, n: usize) -> Vec<String> {
    (0..n)
        .map(|k| pattern.replace('#', &k.to_string()))
        .collect()
}

// Prevents (audit H8) every sync on an iPod with many podcast shows failing
// verification after the write: libgpod regroups the Podcasts playlist.
#[test]
fn many_podcast_shows_verify_and_the_second_sync_is_empty() {
    let mut failures = Vec::new();
    for pattern in ["Show #", "Le podcast de #", "Podcast numéro #"] {
        for n in [12, 20, 33] {
            let what = format!("{n} shows named {pattern:?}");
            let w = World::new(Setup {
                shows: shows(pattern, n),
                managed_episode: false,
                title: "Title1",
                art: false,
            });
            assert_eq!(w.podcast_members(), hex(&w.podcasts), "{what}: set up");
            // The user reorders Road: the sync has a playlist to write.
            w.road(&[1, 0]);
            match w.sync() {
                Ok(out) => {
                    if out.applied == 0 {
                        failures.push(format!("{what}: nothing was written"));
                    }
                }
                Err(e) => {
                    failures.push(format!("{what}: {e}"));
                    continue;
                }
            }
            if w.podcast_members() != hex(&w.podcasts) {
                failures.push(format!("{what}: the Podcasts members changed"));
            }
            w.assert_second_sync_is_empty(&what);
            if let Err(e) = w.sync() {
                failures.push(format!("{what}: second sync: {e}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

// Prevents (audit H8) a removal that drops a whole show from the Podcasts
// playlist failing verification.
#[test]
fn removing_the_only_episode_of_one_show_out_of_eight_verifies() {
    let mut w = World::new(Setup {
        shows: shows("Show #", 8),
        managed_episode: true,
        title: "Title1",
        art: false,
    });
    assert_eq!(w.podcast_members().len(), 8, "set up");
    fs::remove_file(&w.files[0]).unwrap();
    w.env.scan();
    w.road(&[1]);
    let p = w.prepare();
    assert_eq!(p.plan.remove.len(), 1, "set up: {:?}", p.plan);
    let out = w.sync().expect("the sync must verify");
    assert_eq!(out.removed, 1);
    // Only that episode left the Podcasts playlist; the other 7 stayed.
    let mut expected = w.podcasts.clone();
    expected.retain(|d| *d != FIRST);
    assert_eq!(w.podcast_members(), hex(&expected));
    w.assert_second_sync_is_empty("after the removal");
}

// Prevents (audit L50) a title containing " mhii " breaking the comparison of
// a track with artwork, whose real mhii link libgpod renumbers on write.
#[test]
fn a_title_containing_mhii_on_a_track_with_artwork_verifies() {
    let mut w = World::new(Setup {
        shows: shows("Show #", 2),
        managed_episode: false,
        title: "Song mhii 77 and more",
        art: true,
    });
    assert!(
        w.snapshot()
            .lines()
            .any(|l| l.contains("| Song mhii 77 and more |") && l.contains("| art 1 mhii ")),
        "set up: the track has a cover and the title"
    );
    // Removing the first track (and its cover) makes libgpod renumber the
    // artwork link of every track after it, the one with " mhii " included.
    fs::remove_file(&w.files[0]).unwrap();
    w.env.scan();
    w.road(&[1]);
    let before = w.snapshot();
    let out = w.sync().expect("the sync must verify");
    assert_eq!(out.removed, 1);
    let after = w.snapshot();
    let link = |snap: &str| -> String {
        snap.lines()
            .find(|l| l.contains("| Song mhii 77 and more |"))
            .unwrap_or_else(|| panic!("the track is gone: {snap}"))
            .rsplit_once(" mhii ")
            .unwrap()
            .1
            .to_owned()
    };
    assert_ne!(
        link(&before),
        link(&after),
        "set up: the write renumbers the link, which is what the check ignores"
    );
    w.assert_second_sync_is_empty("after the sync");
}

/// The member lines of every playlist header that starts with `prefix`, in
/// database order.
fn playlists_named(snap: &str, prefix: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut inside = false;
    for l in snap.lines() {
        if l.starts_with("P ") {
            inside = l.starts_with(prefix);
            if inside {
                out.push(Vec::new());
            }
        } else if inside {
            out.last_mut().unwrap().push(l.trim().to_owned());
        }
    }
    out
}

// Prevents (review of audit H8) a regular playlist named like the Podcasts
// one having its order check skipped, because the Podcasts one is compared
// without order.
#[test]
fn a_regular_playlist_named_podcasts_syncs_next_to_the_special_one() {
    let w = World::new(Setup {
        shows: shows("Show #", 20),
        managed_episode: false,
        title: "Title2",
        art: false,
    });
    w.list("Podcasts", &[0, 1]);
    w.sync().expect("creating the playlist verifies");
    w.list("Podcasts", &[1, 0]);
    w.sync().expect("reordering it verifies");
    let found = playlists_named(&w.snapshot(), "P Podcasts ");
    assert_eq!(found.len(), 2, "the special one and the regular one");
    let regular: Vec<&Vec<String>> = found.iter().filter(|m| m.len() == 2).collect();
    assert_eq!(regular.len(), 1, "{found:?}");
    assert_eq!(
        regular[0],
        &hex_in_order(&[SECOND, FIRST]),
        "the reorder is on the iPod"
    );
    let mut special = found.iter().find(|m| m.len() == 20).unwrap().clone();
    special.sort();
    assert_eq!(special, hex(&w.podcasts));
    w.assert_second_sync_is_empty("after the reorder");
}

fn hex_in_order(ids: &[u64]) -> Vec<String> {
    ids.iter().map(|d| format!("{d:016x}")).collect()
}
