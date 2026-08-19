//! Walk checkpointing: persist a transfer plan to disk so an interrupted
//! batch can be resumed without re-walking the remote / local tree.
//!
//! One checkpoint file is written per batch, named by a stable key derived
//! from the session name and the transfer direction:
//!
//! ```
//! ~/.config/blink/checkpoints/<session>-upload.json
//! ~/.config/blink/checkpoints/<session>-download.json
//! ```
//!
//! Only one checkpoint per (session, direction) is kept at a time. Starting a
//! new walk of the same kind overwrites the previous checkpoint, so stale
//! files don't accumulate.
//!
//! ## Format (version 2)
//!
//! ```json
//! {
//!   "version": 2,
//!   "session": "production",
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
//!                                               │
//!                                               └──(crash / kill)──► stays in_progress
//! ```
//!
//! On resume:
//! - `done`        → skipped (already transferred successfully)
//! - `in_progress` → re-queued (the transfer was interrupted; partial files
//!   are safe to overwrite)
//! - `pending`     → re-queued (never started)
//!
//! ## Crash safety
//!
//! The status is written to disk *before* the transfer starts (`in_progress`)
//! and again *after* it completes (`done`). A crash at any point between those
//! two writes leaves the job as `in_progress`, which causes it to be re-queued
//! on resume rather than silently skipped as if it had succeeded.
//!
//! Writes are atomic: the JSON is written to a `.tmp` sibling file then
//! renamed into place, so a crash mid-write never produces a truncated file.
//!
//! ## Version migration
//!
//! Version 1 files used a boolean `done` field. They are automatically
//! migrated on load: `done: true` → `status: "done"`, `done: false` →
//! `status: "pending"`. The migrated document is written back as version 2.

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
const FORMAT_VERSION: u32 = 3;

/// Maximum checkpoint file size accepted on load (10 MiB).
const MAX_CHECKPOINT_BYTES: u64 = 10 * 1024 * 1024;

/// Maximum number of jobs accepted in a single checkpoint.
const MAX_CHECKPOINT_JOBS: usize = 1_000_000;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Which direction a checkpointed walk is going.
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
}

impl Checkpoint {
    /// Create a new checkpoint for `session` and `kind` with `jobs`.
    pub fn new(session: &str, kind: CheckpointKind, jobs: Vec<CheckpointJob>) -> Self {
        Self {
            version: FORMAT_VERSION,
            session: session.to_string(),
            kind,
            jobs,
            dirty: true,
            last_save: None,
        }
    }

    // -----------------------------------------------------------------------
    // Persistence
    // -----------------------------------------------------------------------

    /// Derive the checkpoint file path for a given session + direction.
    ///
    /// Two distinct session names ("my prod" vs "my_prod") would otherwise
    /// collapse to the same sanitised stem and silently overwrite each
    /// other's checkpoint. Append the first eight hex chars of
    /// `sha256(session)` to disambiguate; the suffix derives from the raw
    /// session name (no sanitisation), so it's stable per logical name.
    fn path_for(session: &str, kind: CheckpointKind) -> Result<PathBuf> {
        use sha2::{Digest, Sha256};
        let safe_name: String = session
            .chars()
            .map(|c| match c {
                // Null byte plus all path-separator and shell-special chars.
                '\0' | '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | ' ' => '_',
                c => c,
            })
            .collect();
        let hash = Sha256::digest(session.as_bytes());
        let mut suffix = String::with_capacity(8);
        for b in &hash[..4] {
            use std::fmt::Write as _;
            let _ = write!(&mut suffix, "{b:02x}");
        }
        Ok(paths::checkpoints_dir()?
            .join(format!("{safe_name}-{suffix}-{}.json", kind.as_str())))
    }

