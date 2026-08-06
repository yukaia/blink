//! User-action helpers: modal openers, form submitters, and the
//! one-shot async kickoffs they dispatch into.
//!
//! Each group follows the same shape:
//!
//!   - `open_*` — populate the relevant pending-modal state and switch
//!     `self.screen` to the matching modal.
//!   - `submit_*` — validate the form / input, surface errors via the
//!     modal's `error` field, and (on success) call into the async
//!     `start_*` kickoff.
//!   - `start_*` — spawn a tokio task that does the actual work and
//!     posts the result back as an `AppEvent::*`.
//!
//! Pulled out of `mod.rs` so the App lifecycle reads as setup + run
//! loop + helpers, without ~430 lines of "what happens when the user
//! submits THIS modal" inline.

use crate::session::Session;
use crate::transport;
use crate::tui::event::AppEvent;
use crate::tui::state::{EditSessionForm, OverwritePending, PendingDelete};

use super::{App, LogLevel, Pane, Screen};

/// Validate a name the user typed for a new or renamed remote entry,
/// returning the reason it is unusable.
///
/// Shared by the rename and mkdir modals so the two can't drift. They had:
/// `submit_mkdir` rejected `.` and `..`, `submit_rename` did not. A rename to
/// `..` therefore reached [`transport::join_remote`], which rejects the `..`
/// component by returning the base unchanged — so the app asked the server to
/// rename the file onto its own parent directory and surfaced whatever opaque
/// error came back, where the sibling modal gives a clear local one.
///
/// Control characters are refused too: the name goes onto the wire as a path
/// component, and a listing renders it back through `error::sanitize`, so one
/// containing an escape sequence or a bidi override would display as something
/// other than what was actually created.
fn remote_name_error(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("name cannot be empty");
    }
    if name.contains('/') || name.contains('\\') {
        return Some("name cannot contain path separators");
    }
    if name == "." || name == ".." {
        return Some("invalid name");
    }
    if name.chars().any(char::is_control) {
        return Some("name cannot contain control characters");
    }
    None
}

impl App {
    // -------------------------------------------------------------------
    // Edit / delete saved sessions
    // -------------------------------------------------------------------

    pub(super) fn open_edit_session(&mut self) {
        let Some(s) = self.sessions.get(self.session_cursor).cloned() else {
            return;
        };
        self.edit_session_form = Some(EditSessionForm::from_session(&s));
        self.screen = Screen::EditSession;
    }

    pub(super) fn submit_edit_session(&mut self) {
        // Validate. Pull the form into a local so we can borrow self elsewhere.
        let mut form = match self.edit_session_form.take() {
            Some(f) => f,
            None => {
                self.screen = Screen::SessionSelect;
                return;
            }
        };

        let name = form.name.trim().to_string();
        if name.is_empty() {
            form.error = Some("name cannot be empty".into());
            self.edit_session_form = Some(form);
            return;
        }
        let host = form.host.trim().to_string();
        if host.is_empty() {
            form.error = Some("host cannot be empty".into());
            self.edit_session_form = Some(form);
            return;
        }
        let port: u16 = match form.port.trim().parse() {
            Ok(p) if p >= 1 => p,
            _ => {
                form.error = Some("port must be a number 1–65535".into());
                self.edit_session_form = Some(form);
                return;
            }
        };
        // Renaming to a name another session already uses would silently
        // clobber the other one — block that explicitly.
        if name != form.original_name
            && self.sessions.iter().any(|s| s.name == name)
        {
            form.error = Some(format!("a session named `{name}` already exists"));
            self.edit_session_form = Some(form);
            return;
        }

        // Look up the underlying session so we keep protocol / auth / theme
        // overrides intact (those aren't editable here).
        let Some(original) = self
            .sessions
            .iter()
            .find(|s| s.name == form.original_name)
            .cloned()
        else {
            form.error = Some("original session not found".into());
            self.edit_session_form = Some(form);
            return;
        };

        let local_dir_str = form.local_dir.trim();
        let local_dir = if local_dir_str.is_empty() {
            None
        } else {
            Some(std::path::PathBuf::from(local_dir_str))
        };

        // Parallel transfers override. Empty -> None (use global default).
        // Otherwise must be 1..=MAX_PARALLEL; the spec caps at 10. Echo
        // bad input back without losing the rest of the form.
        let parallel_str = form.parallel.trim();
        let parallel_downloads: Option<u8> = if parallel_str.is_empty() {
            None
        } else {
            match parallel_str.parse::<u16>() {
                Ok(n) if n >= 1 && n <= u16::from(crate::config::MAX_PARALLEL) => {
                    Some(n as u8)
                }
                _ => {
                    form.error = Some(format!(
                        "parallel must be a number 1–{} (or empty for default)",
                        crate::config::MAX_PARALLEL
                    ));
                    self.edit_session_form = Some(form);
                    return;
                }
            }
        };

        // Preserve the pinned cert hash only when host, port, and the trust
        // bypass are all unchanged. A change to any of those means the
        // previous pin is no longer meaningful (different target or the
        // user is switching to normal CA verification), so we clear it and
        // let the next connect TOFU.
        let cert_sha256 = if form.accept_invalid_certs
            && original.host == host
            && original.port == port
        {
            original.cert_sha256.clone()
        } else {
            None
        };

        let updated = Session {
            name: name.clone(),
            protocol: original.protocol.clone(),
            host,
            port,
            username: form.username.trim().to_string(),
            remote_dir: if form.remote_dir.trim().is_empty() {
                "/".to_string()
            } else {
                form.remote_dir.trim().to_string()
            },
            local_dir,
            auth: original.auth.clone(),
            parallel_downloads,
            theme: original.theme.clone(),
            accept_invalid_certs: form.accept_invalid_certs,
            cert_sha256,
        };

        match updated.save() {
            Ok(()) => {
                // If the rename succeeded, drop the old `.ini` file. We do
                // this AFTER save so a save failure doesn't lose the original.
                if name != form.original_name
                    && let Err(e) = Session::delete(&form.original_name) {
                        // Soft-failure: the new session is saved, but the old
                        // file stayed behind. Surface as a warn rather than
                        // failing the whole edit.
                        self.push_log(
                            LogLevel::Warn,
                            format!(
                                "renamed session saved, but failed to remove old file: {e}"
                            ),
                        );
                    }
                self.reload_sessions();
                // Keep the cursor pointed at the freshly-edited session if
                // we can find it, otherwise clamp.
                self.session_cursor = self
                    .sessions
                    .iter()
                    .position(|s| s.name == name)
                    .unwrap_or_else(|| self.session_cursor.min(self.sessions.len().saturating_sub(1)));
                self.edit_session_form = None;
                self.screen = Screen::SessionSelect;
                self.push_log(LogLevel::Success, format!("session updated: {name}"));
            }
            Err(e) => {
                form.error = Some(e.to_string());
                self.edit_session_form = Some(form);
            }
        }
    }

