//! Per-screen key handlers for [`App`].
//!
//! Each `handle_*(&mut self, KeyEvent)` lives here so the central
//! [`super::App::handle_key`] dispatcher in `mod.rs` is left as a flat
//! routing table without 2000+ lines of match bodies inline. The
//! handlers are `pub(super)` because the dispatcher (in the parent
//! module) is their only caller — no need to widen them further.
//!
//! Helpers used by individual handlers stay in `mod.rs` where the rest
//! of the App impl lives; submodules can see private inherent methods
//! of `App`, so handlers here can freely call `self.refresh_remote_pane`,
//! `self.push_log`, etc. without visibility changes.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use zeroize::Zeroize;

use crate::checkpoint::CheckpointKind;
use crate::session::{AuthMethod, Session};
use crate::transport;
use crate::tui::state::{EditField, PendingCancel, PostConnectOffer, ViewerKind};

use super::{App, LogLevel, Pane, Screen, is_chord};

/// Apply a single text-editing keystroke to `buf` and report whether the
/// buffer changed.
///
/// Handles Backspace (delete one char), Ctrl+U (clear the buffer), and
/// any printable Char (append). Anything else returns false, letting the
/// caller's match handle navigation / submit / cancel keys above the
/// fallthrough arm that invokes this.
///
/// Centralises the input-editing semantics so adding (e.g.) Ctrl+W
/// later is one change site instead of eight.
fn apply_text_edit(buf: &mut String, key: &KeyEvent) -> bool {
    match key.code {
        KeyCode::Backspace => {
            buf.pop();
            true
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            buf.clear();
            true
        }
        KeyCode::Char(c) if !super::is_chord(key.modifiers) => {
            buf.push(c);
            true
        }
        _ => false,
    }
}

impl App {
    pub(super) fn handle_session_select(&mut self, key: KeyEvent) {
        if is_chord(key.modifiers) && matches!(key.code, KeyCode::Char(_)) {
            return;
        }
        match key.code {
            KeyCode::Up if self.session_cursor > 0 => {
                self.session_cursor -= 1;
            }
            KeyCode::Down if self.session_cursor + 1 < self.sessions.len() => {
                self.session_cursor += 1;
            }
            KeyCode::Enter => {
                let Some(s) = self.sessions.get(self.session_cursor).cloned() else {
                    return;
                };
                // Came from the saved-sessions list, so it is already on disk.
                self.pending_session_unsaved = false;
                match &s.auth {
                    AuthMethod::Password => {
                        self.pending_session = Some(s);
                        self.password_input.zeroize();
                        self.screen = Screen::PasswordPrompt;
                    }
                    AuthMethod::Key { .. } | AuthMethod::Agent => {
                        self.pending_session = Some(s.clone());
                        self.pending_password = None;
                        self.start_connect(s, None);
                    }
                }
            }
            KeyCode::Char('n') => {
                self.new_session_input.clear();
                self.new_session_error = None;
                self.screen = Screen::NewSession;
            }
            KeyCode::Char('e') => {
                self.open_edit_session();
            }
            KeyCode::Char('d') => {
                self.open_delete_session();
            }
            KeyCode::Char('t') => {
                self.cycle_theme();
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                self.request_quit(Screen::SessionSelect);
            }
            _ => {}
        }
    }

