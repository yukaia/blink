//! App state machine and main run loop.
//!
//! Background work (connect, list, …) is dispatched via tokio tasks that send
//! [`AppEvent`]s back through a channel. The run loop selects over keyboard
//! input, ticks, and these app events.

use std::sync::Arc;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use tokio::sync::{mpsc, Mutex};
use zeroize::Zeroize;

use crate::checkpoint::Checkpoint;
use crate::config::Config;
use crate::error::Result;
use crate::preview;
use crate::session::{AuthMethod, Session};
use crate::theme::Theme;
use crate::transfer::{Dispatcher, TransferJob, TransferManager};
use crate::transport::{self, RemoteEntry, Transport};
use crate::tui::event::{AppEvent, Event, EventStream};
use crate::tui::state::{
    EditSessionForm, HostKeyChangedInfo, OverwritePending, PaneEntry, PaneState, PendingCancel,
    PendingDelete, PendingHostKey, Viewer,
};
use crate::tui::{TuiTerminal, TICK_INTERVAL};

use crate::transport::CONNECT_TIMEOUT;

/// Which screen is the user currently looking at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    SessionSelect,
    /// Modal over SessionSelect: user enters a URL for an ad-hoc session.
    NewSession,
    /// Modal over SessionSelect: edit an existing saved session.
    EditSession,
    /// Modal over SessionSelect: confirm deletion of a saved session.
    ConfirmDeleteSession,
    /// Modal over SessionSelect: user types the password before we connect.
    PasswordPrompt,
    /// Modal over SessionSelect: user types the SSH key passphrase. Reached
    /// when an initial key-auth connect fails with [`BlinkError::KeyNeedsPassphrase`].
    KeyPassphrasePrompt,
    /// Modal over Main: connect task is in flight.
    Connection,
    Main,
    /// Modal-ish over Main: incremental substring filter on the active pane.
    Search,
    /// Modal over Main: save current state as a session.
    SaveSession,
    /// Modal over Main: rename a remote file or folder.
    Rename,
    /// Modal over Main: create a new remote directory.
    Mkdir,
    /// Modal over Main: confirm deletion of a remote file or folder.
    ConfirmDelete,
    /// Modal over Main: confirm overwriting an existing file (rename / upload).
    ConfirmOverwrite,
    /// Modal over Main: text or image viewer.
    Viewer,
    /// Overlay; previous_screen is preserved so we know what to show behind.
    Help,
    /// Overlay.
    ConfirmQuit,
    /// Overlay over Main: confirm cancellation of an in-flight transfer.
    ConfirmCancel,
    /// Overlay over Main: confirm disconnect (aborts in-flight transfers,
    /// closes the transport, returns to the session selector).
    ConfirmDisconnect,
    /// Modal over Connection: server presented an unknown host key.
    /// The user must accept (and optionally save) or reject before the
    /// connection can proceed.
    ConfirmHostKey,
    /// Modal: server's host key does not match the stored one (hard error).
    HostKeyChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Local,
    Remote,
    Transfers,
    Log,
}

/// Which page the bottom panel is showing. Updated whenever the user Tabs
/// into one of the bottom panes; sticky while focus is on Local / Remote so
/// the user can keep an eye on whichever they last looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BottomPane {
    Transfers,
    Log,
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub time: chrono::DateTime<chrono::Local>,
    pub level: LogLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Success,
    Warn,
    Error,
}

/// Shared transport: behind a `tokio::Mutex` so background tasks (list, rename,
/// preview, …) can borrow it without contending with the UI loop.
type SharedTransport = Arc<Mutex<Box<dyn Transport>>>;

/// Capacity reserved for a credential typed into a prompt.
///
/// Comfortably longer than any realistic password or key passphrase, so the
/// buffer never has to grow. Growth is the problem: `String::push`
/// reallocating copies the partial secret into a new allocation and frees the
/// old one without wiping it, so fragments outlive every later zeroize.
const CREDENTIAL_CAPACITY: usize = 256;

/// A zeroize-on-drop string buffer, pre-sized so typing into it never
/// reallocates. See [`CREDENTIAL_CAPACITY`].
fn credential_buffer() -> zeroize::Zeroizing<String> {
    zeroize::Zeroizing::new(String::with_capacity(CREDENTIAL_CAPACITY))
}

