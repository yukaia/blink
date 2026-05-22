//! App state machine and main run loop.
//!
//! Background work (connect, list, …) is dispatched via tokio tasks that send
//! [`AppEvent`]s back through a channel. The run loop selects over keyboard
//! input, ticks, and these app events.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::Frame;
use tokio::sync::{mpsc, Mutex};

use crate::checkpoint::Checkpoint;
use crate::config::Config;
use crate::error::Result;
use crate::preview::{self, FileViewKind};
use crate::session::{AuthMethod, Session};
use crate::theme::Theme;
use crate::transfer::{Dispatcher, TransferJob, TransferManager};
use crate::transport::{self, RemoteEntry, Transport};
use crate::tui::event::{AppEvent, Event, EventStream};
use crate::tui::state::{
    EditSessionForm, HostKeyChangedInfo, OverwritePending, PaneEntry, PaneState, PendingCancel,
    PendingDelete, PendingHostKey, ViewSource, Viewer, ViewerKind,
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
    pub password_input: String,

    // SSH key passphrase prompt
    pub passphrase_input: String,
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
    pub log: Vec<LogLine>,
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
}

mod checkpoint_glue;
mod events;
mod handlers;
mod transfers;

impl App {
    pub fn new(config: Config, theme: Theme) -> Self {
        let sessions = Session::list_all().unwrap_or_default();
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
            password_input: String::new(),
            passphrase_input: String::new(),
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
            log: Vec::new(),
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

        // Autoconnect: `blink open` / `blink connect` pre-populate this
        // field. We trigger the connect here, after the runtime is live and
        // the event channel is wired, so `start_connect`'s tokio::spawn lands
        // in the right context.
        if let Some(session) = self.autoconnect.take() {
            match &session.auth {
                AuthMethod::Password => {
                    self.pending_session = Some(session);
                    self.password_input.clear();
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
    // Edit / delete saved sessions
    // -------------------------------------------------------------------

    fn open_edit_session(&mut self) {
        let Some(s) = self.sessions.get(self.session_cursor).cloned() else {
            return;
        };
        self.edit_session_form = Some(EditSessionForm::from_session(&s));
        self.screen = Screen::EditSession;
    }

    fn submit_edit_session(&mut self) {
        // Validate. Pull the form into a local so we can borrow self elsewhere.
        let mut form = match self.edit_session_form.take() {
            Some(f) => f,
            None => {
                self.screen = Screen::SessionSelect;
                return;
            }
        };

        let name = form.name.trim().to_string();
        if name.is_empty() {
            form.error = Some("name cannot be empty".into());
            self.edit_session_form = Some(form);
            return;
        }
        let host = form.host.trim().to_string();
        if host.is_empty() {
            form.error = Some("host cannot be empty".into());
            self.edit_session_form = Some(form);
            return;
        }
        let port: u16 = match form.port.trim().parse() {
            Ok(p) if p >= 1 => p,
            _ => {
                form.error = Some("port must be a number 1–65535".into());
                self.edit_session_form = Some(form);
                return;
            }
        };
        // Renaming to a name another session already uses would silently
        // clobber the other one — block that explicitly.
        if name != form.original_name
            && self.sessions.iter().any(|s| s.name == name)
        {
            form.error = Some(format!("a session named `{name}` already exists"));
            self.edit_session_form = Some(form);
            return;
        }

        // Look up the underlying session so we keep protocol / auth / theme
        // overrides intact (those aren't editable here).
        let Some(original) = self
            .sessions
            .iter()
            .find(|s| s.name == form.original_name)
            .cloned()
        else {
            form.error = Some("original session not found".into());
            self.edit_session_form = Some(form);
            return;
        };

        let local_dir_str = form.local_dir.trim();
        let local_dir = if local_dir_str.is_empty() {
            None
        } else {
            Some(std::path::PathBuf::from(local_dir_str))
        };

        // Parallel transfers override. Empty -> None (use global default).
        // Otherwise must be 1..=MAX_PARALLEL; the spec caps at 10. Echo
        // bad input back without losing the rest of the form.
        let parallel_str = form.parallel.trim();
        let parallel_downloads: Option<u8> = if parallel_str.is_empty() {
            None
        } else {
            match parallel_str.parse::<u16>() {
                Ok(n) if n >= 1 && n <= u16::from(crate::config::MAX_PARALLEL) => {
                    Some(n as u8)
                }
                _ => {
                    form.error = Some(format!(
                        "parallel must be a number 1–{} (or empty for default)",
                        crate::config::MAX_PARALLEL
                    ));
                    self.edit_session_form = Some(form);
                    return;
                }
            }
        };

        // Preserve the pinned cert hash only when host, port, and the trust
        // bypass are all unchanged. A change to any of those means the
        // previous pin is no longer meaningful (different target or the
        // user is switching to normal CA verification), so we clear it and
        // let the next connect TOFU.
        let cert_sha256 = if form.accept_invalid_certs
            && original.host == host
            && original.port == port
        {
            original.cert_sha256.clone()
        } else {
            None
        };

        let updated = Session {
            name: name.clone(),
            protocol: original.protocol.clone(),
            host,
            port,
            username: form.username.trim().to_string(),
            remote_dir: if form.remote_dir.trim().is_empty() {
                "/".to_string()
            } else {
                form.remote_dir.trim().to_string()
            },
            local_dir,
            auth: original.auth.clone(),
            parallel_downloads,
            theme: original.theme.clone(),
            accept_invalid_certs: form.accept_invalid_certs,
            cert_sha256,
        };

        match updated.save() {
            Ok(()) => {
                // If the rename succeeded, drop the old `.ini` file. We do
                // this AFTER save so a save failure doesn't lose the original.
                if name != form.original_name {
                    if let Err(e) = Session::delete(&form.original_name) {
                        // Soft-failure: the new session is saved, but the old
                        // file stayed behind. Surface as a warn rather than
                        // failing the whole edit.
                        self.push_log(
                            LogLevel::Warn,
                            format!(
                                "renamed session saved, but failed to remove old file: {e}"
                            ),
                        );
                    }
                }
                self.sessions = Session::list_all().unwrap_or_default();
                // Keep the cursor pointed at the freshly-edited session if
                // we can find it, otherwise clamp.
                self.session_cursor = self
                    .sessions
                    .iter()
                    .position(|s| s.name == name)
                    .unwrap_or_else(|| self.session_cursor.min(self.sessions.len().saturating_sub(1)));
                self.edit_session_form = None;
                self.screen = Screen::SessionSelect;
                self.push_log(LogLevel::Success, format!("session updated: {name}"));
            }
            Err(e) => {
                form.error = Some(e.to_string());
                self.edit_session_form = Some(form);
            }
        }
    }

    fn open_delete_session(&mut self) {
        let Some(s) = self.sessions.get(self.session_cursor).cloned() else {
            return;
        };
        self.pending_session_delete = Some(s);
        self.screen = Screen::ConfirmDeleteSession;
    }

    /// Cycle the active pane forward (`forward = true`) or backward through
    /// Local → Remote → Transfers → Log → Local. Updates `bottom_pane` so the
    /// bottom panel displays whichever bottom page is now focused.
    fn cycle_pane(&mut self, forward: bool) {
        let next = if forward {
            match self.active_pane {
                Pane::Local => Pane::Remote,
                Pane::Remote => Pane::Transfers,
                Pane::Transfers => Pane::Log,
                Pane::Log => Pane::Local,
            }
        } else {
            match self.active_pane {
                Pane::Local => Pane::Log,
                Pane::Log => Pane::Transfers,
                Pane::Transfers => Pane::Remote,
                Pane::Remote => Pane::Local,
            }
        };
        self.active_pane = next;
        match next {
            Pane::Transfers => self.bottom_pane = BottomPane::Transfers,
            Pane::Log => self.bottom_pane = BottomPane::Log,
            _ => {}
        }
    }

    fn move_transfer_cursor(&mut self, delta: isize) {
        let len = self.active_jobs().len();
        if len == 0 {
            self.transfer_cursor = 0;
            return;
        }
        let max = len - 1;
        let mut next = self.transfer_cursor as isize + delta;
        if next < 0 {
            next = 0;
        }
        if next as usize > max {
            next = max as isize;
        }
        self.transfer_cursor = next as usize;
    }

    fn request_cancel_selected_transfer(&mut self) {
        let jobs = self.active_jobs();
        if jobs.is_empty() {
            return;
        }
        let idx = self.transfer_cursor.min(jobs.len() - 1);
        let job = &jobs[idx];
        self.pending_cancel = Some(PendingCancel::Single {
            id: job.id,
            name: name_for_job(job),
        });
        self.previous_screen = Screen::Main;
        self.screen = Screen::ConfirmCancel;
    }

    /// Cancel every job in the batch the cursor item belongs to. If the
    /// cursor job has no batch_id (single-file enqueue), this falls back
    /// to the single-job cancel modal.
    fn request_cancel_selected_batch(&mut self) {
        let manager = match self.transfer_manager.as_ref() {
            Some(m) => m,
            None => return,
        };
        let active_jobs = self.active_jobs();
        if active_jobs.is_empty() {
            return;
        }
        let idx = self.transfer_cursor.min(active_jobs.len() - 1);
        let cursor_job = &active_jobs[idx];

        let Some(batch_id) = cursor_job.batch_id else {
            // Not a batched job — fall through to the single-job modal so
            // the gesture still does something useful.
            self.request_cancel_selected_transfer();
            return;
        };

        // Count siblings in the batch, including pending ones (which the
        // active-only list above doesn't include).
        let snapshot = manager.snapshot();
        let active = snapshot
            .iter()
            .filter(|j| {
                j.batch_id == Some(batch_id)
                    && matches!(j.state, crate::transfer::TransferState::Active)
            })
            .count();
        let pending = snapshot
            .iter()
            .filter(|j| {
                j.batch_id == Some(batch_id)
                    && matches!(j.state, crate::transfer::TransferState::Pending)
            })
            .count();
        if active == 0 && pending == 0 {
            return;
        }

        self.pending_cancel = Some(PendingCancel::Batch {
            batch_id,
            active,
            pending,
            cursor_name: name_for_job(cursor_job),
        });
        self.previous_screen = Screen::Main;
        self.screen = Screen::ConfirmCancel;
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
        self.sessions = Session::list_all().unwrap_or_default();
        if self.session_cursor >= self.sessions.len().max(1) {
            self.session_cursor = self.sessions.len().saturating_sub(1);
        }

        self.screen = Screen::SessionSelect;
        self.push_log(LogLevel::Info, format!("disconnected from {label}"));
    }

    // -------------------------------------------------------------------
    // Rename
    // -------------------------------------------------------------------

    fn open_rename(&mut self) {
        if self.transport.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }
        let entry = match self.remote.entries.get(self.remote.cursor) {
            Some(e) if e.name != ".." => e.clone(),
            _ => return,
        };
        self.rename_input = entry.name.clone();
        self.rename_original = entry.name;
        self.rename_error = None;
        self.screen = Screen::Rename;
    }

    fn submit_rename(&mut self) {
        let new_name = self.rename_input.trim().to_string();
        if new_name.is_empty() {
            self.rename_error = Some("name cannot be empty".into());
            return;
        }
        if new_name.contains('/') || new_name.contains('\\') {
            self.rename_error = Some("name cannot contain path separators".into());
            return;
        }
        if new_name == self.rename_original {
            // No-op rename — just close.
            self.rename_input.clear();
            self.rename_original.clear();
            self.screen = Screen::Main;
            return;
        }

        let from = transport::join_remote(&self.remote.path, &self.rename_original);
        let to = transport::join_remote(&self.remote.path, &new_name);

        // Collision check against the cached pane listing. If the user navigated
        // here recently the cache is fresh enough; in the rare case it's stale,
        // the server will reject the rename and we'll surface the error.
        let collides = self
            .remote
            .entries
            .iter()
            .any(|e| e.name != ".." && e.name == new_name);
        if collides {
            self.pending_overwrite = Some(OverwritePending::Rename {
                from,
                to,
                target_name: new_name,
            });
            self.screen = Screen::ConfirmOverwrite;
            return;
        }

        self.rename_input.clear();
        self.rename_original.clear();
        self.screen = Screen::Main;
        self.start_rename(from, to);
    }

    fn start_rename(&mut self, from: String, to: String) {
        let Some(t) = self.transport.clone() else {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        };
        let tx = self.app_event_tx.clone();
        let from_label = from
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or(&from)
            .to_string();
        self.push_log(LogLevel::Info, format!("renaming: {from_label}"));
        tokio::spawn(async move {
            let mut transport = t.lock().await;
            let event = match transport.rename(&from, &to).await {
                Ok(()) => AppEvent::Renamed { from, to },
                Err(e) => AppEvent::RenameFailed {
                    from,
                    to,
                    error: e.to_string(),
                },
            };
            let _ = tx.send(event);
        });
    }

    // -------------------------------------------------------------------
    // Mkdir
    // -------------------------------------------------------------------

    fn open_mkdir(&mut self) {
        if self.transport.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }
        self.mkdir_input.clear();
        self.mkdir_error = None;
        self.screen = Screen::Mkdir;
    }

    fn submit_mkdir(&mut self) {
        let name = self.mkdir_input.trim().to_string();
        if name.is_empty() {
            self.mkdir_error = Some("name cannot be empty".into());
            return;
        }
        if name.contains('/') || name.contains('\\') {
            self.mkdir_error = Some("name cannot contain path separators".into());
            return;
        }
        if name == "." || name == ".." {
            self.mkdir_error = Some("invalid directory name".into());
            return;
        }
        let path = transport::join_remote(&self.remote.path, &name);
        self.mkdir_input.clear();
        self.mkdir_error = None;
        self.screen = Screen::Main;
        self.start_mkdir(path);
    }

    fn start_mkdir(&mut self, path: String) {
        let Some(t) = self.transport.clone() else {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        };
        let tx = self.app_event_tx.clone();
        let label = path
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or(&path)
            .to_string();
        self.push_log(LogLevel::Info, format!("mkdir: {label}"));
        tokio::spawn(async move {
            let mut transport = t.lock().await;
            let event = match transport.mkdir(&path).await {
                Ok(()) => AppEvent::MkdirDone { path },
                Err(e) => AppEvent::MkdirFailed {
                    path,
                    error: e.to_string(),
                },
            };
            let _ = tx.send(event);
        });
    }

    // -------------------------------------------------------------------
    // Delete
    // -------------------------------------------------------------------

    fn open_delete(&mut self) {
        if self.transport.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }
        let entry = match self.remote.entries.get(self.remote.cursor) {
            Some(e) if e.name != ".." => e.clone(),
            _ => return,
        };
        let remote_path = transport::join_remote(&self.remote.path, &entry.name);
        self.pending_delete = Some(PendingDelete {
            name: entry.name,
            is_dir: entry.is_dir,
            remote_path,
        });
        self.screen = Screen::ConfirmDelete;
    }

    fn start_delete(&mut self, name: String, remote_path: String, is_dir: bool) {
        let Some(t) = self.transport.clone() else {
            return;
        };
        let tx = self.app_event_tx.clone();
        let label = if is_dir { "deleting folder" } else { "deleting" };
        self.push_log(LogLevel::Info, format!("{label}: {name}"));
        tokio::spawn(async move {
            let mut transport = t.lock().await;
            let result = if is_dir {
                // Recursive: the user confirmed via the modal. Empty-only
                // deletion would leave them stuck on a non-empty folder
                // with no obvious next step.
                transport.delete_dir(&remote_path, true).await
            } else {
                transport.delete_file(&remote_path).await
            };
            let event = match result {
                Ok(()) => AppEvent::Deleted { name },
                Err(e) => AppEvent::DeleteFailed {
                    name,
                    error: e.to_string(),
                },
            };
            let _ = tx.send(event);
        });
    }

    // -------------------------------------------------------------------
    // Upload
    // -------------------------------------------------------------------

    /// Enqueue uploads for the selected items in the local pane. If nothing
    /// is selected, falls back to the cursor item.
    ///
    /// Detects collisions against the cached remote listing and, if any are
    /// found, prompts for overwrite confirmation before enqueueing. With no
    /// collisions the jobs go straight to the dispatcher.
    fn active_pane_mut(&mut self) -> Option<&mut PaneState> {
        match self.active_pane {
            Pane::Local => Some(&mut self.local),
            Pane::Remote => Some(&mut self.remote),
            Pane::Transfers | Pane::Log => None,
        }
    }

    // -------------------------------------------------------------------
    // Transfer dispatcher integration
    // -------------------------------------------------------------------

    fn toggle_pause(&mut self) {
        let Some(manager) = &self.transfer_manager else {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        };
        if manager.is_paused() {
            manager.resume();
        } else {
            manager.pause();
        }
        // The Paused / Resumed log lines are emitted from
        // handle_transfer_event when the dispatcher echoes the event back.
    }

    /// Cycle to the next theme in `Theme::list_all_names`. Built-ins come
    /// first alphabetically, then user themes (deduplicated by name). The
    /// new theme is applied immediately and persisted to `config.ini` so
    /// it survives a restart.
    fn cycle_theme(&mut self) {
        let names = Theme::list_all_names();
        if names.is_empty() {
            return;
        }
        // Find current; if not in the list (shouldn't happen, but defend
        // against it), start from -1 so the first cycle lands on index 0.
        let current_idx = names
            .iter()
            .position(|n| n == &self.theme.name)
            .map(|i| i as isize)
            .unwrap_or(-1);
        let next_idx = ((current_idx + 1) as usize) % names.len();
        let next_name = &names[next_idx];

        match Theme::load(next_name) {
            Ok(theme) => {
                self.config.general.theme = theme.name.clone();
                self.theme = theme;
                self.push_log(
                    LogLevel::Info,
                    format!("theme: {} ({}/{})",
                        next_name,
                        next_idx + 1,
                        names.len()),
                );
                // Persist as a best-effort. A save failure here shouldn't
                // refuse the in-memory swap (the user can see the new theme
                // already), so we log and move on.
                if let Err(e) = self.config.save() {
                    self.push_log(
                        LogLevel::Warn,
                        format!("could not save theme preference: {e}"),
                    );
                }
            }
            Err(e) => {
                // Should be rare since list_all_names already probed parse-
                // ability, but a TOCTOU between probe and load is possible.
                self.push_log(
                    LogLevel::Error,
                    format!("theme {next_name} failed to load: {e}"),
                );
            }
        }
    }

    /// Snapshot of currently-running jobs, for the transfer strip.
    pub fn active_jobs(&self) -> Vec<TransferJob> {
        self.transfer_manager
            .as_ref()
            .map(|m| {
                m.snapshot()
                    .into_iter()
                    .filter(|j| j.state == crate::transfer::TransferState::Active)
                    .collect()
            })
            .unwrap_or_default()
    }

    // -------------------------------------------------------------------
    // Search (substring filter on Local or Remote)
    // -------------------------------------------------------------------

    fn open_search(&mut self) {
        self.search_target = self.active_pane;
        // Pre-populate with the existing filter so re-opening shows what's
        // currently applied — easier to refine than to retype.
        let existing = match self.search_target {
            Pane::Local => self.local.filter.clone(),
            Pane::Remote => self.remote.filter.clone(),
            _ => None,
        };
        self.search_input = existing.unwrap_or_default();
        self.screen = Screen::Search;
    }

    fn apply_search_filter(&mut self) {
        let q = self.search_input.clone();
        match self.search_target {
            Pane::Local => self.local.set_filter(q),
            Pane::Remote => self.remote.set_filter(q),
            _ => {}
        }
    }

    fn move_search_cursor(&mut self, delta: isize) {
        match self.search_target {
            Pane::Local => self.local.move_cursor(delta),
            Pane::Remote => self.remote.move_cursor(delta),
            _ => {}
        }
    }

    // -------------------------------------------------------------------
    // Refresh
    // -------------------------------------------------------------------

    fn refresh_active_pane(&mut self) {
        match self.active_pane {
            Pane::Local => {
                self.refresh_local_pane();
                self.push_log(
                    LogLevel::Info,
                    format!("refreshed: {}", self.local.path),
                );
            }
            Pane::Remote => {
                if self.transport.is_some() {
                    let path = self.remote.path.clone();
                    self.refresh_remote_pane(path);
                } else {
                    self.push_log(LogLevel::Warn, "not connected".into());
                }
            }
            Pane::Transfers | Pane::Log => {
                // Nothing to refresh on these panes.
            }
        }
    }

    // -------------------------------------------------------------------
    // Save current session
    // -------------------------------------------------------------------

    fn open_save_session(&mut self) {
        if self.current_session.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }
        let default_name = self
            .current_session
            .as_ref()
            .map(|s| {
                if s.name.is_empty() {
                    s.host.clone()
                } else {
                    s.name.clone()
                }
            })
            .unwrap_or_default();
        self.save_session_input = default_name;
        self.save_session_error = None;
        self.screen = Screen::SaveSession;
    }

