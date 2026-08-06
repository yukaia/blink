//! Glue between the recursive-walk planner, the dispatcher, and the
//! checkpoint file on disk.
//!
//! These three methods own the checkpoint state on `App`:
//!
//! - [`App::dispatch_plan`] takes a finalised `Vec<PlannedJob>`, writes a
//!   fresh checkpoint to disk *before* enqueuing anything, then hands
//!   jobs to the dispatcher and records the `job_id → cp_idx` mapping
//!   so the per-job event handler in `events.rs` knows which plan
//!   entry each dispatcher event refers to.
//! - [`App::resume_walk`] loads a previously persisted checkpoint, drops
//!   the done entries, and re-queues the remainder through
//!   `dispatch_plan`.
//! - [`App::discard_active_checkpoint`] tears down the in-memory state
//!   and removes the file (called on whole-batch cancel and on
//!   disconnect after a failure).
//!
//! Pulled out of `mod.rs` so the checkpoint side of the app reads as a
//! cohesive unit instead of being threaded through the lifecycle code.
//! The per-event mutation (mark_in_progress, mark_done, flush) lives in
//! `events.rs` — it's tightly coupled with the dispatcher's
//! `TransferEvent` stream, not with the modal flow that triggers a
//! batch.

use crate::checkpoint::{Checkpoint, CheckpointJob, CheckpointKind, JobStatus};
use crate::transfer::Direction;
use crate::tui::plan::PlannedJob;

use super::{App, LogLevel};

impl App {
    /// Tear down the active checkpoint: remove the in-memory state and
    /// delete the file on disk.
    ///
    /// Called when a whole batch is cancelled (user pressed `C` and
    /// confirmed) or when a transfer fails in a way that makes the batch
    /// unresumable. Soft-failures (e.g. the file was already removed) are
    /// logged at `warn` and do not abort other work.
    pub(super) fn discard_active_checkpoint(&mut self) {
        if let Some(cp) = self.active_checkpoint.take() {
            self.checkpoint_job_map.clear();
            if let Err(e) = Checkpoint::remove(&cp.session, cp.kind) {
                self.push_log(
                    LogLevel::Warn,
                    format!("could not remove checkpoint file: {e}"),
                );
            }
        }
    }

    /// Convert a [`PlannedJob`] sequence into queued transfer jobs.
    /// Mkdirs always lead so any file under them lands in an existing
    /// directory; file jobs follow.
    pub(super) fn dispatch_plan(&mut self, plan: Vec<PlannedJob>, kind: Direction) {
        let Some(manager) = self.transfer_manager.clone() else {
            return;
        };

        // Allocate a batch id for any plan with more than one job, so `C`
        // can cancel the whole thing as a unit — including the mkdir that
        // precedes a single upload. A one-job plan gets the no-batch path
        // because the single-job cancel (`c`) already covers it and a batch
        // id would just be noise.
        //
        // (This used to read `file_count > 1 || plan.len() > 1` with a
        // comment claiming mkdir-only plans were excluded. Since
        // `plan.len() >= file_count`, the first clause could never decide
        // anything and mkdir-only plans were batched regardless — the
        // comment described an intent the code never had.)
        let batch_id = if plan.len() > 1 {
            Some(manager.allocate_batch_id())
        } else {
            None
        };

        // ---- checkpoint: write before first enqueue so the file exists
        // even if the app is killed on the first transfer. ---------------
        let ck_kind = match kind {
            Direction::Upload => CheckpointKind::Upload,
            Direction::Download => CheckpointKind::Download,
            Direction::CreateDir => unreachable!(),
        };
        let session_name = self
            .current_session
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "default".to_string());

        let ck_jobs: Vec<CheckpointJob> = plan
            .iter()
            .map(|pj| match pj {
                PlannedJob::Mkdir { remote_path } => CheckpointJob::Mkdir {
                    remote_path: remote_path.clone(),
                    status: JobStatus::Pending,
                },
                PlannedJob::Download { remote_path, local_path } => CheckpointJob::Download {
                    remote_path: remote_path.clone(),
                    local_path: local_path.clone(),
                    status: JobStatus::Pending,
                },
                PlannedJob::Upload { local_path, remote_path } => CheckpointJob::Upload {
                    local_path: local_path.clone(),
                    remote_path: remote_path.clone(),
                    status: JobStatus::Pending,
                },
            })
            .collect();

        let mut checkpoint = Checkpoint::new(&session_name, ck_kind, ck_jobs);
        // First save is critical — it persists the entire plan before any I/O
        // starts. Use flush() (unconditional) rather than flush_if_due()
        // because there's no "previous save" to debounce against.
        if let Err(e) = checkpoint.flush() {
            self.push_log(
                LogLevel::Warn,
                format!("checkpoint save failed (resume unavailable): {e}"),
            );
        }
        self.active_checkpoint = Some(checkpoint);
        self.checkpoint_job_map.clear();
        // ---------------------------------------------------------------

