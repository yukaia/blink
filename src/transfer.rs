//! Parallel transfer manager.
//!
//! Owns a queue of pending transfers and exposes the slot count for the UI to
//! render. The [`dispatcher`] submodule pulls pending jobs and runs them
//! against the [`crate::transport`] layer.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

pub mod dispatcher;
pub use dispatcher::Dispatcher;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// What happened to the queue. A job's events carry the job as it was at
/// that moment, so a handler never has to ask the manager for it: the
/// manager drops old finished jobs (see [`RETAINED_JOBS`]), and an
/// interface that fell behind would find nothing there.
#[derive(Debug, Clone)]
pub enum TransferEvent {
    Queued(TransferJob),
    Started(TransferJob),
    /// Progress signal: a transfer made forward progress.
    /// The UI reads actual byte counts from [`TransferManager::snapshot`].
    Progress,
    Complete(TransferJob),
    Failed {
        job: TransferJob,
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

/// Finished jobs the manager keeps once it holds more than this many jobs.
/// They are dropped oldest first. Keeping every job ever queued grew memory
/// for the life of the session — a few hundred bytes a job, so tens of MB
/// per [`MAX_QUEUED_JOBS`] batch. A job's events are handled well before
/// this many newer jobs could finish, so nothing still asks about one gone.
const RETAINED_JOBS: usize = 4096;

/// Pending jobs [`TransferManager::take_next_pending`] looks past when the
/// ones ahead are waiting for a folder to be made. Bounds the scan, which
/// runs under the lock the renderer takes every frame. The first pending job
/// never waits on anything behind it, so a full window of waiting jobs only
/// lowers parallelism until a mkdir finishes; it never stalls the queue.
const CLAIM_LOOKAHEAD: usize = 256;

/// `path` as a key for [`Inner::unfinished_dirs`]: without a trailing
/// slash, and the root as `/`.
fn dir_key(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() && path.starts_with('/') {
        "/"
    } else {
        trimmed
    }
}

/// Why [`TransferManager`] refused to queue a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueError {
    /// [`MAX_QUEUED_JOBS`] jobs are already pending.
    QueueFull,
    /// A pending or running job already writes this job's destination. See
    /// [`destination_key`].
    Duplicate,
}

/// The destination a job writes, as the key two jobs must not share.
///
/// Two jobs writing one destination at once corrupt it. Both stream into
/// the same `.blink-part` — the second either deletes the first's or takes it for
/// its own partial and appends to it — and the first to finish renames the
/// file into place while the other is still writing, then reports success.
/// A download's destination is its local path; an upload's is its remote
/// path. A mkdir has none: creating a directory twice is harmless.
///
/// Local paths fold case where the filesystem does (Windows, and macOS by
/// default), so `README` and `readme` from one server directory count as
/// one destination there. Remote paths never fold: whether the server's
/// filesystem does is unknowable from here.
pub(crate) fn destination_key(
    direction: Direction,
    remote_path: &str,
    local_path: &Path,
) -> Option<(Direction, String)> {
    match direction {
        Direction::Download => Some((
            direction,
            local_destination_key(local_path, cfg!(any(windows, target_os = "macos"))),
        )),
        Direction::Upload => Some((direction, remote_path.to_string())),
        Direction::CreateDir => None,
    }
}

/// The local half of [`destination_key`], with the case policy passed in so
/// both branches are tested on any host. Lossy conversion is fine for a
/// key: a path with undecodable bytes still maps to one string every time.
fn local_destination_key(path: &Path, fold_case: bool) -> String {
    let s = path.to_string_lossy();
    if fold_case {
        s.to_lowercase()
    } else {
        s.into_owned()
    }
}

/// Manages a queue of jobs and a configurable concurrency cap.
///
/// We don't use Tokio's Semaphore directly because we want to expose the queue
/// snapshot to the UI for rendering, which requires our own bookkeeping.
pub struct TransferManager {
    inner: Arc<Mutex<Inner>>,
    events: mpsc::UnboundedSender<TransferEvent>,
    /// Wakes the dispatcher when there may be something for it to do: a job
    /// queued, a job ended, a worker slot freed, the queue resumed. It used
    /// to poll every 100 ms instead, so a slot sat idle for half that on
    /// average after each job, which capped batches of small files.
    wake: Arc<tokio::sync::Notify>,
}

struct Inner {
    next_id: u64,
    next_batch_id: u64,
    /// Jobs in id order, ids consecutive, so the job with id `id` sits at
    /// `id - first_id`. Finished jobs leave the front; see [`RETAINED_JOBS`].
    jobs: VecDeque<TransferJob>,
    /// Id of `jobs[0]`, or of the next job when `jobs` is empty.
    first_id: u64,
    /// No job before this index is Pending, so [`TransferManager::take_next_pending`]
    /// starts looking here. Jobs only ever leave Pending, and new ones are
    /// appended, so it only moves forward. Scanning from 0 instead passed
    /// every finished job on each claim: quadratic over a batch, under the
    /// lock the renderer takes every frame.
    first_pending: usize,
    /// Number of jobs currently in `Pending` state. Kept in sync with `jobs`
    /// so `enqueue` can check the cap in O(1) instead of scanning the vec.
    pending_count: usize,
    /// Destinations of every Pending or Active job — see [`destination_key`].
    /// Kept in step with job states so an enqueue checks in O(1): a batch
    /// can be [`MAX_QUEUED_JOBS`] long, and scanning `jobs` per enqueue
    /// would make queuing it quadratic.
    in_flight: HashSet<(Direction, String)>,
    parallelism: u8,
    /// Pending jobs accepted before [`EnqueueError::QueueFull`]:
    /// [`MAX_QUEUED_JOBS`], except in tests of what happens past it.
    cap: usize,
    paused: bool,
    /// Remote folders with a mkdir still Pending or Active, and the ids of
    /// those jobs. A job that writes under one of them waits for every such
    /// mkdir queued before it — see [`Inner::waits_on_mkdir`].
    unfinished_dirs: HashMap<String, std::collections::BTreeSet<u64>>,
    /// Abort handles for currently-running worker tasks, keyed by job id.
    /// Populated by [`TransferManager::register_active`] (called from the
    /// dispatcher), removed on completion or cancellation.
    active: HashMap<u64, AbortHandle>,
}

impl Inner {
    /// Where the job with id `id` sits in `jobs`, if it's still there.
    fn index_of(&self, id: u64) -> Option<usize> {
        let idx = usize::try_from(id.checked_sub(self.first_id)?).ok()?;
        (idx < self.jobs.len()).then_some(idx)
    }

