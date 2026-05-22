//! Pane navigation and refresh.
//!
//! Pulls out the methods that move the cursor between panes, descend
//! into / out of directories, and trigger the async listing tasks. The
//! refresh helpers spawn worker tasks that post results back as
//! `AppEvent::Listed` / `AppEvent::LocalListed`, which `events.rs`
//! consumes.

use crate::transport;
use crate::tui::event::AppEvent;
use crate::tui::state::{PaneEntry, PaneState};

use super::{App, BottomPane, LogLevel, Pane};

impl App {
    /// Cycle the active pane forward (`forward = true`) or backward through
    /// Local → Remote → Transfers → Log → Local. Updates `bottom_pane` so the
    /// bottom panel displays whichever bottom page is now focused.
    pub(super) fn cycle_pane(&mut self, forward: bool) {
        let next = if forward {
            match self.active_pane {
                Pane::Local => Pane::Remote,
                Pane::Remote => Pane::Transfers,
                Pane::Transfers => Pane::Log,
                Pane::Log => Pane::Local,
            }
        } else {
            match self.active_pane {
                Pane::Local => Pane::Log,
                Pane::Log => Pane::Transfers,
                Pane::Transfers => Pane::Remote,
                Pane::Remote => Pane::Local,
            }
        };
        self.active_pane = next;
        match next {
            Pane::Transfers => self.bottom_pane = BottomPane::Transfers,
            Pane::Log => self.bottom_pane = BottomPane::Log,
            _ => {}
        }
    }

    pub(super) fn move_transfer_cursor(&mut self, delta: isize) {
        let len = self.active_jobs().len();
        if len == 0 {
            self.transfer_cursor = 0;
            return;
        }
        let max = len - 1;
        let mut next = self.transfer_cursor as isize + delta;
        if next < 0 {
            next = 0;
        }
        if next as usize > max {
            next = max as isize;
        }
        self.transfer_cursor = next as usize;
    }

    pub(super) fn active_pane_mut(&mut self) -> Option<&mut PaneState> {
        match self.active_pane {
            Pane::Local => Some(&mut self.local),
            Pane::Remote => Some(&mut self.remote),
            Pane::Transfers | Pane::Log => None,
        }
    }

    pub(super) fn refresh_active_pane(&mut self) {
        match self.active_pane {
            Pane::Local => {
                self.refresh_local_pane();
                self.push_log(
                    LogLevel::Info,
                    format!("refreshed: {}", self.local.path),
                );
            }
            Pane::Remote => {
                if self.transport.is_some() {
                    let path = self.remote.path.clone();
                    self.refresh_remote_pane(path);
                } else {
                    self.push_log(LogLevel::Warn, "not connected".into());
                }
            }
            Pane::Transfers | Pane::Log => {
                // Nothing to refresh on these panes.
            }
        }
    }

    pub(super) fn local_enter(&mut self) {
        let Some(entry) = self.local.entries.get(self.local.cursor) else {
            return;
        };
        if !entry.is_dir {
            return;
        }
        let mut path = std::path::PathBuf::from(&self.local.path);
        if entry.name == ".." {
            path.pop();
        } else {
            path.push(&entry.name);
        }
        self.local.path = path.display().to_string();
        self.local.cursor = 0;
        self.refresh_local_pane();
    }

    pub(super) fn remote_enter(&mut self) {
        let Some(entry) = self.remote.entries.get(self.remote.cursor) else {
            return;
        };
        if !entry.is_dir {
            return;
        }
        let new_path = if entry.name == ".." {
            transport::parent_remote(&self.remote.path)
        } else {
            transport::join_remote(&self.remote.path, &entry.name)
        };
        self.refresh_remote_pane(new_path);
    }

    /// Kick off a remote `list` task. The result arrives as `AppEvent::Listed`.
    pub(super) fn refresh_remote_pane(&mut self, path: String) {
        let Some(t) = self.transport.clone() else {
            return;
        };
        let path_changed = path != self.remote.path;
        // Reflect the new path immediately so the UI shows where we're going,
        // and so the stale-guard in handle_app_event can compare against it.
        self.remote.path = path.clone();
        self.remote.entries.clear();
        if path_changed {
            // Navigation: drop into the new dir at the top.
            self.remote.cursor = 0;
        }
        let tx = self.app_event_tx.clone();

        tokio::spawn(async move {
            let mut transport = t.lock().await;
            let event = match transport.list(&path).await {
                Ok(entries) => AppEvent::Listed { path, entries },
                Err(e) => AppEvent::ListFailed {
                    path,
                    error: e.to_string(),
                },
            };
            let _ = tx.send(event);
        });
    }

    /// Kick off a local `read_dir` task. The result arrives as
    /// `AppEvent::LocalListed` / `LocalListFailed`.
    ///
    /// Why async: `read_dir` + per-entry `metadata()` is fast on a local
    /// SSD but can stall for seconds on an NFS, SMB, or sshfs mount.
    /// Doing it inline on the UI thread froze the entire event loop —
    /// keystrokes, redraws, transfer-event drain, the lot. Now the work
    /// runs on tokio's blocking pool and the UI keeps ticking.
    pub(super) fn refresh_local_pane(&mut self) {
        let path = self.local.path.clone();
        // Show just `..` immediately so the user sees the navigation
        // landed even before the read_dir finishes.
        self.local.set_entries(vec![PaneEntry {
            name: "..".into(),
            is_dir: true,
            size: 0,
            selected: false,
            previewable_image: false,
        }]);

        let tx = self.app_event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let pb = std::path::PathBuf::from(&path);
            let mut entries = vec![PaneEntry {
                name: "..".into(),
                is_dir: true,
                size: 0,
                selected: false,
                previewable_image: false,
            }];
            let read = match std::fs::read_dir(&pb) {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(AppEvent::LocalListFailed {
                        path,
                        error: e.to_string(),
                    });
                    return;
                }
            };
            for entry in read.flatten() {
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let name = entry.file_name().to_string_lossy().to_string();
                let is_dir = meta.is_dir();
                entries.push(PaneEntry {
                    previewable_image: !is_dir
                        && crate::preview::is_previewable_image(&name),
                    name,
                    is_dir,
                    size: if is_dir { 0 } else { meta.len() },
                    selected: false,
                });
            }
            // Directories first, then alpha within each group.
            entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.name.cmp(&b.name),
            });
            let _ = tx.send(AppEvent::LocalListed { path, entries });
        });
    }
}
