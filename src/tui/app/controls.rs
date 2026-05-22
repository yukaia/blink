//! Transfer-batch controls and theme cycling.
//!
//! Bundles the small one-shot actions triggered from the Main screen
//! key bindings:
//!
//! - `c` — `request_cancel_selected_transfer` (single-job cancel modal)
//! - `C` — `request_cancel_selected_batch` (whole-batch cancel modal,
//!   falls through to single if the job isn't part of a batch)
//! - `p` — `toggle_pause`
//! - `t` — `cycle_theme`
//! - `active_jobs` — small read-only helper the Transfers pane and the
//!   cancel helpers consume; lives here because the cancels are its
//!   primary callers.

use crate::theme::Theme;
use crate::transfer::TransferJob;
use crate::tui::state::PendingCancel;

use super::{name_for_job, App, LogLevel, Screen};

impl App {
    pub(super) fn request_cancel_selected_transfer(&mut self) {
        let jobs = self.active_jobs();
        if jobs.is_empty() {
            return;
        }
        let idx = self.transfer_cursor.min(jobs.len() - 1);
        let job = &jobs[idx];
        self.pending_cancel = Some(PendingCancel::Single {
            id: job.id,
            name: name_for_job(job),
        });
        self.previous_screen = Screen::Main;
        self.screen = Screen::ConfirmCancel;
    }

    /// Cancel every job in the batch the cursor item belongs to. If the
    /// cursor job has no batch_id (single-file enqueue), this falls back
    /// to the single-job cancel modal.
    pub(super) fn request_cancel_selected_batch(&mut self) {
        let manager = match self.transfer_manager.as_ref() {
            Some(m) => m,
            None => return,
        };
        let active_jobs = self.active_jobs();
        if active_jobs.is_empty() {
            return;
        }
        let idx = self.transfer_cursor.min(active_jobs.len() - 1);
        let cursor_job = &active_jobs[idx];

        let Some(batch_id) = cursor_job.batch_id else {
            // Not a batched job — fall through to the single-job modal so
            // the gesture still does something useful.
            self.request_cancel_selected_transfer();
            return;
        };

        // Count siblings in the batch, including pending ones (which the
        // active-only list above doesn't include).
        let snapshot = manager.snapshot();
        let active = snapshot
            .iter()
            .filter(|j| {
                j.batch_id == Some(batch_id)
                    && matches!(j.state, crate::transfer::TransferState::Active)
            })
            .count();
        let pending = snapshot
            .iter()
            .filter(|j| {
                j.batch_id == Some(batch_id)
                    && matches!(j.state, crate::transfer::TransferState::Pending)
            })
            .count();
        if active == 0 && pending == 0 {
            return;
        }

        self.pending_cancel = Some(PendingCancel::Batch {
            batch_id,
            active,
            pending,
            cursor_name: name_for_job(cursor_job),
        });
        self.previous_screen = Screen::Main;
        self.screen = Screen::ConfirmCancel;
    }

    pub(super) fn toggle_pause(&mut self) {
        let Some(manager) = &self.transfer_manager else {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        };
        if manager.is_paused() {
            manager.resume();
        } else {
            manager.pause();
        }
        // The Paused / Resumed log lines are emitted from
        // handle_transfer_event when the dispatcher echoes the event back.
    }

    /// Cycle to the next theme in `Theme::list_all_names`. Built-ins come
    /// first alphabetically, then user themes (deduplicated by name). The
    /// new theme is applied immediately and persisted to `config.ini` so
    /// it survives a restart.
    pub(super) fn cycle_theme(&mut self) {
        let names = Theme::list_all_names();
        if names.is_empty() {
            return;
        }
        // Find current; if not in the list (shouldn't happen, but defend
        // against it), start from -1 so the first cycle lands on index 0.
        let current_idx = names
            .iter()
            .position(|n| n == &self.theme.name)
            .map(|i| i as isize)
            .unwrap_or(-1);
        let next_idx = ((current_idx + 1) as usize) % names.len();
        let next_name = &names[next_idx];

        match Theme::load(next_name) {
            Ok(theme) => {
                self.config.general.theme = theme.name.clone();
                self.theme = theme;
                self.push_log(
                    LogLevel::Info,
                    format!("theme: {} ({}/{})",
                        next_name,
                        next_idx + 1,
                        names.len()),
                );
                // Persist as a best-effort. A save failure here shouldn't
                // refuse the in-memory swap (the user can see the new theme
                // already), so we log and move on.
                if let Err(e) = self.config.save() {
                    self.push_log(
                        LogLevel::Warn,
                        format!("could not save theme preference: {e}"),
                    );
                }
            }
            Err(e) => {
                // Should be rare since list_all_names already probed parse-
                // ability, but a TOCTOU between probe and load is possible.
                self.push_log(
                    LogLevel::Error,
                    format!("theme {next_name} failed to load: {e}"),
                );
            }
        }
    }

    /// Snapshot of currently-running jobs, for the transfer strip and the
    /// cancel helpers above.
    pub fn active_jobs(&self) -> Vec<TransferJob> {
        self.transfer_manager
            .as_ref()
            .map(|m| {
                m.snapshot()
                    .into_iter()
                    .filter(|j| j.state == crate::transfer::TransferState::Active)
                    .collect()
            })
            .unwrap_or_default()
    }
}
