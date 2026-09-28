# Mouse Support

**Date:** 2026-09-28  
**Status:** Draft, for review  

## Summary

blink turns mouse capture on at startup and then throws every mouse event
away. It pays the cost — most terminals need Shift held to select text while
capture is on — and gives nothing back. This adds the core of mouse support:
click to focus and move the cursor, double-click to enter a directory or
transfer a file, the scroll wheel in lists and the viewer, clicks on the
bottom pane's tabs and the session list. A `mouse` setting in `config.ini`,
on by default, turns it all off and leaves capture off for those who would
rather have the terminal's own selection.

Out of scope, by decision: right-click or Ctrl-click selection, clickable
buttons in prompts, drag and drop, drag selection, resizable panes.

## What exists today

- `tui::setup` runs `EnableMouseCapture` unconditionally. crossterm's capture
  turns on any-event tracking (`?1003h`), so every movement of the mouse is
  reported.
- `EventStream::next` (`tui/event.rs`) drops every event that is not a key
  press or a resize (`Some(Ok(_)) => continue`), before it can wake the run
  loop.
- Rendering takes `&App`. Layout rectangles are computed inside the render
  functions and not kept, so nothing records what is on screen where.
- The file panes keep no scroll offset: `file_pane::render` derives the
  visible window from the cursor alone (`window_start = cursor + 1 - height`).
  The transfer list shows its first `height` jobs; the session list draws
  two header lines, then one line per session.

## Design

### Where things are: a hit map recorded while drawing

`App` gains a `HitMap`: one `Cell<Option<Rect>>` per clickable area. `App::draw`
clears it before drawing; each render function that draws a clickable area
records its rectangle as it draws. A click is then matched against exactly
what was last drawn. The render functions keep taking `&App`; `Cell` is what
lets them record through a shared reference.

Areas recorded:

| Area | Recorded by | Rectangle |
|---|---|---|
| local list, remote list | `file_pane::render` | the rows of entries, inside the border, path and footer lines excluded |
| local pane, remote pane | `file_pane::render` | the whole pane including its border, for focus |
| transfers tab, log tab | `bottom_pane::render` | each tab's label in the title row |
| transfer list | `render_transfers` | the rows of jobs |
| viewer body | `views::viewer::render` | the text body |
| session list | `render_session_list` | the rows of sessions, below the two header lines |

Rejected: recomputing layouts in the click handler from the terminal size.
That duplicates every layout, and a duplicated layout is how the image area
once drifted from what `views::viewer` drew.

A row's entry comes from the same window arithmetic the renderer uses:
`file_pane::render`'s window calculation moves into one function,
`visible_window(cursor, len, height) -> Range<usize>`, used by both. The
transfer and session lists map row `r` of their recorded area to index `r`.
A click below the last entry hits nothing.

### Events

`Event` gains `Mouse(MouseEvent)`. `EventStream::next` passes on only a
left-button press, a scroll up and a scroll down; moves, drags, releases and
other buttons are dropped where they are today, so they never wake the loop
or cost a redraw.

crossterm reports no double-clicks. `App` keeps the last left press — the
area and entry index it hit, and when — and a second press on the same entry
of the same area within 400 ms is a double-click. Anything else starts over.

### Gestures

Only these screens respond. On every other screen — each modal and prompt —
the mouse does nothing, including to the panes behind the modal.

**Main view**

- **Click in a file pane**: that pane becomes active; if the click is on a
  row, the cursor moves to it.
- **Double-click on a row**:
  - a directory, or the `..` row: enter it, as Enter and Backspace do;
  - a file in the remote pane: download that file alone;
  - a file in the local pane: upload that file alone.
  "That file alone" means the selection is ignored and left untouched, where
  Ctrl-D and Ctrl-U take the selection. The transfer otherwise takes the
  usual path — overwrite prompt, one-job-per-destination check, checkpoint —
  through a variant of `enqueue_selected_downloads` / `start_selected_uploads`
  that takes its roots explicitly.
- **Scroll wheel over a file pane**: that pane's cursor moves three rows,
  without changing which pane is active.
- **Click on the TRANSFERS or LOG tab**: shows that page and makes the bottom
  pane active.
- **Click on a transfer row**: the bottom pane becomes active and the
  transfer cursor moves to that row. Wheel over the list: the transfer cursor
  moves three rows.

**Viewer**: the wheel scrolls the text three lines per notch, through
`viewer_scroll`. Images do not scroll.

**Session selector**: a click selects a session; a double-click connects, as
Enter does; the wheel moves the selection.

### The setting

`config.ini`:

```ini
[terminal]
image_preview = auto
mouse = true        ; false leaves mouse capture off: the terminal's own
                    ; text selection works, and blink ignores the mouse
```

Parsed with the existing `parse_bool`; absent means `true`. `Config::save`
writes it, so a theme cycle keeps it. It is read at startup only: `tui::setup`
takes it and runs `EnableMouseCapture` only when it is on, and restore
disables capture either way, which is harmless when it was never enabled.
With it off, no mouse event reaches the app, so no handler needs to check it.

## Testing

The app is drawn into ratatui's `TestBackend`, which fills the hit map
exactly as a real terminal would; mouse events are then fed to the handler
and the result checked on `App`'s state. No real terminal is involved.

- clicking each recorded area does what the table above says, and a click
  outside every area, or below the last entry, does nothing;
- a double-click needs the same entry within 400 ms: two slow clicks, or two
  quick clicks on different rows, are two single clicks;
- a double-click on a remote file queues a download of that file only, and
  on a local file an upload of that file only, with other files selected;
  on a directory it enters it;
- the wheel over each area moves the right cursor, and the viewer's scroll;
- with a modal open, clicks on the panes behind it change nothing;
- `visible_window` gives the renderer's window for cursors at the top, the
  middle and the end of a list longer than the pane;
- the `mouse` setting parses, defaults to on, and survives `Config::save`;
- `EventStream` drops movement events.

## Documentation

- README: a "Mouse" subsection under Hotkeys listing the gestures, and the
  `mouse` key in the `config.ini` example.
- CHANGELOG: an Added entry.
