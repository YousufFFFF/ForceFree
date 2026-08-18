//! Everything the interface decides, and nothing it draws.
//!
//! `App` holds the rows, the selection, the scroll position and the mode, and
//! `on_key` is the only way any of it changes. No terminal, no I/O, no
//! rendering — which means the whole interaction model is exercised by ordinary
//! unit tests rather than by a human squinting at a screen.
//!
//! The safety rules are not re-implemented here. Selection refuses git-blocked
//! rows, and reclaiming hands a `Vec<Doomed>` straight to `reclaim::execute`,
//! so the containment check and the byte accounting are the same code the CLI
//! uses. An interface that could delete something the CLI would refuse to would
//! be a bug of the worst kind.

use crate::chart::{self, Bars};
use crate::reclaim::{Doomed, Outcome};
use crate::report;
use crate::scan::{Event, Progress, Project, Target};
use std::path::PathBuf;

/// Nominal bar field used for decisions that must not shift when the window
/// resizes — chiefly where the break-even line falls.
const CUT_FIELD: usize = 16;

/// What the interface is doing right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Nothing scanned yet.
    Idle,
    Scanning,
    Browsing,
    /// Showing exactly what is about to be deleted, waiting for a yes.
    Confirming,
    /// Deleting. Deliberately not interruptible: stopping halfway leaves a
    /// half-removed directory, which is worse than finishing.
    Reclaiming,
    Done(String),
    /// Key reference. Remembers where to go back to.
    Help(Box<Mode>),
}

/// One line in the list.
///
/// A project contributes one row per reclaimable target, because the decision
/// is per directory — you might want a project's `node_modules` gone and its
/// `target/release` kept.
#[derive(Debug, Clone)]
pub struct Row {
    pub project: Project,
    /// Index into `project.targets`, or `None` while the project has been
    /// recognised but not yet measured.
    pub target: Option<usize>,
    pub selected: bool,
}

impl Row {
    pub fn measured(&self) -> Option<&Target> {
        self.target.and_then(|i| self.project.targets.get(i))
    }

    /// Git-blocked work is visible but untouchable, exactly as in the report.
    /// Unmeasured rows are refused too — selecting something with no known size
    /// would let the confirmation screen promise a number it does not have.
    pub fn selectable(&self) -> bool {
        self.project.git_state.is_safe_to_reclaim() && self.measured().is_some()
    }

    pub fn reclaimable_bytes(&self) -> u64 {
        self.measured().map_or(0, |t| t.reclaimable_bytes())
    }

    pub fn rebuild_seconds(&self) -> u32 {
        self.measured().map_or(0, |t| t.rebuild_seconds)
    }

    pub fn efficiency(&self) -> f64 {
        self.measured().map_or(0.0, |t| t.efficiency())
    }
}

pub struct App {
    pub mode: Mode,
    pub rows: Vec<Row>,
    pub cursor: usize,
    /// First visible row. Kept in range by `scroll_into_view`.
    pub offset: usize,
    pub progress: Progress,
    pub roots: Vec<PathBuf>,
    pub worth_rate: f64,
    /// Show rows that fall below break-even. Off by default, as in the CLI.
    pub show_below: bool,
    pub quit: bool,
    /// Rows visible at once. The event loop keeps this in step with the window.
    pub viewport: usize,
}

impl App {
    pub fn new(roots: Vec<PathBuf>, worth_rate: f64) -> Self {
        Self {
            mode: Mode::Idle,
            rows: Vec::new(),
            cursor: 0,
            offset: 0,
            progress: Progress::default(),
            roots,
            worth_rate,
            show_below: false,
            quit: false,
            viewport: 20,
        }
    }

    // ---- scan events -------------------------------------------------------

    pub fn on_scan_event(&mut self, event: Event) {
        match event {
            // Recognised but not measured. It goes on screen immediately with
            // no size, which is the whole reason sizing was made streaming:
            // structure appears in the first second, numbers follow.
            Event::Found { root, ecosystem } => {
                if !self.rows.iter().any(|r| r.project.root == root) {
                    self.rows.push(Row {
                        project: Project {
                            root,
                            ecosystem,
                            git_state: crate::git::GitState::Unknown,
                            targets: Vec::new(),
                        },
                        target: None,
                        selected: false,
                    });
                }
            }
            Event::Project(p) => self.absorb(p),
            Event::Progress(p) => self.progress = p,
        }
    }

