//! `~/.config/iprune/config.toml` and the XDG locations iprune uses.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::IoContext;
use crate::{Error, Result};

/// Where iprune keeps its files. Follows `XDG_CONFIG_HOME` and `XDG_DATA_HOME`.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Option<Self> {
        let dirs = directories::ProjectDirs::from("", "", "iprune")?;
        Some(Paths {
            config_file: dirs.config_dir().join("config.toml"),
            data_dir: dirs.data_dir().to_path_buf(),
        })
    }

    pub fn database(&self) -> PathBuf {
        self.data_dir.join("iprune.sqlite")
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub library: Library,
    pub playlists: Playlists,
    pub sync: Sync,
    pub device: Device,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Library {
    pub roots: Vec<PathBuf>,
    pub exclude_extensions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Playlists {
    pub dir: PathBuf,
    pub path_style: PathStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathStyle {
    #[default]
    Absolute,
    /// Relative to the playlists folder.
    Relative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    All,
    Playlists,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sync {
    pub scope: Scope,
    pub playlists: Vec<String>,
    pub remove_unmanaged: bool,
    pub keep_backups: u32,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Device {
    /// Empty means detect.
    pub mountpoint: PathBuf,
}

impl Default for Library {
    fn default() -> Self {
        Library {
            roots: Vec::new(),
            exclude_extensions: ["m4v", "mp4", "mov"].map(String::from).to_vec(),
        }
    }
}

impl Default for Playlists {
    fn default() -> Self {
        Playlists {
            dir: PathBuf::new(),
            path_style: PathStyle::Absolute,
        }
    }
}

impl Default for Sync {
    fn default() -> Self {
        Sync {
            scope: Scope::All,
            playlists: Vec::new(),
            remove_unmanaged: false,
            keep_backups: 20,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).at(path)?;
        let config: Config = toml::from_str(&text).map_err(|e| Error::Config {
            path: path.to_owned(),
            message: e.to_string(),
        })?;
        config.validate(path)?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).expect("config always serializes");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).at(dir)?;
        }
        std::fs::write(path, text).at(path)
    }

    fn validate(&self, path: &Path) -> Result<()> {
        let fail = |message: &str| {
            Err(Error::Config {
                path: path.to_owned(),
                message: message.to_owned(),
            })
        };
        if self.library.roots.is_empty() {
            return fail("library.roots is empty");
        }
        if self.library.roots.iter().any(|r| !r.is_absolute()) {
            return fail("library.roots must be absolute paths");
        }
        if !self.playlists.dir.is_absolute() {
            return fail("playlists.dir must be an absolute path");
        }
        Ok(())
    }

    /// Whether `ext` (lowercase, no dot) is excluded from the library.
    pub fn excluded(&self, ext: &str) -> bool {
        self.library
            .exclude_extensions
            .iter()
            .any(|e| e.eq_ignore_ascii_case(ext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c/config.toml");
        let mut c = Config::default();
        c.library.roots = vec!["/m".into()];
        c.playlists.dir = "/p".into();
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);

        std::fs::write(
            &path,
            "[library]\nroots = [\"/m\"]\n[playlists]\ndir = \"/p\"\n",
        )
        .unwrap();
        let c = Config::load(&path).unwrap();
        assert!(c.excluded("M4V"));
        assert_eq!(c.sync.keep_backups, 20);
        assert_eq!(c.playlists.path_style, PathStyle::Absolute);
    }

    #[test]
    fn rejects_unknown_keys_and_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[library]\nroots = [\"/m\"]\n[playlists]\ndir = \"/p\"\n[sync]\nartwork = \"all\"\n",
        )
        .unwrap();
        assert!(matches!(Config::load(&path), Err(Error::Config { .. })));
        std::fs::write(
            &path,
            "[library]\nroots = [\"m\"]\n[playlists]\ndir = \"/p\"\n",
        )
        .unwrap();
        assert!(matches!(Config::load(&path), Err(Error::Config { .. })));
    }
}
