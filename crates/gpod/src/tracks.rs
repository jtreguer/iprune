//! Adding, copying, updating and removing tracks. Like playlist changes,
//! every method takes `&mut self`, so no borrowed track can outlive a change.

use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};

use gpod_sys::{Itdb_Playlist, Itdb_Track};

use crate::db::c_path;
use crate::error::take_gerror;
use crate::list::ListIter;
use crate::{Database, Error, Result};

/// What iprune sets on a track. Text left `None` is left as it is on an
/// existing track, and empty on a new one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackFields {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub albumartist: Option<String>,
    pub composer: Option<String>,
    pub genre: Option<String>,
    pub year: Option<i32>,
    pub track_nr: Option<i32>,
    pub tracks: Option<i32>,
    pub cd_nr: Option<i32>,
    pub cds: Option<i32>,
    pub compilation: Option<bool>,
}

impl TrackFields {
    /// Refuses text the iPod cannot hold (a NUL), as setting these fields
    /// would, without changing anything.
    pub fn check(&self) -> Result<()> {
        let texts = [
            &self.title,
            &self.artist,
            &self.album,
            &self.albumartist,
            &self.composer,
            &self.genre,
        ];
        match texts.into_iter().flatten().find(|v| v.contains('\0')) {
            Some(v) => Err(Error::NulInName(v.clone())),
            None => Ok(()),
        }
    }
}

/// The audio facts of a file, set when it is copied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioFields {
    pub size: u32,
    /// Milliseconds.
    pub tracklen: i32,
    /// kbit/s.
    pub bitrate: i32,
    pub samplerate: u16,
    /// As iTunes names it, for example "MPEG audio file".
    pub filetype: String,
    /// Unix times. A `time_added` of 0 leaves an existing track's date.
    pub time_added: i64,
    pub time_modified: i64,
}

/// A track not yet in a database. It owns its `Itdb_Track` and frees it if
/// dropped before [`Database::add_track`] takes it.
struct NewTrack {
    raw: NonNull<Itdb_Track>,
}

impl NewTrack {
    fn new() -> Result<Self> {
        let raw = unsafe { gpod_sys::itdb_track_new() };
        NonNull::new(raw)
            .map(|raw| NewTrack { raw })
            .ok_or_else(|| Error::Write("itdb_track_new failed".into()))
    }

    /// Hands the track over; it is no longer freed here.
    fn into_raw(self) -> *mut Itdb_Track {
        let raw = self.raw.as_ptr();
        std::mem::forget(self);
        raw
    }
}

impl Drop for NewTrack {
    fn drop(&mut self) {
        unsafe { gpod_sys::itdb_track_free(self.raw.as_ptr()) }
    }
}

/// Replaces a string field: frees the old value, duplicates the new one.
///
/// # Safety
/// `field` must point to a field of a live track, holding null or a string
/// allocated by GLib.
unsafe fn set_str(field: *mut *mut glib_sys::gchar, value: &str) -> Result<()> {
    let c = CString::new(value).map_err(|_| Error::NulInName(value.to_owned()))?;
    unsafe {
        glib_sys::g_free((*field).cast());
        *field = glib_sys::g_strdup(c.as_ptr());
    }
    Ok(())
}

/// # Safety
/// `t` must be a live track.
unsafe fn apply_fields(t: *mut Itdb_Track, f: &TrackFields) -> Result<()> {
    unsafe {
        let texts = [
            (&f.title, &raw mut (*t).title),
            (&f.artist, &raw mut (*t).artist),
            (&f.album, &raw mut (*t).album),
            (&f.albumartist, &raw mut (*t).albumartist),
            (&f.composer, &raw mut (*t).composer),
            (&f.genre, &raw mut (*t).genre),
        ];
        // Checked first, so a value with a NUL changes nothing.
        f.check()?;
        for (value, field) in texts {
            if let Some(v) = value {
                set_str(field, v)?;
            }
        }
        let numbers = [
            (f.year, &raw mut (*t).year),
            (f.track_nr, &raw mut (*t).track_nr),
            (f.tracks, &raw mut (*t).tracks),
            (f.cd_nr, &raw mut (*t).cd_nr),
            (f.cds, &raw mut (*t).cds),
        ];
        for (value, field) in numbers {
            if let Some(v) = value {
                *field = v;
            }
        }
        if let Some(c) = f.compilation {
            (*t).compilation = u8::from(c);
        }
    }
    Ok(())
}