    pub(super) fn open_delete_session(&mut self) {
        let Some(s) = self.sessions.get(self.session_cursor).cloned() else {
            return;
        };
        self.pending_session_delete = Some(s);
        self.screen = Screen::ConfirmDeleteSession;
    }

    // -------------------------------------------------------------------
    // Rename
    // -------------------------------------------------------------------

    pub(super) fn open_rename(&mut self) {
        if self.transport.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }
        let entry = match self.remote.entries.get(self.remote.cursor) {
            Some(e) if e.name != ".." => e.clone(),
            _ => return,
        };
        self.rename_input = entry.name.clone();
        self.rename_original = entry.name;
        self.rename_error = None;
        self.screen = Screen::Rename;
    }

    pub(super) fn submit_rename(&mut self) {
        let new_name = self.rename_input.trim().to_string();
        if let Some(reason) = remote_name_error(&new_name) {
            self.rename_error = Some(reason.into());
            return;
        }
        if new_name == self.rename_original {
            // No-op rename — just close.
            self.rename_input.clear();
            self.rename_original.clear();
            self.screen = Screen::Main;
            return;
        }

        let from = transport::join_remote(&self.remote.path, &self.rename_original);
        let to = transport::join_remote(&self.remote.path, &new_name);

        // Collision check against the cached pane listing. If the user navigated
        // here recently the cache is fresh enough; in the rare case it's stale,
        // the server will reject the rename and we'll surface the error.
        let collides = self
            .remote
            .entries
            .iter()
            .any(|e| e.name != ".." && e.name == new_name);
        if collides {
            self.pending_overwrite = Some(OverwritePending::Rename {
                from,
                to,
                target_name: new_name,
            });
            self.screen = Screen::ConfirmOverwrite;
            return;
        }

        self.rename_input.clear();
        self.rename_original.clear();
        self.screen = Screen::Main;
        self.start_rename(from, to);
    }

    pub(super) fn start_rename(&mut self, from: String, to: String) {
        let Some(t) = self.transport.clone() else {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        };
        let tx = self.app_event_tx.clone();
        let from_label = from
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or(&from)
            .to_string();
        self.push_log(LogLevel::Info, format!("renaming: {from_label}"));
        tokio::spawn(async move {
            let mut transport = t.lock().await;
            let event = match transport.rename(&from, &to).await {
                Ok(()) => AppEvent::Renamed { from, to },
                Err(e) => AppEvent::RenameFailed {
                    from,
                    to,
                    error: e.to_string(),
                },
            };
            let _ = tx.send(event);
        });
    }

    // -------------------------------------------------------------------
    // Mkdir
    // -------------------------------------------------------------------

    pub(super) fn open_mkdir(&mut self) {
        if self.transport.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }
        self.mkdir_input.clear();
        self.mkdir_error = None;
        self.screen = Screen::Mkdir;
    }

    pub(super) fn submit_mkdir(&mut self) {
        let name = self.mkdir_input.trim().to_string();
        if let Some(reason) = remote_name_error(&name) {
            self.mkdir_error = Some(reason.into());
            return;
        }
        let path = transport::join_remote(&self.remote.path, &name);
        self.mkdir_input.clear();
        self.mkdir_error = None;
        self.screen = Screen::Main;
        self.start_mkdir(path);
    }

    fn start_mkdir(&mut self, path: String) {
        let Some(t) = self.transport.clone() else {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        };
        let tx = self.app_event_tx.clone();
        let label = path
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or(&path)
            .to_string();
        self.push_log(LogLevel::Info, format!("mkdir: {label}"));
        tokio::spawn(async move {
            let mut transport = t.lock().await;
            let event = match transport.mkdir(&path).await {
                Ok(()) => AppEvent::MkdirDone { path },
                Err(e) => AppEvent::MkdirFailed {
                    path,
                    error: e.to_string(),
                },
            };
            let _ = tx.send(event);
        });
    }

    // -------------------------------------------------------------------
    // Delete
    // -------------------------------------------------------------------

    pub(super) fn open_delete(&mut self) {
        if self.transport.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }
        let entry = match self.remote.entries.get(self.remote.cursor) {
            Some(e) if e.name != ".." => e.clone(),
            _ => return,
        };
        let remote_path = transport::join_remote(&self.remote.path, &entry.name);
        self.pending_delete = Some(PendingDelete {
            name: entry.name,
            is_dir: entry.is_dir,
            remote_path,
        });
        self.screen = Screen::ConfirmDelete;
    }

    pub(super) fn start_delete(&mut self, name: String, remote_path: String, is_dir: bool) {
        let Some(t) = self.transport.clone() else {
            return;
        };
        let tx = self.app_event_tx.clone();
        let label = if is_dir { "deleting folder" } else { "deleting" };
        self.push_log(LogLevel::Info, format!("{label}: {name}"));
        tokio::spawn(async move {
            let mut transport = t.lock().await;
            let result = if is_dir {
                // Recursive: the user confirmed via the modal. Empty-only
                // deletion would leave them stuck on a non-empty folder
                // with no obvious next step.
                transport.delete_dir(&remote_path, true).await
            } else {
                transport.delete_file(&remote_path).await
            };
            let event = match result {
                Ok(()) => AppEvent::Deleted { name },
                Err(e) => AppEvent::DeleteFailed {
                    name,
                    error: e.to_string(),
                },
            };
            let _ = tx.send(event);
        });
    }

    // -------------------------------------------------------------------
    // Search (substring filter on Local or Remote)
    // -------------------------------------------------------------------

    pub(super) fn open_search(&mut self) {
        self.search_target = self.active_pane;
        // Pre-populate with the existing filter so re-opening shows what's
        // currently applied — easier to refine than to retype.
        let existing = match self.search_target {
            Pane::Local => self.local.filter.clone(),
            Pane::Remote => self.remote.filter.clone(),
            _ => None,
        };
        self.search_input = existing.unwrap_or_default();
        self.screen = Screen::Search;
    }

    pub(super) fn apply_search_filter(&mut self) {
        let q = self.search_input.clone();
        match self.search_target {
            Pane::Local => self.local.set_filter(q),
            Pane::Remote => self.remote.set_filter(q),
            _ => {}
        }
    }

    pub(super) fn move_search_cursor(&mut self, delta: isize) {
        match self.search_target {
            Pane::Local => self.local.move_cursor(delta),
            Pane::Remote => self.remote.move_cursor(delta),
            _ => {}
        }
    }

    // -------------------------------------------------------------------
    // Save current session
    // -------------------------------------------------------------------

    pub(super) fn open_save_session(&mut self) {
        if self.current_session.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }
        let default_name = self
            .current_session
            .as_ref()
            .map(|s| {
                if s.name.is_empty() {
                    s.host.clone()
                } else {
                    s.name.clone()
                }
            })
            .unwrap_or_default();
        self.save_session_input = default_name;
        self.save_session_error = None;
        self.screen = Screen::SaveSession;
    }
}

