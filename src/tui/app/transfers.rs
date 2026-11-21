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

use crate::checkpoint::CheckpointKind;
use crate::transport;
use crate::tui::event::AppEvent;
use crate::tui::plan::{
    PlannedJob, drop_conflicting, find_download_conflicts, find_upload_conflicts, safe_local_name,
    walk_local, walk_remote,
};
use crate::tui::state::OverwritePending;

use super::{App, LogLevel, Screen};

/// One selected remote entry, about to be planned as a download.
struct DownloadRoot {
    remote: String,
    local: PathBuf,
    is_dir: bool,
    is_link: bool,
}

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

        // Raw names throughout: these become both the local path read from
        // and the remote path written to. `selection` also keeps the `..`
        // row out, so a stray space on it can't turn into an upload of the
        // parent directory.
        let (lossy, entries): (Vec<_>, Vec<_>) = self
            .local
            .selected_entries()
            .into_iter()
            .partition(|e| e.lossy_name);
        let entries: Vec<(String, bool)> = entries
            .into_iter()
            .map(|e| (e.raw_name.clone(), e.is_dir))
            .collect();
        let lossy = lossy.len();
        if lossy > 0 {
            self.report_lossy_names(lossy);
        }

        if entries.is_empty() {
            if lossy == 0 {
                self.push_log(LogLevel::Warn, "no items to upload".into());
            }
            return;
        }

        // Clear selection upfront — the walk task is async, and we'd rather
        // not surprise the user later if they select more items meanwhile.
        self.local.clear_selection();
        self.start_uploads(entries);
    }

    /// Log `count` local entries left out of an upload because their names
    /// are not valid UTF-8 — the same reason a directory upload skips them.
    pub(super) fn report_lossy_names(&mut self, count: usize) {
        let noun = if count == 1 { "entry" } else { "entries" };
        self.push_log(
            LogLevel::Warn,
            format!(
                "skipped {count} {noun} whose name is not valid UTF-8 and \
                 has no faithful remote form"
            ),
        );
    }

    /// Upload `entries` — `(raw_name, is_dir)` in the local pane's directory
    /// — through the usual conflict check and dispatch. Leaves the selection
    /// alone: a double-click uploads just the file it landed on.
    pub(super) fn start_uploads(&mut self, entries: Vec<(String, bool)>) {
        if self.transfer_manager.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }

        // Build the upload roots: each selected entry becomes a (local, remote)
        // pair. Files become "trivial walks" of a single Upload job; directories
        // unfold via walk_local. Either way the plan goes through the same
        // conflict-check + dispatch flow.
        let local_base = PathBuf::from(&self.local.path);
        // A name that can't be joined onto the remote path is skipped rather
        // than silently retargeted at the destination directory itself.
        let roots: Vec<(PathBuf, String, bool)> = entries
            .iter()
            .filter_map(|(name, is_dir)| {
                let remote = transport::join_remote(&self.remote.path, name)?;
                Some((local_base.join(name), remote, *is_dir))
            })
            .collect();
        let unjoinable = entries.len() - roots.len();
        if unjoinable > 0 {
            let what = if unjoinable == 1 {
                "entry whose name can't be used as a remote file name"
            } else {
                "entries whose names can't be used as remote file names"
            };
            self.push_log(LogLevel::Warn, format!("skipped {unjoinable} {what}"));
        }

        if roots.is_empty() {
            return;
        }
        let pending_count = roots.len();
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
        let connection = self.connection;
        tokio::spawn(async move {
            // Phase 1: build the plan from local FS walks. Files become a
            // single Upload job; directories unfold into mkdirs + uploads.
            let mut plan: Vec<PlannedJob> = Vec::new();
            let mut symlinks_skipped: usize = 0;
            let mut unencodable_skipped: usize = 0;
            let mut unreadable_skipped: usize = 0;
            for (local, remote, is_dir) in roots {
                let chunk = if is_dir {
                    let walk = walk_local(&local, &remote).await;
                    match walk {
                        Ok(r) => {
                            symlinks_skipped += r.symlinks_skipped;
                            unencodable_skipped += r.unencodable_skipped;
                            unreadable_skipped += r.unreadable_skipped;
                            r.plan
                        }
                        Err(e) => {
                            let _ = tx.send(AppEvent::WalkFailed {
                                connection,
                                error: e.to_string(),
                                kind: CheckpointKind::Upload,
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
            let conflicts = match find_upload_conflicts(&t, &plan).await {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(AppEvent::WalkFailed {
                        connection,
                        error: e.to_string(),
                        kind: CheckpointKind::Upload,
                    });
                    return;
                }
            };

            let _ = tx.send(AppEvent::WalkComplete {
                connection,
                plan,
                conflict_indices: conflicts.indices,
                symlinks_skipped,
                unencodable_skipped,
                unreadable_skipped,
                unsafe_skipped: 0,
                unverifiable_uploads: conflicts.unverifiable,
                local_dirs: Vec::new(),
                kind: CheckpointKind::Upload,
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

        // (raw_name, is_dir) pairs from the active selection, or the cursor
        // entry when nothing is selected.
        let selections = self.remote.selection();

        if selections.is_empty() {
            self.push_log(LogLevel::Warn, "no items to download".into());
            return;
        }

        // Clear selection upfront — see start_selected_uploads for rationale.
        self.remote.clear_selection();
        self.enqueue_downloads(selections);
    }

    /// Download `selections` — `(raw_name, is_dir)` in the remote pane's
    /// directory — through the usual conflict check and dispatch. Leaves the
    /// selection alone: a double-click downloads just the file it landed on.
    pub(super) fn enqueue_downloads(&mut self, selections: Vec<(String, bool)>) {
        if self.transfer_manager.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }

        let local_base = PathBuf::from(&self.local.path);
        let links: std::collections::HashSet<&str> = self
            .remote
            .all_rows()
            .iter()
            .filter(|e| e.is_link)
            .map(|e| e.raw_name.as_str())
            .collect();
        // `safe_local_name` is what makes a server-supplied name safe to join
        // onto a local path; it must see the real name, not a sanitized one,
        // or two distinct remote files can land on the same local path.
        let roots: Vec<DownloadRoot> = selections
            .iter()
            .filter_map(|(name, is_dir)| {
                let safe = safe_local_name(name)?;
                let remote = transport::join_remote(&self.remote.path, name)?;
                Some(DownloadRoot {
                    remote,
                    local: local_base.join(safe),
                    is_dir: *is_dir,
                    is_link: links.contains(name.as_str()),
                })
            })
            .collect();
        // Said, as the walk says what it leaves out: the user picked these.
        self.report_unsafe_names(selections.len() - roots.len());
        if roots.is_empty() {
            return;
        }

        let pending_count = roots.len();
        self.push_log(
            LogLevel::Info,
            format!("preparing {pending_count} download(s)…"),
        );
        self.start_download_walk(roots);
    }

    /// Log `count` remote entries left out of a download because their
    /// names can't be used as local file names.
    pub(super) fn report_unsafe_names(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        let what = if count == 1 {
            "entry whose name can't be used as a local file name"
        } else {
            "entries whose names can't be used as local file names"
        };
        self.push_log(LogLevel::Warn, format!("skipped {count} {what}"));
    }

    /// Spawn the download preparation task: walks every root (file or dir)
    /// into a flat plan, then probes each destination file path in the
    /// local FS to populate `conflict_indices`. Posts back as `WalkComplete`.
    fn start_download_walk(&mut self, roots: Vec<DownloadRoot>) {
        let Some(t) = self.transport.clone() else {
            return;
        };
        let tx = self.app_event_tx.clone();
        let connection = self.connection;
        tokio::spawn(async move {
            // Phase 1: build the plan from remote walks. Files become a
            // single Download job; directories unfold into mkdirs (no-ops
            // for downloads — local mkdirs are handled inside walk_remote)
            // plus per-file Download jobs.
            let mut plan: Vec<PlannedJob> = Vec::new();
            let mut symlinks_skipped: usize = 0;
            let mut unreadable_skipped: usize = 0;
            let mut unsafe_skipped: usize = 0;
            let mut local_dirs = Vec::new();
            {
                for root in roots {
                    // A symlink the user picked is followed, as a pick of a
                    // file it points at would be: a link to a folder is
                    // walked as the folder. Where the server can't say what
                    // it points at (FTP), it is fetched as a file.
                    let is_dir = root.is_dir
                        || (root.is_link
                            && matches!(
                                t.lock().await.metadata(&root.remote).await,
                                Ok(Some(e)) if e.is_dir()
                            ));
                    let DownloadRoot { remote, local, .. } = root;
                    let chunk = if is_dir {
                        let walk = walk_remote(&t, &remote, &local).await;
                        match walk {
                            Ok(r) => {
                                symlinks_skipped += r.symlinks_skipped;
                                unreadable_skipped += r.unreadable_skipped;
                                unsafe_skipped += r.unsafe_skipped;
                                local_dirs.extend(r.local_dirs);
                                r.plan
                            }
                            Err(e) => {
                                let _ = tx.send(AppEvent::WalkFailed {
                                    connection,
                                    error: e.to_string(),
                                    kind: CheckpointKind::Download,
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
                connection,
                plan,
                conflict_indices,
                symlinks_skipped,
                // Remote-side walks build local paths through
                // `safe_local_name`, not through UTF-8 decoding.
                unencodable_skipped: 0,
                unreadable_skipped,
                unsafe_skipped,
                unverifiable_uploads: 0,
                local_dirs,
                kind: CheckpointKind::Download,
            });
        });
    }

    /// Create the local folders a download plan records, now that it is going
    /// ahead; see [`crate::tui::plan::WalkResult::local_dirs`]. Off the
    /// interface's thread: a large tree is thousands of folders. A folder
    /// that can't be made, or sits below a symlink another user planted, is
    /// reported in the log pane as `AppEvent::LocalDirsFailed`; a download
    /// into it fails with its own error.
    pub(super) fn create_local_dirs(&mut self, dirs: Vec<PathBuf>) {
        if dirs.is_empty() {
            return;
        }
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let failures = make_local_dirs(dirs).await;
            if !failures.is_empty() {
                let _ = tx.send(AppEvent::LocalDirsFailed { failures });
            }
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
                self.clear_rename_form();
                self.start_rename(from, to, true);
            }
            OverwritePending::DownloadPlan {
                plan,
                conflict_indices,
                local_dirs,
            } => {
                let final_plan = if skip_conflicts {
                    drop_conflicting(plan, &conflict_indices)
                } else {
                    plan
                };
                if final_plan.is_empty() && local_dirs.is_empty() {
                    self.report_all_skipped(CheckpointKind::Download);
                    return;
                }
                self.create_local_dirs(local_dirs);
                self.dispatch_plan(final_plan, CheckpointKind::Download);
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
                if final_plan.is_empty() {
                    self.report_all_skipped(CheckpointKind::Upload);
                    return;
                }
                self.dispatch_plan(final_plan, CheckpointKind::Upload);
            }
        }
    }

    /// Say that skipping conflicts left nothing to transfer. Otherwise the
    /// user answers the prompt and sees no line at all.
    fn report_all_skipped(&mut self, kind: CheckpointKind) {
        let message = match kind {
            CheckpointKind::Download => "nothing to download: every file already exists here",
            CheckpointKind::Upload => "nothing to upload: every file already exists on the server",
        };
        self.push_log(LogLevel::Info, message.into());
    }
}

/// Make each of `dirs`, refusing any whose path runs through a
/// [`transport::foreign_symlink`], as a download's own folders are refused
/// ([`transport::refuse_foreign_symlinks_above`]). Hands back one sanitized
/// line per folder not made.
///
/// The check and the creation are separate steps, as they are for a file's
/// download: this closes the window between planning and dispatch (an
/// overwrite prompt can stay open indefinitely), not a race with a planter
/// working in the same instant.
async fn make_local_dirs(dirs: Vec<PathBuf>) -> Vec<String> {
    // Folders already found not to be planted links: sibling folders share
    // every ancestor, and a large tree would otherwise stat each one again
    // per folder.
    let mut clear: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut failures = Vec::new();
    'dirs: for dir in dirs {
        for at in dir.ancestors() {
            if at.as_os_str().is_empty() || clear.contains(at) {
                continue;
            }
            if transport::foreign_symlink(at).await {
                failures.push(crate::error::sanitize(format!(
                    "not creating local folder {}: {} is a symlink owned by another user",
                    dir.display(),
                    at.display()
                )));
                continue 'dirs;
            }
            clear.insert(at.to_path_buf());
        }
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            failures.push(crate::error::sanitize(format!(
                "could not create local folder {}: {e}",
                dir.display()
            )));
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::tests_support::app_on_main;
    use crate::tui::state::OverwritePending;

    fn logged(a: &App, needle: &str) -> bool {
        a.log.iter().any(|l| l.message.contains(needle))
    }

    fn downloads(paths: &[&str]) -> Vec<PlannedJob> {
        paths
            .iter()
            .map(|p| PlannedJob::Download {
                remote_path: (*p).into(),
                local_path: format!("/l{p}").into(),
            })
            .collect()
    }

    /// A folder below a link another user planted isn't made — not through
    /// the link, where it would land in the link's target — and is reported.
    /// The user's own link is followed, as a download's own folders are.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_planted_symlink_above_an_empty_folder_is_refused() {
        let home = crate::paths::test_home();
        let target = home.path().join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, home.path().join("photos")).unwrap();
        let dir = home.path().join("photos/authorized_keys");

        crate::transport::EFFECTIVE_UID.set(Some(rustix::process::geteuid().as_raw() + 1));
        let failures = make_local_dirs(vec![dir.clone()]).await;
        crate::transport::EFFECTIVE_UID.set(None);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0].contains("owned by another user"),
            "{failures:?}"
        );
        assert!(!target.join("authorized_keys").exists());

        assert!(make_local_dirs(vec![dir]).await.is_empty());
        assert!(target.join("authorized_keys").is_dir());
    }

    /// The link may be the folder itself, not only one above it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_planted_symlink_named_as_the_folder_is_refused() {
        let home = crate::paths::test_home();
        let target = home.path().join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        let dir = home.path().join("empty");
        std::os::unix::fs::symlink(&target, &dir).unwrap();

        crate::transport::EFFECTIVE_UID.set(Some(rustix::process::geteuid().as_raw() + 1));
        let failures = make_local_dirs(vec![dir]).await;
        crate::transport::EFFECTIVE_UID.set(None);
        assert_eq!(failures.len(), 1, "{failures:?}");
    }

    /// A folder that can't be made is said, and the rest are still made.
    #[tokio::test]
    async fn a_folder_that_cant_be_made_is_reported() {
        let home = crate::paths::test_home();
        std::fs::write(home.path().join("file"), b"x").unwrap();
        let bad = home.path().join("file/sub");
        let good = home.path().join("a/b");

        let failures = make_local_dirs(vec![bad, good.clone()]).await;
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0].contains("could not create local folder"),
            "{failures:?}"
        );
        assert!(good.is_dir());
    }

    /// The failures reach the log pane; they used to go only to `tracing`.
    #[test]
    fn folders_not_made_are_shown_in_the_log() {
        let mut a = app_on_main(0);
        a.handle_app_event(AppEvent::LocalDirsFailed {
            failures: vec!["could not create local folder /x: denied".into()],
        });
        assert!(
            logged(&a, "could not create local folder /x"),
            "{:?}",
            a.log
        );
    }

    /// `create_local_dirs` sends its failures back as an event.
    #[tokio::test]
    async fn create_local_dirs_sends_its_failures_back() {
        let home = crate::paths::test_home();
        std::fs::write(home.path().join("file"), b"x").unwrap();
        let mut a = app_on_main(0);
        let mut rx = a.app_event_rx.take().unwrap();
        a.create_local_dirs(vec![home.path().join("file/sub")]);
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("an event within 5 s")
            .unwrap();
        let AppEvent::LocalDirsFailed { failures } = event else {
            panic!("expected LocalDirsFailed");
        };
        assert_eq!(failures.len(), 1, "{failures:?}");
    }

    /// Skipping every conflict of a download leaves nothing, and says so.
    #[test]
    fn skipping_every_conflicting_download_says_so() {
        let mut a = app_on_main(0);
        let plan = downloads(&["/srv/a", "/srv/b"]);
        a.pending_overwrite = Some(OverwritePending::DownloadPlan {
            conflict_indices: (0..plan.len()).collect(),
            plan,
            local_dirs: vec![],
        });
        a.screen = Screen::ConfirmOverwrite;
        a.confirm_overwrite_proceed(true);
        assert!(logged(&a, "nothing to download"), "{:?}", a.log);
    }

    #[test]
    fn skipping_every_conflicting_upload_says_so() {
        let mut a = app_on_main(0);
        a.pending_overwrite = Some(OverwritePending::UploadPlan {
            plan: vec![PlannedJob::Upload {
                local_path: "/l/a".into(),
                remote_path: "/srv/a".into(),
            }],
            conflict_indices: vec![0],
        });
        a.screen = Screen::ConfirmOverwrite;
        a.confirm_overwrite_proceed(true);
        assert!(logged(&a, "nothing to upload"), "{:?}", a.log);
    }

    /// Skipping only some conflicts leaves work, and claims none is left.
    #[test]
    fn skipping_some_conflicts_is_not_called_nothing() {
        let mut a = app_on_main(0);
        let plan = downloads(&["/srv/a", "/srv/b"]);
        a.pending_overwrite = Some(OverwritePending::DownloadPlan {
            conflict_indices: vec![0],
            plan,
            local_dirs: vec![],
        });
        a.confirm_overwrite_proceed(true);
        assert!(!logged(&a, "nothing to download"), "{:?}", a.log);
    }

    /// An upload name that can't be joined onto the remote path is said, as
    /// a download's is.
    #[tokio::test]
    async fn an_unjoinable_upload_name_is_reported() {
        let mut a = app_on_main(0);
        a.transfer_manager = Some(crate::transfer::TransferManager::new(1).0);
        a.start_uploads(vec![("a/b".into(), false)]);
        assert!(logged(&a, "skipped 1 entry whose name"), "{:?}", a.log);
        assert!(!logged(&a, "preparing"), "{:?}", a.log);
    }

    /// Folders are skipped for a non-UTF-8 name too: "entries", not "files".
    #[test]
    fn lossy_names_are_counted_as_entries() {
        let mut a = app_on_main(0);
        a.report_lossy_names(2);
        assert!(logged(&a, "skipped 2 entries"), "{:?}", a.log);
    }
}
