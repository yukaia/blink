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
//!                   are safe to overwrite)
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

use serde::{Deserialize, Serialize};

use crate::error::{BlinkError, Result};
use crate::paths;

/// Current serialization format version.
///
/// Version history:
///   1 — initial format, boolean `done` field per job
///   2 — three-state `status` field: pending / in_progress / done
const FORMAT_VERSION: u32 = 2;

/// Maximum checkpoint file size accepted on load (10 MiB).
const MAX_CHECKPOINT_BYTES: u64 = 10 * 1024 * 1024;

/// Maximum number of jobs accepted in a single checkpoint.
const MAX_CHECKPOINT_JOBS: usize = 1_000_000;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Which direction a checkpointed walk is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
pub enum JobStatus {
    /// Job has not been handed to the dispatcher yet (or was re-queued after
    /// a resume).
    Pending,
    /// The dispatcher has started this job but it has not yet completed. If
    /// the process is killed in this state, the job will be re-queued on
    /// the next resume.
    InProgress,
    /// The job completed successfully. Resume skips these.
    Done,
}

impl Default for JobStatus {
    fn default() -> Self {
        Self::Pending
    }
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

    /// Returns the remote path for log messages.
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
}

impl Checkpoint {
    /// Create a new checkpoint for `session` and `kind` with `jobs`.
    pub fn new(session: &str, kind: CheckpointKind, jobs: Vec<CheckpointJob>) -> Self {
        Self {
            version: FORMAT_VERSION,
            session: session.to_string(),
            kind,
            jobs,
        }
    }

    // -----------------------------------------------------------------------
    // Persistence
    // -----------------------------------------------------------------------

    /// Derive the checkpoint file path for a given session + direction.
    fn path_for(session: &str, kind: CheckpointKind) -> Result<PathBuf> {
        let safe_name: String = session
            .chars()
            .map(|c| match c {
                // Null byte plus all path-separator and shell-special chars.
                '\0' | '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | ' ' => '_',
                c => c,
            })
            .collect();
        Ok(paths::checkpoints_dir()?.join(format!("{safe_name}-{}.json", kind.as_str())))
    }

    /// Atomically write `content` to `path` via a `.tmp` sibling + rename.
    ///
    /// Using rename instead of overwriting in-place means a crash mid-write
    /// never produces a truncated file: the old checkpoint stays intact until
    /// the new one is fully flushed.
    fn atomic_write(path: &Path, content: &str) -> Result<()> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Write this checkpoint to disk, overwriting any previous one for the
    /// same (session, kind) pair.
    pub fn save(&self) -> Result<()> {
        let path = Self::path_for(&self.session, self.kind)?;
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| BlinkError::config(format!("checkpoint serialize: {e}")))?;
        Self::atomic_write(&path, &json)
    }

    /// Mark a job as `in_progress` and flush to disk.
    ///
    /// Called from the `TransferEvent::Started` handler, *before* the
    /// transfer does any I/O. This ensures that a crash during the transfer
    /// leaves the job in a state that triggers re-queue on resume rather than
    /// being silently skipped.
    pub fn mark_in_progress_and_save(&mut self, job_index: usize) -> Result<()> {
        if let Some(j) = self.jobs.get_mut(job_index) {
            j.mark_in_progress();
        }
        self.save()
    }

    /// Mark a job as `done` and flush to disk.
    ///
    /// Called from the `TransferEvent::Complete` handler after a successful
    /// transfer. Jobs that reach this state are skipped on resume.
    pub fn mark_done_and_save(&mut self, job_index: usize) -> Result<()> {
        if let Some(j) = self.jobs.get_mut(job_index) {
            j.mark_done();
        }
        self.save()
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
                if let Value::Object(ref mut map) = job {
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
                        cp.session,
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
                cp.session,
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
    } else if kept > 0 {
        println!();
        println!("Use `blink checkpoints --clean` to remove completed and orphaned checkpoints.");
        println!("Use `blink checkpoints --force` to remove all checkpoint files.");
    }

    Ok(())
}