    pub(super) fn handle_new_session(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.new_session_input.clear();
                self.new_session_error = None;
                self.screen = Screen::SessionSelect;
            }
            KeyCode::Enter => {
                match Session::from_url(&self.new_session_input) {
                    Ok(session) => {
                        self.new_session_input.clear();
                        self.new_session_error = None;
                        // Built from a URL — `from_url` persists nothing, so
                        // the connect flow will offer to save it.
                        self.pending_session_unsaved = true;
                        match &session.auth {
                            AuthMethod::Password => {
                                self.pending_session = Some(session);
                                self.password_input.zeroize();
                                self.screen = Screen::PasswordPrompt;
                            }
                            AuthMethod::Key { .. } | AuthMethod::Agent => {
                                self.pending_session = Some(session.clone());
                                self.pending_password = None;
                                self.start_connect(session, None);
                            }
                        }
                    }
                    Err(e) => {
                        self.new_session_error = Some(e.to_string());
                    }
                }
            }
            _ => {
                if apply_text_edit(&mut self.new_session_input, &key) {
                    self.new_session_error = None;
                }
            }
        }
    }

    pub(super) fn handle_edit_session(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.edit_session_form = None;
                self.screen = Screen::SessionSelect;
            }
            KeyCode::Enter => self.submit_edit_session(),
            KeyCode::Tab | KeyCode::Down => {
                if let Some(f) = self.edit_session_form.as_mut() {
                    f.focused = f.focused.next();
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(f) = self.edit_session_form.as_mut() {
                    f.focused = f.focused.prev();
                }
            }
            // Space toggles the focused boolean (currently only
            // AcceptInvalidCerts). On text fields, Space is a literal
            // character — let the Char arm handle it via fallthrough.
            KeyCode::Char(' ') => {
                if let Some(f) = self.edit_session_form.as_mut() {
                    if !f.focused.is_text_field() {
                        if matches!(f.focused, EditField::AcceptInvalidCerts) {
                            f.accept_invalid_certs = !f.accept_invalid_certs;
                        }
                        f.error = None;
                    } else if let Some(v) = f.current_value_mut() {
                        v.push(' ');
                        f.error = None;
                    }
                }
            }
            _ => {
                if let Some(f) = self.edit_session_form.as_mut()
                    && let Some(v) = f.current_value_mut()
                    && apply_text_edit(v, &key)
                {
                    f.error = None;
                }
            }
        }
    }

    pub(super) fn handle_confirm_delete_session(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(s) = self.pending_session_delete.take() {
                    match Session::delete(&s.name) {
                        Ok(()) => {
                            self.push_log(
                                LogLevel::Success,
                                format!("session deleted: {}", s.name),
                            );
                            self.reload_sessions();
                            // Clamp the cursor: the list just shrank.
                            if self.sessions.is_empty() {
                                self.session_cursor = 0;
                            } else if self.session_cursor >= self.sessions.len() {
                                self.session_cursor = self.sessions.len() - 1;
                            }
                        }
                        Err(e) => {
                            self.push_log(
                                LogLevel::Error,
                                format!("delete {} failed: {e}", s.name),
                            );
                        }
                    }
                }
                self.screen = Screen::SessionSelect;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.pending_session_delete = None;
                self.screen = Screen::SessionSelect;
            }
            _ => {}
        }
    }

    pub(super) fn handle_password_prompt(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.password_input.zeroize();
                self.pending_session = None;
                self.pending_password = None;
                self.screen = Screen::SessionSelect;
            }
            KeyCode::Enter => {
                let Some(session) = self.pending_session.clone() else {
                    self.screen = Screen::SessionSelect;
                    return;
                };
                // Copy out and wipe in place, rather than `mem::take`.
                // Taking hands the pre-sized allocation to the connect task
                // and leaves `String::default()` — capacity zero — in the
                // field, so a retry after a wrong password reallocates on
                // every keystroke, scattering fragments of the partial
                // secret across the heap that no later zeroize can reach.
                // `zeroize` clears the bytes and keeps the capacity.
                let password = zeroize::Zeroizing::new(self.password_input.as_str().to_owned());
                self.password_input.zeroize();
                self.pending_password = Some(password.clone());
                self.start_connect(session, Some(password));
            }
            _ => {
                apply_text_edit(&mut self.password_input, &key);
            }
        }
    }

    pub(super) fn handle_key_passphrase_prompt(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                // Bail on the connect attempt entirely.
                self.passphrase_input.zeroize();
                self.passphrase_error = None;
                self.passphrase_attempted = false;
                self.pending_session = None;
                self.pending_password = None;
                self.screen = Screen::SessionSelect;
                self.push_log(LogLevel::Info, "connect cancelled".into());
            }
            KeyCode::Enter => {
                if self.passphrase_input.is_empty() {
                    // Empty submit would just bounce off the same KeyNeedsPassphrase.
                    // Show a hint instead of round-tripping.
                    self.passphrase_error = Some("enter the passphrase or [esc] to cancel".into());
                    return;
                }
                let Some(session) = self.pending_session.clone() else {
                    self.screen = Screen::SessionSelect;
                    return;
                };
                // See the password prompt: copy out, wipe in place.
                let passphrase = zeroize::Zeroizing::new(self.passphrase_input.as_str().to_owned());
                self.passphrase_input.zeroize();
                self.passphrase_attempted = true;
                self.passphrase_error = None;
                // Cache for the dispatcher: parallel transfers re-open the
                // connection and need to decrypt the key again.
                self.pending_password = Some(passphrase.clone());
                self.start_connect(session, Some(passphrase));
            }
            _ => {
                if apply_text_edit(&mut self.passphrase_input, &key) {
                    self.passphrase_error = None;
                }
            }
        }
    }

    pub(super) fn handle_main(&mut self, key: KeyEvent) {
        // A Ctrl or Alt chord is one of the ctrl+ shortcuts or nothing: the
        // letter arms below don't look at modifiers, and ctrl+c reaching
        // `c` opened the cancel prompt.
        let chord = is_chord(key.modifiers);
        if chord && !matches!(key.code, KeyCode::Char('s' | 'd' | 'u' | 'x')) {
            return;
        }
        match key.code {
            KeyCode::Tab => self.cycle_pane(true),
            KeyCode::BackTab => self.cycle_pane(false),
            KeyCode::Up => match self.active_pane {
                Pane::Local | Pane::Remote => self.active_pane_mut().unwrap().move_cursor(-1),
                Pane::Transfers => self.move_transfer_cursor(-1),
                Pane::Log => self.log_scroll_by(1),
            },
            KeyCode::Down => match self.active_pane {
                Pane::Local | Pane::Remote => self.active_pane_mut().unwrap().move_cursor(1),
                Pane::Transfers => self.move_transfer_cursor(1),
                Pane::Log => self.log_scroll_by(-1),
            },
            KeyCode::PageUp => match self.active_pane {
                Pane::Local | Pane::Remote => self.active_pane_mut().unwrap().move_cursor(-10),
                Pane::Transfers => self.move_transfer_cursor(-10),
                Pane::Log => self.log_scroll_by(self.log_rows() as isize),
            },
            KeyCode::PageDown => match self.active_pane {
                Pane::Local | Pane::Remote => self.active_pane_mut().unwrap().move_cursor(10),
                Pane::Transfers => self.move_transfer_cursor(10),
                Pane::Log => self.log_scroll_by(-(self.log_rows() as isize)),
            },
            KeyCode::Enter => match self.active_pane {
                Pane::Local => self.local_enter(),
                Pane::Remote => self.remote_enter(),
                Pane::Transfers | Pane::Log => {}
            },
            KeyCode::Backspace => match self.active_pane {
                Pane::Local => {
                    let mut path = std::path::PathBuf::from(&self.local.path);
                    if path.pop() {
                        self.local.path = path.display().to_string();
                        self.local.cursor = 0;
                        self.refresh_local_pane();
                    }
                }
                Pane::Remote => {
                    let parent = transport::parent_remote(&self.remote.path);
                    if parent != self.remote.path {
                        self.refresh_remote_pane(parent);
                    }
                }
                Pane::Transfers | Pane::Log => {}
            },
            KeyCode::Char(' ') => {
                if let Some(pane) = self.active_pane_mut() {
                    pane.toggle_selected();
                }
            }
            KeyCode::Char('c') if self.active_pane == Pane::Transfers => {
                self.request_cancel_selected_transfer();
            }
            KeyCode::Char('C') if self.active_pane == Pane::Transfers => {
                self.request_cancel_selected_batch();
            }
            // Resume: re-queue any jobs from the last interrupted walk that
            // haven't completed yet. 'r' resumes the download checkpoint,
            // 'R' resumes the upload checkpoint.
            KeyCode::Char('r') if self.active_pane == Pane::Transfers => {
                self.resume_walk(CheckpointKind::Download);
            }
            KeyCode::Char('R') if self.active_pane == Pane::Transfers => {
                self.resume_walk(CheckpointKind::Upload);
            }
            KeyCode::Char('s') if chord => {
                self.open_save_session();
            }
            KeyCode::Char('d') if chord => {
                self.enqueue_selected_downloads();
            }
            KeyCode::Char('u') if chord => {
                self.start_selected_uploads();
            }
            KeyCode::F(2) if self.active_pane == Pane::Remote => {
                self.open_rename();
            }
            KeyCode::F(7) if self.active_pane == Pane::Remote => {
                self.open_mkdir();
            }
            KeyCode::Delete
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    && self.active_pane == Pane::Remote =>
            {
                self.open_delete();
            }
            // 'D' (uppercase) as an alternative to Shift+Delete for terminals
            // that don't pass that combo cleanly.
            KeyCode::Char('D') if self.active_pane == Pane::Remote => {
                self.open_delete();
            }
            KeyCode::Char('p') => {
                self.toggle_pause();
            }
            KeyCode::Char('t') => {
                self.cycle_theme();
            }
            KeyCode::Char('v') => {
                self.handle_view_request();
            }
            KeyCode::Char('/') => {
                if matches!(self.active_pane, Pane::Local | Pane::Remote) {
                    self.open_search();
                }
            }
            KeyCode::F(5) => {
                self.refresh_active_pane();
            }
            KeyCode::Char('x') if chord && self.transport.is_some() => {
                self.screen = Screen::ConfirmDisconnect;
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                self.request_quit(Screen::Main);
            }
            _ => {}
        }
    }

    /// Quit via the confirmation modal, or immediately when the user has
    /// disabled it with `confirm_quit = false` in config.ini.
    pub(super) fn request_quit(&mut self, from: Screen) {
        if self.config.general.confirm_quit {
            self.quit_return = from;
            self.screen = Screen::ConfirmQuit;
        } else {
            self.should_quit = true;
        }
    }

    pub(super) fn handle_confirm_cancel(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(pc) = self.pending_cancel.take() {
                    if let Some(manager) = &self.transfer_manager {
                        match pc {
                            PendingCancel::Single { id, name } => {
                                // It may have finished while the question
                                // was open; then there is nothing to cancel,
                                // and its own events do the bookkeeping.
                                if !manager.cancel(id) {
                                    self.push_log(
                                        LogLevel::Info,
                                        format!("{name} had already finished"),
                                    );
                                } else {
                                    self.push_log(LogLevel::Warn, format!("cancelled: {name}"));
                                    // Mark it cancelled, as `C` does for a whole
                                    // batch: left `pending`, the file came back
                                    // with the next resume offer or `r`. The rest
                                    // of its batch keeps the checkpoint.
                                    self.cancel_batch_in_checkpoint(&[id]);
                                }
                            }
                            PendingCancel::Batch { batch_id, .. } => {
                                // Collect before cancelling: the ids are what
                                // tie the batch to its checkpoint entries.
                                let batch_ids = manager.job_ids_in_batch(batch_id);
                                let (active_n, pending_n) = manager.cancel_batch(batch_id);
                                self.push_log(
                                    LogLevel::Warn,
                                    format!(
                                        "cancelled batch: {} active + {} queued",
                                        active_n, pending_n
                                    ),
                                );
                                // Mark just this batch's checkpoint entries
                                // cancelled, so a mistaken `r` / `R` doesn't
                                // re-queue work the user threw away — while
                                // leaving any batch running the other way
                                // (which shares the same checkpoint storage)
                                // intact and still resumable.
                                self.cancel_batch_in_checkpoint(&batch_ids);
                                // And the ones that never got a job id.
                                self.cancel_waiting_in_batch(batch_id);
                            }
                        }
                    }
                    // Re-clamp the cursor: cancellation removes jobs from
                    // the active list, so the cursor may now point off the end.
                    let new_len = self.active_jobs().len();
                    if new_len == 0 {
                        self.transfer_cursor = 0;
                    } else if self.transfer_cursor >= new_len {
                        self.transfer_cursor = new_len - 1;
                    }
                }
                self.screen = self.previous_screen.clone();
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.pending_cancel = None;
                self.screen = self.previous_screen.clone();
            }
            _ => {}
        }
    }

    pub(super) fn handle_confirm_host_key(&mut self, key: KeyEvent) {
        use crate::transport::sftp::HostKeyDecision;
        if self.prompt_arming() {
            return;
        }
        let decision = match key.code {
            // y / Y — accept and save to known_hosts
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(HostKeyDecision::AcceptAndSave),
            // t / T — trust once, don't save
            KeyCode::Char('t') | KeyCode::Char('T') => Some(HostKeyDecision::AcceptOnce),
            // n / N / Esc — reject
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(HostKeyDecision::Reject),
            _ => None,
        };

        if let Some(decision) = decision {
            if let Some(mut phk) = self.pending_host_key.take()
                && let Some(tx) = phk.decision_tx.take()
            {
                let _ = tx.send(decision);
            }
            // Return to whatever the prompt interrupted, whichever way the
            // user answered.
            //
            // This prompt is not only raised by the initial connect: every
            // connection a session opens runs the same check, so a transfer
            // worker can raise it while the user is connected and working.
            // Jumping to the session selector on reject left them there with
            // a live transport and a running dispatcher behind it, and
            // jumping to the "connecting…" modal on accept showed a connect
            // that had already finished.
            //
            // During the initial connect the interrupted screen *is*
            // `Connection`, so a reject lands back there and the connect task
            // — now unblocked with `Reject` — fails and drives the usual
            // `ConnectFailed` path, which says why. That is strictly better
            // than silently returning to the selector: the user gets a log
            // line either way.
            self.screen = self.previous_screen.clone();
        }
    }

    pub(super) fn handle_rename(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.clear_rename_form();
                self.screen = Screen::Main;
            }
            KeyCode::Enter => self.submit_rename(),
            _ => {
                if apply_text_edit(&mut self.rename_input, &key) {
                    self.rename_error = None;
                }
            }
        }
    }

    pub(super) fn handle_mkdir(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mkdir_input.clear();
                self.mkdir_error = None;
                self.screen = Screen::Main;
            }
            KeyCode::Enter => self.submit_mkdir(),
            _ => {
                if apply_text_edit(&mut self.mkdir_input, &key) {
                    self.mkdir_error = None;
                }
            }
        }
    }

    pub(super) fn handle_confirm_delete(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(pd) = self.pending_delete.take() {
                    self.start_delete(pd.name, pd.remote_path, pd.is_dir);
                }
                self.screen = Screen::Main;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.pending_delete = None;
                self.screen = Screen::Main;
            }
            _ => {}
        }
    }

    pub(super) fn handle_confirm_overwrite(&mut self, key: KeyEvent) {
        if self.prompt_arming() {
            return;
        }
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                // Y always means "overwrite" / "proceed" for backward
                // compat with the old single-file modals. For plan modals
                // it's equivalent to "overwrite all conflicts".
                self.confirm_overwrite_proceed(false);
            }
            // Skip-conflicts is only offered for a plan. A rename has nothing
            // to skip, and proceeding there meant replacing the file.
            KeyCode::Char('s') | KeyCode::Char('S')
                if !matches!(
                    self.pending_overwrite,
                    Some(crate::tui::state::OverwritePending::Rename { .. })
                ) =>
            {
                self.confirm_overwrite_proceed(true);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.pending_overwrite = None;
                self.rename_input.clear();
                self.rename_original.clear();
                self.screen = Screen::Main;
                self.push_log(LogLevel::Info, "overwrite cancelled".into());
            }
            _ => {}
        }
    }

    pub(super) fn handle_search(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                // Cancel: clear filter and restore the full listing.
                match self.search_target {
                    Pane::Local => self.local.clear_filter(),
                    Pane::Remote => self.remote.clear_filter(),
                    _ => {}
                }
                self.search_input.clear();
                self.screen = Screen::Main;
            }
            KeyCode::Enter => {
                // Accept: keep the filter (already applied live), exit search.
                self.screen = Screen::Main;
            }
            KeyCode::Up => self.move_search_cursor(-1),
            KeyCode::Down => self.move_search_cursor(1),
            KeyCode::PageUp => self.move_search_cursor(-10),
            KeyCode::PageDown => self.move_search_cursor(10),
            _ => {
                if apply_text_edit(&mut self.search_input, &key) {
                    self.apply_search_filter();
                }
            }
        }
    }

    pub(super) fn handle_save_session(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.save_session_error = None;
                self.screen = Screen::Main;
            }
            KeyCode::Enter => {
                let name = self.save_session_input.trim().to_string();
                if name.is_empty() {
                    self.save_session_error = Some("name cannot be empty".into());
                    return;
                }
                let Some(current) = self.current_session.clone() else {
                    self.save_session_error = Some("no active session".into());
                    return;
                };
                // Another saved session by this name would be overwritten
                // without a word; the edit form refuses the same. This
                // session's own file is the one to update.
                let own_file = self.current_session_saved && current.name == name;
                if !own_file && self.sessions.iter().any(|s| s.name == name) {
                    self.save_session_error =
                        Some(format!("a session named `{name}` already exists"));
                    return;
                }

                // Snapshot the current navigation state into the saved session
                // so reopening picks up where we left off.
                let old_name = current.name.clone();
                let mut to_save = current;
                to_save.name = name.clone();
                to_save.remote_dir = if self.remote.path.is_empty() {
                    "/".to_string()
                } else {
                    self.remote.path.clone()
                };
                to_save.local_dir = Some(std::path::PathBuf::from(&self.local.path));

                // A certificate trusted for this connection only stays out
                // of the file. The connection keeps requiring it.
                let mut file_copy = to_save.clone();
                if self.trust_once_pin.is_some() && file_copy.cert_sha256 == self.trust_once_pin {
                    file_copy.cert_sha256 = None;
                }

                match file_copy.save() {
                    Ok(()) => {
                        let origin = crate::transport::origin_of(&to_save);
                        self.current_session = Some(to_save);
                        self.current_session_saved = true;
                        // Checkpoints are keyed by session name; left under
                        // the old one, `r` could no longer find them.
                        if old_name != name {
                            self.move_checkpoints(&old_name, &name, &origin);
                        }
                        // Refresh the sessions list so it reflects the new
                        // entry next time the user opens the selector.
                        self.reload_sessions();
                        self.save_session_error = None;
                        self.screen = Screen::Main;
                        self.push_log(LogLevel::Success, format!("session saved: {name}"));
                    }
                    Err(e) => {
                        self.save_session_error = Some(e.to_string());
                    }
                }
            }
            _ => {
                if apply_text_edit(&mut self.save_session_input, &key) {
                    self.save_session_error = None;
                }
            }
        }
    }

    pub(super) fn handle_viewer(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.close_viewer(),
            KeyCode::Up | KeyCode::Char('k') => self.viewer_scroll(-1),
            KeyCode::Down | KeyCode::Char('j') => self.viewer_scroll(1),
            KeyCode::PageUp => self.viewer_scroll(-20),
            KeyCode::PageDown | KeyCode::Char(' ') => self.viewer_scroll(20),
            KeyCode::Home | KeyCode::Char('g') => self.viewer_scroll_to(0),
            KeyCode::End | KeyCode::Char('G') => self.viewer_scroll_to(usize::MAX),
            _ => {}
        }
    }

    /// Close the viewer and go back to the screen it opened over.
    pub(super) fn close_viewer(&mut self) {
        self.drop_viewer();
        self.screen = self.previous_screen.clone();
    }

    /// Drop the viewer, forcing a full repaint if it showed an image: sixel
    /// and kitty graphics aren't in ratatui's buffer and won't be cleaned up
    /// by the next diff'd draw.
    pub(super) fn drop_viewer(&mut self) {
        let was_image = matches!(
            self.viewer.as_ref().map(|v| &v.kind),
            Some(ViewerKind::Image { .. })
        );
        self.viewer = None;
        self.image_needs_redraw = false;
        if was_image {
            self.needs_terminal_clear = true;
        }
    }

    /// The offer to persist an ad-hoc connection, shown once after it comes
    /// up. `y` hands off to the existing save-session modal (which does the
    /// naming, validation and write); anything else dismisses and leaves the
    /// connection running unsaved — `ctrl+s` still works later.
    pub(super) fn handle_offer_save_session(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.pending_offers.pop_front();
                self.screen = Screen::Main;
                self.open_save_session();
            }
            // Enter is deliberately not a dismiss key: the modal offers `y`
            // and `n/esc`, and every other confirm in the app ignores keys it
            // doesn't list rather than guessing at a default.
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.pending_offers.pop_front();
                self.show_next_offer();
                self.push_log(
                    LogLevel::Info,
                    "not saved — press ctrl+s any time to save this session".into(),
                );
            }
            _ => {}
        }
    }

    /// Remove a discarded upload batch's partial files from the server.
    /// The checkpoint was their only record: left there, they'd sit on the
    /// server for good. One already gone is not a failure.
    pub(super) fn remove_remote_parts(&mut self, paths: Vec<String>) {
        if paths.is_empty() {
            return;
        }
        let Some(t) = self.transport.clone() else {
            return;
        };
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let mut transport = t.lock().await;
            let mut removed = 0usize;
            let mut failures = Vec::new();
            for path in paths {
                match transport.delete_file(&path).await {
                    Ok(()) => removed += 1,
                    Err(crate::error::BlinkError::NotFound(_)) => {}
                    Err(e) => {
                        failures.push(format!("could not remove {path} from the server: {e}"))
                    }
                }
            }
            let _ = tx.send(crate::tui::event::AppEvent::RemotePartsRemoved { removed, failures });
        });
    }

    /// The offer to resume an interrupted batch, shown once per checkpoint
    /// after a connection comes up.
    ///
    /// `later` is deliberately non-destructive: the checkpoint stays on disk
    /// and is offered again next connect. `discard` is the way out, and it
    /// sweeps the batch's orphaned partials because the checkpoint is the
    /// only record of where they are.
    pub(super) fn handle_offer_resume_checkpoint(&mut self, key: KeyEvent) {
        let Some(PostConnectOffer::ResumeCheckpoint(offer)) = self.pending_offers.front().cloned()
        else {
            // Nothing queued — shouldn't happen, but don't strand the user.
            self.show_next_offer();
            return;
        };

        match key.code {
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.pending_offers.pop_front();
                self.resume_walk(offer.kind);
                self.show_next_offer();
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                self.pending_offers.pop_front();
                match crate::checkpoint::discard(&offer.session, &offer.origin, offer.kind) {
                    Ok(outcome) => {
                        let parts = outcome.parts_removed;
                        // `failures` can now include the checkpoint file
                        // itself failing to remove (not just a stray
                        // partial), so "discarded" is only accurate when
                        // nothing in that list is left unresolved.
                        if outcome.failures.is_empty() {
                            self.push_log(
                                LogLevel::Info,
                                if parts > 0 {
                                    format!(
                                        "discarded the {} checkpoint and {parts} partial download(s)",
                                        offer.kind.as_str()
                                    )
                                } else {
                                    format!("discarded the {} checkpoint", offer.kind.as_str())
                                },
                            );
                        } else {
                            self.push_log(
                                LogLevel::Warn,
                                format!(
                                    "could not fully discard the {} checkpoint ({parts} partial download(s) removed)",
                                    offer.kind.as_str()
                                ),
                            );
                        }
                        for failure in outcome.failures {
                            self.push_log(LogLevel::Warn, failure);
                        }
                        self.remove_remote_parts(outcome.remote_parts);
                    }
                    Err(e) => {
                        self.push_log(
                            LogLevel::Error,
                            format!("could not discard the checkpoint: {e}"),
                        );
                    }
                }
                self.show_next_offer();
            }
            KeyCode::Esc => {
                self.pending_offers.pop_front();
                self.push_log(
                    LogLevel::Info,
                    format!(
                        "{} checkpoint kept — press {} in the transfers pane to resume it",
                        offer.kind.as_str(),
                        match offer.kind {
                            CheckpointKind::Download => "r",
                            CheckpointKind::Upload => "R",
                        }
                    ),
                );
                self.show_next_offer();
            }
            _ => {}
        }
    }

    pub(super) fn handle_confirm_disconnect(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.disconnect();
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.screen = Screen::Main;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::super::tests_support::app_on_main;
    use super::super::{App, Pane, Screen};
    use crate::checkpoint::{CheckpointKind, CheckpointOffer};
    use crate::config::Config;
    use crate::session::{AuthMethod, Session};
    use crate::theme::Theme;
    use crate::transfer::TransferManager;
    use crate::tui::state::{
        EditField, EditSessionForm, OverwritePending, PendingCancel, PendingDelete,
        PostConnectOffer, Viewer, ViewerKind,
    };

    fn app() -> App {
        App::new(Config::default(), Theme::load("dracula").unwrap())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn press(a: &mut App, code: KeyCode) {
        a.dispatch_key(key(code));
    }

    fn typed(a: &mut App, s: &str) {
        for c in s.chars() {
            press(a, KeyCode::Char(c));
        }
    }

    fn logged(a: &App, needle: &str) -> bool {
        a.log.iter().any(|l| l.message.contains(needle))
    }

    fn session(url: &str) -> Session {
        Session::from_url(url).unwrap()
    }

    fn named(url: &str, name: &str) -> Session {
        let mut s = session(url);
        s.name = name.into();
        s
    }

    // --- session selector ---

    #[test]
    fn arrows_move_the_session_cursor_and_stop_at_the_ends() {
        let mut a = app();
        a.sessions = vec![
            session("sftp://a@h"),
            session("sftp://b@h"),
            session("sftp://c@h"),
        ];
        for _ in 0..5 {
            press(&mut a, KeyCode::Down);
        }
        assert_eq!(a.session_cursor, 2);
        for _ in 0..5 {
            press(&mut a, KeyCode::Up);
        }
        assert_eq!(a.session_cursor, 0);
    }

    #[test]
    fn enter_with_no_saved_sessions_does_nothing() {
        let mut a = app();
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.screen, Screen::SessionSelect);
        assert!(a.pending_session.is_none());
    }

    #[tokio::test]
    async fn enter_on_an_agent_session_connects_without_asking() {
        let mut a = app();
        let mut s = session("sftp://me@127.0.0.1:1");
        s.auth = AuthMethod::Agent;
        a.sessions = vec![s];
        a.pending_password = Some(zeroize::Zeroizing::new("stale".into()));

        press(&mut a, KeyCode::Enter);

        assert_eq!(a.screen, Screen::Connection);
        assert!(a.pending_password.is_none());
        assert!(logged(&a, "connecting to me@127.0.0.1:1 via sftp"));
    }

    #[test]
    fn n_opens_a_blank_new_session_form() {
        let mut a = app();
        a.new_session_input = "left over".into();
        a.new_session_error = Some("old".into());
        press(&mut a, KeyCode::Char('n'));
        assert_eq!(a.screen, Screen::NewSession);
        assert!(a.new_session_input.is_empty());
        assert!(a.new_session_error.is_none());
    }

    #[test]
    fn e_and_d_open_the_edit_and_delete_modals_for_the_selected_session() {
        let mut a = app();
        a.sessions = vec![named("sftp://me@h", "one"), named("sftp://me@h", "two")];
        a.session_cursor = 1;

        press(&mut a, KeyCode::Char('e'));
        assert_eq!(a.screen, Screen::EditSession);
        assert_eq!(a.edit_session_form.as_ref().unwrap().original_name, "two");

        a.screen = Screen::SessionSelect;
        press(&mut a, KeyCode::Char('d'));
        assert_eq!(a.screen, Screen::ConfirmDeleteSession);
        assert_eq!(a.pending_session_delete.as_ref().unwrap().name, "two");
    }

    #[test]
    fn q_and_esc_ask_before_quitting_and_remember_where_from() {
        for code in [KeyCode::Char('q'), KeyCode::Esc] {
            let mut a = app();
            press(&mut a, code);
            assert_eq!(a.screen, Screen::ConfirmQuit);
            assert_eq!(a.quit_return, Screen::SessionSelect);
            assert!(!a.should_quit);

            let mut a = app_on_main(1);
            press(&mut a, code);
            assert_eq!(a.screen, Screen::ConfirmQuit);
            assert_eq!(a.quit_return, Screen::Main);
        }
    }

    #[test]
    fn with_the_quit_confirmation_off_q_quits_at_once() {
        let mut a = app();
        a.config.general.confirm_quit = false;
        press(&mut a, KeyCode::Char('q'));
        assert!(a.should_quit);

        let mut a = app_on_main(1);
        a.config.general.confirm_quit = false;
        press(&mut a, KeyCode::Esc);
        assert!(a.should_quit);
    }

    // --- new session ---

    #[test]
    fn esc_abandons_the_new_session_form() {
        let mut a = app();
        a.screen = Screen::NewSession;
        typed(&mut a, "sftp://half");
        press(&mut a, KeyCode::Esc);
        assert_eq!(a.screen, Screen::SessionSelect);
        assert!(a.new_session_input.is_empty());
    }

    #[test]
    fn a_bad_url_is_reported_and_editing_clears_the_error() {
        let mut a = app();
        a.screen = Screen::NewSession;
        typed(&mut a, "gopher://x");
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.screen, Screen::NewSession);
        assert!(a.new_session_error.is_some());

        press(&mut a, KeyCode::Backspace);
        assert_eq!(a.new_session_input, "gopher://");
        assert!(a.new_session_error.is_none());

        a.dispatch_key(ctrl('u'));
        assert!(a.new_session_input.is_empty());

        // Keys the form doesn't edit with change nothing.
        a.new_session_error = Some("kept".into());
        press(&mut a, KeyCode::Left);
        assert_eq!(a.new_session_error.as_deref(), Some("kept"));
    }

    // --- edit session ---

    fn editing() -> App {
        let mut a = app();
        let s = named("sftp://me@h", "my");
        a.edit_session_form = Some(EditSessionForm::from_session(&s));
        a.sessions = vec![s];
        a.screen = Screen::EditSession;
        a
    }

    #[test]
    fn up_moves_to_the_previous_field() {
        let mut a = editing();
        a.edit_session_form.as_mut().unwrap().focused = EditField::Host;
        press(&mut a, KeyCode::Up);
        assert_eq!(
            a.edit_session_form.as_ref().unwrap().focused,
            EditField::Name
        );
    }

    #[test]
    fn space_in_a_text_field_types_a_space() {
        let mut a = editing();
        a.edit_session_form.as_mut().unwrap().error = Some("old".into());
        press(&mut a, KeyCode::Char(' '));
        let form = a.edit_session_form.as_ref().unwrap();
        assert_eq!(form.name, "my ");
        assert!(form.error.is_none());
    }

    // --- delete session ---

    #[test]
    fn deleting_the_last_session_in_the_list_moves_the_cursor_up() {
        let _home = crate::paths::test_home();
        let one = named("sftp://me@h", "one");
        let two = named("sftp://me@h", "two");
        one.save().unwrap();
        two.save().unwrap();
        let mut a = app();
        a.reload_sessions();
        a.session_cursor = 1;
        a.pending_session_delete = Some(a.sessions[1].clone());
        a.screen = Screen::ConfirmDeleteSession;

        press(&mut a, KeyCode::Char('y'));

        assert_eq!(a.screen, Screen::SessionSelect);
        assert_eq!(a.sessions.len(), 1);
        assert_eq!(a.session_cursor, 0);
        assert!(logged(&a, "session deleted"));
    }

    #[test]
    fn deleting_the_only_session_leaves_the_cursor_at_zero() {
        let _home = crate::paths::test_home();
        let one = named("sftp://me@h", "one");
        one.save().unwrap();
        let mut a = app();
        a.reload_sessions();
        a.pending_session_delete = Some(one);
        a.screen = Screen::ConfirmDeleteSession;

        press(&mut a, KeyCode::Char('Y'));

        assert!(a.sessions.is_empty());
        assert_eq!(a.session_cursor, 0);
    }

    #[test]
    fn a_failed_session_delete_is_logged() {
        let _home = crate::paths::test_home();
        let mut a = app();
        a.pending_session_delete = Some(named("sftp://me@h", "ghost"));
        a.screen = Screen::ConfirmDeleteSession;
        press(&mut a, KeyCode::Char('y'));
        assert_eq!(a.screen, Screen::SessionSelect);
        assert!(logged(&a, "delete ghost failed"));
    }

    // --- password and passphrase prompts ---

    #[test]
    fn esc_at_the_password_prompt_wipes_it_and_forgets_the_session() {
        let mut a = app();
        a.screen = Screen::PasswordPrompt;
        a.pending_session = Some(session("sftp://me@h"));
        typed(&mut a, "hunt");
        press(&mut a, KeyCode::Backspace);
        assert_eq!(a.password_input.as_str(), "hun");

        press(&mut a, KeyCode::Esc);

        assert_eq!(a.screen, Screen::SessionSelect);
        assert!(a.password_input.is_empty());
        assert!(a.pending_session.is_none());
        assert!(a.pending_password.is_none());
    }

    #[test]
    fn a_password_with_no_session_to_send_it_to_goes_back() {
        let mut a = app();
        a.screen = Screen::PasswordPrompt;
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.screen, Screen::SessionSelect);
    }

    fn at_passphrase_prompt() -> App {
        let mut a = app();
        a.screen = Screen::KeyPassphrasePrompt;
        let mut s = session("sftp://me@127.0.0.1:1");
        s.auth = AuthMethod::Key {
            path: "/k/id".into(),
        };
        a.pending_session = Some(s);
        a
    }

    #[test]
    fn esc_at_the_passphrase_prompt_cancels_the_connect_and_says_so() {
        let mut a = at_passphrase_prompt();
        typed(&mut a, "secret");
        a.passphrase_attempted = true;
        a.passphrase_error = Some("wrong".into());

        press(&mut a, KeyCode::Esc);

        assert_eq!(a.screen, Screen::SessionSelect);
        assert!(a.passphrase_input.is_empty());
        assert!(a.passphrase_error.is_none());
        assert!(!a.passphrase_attempted);
        assert!(a.pending_session.is_none());
        assert!(logged(&a, "connect cancelled"));
    }

    #[test]
    fn an_empty_passphrase_is_not_sent() {
        let mut a = at_passphrase_prompt();
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.screen, Screen::KeyPassphrasePrompt);
        assert_eq!(
            a.passphrase_error.as_deref(),
            Some("enter the passphrase or [esc] to cancel")
        );

        typed(&mut a, "x");
        assert!(a.passphrase_error.is_none(), "typing clears the hint");
    }

    #[test]
    fn a_passphrase_with_no_session_to_send_it_to_goes_back() {
        let mut a = at_passphrase_prompt();
        a.pending_session = None;
        typed(&mut a, "x");
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.screen, Screen::SessionSelect);
    }

    // --- main view: moving around ---

    #[test]
    fn arrows_and_pages_move_the_file_cursor() {
        let mut a = app_on_main(30);
        press(&mut a, KeyCode::Down);
        assert_eq!(a.local.cursor, 1);
        press(&mut a, KeyCode::PageDown);
        assert_eq!(a.local.cursor, 11);
        press(&mut a, KeyCode::PageUp);
        assert_eq!(a.local.cursor, 1);
        press(&mut a, KeyCode::Up);
        assert_eq!(a.local.cursor, 0);
    }

    /// Give `a` `n` downloads, each started as the dispatcher would start
    /// it, so they show in the transfers pane.
    fn with_running(a: &mut App, n: usize) -> Vec<u64> {
        let (m, _events) = TransferManager::new(u8::try_from(n).unwrap());
        for i in 0..n {
            m.enqueue_download(format!("/r/f{i}"), format!("/l/f{i}").into())
                .unwrap();
        }
        let ids = (0..n)
            .map(|_| m.take_next_pending().expect("a job is queued").id)
            .collect();
        a.transfer_manager = Some(m);
        ids
    }

    #[test]
    fn arrows_and_pages_on_the_transfers_pane_move_the_transfer_cursor() {
        let mut a = app_on_main(1);
        with_running(&mut a, 10);
        a.active_pane = Pane::Transfers;
        press(&mut a, KeyCode::Down);
        assert_eq!(a.transfer_cursor, 1);
        press(&mut a, KeyCode::PageDown);
        assert_eq!(a.transfer_cursor, 9, "stops at the last transfer");
        press(&mut a, KeyCode::Up);
        assert_eq!(a.transfer_cursor, 8);
        press(&mut a, KeyCode::PageUp);
        assert_eq!(a.transfer_cursor, 0, "stops at the first");
    }

    #[test]
    fn moving_keys_on_the_log_pane_do_nothing() {
        let mut a = app_on_main(5);
        a.active_pane = Pane::Log;
        for code in [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::PageDown,
            KeyCode::PageUp,
            KeyCode::Enter,
            KeyCode::Backspace,
            KeyCode::Char(' '),
            KeyCode::Char('/'),
            KeyCode::Char('v'),
            KeyCode::F(5),
        ] {
            press(&mut a, code);
        }
        assert_eq!(a.screen, Screen::Main);
        assert_eq!(a.local.cursor, 0);
        assert_eq!(a.local.path, "/l");
    }

    #[test]
    fn tab_and_shift_tab_cycle_the_panes() {
        let mut a = app_on_main(1);
        press(&mut a, KeyCode::Tab);
        assert_eq!(a.active_pane, Pane::Remote);
        press(&mut a, KeyCode::BackTab);
        press(&mut a, KeyCode::BackTab);
        assert_eq!(a.active_pane, Pane::Log);
    }

    #[tokio::test]
    async fn backspace_on_the_local_pane_goes_up_a_folder() {
        let mut a = app_on_main(3);
        a.local.path = "/l/sub".into();
        a.local.cursor = 2;
        press(&mut a, KeyCode::Backspace);
        assert_eq!(a.local.path, "/l");
        assert_eq!(a.local.cursor, 0);

        a.local.path = "/".into();
        press(&mut a, KeyCode::Backspace);
        assert_eq!(a.local.path, "/", "the root has no parent");
    }

    #[test]
    fn backspace_at_the_remote_root_stays_put() {
        let mut a = app_on_main(1);
        a.active_pane = Pane::Remote;
        a.remote.path = "/".into();
        press(&mut a, KeyCode::Backspace);
        assert_eq!(a.remote.path, "/");
        assert!(a.log.is_empty());
    }

    #[test]
    fn space_selects_the_entry_under_the_cursor() {
        let mut a = app_on_main(3);
        press(&mut a, KeyCode::Char(' '));
        assert_eq!(a.local.selected_entries().len(), 1);
        a.active_pane = Pane::Transfers;
        press(&mut a, KeyCode::Char(' '));
        assert_eq!(a.local.selected_entries().len(), 1);
    }

    // --- main view: keys tied to a pane or a connection ---

    #[test]
    fn transfer_keys_do_nothing_outside_the_transfers_pane() {
        let mut a = app_on_main(1);
        with_running(&mut a, 2);
        for c in ['c', 'C', 'r', 'R'] {
            press(&mut a, KeyCode::Char(c));
        }
        assert_eq!(a.screen, Screen::Main);
        assert!(a.pending_cancel.is_none());
        assert!(a.log.is_empty(), "{:?}", a.log);
    }

    #[test]
    fn c_and_shift_c_on_the_transfers_pane_ask_to_cancel() {
        let mut a = app_on_main(1);
        with_running(&mut a, 2);
        a.active_pane = Pane::Transfers;
        press(&mut a, KeyCode::Char('c'));
        assert_eq!(a.screen, Screen::ConfirmCancel);
        assert!(matches!(
            a.pending_cancel,
            Some(PendingCancel::Single { .. })
        ));
    }

    #[test]
    fn r_on_the_transfers_pane_tries_to_resume() {
        let mut a = app_on_main(1);
        a.active_pane = Pane::Transfers;
        press(&mut a, KeyCode::Char('r'));
        press(&mut a, KeyCode::Char('R'));
        assert!(!a.log.is_empty(), "resume reports what it found");
    }

    #[test]
    fn remote_file_keys_do_nothing_on_the_local_pane() {
        let mut a = app_on_main(1);
        a.dispatch_key(key(KeyCode::F(2)));
        a.dispatch_key(key(KeyCode::F(7)));
        a.dispatch_key(key(KeyCode::Char('D')));
        a.dispatch_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::SHIFT));
        assert_eq!(a.screen, Screen::Main);
        assert!(a.log.is_empty(), "{:?}", a.log);
    }

    #[test]
    fn remote_file_keys_on_the_remote_pane_need_a_connection() {
        for k in [
            key(KeyCode::F(2)),
            key(KeyCode::F(7)),
            key(KeyCode::Char('D')),
            KeyEvent::new(KeyCode::Delete, KeyModifiers::SHIFT),
        ] {
            let mut a = app_on_main(1);
            a.active_pane = Pane::Remote;
            a.dispatch_key(k);
            assert_eq!(a.screen, Screen::Main, "{k:?}");
            assert!(logged(&a, "not connected"), "{k:?}");
        }
    }

    #[test]
    fn plain_delete_is_not_shift_delete() {
        let mut a = app_on_main(1);
        a.active_pane = Pane::Remote;
        press(&mut a, KeyCode::Delete);
        assert!(a.log.is_empty());
    }

    #[test]
    fn transfer_and_save_shortcuts_need_a_connection() {
        for c in ['d', 'u', 's'] {
            let mut a = app_on_main(1);
            a.dispatch_key(ctrl(c));
            assert_eq!(a.screen, Screen::Main, "ctrl+{c}");
            assert!(logged(&a, "not connected"), "ctrl+{c}");
        }
    }

    #[test]
    fn ctrl_x_without_a_connection_does_nothing() {
        let mut a = app_on_main(1);
        a.dispatch_key(ctrl('x'));
        assert_eq!(a.screen, Screen::Main);
    }

    #[test]
    fn slash_opens_search_on_the_active_file_pane() {
        let mut a = app_on_main(3);
        a.active_pane = Pane::Transfers;
        press(&mut a, KeyCode::Char('/'));
        assert_eq!(a.screen, Screen::Main);

        a.active_pane = Pane::Local;
        press(&mut a, KeyCode::Char('/'));
        assert_eq!(a.screen, Screen::Search);
        assert_eq!(a.search_target, Pane::Local);
    }

    #[tokio::test]
    async fn f5_refreshes_the_local_pane() {
        let mut a = app_on_main(3);
        press(&mut a, KeyCode::F(5));
        assert!(logged(&a, "refreshed: /l"));
    }

    #[test]
    fn f5_on_the_remote_pane_needs_a_connection() {
        let mut a = app_on_main(1);
        a.active_pane = Pane::Remote;
        press(&mut a, KeyCode::F(5));
        assert!(logged(&a, "not connected"));
    }

    // --- confirmations over the main view ---

    #[test]
    fn declining_a_cancel_keeps_the_transfer() {
        for code in [KeyCode::Char('n'), KeyCode::Char('N'), KeyCode::Esc] {
            let mut a = app_on_main(1);
            let ids = with_running(&mut a, 1);
            a.pending_cancel = Some(PendingCancel::Single {
                id: ids[0],
                name: "f0".into(),
            });
            a.previous_screen = Screen::Main;
            a.screen = Screen::ConfirmCancel;

            press(&mut a, code);

            assert_eq!(a.screen, Screen::Main);
            assert!(a.pending_cancel.is_none());
            assert_eq!(a.active_jobs().len(), 1);
        }
    }

    #[test]
    fn cancelling_the_last_transfer_pulls_the_cursor_back() {
        let mut a = app_on_main(1);
        let ids = with_running(&mut a, 2);
        a.transfer_cursor = 1;
        a.pending_cancel = Some(PendingCancel::Single {
            id: ids[1],
            name: "f1".into(),
        });
        a.previous_screen = Screen::Main;
        a.screen = Screen::ConfirmCancel;

        press(&mut a, KeyCode::Char('y'));

        assert_eq!(a.active_jobs().len(), 1);
        assert_eq!(a.transfer_cursor, 0);
    }

    #[test]
    fn cancelling_every_transfer_resets_the_cursor() {
        let mut a = app_on_main(1);
        let ids = with_running(&mut a, 1);
        a.pending_cancel = Some(PendingCancel::Single {
            id: ids[0],
            name: "f0".into(),
        });
        a.previous_screen = Screen::Main;
        a.screen = Screen::ConfirmCancel;

        press(&mut a, KeyCode::Char('y'));

        assert!(a.active_jobs().is_empty());
        assert_eq!(a.transfer_cursor, 0);
        assert!(logged(&a, "cancelled: f0"));
    }

    #[test]
    fn declining_a_delete_keeps_the_file() {
        for code in [KeyCode::Char('n'), KeyCode::Esc] {
            let mut a = app_on_main(1);
            a.pending_delete = Some(PendingDelete {
                name: "x".into(),
                is_dir: false,
                remote_path: "/x".into(),
            });
            a.screen = Screen::ConfirmDelete;
            press(&mut a, code);
            assert_eq!(a.screen, Screen::Main);
            assert!(a.pending_delete.is_none());
        }
    }

    #[test]
    fn declining_an_overwrite_drops_the_rename_and_says_so() {
        let mut a = app_on_main(1);
        a.rename_input = "b".into();
        a.rename_original = "a".into();
        a.pending_overwrite = Some(OverwritePending::Rename {
            from: "/a".into(),
            to: "/b".into(),
            target_name: "b".into(),
        });
        a.screen = Screen::ConfirmOverwrite;

        press(&mut a, KeyCode::Esc);

        assert_eq!(a.screen, Screen::Main);
        assert!(a.pending_overwrite.is_none());
        assert!(a.rename_input.is_empty());
        assert!(a.rename_original.is_empty());
        assert!(logged(&a, "overwrite cancelled"));
    }

    #[test]
    fn other_keys_leave_an_overwrite_question_open() {
        let mut a = app_on_main(1);
        a.pending_overwrite = Some(OverwritePending::Rename {
            from: "/a".into(),
            to: "/b".into(),
            target_name: "b".into(),
        });
        a.screen = Screen::ConfirmOverwrite;
        press(&mut a, KeyCode::Char('x'));
        assert_eq!(a.screen, Screen::ConfirmOverwrite);
        assert!(a.pending_overwrite.is_some());
    }

    #[test]
    fn rename_and_mkdir_forms_edit_and_cancel() {
        let mut a = app_on_main(1);
        a.screen = Screen::Rename;
        a.rename_error = Some("old".into());
        typed(&mut a, "ab");
        assert_eq!(a.rename_input, "ab");
        assert!(a.rename_error.is_none());
        press(&mut a, KeyCode::Esc);
        assert_eq!(a.screen, Screen::Main);

        a.screen = Screen::Mkdir;
        a.mkdir_error = Some("old".into());
        typed(&mut a, "dir");
        assert_eq!(a.mkdir_input, "dir");
        assert!(a.mkdir_error.is_none());
        press(&mut a, KeyCode::Esc);
        assert_eq!(a.screen, Screen::Main);
        assert!(a.mkdir_input.is_empty());
    }

    // --- search ---

    #[test]
    fn esc_clears_the_search_and_enter_keeps_it() {
        let mut a = app_on_main(20);
        press(&mut a, KeyCode::Char('/'));
        typed(&mut a, "file01");
        assert_eq!(a.local.entries.len(), 10);
        press(&mut a, KeyCode::PageDown);
        assert_eq!(a.local.cursor, 9);
        press(&mut a, KeyCode::PageUp);
        assert_eq!(a.local.cursor, 0);
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.screen, Screen::Main);
        assert_eq!(a.local.entries.len(), 10, "enter keeps the filter");

        press(&mut a, KeyCode::Char('/'));
        assert_eq!(a.search_input, "file01", "reopening shows the filter");
        press(&mut a, KeyCode::Esc);
        assert_eq!(a.screen, Screen::Main);
        assert_eq!(a.local.entries.len(), 20);
        assert!(a.search_input.is_empty());
    }

    // --- save session ---

    fn saving(name: &str) -> App {
        let mut a = app_on_main(1);
        a.current_session = Some(session("sftp://me@h.example"));
        a.screen = Screen::SaveSession;
        a.save_session_input = name.into();
        a
    }

    #[test]
    fn esc_closes_the_save_form() {
        let mut a = saving("x");
        a.save_session_error = Some("old".into());
        press(&mut a, KeyCode::Esc);
        assert_eq!(a.screen, Screen::Main);
        assert!(a.save_session_error.is_none());
    }

    #[test]
    fn a_blank_name_is_refused() {
        let mut a = saving("   ");
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.screen, Screen::SaveSession);
        assert_eq!(
            a.save_session_error.as_deref(),
            Some("name cannot be empty")
        );

        typed(&mut a, "w");
        assert!(a.save_session_error.is_none(), "typing clears the error");
    }

    #[test]
    fn saving_with_no_connection_is_refused() {
        let mut a = saving("work");
        a.current_session = None;
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.save_session_error.as_deref(), Some("no active session"));
    }

    #[test]
    fn saving_records_where_you_are() {
        let _home = crate::paths::test_home();
        let mut a = saving(" work ");
        a.remote.path = String::new();

        press(&mut a, KeyCode::Enter);

        assert_eq!(a.screen, Screen::Main);
        assert!(logged(&a, "session saved: work"));
        let saved = Session::list_all().unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "work");
        assert_eq!(saved[0].remote_dir, "/");
        assert_eq!(
            saved[0].local_dir.as_deref(),
            Some(std::path::Path::new("/l"))
        );
        assert_eq!(a.current_session.as_ref().unwrap().name, "work");
        assert_eq!(a.sessions.len(), 1, "the selector list is reloaded");
    }

    #[test]
    fn a_name_that_cannot_be_saved_says_why() {
        let _home = crate::paths::test_home();
        let mut a = saving("bad\nname");
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.screen, Screen::SaveSession);
        let err = a.save_session_error.clone().unwrap();
        assert!(err.contains("newlines"), "{err}");
    }

    // --- viewer ---

    fn viewing(kind: ViewerKind) -> App {
        let mut a = app_on_main(1);
        a.viewer = Some(Viewer {
            name: "f".into(),
            kind,
            id: 1,
        });
        a.previous_screen = Screen::Main;
        a.screen = Screen::Viewer;
        a
    }

    fn text_lines(n: usize) -> ViewerKind {
        ViewerKind::Text {
            tokens: vec![Vec::new(); n],
            scroll: 0,
        }
    }

    fn scroll_of(a: &App) -> usize {
        match &a.viewer.as_ref().unwrap().kind {
            ViewerKind::Text { scroll, .. } => *scroll,
            _ => panic!("not text"),
        }
    }

    /// End used to put the last line at the top, leaving a page of one
    /// line; it stops with the last line at the bottom, as help does.
    #[test]
    fn the_viewer_scrolls_no_further_than_its_last_full_page() {
        let mut a = viewing(text_lines(100));
        super::super::tests_support::draw(&a);
        let rows = usize::from(a.hit.viewer_body.get().unwrap().height);
        press(&mut a, KeyCode::End);
        assert_eq!(scroll_of(&a), 100 - rows);
        press(&mut a, KeyCode::Down);
        assert_eq!(scroll_of(&a), 100 - rows);
        press(&mut a, KeyCode::Up);
        assert_eq!(scroll_of(&a), 99 - rows, "one line up moves at once");
    }

    #[test]
    fn viewer_keys_scroll_and_stop_at_both_ends() {
        let mut a = viewing(text_lines(50));
        let steps: [(KeyCode, usize); 11] = [
            (KeyCode::Char('j'), 1),
            (KeyCode::Down, 2),
            (KeyCode::Char('k'), 1),
            (KeyCode::Up, 0),
            (KeyCode::Up, 0),
            (KeyCode::PageDown, 20),
            (KeyCode::Char(' '), 40),
            (KeyCode::PageDown, 49),
            (KeyCode::PageUp, 29),
            (KeyCode::Home, 0),
            (KeyCode::End, 49),
        ];
        for (code, want) in steps {
            press(&mut a, code);
            assert_eq!(scroll_of(&a), want, "after {code:?}");
        }
        press(&mut a, KeyCode::Char('g'));
        assert_eq!(scroll_of(&a), 0);
        press(&mut a, KeyCode::Char('G'));
        assert_eq!(scroll_of(&a), 49);
    }

    #[test]
    fn scrolling_a_viewer_that_is_not_text_does_nothing() {
        let mut a = viewing(ViewerKind::Loading);
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::End);
        assert!(matches!(
            a.viewer.as_ref().unwrap().kind,
            ViewerKind::Loading
        ));
    }

    #[test]
    fn closing_a_text_viewer_needs_no_repaint() {
        let mut a = viewing(text_lines(3));
        a.image_needs_redraw = true;
        press(&mut a, KeyCode::Char('q'));
        assert_eq!(a.screen, Screen::Main);
        assert!(a.viewer.is_none());
        assert!(!a.image_needs_redraw);
        assert!(!a.needs_terminal_clear);
    }

    #[test]
    fn closing_an_image_clears_the_terminal() {
        let mut a = viewing(ViewerKind::Image {
            bytes: bytes::Bytes::new(),
            render: Default::default(),
        });
        press(&mut a, KeyCode::Esc);
        assert!(a.viewer.is_none());
        assert!(
            a.needs_terminal_clear,
            "graphics aren't in ratatui's buffer"
        );
    }

    // --- post-connect offers ---

    #[test]
    fn declining_the_save_offer_says_how_to_save_later() {
        let mut a = app_on_main(1);
        super::super::tests_support::queue_offer(&mut a, PostConnectOffer::SaveSession);
        a.screen = Screen::OfferSaveSession;
        press(&mut a, KeyCode::Char('x'));
        assert_eq!(
            a.screen,
            Screen::OfferSaveSession,
            "unlisted keys are ignored"
        );

        press(&mut a, KeyCode::Esc);

        assert_eq!(a.screen, Screen::Main);
        assert!(a.pending_offers.is_empty());
        assert!(logged(&a, "press ctrl+s any time"));
    }

    fn offer(kind: CheckpointKind) -> PostConnectOffer {
        PostConnectOffer::ResumeCheckpoint(CheckpointOffer {
            kind,
            session: "work".into(),
            origin: "me@h:22".into(),
            remaining: 1,
            total: 2,
            age: None,
            sample_paths: vec![],
        })
    }

    #[test]
    fn a_resume_offer_with_nothing_queued_moves_on() {
        let mut a = app_on_main(1);
        a.screen = Screen::OfferResumeCheckpoint;
        press(&mut a, KeyCode::Char('r'));
        assert_eq!(a.screen, Screen::Main);
    }

    #[test]
    fn keeping_a_checkpoint_says_which_key_resumes_it() {
        for (kind, hint) in [
            (CheckpointKind::Download, "press r in"),
            (CheckpointKind::Upload, "press R in"),
        ] {
            let mut a = app_on_main(1);
            super::super::tests_support::queue_offer(&mut a, offer(kind));
            super::super::tests_support::queue_offer(&mut a, PostConnectOffer::SaveSession);
            a.screen = Screen::OfferResumeCheckpoint;

            press(&mut a, KeyCode::Esc);

            assert!(logged(&a, hint), "{kind:?}");
            assert_eq!(a.screen, Screen::OfferSaveSession, "the next offer follows");
        }
    }

    #[test]
    fn discarding_a_checkpoint_says_so() {
        let _home = crate::paths::test_home();
        let mut a = app_on_main(1);
        super::super::tests_support::queue_offer(&mut a, offer(CheckpointKind::Download));
        a.screen = Screen::OfferResumeCheckpoint;

        press(&mut a, KeyCode::Char('d'));

        assert_eq!(a.screen, Screen::Main);
        assert!(a.pending_offers.is_empty());
        assert!(
            logged(&a, "discarded the download checkpoint"),
            "{:?}",
            a.log
        );
    }

    #[test]
    fn other_keys_leave_the_resume_offer_up() {
        let mut a = app_on_main(1);
        super::super::tests_support::queue_offer(&mut a, offer(CheckpointKind::Upload));
        a.screen = Screen::OfferResumeCheckpoint;
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.screen, Screen::OfferResumeCheckpoint);
        assert_eq!(a.pending_offers.len(), 1);
    }

    // --- disconnect ---

    #[test]
    fn declining_to_disconnect_stays_connected() {
        for code in [KeyCode::Char('n'), KeyCode::Esc] {
            let mut a = app_on_main(1);
            a.current_session = Some(session("sftp://me@h"));
            a.screen = Screen::ConfirmDisconnect;
            press(&mut a, code);
            assert_eq!(a.screen, Screen::Main);
            assert!(a.current_session.is_some());
        }
    }

    #[tokio::test]
    async fn confirming_a_disconnect_returns_to_the_selector() {
        let mut a = app_on_main(1);
        a.current_session = Some(session("sftp://me@h"));
        with_running(&mut a, 1);
        a.screen = Screen::ConfirmDisconnect;

        press(&mut a, KeyCode::Char('y'));

        assert_eq!(a.screen, Screen::SessionSelect);
        assert!(a.current_session.is_none());
        assert!(a.transfer_manager.is_none());
    }

    // --- returning from overlays ---------------------------------------------

    #[test]
    fn help_over_the_cancel_prompt_returns_to_it_and_no_still_backs_out() {
        let mut a = app_on_main(1);
        a.pending_cancel = Some(PendingCancel::Single {
            id: 1,
            name: "f".into(),
        });
        a.previous_screen = Screen::Main;
        a.screen = Screen::ConfirmCancel;

        press(&mut a, KeyCode::Char('?'));
        assert_eq!(a.screen, Screen::Help);
        press(&mut a, KeyCode::Esc);
        assert_eq!(a.screen, Screen::ConfirmCancel);
        press(&mut a, KeyCode::Char('n'));
        assert_eq!(a.screen, Screen::Main, "the prompt returned to itself");
    }

    #[test]
    fn the_quit_prompt_does_not_open_help_and_no_still_backs_out() {
        let mut a = app_on_main(1);
        press(&mut a, KeyCode::Char('q'));
        press(&mut a, KeyCode::Char('?'));
        assert_eq!(a.screen, Screen::ConfirmQuit);
        press(&mut a, KeyCode::Char('n'));
        assert_eq!(a.screen, Screen::Main);
    }

    #[test]
    fn help_opened_from_the_selector_draws_the_selector_behind_it() {
        let mut a = app();
        press(&mut a, KeyCode::Char('?'));
        // Tall enough that the selector's heading shows above the modal.
        let buf = super::super::tests_support::draw_sized(&a, 120, 70);
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("terminal sftp / scp"), "{text}");
        assert!(
            !text.contains("not connected"),
            "the main view's header: {text}"
        );
    }

    // --- ctrl+c ---------------------------------------------------------------

    #[test]
    fn ctrl_c_asks_to_quit_from_the_main_view() {
        let mut a = app_on_main(1);
        a.active_pane = Pane::Transfers;
        a.dispatch_key(ctrl('c'));
        assert_eq!(a.screen, Screen::ConfirmQuit);
        assert!(a.pending_cancel.is_none(), "ctrl+c is not `c`");
        press(&mut a, KeyCode::Char('n'));
        assert_eq!(a.screen, Screen::Main);
    }

    #[test]
    fn ctrl_c_over_a_form_returns_to_the_form_on_no() {
        let mut a = app_on_main(1);
        a.screen = Screen::Mkdir;
        typed(&mut a, "new");
        a.dispatch_key(ctrl('c'));
        assert_eq!(a.screen, Screen::ConfirmQuit);
        press(&mut a, KeyCode::Esc);
        assert_eq!(a.screen, Screen::Mkdir);
        assert_eq!(a.mkdir_input, "new");
    }

    #[test]
    fn ctrl_c_on_the_quit_prompt_quits() {
        let mut a = app();
        a.dispatch_key(ctrl('c'));
        assert!(!a.should_quit);
        a.dispatch_key(ctrl('c'));
        assert!(a.should_quit);
    }

    #[test]
    fn ctrl_c_quits_at_once_with_the_confirmation_off() {
        let mut a = app();
        a.config.general.confirm_quit = false;
        a.screen = Screen::PasswordPrompt;
        a.dispatch_key(ctrl('c'));
        assert!(a.should_quit);
    }

    #[test]
    fn ctrl_c_on_help_then_no_returns_to_help_and_esc_to_main() {
        let mut a = app_on_main(1);
        press(&mut a, KeyCode::Char('?'));
        a.dispatch_key(ctrl('c'));
        press(&mut a, KeyCode::Char('n'));
        assert_eq!(a.screen, Screen::Help);
        press(&mut a, KeyCode::Esc);
        assert_eq!(a.screen, Screen::Main);
    }

    #[test]
    fn ctrl_c_in_the_viewer_closes_it_before_asking() {
        let mut a = app_on_main(1);
        a.viewer = Some(Viewer {
            name: "f".into(),
            kind: ViewerKind::Loading,
            id: 1,
        });
        a.previous_screen = Screen::Main;
        a.screen = Screen::Viewer;
        a.dispatch_key(ctrl('c'));
        assert_eq!(a.screen, Screen::ConfirmQuit);
        assert!(a.viewer.is_none());
        press(&mut a, KeyCode::Char('n'));
        assert_eq!(a.screen, Screen::Main);
    }

    // --- chords and text fields -------------------------------------------------

    #[test]
    fn a_question_mark_types_into_a_folder_name() {
        let mut a = app_on_main(1);
        a.screen = Screen::Mkdir;
        typed(&mut a, "why?");
        assert_eq!(a.screen, Screen::Mkdir);
        assert_eq!(a.mkdir_input, "why?");
    }

    #[test]
    fn ctrl_and_alt_chords_type_nothing() {
        let mut a = app_on_main(1);
        a.screen = Screen::Rename;
        a.rename_input = "a".into();
        a.dispatch_key(ctrl('w'));
        a.dispatch_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(a.rename_input, "a");

        a.screen = Screen::PasswordPrompt;
        a.pending_session = Some(session("sftp://me@h"));
        a.dispatch_key(ctrl('w'));
        assert!(a.password_input.is_empty());
    }

    #[test]
    fn altgr_characters_still_type() {
        // Windows reports AltGr as ctrl+alt.
        let mut a = app_on_main(1);
        a.screen = Screen::Mkdir;
        a.dispatch_key(KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(a.mkdir_input, "@");
    }

    #[test]
    fn letter_shortcuts_ignore_ctrl_and_alt_chords() {
        let mut a = app_on_main(1);
        a.dispatch_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT));
        a.dispatch_key(ctrl('q'));
        assert_eq!(a.screen, Screen::Main);
        a.active_pane = Pane::Remote;
        a.dispatch_key(ctrl('D'));
        assert_eq!(a.screen, Screen::Main);

        let mut a = app();
        a.dispatch_key(ctrl('n'));
        assert_eq!(a.screen, Screen::SessionSelect);
    }

    // --- overwrite prompt ---------------------------------------------------------

    #[test]
    fn s_on_the_rename_overwrite_prompt_does_nothing() {
        let mut a = app_on_main(1);
        a.pending_overwrite = Some(OverwritePending::Rename {
            from: "/a".into(),
            to: "/b".into(),
            target_name: "b".into(),
        });
        a.screen = Screen::ConfirmOverwrite;
        press(&mut a, KeyCode::Char('s'));
        assert_eq!(a.screen, Screen::ConfirmOverwrite);
        assert!(a.pending_overwrite.is_some());
        assert!(!logged(&a, "not connected"), "a rename was attempted");
    }
}