/// # Safety
/// `t` must be a live track.
unsafe fn apply_audio(t: *mut Itdb_Track, a: &AudioFields) -> Result<()> {
    unsafe {
        (*t).size = a.size;
        (*t).tracklen = a.tracklen;
        (*t).bitrate = a.bitrate;
        (*t).samplerate = a.samplerate;
        // 0 keeps the date the track was first added.
        if a.time_added != 0 {
            (*t).time_added = a.time_added as _;
        }
        (*t).time_modified = a.time_modified as _;
        set_str(&raw mut (*t).filetype, &a.filetype)?;
    }
    Ok(())
}

impl Database {
    fn track_ptr(&mut self, dbid: u64) -> Result<*mut Itdb_Track> {
        unsafe { ListIter::new(self.raw().tracks, self) }
            .map(|p| p as *mut Itdb_Track)
            .find(|&t| unsafe { (*t).dbid } == dbid)
            .ok_or(Error::NoSuchTrack(dbid))
    }

    fn dbids(&self) -> HashSet<u64> {
        unsafe { ListIter::new(self.raw().tracks, self) }
            .map(|p| unsafe { (*(p as *mut Itdb_Track)).dbid })
            .collect()
    }

    /// Adds a track with these fields and this unused `dbid`, at the end of
    /// the database and of the master playlist. No file is copied yet: see
    /// [`Database::copy_track_file`].
    pub fn add_track(
        &mut self,
        dbid: u64,
        fields: &TrackFields,
        audio: &AudioFields,
    ) -> Result<()> {
        if dbid == 0 || self.dbids().contains(&dbid) {
            return Err(Error::Write(format!("dbid {dbid:016x} is zero or in use")));
        }
        let mpl = unsafe { gpod_sys::itdb_playlist_mpl(self.raw_mut()) };
        if mpl.is_null() {
            return Err(Error::Write("the database has no master playlist".into()));
        }
        let track = NewTrack::new()?;
        unsafe {
            let t = track.raw.as_ptr();
            apply_fields(t, fields)?;
            apply_audio(t, audio)?;
            (*t).dbid = dbid;
            (*t).mediatype = 1; // audio
            (*t).visible = 1;
            (*t).transferred = 0;
            let t = track.into_raw();
            gpod_sys::itdb_track_add(self.raw_mut(), t, -1);
            gpod_sys::itdb_playlist_add_track(mpl, t, -1);
            if (*t).dbid != dbid {
                return Err(Error::Write(format!(
                    "libgpod changed the dbid of a new track from {dbid:016x} to {:016x}",
                    (*t).dbid
                )));
            }
        }
        Ok(())
    }