    // -------------------------------------------------------------------
    // Viewer
    // -------------------------------------------------------------------

    /// Handle 'v' on the main view: classify the cursor file, open the viewer
    /// modal in `Loading` state, and spawn the appropriate fetch task.
    fn handle_view_request(&mut self) {
        let (name, size, source) = match self.active_pane {
            Pane::Local => {
                let entry = match self.local.entries.get(self.local.cursor) {
                    Some(e) if !e.is_dir => e.clone(),
                    _ => return,
                };
                (entry.name.clone(), entry.size, ViewSource::Local)
            }
            Pane::Remote => {
                let entry = match self.remote.entries.get(self.remote.cursor) {
                    Some(e) if !e.is_dir => e.clone(),
                    _ => return,
                };
                (entry.name.clone(), entry.size, ViewSource::Remote)
            }
            Pane::Log | Pane::Transfers => return,
        };

        let kind = preview::detect_view_kind(&name, size);
        if let FileViewKind::Unsupported(reason) = &kind {
            self.push_log(
                LogLevel::Warn,
                format!("can't view {name}: {reason}"),
            );
            return;
        }

        // Open the modal in Loading state. Subsequent ViewLoaded / ViewFailed
        // events populate `kind`.
        self.viewer = Some(Viewer {
            name: name.clone(),
            kind: ViewerKind::Loading,
        });
        self.previous_screen = self.screen.clone();
        self.screen = Screen::Viewer;

        let tx = self.app_event_tx.clone();
        match source {
            ViewSource::Local => {
                let path = std::path::PathBuf::from(&self.local.path).join(&name);
                tokio::spawn(async move {
                    let event = match tokio::fs::read(&path).await {
                        Ok(buf) => AppEvent::ViewLoaded {
                            name,
                            kind,
                            bytes: Bytes::from(buf),
                        },
                        Err(e) => AppEvent::ViewFailed {
                            name,
                            error: e.to_string(),
                        },
                    };
                    let _ = tx.send(event);
                });
            }
            ViewSource::Remote => {
                let Some(t) = self.transport.clone() else {
                    self.viewer = None;
                    self.screen = self.previous_screen.clone();
                    return;
                };
                let remote_path = transport::join_remote(&self.remote.path, &name);
                tokio::spawn(async move {
                    let mut transport = t.lock().await;
                    let event = match transport.read_to_bytes(&remote_path).await {
                        Ok(bytes) => AppEvent::ViewLoaded { name, kind, bytes },
                        Err(e) => AppEvent::ViewFailed {
                            name,
                            error: e.to_string(),
                        },
                    };
                    let _ = tx.send(event);
                });
            }
        }
    }

