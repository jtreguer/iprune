//! The Import view's work, run on worker threads: no GTK here. Each call
//! opens its own connection to the index, as the other jobs do.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use iprune_core::config::{Config, Paths};
use iprune_core::import::{self, Autotag, FileReport, Index, Killer};
use iprune_core::scan;
use iprune_core::store::Store;

use crate::view_model::import::{ImportRow, rows};

/// The inbox as the view shows it.
#[derive(Debug, Clone)]
pub struct Listing {
    pub rows: Vec<ImportRow>,
    /// Paths in the inbox that could not be read, with why.
    pub unreadable: Vec<(PathBuf, String)>,
}

/// Reads the inbox: new files get their row, changed ones start over, and
/// each file's status is worked out.
pub fn read(config: &Config, paths: &Paths) -> Result<Listing> {
    let store = Store::open(&paths.database())?;
    let inbox = import::read_inbox(store.conn(), config).context("reading the inbox")?;
    let index = Index::load(store.conn())?;
    Ok(Listing {
        rows: rows(&inbox, &index),
        unreadable: inbox.unreadable,
    })
}

/// The files in the inbox, hidden ones left out, for the sidebar: cheap
/// enough for the window's thread. None when there is no inbox.
pub fn count(config: &Config) -> usize {
    fn walk(dir: &Path) -> usize {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .flatten()
            .filter(|e| !e.file_name().as_encoded_bytes().starts_with(b"."))
            .map(|e| match e.file_type() {
                Ok(t) if t.is_dir() => walk(&e.path()),
                Ok(t) if t.is_file() => 1,
                _ => 0,
            })
            .sum()
    }
    config.inbox().map_or(0, |dir| walk(&dir))
}

/// Copies files and folders into the inbox; returns how many files.
pub fn add(config: &Config, sources: &[PathBuf]) -> Result<usize> {
    Ok(import::add_to_inbox(config, sources)
        .context("copying into the inbox")?
        .len())
}

/// What a lookup pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Looked {
    pub looked_up: usize,
    pub genres: usize,
    /// Stop ended it.
    pub stopped: bool,
}

/// `path` relative to `dir`, for the line under the grid.
fn shown(path: &Path, dir: &Path) -> String {
    path.strip_prefix(dir).unwrap_or(path).display().to_string()
}

/// Looks up every file waiting for it, then asks for the genres of the
/// candidates chosen since. autotag runs only when there is work; while it
/// runs, `killer` holds the handle Stop uses to end a lookup that does not
/// end by itself. `progress` gets the files done, the total and the file.
pub fn look_up(
    config: &Config,
    paths: &Paths,
    progress: &(dyn Fn(usize, usize, &str) + Sync),
    stop: &AtomicBool,
    killer: &Mutex<Option<Killer>>,
) -> Result<Looked> {
    let store = Store::open(&paths.database())?;
    let inbox = import::read_inbox(store.conn(), config).context("reading the inbox")?;
    let index = Index::load(store.conn())?;
    let pending = import::pending(&inbox, &index, false);
    let genres = import::genres_wanted(&inbox);
    if pending.is_empty() && genres.is_empty() {
        return Ok(Looked::default());
    }
    let mut autotag = Autotag::start(config, paths)?;
    *killer.lock().unwrap_or_else(|e| e.into_inner()) = Some(autotag.killer());
    let result = (|| -> iprune_core::Result<Looked> {
        let looked_up = import::look_up(
            store.conn(),
            &pending,
            &mut autotag,
            &mut |n, total, path| progress(n, total, &shown(path, &inbox.dir)),
            stop,
        )?;
        let genres = import::fill_genres(store.conn(), &genres, &mut autotag, stop)?;
        Ok(Looked {
            looked_up,
            genres,
            stopped: stop.load(Ordering::SeqCst),
        })
    })();
    // Before the drop, which reaps the process: its id may then name
    // another one.
    killer.lock().unwrap_or_else(|e| e.into_inner()).take();
    drop(autotag);
    match result {
        // Killed by Stop: what was looked up until then is kept.
        Err(_) if stop.load(Ordering::SeqCst) => Ok(Looked {
            stopped: true,
            ..Looked::default()
        }),
        r => Ok(r?),
    }
}

/// Files the ready songs, then scans and adds them to `playlist`.
/// `progress` gets the files done, the total and the file; `stop` ends it
/// between files.
pub fn file(
    config: &Config,
    paths: &Paths,
    playlist: Option<&str>,
    progress: &(dyn Fn(usize, usize, &str) + Sync),
    stop: &AtomicBool,
) -> Result<FileReport> {
    // Before autotag starts: a library that is not there refuses at once.
    import::check_roots(config)?;
    let dir = import::inbox_dir(config)?;
    let mut store = Store::open(&paths.database())?;
    let mut autotag = Autotag::start(config, paths)?;
    Ok(import::file(
        &mut store,
        config,
        &mut autotag,
        playlist,
        &mut |n, total, path| progress(n, total, &shown(path, &dir)),
        stop,
    )?)
}

fn songs(n: usize) -> String {
    match n {
        1 => "1 song".into(),
        n => format!("{n} songs"),
    }
}

/// The report after filing: what was imported and where, what was not and
/// why, then the scan and the playlist.
pub fn describe(report: &FileReport, inbox: &Path, root: &Path) -> String {
    let mut out = match report.filed.len() {
        0 => "Nothing was imported.".to_owned(),
        n => format!("{} imported:", songs(n)),
    };
    for f in &report.filed {
        out.push_str(&format!("\n  {}", shown(&f.to, root)));
    }
    for (p, why) in &report.failed {
        out.push_str(&format!("\nNot imported, {why}: {}", shown(p, inbox)));
    }
    for (p, fields) in &report.mismatches {
        out.push_str(&format!(
            "\nReads back otherwise than written ({}), see Get Info: {}",
            fields.join(", "),
            shown(p, root)
        ));
    }
    if let Some(e) = &report.error {
        out.push_str(&format!(
            "\nautotag stopped part way; the songs after it wait in the inbox: {e}"
        ));
    }
    if report.stopped {
        out.push_str("\nStopped: the songs not imported wait in the inbox.");
    }
    for p in &report.not_indexed {
        out.push_str(&format!(
            "\nNot in the library index after the scan: {}",
            p.display()
        ));
    }
    match &report.playlist {
        Some(Ok(name)) => {
            let n = report.filed.iter().filter(|f| f.track.is_some()).count();
            out.push_str(&format!("\nAdded {} to {name}.", songs(n)));
        }
        Some(Err(e)) => out.push_str(&format!("\nNot added to the playlist: {e}")),
        None => {}
    }
    match &report.scan {
        Some(Ok(r)) => {
            out.push_str("\n\n");
            out.push_str(&scan::describe(r));
        }
        Some(Err(e)) => out.push_str(&format!("\n\nThe scan after it failed ({e}); scan again.")),
        None => {}
    }
    out
}

#[cfg(test)]
mod tests;