    /// Replace a project's placeholder with one row per measured target,
    /// preserving any selection the user already made by path.
    fn absorb(&mut self, p: Project) {
        let keep = self.selected_paths();
        self.rows.retain(|r| r.project.root != p.root);
        for i in 0..p.targets.len() {
            let selected = p.targets.get(i).is_some_and(|t| keep.contains(&t.path));
            self.rows.push(Row {
                project: p.clone(),
                target: Some(i),
                selected,
            });
        }
        self.rank();
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        self.rows
            .iter()
            .filter(|r| r.selected)
            .filter_map(|r| r.measured().map(|t| t.path.clone()))
            .collect()
    }

    /// Best deal first.
    ///
    /// Two groups sink below the ranking rather than mixing into it:
    /// not-yet-measured rows, so the list does not thrash mid-scan, and
    /// git-blocked rows, which are not decisions at all. Keeping blocked work
    /// out of the ranking is what makes the break-even line mean the same
    /// thing here as it does in the printed report, which computes it over
    /// eligible rows only.
    fn rank(&mut self) {
        self.rows.sort_by(|a, b| {
            b.measured()
                .is_some()
                .cmp(&a.measured().is_some())
                .then(
                    b.project
                        .git_state
                        .is_safe_to_reclaim()
                        .cmp(&a.project.git_state.is_safe_to_reclaim()),
                )
                .then(b.efficiency().total_cmp(&a.efficiency()))
        });
        self.clamp_cursor();
    }

    /// Rows that could actually be reclaimed. The header totals and the
    /// break-even line are both computed over these, never over the whole list.
    fn eligible(&self) -> impl Iterator<Item = &Row> {
        self.rows.iter().filter(|r| r.selectable())
    }

    pub fn eligible_bytes(&self) -> u64 {
        self.eligible().map(|r| r.reclaimable_bytes()).sum()
    }

    pub fn eligible_seconds(&self) -> u64 {
        self.eligible().map(|r| r.rebuild_seconds() as u64).sum()
    }

    pub fn blocked_bytes(&self) -> u64 {
        self.rows
            .iter()
            .filter(|r| !r.project.git_state.is_safe_to_reclaim())
            .map(|r| r.reclaimable_bytes())
            .sum()
    }

    pub fn scan_finished(&mut self) {
        if self.mode == Mode::Scanning {
            self.mode = Mode::Browsing;
        }
        self.rank();
    }

    // ---- the break-even line ----------------------------------------------

    /// Bar geometry for the visible rows, sharing one scale so the balance
    /// point means the same thing on every line. See [`crate::chart`].
    pub fn bars(&self, field: usize) -> Vec<Bars> {
        let units: Vec<_> = self
            .visible()
            .map(|r| chart::units(r.reclaimable_bytes(), r.rebuild_seconds(), self.worth_rate))
            .collect();
        let scale = chart::scale_for(&units, field);
        units
            .iter()
            .map(|u| chart::bars(*u, scale, field))
            .collect()
    }

    /// Rows the user can currently see: everything, or only what is worth
    /// reclaiming — plus anything already selected, which must never vanish
    /// from under the cursor.
    pub fn visible(&self) -> impl Iterator<Item = &Row> {
        let cut = self.cut_index();
        let show_below = self.show_below;
        self.rows
            .iter()
            .enumerate()
            .filter(move |(i, r)| show_below || *i < cut || r.selected)
            .map(|(_, r)| r)
    }

    pub fn visible_len(&self) -> usize {
        self.visible().count()
    }

    /// Where break-even falls in the full list. Computed at a fixed nominal
    /// width so resizing the window cannot silently change which rows are
    /// considered worth reclaiming.
    /// Computed over eligible rows only, matching `report::ranked`. A row that
    /// cannot be reclaimed is not a deal, and letting one occupy a place above
    /// the line would push a real one below it.
    pub fn cut_index(&self) -> usize {
        let units: Vec<_> = self
            .eligible()
            .map(|r| chart::units(r.reclaimable_bytes(), r.rebuild_seconds(), self.worth_rate))
            .collect();
        let scale = chart::scale_for(&units, CUT_FIELD);
        let drawn: Vec<_> = units
            .iter()
            .map(|u| chart::bars(*u, scale, CUT_FIELD))
            .collect();
        // `rank` keeps eligible rows first, so an index into them is also an
        // index into `rows`.
        chart::break_even_at(&drawn).unwrap_or(units.len())
    }