pub struct App {
    pub config: Config,
    pub theme: Theme,
    pub screen: Screen,
    pub previous_screen: Screen,
    pub active_pane: Pane,

    // Session selector
    pub sessions: Vec<Session>,
    pub session_cursor: usize,

    // Pending connect — set when transitioning to PasswordPrompt or Connection,
    // cleared when the connect resolves or the user cancels.
    pub pending_session: Option<Session>,
    /// Password as it is being typed.
    ///
    /// `Zeroizing` so the buffer is wiped on drop, and pre-sized via
    /// [`credential_buffer`] so the per-keystroke `push` doesn't reallocate:
    /// a realloc copies the partial password into a fresh allocation and
    /// frees the old one *without* clearing it, leaving fragments on the
    /// heap that no later wipe can reach. Abandon paths call `.zeroize()`
    /// rather than `.clear()`, which would only reset the length.
    pub password_input: zeroize::Zeroizing<String>,

    // SSH key passphrase prompt
    /// Key passphrase as it is being typed. Same handling as
    /// [`Self::password_input`].
    pub passphrase_input: zeroize::Zeroizing<String>,
    pub passphrase_error: Option<String>,
    /// Whether the user has already submitted at least one passphrase for the
    /// current `pending_session`. If true on a re-entry to the prompt, the UI
    /// surfaces "passphrase incorrect, try again" instead of the first-time
    /// message.
    passphrase_attempted: bool,

    // New-session form (URL-style ad-hoc input)
    pub new_session_input: String,
    pub new_session_error: Option<String>,

    // Edit-session form
    pub edit_session_form: Option<EditSessionForm>,
    /// Name of the saved session awaiting delete confirmation.
    pub pending_session_delete: Option<Session>,

    // Save-session form
    pub save_session_input: String,
    pub save_session_error: Option<String>,

    // Search (substring filter on Local or Remote)
    pub search_input: String,
    /// Which pane the active search is filtering. Snapshotted when search
    /// opens so the user can't accidentally retarget by Tabbing — we don't
    /// allow Tab inside search anyway.
    pub search_target: Pane,

    // Main view
    pub current_session: Option<Session>,
    pub transport: Option<SharedTransport>,
    pub local: PaneState,
    pub remote: PaneState,
    pub log: std::collections::VecDeque<LogLine>,
    #[allow(dead_code)]
    pub transfers: Vec<TransferJob>,
    /// Which page the bottom panel renders. Auto-updated when the user Tabs
    /// into Transfers or Log; otherwise sticky.
    pub bottom_pane: BottomPane,
    /// Cursor within the active-jobs list when the Transfers pane is focused.
    pub transfer_cursor: usize,
    /// Cancellation in progress: the user has pressed `c` and is being asked
    /// to confirm. Cleared on confirm/cancel.
    pub pending_cancel: Option<PendingCancel>,

    // Rename form
    pub rename_input: String,
    pub rename_original: String,
    pub rename_error: Option<String>,

    // Mkdir form
    pub mkdir_input: String,
    pub mkdir_error: Option<String>,

    // Delete confirmation
    pub pending_delete: Option<PendingDelete>,

    // Overwrite confirmation (shared between rename and upload)
    pub pending_overwrite: Option<OverwritePending>,

    // Async plumbing
    app_event_tx: mpsc::UnboundedSender<AppEvent>,
    app_event_rx: Option<mpsc::UnboundedReceiver<AppEvent>>,

    // Transfer dispatcher integration
    /// Cached for the duration of the connected session so the dispatcher
    /// can open new connections per parallel slot. Cleared on disconnect /
    /// quit / connect failure / user cancel.
    ///
    /// Wrapped in `Zeroizing` so the underlying allocation is wiped when
    /// the field is reassigned to `None` or the `App` is dropped — a
    /// long-lived blink process doesn't keep the credential live on the
    /// heap after the auth window closes.
    pending_password: Option<zeroize::Zeroizing<String>>,
    pub transfer_manager: Option<TransferManager>,
    dispatcher: Option<Dispatcher>,

    // Host-key verification
    /// Pending host-key prompt. Set when `AppEvent::HostKeyUnknown` arrives;
    /// cleared when the user accepts or rejects.
    pub pending_host_key: Option<PendingHostKey>,
    /// Set when `AppEvent::HostKeyChanged` arrives; displayed until dismissed.
    pub host_key_changed_info: Option<HostKeyChangedInfo>,

