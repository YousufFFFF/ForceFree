//! ForceFree — find, rank and reclaim the disk space your dev projects eat.
//!
//! Design commitments, in priority order:
//!   1. Never destroy unbacked-up work. Git state gates every deletion.
//!   2. Dry run is the default. Deleting requires an explicit flag.
//!   3. Rank by restore cost, not just size.
//!   4. Ecosystems are data (detectors/*.toml), never code.

mod detector;
mod git;
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

    /// Skip the confirmation prompt. Only meaningful with --reclaim.
    #[arg(long)]
    yes: bool,

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

    let projects = scan::scan(&root, &detectors, args.aggressive);

    if !args.reclaim {
        report::render(&projects);
        return Ok(());
    }

    reclaim::run(&projects, args.yes)
}
