# Mouse Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Clicks, double-clicks and the scroll wheel work in blink's file panes, bottom pane, viewer and session selector, behind a `mouse` setting that defaults to on.

**Architecture:** Render functions record where each clickable area was drawn into a `HitMap` of `Cell<Option<Rect>>` on `App`; mouse events are matched against it by a new `tui/app/mouse.rs`. File panes gain a stable scroll offset so a click maps to what was drawn and a double-click can land twice on one entry. Only left press and wheel events leave the event reader.

**Tech Stack:** Rust 2024, ratatui 0.30 (`TestBackend` for tests), crossterm 0.29 mouse events, tokio.

**Spec:** `docs/superpowers/specs/2026-09-28-mouse-support-design.md`

## Global Constraints

- Stage 1 only: no right-click or Ctrl-click selection, no clickable prompt buttons, no drag and drop, no drag selection, no resizable panes.
- Double-click: a second left press on the same entry of the same area within 400 ms.
- Wheel step: 3 rows (file panes, transfer list, session list) or 3 lines (viewer).
- Double-click a directory or `..`: enter it. A remote file: download that file alone. A local file: upload that file alone. The selection is ignored and left untouched.
- Only `Screen::Main`, `Screen::Viewer` and `Screen::SessionSelect` respond to the mouse; every other screen ignores it, including the panes drawn behind a modal.
- Config: `mouse` under `[terminal]`, parsed with `parse_bool`, default `true`, written by `Config::save`, read at startup only. Off means `EnableMouseCapture` is never sent.
- Movement, drag, release and non-left-button events are dropped in `EventStream::next` and never wake the run loop.
- Every commit: `cargo fmt` first; `cargo clippy --all-targets` clean; commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **A click with a filter active** must land on the filtered entry drawn at that row, not the unfiltered one — pinned in Task 4.
2. **A double-click far down a long list** (cursor well past the first screenful) must register: the first click must not scroll the entry away — pinned in Task 4.
3. **A click on a pane's path line or footer** focuses the pane and moves no cursor — pinned in Task 4.
4. **Double-click on `..`** goes up a directory, and never transfers anything — pinned in Task 5.
5. **A click after the listing shrank below the drawn window** (a refresh landed between draw and click) hits nothing rather than panicking or picking a wrong entry — pinned in Task 4.

---

### Task 1: Stable scroll offset for the file panes

