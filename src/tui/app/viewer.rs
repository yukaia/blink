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
                if entry.lossy_name {
                    self.report_unopenable(&entry.display_name);
                    return;
                }
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
        let rows = self.viewer_rows();
        if let Some(viewer) = self.viewer.as_mut()
            && let ViewerKind::Text { tokens, scroll } = &mut viewer.kind
        {
            *scroll = scroll
                .saturating_add_signed(delta)
                .min(text_scroll_limit(tokens.len(), rows));
        }
    }

    pub(super) fn viewer_scroll_to(&mut self, target: usize) {
        let rows = self.viewer_rows();
        if let Some(viewer) = self.viewer.as_mut()
            && let ViewerKind::Text { tokens, scroll } = &mut viewer.kind
        {
            *scroll = target.min(text_scroll_limit(tokens.len(), rows));
        }
    }

    /// Lines of text the viewer showed when last drawn; one before then.
    fn viewer_rows(&self) -> usize {
        self.hit
            .get()
            .viewer_body
            .map_or(1, |r| usize::from(r.height).max(1))
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

/// The furthest a text of `lines` lines scrolls in a viewer `rows` tall:
/// its last line at the bottom, not the top. Shared with the renderer.
pub(crate) fn text_scroll_limit(lines: usize, rows: usize) -> usize {
    lines.saturating_sub(rows.max(1))
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
/// since it was listed.
///
/// Only a regular file is read. Opening a FIFO waits for a writer and
/// reading a device may never end, so on Unix the open doesn't block, and
/// anything that turns out not to be a file is refused.
pub(super) async fn read_local_bounded(
    path: &std::path::Path,
    limit: u64,
) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    let mut opts = tokio::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    opts.custom_flags(libc::O_NONBLOCK);
    let file = opts.open(path).await?;
    if !file.metadata().await?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let mut buf = Vec::new();
    file.take(limit + 1).read_to_end(&mut buf).await?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::{image_area, read_local_bounded};
    use crate::config::ImagePreviewMode;
    use crate::tui::app::tests_support::{app_on_main, draw};
    use crate::tui::app::{App, Pane, Screen};
    use crate::tui::state::{ImageArea, ImageRender, PaneEntry, Viewer, ViewerKind};
    use ratatui::layout::Rect;

    fn logged(a: &App, needle: &str) -> bool {
        a.log.iter().any(|l| l.message.contains(needle))
    }

    // --- opening a file ---

    /// An app whose local pane is a real folder holding `files`.
    fn app_in(home: &crate::paths::TestHome, files: &[(&str, &[u8])]) -> App {
        for (name, body) in files {
            std::fs::write(home.path().join(name), body).unwrap();
        }
        let mut a = app_on_main(0);
        a.local.path = home.path().display().to_string();
        a.local.set_entries(
            files
                .iter()
                .map(|(n, b)| PaneEntry::new(n.to_string(), false, b.len() as u64))
                .collect(),
        );
        a
    }

    #[tokio::test]
    async fn viewing_a_local_text_file_loads_it_into_the_viewer() {
        let home = crate::paths::test_home();
        let mut a = app_in(&home, &[("notes.txt", b"one\ntwo\nthree\n")]);
        let mut events = a.app_event_rx.take().unwrap();

        a.handle_view_request();

        assert_eq!(a.screen, Screen::Viewer);
        assert_eq!(a.previous_screen, Screen::Main);
        let v = a.viewer.as_ref().unwrap();
        assert_eq!(v.name, "notes.txt");
        assert!(matches!(v.kind, ViewerKind::Loading));

        let event = events.recv().await.unwrap();
        a.handle_app_event(event);
        match &a.viewer.as_ref().unwrap().kind {
            ViewerKind::Text { tokens, scroll } => {
                assert_eq!(tokens.len(), 3);
                assert_eq!(*scroll, 0);
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_missing_local_file_shows_why_in_the_viewer() {
        let home = crate::paths::test_home();
        let mut a = app_in(&home, &[]);
        a.local
            .set_entries(vec![PaneEntry::new("gone.txt".into(), false, 3)]);
        let mut events = a.app_event_rx.take().unwrap();

        a.handle_view_request();
        a.handle_app_event(events.recv().await.unwrap());

        assert!(
            matches!(a.viewer.as_ref().unwrap().kind, ViewerKind::Unsupported(_)),
            "{:?}",
            a.viewer.as_ref().unwrap().kind
        );
    }

    #[test]
    fn a_folder_cannot_be_viewed() {
        let mut a = app_on_main(0);
        a.local
            .set_entries(vec![PaneEntry::new("dir".into(), true, 0)]);
        a.handle_view_request();
        assert_eq!(a.screen, Screen::Main);
        assert!(a.viewer.is_none());
    }

    #[test]
    fn an_unsupported_file_is_refused_with_a_reason() {
        let mut a = app_on_main(0);
        a.local
            .set_entries(vec![PaneEntry::new("disk.iso".into(), false, 10)]);
        a.handle_view_request();
        assert_eq!(a.screen, Screen::Main);
        assert!(logged(&a, "can't view disk.iso: unsupported file type"));
    }

    #[test]
    fn a_file_whose_name_is_not_utf8_cannot_be_opened() {
        let mut a = app_on_main(0);
        let mut e = PaneEntry::new("bad.txt".into(), false, 10);
        e.lossy_name = true;
        a.local.set_entries(vec![e]);
        a.handle_view_request();
        assert!(a.viewer.is_none());
        assert!(logged(&a, "not valid UTF-8"));
    }

    #[test]
    fn a_remote_file_without_a_connection_does_not_open_the_viewer() {
        let mut a = app_on_main(0);
        a.active_pane = Pane::Remote;
        a.remote
            .set_entries(vec![PaneEntry::new("r.txt".into(), false, 10)]);
        a.handle_view_request();
        assert_eq!(a.screen, Screen::Main);
        assert!(a.viewer.is_none());
    }

    #[test]
    fn the_bottom_panes_have_nothing_to_view() {
        let mut a = app_on_main(3);
        for pane in [Pane::Log, Pane::Transfers] {
            a.active_pane = pane;
            a.handle_view_request();
            assert!(a.viewer.is_none(), "{pane:?}");
        }
    }

    // --- image rendering ---

    fn viewing_image(render: ImageRender) -> App {
        let mut a = app_on_main(1);
        a.viewer = Some(Viewer {
            name: "i.png".into(),
            kind: ViewerKind::Image {
                bytes: bytes::Bytes::from_static(b"png"),
                render,
            },
            id: 7,
        });
        a.screen = Screen::Viewer;
        a.image_needs_redraw = true;
        a
    }

    const AREA: ImageArea = ImageArea {
        x: 1,
        y: 2,
        w: 30,
        h: 10,
    };

    /// The image is drawn on top of the cells the viewer leaves blank for
    /// it; the two layouts are computed separately and must agree.
    #[test]
    fn the_image_area_is_the_viewers_body() {
        let a = viewing_image(ImageRender::default());
        draw(&a);
        let body = a.hit.get().viewer_body.unwrap();
        let area = image_area(Rect::new(0, 0, 100, 30)).unwrap();
        assert_eq!(
            (area.x, area.y, area.w, area.h),
            (body.x, body.y, body.width, body.height)
        );
    }

    #[test]
    fn a_terminal_too_small_for_any_image_has_no_image_area() {
        assert!(image_area(Rect::new(0, 0, 2, 2)).is_none());
    }

    #[test]
    fn nothing_is_written_until_a_redraw_is_owed() {
        let mut a = viewing_image(ImageRender {
            done: Some((AREA, b"esc".to_vec())),
            in_flight: None,
        });
        a.image_needs_redraw = false;
        assert_eq!(a.image_output_for(AREA), None);
    }

    #[test]
    fn a_finished_render_for_this_area_is_written_once() {
        let mut a = viewing_image(ImageRender {
            done: Some((AREA, b"esc".to_vec())),
            in_flight: None,
        });
        assert_eq!(a.image_output_for(AREA), Some(b"esc".to_vec()));
        assert!(!a.image_needs_redraw);
        assert_eq!(a.image_output_for(AREA), None);
    }

    #[test]
    fn a_viewer_that_is_not_an_image_owes_nothing() {
        let mut a = viewing_image(ImageRender::default());
        a.viewer.as_mut().unwrap().kind = ViewerKind::Loading;
        assert_eq!(a.image_output_for(AREA), None);
        assert!(!a.image_needs_redraw);
    }

    #[test]
    fn a_render_already_running_is_not_started_twice() {
        let other = ImageArea { w: 99, ..AREA };
        let mut a = viewing_image(ImageRender {
            done: None,
            in_flight: Some(other),
        });
        // No runtime here: starting a render would panic.
        assert_eq!(a.image_output_for(AREA), None);
        let ViewerKind::Image { render, .. } = &a.viewer.as_ref().unwrap().kind else {
            unreachable!()
        };
        assert_eq!(render.in_flight, Some(other));
        assert!(a.image_needs_redraw, "still owed");
    }

    #[tokio::test]
    async fn a_render_with_no_graphics_protocol_fails_into_the_viewer() {
        let mut a = viewing_image(ImageRender::default());
        a.config.terminal.image_preview = ImagePreviewMode::None;
        let mut events = a.app_event_rx.take().unwrap();

        assert_eq!(a.image_output_for(AREA), None);
        let ViewerKind::Image { render, .. } = &a.viewer.as_ref().unwrap().kind else {
            unreachable!()
        };
        assert_eq!(render.in_flight, Some(AREA));

        a.handle_app_event(events.recv().await.unwrap());

        match &a.viewer.as_ref().unwrap().kind {
            ViewerKind::Unsupported(why) => {
                assert!(why.contains("no supported graphics protocol"), "{why}");
            }
            other => panic!("expected the failure, got {other:?}"),
        }
        assert!(!a.image_needs_redraw);
        assert!(logged(&a, "image preview: no supported graphics protocol"));
    }

    #[test]
    fn a_finished_render_is_kept_and_owed_a_redraw() {
        let mut a = viewing_image(ImageRender {
            done: None,
            in_flight: Some(AREA),
        });
        a.image_needs_redraw = false;
        a.image_rendered(7, AREA, Ok(b"esc".to_vec()));
        let ViewerKind::Image { render, .. } = &a.viewer.as_ref().unwrap().kind else {
            unreachable!()
        };
        assert_eq!(render.in_flight, None);
        assert_eq!(render.done, Some((AREA, b"esc".to_vec())));
        assert!(a.image_needs_redraw);
    }

    #[test]
    fn a_render_for_another_viewer_is_dropped() {
        let mut a = viewing_image(ImageRender {
            done: None,
            in_flight: Some(AREA),
        });
        a.image_rendered(8, AREA, Ok(b"esc".to_vec()));
        let ViewerKind::Image { render, .. } = &a.viewer.as_ref().unwrap().kind else {
            unreachable!()
        };
        assert_eq!(render.done, None);
        assert_eq!(render.in_flight, Some(AREA));
    }

    #[tokio::test]
    async fn a_file_is_read_up_to_one_byte_past_the_limit() {
        let home = crate::paths::test_home();
        let path = home.path().join("f.txt");
        std::fs::write(&path, b"0123456789").unwrap();
        assert_eq!(read_local_bounded(&path, 4).await.unwrap(), b"01234");
        assert_eq!(read_local_bounded(&path, 99).await.unwrap(), b"0123456789");
    }

    /// Opening a FIFO waits for a writer, so viewing one left the viewer on
    /// "loading" and a blocking thread held for as long as none came.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_fifo_is_refused_rather_than_waited_on() {
        use std::os::unix::fs::OpenOptionsExt;

        let home = crate::paths::test_home();
        let path = home.path().join("pipe.txt");
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);

        let read = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            read_local_bounded(&path, 100),
        )
        .await;
        // Release an open still waiting, so a failing run ends instead of
        // hanging on the runtime's shutdown.
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path);

        let err = read.expect("must not wait for a writer").unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }
}
