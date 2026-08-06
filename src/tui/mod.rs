//! Terminal initialization, restoration, and the main run loop.

use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::cursor::Show;
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
pub mod plan;
pub mod state;
pub mod views;
pub mod widgets;

pub use app::App;

pub type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

/// Convenience tick interval for the event loop.
pub const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Restore the terminal from inside a panic, then delegate to the hook that
/// was installed before us.
///
/// [`restore`] only runs on the normal return path — a panic anywhere in the
/// run loop (a render bug, an `unwrap` that turns out to be reachable)
/// unwinds straight past it, and `Terminal`'s `Drop` undoes neither raw mode
/// nor the alternate screen. The user would be left at a shell with no echo
/// and no line editing, with the panic message painted onto the alternate
/// screen they can no longer see.
///
/// Idempotent: installing twice would nest the hooks, and both `run` and
/// `run_with_session` call [`setup`].
fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Best effort — we're already panicking, so a failure here has
            // nowhere useful to go. Order mirrors `restore`.
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture, Show);
            previous(info);
        }));
    });
}

/// Set up the alternate screen, raw mode, and mouse capture. The matching
/// teardown lives in [`restore`] and MUST run on every exit path.
pub fn setup() -> Result<TuiTerminal> {
    // Before raw mode, so the hook is in place for anything that follows.
    install_panic_hook();
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
///
/// `unsaved` is true for `blink connect`, whose session comes from a URL and
/// has no file behind it; the connect flow offers to persist those.
pub async fn run_with_session(
    config: Config,
    theme: Theme,
    session: crate::session::Session,
    unsaved: bool,
) -> Result<()> {
    let mut terminal = setup()?;
    let result = App::with_session(config, theme, session, unsaved)
        .run(&mut terminal)
        .await;
    let _ = restore(&mut terminal);
    result
}
