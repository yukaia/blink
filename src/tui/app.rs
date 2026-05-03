//! App state machine and main run loop.
//!
//! Background work (connect, list, …) is dispatched via tokio tasks that send
//! [`AppEvent`]s back through a channel. The run loop selects over keyboard
//! input, ticks, and these app events.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::Frame;
use tokio::sync::{mpsc, Mutex};

use crate::checkpoint::{Checkpoint, CheckpointJob, CheckpointKind, JobStatus};
use crate::config::Config;
use crate::error::Result;
use crate::preview::{self, FileViewKind};
use crate::session::{AuthMethod, Session};
use crate::theme::Theme;
use crate::transfer::{format_bytes, Direction, Dispatcher, TransferEvent, TransferJob, TransferManager};
use crate::transport::{self, EntryKind, RemoteEntry, Transport};
use crate::tui::event::{AppEvent, Event, EventStream};
use crate::tui::{TuiTerminal, TICK_INTERVAL};

const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
pub struct PaneEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub selected: bool,
    pub previewable_image: bool,
}

#[derive(Debug, Clone)]
pub struct PaneState {
    pub path: String,
    /// Currently visible entries. When [`filter`] is set, this is the filtered
    /// subset; otherwise it's the full list.
    pub entries: Vec<PaneEntry>,
    pub cursor: usize,
    /// Active substring filter, if any. Case-insensitive match against
    /// `entry.name`. The `..` parent entry is always retained so the user can
    /// navigate out of a filtered view.
    pub filter: Option<String>,
    /// Full unfiltered list, stashed while a filter is active so we can
    /// restore on clear (and re-apply on refresh).
    all_entries: Option<Vec<PaneEntry>>,
}

impl PaneState {
    pub fn empty() -> Self {
        Self {
            path: String::new(),
            entries: Vec::new(),
            cursor: 0,
            filter: None,
            all_entries: None,
        }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.cursor = 0;
            return;
        }
        let len = self.entries.len() as isize;
        let mut next = self.cursor as isize + delta;
        if next < 0 {
            next = 0;
        }
        if next >= len {
            next = len - 1;
        }
        self.cursor = next as usize;
    }

    pub fn toggle_selected(&mut self) {
        if let Some(e) = self.entries.get_mut(self.cursor) {
            e.selected = !e.selected;
        }
    }

    /// Replace the underlying entry list. If a filter is active it gets
    /// re-applied against the new list, so refresh-while-filtered keeps the
    /// view narrow. Cursor is clamped to the new range.
    pub fn set_entries(&mut self, entries: Vec<PaneEntry>) {
        if let Some(query) = self.filter.clone() {
            let lower = query.to_ascii_lowercase();
            let filtered: Vec<PaneEntry> = entries
                .iter()
                .filter(|e| {
                    e.name == ".." || e.name.to_ascii_lowercase().contains(&lower)
                })
                .cloned()
                .collect();
            self.all_entries = Some(entries);
            self.entries = filtered;
        } else {
            self.entries = entries;
            self.all_entries = None;
        }
        self.clamp_cursor();
    }

    /// Apply or update the substring filter. Empty `query` clears.
    pub fn set_filter(&mut self, query: String) {
        if query.is_empty() {
            self.clear_filter();
            return;
        }
        if self.all_entries.is_none() {
            self.all_entries = Some(self.entries.clone());
        }
        let lower = query.to_ascii_lowercase();
        let all = self.all_entries.as_ref().unwrap();
        self.entries = all
            .iter()
            .filter(|e| {
                e.name == ".." || e.name.to_ascii_lowercase().contains(&lower)
            })
            .cloned()
            .collect();
        self.filter = Some(query);
        self.clamp_cursor();
    }

    pub fn clear_filter(&mut self) {
        if let Some(all) = self.all_entries.take() {
            self.entries = all;
        }
        self.filter = None;
        self.clamp_cursor();
    }

    fn clamp_cursor(&mut self) {
        if self.entries.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.entries.len() {
            self.cursor = self.entries.len() - 1;
        }
    }
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

/// State of the viewer modal.
#[derive(Debug)]
pub struct Viewer {
    pub name: String,
    pub kind: ViewerKind,
}

#[derive(Debug)]
pub enum ViewerKind {
    /// Fetch in flight.
    Loading,
    /// Decoded text, with the current scroll offset (top visible line).
    Text { lines: Vec<String>, scroll: usize },
    /// Raw image bytes, ready to be emitted by a [`crate::preview::PreviewBackend`].
    Image { bytes: Bytes },
    /// Anything we can't render: too big, unknown extension, fetch failed.
    Unsupported(String),
}

/// Where the viewer is fetching its data from.
#[derive(Debug, Clone, Copy)]
enum ViewSource {
    Local,
    Remote,
}

/// Snapshot of the transfer being confirmed for cancellation.
#[derive(Debug, Clone)]
pub enum PendingCancel {
    /// Cancel a single transfer by id.
    Single { id: u64, name: String },
    /// Cancel every job in a batch. `active` and `pending` are job counts
    /// at the moment the modal opened — the actual cancel re-counts at
    /// confirm time, so a brief race (a job completing between modal-open
    /// and confirm) just means the displayed numbers are slightly stale.
    Batch {
        batch_id: u64,
        active: usize,
        pending: usize,
        /// Display name of the cursor job, used to anchor the modal text
        /// to something the user recognises.
        cursor_name: String,
    },
}

/// One step in a recursive transfer plan, produced by [`walk_remote`] or
/// [`walk_local`] and consumed by `dispatch_plan`. The order in the produced
/// `Vec` matters: directory creations always precede the file transfers
/// inside them. The dispatcher's parallelism then takes over from there.
///
/// Files live as their own enum variant (rather than a single `Transfer` with
/// a `Direction`) so the upload/download split is type-checked at the call
/// site rather than discovered at dispatch time.
#[derive(Debug, Clone)]
pub enum PlannedJob {
    Mkdir { remote_path: String },
    Download { remote_path: String, local_path: std::path::PathBuf },
    Upload { local_path: std::path::PathBuf, remote_path: String },
}

/// Snapshot of the entry being confirmed for deletion.
#[derive(Debug, Clone)]
pub struct PendingDelete {
    pub name: String,
    pub is_dir: bool,
    pub remote_path: String,
}

/// State for the host-key confirmation modal.
///
/// Holds everything needed to render the prompt and to send the user's
/// decision back to the SFTP connect task via the one-shot channel.
pub struct PendingHostKey {
    pub host: String,
    pub key_type: String,
    pub key_b64: String,
    pub fingerprint: String,
    /// One-shot sender; consumed exactly once when the user decides.
    pub decision_tx: Option<tokio::sync::oneshot::Sender<crate::transport::sftp::HostKeyDecision>>,
}

/// State for the host-key-changed error modal.
#[derive(Debug, Clone)]
pub struct HostKeyChangedInfo {
    pub host: String,
    pub stored_key_type: String,
    pub presented_key_type: String,
    pub fingerprint: String,
}

/// Which field of [`EditSessionForm`] is currently focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditField {
    Name,
    Host,
    Port,
    Username,
    RemoteDir,
    LocalDir,
    /// Parallel transfers override. Empty string = use the global default.
    Parallel,
    /// Toggle for `accept_invalid_certs` on the session. Toggled with Space
    /// or Enter rather than text-typed; cycles through it like any other
    /// field with Tab / Up / Down.
    AcceptInvalidCerts,
}

impl EditField {
    fn next(self) -> Self {
        match self {
            Self::Name => Self::Host,
            Self::Host => Self::Port,
            Self::Port => Self::Username,
            Self::Username => Self::RemoteDir,
            Self::RemoteDir => Self::LocalDir,
            Self::LocalDir => Self::Parallel,
            Self::Parallel => Self::AcceptInvalidCerts,
            Self::AcceptInvalidCerts => Self::Name,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Name => Self::AcceptInvalidCerts,
            Self::Host => Self::Name,
            Self::Port => Self::Host,
            Self::Username => Self::Port,
            Self::RemoteDir => Self::Username,
            Self::LocalDir => Self::RemoteDir,
            Self::Parallel => Self::LocalDir,
            Self::AcceptInvalidCerts => Self::Parallel,
        }
    }

    /// Whether this field accepts text input. False for booleans (toggled
    /// with Space) — calling `current_value_mut` on those is meaningless.
    fn is_text_field(self) -> bool {
        !matches!(self, Self::AcceptInvalidCerts)
    }
}

/// State of the edit-session modal. Protocol and auth are intentionally
/// out of scope here — they're rare to change post-creation, and changing
/// auth method correctly (e.g., password → key) needs a full re-auth flow
/// beyond a text input. To change those, the user can delete + recreate.
#[derive(Debug, Clone)]
pub struct EditSessionForm {
    /// Name the session had on disk before this edit. Used to detect a
    /// rename so the old `.ini` file can be removed.
    pub original_name: String,

