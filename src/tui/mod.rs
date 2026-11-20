//! Terminal setup and teardown, the panic hook, and the entry points that
//! run the TUI between them. The run loop itself is `App::run`.

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

/// Panics from background tasks, waiting for the run loop to put them in
/// the log pane. See [`install_panic_hook`].
#[cfg(not(test))]
static BACKGROUND_PANICS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

// `BACKGROUND_PANICS` per test thread: tests run in parallel, and any that
// drains the log's notices would otherwise take another's.
#[cfg(test)]
thread_local! {
    static BACKGROUND_PANICS: std::sync::Mutex<Vec<String>> =
        const { std::sync::Mutex::new(Vec::new()) };
}

/// Run `f` on the queue of background panics.
#[cfg(not(test))]
fn with_panic_queue<T>(f: impl FnOnce(&mut Vec<String>) -> T) -> T {
    f(&mut BACKGROUND_PANICS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner))
}

#[cfg(test)]
fn with_panic_queue<T>(f: impl FnOnce(&mut Vec<String>) -> T) -> T {
    BACKGROUND_PANICS.with(|q| f(&mut q.lock().unwrap_or_else(std::sync::PoisonError::into_inner)))
}

/// How many background panics wait at most; past that, more add nothing
/// the first ones haven't said.
const MAX_BACKGROUND_PANICS: usize = 16;

/// What a background panic says in the log: its message, and where.
fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    let what = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(no message)");
    let message = match info.location() {
        Some(at) => format!("a background task failed: {what} (at {at})"),
        None => format!("a background task failed: {what}"),
    };
    crate::error::sanitize(message)
}

/// Queue `message` for the log pane. Never panics: it runs inside the
/// panic hook.
pub(crate) fn note_background_panic(message: String) {
    with_panic_queue(|queue| {
        if queue.len() < MAX_BACKGROUND_PANICS {
            queue.push(message);
        }
    });
}

/// The background panics since the last call, for the log pane.
pub(crate) fn take_background_panics() -> Vec<String> {
    with_panic_queue(std::mem::take)
}

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
/// message landed in the middle of it. Those go to the log pane instead,
/// through [`take_background_panics`], as well as to `tracing`: that alone
/// is a sink unless `BLINK_LOG_FILE` is set, and a task that panicked
/// never sends the result the UI is waiting for, so the user saw a wait
/// that never ended with no reason given.
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
                note_background_panic(panic_message(info));
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
    let terminal = undo_on_failure(
        || {
            let mut stdout = io::stdout();
            enter_screen(&mut stdout, mouse)?;
            Terminal::new(CrosstermBackend::new(stdout))
        },
        || {
            let _ = leave_screen(&mut io::stdout());
            let _ = disable_raw_mode();
        },
    )?;
    Ok(terminal)
}

/// Run `steps`, and `undo` if they fail. `setup` turns raw mode on before
/// anything else; a step after it failing used to return with raw mode
/// still on, and the caller's `?` then skipped [`restore`], leaving a shell
/// with no echo and no line editing.
fn undo_on_failure<T>(steps: impl FnOnce() -> io::Result<T>, undo: impl FnOnce()) -> io::Result<T> {
    let done = steps();
    if done.is_err() {
        undo();
    }
    done
}

pub fn restore(terminal: &mut TuiTerminal) -> Result<()> {
    // Each step runs whatever the ones before did: the array is evaluated
    // in order, in full, before `first_failure` looks at it.
    Ok(first_failure([
        disable_raw_mode(),
        leave_screen(terminal.backend_mut()),
        // Already shown by `leave_screen`; this keeps ratatui's own record
        // of the cursor in step.
        terminal.show_cursor(),
    ])?)
}

/// The first failure among steps that have all been run. The way out of the
/// TUI must undo as much as it can: `restore` used to stop at a failed
/// `disable_raw_mode`, leaving the alternate screen up and the cursor
/// hidden.
fn first_failure<const N: usize>(steps: [io::Result<()>; N]) -> io::Result<()> {
    steps
        .into_iter()
        .find(|step| step.is_err())
        .unwrap_or(Ok(()))
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
    let mouse = config.terminal.mouse;
    run_app(App::new(config, theme), mouse, warnings).await
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
    let mouse = config.terminal.mouse;
    run_app(
        App::with_session(config, theme, session, unsaved),
        mouse,
        warnings,
    )
    .await
}

/// What [`run`] and [`run_with_session`] share once each has built its
/// `App`: set the terminal up, run, and put it back on every way out.
///
/// A failure to put the terminal back is returned when the app itself
/// ended well; it used to be dropped, leaving a broken terminal with no
/// word of why. When the app failed too, its error is the one returned and
/// the restore failure goes to stderr, which is the main screen again by
/// then, or as near to it as restoring got.
async fn run_app(mut app: App, mouse: bool, warnings: Vec<String>) -> Result<()> {
    let mut terminal = setup(mouse)?;
    app.startup_warnings.splice(0..0, warnings);
    let result = app.run(&mut terminal).await;
    let restored = restore(&mut terminal);
    match (result, restored) {
        (Ok(()), restored) => restored,
        (Err(e), Err(restore_error)) => {
            eprintln!("blink: could not restore the terminal: {restore_error}");
            Err(e)
        }
        (Err(e), Ok(())) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::{enter_screen, first_failure, leave_screen, undo_on_failure};

    #[test]
    fn a_failed_step_after_raw_mode_undoes_it() {
        let mut undone = false;
        let r: std::io::Result<()> =
            undo_on_failure(|| Err(std::io::Error::other("no screen")), || undone = true);
        assert!(r.is_err());
        assert!(undone, "raw mode must not be left on");

        let mut undone = false;
        let r = undo_on_failure(|| Ok(7), || undone = true);
        assert_eq!(r.unwrap(), 7);
        assert!(!undone, "nothing to undo when it worked");
    }

    #[test]
    fn restoring_reports_the_first_failure_of_all_its_steps() {
        let mut ran = Vec::new();
        let mut step = |n: u8, ok: bool| {
            ran.push(n);
            if ok {
                Ok(())
            } else {
                Err(std::io::Error::other(format!("step {n}")))
            }
        };
        // As `restore` builds it: every step evaluated before the result.
        let r = first_failure([step(1, false), step(2, false), step(3, true)]);
        assert_eq!(ran, [1, 2, 3], "a failure doesn't stop the rest");
        assert_eq!(r.unwrap_err().to_string(), "step 1");
        assert!(first_failure([Ok(()), Ok(())]).is_ok());
    }

    /// A background task's panic reaches the log pane, not just `tracing`.
    #[test]
    fn a_background_panic_is_queued_for_the_log_once_each() {
        super::note_background_panic("first".into());
        super::note_background_panic("second".into());
        let taken = super::take_background_panics();
        assert_eq!(taken, ["first", "second"]);
        assert!(super::take_background_panics().is_empty(), "taken once");
    }

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
