//! Output. The thing that makes ForceFree different from every other cleaner is
//! that it does not just say "you can reclaim 8 GB" — it says what getting it
//! back will cost you, and sorts by that.

use crate::scan::{Project, Target};

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

pub fn duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// A size we could not measure completely is prefixed `~` and never presented
/// as if it were exact.
pub fn size_of(t: &Target) -> String {
    if t.unreadable > 0 {
        format!("~{}", bytes(t.bytes))
    } else {
        bytes(t.bytes)
    }
}

fn unreadable_note(n: u32) -> String {
    if n == 0 {
        String::new()
    } else {
        format!("  [{n} unreadable]")
    }
}

/// Same for a whole project: if any target inside it was incomplete, its total
/// is a lower bound too.
fn project_size(p: &Project) -> String {
    if p.unreadable() > 0 {
        format!("~{}", bytes(p.total_bytes()))
    } else {
        bytes(p.total_bytes())
    }
}

/// One line explaining every `~` in the output, printed only when there is one.
pub fn lower_bound_warning(n: u32) -> Option<String> {
    (n > 0).then(|| {
        let noun = if n == 1 { "entry" } else { "entries" };
        format!("Note: {n} {noun} could not be read. Sizes marked ~ are lower bounds.")
    })
}

pub fn render(projects: &[Project]) {
    if projects.is_empty() {
        println!("Nothing reclaimable found.");
        return;
    }

    let (mut safe_bytes, mut safe_secs) = (0u64, 0u64);
    let mut blocked_bytes = 0u64;

    println!();
    for p in projects {
        let safe = p.git_state.is_safe_to_reclaim();
        let marker = if safe { "  " } else { "! " };

        println!(
            "{marker}{}  [{}]  {}  ({})",
            p.root.display(),
            p.ecosystem,
            project_size(p),
            p.git_state.label()
        );

        // Cheapest-to-restore first: that is the order you should delete in.
        let mut targets = p.targets.clone();
        targets.sort_by(|a, b| b.efficiency().partial_cmp(&a.efficiency()).unwrap());

        for t in &targets {
            let name = t
                .path
                .strip_prefix(&p.root)
                .unwrap_or(&t.path)
                .display()
                .to_string();
            println!(
                "      {:<28} {:>10}   restore: {} (~{}){}",
                name,
                size_of(t),
                t.restore_command,
                duration(t.rebuild_seconds as u64),
                unreadable_note(t.unreadable),
            );
        }

        if safe {
            safe_bytes += p.total_bytes();
            safe_secs += p
                .targets
                .iter()
                .map(|t| t.rebuild_seconds as u64)
                .sum::<u64>();
        } else {
            blocked_bytes += p.total_bytes();
        }
        println!();
    }

    println!("─────────────────────────────────────────────");
    println!("Reclaimable now : {}", bytes(safe_bytes));
    println!("Cost to restore : ~{}", duration(safe_secs));
    if let Some(w) = lower_bound_warning(projects.iter().map(|p| p.unreadable()).sum()) {
        println!("{w}");
    }
    if blocked_bytes > 0 {
        println!(
            "Held back       : {} in repos with uncommitted or unpushed work",
            bytes(blocked_bytes)
        );
    }
    println!("\nRun with --reclaim to delete. Nothing has been touched.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes_readably() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1024), "1.0 KB");
        assert_eq!(bytes(1024 * 1024 * 3), "3.0 MB");
    }

    #[test]
    fn formats_durations_readably() {
        assert_eq!(duration(45), "45s");
        assert_eq!(duration(90), "1m 30s");
        assert_eq!(duration(3700), "1h 1m");
    }

    fn target(bytes: u64, unreadable: u32) -> Target {
        Target {
            path: "node_modules".into(),
            bytes,
            unreadable,
            restore_command: "npm ci".into(),
            rebuild_seconds: 45,
        }
    }

    #[test]
    fn incomplete_measurements_are_never_shown_as_exact() {
        assert_eq!(size_of(&target(1024, 0)), "1.0 KB");
        assert_eq!(size_of(&target(1024, 7)), "~1.0 KB");
    }

    #[test]
    fn lower_bound_warning_only_appears_when_something_was_missed() {
        assert!(lower_bound_warning(0).is_none());
        assert!(lower_bound_warning(1).unwrap().contains("1 entry could"));
        assert!(lower_bound_warning(3).unwrap().contains("3 entries could"));
    }
}
