//! Turning [`App`] into bytes.
//!
//! Reuses the same geometry and palette as the printed report, so the chart
//! means the same thing in both places. The one difference: the CLI is pinned
//! to 79 columns because that is what a README code block renders at, while
//! here the real terminal width is known and the bars expand to fill it.

use super::app::{App, Mode, Row};
use crate::chart::{self, Bars};
use crate::palette;
use crate::report;
use anyhow::Result;
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{Clear, ClearType};
use crossterm::{cursor, queue};
use std::io::{stdout, Write};

/// Title, roots, blank, column legend.
const HEADER_ROWS: u16 = 4;
/// Blank, summary, keys.
const FOOTER_ROWS: u16 = 3;

/// How many list rows fit in a window of this height.
pub fn viewport_rows(h: u16) -> usize {
    h.saturating_sub(HEADER_ROWS + FOOTER_ROWS).max(1) as usize
}

/// Which visible row a screen line corresponds to, for mouse clicks.
pub fn row_at(app: &App, screen_row: u16) -> Option<usize> {
    let first = HEADER_ROWS;
    if screen_row < first {
        return None;
    }
    let idx = app.offset + (screen_row - first) as usize;
    (idx < app.visible_len()).then_some(idx)
}

/// Bar field width for a given terminal width, mirroring the CLI's proportions
/// but using whatever room there is. Clamped so the labels never vanish.
fn field_for(width: u16) -> usize {
    let usable = width.saturating_sub(46) as usize;
    (usable / 2).clamp(6, 30)
}

fn name_width(width: u16) -> usize {
    let field = field_for(width);
    (width as usize)
        .saturating_sub(2 + field * 2 + 1 + 1 + 1 + 10 + 1 + 8)
        .clamp(12, 48)
}

pub fn render(app: &App, w: u16, h: u16) -> Result<()> {
    let mut out = stdout();
    render_to(&mut out, app, w, h)
}

/// Draw one frame into any sink.
///
/// Split out from [`render`] so a frame can be produced without a terminal —
/// which is the only way the layout gets asserted on rather than eyeballed.
pub fn render_to(out: &mut impl Write, app: &App, w: u16, h: u16) -> Result<()> {
    queue!(out, Clear(ClearType::All), cursor::MoveTo(0, 0))?;

    header(out, app, w)?;
    match &app.mode {
        Mode::Help(_) => help(out, w, h)?,
        Mode::Confirming => confirm(out, app, w, h)?,
        _ => list(out, app, w, h)?,
    }
    footer(out, app, w, h)?;

    out.flush()?;
    Ok(())
}

fn header(out: &mut impl Write, app: &App, w: u16) -> Result<()> {
    let emphasis = palette::emphasis();
    let dim = palette::dim();
    let structure = palette::structure();

    // Eligible only. Counting git-blocked work into a "reclaimable" headline
    // would promise space the tool will refuse to touch.
    let total = app.eligible_bytes();
    let secs = app.eligible_seconds();
    let blocked = app.blocked_bytes();

    let status = match &app.mode {
        Mode::Idle => "press s to scan".to_string(),
        Mode::Scanning => format!(
            "scanning — {} dirs · {} files",
            app.progress.dirs, app.progress.files
        ),
        Mode::Reclaiming => "reclaiming…".to_string(),
        Mode::Done(msg) => msg.clone(),
        _ => {
            let mut s = format!(
                "{} reclaimable · {} to rebuild",
                report::bytes(total),
                report::duration(secs)
            );
            if blocked > 0 {
                s.push_str(&format!(" · {} held back", report::bytes(blocked)));
            }
            s
        }
    };

    queue!(
        out,
        cursor::MoveTo(0, 0),
        Print(format!(
            "  {emphasis}ForceFree{emphasis:#}  {dim}{}{dim:#}",
            truncate(&status, w.saturating_sub(14) as usize)
        )),
        cursor::MoveTo(0, 1),
        Print(format!(
            "  {dim}{}{dim:#}",
            truncate(&roots_line(app), w.saturating_sub(4) as usize)
        )),
        cursor::MoveTo(0, 3),
    )?;

    let field = field_for(w);
    queue!(
        out,
        Print(format!(
            "  {dim}{:>pad$}space freed {structure}│{structure:#}{dim} time to rebuild{dim:#}",
            "",
            pad = field.saturating_sub(11)
        ))
    )?;
    Ok(())
}