    // ---- movement ----------------------------------------------------------

    fn clamp_cursor(&mut self) {
        let n = self.visible_len();
        if n == 0 {
            self.cursor = 0;
            self.offset = 0;
            return;
        }
        self.cursor = self.cursor.min(n - 1);
        self.scroll_into_view();
    }

    fn scroll_into_view(&mut self) {
        let vp = self.viewport.max(1);
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + vp {
            self.offset = self.cursor + 1 - vp;
        }
    }

    pub fn move_by(&mut self, delta: isize) {
        let n = self.visible_len();
        if n == 0 {
            return;
        }
        let next = (self.cursor as isize + delta).clamp(0, n as isize - 1);
        self.cursor = next as usize;
        self.scroll_into_view();
    }

    pub fn move_to(&mut self, index: usize) {
        let n = self.visible_len();
        if n == 0 {
            return;
        }
        self.cursor = index.min(n - 1);
        self.scroll_into_view();
    }

    // ---- selection ---------------------------------------------------------

    /// Toggle the row under the cursor. Refuses git-blocked and unmeasured
    /// rows: the interface must not be able to queue something the deletion
    /// path would reject.
    pub fn toggle(&mut self) {
        let Some(idx) = self.cursor_row_index() else {
            return;
        };
        if !self.rows[idx].selectable() {
            return;
        }
        self.rows[idx].selected = !self.rows[idx].selected;
    }

    /// Everything above break-even that is safe to touch — the same set the
    /// chart recommends and the same set `--reclaim` would take.
    pub fn select_above_line(&mut self) {
        let cut = self.cut_index();
        for (i, r) in self.rows.iter_mut().enumerate() {
            if i < cut && r.selectable() {
                r.selected = true;
            }
        }
    }

    pub fn select_none(&mut self) {
        for r in &mut self.rows {
            r.selected = false;
        }
    }

    /// Map the visible cursor position back to an index into `rows`.
    fn cursor_row_index(&self) -> Option<usize> {
        let cut = self.cut_index();
        self.rows
            .iter()
            .enumerate()
            .filter(|(i, r)| self.show_below || *i < cut || r.selected)
            .map(|(i, _)| i)
            .nth(self.cursor)
    }

    pub fn selected_rows(&self) -> Vec<&Row> {
        self.rows.iter().filter(|r| r.selected).collect()
    }

    pub fn selected_bytes(&self) -> u64 {
        self.selected_rows()
            .iter()
            .map(|r| r.reclaimable_bytes())
            .sum()
    }

    pub fn selected_seconds(&self) -> u64 {
        self.selected_rows()
            .iter()
            .map(|r| r.rebuild_seconds() as u64)
            .sum()
    }

