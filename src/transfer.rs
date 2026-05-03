//! Parallel transfer manager.
//!
//! Owns a queue of pending transfers and exposes the slot count for the UI to
//! render. The [`dispatcher`] submodule pulls pending jobs and runs them
//! against the [`crate::transport`] layer.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

pub mod dispatcher;
pub use dispatcher::Dispatcher;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Download,
    Upload,
    /// Create a remote directory. Used as a planned step within a recursive
    /// upload, ahead of the file transfers that land inside it.
    CreateDir,
}

#[derive(Debug, Clone)]
pub struct TransferJob {
    pub id: u64,
    pub direction: Direction,
    pub remote_path: String,
    pub local_path: PathBuf,
    pub bytes_total: u64,
    pub bytes_done: u64,
    /// Most recent transfer rate sample. Updated by the dispatcher's progress
    /// forwarder on a ~250ms cadence; the strip in the main view reads this
    /// directly from [`TransferManager::snapshot`].
    pub bytes_per_sec: u64,
    pub state: TransferState,
    /// Identifier shared by every job that came out of the same walk (one
    /// `Ctrl-D` / `Ctrl-U` of a directory). `None` for ad-hoc single-file
    /// enqueues. Used by the Transfers pane's "cancel whole batch" gesture.
    pub batch_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    Pending,
    Active,
    #[allow(dead_code)]
    Paused,
    Complete,
    Failed(String),
}

#[derive(Debug, Clone)]
pub enum TransferEvent {
    Queued(TransferJob),
    Started(u64),
    /// Progress signal: a transfer made forward progress.
    /// The UI reads actual byte counts from [`TransferManager::snapshot`].
    Progress,
    Complete(u64),
    Failed {
        id: u64,
        error: String,
    },
    Paused,
    Resumed,
}

/// Upper bound on the number of pending (not-yet-started) jobs the queue
/// will accept. Prevents a malicious server directory listing from causing
/// unbounded memory growth via recursive download planning.
const MAX_QUEUED_JOBS: usize = 100_000;

/// Manages a queue of jobs and a configurable concurrency cap.
///
/// We don't use Tokio's Semaphore directly because we want to expose the queue
/// snapshot to the UI for rendering, which requires our own bookkeeping.
pub struct TransferManager {
    inner: Arc<Mutex<Inner>>,
    events: mpsc::UnboundedSender<TransferEvent>,
}

struct Inner {
    next_id: u64,
    next_batch_id: u64,
    jobs: Vec<TransferJob>,
    parallelism: u8,
    paused: bool,
    /// Abort handles for currently-running worker tasks, keyed by job id.
    /// Populated by [`TransferManager::register_active`] (called from the
    /// dispatcher), removed on completion or cancellation.
    active: HashMap<u64, AbortHandle>,
}

