//! Per-pane, per-modal, and per-form state types used by the TUI.
//!
//! These are extracted from `app.rs` so the App struct itself reads as a
//! flat list of fields instead of being interleaved with ~400 lines of
//! supporting types. Nothing in here touches the App struct itself — each
//! type either holds its own data and impls a few small helpers, or is a
//! pure data carrier the modal renderers consume.
//!
//! Grouping is by *use site*: a screen module that's added later (modal
//! dispatch, input handlers per screen) imports the matching state type
//! from here.

use bytes::Bytes;

use crate::session::Session;
use crate::tui::plan::PlannedJob;

// ---------------------------------------------------------------------------
// Pane state (file lists)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Viewer
// ---------------------------------------------------------------------------

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
    /// Decoded text with one-shot syntax tokenisation alongside.
    ///
    /// `tokens` is computed once at load time; every subsequent frame just
    /// reads `tokens[start..end]` instead of re-tokenising the whole prefix
    /// (the viewer redraws on the 100 ms TUI tick, so an un-cached approach
    /// for a 10k-line file at the bottom was ~10k tokenize calls per frame).
    Text {
        lines: Vec<String>,
        tokens: Vec<Vec<(crate::highlight::TokenKind, String)>>,
        scroll: usize,
    },
    /// Raw image bytes, ready to be emitted by a [`crate::preview::PreviewBackend`].
    Image { bytes: Bytes },
    /// Anything we can't render: too big, unknown extension, fetch failed.
    Unsupported(String),
}

/// Where the viewer is fetching its data from.
#[derive(Debug, Clone, Copy)]
pub enum ViewSource {
    Local,
    Remote,
}

// ---------------------------------------------------------------------------
// Pending confirmations
// ---------------------------------------------------------------------------

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

/// Snapshot of the entry being confirmed for deletion.
#[derive(Debug, Clone)]
pub struct PendingDelete {
    pub name: String,
    pub is_dir: bool,
    pub remote_path: String,
}

// ---------------------------------------------------------------------------
// Host-key modals
// ---------------------------------------------------------------------------

/// State for the host-key confirmation modal.
///
/// Holds everything needed to render the prompt and to send the user's
/// decision back to the SFTP connect task via the one-shot channel.
pub struct PendingHostKey {
    pub host: String,
    pub key_type: String,
    pub fingerprint: String,
    /// One-shot sender; consumed exactly once when the user decides.
    pub decision_tx: Option<tokio::sync::oneshot::Sender<crate::transport::sftp::HostKeyDecision>>,
}

impl Drop for PendingHostKey {
    /// If this state is dropped without a decision being sent (e.g. a future
    /// refactor takes the modal state on an error path without resolving the
    /// oneshot), default-deny: the connect task receives Reject and unwinds
    /// cleanly instead of blocking forever on the receiver. Fail-closed
    /// matches the rest of the host-key flow.
    fn drop(&mut self) {
        if let Some(tx) = self.decision_tx.take() {
            let _ = tx.send(crate::transport::sftp::HostKeyDecision::Reject);
        }
    }
}

/// State for the host-key-changed error modal.
#[derive(Debug, Clone)]
pub struct HostKeyChangedInfo {
    /// Display form (bare host for port 22, `[host]:port` otherwise).
    pub host: String,
    /// Raw host and port, for the `blink known-hosts remove` command the
    /// modal prints as the recovery path.
    pub lookup_host: String,
    pub lookup_port: u16,
    pub stored_key_type: String,
    pub presented_key_type: String,
    pub fingerprint: String,
}

// ---------------------------------------------------------------------------
// Edit-session form
// ---------------------------------------------------------------------------

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
    pub fn next(self) -> Self {
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

    pub fn prev(self) -> Self {
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
    pub fn is_text_field(self) -> bool {
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

// ---------------------------------------------------------------------------
// Overwrite confirmations
// ---------------------------------------------------------------------------

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
