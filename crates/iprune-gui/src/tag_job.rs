//! Get Info's work, run on a worker thread: no GTK here.

use std::sync::atomic::AtomicBool;

use anyhow::Result;
use iprune_core::config::Paths;
use iprune_core::store::Store;
use iprune_core::tag_edit::{self, Changes, Report};

/// Writes `changes` to the files of `ids` as `iprune tag` does, with its
/// own connection to the index. `progress` gets the files done and the
/// total before each file.
pub fn write(
    paths: &Paths,
    ids: &[i64],
    changes: &Changes,
    progress: &dyn Fn(usize, usize),
    stop: &AtomicBool,
) -> Result<Report> {
    let store = Store::open(&paths.database())?;
    Ok(tag_edit::edit(
        store.conn(),
        ids,
        changes,
        &mut |p| progress(p.done, p.total),
        stop,
    )?)
}

/// What the form says after a write that did not go through for every
/// file: how many were written, then each file refused with why, and the
/// files a Stop left out.
pub fn describe(report: &Report, asked: usize) -> String {
    let written = report.written();
    let mut out = match written {
        1 => "1 file written.".to_string(),
        n => format!("{n} files written."),
    };
    for (r, why) in report.refused() {
        out.push_str(&format!("\nNot written, {why}: {}", r.path.display()));
    }
    if let Some(e) = &report.index_error {
        out.push_str(&format!("\nThe index could not be updated: {e}"));
    }
    let left = asked.saturating_sub(report.results.len());
    if report.stopped && left > 0 {
        out.push_str(&match left {
            1 => "\nStopped: 1 file was not edited.".to_string(),
            n => format!("\nStopped: {n} files were not edited."),
        });
    }
    out
}

#[cfg(test)]
mod tests;