    /// Copies `source` onto the iPod for the track `dbid`, to a new file, and
    /// points the track at it. An existing file is left where it is: its path
    /// is returned so it can be deleted once the new database is written. If
    /// the copy fails, the track still points at its old file.
    pub fn copy_track_file(&mut self, dbid: u64, source: &Path) -> Result<Option<PathBuf>> {
        let t = self.track_ptr(dbid)?;
        let old = self.track_file(dbid)?;
        let c = c_path(source)?;
        let mut err = ptr::null_mut();
        let ok = unsafe {
            // libgpod copies onto the file `ipod_path` names, if any, and
            // deletes it when the copy fails. Without a path it picks a new,
            // unused name.
            let (old_path, old_transferred) = ((*t).ipod_path, (*t).transferred);
            (*t).ipod_path = ptr::null_mut();
            (*t).transferred = 0;
            let ok = gpod_sys::itdb_cp_track_to_ipod(t, c.as_ptr(), &mut err);
            if ok == 0 {
                glib_sys::g_free((*t).ipod_path.cast());
                (*t).ipod_path = old_path;
                (*t).transferred = old_transferred;
            } else {
                glib_sys::g_free(old_path.cast());
            }
            ok
        };
        if ok == 0 {
            return Err(Error::Copy {
                path: source.to_owned(),
                message: unsafe { take_gerror(err) },
            });
        }
        if !err.is_null() {
            unsafe { glib_sys::g_error_free(err) };
        }
        Ok(old)
    }

    /// The track's file on the mounted iPod, if it has one.
    pub fn track_file(&mut self, dbid: u64) -> Result<Option<PathBuf>> {
        let t = self.track_ptr(dbid)?;
        unsafe {
            let p = gpod_sys::itdb_filename_on_ipod(t);
            if p.is_null() {
                return Ok(None);
            }
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(CStr::from_ptr(p).to_bytes()));
            glib_sys::g_free(p.cast());
            Ok(Some(path))
        }
    }

    /// Sets the given fields of an existing track.
    pub fn set_track_fields(&mut self, dbid: u64, fields: &TrackFields) -> Result<()> {
        let t = self.track_ptr(dbid)?;
        unsafe { apply_fields(t, fields) }
    }

    /// Sets the audio facts of an existing track, after a new copy.
    pub fn set_track_audio(&mut self, dbid: u64, audio: &AudioFields) -> Result<()> {
        let t = self.track_ptr(dbid)?;
        unsafe { apply_audio(t, audio) }
    }

    /// Clears what described the track's previous file once new audio
    /// replaced it: gapless data, start and stop times, Sound Check and the
    /// resume position. The iPod would apply them to the new audio (cut it
    /// short, click, play at the wrong level). Play counts, rating, dates and
    /// the album's gapless flag stay. (`samplerate2` needs nothing: libgpod
    /// derives it from the sample rate when it reads the database.)
    pub fn reset_replaced_audio(&mut self, dbid: u64) -> Result<()> {
        let t = self.track_ptr(dbid)?;
        unsafe {
            (*t).pregap = 0;
            (*t).postgap = 0;
            (*t).samplecount = 0;
            (*t).gapless_data = 0;
            (*t).gapless_track_flag = 0;
            (*t).soundcheck = 0;
            (*t).starttime = 0;
            (*t).stoptime = 0;
            (*t).bookmark_time = 0;
        }
        Ok(())
    }

    /// Removes a track: from every playlist first, master and smart ones
    /// included, then from the database. Returns its file, which is not
    /// deleted here: it goes once the new database is written.
    pub fn remove_track(&mut self, dbid: u64) -> Result<Option<PathBuf>> {
        let file = self.track_file(dbid)?;
        let t = self.track_ptr(dbid)?;
        let playlists: Vec<*mut Itdb_Playlist> =
            unsafe { ListIter::new(self.raw().playlists, self) }
                .map(|p| p as *mut Itdb_Playlist)
                .collect();
        unsafe {
            for pl in playlists {
                // Every occurrence, bounded by the playlist's length.
                let mut left = crate::list::len((*pl).members, &*pl);
                while left > 0 && gpod_sys::itdb_playlist_contains_track(pl, t) != 0 {
                    gpod_sys::itdb_playlist_remove_track(pl, t);
                    left -= 1;
                }
                if gpod_sys::itdb_playlist_contains_track(pl, t) != 0 {
                    return Err(Error::Write(format!(
                        "could not take track {dbid:016x} out of a playlist"
                    )));
                }
            }
            gpod_sys::itdb_track_remove(t);
        }
        Ok(file)
    }
}