    // Walk checkpointing
    /// The checkpoint being tracked for the current (or most recent) batch.
    /// `None` when no batch is in flight. Written to disk before the first
    /// job is enqueued; updated as jobs complete; removed when the batch
    /// finishes cleanly. Survives app crashes so the batch can be resumed.
    active_checkpoint: Option<Checkpoint>,
    /// Maps dispatcher job-id → index into `active_checkpoint.jobs` so the
    /// transfer-event handler can look up which checkpoint entry to mark done
    /// without a linear scan of the full plan.
    checkpoint_job_map: std::collections::HashMap<u64, usize>,

    // Viewer
    pub viewer: Option<Viewer>,
    /// Set to true when an image viewer needs its graphics escape sequences
    /// re-emitted (initial open, terminal resize). The run loop emits and
    /// clears this flag after each `terminal.draw`.
    image_needs_redraw: bool,
    /// Force a full terminal repaint on the next loop iteration. Used when
    /// closing an image viewer: sixel and kitty graphics live outside
    /// ratatui's cell buffer, so ratatui's diffing renderer doesn't know to
    /// repaint those cells when the modal goes away.
    needs_terminal_clear: bool,

    // Misc
    #[allow(dead_code)]
    pub status_message: Option<(Instant, String)>,
    pub should_quit: bool,
    /// Session to connect to automatically on startup, bypassing the session
    /// selector. Set by `blink open` and `blink connect`; `None` in normal
    /// interactive mode.
    autoconnect: Option<Session>,
    /// Session files that failed to load during [`App::new`], drained into
    /// the log by [`App::run`] once there is somewhere to put them.
    startup_warnings: Vec<String>,
}

mod actions;
mod checkpoint_glue;
mod controls;
mod events;
mod handlers;
mod panes;
mod transfers;
mod viewer;

impl App {
    pub fn new(config: Config, theme: Theme) -> Self {
        // `new` predates the log, so stash any unreadable session files and
        // let `run` report them once the log exists.
        let listing = Session::list_all_detailed().ok();
        let (sessions, startup_warnings) = match listing {
            Some(l) => (l.sessions, l.skipped),
            None => (Vec::new(), Vec::new()),
        };
        let mut local = PaneState::empty();
        local.path = crate::paths::default_local_dir().display().to_string();
        let (tx, rx) = mpsc::unbounded_channel();

        Self {
            config,
            theme,
            screen: Screen::SessionSelect,
            previous_screen: Screen::SessionSelect,
            active_pane: Pane::Local,
            sessions,
            session_cursor: 0,
            pending_session: None,
            password_input: credential_buffer(),
            passphrase_input: credential_buffer(),
            passphrase_error: None,
            passphrase_attempted: false,
            new_session_input: String::new(),
            new_session_error: None,
            edit_session_form: None,
            pending_session_delete: None,
            save_session_input: String::new(),
            save_session_error: None,
            search_input: String::new(),
            search_target: Pane::Local,
            current_session: None,
            transport: None,
            local,
            remote: PaneState::empty(),
            log: std::collections::VecDeque::new(),
            transfers: Vec::new(),
            bottom_pane: BottomPane::Log,
            transfer_cursor: 0,
            pending_cancel: None,
            rename_input: String::new(),
            rename_original: String::new(),
            rename_error: None,
            mkdir_input: String::new(),
            mkdir_error: None,
            pending_delete: None,
            pending_overwrite: None,
            app_event_tx: tx,
            app_event_rx: Some(rx),
            pending_password: None,
            transfer_manager: None,
            dispatcher: None,
            pending_host_key: None,
            host_key_changed_info: None,
            active_checkpoint: None,
            checkpoint_job_map: std::collections::HashMap::new(),
            viewer: None,
            image_needs_redraw: false,
            needs_terminal_clear: false,
            status_message: None,
            should_quit: false,
            autoconnect: None,
            startup_warnings,
        }
    }

    /// Re-read the sessions directory, reporting any file that wouldn't load.
    ///
    /// Every caller that mutates sessions on disk goes through here so an
    /// unreadable file is never silently dropped from the selector.
    fn reload_sessions(&mut self) {
        match Session::list_all_detailed() {
            Ok(listing) => {
                self.sessions = listing.sessions;
                for skip in listing.skipped {
                    self.push_log(LogLevel::Warn, format!("unreadable session {skip}"));
                }
            }
            Err(e) => {
                self.push_log(LogLevel::Error, format!("could not list sessions: {e}"));
            }
        }
    }

