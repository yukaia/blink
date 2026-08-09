//! Glue between the recursive-walk planner, the dispatcher, and the
//! checkpoint file on disk.
//!
//! These methods own the checkpoint state on `App`:
//!
//! - [`App::dispatch_plan`] takes a finalised `Vec<PlannedJob>`, writes a
//!   fresh checkpoint to disk *before* enqueuing anything, then hands
//!   jobs to the dispatcher and records the `job_id → cp_idx` mapping
//!   so the per-job event handler in `events.rs` knows which plan
//!   entry each dispatcher event refers to.
//! - [`App::resume_walk`] loads a previously persisted checkpoint, drops
//!   the done entries, and re-queues the remainder through
//!   `dispatch_plan`.
//! - [`App::cancel_batch_in_checkpoint`] marks a cancelled batch's entries
//!   and hands off to [`App::settle_checkpoint`], which drops the
//!   checkpoint once nothing resumable is left, or flushes it otherwise.
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

/// Whether [`App::settle_checkpoint`] should write immediately or let the
/// debounce decide.
///
/// Per-job transitions must stay debounced: a batch can be 100k jobs, and an
/// fsync apiece would dominate the transfer. Terminal moments — a cancel —
/// force the write, because the state that would be lost is the state the
/// user just asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CheckpointFlush {
    Debounced,
    Force,
}

impl App {
    /// Tear down the active checkpoint: remove the in-memory state and
    /// delete the file on disk.
    ///
    /// Called when a whole batch is cancelled (user pressed `C` and
    /// confirmed) or when a transfer fails in a way that makes the batch
    /// unresumable. Soft-failures (e.g. the file was already removed) are
    /// logged at `warn` and do not abort other work.
    pub(super) fn cancel_batch_in_checkpoint(&mut self, job_ids: &[u64]) {
        // Mark just this batch's entries, then drop the checkpoint only if
        // nothing resumable is left. Removing the whole file — which is what
        // this used to do — would throw away a *different* batch that is
        // still running and still tracked in the same file.
        let mut touched: Vec<CheckpointKind> = Vec::new();
        for id in job_ids {
            let Some((kind, idx)) = self.checkpoint_job_map.remove(id) else {
                continue;
            };
            if let Some(cp) = self.active_checkpoints.get_mut(&kind) {
                cp.mark_cancelled(idx);
                if !touched.contains(&kind) {
                    touched.push(kind);
                }
            }
        }
        for kind in touched {
            // Force: the user is likely to quit or resume right after
            // cancelling, and a lost cancel means the abandoned jobs come
            // back on the next `r` / `R`.
            self.settle_checkpoint(kind, CheckpointFlush::Force);
        }
    }

    /// Flush a checkpoint, or drop it when it has no work left.
    ///
    /// "No work left" covers both the batch finishing and the batch being
    /// cancelled: either way a later `r` / `R` has nothing to re-queue, and
    /// leaving the file behind invites resuming something already settled.
    pub(super) fn settle_checkpoint(&mut self, kind: CheckpointKind, flush: CheckpointFlush) {
        let Some(cp) = self.active_checkpoints.get_mut(&kind) else {
            return;
        };
        if cp.pending_count() == 0 {
            let session = cp.session.clone();
            self.active_checkpoints.remove(&kind);
            self.checkpoint_job_map.retain(|_, (k, _)| *k != kind);
            if let Err(e) = Checkpoint::remove(&session, kind) {
                self.push_log(
                    LogLevel::Warn,
                    format!("could not remove checkpoint file: {e}"),
                );
            }
            return;
        }
        let written = match flush {
            CheckpointFlush::Debounced => cp.flush_if_due(),
            CheckpointFlush::Force => cp.flush(),
        };
        if let Err(e) = written {
            self.push_log(LogLevel::Warn, format!("checkpoint save failed: {e}"));
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

        // Append to the checkpoint already tracking this direction, if there
        // is one. Overwriting it — which is what a fresh `Checkpoint::new`
        // did — destroyed the plan of a batch that was still running, taking
        // its resumability with it and stranding the `.part` files of its
        // unfinished downloads, since the checkpoint is the only record of
        // where those are.
        let base = match self.active_checkpoints.get_mut(&ck_kind) {
            Some(existing) => existing.append(ck_jobs),
            None => {
                self.active_checkpoints
                    .insert(ck_kind, Checkpoint::new(&session_name, ck_kind, ck_jobs));
                0
            }
        };
        // Persist the whole plan before any I/O starts, so a kill during the
        // first transfer still leaves something to resume. Unconditional
        // rather than debounced: there is nothing to coalesce yet.
        if let Some(cp) = self.active_checkpoints.get_mut(&ck_kind)
            && let Err(e) = cp.flush()
        {
            self.push_log(
                LogLevel::Warn,
                format!("checkpoint save failed (resume unavailable): {e}"),
            );
        }
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
                    self.checkpoint_job_map.insert(id, (ck_kind, base + cp_idx));
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

        // Refuse while a batch of this direction is still tracked: the file
        // on disk *is* that batch's state, so re-queuing from it would
        // duplicate jobs that are already in flight.
        if let Some(active) = self.active_checkpoints.get(&ck_kind)
            && active.pending_count() > 0
        {
            self.push_log(
                LogLevel::Warn,
                format!(
                    "a {} batch is still in flight — let it finish or cancel it first",
                    ck_kind.as_str()
                ),
            );
            return;
        }

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
