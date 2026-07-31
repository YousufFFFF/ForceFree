//! Output. The thing that makes ForceFree different from every other cleaner is
//! that it does not just say "you can reclaim 8 GB" — it says what getting it
//! back will cost you, and shows that as the shape of the page.
//!
//! Every reclaimable target is one row on a diverging chart: space freed grows
//! left of a centre spine, rebuild time grows right. Rows are ranked best-first,
//! and because the two sides share a scale anchored at a fixed rate (see
//! [`crate::chart`]), the point where rows stop leaning left *is* the point where
//! reclaiming stops being worth the wait. That crossover is drawn as a rule.
//!
//! The chart is the only loud thing here. Everything around it — totals, paths,
//! the held-back list — stays quiet and aligned, because in a terminal alignment
//! and restraint do more work than colour does.

use crate::chart::{self, Bars};
use crate::palette;
use crate::scan::{Project, Target};
use anstream::println;
use anstyle::Reset;

/// Column budget. Fixed rather than measured: this is what a README code block
/// renders at, and the README is where the output has to look right.
/// The widest possible row must fit 79 columns:
/// `2 indent + L + 1 spine + R + 1 overflow + 1 + NAME + 1 + 9 size + 1 + TIME`.
///
/// Bars are 16 rather than 18 so the label can hold 24: `dashboard/node_modules`
/// is 22 characters and truncating the project name off the front of it loses
/// the thing that tells two `node_modules` apart. Two cells of bar resolution
/// buys that back.
const LEFT_FIELD: usize = 16;
const RIGHT_FIELD: usize = 16;
const NAME_FIELD: usize = 24;
/// Seven, because `23h 59m` is a legal duration.
const TIME_FIELD: usize = 7;

/// How many rows past the break-even rule to show, so it is visible that the
/// list continues and visible what it continues into.
const ROWS_BELOW_LINE: usize = 3;

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
        let (m, s) = (secs / 60, secs % 60);
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m {s}s")
        }
    } else {
        let (h, m) = (secs / 3600, (secs % 3600) / 60);
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        }
    }
}

/// A size we could not measure completely is prefixed `~` and never presented
/// as if it were exact.
pub fn size_of(t: &Target) -> String {
    let n = bytes(t.reclaimable_bytes());
    if t.unreadable > 0 {
        format!("~{n}")
    } else {
        n
    }
}

/// Explains the gap between what these directories look like and what removing
/// them would give back. Only printed when there is a gap.
///
/// Two things land here: bytes hard linked from outside (a pnpm store keeps
/// them alive), and the same physical file counted twice inside one target.
/// Neither returns to the disk, so both belong in the same caveat.
pub fn shared_warning(shared: u64) -> Option<String> {
    (shared > 0).then(|| {
        format!(
            "{} of the apparent size is hard linked and would not come back \
             — shared package stores, or one file counted under two names.",
            bytes(shared)
        )
    })
}

/// One line explaining every `~` in the output, printed only when there is one.
pub fn lower_bound_warning(n: u32) -> Option<String> {
    (n > 0).then(|| {
        let noun = if n == 1 { "entry" } else { "entries" };
        format!("{n} {noun} could not be read. Sizes marked ~ are lower bounds.")
    })
}

