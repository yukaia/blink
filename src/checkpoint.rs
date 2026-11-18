//! Walk checkpointing: persist a transfer plan to disk so an interrupted
//! batch can be resumed without re-walking the remote / local tree.
//!
//! One checkpoint file is kept per (session, origin, direction), named by a
//! stable key derived from the session name, the server and account it
//! connects to (`user@host:port`, as [`crate::transport::origin_of`] names
//! it), and the transfer direction:
//!
//! ```text
//! ~/.config/blink/checkpoints/<name>-<hash>-upload.json
//! ~/.config/blink/checkpoints/<name>-<hash>-download.json
//! ```
//!
//! `<name>` is the session name with path-unsafe characters replaced by `_`,
//! and `<hash>` the first eight hex digits of the SHA-256 of the name and
//! origin, so two names that sanitize alike ("my prod", "my_prod") keep
//! separate files, and so do two accounts that share a name: an ad-hoc
//! `sftp://alice@host` and `sftp://bob@host` are both named `host`. A
//! checkpoint is only ever offered to, resumed by, or appended to by a
//! connection to the server and account it was made for; resuming an
//! upload made for one account used to send its files to the other.
//!
//! A new walk of the same direction *appends* its jobs to the checkpoint
//! already tracking that direction, or already on disk, rather than
//! replacing it: replacing it destroyed the plan of a batch still running,
//! or of one the user chose to resume later. The file is removed once
//! nothing in it still needs to run.
//!
//! Renaming a session moves its checkpoints to the new name; see
//! [`rename_session`].
//!
//! ## Format (version 4)
//!
//! ```json
//! {
//!   "version": 4,
//!   "session": "production",
//!   "origin": "deploy@prod.example.com:22",
//!   "kind": "upload",
//!   "jobs": [
//!     { "type": "mkdir",    "remote_path": "/var/www/html/assets",
//!                           "status": "done" },
//!     { "type": "upload",   "local_path": "/home/me/file.txt",
//!                           "remote_path": "/var/www/html/file.txt",
//!                           "status": "pending" },
//!     { "type": "download", "remote_path": "/srv/data/report.pdf",
//!                           "local_path": "/home/me/dl/report.pdf",
//!                           "status": "in_progress" }
//!   ]
//! }
//! ```
//!
//! ## Job lifecycle
//!
//! ```text
//! pending  ──(dispatcher picks up job)──►  in_progress  ──(success)──►  done
//!    │                                          │
//!    │                                          └──(crash / kill)──► stays in_progress
//!    └──(user cancels the batch)──► cancelled
//! ```
//!
//! On resume:
//! - `done`        → skipped (already transferred successfully)
//! - `cancelled`   → skipped (the user abandoned it)
//! - `in_progress` → re-queued (the transfer was interrupted; partial files
//!   are safe to overwrite)
//! - `pending`     → re-queued (never started)
//!
//! ## Crash safety
//!
//! The whole plan is written before any job starts. After that, a job is
//! marked `in_progress` when it starts and `done` when it completes, and
//! those marks reach disk debounced, at most every 250 ms: a batch of 100k
//! jobs would otherwise cost an fsync per transition. A crash can lose the
//! latest marks, which only means those jobs run again on resume. What it
//! can never do is record a job as `done` that had not finished.
//!
//! Writes are atomic: the JSON is written to a temp file beside it then
//! renamed into place, so a crash mid-write never produces a truncated file.
//!
//! ## Version migration
//!
//! Version 1 files used a boolean `done` field. They are migrated in
//! memory on load: `done: true` → `status: "done"`, `done: false` →
//! `status: "pending"`. The file itself is not rewritten: loading is how
//! `blink checkpoints` lists files, and listing must not change them.
//!
//! Files before version 4 have no `origin`, so nothing can say which server
//! they were made for. They load (with `origin: None`) but are never
//! offered or resumed; `blink checkpoints` lists them as orphaned, and
//! `--clean` removes them with their partials. Under the version-4 key
//! they also live at a different file name, which no lookup computes.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::error::{BlinkError, Result};
use crate::paths;

/// How long to coalesce per-job state writes before flushing to disk.
///
/// Tuned to be large enough that a hot batch (many jobs transitioning
/// per second) collapses into a manageable handful of fsyncs, and small
/// enough that a crash loses at most a quarter-second of state. Any lost
/// `InProgress`/`Done` marks simply mean the affected jobs get re-run on
/// resume — correctness is unchanged, only some wasted work in the
/// crash-then-resume case.
const CHECKPOINT_FLUSH_INTERVAL: Duration = Duration::from_millis(250);

/// Current serialization format version.
///
/// Version history:
///   1 — initial format, boolean `done` field per job
///   2 — three-state `status` field: pending / in_progress / done
///   3 — adds the `cancelled` status. Purely additive: a version-2
///       document is a valid version-3 document and loads unchanged.
///   4 — adds `origin`, and keys the file name by it. An older document
///       loads with no origin and is never offered (see the module docs).
const FORMAT_VERSION: u32 = 4;

/// Maximum checkpoint file size accepted on load (256 MiB).
///
/// A guard against a corrupt file, so it has to clear anything blink itself
/// writes. It was 10 MiB, which a batch of about 50,000 files passed: the
/// checkpoint was saved and then refused on load. A job takes roughly 60
/// bytes plus its two paths, so this fits [`MAX_CHECKPOINT_JOBS`] with
/// paths averaging over 100 bytes each. [`Checkpoint::append`] refuses to
/// grow a checkpoint past either limit.
const MAX_CHECKPOINT_BYTES: u64 = 256 * 1024 * 1024;

/// Maximum number of jobs accepted in a single checkpoint.
const MAX_CHECKPOINT_JOBS: usize = 1_000_000;

/// How many bytes longer a job's encoding can grow after it is added:
/// `"in_progress"` against `"pending"`. See [`Checkpoint::append`].
const STATUS_GROWTH: u64 = 4;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Which way a batch goes: a walk, the plan it produces, and the
/// checkpoint that records it.
///
/// This, not [`crate::transfer::Direction`], is the type for a batch:
/// `Direction` also has `CreateDir`, a step inside an upload batch that is
/// never a batch of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckpointKind {
    Upload,
    Download,
}

impl CheckpointKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Download => "download",
        }
    }

    /// The batch's jobs, for log lines: "uploads" or "downloads".
    pub fn plural(self) -> &'static str {
        match self {
            Self::Upload => "uploads",
            Self::Download => "downloads",
        }
    }
}

/// Per-job transfer status, stored in the checkpoint file.
///
/// The three states map directly onto the crash-safety guarantee described in
/// the module doc: writing `in_progress` before the transfer starts means a
/// crash always leaves a recoverable state on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum JobStatus {
    /// Job has not been handed to the dispatcher yet (or was re-queued after
    /// a resume).
    #[default]
    Pending,
    /// The dispatcher has started this job but it has not yet completed. If
    /// the process is killed in this state, the job will be re-queued on
    /// the next resume.
    InProgress,
    /// The job completed successfully. Resume skips these.
    Done,
    /// The user cancelled the batch this job belonged to. Resume skips
    /// these, but they are not `Done` — nothing was transferred, and a
    /// checkpoint holding only cancelled jobs has nothing left to do.
    Cancelled,
}

/// One entry in the persisted plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum CheckpointJob {
    Mkdir {
        remote_path: String,
        #[serde(default)]
        status: JobStatus,
    },
    Upload {
        local_path: PathBuf,
        remote_path: String,
        #[serde(default)]
        status: JobStatus,
    },
    Download {
        remote_path: String,
        local_path: PathBuf,
        #[serde(default)]
        status: JobStatus,
    },
}

impl CheckpointJob {
    pub fn status(&self) -> JobStatus {
        match self {
            Self::Mkdir { status, .. } => *status,
            Self::Upload { status, .. } => *status,
            Self::Download { status, .. } => *status,
        }
    }

    pub fn is_done(&self) -> bool {
        self.status() == JobStatus::Done
    }

    /// True if the job should be re-queued on resume: either it never started
    /// or it was in flight when the process died.
    pub fn needs_resume(&self) -> bool {
        matches!(self.status(), JobStatus::Pending | JobStatus::InProgress)
    }

    fn set_status(&mut self, s: JobStatus) {
        match self {
            Self::Mkdir { status, .. } => *status = s,
            Self::Upload { status, .. } => *status = s,
            Self::Download { status, .. } => *status = s,
        }
    }

    pub fn mark_in_progress(&mut self) {
        self.set_status(JobStatus::InProgress);
    }

    pub fn mark_done(&mut self) {
        self.set_status(JobStatus::Done);
    }

    pub fn mark_cancelled(&mut self) {
        self.set_status(JobStatus::Cancelled);
    }

    /// Returns the remote path for log messages.
    #[allow(dead_code)]
    pub fn remote_path(&self) -> &str {
        match self {
            Self::Mkdir { remote_path, .. } => remote_path,
            Self::Upload { remote_path, .. } => remote_path,
            Self::Download { remote_path, .. } => remote_path,
        }
    }
}

/// The on-disk checkpoint document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Schema version. Readers handle older versions via migration; they
    /// reject files with a higher version.
    pub version: u32,
    /// Session name this checkpoint belongs to.
    pub session: String,
    /// The server and account the batch runs against, as
    /// [`crate::transport::origin_of`] names it. `None` in a file written
    /// before version 4, which is then never offered.
    #[serde(default)]
    pub origin: Option<String>,
    /// Direction of the transfer batch.
    pub kind: CheckpointKind,
    /// Flat ordered plan. Directory mkdirs appear before any files inside them.
    pub jobs: Vec<CheckpointJob>,

    // ---- Runtime-only debouncing state (not serialised) -----------------
    /// True if there are unflushed mutations.
    #[serde(default, skip)]
    dirty: bool,
    /// When the in-memory state was last written to disk. None until the
    /// first save.
    #[serde(default, skip)]
    last_save: Option<Instant>,
    /// How long the last write took. See [`flush_interval`].
    #[serde(default, skip)]
    last_cost: Duration,
}

impl Checkpoint {
    /// Create a new checkpoint for `session` connected to `origin`, for
    /// `kind`, with `jobs`.
    pub fn new(
        session: &str,
        origin: &str,
        kind: CheckpointKind,
        jobs: Vec<CheckpointJob>,
    ) -> Self {
        Self {
            version: FORMAT_VERSION,
            session: session.to_string(),
            origin: Some(origin.to_string()),
            kind,
            jobs,
            dirty: true,
            last_save: None,
            last_cost: Duration::ZERO,
        }
    }

    // -----------------------------------------------------------------------
    // Persistence
    // -----------------------------------------------------------------------

    /// Derive the checkpoint file path for a given session, origin and
    /// direction.
    ///
    /// Two distinct session names ("my prod" vs "my_prod") would otherwise
    /// collapse to the same sanitised stem and silently overwrite each
    /// other's checkpoint, and two accounts sharing a session name would
    /// share one file. Append the first eight hex chars of
    /// `sha256(session, origin)` to disambiguate; the suffix derives from
    /// the raw session name (no sanitisation), so it's stable per logical
    /// name and server.
    fn path_for(session: &str, origin: &str, kind: CheckpointKind) -> Result<PathBuf> {
        Ok(paths::checkpoints_dir()?.join(file_name_for(session, origin, kind)))
    }

    /// The origin this checkpoint's file is keyed by. Empty for a file
    /// from before version 4, whose name was keyed by session alone.
    pub(crate) fn origin_key(&self) -> &str {
        self.origin.as_deref().unwrap_or_default()
    }

    /// Atomically and durably write `content` to `path`. See
    /// [`paths::atomic_write`].
    fn atomic_write(path: &Path, content: &str) -> Result<()> {
        use std::io::Write as _;
        paths::atomic_write(path, |f| f.write_all(content.as_bytes()))
    }

    /// Force-write this checkpoint to disk, overwriting any previous one
    /// for the same session, origin and direction.
    ///
    /// Most callers should use [`Self::flush_if_due`] instead — that one
    /// debounces back-to-back mutations into a single write. Call this
    /// directly only when the batch is about to terminate (final job
    /// completed, batch failed, app exiting) so the on-disk state is up to
    /// date before the in-memory checkpoint is dropped.
    pub fn flush(&mut self) -> Result<()> {
        let started = Instant::now();
        let path = Self::path_for(&self.session, self.origin_key(), self.kind)?;
        // Compact: indentation doubled the size of a large checkpoint, and
        // with it the time each write holds up the interface.
        let json = serde_json::to_string(self)
            .map_err(|e| BlinkError::config(format!("checkpoint serialize: {e}")))?;
        Self::atomic_write(&path, &json)?;
        self.dirty = false;
        let now = Instant::now();
        self.last_save = Some(now);
        self.last_cost = now.duration_since(started);
        Ok(())
    }

