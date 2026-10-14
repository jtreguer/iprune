//! The sync's work, run on a worker thread: no GTK here. libgpod objects
//! cannot cross threads, so each step opens the iPod database itself and
//! returns plain data.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use iprune_core::config::{Config, Paths};
use iprune_core::exec::{self, FullWork, Progress};
use iprune_core::store::Store;
use iprune_core::{device, scan, sync};

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
    /// One line on what Apply does.
    pub to_apply: String,
    /// Set when the plan removes many managed playlists or tracks at once.
    pub mass_removal: Option<String>,
    /// False when the tracks to copy do not fit: Apply is refused.
    pub fits: bool,
    /// What Apply applies, as shown.
    pub work: FullWork,
}

/// What an applied sync did.
#[derive(Debug, Clone, Default)]
pub struct Applied {
    pub copied: usize,
    pub recopied: usize,
    pub removed: usize,
    pub retagged: usize,
    /// Playlist changes.
    pub applied: usize,
    pub bytes: u64,
    pub stats_tracks: usize,
    pub plays: u64,
    pub recovered: usize,
    pub stamp: Option<String>,
    /// Set when the sync stopped early; what was done is on the iPod.
    pub stopped: Option<String>,
    /// Files not copied (gone or changed since the scan), with why.
    pub skipped: Vec<(PathBuf, String)>,
    pub warnings: Vec<String>,
}

fn open(paths: &Paths) -> Result<Store> {
    Ok(Store::open(&paths.database())?)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The line on what Apply does.
pub fn to_apply(work: &FullWork) -> String {
    if work.is_empty() {
        return "Nothing to apply.".into();
    }
    let t = &work.tracks;
    format!(
        "Apply copies {} ({}), removes {}, updates the tags of {} and makes {}.",
        plural(t.copies.len(), "track", "tracks"),
        sync::bytes(t.copy_bytes()),
        plural(t.removes.len(), "track", "tracks"),
        plural(t.retags.len(), "track", "tracks"),
        plural(
            work.playlists.ops.len(),
            "playlist change",
            "playlist changes"
        ),
    )
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
    let work = exec::full_work(&prepared);
    let opts = exec::Options::default();
    let mass_removal = exec::check_removals(&work.playlists, prepared.playlists_map.len(), opts)
        .and_then(|()| exec::check_track_removals(&work.tracks, prepared.tracks_map.len(), opts))
        .err()
        .map(|e| e.to_string());
    Ok(Planned {
        fits: prepared.plan.space.fits || work.tracks.copies.is_empty(),
        to_apply: to_apply(&work),
        ipod_name: prepared.ipod.name.clone().unwrap_or_else(|| "iPod".into()),
        scan,
        summary: sync::describe(&prepared, false),
        details: sync::describe(&prepared, true),
        mass_removal,
        work,
        plan: prepared.plan,
    })
}

/// Applies `expected`, the work the user saw. The iPod and the index are
/// read again first; if the work they give is not the same, nothing is
/// written. `cancel` stops the copies after the current file.
pub fn apply(
    config: &Config,
    paths: &Paths,
    expected: &FullWork,
    allow_mass_removal: bool,
    progress: &dyn Fn(Progress),
    cancel: &AtomicBool,
) -> Result<Applied> {
    // Held from before the iPod is read until its write is verified.
    let lock = iprune_core::lock::ipod(paths)?;
    let store = open(paths)?;
    let conn = store.conn();
    let mountpoint = device::mountpoint(config)?;
    let db = gpod::Database::open(&mountpoint)?;
    let prepared = sync::prepare(conn, config, &db)?;
    let work = exec::full_work(&prepared);
    if work != *expected {
        bail!(
            "the library, the playlists or the iPod changed since the plan was made; nothing \
             was written. Sync again to see the new plan"
        );
    }
    if work.is_empty() {
        return Ok(Applied::default());
    }
    let opts = exec::Options { allow_mass_removal };
    let done = exec::sync(
        conn,
        config,
        paths,
        &lock,
        &mountpoint,
        db,
        &prepared,
        &work,
        opts,
        progress,
        cancel,
    )?;
    Ok(Applied {
        copied: done.copied,
        recopied: done.recopied,
        removed: done.removed,
        retagged: done.retagged,
        applied: done.applied,
        bytes: done.bytes,
        stats_tracks: done.pulled.tracks(),
        plays: done.pulled.plays(),
        recovered: done.recovered,
        stamp: Some(done.backup.stamp),
        stopped: done.stopped,
        skipped: done.skipped,
        warnings: done.warnings,
    })
}

/// Whether the Stop button should be enabled after this progress report,
/// or `None` when the report does not change it. Not during a write; back
/// on at each copy unless Stop was already pressed.
pub fn stop_enabled(p: &Progress, cancelled: bool) -> Option<bool> {
    match p {
        Progress::Copying { .. } => Some(!cancelled),
        Progress::Writing => Some(false),
        Progress::Removing { .. } | Progress::Verifying => None,
    }
}

#[cfg(test)]
mod tests;
