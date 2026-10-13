use std::ffi::CStr;
use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot parse {}: {message}", path.display())]
    Parse { path: PathBuf, message: String },
    #[error("path contains a NUL byte: {}", .0.display())]
    NulInPath(PathBuf),
    #[error("name contains a NUL byte: {0:?}")]
    NulInName(String),
    #[error("no playlist with id {0:016x}")]
    NoSuchPlaylist(u64),
    #[error(
        "playlist {0:016x} is the master, a smart or the Podcasts playlist, which iprune never changes"
    )]
    ProtectedPlaylist(u64),
    #[error("no track with dbid {0:016x}")]
    NoSuchTrack(u64),
    #[error("writing the iPod database failed: {0}")]
    Write(String),
}

/// Takes ownership of a `GError`, returns its message and frees it.
///
/// # Safety
/// `err` must be null or a valid `GError` that nothing else will free.
pub(crate) unsafe fn take_gerror(err: *mut glib_sys::GError) -> String {
    if err.is_null() {
        return "unknown error".to_owned();
    }
    unsafe {
        let msg = CStr::from_ptr((*err).message)
            .to_string_lossy()
            .into_owned();
        glib_sys::g_error_free(err);
        msg
    }
}
