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
        let len = self.transfer_row_count();
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
                self.push_log(LogLevel::Info, format!("refreshed: {}", self.local.path));
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

    /// Log that the local entry shown as `name` can't be opened: its name is
    /// not valid UTF-8, and the pane addresses files by `String`.
    pub(super) fn report_unopenable(&mut self, name: &str) {
        self.push_log(
            LogLevel::Warn,
            format!("can't open {name}: its name is not valid UTF-8"),
        );
    }

    pub(super) fn local_enter(&mut self) {
        let Some(entry) = self.local.entries.get(self.local.cursor) else {
            return;
        };
        if !entry.is_dir {
            return;
        }
        if entry.lossy_name {
            let name = entry.display_name.clone();
            self.report_unopenable(&name);
            return;
        }
        let mut path = std::path::PathBuf::from(&self.local.path);
        if entry.is_parent() {
            path.pop();
        } else {
            path.push(&entry.raw_name);
        }
        self.local.path = path.display().to_string();
        self.local.cursor = 0;
        self.refresh_local_pane();
    }

    pub(super) fn remote_enter(&mut self) {
        let Some(entry) = self.remote.entries.get(self.remote.cursor) else {
            return;
        };
        if entry.is_link {
            let name = entry.raw_name.clone();
            self.open_link(name, false);
            return;
        }
        if !entry.is_dir {
            return;
        }
        let new_path = if entry.is_parent() {
            transport::parent_remote(&self.remote.path)
        } else {
            match transport::join_remote(&self.remote.path, &entry.raw_name) {
                Some(p) => p,
                None => {
                    self.push_log(
                        LogLevel::Warn,
                        format!("cannot open `{}`: unusable name", entry.display_name),
                    );
                    return;
                }
            }
        };
        self.refresh_remote_pane(new_path);
    }

    /// Ask the server what the symlink `name` in the shown directory points
    /// at; the answer arrives as `AppEvent::LinkResolved`. A listing can't
    /// say, so a link is listed as not a folder — yet one to a folder should
    /// open like one, as downloading it does. With `download`, a link to
    /// anything else is downloaded, as a double-click on a file would be.
    pub(super) fn open_link(&mut self, name: String, download: bool) {
        let Some(t) = self.transport.clone() else {
            return;
        };
        let dir = self.remote.path.clone();
        let Some(path) = transport::join_remote(&dir, &name) else {
            self.push_log(
                LogLevel::Warn,
                format!(
                    "cannot open `{}`: unusable name",
                    crate::error::sanitize(name)
                ),
            );
            return;
        };
        let tx = self.app_event_tx.clone();
        let connection = self.connection;
        tokio::spawn(async move {
            // Where the server can't say (FTP), it is not taken for a folder.
            let is_dir = matches!(t.lock().await.metadata(&path).await, Ok(Some(e)) if e.is_dir());
            let _ = tx.send(AppEvent::LinkResolved {
                connection,
                dir,
                name,
                is_dir,
                download,
            });
        });
    }

    /// Kick off a remote `list` task. The result arrives as `AppEvent::Listed`.
    ///
    /// A request for the directory already being listed does not start a
    /// second listing: it marks the one in flight to run once more when it
    /// reports back (`finish_remote_listing`). However many requests arrive
    /// meanwhile, that is at most one listing running and one owed. A
    /// request for a different directory — navigation — always starts one,
    /// and drops whatever was owed to the directory being left.
    pub(super) fn refresh_remote_pane(&mut self, path: String) {
        let Some(t) = self.transport.clone() else {
            return;
        };
        if let Some(listing) = self.remote_listing.as_mut()
            && listing.path == path
        {
            listing.again = true;
            return;
        }
        self.remote_listing = Some(super::RemoteListing {
            path: path.clone(),
            again: false,
        });
        let path_changed = path != self.remote.path;
        // Reflect the new path immediately so the UI shows where we're going,
        // and so the stale-guard in handle_app_event can compare against it.
        self.remote.path = path.clone();
        if path_changed {
            // Navigation: the old directory's rows don't belong to the new
            // path, and acting on one would address the wrong file. Drop them
            // and start at the top.
            self.remote.entries.clear();
            self.remote.cursor = 0;
        }
        // An in-place refresh deliberately keeps the current rows until the
        // new listing arrives. Clearing first blanked the pane for however
        // long the listing took — and behind a recursive walk that had the
        // connection, that was the length of the whole walk.
        let tx = self.app_event_tx.clone();
        let connection = self.connection;

        tokio::spawn(async move {
            let mut transport = t.lock().await;
            let event = match transport.list(&path).await {
                Ok(listing) => AppEvent::Listed {
                    connection,
                    path,
                    entries: listing.entries,
                    unreadable: listing.unreadable,
                },
                Err(e) => AppEvent::ListFailed {
                    connection,
                    path,
                    error: e.to_string(),
                },
            };
            let _ = tx.send(event);
        });
    }

    /// A listing of `path` reported back, successfully or not. If it was the
    /// one in flight, clear it, and list again if a refresh was asked for
    /// while it ran. A result for any other path is a listing the user has
    /// since navigated away from; the one in flight is still running.
    pub(super) fn finish_remote_listing(&mut self, path: &str) {
        let Some(listing) = self.remote_listing.take_if(|l| l.path == path) else {
            return;
        };
        if listing.again {
            self.refresh_remote_pane(listing.path);
        }
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
        // A new directory shows just `..` at once, so the user sees the
        // navigation landed before the read_dir finishes. A refresh keeps
        // the rows, and so the cursor, until the new ones arrive.
        if path != self.local_listed {
            self.local
                .set_entries(parent_row(std::path::Path::new(&path)));
            self.local_listed = path.clone();
        }

        let tx = self.app_event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let event = match read_local_entries(std::path::Path::new(&path)) {
                Ok(entries) => AppEvent::LocalListed { path, entries },
                Err(e) => AppEvent::LocalListFailed {
                    path,
                    error: e.to_string(),
                },
            };
            let _ = tx.send(event);
        });
    }
}

