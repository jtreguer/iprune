use std::borrow::Cow;
use std::ffi::CStr;
use std::marker::PhantomData;
use std::os::raw::c_char;
use std::ptr::NonNull;

use gpod_sys::Itdb_Track;

use crate::Database;

/// A track borrowed from a [`Database`].
#[derive(Clone, Copy)]
pub struct TrackRef<'db> {
    raw: NonNull<Itdb_Track>,
    _db: PhantomData<&'db Database>,
}

/// # Safety
/// `p` must be null or a valid NUL-terminated string living for `'a`.
unsafe fn opt_str<'a>(p: *const c_char) -> Option<Cow<'a, str>> {
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy())
}

macro_rules! str_fields {
    ($($name:ident),*) => {$(
        pub fn $name(&self) -> Option<Cow<'db, str>> {
            unsafe { opt_str(self.raw().$name) }
        }
    )*};
}

macro_rules! num_fields {
    ($($name:ident: $ty:ty),*) => {$(
        pub fn $name(&self) -> $ty {
            self.raw().$name
        }
    )*};
}

impl<'db> TrackRef<'db> {
    /// # Safety
    /// `raw` must point to a track owned by a database that outlives `'db`.
    pub(crate) unsafe fn from_raw(raw: *mut Itdb_Track) -> Self {
        TrackRef {
            raw: NonNull::new(raw).expect("null Itdb_Track"),
            _db: PhantomData,
        }
    }

    fn raw(&self) -> &'db Itdb_Track {
        unsafe { &*self.raw.as_ptr() }
    }

    str_fields!(
        title,
        artist,
        album,
        albumartist,
        composer,
        genre,
        filetype,
        ipod_path
    );

    num_fields!(
        dbid: u64,
        size: u32,
        tracklen: i32,
        bitrate: i32,
        samplerate: u16,
        track_nr: i32,
        tracks: i32,
        cd_nr: i32,
        cds: i32,
        year: i32,
        rating: u32,
        playcount: u32,
        recent_playcount: u32,
        skipcount: u32,
        recent_skipcount: u32,
        time_added: i64,
        time_played: i64,
        mediatype: u32,
        mhii_link: u32
    );

    pub fn compilation(&self) -> bool {
        self.raw().compilation != 0
    }

    pub fn has_thumbnails(&self) -> bool {
        unsafe { gpod_sys::itdb_track_has_thumbnails(self.raw.as_ptr()) != 0 }
    }
}

impl std::fmt::Debug for TrackRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackRef")
            .field("dbid", &format_args!("{:016x}", self.dbid()))
            .field("artist", &self.artist())
            .field("title", &self.title())
            .finish()
    }
}