    pub name: String,
    pub host: String,
    pub port: String,
    pub username: String,
    pub remote_dir: String,
    /// Empty string = no override (use default).
    pub local_dir: String,
    /// Parallel transfers override as a string. Empty = no override (use the
    /// global config value). Stored as a string in the form so we can echo
    /// invalid input back to the user before parsing on submit.
    pub parallel: String,
    /// Skip TLS certificate validation. Toggled with Space; the rendered row
    /// shows `[x]` / `[ ]` and a red warning when it's on.
    pub accept_invalid_certs: bool,

    pub focused: EditField,
    pub error: Option<String>,
}

impl EditSessionForm {
    pub fn from_session(s: &Session) -> Self {
        Self {
            original_name: s.name.clone(),
            name: s.name.clone(),
            host: s.host.clone(),
            port: s.port.to_string(),
            username: s.username.clone(),
            remote_dir: s.remote_dir.clone(),
            local_dir: s
                .local_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            parallel: s
                .parallel_downloads
                .map(|n| n.to_string())
                .unwrap_or_default(),
            accept_invalid_certs: s.accept_invalid_certs,
            focused: EditField::Name,
            error: None,
        }
    }

    /// Returns a mutable reference to the focused TEXT field, or `None` for
    /// boolean fields like `AcceptInvalidCerts` (which are toggled, not
    /// typed into).
    pub fn current_value_mut(&mut self) -> Option<&mut String> {
        match self.focused {
            EditField::Name => Some(&mut self.name),
            EditField::Host => Some(&mut self.host),
            EditField::Port => Some(&mut self.port),
            EditField::Username => Some(&mut self.username),
            EditField::RemoteDir => Some(&mut self.remote_dir),
            EditField::LocalDir => Some(&mut self.local_dir),
            EditField::Parallel => Some(&mut self.parallel),
            EditField::AcceptInvalidCerts => None,
        }
    }
}

/// Operation awaiting user confirmation that an existing target may be
/// overwritten.
#[derive(Debug, Clone)]
pub enum OverwritePending {
    /// The user submitted a rename whose target already exists in the
    /// remote pane.
    Rename {
        from: String,
        to: String,
        /// What to display in the modal (just the bare name).
        target_name: String,
    },
    /// A finalized download plan with files that would clobber existing
    /// local files. The user can overwrite all, skip the conflicting ones,
    /// or cancel.
    DownloadPlan {
        plan: Vec<PlannedJob>,
        /// Indices into `plan` that would overwrite an existing local file.
        conflict_indices: Vec<usize>,
    },
    /// A finalized upload plan with files that would clobber existing
    /// remote files. Same three-way choice.
    UploadPlan {
        plan: Vec<PlannedJob>,
        conflict_indices: Vec<usize>,
    },
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
    pending_password: Option<String>,
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
                // Any key dismisses the error and returns to session select.
                self.host_key_changed_info = None;
                self.pending_session = None;
                self.screen = Screen::SessionSelect;
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

