use std::io::{self, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use iprune_core::config::{Config, PathStyle, Paths};
use iprune_core::store::Store;
use iprune_core::{adoption, backup, device, exec, playlists, scan, sync};

/// Exit code when the user answers no to a confirmation.
const EXIT_DECLINED: u8 = 4;
/// Exit code when a check before or after writing the iPod failed.
const EXIT_VERIFICATION: u8 = 3;

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
    /// List the iPod database backups taken before each sync.
    Backups,
    /// Put a backup back on the connected iPod.
    Restore {
        /// The backup's stamp, as `iprune backups` lists it.
        stamp: String,
        /// Restore without asking for confirmation.
        #[arg(long)]
        yes: bool,
    },
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
    print_list(
        &mut out,
        "indexed without tags (tags could not be parsed)",
        &r.untagged,
        |(p, why)| format!("{}: {why}", p.display()),
    )?;
    print_list(
        &mut out,
        "FairPlay protected, indexed as AAC",
        &r.drm,
        |p| p.display().to_string(),
    )?;
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
            "the iPod has no FirewireGuid in iPod_Control/Device/SysInfo, so it cannot be identified; \
             run ipod-read-sysinfo-extended on it first. Nothing saved"
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
    exec::check_removals(&work.playlists, prepared.playlists_map.len(), opts)?;
    exec::check_track_removals(&work.tracks, prepared.tracks_map.len(), opts)?;
    if !work.tracks.copies.is_empty() && !prepared.plan.space.fits {
        bail!("the tracks to copy do not fit on the iPod; nothing was changed");
    }
    device::check_writable(&mp)?;
    drop(out);
    if !args.yes && !confirm("Apply this plan to the iPod?")? {
        return Ok(Outcome::Declined);
    }
    // Ctrl+C finishes the current file, writes what was done and stops.
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Set while the iPod database is written and read back.
    let writing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (cancel, writing) = (cancel.clone(), writing.clone());
        // A second Ctrl+C ends at once, except during a write: a hung copy
        // leaves only an orphan file, which `iprune doctor` finds.
        let _ = ctrlc::set_handler(move || {
            use std::sync::atomic::Ordering::Relaxed;
            if !cancel.swap(true, Relaxed) {
                eprintln!("\nstopping after the current file (Ctrl+C again to quit now)…");
            } else if writing.load(Relaxed) {
                eprintln!("\nwriting the iPod database; it stops once written and verified…");
            } else {
                std::process::exit(130);
            }
        });
    }
    let tty = io::stderr().is_terminal();
    let progress = |p: exec::Progress| {
        writing.store(
            matches!(p, exec::Progress::Writing | exec::Progress::Verifying),
            std::sync::atomic::Ordering::Relaxed,
        );
        if !tty {
            return;
        }
        match p {
            exec::Progress::Removing { done, total } => {
                eprint!("\rremoving {done}/{total}          ")
            }
            exec::Progress::Copying {
                done,
                total,
                bytes,
                bytes_total,
                name,
            } => eprint!(
                "\r\x1b[Kcopying {}/{total} ({} of {}) {name}",
                done + 1,
                sync::bytes(bytes),
                sync::bytes(bytes_total)
            ),
            exec::Progress::Writing => eprint!("\r\x1b[Kwriting the iPod database…"),
            exec::Progress::Verifying => eprint!("\r\x1b[Kreading it back to verify…"),
        }
    };
    let done = exec::sync(
        conn, &config, &paths, &mp, db, &prepared, &work, opts, &progress, &cancel,
    );
    if tty {
        eprintln!();
    }
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
    let (config, store) = load()?;
    let paths = paths()?;
    let conn = store.conn();
    let mp = find_mountpoint(&config)?;
    eprintln!("reading the iPod at {}", mp.display());
    let db = gpod::Database::open(&mp)?;
    let contents = ipod::read(&db);
    let device = match &contents.firewire_guid {
        Some(g) => devices::device_id(conn, g)?,
        None => None,
    };
    let report = doctor::examine(conn, &mp, &contents, device, iprune_core::now_unix())?;
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
    if !fix {
        writeln!(out, "run `iprune doctor --fix` to repair")?;
        return Ok(Outcome::Done);
    }
    drop(out);
    device::check_writable(&mp)?;
    doctor::check_orphans(&report, contents.tracks.len(), allow_mass_removal)?;
    doctor::check_missing(&report, contents.tracks.len(), allow_mass_removal)?;
    if !yes && !confirm("Repair these on the iPod and in the index?")? {
        return Ok(Outcome::Declined);
    }
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
            &mp,
            db,
            &prepared,
            &work,
            opts,
            &|_| {},
            &std::sync::atomic::AtomicBool::new(false),
        )?;
        println!(
            "removed {} database entries and verified the iPod database; backup {}",
            done.removed, done.backup.stamp
        );
    } else {
        drop(db);
    }
    let deleted = doctor::delete_orphans(&report, contents.tracks.len(), allow_mass_removal)?;
    if deleted > 0 {
        println!("deleted {deleted} orphan files");
    }
    doctor::fix_index(conn, device, &report)?;
    if !report.stale_mappings.is_empty() || !report.forgotten.is_empty() {
        println!(
            "dropped {} stale mappings and {} long-missing tracks from the index",
            report.stale_mappings.len(),
            report.forgotten.len()
        );
    }
    Ok(Outcome::Done)
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
            writeln!(out, "  {}", b.stamp)?;
        }
    }
    Ok(())
}

fn run_restore(stamp: &str, yes: bool) -> Result<Outcome> {
    let (config, store) = load()?;
    let paths = paths()?;
    let mp = find_mountpoint(&config)?;
    // From the device files, not the database: the database may be damaged.
    let guid = exec::backup_guid(&paths, device::firewire_guid(&mp).as_deref())?;
    device::check_writable(&mp)?;
    if !yes
        && !confirm(&format!(
            "Put backup {stamp} back on the iPod at {}, replacing its database?",
            mp.display()
        ))?
    {
        return Ok(Outcome::Declined);
    }
    let r = exec::restore(store.conn(), &paths, &mp, &guid, stamp)?;
    println!(
        "restored {}; mappings back to {} tracks and {} playlists",
        r.files.join(" and "),
        r.tracks,
        r.playlists
    );
    if r.dropped_tracks + r.dropped_playlists > 0 {
        println!(
            "{} track and {} playlist mappings dropped: their file is gone",
            r.dropped_tracks, r.dropped_playlists
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
        Command::Backups => run_backups().map(|()| Outcome::Done),
        Command::Restore { stamp, yes } => run_restore(stamp, *yes),
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
            let verification = e
                .downcast_ref::<iprune_core::Error>()
                .is_some_and(iprune_core::Error::is_check_failure);
            ExitCode::from(if verification { EXIT_VERIFICATION } else { 1 })
        }
    }
}