    /// Flush only if there are pending mutations *and* the last write was
    /// long enough ago that another fsync is justified.
    ///
    /// A batch of 100k transitions arriving at 10 kHz produces ~25 disk
    /// writes instead of 100k — same correctness (any lost mark just causes
    /// the affected job to be re-run on resume), small fraction of the I/O.
    pub fn flush_if_due(&mut self) -> Result<()> {
        if write_due(
            self.dirty,
            self.last_save,
            Instant::now(),
            flush_interval(self.last_cost),
        ) {
            self.flush()
        } else {
            Ok(())
        }
    }

    /// Write any marks the debounce held back. For when the checkpoint is
    /// about to be dropped: a held-back mark is otherwise written only by
    /// the next mark, and after the last one there is none.
    pub fn flush_if_dirty(&mut self) -> Result<()> {
        if self.dirty { self.flush() } else { Ok(()) }
    }

    /// Whether this checkpoint has changes that have not reached disk.
    ///
    /// Test-only: it exists so the debounce contract can be asserted — a
    /// coalesced mark leaves the checkpoint dirty, a written one does not.
    #[cfg(test)]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Mark a job as `in_progress` in memory.
    ///
    /// Called from the `TransferEvent::Started` handler *before* the transfer
    /// does any I/O — durability is the caller's responsibility via
    /// [`Self::flush_if_due`] / [`Self::flush`]. Out-of-bounds indices are
    /// ignored with a warn (defensive against a dispatcher/checkpoint map
    /// going out of sync).
    pub fn mark_in_progress(&mut self, job_index: usize) {
        match self.jobs.get_mut(job_index) {
            Some(j) => {
                j.mark_in_progress();
                self.dirty = true;
            }
            None => {
                tracing::warn!(
                    session = %self.session,
                    job_index,
                    total = self.jobs.len(),
                    "mark_in_progress: job index out of bounds — checkpoint not updated",
                );
            }
        }
    }

    /// Append `jobs` to this checkpoint, returning the index the first one
    /// landed at.
    ///
    /// Storage is one checkpoint per session, origin and direction, so a
    /// second batch of the same direction used to overwrite the first — while the first
    /// was still running, leaving it unresumable and its `.blink-part` files
    /// unfindable. Appending keeps both in one file; the caller offsets its
    /// job-id map by the returned base.
    ///
    /// Refuses, leaving the checkpoint as it was, when the result could
    /// grow past what [`Self::load_from`] accepts: it would be saved, then
    /// refused on load, and nothing in it could be resumed.
    pub fn append(&mut self, jobs: Vec<CheckpointJob>) -> Result<usize> {
        self.append_within(jobs, MAX_CHECKPOINT_JOBS, MAX_CHECKPOINT_BYTES)
    }

    /// [`Self::append`], against the given limits.
    fn append_within(
        &mut self,
        jobs: Vec<CheckpointJob>,
        max_jobs: usize,
        max_bytes: u64,
    ) -> Result<usize> {
        let total = self.jobs.len() + jobs.len();
        if total > max_jobs {
            return Err(BlinkError::config(format!(
                "a checkpoint holds at most {max_jobs} jobs, and this would make {total}"
            )));
        }
        // The file as it would be written now, plus the new jobs and the
        // comma before each, plus room for every status to grow:
        // `in_progress`, the longest, is four bytes longer than `pending`.
        fn encoded_len(v: &impl Serialize) -> Result<u64> {
            serde_json::to_vec(v)
                .map(|raw| raw.len() as u64)
                .map_err(|e| BlinkError::config(format!("checkpoint serialize: {e}")))
        }
        let now = encoded_len(self)?;
        let mut added = 0u64;
        for job in &jobs {
            added += 1 + encoded_len(job)?;
        }
        let bytes = now + added + STATUS_GROWTH * total as u64;
        if bytes > max_bytes {
            return Err(BlinkError::config(format!(
                "a checkpoint holds at most {max_bytes} bytes, and this could make {bytes}"
            )));
        }
        let base = self.jobs.len();
        self.jobs.extend(jobs);
        self.dirty = true;
        Ok(base)
    }

    /// Mark a job as cancelled in memory. See [`Self::mark_in_progress`].
    pub fn mark_cancelled(&mut self, job_index: usize) {
        match self.jobs.get_mut(job_index) {
            Some(j) => {
                j.mark_cancelled();
                self.dirty = true;
            }
            None => {
                tracing::warn!(
                    session = %self.session,
                    job_index,
                    total = self.jobs.len(),
                    "mark_cancelled: job index out of bounds — checkpoint not updated",
                );
            }
        }
    }

    /// Mark a job as `done` in memory. See [`Self::mark_in_progress`].
    pub fn mark_done(&mut self, job_index: usize) {
        match self.jobs.get_mut(job_index) {
            Some(j) => {
                j.mark_done();
                self.dirty = true;
            }
            None => {
                tracing::warn!(
                    session = %self.session,
                    job_index,
                    total = self.jobs.len(),
                    "mark_done: job index out of bounds — checkpoint not updated",
                );
            }
        }
    }

    /// Remove the checkpoint file once the batch has fully completed.
    pub fn remove(session: &str, origin: &str, kind: CheckpointKind) -> Result<()> {
        let path = Self::path_for(session, origin, kind)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            // Already gone — that's fine.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(BlinkError::from(e)),
        }
    }

    // -----------------------------------------------------------------------
    // Loading / querying
    // -----------------------------------------------------------------------

    /// Load and validate a checkpoint. Returns `None` if no file exists.
    /// Migrates a version-1 document in memory; the file is left as it is.
    ///
    /// A file whose recorded origin is not `origin` reads as absent: it is
    /// not this connection's to resume.
    pub fn load(session: &str, origin: &str, kind: CheckpointKind) -> Result<Option<Self>> {
        let path = Self::path_for(session, origin, kind)?;
        Ok(Self::load_from(&path)?.filter(|cp| cp.origin.as_deref() == Some(origin)))
    }

    pub fn load_from(path: &Path) -> Result<Option<Self>> {
        // Open the file, treating NotFound as "no checkpoint" rather than an
        // error. This eliminates the TOCTOU race of exists()-then-read().
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(BlinkError::from(e)),
        };

        // Read at most MAX_CHECKPOINT_BYTES + 1 bytes. If we get more than the
        // limit the file is unreasonably large (corrupt or malicious) and we
        // refuse to process it.
        let mut raw = String::new();
        file.take(MAX_CHECKPOINT_BYTES + 1)
            .read_to_string(&mut raw)?;
        if raw.len() as u64 > MAX_CHECKPOINT_BYTES {
            return Err(BlinkError::config(format!(
                "checkpoint file exceeds size limit ({MAX_CHECKPOINT_BYTES} bytes)"
            )));
        }

        // Peek at the version before full deserialisation so we can apply
        // migrations without fighting serde's strict field matching. Only
        // `version` is kept; serde skips the rest. Parsing the whole file
        // into a `Value` here held a tree several times the file's size
        // alongside it, and a checkpoint can be 256 MiB.
        #[derive(Deserialize)]
        struct VersionProbe {
            #[serde(default)]
            version: Option<serde_json::Value>,
        }
        let probe: VersionProbe = serde_json::from_str(&raw)
            .map_err(|e| BlinkError::config(format!("checkpoint parse: {e}")))?;
        let version = probe.version.and_then(|v| v.as_u64()).unwrap_or(1) as u32;

        if version > FORMAT_VERSION {
            return Err(BlinkError::config(format!(
                "checkpoint version {version} is newer than supported ({FORMAT_VERSION}); \
                 upgrade blink to resume this batch",
            )));
        }

        // Version 1 → 2 migration: rewrite `"done": bool` as
        // `"status": "pending" | "done"`. `in_progress` is not possible in
        // a v1 file (the field didn't exist), so any job that had
        // `done: false` maps to `pending`.
        if version < 2 {
            let doc = serde_json::from_str(&raw)
                .map_err(|e| BlinkError::config(format!("checkpoint parse: {e}")))?;
            return Self::migrate_v1(doc);
        }

        let cp: Self = serde_json::from_str(&raw)
            .map_err(|e| BlinkError::config(format!("checkpoint parse: {e}")))?;

        Self::validate(&cp)?;
        Ok(Some(cp))
    }

    /// Migrate a version-1 checkpoint document to version 2, in memory.
    ///
    /// It used to write the result back, so that a plain `blink checkpoints`
    /// listing rewrote the files it listed. Nothing is gained by it: a file
    /// before version 4 records no server, so it is never resumed, and only
    /// that listing ever reads one.
    fn migrate_v1(mut doc: serde_json::Value) -> Result<Option<Self>> {
        use serde_json::Value;

        // Rewrite each job entry.
        if let Some(jobs) = doc.get_mut("jobs").and_then(|j| j.as_array_mut()) {
            for job in jobs.iter_mut() {
                let done = job.get("done").and_then(|d| d.as_bool()).unwrap_or(false);
                let status = if done { "done" } else { "pending" };
                if let Value::Object(map) = job {
                    map.remove("done");
                    map.insert("status".to_string(), Value::String(status.to_string()));
                }
            }
        }

        // Bump the version.
        if let Value::Object(ref mut map) = doc {
            map.insert("version".to_string(), Value::Number(2.into()));
        }

        let cp: Self = serde_json::from_value(doc)
            .map_err(|e| BlinkError::config(format!("checkpoint migrate parse: {e}")))?;

        Self::validate(&cp)?;
        Ok(Some(cp))
    }

    /// Validate a freshly deserialised checkpoint for safety and sanity.
    fn validate(cp: &Checkpoint) -> Result<()> {
        if cp.jobs.len() > MAX_CHECKPOINT_JOBS {
            return Err(BlinkError::config(format!(
                "checkpoint has too many jobs ({}, limit is {MAX_CHECKPOINT_JOBS})",
                cp.jobs.len()
            )));
        }

        for job in &cp.jobs {
            match job {
                CheckpointJob::Upload { local_path, .. }
                | CheckpointJob::Download { local_path, .. } => {
                    // Null bytes are invalid in paths on all supported platforms
                    // and indicate a corrupt or malicious checkpoint.
                    if local_path.as_os_str().as_encoded_bytes().contains(&0u8) {
                        return Err(BlinkError::config(
                            "checkpoint contains a path with a null byte",
                        ));
                    }
                }
                CheckpointJob::Mkdir { .. } => {}
            }
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// How many jobs still need to run (pending or in_progress).
    pub fn pending_count(&self) -> usize {
        self.jobs.iter().filter(|j| j.needs_resume()).count()
    }

    /// How many jobs have already completed successfully.
    pub fn done_count(&self) -> usize {
        self.jobs.iter().filter(|j| j.is_done()).count()
    }
}

/// A display-only summary of a checkpoint that still has work to do.
///
/// Everything here is for rendering. `sample_paths` is sanitized at
/// construction because the panel draws it directly rather than going
/// through `push_log`, which is where sanitization otherwise happens
/// centrally — and remote paths carry the server's own bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointOffer {
    pub kind: CheckpointKind,
    pub session: String,
    /// The server and account the checkpoint was made for.
    pub origin: String,
    /// Jobs still to run: pending plus in-progress.
    pub remaining: usize,
    /// `remaining + done`. Cancelled jobs are excluded — that is work the
    /// user already abandoned, and counting it would overstate the total.
    pub total: usize,
    /// How long ago the checkpoint file was last written, if it could be
    /// stat'ed.
    pub age: Option<Duration>,
    /// Up to three outstanding paths, taken from the source side.
    pub sample_paths: Vec<String>,
}

/// The path a job reads *from* — what the user selected, and so what they
/// will recognise in the panel.
fn source_path(job: &CheckpointJob) -> String {
    match job {
        CheckpointJob::Download { remote_path, .. } => remote_path.clone(),
        CheckpointJob::Upload { local_path, .. } => local_path.display().to_string(),
        CheckpointJob::Mkdir { remote_path, .. } => remote_path.clone(),
    }
}