    /// Build an `App` that automatically connects to `session` on startup,
    /// skipping the session selector entirely. Used by `blink open` and
    /// `blink connect`.
    pub fn with_session(config: Config, theme: Theme, session: Session) -> Self {
        let mut app = Self::new(config, theme);
        app.autoconnect = Some(session);
        app
    }

    pub async fn run(mut self, terminal: &mut TuiTerminal) -> Result<()> {
        let rx = self.app_event_rx.take().expect("rx initialized in new()");
        let mut events = EventStream::new(TICK_INTERVAL, rx);

        self.refresh_local_pane();
        self.push_log(
            LogLevel::Info,
            format!("blink {} — ready", env!("CARGO_PKG_VERSION")),
        );
        let proto = preview::detect(self.config.terminal.image_preview);
        let proto_label = match proto {
            preview::GraphicsProtocol::Kitty => "kitty",
            preview::GraphicsProtocol::Sixel => "sixel",
            preview::GraphicsProtocol::Iterm2 => "iterm2",
            preview::GraphicsProtocol::None => "none",
        };
        self.push_log(
            LogLevel::Info,
            format!("graphics protocol: {proto_label}"),
        );
        // Session files that wouldn't parse. Reported here rather than left
        // to `tracing`, which goes to a sink unless BLINK_LOG_FILE is set —
        // so the session just disappeared from the selector with no clue why.
        for skip in std::mem::take(&mut self.startup_warnings) {
            self.push_log(LogLevel::Warn, format!("unreadable session {skip}"));
        }

        // Autoconnect: `blink open` / `blink connect` pre-populate this
        // field. We trigger the connect here, after the runtime is live and
        // the event channel is wired, so `start_connect`'s tokio::spawn lands
        // in the right context.
        if let Some(session) = self.autoconnect.take() {
            match &session.auth {
                AuthMethod::Password => {
                    self.pending_session = Some(session);
                    self.password_input.zeroize();
                    self.screen = Screen::PasswordPrompt;
                }
                AuthMethod::Key { .. } | AuthMethod::Agent => {
                    self.pending_session = Some(session.clone());
                    self.pending_password = None;
                    self.start_connect(session, None);
                }
            }
        }

        loop {
            if self.needs_terminal_clear {
                terminal.clear()?;
                self.needs_terminal_clear = false;
            }
            terminal.draw(|f| self.draw(f))?;
            self.after_draw(terminal)?;

            match events.next().await? {
                Event::Key(k) => self.handle_key(k),
                Event::App(e) => self.handle_app_event(e),
                Event::Tick => {}
                Event::Resize(_, _) => {
                    self.image_needs_redraw = true;
                }
            }

            if self.should_quit {
                break;
            }
        }

        // Cleanup: stop the dispatcher loop. In-flight workers are orphaned
        // and run to completion against the runtime tear-down — we don't
        // await them here. The cached password is dropped along with self.
        if let Some(d) = self.dispatcher.take() {
            d.shutdown().await;
        }
        self.pending_password = None;

        Ok(())
    }

