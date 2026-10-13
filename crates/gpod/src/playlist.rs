use std::borrow::Cow;
use std::ffi::CStr;
use std::marker::PhantomData;
use std::ptr::NonNull;

use gpod_sys::{Itdb_Playlist, Itdb_Track};

use crate::list::{self, ListIter};
use crate::{Database, TrackRef};

/// A playlist borrowed from a [`Database`].
#[derive(Clone, Copy)]
pub struct PlaylistRef<'db> {
    raw: NonNull<Itdb_Playlist>,
    _db: PhantomData<&'db Database>,
}

impl<'db> PlaylistRef<'db> {
    /// # Safety
    /// `raw` must point to a playlist owned by a database that outlives `'db`.
    pub(crate) unsafe fn from_raw(raw: *mut Itdb_Playlist) -> Self {
        PlaylistRef {
            raw: NonNull::new(raw).expect("null Itdb_Playlist"),
            _db: PhantomData,
        }
    }

    fn raw(&self) -> &'db Itdb_Playlist {
        unsafe { &*self.raw.as_ptr() }
    }

    pub fn name(&self) -> Option<Cow<'db, str>> {
        let p = self.raw().name;
        (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy())
    }

    pub fn id(&self) -> u64 {
        self.raw().id
    }

    pub fn is_master(&self) -> bool {
        unsafe { gpod_sys::itdb_playlist_is_mpl(self.raw.as_ptr()) != 0 }
    }

    /// The Podcasts system playlist.
    pub fn is_podcasts(&self) -> bool {
        unsafe { gpod_sys::itdb_playlist_is_podcasts(self.raw.as_ptr()) != 0 }
    }

    pub fn is_smart(&self) -> bool {
        self.raw().is_spl != 0
    }

    /// Number of entries in `members`, counting repeats.
    pub fn len(&self) -> usize {
        unsafe { list::len(self.raw().members, self.raw()) }
    }

    pub fn is_empty(&self) -> bool {
        self.raw().members.is_null()
    }

    /// Members in playlist order, repeats included.
    pub fn members(&self) -> impl Iterator<Item = TrackRef<'db>> + 'db {
        unsafe { ListIter::new(self.raw().members, self.raw()) }
            .map(|p| unsafe { TrackRef::from_raw(p as *mut Itdb_Track) })
    }
}

impl std::fmt::Debug for PlaylistRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaylistRef")
            .field("name", &self.name())
            .field("len", &self.len())
            .finish()
    }
}
