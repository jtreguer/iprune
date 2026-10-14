use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("config {}: {message}", path.display())]
    Config { path: PathBuf, message: String },
    #[error("no playlist named {0:?}")]
    NoSuchPlaylist(String),
    #[error("a playlist named {0:?} already exists")]
    PlaylistExists(String),
    #[error("invalid playlist name {0:?}: {1}")]
    InvalidName(String, &'static str),
    #[error("{} is not in the library index; run `iprune scan` first", .0.display())]
    NotIndexed(PathBuf),
    #[error("library root {} is not a readable folder (is the disk mounted?)", .0.display())]
    RootMissing(PathBuf),
    #[error("found no audio files under {}; refusing to mark {tracks} tracks missing (is the disk mounted?)", root.display())]
    EmptyRoot { root: PathBuf, tracks: usize },
    #[error("the library index is empty; run `iprune scan` first")]
    EmptyIndex,
    #[error("the iPod {0} was already adopted; use --force to replace its mapping")]
    AlreadyAdopted(String),
    #[error("{}: not valid UTF-8", .0.display())]
    NotUtf8(PathBuf),
    #[error("{} was changed by another program while iprune was editing it; nothing written, try again", .0.display())]
    ChangedOnDisk(PathBuf),
    #[error(
        "the iPod has no FireWire GUID in iPod_Control/Device/SysInfo or SysInfoExtended, so it cannot be identified"
    )]
    NoFirewireGuid,
    #[error(
        "the iPod {0} has not been adopted; run `iprune adopt` first, or every track would be copied again"
    )]
    NotAdopted(String),
    #[error("no mounted iPod found; connect and mount it, or set device.mountpoint in the config")]
    NoIpod,
    #[error("several iPods are mounted ({0}); set device.mountpoint in the config")]
    SeveralIpods(String),
    #[error("{0}; nothing planned")]
    BadScope(String),
    #[error(transparent)]
    Gpod(#[from] gpod::Error),
    #[error("the backup copy {} differs from the iPod file", .0.display())]
    BackupMismatch(std::path::PathBuf),
    #[error("{0}")]
    NotWritable(String),
    #[error(
        "the plan deletes {deletes} of {managed} managed playlists; if that is intended, run again with --allow-mass-removal"
    )]
    MassRemoval { deletes: usize, managed: usize },
    #[error("the checks before writing failed, nothing was written to the iPod:\n  {}", .0.join("\n  "))]
    PreWriteCheck(Vec<String>),
    #[error(
        "the iPod database was written but does not read back as expected:\n  {}\nrun `iprune restore {stamp}` to put the previous database back",
        problems.join("\n  ")
    )]
    Verification {
        problems: Vec<String>,
        stamp: String,
    },
    #[error(
        "writing the iPod database failed part way ({cause}); the iPod may be partly written: run `iprune restore {stamp}`"
    )]
    WriteIncomplete { cause: String, stamp: String },
    #[error("{cause}; undoing the statistics pull also failed: {undo}")]
    UndoFailed { cause: Box<Error>, undo: Box<Error> },
    #[error("no backup {0:?}; `iprune backups` lists them")]
    NoSuchBackup(String),
    #[error("position {position} is out of range (playlist has {len} entries)")]
    BadPosition { position: usize, len: usize },
}

impl Error {
    /// A check before or after writing the iPod failed (exit code 3), or a
    /// write stopped part way.
    pub fn is_check_failure(&self) -> bool {
        match self {
            Error::PreWriteCheck(_)
            | Error::Verification { .. }
            | Error::WriteIncomplete { .. } => true,
            Error::UndoFailed { cause, .. } => cause.is_check_failure(),
            _ => false,
        }
    }
}

pub(crate) trait IoContext<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|source| Error::Io {
            path: path.into(),
            source,
        })
    }
}
