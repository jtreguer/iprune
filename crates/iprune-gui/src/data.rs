//! Loading what the window shows from the config, the index and the
//! playlist files. Nothing here writes to the iPod or to a playlist file.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use iprune_core::config::{Config, Paths};
use iprune_core::stats::{self, TrackStats};
use iprune_core::store::{self, Store, devices};
use iprune_core::{device, playlists, sync};

use crate::view_model::{self, Row};

pub struct Data {
    config: Config,
    store: Store,
    stats: HashMap<i64, TrackStats>,
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
            store,
            stats,
            warning,
        })
    }

    pub fn library(&self) -> Result<Vec<Row>> {
        let tracks = store::all_tracks(self.store.conn())?;
        Ok(view_model::library_rows(&tracks, &self.stats))
    }

    /// Playlist names, as the files are named.
    pub fn playlist_names(&self) -> Result<Vec<String>> {
        Ok(store::playlists(self.store.conn())?
            .into_iter()
            .map(|p| p.name)
            .collect())
    }

    pub fn playlist(&self, name: &str) -> Result<Vec<Row>> {
        let items = playlists::show(self.store.conn(), &self.config, name)?;
        Ok(view_model::playlist_rows(&items, &self.stats))
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
            }));
        }
        known.first().map(|row| info(row, None)).transpose()
    }
}

#[cfg(test)]
mod tests;
