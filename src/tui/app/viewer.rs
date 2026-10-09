//! File viewer: open / scroll / image redraw.
//!
//! The viewer modal lives across two surfaces:
//!
//! - **Text path** — bytes get decoded (with NFO/CP437 fallback in
//!   `events.rs`), tokenised once via [`tokenize_lines`], and rendered
//!   by `views::viewer::render` straight out of the cached token list.
//! - **Image path** — bytes stay raw. Rendering them for the terminal's
//!   graphics protocol runs on the blocking pool (`image_output_for`), and
//!   `after_draw` writes the finished escape codes on top of ratatui's
//!   diff'd buffer. ratatui won't redraw cells it considers unchanged, so
//!   the image persists until `image_needs_redraw` flips back to true
//!   (initial open, resize, a render arriving).

use bytes::Bytes;
use ratatui::layout::Rect;

use crate::preview::{self, FileViewKind};
use crate::transport;
use crate::tui::TuiTerminal;
use crate::tui::event::AppEvent;
use crate::tui::state::{ImageArea, ViewSource, Viewer, ViewerKind};

use super::{App, LogLevel, Pane, Screen};

impl App {
    /// Handle 'v' on the main view: classify the cursor file, open the viewer
    /// modal in `Loading` state, and spawn the appropriate fetch task.
    pub(super) fn handle_view_request(&mut self) {
        // `raw_name` addresses the file; `name` is what the modal and the log
        // show. Keeping them apart matters most here: the viewer is the one
        // place a file's own bytes are rendered, so a name that round-tripped
        // through sanitization would open the wrong file (or none).
        let (raw_name, name, size, source) = match self.active_pane {
            Pane::Local => {
                let entry = match self.local.entries.get(self.local.cursor) {
                    Some(e) if !e.is_dir => e.clone(),
                    _ => return,
                };
                (
                    entry.raw_name,
                    entry.display_name,
                    entry.size,
                    ViewSource::Local,
                )
            }
            Pane::Remote => {
                let entry = match self.remote.entries.get(self.remote.cursor) {
                    Some(e) if !e.is_dir => e.clone(),
                    _ => return,
                };
                (
                    entry.raw_name,
                    entry.display_name,
                    entry.size,
                    ViewSource::Remote,
                )
            }
            Pane::Log | Pane::Transfers => return,
        };

        // Classify on the real name: the extension is what decides text vs.
        // image, and it has to be the extension the file actually has.
        let kind = preview::detect_view_kind(&raw_name, size);
        if let FileViewKind::Unsupported(reason) = &kind {
            self.push_log(LogLevel::Warn, format!("can't view {name}: {reason}"));
            return;
        }

        // Open the modal in Loading state. Subsequent ViewLoaded / ViewFailed
        // events carrying this `id` populate `kind`.
        let id = self.next_viewer_id;
        self.next_viewer_id += 1;
        self.viewer = Some(Viewer {
            name: name.clone(),
            kind: ViewerKind::Loading,
            id,
        });
        self.previous_screen = self.screen.clone();
        self.screen = Screen::Viewer;

        let tx = self.app_event_tx.clone();
        match source {
            ViewSource::Local => {
                let path = std::path::PathBuf::from(&self.local.path).join(&raw_name);
                let limit = preview::view_limit(&kind);
                tokio::spawn(async move {
                    let event = match read_local_bounded(&path, limit).await {
                        Ok(buf) => AppEvent::ViewLoaded {
                            viewer_id: id,
                            name,
                            kind,
                            bytes: Bytes::from(buf),
                        },
                        Err(e) => AppEvent::ViewFailed {
                            viewer_id: id,
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
                let Some(remote_path) = transport::join_remote(&self.remote.path, &raw_name) else {
                    self.viewer = None;
                    self.screen = self.previous_screen.clone();
                    self.push_log(
                        LogLevel::Warn,
                        format!("cannot view `{name}`: unusable name"),
                    );
                    return;
                };
                tokio::spawn(async move {
                    let mut transport = t.lock().await;
                    let event = match transport.read_to_bytes(&remote_path).await {
                        Ok(bytes) => AppEvent::ViewLoaded {
                            viewer_id: id,
                            name,
                            kind,
                            bytes,
                        },
                        Err(e) => AppEvent::ViewFailed {
                            viewer_id: id,
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
        if let Some(viewer) = self.viewer.as_mut()
            && let ViewerKind::Text { tokens, scroll } = &mut viewer.kind
        {
            let max = tokens.len().saturating_sub(1);
            let next = (*scroll as isize + delta).max(0) as usize;
            *scroll = next.min(max);
        }
    }

    pub(super) fn viewer_scroll_to(&mut self, target: usize) {
        if let Some(viewer) = self.viewer.as_mut()
            && let ViewerKind::Text { tokens, scroll } = &mut viewer.kind
        {
            let max = tokens.len().saturating_sub(1);
            *scroll = target.min(max);
        }
    }

    /// Called after each `terminal.draw` to emit graphics escape sequences
    /// for an active image viewer. Ratatui's diffing renderer leaves cells
    /// alone when their buffer contents don't change, so the image persists
    /// across ticks; we only need to re-emit when `image_needs_redraw` says
    /// so. Writing is all that happens here — see [`Self::image_output_for`].
    pub(super) fn after_draw(&mut self, terminal: &mut TuiTerminal) -> std::io::Result<()> {
        if !self.image_needs_redraw {
            return Ok(());
        }
        let size = terminal.size()?;
        let Some(area) = image_area(Rect::new(0, 0, size.width, size.height)) else {
            self.image_needs_redraw = false;
            return Ok(());
        };
        if let Some(escape) = self.image_output_for(area) {
            use std::io::Write;
            let mut stdout = std::io::stdout();
            stdout.write_all(&escape)?;
            stdout.flush()?;
        }
        Ok(())
    }

    /// What to write for the image viewer now, drawn into `area`, if
    /// anything.
    ///
    /// Rendering an image — decode, a Lanczos3 scale of up to 4096×4096, and
    /// encoding for the graphics protocol — ran right here, on the UI thread,
    /// on open and again on every resize, freezing the whole TUI (transfer
    /// events included) for as long as it took. Now a finished render for
    /// `area` is returned at once and reused on later redraws; otherwise one
    /// is started on the blocking pool, unless one is already running, and
    /// `image_rendered` takes the result. While it runs, resizes only change
    /// the area asked for: when the render lands, a result for an area no
    /// longer current is kept but not written, and the next call starts one
    /// render for the current area. However fast a window is dragged, that
    /// is one render running and at most one after it.
    pub(super) fn image_output_for(&mut self, area: ImageArea) -> Option<Vec<u8>> {
        if !self.image_needs_redraw {
            return None;
        }
        let Some(Viewer {
            kind: ViewerKind::Image { bytes, render },
            id,
            ..
        }) = self.viewer.as_mut()
        else {
            self.image_needs_redraw = false;
            return None;
        };
        if let Some((done_area, escape)) = &render.done
            && *done_area == area
        {
            self.image_needs_redraw = false;
            return Some(escape.clone());
        }
        if render.in_flight.is_none() {
            render.in_flight = Some(area);
            let proto = preview::detect(self.config.terminal.image_preview);
            let bytes = bytes.clone();
            let viewer_id = *id;
            let tx = self.app_event_tx.clone();
            tokio::task::spawn_blocking(move || {
                let result = match preview::backend_for(proto) {
                    Some(backend) => backend
                        .render(&bytes, area.x, area.y, area.w, area.h)
                        .map_err(|e| e.to_string()),
                    None => Err("no supported graphics protocol".to_string()),
                };
                let _ = tx.send(AppEvent::ImageRendered {
                    viewer_id,
                    area,
                    result,
                });
            });
        }
        // Still owed: the next draw asks again.
        None
    }

    /// Take a finished image render. One for a viewer since closed, or since
    /// showing another file, is dropped. A failure replaces the image with
    /// the reason, rather than leaving "rendering image…" up for good.
    pub(super) fn image_rendered(
        &mut self,
        viewer_id: u64,
        area: ImageArea,
        result: Result<Vec<u8>, String>,
    ) {
        let Some(viewer) = self.viewer.as_mut().filter(|v| v.id == viewer_id) else {
            return;
        };
        let ViewerKind::Image { render, .. } = &mut viewer.kind else {
            return;
        };
        render.in_flight = None;
        match result {
            Ok(escape) => {
                render.done = Some((area, escape));
                self.image_needs_redraw = true;
            }
            Err(e) => {
                viewer.kind = ViewerKind::Unsupported(format!("image preview failed: {e}"));
                self.image_needs_redraw = false;
                self.push_log(LogLevel::Warn, format!("image preview: {e}"));
            }
        }
    }
}

/// The cells an image is drawn into for a terminal of size `full`: the
/// viewer modal less its border and the hint strip, as `views::viewer`
/// lays it out. `None` when nothing fits.
fn image_area(full: Rect) -> Option<ImageArea> {
    let modal = crate::tui::views::centered_rect(85, 85, full);
    let area = ImageArea {
        x: modal.x.saturating_add(1),
        y: modal.y.saturating_add(1),
        w: modal.width.saturating_sub(2),
        h: modal.height.saturating_sub(2).saturating_sub(1),
    };
    (area.w > 0 && area.h > 0).then_some(area)
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

/// Read at most `limit + 1` bytes of `path`: enough for the viewer to tell
/// a file over its limit from one at it, however large the file has grown
/// since it was listed — or if it never ends, like a FIFO.
pub(super) async fn read_local_bounded(
    path: &std::path::Path,
    limit: u64,
) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    let mut buf = Vec::new();
    tokio::fs::File::open(path)
        .await?
        .take(limit + 1)
        .read_to_end(&mut buf)
        .await?;
    Ok(buf)
}
