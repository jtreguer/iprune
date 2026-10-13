//! Changing a database: playlists and writing. Every method takes
//! `&mut self`, so no `TrackRef` or `PlaylistRef` can be alive while the
//! lists they point into change.

use std::collections::HashMap;
use std::ffi::CString;
use std::path::Path;
use std::ptr;

use gpod_sys::{Itdb_Playlist, Itdb_Track};

use crate::db::c_path;
use crate::error::take_gerror;
use crate::list::ListIter;
use crate::{Database, Error, Result};

fn c_name(name: &str) -> Result<CString> {
    CString::new(name).map_err(|_| Error::NulInName(name.to_owned()))
}

impl Database {
    /// A regular playlist by id: the master, smart and Podcasts playlists
    /// belong to the iPod and are refused.
    fn regular_playlist(&mut self, id: u64) -> Result<*mut Itdb_Playlist> {
        let pl = unsafe { gpod_sys::itdb_playlist_by_id(self.raw_mut(), id) };
        if pl.is_null() {
            return Err(Error::NoSuchPlaylist(id));
        }
        let protected = unsafe {
            gpod_sys::itdb_playlist_is_mpl(pl) != 0
                || gpod_sys::itdb_playlist_is_podcasts(pl) != 0
                || (*pl).is_spl != 0
        };
        if protected {
            return Err(Error::ProtectedPlaylist(id));
        }
        Ok(pl)
    }

    /// Creates an empty regular playlist at the end and returns its id.
    pub fn create_playlist(&mut self, name: &str) -> Result<u64> {
        let name = c_name(name)?;
        unsafe {
            let pl = gpod_sys::itdb_playlist_new(name.as_ptr(), 0);
            if pl.is_null() {
                return Err(Error::Write("itdb_playlist_new failed".into()));
            }
            // Assigns a unique id when the playlist has none.
            gpod_sys::itdb_playlist_add(self.raw_mut(), pl, -1);
            Ok((*pl).id)
        }
    }

    /// Renames a regular playlist.
    pub fn rename_playlist(&mut self, id: u64, name: &str) -> Result<()> {
        let name = c_name(name)?;
        let pl = self.regular_playlist(id)?;
        unsafe {
            glib_sys::g_free((*pl).name.cast());
            (*pl).name = glib_sys::g_strdup(name.as_ptr());
        }
        Ok(())
    }

    /// Removes a regular playlist from the database and frees it. Its tracks
    /// stay in the database.
    pub fn remove_playlist(&mut self, id: u64) -> Result<()> {
        let pl = self.regular_playlist(id)?;
        unsafe { gpod_sys::itdb_playlist_remove(pl) };
        Ok(())
    }

    /// Sets a regular playlist's members as a whole, in order, repeats
    /// allowed: clears it, then adds each track. There is no append: setting
    /// the whole list is what keeps a playlist from doubling. Every dbid is
    /// checked before anything changes.
    pub fn set_playlist_members(&mut self, id: u64, dbids: &[u64]) -> Result<()> {
        let pl = self.regular_playlist(id)?;
        let tracks: HashMap<u64, *mut Itdb_Track> = unsafe {
            ListIter::new(self.raw().tracks, self)
                .map(|p| p as *mut Itdb_Track)
                .map(|t| ((*t).dbid, t))
                .collect()
        };
        let members: Vec<*mut Itdb_Track> = dbids
            .iter()
            .map(|d| tracks.get(d).copied().ok_or(Error::NoSuchTrack(*d)))
            .collect::<Result<_>>()?;
        unsafe {
            // Remove the first member until none is left; bounded so a
            // libgpod that refused a removal cannot loop forever.
            let mut left = crate::list::len((*pl).members, &*pl);
            while !(*pl).members.is_null() && left > 0 {
                let first = (*(*pl).members).data as *mut Itdb_Track;
                gpod_sys::itdb_playlist_remove_track(pl, first);
                left -= 1;
            }
            if !(*pl).members.is_null() {
                return Err(Error::Write(format!("could not clear playlist {id:016x}")));
            }
            for t in members {
                gpod_sys::itdb_playlist_add_track(pl, t, -1);
            }
        }
        Ok(())
    }

    /// Tells the device a sync starts. Does nothing on an iPod Classic, but
    /// libgpod expects the pair around a write.
    pub fn start_sync(&mut self) {
        unsafe { gpod_sys::itdb_start_sync(self.raw_mut()) };
    }

    pub fn stop_sync(&mut self) {
        unsafe { gpod_sys::itdb_stop_sync(self.raw_mut()) };
    }

    /// Writes the database back to the iPod it was read from.
    pub fn write(&mut self) -> Result<()> {
        let mut err = ptr::null_mut();
        let ok = unsafe { gpod_sys::itdb_write(self.raw_mut(), &mut err) };
        if ok == 0 {
            return Err(Error::Write(unsafe { take_gerror(err) }));
        }
        if !err.is_null() {
            unsafe { glib_sys::g_error_free(err) };
        }
        Ok(())
    }

    /// Writes the database to a standalone `iTunesDB` file. For tests and
    /// experiments on copies.
    pub fn write_file(&mut self, path: &Path) -> Result<()> {
        let c = c_path(path)?;
        let mut err = ptr::null_mut();
        let ok = unsafe { gpod_sys::itdb_write_file(self.raw_mut(), c.as_ptr(), &mut err) };
        if ok == 0 {
            return Err(Error::Write(unsafe { take_gerror(err) }));
        }
        if !err.is_null() {
            unsafe { glib_sys::g_error_free(err) };
        }
        Ok(())
    }
}
