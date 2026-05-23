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

use crate::session::{AuthMethod, Session};
use crate::transfer::Direction;
use crate::transport;
use crate::tui::state::{EditField, PendingCancel, ViewerKind};

use super::{App, LogLevel, Pane, Screen};

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
        KeyCode::Char(c) => {
            buf.push(c);
            true
        }
        _ => false,
    }
}

impl App {
    pub(super) fn handle_session_select(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => {
                if self.session_cursor > 0 {
                    self.session_cursor -= 1;
                }
            }
            KeyCode::Down => {
                if self.session_cursor + 1 < self.sessions.len() {
                    self.session_cursor += 1;
                }
            }
            KeyCode::Enter => {
                let Some(s) = self.sessions.get(self.session_cursor).cloned() else {
                    return;
                };
                match &s.auth {
                    AuthMethod::Password => {
                        self.pending_session = Some(s);
                        self.password_input.clear();
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
                self.previous_screen = Screen::SessionSelect;
                self.screen = Screen::ConfirmQuit;
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
                        match &session.auth {
                            AuthMethod::Password => {
                                self.pending_session = Some(session);
                                self.password_input.clear();
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
                if let Some(f) = self.edit_session_form.as_mut() {
                    if let Some(v) = f.current_value_mut() {
                        if apply_text_edit(v, &key) {
                            f.error = None;
                        }
                    }
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
                            self.sessions = Session::list_all().unwrap_or_default();
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
                self.password_input.clear();
                self.pending_session = None;
                self.pending_password = None;
                self.screen = Screen::SessionSelect;
            }
            KeyCode::Enter => {
                let Some(session) = self.pending_session.clone() else {
                    self.screen = Screen::SessionSelect;
                    return;
                };
                let password = zeroize::Zeroizing::new(std::mem::take(&mut self.password_input));
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
                self.passphrase_input.clear();
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
                    self.passphrase_error =
                        Some("enter the passphrase or [esc] to cancel".into());
                    return;
                }
                let Some(session) = self.pending_session.clone() else {
                    self.screen = Screen::SessionSelect;
                    return;
                };
                let passphrase = zeroize::Zeroizing::new(std::mem::take(&mut self.passphrase_input));
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
        match key.code {
            KeyCode::Tab => self.cycle_pane(true),
            KeyCode::BackTab => self.cycle_pane(false),
            KeyCode::Up => match self.active_pane {
                Pane::Local | Pane::Remote => {
                    self.active_pane_mut().unwrap().move_cursor(-1)
                }
                Pane::Transfers => self.move_transfer_cursor(-1),
                Pane::Log => {}
            },
            KeyCode::Down => match self.active_pane {
                Pane::Local | Pane::Remote => {
                    self.active_pane_mut().unwrap().move_cursor(1)
                }
                Pane::Transfers => self.move_transfer_cursor(1),
                Pane::Log => {}
            },
            KeyCode::PageUp => match self.active_pane {
                Pane::Local | Pane::Remote => {
                    self.active_pane_mut().unwrap().move_cursor(-10)
                }
                Pane::Transfers => self.move_transfer_cursor(-10),
                Pane::Log => {}
            },
            KeyCode::PageDown => match self.active_pane {
                Pane::Local | Pane::Remote => {
                    self.active_pane_mut().unwrap().move_cursor(10)
                }
                Pane::Transfers => self.move_transfer_cursor(10),
                Pane::Log => {}
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
                self.resume_walk(Direction::Download);
            }
            KeyCode::Char('R') if self.active_pane == Pane::Transfers => {
                self.resume_walk(Direction::Upload);
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_save_session();
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.enqueue_selected_downloads();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_selected_uploads();
            }
            KeyCode::F(2) => {
                if self.active_pane == Pane::Remote {
                    self.open_rename();
                }
            }
            KeyCode::F(7) => {
                if self.active_pane == Pane::Remote {
                    self.open_mkdir();
                }
            }
            KeyCode::Delete if key.modifiers.contains(KeyModifiers::SHIFT) => {
                if self.active_pane == Pane::Remote {
                    self.open_delete();
                }
            }
            // 'D' (uppercase) as an alternative to Shift+Delete for terminals
            // that don't pass that combo cleanly.
            KeyCode::Char('D') => {
                if self.active_pane == Pane::Remote {
                    self.open_delete();
                }
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
            KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.transport.is_some() {
                    self.screen = Screen::ConfirmDisconnect;
                }
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                self.previous_screen = Screen::Main;
                self.screen = Screen::ConfirmQuit;
            }
            _ => {}
        }
    }

    pub(super) fn handle_confirm_cancel(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(pc) = self.pending_cancel.take() {
                    if let Some(manager) = &self.transfer_manager {
                        match pc {
                            PendingCancel::Single { id, name } => {
                                manager.cancel(id);
                                self.push_log(
                                    LogLevel::Warn,
                                    format!("cancelled: {name}"),
                                );
                                // Remove this job from the checkpoint map so a
                                // subsequent resume re-queues it (it was never
                                // completed, so it must not be skipped). The
                                // checkpoint file itself is kept — the rest of
                                // the batch can still complete and be tracked.
                                self.checkpoint_job_map.remove(&id);
                            }
                            PendingCancel::Batch { batch_id, .. } => {
                                let (active_n, pending_n) =
                                    manager.cancel_batch(batch_id);
                                self.push_log(
                                    LogLevel::Warn,
                                    format!(
                                        "cancelled batch: {} active + {} queued",
                                        active_n, pending_n
                                    ),
                                );
                                // The whole batch is being abandoned. Drop the
                                // checkpoint so stale files don't accumulate
                                // and a mistaken `r` / `R` doesn't re-queue a
                                // batch the user explicitly threw away.
                                self.discard_active_checkpoint();
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
        let decision = match key.code {
            // y / Y — accept and save to known_hosts
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(HostKeyDecision::AcceptAndSave),
            // t / T — trust once, don't save
            KeyCode::Char('t') | KeyCode::Char('T') => Some(HostKeyDecision::AcceptOnce),
            // n / N / Esc — reject
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                Some(HostKeyDecision::Reject)
            }
            _ => None,
        };

        if let Some(decision) = decision {
            if let Some(mut phk) = self.pending_host_key.take() {
                if let Some(tx) = phk.decision_tx.take() {
                    let _ = tx.send(decision);
                }
            }
            // Return to the Connection screen while we wait for the connect
            // task to proceed (or fail). The task is still blocked on the
            // oneshot; it will resume now that we've sent the decision.
            self.screen = match decision {
                HostKeyDecision::Reject => {
                    self.pending_session = None;
                    Screen::SessionSelect
                }
                _ => Screen::Connection,
            };
        }
    }

    pub(super) fn handle_rename(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.rename_input.clear();
                self.rename_original.clear();
                self.rename_error = None;
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
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                // Y always means "overwrite" / "proceed" for backward
                // compat with the old single-file modals. For plan modals
                // it's equivalent to "overwrite all conflicts".
                self.confirm_overwrite_proceed(false);
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                // Skip-conflicts only meaningful for plan variants; for
                // the rename / single-file variants there's nothing to
                // skip and we treat it as a no-op.
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

                // Snapshot the current navigation state into the saved session
                // so reopening picks up where we left off.
                let mut to_save = current;
                to_save.name = name.clone();
                to_save.remote_dir = if self.remote.path.is_empty() {
                    "/".to_string()
                } else {
                    self.remote.path.clone()
                };
                to_save.local_dir = Some(std::path::PathBuf::from(&self.local.path));

                match to_save.save() {
                    Ok(()) => {
                        self.current_session = Some(to_save);
                        // Refresh the sessions list so it reflects the new
                        // entry next time the user opens the selector.
                        self.sessions = Session::list_all().unwrap_or_default();
                        self.save_session_error = None;
                        self.screen = Screen::Main;
                        self.push_log(
                            LogLevel::Success,
                            format!("session saved: {name}"),
                        );
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
            KeyCode::Esc | KeyCode::Char('q') => {
                // If an image was on screen, force a full repaint after we
                // close — sixel and kitty graphics aren't in ratatui's buffer
                // and won't be cleaned up by the next diff'd draw.
                let was_image = matches!(
                    self.viewer.as_ref().map(|v| &v.kind),
                    Some(ViewerKind::Image { .. })
                );
                self.viewer = None;
                self.image_needs_redraw = false;
                if was_image {
                    self.needs_terminal_clear = true;
                }
                self.screen = self.previous_screen.clone();
            }
            KeyCode::Up | KeyCode::Char('k') => self.viewer_scroll(-1),
            KeyCode::Down | KeyCode::Char('j') => self.viewer_scroll(1),
            KeyCode::PageUp => self.viewer_scroll(-20),
            KeyCode::PageDown | KeyCode::Char(' ') => self.viewer_scroll(20),
            KeyCode::Home | KeyCode::Char('g') => self.viewer_scroll_to(0),
            KeyCode::End | KeyCode::Char('G') => self.viewer_scroll_to(usize::MAX),
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
