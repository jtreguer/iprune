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
    /// Let iprune write to a plain folder as if it were an iPod, for tests
    /// and experiments on copies. Off, writes go only to the root of a
    /// mounted FAT32 device.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub allow_folder: bool,
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
        let config = Self::load_unchecked(path)?;
        config.validate(path)?;
        Ok(config)
    }

    /// The config as written, before [`Config::validate`]: for `init
    /// --force`, which mends one that no longer validates.
    pub fn load_unchecked(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).at(path)?;
        toml::from_str(&text).map_err(|e| Error::Config {
            path: path.to_owned(),
            message: e.to_string(),
        })
    }

    /// Writes the config through a temporary file, synced, then renamed
    /// over the old one: a crash leaves either.
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::io::Write as _;
        let text = toml::to_string_pretty(self).expect("config always serializes");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).at(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        let mut f = std::fs::File::create(&tmp).at(&tmp)?;
        f.write_all(text.as_bytes()).at(&tmp)?;
        f.sync_all().at(&tmp)?;
        std::fs::rename(&tmp, path).at(path)
    }

    /// The rules every loaded config meets; `path` is named in the error.
    pub fn validate(&self, path: &Path) -> Result<()> {
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
        if self.library.roots.iter().any(|r| r.parent().is_none()) {
            return fail("library.roots must not be / (it holds the iPod and everything else)");
        }
        let mp = &self.device.mountpoint;
        if !mp.as_os_str().is_empty() && !mp.is_absolute() {
            return fail("device.mountpoint must be an absolute path");
        }
        // The iPod's own files must never be library tracks. An iPod found
        // at run time inside a root is skipped by the scan.
        if !mp.as_os_str().is_empty() {
            let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
            let mp = real(mp);
            if self
                .library
                .roots
                .iter()
                .map(|r| real(r))
                .any(|r| r.starts_with(&mp) || mp.starts_with(&r))
            {
                return fail("device.mountpoint must not be inside a library root, nor hold one");
            }
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
    fn load_with_device(device: &str) -> Result<Config> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!("[library]\nroots = [\"/m\"]\n[playlists]\ndir = \"/p\"\n[device]\n{device}\n"),
        )
        .unwrap();
        Config::load(&path)
    }

    // Prevents (audit M6) a relative mountpoint, which would mean a
    // different folder with every working directory, being accepted.
    #[test]
    fn a_relative_device_mountpoint_is_a_config_error() {
        for bad in ["relative/ipod", "./ipod", "ipod", "../ipod"] {
            let r = load_with_device(&format!("mountpoint = {bad:?}"));
            match r {
                Err(Error::Config { message, .. }) => {
                    assert!(message.contains("device.mountpoint"), "{bad}: {message}")
                }
                other => panic!("{bad}: expected a config error, got {other:?}"),
            }
        }
        // Absolute and empty (detect) are fine.
        let c = load_with_device("mountpoint = \"/run/media/x/IPOD\"").unwrap();
        assert_eq!(c.device.mountpoint, Path::new("/run/media/x/IPOD"));
        assert!(load_with_device("mountpoint = \"\"").is_ok());
    }

    #[test]
    fn allow_folder_is_off_by_default_read_when_set_and_never_written_when_off() {
        assert!(!Config::default().device.allow_folder);
        assert!(
            !load_with_device("mountpoint = \"/x\"")
                .unwrap()
                .device
                .allow_folder
        );
        let c = load_with_device("mountpoint = \"/x\"\nallow_folder = true").unwrap();
        assert!(c.device.allow_folder);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut c = Config::default();
        c.library.roots = vec!["/m".into()];
        c.playlists.dir = "/p".into();
        c.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("allow_folder"), "{text}");
        c.device.allow_folder = true;
        c.save(&path).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("allow_folder = true")
        );
        assert!(Config::load(&path).unwrap().device.allow_folder);
    }
}
