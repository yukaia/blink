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
use crate::transport::RemoteEntry;

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
    /// A connect task completed successfully. The payload carries the freshly
    /// opened transport and, for FTPS with TOFU, the new cert pin the caller
    /// should persist onto the session.
    Connected(crate::transport::Connected),

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

    /// A local directory enumeration completed successfully. Mirror of
    /// `Listed` for the local pane: read_dir + per-entry metadata is
    /// non-trivial on NFS/SMB mounts, so we run it on a worker task and
    /// post the result back here.
    LocalListed {
        path: String,
        entries: Vec<crate::tui::state::PaneEntry>,
    },

    /// Local directory enumeration failed (typically permission denied or
    /// the path was removed). The pane shows just the `..` entry.
    LocalListFailed { path: String, error: String },

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
    /// exists (file-only; mkdirs are silently merged). `symlinks_skipped`
    /// counts entries deliberately omitted from the plan because they were
    /// symbolic links.
    WalkComplete {
        plan: Vec<crate::tui::plan::PlannedJob>,
        conflict_indices: Vec<usize>,
        symlinks_skipped: usize,
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
        /// Display form of the host (bare for the default SSH port,
        /// `[host]:port` otherwise) — used only for the modal label.
        host: String,
        key_type: String,
        /// SHA-256 fingerprint for display (e.g. `SHA256:abc123…`).
        fingerprint: String,
        decision_tx: tokio::sync::oneshot::Sender<crate::transport::sftp::HostKeyDecision>,
    },

    /// The server's host key does not match the stored one — hard reject.
    /// The App should surface this as a clear error before returning to the
    /// session selector.
    HostKeyChanged {
        /// Display form of the host (bare for the default SSH port,
        /// `[host]:port` otherwise) — used for the modal label.
        host: String,
        /// Raw hostname and port as connected. Carried separately from
        /// `host` because the recovery command the modal prints
        /// (`blink known-hosts remove`) takes them unformatted.
        lookup_host: String,
        lookup_port: u16,
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
    /// Set by [`drain_progress`] when collapsing a burst of progress events
    /// turns up something that isn't progress. Returned ahead of the channel
    /// on the next call so nothing is reordered or dropped.
    deferred: Option<AppEvent>,
}

impl EventStream {
    pub fn new(tick_rate: Duration, app: mpsc::UnboundedReceiver<AppEvent>) -> Self {
        Self {
            crossterm: CrosstermEventStream::new(),
            tick: interval(tick_rate),
            app,
            deferred: None,
        }
    }

    pub async fn next(&mut self) -> Result<Event> {
        // Anything held back by the last coalesce pass goes first.
        if let Some(ev) = self.deferred.take() {
            return Ok(Event::App(ev));
        }
        loop {
            tokio::select! {
                // Bias the arms so keystrokes and app events drain before
                // the tick. Without this, a burst of app events under
                // sustained keypress can be starved by tokio's default
                // pseudo-random arm selection — the symptom is laggy
                // keyboard response while a recursive walk is feeding
                // events. Ordering: key → app → tick.
                biased;
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
                Some(ev) = self.app.recv() => {
                    if matches!(ev, AppEvent::Transfer(TransferEvent::Progress)) {
                        self.deferred = drain_progress(&mut self.app);
                    }
                    return Ok(Event::App(ev));
                }
                _ = self.tick.tick() => return Ok(Event::Tick),
            }
        }
    }
}

/// Drop every immediately-available [`TransferEvent::Progress`] from `app`,
/// returning the first non-progress event found (which must be handed back to
/// the caller, not discarded) or `None` if the channel drained.
///
/// Every app event wakes the run loop into a full `terminal.draw()`, and the
/// transports emit one progress update per chunk per worker — so a large
/// transfer produces redraw requests far faster than the terminal can absorb
/// them and the backlog only grows. Progress carries no payload (byte counts
/// live in `TransferManager`, and the App's handler for it is a no-op), so N
/// queued back-to-back mean exactly what one means: redraw. Draining them
/// makes the redraw rate self-limiting — whatever piled up during the last
/// frame becomes one frame — while still repainting immediately instead of
/// waiting for the next tick.
fn drain_progress(app: &mut mpsc::UnboundedReceiver<AppEvent>) -> Option<AppEvent> {
    while let Ok(ev) = app.try_recv() {
        if matches!(ev, AppEvent::Transfer(TransferEvent::Progress)) {
            continue;
        }
        return Some(ev);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress() -> AppEvent {
        AppEvent::Transfer(TransferEvent::Progress)
    }

    #[test]
    fn drains_a_pure_progress_burst() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        for _ in 0..64 {
            tx.send(progress()).unwrap();
        }
        assert!(drain_progress(&mut rx).is_none(), "nothing to defer");
        assert!(rx.try_recv().is_err(), "burst must be fully consumed");
    }

    #[test]
    fn stops_at_the_first_non_progress_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(progress()).unwrap();
        tx.send(progress()).unwrap();
        tx.send(AppEvent::ConnectFailed("boom".into())).unwrap();
        tx.send(progress()).unwrap();

        // The non-progress event is handed back rather than swallowed...
        match drain_progress(&mut rx) {
            Some(AppEvent::ConnectFailed(msg)) => assert_eq!(msg, "boom"),
            _ => panic!("expected the ConnectFailed event to be deferred"),
        }
        // ...and draining stops there, leaving what followed it in order.
        assert!(
            matches!(rx.try_recv(), Ok(AppEvent::Transfer(TransferEvent::Progress))),
            "events queued after the deferred one must survive",
        );
    }

    #[test]
    fn returns_a_leading_non_progress_event_untouched() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(AppEvent::ConnectKeyNeedsPassphrase).unwrap();
        assert!(
            matches!(drain_progress(&mut rx), Some(AppEvent::ConnectKeyNeedsPassphrase)),
            "a non-progress head must not be dropped",
        );
    }

    #[test]
    fn empty_channel_defers_nothing() {
        let (_tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
        assert!(drain_progress(&mut rx).is_none());
    }
}
