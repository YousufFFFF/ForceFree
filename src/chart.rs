//! Geometry for the diverging balance chart.
//!
//! Each row is drawn against a centre spine: space freed grows left, time to
//! rebuild grows right. The point is that you read the *lean* rather than the
//! numbers — heavy on the left is a bargain, heavy on the right is a trap.
//!
//! # Why symmetry means something
//!
//! Both sides are square-rooted, and the right side is multiplied by a reference
//! rate before rooting:
//!
//! ```text
//! left  = sqrt(reclaimable_mb)
//! right = sqrt(rebuild_seconds * rate)
//! ```
//!
//! Those are equal exactly when `reclaimable_mb == rebuild_seconds * rate` — when
//! the row returns precisely the reference rate. So the break-even point is not a
//! knee anyone has to compute; it is simply where rows stop leaning left.
//!
//! # Why scaling does not break it
//!
//! One `cols_per_unit` factor is derived per run so the longest bar fills its
//! field, and *both* sides share it. Multiplying both sides by the same number
//! cannot change which is longer, so the chart uses the full width while the
//! meaning of symmetry stays fixed across runs. This is the property the whole
//! design rests on, and `scaling_preserves_the_balance_point` guards it.
//!
//! Pure arithmetic, no I/O — so it is testable without a terminal.

/// Bytes returned per second of rebuild at which a row balances. Calibrated
/// against real projects: a `node_modules` worth 420 MB for 45s of `npm ci`
/// (9.4 MB/s) is clearly worth doing, a `target/release` worth 230 MB for ten
/// minutes (0.4 MB/s) clearly is not, and 3 MB/s sits between them.
pub const DEFAULT_WORTH_RATE: f64 = 3.0;

const BYTES_PER_MB: f64 = 1_048_576.0;

/// One row's bars, in terminal cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bars {
    pub left: usize,
    pub right: usize,
    /// Either side wanted more cells than the field allows.
    pub overflowed: bool,
}

impl Bars {
    /// Does this row return more than the reference rate? Compared in cells, so
    /// what the caller sees is what the reader sees — a row that rounds to equal
    /// bars is not claimed to lean either way.
    pub fn leans_left(&self) -> bool {
        self.left > self.right
    }
}

/// Unscaled magnitudes for one row. Kept separate from [`Bars`] because the
/// scale factor cannot be known until every row has been measured.
#[derive(Debug, Clone, Copy)]
pub struct Units {
    pub left: f64,
    pub right: f64,
}

/// Where a row sits relative to break-even, before any scaling.
pub fn units(reclaimable_bytes: u64, rebuild_seconds: u32, rate: f64) -> Units {
    let mb = reclaimable_bytes as f64 / BYTES_PER_MB;
    Units {
        left: mb.max(0.0).sqrt(),
        right: (rebuild_seconds as f64 * rate.max(f64::MIN_POSITIVE))
            .max(0.0)
            .sqrt(),
    }
}

/// Cells per unit such that the largest magnitude in `rows` just fills `field`.
///
/// Shared by both sides deliberately; see the module docs.
pub fn scale_for(rows: &[Units], field: usize) -> f64 {
    let widest = rows
        .iter()
        .flat_map(|u| [u.left, u.right])
        .fold(0.0f64, f64::max);
    if widest <= 0.0 || field == 0 {
        return 0.0;
    }
    field as f64 / widest
}

/// Convert one row's magnitudes into drawable cell counts.
///
/// A non-zero quantity always gets at least one cell: rendering a real directory
/// as nothing at all would be a lie of a different kind.
pub fn bars(u: Units, cols_per_unit: f64, field: usize) -> Bars {
    let cells = |magnitude: f64| -> (usize, bool) {
        if magnitude <= 0.0 {
            return (0, false);
        }
        let want = (magnitude * cols_per_unit).round() as usize;
        let want = want.max(1);
        (want.min(field), want > field)
    };
    let (left, left_over) = cells(u.left);
    let (right, right_over) = cells(u.right);
    Bars {
        left,
        right,
        overflowed: left_over || right_over,
    }
}

/// Index of the first row that does not lean left, i.e. where the break-even
/// rule is drawn. `None` when every row leans left.
///
/// Assumes `rows` is already ordered best-first, which is how the report ranks.
pub fn break_even_at(rows: &[Bars]) -> Option<usize> {
    rows.iter().position(|b| !b.leans_left())
}

/// Truncate keeping the *head* — for commands, where `python -m venv .venv &&…`
/// tells you what will run and `…-r requirements.txt` does not.
pub fn truncate_right(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count <= width {
        return s.to_string();
    }
    if width <= 1 {
        return "…".repeat(width);
    }
    let mut out: String = s.chars().take(width - 1).collect();
    out.push('…');
    out
}

