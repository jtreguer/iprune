//! Library, playlists, planner and executor. No UI.

pub mod adopt;
pub mod adoption;
pub mod backup;
pub mod config;
pub mod device;
pub mod doctor;
mod error;
pub mod exec;
pub mod ipod;
pub mod lock;
pub mod paths;
pub mod plan;
pub mod playlist;
pub mod playlists;
pub mod scan;
pub mod stats;
pub mod store;
pub mod sync;
pub mod tags;
pub mod transfer;
pub mod verify;

pub use error::{Error, Result};

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
