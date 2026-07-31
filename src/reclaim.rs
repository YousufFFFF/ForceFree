//! Deletion. Deliberately boring and deliberately hard to trigger by accident.

use crate::report::{bytes, duration, lower_bound_warning};
use crate::scan::Project;
use anyhow::Result;
use std::io::{self, Write};

pub fn run(projects: &[Project], skip_prompt: bool) -> Result<()> {
    let eligible: Vec<&Project> = projects
        .iter()
        .filter(|p| p.git_state.is_safe_to_reclaim())
        .collect();

    let held_back = projects.len() - eligible.len();

    if eligible.is_empty() {
        println!("Nothing eligible to reclaim.");
        if held_back > 0 {
            println!("{held_back} project(s) skipped: uncommitted or unpushed work.");
        }
        return Ok(());
    }

    let total: u64 = eligible.iter().map(|p| p.total_bytes()).sum();
    let secs: u64 = eligible
        .iter()
        .flat_map(|p| p.targets.iter())
        .map(|t| t.rebuild_seconds as u64)
        .sum();

    let unreadable: u32 = eligible.iter().map(|p| p.unreadable()).sum();

    println!(
        "\nAbout to delete {}{} across {} project(s).",
        if unreadable > 0 { "at least " } else { "" },
        bytes(total),
        eligible.len()
    );
    println!("Restoring all of it would cost roughly {}.", duration(secs));
    if let Some(w) = lower_bound_warning(unreadable) {
        println!("{w}");
    }
    if held_back > 0 {
        println!("{held_back} project(s) will be skipped: uncommitted or unpushed work.");
    }

    if !skip_prompt {
        print!("\nType 'yes' to continue: ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if answer.trim() != "yes" {
            println!("Aborted. Nothing was deleted.");
            return Ok(());
        }
    }

    let mut freed = 0u64;
    let mut failures = 0usize;

    for p in &eligible {
        for t in &p.targets {
            match std::fs::remove_dir_all(&t.path) {
                Ok(()) => {
                    freed += t.bytes;
                    println!("  removed {} ({})", t.path.display(), bytes(t.bytes));
                }
                Err(e) => {
                    failures += 1;
                    eprintln!("  FAILED {}: {e}", t.path.display());
                }
            }
        }
    }

    println!("\nFreed {}.", bytes(freed));
    if failures > 0 {
        eprintln!("{failures} target(s) could not be removed (in use, or permissions).");
    }
    Ok(())
}
