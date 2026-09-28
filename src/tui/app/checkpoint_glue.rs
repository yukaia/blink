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
use crate::transfer::{Direction, EnqueueError, TransferManager, destination_key};
use crate::tui::plan::PlannedJob;

use super::{App, LogLevel, WaitingJob};

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

        // Drop jobs whose destination is already being written, before the
        // plan is recorded anywhere. The manager would refuse them at
        // enqueue, but by then they would be in the checkpoint as `pending`
        // with nothing to run them — and a checkpoint with pending entries
        // blocks `r` for the rest of the session.
        let plan = self.drop_duplicate_destinations(&manager, plan);
        if plan.is_empty() {
            return;
        }
        // Jobs owed from earlier go first; while any are still waiting, this
        // plan joins the back of the line rather than taking room they are
        // owed.
        self.queue_waiting_jobs();

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
                PlannedJob::Download {
                    remote_path,
                    local_path,
                } => CheckpointJob::Download {
                    remote_path: remote_path.clone(),
                    local_path: local_path.clone(),
                    status: JobStatus::Pending,
                },
                PlannedJob::Upload {
                    local_path,
                    remote_path,
                } => CheckpointJob::Upload {
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
            let waiting = self.waiting_jobs.entry(ck_kind).or_default();
            if !waiting.is_empty() {
                waiting.push_back(WaitingJob {
                    cp_idx: base + cp_idx,
                    batch_id,
                });
                dropped += 1;
                continue;
            }
            let is_mkdir = matches!(job, PlannedJob::Mkdir { .. });
            let job_id = match (job, batch_id) {
                (PlannedJob::Mkdir { remote_path }, Some(b)) => {
                    manager.enqueue_mkdir_batched(remote_path, b)
                }
                (PlannedJob::Mkdir { remote_path }, None) => manager.enqueue_mkdir(remote_path),
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
                Ok(id) => {
                    self.checkpoint_job_map.insert(id, (ck_kind, base + cp_idx));
                    if is_mkdir {
                        dirs += 1;
                    } else {
                        files += 1;
                    }
                }
                // Queue cap reached. The job waits, still `pending` in the
                // checkpoint, and is queued as earlier jobs finish.
                Err(EnqueueError::QueueFull) => {
                    self.waiting_jobs
                        .entry(ck_kind)
                        .or_default()
                        .push_back(WaitingJob {
                            cp_idx: base + cp_idx,
                            batch_id,
                        });
                    dropped += 1;
                }
                // Filtered out above, and nothing else enqueues between the
                // filter and here: both run on the UI thread. Should it ever
                // happen, the entry must not stay `pending` with nothing to
                // run it, which would block `r` for the session.
                Err(EnqueueError::Duplicate) => {
                    if let Some(cp) = self.active_checkpoints.get_mut(&ck_kind) {
                        cp.mark_cancelled(base + cp_idx);
                    }
                }
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
                LogLevel::Info,
                format!(
                    "transfer queue is full: {dropped} job(s) will be queued as \
                     earlier ones finish"
                ),
            );
        }
    }

    /// Queue as many waiting jobs as the transfer queue has room for, oldest
    /// first. Called whenever a transfer ends, which is when room appears,
    /// and before a new plan is queued.
    ///
    /// A batch that met a full queue used to leave the rest `pending` in the
    /// checkpoint with no job ids: nothing ever ran them, the checkpoint
    /// never settled, and the `r` the log suggested was refused for the rest
    /// of the session because a batch was still "in flight".
    pub(super) fn queue_waiting_jobs(&mut self) {
        let Some(manager) = self.transfer_manager.clone() else {
            return;
        };
        for kind in [CheckpointKind::Download, CheckpointKind::Upload] {
            let mut cancelled = false;
            while let Some(next) = self.waiting_jobs.get(&kind).and_then(|q| q.front()) {
                let (cp_idx, batch_id) = (next.cp_idx, next.batch_id);
                let Some(job) = self
                    .active_checkpoints
                    .get(&kind)
                    .and_then(|cp| cp.jobs.get(cp_idx))
                    .cloned()
                else {
                    // No checkpoint to take it from: nothing left to queue.
                    self.waiting_jobs.remove(&kind);
                    break;
                };
                match enqueue_checkpoint_job(&manager, &job, batch_id) {
                    Ok(id) => {
                        self.checkpoint_job_map.insert(id, (kind, cp_idx));
                    }
                    Err(EnqueueError::QueueFull) => break,
                    // Another job took this destination while it waited. The
                    // entry must not stay `pending` with nothing to run it.
                    Err(EnqueueError::Duplicate) => {
                        if let Some(cp) = self.active_checkpoints.get_mut(&kind) {
                            cp.mark_cancelled(cp_idx);
                        }
                        cancelled = true;
                    }
                }
                if let Some(q) = self.waiting_jobs.get_mut(&kind) {
                    q.pop_front();
                }
            }
            if cancelled {
                self.settle_checkpoint(kind, CheckpointFlush::Debounced);
            }
        }
    }

    /// Drop a cancelled batch's waiting jobs and mark their entries
    /// cancelled. They have no job ids, so the cancel that goes through the
    /// transfer manager cannot reach them; left alone, they would be queued
    /// after the rest of their batch was cancelled.
    pub(super) fn cancel_waiting_in_batch(&mut self, batch_id: u64) {
        let mut touched = Vec::new();
        for (kind, queue) in self.waiting_jobs.iter_mut() {
            let before = queue.len();
            let mut kept = std::collections::VecDeque::with_capacity(before);
            for w in queue.drain(..) {
                if w.batch_id == Some(batch_id) {
                    if let Some(cp) = self.active_checkpoints.get_mut(kind) {
                        cp.mark_cancelled(w.cp_idx);
                    }
                } else {
                    kept.push_back(w);
                }
            }
            if kept.len() != before {
                touched.push(*kind);
            }
            *queue = kept;
        }
        for kind in touched {
            self.settle_checkpoint(kind, CheckpointFlush::Force);
        }
    }

    /// Remove plan entries whose destination another job already writes —
    /// one still queued or running, or an earlier entry of this same plan.
    ///
    /// Two jobs on one destination corrupt it: see
    /// [`crate::transfer::destination_key`]. The two cases are reported
    /// differently. A duplicate of a live job loses nothing, because that
    /// job writes the same file, so it is counted in one line. A collision
    /// inside the plan — `README` and `readme` from one server directory on
    /// a case-insensitive filesystem — means one of the two files will not
    /// be transferred, so each one is named.
    fn drop_duplicate_destinations(
        &mut self,
        manager: &TransferManager,
        plan: Vec<PlannedJob>,
    ) -> Vec<PlannedJob> {
        let mut seen = std::collections::HashSet::new();
        let mut already_live = 0usize;
        let mut kept = Vec::with_capacity(plan.len());
        for job in plan {
            let key = match &job {
                PlannedJob::Mkdir { .. } => None,
                PlannedJob::Download {
                    remote_path,
                    local_path,
                } => destination_key(Direction::Download, remote_path, local_path),
                PlannedJob::Upload {
                    local_path,
                    remote_path,
                } => destination_key(Direction::Upload, remote_path, local_path),
            };
            let Some(key) = key else {
                kept.push(job);
                continue;
            };
            if manager.is_in_flight(&key) {
                already_live += 1;
                continue;
            }
            if !seen.insert(key) {
                let (verb, source) = match &job {
                    PlannedJob::Download { remote_path, .. } => {
                        ("downloading", remote_path.clone())
                    }
                    PlannedJob::Upload { local_path, .. } => {
                        ("uploading", local_path.display().to_string())
                    }
                    PlannedJob::Mkdir { .. } => unreachable!("mkdirs have no key"),
                };
                self.push_log(
                    LogLevel::Warn,
                    format!(
                        "not {verb} {source}: another file in this batch has \
                         the same destination"
                    ),
                );
                continue;
            }
            kept.push(job);
        }
        if already_live > 0 {
            self.push_log(
                LogLevel::Info,
                format!("skipped {already_live} file(s) already queued or running"),
            );
        }
        kept
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
                CheckpointJob::Download {
                    remote_path,
                    local_path,
                    ..
                } => PlannedJob::Download {
                    remote_path: remote_path.clone(),
                    local_path: local_path.clone(),
                },
                CheckpointJob::Upload {
                    local_path,
                    remote_path,
                    ..
                } => PlannedJob::Upload {
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

/// Queue one checkpoint entry as a transfer job, in `batch_id` if it has one.
fn enqueue_checkpoint_job(
    manager: &TransferManager,
    job: &CheckpointJob,
    batch_id: Option<u64>,
) -> Result<u64, EnqueueError> {
    match (job.clone(), batch_id) {
        (CheckpointJob::Mkdir { remote_path, .. }, Some(b)) => {
            manager.enqueue_mkdir_batched(remote_path, b)
        }
        (CheckpointJob::Mkdir { remote_path, .. }, None) => manager.enqueue_mkdir(remote_path),
        (
            CheckpointJob::Download {
                remote_path,
                local_path,
                ..
            },
            Some(b),
        ) => manager.enqueue_download_batched(remote_path, local_path, b),
        (
            CheckpointJob::Download {
                remote_path,
                local_path,
                ..
            },
            None,
        ) => manager.enqueue_download(remote_path, local_path),
        (
            CheckpointJob::Upload {
                local_path,
                remote_path,
                ..
            },
            Some(b),
        ) => manager.enqueue_upload_batched(local_path, remote_path, b),
        (
            CheckpointJob::Upload {
                local_path,
                remote_path,
                ..
            },
            None,
        ) => manager.enqueue_upload(local_path, remote_path),
    }
}