    fn draw(&self, f: &mut Frame) {
        match self.screen {
            Screen::SessionSelect => crate::tui::views::session_select::render(f, self),
            Screen::NewSession => {
                crate::tui::views::session_select::render(f, self);
                crate::tui::views::new_session::render(f, self);
            }
            Screen::EditSession => {
                crate::tui::views::session_select::render(f, self);
                crate::tui::views::edit_session::render(f, self);
            }
            Screen::ConfirmDeleteSession => {
                crate::tui::views::session_select::render(f, self);
                crate::tui::views::confirm_delete_session::render(f, self);
            }
            Screen::PasswordPrompt => {
                crate::tui::views::session_select::render(f, self);
                crate::tui::views::password_prompt::render(f, self);
            }
            Screen::KeyPassphrasePrompt => {
                crate::tui::views::session_select::render(f, self);
                crate::tui::views::key_passphrase_prompt::render(f, self);
            }
            Screen::Connection => {
                crate::tui::views::main::render(f, self);
                crate::tui::views::connection::render(f, self);
            }
            Screen::Main => crate::tui::views::main::render(f, self),
            Screen::Search => {
                crate::tui::views::main::render(f, self);
                crate::tui::views::search::render(f, self);
            }
            Screen::SaveSession => {
                crate::tui::views::main::render(f, self);
                crate::tui::views::save_session::render(f, self);
            }
            Screen::Rename => {
                crate::tui::views::main::render(f, self);
                crate::tui::views::rename::render(f, self);
            }
            Screen::Mkdir => {
                crate::tui::views::main::render(f, self);
                crate::tui::views::mkdir::render(f, self);
            }
            Screen::ConfirmDelete => {
                crate::tui::views::main::render(f, self);
                crate::tui::views::confirm_delete::render(f, self);
            }
            Screen::ConfirmOverwrite => {
                crate::tui::views::main::render(f, self);
                crate::tui::views::confirm_overwrite::render(f, self);
            }
            Screen::Viewer => {
                crate::tui::views::main::render(f, self);
                crate::tui::views::viewer::render(f, self);
            }
            Screen::ConfirmCancel => {
                crate::tui::views::main::render(f, self);
                crate::tui::views::confirm_cancel::render(f, self);
            }
            Screen::ConfirmDisconnect => {
                crate::tui::views::main::render(f, self);
                crate::tui::views::confirm_disconnect::render(f, self);
            }
            Screen::ConfirmHostKey => {
                crate::tui::views::session_select::render(f, self);
                crate::tui::views::confirm_host_key::render(f, self);
            }
            Screen::HostKeyChanged => {
                crate::tui::views::session_select::render(f, self);
                crate::tui::views::host_key_changed::render(f, self);
            }
            Screen::Help => {
                crate::tui::views::main::render(f, self);
                crate::tui::views::help::render(f, self);
            }
            Screen::ConfirmQuit => {
                if self.previous_screen == Screen::Main {
                    crate::tui::views::main::render(f, self);
                } else {
                    crate::tui::views::session_select::render(f, self);
                }
                crate::tui::views::confirm_quit::render(f, self);
            }
        }
    }

    // -------------------------------------------------------------------
    // Key handling
    // -------------------------------------------------------------------