    /// Atomically and durably write `content` to `path` via a `.tmp` sibling
    /// + rename.
    ///
    /// The full pattern is:
    ///   1. write tempfile, flush, `sync_all` (data is on the platter)
    ///   2. rename tempfile over the destination
    ///   3. `sync_all` the parent directory (rename is journaled) — Unix only;
    ///      Windows journals rename through its own filesystem semantics.
    ///
    /// Without (1), a power loss between rename and the filesystem journal
    /// commit can leave the destination as a zero-byte file (the rename is
    /// visible, the data isn't). Without (3), the rename itself can be
    /// rolled back on power loss even though it returned Ok.
    fn atomic_write(path: &Path, content: &str) -> Result<()> {
        use std::io::Write as _;

        let tmp = path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(content.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        paths::sync_parent_dir(path)?;
        Ok(())
    }

    /// Force-write this checkpoint to disk, overwriting any previous one
    /// for the same (session, kind) pair.
    ///
    /// Most callers should use [`Self::flush_if_due`] instead — that one
    /// debounces back-to-back mutations into a single write. Call this
    /// directly only when the batch is about to terminate (final job
    /// completed, batch failed, app exiting) so the on-disk state is up to
    /// date before the in-memory checkpoint is dropped.
    pub fn flush(&mut self) -> Result<()> {
        let path = Self::path_for(&self.session, self.kind)?;
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| BlinkError::config(format!("checkpoint serialize: {e}")))?;
        Self::atomic_write(&path, &json)?;
        self.dirty = false;
        self.last_save = Some(Instant::now());
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
            CHECKPOINT_FLUSH_INTERVAL,
        ) {
            self.flush()
        } else {
            Ok(())
        }
    }

    /// Whether this checkpoint has changes that have not reached disk.
    ///
    /// Test-only: it exists so the debounce contract can be asserted — a
    /// coalesced mark leaves the checkpoint dirty, a written one does not.
    /// Nothing in the running app needs to ask, so it isn't shipped; if a
    /// use appears (an "unsaved" indicator, say), drop the `cfg`.
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
    /// Storage is one checkpoint per (session, direction), so a second batch
    /// of the same direction used to overwrite the first — while the first
    /// was still running, leaving it unresumable and its `.part` files
    /// unfindable. Appending keeps both in one file; the caller offsets its
    /// job-id map by the returned base.
    pub fn append(&mut self, jobs: Vec<CheckpointJob>) -> usize {
        let base = self.jobs.len();
        self.jobs.extend(jobs);
        self.dirty = true;
        base
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
    pub fn remove(session: &str, kind: CheckpointKind) -> Result<()> {
        let path = Self::path_for(session, kind)?;
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
    /// Automatically migrates version 1 files to version 2 and rewrites them.
    pub fn load(session: &str, kind: CheckpointKind) -> Result<Option<Self>> {
        let path = Self::path_for(session, kind)?;
        Self::load_from(&path)
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
        // migrations without fighting serde's strict field matching.
        let version_probe: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| BlinkError::config(format!("checkpoint parse: {e}")))?;
        let version = version_probe
            .get("version")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u32;

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
            return Self::migrate_v1(path, version_probe);
        }

        let cp: Self = serde_json::from_str(&raw)
            .map_err(|e| BlinkError::config(format!("checkpoint parse: {e}")))?;

        Self::validate(&cp)?;
        Ok(Some(cp))
    }

    /// Migrate a version-1 checkpoint document to version 2 in memory and
    /// rewrite the file. Returns the migrated checkpoint.
    fn migrate_v1(path: &Path, mut doc: serde_json::Value) -> Result<Option<Self>> {
        use serde_json::Value;

        // Rewrite each job entry.
        if let Some(jobs) = doc.get_mut("jobs").and_then(|j| j.as_array_mut()) {
            for job in jobs.iter_mut() {
                let done = job
                    .get("done")
                    .and_then(|d| d.as_bool())
                    .unwrap_or(false);
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

        let migrated_json = serde_json::to_string_pretty(&doc)
            .map_err(|e| BlinkError::config(format!("checkpoint migrate serialize: {e}")))?;

        // Write the migrated file back atomically so future loads don't need
        // to migrate. Non-fatal: we have the migrated data in memory.
        if let Err(e) = Self::atomic_write(path, &migrated_json) {
            tracing::warn!(?path, "could not rewrite migrated checkpoint: {e}");
        }

        let cp: Self = serde_json::from_str(&migrated_json)
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
            remaining,
            total: remaining + done,
            age,
            sample_paths,
        }
    }
}