/// Truncate a path for display, keeping the tail — `…app/node_modules` says more
/// than `Dump/Legacy Furni…`.
pub fn truncate_left(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count <= width {
        return s.to_string();
    }
    if width <= 1 {
        return "…".repeat(width);
    }
    let skip = count - (width - 1);
    let mut out = String::with_capacity(width);
    out.push('…');
    out.extend(s.chars().skip(skip));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1_048_576;
    const FIELD: usize = 18;

    fn row(mb: u64, secs: u32) -> Units {
        units(mb * MB, secs, DEFAULT_WORTH_RATE)
    }

    fn drawn(rows: &[Units]) -> Vec<Bars> {
        let scale = scale_for(rows, FIELD);
        rows.iter().map(|u| bars(*u, scale, FIELD)).collect()
    }

    /// A row returning exactly the reference rate must balance. This is the
    /// claim the whole chart makes, so it is the first thing to hold.
    #[test]
    fn break_even_row_is_symmetric() {
        // 3 MB/s for 100 s = 300 MB.
        let u = row(300, 100);
        assert!((u.left - u.right).abs() < 1e-9, "{u:?}");

        let b = bars(u, scale_for(&[u], FIELD), FIELD);
        assert_eq!(b.left, b.right);
        assert!(!b.leans_left(), "exact break-even is not a win");
    }

    #[test]
    fn better_than_reference_leans_left() {
        // 420 MB for 45s is 9.3 MB/s, comfortably above the 3 MB/s reference.
        let b = drawn(&[row(420, 45)]);
        assert!(b[0].leans_left(), "{:?}", b[0]);
    }

    #[test]
    fn worse_than_reference_leans_right() {
        // 230 MB for ten minutes is 0.38 MB/s.
        let b = drawn(&[row(230, 600)]);
        assert!(!b[0].leans_left(), "{:?}", b[0]);
        assert!(b[0].right > b[0].left);
    }

    /// Bar lengths may change with the scale; which way a row leans may not.
    /// Without this the chart would mean different things in different runs.
    #[test]
    fn scaling_preserves_the_balance_point() {
        let rows = [row(420, 45), row(300, 100), row(230, 600), row(20, 300)];
        let tight = scale_for(&rows, 8);
        let wide = scale_for(&rows, 40);

        for u in rows {
            let a = bars(u, tight, 8);
            let b = bars(u, wide, 40);
            assert_eq!(
                a.leans_left(),
                b.leans_left(),
                "lean changed with field width for {u:?}"
            );
        }
    }

    #[test]
    fn oversized_bars_clamp_and_flag_overflow() {
        let u = row(400, 10);
        // A scale far too large for the field.
        let b = bars(u, 100.0, FIELD);
        assert_eq!(b.left, FIELD);
        assert!(b.overflowed);
    }

    #[test]
    fn a_nonzero_target_never_renders_as_zero_cells() {
        // 4 KB next to a 4 GB neighbour still has to be visible.
        let rows = [units(4096, 1, DEFAULT_WORTH_RATE), row(4096, 60)];
        let scale = scale_for(&rows, FIELD);
        let small = bars(rows[0], scale, FIELD);
        assert!(small.left >= 1, "{small:?}");
    }

    #[test]
    fn a_genuinely_empty_side_stays_empty() {
        // Nothing to reclaim is different from a little to reclaim.
        let b = bars(units(0, 60, DEFAULT_WORTH_RATE), 1.0, FIELD);
        assert_eq!(b.left, 0);
    }

    #[test]
    fn break_even_is_the_first_row_that_stops_leaning_left() {
        let rows = [row(420, 45), row(439, 60), row(155, 60), row(230, 600)];
        let b = drawn(&rows);
        let at = break_even_at(&b).expect("some row must fall below");
        assert!(b[at - 1].leans_left());
        assert!(!b[at].leans_left());
    }

    #[test]
    fn break_even_is_absent_when_everything_is_worth_doing() {
        let b = drawn(&[row(4096, 45), row(2048, 30)]);
        assert_eq!(break_even_at(&b), None);
    }

    /// Paths and commands truncate in opposite directions, because the
    /// informative end is at opposite ends.
    #[test]
    fn commands_truncate_from_the_right_keeping_the_head() {
        assert_eq!(truncate_right("npm ci", 20), "npm ci");
        assert_eq!(
            truncate_right(
                "python -m venv .venv && pip install -r requirements.txt",
                24
            ),
            "python -m venv .venv &&…"
        );
        assert_eq!(truncate_right("abcdef", 3).chars().count(), 3);
    }

    #[test]
    fn paths_truncate_from_the_left_keeping_the_tail() {
        assert_eq!(truncate_left("node_modules", 22), "node_modules");
        assert_eq!(
            truncate_left("Dump/Legacy Furniture Hub/furniture-app/node_modules", 20),
            "…re-app/node_modules"
        );
        assert_eq!(truncate_left("abcdef", 3).chars().count(), 3);
    }

    /// Multi-byte paths must not panic or blow the column budget.
    #[test]
    fn truncation_counts_characters_not_bytes() {
        let s = "проект/node_modules";
        let out = truncate_left(s, 10);
        assert_eq!(out.chars().count(), 10);
    }

    #[test]
    fn no_row_exceeds_the_column_budget() {
        let rows = [
            row(8192, 5),
            row(420, 45),
            row(300, 100),
            row(1, 3600),
            units(1, 1, DEFAULT_WORTH_RATE),
        ];
        let scale = scale_for(&rows, FIELD);
        for u in rows {
            let b = bars(u, scale, FIELD);
            assert!(b.left <= FIELD && b.right <= FIELD, "{b:?}");
        }
    }
}