    /// What the confirmation screen promises and the deletion performs — one
    /// list, built once, so the two cannot disagree.
    pub fn doomed(&self) -> Vec<Doomed<'_>> {
        self.rows
            .iter()
            .filter(|r| r.selected)
            .filter_map(|r| {
                r.measured().map(|t| Doomed {
                    project: &r.project,
                    target: t,
                })
            })
            .collect()
    }

    pub fn finish_reclaim(&mut self, out: Outcome) {
        let mut msg = format!(
            "Freed {} across {} target(s).",
            report::bytes(out.freed),
            out.removed
        );
        if out.failures > 0 {
            msg.push_str(&format!(" {} could not be removed.", out.failures));
        }
        if out.refused > 0 {
            msg.push_str(&format!(
                " {} refused — did not resolve inside their project.",
                out.refused
            ));
        }
        // Drop what is gone; anything that failed or was refused stays on
        // screen, because it is still occupying the disk.
        let removed_ok = out.failures == 0 && out.refused == 0;
        if removed_ok {
            self.rows.retain(|r| !r.selected);
        }
        self.select_none();
        self.clamp_cursor();
        self.mode = Mode::Done(msg);
    }

    // ---- input -------------------------------------------------------------

    /// The single entry point for every key. Returns the action the event loop
    /// has to perform, because starting a scan or deleting files is I/O and
    /// this module does none.
    pub fn on_key(&mut self, key: Key) -> Action {
        // Help is modal and swallows everything except the way out.
        if let Mode::Help(back) = &self.mode {
            let back = (**back).clone();
            if matches!(key, Key::Quit | Key::Esc | Key::Help | Key::Enter) {
                self.mode = back;
            }
            return Action::None;
        }

        match self.mode {
            // Deletion in progress ignores input rather than offering a
            // half-way out.
            Mode::Reclaiming => Action::None,

            Mode::Confirming => match key {
                Key::Char('y') | Key::Enter => {
                    self.mode = Mode::Reclaiming;
                    Action::Reclaim
                }
                // Anything else is a no. A destructive prompt should not have
                // to be argued with.
                _ => {
                    self.mode = Mode::Browsing;
                    Action::None
                }
            },

            _ => self.on_key_browsing(key),
        }
    }

    fn on_key_browsing(&mut self, key: Key) -> Action {
        match key {
            Key::Quit | Key::Esc => {
                self.quit = true;
                Action::None
            }
            Key::Help => {
                self.mode = Mode::Help(Box::new(self.mode.clone()));
                Action::None
            }
            Key::Up => {
                self.move_by(-1);
                Action::None
            }
            Key::Down => {
                self.move_by(1);
                Action::None
            }
            Key::PageUp => {
                self.move_by(-(self.viewport as isize));
                Action::None
            }
            Key::PageDown => {
                self.move_by(self.viewport as isize);
                Action::None
            }
            Key::Home => {
                self.move_to(0);
                Action::None
            }
            Key::End => {
                self.move_to(usize::MAX);
                Action::None
            }
            Key::Space => {
                self.toggle();
                Action::None
            }
            Key::Char('a') => {
                self.select_above_line();
                Action::None
            }
            Key::Char('n') => {
                self.select_none();
                Action::None
            }
            Key::Char('t') => {
                self.show_below = !self.show_below;
                self.clamp_cursor();
                Action::None
            }
            Key::Char('s') => {
                if self.mode == Mode::Scanning {
                    Action::None
                } else {
                    self.rows.clear();
                    self.cursor = 0;
                    self.offset = 0;
                    self.progress = Progress::default();
                    self.mode = Mode::Scanning;
                    Action::StartScan
                }
            }
            Key::Char('r') | Key::Enter => {
                if self.doomed().is_empty() {
                    Action::None
                } else {
                    self.mode = Mode::Confirming;
                    Action::None
                }
            }
            Key::Char(_) => Action::None,
        }
    }
}

/// Input, already reduced to what the interface cares about. Keeping crossterm
/// out of this module is what lets the state machine be tested without a
/// terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Space,
    Enter,
    Esc,
    Quit,
    Help,
    Char(char),
}