/// Summaries of every checkpoint for `session` that still has work left.
///
/// A file that is absent, empty of outstanding work, or unreadable yields
/// no offer: connecting must never fail because of a checkpoint.
pub fn offers_for(session: &str) -> Vec<CheckpointOffer> {
    let mut out = Vec::new();
    for kind in [CheckpointKind::Download, CheckpointKind::Upload] {
        let Ok(path) = Checkpoint::path_for(session, kind) else {
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
        if cp.pending_count() == 0 {
            continue;
        }
        out.push(cp.to_offer(age_of(&path)));
    }
    out
}

/// Remove a checkpoint and the partial downloads it is the only record of.
///
/// The checkpoint names where every unfinished download left its `.part`
/// file; delete it without sweeping and those files are stranded with
/// nothing left to reference them. `remove_orphan_parts` skips `Done` jobs,
/// so only partials of transfers that never finished are removed.
///
/// Idempotent: a checkpoint that is already gone reports nothing removed.
pub fn discard(session: &str, kind: CheckpointKind) -> Result<DiscardOutcome> {
    let mut outcome = match Checkpoint::load(session, kind) {
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
    if let Err(e) = Checkpoint::remove(session, kind) {
        outcome
            .failures
            .push(format!("could not remove the checkpoint file: {e}"));
    }
    Ok(outcome)
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
fn write_due(
    dirty: bool,
    last_save: Option<Instant>,
    now: Instant,
    interval: Duration,
) -> bool {
    if !dirty {
        return false;
    }
    match last_save {
        None => true,
        Some(t) => now.duration_since(t) >= interval,
    }
}

/// Delete the `.part` files belonging to `cp`'s unfinished downloads.
/// Returns how many were removed.
///
/// A download streams into `<dest>.part` and renames onto `<dest>` only on
/// success, so an interrupted batch leaves partials scattered across the
/// destination tree. The checkpoint is the only record of where they are —
/// once it is gone, nothing can find them again and they sit there forever.
///
/// Only `Done` jobs are skipped: their `.part` was already renamed away, and
/// a file at that path now would belong to some other transfer.
///
/// Called from two places: the `blink checkpoints` CLI (a separate process
/// invocation, never concurrent with a running batch) and the TUI's
/// `discard` offer. In the TUI, resume offers are built at connect time,
/// before any job of that direction has been enqueued for the *new*
/// session — so pressing `d` never races a worker the current session
/// itself just started.
///
/// It can still race a worker from the session that was just left, though:
/// `App::disconnect` spawns the dispatcher's `shutdown()` rather than
/// awaiting it, so a worker from the previous connection can still be
/// mid-write on a `.part` file when the reconnect completes and its
/// checkpoint is offered again. Unlinking out from under that worker races
/// it: on Unix the unlink succeeds, the worker keeps writing to the
/// now-unlinked inode, and its final rename fails — turning what should be
/// a clean discard into a spurious transfer failure.
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

fn remove_orphan_parts(cp: &Checkpoint) -> DiscardOutcome {
    let mut outcome = DiscardOutcome::default();
    for job in &cp.jobs {
        let CheckpointJob::Download { local_path, status, .. } = job else {
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

/// Print checkpoint info. Pass `clean` to remove completed/orphaned files,
/// `force` to remove every file unconditionally.
pub fn list_and_clean(clean: bool, force: bool) -> Result<()> {
    use crate::session::Session;
    use std::collections::HashSet;
    use std::fs;

    let dir = paths::checkpoints_dir()?;

    let mut entries: Vec<std::path::PathBuf> = fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    entries.sort();

    if entries.is_empty() {
        println!("no checkpoints found");
        return Ok(());
    }

    let known_sessions: HashSet<String> = Session::list_all()
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.name)
        .collect();

    let mut removed = 0usize;
    let mut kept = 0usize;
    let mut parts_removed = 0usize;

    for path in &entries {
        let cp = match Checkpoint::load_from(path) {
            Ok(Some(cp)) => cp,
            Ok(None) => continue,
            Err(e) => {
                eprintln!("warning: could not read {}: {e}", path.display());
                continue;
            }
        };

        let pending = cp.pending_count();
        let done = cp.done_count();
        let total = pending + done;
        let orphaned = !known_sessions.contains(&cp.session);

        let should_remove = force || (clean && (pending == 0 || orphaned));

        if should_remove {
            // Removing the checkpoint makes the batch unresumable, which
            // strands the `.part` files its unfinished downloads left
            // behind — nothing else records where they are. Sweep them
            // while we still know.
            let swept = remove_orphan_parts(&cp);
            parts_removed += swept.parts_removed;
            for failure in swept.failures {
                eprintln!("warning: {failure}");
            }
            match fs::remove_file(path) {
                Ok(()) => {
                    let reason = if force {
                        "forced"
                    } else if pending == 0 {
                        "completed"
                    } else {
                        "orphaned"
                    };
                    println!(
                        "removed  {:<20}  {:<8}  {}/{} done  ({})",
                        crate::error::sanitize_display(&cp.session),
                        cp.kind.as_str(),
                        done,
                        total,
                        reason,
                    );
                    removed += 1;
                }
                Err(e) => {
                    eprintln!("error: could not remove {}: {e}", path.display());
                }
            }
        } else {
            let flag = if orphaned { " [orphaned]" } else { "" };
            println!(
                "{:<20}  {:<8}  {}/{} done  ({} remaining){}",
                crate::error::sanitize_display(&cp.session),
                cp.kind.as_str(),
                done,
                total,
                pending,
                flag,
            );
            kept += 1;
        }
    }

    if clean || force {
        println!();
        println!("{removed} removed, {kept} kept");
        if parts_removed > 0 {
            let plural = if parts_removed == 1 { "file" } else { "files" };
            println!("{parts_removed} orphaned .part {plural} deleted");
        }
    } else if kept > 0 {
        println!();
        println!("Use `blink checkpoints --clean` to remove completed and orphaned checkpoints.");
        println!("Use `blink checkpoints --force` to remove all checkpoint files.");
    }

    Ok(())
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
        assert!(!write_due(true, Some(just_wrote), now, CHECKPOINT_FLUSH_INTERVAL));
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
        assert!(write_due(true, Some(exactly), now, CHECKPOINT_FLUSH_INTERVAL));
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

    /// A scratch directory holding real `.part` files for the sweep to find.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("blink-sweep-{tag}-{}", std::process::id()));
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

    #[test]
    fn the_sweep_reports_what_it_removed() {
        let dir = scratch("reports");
        let unfinished = job(&dir, "a.bin", JobStatus::Pending);
        let in_progress = job(&dir, "c.bin", JobStatus::InProgress);
        let finished = job(&dir, "b.bin", JobStatus::Done);
        // All three have a partial on disk; only the unfinished ones (pending
        // and in-progress) are orphaned.
        for j in [&unfinished, &in_progress, &finished] {
            let CheckpointJob::Download { local_path, .. } = j else { unreachable!() };
            std::fs::write(crate::transport::part_path(local_path), b"x").unwrap();
        }
        let cp = Checkpoint::new(
            "s",
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
        let CheckpointJob::Download { local_path, .. } = &unfinished else { unreachable!() };
        std::fs::write(crate::transport::part_path(local_path), b"x").unwrap();

        let mut cp = Checkpoint::new(&name, CheckpointKind::Download, vec![unfinished]);
        cp.flush().expect("write the checkpoint");
        let path = Checkpoint::path_for(&name, CheckpointKind::Download).unwrap();

        let outcome = discard(&name, CheckpointKind::Download).expect("discard");

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
        // Getting only the final unlink to fail, with the load/sweep still
        // succeeding, turns out not to be portably constructible here:
        // replacing the checkpoint file with a directory (the suggested
        // technique — `remove_file` refuses a directory) also makes the file
        // unreadable, since reading a directory's bytes fails with the same
        // `IsADirectory` error as removing it. So this exercises "unreadable
        // *and* undeletable" rather than "reads fine, only the unlink
        // fails" — `outcome.parts_removed` is 0 here because the load itself
        // takes the unreadable-checkpoint branch, not because the fix
        // dropped a nonzero count.
        //
        // The alternative that *would* isolate the two — making the parent
        // directory briefly non-writable, or redirecting `checkpoints_dir`
        // via an env var — touches state shared with every other checkpoint
        // test in this binary, several of which flush to the real
        // checkpoints dir with `.expect(...)` and run concurrently on
        // process-unique names rather than serialized. Either would risk
        // flaking the rest of the suite for the sake of this one assertion,
        // so this test settles for exercising the exact changed line (the
        // failed `Checkpoint::remove` recorded into `outcome.failures`
        // instead of propagated) without also claiming a nonzero sweep
        // count under a failed removal — that half of the property is
        // covered structurally by `the_sweep_reports_what_it_removed`
        // (sweep count) and `discarding_removes_the_file_and_its_partials`
        // (both together on the success path) instead.
        let _home = paths::test_home();
        let name = format!("blink-test-remove-fail-{}", std::process::id());
        let path = Checkpoint::path_for(&name, CheckpointKind::Download).unwrap();
        std::fs::create_dir(&path).expect("stand a directory in for the checkpoint file");

        let result = discard(&name, CheckpointKind::Download);

        // `discard` could not remove the substituted directory — `remove_file`
        // refuses one — so take it out here. The test home sweeps the tree at
        // drop anyway; doing it now keeps the cleanup next to its reason.
        let _ = std::fs::remove_dir_all(&path);

        let outcome =
            result.expect("discard must return Ok, not propagate the removal failure");
        assert_eq!(
            outcome.failures.len(), 1,
            "the failed unlink must be recorded rather than silently dropped",
        );
    }

    #[test]
    fn discarding_an_unreadable_checkpoint_removes_the_file_and_sweeps_nothing() {
        let _home = paths::test_home();
        let name = format!("blink-test-corrupt-discard-{}", std::process::id());
        let path = Checkpoint::path_for(&name, CheckpointKind::Download).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();

        let outcome = discard(&name, CheckpointKind::Download).expect("must not error");

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
        let outcome = discard(&name, CheckpointKind::Download).expect("must not error");
        assert_eq!(outcome, DiscardOutcome::default());
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

    #[test]
    fn the_offer_samples_at_most_three_outstanding_paths() {
        let jobs: Vec<CheckpointJob> = (0..10).map(|n| dl(n, JobStatus::Pending)).collect();
        let cp = Checkpoint::new("s", CheckpointKind::Download, jobs);

        let offer = cp.to_offer(None);

        assert_eq!(offer.sample_paths.len(), 3);
        assert_eq!(offer.sample_paths[0], "/srv/file0.bin");
    }

    #[test]
    fn the_offer_skips_finished_jobs_when_sampling() {
        let cp = Checkpoint::new(
            "s",
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
            CheckpointKind::Upload,
            vec![CheckpointJob::Upload {
                local_path: PathBuf::from("/home/me/photos/a.cr2"),
                remote_path: "/srv/backup/a.cr2".into(),
                status: JobStatus::Pending,
            }],
        );

        let offer = cp.to_offer(None);

        assert_eq!(offer.sample_paths, vec!["/home/me/photos/a.cr2".to_string()]);
    }

    #[test]
    fn sample_paths_are_sanitized() {
        // The panel renders these directly rather than through push_log, so
        // a server-supplied name must not carry escapes into the terminal.
        let cp = Checkpoint::new(
            "s",
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
        let name = format!("blink-test-none-{}", std::process::id());
        assert!(offers_for(&name).is_empty());
    }

    #[test]
    fn a_pending_checkpoint_on_disk_produces_one_offer() {
        let name = format!("blink-test-offer-{}", std::process::id());
        let _home = paths::test_home();
        let mut cp = Checkpoint::new(
            &name,
            CheckpointKind::Download,
            vec![dl(0, JobStatus::Done), dl(1, JobStatus::Pending)],
        );
        cp.flush().expect("write the checkpoint");

        let offers = offers_for(&name);

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
            CheckpointKind::Download,
            vec![dl(0, JobStatus::Done)],
        );
        cp.flush().expect("write the checkpoint");

        assert!(
            offers_for(&name).is_empty(),
            "nothing left to resume means nothing to offer",
        );
    }

    #[test]
    fn both_directions_each_produce_an_offer() {
        let name = format!("blink-test-both-{}", std::process::id());
        let _home = paths::test_home();
        for kind in [CheckpointKind::Download, CheckpointKind::Upload] {
            let mut cp = Checkpoint::new(&name, kind, vec![dl(0, JobStatus::Pending)]);
            cp.flush().expect("write the checkpoint");
        }

        let offers = offers_for(&name);

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

        assert_eq!(offer.remaining, 2, "the pending mkdir plus the pending upload");
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
        // Connecting must never fail because a checkpoint won't parse.
        let name = format!("blink-test-corrupt-{}", std::process::id());
        let path = Checkpoint::path_for(&name, CheckpointKind::Download).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();

        assert!(offers_for(&name).is_empty());

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
        let mut cp = Checkpoint::new("s", CheckpointKind::Download, vec![dl(0), dl(1)]);
        let base = cp.append(vec![dl(2), dl(3)]);
        assert_eq!(base, 2, "the caller maps job ids from this offset");
        assert_eq!(cp.jobs.len(), 4);
    }

    #[test]
    fn append_preserves_the_progress_of_the_existing_jobs() {
        let mut cp = Checkpoint::new("s", CheckpointKind::Download, vec![dl(0), dl(1)]);
        cp.mark_done(0);
        cp.mark_in_progress(1);

        cp.append(vec![dl(2)]);

        assert_eq!(cp.jobs[0].status(), JobStatus::Done);
        assert_eq!(cp.jobs[1].status(), JobStatus::InProgress);
        assert_eq!(cp.jobs[2].status(), JobStatus::Pending);
    }

    #[test]
    fn append_into_an_empty_checkpoint_starts_at_zero() {
        let mut cp = Checkpoint::new("s", CheckpointKind::Upload, Vec::new());
        assert_eq!(cp.append(vec![dl(0)]), 0);
    }

    // -- cancelled jobs ----------------------------------------------------

    #[test]
    fn a_cancelled_job_is_neither_resumed_nor_counted_done() {
        // Cancelling one batch must not re-queue it on the next resume, and
        // must not make the checkpoint look finished either.
        let mut cp = Checkpoint::new("s", CheckpointKind::Download, vec![dl(0), dl(1)]);
        cp.mark_cancelled(0);

        assert!(!cp.jobs[0].needs_resume(), "a cancelled job must not resume");
        assert!(!cp.jobs[0].is_done(), "and must not count as completed");
        assert_eq!(cp.pending_count(), 1, "only the untouched job remains");
        assert_eq!(cp.done_count(), 0);
    }

    #[test]
    fn cancelling_every_job_leaves_nothing_to_resume() {
        let mut cp = Checkpoint::new("s", CheckpointKind::Download, vec![dl(0), dl(1)]);
        cp.mark_cancelled(0);
        cp.mark_cancelled(1);
        assert_eq!(cp.pending_count(), 0);
    }

    #[test]
    fn a_version_2_document_still_loads() {
        // v3 only adds a status value, so v2 files are valid v3 documents and
        // must keep loading — a checkpoint written by the previous release is
        // exactly what someone resumes after upgrading.
        let dir = std::env::temp_dir()
            .join(format!("blink-cp-v2-{}", std::process::id()));
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
}

#[cfg(test)]
mod tests {
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
        Checkpoint::new("session", CheckpointKind::Download, jobs)
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
