//! Transfer-orchestration helpers on [`App`].
//!
//! These take a selection (or the cursor entry) and turn it into a fully
//! queued batch by running through three phases:
//!
//!   1. Build a list of `(local, remote, is_dir)` roots from the pane
//!      selection.
//!   2. Spawn an async task that walks each root into a flat
//!      [`PlannedJob`] plan and probes for conflicts; the task posts
//!      `AppEvent::WalkComplete` (or `WalkFailed`) when finished.
//!   3. On `WalkComplete`, either dispatch directly (no conflicts) or
//!      pop the overwrite-confirmation modal. The user's answer flows
//!      back through `confirm_overwrite_proceed` here, which calls into
//!      [`App::dispatch_plan`] in `checkpoint_glue.rs`.
//!
//! Splitting these out of `mod.rs` keeps the orchestration close
//! together: the enqueue / walk-spawn / confirm path is one cohesive
//! unit rather than scattered across the App lifecycle code.

use std::path::PathBuf;

use crate::transfer::Direction;
use crate::transport;
use crate::tui::event::AppEvent;
use crate::tui::plan::{
    drop_conflicting, find_download_conflicts, find_upload_conflicts, safe_local_name, walk_local,
    walk_remote, PlannedJob,
};
use crate::tui::state::OverwritePending;

use super::{App, LogLevel, Screen};

impl App {
    /// Enqueue uploads for the selected items in the local pane. If
    /// nothing is selected, falls back to the cursor item.
    ///
    /// Detects collisions against the cached remote listing and, if any are
    /// found, prompts for overwrite confirmation before enqueueing. With no
    /// collisions the jobs go straight to the dispatcher.
    pub(super) fn start_selected_uploads(&mut self) {
        if self.transfer_manager.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }

        let any_selected = self.local.entries.iter().any(|e| e.selected);
        let entries: Vec<(String, bool)> = if any_selected {
            self.local
                .entries
                .iter()
                .filter(|e| e.selected)
                .map(|e| (e.name.clone(), e.is_dir))
                .collect()
        } else {
            match self.local.entries.get(self.local.cursor) {
                Some(e) if e.name != ".." => vec![(e.name.clone(), e.is_dir)],
                _ => Vec::new(),
            }
        };

        if entries.is_empty() {
            self.push_log(LogLevel::Warn, "no items to upload".into());
            return;
        }

        // Clear selection upfront — the walk task is async, and we'd rather
        // not surprise the user later if they select more items meanwhile.
        for e in &mut self.local.entries {
            e.selected = false;
        }

        // Build the upload roots: each selected entry becomes a (local, remote)
        // pair. Files become "trivial walks" of a single Upload job; directories
        // unfold via walk_local. Either way the plan goes through the same
        // conflict-check + dispatch flow.
        let local_base = PathBuf::from(&self.local.path);
        let roots: Vec<(PathBuf, String, bool)> = entries
            .iter()
            .map(|(name, is_dir)| {
                (
                    local_base.join(name),
                    transport::join_remote(&self.remote.path, name),
                    *is_dir,
                )
            })
            .collect();

