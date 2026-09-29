//! Mouse input: clicks, double-clicks and the wheel.
//!
//! Events are matched against the [`crate::tui::hit::HitMap`] the last draw
//! recorded. Only the main view, the viewer and the session selector take
//! the mouse; on every other screen — each modal and prompt — it does
//! nothing, including to the panes drawn behind the modal.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::{App, BottomPane, Pane, Screen};
use crate::tui::hit::row_in;
use crate::tui::state::PaneState;

/// Two presses on one entry within this long are a double-click.
pub(super) const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Rows (or viewer lines) one notch of the wheel moves.
pub(super) const WHEEL_STEP: isize = 3;

/// What a press landed on, as far as a double-click is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClickTarget {
    Local(usize),
    Remote(usize),
    Session(usize),
}

/// The last left press on something a double-click can act on.
pub(super) struct LastClick {
    target: ClickTarget,
    at: Instant,
}

impl App {
    pub(super) fn handle_mouse(&mut self, m: MouseEvent) {
        self.handle_mouse_at(m, Instant::now());
    }

    /// [`Self::handle_mouse`] with the time of the event given, so tests can
    /// place two presses either side of [`DOUBLE_CLICK`].
    pub(super) fn handle_mouse_at(&mut self, m: MouseEvent, now: Instant) {
        match self.screen {
            Screen::Main => self.mouse_main(m, now),
            Screen::Viewer => self.mouse_viewer(m),
            Screen::SessionSelect => self.mouse_session_select(m, now),
            _ => {}
        }
    }

    /// Record a press on `target`; true when it completes a double-click. A
    /// double-click consumes the pair, so a third quick press starts over.
    pub(super) fn is_double_click(&mut self, target: ClickTarget, now: Instant) -> bool {
        let double = self
            .last_click
            .as_ref()
            .is_some_and(|c| c.target == target && now.duration_since(c.at) <= DOUBLE_CLICK);
        self.last_click = if double {
            None
        } else {
            Some(LastClick { target, at: now })
        };
        double
    }

    pub(super) fn pane_mut(&mut self, pane: Pane) -> &mut PaneState {
        match pane {
            Pane::Local => &mut self.local,
            _ => &mut self.remote,
        }
    }

    fn mouse_main(&mut self, m: MouseEvent, now: Instant) {
        let (col, row) = (m.column, m.row);
        for pane in [Pane::Local, Pane::Remote] {
            let (pane_area, list_area) = match pane {
                Pane::Local => (self.hit.local_pane.get(), self.hit.local_list.get()),
                _ => (self.hit.remote_pane.get(), self.hit.remote_list.get()),
            };
            if row_in(pane_area, col, row).is_none() {
                continue;
            }
            match m.kind {
                MouseEventKind::ScrollUp => self.pane_mut(pane).move_cursor(-WHEEL_STEP),
                MouseEventKind::ScrollDown => self.pane_mut(pane).move_cursor(WHEEL_STEP),
                MouseEventKind::Down(MouseButton::Left) => {
                    self.click_pane(pane, row_in(list_area, col, row), now);
                }
                _ => {}
            }
            return;
        }

        let left = matches!(m.kind, MouseEventKind::Down(MouseButton::Left));
        if left && row_in(self.hit.transfers_tab.get(), col, row).is_some() {
            self.bottom_pane = BottomPane::Transfers;
            self.active_pane = Pane::Transfers;
            return;
        }
        if left && row_in(self.hit.log_tab.get(), col, row).is_some() {
            self.bottom_pane = BottomPane::Log;
            self.active_pane = Pane::Log;
            return;
        }
        if let Some(r) = row_in(self.hit.transfer_list.get(), col, row) {
            match m.kind {
                MouseEventKind::ScrollUp => self.move_transfer_cursor(-WHEEL_STEP),
                MouseEventKind::ScrollDown => self.move_transfer_cursor(WHEEL_STEP),
                MouseEventKind::Down(MouseButton::Left) => {
                    self.bottom_pane = BottomPane::Transfers;
                    self.active_pane = Pane::Transfers;
                    if r < self.active_jobs().len() {
                        self.transfer_cursor = r;
                    }
                }
                _ => {}
            }
        }
    }

