//! Safe wrapper around libgpod. This is the only crate in the workspace that
//! contains `unsafe` code.

mod db;
mod error;
mod list;
mod playlist;
pub mod snapshot;
mod track;
mod write;

pub use db::{Database, DeviceInfo};
pub use error::{Error, Result};
pub use playlist::PlaylistRef;
pub use track::TrackRef;
