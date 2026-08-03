//! ForceFree — find, rank and reclaim the disk space your dev projects eat.
//!
//! Design commitments, in priority order:
//!   1. Never destroy unbacked-up work. Git state gates every deletion.
//!   2. Dry run is the default. Deleting requires an explicit flag.
//!   3. Rank by restore cost, not just size.
//!   4. Ecosystems are data (detectors/*.toml), never code.

mod chart;
mod detector;
mod git;
mod links;
mod palette;
mod reclaim;
mod report;
mod scan;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "forcefree",
    version,
    about = "Find, rank and reclaim the disk space your dev projects are quietly eating."
)]
struct Args {
    /// Directories to scan. Give as many as you like; overlapping ones are
    /// merged. Defaults to the current directory.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,

    /// Actually delete. Without this, ForceFree only reports.
    #[arg(long)]
    reclaim: bool,

    /// Include targets marked aggressive_only (build outputs, dist dirs).
    #[arg(long)]
    aggressive: bool,

    /// Skip hard link accounting. Faster, but sizes then include bytes that
    /// deletion would not actually give back.
    #[arg(long)]
    no_link_check: bool,

    /// Skip the confirmation prompt. Only meaningful with --reclaim.
    #[arg(long, requires = "reclaim")]
    yes: bool,

    /// Megabytes per second of rebuild at which reclaiming breaks even. Rows
    /// above this lean left in the chart; rows below lean right.
    #[arg(long, value_name = "MB_PER_SEC", default_value_t = chart::DEFAULT_WORTH_RATE)]
    worth: f64,

    /// Show every target, not just those above the break-even line.
    #[arg(long)]
    all: bool,

    /// List the ecosystems this build knows about, then exit.
    #[arg(long)]
    list_detectors: bool,
}

/// Collect every project, showing what the scan is doing while it does it.
///
/// A whole-disk scan runs for minutes, and the previous behaviour was to print
/// "Scanning ..." and then say nothing at all until it finished. The line is
/// redrawn in place on stderr, and only when stderr is a terminal — piped or
/// redirected output stays clean.
fn scan_with_progress(
    roots: &[PathBuf],
    detectors: &[detector::Detector],
    opts: scan::Options,
) -> Vec<scan::Project> {
    use std::io::{IsTerminal, Write};

    // Nothing to draw on, so skip the bookkeeping entirely.
    if !std::io::stderr().is_terminal() {
        return scan::scan(roots, detectors, opts);
    }

    let mut projects = Vec::new();
    let mut found = 0usize;
    let mut current = String::new();

    scan::scan_with(roots, detectors, opts, &mut |event| match event {
        scan::Event::Found { root, ecosystem } => {
            found += 1;
            // The name of whatever is being measured right now, so a long pause
            // on one enormous directory is explained rather than mysterious.
            current = format!(
                "{} [{ecosystem}]",
                root.file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            );
        }
        scan::Event::Project(p) => projects.push(p),
        scan::Event::Progress(p) => {
            // \r rather than a newline: one line that updates, not a log.
            let _ = write!(
                std::io::stderr(),
                "\r  {} dirs · {} files · {found} projects · {:.40}   ",
                p.dirs,
                p.files,
                current
            );
            let _ = std::io::stderr().flush();
        }
    });

    // Wipe the progress line so it does not collide with the report.
    let _ = write!(std::io::stderr(), "\r{:78}\r", "");
    let _ = std::io::stderr().flush();
    projects
}

fn main() -> Result<()> {
    let args = Args::parse();
    let detectors = detector::load_builtin()?;

    if args.list_detectors {
        println!("{} ecosystems supported:\n", detectors.len());
        for d in &detectors {
            println!(
                "  {:<10} {:<18} markers: {}",
                d.id,
                d.name,
                d.markers.join(", ")
            );
        }
        println!("\nMissing yours? Add detectors/<id>.toml — see CONTRIBUTING.md");
        return Ok(());
    }

    let roots: Vec<PathBuf> = args
        .paths
        .iter()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
        .collect();
    for r in &roots {
        eprintln!("Scanning {} ...", report::display_path(r));
    }

    let projects = scan_with_progress(
        &roots,
        &detectors,
        scan::Options {
            aggressive: args.aggressive,
            skip_link_check: args.no_link_check,
        },
    );

    // Always show the report, including before deleting. Asking "type yes" over
    // a figure the user has never seen itemised is not informed consent.
    report::render(&projects, args.worth, args.all);

    if !args.reclaim {
        return Ok(());
    }

    reclaim::run(&projects, args.yes, args.worth, args.all)
}
