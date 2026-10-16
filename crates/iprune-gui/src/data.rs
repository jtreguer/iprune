//! Loading what the window shows from the config, the index and the
//! playlist files. Nothing here writes to the iPod or to a playlist file.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use iprune_core::adopt::LibPlaylist;
use iprune_core::config::{Config, Paths, Scope};
use iprune_core::stats::{self, TrackStats};
use iprune_core::store::{self, Store, TrackRow, devices};
use iprune_core::{device, playlists, sync};

use crate::view_model::{self, Row};

pub struct Data {
    config: Config,
    paths: Paths,
    store: Store,
    stats: std::cell::RefCell<HashMap<i64, TrackStats>>,
    /// Set when the playlists folder could not be read at start; the index
    /// still has the playlists as last read.
    pub warning: Option<String>,
}

/// The iPod as the sidebar shows it.
#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub name: String,
    pub connected: bool,
    /// False for a mounted iPod iprune has not adopted.
    pub adopted: bool,
    pub mapped_tracks: usize,
    pub free: Option<u64>,
    pub capacity: Option<u64>,
    pub last_sync: Option<i64>,
    /// The device row of an adopted iPod.
    pub id: Option<i64>,
    /// What it syncs, in a few words, for an adopted iPod.
    pub selection: Option<String>,
}

/// A selection in a few words, for the sidebar.
pub fn selection_summary(s: &devices::DeviceScope) -> String {
    let n = s.playlists.len();
    let playlists = if n == 1 { "playlist" } else { "playlists" };
    match s.kind {
        Scope::Playlists => format!("syncs {n} {playlists}"),
        Scope::Except if n > 0 => format!("syncs all but {n} {playlists}"),
        Scope::All | Scope::Except => "syncs the whole library".into(),
    }
}

impl Data {
    pub fn open() -> Result<Data> {
        let paths =
            Paths::from_env().ok_or_else(|| anyhow!("cannot determine the home directory"))?;
        if !paths.config_file.exists() {
            bail!(
                "no config at {}; run `iprune init` first",
                paths.config_file.display()
            );
        }
        let config = Config::load(&paths.config_file)?;
        let store = Store::open(&paths.database())?;
        // Picks up playlist files changed by another program; only the index
        // is written. An unreadable folder (disk unmounted) leaves the index
        // as it was rather than closing the library.
        let warning = playlists::sync_from_disk(store.conn(), &config, false)
            .context("reading the playlists folder")
            .err()
            .map(|e| format!("{e:#}"));
        let stats = stats::all(store.conn())?;
        Ok(Data {
            config,
            paths,
            store,
            stats: std::cell::RefCell::new(stats),
            warning,
        })
    }

    pub fn library(&self) -> Result<Vec<Row>> {
        let tracks = store::all_tracks(self.store.conn())?;
        Ok(view_model::library_rows(&tracks, &self.stats.borrow()))
    }

    /// Playlist names, as the files are named.
    pub fn playlist_names(&self) -> Result<Vec<String>> {
        Ok(store::playlists(self.store.conn())?
            .into_iter()
            .map(|p| p.name)
            .collect())
    }

    /// The playlists holding the library track `track_id`, by name.
    pub fn playlists_holding(&self, track_id: i64) -> Result<Vec<String>> {
        Ok(store::playlists_holding(self.store.conn(), track_id)?)
    }

    pub fn playlist(&self, name: &str) -> Result<Vec<Row>> {
        let items = playlists::show(self.store.conn(), &self.config, name)?;
        Ok(view_model::playlist_rows(&items, &self.stats.borrow()))
    }

    /// The playlist file's modification time, if it can be read.
    pub fn playlist_mtime(&self, name: &str) -> Option<i64> {
        let file = playlists::file_for(&self.config, name).ok()?;
        iprune_core::playlist::mtime_ns(&file).ok()
    }

    /// Reads the play statistics again, after a sync pulled new ones.
    pub fn reload_stats(&self) -> Result<()> {
        *self.stats.borrow_mut() = stats::all(self.store.conn())?;
        Ok(())
    }

    /// For the sync worker, which opens its own connection to the index.
    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn playlists_dir(&self) -> &std::path::Path {
        &self.config.playlists.dir
    }

    /// Re-reads playlist files changed since they were last read. Only the
    /// index is written.
    pub fn reload_playlists(&self) -> Result<playlists::LoadReport> {
        Ok(playlists::sync_from_disk(
            self.store.conn(),
            &self.config,
            false,
        )?)
    }

    /// Adds library files to a playlist, at 1-based `at` or at the end.
    pub fn add(&self, name: &str, files: &[PathBuf], at: Option<usize>) -> Result<()> {
        Ok(playlists::add(
            self.store.conn(),
            &self.config,
            name,
            files,
            at,
        )?)
    }

