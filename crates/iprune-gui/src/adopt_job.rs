//! Adoption from the window, run on a worker thread: no GTK here. The
//! worker scans the library, reads the iPod and matches them; the window
//! shows the proposals, one row each, and the user picks a library file for
//! each or leaves the iPod track unmanaged. Only the index is written.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use iprune_core::adoption::{self, Run};
use iprune_core::config::{Config, Paths};
use iprune_core::store::{Store, TrackRow, devices};
use iprune_core::{adopt, device, scan, sync};

use crate::view_model;

/// One choice in a proposal's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// The library track, or `None` to leave the iPod track unmanaged.
    pub track_id: Option<i64>,
    pub label: String,
}

/// One proposal, as the review grid shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRow {
    pub dbid: u64,
    /// "Artist – Title".
    pub ipod: String,
    pub album: String,
    /// Size and time of the iPod's file.
    pub detail: String,
    pub why: &'static str,
    /// Leaving it unmanaged first, then the candidates, best first.
    pub choices: Vec<Choice>,
    /// The index in `choices` that `iprune adopt --accept-proposals` takes.
    pub suggested: usize,
}

/// An adoption ready to be reviewed.
pub struct Review {
    pub run: Run,
    pub guid: String,
    pub ipod_name: String,
    /// What was matched, as `iprune adopt` prints it.
    pub summary: String,
    /// Where the full report was written.
    pub report: PathBuf,
    pub rows: Vec<ReviewRow>,
}

/// What a saved adoption holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub tracks: usize,
    pub playlists: usize,
    /// iPod tracks left unmanaged: unresolved proposals and tracks with no
    /// library file.
    pub unmanaged: usize,
    /// Library tracks no iPod track holds, copied by the next sync.
    pub to_copy: usize,
}

fn text(v: &Option<String>) -> &str {
    v.as_deref().filter(|s| !s.trim().is_empty()).unwrap_or("?")
}