fn roots_line(app: &App) -> String {
    app.roots
        .iter()
        .map(|r| report::display_path(r))
        .collect::<Vec<_>>()
        .join("  ")
}

/// A drawable line: either a row, or the break-even rule that sits between
/// them. Modelled explicitly because an earlier version drew the rule *instead
/// of* the row at the cut index, which silently swallowed a reclaimable row.
enum Item<'a> {
    Rule,
    Row(&'a Row, Bars, usize),
}

fn list(out: &mut impl Write, app: &App, w: u16, h: u16) -> Result<()> {
    let field = field_for(w);
    let bars = app.bars(field);
    let cut = chart::break_even_at(&bars);
    let rows: Vec<&Row> = app.visible().collect();

    let mut items: Vec<Item> = Vec::with_capacity(rows.len() + 1);
    let mut first_item_of_offset = 0usize;
    for (i, row) in rows.iter().enumerate() {
        if Some(i) == cut {
            items.push(Item::Rule);
        }
        if i == app.offset {
            // Scroll in row space, so the rule never steals the top line.
            first_item_of_offset = items.len();
        }
        items.push(Item::Row(row, bars[i], i));
    }

    let vp = viewport_rows(h);
    let structure = palette::structure();
    let dim = palette::dim();

    for (screen, item) in items.iter().skip(first_item_of_offset).take(vp).enumerate() {
        queue!(out, cursor::MoveTo(0, HEADER_ROWS + screen as u16))?;
        match item {
            Item::Rule => queue!(
                out,
                Print(format!(
                    "  {structure}{}┼{}{structure:#} {dim}break-even · {:.0} MB/s{dim:#}",
                    "─".repeat(field),
                    "─".repeat(field),
                    app.worth_rate
                ))
            )?,
            Item::Row(row, b, i) => queue!(out, Print(row_line(app, row, *b, *i, w)))?,
        }
    }
    Ok(())
}

fn row_line(app: &App, row: &Row, bars: Bars, index: usize, w: u16) -> String {
    let field = field_for(w);
    let spine = palette::structure();
    let dim = palette::dim();

    let (space, cost) = if row.project.git_state.is_safe_to_reclaim() {
        (palette::space_freed(), palette::time_cost())
    } else {
        // Untouchable work is drawn in one colour so it reads as a block
        // rather than as a decision.
        (palette::held_back(), palette::held_back())
    };

    let left = bars.left.min(field);
    let right = bars.right.min(field);

    let marker = if row.selected {
        "▸"
    } else if index == app.cursor {
        "·"
    } else {
        " "
    };
    let check = if row.selected { "[x]" } else { "[ ]" };

    let label = chart::truncate_left(&label_for(row), name_width(w));
    let size = match row.measured() {
        Some(_) => report::bytes(row.reclaimable_bytes()),
        None => "measuring…".to_string(),
    };
    let time = match row.measured() {
        Some(_) => report::duration(row.rebuild_seconds() as u64),
        None => String::new(),
    };

    let body = format!(
        "{marker}{check} {pad}{space}{bl}{space:#}{spine}│{spine:#}{cost}{br}{cost:#}{tail} \
         {label:<nw$} {size:>10} {time:>8}",
        pad = " ".repeat(field - left),
        bl = "█".repeat(left),
        br = "█".repeat(right),
        tail = " ".repeat(field - right),
        nw = name_width(w),
    );

    if index == app.cursor {
        // Reverse video for the cursor rather than a colour, so it survives
        // NO_COLOR and does not collide with the palette.
        format!("{}{body}{}", Attribute::Reverse, Attribute::NoReverse)
    } else if row.measured().is_none() {
        format!("{dim}{body}{dim:#}")
    } else {
        body
    }
}

fn label_for(row: &Row) -> String {
    let project = row
        .project
        .root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match row.measured() {
        Some(t) => {
            let target = t
                .path
                .strip_prefix(&row.project.root)
                .unwrap_or(&t.path)
                .display()
                .to_string();
            format!("{project}/{target}")
        }
        None => format!("{project} [{}]", row.project.ecosystem),
    }
}

