//! The interactive interface.
//!
//! Split so that almost none of it needs a terminal to test: [`app`] holds
//! every decision as a pure state machine, [`draw`] turns that state into
//! bytes, and this module owns the parts that genuinely must touch the
//! outside world — raw mode, the alternate screen, the event loop, and the
//! background thread that runs the scan.
//!
//! Built on crossterm rather than ratatui. The rendering that makes this tool
//! look like itself already exists in `chart` and `report`, so a widget
//! framework would mostly be a canvas to redraw it on, at 34 new crates
//! against a 50-crate tree. crossterm is 10, and it is ratatui's own backend,
//! so adopting one later would keep every line of the input handling here.

mod app;
mod draw;

pub use app::{Action, App, Key, Mode};

use crate::detector::Detector;
use crate::reclaim;
use crate::scan;
use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use std::io::stdout;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

/// How long to wait for a keypress before redrawing anyway. Short enough that
/// scan progress looks live, long enough not to spin a core.
const TICK: Duration = Duration::from_millis(80);

/// Restores the terminal however we leave — normal return, error, or panic.
///
/// A TUI that exits without undoing raw mode leaves the user with a shell that
/// does not echo, which is the rudest possible failure mode. `Drop` covers the
/// error paths and the panic hook covers the rest.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(
            stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            cursor::Hide
        )?;

        // The hook runs before unwinding reaches our Drop, so the backtrace is
        // printed to a usable terminal instead of a garbled one.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            Self::restore();
            previous(info);
        }));
        Ok(Self)
    }

    /// Idempotent on purpose: it can run from both the panic hook and `Drop`.
    fn restore() {
        let _ = disable_raw_mode();
        let _ = execute!(
            stdout(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            cursor::Show
        );
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        Self::restore();
    }
}

/// Run the interface to completion.
pub fn run(
    roots: Vec<PathBuf>,
    detectors: Vec<Detector>,
    opts: scan::Options,
    worth_rate: f64,
    auto_start: bool,
) -> Result<()> {
    let _guard = TerminalGuard::enter()?;

    let mut app = App::new(roots, worth_rate);
    let mut scan_rx: Option<Receiver<ScanMsg>> = None;

    if auto_start {
        app.mode = Mode::Scanning;
        scan_rx = Some(spawn_scan(&app, &detectors, opts));
    }

    loop {
        let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
        app.viewport = draw::viewport_rows(h);
        draw::render(&app, w, h)?;

        // Drain whatever the scan has produced since the last frame.
        if let Some(rx) = &scan_rx {
            let mut finished = false;
            for msg in rx.try_iter() {
                match msg {
                    ScanMsg::Event(e) => app.on_scan_event(e),
                    ScanMsg::Done => finished = true,
                }
            }
            if finished {
                app.scan_finished();
                scan_rx = None;
            }
        }

        if event::poll(TICK)? {
            match event::read()? {
                event::Event::Key(k) => {
                    if let Some(key) = translate(k) {
                        match app.on_key(key) {
                            Action::StartScan => {
                                scan_rx = Some(spawn_scan(&app, &detectors, opts));
                            }
                            Action::Reclaim => {
                                // Redraw first so the screen says what is
                                // happening before the process blocks on it.
                                draw::render(&app, w, h)?;
                                let outcome = reclaim::execute(&app.doomed());
                                app.finish_reclaim(outcome);
                            }
                            Action::None => {}
                        }
                    }
                }
                event::Event::Mouse(m) => match m.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(row) = draw::row_at(&app, m.row) {
                            app.move_to(row);
                            app.toggle();
                        }
                    }
                    MouseEventKind::ScrollDown => app.move_by(1),
                    MouseEventKind::ScrollUp => app.move_by(-1),
                    _ => {}
                },
                _ => {}
            }
        }

        if app.quit {
            break;
        }
    }
    Ok(())
}

enum ScanMsg {
    Event(scan::Event),
    Done,
}

/// Scanning happens off the UI thread so the interface stays responsive and
/// rows can appear while the walk is still going.
fn spawn_scan(app: &App, detectors: &[Detector], opts: scan::Options) -> Receiver<ScanMsg> {
    let (tx, rx) = mpsc::channel();
    let roots = app.roots.clone();
    let detectors = detectors.to_vec();
    std::thread::spawn(move || {
        scan::scan_with(&roots, &detectors, opts, &mut |e| {
            let _ = tx.send(ScanMsg::Event(e));
        });
        let _ = tx.send(ScanMsg::Done);
    });
    rx
}

/// Reduce a crossterm key to the small set the app understands.
fn translate(k: KeyEvent) -> Option<Key> {
    // Windows reports press *and* release; acting on both double-fires.
    if k.kind != KeyEventKind::Press {
        return None;
    }
    // Ctrl-C should always work, whatever mode we are in.
    if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c')) {
        return Some(Key::Quit);
    }
    Some(match k.code {
        KeyCode::Up | KeyCode::Char('k') => Key::Up,
        KeyCode::Down | KeyCode::Char('j') => Key::Down,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Home | KeyCode::Char('g') => Key::Home,
        KeyCode::End | KeyCode::Char('G') => Key::End,
        KeyCode::Char(' ') => Key::Space,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Char('q') => Key::Quit,
        KeyCode::Char('?') => Key::Help,
        KeyCode::Char(c) => Key::Char(c),
        _ => return None,
    })
}