    fn handle_session_select(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => {
                if self.session_cursor > 0 {
                    self.session_cursor -= 1;
                }
            }
            KeyCode::Down => {
                if self.session_cursor + 1 < self.sessions.len() {
                    self.session_cursor += 1;
                }
            }
            KeyCode::Enter => {
                let Some(s) = self.sessions.get(self.session_cursor).cloned() else {
                    return;
                };
                match &s.auth {
                    AuthMethod::Password => {
                        self.pending_session = Some(s);
                        self.password_input.clear();
                        self.screen = Screen::PasswordPrompt;
                    }
                    AuthMethod::Key { .. } | AuthMethod::Agent => {
                        self.pending_session = Some(s.clone());
                        self.pending_password = None;
                        self.start_connect(s, None);
                    }
                }
            }
            KeyCode::Char('n') => {
                self.new_session_input.clear();
                self.new_session_error = None;
                self.screen = Screen::NewSession;
            }
            KeyCode::Char('e') => {
                self.open_edit_session();
            }
            KeyCode::Char('d') => {
                self.open_delete_session();
            }
            KeyCode::Char('t') => {
                self.cycle_theme();
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                self.previous_screen = Screen::SessionSelect;
                self.screen = Screen::ConfirmQuit;
            }
            _ => {}
        }
    }

    fn handle_new_session(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.new_session_input.clear();
                self.new_session_error = None;
                self.screen = Screen::SessionSelect;
            }
            KeyCode::Enter => {
                match Session::from_url(&self.new_session_input) {
                    Ok(session) => {
                        self.new_session_input.clear();
                        self.new_session_error = None;
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
                    Err(e) => {
                        self.new_session_error = Some(e.to_string());
                    }
                }
            }
            KeyCode::Backspace => {
                self.new_session_input.pop();
                self.new_session_error = None;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.new_session_input.clear();
                self.new_session_error = None;
            }
            KeyCode::Char(c) => {
                self.new_session_input.push(c);
                self.new_session_error = None;
            }
            _ => {}
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

    fn handle_edit_session(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.edit_session_form = None;
                self.screen = Screen::SessionSelect;
            }
            KeyCode::Enter => self.submit_edit_session(),
            KeyCode::Tab | KeyCode::Down => {
                if let Some(f) = self.edit_session_form.as_mut() {
                    f.focused = f.focused.next();
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(f) = self.edit_session_form.as_mut() {
                    f.focused = f.focused.prev();
                }
            }
            // Space toggles the focused boolean (currently only
            // AcceptInvalidCerts). On text fields, Space is a literal
            // character — let the Char arm handle it via fallthrough.
            KeyCode::Char(' ') => {
                if let Some(f) = self.edit_session_form.as_mut() {
                    if !f.focused.is_text_field() {
                        if matches!(f.focused, EditField::AcceptInvalidCerts) {
                            f.accept_invalid_certs = !f.accept_invalid_certs;
                        }
                        f.error = None;
                    } else if let Some(v) = f.current_value_mut() {
                        v.push(' ');
                        f.error = None;
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(f) = self.edit_session_form.as_mut() {
                    if let Some(v) = f.current_value_mut() {
                        v.pop();
                        f.error = None;
                    }
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(f) = self.edit_session_form.as_mut() {
                    if let Some(v) = f.current_value_mut() {
                        v.clear();
                        f.error = None;
                    }
                }
            }
            KeyCode::Char(c) => {
                if let Some(f) = self.edit_session_form.as_mut() {
                    if let Some(v) = f.current_value_mut() {
                        v.push(c);
                        f.error = None;
                    }
                }
            }
            _ => {}
        }
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

    fn handle_confirm_delete_session(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(s) = self.pending_session_delete.take() {
                    match Session::delete(&s.name) {
                        Ok(()) => {
                            self.push_log(
                                LogLevel::Success,
                                format!("session deleted: {}", s.name),
                            );
                            self.sessions = Session::list_all().unwrap_or_default();
                            // Clamp the cursor: the list just shrank.
                            if self.sessions.is_empty() {
                                self.session_cursor = 0;
                            } else if self.session_cursor >= self.sessions.len() {
                                self.session_cursor = self.sessions.len() - 1;
                            }
                        }
                        Err(e) => {
                            self.push_log(
                                LogLevel::Error,
                                format!("delete {} failed: {e}", s.name),
                            );
                        }
                    }
                }
                self.screen = Screen::SessionSelect;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.pending_session_delete = None;
                self.screen = Screen::SessionSelect;
            }
            _ => {}
        }
    }

    fn handle_password_prompt(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.password_input.clear();
                self.pending_session = None;
                self.pending_password = None;
                self.screen = Screen::SessionSelect;
            }
            KeyCode::Enter => {
                let Some(session) = self.pending_session.clone() else {
                    self.screen = Screen::SessionSelect;
                    return;
                };
                let password = std::mem::take(&mut self.password_input);
                self.pending_password = Some(password.clone());
                self.start_connect(session, Some(password));
            }
            KeyCode::Backspace => {
                self.password_input.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.password_input.clear();
            }
            KeyCode::Char(c) => {
                self.password_input.push(c);
            }
            _ => {}
        }
    }

    fn handle_key_passphrase_prompt(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                // Bail on the connect attempt entirely.
                self.passphrase_input.clear();
                self.passphrase_error = None;
                self.passphrase_attempted = false;
                self.pending_session = None;
                self.pending_password = None;
                self.screen = Screen::SessionSelect;
                self.push_log(LogLevel::Info, "connect cancelled".into());
            }
            KeyCode::Enter => {
                if self.passphrase_input.is_empty() {
                    // Empty submit would just bounce off the same KeyNeedsPassphrase.
                    // Show a hint instead of round-tripping.
                    self.passphrase_error =
                        Some("enter the passphrase or [esc] to cancel".into());
                    return;
                }
                let Some(session) = self.pending_session.clone() else {
                    self.screen = Screen::SessionSelect;
                    return;
                };
                let passphrase = std::mem::take(&mut self.passphrase_input);
                self.passphrase_attempted = true;
                self.passphrase_error = None;
                // Cache for the dispatcher: parallel transfers re-open the
                // connection and need to decrypt the key again.
                self.pending_password = Some(passphrase.clone());
                self.start_connect(session, Some(passphrase));
            }
            KeyCode::Backspace => {
                self.passphrase_input.pop();
                self.passphrase_error = None;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.passphrase_input.clear();
                self.passphrase_error = None;
            }
            KeyCode::Char(c) => {
                self.passphrase_input.push(c);
                self.passphrase_error = None;
            }
            _ => {}
        }
    }

    fn handle_main(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab => self.cycle_pane(true),
            KeyCode::BackTab => self.cycle_pane(false),
            KeyCode::Up => match self.active_pane {
                Pane::Local | Pane::Remote => {
                    self.active_pane_mut().unwrap().move_cursor(-1)
                }
                Pane::Transfers => self.move_transfer_cursor(-1),
                Pane::Log => {}
            },
            KeyCode::Down => match self.active_pane {
                Pane::Local | Pane::Remote => {
                    self.active_pane_mut().unwrap().move_cursor(1)
                }
                Pane::Transfers => self.move_transfer_cursor(1),
                Pane::Log => {}
            },
            KeyCode::PageUp => match self.active_pane {
                Pane::Local | Pane::Remote => {
                    self.active_pane_mut().unwrap().move_cursor(-10)
                }
                Pane::Transfers => self.move_transfer_cursor(-10),
                Pane::Log => {}
            },
            KeyCode::PageDown => match self.active_pane {
                Pane::Local | Pane::Remote => {
                    self.active_pane_mut().unwrap().move_cursor(10)
                }
                Pane::Transfers => self.move_transfer_cursor(10),
                Pane::Log => {}
            },
            KeyCode::Enter => match self.active_pane {
                Pane::Local => self.local_enter(),
                Pane::Remote => self.remote_enter(),
                Pane::Transfers | Pane::Log => {}
            },
            KeyCode::Backspace => match self.active_pane {
                Pane::Local => {
                    let mut path = std::path::PathBuf::from(&self.local.path);
                    if path.pop() {
                        self.local.path = path.display().to_string();
                        self.local.cursor = 0;
                        self.refresh_local_pane();
                    }
                }
                Pane::Remote => {
                    let parent = transport::parent_remote(&self.remote.path);
                    if parent != self.remote.path {
                        self.refresh_remote_pane(parent);
                    }
                }
                Pane::Transfers | Pane::Log => {}
            },
            KeyCode::Char(' ') => {
                if let Some(pane) = self.active_pane_mut() {
                    pane.toggle_selected();
                }
            }
            KeyCode::Char('c') if self.active_pane == Pane::Transfers => {
                self.request_cancel_selected_transfer();
            }
            KeyCode::Char('C') if self.active_pane == Pane::Transfers => {
                self.request_cancel_selected_batch();
            }
            // Resume: re-queue any jobs from the last interrupted walk that
            // haven't completed yet. 'r' resumes the download checkpoint,
            // 'R' resumes the upload checkpoint.
            KeyCode::Char('r') if self.active_pane == Pane::Transfers => {
                self.resume_walk(Direction::Download);
            }
            KeyCode::Char('R') if self.active_pane == Pane::Transfers => {
                self.resume_walk(Direction::Upload);
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_save_session();
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.enqueue_selected_downloads();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_selected_uploads();
            }
            KeyCode::F(2) => {
                if self.active_pane == Pane::Remote {
                    self.open_rename();
                }
            }
            KeyCode::F(7) => {
                if self.active_pane == Pane::Remote {
                    self.open_mkdir();
                }
            }
            KeyCode::Delete if key.modifiers.contains(KeyModifiers::SHIFT) => {
                if self.active_pane == Pane::Remote {
                    self.open_delete();
                }
            }
            // 'D' (uppercase) as an alternative to Shift+Delete for terminals
            // that don't pass that combo cleanly.
            KeyCode::Char('D') => {
                if self.active_pane == Pane::Remote {
                    self.open_delete();
                }
            }
            KeyCode::Char('p') => {
                self.toggle_pause();
            }
            KeyCode::Char('t') => {
                self.cycle_theme();
            }
            KeyCode::Char('v') => {
                self.handle_view_request();
            }
            KeyCode::Char('/') => {
                if matches!(self.active_pane, Pane::Local | Pane::Remote) {
                    self.open_search();
                }
            }
            KeyCode::F(5) => {
                self.refresh_active_pane();
            }
            KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.transport.is_some() {
                    self.screen = Screen::ConfirmDisconnect;
                }
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                self.previous_screen = Screen::Main;
                self.screen = Screen::ConfirmQuit;
            }
            _ => {}
        }
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

    fn handle_confirm_cancel(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(pc) = self.pending_cancel.take() {
                    if let Some(manager) = &self.transfer_manager {
                        match pc {
                            PendingCancel::Single { id, name } => {
                                manager.cancel(id);
                                self.push_log(
                                    LogLevel::Warn,
                                    format!("cancelled: {name}"),
                                );
                                // Remove this job from the checkpoint map so a
                                // subsequent resume re-queues it (it was never
                                // completed, so it must not be skipped). The
                                // checkpoint file itself is kept — the rest of
                                // the batch can still complete and be tracked.
                                self.checkpoint_job_map.remove(&id);
                            }
                            PendingCancel::Batch { batch_id, .. } => {
                                let (active_n, pending_n) =
                                    manager.cancel_batch(batch_id);
                                self.push_log(
                                    LogLevel::Warn,
                                    format!(
                                        "cancelled batch: {} active + {} queued",
                                        active_n, pending_n
                                    ),
                                );
                                // The whole batch is being abandoned. Drop the
                                // checkpoint so stale files don't accumulate
                                // and a mistaken `r` / `R` doesn't re-queue a
                                // batch the user explicitly threw away.
                                self.discard_active_checkpoint();
                            }
                        }
                    }
                    // Re-clamp the cursor: cancellation removes jobs from
                    // the active list, so the cursor may now point off the end.
                    let new_len = self.active_jobs().len();
                    if new_len == 0 {
                        self.transfer_cursor = 0;
                    } else if self.transfer_cursor >= new_len {
                        self.transfer_cursor = new_len - 1;
                    }
                }
                self.screen = self.previous_screen.clone();
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.pending_cancel = None;
                self.screen = self.previous_screen.clone();
            }
            _ => {}
        }
    }

    /// Remove the active checkpoint from memory and delete its file on disk.
    ///
    /// Called when a whole batch is cancelled (user pressed `C` and confirmed)
    /// or when a transfer fails in a way that makes the batch unresumable.
    /// Soft-failures (e.g. the file was already removed) are logged at `warn`
    /// and do not abort other work.
    fn discard_active_checkpoint(&mut self) {
        if let Some(cp) = self.active_checkpoint.take() {
            self.checkpoint_job_map.clear();
            if let Err(e) = Checkpoint::remove(&cp.session, cp.kind) {
                self.push_log(
                    LogLevel::Warn,
                    format!("could not remove checkpoint file: {e}"),
                );
            }
        }
    }

    // -------------------------------------------------------------------
    // Disconnect (return to the session selector)
    // -------------------------------------------------------------------

    fn handle_confirm_disconnect(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.disconnect();
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.screen = Screen::Main;
            }
            _ => {}
        }
    }

    fn handle_confirm_host_key(&mut self, key: KeyEvent) {
        use crate::transport::sftp::HostKeyDecision;
        let decision = match key.code {
            // y / Y — accept and save to known_hosts
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(HostKeyDecision::AcceptAndSave),
            // t / T — trust once, don't save
            KeyCode::Char('t') | KeyCode::Char('T') => Some(HostKeyDecision::AcceptOnce),
            // n / N / Esc — reject
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                Some(HostKeyDecision::Reject)
            }
            _ => None,
        };

        if let Some(decision) = decision {
            if let Some(mut phk) = self.pending_host_key.take() {
                if let Some(tx) = phk.decision_tx.take() {
                    let _ = tx.send(decision);
                }
            }
            // Return to the Connection screen while we wait for the connect
            // task to proceed (or fail). The task is still blocked on the
            // oneshot; it will resume now that we've sent the decision.
            self.screen = match decision {
                HostKeyDecision::Reject => {
                    self.pending_session = None;
                    Screen::SessionSelect
                }
                _ => Screen::Connection,
            };
        }
    }

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

    fn handle_rename(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.rename_input.clear();
                self.rename_original.clear();
                self.rename_error = None;
                self.screen = Screen::Main;
            }
            KeyCode::Enter => self.submit_rename(),
            KeyCode::Backspace => {
                self.rename_input.pop();
                self.rename_error = None;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.rename_input.clear();
                self.rename_error = None;
            }
            KeyCode::Char(c) => {
                self.rename_input.push(c);
                self.rename_error = None;
            }
            _ => {}
        }
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

    fn handle_mkdir(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mkdir_input.clear();
                self.mkdir_error = None;
                self.screen = Screen::Main;
            }
            KeyCode::Enter => self.submit_mkdir(),
            KeyCode::Backspace => {
                self.mkdir_input.pop();
                self.mkdir_error = None;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.mkdir_input.clear();
                self.mkdir_error = None;
            }
            KeyCode::Char(c) => {
                self.mkdir_input.push(c);
                self.mkdir_error = None;
            }
            _ => {}
        }
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

    fn handle_confirm_delete(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(pd) = self.pending_delete.take() {
                    self.start_delete(pd.name, pd.remote_path, pd.is_dir);
                }
                self.screen = Screen::Main;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.pending_delete = None;
                self.screen = Screen::Main;
            }
            _ => {}
        }
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
    fn start_selected_uploads(&mut self) {
        if self.transfer_manager.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }

        let any_selected = self.local.entries.iter().any(|e| e.selected);
        let entries: Vec<(String, bool)> = if any_selected {
            self.local
                .entries
                .iter()
                .filter(|e| e.selected)
                .map(|e| (e.name.clone(), e.is_dir))
                .collect()
        } else {
            match self.local.entries.get(self.local.cursor) {
                Some(e) if e.name != ".." => vec![(e.name.clone(), e.is_dir)],
                _ => Vec::new(),
            }
        };

        if entries.is_empty() {
            self.push_log(LogLevel::Warn, "no items to upload".into());
            return;
        }

        // Clear selection upfront — the walk task is async, and we'd rather
        // not surprise the user later if they select more items meanwhile.
        for e in &mut self.local.entries {
            e.selected = false;
        }

        // Build the upload roots: each selected entry becomes a (local, remote)
        // pair. Files become "trivial walks" of a single Upload job; directories
        // unfold via walk_local. Either way the plan goes through the same
        // conflict-check + dispatch flow.
        let local_base = std::path::PathBuf::from(&self.local.path);
        let roots: Vec<(std::path::PathBuf, String, bool)> = entries
            .iter()
            .map(|(name, is_dir)| {
                (
                    local_base.join(name),
                    transport::join_remote(&self.remote.path, name),
                    *is_dir,
                )
            })
            .collect();

        let pending_count = entries.len();
        self.push_log(
            LogLevel::Info,
            format!("preparing {pending_count} upload(s)…"),
        );
        self.start_upload_walk(roots);
    }

    /// Spawn the upload preparation task: walks every root (file or dir) into
    /// a flat plan, then probes each destination directory once for existing
    /// names to populate `conflict_indices`. Posts back as `WalkComplete`.
    fn start_upload_walk(
        &mut self,
        roots: Vec<(std::path::PathBuf, String, bool)>,
    ) {
        let Some(t) = self.transport.clone() else {
            return;
        };
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            // Phase 1: build the plan from local FS walks. Files become a
            // single Upload job; directories unfold into mkdirs + uploads.
            let mut plan: Vec<PlannedJob> = Vec::new();
            for (local, remote, is_dir) in roots {
                let chunk = if is_dir {
                    let walk = walk_local(&local, &remote).await;
                    match walk {
                        Ok(p) => p,
                        Err(e) => {
                            let _ = tx.send(AppEvent::WalkFailed {
                                error: e.to_string(),
                                kind: Direction::Upload,
                            });
                            return;
                        }
                    }
                } else {
                    vec![PlannedJob::Upload {
                        local_path: local,
                        remote_path: remote,
                    }]
                };
                plan.extend(chunk);
            }

            // Phase 2: collect every destination directory mentioned by the
            // plan (the parent of each Upload job), then list each one once
            // and check for conflicts in O(dirs) round-trips.
            let mut transport = t.lock().await;
            let conflict_indices =
                match find_upload_conflicts(&mut **transport, &plan).await {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx.send(AppEvent::WalkFailed {
                            error: e.to_string(),
                            kind: Direction::Upload,
                        });
                        return;
                    }
                };

            let _ = tx.send(AppEvent::WalkComplete {
                plan,
                conflict_indices,
                kind: Direction::Upload,
            });
        });
    }

    /// Convert a [`PlannedJob`] sequence into queued transfer jobs. Mkdirs
    /// land first via `enqueue_mkdir`; files via `enqueue_download` /
    /// `enqueue_upload`. The dispatcher's parallelism then takes over.
    ///
    /// Before any jobs are enqueued, the plan is written to a checkpoint file
    /// so that a crash or forced quit mid-batch can be resumed with
    /// `--resume` (CLI) or the `r` key in the Transfers pane.
    fn dispatch_plan(&mut self, plan: Vec<PlannedJob>, kind: Direction) {
        let Some(manager) = self.transfer_manager.clone() else {
            return;
        };

        // Only allocate a batch id when there's actually a batch — i.e.
        // more than one file job (mkdirs alone don't count). Single-file
        // plans get the no-batch path so the existing single-job cancel
        // covers them; allocating a batch id would just be noise.
        let file_count = plan
            .iter()
            .filter(|j| !matches!(j, PlannedJob::Mkdir { .. }))
            .count();
        let batch_id = if file_count > 1 || plan.len() > 1 {
            Some(manager.allocate_batch_id())
        } else {
            None
        };

        // ---- checkpoint: write before first enqueue so the file exists
        // even if the app is killed on the first transfer. ---------------
        let ck_kind = match kind {
            Direction::Upload => CheckpointKind::Upload,
            Direction::Download => CheckpointKind::Download,
            Direction::CreateDir => unreachable!(),
        };
        let session_name = self
            .current_session
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "default".to_string());

        let ck_jobs: Vec<CheckpointJob> = plan
            .iter()
            .map(|pj| match pj {
                PlannedJob::Mkdir { remote_path } => CheckpointJob::Mkdir {
                    remote_path: remote_path.clone(),
                    status: JobStatus::Pending,
                },
                PlannedJob::Download { remote_path, local_path } => CheckpointJob::Download {
                    remote_path: remote_path.clone(),
                    local_path: local_path.clone(),
                    status: JobStatus::Pending,
                },
                PlannedJob::Upload { local_path, remote_path } => CheckpointJob::Upload {
                    local_path: local_path.clone(),
                    remote_path: remote_path.clone(),
                    status: JobStatus::Pending,
                },
            })
            .collect();

        let checkpoint = Checkpoint::new(&session_name, ck_kind, ck_jobs);
        if let Err(e) = checkpoint.save() {
            self.push_log(
                LogLevel::Warn,
                format!("checkpoint save failed (resume unavailable): {e}"),
            );
        }
        self.active_checkpoint = Some(checkpoint);
        self.checkpoint_job_map.clear();
        // ---------------------------------------------------------------

        let mut dirs = 0usize;
        let mut files = 0usize;
        for (cp_idx, job) in plan.into_iter().enumerate() {
            let job_id = match (job, batch_id) {
                (PlannedJob::Mkdir { remote_path }, Some(b)) => {
                    dirs += 1;
                    manager.enqueue_mkdir_batched(remote_path, b)
                }
                (PlannedJob::Mkdir { remote_path }, None) => {
                    dirs += 1;
                    manager.enqueue_mkdir(remote_path)
                }
                (
                    PlannedJob::Download {
                        remote_path,
                        local_path,
                    },
                    Some(b),
                ) => {
                    files += 1;
                    manager.enqueue_download_batched(remote_path, local_path, b)
                }
                (
                    PlannedJob::Download {
                        remote_path,
                        local_path,
                    },
                    None,
                ) => {
                    files += 1;
                    manager.enqueue_download(remote_path, local_path)
                }
                (
                    PlannedJob::Upload {
                        local_path,
                        remote_path,
                    },
                    Some(b),
                ) => {
                    files += 1;
                    manager.enqueue_upload_batched(local_path, remote_path, b)
                }
                (
                    PlannedJob::Upload {
                        local_path,
                        remote_path,
                    },
                    None,
                ) => {
                    files += 1;
                    manager.enqueue_upload(local_path, remote_path)
                }
            };
            if let Some(id) = job_id {
                self.checkpoint_job_map.insert(id, cp_idx);
            }
        }
        let label = match kind {
            Direction::Download => "downloads",
            Direction::Upload => "uploads",
            Direction::CreateDir => unreachable!(),
        };
        self.push_log(
            LogLevel::Info,
            format!("queued {label}: {files} file(s) + {dirs} folder(s)"),
        );
    }

    /// Dispatch a *resumed* plan: load the checkpoint for `kind`, skip jobs
    /// already marked done, and enqueue only the remaining ones.
    ///
    /// Called from the `r` keybinding in the Transfers pane (or `--resume`
    /// at startup). Logs a message if there is nothing to resume.
    pub fn resume_walk(&mut self, kind: Direction) {
        let ck_kind = match kind {
            Direction::Upload => CheckpointKind::Upload,
            Direction::Download => CheckpointKind::Download,
            Direction::CreateDir => unreachable!(),
        };
        let session_name = self
            .current_session
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "default".to_string());

        let checkpoint = match Checkpoint::load(&session_name, ck_kind) {
            Ok(Some(cp)) => cp,
            Ok(None) => {
                self.push_log(LogLevel::Warn, "no checkpoint found to resume".into());
                return;
            }
            Err(e) => {
                self.push_log(LogLevel::Error, format!("checkpoint load failed: {e}"));
                return;
            }
        };

        let pending = checkpoint.pending_count();
        let done = checkpoint.done_count();
        if pending == 0 {
            self.push_log(
                LogLevel::Info,
                "checkpoint is already complete — nothing to resume".into(),
            );
            let _ = Checkpoint::remove(&session_name, ck_kind);
            return;
        }

        self.push_log(
            LogLevel::Info,
            format!(
                "resuming {}: skipping {done} already-done, re-queuing {pending}",
                ck_kind.as_str()
            ),
        );

        // Rebuild a PlannedJob list from the undone entries only.
        let resume_plan: Vec<PlannedJob> = checkpoint
            .jobs
            .iter()
            .filter(|j| j.needs_resume())
            .map(|j| match j {
                CheckpointJob::Mkdir { remote_path, .. } => PlannedJob::Mkdir {
                    remote_path: remote_path.clone(),
                },
                CheckpointJob::Download { remote_path, local_path, .. } => PlannedJob::Download {
                    remote_path: remote_path.clone(),
                    local_path: local_path.clone(),
                },
                CheckpointJob::Upload { local_path, remote_path, .. } => PlannedJob::Upload {
                    local_path: local_path.clone(),
                    remote_path: remote_path.clone(),
                },
            })
            .collect();

        // dispatch_plan overwrites the checkpoint with a fresh plan covering
        // only the re-queued jobs, all starting as `pending`. They will
        // transition through `in_progress` → `done` as they run.
        self.dispatch_plan(resume_plan, kind);
    }

    // -------------------------------------------------------------------
    // Overwrite confirmation (shared across rename, download, upload)
    // -------------------------------------------------------------------

    fn handle_confirm_overwrite(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                // Y always means "overwrite" / "proceed" for backward
                // compat with the old single-file modals. For plan modals
                // it's equivalent to "overwrite all conflicts".
                self.confirm_overwrite_proceed(false);
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                // Skip-conflicts only meaningful for plan variants; for
                // the rename / single-file variants there's nothing to
                // skip and we treat it as a no-op.
                self.confirm_overwrite_proceed(true);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.pending_overwrite = None;
                self.rename_input.clear();
                self.rename_original.clear();
                self.screen = Screen::Main;
                self.push_log(LogLevel::Info, "overwrite cancelled".into());
            }
            _ => {}
        }
    }

    fn confirm_overwrite_proceed(&mut self, skip_conflicts: bool) {
        let Some(op) = self.pending_overwrite.take() else {
            self.screen = Screen::Main;
            return;
        };
        self.screen = Screen::Main;
        match op {
            OverwritePending::Rename { from, to, .. } => {
                self.rename_input.clear();
                self.rename_original.clear();
                self.start_rename(from, to);
            }
            OverwritePending::DownloadPlan {
                plan,
                conflict_indices,
            } => {
                let final_plan = if skip_conflicts {
                    drop_conflicting(plan, &conflict_indices)
                } else {
                    plan
                };
                self.dispatch_plan(final_plan, Direction::Download);
            }
            OverwritePending::UploadPlan {
                plan,
                conflict_indices,
            } => {
                let final_plan = if skip_conflicts {
                    drop_conflicting(plan, &conflict_indices)
                } else {
                    plan
                };
                self.dispatch_plan(final_plan, Direction::Upload);
            }
        }
    }

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

    /// Enqueue downloads for the selected items in the remote pane. If
    /// nothing is selected, falls back to the cursor item.
    fn enqueue_selected_downloads(&mut self) {
        if self.transfer_manager.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }

        // Collect (name, is_dir) pairs from the active selection or the
        // cursor entry.
        let selections: Vec<(String, bool)> = {
            let any_selected = self.remote.entries.iter().any(|e| e.selected);
            if any_selected {
                self.remote
                    .entries
                    .iter()
                    .filter(|e| e.selected)
                    .map(|e| (e.name.clone(), e.is_dir))
                    .collect()
            } else {
                match self.remote.entries.get(self.remote.cursor) {
                    Some(e) if e.name != ".." => vec![(e.name.clone(), e.is_dir)],
                    _ => Vec::new(),
                }
            }
        };

        if selections.is_empty() {
            self.push_log(LogLevel::Warn, "no items to download".into());
            return;
        }

        // Clear selection upfront — see start_selected_uploads for rationale.
        for e in &mut self.remote.entries {
            e.selected = false;
        }

        let local_base = std::path::PathBuf::from(&self.local.path);
        let roots: Vec<(String, std::path::PathBuf, bool)> = selections
            .iter()
            .filter_map(|(name, is_dir)| {
                let safe = safe_local_name(name)?;
                Some((
                    transport::join_remote(&self.remote.path, name),
                    local_base.join(safe),
                    *is_dir,
                ))
            })
            .collect();

        let pending_count = roots.len();
        self.push_log(
            LogLevel::Info,
            format!("preparing {pending_count} download(s)…"),
        );
        self.start_download_walk(roots);
    }

    /// Spawn the download preparation task: walks every root (file or dir)
    /// into a flat plan, then probes each destination file path in the
    /// local FS to populate `conflict_indices`. Posts back as `WalkComplete`.
    fn start_download_walk(
        &mut self,
        roots: Vec<(String, std::path::PathBuf, bool)>,
    ) {
        let Some(t) = self.transport.clone() else {
            return;
        };
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            // Phase 1: build the plan from remote walks. Files become a
            // single Download job; directories unfold into mkdirs (no-ops
            // for downloads — local mkdirs are handled inside walk_remote)
            // plus per-file Download jobs.
            let mut plan: Vec<PlannedJob> = Vec::new();
            {
                let mut transport = t.lock().await;
                for (remote, local, is_dir) in roots {
                    let chunk = if is_dir {
                        let walk = walk_remote(&mut **transport, &remote, &local).await;
                        match walk {
                            Ok(p) => p,
                            Err(e) => {
                                let _ = tx.send(AppEvent::WalkFailed {
                                    error: e.to_string(),
                                    kind: Direction::Download,
                                });
                                return;
                            }
                        }
                    } else {
                        vec![PlannedJob::Download {
                            remote_path: remote,
                            local_path: local,
                        }]
                    };
                    plan.extend(chunk);
                }
            }

            // Phase 2: local-FS conflict probe. Every Download job's
            // destination gets metadata-checked. This is sync I/O on the
            // local disk — fast even for thousands of files.
            let conflict_indices = find_download_conflicts(&plan).await;

            let _ = tx.send(AppEvent::WalkComplete {
                plan,
                conflict_indices,
                kind: Direction::Download,
            });
        });
    }

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

    fn handle_search(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                // Cancel: clear filter and restore the full listing.
                match self.search_target {
                    Pane::Local => self.local.clear_filter(),
                    Pane::Remote => self.remote.clear_filter(),
                    _ => {}
                }
                self.search_input.clear();
                self.screen = Screen::Main;
            }
            KeyCode::Enter => {
                // Accept: keep the filter (already applied live), exit search.
                self.screen = Screen::Main;
            }
            KeyCode::Up => self.move_search_cursor(-1),
            KeyCode::Down => self.move_search_cursor(1),
            KeyCode::PageUp => self.move_search_cursor(-10),
            KeyCode::PageDown => self.move_search_cursor(10),
            KeyCode::Backspace => {
                self.search_input.pop();
                self.apply_search_filter();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_input.clear();
                self.apply_search_filter();
            }
            KeyCode::Char(c) => {
                self.search_input.push(c);
                self.apply_search_filter();
            }
            _ => {}
        }
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

    fn handle_save_session(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.save_session_error = None;
                self.screen = Screen::Main;
            }
            KeyCode::Enter => {
                let name = self.save_session_input.trim().to_string();
                if name.is_empty() {
                    self.save_session_error = Some("name cannot be empty".into());
                    return;
                }
                let Some(current) = self.current_session.clone() else {
                    self.save_session_error = Some("no active session".into());
                    return;
                };

                // Snapshot the current navigation state into the saved session
                // so reopening picks up where we left off.
                let mut to_save = current;
                to_save.name = name.clone();
                to_save.remote_dir = if self.remote.path.is_empty() {
                    "/".to_string()
                } else {
                    self.remote.path.clone()
                };
                to_save.local_dir = Some(std::path::PathBuf::from(&self.local.path));

                match to_save.save() {
                    Ok(()) => {
                        self.current_session = Some(to_save);
                        // Refresh the sessions list so it reflects the new
                        // entry next time the user opens the selector.
                        self.sessions = Session::list_all().unwrap_or_default();
                        self.save_session_error = None;
                        self.screen = Screen::Main;
                        self.push_log(
                            LogLevel::Success,
                            format!("session saved: {name}"),
                        );
                    }
                    Err(e) => {
                        self.save_session_error = Some(e.to_string());
                    }
                }
            }
            KeyCode::Backspace => {
                self.save_session_input.pop();
                self.save_session_error = None;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.save_session_input.clear();
                self.save_session_error = None;
            }
            KeyCode::Char(c) => {
                self.save_session_input.push(c);
                self.save_session_error = None;
            }
            _ => {}
        }
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

    fn handle_viewer(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                // If an image was on screen, force a full repaint after we
                // close — sixel and kitty graphics aren't in ratatui's buffer
                // and won't be cleaned up by the next diff'd draw.
                let was_image = matches!(
                    self.viewer.as_ref().map(|v| &v.kind),
                    Some(ViewerKind::Image { .. })
                );
                self.viewer = None;
                self.image_needs_redraw = false;
                if was_image {
                    self.needs_terminal_clear = true;
                }
                self.screen = self.previous_screen.clone();
            }
            KeyCode::Up | KeyCode::Char('k') => self.viewer_scroll(-1),
            KeyCode::Down | KeyCode::Char('j') => self.viewer_scroll(1),
            KeyCode::PageUp => self.viewer_scroll(-20),
            KeyCode::PageDown | KeyCode::Char(' ') => self.viewer_scroll(20),
            KeyCode::Home | KeyCode::Char('g') => self.viewer_scroll_to(0),
            KeyCode::End | KeyCode::Char('G') => self.viewer_scroll_to(usize::MAX),
            _ => {}
        }
    }

    fn viewer_scroll(&mut self, delta: isize) {
        if let Some(viewer) = self.viewer.as_mut() {
            if let ViewerKind::Text { lines, scroll } = &mut viewer.kind {
                let max = lines.len().saturating_sub(1);
                let next = (*scroll as isize + delta).max(0) as usize;
                *scroll = next.min(max);
            }
        }
    }

    fn viewer_scroll_to(&mut self, target: usize) {
        if let Some(viewer) = self.viewer.as_mut() {
            if let ViewerKind::Text { lines, scroll } = &mut viewer.kind {
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

        let escape = backend.render(bytes, body_x, body_y, body_w, body_h);
        if escape.is_empty() {
            self.push_log(
                LogLevel::Warn,
                "image render produced no output (decode failed or sixel stub)".into(),
            );
            self.image_needs_redraw = false;
            return Ok(());
        }

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
    fn start_connect(&mut self, session: Session, password: Option<String>) {
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
            let result = match tokio::time::timeout(
                CONNECT_TIMEOUT,
                transport::open(&session, password.as_deref(), tx_for_transport),
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
    // App events from background tasks
    // -------------------------------------------------------------------

    fn handle_app_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Connected(transport) => {
                // Stale guard: user may have cancelled before connect resolved.
                let Some(session) = self.pending_session.take() else {
                    drop(transport);
                    return;
                };
                let remote_dir = session.remote_dir.clone();
                let password = self.pending_password.clone();
                // Successful connect — clear any leftover passphrase state.
                self.passphrase_input.clear();
                self.passphrase_error = None;
                self.passphrase_attempted = false;

                // Apply the session's local_dir override, if any. Empty
                // strings and missing fields fall through to the
                // already-initialized default (typically the user's home).
                if let Some(configured) = session.local_dir.as_ref() {
                    if let Some(resolved) = resolve_local_dir(configured) {
                        self.local.path = resolved.display().to_string();
                        self.local.cursor = 0;
                        self.local.clear_filter();
                        self.refresh_local_pane();
                    } else {
                        self.push_log(
                            LogLevel::Warn,
                            format!(
                                "session local_dir `{}` not found; \
                                 keeping {}",
                                configured.display(),
                                self.local.path
                            ),
                        );
                    }
                }

                // Spin up the transfer manager + dispatcher for this session.
                // The dispatcher opens its own connections per parallel slot,
                // so it gets a copy of the password (cached on App for the
                // duration of the session).
                let parallelism = session
                    .parallel_downloads
                    .unwrap_or(self.config.general.parallel_downloads);
                let (manager, mut events_rx) = TransferManager::new(parallelism);
                let dispatcher =
                    Dispatcher::spawn(manager.clone(), session.clone(), password, self.app_event_tx.clone());

                // Forwarder: drain the dispatcher's event stream into the App
                // event channel as `AppEvent::Transfer(...)`.
                let app_tx = self.app_event_tx.clone();
                tokio::spawn(async move {
                    while let Some(ev) = events_rx.recv().await {
                        if app_tx.send(AppEvent::Transfer(ev)).is_err() {
                            break;
                        }
                    }
                });

                self.transport = Some(Arc::new(Mutex::new(transport)));
                self.transfer_manager = Some(manager);
                self.dispatcher = Some(dispatcher);
                let is_scp = session.protocol == crate::session::Protocol::Scp;
                self.current_session = Some(session);
                self.screen = Screen::Main;
                self.active_pane = Pane::Remote;
                self.push_log(
                    LogLevel::Success,
                    format!("connected · {parallelism} parallel slot(s)"),
                );
                if is_scp {
                    self.push_log(
                        LogLevel::Warn,
                        "scp:// is routed through SFTP internally; \
                         full file-manager operations are available".into(),
                    );
                }
                self.refresh_remote_pane(remote_dir);
            }
            AppEvent::ConnectFailed(err) => {
                if self.pending_session.is_none() {
                    return; // user already moved on
                }
                self.pending_session = None;
                self.pending_password = None;
                self.password_input.clear();
                self.passphrase_input.clear();
                self.passphrase_error = None;
                self.passphrase_attempted = false;
                self.screen = Screen::SessionSelect;
                self.push_log(LogLevel::Error, format!("connect failed: {err}"));
            }
            AppEvent::ConnectKeyNeedsPassphrase => {
                // Stale guard: the user may have escaped out before this
                // result came back.
                if self.pending_session.is_none() {
                    return;
                }
                let was_attempted = self.passphrase_attempted;
                self.passphrase_input.clear();
                self.passphrase_error = if was_attempted {
                    Some("passphrase incorrect, try again".into())
                } else {
                    None
                };
                self.screen = Screen::KeyPassphrasePrompt;
            }
            AppEvent::Listed { path, entries } => {
                // Discard stale responses (user navigated again before this returned).
                if path != self.remote.path {
                    return;
                }
                self.remote
                    .set_entries(build_remote_pane_entries(&entries, &path));
            }
            AppEvent::ListFailed { path, error } => {
                self.push_log(
                    LogLevel::Error,
                    format!("list {path} failed: {error}"),
                );
            }
            AppEvent::Renamed { from, to } => {
                let from_name = from
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or(&from)
                    .to_string();
                let to_name = to
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or(&to)
                    .to_string();
                self.push_log(
                    LogLevel::Success,
                    format!("renamed: {from_name} → {to_name}"),
                );
                let path = self.remote.path.clone();
                self.refresh_remote_pane(path);
            }
            AppEvent::RenameFailed { from, to: _, error } => {
                self.push_log(
                    LogLevel::Error,
                    format!("rename {from} failed: {error}"),
                );
            }
            AppEvent::MkdirDone { path } => {
                let name = path
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or(&path)
                    .to_string();
                self.push_log(LogLevel::Success, format!("created: {name}"));
                let dir = self.remote.path.clone();
                self.refresh_remote_pane(dir);
            }
            AppEvent::MkdirFailed { path, error } => {
                let name = path
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or(&path)
                    .to_string();
                self.push_log(LogLevel::Error, format!("mkdir {name} failed: {error}"));
            }
            AppEvent::Deleted { name } => {
                self.push_log(LogLevel::Success, format!("deleted: {name}"));
                let path = self.remote.path.clone();
                self.refresh_remote_pane(path);
            }
            AppEvent::DeleteFailed { name, error } => {
                self.push_log(
                    LogLevel::Error,
                    format!("delete {name} failed: {error}"),
                );
            }
            AppEvent::WalkComplete {
                plan,
                conflict_indices,
                kind,
            } => {
                if conflict_indices.is_empty() {
                    self.dispatch_plan(plan, kind);
                } else {
                    let pending = match kind {
                        Direction::Download => OverwritePending::DownloadPlan {
                            plan,
                            conflict_indices,
                        },
                        Direction::Upload => OverwritePending::UploadPlan {
                            plan,
                            conflict_indices,
                        },
                        Direction::CreateDir => unreachable!(),
                    };
                    self.pending_overwrite = Some(pending);
                    self.screen = Screen::ConfirmOverwrite;
                }
            }
            AppEvent::WalkFailed { error, kind } => {
                let label = match kind {
                    Direction::Download => "downloads",
                    Direction::Upload => "uploads",
                    Direction::CreateDir => unreachable!(),
                };
                self.push_log(
                    LogLevel::Error,
                    format!("preparing {label} failed: {error}"),
                );
            }
            AppEvent::ViewLoaded { name, kind, bytes } => {
                let mut needs_redraw = false;
                if let Some(viewer) = self.viewer.as_mut() {
                    if viewer.name == name {
                        viewer.kind = match kind {
                            FileViewKind::Text => {
                                let text = if preview::is_nfo_file(&name) {
                                    preview::decode_cp437(&bytes)
                                } else {
                                    String::from_utf8_lossy(&bytes).into_owned()
                                };
                                let lines: Vec<String> =
                                    text.lines().map(crate::error::sanitize_line).collect();
                                ViewerKind::Text { lines, scroll: 0 }
                            }
                            FileViewKind::Image => {
                                // Only enter Image state if a graphics backend
                                // is available; otherwise show a useful
                                // explanation in the viewer.
                                let proto = preview::detect(
                                    self.config.terminal.image_preview,
                                );
                                if matches!(proto, preview::GraphicsProtocol::None)
                                    || preview::backend_for(proto).is_none()
                                {
                                    let term = std::env::var("TERM")
                                        .unwrap_or_else(|_| "<unset>".into());
                                    ViewerKind::Unsupported(format!(
                                        "no supported graphics protocol \
                                         (TERM={term}). \
                                         supported: kitty, ghostty, wezterm, iterm2"
                                    ))
                                } else {
                                    needs_redraw = true;
                                    ViewerKind::Image { bytes }
                                }
                            }
                            FileViewKind::Unsupported(reason) => {
                                ViewerKind::Unsupported(reason)
                            }
                        };
                    }
                }
                if needs_redraw {
                    self.image_needs_redraw = true;
                }
            }
            AppEvent::ViewFailed { name, error } => {
                if let Some(viewer) = self.viewer.as_mut() {
                    if viewer.name == name {
                        viewer.kind =
                            ViewerKind::Unsupported(format!("read failed: {error}"));
                    }
                }
                self.push_log(LogLevel::Error, format!("view {name} failed: {error}"));
            }
            AppEvent::Transfer(ev) => self.handle_transfer_event(ev),
            AppEvent::HostKeyUnknown {
                host,
                key_type,
                key_b64,
                fingerprint,
                decision_tx,
            } => {
                self.pending_host_key = Some(PendingHostKey {
                    host,
                    key_type,
                    key_b64,
                    fingerprint,
                    decision_tx: Some(decision_tx),
                });
                self.previous_screen = self.screen.clone();
                self.screen = Screen::ConfirmHostKey;
            }
            AppEvent::HostKeyChanged {
                host,
                stored_key_type,
                presented_key_type,
                fingerprint,
            } => {
                self.push_log(
                    LogLevel::Error,
                    format!(
                        "HOST KEY MISMATCH for {host}: stored {stored_key_type}, \
                         got {presented_key_type} — connection refused"
                    ),
                );
                self.host_key_changed_info = Some(HostKeyChangedInfo {
                    host,
                    stored_key_type,
                    presented_key_type,
                    fingerprint,
                });
                self.pending_session = None;
                self.screen = Screen::HostKeyChanged;
            }
        }
    }

    fn handle_transfer_event(&mut self, ev: TransferEvent) {
        // Look up jobs by id from the manager's snapshot. The manager retains
        // jobs across state changes, so this works for Started/Complete/Failed
        // alike.
        let snapshot = self
            .transfer_manager
            .as_ref()
            .map(|m| m.snapshot())
            .unwrap_or_default();
        let lookup = |id: u64| snapshot.iter().find(|j| j.id == id).cloned();

        match ev {
            TransferEvent::Queued(job) => {
                self.push_log(LogLevel::Info, format!("queued: {}", job.remote_path));
            }
            TransferEvent::Started(id) => {
                // Write `in_progress` to disk before the transfer does any
                // I/O. A crash between here and the `Complete` write leaves
                // the job as `in_progress`, which causes it to be re-queued
                // on resume rather than silently skipped.
                if let Some(cp_idx) = self.checkpoint_job_map.get(&id).copied() {
                    if let Some(cp) = self.active_checkpoint.as_mut() {
                        if let Err(e) = cp.mark_in_progress_and_save(cp_idx) {
                            tracing::warn!(id, cp_idx, "checkpoint in_progress write failed: {e}");
                        }
                    }
                }
                if let Some(j) = lookup(id) {
                    self.push_log(
                        LogLevel::Info,
                        format!("downloading: {}", j.remote_path),
                    );
                }
            }
            TransferEvent::Progress => {
                // Progress is tracked inside TransferManager; no per-tick log
                // spam. A later pass can render an active-transfers strip in
                // the header from `manager.snapshot()`.
            }
            TransferEvent::Complete(id) => {
                // Mark the job done in the checkpoint before updating the log
                // so the file is consistent even if we crash immediately after.
                if let Some(cp_idx) = self.checkpoint_job_map.get(&id).copied() {
                    if let Some(cp) = self.active_checkpoint.as_mut() {
                        if let Err(e) = cp.mark_done_and_save(cp_idx) {
                            // Non-fatal: the user can still re-transfer the
                            // failed job, they just can't resume from the
                            // checkpoint for this specific entry.
                            tracing::warn!(id, cp_idx, "checkpoint update failed: {e}");
                        }
                        // When every job is done, remove the checkpoint file
                        // so a subsequent `r` press doesn't try to resume
                        // a completed batch.
                        if cp.pending_count() == 0 {
                            let session = cp.session.clone();
                            let kind = cp.kind;
                            self.active_checkpoint = None;
                            self.checkpoint_job_map.clear();
                            if let Err(e) = Checkpoint::remove(&session, kind) {
                                tracing::warn!("could not remove completed checkpoint: {e}");
                            }
                        }
                    }
                }
                if let Some(j) = lookup(id) {
                    self.push_log(
                        LogLevel::Success,
                        format!(
                            "complete: {} ({})",
                            j.remote_path,
                            format_bytes(j.bytes_total)
                        ),
                    );
                    // Uploads land new files on the remote side; refresh the
                    // pane so the user sees them. Skip for downloads — the
                    // local pane doesn't auto-refresh on its own either, and
                    // a flood of small downloads would thrash the listing.
                    if j.direction == crate::transfer::Direction::Upload {
                        let path = self.remote.path.clone();
                        self.refresh_remote_pane(path);
                    }
                }
            }
            TransferEvent::Failed { id, error } => {
                // Evict the job from the checkpoint map regardless of the
                // failure reason. For a "cancelled" failure the single-cancel
                // path already removed it above; this is a belt-and-suspenders
                // guard for transport errors and unexpected failures.
                //
                // We do NOT mark the job `done` in the file — a failed job
                // should be re-queued on resume, not silently skipped.
                self.checkpoint_job_map.remove(&id);

                // Failed jobs are left as `in_progress` in the checkpoint
                // file (the `Started` write already flipped them from
                // `pending`). On resume they will be re-queued, which is
                // safe: partial downloads are overwritten, mkdir is
                // idempotent. If the batch was explicitly discarded via
                // batch-cancel, `active_checkpoint` is already None.

                let label = lookup(id)
                    .map(|j| j.remote_path)
                    .unwrap_or_else(|| format!("id={id}"));
                self.push_log(LogLevel::Error, format!("failed: {label}: {error}"));
            }
            TransferEvent::Paused => {
                self.push_log(LogLevel::Warn, "transfers paused".into());
            }
            TransferEvent::Resumed => {
                self.push_log(LogLevel::Info, "transfers resumed".into());
            }
        }
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

    fn refresh_local_pane(&mut self) {
        let path = std::path::PathBuf::from(&self.local.path);
        let mut entries = vec![PaneEntry {
            name: "..".into(),
            is_dir: true,
            size: 0,
            selected: false,
            previewable_image: false,
        }];
        if let Ok(read) = std::fs::read_dir(&path) {
            for entry in read.flatten() {
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let name = entry.file_name().to_string_lossy().to_string();
                let is_dir = meta.is_dir();
                entries.push(PaneEntry {
                    previewable_image: !is_dir && crate::preview::is_previewable_image(&name),
                    name,
                    is_dir,
                    size: if is_dir { 0 } else { meta.len() },
                    selected: false,
                });
            }
        }
        // Directories first, then alpha within each group.
        entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });
        self.local.set_entries(entries);
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

/// Validate a server-supplied file name before using it in a local `Path::join`.
///
/// Returns `None` if the name would escape the intended directory via path
/// separators, a `..` component, or a null byte. Callers must skip the entry.
fn safe_local_name(name: &str) -> Option<&str> {
    if name.is_empty() || name == ".." || name == "." {
        return None;
    }
    if name.bytes().any(|b| matches!(b, b'\0' | b'/' | b'\\')) {
        return None;
    }
    Some(name)
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

// ---------------------------------------------------------------------------
// Walk helpers (recursive transfer planning)
// ---------------------------------------------------------------------------

/// Walk a remote subtree rooted at `remote_root` and produce a flat plan of
/// mkdirs + file downloads with destinations rooted at `local_root`. The
/// plan is iterative-DFS pre-order so directories appear in the plan before
/// any files under them. The dispatcher will run the plan in queue order;
/// because each level's mkdir is enqueued before its file children,
/// `enqueue_mkdir` lands first by virtue of being earlier in the queue.
///
/// Transport-level errors propagate; partial trees are NOT fixed up here —
/// the caller surfaces a `WalkFailed` log line and the user can retry.
async fn walk_remote(
    transport: &mut dyn Transport,
    remote_root: &str,
    local_root: &std::path::Path,
) -> Result<Vec<PlannedJob>> {
    let mut out: Vec<PlannedJob> = Vec::new();
    // Always mkdir the root itself first so the local destination exists
    // before any files inside try to land in it. The transport mkdir is
    // for the remote side, but downloads need local mkdirs — handle those
    // separately via tokio::fs::create_dir_all in the worker. Cleaner here
    // to use a synthetic "ensure local dir" idea: we simply make sure the
    // first job's parent will be created on disk. The download path already
    // calls create_dir_all on the parent, so we don't need explicit local
    // mkdir jobs.

    // Iterative DFS. Stack holds (remote_path_to_visit, local_path_dest).
    let mut stack: Vec<(String, std::path::PathBuf)> =
        vec![(remote_root.to_string(), local_root.to_path_buf())];

    while let Some((remote_dir, local_dir)) = stack.pop() {
        // Ensure the local dir exists ahead of file writes. (The download
        // worker also does create_dir_all on the parent, but doing it here
        // means an empty remote dir still produces a real local dir.)
        if let Err(e) = tokio::fs::create_dir_all(&local_dir).await {
            return Err(crate::error::BlinkError::transport(format!(
                "create local dir {}: {e}",
                local_dir.display()
            )));
        }

        let entries = transport.list(&remote_dir).await?;

        // Pre-collect subdirs so we can push them in reverse for a stable
        // depth-first ordering (leftmost child popped next).
        let mut subdirs: Vec<(String, std::path::PathBuf)> = Vec::new();

        for entry in entries {
            if entry.name == "." || entry.name == ".." {
                continue;
            }
            let safe_name = match safe_local_name(&entry.name) {
                Some(n) => n,
                None => {
                    tracing::warn!("skipping remote entry with unsafe name: {:?}", entry.name);
                    continue;
                }
            };
            let remote_child = transport::join_remote(&remote_dir, &entry.name);
            let local_child = local_dir.join(safe_name);
            match entry.kind {
                EntryKind::Directory => {
                    subdirs.push((remote_child, local_child));
                }
                EntryKind::File | EntryKind::Symlink | EntryKind::Other => {
                    // Treat symlinks as files: we'll fetch whatever they point at.
                    // Other-kinds (sockets, devices) are rare on download targets;
                    // including them here lets the transport surface a real error
                    // rather than silently skipping data.
                    out.push(PlannedJob::Download {
                        remote_path: remote_child,
                        local_path: local_child,
                    });
                }
            }
        }

        for sub in subdirs.into_iter().rev() {
            stack.push(sub);
        }
    }
    Ok(out)
}

/// Walk a local subtree rooted at `local_root` and produce a flat plan of
/// remote mkdirs + file uploads with destinations rooted at `remote_root`.
/// Mirror image of [`walk_remote`].
async fn walk_local(
    local_root: &std::path::Path,
    remote_root: &str,
) -> Result<Vec<PlannedJob>> {
    let mut out: Vec<PlannedJob> = Vec::new();

    // Iterative DFS. Stack holds (local_path_to_visit, remote_path_dest).
    let mut stack: Vec<(std::path::PathBuf, String)> =
        vec![(local_root.to_path_buf(), remote_root.to_string())];

    while let Some((local_dir, remote_dir)) = stack.pop() {
        // Mkdir the destination ahead of any files inside.
        out.push(PlannedJob::Mkdir {
            remote_path: remote_dir.clone(),
        });

        let mut read = tokio::fs::read_dir(&local_dir).await.map_err(|e| {
            crate::error::BlinkError::transport(format!(
                "readdir {}: {e}",
                local_dir.display()
            ))
        })?;

        let mut subdirs: Vec<(std::path::PathBuf, String)> = Vec::new();

        loop {
            let next = read.next_entry().await.map_err(|e| {
                crate::error::BlinkError::transport(format!(
                    "readdir entry {}: {e}",
                    local_dir.display()
                ))
            })?;
            let Some(entry) = next else { break };
            let meta = match entry.metadata().await {
                Ok(m) => m,
                Err(_) => continue, // unreadable entry; skip rather than fail walk
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let local_child = entry.path();
            let remote_child = transport::join_remote(&remote_dir, &name);

            if meta.is_dir() {
                subdirs.push((local_child, remote_child));
            } else if meta.is_file() {
                out.push(PlannedJob::Upload {
                    local_path: local_child,
                    remote_path: remote_child,
                });
            }
            // Symlinks / other types: skip silently for uploads. Following
            // them would be ambiguous (relative target? pointing outside
            // the source tree?) and they're rare for upload payloads.
        }

        for sub in subdirs.into_iter().rev() {
            stack.push(sub);
        }
    }
    Ok(out)
}

/// Local-FS conflict probe for download plans. Iterates every Download job
/// and stat()s its destination; returns the indices whose destinations
/// already exist as files. Mkdir entries are silently merged (creating a
/// dir that exists is a no-op) and don't count as conflicts.
async fn find_download_conflicts(plan: &[PlannedJob]) -> Vec<usize> {
    let mut conflicts = Vec::new();
    for (i, job) in plan.iter().enumerate() {
        if let PlannedJob::Download { local_path, .. } = job {
            if tokio::fs::metadata(local_path).await.is_ok() {
                conflicts.push(i);
            }
        }
    }
    conflicts
}

/// Remote conflict probe for upload plans. Groups the plan's Upload jobs
/// by destination directory, lists each directory once, and matches names
/// in O(dirs) round-trips instead of O(files). Mkdirs aren't conflicts —
/// `transport.mkdir` is idempotent.
///
/// If a destination directory doesn't exist yet, it has no conflicts by
/// definition. We swallow the listing error in that case.
async fn find_upload_conflicts(
    transport: &mut dyn Transport,
    plan: &[PlannedJob],
) -> Result<Vec<usize>> {
    use std::collections::HashMap;

    // Group upload jobs by destination directory.
    let mut by_dir: HashMap<String, Vec<(usize, String)>> = HashMap::new();
    for (i, job) in plan.iter().enumerate() {
        if let PlannedJob::Upload { remote_path, .. } = job {
            let (dir, name) = match remote_path.rsplit_once('/') {
                Some(("", n)) => ("/".to_string(), n.to_string()),
                Some((d, n)) => (d.to_string(), n.to_string()),
                None => (".".to_string(), remote_path.clone()),
            };
            by_dir.entry(dir).or_default().push((i, name));
        }
    }

    let mut conflicts = Vec::new();
    for (dir, entries) in by_dir {
        let listing = match transport.list(&dir).await {
            Ok(l) => l,
            // "Directory doesn't exist yet" — no conflicts there. Other
            // errors also short-circuit to "no conflicts" because the
            // upload itself will surface the real issue with a clear path.
            Err(_) => continue,
        };
        for (i, name) in entries {
            if listing.iter().any(|e| e.name == name) {
                conflicts.push(i);
            }
        }
    }
    conflicts.sort_unstable();
    Ok(conflicts)
}

/// Apply the user's "skip conflicts" choice. Returns the plan with the
/// flagged jobs removed. If skipping a file makes its parent mkdir
/// unnecessary (no remaining files target that directory), we keep the
/// mkdir anyway — it's idempotent on the remote side and the cost is one
/// no-op call. Removing it would require another graph-walk over the plan
/// and the savings aren't worth the complexity.
fn drop_conflicting(plan: Vec<PlannedJob>, conflicts: &[usize]) -> Vec<PlannedJob> {
    use std::collections::HashSet;
    let drop: HashSet<usize> = conflicts.iter().copied().collect();
    plan.into_iter()
        .enumerate()
        .filter(|(i, _)| !drop.contains(i))
        .map(|(_, j)| j)
        .collect()
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
