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
    /// Directory to scan. Defaults to the current directory.
    #[arg(default_value = ".")]
    path: PathBuf,

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

    let root = args.path.canonicalize().unwrap_or(args.path.clone());
    eprintln!("Scanning {} ...", root.display());

    let projects = scan::scan(
        &root,
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