        let pending_count = entries.len();
        self.push_log(
            LogLevel::Info,
            format!("preparing {pending_count} upload(s)…"),
        );
        self.start_upload_walk(roots);
    }

    /// Spawn the upload preparation task: walks every root (file or dir) into
    /// a flat plan, then probes each destination directory once for existing
    /// names to populate `conflict_indices`. Posts back as `WalkComplete`.
    fn start_upload_walk(&mut self, roots: Vec<(PathBuf, String, bool)>) {
        let Some(t) = self.transport.clone() else {
            return;
        };
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            // Phase 1: build the plan from local FS walks. Files become a
            // single Upload job; directories unfold into mkdirs + uploads.
            let mut plan: Vec<PlannedJob> = Vec::new();
            let mut symlinks_skipped: usize = 0;
            for (local, remote, is_dir) in roots {
                let chunk = if is_dir {
                    let walk = walk_local(&local, &remote).await;
                    match walk {
                        Ok(r) => {
                            symlinks_skipped += r.symlinks_skipped;
                            r.plan
                        }
                        Err(e) => {
                            let _ = tx.send(AppEvent::WalkFailed {
                                error: e.to_string(),
                                kind: Direction::Upload,
                            });
                            return;
                        }
                    }
                } else {
                    vec![PlannedJob::Upload {
                        local_path: local,
                        remote_path: remote,
                    }]
                };
                plan.extend(chunk);
            }

            // Phase 2: collect every destination directory mentioned by the
            // plan (the parent of each Upload job), then list each one once
            // and check for conflicts in O(dirs) round-trips.
            let mut transport = t.lock().await;
            let conflict_indices =
                match find_upload_conflicts(&mut **transport, &plan).await {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx.send(AppEvent::WalkFailed {
                            error: e.to_string(),
                            kind: Direction::Upload,
                        });
                        return;
                    }
                };

            let _ = tx.send(AppEvent::WalkComplete {
                plan,
                conflict_indices,
                symlinks_skipped,
                kind: Direction::Upload,
            });
        });
    }

    /// Enqueue downloads for the selected items in the remote pane. If
    /// nothing is selected, falls back to the cursor item.
    pub(super) fn enqueue_selected_downloads(&mut self) {
        if self.transfer_manager.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }

        // Collect (name, is_dir) pairs from the active selection or the
        // cursor entry.
        let selections: Vec<(String, bool)> = {
            let any_selected = self.remote.entries.iter().any(|e| e.selected);
            if any_selected {
                self.remote
                    .entries
                    .iter()
                    .filter(|e| e.selected)
                    .map(|e| (e.name.clone(), e.is_dir))
                    .collect()
            } else {
                match self.remote.entries.get(self.remote.cursor) {
                    Some(e) if e.name != ".." => vec![(e.name.clone(), e.is_dir)],
                    _ => Vec::new(),
                }
            }
        };

        if selections.is_empty() {
            self.push_log(LogLevel::Warn, "no items to download".into());
            return;
        }

        // Clear selection upfront — see start_selected_uploads for rationale.
        for e in &mut self.remote.entries {
            e.selected = false;
        }

        let local_base = PathBuf::from(&self.local.path);
        let roots: Vec<(String, PathBuf, bool)> = selections
            .iter()
            .filter_map(|(name, is_dir)| {
                let safe = safe_local_name(name)?;
                Some((
                    transport::join_remote(&self.remote.path, name),
                    local_base.join(safe),
                    *is_dir,
                ))
            })
            .collect();

        let pending_count = roots.len();
        self.push_log(
            LogLevel::Info,
            format!("preparing {pending_count} download(s)…"),
        );
        self.start_download_walk(roots);
    }

    /// Spawn the download preparation task: walks every root (file or dir)
    /// into a flat plan, then probes each destination file path in the
    /// local FS to populate `conflict_indices`. Posts back as `WalkComplete`.
    fn start_download_walk(&mut self, roots: Vec<(String, PathBuf, bool)>) {
        let Some(t) = self.transport.clone() else {
            return;
        };
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            // Phase 1: build the plan from remote walks. Files become a
            // single Download job; directories unfold into mkdirs (no-ops
            // for downloads — local mkdirs are handled inside walk_remote)
            // plus per-file Download jobs.
            let mut plan: Vec<PlannedJob> = Vec::new();
            let mut symlinks_skipped: usize = 0;
            {
                let mut transport = t.lock().await;
                for (remote, local, is_dir) in roots {
                    let chunk = if is_dir {
                        let walk = walk_remote(&mut **transport, &remote, &local).await;
                        match walk {
                            Ok(r) => {
                                symlinks_skipped += r.symlinks_skipped;
                                r.plan
                            }
                            Err(e) => {
                                let _ = tx.send(AppEvent::WalkFailed {
                                    error: e.to_string(),
                                    kind: Direction::Download,
                                });
                                return;
                            }
                        }
                    } else {
                        vec![PlannedJob::Download {
                            remote_path: remote,
                            local_path: local,
                        }]
                    };
                    plan.extend(chunk);
                }
            }

            // Phase 2: local-FS conflict probe. Every Download job's
            // destination gets metadata-checked. This is sync I/O on the
            // local disk — fast even for thousands of files.
            let conflict_indices = find_download_conflicts(&plan).await;

            let _ = tx.send(AppEvent::WalkComplete {
                plan,
                conflict_indices,
                symlinks_skipped,
                kind: Direction::Download,
            });
        });
    }

    /// Apply the user's answer from the overwrite-confirmation modal.
    /// `skip_conflicts = true` corresponds to the `s` key in the modal —
    /// drop the conflicting jobs and proceed; otherwise overwrite all.
    pub(super) fn confirm_overwrite_proceed(&mut self, skip_conflicts: bool) {
        let Some(op) = self.pending_overwrite.take() else {
            self.screen = Screen::Main;
            return;
        };
        self.screen = Screen::Main;
        match op {
            OverwritePending::Rename { from, to, .. } => {
                self.rename_input.clear();
                self.rename_original.clear();
                self.start_rename(from, to);
            }
            OverwritePending::DownloadPlan {
                plan,
                conflict_indices,
            } => {
                let final_plan = if skip_conflicts {
                    drop_conflicting(plan, &conflict_indices)
                } else {
                    plan
                };
                self.dispatch_plan(final_plan, Direction::Download);
            }
            OverwritePending::UploadPlan {
                plan,
                conflict_indices,
            } => {
                let final_plan = if skip_conflicts {
                    drop_conflicting(plan, &conflict_indices)
                } else {
                    plan
                };
                self.dispatch_plan(final_plan, Direction::Upload);
            }
        }
    }
}
