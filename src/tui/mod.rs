//! Terminal initialization, restoration, and the main run loop.

use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::config::Config;
use crate::error::Result;
use crate::theme::Theme;

pub mod app;
pub mod event;
pub mod views;
pub mod widgets;

pub use app::App;

pub type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

/// Convenience tick interval for the event loop.
pub const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Set up the alternate screen, raw mode, and mouse capture. The matching
/// teardown lives in [`restore`] and MUST run on every exit path.
pub fn setup() -> Result<TuiTerminal> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

pub fn restore(terminal: &mut TuiTerminal) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Run the TUI to completion. Always restores the terminal even on error.
pub async fn run(config: Config, theme: Theme) -> Result<()> {
    let mut terminal = setup()?;
    let result = App::new(config, theme).run(&mut terminal).await;
    let _ = restore(&mut terminal);
    result
}

/// Run the TUI starting with an automatic connection to `session`, bypassing
/// the session selector. Used by `blink open` and `blink connect`.
pub async fn run_with_session(
    config: Config,
    theme: Theme,
    session: crate::session::Session,
) -> Result<()> {
    let mut terminal = setup()?;
    let result = App::with_session(config, theme, session)
        .run(&mut terminal)
        .await;
    let _ = restore(&mut terminal);
    result
}