    /// Whether `job` writes under a remote folder whose mkdir, queued before
    /// it, has not finished.
    ///
    /// A plan queues each folder's mkdir before what goes in it, but the
    /// dispatcher starts jobs the moment a worker is free, so queue order
    /// is only start order. An upload into a new folder ran alongside the
    /// folder's mkdir on another connection and could reach the server
    /// first; on SFTP it usually did, since mkdir takes two round trips to
    /// the upload's one. Only mkdirs queued earlier count: one queued later
    /// (another batch making the same folder) must not hold back work ahead
    /// of it in the queue.
    fn waits_on_mkdir(&self, job: &TransferJob) -> bool {
        if job.direction == Direction::Download || self.unfinished_dirs.is_empty() {
            return false;
        }
        let mut path = dir_key(&job.remote_path);
        while let Some((parent, _)) = path.rsplit_once('/') {
            let parent = if parent.is_empty() { "/" } else { parent };
            if self
                .unfinished_dirs
                .get(parent)
                .and_then(|ids| ids.first())
                .is_some_and(|&first| first < job.id)
            {
                return true;
            }
            if parent == "/" {
                break;
            }
            path = parent;
        }
        false
    }

    /// Move the job at `idx` to `state`, keeping the counts and sets that
    /// follow job states in step. Every state change goes through here.
    fn set_state(&mut self, idx: usize, state: TransferState) {
        let j = &mut self.jobs[idx];
        let was_pending = j.state == TransferState::Pending;
        let was_live = matches!(j.state, TransferState::Pending | TransferState::Active);
        let ends = matches!(state, TransferState::Complete | TransferState::Failed(_));
        j.state = state;
        let (id, direction) = (j.id, j.direction);
        let key = destination_key(direction, &j.remote_path, &j.local_path);
        let dir = dir_key(&j.remote_path).to_string();
        if was_pending {
            self.pending_count = self.pending_count.saturating_sub(1);
        }
        if was_live && ends {
            // The job has stopped writing its destination; another may.
            if let Some(key) = key {
                self.in_flight.remove(&key);
            }
            // The folder is made, or failed to be: what waited on it may go.
            if direction == Direction::CreateDir
                && let Some(ids) = self.unfinished_dirs.get_mut(&dir)
            {
                ids.remove(&id);
                if ids.is_empty() {
                    self.unfinished_dirs.remove(&dir);
                }
            }
        }
    }