fn confirm(out: &mut impl Write, app: &App, w: u16, h: u16) -> Result<()> {
    let held = palette::held_back();
    let emphasis = palette::emphasis();
    let dim = palette::dim();
    let mut y = HEADER_ROWS;

    queue!(
        out,
        cursor::MoveTo(0, y),
        Print(format!(
            "  {held}About to delete{held:#} {emphasis}{}{emphasis:#} across {} target(s).",
            report::bytes(app.selected_bytes()),
            app.selected_rows().len()
        ))
    )?;
    y += 1;
    queue!(
        out,
        cursor::MoveTo(0, y),
        Print(format!(
            "  {dim}Restoring all of it would cost about {}.{dim:#}",
            report::duration(app.selected_seconds())
        ))
    )?;
    y += 2;

    // Itemised, always. A confirmation over a figure nobody has seen broken
    // down is not informed consent.
    let room = h.saturating_sub(y + FOOTER_ROWS + 1) as usize;
    let selected = app.selected_rows();
    for row in selected.iter().take(room) {
        let path = row
            .measured()
            .map(|t| report::display_path(&t.path))
            .unwrap_or_default();
        queue!(
            out,
            cursor::MoveTo(0, y),
            Print(format!(
                "    {}",
                truncate(&path, w.saturating_sub(6) as usize)
            ))
        )?;
        y += 1;
    }
    if selected.len() > room {
        queue!(
            out,
            cursor::MoveTo(0, y),
            Print(format!(
                "    {dim}… and {} more{dim:#}",
                selected.len() - room
            ))
        )?;
    }
    Ok(())
}

fn help(out: &mut impl Write, w: u16, _h: u16) -> Result<()> {
    let emphasis = palette::emphasis();
    let dim = palette::dim();
    let keys: [(&str, &str); 10] = [
        ("↑ ↓  j k", "move"),
        ("g  G", "top / bottom"),
        ("PgUp PgDn", "page"),
        ("space", "select or deselect"),
        ("a", "select everything above the line"),
        ("n", "clear the selection"),
        ("t", "show or hide rows below the line"),
        ("s", "scan again"),
        ("r  Enter", "reclaim what is selected"),
        ("q  Esc", "quit"),
    ];
    for (i, (k, what)) in keys.iter().enumerate() {
        queue!(
            out,
            cursor::MoveTo(0, HEADER_ROWS + i as u16),
            Print(format!(
                "    {emphasis}{k:<12}{emphasis:#} {dim}{}{dim:#}",
                truncate(what, w.saturating_sub(20) as usize)
            ))
        )?;
    }
    Ok(())
}

fn footer(out: &mut impl Write, app: &App, w: u16, h: u16) -> Result<()> {
    let dim = palette::dim();
    let emphasis = palette::emphasis();
    let y = h.saturating_sub(2);

    let hint = match app.mode {
        Mode::Confirming => "y  delete    any other key  cancel",
        Mode::Reclaiming => "working…",
        Mode::Help(_) => "any key  back",
        _ => {
            "space select   a all above line   r reclaim   t show all   s rescan   ? help   q quit"
        }
    };

    let left = if matches!(
        app.mode,
        Mode::Confirming | Mode::Reclaiming | Mode::Help(_)
    ) {
        String::new()
    } else {
        format!(
            "{emphasis}{}{emphasis:#} selected · {} ",
            report::bytes(app.selected_bytes()),
            report::duration(app.selected_seconds())
        )
    };

    queue!(
        out,
        cursor::MoveTo(0, y),
        SetAttribute(Attribute::Reset),
        Print(format!("  {left}")),
        cursor::MoveTo(0, y + 1),
        Print(format!(
            "  {dim}{}{dim:#}",
            truncate(hint, w.saturating_sub(4) as usize)
        ))
    )?;
    Ok(())
}