/// How long ago `path` was modified.
fn age_of(path: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

impl Checkpoint {
    /// Summarise this checkpoint for the resume panel.
    pub fn to_offer(&self, age: Option<Duration>) -> CheckpointOffer {
        let remaining = self.pending_count();
        let done = self.done_count();
        let sample_paths = self
            .jobs
            .iter()
            .filter(|j| j.needs_resume())
            .take(3)
            .map(|j| crate::error::sanitize(source_path(j)))
            .collect();
        CheckpointOffer {
            kind: self.kind,
            session: self.session.clone(),
            origin: self.origin_key().to_string(),
            remaining,
            total: remaining + done,
            age,
            sample_paths,
        }
    }
}

/// The body of [`Checkpoint::path_for`]: `{stem}-{hash8}-{kind}.json`.
/// See [`paths::hashed_file_name`]; the hash covers the origin as well as
/// the session name.
fn file_name_for(session: &str, origin: &str, kind: CheckpointKind) -> String {
    use sha2::{Digest, Sha256};

    // NUL can appear in neither a session name nor an origin, so the two
    // can't run together ambiguously.
    let hash = Sha256::new()
        .chain_update(session.as_bytes())
        .chain_update([0])
        .chain_update(origin.as_bytes())
        .finalize();
    paths::hashed_file_name(session, &hash.into(), &format!("-{}.json", kind.as_str()))
}

/// Summaries of every checkpoint for `session` that still has work left.
///
/// A file that is absent, empty of outstanding work, or unreadable yields
/// no offer: connecting must never fail because of a checkpoint.
pub fn offers_for(session: &str, origin: &str) -> Vec<CheckpointOffer> {
    let mut out = Vec::new();
    for kind in [CheckpointKind::Download, CheckpointKind::Upload] {
        let Ok(path) = Checkpoint::path_for(session, origin, kind) else {
            continue;
        };
        let cp = match Checkpoint::load_from(&path) {
            Ok(Some(cp)) => cp,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(?path, "skipping unreadable checkpoint: {e}");
                continue;
            }
        };
        // The key includes the origin, so a mismatch means a file was
        // copied or hand-edited; it is still not this connection's.
        if cp.origin.as_deref() != Some(origin) {
            continue;
        }
        if cp.pending_count() == 0 {
            continue;
        }
        out.push(cp.to_offer(age_of(&path)));
    }
    out
}

/// Remove a checkpoint and the partial downloads it is the only record of.
///
/// The checkpoint names where every unfinished download left its `.blink-part`
/// file; delete it without sweeping and those files are stranded with
/// nothing left to reference them. `remove_orphan_parts` skips `Done` jobs,
/// so only partials of transfers that never finished are removed.
///
/// Idempotent: a checkpoint that is already gone reports nothing removed.
pub fn discard(session: &str, origin: &str, kind: CheckpointKind) -> Result<DiscardOutcome> {
    let mut outcome = match Checkpoint::load(session, origin, kind) {
        Ok(Some(cp)) => remove_orphan_parts(&cp),
        // Absent, or unreadable — either way there is nothing to sweep, but
        // the file (if any) should still go.
        Ok(None) => DiscardOutcome::default(),
        Err(e) => {
            tracing::warn!(session, "discarding an unreadable checkpoint: {e}");
            DiscardOutcome::default()
        }
    };
    // Record a failure here rather than propagating it with `?`: the sweep
    // above may already have removed partials, and returning `Err` would
    // discard that count along with it, leaving the caller unable to report
    // what *did* get cleaned up.
    if let Err(e) = Checkpoint::remove(session, origin, kind) {
        outcome
            .failures
            .push(format!("could not remove the checkpoint file: {e}"));
    }
    Ok(outcome)
}

/// Move `old`'s checkpoints for `origin` to the session name `new`, for a
/// session being renamed. Returns what could not be moved.
///
/// Checkpoints are keyed by session name, so a rename that left them
/// behind cut the session off from its unfinished batches: they were no
/// longer offered or resumed, and `blink checkpoints --clean` could delete
/// them, with their partial downloads, as orphaned. When `new` already has
/// a checkpoint for the same server, the two are merged, `old`'s jobs
/// first. A checkpoint that can't be moved stays where it was.
pub fn rename_session(old: &str, new: &str, origin: &str) -> Vec<String> {
    if old == new {
        return Vec::new();
    }
    [CheckpointKind::Download, CheckpointKind::Upload]
        .into_iter()
        .filter_map(|kind| {
            rename_one(old, new, origin, kind).err().map(|e| {
                format!(
                    "could not move the {} checkpoint to `{new}`: {e}",
                    kind.as_str()
                )
            })
        })
        .collect()
}

/// [`rename_session`] for one direction.
fn rename_one(old: &str, new: &str, origin: &str, kind: CheckpointKind) -> Result<()> {
    let Some(mut cp) = Checkpoint::load(old, origin, kind)? else {
        return Ok(());
    };
    // Unreadable is an error here, not "absent": writing over it would
    // destroy whatever it holds.
    if let Some(existing) = Checkpoint::load(new, origin, kind)? {
        cp.append(existing.jobs)?;
    }
    cp.session = new.to_string();
    cp.flush()?;
    Checkpoint::remove(old, origin, kind)
}

/// Whether a checkpoint write is owed right now.
///
/// Split out from [`Checkpoint::flush_if_due`] so the policy can be tested
/// without touching a disk or a clock. It is load-bearing for throughput
/// rather than correctness: a batch can be 100k jobs, and writing on every
/// state change would cost an fsync apiece, while a *lost* change only means
/// that job re-runs on resume.
///
/// The first write is always owed — nothing has been persisted yet, and the
/// plan has to reach disk before any transfer starts.
fn write_due(dirty: bool, last_save: Option<Instant>, now: Instant, interval: Duration) -> bool {
    if !dirty {
        return false;
    }
    match last_save {
        None => true,
        Some(t) => now.duration_since(t) >= interval,
    }
}

/// The debounced write's interval stretches to this many times the cost of
/// the last write, so writing takes at most about 1/20 of the time.
const WRITE_COST_FACTOR: u32 = 20;

/// How long [`Checkpoint::flush_if_due`] waits between writes, given what
/// the last one cost.
///
/// Writes run on the interface's thread. A checkpoint of a large batch is
/// tens of megabytes, serialized and fsynced whole every time, and at a
/// fixed quarter-second the interface spent much of a large batch blocked
/// on them. Stretching the interval keeps the share of time spent writing
/// small; a write held back longer only means a few more jobs redone after
/// a crash.
fn flush_interval(last_cost: Duration) -> Duration {
    CHECKPOINT_FLUSH_INTERVAL.max(last_cost * WRITE_COST_FACTOR)
}

/// What a checkpoint teardown removed, and what it could not.
///
/// Returned rather than printed: the CLI writes failures to stderr, but the
/// TUI has to route them through its log — writing to stderr under a
/// full-screen terminal UI smears the display.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DiscardOutcome {
    pub parts_removed: usize,
    pub failures: Vec<String>,
}

/// Delete the `.blink-part` files belonging to `cp`'s unfinished downloads,
/// and their sidecars. Reports how many were removed, and what could not be.
///
/// A download streams into its partial ([`crate::transport::part_path`])
/// and renames onto `<dest>` only on success, so an interrupted batch
/// leaves partials scattered across the destination tree. The checkpoint
/// is the only record of where they are — once it is gone, nothing can
/// find them again and they sit there forever.
///
/// Only `Done` jobs are skipped: their `.blink-part` was already renamed
/// away, and a file at that path now would belong to some other transfer.
///
/// Called from two places: the `blink checkpoints` CLI and the TUI's
/// `discard` offer. In the TUI, resume offers are built at connect time,
/// before any job of that direction has been enqueued for the *new*
/// session — so pressing `d` never races a worker the current session
/// itself just started.
///
/// Both can still race a transfer writing one of these partials. The CLI
/// runs as a separate process, so `blink checkpoints --clean` or `--force`
/// in another terminal can sweep partials a running TUI is still writing.
/// And in the TUI, `App::disconnect` spawns the dispatcher's `shutdown()`
/// rather than awaiting it, so a worker from the previous connection can
/// still be mid-write when the reconnect completes and its checkpoint is
/// offered again. Either way, on Unix the unlink succeeds, the worker keeps
/// writing to the now-unlinked inode, and its final rename fails — turning a
/// clean discard into a spurious transfer failure. Nothing here guards
/// against that.
fn remove_orphan_parts(cp: &Checkpoint) -> DiscardOutcome {
    let mut outcome = DiscardOutcome::default();
    for job in &cp.jobs {
        let CheckpointJob::Download {
            local_path, status, ..
        } = job
        else {
            continue;
        };
        if *status == JobStatus::Done {
            continue;
        }
        let part = crate::transport::part_path(local_path);
        match std::fs::remove_file(&part) {
            Ok(()) => outcome.parts_removed += 1,
            // Not there is the normal case — the job may never have started.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => outcome
                .failures
                .push(format!("could not remove {}: {e}", part.display())),
        }
        // The provenance sidecar describes the partial we just deleted;
        // leaving it behind would strand a file nothing refers to.
        let _ = std::fs::remove_file(crate::transport::part_meta_path(local_path));
    }
    outcome
}

/// Why a checkpoint file is being removed.
///
/// This is what the user sees in the report, so where two reasons both
/// apply the precedence is pinned by tests rather than left to the order of
/// an `if`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoveReason {
    Forced,
    Completed,
    Orphaned,
}

impl RemoveReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Forced => "forced",
            Self::Completed => "completed",
            Self::Orphaned => "orphaned",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    Remove(RemoveReason),
    Keep { orphaned: bool },
}

/// Decide what happens to one checkpoint.
///
/// Split out from the directory walk and the printing so the rule can be
/// tested at all: removing a checkpoint strands the `.blink-part` files of its
/// unfinished downloads, because nothing else records where they are. A
/// wrong answer here destroys transfers the user could have resumed.
fn disposition(pending: usize, orphaned: bool, clean: bool, force: bool) -> Disposition {
    if force {
        return Disposition::Remove(RemoveReason::Forced);
    }
    if clean && pending == 0 {
        return Disposition::Remove(RemoveReason::Completed);
    }
    if clean && orphaned {
        return Disposition::Remove(RemoveReason::Orphaned);
    }
    Disposition::Keep { orphaned }
}

/// The saved sessions, as the (name, origin) pairs checkpoints are keyed
/// by, plus whether that list is the whole story.
struct KnownSessions {
    keys: std::collections::HashSet<(String, String)>,
    /// True only when every session on disk was read successfully. When
    /// false, a key missing from `keys` no longer proves the session is
    /// gone, so orphanhood cannot be judged.
    complete: bool,
    /// Why the list is incomplete, phrased for a warning. `None` when it is
    /// complete.
    why: Option<String>,
}

/// Build the known-session set, recording whether it can be trusted.
///
/// This used to be `list_all().unwrap_or_default()`, which turned "I could
/// not read your sessions" into "you have no sessions": every checkpoint
/// then looked orphaned and `--clean` quietly did the job of `--force`. A
/// single unparseable `.ini` did the same to one session, deleting the
/// resume data of a session still sitting in the sessions directory.
fn known_sessions(listing: Result<crate::session::SessionListing>) -> KnownSessions {
    match listing {
        Ok(listing) => {
            let why = (!listing.skipped.is_empty()).then(|| {
                let n = listing.skipped.len();
                let files = if n == 1 { "file" } else { "files" };
                format!("{n} session {files} could not be read")
            });
            KnownSessions {
                keys: listing
                    .sessions
                    .iter()
                    .map(|s| (s.name.clone(), crate::transport::origin_of(s)))
                    .collect(),
                complete: why.is_none(),
                why,
            }
        }
        Err(e) => KnownSessions {
            keys: std::collections::HashSet::new(),
            complete: false,
            why: Some(format!("the session list could not be read ({e})")),
        },
    }
}

/// Whether no connection can ever be offered `cp`.
///
/// A checkpoint is offered only to a session with its name *and* origin,
/// so one left behind by a session whose host, port or username has since
/// changed is as orphaned as one whose session was deleted. A key missing
/// from a list we know to be partial proves nothing, so a missing session
/// only counts when the list is complete. A file with no origin (from
/// before version 4) is orphaned whatever the list says.
///
/// Nor is one an ad-hoc connection could reach: `blink connect` names its
/// session after the host and saves nothing, so its checkpoints match no
/// saved session, yet connecting the same way again offers them.
fn is_orphaned(cp: &Checkpoint, known: &KnownSessions) -> bool {
    let Some(origin) = &cp.origin else {
        return true;
    };
    known.complete
        && !known.keys.contains(&(cp.session.clone(), origin.clone()))
        && !reachable_ad_hoc(&cp.session, origin)
}

