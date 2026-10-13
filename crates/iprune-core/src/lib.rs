//! Library, playlists, planner and executor. No UI.

pub mod config;
mod error;
pub mod paths;
pub mod playlist;
pub mod playlists;
pub mod scan;
pub mod store;
pub mod tags;

pub use error::{Error, Result};

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
