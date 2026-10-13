use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};

use gpod_sys::{Itdb_Playlist, Itdb_Track, Itdb_iTunesDB};

use crate::error::take_gerror;
use crate::list::{self, ListIter};
use crate::{Error, PlaylistRef, Result, TrackRef};

/// An open iPod database. libgpod is not thread safe; holding a `NonNull`
/// keeps this neither `Send` nor `Sync`.
pub struct Database {
    raw: NonNull<Itdb_iTunesDB>,
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::NulInPath(path.to_owned()))
}

impl Database {
    /// Parses the database of an iPod mounted at `mountpoint`.
    pub fn open(mountpoint: &Path) -> Result<Self> {
        Self::parse(mountpoint, gpod_sys::itdb_parse)
    }

    /// Parses a standalone `iTunesDB` file, with no device attached.
    pub fn open_file(itunesdb: &Path) -> Result<Self> {
        Self::parse(itunesdb, gpod_sys::itdb_parse_file)
    }

    fn parse(
        path: &Path,
        f: unsafe extern "C" fn(
            *const glib_sys::gchar,
            *mut *mut glib_sys::GError,
        ) -> *mut Itdb_iTunesDB,
    ) -> Result<Self> {
        let c = c_path(path)?;
        let mut err = ptr::null_mut();
        let raw = unsafe { f(c.as_ptr(), &mut err) };
        match NonNull::new(raw) {
            Some(raw) => {
                if !err.is_null() {
                    unsafe { glib_sys::g_error_free(err) };
                }
                Ok(Database { raw })
            }
            None => Err(Error::Parse {
                path: path.to_owned(),
                message: unsafe { take_gerror(err) },
            }),
        }
    }

    fn raw(&self) -> &Itdb_iTunesDB {
        unsafe { self.raw.as_ref() }
    }

    pub fn tracks(&self) -> impl Iterator<Item = TrackRef<'_>> {
        unsafe { ListIter::new(self.raw().tracks, self) }
            .map(|p| unsafe { TrackRef::from_raw(p as *mut Itdb_Track) })
    }

    pub fn playlists(&self) -> impl Iterator<Item = PlaylistRef<'_>> {
        unsafe { ListIter::new(self.raw().playlists, self) }
            .map(|p| unsafe { PlaylistRef::from_raw(p as *mut Itdb_Playlist) })
    }

    pub fn track_count(&self) -> usize {
        unsafe { list::len(self.raw().tracks, self) }
    }

    pub fn playlist_count(&self) -> usize {
        unsafe { list::len(self.raw().playlists, self) }
    }

    pub fn track_by_dbid(&self, dbid: u64) -> Option<TrackRef<'_>> {
        self.tracks().find(|t| t.dbid() == dbid)
    }

    /// The mountpoint the database was read from, if any.
    pub fn mountpoint(&self) -> Option<PathBuf> {
        let p = unsafe { gpod_sys::itdb_get_mountpoint(self.raw.as_ptr()) };
        (!p.is_null()).then(|| {
            PathBuf::from(std::ffi::OsStr::from_bytes(
                unsafe { CStr::from_ptr(p) }.to_bytes(),
            ))
        })
    }

    /// The `FirewireGuid` from the device's SysInfo, if known.
    pub fn firewire_guid(&self) -> Option<String> {
        let device = self.raw().device;
        if device.is_null() {
            return None;
        }
        unsafe {
            let p = gpod_sys::itdb_device_get_sysinfo(device, c"FirewireGuid".as_ptr());
            if p.is_null() {
                return None;
            }
            let guid = CStr::from_ptr(p).to_string_lossy().into_owned();
            glib_sys::g_free(p.cast());
            Some(guid)
        }
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        unsafe { gpod_sys::itdb_free(self.raw.as_ptr()) }
    }
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("tracks", &self.track_count())
            .field("playlists", &self.playlist_count())
            .finish()
    }
}