#[cfg(test)]
mod tests {
    use super::remote_name_error;

    #[test]
    fn accepts_ordinary_names() {
        for name in ["report.txt", "My Folder", "2024-01-01T00:00:00.log", "café"] {
            assert_eq!(remote_name_error(name), None, "{name} should be allowed");
        }
    }

    #[test]
    fn rejects_dot_and_dotdot() {
        // The gap this closes: `submit_rename` accepted these, so `..` reached
        // join_remote, which drops the component and returns the base — the
        // app then asked the server to rename a file onto its own parent.
        assert!(remote_name_error(".").is_some());
        assert!(remote_name_error("..").is_some());
    }

    #[test]
    fn allows_names_that_merely_start_with_dots() {
        assert_eq!(remote_name_error(".env"), None);
        assert_eq!(remote_name_error("..config"), None);
    }

    #[test]
    fn rejects_empty_and_separators() {
        assert!(remote_name_error("").is_some());
        assert!(remote_name_error("a/b").is_some());
        assert!(remote_name_error("a\\b").is_some());
    }

    #[test]
    fn rejects_control_characters() {
        // These would go onto the wire verbatim but render back through
        // `error::sanitize`, so the listing would disagree with reality.
        assert!(remote_name_error("a\u{1b}[31mb").is_some());
        assert!(remote_name_error("a\0b").is_some());
        assert!(remote_name_error("a\nb").is_some());
    }
}