    fn handle_key(&mut self, key: KeyEvent) {
        // '?' toggles help from anywhere except inside Help itself or text-
        // entry / viewer screens (where '?' should be treated normally).
        if key.code == KeyCode::Char('?')
            && self.screen != Screen::Help
            && self.screen != Screen::PasswordPrompt
            && self.screen != Screen::KeyPassphrasePrompt
            && self.screen != Screen::NewSession
            && self.screen != Screen::EditSession
            && self.screen != Screen::SaveSession
            && self.screen != Screen::Search
            && self.screen != Screen::Rename
            && self.screen != Screen::Viewer
            && self.screen != Screen::ConfirmHostKey
            && self.screen != Screen::HostKeyChanged
        {
            self.previous_screen = self.screen.clone();
            self.screen = Screen::Help;
            return;
        }

        match self.screen {
            Screen::Help => {
                if matches!(key.code, KeyCode::Char('?') | KeyCode::Esc) {
                    self.screen = self.previous_screen.clone();
                }
            }
            Screen::ConfirmQuit => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.should_quit = true;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.screen = self.previous_screen.clone();
                }
                _ => {}
            },
            Screen::ConfirmCancel => self.handle_confirm_cancel(key),
            Screen::ConfirmDisconnect => self.handle_confirm_disconnect(key),
            Screen::ConfirmHostKey => self.handle_confirm_host_key(key),
            Screen::HostKeyChanged => {
                // Only explicit dismiss keys close this screen — `any key
                // dismisses` lets a fast typist hammer past the MITM warning
                // before they've read it. Make them stop and acknowledge.
                match key.code {
                    KeyCode::Enter
                    | KeyCode::Esc
                    | KeyCode::Char('q')
                    | KeyCode::Char('Q') => {
                        self.host_key_changed_info = None;
                        self.pending_session = None;
                        self.screen = Screen::SessionSelect;
                    }
                    _ => {}
                }
            }
            Screen::SessionSelect => self.handle_session_select(key),
            Screen::NewSession => self.handle_new_session(key),
            Screen::EditSession => self.handle_edit_session(key),
            Screen::ConfirmDeleteSession => self.handle_confirm_delete_session(key),
            Screen::PasswordPrompt => self.handle_password_prompt(key),
            Screen::KeyPassphrasePrompt => self.handle_key_passphrase_prompt(key),
            Screen::Connection => {
                if key.code == KeyCode::Esc {
                    self.pending_session = None;
                    self.pending_password = None;
                    self.screen = Screen::SessionSelect;
                    self.push_log(LogLevel::Info, "connect cancelled".into());
                }
            }
            Screen::Main => self.handle_main(key),
            Screen::Search => self.handle_search(key),
            Screen::SaveSession => self.handle_save_session(key),
            Screen::Rename => self.handle_rename(key),
            Screen::Mkdir => self.handle_mkdir(key),
            Screen::ConfirmDelete => self.handle_confirm_delete(key),
            Screen::ConfirmOverwrite => self.handle_confirm_overwrite(key),
            Screen::Viewer => self.handle_viewer(key),
        }
    }


    // -------------------------------------------------------------------
    // Disconnect (return to the session selector)
    // -------------------------------------------------------------------

    /// Tear down the connected session and return to the selector.
    ///
    /// Steps:
    ///   1. Take the dispatcher and shut it down in a detached task. The
    ///      shutdown signals the loop's atomic flag and waits for the loop
    ///      task to exit (~100ms in the worst case). In-flight workers are
    ///      orphaned and complete or fail naturally as the runtime tears
    ///      down their futures — same behavior as the run-loop teardown
    ///      path.
    ///   2. Drop the transport and clear caches: cached password, transfer
    ///      manager, current session, remote pane state.
    ///   3. Reset the local pane to a clean cursor and switch focus back
    ///      to it (the remote pane has nothing to show until the user
    ///      reconnects).
    ///   4. Refresh the saved-sessions list — the user may have edited or
    ///      deleted sessions while connected, and the selector should
    ///      reflect that on return.
    fn disconnect(&mut self) {
        // 1. Dispatcher: shut down off-thread so we don't block the UI loop.
        if let Some(d) = self.dispatcher.take() {
            tokio::spawn(async move {
                d.shutdown().await;
            });
        }

        // 2. Drop the transport. We don't `close().await` — that would
        //    require holding the UI loop and the underlying connection
        //    will be cleaned up by Drop / runtime teardown when the last
        //    reference goes out of scope. The protocol-level QUIT is best
        //    effort regardless.
        let _ = self.transport.take();

        let label = self
            .current_session
            .as_ref()
            .map(|s| format!("{}@{}:{}", s.username, s.host, s.port))
            .unwrap_or_else(|| "session".to_string());

        // 3. Clear connection-scoped state.
        self.transfer_manager = None;
        self.current_session = None;
        self.pending_password = None;
        self.pending_cancel = None;
        self.pending_overwrite = None;
        self.pending_delete = None;
        self.transfer_cursor = 0;
        self.bottom_pane = BottomPane::Log;
        self.remote = PaneState::empty();

        // 4. Reset local pane focus and refresh the sessions list.
        self.active_pane = Pane::Local;
        self.reload_sessions();
        if self.session_cursor >= self.sessions.len().max(1) {
            self.session_cursor = self.sessions.len().saturating_sub(1);
        }

        self.screen = Screen::SessionSelect;
        self.push_log(LogLevel::Info, format!("disconnected from {label}"));
    }

    // -------------------------------------------------------------------
    // Connect lifecycle
    // -------------------------------------------------------------------

    /// Spawn a connect task. The result lands as `AppEvent::Connected` /
    /// `AppEvent::ConnectFailed` / `AppEvent::ConnectKeyNeedsPassphrase` and
    /// is processed by [`handle_app_event`].
    fn start_connect(
        &mut self,
        session: Session,
        password: Option<zeroize::Zeroizing<String>>,
    ) {
        self.push_log(
            LogLevel::Info,
            format!(
                "connecting to {}@{}:{} via {}…",
                session.username,
                session.host,
                session.port,
                session.protocol.as_str()
            ),
        );
        self.screen = Screen::Connection;
        // Clone the app event sender so the transport's host-key handler can
        // send HostKeyUnknown / HostKeyChanged events back to the TUI.
        let tx = self.app_event_tx.clone();
        let tx_for_transport = self.app_event_tx.clone();
        tokio::spawn(async move {
            // Borrow as &str just for the duration of the connect; the
            // Zeroizing<String> is moved into this task and zeroes on drop
            // when the future returns.
            let pw_borrow = password.as_ref().map(|p| p.as_str());
            let result = match tokio::time::timeout(
                CONNECT_TIMEOUT,
                transport::open(&session, pw_borrow, tx_for_transport),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(crate::error::BlinkError::connect("connection timed out")),
            };
            let event = match result {
                Ok(t) => AppEvent::Connected(t),
                Err(crate::error::BlinkError::KeyNeedsPassphrase) => {
                    AppEvent::ConnectKeyNeedsPassphrase
                }
                Err(e) => AppEvent::ConnectFailed(e.to_string()),
            };
            let _ = tx.send(event);
        });
    }

    // -------------------------------------------------------------------
    // Logging
    // -------------------------------------------------------------------

    pub fn push_log(&mut self, level: LogLevel, message: String) {
        self.log.push_back(LogLine {
            time: chrono::Local::now(),
            level,
            message,
        });
        // VecDeque pop_front is O(1); the old Vec drain(0..n) shifted every
        // remaining element down by N on every overflow.
        while self.log.len() > 500 {
            self.log.pop_front();
        }
    }
}

