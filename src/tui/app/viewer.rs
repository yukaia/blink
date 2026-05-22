//! File viewer: open / scroll / image redraw.
//!
//! The viewer modal lives across two surfaces:
//!
//! - **Text path** — bytes get decoded (with NFO/CP437 fallback in
//!   `events.rs`), tokenised once via [`tokenize_lines`], and rendered
//!   by `views::viewer::render` straight out of the cached token list.
//! - **Image path** — bytes stay raw; `after_draw` emits graphics
//!   escape codes on top of ratatui's diff'd buffer. ratatui won't
//!   redraw cells it considers unchanged, so the image persists until
//!   `image_needs_redraw` flips back to true (initial open, resize).

use bytes::Bytes;
use ratatui::layout::Rect;

use crate::preview::{self, FileViewKind};
use crate::transport;
use crate::tui::event::AppEvent;
use crate::tui::state::{ViewSource, Viewer, ViewerKind};
use crate::tui::TuiTerminal;

use super::{App, LogLevel, Pane, Screen};

impl App {
    /// Handle 'v' on the main view: classify the cursor file, open the viewer
    /// modal in `Loading` state, and spawn the appropriate fetch task.
    pub(super) fn handle_view_request(&mut self) {
        let (name, size, source) = match self.active_pane {
            Pane::Local => {
                let entry = match self.local.entries.get(self.local.cursor) {
                    Some(e) if !e.is_dir => e.clone(),
                    _ => return,
                };
                (entry.name.clone(), entry.size, ViewSource::Local)
            }
            Pane::Remote => {
                let entry = match self.remote.entries.get(self.remote.cursor) {
                    Some(e) if !e.is_dir => e.clone(),
                    _ => return,
                };
                (entry.name.clone(), entry.size, ViewSource::Remote)
            }
            Pane::Log | Pane::Transfers => return,
        };

        let kind = preview::detect_view_kind(&name, size);
        if let FileViewKind::Unsupported(reason) = &kind {
            self.push_log(
                LogLevel::Warn,
                format!("can't view {name}: {reason}"),
            );
            return;
        }

        // Open the modal in Loading state. Subsequent ViewLoaded / ViewFailed
        // events populate `kind`.
        self.viewer = Some(Viewer {
            name: name.clone(),
            kind: ViewerKind::Loading,
        });
        self.previous_screen = self.screen.clone();
        self.screen = Screen::Viewer;

        let tx = self.app_event_tx.clone();
        match source {
            ViewSource::Local => {
                let path = std::path::PathBuf::from(&self.local.path).join(&name);
                tokio::spawn(async move {
                    let event = match tokio::fs::read(&path).await {
                        Ok(buf) => AppEvent::ViewLoaded {
                            name,
                            kind,
                            bytes: Bytes::from(buf),
                        },
                        Err(e) => AppEvent::ViewFailed {
                            name,
                            error: e.to_string(),
                        },
                    };
                    let _ = tx.send(event);
                });
            }
            ViewSource::Remote => {
                let Some(t) = self.transport.clone() else {
                    self.viewer = None;
                    self.screen = self.previous_screen.clone();
                    return;
                };
                let remote_path = transport::join_remote(&self.remote.path, &name);
                tokio::spawn(async move {
                    let mut transport = t.lock().await;
                    let event = match transport.read_to_bytes(&remote_path).await {
                        Ok(bytes) => AppEvent::ViewLoaded { name, kind, bytes },
                        Err(e) => AppEvent::ViewFailed {
                            name,
                            error: e.to_string(),
                        },
                    };
                    let _ = tx.send(event);
                });
            }
        }
    }

    pub(super) fn viewer_scroll(&mut self, delta: isize) {
        if let Some(viewer) = self.viewer.as_mut() {
            if let ViewerKind::Text { lines, scroll, .. } = &mut viewer.kind {
                let max = lines.len().saturating_sub(1);
                let next = (*scroll as isize + delta).max(0) as usize;
                *scroll = next.min(max);
            }
        }
    }

    pub(super) fn viewer_scroll_to(&mut self, target: usize) {
        if let Some(viewer) = self.viewer.as_mut() {
            if let ViewerKind::Text { lines, scroll, .. } = &mut viewer.kind {
                let max = lines.len().saturating_sub(1);
                *scroll = target.min(max);
            }
        }
    }

    /// Called after each `terminal.draw` to emit graphics escape sequences
    /// for an active image viewer. Ratatui's diffing renderer leaves cells
    /// alone when their buffer contents don't change, so the image persists
    /// across ticks; we only need to re-emit on first open and on resize
    /// (both gated by `image_needs_redraw`).
    pub(super) fn after_draw(&mut self, terminal: &mut TuiTerminal) -> std::io::Result<()> {
        if !self.image_needs_redraw {
            return Ok(());
        }
        let Some(viewer) = &self.viewer else {
            self.image_needs_redraw = false;
            return Ok(());
        };
        let ViewerKind::Image { bytes } = &viewer.kind else {
            self.image_needs_redraw = false;
            return Ok(());
        };

        let size = terminal.size()?;
        let full = Rect::new(0, 0, size.width, size.height);
        let modal = crate::tui::views::centered_rect(85, 85, full);

        // Match the layout in views::viewer::render: borders take 1 cell on
        // each side, then we reserve the bottom row of the inside for the
        // hint strip. The image draws into what's left.
        let body_x = modal.x.saturating_add(1);
        let body_y = modal.y.saturating_add(1);
        let body_w = modal.width.saturating_sub(2);
        let body_h = modal.height.saturating_sub(2).saturating_sub(1);
        if body_w == 0 || body_h == 0 {
            self.image_needs_redraw = false;
            return Ok(());
        }

        let proto = preview::detect(self.config.terminal.image_preview);
        let backend = match preview::backend_for(proto) {
            Some(b) => b,
            None => {
                self.image_needs_redraw = false;
                return Ok(());
            }
        };

        let escape = match backend.render(bytes, body_x, body_y, body_w, body_h) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.push_log(LogLevel::Warn, format!("image preview: {e}"));
                self.image_needs_redraw = false;
                return Ok(());
            }
        };

        use std::io::Write;
        let mut stdout = std::io::stdout();
        stdout.write_all(&escape)?;
        stdout.flush()?;
        self.image_needs_redraw = false;
        let _ = terminal; // not needed beyond the size() call
        Ok(())
    }
}

/// Tokenise every line of a file once, at view-load time, so per-frame
/// rendering becomes an array lookup instead of replaying the highlighter
/// from line 0. The viewer redraws on the 100 ms TUI tick — without this
/// cache, a 10k-line file scrolled to the bottom does ~10k tokenize calls
/// per frame just to reach the visible region.
pub(super) fn tokenize_lines(
    name: &str,
    lines: &[String],
) -> Vec<Vec<(crate::highlight::TokenKind, String)>> {
    let lang = crate::highlight::lang_for_name(name);
    let mut state = crate::highlight::LineState::default();
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let (tokens, ns) = crate::highlight::tokenize(lang, line, state);
        state = ns;
        out.push(tokens);
    }
    out
}