    fn viewer_scroll(&mut self, delta: isize) {
        if let Some(viewer) = self.viewer.as_mut() {
            if let ViewerKind::Text { lines, scroll, .. } = &mut viewer.kind {
                let max = lines.len().saturating_sub(1);
                let next = (*scroll as isize + delta).max(0) as usize;
                *scroll = next.min(max);
            }
        }
    }

    fn viewer_scroll_to(&mut self, target: usize) {
        if let Some(viewer) = self.viewer.as_mut() {
            if let ViewerKind::Text { lines, scroll, .. } = &mut viewer.kind {
                let max = lines.len().saturating_sub(1);
                *scroll = target.min(max);
            }
        }
    }

    /// Called after each `terminal.draw` to emit graphics escape sequences
    /// for an active image viewer. Ratatui's diffing renderer leaves cells
    /// alone when their buffer contents don't change, so the image persists
    /// across ticks; we only need to re-emit on first open and on resize
    /// (both gated by `image_needs_redraw`).
    fn after_draw(&mut self, terminal: &mut TuiTerminal) -> std::io::Result<()> {
        if !self.image_needs_redraw {
            return Ok(());
        }
        let Some(viewer) = &self.viewer else {
            self.image_needs_redraw = false;
            return Ok(());
        };
        let ViewerKind::Image { bytes } = &viewer.kind else {
            self.image_needs_redraw = false;
            return Ok(());
        };

        let size = terminal.size()?;
        let full = Rect::new(0, 0, size.width, size.height);
        let modal = crate::tui::views::centered_rect(85, 85, full);

        // Match the layout in views::viewer::render: borders take 1 cell on
        // each side, then we reserve the bottom row of the inside for the
        // hint strip. The image draws into what's left.
        let body_x = modal.x.saturating_add(1);
        let body_y = modal.y.saturating_add(1);
        let body_w = modal.width.saturating_sub(2);
        let body_h = modal.height.saturating_sub(2).saturating_sub(1);
        if body_w == 0 || body_h == 0 {
            self.image_needs_redraw = false;
            return Ok(());
        }

        let proto = preview::detect(self.config.terminal.image_preview);
        let backend = match preview::backend_for(proto) {
            Some(b) => b,
            None => {
                self.image_needs_redraw = false;
                return Ok(());
            }
        };

        let escape = match backend.render(bytes, body_x, body_y, body_w, body_h) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.push_log(LogLevel::Warn, format!("image preview: {e}"));
                self.image_needs_redraw = false;
                return Ok(());
            }
        };

        use std::io::Write;
        let mut stdout = std::io::stdout();
        stdout.write_all(&escape)?;
        stdout.flush()?;
        self.image_needs_redraw = false;
        let _ = terminal; // not needed beyond the size() call
        Ok(())
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
    // Pane navigation
    // -------------------------------------------------------------------

    fn local_enter(&mut self) {
        let Some(entry) = self.local.entries.get(self.local.cursor) else {
            return;
        };
        if !entry.is_dir {
            return;
        }
        let mut path = std::path::PathBuf::from(&self.local.path);
        if entry.name == ".." {
            path.pop();
        } else {
            path.push(&entry.name);
        }
        self.local.path = path.display().to_string();
        self.local.cursor = 0;
        self.refresh_local_pane();
    }

    fn remote_enter(&mut self) {
        let Some(entry) = self.remote.entries.get(self.remote.cursor) else {
            return;
        };
        if !entry.is_dir {
            return;
        }
        let new_path = if entry.name == ".." {
            transport::parent_remote(&self.remote.path)
        } else {
            transport::join_remote(&self.remote.path, &entry.name)
        };
        self.refresh_remote_pane(new_path);
    }

    /// Kick off a remote `list` task. The result arrives as `AppEvent::Listed`.
    fn refresh_remote_pane(&mut self, path: String) {
        let Some(t) = self.transport.clone() else {
            return;
        };
        let path_changed = path != self.remote.path;
        // Reflect the new path immediately so the UI shows where we're going,
        // and so the stale-guard in handle_app_event can compare against it.
        self.remote.path = path.clone();
        self.remote.entries.clear();
        if path_changed {
            // Navigation: drop into the new dir at the top.
            self.remote.cursor = 0;
        }
        let tx = self.app_event_tx.clone();

        tokio::spawn(async move {
            let mut transport = t.lock().await;
            let event = match transport.list(&path).await {
                Ok(entries) => AppEvent::Listed { path, entries },
                Err(e) => AppEvent::ListFailed {
                    path,
                    error: e.to_string(),
                },
            };
            let _ = tx.send(event);
        });
    }

    /// Kick off a local `read_dir` task. The result arrives as
    /// `AppEvent::LocalListed` / `LocalListFailed`.
    ///
    /// Why async: `read_dir` + per-entry `metadata()` is fast on a local
    /// SSD but can stall for seconds on an NFS, SMB, or sshfs mount.
    /// Doing it inline on the UI thread froze the entire event loop —
    /// keystrokes, redraws, transfer-event drain, the lot. Now the work
    /// runs on tokio's blocking pool and the UI keeps ticking.
    fn refresh_local_pane(&mut self) {
        let path = self.local.path.clone();
        // Show just `..` immediately so the user sees the navigation
        // landed even before the read_dir finishes.
        self.local.set_entries(vec![PaneEntry {
            name: "..".into(),
            is_dir: true,
            size: 0,
            selected: false,
            previewable_image: false,
        }]);

        let tx = self.app_event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let pb = std::path::PathBuf::from(&path);
            let mut entries = vec![PaneEntry {
                name: "..".into(),
                is_dir: true,
                size: 0,
                selected: false,
                previewable_image: false,
            }];
            let read = match std::fs::read_dir(&pb) {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(AppEvent::LocalListFailed {
                        path,
                        error: e.to_string(),
                    });
                    return;
                }
            };
            for entry in read.flatten() {
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let name = entry.file_name().to_string_lossy().to_string();
                let is_dir = meta.is_dir();
                entries.push(PaneEntry {
                    previewable_image: !is_dir
                        && crate::preview::is_previewable_image(&name),
                    name,
                    is_dir,
                    size: if is_dir { 0 } else { meta.len() },
                    selected: false,
                });
            }
            // Directories first, then alpha within each group.
            entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.name.cmp(&b.name),
            });
            let _ = tx.send(AppEvent::LocalListed { path, entries });
        });
    }

    // -------------------------------------------------------------------
    // Logging
    // -------------------------------------------------------------------

    pub fn push_log(&mut self, level: LogLevel, message: String) {
        self.log.push(LogLine {
            time: chrono::Local::now(),
            level,
            message,
        });
        if self.log.len() > 500 {
            let drop_n = self.log.len() - 500;
            self.log.drain(0..drop_n);
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

/// Tokenise every line of a file once, at view-load time, so per-frame
/// rendering becomes an array lookup instead of replaying the highlighter
/// from line 0. The viewer redraws on the 100 ms TUI tick — without this
/// cache, a 10k-line file scrolled to the bottom does ~10k tokenize calls
/// per frame just to reach the visible region.
fn tokenize_lines(
    name: &str,
    lines: &[String],
) -> Vec<Vec<(crate::highlight::TokenKind, String)>> {
    let lang = crate::highlight::lang_for_name(name);
    let mut state = crate::highlight::LineState::default();
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let (tokens, ns) = crate::highlight::tokenize(lang, line, state);
        state = ns;
        out.push(tokens);
    }
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
