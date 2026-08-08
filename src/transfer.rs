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
///
/// Also consulted by the TUI walkers (`walk_remote` / `walk_local`) so the
/// out-of-memory check fires *before* the full plan is materialised, not
/// after.
pub(crate) const MAX_QUEUED_JOBS: usize = 100_000;

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
    /// Number of jobs currently in `Pending` state. Kept in sync with `jobs`
    /// so `enqueue` can check the cap in O(1) instead of scanning the vec.
    pending_count: usize,
    /// Maps job id → index in `jobs` for O(1) lookups by id in `mark` and
    /// `update_progress`. Jobs are never removed from `jobs`, so indices are
    /// stable for the lifetime of the manager.
    job_index: HashMap<u64, usize>,
    parallelism: u8,
    paused: bool,
    /// Abort handles for currently-running worker tasks, keyed by job id.
    /// Populated by [`TransferManager::register_active`] (called from the
    /// dispatcher), removed on completion or cancellation.
    active: HashMap<u64, AbortHandle>,
}

impl TransferManager {
    /// Build a manager with a concurrency cap of `parallelism`.
    ///
    /// The value is clamped to `1..=MAX_PARALLEL` here rather than at the
    /// call sites. The dispatcher's gate is `active >= parallelism()`, so a
    /// zero means `0 >= 0` on every loop iteration: nothing is ever
    /// dispatched and every job sits Pending forever with no error and no
    /// log line. That value is reachable from a hand-edited session file,
    /// which parses the field without a range check — and any future caller
    /// could reintroduce it. Clamping at the single constructor makes the
    /// livelock unrepresentable instead of relying on each caller to check.
    pub fn new(parallelism: u8) -> (Self, mpsc::UnboundedReceiver<TransferEvent>) {
        let parallelism = parallelism.clamp(1, crate::config::MAX_PARALLEL);
        let (tx, rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Mutex::new(Inner {
            next_id: 1,
            next_batch_id: 1,
            jobs: Vec::new(),
            pending_count: 0,
            job_index: HashMap::new(),
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

    /// Snapshot of every job the manager has ever tracked, terminal states
    /// included.
    ///
    /// Clones the full history, which grows for the lifetime of the session.
    /// Render paths must NOT use this — see [`Self::active_jobs`], which is
    /// bounded by the concurrency limit. Currently only the dispatcher tests
    /// need the whole list (to assert every job reached a terminal state).
    #[allow(dead_code)]
    pub fn snapshot(&self) -> Vec<TransferJob> {
        self.inner.lock().jobs.clone()
    }

    /// Look up a single job by id. O(1) via the id → index map — event
    /// handlers should prefer this over cloning the whole list with
    /// [`Self::snapshot`].
    pub fn job(&self, id: u64) -> Option<TransferJob> {
        let inner = self.inner.lock();
        inner.job_index.get(&id).map(|&idx| inner.jobs[idx].clone())
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
        if inner.pending_count >= MAX_QUEUED_JOBS {
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
        let idx = inner.jobs.len();
        inner.jobs.push(job.clone());
        inner.job_index.insert(id, idx);
        inner.pending_count += 1;
        let _ = self.events.send(TransferEvent::Queued(job));
        Some(id)
    }

    /// Mark a job's state. Used by the dispatcher (once it lands).
    pub fn mark(&self, id: u64, state: TransferState) {
        {
            let mut inner = self.inner.lock();
            if let Some(&idx) = inner.job_index.get(&id) {
                let j = &mut inner.jobs[idx];
                let was_pending = j.state == TransferState::Pending;
                j.state = state.clone();
                if was_pending {
                    inner.pending_count = inner.pending_count.saturating_sub(1);
                }
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
        // the progress bar. Only clamp when we actually know the total —
        // an FTP server that doesn't report SIZE leaves bytes_total at 0,
        // and clamping against that would peg the progress display at 0%
        // for the entire transfer.
        let bytes_done = if bytes_total > 0 {
            bytes_done.min(bytes_total)
        } else {
            bytes_done
        };
        {
            let mut inner = self.inner.lock();
            if let Some(&idx) = inner.job_index.get(&id) {
                let j = &mut inner.jobs[idx];
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

    /// Snapshot of only the jobs currently running.
    ///
    /// The render path asks for this every frame. `jobs` retains every job
    /// the manager has ever seen — indices have to stay stable for
    /// `job_index` — so going through [`Self::snapshot`] and filtering
    /// afterwards clones the whole history to keep at most `parallelism`
    /// entries. After a batch of [`MAX_QUEUED_JOBS`] that is 100k structs
    /// (two heap `String`s apiece) copied per frame, at a frame rate the
    /// transfers themselves are driving. Filtering under the lock bounds
    /// the clone by [`crate::config::MAX_PARALLEL`] instead.
    pub fn active_jobs(&self) -> Vec<TransferJob> {
        self.inner
            .lock()
            .jobs
            .iter()
            .filter(|j| j.state == TransferState::Active)
            .cloned()
            .collect()
    }

    /// Count every job that would be abandoned if the session ended now, as
    /// `(active, pending)`.
    ///
    /// Both endings drop queued work as well as running work — quit breaks the
    /// run loop and shuts the dispatcher down, disconnect additionally drops
    /// the manager — so a confirmation that counts only the running jobs
    /// understates what the user is about to lose. Scans without cloning
    /// because the confirmation modals ask on every frame they are open.
    pub fn queue_counts(&self) -> (usize, usize) {
        let inner = self.inner.lock();
        let mut active = 0;
        let mut pending = 0;
        for j in &inner.jobs {
            match j.state {
                TransferState::Active => active += 1,
                TransferState::Pending => pending += 1,
                _ => {}
            }
        }
        (active, pending)
    }

    /// Every job id belonging to `batch_id`, whatever its state.
    ///
    /// Used by the cancel path to find the checkpoint entries that belong to
    /// the batch being abandoned, so they can be marked without disturbing a
    /// batch running the other way.
    pub fn job_ids_in_batch(&self, batch_id: u64) -> Vec<u64> {
        self.inner
            .lock()
            .jobs
            .iter()
            .filter(|j| j.batch_id == Some(batch_id))
            .map(|j| j.id)
            .collect()
    }

    /// Count the Active and Pending jobs belonging to `batch_id`, as
    /// `(active, pending)`. Scans without cloning — this runs on the
    /// batch-cancel keypress, not per frame.
    pub fn batch_counts(&self, batch_id: u64) -> (usize, usize) {
        let inner = self.inner.lock();
        let mut active = 0;
        let mut pending = 0;
        for j in inner.jobs.iter().filter(|j| j.batch_id == Some(batch_id)) {
            match j.state {
                TransferState::Active => active += 1,
                TransferState::Pending => pending += 1,
                _ => {}
            }
        }
        (active, pending)
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
            // Find the index of the first pending job, then update state and
            // pending_count in separate steps to satisfy the borrow checker.
            let idx = inner
                .jobs
                .iter()
                .position(|j| j.state == TransferState::Pending)?;
            inner.jobs[idx].state = TransferState::Active;
            inner.pending_count = inner.pending_count.saturating_sub(1);
            inner.jobs[idx].clone()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> TransferManager {
        // The event receiver is dropped; every `send` then fails silently,
        // which is exactly what the production code already tolerates.
        TransferManager::new(4).0
    }

    // -- parallelism bounds ------------------------------------------------
    //
    // The dispatcher gates on `active >= manager.parallelism()`. A zero here
    // makes that `0 >= 0` on every iteration, so nothing is ever dispatched
    // and every transfer sits Pending forever with no error. The constructor
    // is the single point every caller goes through, so it — not the callers
    // — is where the range has to be enforced.

    #[test]
    fn new_clamps_zero_parallelism_to_one() {
        let (m, _rx) = TransferManager::new(0);
        assert_eq!(
            m.parallelism(),
            1,
            "zero would livelock the dispatcher rather than run serially",
        );
    }

    #[test]
    fn new_clamps_parallelism_to_the_documented_maximum() {
        let (m, _rx) = TransferManager::new(200);
        assert_eq!(m.parallelism(), crate::config::MAX_PARALLEL);
    }

    #[test]
    fn new_leaves_in_range_parallelism_alone() {
        let (m, _rx) = TransferManager::new(4);
        assert_eq!(m.parallelism(), 4);
    }

    #[test]
    fn active_jobs_returns_only_running_ones() {
        let m = manager();
        let queued = m.enqueue_download("/a".into(), "/tmp/a".into()).unwrap();
        m.enqueue_download("/b".into(), "/tmp/b".into()).unwrap();
        let done = m.enqueue_download("/c".into(), "/tmp/c".into()).unwrap();

        m.mark(queued, TransferState::Active);
        m.mark(done, TransferState::Complete);

        let active = m.active_jobs();
        assert_eq!(active.len(), 1, "pending and complete must be excluded");
        assert_eq!(active[0].id, queued);
    }

    #[test]
    fn active_jobs_is_empty_when_nothing_runs() {
        let m = manager();
        m.enqueue_download("/a".into(), "/tmp/a".into()).unwrap();
        assert!(m.active_jobs().is_empty(), "a pending job is not active");
    }

    #[test]
    fn active_jobs_does_not_grow_with_finished_history() {
        // The whole point of filtering under the lock: a long tail of
        // completed jobs must not inflate what the render path clones.
        let m = manager();
        for i in 0..500 {
            let id = m
                .enqueue_download(format!("/f{i}"), format!("/tmp/f{i}").into())
                .unwrap();
            m.mark(id, TransferState::Complete);
        }
        let running = m.enqueue_download("/live".into(), "/tmp/live".into()).unwrap();
        m.mark(running, TransferState::Active);

        assert_eq!(m.active_jobs().len(), 1);
        assert_eq!(m.snapshot().len(), 501, "history itself is still retained");
    }

    #[test]
    fn batch_counts_splits_active_and_pending() {
        let m = manager();
        let batch = m.allocate_batch_id();
        let a = m.enqueue_download_batched("/a".into(), "/tmp/a".into(), batch).unwrap();
        let b = m.enqueue_download_batched("/b".into(), "/tmp/b".into(), batch).unwrap();
        m.enqueue_download_batched("/c".into(), "/tmp/c".into(), batch).unwrap();

        m.mark(a, TransferState::Active);
        m.mark(b, TransferState::Complete);

        // a is Active, b is Complete (counted in neither), c is Pending.
        assert_eq!(m.batch_counts(batch), (1, 1));
    }

    #[test]
    fn batch_counts_ignores_other_batches_and_loose_jobs() {
        let m = manager();
        let mine = m.allocate_batch_id();
        let theirs = m.allocate_batch_id();
        m.enqueue_download_batched("/a".into(), "/tmp/a".into(), mine).unwrap();
        m.enqueue_download_batched("/b".into(), "/tmp/b".into(), theirs).unwrap();
        m.enqueue_download("/loose".into(), "/tmp/loose".into()).unwrap();

        assert_eq!(m.batch_counts(mine), (0, 1));
        assert_eq!(m.batch_counts(theirs), (0, 1));
    }

    #[test]
    fn queue_counts_splits_active_from_pending() {
        let m = manager();
        let a = m.enqueue_download("/a".into(), "/tmp/a".into()).unwrap();
        let b = m.enqueue_download("/b".into(), "/tmp/b".into()).unwrap();
        m.enqueue_download("/c".into(), "/tmp/c".into()).unwrap();
        m.mark(a, TransferState::Active);
        m.mark(b, TransferState::Complete);

        // a Active, b Complete (counted in neither), c Pending.
        assert_eq!(m.queue_counts(), (1, 1));
    }

    #[test]
    fn queue_counts_sees_queued_work_with_nothing_running() {
        // The case the confirmations used to miss entirely: a large batch sat
        // queued behind the concurrency limit still gets thrown away on quit.
        let m = manager();
        for i in 0..50 {
            m.enqueue_download(format!("/f{i}"), format!("/tmp/f{i}").into())
                .unwrap();
        }
        assert_eq!(m.queue_counts(), (0, 50));
    }

    #[test]
    fn queue_counts_ignores_finished_history() {
        let m = manager();
        for i in 0..20 {
            let id = m
                .enqueue_download(format!("/f{i}"), format!("/tmp/f{i}").into())
                .unwrap();
            m.mark(id, TransferState::Complete);
        }
        assert_eq!(m.queue_counts(), (0, 0), "nothing left to cancel");
    }

    #[test]
    fn batch_counts_unknown_batch_is_zero() {
        let m = manager();
        m.enqueue_download("/a".into(), "/tmp/a".into()).unwrap();
        assert_eq!(m.batch_counts(9999), (0, 0));
    }
}