impl TransferManager {
    pub fn new(parallelism: u8) -> (Self, mpsc::UnboundedReceiver<TransferEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Mutex::new(Inner {
            next_id: 1,
            next_batch_id: 1,
            jobs: Vec::new(),
            parallelism,
            paused: false,
            active: HashMap::new(),
        }));
        (
            Self {
                inner,
                events: tx,
            },
            rx,
        )
    }

    /// Adjust concurrency at runtime. New jobs honour the new limit; running
    /// jobs are not interrupted.
    #[allow(dead_code)]
    pub fn set_parallelism(&self, n: u8) {
        let n = n.clamp(1, crate::config::MAX_PARALLEL);
        self.inner.lock().parallelism = n;
    }

    /// Snapshot of every tracked job (for rendering).
    pub fn snapshot(&self) -> Vec<TransferJob> {
        self.inner.lock().jobs.clone()
    }

    pub fn pause(&self) {
        self.inner.lock().paused = true;
        let _ = self.events.send(TransferEvent::Paused);
    }

    pub fn resume(&self) {
        self.inner.lock().paused = false;
        let _ = self.events.send(TransferEvent::Resumed);
    }

    pub fn is_paused(&self) -> bool {
        self.inner.lock().paused
    }

    /// Queue a new download. Returns the assigned job id, or `None` if the
    /// pending-job cap ([`MAX_QUEUED_JOBS`]) has been reached.
    pub fn enqueue_download(&self, remote_path: String, local_path: PathBuf) -> Option<u64> {
        self.enqueue(Direction::Download, remote_path, local_path, None)
    }

    /// Queue a new upload. Returns the assigned job id, or `None` if the cap
    /// has been reached.
    pub fn enqueue_upload(&self, local_path: PathBuf, remote_path: String) -> Option<u64> {
        self.enqueue(Direction::Upload, remote_path, local_path, None)
    }

    /// Queue a remote-side `mkdir`. The `local_path` field is unused for this
    /// direction; we pass an empty PathBuf to satisfy the shared shape.
    pub fn enqueue_mkdir(&self, remote_path: String) -> Option<u64> {
        self.enqueue(Direction::CreateDir, remote_path, PathBuf::new(), None)
    }

    /// Reserve a fresh batch id. Subsequent calls to `enqueue_*_batched`
    /// using this id stamp every job with the same value, which lets the
    /// UI's "cancel whole batch" gesture find them by group.
    pub fn allocate_batch_id(&self) -> u64 {
        let mut inner = self.inner.lock();
        let id = inner.next_batch_id;
        inner.next_batch_id += 1;
        id
    }

    /// Queue a download as part of a batch. See [`allocate_batch_id`].
    pub fn enqueue_download_batched(
        &self,
        remote_path: String,
        local_path: PathBuf,
        batch_id: u64,
    ) -> Option<u64> {
        self.enqueue(Direction::Download, remote_path, local_path, Some(batch_id))
    }

    /// Queue an upload as part of a batch. See [`allocate_batch_id`].
    pub fn enqueue_upload_batched(
        &self,
        local_path: PathBuf,
        remote_path: String,
        batch_id: u64,
    ) -> Option<u64> {
        self.enqueue(Direction::Upload, remote_path, local_path, Some(batch_id))
    }

    /// Queue an mkdir as part of a batch. See [`allocate_batch_id`].
    pub fn enqueue_mkdir_batched(&self, remote_path: String, batch_id: u64) -> Option<u64> {
        self.enqueue(
            Direction::CreateDir,
            remote_path,
            PathBuf::new(),
            Some(batch_id),
        )
    }

    fn enqueue(
        &self,
        direction: Direction,
        remote_path: String,
        local_path: PathBuf,
        batch_id: Option<u64>,
    ) -> Option<u64> {
        let mut inner = self.inner.lock();
        // Cap the number of pending jobs so a large server directory listing
        // cannot grow the queue without bound and exhaust memory.
        let pending = inner
            .jobs
            .iter()
            .filter(|j| j.state == TransferState::Pending)
            .count();
        if pending >= MAX_QUEUED_JOBS {
            return None;
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let job = TransferJob {
            id,
            direction,
            remote_path,
            local_path,
            bytes_total: 0,
            bytes_done: 0,
            bytes_per_sec: 0,
            state: TransferState::Pending,
            batch_id,
        };
        inner.jobs.push(job.clone());
        let _ = self.events.send(TransferEvent::Queued(job));
        Some(id)
    }

    /// Mark a job's state. Used by the dispatcher (once it lands).
    pub fn mark(&self, id: u64, state: TransferState) {
        {
            let mut inner = self.inner.lock();
            if let Some(j) = inner.jobs.iter_mut().find(|j| j.id == id) {
                j.state = state.clone();
            }
        }
        match state {
            TransferState::Active => {
                let _ = self.events.send(TransferEvent::Started(id));
            }
            TransferState::Complete => {
                let _ = self.events.send(TransferEvent::Complete(id));
            }
            TransferState::Failed(e) => {
                let _ = self.events.send(TransferEvent::Failed { id, error: e });
            }
            _ => {}
        }
    }

    pub fn update_progress(&self, id: u64, bytes_done: u64, bytes_total: u64, bytes_per_sec: u64) {
        // Clamp so a server reporting bytes_done > bytes_total cannot overflow
        // the progress bar or cause panics in percentage arithmetic.
        let bytes_done = bytes_done.min(bytes_total);
        {
            let mut inner = self.inner.lock();
            if let Some(j) = inner.jobs.iter_mut().find(|j| j.id == id) {
                j.bytes_done = bytes_done;
                j.bytes_total = bytes_total;
                j.bytes_per_sec = bytes_per_sec;
            }
        }
        let _ = self.events.send(TransferEvent::Progress);
    }

    #[allow(dead_code)]
    pub fn pending_jobs(&self) -> Vec<TransferJob> {
        self.inner
            .lock()
            .jobs
            .iter()
            .filter(|j| j.state == TransferState::Pending)
            .cloned()
            .collect()
    }

    /// Atomically claim the first pending job for a worker: marks it `Active`,
    /// emits a `Started` event, and returns a clone. Returns `None` when the
    /// queue holds nothing pending.
    ///
    /// This is the single transition point from `Pending` → `Active`. Callers
    /// must NOT also call `mark(id, Active)` on the returned job — doing so
    /// would emit a duplicate `Started` event.
    pub fn take_next_pending(&self) -> Option<TransferJob> {
        let cloned = {
            let mut inner = self.inner.lock();
            match inner
                .jobs
                .iter_mut()
                .find(|j| j.state == TransferState::Pending)
            {
                Some(j) => {
                    j.state = TransferState::Active;
                    j.clone()
                }
                None => return None,
            }
        };
        let _ = self.events.send(TransferEvent::Started(cloned.id));
        Some(cloned)
    }

    /// Register an [`AbortHandle`] for a running worker so the manager can
    /// cancel it later. Called by the dispatcher immediately after spawning.
    pub fn register_active(&self, id: u64, handle: AbortHandle) {
        self.inner.lock().active.insert(id, handle);
    }

    /// Remove a worker's entry from the active map. Returns `true` if the
    /// entry was present (the natural-completion path), `false` if the entry
    /// had already been removed by [`cancel`] (cancellation won the race).
    pub fn deregister_active(&self, id: u64) -> bool {
        self.inner.lock().active.remove(&id).is_some()
    }

    /// Cancel a running transfer by id. Aborts the worker task at its next
    /// `.await` point and marks the job as failed with a "cancelled" reason.
    /// No-op if the id isn't currently running.
    ///
    /// Cancellation may leave a partial file on disk for downloads in flight.
    pub fn cancel(&self, id: u64) {
        let handle = self.inner.lock().active.remove(&id);
        if let Some(h) = handle {
            h.abort();
            self.mark(id, TransferState::Failed("cancelled".into()));
        }
    }

    /// Cancel every Active and Pending job sharing `batch_id`. Returns
    /// `(active_cancelled, pending_cancelled)`.
    ///
    /// Active jobs go through the usual `cancel` path (AbortHandle + mark
    /// failed). Pending jobs are marked failed in place — they would have
    /// been claimed by the dispatcher's `take_next_pending` eventually, so
    /// flipping their state is enough to skip them. Complete and previously-
    /// failed jobs are ignored.
    pub fn cancel_batch(&self, batch_id: u64) -> (usize, usize) {
        // Phase 1: collect ids while holding the lock briefly. We can't
        // call `cancel` / `mark` with the lock held because they take it
        // again internally.
        let (active_ids, pending_ids): (Vec<u64>, Vec<u64>) = {
            let inner = self.inner.lock();
            let active: Vec<u64> = inner
                .jobs
                .iter()
                .filter(|j| {
                    j.batch_id == Some(batch_id)
                        && matches!(j.state, TransferState::Active)
                })
                .map(|j| j.id)
                .collect();
            let pending: Vec<u64> = inner
                .jobs
                .iter()
                .filter(|j| {
                    j.batch_id == Some(batch_id)
                        && matches!(j.state, TransferState::Pending)
                })
                .map(|j| j.id)
                .collect();
            (active, pending)
        };

        let active_n = active_ids.len();
        let pending_n = pending_ids.len();

        for id in active_ids {
            self.cancel(id);
        }
        for id in pending_ids {
            self.mark(id, TransferState::Failed("cancelled".into()));
        }
        (active_n, pending_n)
    }

    pub fn parallelism(&self) -> u8 {
        self.inner.lock().parallelism
    }
}

impl Clone for TransferManager {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            events: self.events.clone(),
        }
    }
}

/// Format a byte rate for display (e.g. "3.2 MB/s").
pub fn format_bytes_per_sec(bytes: u64) -> String {
    format_bytes(bytes) + "/s"
}

/// Format a byte size for display ("1.2 MB", "234 KB").
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * KB;
    const GB: f64 = 1024.0 * MB;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GiB", b / GB)
    } else if b >= MB {
        format!("{:.1} MiB", b / MB)
    } else if b >= KB {
        format!("{:.0} KiB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

pub fn format_eta(bytes_remaining: u64, bytes_per_sec: u64) -> String {
    if bytes_per_sec == 0 {
        return "—".into();
    }
    let total_secs = bytes_remaining.max(1) / bytes_per_sec.max(1);
    if total_secs == 0 {
        return "0:01".into();
    }
    let m = total_secs / 60;
    let s = total_secs % 60;
    format!("{m}:{s:02}")
}
