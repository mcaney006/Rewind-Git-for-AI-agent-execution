//! Read-only native replay interface.

mod model;
mod terminal;
mod ui;

use std::io::{Stdout, stdout};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use rewind_domain::{ObjectId, RunId};
use rewind_store::StoreError;
use thiserror::Error;

use crate::model::App;

const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// A failure to open, read, or present a recorded run.
#[derive(Debug, Error)]
pub enum ReplayError {
    /// The read-only metadata or immutable-object operation failed.
    #[error("cannot read replay data: {0}")]
    Store(#[from] StoreError),
    /// The configured terminal cache cannot retain any bytes.
    #[error("terminal replay cache must be greater than zero bytes")]
    InvalidTerminalCache,
    /// A verified terminal object disagreed with its event metadata.
    #[error(
        "terminal object {object_id} has {actual} bytes, but its event records {expected} bytes"
    )]
    TerminalLength {
        /// The affected immutable object.
        object_id: ObjectId,
        /// The byte count in the event.
        expected: usize,
        /// The verified logical byte count.
        actual: usize,
    },
    /// A terminal setup, input, or drawing operation failed.
    #[error("cannot {operation}: {source}")]
    Terminal {
        /// The failed terminal operation.
        operation: &'static str,
        /// The operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// The requested store path cannot be retained for diagnostics.
    #[error("invalid replay store path: {0}")]
    InvalidStorePath(PathBuf),
}

/// Native replay result.
pub type Result<T> = std::result::Result<T, ReplayError>;

/// Opens a run in the read-only native replay interface.
///
/// Timeline reads are paged, immutable terminal objects are verified through
/// the store, and retained terminal bytes never exceed `terminal_cache_bytes`.
/// The function restores raw mode and the alternate screen on every return
/// path. It never mutates run or execution state.
pub fn replay(store_root: &Path, run_id: RunId, terminal_cache_bytes: usize) -> Result<()> {
    if store_root.as_os_str().is_empty() {
        return Err(ReplayError::InvalidStorePath(store_root.to_path_buf()));
    }
    let mut app = App::load(store_root, run_id, terminal_cache_bytes)?;
    let mut terminal = TerminalSession::enter()?;

    while !app.should_quit {
        terminal
            .terminal
            .draw(|frame| ui::draw(frame, &app))
            .map_err(|source| ReplayError::Terminal {
                operation: "draw replay interface",
                source,
            })?;

        if event::poll(FRAME_INTERVAL).map_err(|source| ReplayError::Terminal {
            operation: "poll terminal input",
            source,
        })? {
            match event::read().map_err(|source| ReplayError::Terminal {
                operation: "read terminal input",
                source,
            })? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    handle_key(&mut app, key)?;
                }
                Event::Resize(_, _)
                | Event::FocusGained
                | Event::FocusLost
                | Event::Mouse(_)
                | Event::Paste(_)
                | Event::Key(_) => {}
            }
        }
        app.tick(Instant::now())?;
    }
    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) -> Result<()> {
    if app.show_help {
        if matches!(
            key.code,
            KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q')
        ) {
            app.show_help = false;
        }
        return Ok(());
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return Ok(());
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('?') => app.show_help = true,
        KeyCode::Tab => app.cycle_focus(false),
        KeyCode::BackTab => app.cycle_focus(true),
        KeyCode::Char(' ') => app.toggle_playback(),
        KeyCode::Char('+') | KeyCode::Char('=') => app.change_speed(true),
        KeyCode::Char('-') => app.change_speed(false),
        KeyCode::Char('[') => app.step_checkpoint(false)?,
        KeyCode::Char(']') => app.step_checkpoint(true)?,
        KeyCode::Left => app.step_event(false)?,
        KeyCode::Right => app.step_event(true)?,
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(false)?,
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(true)?,
        KeyCode::PageUp => app.scroll_workspace_preview(false),
        KeyCode::PageDown => app.scroll_workspace_preview(true),
        KeyCode::Enter => app.activate()?,
        KeyCode::Backspace
        | KeyCode::Delete
        | KeyCode::Insert
        | KeyCode::Home
        | KeyCode::End
        | KeyCode::F(_)
        | KeyCode::Null
        | KeyCode::CapsLock
        | KeyCode::ScrollLock
        | KeyCode::NumLock
        | KeyCode::PrintScreen
        | KeyCode::Pause
        | KeyCode::Menu
        | KeyCode::KeypadBegin
        | KeyCode::Media(_)
        | KeyCode::Modifier(_)
        | KeyCode::Char(_) => {}
    }
    Ok(())
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode().map_err(|source| ReplayError::Terminal {
            operation: "enable raw terminal mode",
            source,
        })?;
        let mut output = stdout();
        if let Err(source) = execute!(output, EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(ReplayError::Terminal {
                operation: "enter alternate terminal screen",
                source,
            });
        }
        let backend = CrosstermBackend::new(output);
        let terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(source) => {
                let mut output = stdout();
                let _ = execute!(output, Show, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                return Err(ReplayError::Terminal {
                    operation: "initialize terminal renderer",
                    source,
                });
            }
        };
        Ok(Self { terminal })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = execute!(self.terminal.backend_mut(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}
