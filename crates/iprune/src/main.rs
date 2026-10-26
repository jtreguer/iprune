use std::io::{self, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

/// Prints a line, ignoring a terminal that is gone (SIGHUP): once a command
/// applies, its output must not stop it.
macro_rules! say {
    ($($arg:tt)*) => {
        let _ = writeln!(io::stdout(), $($arg)*);
    };
}

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use iprune_core::config::{Config, PathStyle, Paths, Scope as ConfigScope};
use iprune_core::store::{Store, devices};
use iprune_core::{adoption, backup, device, exec, playlists, scan, sync};

/// Exit code when the user answers no to a confirmation.
const EXIT_DECLINED: u8 = 4;
/// Exit code when a check before or after writing the iPod failed.
const EXIT_VERIFICATION: u8 = 3;

#[derive(Parser)]
#[command(
    version,
    about = "Playlist manager and sync tool for the iPod Classic and Video"
)]
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
        /// How tracks are written in playlist files (default absolute; with
        /// --force, the current setting when not given).
        #[arg(long, value_enum)]
        path_style: Option<Style>,
        /// Change an existing config file: the library folders and playlists
        /// folder given replace the old ones, every other setting is kept.
        #[arg(long)]
        force: bool,
    },
    /// Index the library and load the playlists.
    Scan,
    /// Match the tracks already on the iPod to library files. Only reads
    /// the iPod.
    Adopt {
        /// Read this iTunesDB file instead of a connected iPod. Report only:
        /// nothing is saved.
        #[arg(long, value_name = "ITUNESDB")]
        file: Option<PathBuf>,
        /// Take the default match for every proposal.
        #[arg(long)]
        accept_proposals: bool,
        /// Replace an earlier adoption of this iPod.
        #[arg(long)]
        force: bool,
    },
    /// Scan the library, plan a sync with the connected iPod and apply it:
    /// track copies, removals, tag updates and playlists.
    Sync {
        /// Show the plan without applying it.
        #[arg(long)]
        plan_only: bool,
        /// List every change, not only the counts.
        #[arg(short, long)]
        verbose: bool,
        /// Print the plan as JSON instead of text.
        #[arg(long, conflicts_with = "verbose", requires = "plan_only")]
        json: bool,
        /// Apply without asking for confirmation.
        #[arg(long)]
        yes: bool,
        /// Allow deleting more than a tenth of the managed playlists.
        #[arg(long)]
        allow_mass_removal: bool,
    },
    /// Find what a crash or an interrupted sync left behind on the iPod and
    /// in the index; with --fix, repair it.
    Doctor {
        /// Repair what was found (asks first).
        #[arg(long)]
        fix: bool,
        /// Repair without asking for confirmation.
        #[arg(long, requires = "fix")]
        yes: bool,
        /// Allow removing many entries or files at once.
        #[arg(long, requires = "fix")]
        allow_mass_removal: bool,
    },
    /// Unmount the iPod and power it off, so it can be unplugged.
    Eject,
    /// List the iPod database backups taken before each sync.
    Backups,
    /// Put a backup back on the connected iPod.
    Restore {
        /// The backup's stamp, as `iprune backups` lists it.
        stamp: String,
        /// The iPod's backup folder, as `iprune backups` lists it. Needed
        /// only when the iPod's GUID cannot be read.
        #[arg(long, value_name = "GUID")]
        guid: Option<String>,
        /// The iPod's mountpoint, when it cannot be found: an iPod whose
        /// database is gone is also looked for by its device files.
        #[arg(long, value_name = "DIR")]
        mountpoint: Option<PathBuf>,
        /// Restore even when the iPod's database cannot be read, so the
        /// plays in its Play Counts file cannot be pulled first. The file is
        /// kept in the backup taken before the restore.
        #[arg(long)]
        discard_play_counts: bool,
        /// Restore without asking for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Show or set what an iPod syncs: the whole library, only some
    /// playlists, or all but some. Each iPod has its own; one never set
    /// follows [sync] in the config.
    Scope {
        #[command(subcommand)]
        set: Option<ScopeCommand>,
        /// The iPod, by its GUID as `iprune backups` lists it, instead of
        /// the connected one.
        #[arg(long, value_name = "GUID", global = true)]
        guid: Option<String>,
    },
    /// List and edit playlists.
    #[command(subcommand)]
    Playlist(PlaylistCommand),
    /// Show or edit the tags of library files: the fields synced to the
    /// iPod. With no field option, prints them. An empty value clears a
    /// field.
    Tag {
        /// Indexed library files.
        #[arg(required = true)]
        files: Vec<PathBuf>,
        #[command(flatten)]
        fields: TagFields,
    },
    /// Print every track and playlist in a stable form for diffing.
    Snapshot {
        /// iPod mountpoint, or an iTunesDB file.
        path: PathBuf,
    },
}