    /// Removes the entries at 1-based `positions`.
    pub fn remove(&self, name: &str, positions: &[usize]) -> Result<()> {
        Ok(playlists::remove(
            self.store.conn(),
            &self.config,
            name,
            positions,
        )?)
    }

    /// Moves the entries at 1-based `positions` before `before`.
    pub fn move_entries(&self, name: &str, positions: &[usize], before: usize) -> Result<()> {
        Ok(playlists::move_entries(
            self.store.conn(),
            &self.config,
            name,
            positions,
            before,
        )?)
    }

    pub fn create(&self, name: &str) -> Result<()> {
        Ok(playlists::create(self.store.conn(), &self.config, name)?)
    }

    pub fn rename(&self, old: &str, new: &str) -> Result<()> {
        Ok(playlists::rename(
            self.store.conn(),
            &self.config,
            old,
            new,
        )?)
    }

    /// Deletes the playlist file. One mapped to the iPod stays as a
    /// tombstone until the next sync deletes it there.
    pub fn delete(&self, name: &str) -> Result<()> {
        Ok(playlists::delete(self.store.conn(), &self.config, name)?)
    }

    /// What the iPod `id` syncs, and whether it was set for it (false: it
    /// follows the config).
    pub fn selection(&self, id: i64) -> Result<(devices::DeviceScope, bool)> {
        Ok(sync::device_scope(self.store.conn(), &self.config, id)?)
    }

    /// The library and playlists a selection is sized against.
    pub fn selection_inputs(&self) -> Result<(Vec<TrackRow>, Vec<LibPlaylist>)> {
        let conn = self.store.conn();
        Ok((store::all_tracks(conn)?, devices::library_playlists(conn)?))
    }

    /// Sets what the iPod `id` syncs. Refuses a selection that names a
    /// playlist that is gone or keeps playlists holding no track.
    pub fn set_selection(&self, id: i64, s: &devices::DeviceScope) -> Result<()> {
        let conn = self.store.conn();
        let (library, lists) = self.selection_inputs()?;
        if let Some(why) = sync::scope_problem(s, true, &library, &lists) {
            bail!("{why}");
        }
        devices::set_scope(conn, id, Some(s))?;
        Ok(())
    }

    /// The iPod to show: a mounted one that iprune adopted, else a mounted
    /// one it did not, else the first adopted one, not connected.
    pub fn device(&self) -> Result<Option<DeviceInfo>> {
        let conn = self.store.conn();
        let known = devices::devices(conn)?;
        let mounted: Vec<PathBuf> = if self.config.device.mountpoint.as_os_str().is_empty() {
            device::find_ipods()?
                .into_iter()
                .map(|m| m.mountpoint)
                .collect()
        } else {
            vec![self.config.device.mountpoint.clone()]
        };
        let mounted: Vec<(PathBuf, String)> = mounted
            .into_iter()
            .filter_map(|m| {
                let g = device::firewire_guid(&m)?;
                Some((m, g))
            })
            .collect();
        let known_of = |guid: &str| {
            known
                .iter()
                .find(|d| device::normalize_guid(&d.firewire_guid) == guid)
        };
        let info = |row: &devices::DeviceRow, at: Option<&PathBuf>| -> Result<DeviceInfo> {
            let space = at.and_then(|m| sync::space(m).ok());
            Ok(DeviceInfo {
                name: row.name.clone().unwrap_or_else(|| "iPod".into()),
                connected: at.is_some(),
                adopted: true,
                mapped_tracks: devices::mapped_track_count(conn, row.id)?,
                free: space.map(|s| s.free),
                capacity: space.map(|s| s.capacity),
                last_sync: row.last_sync_at,
                id: Some(row.id),
                selection: Some(selection_summary(
                    &sync::device_scope(conn, &self.config, row.id)?.0,
                )),
            })
        };
        if let Some((m, row)) = mounted.iter().find_map(|(m, g)| Some((m, known_of(g)?))) {
            return info(row, Some(m)).map(Some);
        }
        if let Some((m, _)) = mounted.first() {
            let space = sync::space(m).ok();
            return Ok(Some(DeviceInfo {
                name: "iPod not adopted".into(),
                connected: true,
                adopted: false,
                mapped_tracks: 0,
                free: space.map(|s| s.free),
                capacity: space.map(|s| s.capacity),
                last_sync: None,
                id: None,
                selection: None,
            }));
        }
        known.first().map(|row| info(row, None)).transpose()
    }
}

#[cfg(test)]
mod tests;
