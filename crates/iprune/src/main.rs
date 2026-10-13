use std::io::{self, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use iprune_core::config::{Config, PathStyle, Paths};
use iprune_core::store::Store;
use iprune_core::{playlists, scan};

/// Exit code when the user answers no to a confirmation.
const EXIT_DECLINED: u8 = 4;

#[derive(Parser)]
#[command(version, about = "Playlist manager and sync tool for the iPod Classic")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write the config file.
    Init {
        /// Music folder to index. Repeat for several.
        #[arg(long = "library", required = true, value_name = "DIR")]
        library: Vec<PathBuf>,
        /// Folder holding the .m3u8 playlists.
        #[arg(long, value_name = "DIR")]
        playlists: PathBuf,
        /// How tracks are written in playlist files.
        #[arg(long, value_enum, default_value_t = Style::Absolute)]
        path_style: Style,
        /// Overwrite an existing config file.
        #[arg(long)]
        force: bool,
    },
    /// Index the library and load the playlists.
    Scan,
    /// List and edit playlists.
    #[command(subcommand)]
    Playlist(PlaylistCommand),
    /// Print every track and playlist in a stable form for diffing.
    Snapshot {
        /// iPod mountpoint, or an iTunesDB file.
        path: PathBuf,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Style {
    Absolute,
    Relative,
}

#[derive(Subcommand)]
enum PlaylistCommand {
    /// List playlists with their number of entries.
    List,
    /// Print a playlist's entries with their positions.
    Show { name: String },
    /// Create an empty playlist.
    New { name: String },
    /// Delete a playlist file.
    Rm {
        name: String,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Rename a playlist.
    Rename { old: String, new: String },
    /// Add library files to a playlist.
    Add {
        name: String,
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Insert at this 1-based position instead of at the end.
        #[arg(long)]
        at: Option<usize>,
    },
    /// Remove entries by 1-based position.
    Remove {
        name: String,
        #[arg(required = true)]
        positions: Vec<usize>,
    },
    /// Move the entry at FROM to position TO (both 1-based).
    Move {
        name: String,
        from: usize,
        to: usize,
    },
}

/// Whether a command ran to the end or the user declined.
enum Outcome {
    Done,
    Declined,
}

fn paths() -> Result<Paths> {
    Paths::from_env().ok_or_else(|| anyhow!("cannot determine the home directory"))
}

fn load() -> Result<(Config, Store)> {
    let paths = paths()?;
    if !paths.config_file.exists() {
        bail!(
            "no config at {}; run `iprune init` first",
            paths.config_file.display()
        );
    }
    let config = Config::load(&paths.config_file)?;
    let store = Store::open(&paths.database())?;
    Ok((config, store))
}

fn absolute_dir(p: &Path) -> Result<PathBuf> {
    let abs = std::path::absolute(p).with_context(|| format!("resolving {}", p.display()))?;
    if !abs.is_dir() {
        bail!("{} is not a folder", abs.display());
    }
    Ok(abs)
}

fn init(library: &[PathBuf], playlists_dir: &Path, style: Style, force: bool) -> Result<()> {
    let paths = paths()?;
    if paths.config_file.exists() && !force {
        bail!(
            "{} already exists; use --force to overwrite it",
            paths.config_file.display()
        );
    }
    let mut config = Config::default();
    config.library.roots = library
        .iter()
        .map(|p| absolute_dir(p))
        .collect::<Result<_>>()?;
    config.playlists.dir = absolute_dir(playlists_dir)?;
    config.playlists.path_style = match style {
        Style::Absolute => PathStyle::Absolute,
        Style::Relative => PathStyle::Relative,
    };
    config.save(&paths.config_file)?;
    println!("wrote {}", paths.config_file.display());
    Ok(())
}

fn print_list<T>(
    out: &mut impl Write,
    title: &str,
    items: &[T],
    line: impl Fn(&T) -> String,
) -> io::Result<()> {
    if !items.is_empty() {
        writeln!(out, "{title}: {}", items.len())?;
        for i in items {
            writeln!(out, "  {}", line(i))?;
        }
    }
    Ok(())
}

fn run_scan() -> Result<()> {
    let (config, mut store) = load()?;
    let tty = io::stderr().is_terminal();
    let progress = |p: scan::Progress| {
        if tty {
            match p {
                scan::Progress::Walking { files } => eprint!("\rfound {files} files"),
                scan::Progress::Reading { done, total } => {
                    eprint!("\rreading {done}/{total}      ")
                }
            }
        }
    };
    let r = scan::scan(&mut store, &config, &progress)?;
    if tty {
        eprintln!();
    }
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "library: {} unchanged, {} added, {} updated, {} back from missing, {} moved, {} missing",
        r.unchanged,
        r.added,
        r.updated,
        r.returned,
        r.moved.len(),
        r.missing.len()
    )?;
    print_list(&mut out, "moved", &r.moved, |(a, b)| {
        format!("{} -> {}", a.display(), b.display())
    })?;
    print_list(&mut out, "missing", &r.missing, |p| p.display().to_string())?;
    print_list(&mut out, "unreadable", &r.unreadable, |(p, why)| {
        format!("{}: {why}", p.display())
    })?;
    print_list(&mut out, "unsupported", &r.unsupported, |(p, why)| {
        format!("{}: {why}", p.display())
    })?;
    print_list(&mut out, "names not in UTF-8", &r.non_utf8, |p| {
        p.display().to_string()
    })?;
    print_list(
        &mut out,
        "playlists rewritten for moved files",
        &r.rewritten_playlists,
        |n| n.clone(),
    )?;
    let pl = &r.playlists;
    writeln!(
        out,
        "playlists: {} loaded, {} unresolved entries",
        pl.loaded,
        pl.unresolved.len()
    )?;
    print_list(&mut out, "unresolved", &pl.unresolved, |(n, e)| {
        format!("{n}: {e}")
    })?;
    print_list(
        &mut out,
        "playlists removed from the index",
        &pl.removed,
        |n| n.clone(),
    )?;
    print_list(
        &mut out,
        "playlist files skipped",
        &pl.skipped,
        |(p, why)| format!("{}: {why}", p.display()),
    )?;
    Ok(())
}

fn confirm(question: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("{question} needs confirmation; pass --yes");
    }
    eprint!("{question} [y/N] ");
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn playlist(cmd: &PlaylistCommand) -> Result<Outcome> {
    let (config, store) = load()?;
    let conn = store.conn();
    let mut out = io::stdout().lock();
    match cmd {
        PlaylistCommand::List => {
            playlists::sync_from_disk(conn, &config, false)?;
            for p in iprune_core::store::playlist_summaries(conn)? {
                let unresolved = if p.unresolved > 0 {
                    format!("  ({} unresolved)", p.unresolved)
                } else {
                    String::new()
                };
                writeln!(out, "{:>5}  {}{unresolved}", p.entries, p.name)?;
            }
        }
        PlaylistCommand::Show { name } => {
            for (i, item) in playlists::show(conn, &config, name)?.iter().enumerate() {
                let what = match &item.track {
                    Some(t) => format!(
                        "{} - {}",
                        t.tags.artist.as_deref().unwrap_or("?"),
                        t.display_title()
                    ),
                    None => format!("[unresolved] {}", item.entry.location),
                };
                writeln!(out, "{:>4}  {what}", i + 1)?;
            }
        }
        PlaylistCommand::New { name } => playlists::create(conn, &config, name)?,
        PlaylistCommand::Rm { name, yes } => {
            // Check it exists before asking.
            playlists::show(conn, &config, name)?;
            if !yes && !confirm(&format!("delete playlist {name:?}?"))? {
                return Ok(Outcome::Declined);
            }
            playlists::delete(conn, &config, name)?;
        }
        PlaylistCommand::Rename { old, new } => playlists::rename(conn, &config, old, new)?,
        PlaylistCommand::Add { name, files, at } => {
            playlists::add(conn, &config, name, files, *at)?
        }
        PlaylistCommand::Remove { name, positions } => {
            playlists::remove(conn, &config, name, positions)?
        }
        PlaylistCommand::Move { name, from, to } => {
            playlists::move_entry(conn, &config, name, *from, *to)?
        }
    }
    Ok(Outcome::Done)
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
        Command::Init {
            library,
            playlists,
            path_style,
            force,
        } => init(library, playlists, *path_style, *force).map(|()| Outcome::Done),
        Command::Scan => run_scan().map(|()| Outcome::Done),
        Command::Playlist(cmd) => playlist(cmd),
        Command::Snapshot { path } => snapshot(path).map(|()| Outcome::Done),
    };
    match result {
        Ok(Outcome::Done) => ExitCode::SUCCESS,
        Ok(Outcome::Declined) => ExitCode::from(EXIT_DECLINED),
        Err(e)
            if e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe) =>
        {
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("iprune: {e:#}");
            ExitCode::from(1)
        }
    }
}