/// Build the visible entries for the remote pane, prepending `..` unless the
/// path is already root.
fn build_remote_pane_entries(remote_entries: &[RemoteEntry], path: &str) -> Vec<PaneEntry> {
    let mut entries: Vec<PaneEntry> = remote_entries
        .iter()
        .map(|e| PaneEntry {
            name: e.name.clone(),
            is_dir: e.is_dir(),
            size: e.size,
            selected: false,
            previewable_image: !e.is_dir() && crate::preview::is_previewable_image(&e.name),
        })
        .collect();
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });

    let mut out = Vec::with_capacity(entries.len() + 1);
    if path != "/" {
        out.push(PaneEntry {
            name: "..".into(),
            is_dir: true,
            size: 0,
            selected: false,
            previewable_image: false,
        });
    }
    out.extend(entries);
    out
}

/// Pull a short display name from a TransferJob — the basename of its remote
/// path, falling back to the local file name if the remote is empty.
fn name_for_job(job: &TransferJob) -> String {
    let from_remote = job
        .remote_path
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(&job.remote_path);
    if !from_remote.is_empty() {
        return from_remote.to_string();
    }
    job.local_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| job.remote_path.clone())
}

/// Resolve a session's `local_dir` override into a usable filesystem path.
///
/// - `~` and `~/...` expand against the user's home directory
/// - The resolved path is checked for existence and dir-ness; missing or
///   non-directory paths return `None` so the caller can fall back / warn
///
/// Doesn't try to handle `~user/...` (different user's home) — that's a
/// shell convenience the path-resolution crate ecosystem doesn't agree on,
/// and not worth dragging in a dep for.
fn resolve_local_dir(raw: &std::path::Path) -> Option<std::path::PathBuf> {
    let raw_str = raw.to_string_lossy();
    let expanded = if let Some(rest) = raw_str.strip_prefix("~/") {
        let home = directories::UserDirs::new()?.home_dir().to_path_buf();
        home.join(rest)
    } else if raw_str == "~" {
        directories::UserDirs::new()?.home_dir().to_path_buf()
    } else {
        raw.to_path_buf()
    };
    if expanded.is_dir() {
        Some(expanded)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_buffer_does_not_reallocate_while_typing() {
        // The whole point of pre-sizing: a realloc copies the partial secret
        // into a fresh allocation and frees the old one without wiping it,
        // leaving fragments no later zeroize can reach. Assert the buffer
        // absorbs an unreasonably long credential without growing.
        let mut buf = credential_buffer();
        let capacity_before = buf.capacity();
        let ptr_before = buf.as_ptr();

        for _ in 0..CREDENTIAL_CAPACITY {
            buf.push('x');
        }

        assert_eq!(buf.capacity(), capacity_before, "buffer must not grow");
        assert_eq!(buf.as_ptr(), ptr_before, "buffer must not move");
    }

    #[test]
    fn zeroize_keeps_the_capacity_for_the_next_attempt() {
        // Abandon paths call `.zeroize()`, not `.clear()`. It must both empty
        // the buffer and leave the pre-sized allocation in place, or a retry
        // after a wrong password would start reallocating again.
        let mut buf = credential_buffer();
        buf.push_str("hunter2");
        let capacity_before = buf.capacity();

        buf.zeroize();

        assert!(buf.is_empty(), "zeroize must empty the buffer");
        assert_eq!(buf.capacity(), capacity_before, "capacity must survive");
    }
}
