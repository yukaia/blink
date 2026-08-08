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

/// One row in a file pane.
///
/// Carries the same two-name split as [`crate::transport::RemoteEntry`], and
/// for the same reason: the string that is safe to render is lossy, so it
/// cannot be the string blink uses to open, download, rename, or delete the
/// file. Build with [`PaneEntry::new`] so the two can never drift.
#[derive(Debug, Clone)]
pub struct PaneEntry {
    /// The name as the source reported it. Use for every path.
    pub raw_name: String,
    /// Sanitized for terminal rendering. Never use to address anything.
    pub display_name: String,
    pub is_dir: bool,
    pub size: u64,
    pub previewable_image: bool,
}

impl PaneEntry {
    pub fn new(raw_name: String, is_dir: bool, size: u64) -> Self {
        let display_name = crate::error::sanitize(raw_name.clone());
        let previewable_image =
            !is_dir && crate::preview::is_previewable_image(&raw_name);
        Self {
            raw_name,
            display_name,
            is_dir,
            size,
            previewable_image,
        }
    }

    /// The synthetic `..` row every pane shows below the root.
    pub fn parent() -> Self {
        Self::new("..".to_string(), true, 0)
    }

    /// Whether this is the synthetic parent row, which no transfer or
    /// delete should ever act on.
    pub fn is_parent(&self) -> bool {
        self.raw_name == ".."
    }
}

#[derive(Debug, Clone)]
pub struct PaneState {
    pub path: String,
    /// Currently visible entries. When [`filter`] is set, this is the filtered
    /// subset; otherwise it's the full list.
    pub entries: Vec<PaneEntry>,
    pub cursor: usize,
    /// Active substring filter, if any. Case-insensitive match against
    /// `entry.display_name` — the user filters on what they can read. The
    /// `..` parent entry is always retained so the user can
    /// navigate out of a filtered view.
    pub filter: Option<String>,
    /// Full unfiltered list, stashed while a filter is active so we can
    /// restore on clear (and re-apply on refresh).
    all_entries: Option<Vec<PaneEntry>>,
    /// Raw names of the selected entries.
    ///
    /// Deliberately *not* a flag on [`PaneEntry`]. Filtering stashes a clone
    /// of the rows in `all_entries` and rebuilds `entries` from it, so a flag
    /// living on a row is discarded the moment the filter changes or clears —
    /// which silently threw away every selection made while a filter was
    /// active. Keying by name survives every rebuild of the row list.
    ///
    /// A name is unique within a directory on every protocol blink speaks, so
    /// it identifies a row unambiguously. Cleared by [`Self::set_entries`],
    /// i.e. whenever a new listing arrives.
    selected: std::collections::HashSet<String>,
}