/// Greedy word wrap. Notes are prose, and prose that runs past the terminal
/// wraps wherever the terminal decides — usually mid-word and always ragged,
/// which undoes the alignment everything else here works to keep.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let extra = if line.is_empty() { 0 } else { 1 };
        if !line.is_empty() && line.chars().count() + extra + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        } else if extra == 1 {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// A wrapped, indented, dimmed note.
fn note(text: &str) {
    let dim = palette::dim();
    for line in wrap(text, 76) {
        println!("  {dim}{line}{dim:#}");
    }
}

/// `project/target`, which is what identifies a row to a human. Truncated from
/// the left because the tail carries the information.
fn row_label(p: &Project, t: &Target) -> String {
    let target = t
        .path
        .strip_prefix(&p.root)
        .unwrap_or(&t.path)
        .display()
        .to_string();
    let project = p
        .root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let full = if project.is_empty() {
        target
    } else {
        format!("{project}/{target}")
    };
    chart::truncate_left(&full, NAME_FIELD)
}

/// The command that puts this back, printed under its row.
///
/// Not decoration: a reclaimable directory is one that can be regenerated by a
/// stated command, and stating it is what makes the number above it a *cost*
/// rather than just a loss. Dim and indented so it groups with its row without
/// competing with the bars.
fn restore_line(t: &Target) -> String {
    let dim = palette::dim();
    // Aligned under the label column.
    let indent = 2 + LEFT_FIELD + 1 + RIGHT_FIELD + 2;
    format!(
        "{:indent$}{dim}{}{dim:#}",
        "",
        // Head-first: what the command *starts* with is what identifies it.
        chart::truncate_right(&t.restore_command, 78 - indent)
    )
}

fn bar_row(b: Bars, label: &str, size: &str, time: &str) -> String {
    let space = palette::space_freed();
    let cost = palette::time_cost();
    let spine = palette::structure();
    let over = if b.overflowed { "▸" } else { " " };

    // `chart::bars` clamps to the field, but clamp again rather than let an
    // arithmetic underflow two modules away panic a tool people point at their
    // disk. Saturating here also keeps the spine in its column no matter what.
    let left = b.left.min(LEFT_FIELD);
    let right = b.right.min(RIGHT_FIELD);

    format!(
        "  {pad}{space}{bar_l}{space:#}{spine}│{spine:#}{cost}{bar_r}{cost:#}{tail}{over} \
         {label:<NAME_FIELD$} {size:>9} {time:>TIME_FIELD$}",
        pad = " ".repeat(LEFT_FIELD - left),
        bar_l = "█".repeat(left),
        bar_r = "█".repeat(right),
        tail = " ".repeat(RIGHT_FIELD - right),
    )
}

pub fn render(projects: &[Project], worth_rate: f64, show_all: bool) {
    if projects.is_empty() {
        println!("Nothing reclaimable found.");
        return;
    }

    // Ranking lives here, not in the scan. One flat list across every project,
    // because "what should I delete first" does not respect project boundaries —
    // grouping would bury the ranking it exists to show.
    let mut rows: Vec<(&Project, &Target)> = projects
        .iter()
        .filter(|p| p.git_state.is_safe_to_reclaim())
        .flat_map(|p| p.targets.iter().map(move |t| (p, t)))
        .collect();
    rows.sort_by(|a, b| b.1.efficiency().total_cmp(&a.1.efficiency()));

    let blocked: Vec<&Project> = projects
        .iter()
        .filter(|p| !p.git_state.is_safe_to_reclaim())
        .collect();

    let structure = palette::structure();
    let dim = palette::dim();
    let emphasis = palette::emphasis();

    if rows.is_empty() {
        println!("\n  Nothing here is eligible to reclaim.");
        render_blocked(&blocked);
        return;
    }

    let units: Vec<_> = rows
        .iter()
        .map(|(_, t)| chart::units(t.reclaimable_bytes(), t.rebuild_seconds, worth_rate))
        .collect();
    let scale = chart::scale_for(&units, LEFT_FIELD.min(RIGHT_FIELD));
    let drawn: Vec<Bars> = units
        .iter()
        .map(|u| chart::bars(*u, scale, LEFT_FIELD))
        .collect();

    let total_bytes: u64 = rows.iter().map(|(_, t)| t.reclaimable_bytes()).sum();
    let total_secs: u64 = rows.iter().map(|(_, t)| t.rebuild_seconds as u64).sum();

    println!();
    println!(
        "  {emphasis}{}{emphasis:#} reclaimable across {} targets · {} to rebuild it all",
        bytes(total_bytes),
        rows.len(),
        duration(total_secs),
    );
    println!();
    println!(
        "  {dim}{:>pad$}space freed {structure}│{structure:#}{dim} time to rebuild{dim:#}",
        "",
        pad = LEFT_FIELD.saturating_sub(11),
    );

    // Everything that leans left, plus a few that don't, so the reader can see
    // where the good deals stop and what lies immediately beyond.
    let cut = chart::break_even_at(&drawn);
    let shown = if show_all {
        rows.len()
    } else {
        match cut {
            Some(at) => (at + ROWS_BELOW_LINE).min(rows.len()),
            None => rows.len(),
        }
    };

    for (i, ((p, t), b)) in rows.iter().zip(&drawn).enumerate().take(shown) {
        if Some(i) == cut {
            println!(
                "  {structure}{}┼{}{structure:#}  {dim}break-even · {:.0} MB per second{dim:#}",
                "─".repeat(LEFT_FIELD),
                "─".repeat(RIGHT_FIELD),
                worth_rate,
            );
        }
        println!(
            "{}",
            bar_row(
                *b,
                &row_label(p, t),
                &size_of(t),
                &duration(t.rebuild_seconds as u64)
            )
        );
        println!("{}", restore_line(t));
    }

    if shown < rows.len() {
        let rest = &rows[shown..];
        let rest_bytes: u64 = rest.iter().map(|(_, t)| t.reclaimable_bytes()).sum();
        let rest_secs: u64 = rest.iter().map(|(_, t)| t.rebuild_seconds as u64).sum();
        println!();
        println!(
            "  {dim}{} more below the line: {} for another {}.  --all to see them.{dim:#}",
            rest.len(),
            bytes(rest_bytes),
            duration(rest_secs),
        );
    }

    // Caveats last, and only when they apply.
    let shared: u64 = projects.iter().map(|p| p.shared_bytes()).sum();
    let unreadable: u32 = projects.iter().map(|p| p.unreadable()).sum();
    if shared > 0 || unreadable > 0 {
        println!();
        if let Some(w) = shared_warning(shared) {
            note(&w);
        }
        if let Some(w) = lower_bound_warning(unreadable) {
            note(&w);
        }
    }

    render_blocked(&blocked);
    println!();
    println!("  {dim}Run with --reclaim to delete. Nothing has been touched.{dim:#}");
    let _ = Reset;
}

/// Work that isn't backed up. Kept out of the chart deliberately — the chart is
/// for decisions you can act on — but never kept quiet, because refusing to
/// touch this is the reason the tool is trusted at all.
fn render_blocked(blocked: &[&Project]) {
    if blocked.is_empty() {
        return;
    }
    let held = palette::held_back();
    let dim = palette::dim();
    let total: u64 = blocked.iter().map(|p| p.reclaimable_bytes()).sum();

    println!();
    println!(
        "  {held}Held back{held:#} — {} in repos with work that isn't backed up",
        bytes(total),
    );
    for p in blocked {
        let name = p
            .root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.root.display().to_string());
        println!(
            "    {:<26} {dim}{:<16} {}{dim:#}",
            chart::truncate_left(&name, 26),
            chart::truncate_left(&p.ecosystem, 16),
            p.git_state.label()
        );
    }
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

    /// "3m 0s" and "1h 0m" read as though the precision meant something.
    #[test]
    fn whole_units_drop_the_trailing_zero() {
        assert_eq!(duration(180), "3m");
        assert_eq!(duration(600), "10m");
        assert_eq!(duration(3600), "1h");
    }

    fn target(bytes: u64, unreadable: u32) -> Target {
        Target {
            path: "node_modules".into(),
            bytes,
            shared_bytes: 0,
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

    /// Enough to measure printable width without pulling in a parser.
    fn visible_width(s: &str) -> usize {
        let mut n = 0;
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                n += 1;
            }
        }
        n
    }

    /// The fixed grid is the promise; this is the assertion that keeps it.
    #[test]
    fn a_bar_row_fits_the_column_budget() {
        let b = Bars {
            left: 9,
            right: 5,
            overflowed: false,
        };
        let w = visible_width(&bar_row(b, "…app/node_modules", "420.8 MB", "45s"));
        assert!(w <= 80, "{w} columns");
    }

    #[test]
    fn the_widest_possible_row_still_fits() {
        let b = Bars {
            left: LEFT_FIELD,
            right: RIGHT_FIELD,
            overflowed: true,
        };
        let label = "x".repeat(NAME_FIELD);
        let w = visible_width(&bar_row(b, &label, "1023.9 GB", "23h 59m"));
        assert!(w <= 80, "{w} columns");
    }

    /// Notes are the only prose in the output, so they are the only thing that
    /// can wrap. Terminal wrapping lands mid-word and ruins the alignment
    /// everything else maintains.
    #[test]
    fn notes_wrap_within_the_budget() {
        let long = shared_warning(785_000_000).unwrap();
        let lines = wrap(&long, 76);
        assert!(lines.len() > 1, "this note is long enough to need wrapping");
        for l in &lines {
            assert!(
                l.chars().count() <= 76,
                "{} chars: {l:?}",
                l.chars().count()
            );
        }
        // Wrapping must not drop or duplicate words.
        assert_eq!(
            lines.join(" ").split_whitespace().collect::<Vec<_>>(),
            long.split_whitespace().collect::<Vec<_>>()
        );
    }

    #[test]
    fn wrapping_handles_a_word_longer_than_the_line() {
        let lines = wrap(&"x".repeat(100), 20);
        assert_eq!(lines.len(), 1, "an unbreakable word is left alone");
    }

    /// Invariant 4: the restore cost is always shown alongside the bytes. An
    /// early draft of this chart dropped the command entirely, which turns a
    /// stated cost back into an unexplained loss.
    #[test]
    fn every_row_states_how_to_get_it_back() {
        let t = target(1024, 0);
        let line = restore_line(&t);
        assert!(line.contains("npm ci"), "{line:?}");
        assert!(visible_width(&line) <= 80, "{line:?}");
    }

    #[test]
    fn a_very_long_restore_command_still_fits() {
        let mut t = target(1024, 0);
        t.restore_command =
            "python -m venv .venv && source .venv/bin/activate && pip install -r requirements-dev.txt"
                .into();
        assert!(visible_width(&restore_line(&t)) <= 80);
    }

    /// Bars of different lengths must not shift the spine, or the chart stops
    /// being readable as a chart.
    #[test]
    fn the_spine_sits_at_the_same_column_on_every_row() {
        let column_of_spine = |b: Bars| {
            let row = bar_row(b, "x", "1 B", "1s");
            let mut col = 0;
            let mut chars = row.chars();
            while let Some(c) = chars.next() {
                if c == '\u{1b}' {
                    for c in chars.by_ref() {
                        if c == 'm' {
                            break;
                        }
                    }
                } else if c == '│' {
                    return col;
                } else {
                    col += 1;
                }
            }
            panic!("no spine in row");
        };

        let a = column_of_spine(Bars {
            left: 0,
            right: RIGHT_FIELD,
            overflowed: false,
        });
        let b = column_of_spine(Bars {
            left: LEFT_FIELD,
            right: 0,
            overflowed: false,
        });
        let c = column_of_spine(Bars {
            left: 7,
            right: 11,
            overflowed: false,
        });
        // An over-long bar must be clamped rather than shove the spine along.
        let d = column_of_spine(Bars {
            left: LEFT_FIELD + 9,
            right: RIGHT_FIELD + 9,
            overflowed: true,
        });
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(c, d);
    }
}