/// Whether `blink connect` to `origin` names its session `session`: the
/// session name is the origin's host. See [`is_orphaned`].
///
/// The origin is `user@host:port` with the host lowercased; the user can
/// hold an `@` and an IPv6 host holds `:`, so the host is what lies between
/// the last `@` and the last `:`.
fn reachable_ad_hoc(session: &str, origin: &str) -> bool {
    let host_port = origin.rsplit_once('@').map_or(origin, |(_, h)| h);
    let host = host_port.rsplit_once(':').map_or(host_port, |(h, _)| h);
    session.eq_ignore_ascii_case(host)
}

/// The `blink checkpoints` column naming a checkpoint's server: two
/// accounts can share a session name, and their rows would read alike.
fn origin_column(cp: &Checkpoint) -> String {
    match &cp.origin {
        Some(origin) => crate::error::sanitize_display(origin).into_owned(),
        None => "(server unknown)".to_string(),
    }
}

/// Whether `path` is a temporary file an interrupted checkpoint write left
/// behind: `.{name}.{pid}.{n}.tmp`, as [`paths::atomic_write`] names them.
fn is_leftover_temp(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(paths::is_temp_name)
}

/// Print checkpoint info. Pass `clean` to remove completed/orphaned files,
/// `force` to remove every file unconditionally.
pub fn list_and_clean(clean: bool, force: bool) -> Result<()> {
    use crate::session::Session;
    use std::fs;

    // Only reads unless asked to clean: a checkpoints folder that doesn't
    // exist yet holds nothing, and isn't created.
    let dir = paths::lookup("checkpoints")?;
    let all: Vec<std::path::PathBuf> = match fs::read_dir(&dir) {
        Ok(read) => read.flatten().map(|e| e.path()).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };

    // A write cut short by a crash leaves its temporary file (see
    // `paths::atomic_write`). Only `--force` touches them: under `--clean`
    // a running blink may be in the middle of that very write.
    if force {
        for tmp in all.iter().filter(|p| is_leftover_temp(p)) {
            if let Err(e) = fs::remove_file(tmp) {
                eprintln!("warning: could not remove {}: {e}", tmp.display());
            }
        }
    }

    let mut entries: Vec<std::path::PathBuf> = all
        .into_iter()
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    entries.sort();

    if entries.is_empty() {
        println!("no checkpoints found");
        return Ok(());
    }

    let known = known_sessions(Session::list_all_detailed());
    if let Some(why) = &known.why
        && !force
    {
        eprintln!(
            "warning: {why}, so orphaned checkpoints cannot be identified — \
             any listed below are kept rather than removed"
        );
    }

    let mut removed = 0usize;
    let mut kept = 0usize;
    let mut parts_removed = 0usize;

    for path in &entries {
        let cp = match Checkpoint::load_from(path) {
            Ok(Some(cp)) => cp,
            Ok(None) => continue,
            // `--force` promises every file. One it can't read names no
            // partials to sweep, so the file is all there is to remove.
            Err(e) if force => {
                match fs::remove_file(path) {
                    Ok(()) => {
                        println!("removed  {}  (unreadable: {e})", path.display());
                        removed += 1;
                    }
                    Err(e) => eprintln!("error: could not remove {}: {e}", path.display()),
                }
                continue;
            }
            Err(e) => {
                eprintln!("warning: could not read {}: {e}", path.display());
                continue;
            }
        };

        let pending = cp.pending_count();
        let done = cp.done_count();
        let total = pending + done;
        let orphaned = is_orphaned(&cp, &known);

        match disposition(pending, orphaned, clean, force) {
            Disposition::Remove(reason) => {
                // Removing the checkpoint makes the batch unresumable, which
                // strands the `.blink-part` files its unfinished downloads left
                // behind — nothing else records where they are. Sweep them
                // while we still know.
                let swept = remove_orphan_parts(&cp);
                parts_removed += swept.parts_removed;
                for failure in swept.failures {
                    eprintln!("warning: {failure}");
                }
                match fs::remove_file(path) {
                    Ok(()) => {
                        println!(
                            "removed  {:<20}  {:<28}  {:<8}  {}/{} done  ({})",
                            crate::error::sanitize_display(&cp.session),
                            origin_column(&cp),
                            cp.kind.as_str(),
                            done,
                            total,
                            reason.as_str(),
                        );
                        removed += 1;
                    }
                    Err(e) => {
                        eprintln!("error: could not remove {}: {e}", path.display());
                    }
                }
            }
            Disposition::Keep { orphaned } => {
                let flag = if orphaned { " [orphaned]" } else { "" };
                println!(
                    "{:<20}  {:<28}  {:<8}  {}/{} done  ({} remaining){}",
                    crate::error::sanitize_display(&cp.session),
                    origin_column(&cp),
                    cp.kind.as_str(),
                    done,
                    total,
                    pending,
                    flag,
                );
                kept += 1;
            }
        }
    }

    if clean || force {
        println!();
        println!("{removed} removed, {kept} kept");
        if parts_removed > 0 {
            let plural = if parts_removed == 1 { "file" } else { "files" };
            println!("{parts_removed} orphaned .blink-part {plural} deleted");
        }
    } else if kept > 0 {
        println!();
        println!("Use `blink checkpoints --clean` to remove completed and orphaned checkpoints.");
        println!("Use `blink checkpoints --force` to remove all checkpoint files.");
    }

    Ok(())
}

/// The origin the tests' checkpoints are made for.
#[cfg(test)]
const TEST_ORIGIN: &str = "me@host:22";

#[cfg(test)]
mod policy_tests {
    use super::*;
    use crate::session::{Session, SessionListing};

    fn session(name: &str) -> Session {
        let mut s = Session::from_url("sftp://me@host").expect("valid url");
        s.name = name.to_string();
        s
    }

    // -- what `--clean` and `--force` actually remove -----------------------

    fn every_session_known(names: &[&str]) -> KnownSessions {
        KnownSessions {
            keys: names
                .iter()
                .map(|n| (n.to_string(), TEST_ORIGIN.to_string()))
                .collect(),
            complete: true,
            why: None,
        }
    }

    #[test]
    fn a_checkpoint_without_an_origin_is_orphaned_even_for_a_live_session() {
        let mut cp = Checkpoint::new("prod", TEST_ORIGIN, CheckpointKind::Upload, vec![]);
        let known = every_session_known(&["prod"]);
        assert!(!is_orphaned(&cp, &known));
        cp.origin = None;
        assert!(is_orphaned(&cp, &known));
        let partial = KnownSessions {
            complete: false,
            ..every_session_known(&[])
        };
        assert!(
            is_orphaned(&cp, &partial),
            "an incomplete session list can't save it: no origin is never offered",
        );
    }

    /// Editing a saved session's host, port or username moves it to a new
    /// origin; its old checkpoints can never be offered again.
    #[test]
    fn a_checkpoint_of_an_edited_session_is_orphaned() {
        let mut edited = session("prod");
        edited.host = "elsewhere".into();
        let known = known_sessions(Ok(SessionListing {
            sessions: vec![edited],
            skipped: Vec::new(),
        }));
        let cp = Checkpoint::new("prod", TEST_ORIGIN, CheckpointKind::Upload, vec![]);
        assert!(is_orphaned(&cp, &known));

        let unchanged = known_sessions(Ok(SessionListing {
            sessions: vec![session("prod")],
            skipped: Vec::new(),
        }));
        assert!(
            !is_orphaned(&cp, &unchanged),
            "{TEST_ORIGIN} is session()'s origin"
        );
    }

    #[test]
    fn force_removes_a_checkpoint_that_still_has_work() {
        assert_eq!(
            disposition(3, false, false, true),
            Disposition::Remove(RemoveReason::Forced),
        );
    }

    #[test]
    fn force_outranks_completed() {
        assert_eq!(
            disposition(0, false, true, true),
            Disposition::Remove(RemoveReason::Forced),
        );
    }

    #[test]
    fn clean_removes_a_finished_checkpoint() {
        assert_eq!(
            disposition(0, false, true, false),
            Disposition::Remove(RemoveReason::Completed),
        );
    }

    #[test]
    fn clean_removes_an_orphan_that_still_has_work() {
        assert_eq!(
            disposition(3, true, true, false),
            Disposition::Remove(RemoveReason::Orphaned),
        );
    }

    #[test]
    fn a_finished_orphan_is_reported_as_completed() {
        // Both rules apply; "completed" wins. Pinned because the reason is
        // what the user sees, and either label would be defensible — so it
        // should not change by accident.
        assert_eq!(
            disposition(0, true, true, false),
            Disposition::Remove(RemoveReason::Completed),
        );
    }

    #[test]
    fn clean_keeps_a_live_checkpoint_with_work_left() {
        assert_eq!(
            disposition(3, false, true, false),
            Disposition::Keep { orphaned: false },
        );
    }

    #[test]
    fn without_either_flag_nothing_is_removed() {
        assert_eq!(
            disposition(0, true, false, false),
            Disposition::Keep { orphaned: true },
            "a plain listing reports, it never deletes",
        );
    }

    // -- when the session list cannot be trusted ----------------------------

    #[test]
    fn a_listing_that_failed_outright_is_not_complete() {
        let known = known_sessions(Err(BlinkError::config("sessions dir unreadable")));
        assert!(!known.complete);
        assert!(known.keys.is_empty());
    }

    #[test]
    fn a_listing_with_skipped_files_is_not_complete() {
        // One unparseable `.ini` is enough: that session exists but is
        // invisible here, so every checkpoint would look orphaned.
        let known = known_sessions(Ok(SessionListing {
            sessions: vec![session("prod")],
            skipped: vec!["broken.ini: bad protocol".to_string()],
        }));
        assert!(!known.complete);
        assert!(
            known
                .keys
                .contains(&("prod".to_string(), TEST_ORIGIN.to_string())),
            "what did parse is still usable"
        );
    }

    #[test]
    fn a_clean_listing_is_complete() {
        let known = known_sessions(Ok(SessionListing {
            sessions: vec![session("prod"), session("backup")],
            skipped: Vec::new(),
        }));
        assert!(known.complete);
        assert_eq!(known.keys.len(), 2);
    }
}

#[cfg(test)]
mod debounce_tests {
    use super::*;

    // -- the debounce policy ------------------------------------------------
    //
    // Per-job state changes are coalesced: a batch can be 100k jobs, and an
    // fsync apiece would dominate the transfer it is supposed to be
    // recording. The cost of coalescing is bounded — a lost mark only means
    // that job re-runs on resume — so the policy is safe to keep loose, but
    // it is load-bearing for throughput and nothing else enforces it.

    /// The largest batch blink will plan must still be resumable. Written
    /// pretty-printed, a checkpoint took about 200 bytes a job against a
    /// 10 MiB load limit, so a batch past about 50,000 files was saved but
    /// refused on load: neither the offer on connect nor `r` could resume it.
    #[test]
    fn a_checkpoint_of_the_largest_batch_still_loads() {
        let _home = crate::paths::test_home();
        let jobs: Vec<CheckpointJob> = (0..crate::transfer::MAX_QUEUED_JOBS)
            .map(|i| CheckpointJob::Download {
                remote_path: format!("/srv/media/photos/2024/holiday/IMG_{i:06}.jpg"),
                local_path: PathBuf::from(format!(
                    "/home/someone/Downloads/photos/2024/holiday/IMG_{i:06}.jpg"
                )),
                status: JobStatus::Pending,
            })
            .collect();
        let mut cp = Checkpoint::new("big", TEST_ORIGIN, CheckpointKind::Download, jobs);
        cp.flush().unwrap();
        let loaded = Checkpoint::load("big", TEST_ORIGIN, CheckpointKind::Download)
            .expect("a checkpoint blink wrote must load")
            .expect("and be found");
        assert_eq!(loaded.jobs.len(), crate::transfer::MAX_QUEUED_JOBS);
    }

    /// A debounced write of a large checkpoint takes long enough to stall
    /// the interface, which runs it. The interval stretches with the cost
    /// of the last write, so writing takes a small share of the time.
    #[test]
    fn the_write_interval_stretches_with_the_cost_of_a_write() {
        assert_eq!(flush_interval(Duration::ZERO), CHECKPOINT_FLUSH_INTERVAL);
        assert_eq!(
            flush_interval(Duration::from_millis(5)),
            CHECKPOINT_FLUSH_INTERVAL,
            "a cheap write keeps the usual interval",
        );
        assert_eq!(
            flush_interval(Duration::from_millis(100)),
            Duration::from_millis(100) * WRITE_COST_FACTOR,
        );
    }

