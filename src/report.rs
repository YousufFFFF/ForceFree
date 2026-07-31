//! Output. The thing that makes ForceFree different from every other cleaner is
//! that it does not just say "you can reclaim 8 GB" — it says what getting it
//! back will cost you, and sorts by that.

use crate::scan::Project;

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
            bytes(p.total_bytes()),
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
                "      {:<28} {:>10}   restore: {} (~{})",
                name,
                bytes(t.bytes),
                t.restore_command,
                duration(t.rebuild_seconds as u64)
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
}