**Files:**
- Modify: `src/tui/state.rs` (struct `PaneState`, `PaneState::empty`, new free fn after `impl PaneState`)
- Modify: `src/tui/widgets.rs` (`file_pane::render`, the "Entry list, windowed around cursor" block)
- Test: `src/tui/state.rs` (existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `PaneState::view_offset: std::cell::Cell<usize>`; `pub fn scrolled_window(offset: usize, cursor: usize, len: usize, height: usize) -> std::ops::Range<usize>` in `crate::tui::state`.

- [ ] **Step 1: Write the failing tests** — append inside `mod tests` in `src/tui/state.rs`:

```rust
    // -- scrolled_window ------------------------------------------------------
    //
    // The window used to be a function of the cursor alone, so any move
    // past the first screenful shifted the whole list. A click moves the
    // cursor; with that window the clicked row jumped away and a
    // double-click could never land twice on one entry.

    use super::scrolled_window;

    #[test]
    fn the_window_stays_put_while_the_cursor_moves_inside_it() {
        assert_eq!(scrolled_window(10, 15, 100, 10), 10..20);
        assert_eq!(scrolled_window(10, 10, 100, 10), 10..20);
        assert_eq!(scrolled_window(10, 19, 100, 10), 10..20);
    }

    #[test]
    fn the_window_scrolls_just_enough_when_the_cursor_leaves_it() {
        assert_eq!(scrolled_window(10, 20, 100, 10), 11..21, "one past the bottom");
        assert_eq!(scrolled_window(10, 9, 100, 10), 9..19, "one past the top");
        assert_eq!(scrolled_window(0, 50, 100, 10), 41..51, "a jump");
    }

    #[test]
    fn the_window_clamps_when_the_list_shrinks() {
        assert_eq!(scrolled_window(50, 3, 5, 10), 0..5, "fits entirely");
        assert_eq!(scrolled_window(90, 19, 20, 10), 10..20, "no empty rows below");
    }

    #[test]
    fn an_empty_list_or_pane_shows_nothing() {
        assert_eq!(scrolled_window(0, 0, 0, 10), 0..0);
        assert_eq!(scrolled_window(0, 0, 10, 0), 0..0);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --quiet tui::state::tests::the_window 2>&1 | tail -5`
Expected: compile error, `unresolved import super::scrolled_window`.

- [ ] **Step 3: Implement** — in `src/tui/state.rs`, add the field to `PaneState` (after `pub cursor: usize,`):

```rust
    /// The first entry the pane showed when last drawn. The window only
    /// scrolls when the cursor leaves it — see [`scrolled_window`] — so a
    /// click on row `r` means entry `view_offset + r`. A `Cell` because the
    /// renderer, which only has `&App`, is what updates it.
    pub view_offset: std::cell::Cell<usize>,
```

and in `PaneState::empty` add `view_offset: std::cell::Cell::new(0),` after `cursor: 0,`. Then add, after the closing brace of `impl PaneState`:

```rust
/// The entries a list `height` rows tall shows, given the first one it
/// showed last time (`offset`) and where the cursor is.
///
/// The window keeps its place while the cursor stays inside it and scrolls
/// just far enough to show the cursor when it leaves. A list that shrank is
/// pulled up so no empty rows sit below its end.
pub fn scrolled_window(offset: usize, cursor: usize, len: usize, height: usize) -> std::ops::Range<usize> {
    if len == 0 || height == 0 {
        return 0..0;
    }
    let cursor = cursor.min(len - 1);
    let mut start = offset.min(len.saturating_sub(height));
    if cursor < start {
        start = cursor;
    }
    if cursor >= start + height {
        start = cursor + 1 - height;
    }
    start..(start + height).min(len)
}
```

In `src/tui/widgets.rs`, `file_pane::render`, replace:

```rust
        let cursor = state.cursor.min(len.saturating_sub(1));
        let window_start = (cursor + 1).saturating_sub(h);
        let window_end = (window_start + h).min(len);

        let mut lines = Vec::with_capacity(window_end.saturating_sub(window_start));
        for i in window_start..window_end {
```

with:

```rust
        let cursor = state.cursor.min(len.saturating_sub(1));
        let window =
            crate::tui::state::scrolled_window(state.view_offset.get(), state.cursor, len, h);
        state.view_offset.set(window.start);

        let mut lines = Vec::with_capacity(window.len());
        for i in window {
```

- [ ] **Step 4: Run the tests**

Run: `cargo fmt && cargo test --quiet 2>&1 | grep -E "test result|FAILED"`
Expected: all pass (existing suite plus 4 new).

- [ ] **Step 5: Commit**

```bash
git add src/tui/state.rs src/tui/widgets.rs
git commit -m "feat(tui): give file panes a stable scroll offset

The window was a function of the cursor alone, so past the first
screenful every move shifted the list. Needed for mouse support: a click
must map to what was drawn, and stay there for a double-click.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Record where clickable areas are drawn

**Files:**
- Create: `src/tui/hit.rs`
- Modify: `src/tui/mod.rs` (declare `pub mod hit;`)
- Modify: `src/tui/app/mod.rs` (field `hit` on `App`, initialise in `App::new`, clear at the top of `App::draw`)
- Modify: `src/tui/widgets.rs` (`file_pane::render`, `bottom_pane::render`, `render_transfers`)
- Modify: `src/tui/views.rs` (`session_select::render_session_list`, `viewer::render`)
- Test: `src/tui/app/mod.rs` (`mod tests`)

**Interfaces:**
- Consumes: `PaneState::view_offset` (Task 1).
- Produces: `crate::tui::hit::HitMap` with fields `local_pane, remote_pane, local_list, remote_list, transfers_tab, log_tab, transfer_list, viewer_body, session_list: Cell<Option<Rect>>`, `HitMap::clear(&self)`; `crate::tui::hit::row_in(area: Option<Rect>, col: u16, row: u16) -> Option<usize>`; `App::hit: HitMap` (pub(crate)); test fixtures `crate::tui::app::tests_support::{app_on_main(n: usize) -> App, draw(a: &App) -> ratatui::buffer::Buffer}`.

- [ ] **Step 1: Write the failing tests.** In `src/tui/app/mod.rs`, add a fixtures module next to `#[cfg(test)] mod tests` (the mouse tests in Tasks 4–6 use it too):

```rust
#[cfg(test)]
pub(super) mod tests_support {
    use super::*;
    use crate::tui::state::PaneEntry;

    /// An app on the main view, local pane `/l` holding `n` files.
    pub(crate) fn app_on_main(n: usize) -> App {
        let mut a = App::new(Config::default(), Theme::load("dracula").unwrap());
        a.screen = Screen::Main;
        a.local.path = "/l".into();
        a.local.set_entries(
            (0..n)
                .map(|i| PaneEntry::new(format!("file{i:03}"), false, 1))
                .collect(),
        );
        a
    }

    /// Draw `a` into a 100x30 test terminal, filling its hit map, and hand
    /// back what was drawn.
    pub(crate) fn draw(a: &App) -> ratatui::buffer::Buffer {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        t.draw(|f| a.draw(f)).unwrap();
        t.backend().buffer().clone()
    }
}
```

Then in `mod tests`, before the `// -- image rendering off the UI thread` block:

```rust
    // -- the hit map -----------------------------------------------------------
    //
    // Mouse clicks are matched against where things were last drawn. These
    // draw into a TestBackend and read the recorded areas back, checking
    // each against the text the renderer actually put there.

    use super::tests_support::{app_on_main, draw};

    fn text_at(buf: &ratatui::buffer::Buffer, r: ratatui::layout::Rect) -> String {
        (r.x..r.x + r.width).map(|x| buf[(x, r.y)].symbol().to_string()).collect()
    }

    #[test]
    fn the_file_list_area_is_recorded_where_its_rows_are_drawn() {
        let a = app_on_main(5);
        let buf = draw(&a);
        let list = a.hit.local_list.get().expect("recorded");
        let first = ratatui::layout::Rect { height: 1, ..list };
        assert!(text_at(&buf, first).contains("file000"), "{:?}", text_at(&buf, first));
        assert!(a.hit.local_pane.get().unwrap().width > list.width);
    }

    #[test]
    fn the_bottom_tabs_are_recorded_over_their_labels() {
        let a = app_on_main(0);
        let buf = draw(&a);
        assert_eq!(text_at(&buf, a.hit.transfers_tab.get().unwrap()), " TRANSFERS ");
        assert_eq!(text_at(&buf, a.hit.log_tab.get().unwrap()), " LOG ");
    }

    #[test]
    fn the_session_rows_are_recorded_below_their_heading() {
        let mut a = app();
        a.sessions = vec![Session::from_url("sftp://me@alpha.example").unwrap()];
        let buf = draw(&a);
        let rows = a.hit.session_list.get().expect("recorded");
        assert!(text_at(&buf, ratatui::layout::Rect { height: 1, ..rows }).contains("alpha.example"));
    }

    #[test]
    fn a_new_draw_forgets_areas_no_longer_drawn() {
        let mut a = app_on_main(1);
        draw(&a);
        assert!(a.hit.local_list.get().is_some());
        a.screen = Screen::SessionSelect;
        draw(&a);
        assert!(a.hit.local_list.get().is_none(), "the session selector draws no panes");
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --quiet the_file_list_area 2>&1 | tail -3`
Expected: compile error, `no field hit on type App`.

- [ ] **Step 3: Implement.** Create `src/tui/hit.rs`:

```rust
//! Where clickable things were last drawn.
//!
//! The render functions take `&App` and compute their layouts as they draw,
//! so nothing else knows where a pane or a list ended up. Each records its
//! area here as it draws; a mouse event is then matched against exactly
//! what is on screen. Recomputing the layouts in the mouse handler instead
//! would duplicate every one of them, and a duplicated layout drifts.

use std::cell::Cell;

use ratatui::layout::Rect;

/// One slot per clickable area, cleared at the start of every draw so a
/// screen that no longer draws an area no longer answers for it.
#[derive(Debug, Default)]
pub struct HitMap {
    pub local_pane: Cell<Option<Rect>>,
    pub remote_pane: Cell<Option<Rect>>,
    pub local_list: Cell<Option<Rect>>,
    pub remote_list: Cell<Option<Rect>>,
    pub transfers_tab: Cell<Option<Rect>>,
    pub log_tab: Cell<Option<Rect>>,
    pub transfer_list: Cell<Option<Rect>>,
    pub viewer_body: Cell<Option<Rect>>,
    pub session_list: Cell<Option<Rect>>,
}

impl HitMap {
    pub fn clear(&self) {
        for cell in [
            &self.local_pane,
            &self.remote_pane,
            &self.local_list,
            &self.remote_list,
            &self.transfers_tab,
            &self.log_tab,
            &self.transfer_list,
            &self.viewer_body,
            &self.session_list,
        ] {
            cell.set(None);
        }
    }
}

/// The row of `area` that the cell at (`col`, `row`) is on, counted from the
/// area's top, or `None` when the cell is outside it.
pub fn row_in(area: Option<Rect>, col: u16, row: u16) -> Option<usize> {
    let a = area?;
    let inside = col >= a.x && col < a.x + a.width && row >= a.y && row < a.y + a.height;
    inside.then(|| usize::from(row - a.y))
}
```

In `src/tui/mod.rs` add `pub mod hit;` after `pub mod event;`.

In `src/tui/app/mod.rs`: add the field to `App` (after `pub viewer: Option<Viewer>,`):

```rust
    /// Where clickable areas were last drawn. See [`crate::tui::hit`].
    pub(crate) hit: crate::tui::hit::HitMap,
```

initialise it in `App::new` with `hit: Default::default(),` after `viewer: None,`, and make the first statement of `fn draw(&self, f: &mut Frame)`:

```rust
        self.hit.clear();
```

In `src/tui/widgets.rs`, `file_pane::render`, directly after `let list_area = inner_layout[1];`:

```rust
        let (pane_hit, list_hit) = match which {
            Pane::Local => (&app.hit.local_pane, &app.hit.local_list),
            _ => (&app.hit.remote_pane, &app.hit.remote_list),
        };
        pane_hit.set(Some(area));
        list_hit.set(Some(list_area));
```

In `bottom_pane::render`, directly after `f.render_widget(block, area);`:

```rust
        // The tab labels sit in the title row, one cell in from the corner:
        // " " then " TRANSFERS " then "·" then " LOG ", as `tab_title` draws.
        let transfers_x = area.x + 2;
        let log_x = transfers_x + TRANSFERS_TAB.len() as u16 + 1;
        app.hit.transfers_tab.set(Some(Rect::new(
            transfers_x,
            area.y,
            TRANSFERS_TAB.len() as u16,
            1,
        )));
        app.hit
            .log_tab
            .set(Some(Rect::new(log_x, area.y, LOG_TAB.len() as u16, 1)));
```

and above `fn tab_title` add the two labels, used by both:

```rust
    const TRANSFERS_TAB: &str = " TRANSFERS ";
    const LOG_TAB: &str = " LOG ";
```

replacing the literals `" TRANSFERS "` and `" LOG "` in `tab_title` with `TRANSFERS_TAB` and `LOG_TAB`. In `render_transfers`, as its first statement:

```rust
        app.hit.transfer_list.set(Some(area));
```

In `src/tui/views.rs`, `render_session_list`, directly before `f.render_widget(Paragraph::new(lines), area);`:

```rust
        // Sessions start below the heading and its blank line, one per row.
        app.hit.session_list.set(Some(Rect {
            y: area.y + 2,
            height: area.height.saturating_sub(2),
            ..area
        }));
```

In `viewer::render`, directly after the `let body = Rect::new(...)` statement:

```rust
        app.hit.viewer_body.set(Some(body));
```

- [ ] **Step 4: Run the tests**

Run: `cargo fmt && cargo test --quiet 2>&1 | grep -E "test result|FAILED"`
Expected: all pass (4 new).

- [ ] **Step 5: Commit**

```bash
git add src/tui/hit.rs src/tui/mod.rs src/tui/app/mod.rs src/tui/widgets.rs src/tui/views.rs
git commit -m "feat(tui): record where clickable areas are drawn

A HitMap on App, cleared each draw and filled in by the render functions,
so mouse events can be matched against what is on screen.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: The `mouse` setting

**Files:**
- Modify: `src/config.rs` (`Terminal`, `Config::default`, `Config::load_from`, `Config::save`, module doc)
- Modify: `src/tui/mod.rs` (`setup`, `run`, `run_with_session`)
- Modify: `README.md` (the `config.ini — global` example)
- Test: `src/config.rs` (`mod tests`)

**Interfaces:**
- Produces: `Terminal::mouse: bool`; `tui::setup(mouse: bool) -> Result<TuiTerminal>`.

- [ ] **Step 1: Write the failing tests** — in `src/config.rs` `mod tests`:

```rust
    // -- mouse ------------------------------------------------------------------

    fn load_str(tag: &str, body: &str) -> Result<Config> {
        let dir = std::env::temp_dir().join(format!("blink-config-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.ini");
        std::fs::write(&path, body).unwrap();
        let cfg = Config::load_from(&path);
        let _ = std::fs::remove_dir_all(&dir);
        cfg
    }

    #[test]
    fn mouse_is_on_unless_turned_off() {
        assert!(Config::default().terminal.mouse);
        assert!(load_str("mouse-absent", "[terminal]\nimage_preview = auto\n").unwrap().terminal.mouse);
        assert!(!load_str("mouse-off", "[terminal]\nmouse = false\n").unwrap().terminal.mouse);
        assert!(load_str("mouse-on", "[terminal]\nmouse = yes\n").unwrap().terminal.mouse);
    }

    #[test]
    fn a_mouse_value_that_is_not_a_boolean_is_refused() {
        assert!(load_str("mouse-bad", "[terminal]\nmouse = sometimes\n").is_err());
    }

    #[test]
    fn saving_keeps_the_mouse_setting() {
        let _home = crate::paths::test_home();
        let mut cfg = Config::default();
        cfg.terminal.mouse = false;
        cfg.save().unwrap();
        let back = Config::load_from(&paths::config_file().unwrap()).unwrap();
        assert!(!back.terminal.mouse);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --quiet config::tests::mouse 2>&1 | tail -3`
Expected: compile error, `no field mouse on type Terminal`.

- [ ] **Step 3: Implement.** In `src/config.rs`: add to `Terminal`:

```rust
    /// Mouse support. Off leaves mouse capture off, so the terminal's own
    /// text selection works and blink ignores the mouse.
    pub mouse: bool,
```

set `mouse: true,` in `Config::default`'s `Terminal`. Replace the `[terminal]` block of `Config::load_from`:

```rust
        if let Some(s) = ini.section(Some("terminal"))
            && let Some(v) = s.get("image_preview")
        {
```

with a block that reads both keys:

```rust
        if let Some(s) = ini.section(Some("terminal")) {
            if let Some(v) = s.get("image_preview") {
```

closing the new outer `if` after the `image_preview` match, and adding inside it, after that match:

```rust
            if let Some(v) = s.get("mouse") {
                cfg.terminal.mouse = parse_bool(v)?;
            }
```

In `Config::save`, change the `[terminal]` write to:

```rust
        ini.with_section(Some("terminal"))
            .set(
                "image_preview",
                match self.terminal.image_preview {
                    ImagePreviewMode::Auto => "auto",
                    ImagePreviewMode::Kitty => "kitty",
                    ImagePreviewMode::Sixel => "sixel",
                    ImagePreviewMode::Iterm2 => "iterm2",
                    ImagePreviewMode::None => "none",
                },
            )
            .set("mouse", self.terminal.mouse.to_string());
```

and add `//! mouse = true           ; false: no mouse capture, the terminal's own selection` to the module doc's `[terminal]` example.

In `src/tui/mod.rs`, change `setup` to take the setting:

```rust
/// Set up the alternate screen, raw mode, and — when `mouse` is on — mouse
/// capture. The matching teardown lives in [`restore`] and MUST run on
/// every exit path; it disables capture either way, which is harmless when
/// it was never enabled.
pub fn setup(mouse: bool) -> Result<TuiTerminal> {
    // Before raw mode, so the hook is in place for anything that follows.
    install_panic_hook();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    if mouse {
        execute!(stdout, EnableMouseCapture)?;
    }
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}
```

and in `run` and `run_with_session` replace `let mut terminal = setup()?;` with `let mut terminal = setup(config.terminal.mouse)?;`.

In `README.md`'s `config.ini — global` example, add under `image_preview`:

```ini
mouse = true                ; false: no mouse capture — the terminal's own
                            ; text selection works, and blink ignores the mouse
```

- [ ] **Step 4: Run the tests**

Run: `cargo fmt && cargo test --quiet 2>&1 | grep -E "test result|FAILED"`
Expected: all pass (3 new).

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/tui/mod.rs README.md
git commit -m "feat(config): a mouse setting, on by default

Off, blink never enables mouse capture, so the terminal's own text
selection works.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Mouse events reach the app; clicks and the wheel in the file panes

**Files:**
- Modify: `src/tui/event.rs` (`Event`, `EventStream::next`, new fn `wanted_mouse`)
- Create: `src/tui/app/mouse.rs`
- Modify: `src/tui/app/mod.rs` (`mod mouse;`, field `last_click`, run loop arm)
- Test: `src/tui/event.rs` (`mod tests`), `src/tui/app/mouse.rs` (`mod tests`)

**Interfaces:**
- Consumes: `HitMap`, `row_in` (Task 2); `PaneState::view_offset` (Task 1).
- Produces: `Event::Mouse(MouseEvent)`; `App::handle_mouse(&mut self, m: MouseEvent)`; `App::handle_mouse_at(&mut self, m: MouseEvent, now: Instant)`; in `mouse.rs`: `DOUBLE_CLICK: Duration`, `WHEEL_STEP: isize`, `ClickTarget`, `LastClick`, `App::is_double_click`, `App::pane_mut`, and `App::open_or_transfer(&mut self, pane: Pane)` (directories only in this task; Task 5 adds files).

- [ ] **Step 1: Write the failing tests.** In `src/tui/event.rs` `mod tests`:

```rust
    #[test]
    fn only_left_presses_and_the_wheel_are_wanted() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let ev = |kind| MouseEvent { kind, column: 0, row: 0, modifiers: KeyModifiers::NONE };
        assert!(wanted_mouse(&ev(MouseEventKind::Down(MouseButton::Left))));
        assert!(wanted_mouse(&ev(MouseEventKind::ScrollUp)));
        assert!(wanted_mouse(&ev(MouseEventKind::ScrollDown)));
        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::ScrollLeft,
        ] {
            assert!(!wanted_mouse(&ev(kind)), "{kind:?} must not wake the loop");
        }
    }
```

Create `src/tui/app/mouse.rs` with only its test module for now (the implementation goes above it in Step 3):

```rust
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
            kind: if down { MouseEventKind::ScrollDown } else { MouseEventKind::ScrollUp },
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
        a.local.set_entries(vec![PaneEntry::new("only".into(), false, 1)]);
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
        let e = &a.local.entries[a.local.cursor];
        assert!(e.raw_name == ".." || e.raw_name.contains('7'), "{}", e.raw_name);
    }

    #[tokio::test]
    async fn two_quick_clicks_on_one_directory_enter_it() {
        let mut a = app_on_main(0);
        a.local.path = "/tmp".into();
        a.local.set_entries(vec![PaneEntry::new("sub".into(), true, 0)]);
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
        a.local.set_entries(vec![PaneEntry::new("sub".into(), true, 0)]);
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
            (0..200).map(|i| PaneEntry::new(format!("d{i:03}"), true, 0)).collect(),
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
}
```

In `src/tui/app/mod.rs`, declare `mod mouse;` next to the other `mod` lines.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --quiet 2>&1 | grep -E "^error" | sort | uniq -c`
Expected: compile errors for `wanted_mouse`, `handle_mouse_at`.

- [ ] **Step 3: Implement.** In `src/tui/event.rs`, add `Mouse(MouseEvent),` to `Event` (after `Key(KeyEvent),`), extend the crossterm import with `MouseButton, MouseEvent, MouseEventKind`, add this arm to `EventStream::next` before `Some(Ok(_)) => continue,`:

```rust
                        Some(Ok(CrosstermEvent::Mouse(m))) if wanted_mouse(&m) => {
                            return Ok(Event::Mouse(m));
                        }
```

and add the function:

```rust
/// The mouse events blink acts on: a left press and the wheel. Capture
/// reports every movement too; dropping the rest here keeps them from
/// waking the run loop into a redraw each.
fn wanted_mouse(m: &MouseEvent) -> bool {
    matches!(
        m.kind,
        MouseEventKind::Down(MouseButton::Left) | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    )
}
```

In `src/tui/app/mod.rs`: add the run-loop arm `Event::Mouse(m) => self.handle_mouse(m),` after `Event::Key(k) => self.handle_key(k),`; add to `App` (after `hit`):

```rust
    /// The last left press, for double-click detection. See `mouse.rs`.
    last_click: Option<mouse::LastClick>,
```

initialised `last_click: None,` in `App::new`.

In `src/tui/app/mouse.rs`, above the test module:

```rust
//! Mouse input: clicks, double-clicks and the wheel.
//!
//! Events are matched against the [`crate::tui::hit::HitMap`] the last draw
//! recorded. Only the main view, the viewer and the session selector take
//! the mouse; on every other screen — each modal and prompt — it does
//! nothing, including to the panes drawn behind the modal.

use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use super::{App, Pane, Screen};
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
        if self.screen == Screen::Main {
            self.mouse_main(m, now);
        }
    }

    /// Record a press on `target`; true when it completes a double-click. A
    /// double-click consumes the pair, so a third quick press starts over.
    pub(super) fn is_double_click(&mut self, target: ClickTarget, now: Instant) -> bool {
        let double = self
            .last_click
            .as_ref()
            .is_some_and(|c| c.target == target && now.duration_since(c.at) <= DOUBLE_CLICK);
        self.last_click = if double { None } else { Some(LastClick { target, at: now }) };
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

    /// A double-click on the cursor entry of `pane`: enter a directory
    /// (or go up, for `..`).
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
        }
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo fmt && cargo test --quiet 2>&1 | grep -E "test result|FAILED"`
Expected: all pass (12 new). Then `cargo clippy --all-targets --quiet 2>&1 | grep -c ^warning` → `0`.

- [ ] **Step 5: Commit**

```bash
git add src/tui/event.rs src/tui/app/mouse.rs src/tui/app/mod.rs
git commit -m "feat(tui): clicks, double-clicks and the wheel in the file panes

Left presses and the wheel now leave the event reader; movement, drags
and releases are still dropped before they wake the loop. A click
focuses a pane and moves its cursor; a double-click on a directory
enters it; the wheel moves the cursor of the pane under it.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: Double-click transfers the clicked file

**Files:**
- Modify: `src/tui/app/transfers.rs` (`start_selected_uploads`, `enqueue_selected_downloads`)
- Modify: `src/tui/app/mouse.rs` (`open_or_transfer`)
- Test: `src/tui/app/mouse.rs` (`mod tests`)

**Interfaces:**
- Consumes: `open_or_transfer`, `ClickTarget` (Task 4).
- Produces: `App::start_uploads(&mut self, entries: Vec<(String, bool)>)`, `App::enqueue_downloads(&mut self, selections: Vec<(String, bool)>)` — the selection-taking functions become thin wrappers over these.

- [ ] **Step 1: Write the failing tests** — in `src/tui/app/mouse.rs` `mod tests`:

```rust
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
        a.handle_mouse_at(press(list.x + 2, list.y + r), t + Duration::from_millis(100));
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the walk reports")
            .unwrap();
        match ev {
            AppEvent::WalkComplete { plan, .. } => plan
                .iter()
                .map(|j| match j {
                    crate::tui::plan::PlannedJob::Upload { remote_path, .. }
                    | crate::tui::plan::PlannedJob::Download { remote_path, .. } => remote_path.clone(),
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
        assert_eq!(roots, vec!["/r/file002".to_string()], "the clicked file alone");
        assert_eq!(a.local.selected_count(), 1, "the selection is left as it was");
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
        a.local.set_entries(vec![PaneEntry::parent(), PaneEntry::new("f".into(), false, 1)]);
        draw(&a);
        let (c, r) = local_row(&a, 0);
        let t = Instant::now();
        a.handle_mouse_at(press(c, r), t);
        a.handle_mouse_at(press(c, r), t + Duration::from_millis(100));
        assert_eq!(a.local.path, "/tmp");
        assert_eq!(a.transfer_manager.as_ref().unwrap().queue_counts(), (0, 0));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --quiet -- alone parent_row 2>&1 | grep -E "^    [a-z]|test result"`
Expected: the two `alone` tests fail (the walk never reports: a file double-click does nothing yet).

- [ ] **Step 3: Implement.** In `src/tui/app/transfers.rs`, split `start_selected_uploads` into the selection wrapper and an explicit-roots function:

```rust
    /// Enqueue uploads for the selected items in the local pane. If
    /// nothing is selected, falls back to the cursor item.
    pub(super) fn start_selected_uploads(&mut self) {
        if self.transfer_manager.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }
        let entries = self.local.selection();
        if entries.is_empty() {
            self.push_log(LogLevel::Warn, "no items to upload".into());
            return;
        }
        // Clear selection upfront — the walk task is async, and we'd rather
        // not surprise the user later if they select more items meanwhile.
        self.local.clear_selection();
        self.start_uploads(entries);
    }

    /// Upload `entries` — `(raw_name, is_dir)` in the local pane's directory
    /// — through the usual conflict check and dispatch. Leaves the selection
    /// alone: a double-click uploads just the file it landed on.
    pub(super) fn start_uploads(&mut self, entries: Vec<(String, bool)>) {
        if self.transfer_manager.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }
```

followed by the existing body from `let local_base = PathBuf::from(&self.local.path);` through `self.start_upload_walk(roots);`, unchanged. Do the same for downloads:

```rust
    pub(super) fn enqueue_selected_downloads(&mut self) {
        if self.transfer_manager.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }
        let selections = self.remote.selection();
        if selections.is_empty() {
            self.push_log(LogLevel::Warn, "no items to download".into());
            return;
        }
        self.remote.clear_selection();
        self.enqueue_downloads(selections);
    }

    /// Download `selections` — `(raw_name, is_dir)` in the remote pane's
    /// directory — through the usual conflict check and dispatch. Leaves the
    /// selection alone.
    pub(super) fn enqueue_downloads(&mut self, selections: Vec<(String, bool)>) {
        if self.transfer_manager.is_none() {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        }
```

followed by the existing body from `let local_base = PathBuf::from(&self.local.path);` through `self.start_download_walk(roots);`.

In `src/tui/app/mouse.rs`, replace `open_or_transfer` with:

```rust
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
```

- [ ] **Step 4: Run the tests**

Run: `cargo fmt && cargo test --quiet 2>&1 | grep -E "test result|FAILED"`
Expected: all pass (3 new).

- [ ] **Step 5: Commit**

```bash
git add src/tui/app/transfers.rs src/tui/app/mouse.rs
git commit -m "feat(tui): double-click a file to transfer it alone

Remote files download, local files upload, through the usual conflict
check and duplicate rule; the selection is ignored and left untouched.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 6: Bottom pane, viewer and session selector; documentation

**Files:**
- Modify: `src/tui/app/mouse.rs` (`handle_mouse_at`, `mouse_main`, new `mouse_viewer`, `mouse_session_select`)
- Modify: `README.md` (Hotkeys: a Mouse subsection)
- Modify: `CHANGELOG.md` (Unreleased: Added)
- Test: `src/tui/app/mouse.rs` (`mod tests`)

**Interfaces:**
- Consumes: everything above; `App::move_transfer_cursor`, `App::active_jobs`, `App::viewer_scroll`, `App::handle_session_select`, `BottomPane`.
- Produces: nothing further.

- [ ] **Step 1: Write the failing tests** — in `src/tui/app/mouse.rs` `mod tests`:

```rust
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
            m.enqueue_download(format!("/r/{i}"), format!("/l/{i}").into()).unwrap();
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
        a.handle_mouse_at(press(rows.x + 4, rows.y + 1), t + Duration::from_millis(100));
        assert_eq!(a.screen, Screen::PasswordPrompt, "password auth asks first");
        assert_eq!(a.pending_session.as_ref().unwrap().host, "h1.example");
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --quiet -- tab_shows transfer_row viewer_text a_session 2>&1 | grep -E "^    [a-z]|test result"`
Expected: all five fail.

- [ ] **Step 3: Implement.** In `src/tui/app/mouse.rs`: add `Session(usize),` to `ClickTarget`, and replace the body of `handle_mouse_at` with:

```rust
        match self.screen {
            Screen::Main => self.mouse_main(m, now),
            Screen::Viewer => self.mouse_viewer(m),
            Screen::SessionSelect => self.mouse_session_select(m, now),
            _ => {}
        }
```

add `use super::BottomPane;` and `use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};` to the imports, append to the end of `mouse_main` (after the pane loop):

```rust
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
```

and add:

```rust
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
```

In `README.md`, after the viewer-keys paragraph under `## Hotkeys` ("In the viewer: …"), add:

```markdown
### Mouse

On by default; `mouse = false` under `[terminal]` in `config.ini` turns it
off and leaves the terminal's own text selection alone.

- **Click** a file pane to focus it; click a row to move the cursor there.
- **Double-click** a directory (or `..`) to enter it; a remote file to
  download it, or a local file to upload it — just that file, whatever
  else is selected.
- **Scroll wheel** moves the cursor in the pane under the mouse, the
  transfer list, and the session list, and scrolls the viewer.
- **Click** the TRANSFERS / LOG tabs to switch them, and a transfer to select
  it. In the session selector, click a session to select it and
  double-click to connect.

Prompts and modals ignore the mouse. With capture on, most terminals need
Shift held to select text.
```

In `CHANGELOG.md`, under `## [Unreleased]`, add:

```markdown
### Added

- **Mouse support.** Click to focus a pane and move its cursor;
  double-click a directory to enter it, or a file to transfer it — a
  remote file downloads, a local one uploads, just that file. The scroll
  wheel moves the cursor in the file panes, the transfer list and the
  session list, and scrolls the viewer; the bottom pane's tabs and
  transfers respond to clicks, and a session double-clicked connects.
  `mouse = false` under `[terminal]` in `config.ini` turns it off, leaving
  mouse capture off so the terminal's own text selection works. File panes
  now keep their scroll position while the cursor stays in view, instead
  of shifting with every move.
```

- [ ] **Step 4: Run everything**

Run: `cargo fmt && cargo fmt --check && cargo clippy --all-targets --quiet 2>&1 | grep -c ^warning; cargo doc --quiet 2>&1 | grep -c ^warning; cargo test --quiet 2>&1 | grep -E "test result|FAILED"; cargo check --quiet --target x86_64-pc-windows-gnu`
Expected: `0`, `0`, all tests pass (5 new), Windows check clean.

- [ ] **Step 5: Commit**

```bash
git add src/tui/app/mouse.rs README.md CHANGELOG.md
git commit -m "feat(tui): the mouse in the bottom pane, viewer and session selector

Tabs and transfer rows respond to clicks, the wheel moves the transfer
cursor and scrolls viewer text, and the session selector selects on a
click and connects on a double-click. Documents mouse support.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```
