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
/// Control characters are refused too, and the invisible and reordering
/// characters with them: the name goes onto the wire as a path component,
/// and a listing renders it back through `error::sanitize`, so one
/// containing an escape sequence or a bidi override would display as
/// something other than what was actually created.
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
    // Whatever a listing would show as something else: control characters,
    // and the invisible and reordering ones `error::sanitize` replaces.
    if crate::error::sanitize_display(name) != name {
        return Some("name cannot contain control or invisible characters");
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
        if name != form.original_name && self.sessions.iter().any(|s| s.name == name) {
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
                Ok(n) if n >= 1 && n <= u16::from(crate::config::MAX_PARALLEL) => Some(n as u8),
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
        let cert_sha256 =
            if form.accept_invalid_certs && original.host == host && original.port == port {
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
                    && let Err(e) = Session::delete(&form.original_name)
                {
                    // Soft-failure: the new session is saved, but the old
                    // file stayed behind. Surface as a warn rather than
                    // failing the whole edit.
                    self.push_log(
                        LogLevel::Warn,
                        format!("renamed session saved, but failed to remove old file: {e}"),
                    );
                }
                self.reload_sessions();
                // Keep the cursor pointed at the freshly-edited session if
                // we can find it, otherwise clamp.
                self.session_cursor = self
                    .sessions
                    .iter()
                    .position(|s| s.name == name)
                    .unwrap_or_else(|| {
                        self.session_cursor
                            .min(self.sessions.len().saturating_sub(1))
                    });
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
            Some(e) if !e.is_parent() => e.clone(),
            _ => return,
        };
        // The form is rendered, so it gets the sanitized name; the wire
        // source is kept beside it. A name carrying control characters
        // therefore shows up readable and can only be renamed *to* a clean
        // name — `remote_name_error` refuses to send those characters back
        // out — which is a reasonable way to fix such a file rather than a
        // limitation worth working around.
        self.rename_input = entry.display_name.clone();
        self.rename_original = entry.display_name;
        self.rename_source = entry.raw_name;
        self.rename_error = None;
        self.screen = Screen::Rename;
    }

    pub(super) fn submit_rename(&mut self) {
        let new_name = self.rename_input.trim().to_string();
        if let Some(reason) = remote_name_error(&new_name) {
            self.rename_error = Some(reason.into());
            return;
        }
        // Compare against the wire name: for an ordinary file the two are
        // identical, and for one carrying control characters the user cannot
        // type it back, so any submit there is a genuine rename.
        if new_name == self.rename_source {
            // No-op rename — just close.
            self.clear_rename_form();
            self.screen = Screen::Main;
            return;
        }

        // `remote_name_error` already rejected `.`/`..` and separators for
        // the typed name, and the source came from a listing entry we
        // navigated to — but join is the authority on what is joinable, so
        // ask it rather than assuming the two agree.
        let (Some(from), Some(to)) = (
            transport::join_remote(&self.remote.path, &self.rename_source),
            transport::join_remote(&self.remote.path, &new_name),
        ) else {
            self.rename_error = Some("that name cannot be used here".into());
            return;
        };

        // Collision check against the cached pane listing. If the user navigated
        // here recently the cache is fresh enough; in the rare case it's stale,
        // the server will reject the rename and we'll surface the error.
        let collides = self
            .remote
            .entries
            .iter()
            .any(|e| !e.is_parent() && e.raw_name == new_name);
        if collides {
            self.pending_overwrite = Some(OverwritePending::Rename {
                from,
                to,
                target_name: new_name,
            });
            self.screen = Screen::ConfirmOverwrite;
            return;
        }

        self.clear_rename_form();
        self.screen = Screen::Main;
        self.start_rename(from, to, false);
    }

    /// Reset every field of the rename form together, so a new field can't
    /// be left behind when one of the several exit paths clears the others.
    pub(super) fn clear_rename_form(&mut self) {
        self.rename_input.clear();
        self.rename_original.clear();
        self.rename_source.clear();
        self.rename_error = None;
    }

    /// Rename `from` to `to`. `overwrite` is the user's confirmation that an
    /// existing `to` should be replaced; without it, a target that turned up
    /// since the listing makes the rename fail rather than vanish.
    pub(super) fn start_rename(&mut self, from: String, to: String, overwrite: bool) {
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
            let renamed = if overwrite {
                transport.rename_over(&from, &to).await
            } else {
                transport.rename(&from, &to).await
            };
            let event = match renamed {
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
        let Some(path) = transport::join_remote(&self.remote.path, &name) else {
            self.mkdir_error = Some("that name cannot be used here".into());
            return;
        };
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
            Some(e) if !e.is_parent() => e.clone(),
            _ => return,
        };
        // Delete addresses the real name; the modal shows the readable one.
        // Conflating them is how a confirmation for one file ends up
        // unlinking another whose name merely renders the same.
        let Some(remote_path) = transport::join_remote(&self.remote.path, &entry.raw_name) else {
            self.push_log(
                LogLevel::Warn,
                format!("cannot delete `{}`: unusable name", entry.display_name),
            );
            return;
        };
        self.pending_delete = Some(PendingDelete {
            name: entry.display_name,
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
        let label = if is_dir {
            "deleting folder"
        } else {
            "deleting"
        };
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
    use std::path::PathBuf;
    use std::sync::Arc;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::Mutex;

    use super::remote_name_error;
    use crate::config::Config;
    use crate::paths;
    use crate::session::{AuthMethod, Protocol, Session};
    use crate::theme::Theme;
    use crate::transport::Transport;
    use crate::transport::mock::MockTransport;
    use crate::tui::app::{App, Pane, Screen};
    use crate::tui::event::AppEvent;
    use crate::tui::state::{EditField, PaneEntry};

    fn app() -> App {
        App::new(Config::default(), Theme::load("dracula").unwrap())
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn logged(a: &App, needle: &str) -> bool {
        a.log.iter().any(|l| l.message.contains(needle))
    }

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

    /// Not control characters, but replaced in a listing all the same: a
    /// name holding one would be shown as another name than was created.
    #[test]
    fn rejects_invisible_and_reordering_characters() {
        assert!(remote_name_error("report\u{202e}gnp.exe").is_some());
        assert!(remote_name_error("a\u{200b}b").is_some());
    }

    // -- editing a saved session --------------------------------------------

    /// An FTPS session with everything the edit form can't change set to
    /// something other than its default, and a certificate pinned.
    fn ftps_session() -> Session {
        Session {
            name: "files".to_string(),
            protocol: Protocol::Ftps,
            host: "ftp.example".to_string(),
            port: 2121,
            username: "me".to_string(),
            remote_dir: "/pub".to_string(),
            local_dir: None,
            auth: AuthMethod::Password,
            parallel_downloads: Some(2),
            theme: Some("nord".to_string()),
            accept_invalid_certs: true,
            cert_sha256: Some("ab".repeat(32)),
        }
    }

    /// An app on the session selector with `sessions` saved and listed,
    /// and the edit form open on the first of them by name.
    fn editing(sessions: &[Session]) -> (App, paths::TestHome) {
        let home = paths::test_home();
        for s in sessions {
            s.save().unwrap();
        }
        let mut a = app();
        a.sessions = Session::list_all().unwrap();
        a.screen = Screen::SessionSelect;
        a.session_cursor = 0;
        a.open_edit_session();
        (a, home)
    }

    fn on_disk(name: &str) -> Session {
        Session::list_all()
            .unwrap()
            .into_iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no saved session named {name}"))
    }

    fn form(a: &mut App) -> &mut crate::tui::state::EditSessionForm {
        a.edit_session_form.as_mut().expect("the form is open")
    }

    #[test]
    fn opening_the_edit_form_fills_it_from_the_session() {
        let (mut a, _home) = editing(&[ftps_session()]);

        assert_eq!(a.screen, Screen::EditSession);
        let f = form(&mut a);
        assert_eq!(
            (
                f.name.as_str(),
                f.host.as_str(),
                f.port.as_str(),
                f.username.as_str()
            ),
            ("files", "ftp.example", "2121", "me"),
        );
        assert_eq!((f.remote_dir.as_str(), f.local_dir.as_str()), ("/pub", ""));
        assert_eq!(f.parallel, "2");
        assert!(f.accept_invalid_certs);
        assert_eq!(f.original_name, "files");
    }

    #[test]
    fn opening_the_edit_form_with_no_sessions_does_nothing() {
        let _home = paths::test_home();
        let mut a = app();
        a.sessions.clear();
        a.screen = Screen::SessionSelect;

        a.open_edit_session();

        assert_eq!(a.screen, Screen::SessionSelect);
        assert!(a.edit_session_form.is_none());
    }

    #[test]
    fn submitting_the_form_saves_what_was_typed() {
        let (mut a, _home) = editing(&[ftps_session()]);
        let f = form(&mut a);
        f.username = "  you  ".into();
        f.remote_dir = "/srv".into();
        f.local_dir = "/tmp".into();
        f.parallel = "5".into();

        a.submit_edit_session();

        assert_eq!(a.screen, Screen::SessionSelect);
        assert!(a.edit_session_form.is_none());
        assert!(logged(&a, "session updated: files"));
        let s = on_disk("files");
        assert_eq!(s.username, "you", "trimmed");
        assert_eq!(s.remote_dir, "/srv");
        assert_eq!(s.local_dir, Some(PathBuf::from("/tmp")));
        assert_eq!(s.parallel_downloads, Some(5));
        assert_eq!(a.sessions.len(), 1, "the list is reloaded");
        assert_eq!(a.sessions[0].username, "you");
    }

    /// The form doesn't show the protocol, the auth method or the theme,
    /// so an edit must not reset them.
    #[test]
    fn what_the_form_cant_change_is_kept() {
        let mut s = Session::from_url("sftp://me@host.example").unwrap();
        s.auth = AuthMethod::Key {
            path: PathBuf::from("/home/me/.ssh/id"),
        };
        s.theme = Some("nord".into());
        let (mut a, _home) = editing(&[s.clone()]);
        form(&mut a).username = "you".into();

        a.submit_edit_session();

        let saved = on_disk(&s.name);
        assert_eq!(saved.protocol, Protocol::Sftp);
        assert_eq!(saved.auth, s.auth);
        assert_eq!(saved.theme, s.theme);
    }

    #[test]
    fn empty_optional_fields_mean_the_defaults() {
        let (mut a, _home) = editing(&[ftps_session()]);
        let f = form(&mut a);
        f.remote_dir = "  ".into();
        f.local_dir = "".into();
        f.parallel = "".into();

        a.submit_edit_session();

        let s = on_disk("files");
        assert_eq!(s.remote_dir, "/");
        assert_eq!(s.local_dir, None);
        assert_eq!(s.parallel_downloads, None);
    }

    /// Each refused: the form stays open with the reason, what was typed
    /// is kept, and the file is as it was.
    #[test]
    fn a_form_that_cant_be_saved_stays_open_with_the_reason() {
        type Break = fn(&mut crate::tui::state::EditSessionForm);
        let cases: [(&str, Break); 9] = [
            ("name cannot be empty", |f| f.name = "  ".into()),
            ("host cannot be empty", |f| f.host = "".into()),
            ("port must be", |f| f.port = "0".into()),
            ("port must be", |f| f.port = "70000".into()),
            ("port must be", |f| f.port = "ssh".into()),
            ("parallel must be", |f| f.parallel = "0".into()),
            ("parallel must be", |f| f.parallel = "99".into()),
            ("parallel must be", |f| f.parallel = "many".into()),
            ("local_dir must be", |f| f.local_dir = "downloads".into()),
        ];
        for (reason, break_it) in cases {
            let (mut a, _home) = editing(&[ftps_session()]);
            form(&mut a).username = "typed".into();
            break_it(form(&mut a));

            a.submit_edit_session();

            assert_eq!(a.screen, Screen::EditSession, "{reason}");
            let f = form(&mut a);
            let error = f.error.clone().unwrap_or_default();
            assert!(error.contains(reason), "{reason}: got {error:?}");
            assert_eq!(f.username, "typed", "{reason}");
            assert_eq!(on_disk("files").username, "me", "{reason}");
        }
    }

    #[test]
    fn renaming_a_session_moves_it_rather_than_copying_it() {
        let (mut a, _home) = editing(&[ftps_session()]);
        form(&mut a).name = "archive".into();

        a.submit_edit_session();

        let names: Vec<_> = Session::list_all()
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["archive"]);
        assert_eq!(a.sessions[a.session_cursor].name, "archive");
    }

    /// Saving under a name another session has would write over it.
    #[test]
    fn renaming_onto_another_sessions_name_is_refused() {
        let other = Session::from_url("sftp://me@other.example").unwrap();
        let mut first = ftps_session();
        first.name = "a-first".into();
        let (mut a, _home) = editing(&[first, other.clone()]);
        assert_eq!(form(&mut a).original_name, "a-first");
        form(&mut a).name = other.name.clone();

        a.submit_edit_session();

        assert_eq!(a.screen, Screen::EditSession);
        assert!(
            form(&mut a)
                .error
                .as_deref()
                .unwrap()
                .contains("already exists")
        );
        assert_eq!(on_disk(&other.name).host, "other.example");
        assert_eq!(on_disk("a-first").host, "ftp.example");
    }

    // The FTPS pin is a statement about one server's certificate. It stays
    // with the session only while the session still names that server and
    // still pins.

    fn pin_after(change: fn(&mut crate::tui::state::EditSessionForm)) -> Option<String> {
        let (mut a, _home) = editing(&[ftps_session()]);
        change(form(&mut a));
        a.submit_edit_session();
        assert_eq!(a.screen, Screen::SessionSelect, "the edit saves");
        on_disk("files").cert_sha256
    }

    #[test]
    fn an_edit_that_keeps_the_server_keeps_the_certificate_pin() {
        assert_eq!(
            pin_after(|f| f.username = "you".into()),
            Some("ab".repeat(32))
        );
    }

    #[test]
    fn changing_the_host_drops_the_certificate_pin() {
        assert_eq!(pin_after(|f| f.host = "elsewhere.example".into()), None);
    }

    #[test]
    fn changing_the_port_drops_the_certificate_pin() {
        assert_eq!(pin_after(|f| f.port = "21".into()), None);
    }

    #[test]
    fn turning_chain_validation_back_on_drops_the_certificate_pin() {
        assert_eq!(pin_after(|f| f.accept_invalid_certs = false), None);
    }

    #[test]
    fn escape_leaves_the_edit_form_without_saving() {
        let (mut a, _home) = editing(&[ftps_session()]);
        form(&mut a).username = "you".into();

        a.handle_edit_session(press(KeyCode::Esc));

        assert_eq!(a.screen, Screen::SessionSelect);
        assert!(a.edit_session_form.is_none());
        assert_eq!(on_disk("files").username, "me");
    }

    #[test]
    fn tab_and_shift_tab_walk_the_fields_and_wrap() {
        let (mut a, _home) = editing(&[ftps_session()]);
        assert_eq!(form(&mut a).focused, EditField::Name);

        a.handle_edit_session(press(KeyCode::Tab));
        assert_eq!(form(&mut a).focused, EditField::Host);
        a.handle_edit_session(press(KeyCode::BackTab));
        a.handle_edit_session(press(KeyCode::Up));
        assert_eq!(
            form(&mut a).focused,
            EditField::AcceptInvalidCerts,
            "wraps back"
        );
        a.handle_edit_session(press(KeyCode::Down));
        assert_eq!(form(&mut a).focused, EditField::Name, "and forward");
    }

    #[test]
    fn typing_edits_the_focused_field_and_clears_the_error() {
        let (mut a, _home) = editing(&[ftps_session()]);
        let f = form(&mut a);
        f.focused = EditField::Host;
        f.error = Some("old".into());

        a.handle_edit_session(press(KeyCode::Backspace));
        a.handle_edit_session(press(KeyCode::Char(' ')));
        a.handle_edit_session(press(KeyCode::Char('x')));

        let f = form(&mut a);
        assert_eq!(f.host, "ftp.exampl x");
        assert_eq!(f.name, "files", "no other field");
        assert_eq!(f.error, None);
    }

    #[test]
    fn space_toggles_the_certificate_checkbox() {
        let (mut a, _home) = editing(&[ftps_session()]);
        form(&mut a).focused = EditField::AcceptInvalidCerts;

        a.handle_edit_session(press(KeyCode::Char(' ')));
        assert!(!form(&mut a).accept_invalid_certs);
        a.handle_edit_session(press(KeyCode::Char('x')));
        assert!(
            !form(&mut a).accept_invalid_certs,
            "typing does nothing there"
        );
        a.handle_edit_session(press(KeyCode::Char(' ')));
        assert!(form(&mut a).accept_invalid_certs);
    }

    #[test]
    fn enter_submits_the_edit_form() {
        let (mut a, _home) = editing(&[ftps_session()]);
        form(&mut a).username = "you".into();

        a.handle_edit_session(press(KeyCode::Enter));

        assert_eq!(on_disk("files").username, "you");
    }

    // -- deleting a saved session ---------------------------------------------

    /// An app on the selector listing two saved sessions, `a` and `b`, with
    /// the delete question open on `b`.
    fn deleting() -> (App, paths::TestHome) {
        let home = paths::test_home();
        for host in ["a", "b"] {
            Session::from_url(&format!("sftp://me@{host}"))
                .unwrap()
                .save()
                .unwrap();
        }
        let mut a = app();
        a.sessions = Session::list_all().unwrap();
        a.screen = Screen::SessionSelect;
        a.session_cursor = 1;
        a.open_delete_session();
        (a, home)
    }

    #[test]
    fn deleting_a_session_asks_first() {
        let (a, _home) = deleting();

        assert_eq!(a.screen, Screen::ConfirmDeleteSession);
        assert_eq!(a.pending_session_delete.as_ref().unwrap().name, "b");
        assert_eq!(Session::list_all().unwrap().len(), 2, "nothing is gone yet");
    }

    #[test]
    fn confirming_deletes_the_session_and_keeps_the_cursor_on_the_list() {
        let (mut a, _home) = deleting();

        a.handle_confirm_delete_session(press(KeyCode::Char('y')));

        assert_eq!(a.screen, Screen::SessionSelect);
        assert!(logged(&a, "session deleted: b"));
        let names: Vec<_> = a.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a"]);
        assert_eq!(Session::list_all().unwrap().len(), 1);
        assert_eq!(a.session_cursor, 0, "it was on the last row, now gone");
        assert!(a.pending_session_delete.is_none());
    }

    #[test]
    fn declining_keeps_the_session() {
        for key in [KeyCode::Char('n'), KeyCode::Esc] {
            let (mut a, _home) = deleting();

            a.handle_confirm_delete_session(press(key));

            assert_eq!(a.screen, Screen::SessionSelect);
            assert!(a.pending_session_delete.is_none());
            assert_eq!(Session::list_all().unwrap().len(), 2);
        }
    }

    #[test]
    fn the_delete_question_ignores_other_keys() {
        let (mut a, _home) = deleting();

        a.handle_confirm_delete_session(press(KeyCode::Enter));

        assert_eq!(a.screen, Screen::ConfirmDeleteSession);
        assert_eq!(Session::list_all().unwrap().len(), 2);
    }

    /// The file went some other way while the question was open.
    #[test]
    fn a_session_already_gone_is_reported_not_crashed_on() {
        let (mut a, _home) = deleting();
        Session::delete("b").unwrap();

        a.handle_confirm_delete_session(press(KeyCode::Char('y')));

        assert_eq!(a.screen, Screen::SessionSelect);
        assert!(logged(&a, "delete b failed"));
    }

    // -- remote mkdir and delete ----------------------------------------------

    /// What the app is connected to in these tests, and where its
    /// background tasks report.
    struct Remote {
        server: MockTransport,
        events: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    }

    impl Remote {
        /// The next event a background task sends, handed to the app.
        async fn deliver(&mut self, a: &mut App) {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), self.events.recv())
                .await
                .expect("the task reports back")
                .expect("the event channel is open");
            a.handle_app_event(ev);
        }

        async fn has(&self, path: &str) -> bool {
            self.server.clone().metadata(path).await.unwrap().is_some()
        }
    }

    /// An app on the main screen browsing `/srv` on a server that holds
    /// `/srv/old.txt` and the folder `/srv/logs` with a file in it.
    fn connected() -> (App, Remote) {
        let server = MockTransport::new()
            .with_file("/srv/old.txt", b"old")
            .with_file("/srv/logs/today.log", b"log");
        let mut a = app();
        a.transport = Some(Arc::new(Mutex::new(
            Box::new(server.clone()) as Box<dyn Transport>
        )));
        a.screen = Screen::Main;
        a.active_pane = Pane::Remote;
        a.remote.path = "/srv".into();
        a.remote.set_entries(vec![
            PaneEntry::parent(),
            PaneEntry::new("logs".into(), true, 0),
            PaneEntry::new("old.txt".into(), false, 3),
        ]);
        let events = a.app_event_rx.take().unwrap();
        (a, Remote { server, events })
    }

    #[test]
    fn mkdir_and_delete_need_a_connection() {
        let mut a = app();
        a.screen = Screen::Main;

        a.open_mkdir();
        a.open_delete();

        assert_eq!(a.screen, Screen::Main);
        assert!(logged(&a, "not connected"));
    }

    #[tokio::test]
    async fn opening_the_new_folder_prompt_starts_it_empty() {
        let (mut a, _remote) = connected();
        a.mkdir_input = "left over".into();
        a.mkdir_error = Some("left over".into());

        a.open_mkdir();

        assert_eq!(a.screen, Screen::Mkdir);
        assert_eq!(a.mkdir_input, "");
        assert_eq!(a.mkdir_error, None);
    }

    #[tokio::test]
    async fn a_new_folder_is_made_in_the_folder_on_screen() {
        let (mut a, mut remote) = connected();
        a.open_mkdir();
        for c in " new ".chars() {
            a.handle_mkdir(press(KeyCode::Char(c)));
        }

        a.handle_mkdir(press(KeyCode::Enter));
        assert_eq!(a.screen, Screen::Main);
        assert_eq!(a.mkdir_input, "");
        remote.deliver(&mut a).await;

        assert!(remote.has("/srv/new").await, "trimmed, and under /srv");
        assert!(logged(&a, "created: new"));
    }

    #[tokio::test]
    async fn a_folder_name_that_cant_be_used_is_refused_in_the_prompt() {
        let (mut a, mut remote) = connected();
        a.open_mkdir();
        for name in ["", "..", "a/b"] {
            a.mkdir_input = name.into();

            a.handle_mkdir(press(KeyCode::Enter));

            assert_eq!(a.screen, Screen::Mkdir, "{name:?}");
            assert!(a.mkdir_error.is_some(), "{name:?}");
        }
        assert!(
            remote.events.try_recv().is_err(),
            "nothing was asked of the server"
        );

        a.handle_mkdir(press(KeyCode::Char('c')));
        assert_eq!(a.mkdir_error, None, "typing clears the complaint");
    }

    #[tokio::test]
    async fn escape_leaves_the_new_folder_prompt() {
        let (mut a, remote) = connected();
        a.open_mkdir();
        a.mkdir_input = "new".into();

        a.handle_mkdir(press(KeyCode::Esc));

        assert_eq!(a.screen, Screen::Main);
        assert_eq!(a.mkdir_input, "");
        assert!(!remote.has("/srv/new").await);
    }

    #[tokio::test]
    async fn a_failed_mkdir_is_logged_by_name() {
        let (mut a, _remote) = connected();

        a.handle_app_event(AppEvent::MkdirFailed {
            path: "/srv/new".into(),
            error: "permission denied".into(),
        });

        assert!(logged(&a, "mkdir new failed: permission denied"));
    }

    #[tokio::test]
    async fn deleting_asks_first_naming_the_entry_under_the_cursor() {
        let (mut a, remote) = connected();
        a.remote.cursor = 2;

        a.open_delete();

        assert_eq!(a.screen, Screen::ConfirmDelete);
        let pd = a.pending_delete.as_ref().unwrap();
        assert_eq!(
            (pd.name.as_str(), pd.remote_path.as_str(), pd.is_dir),
            ("old.txt", "/srv/old.txt", false),
        );
        assert!(remote.has("/srv/old.txt").await, "nothing is gone yet");
    }

    /// The parent row is a way up, not a thing to delete.
    #[tokio::test]
    async fn the_parent_row_cant_be_deleted() {
        let (mut a, _remote) = connected();
        a.remote.cursor = 0;

        a.open_delete();

        assert_eq!(a.screen, Screen::Main);
        assert!(a.pending_delete.is_none());
        assert!(a.log.is_empty(), "and it isn't an error to have tried");
    }

    /// The question shows a readable name; the delete addresses the bytes
    /// the server listed.
    #[tokio::test]
    async fn a_name_with_control_characters_is_shown_clean_and_addressed_raw() {
        let (mut a, _remote) = connected();
        a.remote
            .set_entries(vec![PaneEntry::new("a\u{1b}b".into(), false, 1)]);
        a.remote.cursor = 0;

        a.open_delete();

        let pd = a.pending_delete.as_ref().unwrap();
        assert_eq!(pd.name, "a b");
        assert_eq!(pd.remote_path, "/srv/a\u{1b}b");
    }

    #[tokio::test]
    async fn a_name_that_cant_be_joined_isnt_deleted() {
        let (mut a, _remote) = connected();
        a.remote
            .set_entries(vec![PaneEntry::new(".".into(), false, 1)]);
        a.remote.cursor = 0;

        a.open_delete();

        assert_eq!(a.screen, Screen::Main);
        assert!(a.pending_delete.is_none());
        assert!(logged(&a, "cannot delete"));
    }

    #[tokio::test]
    async fn confirming_deletes_the_file() {
        let (mut a, mut remote) = connected();
        a.remote.cursor = 2;
        a.open_delete();

        a.handle_confirm_delete(press(KeyCode::Char('y')));
        assert_eq!(a.screen, Screen::Main);
        assert!(a.pending_delete.is_none());
        remote.deliver(&mut a).await;

        assert!(!remote.has("/srv/old.txt").await);
        assert!(remote.has("/srv/logs/today.log").await, "and nothing else");
        assert!(logged(&a, "deleted: old.txt"));
    }

    #[tokio::test]
    async fn confirming_deletes_a_folder_with_what_is_in_it() {
        let (mut a, mut remote) = connected();
        a.remote.cursor = 1;
        a.open_delete();
        assert!(a.pending_delete.as_ref().unwrap().is_dir);

        a.handle_confirm_delete(press(KeyCode::Char('Y')));
        remote.deliver(&mut a).await;

        assert!(!remote.has("/srv/logs").await);
        assert!(!remote.has("/srv/logs/today.log").await);
        assert!(remote.has("/srv/old.txt").await);
        assert!(logged(&a, "deleting folder: logs"));
    }

    #[tokio::test]
    async fn declining_deletes_nothing() {
        for key in [KeyCode::Char('n'), KeyCode::Esc] {
            let (mut a, mut remote) = connected();
            a.remote.cursor = 2;
            a.open_delete();

            a.handle_confirm_delete(press(key));

            assert_eq!(a.screen, Screen::Main);
            assert!(a.pending_delete.is_none());
            assert!(remote.events.try_recv().is_err());
            assert!(remote.has("/srv/old.txt").await);
        }
    }

    #[tokio::test]
    async fn the_delete_question_ignores_keys_it_doesnt_list() {
        let (mut a, remote) = connected();
        a.remote.cursor = 2;
        a.open_delete();

        a.handle_confirm_delete(press(KeyCode::Enter));

        assert_eq!(a.screen, Screen::ConfirmDelete);
        assert!(remote.has("/srv/old.txt").await);
    }

    #[tokio::test]
    async fn a_failed_delete_is_logged_by_name() {
        let (mut a, _remote) = connected();

        a.handle_app_event(AppEvent::DeleteFailed {
            name: "old.txt".into(),
            error: "permission denied".into(),
        });

        assert!(logged(&a, "delete old.txt failed: permission denied"));
    }

    // -- search -----------------------------------------------------------

    /// An app on the main screen with three files and the parent row in
    /// each of the local and remote panes.
    fn browsing() -> App {
        let mut a = app();
        a.screen = Screen::Main;
        let rows = || {
            vec![
                PaneEntry::parent(),
                PaneEntry::new("Report.txt".into(), false, 1),
                PaneEntry::new("notes.md".into(), false, 1),
                PaneEntry::new("report-old.txt".into(), false, 1),
            ]
        };
        a.local.set_entries(rows());
        a.remote.set_entries(rows());
        a
    }

    fn names(pane: &crate::tui::state::PaneState) -> Vec<&str> {
        pane.entries.iter().map(|e| e.raw_name.as_str()).collect()
    }

    fn type_into_search(a: &mut App, text: &str) {
        for c in text.chars() {
            a.handle_search(press(KeyCode::Char(c)));
        }
    }

    #[test]
    fn search_filters_the_pane_it_was_opened_on_as_you_type() {
        let mut a = browsing();
        a.active_pane = Pane::Remote;

        a.open_search();
        assert_eq!(a.screen, Screen::Search);
        type_into_search(&mut a, "REP");

        assert_eq!(names(&a.remote), ["..", "Report.txt", "report-old.txt"]);
        assert_eq!(a.local.entries.len(), 4, "the other pane is left alone");

        a.handle_search(press(KeyCode::Backspace));
        a.handle_search(press(KeyCode::Backspace));
        a.handle_search(press(KeyCode::Backspace));
        assert_eq!(
            a.remote.entries.len(),
            4,
            "an empty search shows everything"
        );
    }

    #[test]
    fn enter_keeps_the_filter_and_escape_drops_it() {
        let mut a = browsing();
        a.active_pane = Pane::Local;
        a.open_search();
        type_into_search(&mut a, "notes");

        a.handle_search(press(KeyCode::Enter));
        assert_eq!(a.screen, Screen::Main);
        assert_eq!(names(&a.local), ["..", "notes.md"]);

        a.open_search();
        assert_eq!(a.search_input, "notes", "reopening shows what is applied");
        a.handle_search(press(KeyCode::Esc));
        assert_eq!(a.screen, Screen::Main);
        assert_eq!(a.local.entries.len(), 4);
        assert_eq!(a.search_input, "");
    }

    #[test]
    fn the_arrow_keys_move_through_the_matches() {
        let mut a = browsing();
        a.active_pane = Pane::Remote;
        a.open_search();
        type_into_search(&mut a, "rep");
        let start = a.remote.cursor;

        a.handle_search(press(KeyCode::Down));
        assert_eq!(a.remote.cursor, start + 1);
        a.handle_search(press(KeyCode::PageDown));
        assert_eq!(a.remote.cursor, 2, "stops at the last match");
        a.handle_search(press(KeyCode::Up));
        assert_eq!(a.remote.cursor, 1);
        a.handle_search(press(KeyCode::PageUp));
        assert_eq!(a.remote.cursor, 0);
        assert_eq!(a.local.cursor, 0, "the other pane's cursor stays");
    }

    /// `/` is only bound on the file panes, but the search itself must not
    /// touch either when it is somehow opened elsewhere.
    #[test]
    fn a_search_opened_off_the_file_panes_filters_nothing() {
        let mut a = browsing();
        a.active_pane = Pane::Log;

        a.open_search();
        type_into_search(&mut a, "rep");
        a.handle_search(press(KeyCode::Down));

        assert_eq!(a.search_input, "rep");
        assert_eq!((a.local.entries.len(), a.remote.entries.len()), (4, 4));
    }
}
