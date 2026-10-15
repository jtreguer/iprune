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
        "the iPod has no FireWire GUID in iPod_Control/Device/SysInfo or SysInfoExtended, nor a USB serial, so it cannot be identified"
    )]
    NoFirewireGuid,
    #[error(
        "the iPod {0} has not been adopted; run `iprune adopt` first, or every track would be copied again"
    )]
    NotAdopted(String),
    #[error(
        "adopting the iPod {guid} matched none of its {tracks} tracks, so a sync would copy every track next to them; check that the library is scanned, then run `iprune adopt --force`"
    )]
    NothingMatched { guid: String, tracks: usize },
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
    #[error(
        "the plan removes {removes} of {managed} tracks managed on the iPod; if that is intended, run again with --allow-mass-removal"
    )]
    MassTrackRemoval { removes: usize, managed: usize },
    #[error(
        "the plan copies {missing} of {managed} managed tracks again because their files are missing from the iPod: that many more likely means its music folder is not readable than lost files, unless an older backup was just restored. If that is intended, run again with --allow-mass-removal"
    )]
    MassMissingFiles { missing: usize, managed: usize },
    #[error(
        "{files} files on the iPod are not in its database, which holds {tracks} tracks: that many more likely means a damaged database (see `iprune backups` and `iprune restore`) than leftovers. Nothing deleted; run again with --allow-mass-removal if you are sure"
    )]
    MassOrphans { files: usize, tracks: usize },
    #[error(
        "{entries} of the {tracks} tracks in the iPod database have no file: that many more likely means the iPod's music folder is not readable than lost files. Nothing removed; run again with --allow-mass-removal if you are sure"
    )]
    MassMissing { entries: usize, tracks: usize },
    #[error(
        "the checks before the final write failed, so it was not made; the iPod holds the last checkpoint, written and verified:\n  {}",
        .0.join("\n  ")
    )]
    CheckAfterCheckpoint(Vec<String>),
    #[error("the tracks to copy do not fit on the iPod; nothing was changed")]
    NoSpace,
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
    #[error("each library file can match one iPod track only:\n  {}", .0.join("\n  "))]
    ChoiceConflicts(Vec<String>),
    #[error(
        "another iprune is syncing, repairing, restoring or ejecting the iPod; try again once it is done"
    )]
    IpodBusy,
    #[error("eject: {0}")]
    Eject(String),
    #[error("no backup {0:?}; `iprune backups` lists them")]
    NoSuchBackup(String),
    #[error(
        "the iPod's FireWire GUID cannot be read, so iprune cannot tell which backups are its own; name its folder from `iprune backups` with --guid"
    )]
    RestoreNeedsGuid,
    #[error("the connected iPod is {ipod}, not {given}; nothing was changed")]
    GuidMismatch { ipod: String, given: String },
    #[error("backup {stamp} cannot be read ({why}); nothing was changed")]
    BadBackup { stamp: String, why: String },
    #[error(
        "the iPod's database cannot be read, so the plays in its Play Counts file cannot be pulled first; nothing was changed. Run again with --discard-play-counts to restore anyway: the file is kept in the backup taken before the restore"
    )]
    PlayCountsUnread,
    #[error(
        "backup {stamp} was put back but does not read back as expected:\n  {}\nthe iPod stays marked as being restored. Run `iprune restore {stamp}` again, or `iprune restore {saved}` to put back what it held before",
        problems.join("\n  ")
    )]
    RestoreVerification {
        problems: Vec<String>,
        stamp: String,
        saved: String,
    },
    #[error(
        "the iPod's artwork database cannot be read ({files} cover art files, {lost} tracks whose cover is no longer found): a write would delete the cover art. Nothing was written; `iprune backups` and `iprune restore` can put back an earlier ArtworkDB"
    )]
    ArtworkUnreadable { files: usize, lost: usize },
    #[error(
        "restoring needs {} free on the iPod for its temporary copies and it has {}; nothing was changed",
        crate::sync::bytes(*needed),
        crate::sync::bytes(*free)
    )]
    NoRoomToRestore { needed: u64, free: u64 },
    #[error(
        "the iPod changed since it was read (its database, its Play Counts or the iPod itself); nothing was written. Run it again to see the new plan"
    )]
    IpodChanged,
    #[error(
        "the iPod's database is signed, but libgpod would write it unsigned ({0}), and the iPod would then show no music; nothing was written"
    )]
    WouldNotSign(String),
    #[error(
        "writing the iPod database failed ({0}) and nothing on the iPod changed, but its Play Counts file could not be put back from Play Counts.bak; its plays are already counted, so leave it as it is"
    )]
    WriteRefusedCountsKept(String),
    #[error(
        "new playlists take over iPod playlists iprune did not manage, taking tracks out of them: {}. Those lists exist only on the iPod; if that is intended, run again with --allow-mass-removal",
        .0.join(", ")
    )]
    PlaylistTakeover(Vec<String>),
    #[error(
        "the iPod could not be backed up before the next write ({0}), so that write was not made. The iPod holds the last checkpoint, written and verified; the tracks copied since are files no database entry uses, which `iprune doctor` finds, and the next sync copies them again"
    )]
    CheckpointBackupFailed(String),
    #[error(
        "the plan removes {removes} of the {on_ipod} tracks on the iPod that iprune does not manage (sync.remove_unmanaged): their play counts are not kept, and they may exist nowhere else. If that is intended, run again with --allow-mass-removal"
    )]
    MassUnmanagedRemoval { removes: usize, on_ipod: usize },
    #[error(
        "iPod detection is off (IPRUNE_NO_DETECT is set, as for everything cargo runs); set device.mountpoint in the config, or use the installed iprune"
    )]
    DetectionOff,
    #[error("the index cannot be upgraded: {0}")]
    Migration(String),
    #[error(
        "a restore of backup {0} did not finish, so the iPod and the index may disagree; run `iprune restore {0}` again first"
    )]
    RestoreUnfinished(String),
    #[error("position {position} is out of range (playlist has {len} entries)")]
    BadPosition { position: usize, len: usize },
}

impl Error {
    /// A check before or after writing the iPod failed (exit code 3), or a
    /// write stopped part way.
    pub fn is_check_failure(&self) -> bool {
        match self {
            Error::PreWriteCheck(_)
            | Error::CheckAfterCheckpoint(_)
            | Error::Verification { .. }
            | Error::RestoreVerification { .. }
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