/// The last two parts of a path: enough to tell duplicates apart.
fn short_path(p: &Path) -> String {
    let parts: Vec<_> = p.iter().collect();
    let tail = &parts[parts.len().saturating_sub(2)..];
    tail.iter()
        .map(|s| s.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn library_label(t: &TrackRow) -> String {
    let mut s = format!("{} – {}", text(&t.tags.artist), t.display_title());
    if let Some(album) = t.tags.album.as_deref().filter(|a| !a.trim().is_empty()) {
        s.push_str(&format!(" ({album})"));
    }
    if let Some(ms) = t.info.duration_ms {
        s.push_str(&format!(" · {}", view_model::duration(ms)));
    }
    s.push_str(&format!(
        " · {} · {}",
        sync::bytes(t.size),
        short_path(&t.path)
    ));
    s
}

/// The review grid's rows, one per proposal, in the run's order. Pure.
pub fn review_rows(run: &Run) -> Vec<ReviewRow> {
    let suggested: HashMap<u64, i64> = adopt::resolve(&run.adoption, true).0.into_iter().collect();
    let on_ipod: HashMap<u64, &iprune_core::ipod::Track> =
        run.ipod.tracks.iter().map(|t| (t.adopt.dbid, t)).collect();
    run.adoption
        .proposals
        .iter()
        .map(|p| {
            let t = on_ipod.get(&p.dbid);
            let mut choices = vec![Choice {
                track_id: None,
                label: "Leave it on the iPod, unmanaged".into(),
            }];
            choices.extend(p.candidates.iter().filter_map(|id| {
                Some(Choice {
                    track_id: Some(*id),
                    label: library_label(run.library.get(id)?),
                })
            }));
            let pick = suggested.get(&p.dbid).copied();
            ReviewRow {
                dbid: p.dbid,
                ipod: t.map_or_else(
                    || format!("{:016x}", p.dbid),
                    |t| format!("{} – {}", text(&t.adopt.artist), text(&t.adopt.title)),
                ),
                album: t.and_then(|t| t.adopt.album.clone()).unwrap_or_default(),
                detail: t.map_or_else(String::new, |t| {
                    format!(
                        "{} · {}",
                        sync::bytes(t.adopt.size),
                        view_model::duration(t.adopt.duration_ms)
                    )
                }),
                why: adoption::reason(p.reason),
                suggested: choices
                    .iter()
                    .position(|c| c.track_id.is_some() && c.track_id == pick)
                    .unwrap_or(0),
                choices,
            }
        })
        .collect()
}

/// The decision per proposal from the index picked in each row. A pick out
/// of range leaves the track unmanaged.
pub fn choices(rows: &[ReviewRow], picked: &[usize]) -> HashMap<u64, Option<i64>> {
    rows.iter()
        .zip(picked)
        .map(|(r, &i)| (r.dbid, r.choices.get(i).and_then(|c| c.track_id)))
        .collect()
}

/// One line per library file picked for more than one iPod track, or for
/// one when another already matched it: Save is refused while there are any.
pub fn conflicts(review: &Review, picked: &[usize]) -> Vec<String> {
    let chosen = choices(&review.rows, picked);
    let Err(conflicts) = adopt::resolve_choices(&review.run.adoption, &chosen) else {
        return Vec::new();
    };
    conflicts
        .iter()
        .map(|c| {
            let name = review
                .run
                .library
                .get(&c.track_id)
                .map_or_else(|| c.track_id.to_string(), library_label);
            let rows = c
                .dbids
                .iter()
                .filter(|d| review.rows.iter().any(|r| r.dbid == **d))
                .count();
            match (c.dbids.len() - rows, rows) {
                (0, _) => format!("{name} is picked for {rows} iPod tracks"),
                (_, 1) => format!("{name} is picked, but another iPod track already matched it"),
                _ => format!(
                    "{name} is picked for {rows} iPod tracks, and another already matched it"
                ),
            }
        })
        .collect()
}

fn refuse_adopted(store: &Store, guid: &str) -> Result<()> {
    let conn = store.conn();
    if let Some(id) = devices::device_id(conn, guid)?
        && devices::mapped_track_count(conn, id)? > 0
    {
        bail!("this iPod is already adopted; `iprune adopt --force` adopts it again");
    }
    Ok(())
}

/// Scans the library, reads the iPod and matches them. Refuses an iPod
/// already adopted: adopting again replaces its mapping, which only
/// `iprune adopt --force` does. `progress` reports `(done, total)` iPod
/// files compared. Writes the index and `adopt-report.txt`.
pub fn prepare(config: &Config, paths: &Paths, progress: &dyn Fn(usize, usize)) -> Result<Review> {
    let mut store = Store::open(&paths.database())?;
    scan::scan(&mut store, config, &|_| {}).context("scanning the library")?;
    let mountpoint = device::mountpoint(config)?;
    let conn = store.conn();
    // Checked first too, before the iPod files are read: the GUID libgpod
    // reports is checked again below.
    if let Some(guid) = device::firewire_guid(&mountpoint) {
        for d in devices::devices(conn)? {
            if device::normalize_guid(&d.firewire_guid) == guid {
                refuse_adopted(&store, &d.firewire_guid)?;
            }
        }
    }
    let db = gpod::Database::open(&mountpoint)?;
    let run = adoption::run(conn, &db, false, progress)?;
    let Some(guid) = run.ipod.firewire_guid.clone() else {
        bail!(
            "the iPod has no FirewireGuid in iPod_Control/Device/SysInfo, so it cannot be \
             identified; run ipod-read-sysinfo-extended on it first"
        );
    };
    refuse_adopted(&store, &guid)?;
    let report = paths.data_dir.join("adopt-report.txt");
    std::fs::write(&report, adoption::report(&run))
        .with_context(|| format!("writing {}", report.display()))?;
    Ok(Review {
        ipod_name: run.ipod.name.clone().unwrap_or_else(|| "iPod".into()),
        summary: adoption::summary(&run),
        rows: review_rows(&run),
        report,
        guid,
        run,
    })
}

/// Saves the adoption with the picks made in the grid, if the iPod mounted
/// is still the one reviewed.
pub fn save(config: &Config, paths: &Paths, mut review: Review, picked: &[usize]) -> Result<Saved> {
    let mountpoint = device::mountpoint(config)?;
    match device::firewire_guid(&mountpoint) {
        Some(g) if g == device::normalize_guid(&review.guid) => {}
        Some(_) => bail!("the iPod connected is not the one reviewed; nothing saved"),
        None => bail!("the iPod reviewed is no longer connected; nothing saved"),
    }
    let store = Store::open(&paths.database())?;
    let conn = store.conn();
    let chosen = choices(&review.rows, picked);
    adoption::choose(conn, &mut review.run, &chosen)?;
    adoption::save(conn, &review.run, &review.guid, false)?;
    let run = &review.run;
    Ok(Saved {
        tracks: run.mapping.len(),
        playlists: run.playlists.matched.len(),
        unmanaged: run.left.len() + run.adoption.unmatched_ipod.len(),
        to_copy: run.library.len() - run.mapping.len(),
    })
}

#[cfg(test)]
mod tests;