        let mut dirs = 0usize;
        let mut files = 0usize;
        let mut dropped = 0usize;
        for (cp_idx, job) in plan.into_iter().enumerate() {
            let is_mkdir = matches!(job, PlannedJob::Mkdir { .. });
            let job_id = match (job, batch_id) {
                (PlannedJob::Mkdir { remote_path }, Some(b)) => {
                    manager.enqueue_mkdir_batched(remote_path, b)
                }
                (PlannedJob::Mkdir { remote_path }, None) => {
                    manager.enqueue_mkdir(remote_path)
                }
                (
                    PlannedJob::Download {
                        remote_path,
                        local_path,
                    },
                    Some(b),
                ) => manager.enqueue_download_batched(remote_path, local_path, b),
                (
                    PlannedJob::Download {
                        remote_path,
                        local_path,
                    },
                    None,
                ) => manager.enqueue_download(remote_path, local_path),
                (
                    PlannedJob::Upload {
                        local_path,
                        remote_path,
                    },
                    Some(b),
                ) => manager.enqueue_upload_batched(local_path, remote_path, b),
                (
                    PlannedJob::Upload {
                        local_path,
                        remote_path,
                    },
                    None,
                ) => manager.enqueue_upload(local_path, remote_path),
            };
            match job_id {
                Some(id) => {
                    self.checkpoint_job_map.insert(id, cp_idx);
                    if is_mkdir {
                        dirs += 1;
                    } else {
                        files += 1;
                    }
                }
                // Queue cap reached. The job stays `pending` in the
                // checkpoint, so a later resume picks it up — but the user
                // must be told the batch was only partially enqueued.
                None => dropped += 1,
            }
        }
        let label = match kind {
            Direction::Download => "downloads",
            Direction::Upload => "uploads",
            Direction::CreateDir => unreachable!(),
        };
        self.push_log(
            LogLevel::Info,
            format!("queued {label}: {files} file(s) + {dirs} folder(s)"),
        );
        if dropped > 0 {
            self.push_log(
                LogLevel::Warn,
                format!(
                    "transfer queue is full: {dropped} job(s) not enqueued — \
                     resume ({}) after the queue drains to pick them up",
                    match kind {
                        Direction::Download => "r",
                        _ => "R",
                    }
                ),
            );
        }
    }

    /// Dispatch a *resumed* plan: load the checkpoint for `kind`, skip jobs
    /// already marked done, and enqueue only the remaining ones.
    ///
    /// Called from the `r` keybinding in the Transfers pane (or `--resume`
    /// at startup). Logs a message if there is nothing to resume.
    pub fn resume_walk(&mut self, kind: Direction) {
        let ck_kind = match kind {
            Direction::Upload => CheckpointKind::Upload,
            Direction::Download => CheckpointKind::Download,
            Direction::CreateDir => unreachable!(),
        };
        let session_name = self
            .current_session
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "default".to_string());

        let checkpoint = match Checkpoint::load(&session_name, ck_kind) {
            Ok(Some(cp)) => cp,
            Ok(None) => {
                self.push_log(LogLevel::Warn, "no checkpoint found to resume".into());
                return;
            }
            Err(e) => {
                self.push_log(LogLevel::Error, format!("checkpoint load failed: {e}"));
                return;
            }
        };

        let pending = checkpoint.pending_count();
        let done = checkpoint.done_count();
        if pending == 0 {
            self.push_log(
                LogLevel::Info,
                "checkpoint is already complete — nothing to resume".into(),
            );
            let _ = Checkpoint::remove(&session_name, ck_kind);
            return;
        }

        self.push_log(
            LogLevel::Info,
            format!(
                "resuming {}: skipping {done} already-done, re-queuing {pending}",
                ck_kind.as_str()
            ),
        );

        // Rebuild a PlannedJob list from the undone entries only.
        let resume_plan: Vec<PlannedJob> = checkpoint
            .jobs
            .iter()
            .filter(|j| j.needs_resume())
            .map(|j| match j {
                CheckpointJob::Mkdir { remote_path, .. } => PlannedJob::Mkdir {
                    remote_path: remote_path.clone(),
                },
                CheckpointJob::Download { remote_path, local_path, .. } => PlannedJob::Download {
                    remote_path: remote_path.clone(),
                    local_path: local_path.clone(),
                },
                CheckpointJob::Upload { local_path, remote_path, .. } => PlannedJob::Upload {
                    local_path: local_path.clone(),
                    remote_path: remote_path.clone(),
                },
            })
            .collect();

        // dispatch_plan overwrites the checkpoint with a fresh plan covering
        // only the re-queued jobs, all starting as `pending`. They will
        // transition through `in_progress` → `done` as they run.
        self.dispatch_plan(resume_plan, kind);
    }
}