    /// The wheel over the viewer body scrolls its text. Images don't scroll.
    fn mouse_viewer(&mut self, m: MouseEvent) {
        if row_in(self.hit.viewer_body.get(), m.column, m.row).is_none() {
            return;
        }
        match m.kind {
            MouseEventKind::ScrollUp => self.viewer_scroll(-WHEEL_STEP),
            MouseEventKind::ScrollDown => self.viewer_scroll(WHEEL_STEP),
            _ => {}
        }
    }

    /// A click selects a session; a double-click connects, as Enter does;
    /// the wheel moves the selection.
    fn mouse_session_select(&mut self, m: MouseEvent, now: Instant) {
        let Some(r) = row_in(self.hit.session_list.get(), m.column, m.row) else {
            return;
        };
        let len = self.sessions.len();
        match m.kind {
            MouseEventKind::ScrollUp => {
                self.session_cursor = self.session_cursor.saturating_sub(WHEEL_STEP as usize);
            }
            MouseEventKind::ScrollDown if len > 0 => {
                self.session_cursor = (self.session_cursor + WHEEL_STEP as usize).min(len - 1);
            }
            MouseEventKind::Down(MouseButton::Left) if r < len => {
                self.session_cursor = r;
                if self.is_double_click(ClickTarget::Session(r), now) {
                    self.handle_session_select(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                }
            }
            _ => {}
        }
    }

    /// A left press in a file pane: focus it, and on a row, move the cursor
    /// there. Row `r` is entry `view_offset + r`, as the last draw showed.
    fn click_pane(&mut self, pane: Pane, list_row: Option<usize>, now: Instant) {
        self.active_pane = pane;
        let Some(r) = list_row else {
            self.last_click = None;
            return;
        };
        let state = self.pane_mut(pane);
        let idx = state.view_offset.get() + r;
        if idx >= state.entries.len() {
            self.last_click = None;
            return;
        }
        state.cursor = idx;
        let target = match pane {
            Pane::Local => ClickTarget::Local(idx),
            _ => ClickTarget::Remote(idx),
        };
        if self.is_double_click(target, now) {
            self.open_or_transfer(pane);
        }
    }

    /// A double-click on the cursor entry of `pane`: enter a directory (or
    /// go up, for `..`); transfer a file to the other side — download from
    /// the remote pane, upload from the local — that file alone, whatever
    /// else is selected.
    pub(super) fn open_or_transfer(&mut self, pane: Pane) {
        let state = match pane {
            Pane::Local => &self.local,
            _ => &self.remote,
        };
        let Some(entry) = state.entries.get(state.cursor) else {
            return;
        };
        if entry.is_dir {
            match pane {
                Pane::Local => self.local_enter(),
                _ => self.remote_enter(),
            }
            return;
        }
        let file = vec![(entry.raw_name.clone(), false)];
        match pane {
            Pane::Local => self.start_uploads(file),
            _ => self.enqueue_downloads(file),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    use crate::tui::app::tests_support::{app_on_main, draw};
    use crate::tui::app::{App, Pane, Screen};
    use crate::tui::state::PaneEntry;

    fn press(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn wheel(down: bool, col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: if down {
                MouseEventKind::ScrollDown
            } else {
                MouseEventKind::ScrollUp
            },
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// The screen cell of row `r` in the local list, as last drawn.
    fn local_row(a: &App, r: u16) -> (u16, u16) {
        let list = a.hit.local_list.get().unwrap();
        (list.x + 2, list.y + r)
    }

    #[tokio::test]
    async fn a_click_on_a_row_focuses_the_pane_and_moves_the_cursor() {
        let mut a = app_on_main(10);
        a.active_pane = Pane::Remote;
        draw(&a);
        let (c, r) = local_row(&a, 3);
        a.handle_mouse_at(press(c, r), Instant::now());
        assert_eq!(a.active_pane, Pane::Local);
        assert_eq!(a.local.cursor, 3);
    }

    #[tokio::test]
    async fn a_click_on_the_path_line_focuses_without_moving_the_cursor() {
        let mut a = app_on_main(10);
        a.active_pane = Pane::Remote;
        a.local.cursor = 5;
        draw(&a);
        let list = a.hit.local_list.get().unwrap();
        a.handle_mouse_at(press(list.x + 2, list.y - 1), Instant::now());
        assert_eq!(a.active_pane, Pane::Local);
        assert_eq!(a.local.cursor, 5);
    }

    #[tokio::test]
    async fn a_click_below_the_last_entry_moves_nothing() {
        let mut a = app_on_main(3);
        draw(&a);
        let (c, r) = local_row(&a, 8);
        a.handle_mouse_at(press(c, r), Instant::now());
        assert_eq!(a.local.cursor, 0);
    }

    #[tokio::test]
    async fn a_click_after_the_listing_shrank_hits_nothing() {
        let mut a = app_on_main(10);
        draw(&a);
        a.local
            .set_entries(vec![PaneEntry::new("only".into(), false, 1)]);
        let (c, r) = local_row(&a, 6);
        a.handle_mouse_at(press(c, r), Instant::now());
        assert_eq!(a.local.cursor, 0, "row 6 of a one-entry list is nothing");
    }

    #[tokio::test]
    async fn a_click_with_a_filter_lands_on_the_filtered_entry_drawn_there() {
        let mut a = app_on_main(10);
        a.local.set_filter("7".into());
        draw(&a);
        let (c, r) = local_row(&a, 0);
        a.handle_mouse_at(press(c, r), Instant::now());
        assert_eq!(
            a.local.entries[a.local.cursor].raw_name, "file007",
            "row 0 of the filtered list, not of the full one (file000)",
        );
    }

    #[tokio::test]
    async fn two_quick_clicks_on_one_directory_enter_it() {
        let mut a = app_on_main(0);
        a.local.path = "/tmp".into();
        a.local
            .set_entries(vec![PaneEntry::new("sub".into(), true, 0)]);
        draw(&a);
        let (c, r) = local_row(&a, 0);
        let t = Instant::now();
        a.handle_mouse_at(press(c, r), t);
        a.handle_mouse_at(press(c, r), t + Duration::from_millis(200));
        assert_eq!(a.local.path, "/tmp/sub");
    }

    #[tokio::test]
    async fn two_slow_clicks_are_two_single_clicks() {
        let mut a = app_on_main(0);
        a.local.path = "/tmp".into();
        a.local
            .set_entries(vec![PaneEntry::new("sub".into(), true, 0)]);
        draw(&a);
        let (c, r) = local_row(&a, 0);
        let t = Instant::now();
        a.handle_mouse_at(press(c, r), t);
        a.handle_mouse_at(press(c, r), t + Duration::from_millis(600));
        assert_eq!(a.local.path, "/tmp");
    }

    #[tokio::test]
    async fn quick_clicks_on_two_rows_are_two_single_clicks() {
        let mut a = app_on_main(0);
        a.local.path = "/tmp".into();
        a.local.set_entries(vec![
            PaneEntry::new("a".into(), true, 0),
            PaneEntry::new("b".into(), true, 0),
        ]);
        draw(&a);
        let t = Instant::now();
        let (c, r0) = local_row(&a, 0);
        let (_, r1) = local_row(&a, 1);
        a.handle_mouse_at(press(c, r0), t);
        a.handle_mouse_at(press(c, r1), t + Duration::from_millis(100));
        assert_eq!(a.local.path, "/tmp");
        assert_eq!(a.local.cursor, 1);
    }

    #[tokio::test]
    async fn a_double_click_far_down_a_long_list_registers() {
        let mut a = app_on_main(0);
        a.local.path = "/tmp".into();
        a.local.set_entries(
            (0..200)
                .map(|i| PaneEntry::new(format!("d{i:03}"), true, 0))
                .collect(),
        );
        a.local.cursor = 150;
        draw(&a);
        let (c, r) = local_row(&a, 2);
        let t = Instant::now();
        a.handle_mouse_at(press(c, r), t);
        let clicked = a.local.entries[a.local.cursor].raw_name.clone();
        draw(&a); // the run loop draws between the two presses
        a.handle_mouse_at(press(c, r), t + Duration::from_millis(150));
        assert_eq!(a.local.path, format!("/tmp/{clicked}"));
    }

    #[tokio::test]
    async fn the_wheel_moves_the_cursor_of_the_pane_under_it_without_focusing_it() {
        let mut a = app_on_main(20);
        a.active_pane = Pane::Remote;
        draw(&a);
        let (c, r) = local_row(&a, 0);
        a.handle_mouse_at(wheel(true, c, r), Instant::now());
        assert_eq!(a.local.cursor, 3);
        assert_eq!(a.active_pane, Pane::Remote);
        a.handle_mouse_at(wheel(false, c, r), Instant::now());
        assert_eq!(a.local.cursor, 0);
    }

    #[tokio::test]
    async fn the_panes_behind_a_modal_ignore_the_mouse() {
        let mut a = app_on_main(10);
        draw(&a);
        a.screen = Screen::Search;
        let (c, r) = local_row(&a, 4);
        a.handle_mouse_at(press(c, r), Instant::now());
        a.handle_mouse_at(wheel(true, c, r), Instant::now());
        assert_eq!(a.local.cursor, 0);
    }

    use crate::tui::event::AppEvent;

    /// Double-click the local row `r` and return the upload walk's roots,
    /// read from the WalkComplete it posts.
    async fn double_click_and_walk(a: &mut App, pane: Pane, r: u16) -> Vec<String> {
        let mut rx = a.app_event_rx.take().unwrap();
        let list = match pane {
            Pane::Local => a.hit.local_list.get().unwrap(),
            _ => a.hit.remote_list.get().unwrap(),
        };
        let t = Instant::now();
        a.handle_mouse_at(press(list.x + 2, list.y + r), t);
        a.handle_mouse_at(
            press(list.x + 2, list.y + r),
            t + Duration::from_millis(100),
        );
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the walk reports")
            .unwrap();
        match ev {
            AppEvent::WalkComplete { plan, .. } => plan
                .iter()
                .map(|j| match j {
                    crate::tui::plan::PlannedJob::Upload { remote_path, .. }
                    | crate::tui::plan::PlannedJob::Download { remote_path, .. } => {
                        remote_path.clone()
                    }
                    crate::tui::plan::PlannedJob::Mkdir { remote_path } => remote_path.clone(),
                })
                .collect(),
            _ => panic!("expected WalkComplete"),
        }
    }

    fn connected(a: &mut App) {
        // A file under /r makes the mock list /r, as the upload's conflict
        // check does; without it the listing fails and so does the walk.
        a.transport = Some(std::sync::Arc::new(tokio::sync::Mutex::new(Box::new(
            crate::transport::mock::MockTransport::new().with_file("/r/.keep", b""),
        )
            as Box<dyn crate::transport::Transport>)));
        a.transfer_manager = Some(crate::transfer::TransferManager::new(1).0);
        a.remote.path = "/r".into();
    }

    #[tokio::test]
    async fn double_clicking_a_local_file_uploads_it_alone() {
        let mut a = app_on_main(3);
        connected(&mut a);
        a.local.toggle_selected(); // file000 selected, cursor on it
        draw(&a);
        let roots = double_click_and_walk(&mut a, Pane::Local, 2).await;
        assert_eq!(
            roots,
            vec!["/r/file002".to_string()],
            "the clicked file alone"
        );
        assert_eq!(
            a.local.selected_count(),
            1,
            "the selection is left as it was"
        );
    }

    #[tokio::test]
    async fn double_clicking_a_remote_file_downloads_it_alone() {
        let mut a = app_on_main(0);
        connected(&mut a);
        a.local.path = std::env::temp_dir().display().to_string();
        a.remote.set_entries(vec![
            PaneEntry::new("x.bin".into(), false, 1),
            PaneEntry::new("y.bin".into(), false, 1),
        ]);
        a.remote.toggle_selected(); // x.bin selected
        draw(&a);
        let roots = double_click_and_walk(&mut a, Pane::Remote, 1).await;
        assert_eq!(roots, vec!["/r/y.bin".to_string()]);
        assert_eq!(a.remote.selected_count(), 1);
    }

    #[tokio::test]
    async fn double_clicking_the_parent_row_goes_up_and_transfers_nothing() {
        let mut a = app_on_main(0);
        connected(&mut a);
        a.local.path = "/tmp/sub".into();
        a.local.set_entries(vec![
            PaneEntry::parent(),
            PaneEntry::new("f".into(), false, 1),
        ]);
        draw(&a);
        let mut rx = a.app_event_rx.take().unwrap();
        let (c, r) = local_row(&a, 0);
        let t = Instant::now();
        a.handle_mouse_at(press(c, r), t);
        a.handle_mouse_at(press(c, r), t + Duration::from_millis(100));
        assert_eq!(a.local.path, "/tmp");
        // A transfer would show as the walk it starts reporting back.
        // Entering the directory posts a listing, so watch every event for
        // a while and fail on a walk.
        let watch = tokio::time::sleep(Duration::from_millis(300));
        tokio::pin!(watch);
        loop {
            tokio::select! {
                () = &mut watch => break,
                Some(ev) = rx.recv() => {
                    assert!(
                        !matches!(ev, AppEvent::WalkComplete { .. } | AppEvent::WalkFailed { .. }),
                        "`..` must never be transferred",
                    );
                }
            }
        }
    }

    use crate::session::Session;
    use crate::tui::app::BottomPane;

    fn centre(r: ratatui::layout::Rect) -> (u16, u16) {
        (r.x + r.width / 2, r.y)
    }

    #[test]
    fn clicking_a_tab_shows_that_page_and_focuses_the_bottom_pane() {
        let mut a = app_on_main(0);
        draw(&a);
        let (c, r) = centre(a.hit.log_tab.get().unwrap());
        a.handle_mouse_at(press(c, r), Instant::now());
        assert_eq!(a.bottom_pane, BottomPane::Log);
        assert_eq!(a.active_pane, Pane::Log);
        let (c, r) = centre(a.hit.transfers_tab.get().unwrap());
        a.handle_mouse_at(press(c, r), Instant::now());
        assert_eq!(a.bottom_pane, BottomPane::Transfers);
        assert_eq!(a.active_pane, Pane::Transfers);
    }

    #[test]
    fn clicking_a_transfer_row_selects_it() {
        let mut a = app_on_main(0);
        let m = crate::transfer::TransferManager::new(4).0;
        for i in 0..3 {
            m.enqueue_download(format!("/r/{i}"), format!("/l/{i}").into())
                .unwrap();
            m.take_next_pending().unwrap();
        }
        a.transfer_manager = Some(m);
        a.bottom_pane = BottomPane::Transfers;
        draw(&a);
        let list = a.hit.transfer_list.get().unwrap();
        a.handle_mouse_at(press(list.x + 2, list.y + 2), Instant::now());
        assert_eq!(a.active_pane, Pane::Transfers);
        assert_eq!(a.transfer_cursor, 2);
        a.handle_mouse_at(wheel(false, list.x + 2, list.y), Instant::now());
        assert_eq!(a.transfer_cursor, 0);
    }

    #[test]
    fn the_wheel_scrolls_the_viewer_text() {
        let mut a = app_on_main(0);
        let tokens = (0..100)
            .map(|i| vec![(crate::highlight::TokenKind::Plain, format!("line {i}"))])
            .collect();
        a.viewer = Some(crate::tui::state::Viewer {
            name: "t.txt".into(),
            kind: crate::tui::state::ViewerKind::Text { tokens, scroll: 0 },
            id: 1,
        });
        a.screen = Screen::Viewer;
        draw(&a);
        let body = a.hit.viewer_body.get().unwrap();
        a.handle_mouse_at(wheel(true, body.x + 1, body.y + 1), Instant::now());
        match &a.viewer.as_ref().unwrap().kind {
            crate::tui::state::ViewerKind::Text { scroll, .. } => assert_eq!(*scroll, 3),
            _ => unreachable!(),
        }
    }

    fn selector_with(n: usize) -> App {
        let mut a = crate::tui::app::App::new(
            crate::config::Config::default(),
            crate::theme::Theme::load("dracula").unwrap(),
        );
        a.sessions = (0..n)
            .map(|i| {
                let mut s = Session::from_url(&format!("sftp://me@h{i}.example")).unwrap();
                s.auth = crate::session::AuthMethod::Password;
                s
            })
            .collect();
        a
    }

    #[test]
    fn a_click_selects_a_session_and_the_wheel_moves_the_selection() {
        let mut a = selector_with(5);
        draw(&a);
        let rows = a.hit.session_list.get().unwrap();
        a.handle_mouse_at(press(rows.x + 4, rows.y + 3), Instant::now());
        assert_eq!(a.session_cursor, 3);
        a.handle_mouse_at(wheel(false, rows.x + 4, rows.y), Instant::now());
        assert_eq!(a.session_cursor, 0);
        a.handle_mouse_at(wheel(true, rows.x + 4, rows.y), Instant::now());
        assert_eq!(a.session_cursor, 3);
    }

    #[test]
    fn a_double_click_on_a_session_connects_as_enter_does() {
        let mut a = selector_with(2);
        draw(&a);
        let rows = a.hit.session_list.get().unwrap();
        let t = Instant::now();
        a.handle_mouse_at(press(rows.x + 4, rows.y + 1), t);
        a.handle_mouse_at(
            press(rows.x + 4, rows.y + 1),
            t + Duration::from_millis(100),
        );
        assert_eq!(a.screen, Screen::PasswordPrompt, "password auth asks first");
        assert_eq!(a.pending_session.as_ref().unwrap().host, "h1.example");
    }

    /// A key press between two clicks breaks the pair. Otherwise a click,
    /// a key that moves or navigates, and a click on the same row within
    /// the double-click window acted on whatever that row then held: a
    /// directory never double-clicked was entered, a file never
    /// double-clicked was transferred.
    #[tokio::test]
    async fn a_key_press_between_two_clicks_breaks_the_double_click() {
        let mut a = app_on_main(0);
        a.local.path = "/tmp".into();
        a.local.set_entries(vec![
            PaneEntry::new("a".into(), true, 0),
            PaneEntry::new("b".into(), true, 0),
        ]);
        draw(&a);
        let (c, r) = local_row(&a, 0);
        let t = Instant::now();
        a.handle_mouse_at(press(c, r), t);
        a.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Down,
            KeyModifiers::NONE,
        ));
        a.handle_mouse_at(press(c, r), t + Duration::from_millis(150));
        assert_eq!(a.local.path, "/tmp", "two single clicks, not a double");
        assert_eq!(a.local.cursor, 0);
    }

    #[tokio::test]
    async fn a_click_on_the_footer_focuses_without_moving_the_cursor() {
        let mut a = app_on_main(10);
        a.active_pane = Pane::Remote;
        a.local.cursor = 5;
        draw(&a);
        let list = a.hit.local_list.get().unwrap();
        a.handle_mouse_at(press(list.x + 2, list.y + list.height), Instant::now());
        assert_eq!(a.active_pane, Pane::Local);
        assert_eq!(a.local.cursor, 5);
    }

    /// The pane was scrolled well down when drawn; a refresh then left a
    /// short listing. Row 3 of the old window is past its end.
    #[tokio::test]
    async fn a_click_after_a_scrolled_listing_shrank_hits_nothing() {
        let mut a = app_on_main(200);
        a.local.cursor = 150;
        draw(&a);
        assert!(a.local.view_offset.get() > 0, "the pane was scrolled");
        a.local.set_entries(
            (0..5)
                .map(|i| PaneEntry::new(format!("n{i}"), false, 1))
                .collect(),
        );
        let before = a.local.cursor;
        let (c, r) = local_row(&a, 3);
        a.handle_mouse_at(press(c, r), Instant::now());
        assert_eq!(a.local.cursor, before, "a row past the end moves nothing");
    }
}