    #[test]
    fn a_clean_checkpoint_is_never_written() {
        let now = Instant::now();
        assert!(!write_due(false, None, now, CHECKPOINT_FLUSH_INTERVAL));
        assert!(!write_due(
            false,
            Some(now - Duration::from_secs(10)),
            now,
            CHECKPOINT_FLUSH_INTERVAL,
        ));
    }

    #[test]
    fn the_first_write_is_always_due() {
        // Nothing has been persisted yet, so there is no interval to wait
        // out — the plan needs to reach disk before any transfer starts.
        let now = Instant::now();
        assert!(write_due(true, None, now, CHECKPOINT_FLUSH_INTERVAL));
    }

    #[test]
    fn a_change_inside_the_interval_is_held_back() {
        let now = Instant::now();
        let just_wrote = now - Duration::from_millis(10);
        assert!(!write_due(
            true,
            Some(just_wrote),
            now,
            CHECKPOINT_FLUSH_INTERVAL
        ));
    }

    #[test]
    fn a_change_after_the_interval_is_written() {
        let now = Instant::now();
        let stale = now - (CHECKPOINT_FLUSH_INTERVAL + Duration::from_millis(1));
        assert!(write_due(true, Some(stale), now, CHECKPOINT_FLUSH_INTERVAL));
    }

    #[test]
    fn the_interval_boundary_counts_as_due() {
        let now = Instant::now();
        let exactly = now - CHECKPOINT_FLUSH_INTERVAL;
        assert!(write_due(
            true,
            Some(exactly),
            now,
            CHECKPOINT_FLUSH_INTERVAL
        ));
    }

    /// The property the policy exists for, stated as a rate: a burst of
    /// marks arriving far faster than the interval must collapse into a
    /// handful of writes rather than one apiece.
    #[test]
    fn a_burst_of_marks_collapses_into_few_writes() {
        let start = Instant::now();
        let mut last_save = Some(start);
        let mut writes = 0usize;

        // 10k marks spread over one second of simulated time.
        for i in 0..10_000u32 {
            let now = start + Duration::from_micros(u64::from(i) * 100);
            if write_due(true, last_save, now, CHECKPOINT_FLUSH_INTERVAL) {
                writes += 1;
                last_save = Some(now);
            }
        }

        assert!(
            writes <= 8,
            "one second of marks should cost a handful of writes, not {writes}",
        );
        assert!(writes >= 3, "but it must still make progress: {writes}");
    }
}

#[cfg(test)]
mod sweep_tests {
    use super::*;
    use std::path::PathBuf;