/// Work the event loop must do on the app's behalf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    StartScan,
    Reclaim,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GitState;
    use std::path::Path;

    const MB: u64 = 1_048_576;

    fn target(path: &str, mb: u64, secs: u32) -> Target {
        Target {
            path: PathBuf::from(path),
            bytes: mb * MB,
            shared_bytes: 0,
            unreadable: 0,
            restore_command: "npm ci".into(),
            rebuild_seconds: secs,
        }
    }

    fn project(root: &str, state: GitState, targets: Vec<Target>) -> Project {
        Project {
            root: PathBuf::from(root),
            ecosystem: "Node.js".into(),
            git_state: state,
            targets,
        }
    }

    /// A good deal, a bad deal, and something git-blocked, in the state the
    /// user would actually see them: a scan that has run to completion.
    fn app() -> App {
        let mut a = App::new(vec![PathBuf::from("/x")], chart::DEFAULT_WORTH_RATE);
        a.mode = Mode::Scanning;
        a.on_scan_event(Event::Project(project(
            "/good",
            GitState::Clean,
            vec![target("/good/node_modules", 900, 45)],
        )));
        a.on_scan_event(Event::Project(project(
            "/bad",
            GitState::Clean,
            vec![target("/bad/target/release", 100, 600)],
        )));
        a.on_scan_event(Event::Project(project(
            "/dirty",
            GitState::Dirty,
            vec![target("/dirty/node_modules", 800, 45)],
        )));
        a.show_below = true;
        a.scan_finished();
        a
    }

    #[test]
    fn movement_stays_in_bounds() {
        let mut empty = App::new(vec![], chart::DEFAULT_WORTH_RATE);
        empty.move_by(-5);
        empty.move_by(5);
        assert_eq!(empty.cursor, 0);

        let mut a = app();
        a.move_by(-10);
        assert_eq!(a.cursor, 0);
        a.move_by(1000);
        assert_eq!(a.cursor, a.visible_len() - 1);
    }

    /// The trust gate has to hold in the interface too. If the UI could select
    /// a dirty repo, the only thing standing between it and deletion would be
    /// a check two modules away.
    #[test]
    fn blocked_rows_cannot_be_selected() {
        let mut a = app();
        let dirty = a
            .visible()
            .position(|r| r.project.root == Path::new("/dirty"))
            .expect("the dirty project is on screen");
        a.move_to(dirty);
        a.toggle();
        assert!(a.selected_rows().is_empty(), "a dirty repo was selected");
    }

    #[test]
    fn unmeasured_rows_cannot_be_selected() {
        let mut a = App::new(vec![], chart::DEFAULT_WORTH_RATE);
        a.on_scan_event(Event::Found {
            root: PathBuf::from("/pending"),
            ecosystem: "Rust".into(),
        });
        a.move_to(0);
        a.toggle();
        assert!(a.selected_rows().is_empty());
    }

    /// The chart, the CLI and the interface must agree on what is worth doing.
    #[test]
    fn select_above_the_line_takes_only_safe_good_deals() {
        let mut a = app();
        a.select_above_line();
        let picked: Vec<_> = a
            .selected_rows()
            .iter()
            .map(|r| r.project.root.clone())
            .collect();
        assert_eq!(picked, vec![PathBuf::from("/good")]);
    }

    /// A project appears the moment it is recognised and is replaced, not
    /// duplicated, once measured.
    #[test]
    fn found_then_project_updates_one_row() {
        let mut a = App::new(vec![], chart::DEFAULT_WORTH_RATE);
        a.on_scan_event(Event::Found {
            root: PathBuf::from("/p"),
            ecosystem: "Node.js".into(),
        });
        assert_eq!(a.rows.len(), 1);
        assert!(a.rows[0].measured().is_none(), "no size yet");

        a.on_scan_event(Event::Project(project(
            "/p",
            GitState::Clean,
            vec![target("/p/node_modules", 500, 45)],
        )));
        assert_eq!(
            a.rows.len(),
            1,
            "the placeholder was replaced, not added to"
        );
        assert!(a.rows[0].measured().is_some());
    }

    /// Duplicate Found events must not stack up rows.
    #[test]
    fn a_repeated_found_does_not_duplicate() {
        let mut a = App::new(vec![], chart::DEFAULT_WORTH_RATE);
        for _ in 0..3 {
            a.on_scan_event(Event::Found {
                root: PathBuf::from("/p"),
                ecosystem: "Node.js".into(),
            });
        }
        assert_eq!(a.rows.len(), 1);
    }

    /// Git-blocked work is not a deal and must not occupy a place in the
    /// ranking, or it pushes a genuinely reclaimable row below the line.
    #[test]
    fn blocked_rows_sink_below_eligible_ones() {
        let a = app();
        let blocked_first = a
            .rows
            .iter()
            .position(|r| !r.project.git_state.is_safe_to_reclaim())
            .unwrap();
        let eligible_last = a
            .rows
            .iter()
            .rposition(|r| r.project.git_state.is_safe_to_reclaim())
            .unwrap();
        assert!(
            blocked_first > eligible_last,
            "a blocked row ranked above an eligible one"
        );
    }

    /// The headline must not promise space the tool will refuse to touch.
    #[test]
    fn totals_count_only_what_can_be_reclaimed() {
        let a = app();
        assert_eq!(a.eligible_bytes(), (900 + 100) * MB);
        assert_eq!(a.blocked_bytes(), 800 * MB);
    }

    /// The break-even line has to fall in the same place as the printed
    /// report's, which computes it over eligible rows only.
    #[test]
    fn the_line_ignores_blocked_rows() {
        let a = app();
        let cut = a.cut_index();
        for r in a.rows.iter().take(cut) {
            assert!(
                r.project.git_state.is_safe_to_reclaim(),
                "a blocked row landed above the break-even line"
            );
        }
    }

    #[test]
    fn measured_rows_rank_above_pending_ones() {
        let mut a = App::new(vec![], chart::DEFAULT_WORTH_RATE);
        a.on_scan_event(Event::Found {
            root: PathBuf::from("/pending"),
            ecosystem: "Rust".into(),
        });
        a.on_scan_event(Event::Project(project(
            "/measured",
            GitState::Clean,
            vec![target("/measured/node_modules", 500, 45)],
        )));
        assert_eq!(a.rows[0].project.root, PathBuf::from("/measured"));
    }

    /// Selection survives a project being re-measured mid-scan.
    #[test]
    fn selection_survives_remeasurement() {
        let mut a = app();
        a.select_above_line();
        assert_eq!(a.selected_rows().len(), 1);
        a.on_scan_event(Event::Project(project(
            "/good",
            GitState::Clean,
            vec![target("/good/node_modules", 950, 45)],
        )));
        assert_eq!(a.selected_rows().len(), 1, "selection was lost on update");
    }

    /// What the confirmation screen lists is what gets deleted.
    #[test]
    fn reclaim_builds_the_plan_from_the_selection() {
        let mut a = app();
        a.select_above_line();
        let doomed = a.doomed();
        assert_eq!(doomed.len(), 1);
        assert_eq!(doomed[0].target.path, PathBuf::from("/good/node_modules"));
        assert_eq!(a.selected_bytes(), 900 * MB);
    }

    #[test]
    fn reclaim_needs_a_selection() {
        let mut a = app();
        assert_eq!(a.on_key(Key::Char('r')), Action::None);
        assert_eq!(a.mode, Mode::Browsing, "must not confirm an empty plan");
    }

    #[test]
    fn confirming_defaults_to_no() {
        let mut a = app();
        a.select_above_line();
        a.on_key(Key::Char('r'));
        assert_eq!(a.mode, Mode::Confirming);
        // Any key that is not an explicit yes cancels.
        assert_eq!(a.on_key(Key::Char('x')), Action::None);
        assert_eq!(a.mode, Mode::Browsing);
        assert_eq!(a.selected_rows().len(), 1, "cancelling keeps the selection");
    }

    #[test]
    fn confirming_yes_asks_the_loop_to_delete() {
        let mut a = app();
        a.select_above_line();
        a.on_key(Key::Char('r'));
        assert_eq!(a.on_key(Key::Char('y')), Action::Reclaim);
        assert_eq!(a.mode, Mode::Reclaiming);
    }

    /// No mode may trap the user.
    #[test]
    fn quit_works_from_every_mode() {
        for mode in [Mode::Idle, Mode::Browsing, Mode::Done(String::new())] {
            let mut a = app();
            a.mode = mode.clone();
            a.on_key(Key::Quit);
            assert!(a.quit, "could not quit from {mode:?}");
        }
        // Help returns to where it came from, then quits.
        let mut a = app();
        a.on_key(Key::Help);
        assert!(matches!(a.mode, Mode::Help(_)));
        a.on_key(Key::Esc);
        assert_eq!(a.mode, Mode::Browsing);
        a.on_key(Key::Quit);
        assert!(a.quit);
    }

    /// Deletion is not interruptible; keys must not leak through it.
    #[test]
    fn input_is_ignored_while_deleting() {
        let mut a = app();
        a.mode = Mode::Reclaiming;
        assert_eq!(a.on_key(Key::Quit), Action::None);
        assert!(!a.quit);
    }

    #[test]
    fn hiding_below_line_rows_keeps_the_cursor_valid() {
        let mut a = app();
        a.move_by(1000);
        a.show_below = false;
        a.on_key(Key::Char('t'));
        a.on_key(Key::Char('t'));
        assert!(a.cursor < a.visible_len().max(1));
    }

    /// Hiding below-the-line rows must not hide something already chosen.
    #[test]
    fn a_selected_row_stays_visible_below_the_line() {
        let mut a = app();
        let bad = a
            .visible()
            .position(|r| r.project.root == Path::new("/bad"))
            .expect("the bad deal is on screen while show_below is set");
        a.move_to(bad);
        a.toggle();
        assert_eq!(a.selected_rows().len(), 1);

        a.show_below = false;
        assert!(
            a.visible().any(|r| r.project.root == Path::new("/bad")),
            "a selected row disappeared when the list was collapsed"
        );
    }

    #[test]
    fn starting_a_scan_clears_the_previous_one() {
        let mut a = app();
        assert!(!a.rows.is_empty());
        assert_eq!(a.on_key(Key::Char('s')), Action::StartScan);
        assert!(a.rows.is_empty());
        assert_eq!(a.mode, Mode::Scanning);
    }
}
