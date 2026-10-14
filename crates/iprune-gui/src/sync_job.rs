//! The sync's work, run on a worker thread: no GTK here. libgpod objects
//! cannot cross threads, so each step opens the iPod database itself and
//! returns plain data.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use iprune_core::config::{Config, Paths};
use iprune_core::store::Store;
use iprune_core::{device, exec, scan, sync};

/// A plan ready to be shown.
#[derive(Debug, Clone)]
pub struct Planned {
    /// The whole plan. The dialog shows its text; tests look at it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub plan: iprune_core::plan::Plan,
    pub ipod_name: String,
    /// One line on what the scan found.
    pub scan: String,
    /// The plan's counts, as `iprune sync` prints them.
    pub summary: String,
    /// Every change, one per line.
    pub details: String,
    /// Playlist changes this version applies.
    pub ops: usize,
    /// Playlist entries left out until tracks are copied.
    pub left_out: usize,
    /// Set when the plan deletes many managed playlists at once.
    pub mass_removal: Option<String>,
    /// False when the plan's track copies would not fit; playlist changes
    /// need no space.
    pub fits: bool,
    /// The playlist changes this version applies, as shown.
    pub work: exec::Work,
}

/// What an applied sync did.
#[derive(Debug, Clone)]
pub struct Applied {
    pub applied: usize,
    pub stats_tracks: usize,
    pub plays: u64,
    pub recovered: usize,
    pub stamp: Option<String>,
    pub warnings: Vec<String>,
}

fn open(paths: &Paths) -> Result<Store> {
    Ok(Store::open(&paths.database())?)
}

/// Scans the library, reads the iPod and plans. Writes only the index.
pub fn plan(config: &Config, paths: &Paths) -> Result<Planned> {
    let mut store = open(paths)?;
    let r = scan::scan(&mut store, config, &|_| {}).context("scanning the library")?;
    let scan = format!(
        "library: {} unchanged, {} added, {} updated, {} moved, {} missing",
        r.unchanged,
        r.added,
        r.updated + r.returned,
        r.moved.len(),
        r.missing.len()
    );
    let mountpoint: PathBuf = device::mountpoint(config)?;
    let db = gpod::Database::open(&mountpoint)?;
    let prepared = sync::prepare(store.conn(), config, &db)?;
    let work = exec::playlist_work(&prepared.plan, &prepared.tracks_map, &prepared.ipod);
    let mass_removal = exec::check_removals(
        &work,
        prepared.playlists_map.len(),
        exec::Options::default(),
    )
    .err()
    .map(|e| e.to_string());
    Ok(Planned {
        fits: prepared.plan.space.fits,
        work: work.clone(),
        ipod_name: prepared.ipod.name.clone().unwrap_or_else(|| "iPod".into()),
        scan,
        summary: sync::describe(&prepared, false),
        details: sync::describe(&prepared, true),
        ops: work.ops.len(),
        left_out: work.left_out,
        mass_removal,
        plan: prepared.plan,
    })
}

/// Applies `expected`, the playlist changes the user saw. The iPod and the
/// index are read again first; if the changes they give are not the same,
/// nothing is written. Only what is applied is compared, so a change in
/// free space or in the track copies (not applied yet) does not refuse it.
pub fn apply(
    config: &Config,
    paths: &Paths,
    expected: &exec::Work,
    allow_mass_removal: bool,
) -> Result<Applied> {
    let store = open(paths)?;
    let conn = store.conn();
    let mountpoint = device::mountpoint(config)?;
    let db = gpod::Database::open(&mountpoint)?;
    let prepared = sync::prepare(conn, config, &db)?;
    let work = exec::playlist_work(&prepared.plan, &prepared.tracks_map, &prepared.ipod);
    if work != *expected {
        bail!(
            "the playlists or the iPod changed since the plan was made; nothing was written. \
             Sync again to see the new plan"
        );
    }
    if work.ops.is_empty() {
        return Ok(Applied {
            applied: 0,
            stats_tracks: 0,
            plays: 0,
            recovered: 0,
            stamp: None,
            warnings: Vec::new(),
        });
    }
    let opts = exec::Options { allow_mass_removal };
    let done = exec::sync_playlists(conn, config, paths, &mountpoint, db, &prepared, &work, opts)?;
    Ok(Applied {
        applied: done.applied,
        stats_tracks: done.pulled.tracks(),
        plays: done.pulled.plays(),
        recovered: done.recovered,
        stamp: Some(done.backup.stamp),
        warnings: done.warnings,
    })
}

#[cfg(test)]
mod tests;
