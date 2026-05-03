//! Event multiplexer.
//!
//! Streams keyboard / resize events from crossterm, ticks from a timer, and
//! asynchronous results ([`AppEvent`]) from background tasks into a single
//! source for the App to consume.

use std::time::Duration;

use bytes::Bytes;
use crossterm::event::{
    Event as CrosstermEvent, EventStream as CrosstermEventStream, KeyEvent, KeyEventKind,
};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::time::{interval, Interval};

use crate::error::Result;
use crate::preview::FileViewKind;
use crate::transfer::{Direction, TransferEvent};
use crate::transport::{RemoteEntry, Transport};

/// Top-level event consumed by the App run loop.
pub enum Event {
    Key(KeyEvent),
    Tick,
    #[allow(dead_code)]
    Resize(u16, u16),
    App(AppEvent),
}

/// Asynchronous results delivered back from spawned tasks.
///
/// `AppEvent` is intentionally NOT `Debug` / `Clone`: [`AppEvent::Connected`]
/// carries an owned `Box<dyn Transport>` that can't sensibly be cloned, and
/// debug-formatting a transport is meaningless.
pub enum AppEvent {
    /// A connect task completed successfully. The payload is the freshly
    /// opened transport, ready to be used.
    Connected(Box<dyn Transport>),

    /// A connect task failed.
    ConnectFailed(String),

    /// A connect task failed because the configured SSH key is encrypted and
    /// no passphrase was supplied (or the supplied one was wrong). The App
    /// transitions to the passphrase prompt; on retry, the cached
    /// `pending_session` is reused.
    ConnectKeyNeedsPassphrase,

    /// A directory listing completed successfully.
    Listed { path: String, entries: Vec<RemoteEntry> },

    /// A directory listing failed.
    ListFailed { path: String, error: String },

    /// A rename completed successfully.
    Renamed { from: String, to: String },

    /// A rename failed.
    RenameFailed { from: String, #[allow(dead_code)] to: String, error: String },

    /// A remote mkdir completed successfully.
    MkdirDone { path: String },

    /// A remote mkdir failed.
    MkdirFailed { path: String, error: String },

    /// A delete completed successfully.
    Deleted { name: String },

    /// A delete failed.
    DeleteFailed { name: String, error: String },

    /// A recursive walk finished. The plan is a flat list of jobs to enqueue
    /// in order: directory creations come before any files inside them.
    /// `conflict_indices` lists positions in `plan` whose destination already
    /// exists (file-only; mkdirs are silently merged).
    WalkComplete {
        plan: Vec<crate::tui::app::PlannedJob>,
        conflict_indices: Vec<usize>,
        kind: Direction,
    },

    /// A recursive walk failed.
    WalkFailed {
        error: String,
        kind: Direction,
    },

    /// File contents fetched for the viewer.
    ViewLoaded {
        name: String,
        kind: FileViewKind,
        bytes: Bytes,
    },

    /// File-fetch for the viewer failed.
    ViewFailed { name: String, error: String },

    /// The SFTP/SCP transport encountered an unknown host key and needs the
    /// user to decide whether to trust it. The `decision_tx` sender must be
    /// resolved (by sending a [`crate::transport::sftp::HostKeyDecision`])
    /// before the connect task can proceed.
    HostKeyUnknown {
        /// `host:port` string, matching the known-hosts file key format.
        host: String,
        key_type: String,
        key_b64: String,
        /// SHA-256 fingerprint for display (e.g. `SHA256:abc123…`).
        fingerprint: String,
        decision_tx: tokio::sync::oneshot::Sender<crate::transport::sftp::HostKeyDecision>,
    },

    /// The server's host key does not match the stored one — hard reject.
    /// The App should surface this as a clear error before returning to the
    /// session selector.
    HostKeyChanged {
        host: String,
        stored_key_type: String,
        presented_key_type: String,
        fingerprint: String,
    },

    /// Transfer dispatcher emitted an event. Reserved for the next wiring pass.
    #[allow(dead_code)]
    Transfer(TransferEvent),
}

pub struct EventStream {
    crossterm: CrosstermEventStream,
    tick: Interval,
    app: mpsc::UnboundedReceiver<AppEvent>,
}

impl EventStream {
    pub fn new(tick_rate: Duration, app: mpsc::UnboundedReceiver<AppEvent>) -> Self {
        Self {
            crossterm: CrosstermEventStream::new(),
            tick: interval(tick_rate),
            app,
        }
    }

    pub async fn next(&mut self) -> Result<Event> {
        loop {
            tokio::select! {
                _ = self.tick.tick() => return Ok(Event::Tick),
                ev = self.crossterm.next() => {
                    match ev {
                        Some(Ok(CrosstermEvent::Key(k))) => {
                            // Filter out key-release / repeat events on Windows.
                            if k.kind == KeyEventKind::Press {
                                return Ok(Event::Key(k));
                            }
                        }
                        Some(Ok(CrosstermEvent::Resize(w, h))) => {
                            return Ok(Event::Resize(w, h));
                        }
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => return Err(e.into()),
                        None => return Ok(Event::Tick),
                    }
                }
                Some(ev) = self.app.recv() => return Ok(Event::App(ev)),
            }
        }
    }
}