    /// Drop finished jobs from the front past [`RETAINED_JOBS`]. Stops at
    /// the first one still Pending or Active: ids must stay consecutive.
    fn drop_old_finished(&mut self) {
        while self.jobs.len() > RETAINED_JOBS
            && self.jobs.front().is_some_and(|j| {
                matches!(j.state, TransferState::Complete | TransferState::Failed(_))
            })
        {
            self.jobs.pop_front();
            self.first_id += 1;
            self.first_pending = self.first_pending.saturating_sub(1);
        }
    }
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
        Self::with_cap(parallelism, MAX_QUEUED_JOBS)
    }

    /// A manager whose queue fills at `cap` pending jobs, so tests can reach
    /// a full queue without queuing [`MAX_QUEUED_JOBS`] of them.
    #[cfg(test)]
    pub fn with_queue_cap(parallelism: u8, cap: usize) -> Self {
        Self::with_cap(parallelism, cap).0
    }

    fn with_cap(parallelism: u8, cap: usize) -> (Self, mpsc::UnboundedReceiver<TransferEvent>) {
        let parallelism = parallelism.clamp(1, crate::config::MAX_PARALLEL);
        let (tx, rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Mutex::new(Inner {
            next_id: 1,
            next_batch_id: 1,
            jobs: VecDeque::new(),
            first_id: 1,
            first_pending: 0,
            pending_count: 0,
            in_flight: HashSet::new(),
            parallelism,
            cap,
            paused: false,
            unfinished_dirs: HashMap::new(),
            active: HashMap::new(),
        }));
        let wake = Arc::new(tokio::sync::Notify::new());
        (
            Self {
                inner,
                events: tx,
                wake,
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
        self.wake();
    }

    /// Wake the dispatcher. A wake with no one waiting is kept for the next
    /// wait, so none is lost between its check and its wait.
    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }

    /// Wait for [`Self::wake`].
    pub(crate) async fn woken(&self) {
        self.wake.notified().await;
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
        self.inner.lock().jobs.iter().cloned().collect()
    }

    /// Look up a single job by id, while the manager still holds it.
    /// Event handlers take the job from the event instead.
    #[cfg(test)]
    pub fn job(&self, id: u64) -> Option<TransferJob> {
        let inner = self.inner.lock();
        inner.index_of(id).map(|idx| inner.jobs[idx].clone())
    }

    pub fn pause(&self) {
        self.inner.lock().paused = true;
        let _ = self.events.send(TransferEvent::Paused);
    }

    pub fn resume(&self) {
        self.inner.lock().paused = false;
        self.wake();
        let _ = self.events.send(TransferEvent::Resumed);
    }

    pub fn is_paused(&self) -> bool {
        self.inner.lock().paused
    }

    /// Queue a new download. Returns the assigned job id, or why it was
    /// refused: the pending-job cap ([`MAX_QUEUED_JOBS`]) was reached, or a
    /// job already writes this destination ([`destination_key`]).
    pub fn enqueue_download(
        &self,
        remote_path: String,
        local_path: PathBuf,
    ) -> Result<u64, EnqueueError> {
        self.enqueue(Direction::Download, remote_path, local_path, None)
    }

    /// Queue a new upload. Refused as [`Self::enqueue_download`] is.
    pub fn enqueue_upload(
        &self,
        local_path: PathBuf,
        remote_path: String,
    ) -> Result<u64, EnqueueError> {
        self.enqueue(Direction::Upload, remote_path, local_path, None)
    }

    /// Queue a remote-side `mkdir`. The `local_path` field is unused for this
    /// direction; we pass an empty PathBuf to satisfy the shared shape.
    pub fn enqueue_mkdir(&self, remote_path: String) -> Result<u64, EnqueueError> {
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

    /// Queue a download as part of a batch. See [`Self::allocate_batch_id`].
    pub fn enqueue_download_batched(
        &self,
        remote_path: String,
        local_path: PathBuf,
        batch_id: u64,
    ) -> Result<u64, EnqueueError> {
        self.enqueue(Direction::Download, remote_path, local_path, Some(batch_id))
    }

    /// Queue an upload as part of a batch. See [`Self::allocate_batch_id`].
    pub fn enqueue_upload_batched(
        &self,
        local_path: PathBuf,
        remote_path: String,
        batch_id: u64,
    ) -> Result<u64, EnqueueError> {
        self.enqueue(Direction::Upload, remote_path, local_path, Some(batch_id))
    }

    /// Queue an mkdir as part of a batch. See [`Self::allocate_batch_id`].
    pub fn enqueue_mkdir_batched(
        &self,
        remote_path: String,
        batch_id: u64,
    ) -> Result<u64, EnqueueError> {
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
    ) -> Result<u64, EnqueueError> {
        let mut inner = self.inner.lock();
        // Cap the number of pending jobs so a large server directory listing
        // cannot grow the queue without bound and exhaust memory.
        if inner.pending_count >= inner.cap {
            return Err(EnqueueError::QueueFull);
        }
        let key = destination_key(direction, &remote_path, &local_path);
        if let Some(key) = &key
            && inner.in_flight.contains(key)
        {
            return Err(EnqueueError::Duplicate);
        }
        if let Some(key) = key {
            inner.in_flight.insert(key);
        }
        let id = inner.next_id;
        inner.next_id += 1;
        if direction == Direction::CreateDir {
            inner
                .unfinished_dirs
                .entry(dir_key(&remote_path).to_string())
                .or_default()
                .insert(id);
        }
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
        inner.jobs.push_back(job.clone());
        inner.pending_count += 1;
        drop(inner);
        self.wake();
        let _ = self.events.send(TransferEvent::Queued(job));
        Ok(id)
    }

    /// Whether a Pending or Active job writes this destination. Lets a
    /// caller drop duplicates from a plan before recording it anywhere.
    pub(crate) fn is_in_flight(&self, key: &(Direction, String)) -> bool {
        self.inner.lock().in_flight.contains(key)
    }

    /// Mark a job's state. Used by the dispatcher (once it lands).
    ///
    /// An id the manager doesn't hold is ignored: there is no job to change
    /// or report.
    pub fn mark(&self, id: u64, state: TransferState) {
        let job = {
            let mut inner = self.inner.lock();
            let Some(idx) = inner.index_of(id) else {
                return;
            };
            let ends = matches!(state, TransferState::Complete | TransferState::Failed(_));
            inner.set_state(idx, state.clone());
            let job = inner.jobs[idx].clone();
            if ends {
                inner.drop_old_finished();
            }
            job
        };
        match state {
            TransferState::Active => {
                let _ = self.events.send(TransferEvent::Started(job));
            }
            TransferState::Complete => {
                self.wake();
                let _ = self.events.send(TransferEvent::Complete(job));
            }
            TransferState::Failed(error) => {
                self.wake();
                let _ = self.events.send(TransferEvent::Failed { job, error });
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
            if let Some(idx) = inner.index_of(id) {
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
    /// The render path asks for this every frame. `jobs` retains up to
    /// [`RETAINED_JOBS`] finished jobs besides every live one, so going
    /// through [`Self::snapshot`] and filtering
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
    /// queue holds nothing pending, or nothing that can start yet: a job
    /// waits while a folder it writes under is still being made (see
    /// [`Inner::waits_on_mkdir`]).
    ///
    /// This is the single transition point from `Pending` → `Active`. Callers
    /// must NOT also call `mark(id, Active)` on the returned job — doing so
    /// would emit a duplicate `Started` event.
    pub fn take_next_pending(&self) -> Option<TransferJob> {
        let cloned = {
            let mut inner = self.inner.lock();
            let mut first_seen = None;
            let mut found = None;
            let mut waiting = 0usize;
            for (idx, j) in inner.jobs.iter().enumerate().skip(inner.first_pending) {
                if j.state != TransferState::Pending {
                    continue;
                }
                first_seen.get_or_insert(idx);
                if !inner.waits_on_mkdir(j) {
                    found = Some(idx);
                    break;
                }
                waiting += 1;
                if waiting >= CLAIM_LOOKAHEAD {
                    break;
                }
            }
            // The cursor stops at the first job still Pending, waiting or not.
            let Some(idx) = found else {
                inner.first_pending = first_seen.unwrap_or(inner.jobs.len());
                return None;
            };
            inner.first_pending = match first_seen {
                Some(first) if first < idx => first,
                _ => idx + 1,
            };
            inner.set_state(idx, TransferState::Active);
            inner.jobs[idx].clone()
        };
        let _ = self.events.send(TransferEvent::Started(cloned.clone()));
        Some(cloned)
    }

    /// Register an [`AbortHandle`] for a running worker so the manager can
    /// cancel it later. Called by the dispatcher immediately after spawning.
    ///
    /// A job cancelled between being claimed and this call has no worker
    /// to keep: the handle is aborted instead, and since the worker waits
    /// for registration before starting, it never runs.
    pub fn register_active(&self, id: u64, handle: AbortHandle) {
        let mut inner = self.inner.lock();
        let still_active = inner
            .index_of(id)
            .is_some_and(|idx| inner.jobs[idx].state == TransferState::Active);
        if still_active {
            inner.active.insert(id, handle);
        } else {
            drop(inner);
            handle.abort();
        }
    }

    /// Remove a worker's entry from the active map. Returns `true` if the
    /// entry was present (the natural-completion path), `false` if the entry
    /// had already been removed by [`Self::cancel`] (cancellation won the race).
    pub fn deregister_active(&self, id: u64) -> bool {
        self.inner.lock().active.remove(&id).is_some()
    }

    /// Cancel a running transfer by id. Aborts the worker task at its next
    /// `.await` point and marks the job as failed with a "cancelled" reason.
    /// No-op if the id isn't currently running.
    ///
    /// Cancellation may leave a partial file on disk for downloads in flight.
    ///
    /// Returns whether it cancelled anything: `false` when the job had
    /// already finished (the user may have confirmed after it did).
    pub fn cancel(&self, id: u64) -> bool {
        self.cancel_matching(|j| j.id == id, false).0 > 0
    }

    /// Cancel every Active and Pending job sharing `batch_id`. Returns
    /// `(active_cancelled, pending_cancelled)`. Complete and previously-
    /// failed jobs are ignored.
    pub fn cancel_batch(&self, batch_id: u64) -> (usize, usize) {
        self.cancel_matching(|j| j.batch_id == Some(batch_id), true)
    }

    /// Cancel every Active and Pending job, as [`Self::cancel_batch`] does
    /// one batch's. Returns `(active_cancelled, pending_cancelled)`.
    ///
    /// For ending a connection: shutting the dispatcher down only stops new
    /// jobs from starting, and running workers hold their own connections,
    /// so without this they carried on after a disconnect the user was told
    /// would cancel them.
    pub fn cancel_all(&self) -> (usize, usize) {
        self.cancel_matching(|_| true, true)
    }

    /// Cancel the Active jobs `select` picks, and the Pending ones too when
    /// `pending_too`. Returns `(active_cancelled, pending_cancelled)`.
    ///
    /// Every pick is marked under one hold of the lock. Collecting them
    /// first and marking them afterwards, as this used to, let the
    /// dispatcher start a job in between: it was then marked cancelled but
    /// never stopped, its destination was freed for another job to write
    /// while it still did, and it was reported twice, cancelled and then
    /// finished. An Active job whose worker is not registered yet is marked
    /// too; [`Self::register_active`] stops that worker before it starts.
    fn cancel_matching(
        &self,
        select: impl Fn(&TransferJob) -> bool,
        pending_too: bool,
    ) -> (usize, usize) {
        let cancelled = TransferState::Failed("cancelled".into());
        let mut handles = Vec::new();
        let mut ended = Vec::new();
        let (mut active_n, mut pending_n) = (0, 0);
        {
            let mut inner = self.inner.lock();
            let picked: Vec<usize> = inner
                .jobs
                .iter()
                .enumerate()
                .filter(|(_, j)| {
                    let live = match j.state {
                        TransferState::Active => true,
                        TransferState::Pending => pending_too,
                        _ => false,
                    };
                    live && select(j)
                })
                .map(|(idx, _)| idx)
                .collect();
            for idx in picked {
                let id = inner.jobs[idx].id;
                if inner.jobs[idx].state == TransferState::Active {
                    active_n += 1;
                    handles.extend(inner.active.remove(&id));
                } else {
                    pending_n += 1;
                }
                inner.set_state(idx, cancelled.clone());
                ended.push(inner.jobs[idx].clone());
            }
            inner.drop_old_finished();
        }
        for h in handles {
            h.abort();
        }
        if !ended.is_empty() {
            self.wake();
        }
        for job in ended {
            let _ = self.events.send(TransferEvent::Failed {
                job,
                error: "cancelled".into(),
            });
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
            wake: Arc::clone(&self.wake),
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

    /// `take_next_pending` resumes its search where the last one stopped;
    /// it must still find every pending job, in order, and skip ones that
    /// left Pending some other way.
    #[test]
    fn pending_jobs_are_claimed_in_order_past_cancelled_ones() {
        let (m, _rx) = TransferManager::new(1);
        let a = m.enqueue_mkdir("/a".into()).unwrap();
        let b = m.enqueue_mkdir("/b".into()).unwrap();
        let c = m.enqueue_mkdir("/c".into()).unwrap();
        assert_eq!(m.take_next_pending().unwrap().id, a);
        m.mark(b, TransferState::Failed("cancelled".into()));
        assert_eq!(m.take_next_pending().unwrap().id, c);
        assert!(m.take_next_pending().is_none());
        let d = m.enqueue_mkdir("/d".into()).unwrap();
        assert_eq!(m.take_next_pending().unwrap().id, d, "a job queued later");
        assert!(m.take_next_pending().is_none());
    }

    /// A recursive upload queues each folder's mkdir ahead of what goes in
    /// it, but queue order is only start order: with two or more workers,
    /// an upload into a folder ran alongside the folder's mkdir and could
    /// reach the server first, failing with "no such file". It now waits.
    #[test]
    fn an_upload_waits_for_its_folders_mkdir() {
        let (m, _rx) = TransferManager::new(2);
        let dir = m.enqueue_mkdir("/up/a".into()).unwrap();
        let file = m.enqueue_upload("/l/f".into(), "/up/a/f".into()).unwrap();
        assert_eq!(m.take_next_pending().unwrap().id, dir);
        assert!(
            m.take_next_pending().is_none(),
            "the upload must not start while its folder is being made"
        );
        m.mark(dir, TransferState::Complete);
        assert_eq!(m.take_next_pending().unwrap().id, file);
    }

    /// Deeper folders wait for every folder above them, queued or running.
    #[test]
    fn a_folder_waits_for_the_folders_above_it() {
        let (m, _rx) = TransferManager::new(3);
        let top = m.enqueue_mkdir("/up".into()).unwrap();
        let mid = m.enqueue_mkdir("/up/a".into()).unwrap();
        let deep = m.enqueue_upload("/l/f".into(), "/up/a/b/f".into()).unwrap();
        assert_eq!(m.take_next_pending().unwrap().id, top);
        assert!(m.take_next_pending().is_none());
        m.mark(top, TransferState::Complete);
        assert_eq!(m.take_next_pending().unwrap().id, mid);
        assert!(m.take_next_pending().is_none(), "/up/a is still being made");
        // A failed mkdir releases what waits on it: the job then fails on
        // its own, with the server's reason, rather than waiting forever.
        m.mark(mid, TransferState::Failed("denied".into()));
        assert_eq!(m.take_next_pending().unwrap().id, deep);
    }

    /// A second batch making the same folder queues its mkdir behind the
    /// first batch's remaining files. They must not wait for it: past the
    /// claim lookahead, nothing could reach that mkdir, and the queue would
    /// stall for good.
    #[test]
    fn a_later_mkdir_of_the_same_folder_holds_nothing_back() {
        let (m, _rx) = TransferManager::new(1);
        let first = m.enqueue_mkdir("/up".into()).unwrap();
        let files: Vec<u64> = (0..CLAIM_LOOKAHEAD + 1)
            .map(|i| {
                m.enqueue_upload(format!("/l/{i}").into(), format!("/up/{i}"))
                    .unwrap()
            })
            .collect();
        assert_eq!(m.take_next_pending().unwrap().id, first);
        m.mark(first, TransferState::Complete);
        let again = m.enqueue_mkdir("/up".into()).unwrap();
        for id in files {
            let j = m.take_next_pending().expect("the earlier files go first");
            assert_eq!(j.id, id);
            m.mark(id, TransferState::Complete);
        }
        assert_eq!(m.take_next_pending().unwrap().id, again);
    }

    /// Only jobs under an unfinished mkdir wait; the rest pass them, and
    /// the waiting ones are still claimed, in order, once free.
    #[test]
    fn jobs_elsewhere_pass_the_ones_that_wait() {
        let (m, _rx) = TransferManager::new(4);
        let dir = m.enqueue_mkdir("/up/a".into()).unwrap();
        let held = m.enqueue_upload("/l/1".into(), "/up/a/1".into()).unwrap();
        let other = m.enqueue_upload("/l/2".into(), "/up/b/2".into()).unwrap();
        let download = m.enqueue_download("/up/a/3".into(), "/l/3".into()).unwrap();
        assert_eq!(m.take_next_pending().unwrap().id, dir);
        assert_eq!(m.take_next_pending().unwrap().id, other);
        assert_eq!(
            m.take_next_pending().unwrap().id,
            download,
            "a download writes locally and waits for no remote mkdir",
        );
        assert!(m.take_next_pending().is_none());
        m.mark(dir, TransferState::Complete);
        assert_eq!(m.take_next_pending().unwrap().id, held);
        assert!(m.take_next_pending().is_none());
    }

    /// Finished jobs are dropped past `RETAINED_JOBS`, oldest first, and
    /// lookups by id keep working for the ones left.
    #[test]
    fn old_finished_jobs_are_dropped() {
        let (m, _rx) = TransferManager::new(1);
        let ids: Vec<u64> = (0..RETAINED_JOBS + 10)
            .map(|i| m.enqueue_mkdir(format!("/d{i}")).unwrap())
            .collect();
        // A live job at the front holds everything behind it.
        let first = m.take_next_pending().unwrap().id;
        for _ in 1..ids.len() {
            let j = m.take_next_pending().unwrap();
            m.mark(j.id, TransferState::Complete);
        }
        assert_eq!(m.snapshot().len(), ids.len(), "nothing passes a live job");

        m.mark(first, TransferState::Complete);
        assert_eq!(m.snapshot().len(), RETAINED_JOBS);
        assert!(m.job(ids[0]).is_none());
        assert!(m.job(ids[10]).is_some());
        assert_eq!(
            m.job(*ids.last().unwrap()).unwrap().id,
            *ids.last().unwrap()
        );

        let next = m.enqueue_mkdir("/later".into()).unwrap();
        assert_eq!(m.take_next_pending().unwrap().id, next);
        m.mark(next, TransferState::Complete);
        assert_eq!(m.job(next).unwrap().state, TransferState::Complete);
    }

    #[test]
    fn cancel_all_cancels_running_and_queued_jobs_only() {
        let (m, _rx) = TransferManager::new(1);
        let done = m.enqueue_mkdir("/done".into()).unwrap();
        let running = m.enqueue_mkdir("/running".into()).unwrap();
        let queued = m.enqueue_mkdir("/queued".into()).unwrap();
        m.take_next_pending();
        m.mark(done, TransferState::Complete);
        m.take_next_pending();
        assert_eq!(m.job(running).unwrap().state, TransferState::Active);
        assert_eq!(m.cancel_all(), (1, 1));
        assert_eq!(m.job(done).unwrap().state, TransferState::Complete);
        for id in [running, queued] {
            assert_eq!(
                m.job(id).unwrap().state,
                TransferState::Failed("cancelled".into())
            );
        }
    }

    /// The dispatcher marks a job Active, then spawns its worker and
    /// registers it. A cancel in between found no worker to stop: a single
    /// cancel said the job "had already finished", and a batch cancel or a
    /// disconnect left it running. It is now cancelled at once, and its
    /// worker is stopped as it registers, before it does anything.
    #[tokio::test]
    async fn a_job_cancelled_before_its_worker_registers_never_runs() {
        let (m, _rx) = TransferManager::new(1);
        let id = m.enqueue_mkdir("/a".into()).unwrap();
        m.take_next_pending();
        assert!(m.cancel(id), "a started job can be cancelled");
        assert_eq!(
            m.job(id).unwrap().state,
            TransferState::Failed("cancelled".into())
        );

        let worker = tokio::spawn(std::future::pending::<()>());
        m.register_active(id, worker.abort_handle());
        assert!(worker.await.unwrap_err().is_cancelled());
        assert!(!m.deregister_active(id), "nothing is left registered");
    }

    /// Each cancelled job is reported once. A job that the dispatcher
    /// claimed while a batch cancel was part way through used to be marked
    /// cancelled while it kept running, and then reported again when it
    /// finished.
    #[test]
    fn cancelling_reports_each_job_once() {
        let (m, mut rx) = TransferManager::new(1);
        for i in 0..3 {
            m.enqueue_mkdir(format!("/d{i}")).unwrap();
        }
        m.take_next_pending();
        while rx.try_recv().is_ok() {}
        assert_eq!(m.cancel_all(), (1, 2));
        let mut failed = 0;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, TransferEvent::Failed { .. }) {
                failed += 1;
            }
        }
        assert_eq!(failed, 3);
        assert!(m.take_next_pending().is_none(), "nothing left to start");
    }

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

    // -- duplicate destinations ------------------------------------------------
    //
    // Two jobs writing one destination at once corrupt it: both stream into
    // the same `.blink-part`, and the first to finish renames it into place while
    // the other is still writing. So a destination is held from enqueue until
    // its job completes or fails, and a second job for it is refused.

    #[test]
    fn a_second_download_to_a_queued_destination_is_refused() {
        let m = manager();
        m.enqueue_download("/a".into(), "/tmp/x".into()).unwrap();
        assert_eq!(
            m.enqueue_download("/b".into(), "/tmp/x".into()),
            Err(EnqueueError::Duplicate),
        );
    }

    #[test]
    fn a_second_download_to_a_running_destination_is_refused() {
        let m = manager();
        m.enqueue_download("/a".into(), "/tmp/x".into()).unwrap();
        m.take_next_pending().expect("the first job starts");
        assert_eq!(
            m.enqueue_download("/a".into(), "/tmp/x".into()),
            Err(EnqueueError::Duplicate),
        );
    }

    #[test]
    fn a_destination_is_free_again_once_its_job_ends() {
        let m = manager();
        let done = m.enqueue_download("/a".into(), "/tmp/x".into()).unwrap();
        m.mark(done, TransferState::Complete);
        let failed = m
            .enqueue_download("/a".into(), "/tmp/x".into())
            .expect("a completed job no longer holds its destination");
        m.mark(failed, TransferState::Failed("boom".into()));
        m.enqueue_download("/a".into(), "/tmp/x".into())
            .expect("nor does a failed one");
    }

    #[test]
    fn a_cancelled_job_frees_its_destination() {
        let m = manager();
        let batch = m.allocate_batch_id();
        m.enqueue_download_batched("/a".into(), "/tmp/x".into(), batch)
            .unwrap();
        m.cancel_batch(batch);
        m.enqueue_download("/a".into(), "/tmp/x".into())
            .expect("a cancelled job no longer holds its destination");
    }

    #[test]
    fn uploads_are_held_by_their_remote_path() {
        let m = manager();
        m.enqueue_upload("/tmp/x".into(), "/r/x".into()).unwrap();
        assert_eq!(
            m.enqueue_upload("/tmp/y".into(), "/r/x".into()),
            Err(EnqueueError::Duplicate),
            "two uploads to one remote path write one `.blink-part` there",
        );
        m.enqueue_upload("/tmp/x".into(), "/r/other".into())
            .expect("one local source may go to two remote paths");
    }

    #[test]
    fn a_download_and_an_upload_do_not_hold_each_other() {
        let m = manager();
        m.enqueue_download("/r/x".into(), "/tmp/x".into()).unwrap();
        m.enqueue_upload("/tmp/x".into(), "/r/x".into())
            .expect("an upload's destination is remote, not local");
    }

    #[test]
    fn mkdirs_are_never_refused() {
        let m = manager();
        m.enqueue_mkdir("/r/d".into()).unwrap();
        m.enqueue_mkdir("/r/d".into())
            .expect("creating a directory twice is harmless");
    }

    #[test]
    fn is_in_flight_tracks_the_destination() {
        let m = manager();
        let key = destination_key(Direction::Download, "/a", Path::new("/tmp/x")).unwrap();
        assert!(!m.is_in_flight(&key));
        let id = m.enqueue_download("/a".into(), "/tmp/x".into()).unwrap();
        assert!(m.is_in_flight(&key));
        m.mark(id, TransferState::Complete);
        assert!(!m.is_in_flight(&key));
    }

    #[test]
    fn local_destinations_fold_case_only_where_the_filesystem_does() {
        let upper = Path::new("/tmp/README");
        let lower = Path::new("/tmp/readme");
        assert_ne!(
            local_destination_key(upper, false),
            local_destination_key(lower, false),
            "a case-sensitive filesystem holds both",
        );
        assert_eq!(
            local_destination_key(upper, true),
            local_destination_key(lower, true),
            "a case-insensitive one holds one file under both names",
        );
    }

    #[test]
    fn remote_destinations_never_fold_case() {
        assert_ne!(
            destination_key(Direction::Upload, "/r/README", Path::new("/l")),
            destination_key(Direction::Upload, "/r/readme", Path::new("/l")),
        );
    }

    #[test]
    fn the_queue_cap_refuses_past_its_limit_and_frees_as_jobs_start() {
        let m = TransferManager::with_queue_cap(1, 2);
        m.enqueue_download("/a".into(), "/tmp/a".into()).unwrap();
        m.enqueue_download("/b".into(), "/tmp/b".into()).unwrap();
        assert_eq!(
            m.enqueue_download("/c".into(), "/tmp/c".into()),
            Err(EnqueueError::QueueFull),
        );
        m.take_next_pending().unwrap();
        m.enqueue_download("/c".into(), "/tmp/c".into())
            .expect("the cap counts pending jobs, and one has started");
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
        let running = m
            .enqueue_download("/live".into(), "/tmp/live".into())
            .unwrap();
        m.mark(running, TransferState::Active);

        assert_eq!(m.active_jobs().len(), 1);
        assert_eq!(m.snapshot().len(), 501, "history itself is still retained");
    }

    #[test]
    fn batch_counts_splits_active_and_pending() {
        let m = manager();
        let batch = m.allocate_batch_id();
        let a = m
            .enqueue_download_batched("/a".into(), "/tmp/a".into(), batch)
            .unwrap();
        let b = m
            .enqueue_download_batched("/b".into(), "/tmp/b".into(), batch)
            .unwrap();
        m.enqueue_download_batched("/c".into(), "/tmp/c".into(), batch)
            .unwrap();

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
        m.enqueue_download_batched("/a".into(), "/tmp/a".into(), mine)
            .unwrap();
        m.enqueue_download_batched("/b".into(), "/tmp/b".into(), theirs)
            .unwrap();
        m.enqueue_download("/loose".into(), "/tmp/loose".into())
            .unwrap();

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
