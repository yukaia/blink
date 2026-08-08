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

use crate::checkpoint::{Checkpoint, CheckpointKind};
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
    /// Modal over Main: a connection that isn't backed by a saved session
    /// just came up — offer to persist it.
    ///
    /// `n` (and `blink connect`) deliberately connect ad-hoc, so nothing is
    /// written to disk. That is easy to mistake for a failed save when the
    /// selector you came from is titled "SAVED SESSIONS", so the offer is
    /// made once, at the point the connection has proven it works.
    OfferSaveSession,
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
///
/// Long-running work must take this per operation rather than holding it for
/// its whole duration — see [`crate::tui::plan::walk_remote`].
pub(crate) type SharedTransport = Arc<Mutex<Box<dyn Transport>>>;

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
    /// Whether `pending_session` came from a URL rather than a file on disk,
    /// i.e. connecting will not leave anything saved. Set alongside
    /// `pending_session` at every site that populates it, and consumed by the
    /// `Connected` handler to decide whether to offer persistence.
    pending_session_unsaved: bool,
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
    /// The new name as the user is typing it. Rendered in the modal, so it
    /// must never be seeded with unsanitized server bytes.
    pub rename_input: String,
    /// The current name, sanitized — this is the "from:" line in the modal.
    pub rename_original: String,
    /// The current name exactly as the server reported it. Never rendered;
    /// this is what the rename addresses on the wire.
    pub rename_source: String,
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
    /// Host keys accepted with "trust once" for the connection being opened
    /// (or the one currently up). Created per connect attempt and handed to
    /// the dispatcher so every worker connection shares the decision instead
    /// of re-prompting. Dropped on disconnect, which is the scope the prompt
    /// promises the user.
    pending_trust: crate::known_hosts::SessionTrust,
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
    /// The checkpoint being tracked for each direction, at most one per
    /// direction.
    ///
    /// Keyed by kind rather than held as a single slot: a download batch and
    /// an upload batch can be in flight at once, and a single slot meant the
    /// second one to start silently displaced the first — which stopped being
    /// updated and became unresumable while still running.
    active_checkpoints: std::collections::HashMap<CheckpointKind, Checkpoint>,
    /// Maps dispatcher job-id → the checkpoint entry it corresponds to, as
    /// `(kind, index)`, so the transfer-event handler can mark the right
    /// entry without a linear scan of the plan.
    checkpoint_job_map: std::collections::HashMap<u64, (CheckpointKind, usize)>,

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
    /// Whether [`Self::autoconnect`] was built from a URL (`blink connect`)
    /// rather than loaded from disk (`blink open`).
    autoconnect_unsaved: bool,
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
            pending_session_unsaved: false,
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
            rename_source: String::new(),
            rename_error: None,
            mkdir_input: String::new(),
            mkdir_error: None,
            pending_delete: None,
            pending_overwrite: None,
            app_event_tx: tx,
            app_event_rx: Some(rx),
            pending_password: None,
            pending_trust: crate::known_hosts::SessionTrust::new(),
            transfer_manager: None,
            dispatcher: None,
            pending_host_key: None,
            host_key_changed_info: None,
            active_checkpoints: std::collections::HashMap::new(),
            checkpoint_job_map: std::collections::HashMap::new(),
            viewer: None,
            image_needs_redraw: false,
            needs_terminal_clear: false,
            status_message: None,
            should_quit: false,
            autoconnect: None,
            autoconnect_unsaved: false,
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
    ///
    /// `unsaved` marks a session built from a URL (`blink connect`), which
    /// has no file behind it — the connect flow offers to persist those.
    pub fn with_session(
        config: Config,
        theme: Theme,
        session: Session,
        unsaved: bool,
    ) -> Self {
        let mut app = Self::new(config, theme);
        app.autoconnect = Some(session);
        app.autoconnect_unsaved = unsaved;
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
            self.pending_session_unsaved = self.autoconnect_unsaved;
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
            Screen::OfferSaveSession => {
                crate::tui::views::main::render(f, self);
                crate::tui::views::offer_save_session::render(f, self);
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
                // A transfer worker can raise this prompt while a session is
                // up. Drawing the selector behind it then reads as though the
                // connection had been dropped; draw whatever the prompt
                // actually interrupted.
                if self.transport.is_some() {
                    crate::tui::views::main::render(f, self);
                } else {
                    crate::tui::views::session_select::render(f, self);
                }
                crate::tui::views::confirm_host_key::render(f, self);
            }
            Screen::HostKeyChanged => {
                // Same reasoning as ConfirmHostKey: a worker can raise this
                // mid-session, and the connection is still up while the
                // warning is on screen — dismissing it is what tears the
                // session down. Draw what is actually behind the modal.
                if self.transport.is_some() {
                    crate::tui::views::main::render(f, self);
                } else {
                    crate::tui::views::session_select::render(f, self);
                }
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
                        // Every connection a session opens runs the host-key
                        // check, so a transfer worker can raise this while the
                        // user is connected. Returning to the selector while
                        // the transport and dispatcher kept running left the
                        // next connect to reassign `self.dispatcher` and
                        // orphan the old loop — and left a session up whose
                        // peer just failed to prove its identity. Tear it
                        // down; `disconnect` lands on the selector itself.
                        if self.transport.is_some() {
                            self.disconnect();
                        } else {
                            // Raised during the initial connect: nothing was
                            // established, so there is nothing to tear down
                            // and nothing to report as a disconnect.
                            self.pending_session = None;
                            self.screen = Screen::SessionSelect;
                        }
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
            Screen::OfferSaveSession => self.handle_offer_save_session(key),
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
        // Forget any "trust once" acceptance — it was scoped to the
        // connection we just tore down.
        self.pending_trust = crate::known_hosts::SessionTrust::new();
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
        // A fresh store per attempt: an accept-once from a previous session
        // must not silently carry into a new one.
        self.pending_trust = crate::known_hosts::SessionTrust::new();
        let trust = self.pending_trust.clone();
        tokio::spawn(async move {
            // Borrow as &str just for the duration of the connect; the
            // Zeroizing<String> is moved into this task and zeroes on drop
            // when the future returns.
            let pw_borrow = password.as_ref().map(|p| p.as_str());
            let result = match tokio::time::timeout(
                CONNECT_TIMEOUT,
                transport::open(&session, pw_borrow, tx_for_transport, trust),
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

    /// Append a line to the log pane.
    ///
    /// The message is sanitized here, at the one funnel every log line goes
    /// through, rather than at each of the ~20 call sites. Most of them
    /// interpolate a remote path or filename, and those now carry the
    /// server's own bytes verbatim (see [`crate::transport::RemoteEntry`]) —
    /// so without this a listing could inject escape sequences into the log
    /// pane. Sanitizing centrally means a new call site cannot forget.
    pub fn push_log(&mut self, level: LogLevel, message: String) {
        self.log.push_back(LogLine {
            time: chrono::Local::now(),
            level,
            message: crate::error::sanitize(message),
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
        .map(|e| PaneEntry::new(e.raw_name.clone(), e.is_dir(), e.size))
        .collect();
    // Sort on what the user reads, so the pane is ordered the way it looks.
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.display_name.cmp(&b.display_name),
    });

    let mut out = Vec::with_capacity(entries.len() + 1);
    if path != "/" {
        out.push(PaneEntry::parent());
    }
    out.extend(entries);
    out
}

/// Pull a short display name from a TransferJob — the basename of its remote
/// path, falling back to the local file name if the remote is empty.
///
/// Shared with the transfers pane renderer, which had a byte-identical copy
/// of this. Both needed the same sanitize fix when remote paths started
/// carrying the server's own bytes; had only one been found, half the UI
/// would still be rendering escape sequences from a listing.
pub(crate) fn name_for_job(job: &TransferJob) -> String {
    let from_remote = job
        .remote_path
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(&job.remote_path);
    let raw = if !from_remote.is_empty() {
        from_remote.to_string()
    } else {
        job.local_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| job.remote_path.clone())
    };
    // Server-controlled, and this ends up rendered in the cancel modal.
    crate::error::sanitize(raw)
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

    // -- offer-to-save flow ------------------------------------------------
    //
    // `n` and `blink connect` build a session from a URL and persist nothing,
    // which reads as "it connected but didn't save" when the selector you came
    // from is titled SAVED SESSIONS. These cover the offer that closes that
    // gap. The key handlers need no terminal — only `run` does — so the state
    // machine can be driven directly.

    use crate::transfer::{Direction, TransferEvent};

    fn app() -> App {
        App::new(Config::default(), Theme::load("dracula").unwrap())
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn connecting_from_a_url_marks_the_session_unsaved() {
        let mut a = app();
        a.screen = Screen::NewSession;
        a.new_session_input = "sftp://me@host.example.com".into();
        a.handle_new_session(press(KeyCode::Enter));

        assert!(
            a.pending_session_unsaved,
            "a URL-built session has no file behind it",
        );
        assert_eq!(a.screen, Screen::PasswordPrompt);
    }

    #[test]
    fn connecting_to_a_saved_session_does_not_mark_it_unsaved() {
        let mut a = app();
        a.sessions = vec![Session::from_url("sftp://me@saved.example.com").unwrap()];
        a.session_cursor = 0;
        // Poison the flag first, so the test proves it is reset rather than
        // merely observing the default.
        a.pending_session_unsaved = true;

        a.handle_session_select(press(KeyCode::Enter));

        assert!(
            !a.pending_session_unsaved,
            "a session picked from the saved list is already on disk",
        );
    }

    #[test]
    fn declining_the_offer_returns_to_main() {
        let mut a = app();
        a.current_session = Some(Session::from_url("sftp://me@host").unwrap());
        a.screen = Screen::OfferSaveSession;

        a.handle_offer_save_session(press(KeyCode::Char('n')));

        assert_eq!(a.screen, Screen::Main, "declining leaves the connection up");
        assert!(
            a.log.iter().any(|l| l.message.contains("ctrl+s")),
            "declining must point at the way to save later",
        );
    }

    #[test]
    fn accepting_the_offer_opens_the_save_modal() {
        let mut a = app();
        a.current_session = Some(Session::from_url("sftp://me@host.example.com").unwrap());
        a.screen = Screen::OfferSaveSession;

        a.handle_offer_save_session(press(KeyCode::Char('y')));

        assert_eq!(a.screen, Screen::SaveSession, "hands off to the save form");
        assert!(
            !a.save_session_input.is_empty(),
            "the save form should arrive pre-filled with a name",
        );
    }

    #[test]
    fn the_offer_ignores_keys_it_does_not_list() {
        // Matches every other confirm modal: unlisted keys do nothing rather
        // than being guessed at as a default, so Enter can't dismiss the
        // offer by accident.
        let mut a = app();
        a.current_session = Some(Session::from_url("sftp://me@host").unwrap());
        a.screen = Screen::OfferSaveSession;

        for code in [KeyCode::Enter, KeyCode::Char('x'), KeyCode::Tab] {
            a.handle_offer_save_session(press(code));
            assert_eq!(a.screen, Screen::OfferSaveSession, "{code:?} must not dismiss");
        }
    }

    #[test]
    fn the_offer_is_made_only_once_per_connect() {
        // The Connected handler consumes the flag with `mem::take`, so a
        // second connect that didn't set it can't inherit a stale offer.
        let mut a = app();
        a.pending_session_unsaved = true;
        let first = std::mem::take(&mut a.pending_session_unsaved);
        let second = std::mem::take(&mut a.pending_session_unsaved);
        assert!(first && !second);
    }

    // -- host-key modal ----------------------------------------------------
    //
    // A worker connection can raise this prompt while the user is connected
    // and working. It used to answer by jumping to the session selector
    // (reject) or the "connecting…" modal (accept) — both wrong for a
    // session that is already up, and the reject path dropped the user into
    // the selector with a live transport and a running dispatcher behind it.
    // The modal must return to whatever it interrupted.

    fn pending_host_key() -> PendingHostKey {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        PendingHostKey {
            host: "h".into(),
            key_type: "ssh-ed25519".into(),
            fingerprint: "SHA256:abc".into(),
            decision_tx: Some(tx),
        }
    }

    #[test]
    fn rejecting_a_host_key_returns_to_the_screen_it_interrupted() {
        let mut a = app();
        a.previous_screen = Screen::Main;
        a.screen = Screen::ConfirmHostKey;
        a.pending_host_key = Some(pending_host_key());

        a.handle_confirm_host_key(press(KeyCode::Char('n')));

        assert_eq!(
            a.screen,
            Screen::Main,
            "rejecting a worker's prompt must not abandon the live session",
        );
    }

    #[test]
    fn accepting_a_host_key_returns_to_the_screen_it_interrupted() {
        let mut a = app();
        a.previous_screen = Screen::Main;
        a.screen = Screen::ConfirmHostKey;
        a.pending_host_key = Some(pending_host_key());

        a.handle_confirm_host_key(press(KeyCode::Char('t')));

        assert_eq!(
            a.screen,
            Screen::Main,
            "accepting mid-session must not show the connecting modal",
        );
    }

    #[test]
    fn rejecting_during_the_initial_connect_leaves_the_connect_to_report_it() {
        // During the first connect the prompt interrupts the Connection
        // screen. Returning there lets the connect task fail and drive the
        // usual ConnectFailed path, which logs why — rather than silently
        // dropping the user in the selector.
        let mut a = app();
        a.previous_screen = Screen::Connection;
        a.screen = Screen::ConfirmHostKey;
        a.pending_session = Some(Session::from_url("sftp://me@host").unwrap());
        a.pending_host_key = Some(pending_host_key());

        a.handle_confirm_host_key(press(KeyCode::Char('n')));

        assert_eq!(a.screen, Screen::Connection);
        assert!(
            a.pending_session.is_some(),
            "ConnectFailed needs the pending session to report the failure",
        );
    }

    #[test]
    fn a_second_host_key_prompt_keeps_the_original_return_screen() {
        // Two prompts arriving back to back must not make ConfirmHostKey its
        // own return target — that would strand the user on the modal.
        let mut a = app();
        a.screen = Screen::Main;

        for _ in 0..2 {
            let (tx, _rx) = tokio::sync::oneshot::channel();
            a.handle_app_event(AppEvent::HostKeyUnknown {
                host: "h".into(),
                key_type: "ssh-ed25519".into(),
                fingerprint: "SHA256:abc".into(),
                decision_tx: tx,
            });
        }

        assert_eq!(a.screen, Screen::ConfirmHostKey);
        assert_eq!(
            a.previous_screen,
            Screen::Main,
            "the return screen must survive a second prompt",
        );
    }

    // -- transfer log lines ------------------------------------------------
    //
    // Every started job announced itself as "downloading:", whatever it was.

    fn app_with_manager() -> App {
        let mut a = app();
        a.transfer_manager = Some(TransferManager::new(1).0);
        a
    }

    fn last_log(a: &App) -> String {
        a.log.back().map(|l| l.message.clone()).unwrap_or_default()
    }

    #[test]
    fn a_started_download_says_downloading() {
        let mut a = app_with_manager();
        let id = a
            .transfer_manager
            .as_ref()
            .unwrap()
            .enqueue_download("/r/f.bin".into(), "/l/f.bin".into())
            .unwrap();
        a.handle_transfer_event(TransferEvent::Started(id));
        assert!(last_log(&a).starts_with("downloading: "), "{}", last_log(&a));
    }

    #[test]
    fn a_started_upload_does_not_say_downloading() {
        let mut a = app_with_manager();
        let id = a
            .transfer_manager
            .as_ref()
            .unwrap()
            .enqueue_upload("/l/f.bin".into(), "/r/f.bin".into())
            .unwrap();
        a.handle_transfer_event(TransferEvent::Started(id));
        let msg = last_log(&a);
        assert!(msg.starts_with("uploading: "), "{msg}");
    }

    #[test]
    fn a_started_mkdir_says_what_it_is() {
        let mut a = app_with_manager();
        let id = a
            .transfer_manager
            .as_ref()
            .unwrap()
            .enqueue_mkdir("/r/newdir".into())
            .unwrap();
        a.handle_transfer_event(TransferEvent::Started(id));
        let msg = last_log(&a);
        assert!(
            !msg.contains("downloading") && !msg.contains("uploading"),
            "a directory creation is neither: {msg}"
        );
        assert!(msg.contains("newdir"), "{msg}");
    }

    #[test]
    fn a_completed_mkdir_does_not_report_a_byte_count() {
        // "complete: /r/newdir (0 B)" reads as a failed transfer.
        let mut a = app_with_manager();
        let id = a
            .transfer_manager
            .as_ref()
            .unwrap()
            .enqueue_mkdir("/r/newdir".into())
            .unwrap();
        a.handle_transfer_event(TransferEvent::Complete(id));
        let msg = last_log(&a);
        assert!(!msg.contains(" B)"), "no size for a directory: {msg}");
    }

    #[test]
    fn a_completed_download_still_reports_its_size() {
        let mut a = app_with_manager();
        let m = a.transfer_manager.as_ref().unwrap().clone();
        let id = m.enqueue_download("/r/f.bin".into(), "/l/f.bin".into()).unwrap();
        m.update_progress(id, 2048, 2048, 0);
        a.handle_transfer_event(TransferEvent::Complete(id));
        let msg = last_log(&a);
        assert!(msg.contains("2 KiB"), "{msg}");
    }

    // -- checkpoints across concurrent batches ------------------------------
    //
    // Storage is one checkpoint per (session, direction). Queuing a second
    // batch used to write a fresh one over the top, so a batch that was still
    // running lost its plan — unresumable, and its unfinished downloads'
    // `.part` files unfindable, since the checkpoint is the only record of
    // where they are.

    /// An app whose checkpoints go under a name no real session will use.
    fn checkpoint_app(tag: &str) -> App {
        let mut a = app();
        let name = format!("blink-test-{tag}-{}", std::process::id());
        let mut s = Session::from_url("sftp://me@host").unwrap();
        s.name = name;
        a.current_session = Some(s);
        a.transfer_manager = Some(TransferManager::new(1).0);
        a
    }

    fn clean_checkpoints(a: &App) {
        if let Some(s) = a.current_session.as_ref() {
            let _ = crate::checkpoint::Checkpoint::remove(&s.name, CheckpointKind::Download);
            let _ = crate::checkpoint::Checkpoint::remove(&s.name, CheckpointKind::Upload);
        }
    }

    fn download(n: u32) -> crate::tui::plan::PlannedJob {
        crate::tui::plan::PlannedJob::Download {
            remote_path: format!("/r/{n}"),
            local_path: std::path::PathBuf::from(format!("/l/{n}")),
        }
    }

    fn upload(n: u32) -> crate::tui::plan::PlannedJob {
        crate::tui::plan::PlannedJob::Upload {
            local_path: std::path::PathBuf::from(format!("/l/{n}")),
            remote_path: format!("/r/{n}"),
        }
    }

    #[tokio::test]
    async fn a_second_batch_joins_the_first_instead_of_replacing_it() {
        let mut a = checkpoint_app("merge");

        a.dispatch_plan(vec![download(0), download(1)], Direction::Download);
        let first_ids: Vec<u64> = a.checkpoint_job_map.keys().copied().collect();
        assert_eq!(first_ids.len(), 2, "first batch tracked");

        a.dispatch_plan(vec![download(2)], Direction::Download);

        let cp = a
            .active_checkpoints
            .get(&CheckpointKind::Download)
            .expect("a download checkpoint must still be tracked");
        assert_eq!(cp.jobs.len(), 3, "both batches live in one checkpoint");
        assert_eq!(
            a.checkpoint_job_map.len(),
            3,
            "the first batch's jobs must still be mapped",
        );
        for id in first_ids {
            assert!(
                a.checkpoint_job_map.contains_key(&id),
                "job {id} from the first batch lost its checkpoint entry",
            );
        }

        // Every job maps to a distinct entry — an off-by-one in the append
        // offset would alias two jobs onto one.
        let mut idx: Vec<usize> = a.checkpoint_job_map.values().map(|(_, i)| *i).collect();
        idx.sort_unstable();
        idx.dedup();
        assert_eq!(idx.len(), 3, "checkpoint indices must be distinct");

        clean_checkpoints(&a);
    }

    // -- the completion path must not fsync per job ------------------------
    //
    // `settle_checkpoint` decides whether a state change reaches disk now or
    // is coalesced. An earlier draft of the per-batch work flushed
    // unconditionally, which put an fsync on every completed job — a batch
    // can be 100k of them. The suite was green with that in place; nothing
    // described the contract until these.

    #[tokio::test]
    async fn completing_a_job_coalesces_its_mark_instead_of_writing() {
        let mut a = checkpoint_app("debounce");
        a.dispatch_plan(
            vec![download(0), download(1), download(2)],
            Direction::Download,
        );
        // `dispatch_plan` persists the plan up front, so the checkpoint
        // starts clean and inside the debounce interval.
        let ids: Vec<u64> = a.checkpoint_job_map.keys().copied().collect();
        assert!(
            !a.active_checkpoints[&CheckpointKind::Download].is_dirty(),
            "the initial plan write is unconditional",
        );

        a.handle_transfer_event(TransferEvent::Complete(ids[0]));

        // Two jobs still pending, so this took the flush branch rather than
        // the drop-the-checkpoint branch. The mark must be held, not written.
        // (The interval is 250ms; these are two function calls.)
        assert!(
            a.active_checkpoints[&CheckpointKind::Download].is_dirty(),
            "a completed job forced a write instead of coalescing",
        );

        clean_checkpoints(&a);
    }

    #[tokio::test]
    async fn cancelling_a_batch_writes_immediately() {
        // The other side of the contract: a cancel is exactly the state the
        // user would lose by quitting straight afterwards, so it is forced
        // rather than coalesced.
        let mut a = checkpoint_app("force");
        a.dispatch_plan(
            vec![download(0), download(1), download(2)],
            Direction::Download,
        );
        let ids: Vec<u64> = a.checkpoint_job_map.keys().copied().collect();

        // Dirty the checkpoint, then cancel one job of the batch.
        a.handle_transfer_event(TransferEvent::Complete(ids[0]));
        assert!(a.active_checkpoints[&CheckpointKind::Download].is_dirty());

        a.cancel_batch_in_checkpoint(&ids[1..2]);

        assert!(
            !a.active_checkpoints[&CheckpointKind::Download].is_dirty(),
            "a cancel must reach disk rather than waiting out the interval",
        );

        clean_checkpoints(&a);
    }

    #[tokio::test]
    async fn upload_and_download_batches_keep_separate_checkpoints() {
        // A single slot meant an upload batch displaced a running download
        // batch, across directions as well as within one.
        let mut a = checkpoint_app("kinds");

        a.dispatch_plan(vec![download(0), download(1)], Direction::Download);
        a.dispatch_plan(vec![upload(0), upload(1)], Direction::Upload);

        assert_eq!(
            a.active_checkpoints.len(),
            2,
            "each direction tracks its own batch",
        );
        assert_eq!(a.checkpoint_job_map.len(), 4);

        clean_checkpoints(&a);
    }

    #[tokio::test]
    async fn cancelling_one_batch_leaves_the_other_direction_alone() {
        let mut a = checkpoint_app("cancel");

        a.dispatch_plan(vec![download(0), download(1)], Direction::Download);
        let dl_ids: Vec<u64> = a.checkpoint_job_map.keys().copied().collect();
        a.dispatch_plan(vec![upload(0)], Direction::Upload);

        a.cancel_batch_in_checkpoint(&dl_ids);

        assert!(
            a.active_checkpoints.contains_key(&CheckpointKind::Upload),
            "the upload batch must survive a download cancel",
        );
        assert!(
            !a.active_checkpoints.contains_key(&CheckpointKind::Download),
            "the cancelled download batch has nothing left to resume",
        );

        clean_checkpoints(&a);
    }

    // -- pane refresh ------------------------------------------------------

    #[tokio::test]
    async fn refreshing_the_remote_pane_keeps_showing_the_current_listing() {
        // Refreshing in place used to blank the pane and only repopulate when
        // the listing came back — which, behind a recursive walk holding the
        // connection, could be minutes.
        let mut a = app();
        a.transport = Some(Arc::new(Mutex::new(
            Box::new(crate::transport::mock::MockTransport::new()) as Box<dyn Transport>,
        )));
        a.remote.path = "/srv".into();
        a.remote.set_entries(vec![PaneEntry::new("a.txt".into(), false, 1)]);

        a.refresh_remote_pane("/srv".to_string());

        assert_eq!(
            a.remote.entries.len(),
            1,
            "an in-place refresh must not blank the pane while it waits",
        );
    }

    #[tokio::test]
    async fn navigating_the_remote_pane_drops_the_previous_listing() {
        // Navigation is different: the old directory's rows do not belong to
        // the new path, and acting on them would address the wrong files.
        let mut a = app();
        a.transport = Some(Arc::new(Mutex::new(
            Box::new(crate::transport::mock::MockTransport::new()) as Box<dyn Transport>,
        )));
        a.remote.path = "/srv".into();
        a.remote.set_entries(vec![PaneEntry::new("a.txt".into(), false, 1)]);

        a.refresh_remote_pane("/srv/sub".to_string());

        assert!(
            a.remote.entries.is_empty(),
            "stale rows must not survive a navigation",
        );
        assert_eq!(a.remote.cursor, 0);
    }

    // -- changed host key --------------------------------------------------
    //
    // A worker connection can raise this while a session is up. Dismissing it
    // returned to the selector but left the transport, manager and dispatcher
    // running behind it — so the next connect reassigned `App::dispatcher`
    // and orphaned the previous loop. A key that *changed* mid-session is
    // also the one case where the connection is definitely not trustworthy,
    // so tearing it down is right on its own merits.

    fn changed_info() -> HostKeyChangedInfo {
        HostKeyChangedInfo {
            host: "h".into(),
            lookup_host: "h".into(),
            lookup_port: 22,
            stored_key_type: "ssh-ed25519".into(),
            presented_key_type: "ssh-rsa".into(),
            fingerprint: "SHA256:abc".into(),
        }
    }

    #[tokio::test]
    async fn dismissing_a_changed_host_key_tears_down_a_live_connection() {
        let mut a = app();
        a.transport = Some(Arc::new(Mutex::new(
            Box::new(crate::transport::mock::MockTransport::new()) as Box<dyn Transport>,
        )));
        a.current_session = Some(Session::from_url("sftp://me@host").unwrap());
        a.transfer_manager = Some(TransferManager::new(2).0);
        a.screen = Screen::HostKeyChanged;
        a.host_key_changed_info = Some(changed_info());

        a.handle_key(press(KeyCode::Enter));

        assert!(
            a.transport.is_none(),
            "the connection must not outlive the mismatch warning",
        );
        assert!(a.transfer_manager.is_none(), "manager must be cleared too");
        assert!(a.current_session.is_none());
        assert_eq!(a.screen, Screen::SessionSelect);
        assert!(a.host_key_changed_info.is_none(), "modal state must clear");
    }

    #[tokio::test]
    async fn dismissing_a_changed_host_key_during_the_initial_connect_is_quiet() {
        // No connection was ever established, so there is nothing to tear
        // down and nothing to report as a disconnect.
        let mut a = app();
        a.pending_session = Some(Session::from_url("sftp://me@host").unwrap());
        a.screen = Screen::HostKeyChanged;
        a.host_key_changed_info = Some(changed_info());

        a.handle_key(press(KeyCode::Enter));

        assert_eq!(a.screen, Screen::SessionSelect);
        assert!(a.pending_session.is_none());
        assert!(
            !a.log.iter().any(|l| l.message.contains("disconnected from")),
            "nothing was connected, so nothing was disconnected",
        );
    }

    #[tokio::test]
    async fn the_changed_host_key_warning_still_ignores_stray_keys() {
        let mut a = app();
        a.screen = Screen::HostKeyChanged;
        a.host_key_changed_info = Some(changed_info());

        for code in [KeyCode::Char('y'), KeyCode::Char('x'), KeyCode::Tab] {
            a.handle_key(press(code));
            assert_eq!(
                a.screen,
                Screen::HostKeyChanged,
                "{code:?} must not dismiss a MITM warning",
            );
        }
    }

    // -- rename form -------------------------------------------------------

    fn connected_app_with_remote(entries: Vec<PaneEntry>) -> App {
        let mut a = app();
        a.remote.path = "/srv".into();
        a.remote.set_entries(entries);
        a.remote.cursor = 0;
        a.transport = Some(Arc::new(Mutex::new(
            Box::new(crate::transport::mock::MockTransport::new()) as Box<dyn Transport>,
        )));
        a
    }

    #[test]
    fn the_rename_form_shows_a_readable_name_but_renames_the_real_one() {
        // The form's contents are rendered straight into the modal, so they
        // must be the sanitized name. The rename itself still has to address
        // the file the server actually listed.
        let raw = "re\u{202E}port.txt";
        let mut a = connected_app_with_remote(vec![PaneEntry::new(raw.into(), false, 1)]);

        a.open_rename();

        assert_eq!(
            a.rename_original, "re port.txt",
            "the modal must not render the server's control characters",
        );
        assert!(
            !a.rename_input.contains('\u{202E}'),
            "the editable field is rendered too: {:?}",
            a.rename_input,
        );
        assert_eq!(
            a.rename_source, raw,
            "the rename source must be the name on the wire",
        );
    }

    #[test]
    fn an_ordinary_rename_prefills_the_current_name() {
        let mut a = connected_app_with_remote(vec![PaneEntry::new("notes.md".into(), false, 1)]);
        a.open_rename();
        assert_eq!(a.rename_input, "notes.md");
        assert_eq!(a.rename_source, "notes.md");
    }

    // -- credential buffers survive a submit -------------------------------
    //
    // `credential_buffer` pre-sizes so a per-keystroke `push` never
    // reallocates: a realloc copies the partial secret into a fresh
    // allocation and frees the old one unwiped, leaving fragments no later
    // zeroize can reach. Submitting with `mem::take` handed that allocation
    // away and left `String::default()` — capacity zero — behind, so the
    // protection covered the first attempt and nothing after it. A retry
    // after a wrong password is exactly when it matters.

    #[tokio::test]
    async fn submitting_a_password_leaves_the_buffer_pre_sized() {
        let mut a = app();
        a.pending_session = Some(Session::from_url("sftp://me@host").unwrap());
        a.password_input.push_str("hunter2");
        let capacity_before = a.password_input.capacity();

        a.handle_password_prompt(press(KeyCode::Enter));

        assert!(a.password_input.is_empty(), "buffer must be emptied");
        assert_eq!(
            a.password_input.capacity(),
            capacity_before,
            "the pre-sized allocation must survive for the retry",
        );
    }

    #[tokio::test]
    async fn submitting_a_passphrase_leaves_the_buffer_pre_sized() {
        let mut a = app();
        a.pending_session = Some(Session::from_url("sftp://me@host").unwrap());
        a.passphrase_input.push_str("correct horse");
        let capacity_before = a.passphrase_input.capacity();

        a.handle_key_passphrase_prompt(press(KeyCode::Enter));

        assert!(a.passphrase_input.is_empty());
        assert_eq!(a.passphrase_input.capacity(), capacity_before);
    }

    #[tokio::test]
    async fn retyping_a_password_after_a_failed_attempt_does_not_reallocate() {
        // The end-to-end property: submit, get rejected, type again. The
        // second attempt must not grow the buffer, or it leaves the same
        // heap fragments the pre-sizing exists to prevent.
        let mut a = app();
        a.pending_session = Some(Session::from_url("sftp://me@host").unwrap());
        a.password_input.push_str("wrong");
        a.handle_password_prompt(press(KeyCode::Enter));

        let capacity_before = a.password_input.capacity();
        let ptr_before = a.password_input.as_ptr();
        for _ in 0..CREDENTIAL_CAPACITY {
            a.password_input.push('x');
        }

        assert_eq!(a.password_input.capacity(), capacity_before, "buffer grew");
        assert_eq!(a.password_input.as_ptr(), ptr_before, "buffer moved");
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