/// Plain right-truncation for text that is not a path.
fn truncate(s: &str, width: usize) -> String {
    chart::truncate_right(s, width)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout must degrade rather than produce negative widths, which
    /// would panic inside `repeat`.
    #[test]
    fn field_widths_stay_sane_at_any_terminal_size() {
        for w in [1u16, 20, 40, 60, 80, 120, 400] {
            let f = field_for(w);
            assert!((6..=30).contains(&f), "width {w} gave field {f}");
            assert!(name_width(w) >= 12);
        }
    }

    #[test]
    fn viewport_is_never_zero() {
        for h in [0u16, 1, 5, 24, 100] {
            assert!(viewport_rows(h) >= 1, "height {h}");
        }
    }

    use crate::git::GitState;
    use crate::scan::{Event, Project, Target};
    use std::path::PathBuf;

    fn populated() -> App {
        let mut a = App::new(vec![PathBuf::from("/dev")], chart::DEFAULT_WORTH_RATE);
        a.mode = super::Mode::Scanning;
        for (root, path, mb, secs, state) in [
            (
                "/dev/dashboard",
                "node_modules",
                3800u64,
                45u32,
                GitState::Clean,
            ),
            ("/dev/parser", "target/debug", 6700, 180, GitState::Clean),
            ("/dev/mobile", ".gradle", 300, 300, GitState::Clean),
            ("/dev/thesis", ".venv", 2100, 60, GitState::Dirty),
        ] {
            a.on_scan_event(Event::Project(Project {
                root: PathBuf::from(root),
                ecosystem: "Node.js".into(),
                git_state: state,
                targets: vec![Target {
                    path: PathBuf::from(root).join(path),
                    bytes: mb * 1_048_576,
                    shared_bytes: 0,
                    unreadable: 0,
                    restore_command: "npm ci".into(),
                    rebuild_seconds: secs,
                }],
            }));
        }
        a.show_below = true;
        a.scan_finished();
        a
    }

    fn frame(app: &App, w: u16, h: u16) -> Vec<String> {
        let mut buf: Vec<u8> = Vec::new();
        render_to(&mut buf, app, w, h).unwrap();
        String::from_utf8(buf)
            .unwrap()
            .split('\u{1b}')
            // Every crossterm sequence begins with ESC; dropping through the
            // terminating letter leaves just the printable text of each chunk.
            .map(
                |chunk| match chunk.find(|c: char| c.is_ascii_alphabetic()) {
                    Some(i) => chunk[i + 1..].to_string(),
                    None => chunk.to_string(),
                },
            )
            .filter(|s| !s.trim().is_empty())
            .collect()
    }

    /// The chart must not wrap. A wrapped row in a fixed grid looks like a bug
    /// and destroys the alignment the whole design depends on.
    #[test]
    fn no_rendered_chunk_exceeds_the_terminal_width() {
        let app = populated();
        for (w, h) in [(80u16, 24u16), (60, 20), (120, 40), (200, 50)] {
            for line in frame(&app, w, h) {
                assert!(
                    line.chars().count() <= w as usize,
                    "{} chars at width {w}: {line:?}",
                    line.chars().count()
                );
            }
        }
    }

    /// Rendering must survive absurd geometry rather than panicking inside
    /// `repeat` or a subtraction.
    #[test]
    fn rendering_survives_a_tiny_window() {
        let app = populated();
        for (w, h) in [(1u16, 1u16), (10, 3), (20, 5), (40, 8)] {
            let mut buf: Vec<u8> = Vec::new();
            render_to(&mut buf, &app, w, h).expect("no panic and no error");
        }
    }

    #[test]
    fn every_mode_renders() {
        let mut app = populated();
        app.select_above_line();
        for mode in [
            super::Mode::Idle,
            super::Mode::Scanning,
            super::Mode::Browsing,
            super::Mode::Confirming,
            super::Mode::Reclaiming,
            super::Mode::Done("freed 1 GB".into()),
            super::Mode::Help(Box::new(super::Mode::Browsing)),
        ] {
            app.mode = mode.clone();
            let mut buf: Vec<u8> = Vec::new();
            render_to(&mut buf, &app, 80, 24).unwrap_or_else(|e| panic!("{mode:?}: {e}"));
        }
    }

    /// Every row must be drawn. The rule sits *between* rows; an earlier
    /// version drew it in place of the row at the cut index, which quietly
    /// removed a reclaimable directory from the list.
    #[test]
    fn the_break_even_rule_does_not_swallow_a_row() {
        let app = populated();
        let text = frame(&app, 100, 40).join("\n");
        for name in ["node_modules", "target/debug", ".gradle", ".venv"] {
            assert!(text.contains(name), "{name} missing from the list:\n{text}");
        }
        assert!(text.contains("break-even"));
    }

    /// Development aid: prints a frame so the layout can be looked at without
    /// a terminal. Ignored by default.
    ///
    /// `cargo test dump_a_frame -- --ignored --nocapture`
    #[test]
    #[ignore = "prints a frame for inspection; not an assertion"]
    fn dump_a_frame() {
        let mut app = populated();
        app.select_above_line();
        for line in frame(&app, 90, 22) {
            println!("{line}");
        }
    }

    /// The confirmation screen must name every path it is about to remove.
    #[test]
    fn confirming_lists_the_actual_paths() {
        let mut app = populated();
        app.select_above_line();
        app.mode = super::Mode::Confirming;
        let text = frame(&app, 100, 30).join("\n");
        assert!(text.contains("node_modules"), "{text}");
        assert!(text.contains("About to delete"), "{text}");
    }
}
