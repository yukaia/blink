//! Terminal initialization, restoration, and the main run loop.

use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::cursor::Show;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::config::Config;
use crate::error::Result;
use crate::theme::Theme;

pub mod app;
pub mod event;
pub mod hit;
pub mod plan;
pub mod state;
pub mod views;
pub mod widgets;

pub use app::App;

pub type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

/// Convenience tick interval for the event loop.
pub const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Restore the terminal from inside a panic on the UI thread, then delegate
/// to the hook that was installed before us.
///
/// [`restore`] only runs on the normal return path — a panic anywhere in the
/// run loop (a render bug, an `unwrap` that turns out to be reachable)
/// unwinds straight past it, and `Terminal`'s `Drop` undoes neither raw mode
/// nor the alternate screen. The user would be left at a shell with no echo
/// and no line editing, with the panic message painted onto the alternate
/// screen they can no longer see.
///
/// Only the UI thread's panics end the TUI, though. The run loop runs on
/// the thread that calls [`setup`] (`main`'s, inside `block_on`); spawned
/// tasks run on the runtime's worker threads, and a panic there is caught
/// by tokio while the app carries on. Restoring the terminal for one of
/// those, as this hook used to for every panic, left the still-running TUI
/// drawing in cooked mode over the main screen, and the default hook's
/// message landed in the middle of it. Those go to the log instead.
///
/// Idempotent: installing twice would nest the hooks, and both `run` and
/// `run_with_session` call [`setup`].
fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let ui_thread = std::thread::current().id();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if std::thread::current().id() != ui_thread {
                tracing::error!("background task panicked: {info}");
                return;
            }
            // Best effort — we're already panicking, so a failure here has
            // nowhere useful to go.
            let _ = disable_raw_mode();
            let _ = leave_screen(&mut io::stdout());
            previous(info);
        }));
    });
}

/// Set up the alternate screen, raw mode, and — when `mouse` is on — mouse
/// capture. The matching teardown lives in [`restore`] and MUST run on
/// every exit path; it disables capture either way, which is harmless when
/// it was never enabled.
pub fn setup(mouse: bool) -> Result<TuiTerminal> {
    // Before raw mode, so the hook is in place for anything that follows.
    install_panic_hook();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    enter_screen(&mut stdout, mouse)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

pub fn restore(terminal: &mut TuiTerminal) -> Result<()> {
    disable_raw_mode()?;
    leave_screen(terminal.backend_mut())?;
    // Already shown by `leave_screen`; this keeps ratatui's own record of
    // the cursor in step.
    terminal.show_cursor()?;
    Ok(())
}

/// Switch `out` to the alternate screen, capturing the mouse if `mouse`.
fn enter_screen(out: &mut impl io::Write, mouse: bool) -> io::Result<()> {
    execute!(out, EnterAlternateScreen)?;
    if mouse {
        execute!(out, EnableMouseCapture)?;
    }
    Ok(())
}

/// Undo [`enter_screen`] and show the cursor. Shared by [`restore`] and the
/// panic hook, so the two ways out leave the terminal the same.
fn leave_screen(out: &mut impl io::Write) -> io::Result<()> {
    execute!(out, LeaveAlternateScreen, DisableMouseCapture, Show)
}

/// Run the TUI to completion. Always restores the terminal even on error.
///
/// `warnings` are things to tell the user from before the TUI started —
/// why the configured theme wasn't used, a logging setting that didn't
/// take — and go to the log, where they can be seen.
pub async fn run(config: Config, theme: Theme, warnings: Vec<String>) -> Result<()> {
    let mut terminal = setup(config.terminal.mouse)?;
    let mut app = App::new(config, theme);
    app.startup_warnings.splice(0..0, warnings);
    let result = app.run(&mut terminal).await;
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
    warnings: Vec<String>,
    session: crate::session::Session,
    unsaved: bool,
) -> Result<()> {
    let mut terminal = setup(config.terminal.mouse)?;
    let mut app = App::with_session(config, theme, session, unsaved);
    app.startup_warnings.splice(0..0, warnings);
    let result = app.run(&mut terminal).await;
    let _ = restore(&mut terminal);
    result
}

#[cfg(test)]
mod tests {
    use super::{enter_screen, leave_screen};

    const ALT_SCREEN_ON: &str = "\x1b[?1049h";
    const ALT_SCREEN_OFF: &str = "\x1b[?1049l";
    const MOUSE_ON: &str = "\x1b[?1000h";
    const MOUSE_OFF: &str = "\x1b[?1000l";
    const SHOW_CURSOR: &str = "\x1b[?25h";

    fn written(f: impl FnOnce(&mut Vec<u8>) -> std::io::Result<()>) -> String {
        let mut out = Vec::new();
        f(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn entering_switches_to_the_alternate_screen_without_the_mouse() {
        let out = written(|o| enter_screen(o, false));
        assert_eq!(out, ALT_SCREEN_ON);
    }

    #[test]
    fn entering_with_the_mouse_on_captures_it_after_switching() {
        let out = written(|o| enter_screen(o, true));
        assert!(out.starts_with(ALT_SCREEN_ON), "{out:?}");
        assert!(out.contains(MOUSE_ON), "{out:?}");
    }

    /// Mouse capture is released whether or not it was taken: leaving does
    /// not know how the screen was entered, and releasing it twice is
    /// harmless where leaving it on is not.
    #[test]
    fn leaving_restores_the_screen_releases_the_mouse_and_shows_the_cursor() {
        let out = written(leave_screen);
        assert!(out.starts_with(ALT_SCREEN_OFF), "{out:?}");
        assert!(out.contains(MOUSE_OFF), "{out:?}");
        assert!(out.ends_with(SHOW_CURSOR), "{out:?}");
    }
}
