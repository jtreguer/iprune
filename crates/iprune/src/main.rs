use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about = "Playlist manager and sync tool for the iPod Classic")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print every track and playlist in a stable form for diffing.
    Snapshot {
        /// iPod mountpoint, or an iTunesDB file.
        path: PathBuf,
    },
}

fn snapshot(path: &Path) -> Result<()> {
    let db = if path.is_dir() {
        gpod::Database::open(path)
    } else {
        gpod::Database::open_file(path)
    }?;
    let mut out = BufWriter::new(io::stdout().lock());
    match gpod::snapshot::write(&db, &mut out).and_then(|()| out.flush()) {
        // `iprune snapshot X | head` is not an error.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        r => r.context("writing snapshot"),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Snapshot { path } => snapshot(path),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("iprune: {e:#}");
            ExitCode::from(1)
        }
    }
}