#[derive(clap::Args)]
struct TagFields {
    #[arg(long)]
    title: Option<String>,
    #[arg(long)]
    artist: Option<String>,
    #[arg(long)]
    album_artist: Option<String>,
    #[arg(long)]
    album: Option<String>,
    #[arg(long)]
    composer: Option<String>,
    #[arg(long)]
    genre: Option<String>,
    #[arg(long, value_name = "N", value_parser = number)]
    year: Option<Number>,
    #[arg(long, value_name = "N", value_parser = number)]
    track: Option<Number>,
    #[arg(long, value_name = "N", value_parser = number)]
    track_total: Option<Number>,
    #[arg(long, value_name = "N", value_parser = number)]
    disc: Option<Number>,
    #[arg(long, value_name = "N", value_parser = number)]
    disc_total: Option<Number>,
    #[arg(long, value_enum)]
    compilation: Option<YesNo>,
}

/// A number option's value; none when given empty, to clear the field.
#[derive(Clone, Copy)]
struct Number(Option<u32>);

fn number(s: &str) -> std::result::Result<Number, String> {
    match s.trim() {
        "" => Ok(Number(None)),
        t => t
            .parse()
            .map(|n| Number(Some(n)))
            .map_err(|_| format!("{s:?} is not a whole number")),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum YesNo {
    Yes,
    No,
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

#[derive(Subcommand)]
enum ScopeCommand {
    /// Sync the whole library.
    All,
    /// Sync only these playlists and their tracks.
    Only {
        #[arg(required = true)]
        playlists: Vec<String>,
    },
    /// Sync the whole library but these playlists and their tracks.
    Except {
        #[arg(required = true)]
        playlists: Vec<String>,
    },
    /// Follow [sync] in the config again.
    Default,
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

fn init(
    library: &[PathBuf],
    playlists_dir: &Path,
    style: Option<Style>,
    force: bool,
) -> Result<()> {
    let paths = paths()?;
    let exists = paths.config_file.exists();
    if exists && !force {
        bail!(
            "{} already exists; use --force to change its folders",
            paths.config_file.display()
        );
    }
    // With --force, only what is given changes.
    // One that no longer validates is mended here, so it is read unchecked;
    // one that cannot be read at all is left for the user to fix or remove.
    let mut config = match exists {
        true => Config::load_unchecked(&paths.config_file).with_context(|| {
            format!(
                "the current config cannot be read; fix it, or remove {} to start over",
                paths.config_file.display()
            )
        })?,
        false => Config::default(),
    };
    config.library.roots = library
        .iter()
        .map(|p| absolute_dir(p))
        .collect::<Result<_>>()?;
    config.playlists.dir = absolute_dir(playlists_dir)?;
    match style {
        Some(Style::Absolute) => config.playlists.path_style = PathStyle::Absolute,
        Some(Style::Relative) => config.playlists.path_style = PathStyle::Relative,
        None => {}
    }
    config.validate(&paths.config_file)?;
    config.save(&paths.config_file)?;
    println!("wrote {}", paths.config_file.display());
    // Tracks a new set of folders leaves out are marked missing by the next
    // scan, and the sync after it removes them from the iPod.
    let db = paths.database();
    if db.is_file() {
        let store = iprune_core::store::Store::open(&db)?;
        let outside = iprune_core::store::all_tracks(store.conn())?
            .iter()
            .filter(|t| t.missing_since.is_none())
            .filter(|t| !config.library.roots.iter().any(|r| under(&t.path, r)))
            .count();
        if outside > 0 {
            eprintln!(
                "warning: {outside} indexed tracks are outside the library folders now; the next scan \
                 marks them missing, and the sync after it removes them from the iPod"
            );
        }
    }
    Ok(())
}

/// Whether `path` is inside `root`, compared in NFC as the index looks paths up.
fn under(path: &Path, root: &Path) -> bool {
    use iprune_core::paths::key;
    match (key(path), key(root)) {
        (Some(p), Some(r)) => Path::new(&p).starts_with(&r),
        _ => path.starts_with(root),
    }
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

fn scan_with_progress(config: &Config, store: &mut Store) -> Result<scan::ScanReport> {
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
    let r = scan::scan(store, config, &progress)?;
    if tty {
        eprintln!();
    }
    Ok(r)
}

fn run_scan() -> Result<()> {
    let (config, mut store) = load()?;
    let r = scan_with_progress(&config, &mut store)?;
    io::stdout()
        .lock()
        .write_all(scan::describe(&r).as_bytes())?;
    Ok(())
}

fn find_mountpoint(config: &Config) -> Result<PathBuf> {
    Ok(device::mountpoint(config)?)
}

fn adopt(file: Option<&Path>, accept_proposals: bool, force: bool) -> Result<()> {
    let (config, store) = load()?;
    let conn = store.conn();
    playlists::sync_from_disk(conn, &config, false)?;
    let db = match file {
        Some(f) => gpod::Database::open_file(f)?,
        None => {
            let mp = find_mountpoint(&config)?;
            eprintln!("reading the iPod at {}", mp.display());
            gpod::Database::open(&mp)?
        }
    };
    let tty = io::stderr().is_terminal();
    let progress = |done: usize, total: usize| {
        if tty {
            eprint!("\rcomparing iPod files with the library {done}/{total}      ");
            if done == total {
                eprintln!();
            }
        }
    };
    let run = adoption::run(conn, &db, accept_proposals, &progress)?;
    let report_path = paths()?.data_dir.join("adopt-report.txt");
    std::fs::write(&report_path, adoption::report(&run))
        .with_context(|| format!("writing {}", report_path.display()))?;
    println!("{}", adoption::summary(&run));
    println!("full report: {}", report_path.display());
    if file.is_some() {
        println!("read from a file: nothing saved");
        return Ok(());
    }
    let Some(guid) = run.ipod.firewire_guid.clone() else {
        bail!(
            "the iPod has no FireWire GUID in iPod_Control/Device/SysInfo or SysInfoExtended, nor \
             a USB serial, so it cannot be identified; run ipod-read-sysinfo-extended on it \
             first. Nothing saved"
        );
    };
    if !accept_proposals && !run.adoption.proposals.is_empty() {
        println!(
            "{} proposals need a decision: review the report, then run again with --accept-proposals. Nothing saved",
            run.adoption.proposals.len()
        );
        return Ok(());
    }
    adoption::save(conn, &run, &guid, force)?;
    println!(
        "saved: {} tracks and {} playlists mapped to iPod {guid}",
        run.mapping.len(),
        run.playlists.matched.len()
    );
    Ok(())
}

struct SyncArgs {
    plan_only: bool,
    verbose: bool,
    json: bool,
    yes: bool,
    allow_mass_removal: bool,
}

fn run_sync(args: SyncArgs) -> Result<Outcome> {
    let (config, mut store) = load()?;
    let paths = paths()?;
    let r = scan_with_progress(&config, &mut store)?;
    eprintln!(
        "library: {} unchanged, {} added, {} updated, {} moved, {} missing",
        r.unchanged,
        r.added,
        r.updated + r.returned,
        r.moved.len(),
        r.missing.len()
    );
    let mp = find_mountpoint(&config)?;
    // Held from before the iPod is read until its write is verified, so
    // another iprune cannot change it in between.
    let lock = iprune_core::lock::ipod(&paths)?;
    eprintln!("reading the iPod at {}", mp.display());
    let db = gpod::Database::open(&mp)?;
    let conn = store.conn();
    let prepared = sync::prepare(conn, &config, &db)?;
    if prepared.recovered > 0 {
        eprintln!(
            "took back the mappings of {} tracks an interrupted sync had copied",
            prepared.recovered
        );
    }
    let plan_json = serde_json::to_string_pretty(&prepared.plan)?;
    let plan_path = paths.data_dir.join("plan.json");
    std::fs::write(&plan_path, &plan_json)
        .with_context(|| format!("writing {}", plan_path.display()))?;
    let mut out = io::stdout().lock();
    if args.json {
        writeln!(out, "{plan_json}")?;
    } else {
        write!(out, "{}", sync::describe(&prepared, args.verbose))?;
    }
    if args.plan_only {
        if !prepared.plan.space.fits {
            bail!("the plan needs more space than the iPod has free");
        }
        if !args.json {
            writeln!(
                out,
                "plan saved to {}; nothing was written to the iPod",
                plan_path.display()
            )?;
        }
        return Ok(Outcome::Done);
    }
    let work = exec::full_work(&prepared);
    if work.is_empty() {
        writeln!(out, "\nNothing to apply.")?;
        return Ok(Outcome::Done);
    }
    let t = &work.tracks;
    writeln!(
        out,
        "\nTo apply: {} to copy ({}), {} to remove, {} to retag, {} playlist changes.",
        plural(t.copies.len(), "track", "tracks"),
        sync::bytes(t.copy_bytes()),
        t.removes.len(),
        t.retags.len(),
        work.playlists.ops.len()
    )?;
    let opts = exec::Options {
        allow_mass_removal: args.allow_mass_removal,
    };
    exec::check_guards(&prepared, &work, opts)?;
    if !work.tracks.copies.is_empty() && !prepared.plan.space.fits {
        bail!("the tracks to copy do not fit on the iPod; nothing was changed");
    }
    device::check_writable(&mp, config.device.allow_folder)?;
    drop(out);
    if !args.yes && !confirm(&format!("Apply this plan to the iPod at {}?", mp.display()))? {
        return Ok(Outcome::Declined);
    }
    let _inhibit = applying("Writing to the iPod");
    let tty = io::stderr().is_terminal();
    let progress = |p: exec::Progress| {
        if !tty {
            return;
        }
        // Errors ignored: once the terminal is gone (SIGHUP), writing to it
        // fails, and must not stop the sync.
        let mut err = io::stderr();
        let _ = match p {
            exec::Progress::Removing { done, total } => {
                write!(err, "\rremoving {done}/{total}          ")
            }
            exec::Progress::Copying {
                done,
                total,
                bytes,
                bytes_total,
                name,
            } => write!(
                err,
                "\r\x1b[Kcopying {}/{total} ({} of {}) {name}",
                done + 1,
                sync::bytes(bytes),
                sync::bytes(bytes_total)
            ),
            exec::Progress::Writing => write!(err, "\r\x1b[Kwriting the iPod database…"),
            exec::Progress::Verifying => write!(err, "\r\x1b[Kreading it back to verify…"),
        };
    };
    let done = exec::sync(
        conn, &config, &paths, &lock, &mp, db, &prepared, &work, opts, &progress, &CANCEL,
    );
    if tty {
        let _ = writeln!(io::stderr());
    }
    iprune_core::notify::send(&match &done {
        Ok(o) => iprune_core::notify::synced(o),
        Err(e) => iprune_core::notify::failed(&format!("{e:#}")),
    });
    let done = done?;
    let mut out = io::stdout().lock();
    if done.recovered > 0 {
        writeln!(
            out,
            "an earlier sync stopped before writing; its statistics pull for {} tracks was undone",
            done.recovered
        )?;
    }
    writeln!(
        out,
        "copied {} tracks ({}), copied {} again, removed {}, retagged {}, applied {} playlist changes; verified",
        done.copied,
        sync::bytes(done.bytes),
        done.recopied,
        done.removed,
        done.retagged,
        done.applied
    )?;
    writeln!(
        out,
        "statistics updated for {} tracks ({} new plays); backup {}",
        done.pulled.tracks(),
        done.pulled.plays(),
        done.backup.stamp
    )?;
    for (path, why) in &done.skipped {
        writeln!(out, "not copied, {why}: {}", path.display())?;
    }
    for w in &done.warnings {
        writeln!(out, "warning: {w}")?;
    }
    if let Some(why) = done.stopped {
        bail!(
            "the sync stopped early ({why}); what was done is written and verified, and the next sync continues"
        );
    }
    Ok(Outcome::Done)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn run_doctor(fix: bool, yes: bool, allow_mass_removal: bool) -> Result<Outcome> {
    use iprune_core::{doctor, ipod, store::devices};
    let (config, mut store) = load()?;
    let paths = paths()?;
    let mp = find_mountpoint(&config)?;
    // A repair forgets long-missing tracks: the library is scanned first, so
    // one whose file came back is not forgotten. If the scan fails (a disk
    // not mounted), nothing is forgotten this time.
    let scanned = !fix || {
        match scan_with_progress(&config, &mut store) {
            Ok(_) => true,
            Err(e) => {
                eprintln!(
                    "the library could not be scanned ({e:#}); no track is forgotten this time"
                );
                false
            }
        }
    };
    let conn = store.conn();
    // Held from the examination to the last repair: a sync in between
    // would make the report wrong (its new files would look orphaned).
    let lock = iprune_core::lock::ipod(&paths)?;
    eprintln!("reading the iPod at {}", mp.display());
    let then = sync::identity(&mp)?;
    let db = gpod::Database::open(&mp)?;
    let contents = ipod::read(&db);
    let device = match &contents.firewire_guid {
        Some(g) => devices::device_id(conn, g)?,
        None => None,
    };
    if fix
        && let Some(d) = device
        && let Some(stamp) = devices::restoring(conn, d)?
    {
        return Err(iprune_core::Error::RestoreUnfinished(stamp).into());
    }
    let mut report = doctor::examine(conn, &mp, &contents, device, iprune_core::now_unix())?;
    if !scanned {
        report.forgotten.clear();
    }
    let mut out = io::stdout().lock();
    print_list(
        &mut out,
        "files on the iPod not in its database",
        &report.orphan_files,
        |p| p.display().to_string(),
    )?;
    print_list(
        &mut out,
        "database entries whose file is gone",
        &report.missing_files,
        |(d, what)| format!("{d:016x} {what}"),
    )?;
    print_list(
        &mut out,
        "database entries with a damaged path (not repaired)",
        &report.bad_paths,
        |(d, p)| format!("{d:016x} {p}"),
    )?;
    print_list(
        &mut out,
        "playlists holding a track more than once",
        &report.repeated,
        |(n, managed)| match managed {
            true => format!("{n} (the next sync rebuilds it from its file)"),
            false => format!("{n} (not managed by iprune: left as it is)"),
        },
    )?;
    print_list(
        &mut out,
        "mappings to tracks no longer on the iPod",
        &report.stale_mappings,
        |(id, d)| format!("track {id} -> {d:016x}"),
    )?;
    print_list(
        &mut out,
        "library tracks missing for over 30 days, on no device",
        &report.forgotten,
        |(_, p)| p.display().to_string(),
    )?;
    if report.is_clean() {
        writeln!(out, "nothing to repair")?;
        return Ok(Outcome::Done);
    }
    // Damaged paths are only reported: with nothing else, there is nothing
    // to ask about.
    let repairable = doctor::Report {
        bad_paths: Vec::new(),
        ..report.clone()
    };
    if repairable.is_clean() {
        writeln!(out, "nothing to repair automatically")?;
        return Ok(Outcome::Done);
    }
    if !fix {
        writeln!(out, "run `iprune doctor --fix` to repair")?;
        return Ok(Outcome::Done);
    }
    drop(out);
    device::check_writable(&mp, config.device.allow_folder)?;
    doctor::check_orphans(&report, contents.tracks.len(), allow_mass_removal)?;
    doctor::check_missing(&report, contents.tracks.len(), allow_mass_removal)?;
    if !yes
        && !confirm(&format!(
            "Repair these on the iPod at {} and in the index?",
            mp.display()
        ))?
    {
        return Ok(Outcome::Declined);
    }
    // The report is of the iPod as it was read; it may have been swapped
    // or changed while the prompt waited.
    sync::check_identity(&mp, &then)?;
    let _inhibit = applying("Repairing the iPod");
    // Database entries first, through the sync's backup, checks and
    // verification; then the orphan files, then the index.
    if !report.missing_files.is_empty() {
        if device.is_none() {
            bail!(
                "the iPod has not been adopted: run `iprune adopt` before repairing its database"
            );
        }
        let prepared = sync::prepare(conn, &config, &db)?;
        let work = doctor::repair_work(&prepared, &report);
        let opts = exec::Options { allow_mass_removal };
        let done = exec::sync(
            conn,
            &config,
            &paths,
            &lock,
            &mp,
            db,
            &prepared,
            &work,
            opts,
            &|_| {},
            &CANCEL,
        )?;
        say!(
            "removed {} database entries and verified the iPod database; backup {}",
            done.removed,
            done.backup.stamp
        );
    } else {
        drop(db);
    }
    let deleted = doctor::delete_orphans(&report, contents.tracks.len(), allow_mass_removal)?;
    if deleted > 0 {
        say!(
            "moved {deleted} orphan files to {} on the iPod: delete that folder once you are sure",
            doctor::ORPHANS
        );
    }
    doctor::fix_index(conn, device, &report)?;
    if !report.stale_mappings.is_empty() || !report.forgotten.is_empty() {
        say!(
            "dropped {} stale mappings and {} long-missing tracks from the index",
            report.stale_mappings.len(),
            report.forgotten.len()
        );
    }
    Ok(Outcome::Done)
}

fn run_eject() -> Result<()> {
    let (config, _store) = load()?;
    let mp = find_mountpoint(&config)?;
    // Not while another iprune writes the iPod.
    let _lock = iprune_core::lock::ipod(&paths()?)?;
    match device::eject(&mp)? {
        Some(warning) => println!("{warning}"),
        None => println!("ejected {}: the iPod can be unplugged", mp.display()),
    }
    Ok(())
}

fn run_backups() -> Result<()> {
    let root = paths()?.data_dir.join("backups");
    let mut out = io::stdout().lock();
    let mut guids: Vec<String> = match std::fs::read_dir(&root) {
        Ok(d) => d
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", root.display())),
    };
    guids.sort();
    if guids.is_empty() {
        writeln!(out, "no backups yet")?;
    }
    for guid in guids {
        writeln!(out, "iPod {guid}")?;
        for b in backup::list(&root, &guid)? {
            match backup::summary(&b) {
                Some(s) => writeln!(out, "  {}  {s}", b.stamp)?,
                None => writeln!(out, "  {}", b.stamp)?,
            }
        }
    }
    Ok(())
}

fn run_scope(set: Option<&ScopeCommand>, guid: Option<&str>) -> Result<()> {
    let (config, store) = load()?;
    let conn = store.conn();
    let (guid, mountpoint) = match guid {
        Some(g) => (g.to_owned(), None),
        None => {
            let mp = find_mountpoint(&config)?;
            let g = device::firewire_guid(&mp).ok_or(iprune_core::Error::NoFirewireGuid)?;
            (g, Some(mp))
        }
    };
    let id = devices::device_id(conn, &guid)?
        .ok_or_else(|| iprune_core::Error::NotAdopted(guid.clone()))?;
    playlists::sync_from_disk(conn, &config, false)?;
    let library = iprune_core::store::all_tracks(conn)?;
    let lists = devices::library_playlists(conn)?;
    let chosen = |kind, names: &[String]| {
        let mut playlists = names.to_vec();
        playlists.sort();
        playlists.dedup();
        devices::DeviceScope { kind, playlists }
    };
    let new = match set {
        None => None,
        Some(ScopeCommand::All) => Some(Some(chosen(ConfigScope::All, &[]))),
        Some(ScopeCommand::Only { playlists }) => {
            Some(Some(chosen(ConfigScope::Playlists, playlists)))
        }
        Some(ScopeCommand::Except { playlists }) => {
            Some(Some(chosen(ConfigScope::Except, playlists)))
        }
        Some(ScopeCommand::Default) => Some(None),
    };
    if let Some(new) = new {
        if let Some(why) = new
            .as_ref()
            .and_then(|s| sync::scope_problem(s, true, &library, &lists))
        {
            bail!("{why}; nothing changed");
        }
        devices::set_scope(conn, id, new.as_ref())?;
    }
    let (scope, own) = sync::device_scope(conn, &config, id)?;
    let row = devices::devices(conn)?.into_iter().find(|d| d.id == id);
    let name = row
        .as_ref()
        .and_then(|d| d.name.clone())
        .unwrap_or_else(|| "iPod".into());
    let guid = row.map_or(guid, |d| d.firewire_guid);
    let mut out = io::stdout().lock();
    writeln!(out, "{name} ({guid})")?;
    let from = if own {
        "set for this iPod"
    } else {
        "from [sync] in the config"
    };
    writeln!(out, "syncs {}, {from}", scope.describe())?;
    let (tracks, bytes) = scope.size(&library, &lists);
    let room = mountpoint
        .and_then(|m| sync::space(&m).ok())
        .map(|s| format!("; the iPod holds {}", sync::bytes(s.capacity)))
        .unwrap_or_default();
    writeln!(out, "{tracks} tracks, {}{room}", sync::bytes(bytes))?;
    Ok(())
}

fn run_restore(
    stamp: &str,
    guid: Option<&str>,
    mountpoint: Option<&Path>,
    discard_play_counts: bool,
    yes: bool,
) -> Result<Outcome> {
    let (config, store) = load()?;
    let paths = paths()?;
    let mp = match mountpoint {
        Some(m) => m.to_owned(),
        None => match device::mountpoint(&config) {
            Ok(m) => m,
            // An iPod whose iTunesDB is gone, the state a restore is for.
            Err(iprune_core::Error::NoIpod) => {
                match device::find_ipods_without_database()?.as_slice() {
                    [one] => one.mountpoint.clone(),
                    [] => bail!(
                        "no mounted iPod found, with or without a database; connect and mount it, \
                         or pass --mountpoint"
                    ),
                    _ => bail!("several iPods without a database are mounted; pass --mountpoint"),
                }
            }
            Err(e) => return Err(e.into()),
        },
    };
    // From the device files, not the database: the database may be damaged.
    let read = device::firewire_guid(&mp);
    if let (Some(read), Some(given)) = (&read, guid)
        && device::normalize_guid(read) != device::normalize_guid(given)
    {
        return Err(iprune_core::Error::GuidMismatch {
            ipod: read.clone(),
            given: given.to_owned(),
        }
        .into());
    }
    let guid = exec::backup_guid(&paths, read.as_deref().or(guid))?;
    device::check_writable(&mp, config.device.allow_folder)?;
    let backups = paths.data_dir.join("backups");
    let what = backup::list(&backups, &guid)?
        .into_iter()
        .find(|b| b.stamp == stamp)
        .and_then(|b| backup::summary(&b))
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    if !yes
        && !confirm(&format!(
            "Put backup {stamp} of iPod {guid}{what} back on the iPod at {}, replacing its database? \
             What it holds now is backed up first.",
            mp.display()
        ))?
    {
        return Ok(Outcome::Declined);
    }
    let _inhibit = applying("Restoring the iPod database");
    let r = exec::restore(
        store.conn(),
        &config,
        &paths,
        &mp,
        &guid,
        stamp,
        discard_play_counts,
    )?;
    say!(
        "saved what the iPod held as backup {}; statistics pulled for {} tracks ({} plays)",
        r.saved.stamp,
        r.pulled.tracks(),
        r.pulled.plays()
    );
    if !r.set_aside.is_empty() {
        say!(
            "took {} off the iPod: kept in backup {}",
            r.set_aside.join(" and "),
            r.saved.stamp
        );
    }
    say!(
        "restored {} and read it back; mappings back to {} tracks and {} playlists",
        r.files.join(", "),
        r.tracks,
        r.playlists
    );
    if r.dropped_tracks > 0 {
        say!(
            "{} track mappings dropped: their library file is gone",
            r.dropped_tracks
        );
    }
    if r.playlists_to_delete > 0 {
        say!(
            "{} iPod playlists came back whose library playlist is gone; the next sync deletes them",
            r.playlists_to_delete
        );
    }
    Ok(Outcome::Done)
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

fn run_tag(files: &[PathBuf], f: &TagFields) -> Result<()> {
    use iprune_core::tag_edit::{self, Changes, Outcome};
    let text = |v: &Option<String>| v.clone().map(Some);
    let changes = Changes {
        title: text(&f.title),
        artist: text(&f.artist),
        album: text(&f.album),
        album_artist: text(&f.album_artist),
        composer: text(&f.composer),
        genre: text(&f.genre),
        year: f.year.map(|n| n.0),
        track_no: f.track.map(|n| n.0),
        track_total: f.track_total.map(|n| n.0),
        disc_no: f.disc.map(|n| n.0),
        disc_total: f.disc_total.map(|n| n.0),
        compilation: f.compilation.map(|c| c == YesNo::Yes),
    };
    // As in the window: one name and one track number make no sense for
    // several songs.
    if files.len() > 1 && (f.title.is_some() || f.track.is_some()) {
        use clap::CommandFactory;
        Cli::command()
            .error(
                clap::error::ErrorKind::ArgumentConflict,
                "--title and --track take a single file",
            )
            .exit();
    }
    if let Err(why) = changes.cleaned().check() {
        use clap::CommandFactory;
        Cli::command()
            .error(clap::error::ErrorKind::ValueValidation, why)
            .exit();
    }
    let (_, store) = load()?;
    let conn = store.conn();
    let cwd = std::env::current_dir().context("reading the current folder")?;
    // Each file given, with its track when it is indexed.
    let mut given = Vec::new();
    for file in files {
        let abs = iprune_core::paths::absolute(file, &cwd);
        let track = match iprune_core::paths::key(&abs) {
            Some(k) => iprune_core::store::present_track_by_key(conn, &k)?,
            None => None,
        };
        given.push((abs, track));
    }
    let not_indexed = "not in the library index; run `iprune scan` first";
    let mut refused = 0;
    if changes.is_empty() {
        let mut out = io::stdout().lock();
        for (abs, track) in &given {
            let Some(t) = track else {
                refused += 1;
                writeln!(out, "refused    {}: {not_indexed}", abs.display())?;
                continue;
            };
            let g = &t.tags;
            let text = |v: &Option<String>| v.clone().unwrap_or_default();
            let num = |v: Option<u32>| v.map(|n| n.to_string()).unwrap_or_default();
            writeln!(out, "{}", t.path.display())?;
            for (name, value) in [
                ("title", text(&g.title)),
                ("artist", text(&g.artist)),
                ("album artist", text(&g.album_artist)),
                ("album", text(&g.album)),
                ("composer", text(&g.composer)),
                ("genre", text(&g.genre)),
                ("year", num(g.year)),
                ("track", num(g.track_no)),
                ("track total", num(g.track_total)),
                ("disc", num(g.disc_no)),
                ("disc total", num(g.disc_total)),
                (
                    "compilation",
                    (if g.compilation { "yes" } else { "no" }).into(),
                ),
            ] {
                writeln!(out, "  {name:<14}{value}")?;
            }
        }
    } else {
        let ids: Vec<i64> = given
            .iter()
            .filter_map(|(_, t)| t.as_ref().map(|t| t.id))
            .collect();
        let _held = applying("Writing tags to library files");
        let report = tag_edit::edit(conn, &ids, &changes, &mut |_| {}, &CANCEL)?;
        let mut shown = std::collections::HashSet::new();
        for (abs, track) in &given {
            let Some(t) = track else {
                refused += 1;
                say!("refused    {}: {not_indexed}", abs.display());
                continue;
            };
            // A file given twice is edited, and reported, once.
            if !shown.insert(t.id) {
                continue;
            }
            match report
                .results
                .iter()
                .find(|r| r.track_id == t.id)
                .map(|r| &r.outcome)
            {
                Some(Outcome::Written) => {
                    say!("written    {}", t.path.display());
                }
                Some(Outcome::Unchanged) => {
                    say!("unchanged  {}", t.path.display());
                }
                Some(Outcome::Refused(why)) => {
                    refused += 1;
                    say!("refused    {}: {why}", t.path.display());
                }
                None => {
                    refused += 1;
                    say!("stopped    {}: not edited", t.path.display());
                }
            }
        }
        if let Some(e) = &report.index_error {
            bail!("the index could not be updated: {e}");
        }
    }
    if refused > 0 {
        bail!("{refused} of {} files were not edited", files.len());
    }
    Ok(())
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

/// Set once a command starts changing the iPod or the index after its
/// confirmation: a signal then asks it to stop at the next safe point
/// instead of ending the process.
static APPLYING: AtomicBool = AtomicBool::new(false);
/// Asked to stop: copies end after the current file, and what was done is
/// written and verified.
static CANCEL: AtomicBool = AtomicBool::new(false);

/// Ctrl+C, SIGTERM (logout, shutdown) and SIGHUP (the terminal closed) end
/// iprune at once until it applies anything; from then on they only ask it
/// to stop, and it never exits in the middle of a write. A copy that hangs
/// is ended with `kill -9`, which leaves an orphan `iprune doctor` finds.
fn install_signals() {
    let _ = ctrlc::set_handler(|| {
        if !APPLYING.load(Ordering::SeqCst) {
            std::process::exit(130);
        }
        let msg = if CANCEL.swap(true, Ordering::SeqCst) {
            "\nstill finishing: iprune stops once it is safe to"
        } else {
            "\nstopping as soon as it is safe to: what was done is written and verified first…"
        };
        let _ = writeln!(io::stderr(), "{msg}");
    });
}

/// Marks the command as applying, and keeps the machine from suspending or
/// shutting down until the returned inhibitor is dropped.
fn applying(why: &str) -> Option<iprune_core::inhibit::Inhibitor> {
    APPLYING.store(true, Ordering::SeqCst);
    let held = iprune_core::inhibit::hold(why);
    if held.is_none() {
        let _ = writeln!(
            io::stderr(),
            "could not keep the machine from sleeping (systemd-inhibit): do not suspend it until this is done"
        );
    }
    held
}

fn main() -> ExitCode {
    install_signals();
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Init {
            library,
            playlists,
            path_style,
            force,
        } => init(library, playlists, *path_style, *force).map(|()| Outcome::Done),
        Command::Scan => run_scan().map(|()| Outcome::Done),
        Command::Adopt {
            file,
            accept_proposals,
            force,
        } => adopt(file.as_deref(), *accept_proposals, *force).map(|()| Outcome::Done),
        Command::Sync {
            plan_only,
            verbose,
            json,
            yes,
            allow_mass_removal,
        } => run_sync(SyncArgs {
            plan_only: *plan_only,
            verbose: *verbose,
            json: *json,
            yes: *yes,
            allow_mass_removal: *allow_mass_removal,
        }),
        Command::Doctor {
            fix,
            yes,
            allow_mass_removal,
        } => run_doctor(*fix, *yes, *allow_mass_removal),
        Command::Eject => run_eject().map(|()| Outcome::Done),
        Command::Backups => run_backups().map(|()| Outcome::Done),
        Command::Restore {
            stamp,
            guid,
            mountpoint,
            discard_play_counts,
            yes,
        } => run_restore(
            stamp,
            guid.as_deref(),
            mountpoint.as_deref(),
            *discard_play_counts,
            *yes,
        ),
        Command::Scope { set, guid } => {
            run_scope(set.as_ref(), guid.as_deref()).map(|()| Outcome::Done)
        }
        Command::Playlist(cmd) => playlist(cmd),
        Command::Tag { files, fields } => run_tag(files, fields).map(|()| Outcome::Done),
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
            // The terminal may be gone (SIGHUP): the exit code still tells.
            let _ = writeln!(io::stderr(), "iprune: {e:#}");
            let verification = e
                .downcast_ref::<iprune_core::Error>()
                .is_some_and(iprune_core::Error::is_check_failure);
            ExitCode::from(if verification { EXIT_VERIFICATION } else { 1 })
        }
    }
}