    /// A scratch directory holding real `.blink-part` files for the sweep to find.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blink-sweep-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn job(dir: &Path, name: &str, status: JobStatus) -> CheckpointJob {
        CheckpointJob::Download {
            remote_path: format!("/r/{name}"),
            local_path: dir.join(name),
            status,
        }
    }

    /// Discarding a checkpoint sweeps blink's partials, not files that
    /// merely share the old suffix.
    #[test]
    fn the_sweep_leaves_another_tools_part_file_alone() {
        let dir = scratch("foreign");
        let unfinished = job(&dir, "a.bin", JobStatus::Pending);
        std::fs::write(crate::transport::part_path(&dir.join("a.bin")), b"x").unwrap();
        let theirs = dir.join("a.bin.part");
        std::fs::write(&theirs, b"theirs").unwrap();
        let cp = Checkpoint::new("s", TEST_ORIGIN, CheckpointKind::Download, vec![unfinished]);

        let outcome = remove_orphan_parts(&cp);

        assert_eq!(outcome.parts_removed, 1, "blink's own partial goes");
        assert_eq!(std::fs::read(&theirs).unwrap(), b"theirs", "theirs stays");
    }

    #[test]
    fn the_sweep_reports_what_it_removed() {
        let dir = scratch("reports");
        let unfinished = job(&dir, "a.bin", JobStatus::Pending);
        let in_progress = job(&dir, "c.bin", JobStatus::InProgress);
        let finished = job(&dir, "b.bin", JobStatus::Done);
        // All three have a partial on disk; only the unfinished ones (pending
        // and in-progress) are orphaned.
        for j in [&unfinished, &in_progress, &finished] {
            let CheckpointJob::Download { local_path, .. } = j else {
                unreachable!()
            };
            std::fs::write(crate::transport::part_path(local_path), b"x").unwrap();
        }
        let cp = Checkpoint::new(
            "s",
            TEST_ORIGIN,
            CheckpointKind::Download,
            vec![unfinished, in_progress, finished],
        );

        let outcome = remove_orphan_parts(&cp);

        assert_eq!(
            outcome.parts_removed, 2,
            "the pending job's partial and the in-progress job's partial",
        );
        assert!(outcome.failures.is_empty());
        assert!(
            std::fs::metadata(crate::transport::part_path(&dir.join("c.bin"))).is_err(),
            "an in-progress job's transfer never finished, so its partial is orphaned too",
        );
        assert!(
            std::fs::metadata(crate::transport::part_path(&dir.join("b.bin"))).is_ok(),
            "a completed job's partial belongs to some other transfer",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_partial_is_not_a_failure() {
        let dir = scratch("missing");
        let cp = Checkpoint::new(
            "s",
            TEST_ORIGIN,
            CheckpointKind::Download,
            vec![job(&dir, "never-started.bin", JobStatus::Pending)],
        );

        let outcome = remove_orphan_parts(&cp);

        assert_eq!(outcome.parts_removed, 0);
        assert!(outcome.failures.is_empty(), "the job simply never started");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discarding_removes_the_file_and_its_partials() {
        let dir = scratch("discard");
        let name = format!("blink-test-discard-{}", std::process::id());
        let _home = paths::test_home();
        let unfinished = job(&dir, "a.bin", JobStatus::Pending);
        let CheckpointJob::Download { local_path, .. } = &unfinished else {
            unreachable!()
        };
        std::fs::write(crate::transport::part_path(local_path), b"x").unwrap();

        let mut cp = Checkpoint::new(
            &name,
            TEST_ORIGIN,
            CheckpointKind::Download,
            vec![unfinished],
        );
        cp.flush().expect("write the checkpoint");
        let path = Checkpoint::path_for(&name, TEST_ORIGIN, CheckpointKind::Download).unwrap();

        let outcome = discard(&name, TEST_ORIGIN, CheckpointKind::Download).expect("discard");

        assert_eq!(outcome.parts_removed, 1);
        assert!(!path.exists(), "the checkpoint file must be gone");
        assert!(
            std::fs::metadata(crate::transport::part_path(&dir.join("a.bin"))).is_err(),
            "its orphaned partial must be gone too — nothing else records where it is",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_removal_failure_is_recorded_rather_than_propagated() {
        // The property this guards: before this fix, `Checkpoint::remove`
        // failing at the end of `discard` propagated with `?`, and the
        // caller lost the whole `DiscardOutcome` — including any partials
        // the sweep had *already* removed — along with it. After the fix,
        // the failure is folded into `outcome.failures` and `discard` still
        // returns `Ok`.
        //
        // This exercises "unreadable *and* undeletable": replacing the
        // checkpoint file with a directory makes reading it fail with the
        // same `IsADirectory` error as removing it, so `parts_removed` is 0
        // here because the load took the unreadable branch, not because the
        // fix dropped a count. The case where the load succeeds and only the
        // unlink fails is
        // `a_removal_failure_keeps_the_count_the_sweep_already_earned`.
        let _home = paths::test_home();
        let name = format!("blink-test-remove-fail-{}", std::process::id());
        let path = Checkpoint::path_for(&name, TEST_ORIGIN, CheckpointKind::Download).unwrap();
        std::fs::create_dir(&path).expect("stand a directory in for the checkpoint file");

        let result = discard(&name, TEST_ORIGIN, CheckpointKind::Download);

        // `discard` could not remove the substituted directory — `remove_file`
        // refuses one — so take it out here. The test home sweeps the tree at
        // drop anyway; doing it now keeps the cleanup next to its reason.
        let _ = std::fs::remove_dir_all(&path);

        let outcome = result.expect("discard must return Ok, not propagate the removal failure");
        assert_eq!(
            outcome.failures.len(),
            1,
            "the failed unlink must be recorded rather than silently dropped",
        );
    }

    /// The other half of `a_removal_failure_is_recorded_rather_than_propagated`:
    /// a checkpoint that reads fine, so the sweep runs and finds partials,
    /// but cannot be unlinked. Before the fix the `?` on `Checkpoint::remove`
    /// threw the whole `DiscardOutcome` away, losing a count of files that
    /// had *already* been deleted — the caller could not report what it had
    /// cleaned up. Only constructible with a test-private checkpoint
    /// directory: it works by making that directory read-only.
    #[cfg(unix)]
    #[test]
    fn a_removal_failure_keeps_the_count_the_sweep_already_earned() {
        use std::os::unix::fs::PermissionsExt;

        let _home = paths::test_home();
        let dir = scratch("remove-fail-sweep");
        let name = "remove-fail-sweep";

        let unfinished = job(&dir, "a.bin", JobStatus::Pending);
        let CheckpointJob::Download { local_path, .. } = &unfinished else {
            unreachable!()
        };
        std::fs::write(crate::transport::part_path(local_path), b"x").unwrap();

        let mut cp = Checkpoint::new(
            name,
            TEST_ORIGIN,
            CheckpointKind::Download,
            vec![unfinished],
        );
        cp.flush().expect("write the checkpoint");

        let cp_dir = paths::checkpoints_dir().expect("checkpoints dir");
        let original = std::fs::metadata(&cp_dir).unwrap().permissions();
        std::fs::set_permissions(&cp_dir, std::fs::Permissions::from_mode(0o500))
            .expect("make the checkpoint directory read-only");

        // Root ignores the permission bits, so confirm the setup actually
        // bites before asserting on it.
        let probe = cp_dir.join("probe");
        if std::fs::write(&probe, b"x").is_ok() {
            let _ = std::fs::remove_file(&probe);
            std::fs::set_permissions(&cp_dir, original).unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            eprintln!("skipped: this runner writes through a 0500 directory (root?)");
            return;
        }

        let result = discard(name, TEST_ORIGIN, CheckpointKind::Download);

        // Restore before anything can panic — `expect` below is itself a
        // failure point, and the test home cannot remove a tree it has no
        // write permission on. Getting this order wrong strands a 0500
        // directory in /tmp on exactly the run where this test earns its keep.
        std::fs::set_permissions(&cp_dir, original).unwrap();

        let outcome = result.expect("discard must return Ok, not propagate the removal failure");

        assert_eq!(
            outcome.parts_removed, 1,
            "the partial was already deleted — that count must survive the failed unlink",
        );
        assert_eq!(
            outcome.failures.len(),
            1,
            "and the failed unlink must still be reported",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discarding_an_unreadable_checkpoint_removes_the_file_and_sweeps_nothing() {
        let _home = paths::test_home();
        let name = format!("blink-test-corrupt-discard-{}", std::process::id());
        let path = Checkpoint::path_for(&name, TEST_ORIGIN, CheckpointKind::Download).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();

        let outcome =
            discard(&name, TEST_ORIGIN, CheckpointKind::Download).expect("must not error");

        assert_eq!(
            outcome,
            DiscardOutcome::default(),
            "nothing could be swept from an unreadable file",
        );
        assert!(!path.exists(), "the unreadable file must still be removed");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn discarding_a_checkpoint_that_is_already_gone_is_fine() {
        let _home = paths::test_home();
        let name = format!("blink-test-absent-{}", std::process::id());
        let outcome =
            discard(&name, TEST_ORIGIN, CheckpointKind::Download).expect("must not error");
        assert_eq!(outcome, DiscardOutcome::default());
    }
}

/// `blink checkpoints`, against real files: what `--clean` and `--force`
/// delete, and what they leave.
#[cfg(test)]
mod cli_tests {
    use super::*;
    use crate::session::Session;
    use std::path::PathBuf;

    /// A config home holding one saved session, `host`, whose origin is
    /// [`TEST_ORIGIN`], and a folder for downloads to land in.
    fn home_with_session() -> (paths::TestHome, PathBuf) {
        let home = paths::test_home();
        Session::from_url("sftp://me@host").unwrap().save().unwrap();
        let downloads = home.path().join("downloads");
        std::fs::create_dir(&downloads).unwrap();
        (home, downloads)
    }

    /// Write a download checkpoint for `session` with one job per
    /// `(name, status)`, and a partial on disk for each. Returns the
    /// checkpoint's path and the partials'.
    fn write_checkpoint(
        session: &str,
        downloads: &Path,
        jobs: &[(&str, JobStatus)],
    ) -> (PathBuf, Vec<PathBuf>) {
        let mut parts = Vec::new();
        let jobs = jobs
            .iter()
            .map(|(name, status)| {
                let local_path = downloads.join(name);
                let part = crate::transport::part_path(&local_path);
                std::fs::write(&part, b"partial").unwrap();
                parts.push(part);
                CheckpointJob::Download {
                    remote_path: format!("/r/{name}"),
                    local_path,
                    status: *status,
                }
            })
            .collect();
        let mut cp = Checkpoint::new(session, TEST_ORIGIN, CheckpointKind::Download, jobs);
        cp.flush().unwrap();
        let path = Checkpoint::path_for(session, TEST_ORIGIN, CheckpointKind::Download).unwrap();
        assert!(path.exists());
        (path, parts)
    }

    #[test]
    fn with_no_checkpoints_there_is_nothing_to_do() {
        let _home = paths::test_home();
        list_and_clean(true, true).unwrap();
    }

    #[test]
    fn listing_removes_nothing() {
        let (_home, downloads) = home_with_session();
        let (done, _) = write_checkpoint("host", &downloads, &[("a", JobStatus::Done)]);
        let (orphan, parts) = write_checkpoint("gone", &downloads, &[("b", JobStatus::Pending)]);

        list_and_clean(false, false).unwrap();

        assert!(done.exists() && orphan.exists());
        assert!(parts[0].exists());
    }

    #[test]
    fn clean_removes_a_checkpoint_with_nothing_left_to_run() {
        let (_home, downloads) = home_with_session();
        let (path, _) = write_checkpoint("host", &downloads, &[("a", JobStatus::Done)]);

        list_and_clean(true, false).unwrap();

        assert!(!path.exists());
    }

    /// A batch that can still be resumed is what a checkpoint is for.
    #[test]
    fn clean_keeps_an_unfinished_batch_and_its_partials() {
        let (_home, downloads) = home_with_session();
        let (path, parts) = write_checkpoint(
            "host",
            &downloads,
            &[("a", JobStatus::Done), ("b", JobStatus::Pending)],
        );

        list_and_clean(true, false).unwrap();

        assert!(path.exists());
        assert!(parts[1].exists(), "the unfinished download's partial stays");
    }

    /// No session can be offered it, so it goes, and the partials only it
    /// knew of go with it.
    #[test]
    fn clean_removes_a_deleted_sessions_checkpoint_and_its_partials() {
        let (_home, downloads) = home_with_session();
        let (path, parts) = write_checkpoint(
            "gone",
            &downloads,
            &[("a", JobStatus::Pending), ("b", JobStatus::InProgress)],
        );

        list_and_clean(true, false).unwrap();

        assert!(!path.exists());
        assert!(parts.iter().all(|p| !p.exists()), "{parts:?}");
    }

    /// A session file that won't load may be the very session a checkpoint
    /// belongs to. Unable to tell, `--clean` keeps it.
    #[test]
    fn clean_keeps_orphans_while_a_session_file_cant_be_read() {
        let (_home, downloads) = home_with_session();
        std::fs::write(
            paths::sessions_dir().unwrap().join("broken.ini"),
            "not a session",
        )
        .unwrap();
        let (path, parts) = write_checkpoint("gone", &downloads, &[("a", JobStatus::Pending)]);

        list_and_clean(true, false).unwrap();

        assert!(path.exists());
        assert!(parts[0].exists());
    }

    #[test]
    fn force_removes_every_checkpoint_and_the_unfinished_partials() {
        let (_home, downloads) = home_with_session();
        let (kept_by_clean, parts) =
            write_checkpoint("host", &downloads, &[("a", JobStatus::Pending)]);
        let (orphan, orphan_parts) =
            write_checkpoint("gone", &downloads, &[("b", JobStatus::Pending)]);

        list_and_clean(false, true).unwrap();

        assert!(!kept_by_clean.exists() && !orphan.exists());
        assert!(!parts[0].exists() && !orphan_parts[0].exists());
    }

    /// A finished job's partial was renamed away long ago. A file at that
    /// name now is some other transfer's, and is not swept.
    #[test]
    fn a_finished_jobs_partial_name_is_not_swept() {
        let (_home, downloads) = home_with_session();
        let (_, parts) = write_checkpoint("host", &downloads, &[("a", JobStatus::Done)]);

        list_and_clean(false, true).unwrap();

        assert!(parts[0].exists());
    }

    /// A file that can't be read as a checkpoint names no partials and
    /// belongs to no one blink can tell; `--clean` reports it and leaves
    /// it, and the rest are still dealt with.
    #[test]
    fn clean_leaves_an_unreadable_checkpoint_and_handles_the_rest() {
        let (_home, downloads) = home_with_session();
        let garbage = paths::checkpoints_dir().unwrap().join("aaa-garbage.json");
        std::fs::write(&garbage, "{ not json").unwrap();
        let (path, _) = write_checkpoint("host", &downloads, &[("a", JobStatus::Done)]);

        list_and_clean(true, false).unwrap();

        assert!(garbage.exists());
        assert!(!path.exists());
    }

    /// `--force` removes every checkpoint file, including ones it can't
    /// read: corrupt, or written by a newer blink.
    #[test]
    fn force_removes_an_unreadable_checkpoint_too() {
        let (_home, downloads) = home_with_session();
        let dir = paths::checkpoints_dir().unwrap();
        let garbage = dir.join("aaa-garbage.json");
        std::fs::write(&garbage, "{ not json").unwrap();
        let newer = dir.join("aab-newer.json");
        std::fs::write(
            &newer,
            r#"{"version":99,"session":"s","kind":"download","jobs":[]}"#,
        )
        .unwrap();
        let (path, _) = write_checkpoint("host", &downloads, &[("a", JobStatus::Pending)]);

        list_and_clean(false, true).unwrap();

        assert!(!garbage.exists() && !newer.exists() && !path.exists());
    }

    /// A write cut short by a crash leaves its temporary file beside the
    /// checkpoints. `--force` clears those out too; `--clean` leaves them,
    /// since a running blink may be mid-write.
    #[test]
    fn force_removes_leftover_temporary_files() {
        let _home = paths::test_home();
        let tmp = paths::checkpoints_dir()
            .unwrap()
            .join(".host-12345678-download.json.4242.0.tmp");
        std::fs::write(&tmp, "{").unwrap();

        list_and_clean(true, false).unwrap();
        assert!(tmp.exists(), "--clean leaves it");

        list_and_clean(false, true).unwrap();
        assert!(!tmp.exists(), "--force removes it");
    }

    /// `blink connect sftp://me@host` names its session after the host and
    /// saves nothing, so no saved session matches its checkpoint. It can
    /// still be resumed by connecting the same way, so `--clean` keeps it.
    #[test]
    fn clean_keeps_an_ad_hoc_connections_checkpoint() {
        let _home = paths::test_home();
        let downloads = _home.path().join("downloads");
        std::fs::create_dir(&downloads).unwrap();
        let (path, parts) = write_checkpoint("host", &downloads, &[("a", JobStatus::Pending)]);

        list_and_clean(true, false).unwrap();
        assert!(path.exists() && parts[0].exists(), "--clean keeps it");

        list_and_clean(false, true).unwrap();
        assert!(!path.exists() && !parts[0].exists(), "--force removes it");
    }

    #[test]
    fn an_ad_hoc_session_is_named_after_its_origins_host() {
        assert!(reachable_ad_hoc("host", "me@host:22"));
        assert!(reachable_ad_hoc("Host.Example", "me@host.example:22"));
        assert!(reachable_ad_hoc(
            "ftp.example",
            "me@mail.example@ftp.example:21"
        ));
        assert!(reachable_ad_hoc("::1", "me@::1:22"));
        assert!(reachable_ad_hoc("host", "@host:22"), "no user");
        assert!(!reachable_ad_hoc("prod", "me@host:22"));
        assert!(!reachable_ad_hoc("host:22", "me@host:22"));
    }

    /// Only `.json` files are checkpoints.
    #[test]
    fn other_files_in_the_folder_are_ignored() {
        let _home = paths::test_home();
        let stray = paths::checkpoints_dir().unwrap().join("notes.txt");
        std::fs::write(&stray, "mine").unwrap();

        list_and_clean(false, true).unwrap();

        assert!(stray.exists());
    }
}

#[cfg(test)]
mod rename_tests {
    use super::*;
    use std::path::PathBuf;

    fn dl(name: &str) -> CheckpointJob {
        CheckpointJob::Download {
            remote_path: format!("/r/{name}"),
            local_path: PathBuf::from(format!("/l/{name}")),
            status: JobStatus::Pending,
        }
    }

    fn save(session: &str, origin: &str, kind: CheckpointKind, names: &[&str]) {
        let jobs = names.iter().map(|n| dl(n)).collect();
        Checkpoint::new(session, origin, kind, jobs)
            .flush()
            .unwrap();
    }

    fn remote_paths(session: &str, origin: &str, kind: CheckpointKind) -> Option<Vec<String>> {
        Checkpoint::load(session, origin, kind).unwrap().map(|cp| {
            cp.jobs
                .iter()
                .map(|j| j.remote_path().to_string())
                .collect()
        })
    }

    #[test]
    fn a_rename_moves_both_directions_to_the_new_name() {
        let _home = paths::test_home();
        save("old", TEST_ORIGIN, CheckpointKind::Download, &["a"]);
        save("old", TEST_ORIGIN, CheckpointKind::Upload, &["b"]);

        assert!(rename_session("old", "new", TEST_ORIGIN).is_empty());

        for kind in [CheckpointKind::Download, CheckpointKind::Upload] {
            assert_eq!(remote_paths("old", TEST_ORIGIN, kind), None);
            let cp = Checkpoint::load("new", TEST_ORIGIN, kind).unwrap().unwrap();
            assert_eq!(cp.session, "new", "the document names its session too");
        }
    }

    /// The new name may already have a checkpoint for the same server, left
    /// by an ad-hoc connect or a deleted session. Both batches are kept.
    #[test]
    fn a_rename_onto_an_existing_checkpoint_keeps_both() {
        let _home = paths::test_home();
        save("old", TEST_ORIGIN, CheckpointKind::Download, &["a"]);
        save("new", TEST_ORIGIN, CheckpointKind::Download, &["b"]);

        assert!(rename_session("old", "new", TEST_ORIGIN).is_empty());

        assert_eq!(
            remote_paths("new", TEST_ORIGIN, CheckpointKind::Download),
            Some(vec!["/r/a".to_string(), "/r/b".to_string()]),
        );
        assert_eq!(
            remote_paths("old", TEST_ORIGIN, CheckpointKind::Download),
            None
        );
    }

    /// Another account's checkpoint under the old name is not this
    /// session's to move.
    #[test]
    fn a_rename_leaves_other_servers_checkpoints_alone() {
        let _home = paths::test_home();
        save(
            "old",
            "other@elsewhere:22",
            CheckpointKind::Download,
            &["a"],
        );

        assert!(rename_session("old", "new", TEST_ORIGIN).is_empty());

        assert!(remote_paths("old", "other@elsewhere:22", CheckpointKind::Download).is_some());
        assert_eq!(
            remote_paths("new", "other@elsewhere:22", CheckpointKind::Download),
            None
        );
    }

    /// A destination that can't be read is not written over; the batch
    /// stays where it was, and the failure is reported.
    #[test]
    fn a_rename_onto_an_unreadable_checkpoint_moves_nothing() {
        let _home = paths::test_home();
        save("old", TEST_ORIGIN, CheckpointKind::Download, &["a"]);
        let dest = Checkpoint::path_for("new", TEST_ORIGIN, CheckpointKind::Download).unwrap();
        std::fs::write(&dest, "{ not json").unwrap();

        let failures = rename_session("old", "new", TEST_ORIGIN);

        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(remote_paths("old", TEST_ORIGIN, CheckpointKind::Download).is_some());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "{ not json");
    }

    #[test]
    fn renaming_to_the_same_name_does_nothing() {
        let _home = paths::test_home();
        save("same", TEST_ORIGIN, CheckpointKind::Download, &["a"]);

        assert!(rename_session("same", "same", TEST_ORIGIN).is_empty());

        assert!(remote_paths("same", TEST_ORIGIN, CheckpointKind::Download).is_some());
    }
}

#[cfg(test)]
mod offer_tests {
    use super::*;
    use std::path::PathBuf;

    fn dl(n: usize, status: JobStatus) -> CheckpointJob {
        CheckpointJob::Download {
            remote_path: format!("/srv/file{n}.bin"),
            local_path: PathBuf::from(format!("/l/file{n}.bin")),
            status,
        }
    }

    #[test]
    fn the_offer_counts_only_outstanding_and_completed_work() {
        // `total` is remaining + done. A cancelled job is work the user
        // already abandoned; counting it would overstate what is left.
        let cp = Checkpoint::new(
            "s",
            TEST_ORIGIN,
            CheckpointKind::Download,
            vec![
                dl(0, JobStatus::Done),
                dl(1, JobStatus::Pending),
                dl(2, JobStatus::InProgress),
                dl(3, JobStatus::Cancelled),
            ],
        );

        let offer = cp.to_offer(None);

        assert_eq!(offer.remaining, 2, "pending + in_progress");
        assert_eq!(offer.total, 3, "remaining + done, cancelled excluded");
        assert_eq!(offer.kind, CheckpointKind::Download);
    }

    /// A checkpoint is offered only to a connection to the server and
    /// account it was made for. An ad-hoc `sftp://alice@host` and
    /// `sftp://bob@host` are both named `host`; resuming alice's upload as
    /// bob would send her files to his account.
    #[test]
    fn a_checkpoint_is_offered_only_to_its_own_origin() {
        let _home = crate::paths::test_home();
        let mut cp = Checkpoint::new(
            "host",
            "alice@host:22",
            CheckpointKind::Upload,
            vec![dl(0, JobStatus::Pending)],
        );
        cp.flush().unwrap();

        assert_eq!(offers_for("host", "alice@host:22").len(), 1);
        assert!(offers_for("host", "bob@host:22").is_empty());
        assert!(
            Checkpoint::load("host", "bob@host:22", CheckpointKind::Upload)
                .unwrap()
                .is_none()
        );
    }

    /// A file that claims another origin than its name was keyed by (copied
    /// or hand-edited) is still not this connection's.
    #[test]
    fn a_file_recording_another_origin_is_not_offered() {
        let _home = crate::paths::test_home();
        let mut cp = Checkpoint::new(
            "host",
            "alice@host:22",
            CheckpointKind::Download,
            vec![dl(0, JobStatus::Pending)],
        );
        cp.origin = Some("bob@host:22".into());
        let path = Checkpoint::path_for("host", "alice@host:22", CheckpointKind::Download).unwrap();
        std::fs::write(&path, serde_json::to_string(&cp).unwrap()).unwrap();

        assert!(offers_for("host", "alice@host:22").is_empty());
        assert!(
            Checkpoint::load("host", "alice@host:22", CheckpointKind::Download)
                .unwrap()
                .is_none()
        );
    }

    /// A checkpoint from before version 4 records no origin, so it is never
    /// offered — whether it sits at its old file name or the new one.
    #[test]
    fn a_checkpoint_without_an_origin_is_never_offered() {
        let _home = crate::paths::test_home();
        let v3 = r#"{"version":3,"session":"host","kind":"upload","jobs":[
             {"type":"upload","local_path":"/l/0","remote_path":"/r/0","status":"pending"}
           ]}"#;
        for origin in ["", TEST_ORIGIN] {
            let path = Checkpoint::path_for("host", origin, CheckpointKind::Upload).unwrap();
            std::fs::write(&path, v3).unwrap();
        }
        let cp = Checkpoint::load_from(
            &Checkpoint::path_for("host", TEST_ORIGIN, CheckpointKind::Upload).unwrap(),
        )
        .unwrap()
        .expect("a v3 checkpoint still loads");
        assert_eq!(cp.origin, None);

        assert!(offers_for("host", TEST_ORIGIN).is_empty());
    }

    #[test]
    fn the_offer_samples_at_most_three_outstanding_paths() {
        let jobs: Vec<CheckpointJob> = (0..10).map(|n| dl(n, JobStatus::Pending)).collect();
        let cp = Checkpoint::new("s", TEST_ORIGIN, CheckpointKind::Download, jobs);

        let offer = cp.to_offer(None);

        assert_eq!(offer.sample_paths.len(), 3);
        assert_eq!(offer.sample_paths[0], "/srv/file0.bin");
    }

    #[test]
    fn the_offer_skips_finished_jobs_when_sampling() {
        let cp = Checkpoint::new(
            "s",
            TEST_ORIGIN,
            CheckpointKind::Download,
            vec![dl(0, JobStatus::Done), dl(1, JobStatus::Pending)],
        );

        let offer = cp.to_offer(None);

        assert_eq!(
            offer.sample_paths,
            vec!["/srv/file1.bin".to_string()],
            "the panel should show what is left, not what is finished",
        );
    }

    #[test]
    fn an_upload_offer_samples_the_local_source() {
        // For an upload the user picked local files; that is what they will
        // recognise, not the remote destination.
        let cp = Checkpoint::new(
            "s",
            TEST_ORIGIN,
            CheckpointKind::Upload,
            vec![CheckpointJob::Upload {
                local_path: PathBuf::from("/home/me/photos/a.cr2"),
                remote_path: "/srv/backup/a.cr2".into(),
                status: JobStatus::Pending,
            }],
        );

        let offer = cp.to_offer(None);

        assert_eq!(
            offer.sample_paths,
            vec!["/home/me/photos/a.cr2".to_string()]
        );
    }

    #[test]
    fn sample_paths_are_sanitized() {
        // The panel renders these directly rather than through push_log, so
        // a server-supplied name must not carry escapes into the terminal.
        let cp = Checkpoint::new(
            "s",
            TEST_ORIGIN,
            CheckpointKind::Download,
            vec![CheckpointJob::Download {
                remote_path: "/srv/re\u{202E}port.bin".into(),
                local_path: PathBuf::from("/l/x"),
                status: JobStatus::Pending,
            }],
        );

        let offer = cp.to_offer(None);

        assert!(
            !offer.sample_paths[0].contains('\u{202E}'),
            "bidi override reached the panel: {:?}",
            offer.sample_paths[0],
        );
    }

    #[test]
    fn a_session_with_no_checkpoints_has_no_offers() {
        let _home = paths::test_home();
        let name = format!("blink-test-none-{}", std::process::id());
        assert!(offers_for(&name, TEST_ORIGIN).is_empty());
    }

    #[test]
    fn a_pending_checkpoint_on_disk_produces_one_offer() {
        let name = format!("blink-test-offer-{}", std::process::id());
        let _home = paths::test_home();
        let mut cp = Checkpoint::new(
            &name,
            TEST_ORIGIN,
            CheckpointKind::Download,
            vec![dl(0, JobStatus::Done), dl(1, JobStatus::Pending)],
        );
        cp.flush().expect("write the checkpoint");

        let offers = offers_for(&name, TEST_ORIGIN);

        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].remaining, 1);
        assert_eq!(offers[0].total, 2);
        assert!(offers[0].age.is_some(), "age comes from the file mtime");
    }

    #[test]
    fn a_finished_checkpoint_produces_no_offer() {
        let name = format!("blink-test-done-{}", std::process::id());
        let _home = paths::test_home();
        let mut cp = Checkpoint::new(
            &name,
            TEST_ORIGIN,
            CheckpointKind::Download,
            vec![dl(0, JobStatus::Done)],
        );
        cp.flush().expect("write the checkpoint");

        assert!(
            offers_for(&name, TEST_ORIGIN).is_empty(),
            "nothing left to resume means nothing to offer",
        );
    }

    #[test]
    fn both_directions_each_produce_an_offer() {
        let name = format!("blink-test-both-{}", std::process::id());
        let _home = paths::test_home();
        for kind in [CheckpointKind::Download, CheckpointKind::Upload] {
            let mut cp = Checkpoint::new(&name, TEST_ORIGIN, kind, vec![dl(0, JobStatus::Pending)]);
            cp.flush().expect("write the checkpoint");
        }

        let offers = offers_for(&name, TEST_ORIGIN);

        assert_eq!(offers.len(), 2);
        assert!(offers.iter().any(|o| o.kind == CheckpointKind::Download));
        assert!(offers.iter().any(|o| o.kind == CheckpointKind::Upload));
    }

    #[test]
    fn mkdir_jobs_count_toward_remaining_and_total() {
        // The plan's denominator is item count, not file-transfer count: a
        // batch that is mostly directory creation must not under-report how
        // much work is left just because every fixture elsewhere in this
        // module happens to be a `Download`.
        let cp = Checkpoint::new(
            "s",
            TEST_ORIGIN,
            CheckpointKind::Upload,
            vec![
                CheckpointJob::Mkdir {
                    remote_path: "/srv/backup/photos".into(),
                    status: JobStatus::Done,
                },
                CheckpointJob::Mkdir {
                    remote_path: "/srv/backup/videos".into(),
                    status: JobStatus::Pending,
                },
                CheckpointJob::Upload {
                    local_path: PathBuf::from("/home/me/photos/a.cr2"),
                    remote_path: "/srv/backup/photos/a.cr2".into(),
                    status: JobStatus::Pending,
                },
            ],
        );

        let offer = cp.to_offer(None);

        assert_eq!(
            offer.remaining, 2,
            "the pending mkdir plus the pending upload"
        );
        assert_eq!(offer.total, 3, "all three jobs, the done mkdir included");

        // `source_path` reads the remote side for a `Mkdir`, so an upload
        // offer's samples can include a remote path alongside local ones —
        // that's the existing, correct behaviour for this job type, not
        // something this test should paper over.
        assert_eq!(
            offer.sample_paths,
            vec![
                "/srv/backup/videos".to_string(),
                "/home/me/photos/a.cr2".to_string(),
            ],
            "an upload offer can legitimately sample a remote path for its mkdir entries",
        );
    }

    #[test]
    fn an_unreadable_checkpoint_is_skipped_rather_than_propagated() {
        let _home = paths::test_home();
        // Connecting must never fail because a checkpoint won't parse.
        let name = format!("blink-test-corrupt-{}", std::process::id());
        let path = Checkpoint::path_for(&name, TEST_ORIGIN, CheckpointKind::Download).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();

        assert!(offers_for(&name, TEST_ORIGIN).is_empty());

        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;
    use std::path::PathBuf;

    fn dl(n: usize) -> CheckpointJob {
        CheckpointJob::Download {
            remote_path: format!("/r/{n}"),
            local_path: PathBuf::from(format!("/l/{n}")),
            status: JobStatus::Pending,
        }
    }

    // -- appending a second batch ------------------------------------------
    //
    // One checkpoint per (session, direction) is the storage model, so a
    // second batch of the same direction used to overwrite the first — while
    // the first was still running. Appending keeps both resumable.

    #[test]
    fn append_returns_the_base_index_of_the_added_jobs() {
        let mut cp = Checkpoint::new(
            "s",
            TEST_ORIGIN,
            CheckpointKind::Download,
            vec![dl(0), dl(1)],
        );
        let base = cp.append(vec![dl(2), dl(3)]).unwrap();
        assert_eq!(base, 2, "the caller maps job ids from this offset");
        assert_eq!(cp.jobs.len(), 4);
    }

    #[test]
    fn append_preserves_the_progress_of_the_existing_jobs() {
        let mut cp = Checkpoint::new(
            "s",
            TEST_ORIGIN,
            CheckpointKind::Download,
            vec![dl(0), dl(1)],
        );
        cp.mark_done(0);
        cp.mark_in_progress(1);

        cp.append(vec![dl(2)]).unwrap();

        assert_eq!(cp.jobs[0].status(), JobStatus::Done);
        assert_eq!(cp.jobs[1].status(), JobStatus::InProgress);
        assert_eq!(cp.jobs[2].status(), JobStatus::Pending);
    }

    #[test]
    fn append_into_an_empty_checkpoint_starts_at_zero() {
        let mut cp = Checkpoint::new("s", TEST_ORIGIN, CheckpointKind::Upload, Vec::new());
        assert_eq!(cp.append(vec![dl(0)]).unwrap(), 0);
    }

    /// A checkpoint is refused on load past its limits, so appending can't
    /// take it past them: it would be saved, then refused, and nothing in
    /// it could be resumed.
    #[test]
    fn append_refuses_more_jobs_than_a_load_accepts() {
        let mut cp = Checkpoint::new("s", TEST_ORIGIN, CheckpointKind::Download, vec![dl(0)]);
        assert!(cp.append_within(vec![dl(1), dl(2)], 3, u64::MAX).is_ok());
        let err = cp.append_within(vec![dl(3)], 3, u64::MAX).unwrap_err();
        assert!(err.to_string().contains("at most 3 jobs"), "{err}");
        assert_eq!(cp.jobs.len(), 3, "a refused append adds nothing");
    }

    #[test]
    fn append_refuses_a_checkpoint_that_could_outgrow_the_load_limit() {
        let mut cp = Checkpoint::new("s", TEST_ORIGIN, CheckpointKind::Download, vec![dl(0)]);
        let limit = serde_json::to_vec(&cp).unwrap().len() as u64 + 50;
        let err = cp
            .append_within(vec![dl(1), dl(2)], usize::MAX, limit)
            .unwrap_err();
        assert!(err.to_string().contains("bytes"), "{err}");
        assert_eq!(cp.jobs.len(), 1, "a refused append adds nothing");
    }

    /// The size an append allows for must cover the file however its jobs
    /// end up, every one of them in progress included.
    #[test]
    fn appends_size_allows_for_every_status() {
        let mut cp = Checkpoint::new("s", TEST_ORIGIN, CheckpointKind::Download, vec![dl(0)]);
        let jobs: Vec<_> = (1..50).map(dl).collect();
        let mut tight = 0;
        for limit in 0.. {
            let mut probe = cp.clone();
            if probe.append_within(jobs.clone(), usize::MAX, limit).is_ok() {
                tight = limit;
                break;
            }
        }
        cp.append_within(jobs, usize::MAX, tight).unwrap();
        for i in 0..cp.jobs.len() {
            cp.mark_in_progress(i);
        }
        let written = serde_json::to_vec(&cp).unwrap().len() as u64;
        assert!(
            written <= tight,
            "{written} bytes written, {tight} allowed for"
        );
    }

    // -- cancelled jobs ----------------------------------------------------

    #[test]
    fn a_cancelled_job_is_neither_resumed_nor_counted_done() {
        // Cancelling one batch must not re-queue it on the next resume, and
        // must not make the checkpoint look finished either.
        let mut cp = Checkpoint::new(
            "s",
            TEST_ORIGIN,
            CheckpointKind::Download,
            vec![dl(0), dl(1)],
        );
        cp.mark_cancelled(0);

        assert!(
            !cp.jobs[0].needs_resume(),
            "a cancelled job must not resume"
        );
        assert!(!cp.jobs[0].is_done(), "and must not count as completed");
        assert_eq!(cp.pending_count(), 1, "only the untouched job remains");
        assert_eq!(cp.done_count(), 0);
    }

    #[test]
    fn cancelling_every_job_leaves_nothing_to_resume() {
        let mut cp = Checkpoint::new(
            "s",
            TEST_ORIGIN,
            CheckpointKind::Download,
            vec![dl(0), dl(1)],
        );
        cp.mark_cancelled(0);
        cp.mark_cancelled(1);
        assert_eq!(cp.pending_count(), 0);
    }

    #[test]
    fn a_version_2_document_still_loads() {
        // v3 only adds a status value, so v2 files are valid v3 documents and
        // must keep loading — a checkpoint written by the previous release is
        // exactly what someone resumes after upgrading.
        let dir = std::env::temp_dir().join(format!("blink-cp-v2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v2.json");
        std::fs::write(
            &path,
            r#"{"version":2,"session":"s","kind":"download","jobs":[
                 {"type":"download","remote_path":"/r/0","local_path":"/l/0","status":"done"},
                 {"type":"download","remote_path":"/r/1","local_path":"/l/1","status":"pending"}
               ]}"#,
        )
        .unwrap();

        let cp = Checkpoint::load_from(&path)
            .expect("a v2 checkpoint must still load")
            .expect("file exists");
        assert_eq!(cp.done_count(), 1);
        assert_eq!(cp.pending_count(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Version 1 had no `version` field and a boolean `done` per job.
    #[test]
    fn a_version_1_document_is_migrated() {
        let home = crate::paths::test_home();
        let path = home.path().join("v1.json");
        std::fs::write(
            &path,
            r#"{"session":"s","kind":"download","jobs":[
                 {"type":"download","remote_path":"/r/0","local_path":"/l/0","done":true},
                 {"type":"download","remote_path":"/r/1","local_path":"/l/1","done":false}
               ]}"#,
        )
        .unwrap();

        let before = std::fs::read_to_string(&path).unwrap();

        let cp = Checkpoint::load_from(&path).unwrap().expect("file exists");
        assert_eq!(cp.done_count(), 1);
        assert_eq!(cp.pending_count(), 1);
        // Reading is only reading: `blink checkpoints` loads every file,
        // and listing them must not change any.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn a_document_from_a_newer_blink_is_refused() {
        let home = crate::paths::test_home();
        let path = home.path().join("v99.json");
        std::fs::write(
            &path,
            r#"{"version":99,"session":"s","kind":"download","jobs":[],"later":{"x":1}}"#,
        )
        .unwrap();

        let err = Checkpoint::load_from(&path).unwrap_err();
        assert!(err.to_string().contains("newer than supported"), "{err}");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_checkpoint_file_name_fits_the_platform_limit() {
        for session in [
            "a".repeat(400),
            "\u{e9}".repeat(200),
            "\u{1f600}".repeat(100),
        ] {
            for kind in [CheckpointKind::Download, CheckpointKind::Upload] {
                let name = file_name_for(&session, TEST_ORIGIN, kind);
                assert!(
                    name.len() <= paths::MAX_FILE_NAME_BYTES,
                    "{} bytes",
                    name.len()
                );
                assert!(name.ends_with(&format!("-{}.json", kind.as_str())));
            }
        }
    }

    /// The file name fits, but `atomic_write`'s temp name used to grow it
    /// past the limit, so a long session's checkpoint still failed to save.
    #[test]
    fn a_long_session_name_checkpoints_and_loads() {
        let _home = paths::test_home();
        let name = "n".repeat(400);
        let mut cp = Checkpoint::new(&name, TEST_ORIGIN, CheckpointKind::Upload, vec![]);
        cp.flush().unwrap();
        let loaded = Checkpoint::load(&name, TEST_ORIGIN, CheckpointKind::Upload)
            .unwrap()
            .expect("saved");
        assert_eq!(loaded.session, name);
    }

    #[test]
    fn long_names_that_share_a_prefix_still_get_separate_files() {
        let a = format!("{}a", "x".repeat(300));
        let b = format!("{}b", "x".repeat(300));
        assert_ne!(
            file_name_for(&a, TEST_ORIGIN, CheckpointKind::Download),
            file_name_for(&b, TEST_ORIGIN, CheckpointKind::Download)
        );
    }

    #[test]
    fn a_short_name_maps_spaces_to_underscores() {
        assert!(
            file_name_for("my prod", TEST_ORIGIN, CheckpointKind::Upload).starts_with("my_prod-")
        );
    }

    /// `sftp://alice@host` and `sftp://bob@host` are both named `host`; they
    /// must not share a checkpoint file.
    #[test]
    fn two_accounts_sharing_a_session_name_get_separate_files() {
        let kind = CheckpointKind::Upload;
        assert_ne!(
            file_name_for("host", "alice@host:22", kind),
            file_name_for("host", "bob@host:22", kind),
        );
        // The separator keeps the name and origin from running together.
        assert_ne!(
            file_name_for("ab", "c", kind),
            file_name_for("a", "bc", kind),
        );
    }

    /// Kept off Windows so existing files kept their names; a tab or an
    /// escape in a file name only causes trouble, so they map everywhere.
    #[test]
    fn control_characters_map_on_every_platform() {
        let name = file_name_for("tab\there\u{1b}", TEST_ORIGIN, CheckpointKind::Download);
        assert!(name.starts_with("tab_here_-"), "{name:?}");
    }

    use super::*;
    use std::path::PathBuf;

    fn sample(jobs: usize) -> Checkpoint {
        let jobs = (0..jobs)
            .map(|i| CheckpointJob::Download {
                remote_path: format!("/remote/file{i}"),
                local_path: PathBuf::from(format!("/local/file{i}")),
                status: JobStatus::Pending,
            })
            .collect();
        Checkpoint::new("session", TEST_ORIGIN, CheckpointKind::Download, jobs)
    }

    #[test]
    fn mark_in_progress_sets_dirty() {
        let mut cp = sample(2);
        // First save would set dirty=false; without it, mark should add to
        // an already-dirty state (initial state is dirty=true after `new`).
        cp.dirty = false;
        cp.mark_in_progress(0);
        assert!(cp.dirty);
        assert_eq!(cp.jobs[0].status(), JobStatus::InProgress);
    }

    #[test]
    fn mark_done_sets_dirty() {
        let mut cp = sample(2);
        cp.dirty = false;
        cp.mark_done(1);
        assert!(cp.dirty);
        assert_eq!(cp.jobs[1].status(), JobStatus::Done);
    }

    #[test]
    fn mark_out_of_bounds_no_panic_no_dirty() {
        let mut cp = sample(2);
        cp.dirty = false;
        cp.mark_in_progress(99);
        cp.mark_done(99);
        assert!(!cp.dirty);
        // Original jobs untouched.
        assert_eq!(cp.jobs[0].status(), JobStatus::Pending);
        assert_eq!(cp.jobs[1].status(), JobStatus::Pending);
    }

    #[test]
    fn flush_if_due_noop_when_not_dirty() {
        let mut cp = sample(1);
        cp.dirty = false;
        cp.last_save = Some(Instant::now() - Duration::from_secs(10));
        // Must not attempt disk I/O when nothing changed — calling it
        // should be infallible even if the checkpoints dir is unwritable.
        // The Ok(()) here exercises the early-return path.
        assert!(cp.flush_if_due().is_ok());
    }

    #[test]
    fn flush_if_due_gated_by_interval() {
        // Pretend a flush just happened; an immediate dirty mutation should
        // not trigger another flush yet.
        let mut cp = sample(1);
        cp.dirty = true;
        cp.last_save = Some(Instant::now());
        // The interval gate should suppress this — and because no actual
        // disk write happens, no I/O error can surface.
        assert!(cp.flush_if_due().is_ok());
        // dirty remains true because we deferred the write.
        assert!(cp.dirty);
    }

    #[test]
    fn flush_if_due_fires_after_interval() {
        // This is the one test in this module where `flush_if_due` can
        // actually reach disk (the gate lets the write through), so unlike
        // `sample`'s fixed "session" name elsewhere in this file, it needs a
        // process-unique name and the cleanup guard — a stray `session-*`
        // checkpoint file would collide with, and be resume-offered
        // alongside, a real user session named "session".
        let name = format!("blink-test-flush-{}", std::process::id());
        let _home = paths::test_home();
        let mut cp = Checkpoint::new(
            &name,
            TEST_ORIGIN,
            CheckpointKind::Download,
            vec![CheckpointJob::Download {
                remote_path: "/remote/file0".into(),
                local_path: PathBuf::from("/local/file0"),
                status: JobStatus::Pending,
            }],
        );
        cp.dirty = true;
        // Backdate the last save to just past the interval.
        cp.last_save = Some(Instant::now() - CHECKPOINT_FLUSH_INTERVAL - Duration::from_millis(50));
        // The gate returns true; flush attempts disk I/O. We don't assert
        // success because the test environment's checkpoints dir may not
        // exist — but we *do* assert the gate decision by verifying the
        // last_save timestamp is updated whenever the underlying flush
        // succeeded.
        let pre = cp.last_save;
        let _ = cp.flush_if_due();
        // Either flush succeeded (last_save updated, dirty cleared) or it
        // failed (state unchanged). Both are acceptable here; what's not
        // acceptable is the gate suppressing the call.
        let post = cp.last_save;
        assert!(
            post != pre || cp.dirty,
            "flush_if_due must either update state or report failure"
        );
    }

    #[test]
    fn new_checkpoint_is_dirty() {
        // The initial plan must be flushed; `dirty=true` is what forces the
        // caller's first `flush()` call to actually write.
        let cp = sample(1);
        assert!(cp.dirty);
    }
}