impl PaneState {
    pub fn empty() -> Self {
        Self {
            path: String::new(),
            entries: Vec::new(),
            cursor: 0,
            filter: None,
            all_entries: None,
            selected: std::collections::HashSet::new(),
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
        let Some(e) = self.entries.get(self.cursor) else {
            return;
        };
        // `..` is a navigation affordance, not a file — see `selection`.
        if e.is_parent() {
            return;
        }
        if !self.selected.remove(&e.raw_name) {
            self.selected.insert(e.raw_name.clone());
        }
    }

    /// Whether `entry` is selected. Used by the renderer.
    pub fn is_selected(&self, entry: &PaneEntry) -> bool {
        self.selected.contains(&entry.raw_name)
    }

    /// How many entries are selected, including any currently hidden by the
    /// filter — the footer should report what a transfer would act on, not
    /// what happens to be on screen.
    pub fn selected_count(&self) -> usize {
        self.selected.len()
    }

    /// Total size of the selected entries, filter-hidden ones included.
    pub fn selected_size(&self) -> u64 {
        self.all_rows()
            .iter()
            .filter(|e| self.is_selected(e))
            .map(|e| e.size)
            .sum()
    }

    /// Drop every selection.
    pub fn clear_selection(&mut self) {
        self.selected.clear();
    }

    /// Every row in this directory, filter or no filter.
    fn all_rows(&self) -> &[PaneEntry] {
        self.all_entries.as_deref().unwrap_or(&self.entries)
    }

    /// The entries a transfer should act on, as `(raw_name, is_dir)`.
    ///
    /// Explicitly selected entries win; with nothing selected, the cursor
    /// entry stands in. The `..` row is excluded from both — it is a
    /// navigation affordance, not a file, and treating it as a transfer root
    /// resolves to the *parent* directory. The cursor path always guarded
    /// against that; the selection path did not, so selecting `..` and
    /// pressing ctrl+u walked and uploaded the whole parent tree.
    ///
    /// Shared by the upload and download paths so the two cannot drift on
    /// which entries they consider.
    pub fn selection(&self) -> Vec<(String, bool)> {
        let selected: Vec<(String, bool)> = self
            .all_rows()
            .iter()
            .filter(|e| self.is_selected(e) && !e.is_parent())
            .map(|e| (e.raw_name.clone(), e.is_dir))
            .collect();
        if !selected.is_empty() {
            return selected;
        }
        match self.entries.get(self.cursor) {
            Some(e) if !e.is_parent() => vec![(e.raw_name.clone(), e.is_dir)],
            _ => Vec::new(),
        }
    }

    /// Replace the underlying entry list. If a filter is active it gets
    /// re-applied against the new list, so refresh-while-filtered keeps the
    /// view narrow. Cursor is clamped to the new range.
    pub fn set_entries(&mut self, entries: Vec<PaneEntry>) {
        // A new listing means new rows; carrying selections across a refresh
        // or a navigation would be surprising. Filtering deliberately does
        // *not* come through here.
        self.selected.clear();
        if let Some(query) = self.filter.clone() {
            let lower = query.to_ascii_lowercase();
            let filtered: Vec<PaneEntry> = entries
                .iter()
                .filter(|e| {
                    e.is_parent() || e.display_name.to_ascii_lowercase().contains(&lower)
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
                e.is_parent() || e.display_name.to_ascii_lowercase().contains(&lower)
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

#[cfg(test)]
mod pane_tests {
    use super::*;

    fn file(name: &str) -> PaneEntry {
        PaneEntry::new(name.to_string(), false, 1)
    }

    fn pane(entries: Vec<PaneEntry>) -> PaneState {
        let mut p = PaneState::empty();
        p.set_entries(entries);
        p
    }

    #[test]
    fn selection_falls_back_to_the_cursor_entry() {
        let mut p = pane(vec![PaneEntry::parent(), file("a.txt"), file("b.txt")]);
        p.cursor = 2;
        assert_eq!(p.selection(), vec![("b.txt".to_string(), false)]);
    }

    #[test]
    fn selection_prefers_explicitly_selected_entries() {
        let mut p = pane(vec![PaneEntry::parent(), file("a.txt"), file("b.txt")]);
        p.cursor = 1;
        p.cursor = 2;
        p.toggle_selected();
        p.cursor = 1;
        assert_eq!(p.selection(), vec![("b.txt".to_string(), false)]);
    }

    #[test]
    fn selection_never_includes_the_parent_row() {
        // `..` is a navigation affordance, not a file. Selecting it and
        // pressing ctrl+u used to walk and upload the entire parent
        // directory — the cursor path guarded against it, the selection
        // path did not.
        let mut p = pane(vec![PaneEntry::parent(), file("a.txt")]);
        p.cursor = 0;
        p.toggle_selected(); // the parent row — must be refused
        p.cursor = 1;
        p.toggle_selected();
        assert_eq!(
            p.selection(),
            vec![("a.txt".to_string(), false)],
            "the parent row must never become a transfer root",
        );
    }

    #[test]
    fn selection_on_the_parent_row_alone_is_empty() {
        let mut p = pane(vec![PaneEntry::parent(), file("a.txt")]);
        p.cursor = 0;
        assert!(p.selection().is_empty(), "nothing to transfer");
    }

    #[test]
    fn selection_uses_raw_names() {
        let mut p = pane(vec![file("re\u{202E}port.txt")]);
        p.cursor = 0;
        assert_eq!(
            p.selection(),
            vec![("re\u{202E}port.txt".to_string(), false)],
            "transfers address the real name, not the rendered one",
        );
    }

    #[test]
    fn selection_is_empty_for_an_empty_pane() {
        let p = PaneState::empty();
        assert!(p.selection().is_empty());
    }

    // -- selection vs. filtering -------------------------------------------
    //
    // Selection used to live as a flag on the entry rows, and `set_filter`
    // stashes a *clone* of those rows. Toggling touched the filtered copies,
    // and `clear_filter` restored the stale clone — so every selection made
    // while a filter was active was silently discarded. Leaving search with
    // Esc is exactly that path: the user selects ten files, presses Esc,
    // ctrl+d, and gets the one under the cursor.

    #[test]
    fn selections_made_while_filtered_survive_clearing_the_filter() {
        let mut p = pane(vec![
            PaneEntry::parent(),
            file("alpha.txt"),
            file("alpha2.txt"),
            file("beta.txt"),
        ]);
        p.set_filter("alpha".into());
        assert_eq!(p.entries.len(), 3, "parent plus the two alphas");

        p.cursor = 1;
        p.toggle_selected();
        p.cursor = 2;
        p.toggle_selected();

        p.clear_filter();

        let mut names: Vec<String> = p.selection().into_iter().map(|(n, _)| n).collect();
        names.sort();
        assert_eq!(
            names,
            vec!["alpha.txt".to_string(), "alpha2.txt".to_string()],
            "selections must outlive the filter that was active when they were made",
        );
    }

    #[test]
    fn selections_survive_narrowing_the_filter() {
        let mut p = pane(vec![file("alpha.txt"), file("alpha2.txt")]);
        p.set_filter("alpha".into());
        p.cursor = 0;
        p.toggle_selected();
        p.cursor = 1;
        p.toggle_selected();

        // Narrow so only one of them is visible.
        p.set_filter("alpha2".into());
        assert_eq!(p.entries.len(), 1);

        assert_eq!(
            p.selection().len(),
            2,
            "a selection hidden by the filter is still selected",
        );
        assert_eq!(p.selected_count(), 2, "and the footer must say so");
    }

    #[test]
    fn refreshing_the_pane_clears_the_selection() {
        // Unchanged behaviour: a new listing means new rows, and carrying
        // selections across a refresh or a navigation would be surprising.
        let mut p = pane(vec![file("a.txt")]);
        p.cursor = 0;
        p.toggle_selected();
        assert_eq!(p.selected_count(), 1);

        p.set_entries(vec![file("a.txt"), file("b.txt")]);
        assert_eq!(p.selected_count(), 0, "a fresh listing starts unselected");
    }

    #[test]
    fn toggling_twice_deselects() {
        let mut p = pane(vec![file("a.txt")]);
        p.cursor = 0;
        p.toggle_selected();
        p.toggle_selected();
        assert_eq!(p.selected_count(), 0);
        assert!(p.selection().len() == 1, "falls back to the cursor entry");
    }

    #[test]
    fn the_parent_row_cannot_be_selected() {
        let mut p = pane(vec![PaneEntry::parent(), file("a.txt")]);
        p.cursor = 0;
        p.toggle_selected();
        assert_eq!(p.selected_count(), 0, "`..` is not a file");
    }

    #[test]
    fn is_selected_tracks_the_toggle() {
        let mut p = pane(vec![file("a.txt"), file("b.txt")]);
        p.cursor = 0;
        p.toggle_selected();
        assert!(p.is_selected(&p.entries[0]));
        assert!(!p.is_selected(&p.entries[1]));
    }

    #[test]
    fn clear_selection_empties_it() {
        let mut p = pane(vec![file("a.txt"), file("b.txt")]);
        p.cursor = 0;
        p.toggle_selected();
        p.cursor = 1;
        p.toggle_selected();
        p.clear_selection();
        assert_eq!(p.selected_count(), 0);
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
    /// Decoded text, stored purely as one-shot syntax tokenisation.
    ///
    /// `tokens` is computed once at load time; every subsequent frame just
    /// reads `tokens[start..end]` instead of re-tokenising the whole prefix
    /// (the viewer redraws on the 100 ms TUI tick, so an un-cached approach
    /// for a 10k-line file at the bottom was ~10k tokenize calls per frame).
    ///
    /// One entry per line, and the spans of a line concatenate back to that
    /// line — so `tokens.len()` is the line count and no separate `Vec<String>`
    /// of lines is kept. Holding both meant a file up to the 1 MB viewer limit
    /// sat in memory twice, and the copy was read only for its `.len()`.
    Text {
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
// Post-connect offers
// ---------------------------------------------------------------------------

/// Something to ask the user immediately after a connection comes up.
///
/// Held in a queue so one place owns "what happens after connect" — the
/// order matters, and splitting it across the handlers that answer each
/// offer is how that ordering drifts.
#[derive(Debug, Clone)]
pub enum PostConnectOffer {
    /// A previous batch left work unfinished. Carries its summary.
    ResumeCheckpoint(crate::checkpoint::CheckpointOffer),
    /// The connection isn't backed by a saved session.
    SaveSession,
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