/// The `..` row, for a directory that has a parent. The root has none, as
/// on the remote side.
fn parent_row(dir: &std::path::Path) -> Vec<PaneEntry> {
    if dir.parent().is_some() {
        vec![PaneEntry::parent()]
    } else {
        Vec::new()
    }
}

/// The local pane's rows for `dir`: the parent row, then directories, then
/// files, each group in name order.
fn read_local_entries(dir: &std::path::Path) -> std::io::Result<Vec<PaneEntry>> {
    let mut entries = parent_row(dir);
    for entry in std::fs::read_dir(dir)?.flatten() {
        // Through a symlink, so a link to a folder is a folder. A link
        // whose target is gone is still listed, as the link itself.
        let meta = match std::fs::metadata(entry.path()).or_else(|_| entry.metadata()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_dir = meta.is_dir();
        entries.push(PaneEntry::local(
            &entry.file_name(),
            is_dir,
            if is_dir { 0 } else { meta.len() },
        ));
    }
    // Directories first, then alpha within each group — ordered by what
    // the user reads, matching the remote pane.
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.display_name.cmp(&b.display_name),
    });
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::read_local_entries;

    /// `DirEntry::metadata` describes a symlink, not what it points at, so
    /// a link to a folder was listed as a file a few bytes long: it could
    /// not be entered, and uploading it tried to read a directory.
    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_folder_is_listed_as_a_folder() {
        let home = crate::paths::test_home();
        let target = home.path().join("real");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("inside.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(&target, home.path().join("link")).unwrap();

        let entries = read_local_entries(home.path()).unwrap();

        let link = entries.iter().find(|e| e.raw_name == "link").unwrap();
        assert!(link.is_dir);
    }

    /// A link to a file reports the file's size, not the link's.
    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_file_reports_the_files_size() {
        let home = crate::paths::test_home();
        std::fs::write(home.path().join("real.bin"), vec![0u8; 5000]).unwrap();
        std::os::unix::fs::symlink(home.path().join("real.bin"), home.path().join("link")).unwrap();

        let entries = read_local_entries(home.path()).unwrap();

        let link = entries.iter().find(|e| e.raw_name == "link").unwrap();
        assert_eq!((link.is_dir, link.size), (false, 5000));
    }

    /// A link whose target is gone still shows, so it can be seen and
    /// dealt with, as it did before links were followed.
    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_still_listed() {
        let home = crate::paths::test_home();
        std::os::unix::fs::symlink(home.path().join("gone"), home.path().join("link")).unwrap();

        let entries = read_local_entries(home.path()).unwrap();

        assert!(entries.iter().any(|e| e.raw_name == "link" && !e.is_dir));
    }

    #[cfg(unix)]
    #[test]
    fn the_root_has_no_parent_row() {
        let entries = super::read_local_entries(std::path::Path::new("/")).unwrap();
        assert!(
            entries.iter().all(|e| !e.is_parent()),
            "the remote pane shows none at /"
        );
    }
}
