//! The Scan button's work, run on a worker thread: no GTK here.

use anyhow::{Context, Result};
use iprune_core::config::{Config, Paths};
use iprune_core::scan;
use iprune_core::store::Store;

/// Scans the library as `iprune scan` does, with its own connection to the
/// index, and returns the report as `iprune scan` prints it.
pub fn scan(config: &Config, paths: &Paths) -> Result<String> {
    let mut store = Store::open(&paths.database())?;
    let r = scan::scan(&mut store, config, &|_| {}).context("scanning the library")?;
    Ok(scan::describe(&r))
}

#[cfg(test)]
mod tests;
