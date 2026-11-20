//! Custom render widgets used by the views.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::tui::app::{App, Pane};

/// Columns `s` takes in the terminal.
fn columns(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Columns `c` takes in the terminal.
fn char_columns(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Truncate `s` to `max` terminal columns by replacing the middle with `…`,
/// so both the head and the tail of a long path stay readable.
///
/// Measured in columns, not characters: a CJK or emoji character takes
/// two, and counting it as one left a "shortened" path still wider than
/// its room, for ratatui to cut at the right — losing the tail this keeps.
pub(crate) fn truncate_middle(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if columns(s) <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".into();
    }
    let keep = max - 1;
    let mut head = String::new();
    let mut head_w = 0;
    for c in s.chars() {
        let w = char_columns(c);
        if head_w + w > keep / 2 {
            break;
        }
        head.push(c);
        head_w += w;
    }
    // Whatever the head left over, a wide character not fitting, goes to
    // the tail.
    let mut tail = Vec::new();
    let mut tail_w = 0;
    for c in s.chars().rev() {
        let w = char_columns(c);
        if tail_w + w > keep - head_w {
            break;
        }
        tail.push(c);
        tail_w += w;
    }
    let tail: String = tail.into_iter().rev().collect();
    format!("{head}…{tail}")
}

/// Columns the timestamp takes before a log message: `" [HH:MM:SS] "`.
pub(crate) const LOG_PREFIX_W: usize = 12;

/// `message` broken into the rows the log pane draws it on, in a pane
/// `width` columns wide: each at most the width less the timestamp, broken
/// after a space where one falls in the row, and mid-word where none does.
///
/// The log is the only place some errors are given in full, and a line
/// running past the pane used to be cut off there. The app counts rows
/// with this too, to scroll by them, so the two can't disagree.
pub(crate) fn log_message_rows(message: &str, width: usize) -> Vec<String> {
    let room = width.saturating_sub(LOG_PREFIX_W).max(1);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut row_w = 0;
    // The byte just past the last space in `row`, and the columns up to it.
    let mut after_space: Option<(usize, usize)> = None;
    for c in message.chars() {
        let w = char_columns(c);
        if row_w + w > room && !row.is_empty() {
            match after_space.take() {
                Some((at, at_w)) if at < row.len() => {
                    let rest = row.split_off(at);
                    rows.push(row.trim_end().to_string());
                    row = rest;
                    row_w -= at_w;
                }
                _ => {
                    rows.push(std::mem::take(&mut row).trim_end().to_string());
                    row_w = 0;
                }
            }
        }
        // A row broken off just before a space doesn't open with it.
        if c == ' ' && row.is_empty() && !rows.is_empty() {
            continue;
        }
        row.push(c);
        row_w += w;
        if c == ' ' {
            after_space = Some((row.len(), row_w));
        }
    }
    rows.push(row);
    rows
}

// ---------------------------------------------------------------------------
// File pane (used for both Local and Remote)
// ---------------------------------------------------------------------------

pub mod file_pane {
    use super::*;

    pub fn render(f: &mut Frame, app: &App, area: Rect, which: Pane) {
        let active = app.active_pane == which;
        let (state, label) = match which {
            Pane::Local => (&app.local, "LOCAL"),
            Pane::Remote => (&app.remote, "REMOTE"),
            Pane::Transfers | Pane::Log => return,
        };

        let border_color = if active {
            app.theme.border_active
        } else {
            app.theme.border_inactive
        };
        let title = if active {
            format!(" {label} · active ")
        } else {
            format!(" {label} ")
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(Span::styled(
                title,
                Style::default()
                    .fg(border_color)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        f.render_widget(block, area);

        // path (1) | list (flex) | footer (1)
        let inner_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);

        // Path line, with the active filter (if any) shown inline. A remote
        // path is made of the server's names, so it is sanitized like they
        // are; a long one loses its middle, not the end that says where
        // the pane is.
        let filter = state
            .filter
            .as_ref()
            .map(|q| format!("  /{}", crate::error::sanitize(q.clone())));
        let filter_w = filter.as_deref().map_or(0, columns);
        let path_room = usize::from(inner.width).saturating_sub(1 + filter_w);
        let mut path_spans = vec![Span::styled(
            format!(
                " {}",
                truncate_middle(&crate::error::sanitize(state.path.clone()), path_room)
            ),
            Style::default().fg(app.theme.dim),
        )];
        if let Some(filter) = filter {
            path_spans.push(Span::styled(
                filter,
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(path_spans)), inner_layout[0]);

        // Entry list, windowed around cursor.
        let list_area = inner_layout[1];
        app.hit.set(|a| match which {
            Pane::Local => {
                a.local_pane = Some(area);
                a.local_list = Some(list_area);
            }
            _ => {
                a.remote_pane = Some(area);
                a.remote_list = Some(list_area);
            }
        });
        let h = list_area.height as usize;
        let len = state.entries.len();
        // One cursor for both the highlight and the scrolling.
        let cursor = state.cursor.min(len.saturating_sub(1));
        let window = crate::tui::state::scrolled_window(state.view_offset.get(), cursor, len, h);
        state.view_offset.set(window.start);

        let mut lines = Vec::with_capacity(window.len());
        for i in window {
            let e = &state.entries[i];
            let is_cursor = i == cursor;

            let (icon, base_style) = if e.is_dir {
                ("▸ ", Style::default().fg(app.theme.directory))
            } else if e.is_link {
                // What it points at is only known once it is opened.
                ("↪ ", Style::default().fg(app.theme.fg))
            } else if e.previewable_image {
                ("  ", Style::default().fg(app.theme.image))
            } else {
                ("  ", Style::default().fg(app.theme.fg))
            };
            let is_selected = state.is_selected(e);
            let row_style = if is_selected {
                Style::default().fg(app.theme.selected)
            } else {
                base_style
            };
            let sel_marker = if is_selected { "☑ " } else { "" };

            // Right-align the size column.
            let size_str = if e.is_dir {
                "—".to_string()
            } else {
                crate::transfer::format_bytes(e.size)
            };
            // The name gets what the row has left after the lead, icon,
            // marker, a gap, the size and the two columns after it, losing
            // its middle past that as a transfer's does. Uncut, a long name
            // pushed the size off the pane and its extension with it.
            let avail = list_area.width as usize;
            let fixed = 1 + columns(icon) + columns(sel_marker) + 1 + columns(&size_str) + 2;
            let name = truncate_middle(&e.display_name, avail.saturating_sub(fixed));

            let mut row = vec![
                Span::raw(" "),
                Span::raw(icon),
                Span::styled(sel_marker.to_string(), row_style),
                Span::styled(name, row_style),
            ];
            // Display width, so a CJK or emoji name is padded by the cells
            // it occupies.
            let used: usize = row.iter().map(|s| s.width()).sum();
            let pad = avail.saturating_sub(used + columns(&size_str) + 2);
            row.push(Span::raw(" ".repeat(pad)));
            row.push(Span::styled(size_str, Style::default().fg(app.theme.dim)));
            row.push(Span::raw(" "));

            let mut line = Line::from(row);
            if is_cursor && active {
                line = line.style(Style::default().bg(app.theme.cursor_bg));
            }
            lines.push(line);
        }
        f.render_widget(Paragraph::new(lines), list_area);

        // Footer
        // Counts the whole selection, including entries the active filter is
        // hiding — the footer should report what a transfer would act on.
        let selected_count = state.selected_count();
        let selected_size: u64 = state.selected_size();
        // `..` is a way out, not an item. With a filter on, the rows shown
        // are counted against the folder's, so "3 items" doesn't read as the
        // whole of a folder of 400.
        let items = state.entries.iter().filter(|e| !e.is_parent()).count();
        let footer_text = format!(
            " {}{}",
            item_count(
                items,
                state
                    .filter
                    .is_some()
                    .then(|| { state.all_rows().iter().filter(|e| !e.is_parent()).count() })
            ),
            if selected_count == 0 {
                String::new()
            } else {
                format!(
                    " · {} selected ({})",
                    selected_count,
                    crate::transfer::format_bytes(selected_size),
                )
            }
        );
        f.render_widget(
            Paragraph::new(Span::styled(
                footer_text,
                Style::default().fg(app.theme.dim),
            )),
            inner_layout[2],
        );
    }

    /// `shown` items, or `shown of total` while a filter hides some, in the
    /// singular for one.
    pub(crate) fn item_count(shown: usize, total: Option<usize>) -> String {
        let of = total.unwrap_or(shown);
        let noun = if of == 1 { "item" } else { "items" };
        match total {
            Some(total) => format!("{shown} of {total} {noun}"),
            None => format!("{shown} {noun}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Bottom pane (Transfers + Log, switchable via Tab)
// ---------------------------------------------------------------------------

pub mod bottom_pane {
    use super::*;
    use crate::tui::app::{BottomPane, LogLevel};

    pub fn render(f: &mut Frame, app: &App, area: Rect) {
        app.hit.set(|a| a.bottom_pane = Some(area));
        let focused = matches!(app.active_pane, Pane::Transfers | Pane::Log);
        let border_color = if focused {
            app.theme.border_active
        } else {
            app.theme.border_inactive
        };

        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(tab_title(app));
        let body = Block::default().borders(Borders::ALL).inner(area);
        let log = (app.bottom_pane == BottomPane::Log).then(|| log_window(app, body));
        let corner = match &log {
            // Scrolled back, say so: new lines arrive out of sight.
            Some(w) if w.newer > 0 => Some(format!(" ↓ {} newer ", w.newer)),
            Some(_) => None,
            // Only running jobs have rows; the queue behind them, which
            // quitting would discard too, is counted here, so a batch
            // waiting behind the limit or a pause isn't invisible.
            None => match app.queue_counts() {
                (_, 0) => None,
                (_, pending) => Some(format!(" +{pending} queued ")),
            },
        };
        if let Some(text) = corner {
            block = block.title_top(
                Line::from(Span::styled(text, Style::default().fg(app.theme.accent)))
                    .right_aligned(),
            );
        }
        let inner = block.inner(area);
        f.render_widget(block, area);

        // The tab labels sit in the title row, one cell in from the corner:
        // " " then " TRANSFERS " then "·" then " LOG ", as `tab_title` draws.
        // Clipped to the pane: in a terminal too narrow for the title,
        // ratatui cuts the labels short, and a label's area ran on past
        // them, so a click on the pane's corner switched to a tab nobody
        // could see.
        let transfers_x = area.x.saturating_add(2);
        let log_x = transfers_x.saturating_add(TRANSFERS_TAB.len() as u16 + 1);
        let tab = |x: u16, label: &str| {
            Some(Rect::new(x, area.y, label.len() as u16, 1).intersection(tab_row(area)))
                .filter(|r| !r.is_empty())
        };
        app.hit.set(|a| {
            a.transfers_tab = tab(transfers_x, TRANSFERS_TAB);
            a.log_tab = tab(log_x, LOG_TAB);
        });

        match log {
            None => render_transfers(f, app, inner),
            Some(w) => {
                app.hit.set(|a| a.log_body = Some(inner));
                f.render_widget(Paragraph::new(w.lines), inner);
            }
        }
    }

    /// Where a title's text can go: the top row inside the corners.
    fn tab_row(area: Rect) -> Rect {
        Rect::new(
            area.x.saturating_add(1),
            area.y,
            area.width.saturating_sub(2),
            area.height.min(1),
        )
    }

    const TRANSFERS_TAB: &str = " TRANSFERS ";
    const LOG_TAB: &str = " LOG ";

    /// Title line that doubles as a tab indicator. The currently-displayed
    /// page is rendered in the active border color; the other in dim.
    fn tab_title(app: &App) -> Line<'static> {
        let on_transfers = app.bottom_pane == BottomPane::Transfers;
        let active_style = Style::default()
            .fg(app.theme.border_active)
            .add_modifier(Modifier::BOLD);
        let inactive_style = Style::default().fg(app.theme.dim);

        Line::from(vec![
            Span::raw(" "),
            Span::styled(
                TRANSFERS_TAB,
                if on_transfers {
                    active_style
                } else {
                    inactive_style
                },
            ),
            Span::styled("·", inactive_style),
            Span::styled(
                LOG_TAB,
                if !on_transfers {
                    active_style
                } else {
                    inactive_style
                },
            ),
            Span::raw(" "),
        ])
    }

    // ----- Transfers page --------------------------------------------------

    fn render_transfers(f: &mut Frame, app: &App, area: Rect) {
        app.hit.set(|a| a.transfer_list = Some(area));
        let jobs = app.active_jobs();
        let paused = app
            .transfer_manager
            .as_ref()
            .map(|m| m.is_paused())
            .unwrap_or(false);
        if jobs.is_empty() {
            let text = match (app.queue_counts().1, paused) {
                (0, _) => " no active transfers".to_string(),
                (n, true) => format!(" none running · {n} queued, paused"),
                (n, false) => format!(" none running · {n} queued"),
            };
            let p = Paragraph::new(Line::from(Span::styled(
                text,
                Style::default().fg(app.theme.dim),
            )));
            f.render_widget(p, area);
            return;
        }

        let focused = app.active_pane == Pane::Transfers;
        let cursor = app.transfer_cursor.min(jobs.len().saturating_sub(1));

        // Up to MAX_PARALLEL jobs run at once, more than the pane has rows:
        // the list scrolls to keep the cursor, and what `c` cancels, in view.
        let window = crate::tui::state::scrolled_window(
            app.transfer_view_offset.get(),
            cursor,
            jobs.len(),
            area.height as usize,
        );
        app.transfer_view_offset.set(window.start);
        let mut lines = Vec::with_capacity(area.height as usize);
        for (i, job) in jobs
            .iter()
            .enumerate()
            .skip(window.start)
            .take(window.len())
        {
            let is_cursor = i == cursor && focused;
            lines.push(render_transfer_line(
                app,
                area.width as usize,
                job,
                paused,
                is_cursor,
            ));
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    /// One row in the active-transfers list: arrow, name, percent, bar, speed.
    /// Filenames longer than the available width are truncated middle-out so
    /// both head and tail stay visible.
    fn render_transfer_line(
        app: &App,
        width: usize,
        job: &crate::transfer::TransferJob,
        paused: bool,
        is_cursor: bool,
    ) -> Line<'static> {
        const BAR_WIDTH: usize = 12;

        let percent: u8 = if job.bytes_total == 0 {
            0
        } else {
            (((job.bytes_done as f64 / job.bytes_total as f64) * 100.0).clamp(0.0, 100.0)) as u8
        };
        let speed_str = if paused {
            "paused".to_string()
        } else if job.bytes_per_sec == 0 {
            "—".to_string()
        } else {
            let speed = crate::transfer::format_bytes_per_sec(job.bytes_per_sec);
            let remaining = job.bytes_total.saturating_sub(job.bytes_done);
            if remaining > 0 {
                format!(
                    "{speed} ETA {}",
                    crate::transfer::format_eta(remaining, job.bytes_per_sec)
                )
            } else {
                speed
            }
        };

        let percent_str = format!("{percent:3}%");
        // The leading space and the arrow's two columns before the name,
        // and the one space after it before the percent.
        let prefix_w = 1 + 2 + 1;
        let right_w = columns(&percent_str) + 1 + BAR_WIDTH + 1 + columns(&speed_str) + 1;
        let avail_for_name = width.saturating_sub(prefix_w + right_w);

        // Padded by columns: a wide name counted by characters pushed the
        // percent, bar and speed off the row.
        let display_name = crate::tui::app::name_for_job(job);
        let name_str = super::truncate_middle(&display_name, avail_for_name);
        let name_pad = avail_for_name.saturating_sub(columns(&name_str));

        let percent_color = if paused {
            app.theme.dim
        } else if percent >= 100 {
            app.theme.success
        } else {
            app.theme.fg
        };
        let bar_color = if paused {
            app.theme.dim
        } else {
            app.theme.success
        };
        let bar_dim = app.theme.border_inactive;
        let filled = (BAR_WIDTH * percent as usize / 100).min(BAR_WIDTH);
        let empty = BAR_WIDTH - filled;

        let arrow = match job.direction {
            crate::transfer::Direction::Download => "↓ ",
            crate::transfer::Direction::Upload | crate::transfer::Direction::CreateDir => "↑ ",
        };
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                arrow,
                Style::default()
                    .fg(app.theme.directory)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(name_str, Style::default().fg(app.theme.fg)),
            Span::raw(" ".repeat(name_pad)),
            Span::raw(" "),
            Span::styled(
                percent_str,
                Style::default()
                    .fg(percent_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled("█".repeat(filled), Style::default().fg(bar_color)),
            Span::styled("░".repeat(empty), Style::default().fg(bar_dim)),
            Span::raw(" "),
            Span::styled(speed_str, Style::default().fg(app.theme.dim)),
            Span::raw(" "),
        ]);
        if is_cursor {
            line.style(Style::default().bg(app.theme.cursor_bg))
        } else {
            line
        }
    }

    /// [`render_transfer_line`] for a job, unpaused and off the cursor.
    #[cfg(test)]
    pub(crate) fn transfer_line_for_test(
        app: &App,
        width: usize,
        job: &crate::transfer::TransferJob,
    ) -> Line<'static> {
        render_transfer_line(app, width, job, false, false)
    }

    // ----- Log page -------------------------------------------------------

    /// What the log page draws in `area`, and how many entries lie wholly
    /// below it.
    struct LogWindow {
        lines: Vec<Line<'static>>,
        newer: usize,
    }

    /// The rows of the log that fit `area`, scrolled back
    /// [`App::log_scroll`] rows from the newest.
    ///
    /// Each entry wraps onto as many rows as its message takes (see
    /// [`super::log_message_rows`]), continuation rows indented under the
    /// message rather than the timestamp. A long error used to run off the
    /// pane's edge with nothing to show the rest.
    fn log_window(app: &App, area: Rect) -> LogWindow {
        let h = usize::from(area.height);
        let width = usize::from(area.width);
        // Rows newest first, walked back only as far as the window needs;
        // `ends[i]` is where the `i`th-newest entry's rows stop.
        let mut rev: Vec<Line<'static>> = Vec::new();
        let mut ends = Vec::new();
        for entry in app.log.iter().rev() {
            let level_color = match entry.level {
                LogLevel::Info => app.theme.dim,
                LogLevel::Success => app.theme.success,
                LogLevel::Warn => app.theme.warning,
                LogLevel::Error => app.theme.error,
            };
            let ts = entry.time.format("%H:%M:%S").to_string();
            let rows = super::log_message_rows(&entry.message, width);
            let mut lines: Vec<Line<'static>> = rows
                .into_iter()
                .enumerate()
                .map(|(i, text)| {
                    let lead = if i == 0 {
                        Span::styled(format!(" [{ts}] "), Style::default().fg(app.theme.dim))
                    } else {
                        Span::raw(" ".repeat(super::LOG_PREFIX_W))
                    };
                    Line::from(vec![
                        lead,
                        Span::styled(text, Style::default().fg(level_color)),
                    ])
                })
                .collect();
            while let Some(line) = lines.pop() {
                rev.push(line);
            }
            ends.push(rev.len());
            if rev.len() >= app.log_scroll.saturating_add(h) {
                break;
            }
        }
        // Stopped early, the window is exactly `log_scroll` rows back;
        // having run out, it is held with the first entry at the top.
        let skip = app.log_scroll.min(rev.len().saturating_sub(h));
        let newer = ends.iter().filter(|&&end| end <= skip).count();
        let mut lines: Vec<Line<'static>> = rev.drain(skip..).take(h).collect();
        lines.reverse();
        LogWindow { lines, newer }
    }

    #[cfg(test)]
    mod tests {
        use crate::transfer::{Direction, TransferJob, TransferState};
        use crate::tui::app::name_for_job as display_name_for;

        fn job(remote: &str) -> TransferJob {
            TransferJob {
                id: 1,
                direction: Direction::Download,
                remote_path: remote.to_string(),
                local_path: std::path::PathBuf::from("/tmp/x"),
                bytes_total: 0,
                bytes_done: 0,
                bytes_per_sec: 0,
                state: TransferState::Pending,
                batch_id: None,
            }
        }

        #[test]
        fn transfer_row_name_is_sanitized() {
            // Remote paths carry the server's own bytes now, and this row is
            // rendered straight into the terminal.
            let name = display_name_for(&job("/srv/re\u{202E}port.txt"));
            assert!(
                !name.contains('\u{202E}'),
                "bidi override reached the pane: {name:?}"
            );
            assert_eq!(name, "re port.txt");
        }

        #[test]
        fn transfer_row_name_strips_escape_sequences() {
            let name = display_name_for(&job("/srv/a\u{1b}[31mb.txt"));
            assert!(!name.contains('\u{1b}'), "ESC reached the pane: {name:?}");
        }

        #[test]
        fn ordinary_transfer_row_name_is_the_basename() {
            assert_eq!(display_name_for(&job("/srv/data/report.pdf")), "report.pdf");
        }

        // The overwrite-confirmation modal is the third place a
        // server-supplied name reaches the screen, and it sits next to the
        // two above deliberately: all three state one policy. The modal is
        // the prompt `is_deceptive_format` exists for — its doc comment gives
        // the reason as stopping a name that would "disguise a name the user
        // already cleared through the overwrite prompt" — so two distinct
        // remote names must not render identically in it.
        #[test]
        fn overwrite_modal_name_is_sanitized() {
            use crate::tui::views::conflict_row_names;
            let name = conflict_row_names(&["/srv/re\u{202E}port.txt"]).remove(0);
            assert!(
                !name.contains('\u{202E}'),
                "bidi override reached the modal: {name:?}",
            );
            assert_eq!(name, "re port.txt");
        }

        #[test]
        fn overwrite_modal_name_strips_escape_sequences() {
            use crate::tui::views::conflict_row_names;
            let name = conflict_row_names(&["/srv/a\u{1b}[31mb.txt"]).remove(0);
            assert!(!name.contains('\u{1b}'), "ESC reached the modal: {name:?}");
        }

        #[test]
        fn ordinary_overwrite_modal_name_is_the_basename() {
            use crate::tui::views::conflict_row_names;
            assert_eq!(
                conflict_row_names(&["/srv/data/report.pdf"]).remove(0),
                "report.pdf"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Status bar (contextual hotkey hints)
// ---------------------------------------------------------------------------

pub mod status_bar {
    use super::*;

    pub fn render_main(f: &mut Frame, app: &App, area: Rect) {
        let key_style = Style::default()
            .fg(app.theme.accent)
            .add_modifier(Modifier::BOLD);
        let dim_style = Style::default().fg(app.theme.dim);
        let bg = Style::default().bg(app.theme.cursor_bg).fg(app.theme.fg);

        let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
        for (key, what) in main_hints(app.active_pane) {
            spans.push(Span::styled(*key, key_style));
            spans.push(Span::raw(format!(" {what}  ")));
        }

        // Right-align the version. Drop it silently if the terminal is too
        // narrow for both halves to fit (matches the session-select bar).
        let version_label = format!("v{} ", env!("CARGO_PKG_VERSION"));
        let left_w: usize = spans.iter().map(Span::width).sum();
        let ver_w = Span::raw(version_label.as_str()).width();
        let total_w = area.width as usize;
        if left_w + ver_w + 2 <= total_w {
            let pad = total_w - left_w - ver_w;
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(version_label, dim_style));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(bg), area);
    }

    /// The keys the main view's bar names for the focused pane, as
    /// `(key, what it does)`.
    ///
    /// The same file-pane keys used to show whatever had focus: `[space]
    /// select` over the Transfers pane, where it does nothing, and never
    /// the keys that act there. Download was named but not upload.
    pub(crate) fn main_hints(pane: Pane) -> &'static [(&'static str, &'static str)] {
        match pane {
            Pane::Local | Pane::Remote => &[
                ("[?]", "help"),
                ("[↵]", "open"),
                ("[⌫]", "up"),
                ("[space]", "select"),
                ("[^d]", "download"),
                ("[^u]", "upload"),
                ("[v]", "view"),
                ("[^s]", "save"),
                ("[tab]", "switch"),
                ("[q]", "quit"),
            ],
            Pane::Transfers => &[
                ("[?]", "help"),
                ("[c]", "cancel"),
                ("[C]", "cancel batch"),
                ("[r/R]", "resume download/upload"),
                ("[p]", "pause"),
                ("[tab]", "switch"),
                ("[q]", "quit"),
            ],
            Pane::Log => &[
                ("[?]", "help"),
                ("[↑↓]", "scroll"),
                ("[pgup/pgdn]", "page"),
                ("[tab]", "switch"),
                ("[q]", "quit"),
            ],
        }
    }

    pub fn render_session_select(f: &mut Frame, app: &App, area: Rect) {
        let key_style = Style::default()
            .fg(app.theme.accent)
            .add_modifier(Modifier::BOLD);
        let dim_style = Style::default().fg(app.theme.dim);
        let bg = Style::default().bg(app.theme.cursor_bg).fg(app.theme.fg);

        // Left-aligned hotkey strip; right-aligned info cluster (theme + version).
        // Both right-side labels render only if the terminal is wide enough,
        // independently — narrow terminals just show fewer.
        let theme_label = format!("theme: {}  ", app.theme.name);
        let version_label = format!("v{} ", env!("CARGO_PKG_VERSION"));

        // Build the left half first.
        let mut left_spans: Vec<Span<'static>> = vec![
            Span::raw(" "),
            Span::styled("[↵]", key_style),
            Span::raw(" connect  "),
            Span::styled("[n]", key_style),
            Span::raw(" new  "),
            Span::styled("[e]", key_style),
            Span::raw(" edit  "),
            Span::styled("[d]", key_style),
            Span::raw(" delete  "),
            Span::styled("[t]", key_style),
            Span::raw(" theme  "),
            Span::styled("[q]", key_style),
            Span::raw(" quit"),
        ];

        // Display columns, not chars: a theme label in CJK is two columns a
        // character and overran the bar measured the other way.
        let left_w: usize = left_spans.iter().map(Span::width).sum();
        let theme_w = Span::raw(theme_label.as_str()).width();
        let ver_w = Span::raw(version_label.as_str()).width();
        let total_w = area.width as usize;
        // Try to fit theme + version. If only one fits, prefer the version
        // (more universally useful) over the theme (also visible in colors).
        if left_w + theme_w + ver_w + 2 <= total_w {
            let pad = total_w - left_w - theme_w - ver_w;
            left_spans.push(Span::raw(" ".repeat(pad)));
            left_spans.push(Span::styled(theme_label, dim_style));
            left_spans.push(Span::styled(version_label, dim_style));
        } else if left_w + ver_w + 2 <= total_w {
            let pad = total_w - left_w - ver_w;
            left_spans.push(Span::raw(" ".repeat(pad)));
            left_spans.push(Span::styled(version_label, dim_style));
        }
        f.render_widget(Paragraph::new(Line::from(left_spans)).style(bg), area);
    }
}

#[cfg(test)]
mod tests {
    use super::{LOG_PREFIX_W, log_message_rows, truncate_middle};
    use crate::tui::app::tests_support::{app_on_main, draw, draw_sized, press_key};
    use crate::tui::app::{App, BottomPane, LogLevel, Pane};
    use crate::tui::state::PaneEntry;

    fn text(buf: &ratatui::buffer::Buffer) -> String {
        let area = buf.area;
        (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn cols(s: &str) -> usize {
        unicode_width::UnicodeWidthStr::width(s)
    }

    // --- widths ---

    #[test]
    fn truncate_middle_keeps_head_and_tail_of_ascii() {
        assert_eq!(truncate_middle("abcdefghij", 5), "ab…ij");
        assert_eq!(truncate_middle("abc", 5), "abc");
        assert_eq!(truncate_middle("abc", 1), "…");
        assert_eq!(truncate_middle("abc", 0), "");
    }

    /// Counted in characters, nine CJK characters "fit" in nine columns and
    /// came back unshortened, eighteen wide.
    #[test]
    fn truncate_middle_counts_terminal_columns() {
        let s = "日本語日本語日本語";
        for max in 1..=18 {
            let out = truncate_middle(s, max);
            assert!(cols(&out) <= max, "{max}: {out:?} is {} wide", cols(&out));
        }
        let out = truncate_middle(s, 9);
        assert!(out.starts_with('日') && out.ends_with('語'), "{out:?}");
        assert_eq!(truncate_middle(s, 18), s);
    }

    /// A wide path was "shortened" to more than its room, and ratatui cut
    /// the right end: the part that says where the pane is.
    #[test]
    fn a_wide_path_keeps_its_tail_on_screen() {
        let mut a = app_on_main(1);
        a.local.path = format!("/{}TAIL-END", "写真/".repeat(40));
        let shown = text(&draw(&a));
        assert!(shown.contains("TAIL-END"), "{shown}");
    }

    /// An uncut long name pushed the size off the pane, and the extension
    /// with it.
    #[test]
    fn a_long_name_keeps_its_extension_and_size() {
        let mut a = app_on_main(0);
        a.local.set_entries(vec![PaneEntry::new(
            format!("{}.tar.gz", "long-name-".repeat(20)),
            false,
            123_456,
        )]);
        let shown = text(&draw(&a));
        let size = crate::transfer::format_bytes(123_456);
        let row = shown
            .lines()
            .find(|l| l.contains(".tar.gz"))
            .unwrap_or_else(|| panic!("the extension is cut:\n{shown}"));
        assert!(row.contains(&size), "{row}");
        assert!(row.contains('…'), "{row}");
    }

    /// The filter is measured in columns too, so a wide one doesn't push
    /// the path's tail out.
    #[test]
    fn a_wide_filter_leaves_room_for_the_path_tail() {
        let mut a = app_on_main(1);
        a.local.path = format!("/{}TAIL-END", "dir/".repeat(30));
        a.local.set_filter("検索検索検索検索".into());
        let shown = text(&draw(&a));
        assert!(shown.contains("TAIL-END"), "{shown}");
        // The whole filter fits beside the path's tail: measured in
        // characters, the path took the columns the filter's second half
        // needed, and that half was cut at the border.
        let row = shown.lines().find(|l| l.contains("TAIL-END")).unwrap();
        assert_eq!(row.matches('索').count(), 4, "{row}");
    }

    #[test]
    fn a_wide_transfer_name_leaves_the_speed_on_the_row() {
        let job = crate::transfer::TransferJob {
            id: 1,
            direction: crate::transfer::Direction::Download,
            remote_path: format!("/srv/{}", "日本語".repeat(20)),
            local_path: std::path::PathBuf::from("/tmp/x"),
            bytes_total: 1000,
            bytes_done: 500,
            bytes_per_sec: 100,
            state: crate::transfer::TransferState::Active,
            batch_id: None,
        };
        let a = app_on_main(0);
        let line = super::bottom_pane::transfer_line_for_test(&a, 60, &job);
        assert!(line.width() <= 60, "{} wide", line.width());
        let speed = crate::transfer::format_bytes_per_sec(100);
        let last = line
            .spans
            .iter()
            .rev()
            .find(|s| !s.content.trim().is_empty())
            .unwrap();
        assert!(last.content.contains(&speed), "{:?}", last.content);
    }

    // --- footer ---

    #[test]
    fn the_footer_counts_one_item_in_the_singular() {
        let a = app_on_main(1);
        let shown = text(&draw(&a));
        assert!(shown.contains(" 1 item"), "{shown}");
        assert!(!shown.contains("1 items"), "{shown}");
    }

    /// With a filter on, "3 items" read as the whole folder.
    #[test]
    fn a_filtered_footer_counts_against_the_folder() {
        let mut a = app_on_main(40);
        a.local.set_filter("file00".into());
        let shown = text(&draw(&a));
        assert!(shown.contains("10 of 40 items"), "{shown}");
    }

    // --- queued transfers ---

    fn with_queued(a: &mut App, n: usize) -> crate::transfer::TransferManager {
        let (m, _events) = crate::transfer::TransferManager::new(1);
        for i in 0..n {
            m.enqueue_download(format!("/r/f{i}"), format!("/l/f{i}").into())
                .unwrap();
        }
        a.transfer_manager = Some(m.clone());
        a.bottom_pane = BottomPane::Transfers;
        m
    }

    /// Only running jobs have rows, and nothing said a queue stood behind
    /// them: paused before anything started, the pane read "no active
    /// transfers" with work waiting.
    #[test]
    fn the_transfers_pane_counts_the_queue() {
        let mut a = app_on_main(0);
        let m = with_queued(&mut a, 3);
        let shown = text(&draw(&a));
        assert!(shown.contains("+3 queued"), "{shown}");
        assert!(shown.contains("none running · 3 queued"), "{shown}");
        assert!(!shown.contains("no active transfers"), "{shown}");

        m.pause();
        let shown = text(&draw(&a));
        assert!(shown.contains("3 queued, paused"), "{shown}");
    }

    #[test]
    fn an_empty_queue_says_so() {
        let mut a = app_on_main(0);
        a.bottom_pane = BottomPane::Transfers;
        let shown = text(&draw(&a));
        assert!(shown.contains("no active transfers"), "{shown}");
        assert!(!shown.contains("queued"), "{shown}");
    }

    // --- log wrapping ---

    #[test]
    fn a_log_message_breaks_after_spaces_within_its_room() {
        let width = LOG_PREFIX_W + 10;
        let rows = log_message_rows("one two three four", width);
        assert_eq!(rows, ["one two", "three four"]);
        for row in log_message_rows(&"word ".repeat(30), width) {
            assert!(cols(&row) <= 10, "{row:?}");
            assert!(!row.starts_with(' '), "{row:?}");
        }
    }

    #[test]
    fn a_word_longer_than_the_room_breaks_mid_word() {
        let rows = log_message_rows(&"x".repeat(25), LOG_PREFIX_W + 10);
        assert_eq!(rows, ["x".repeat(10), "x".repeat(10), "x".repeat(5)]);
    }

    #[test]
    fn wide_characters_wrap_by_columns() {
        for row in log_message_rows(&"日本語".repeat(10), LOG_PREFIX_W + 7) {
            assert!(cols(&row) <= 7, "{row:?}");
        }
    }

    #[test]
    fn an_empty_or_short_message_is_one_row() {
        assert_eq!(log_message_rows("", 80), [""]);
        assert_eq!(log_message_rows("short", 80), ["short"]);
        assert_eq!(log_message_rows("short", usize::MAX), ["short"]);
    }

    fn on_log(entries: &[String]) -> App {
        let mut a = app_on_main(0);
        a.log.clear();
        for e in entries {
            a.push_log(LogLevel::Info, e.clone());
        }
        a.bottom_pane = BottomPane::Log;
        a.active_pane = Pane::Log;
        a
    }

    /// A message longer than the pane ran off its edge, and the log is the
    /// only place some errors are given whole.
    #[test]
    fn a_long_log_line_wraps_with_its_end_on_screen() {
        let long = format!("{} THE-REAL-REASON", "upload failed because ".repeat(10));
        let a = on_log(&[long]);
        let shown = text(&draw(&a));
        assert!(shown.contains("THE-REAL-REASON"), "{shown}");
        let indent = format!("│{}", " ".repeat(LOG_PREFIX_W));
        let continuation = shown.lines().any(|l| {
            l.strip_prefix(indent.as_str())
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_alphabetic()))
        });
        assert!(
            continuation,
            "continuation rows sit under the message:\n{shown}"
        );
    }

    fn long_entries(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("entry {i:02} {}", "padding words ".repeat(10)))
            .collect()
    }

    /// Scrolling counts rows: a key moves the view one row even through an
    /// entry that wraps, and the top is reached with the first entry's
    /// first row showing.
    #[test]
    fn the_log_scrolls_by_wrapped_rows_to_its_first_line() {
        use crossterm::event::KeyCode;
        let mut a = on_log(&long_entries(20));
        draw(&a);
        for _ in 0..200 {
            press_key(&mut a, KeyCode::PageUp);
        }
        let shown = text(&draw(&a));
        assert!(shown.contains("entry 00"), "{shown}");
        press_key(&mut a, KeyCode::Down);
        let shown = text(&draw(&a));
        assert!(
            !shown.contains("entry 00"),
            "one row down moves at once:\n{shown}"
        );
        assert!(shown.contains("padding"), "{shown}");
    }

    /// Scrolled back, a new entry that wraps onto several rows leaves the
    /// view exactly where it was.
    #[test]
    fn a_wrapped_entry_arriving_holds_a_scrolled_view() {
        use crossterm::event::KeyCode;
        let mut a = on_log(&long_entries(20));
        draw(&a);
        for _ in 0..5 {
            press_key(&mut a, KeyCode::Up);
        }
        let before = text(&draw_sized(&a, 100, 30));
        assert!(before.contains("↓ 2 newer"), "{before}");
        a.push_log(
            LogLevel::Info,
            long_entries(1).remove(0).replace("00", "NEW"),
        );
        let after = text(&draw_sized(&a, 100, 30));
        // The same rows, one more entry below them.
        assert_eq!(before.replace("↓ 2 newer", "↓ 3 newer"), after);
    }

    /// The corner counts entries below the view, not rows.
    #[test]
    fn the_newer_count_counts_entries() {
        use crossterm::event::KeyCode;
        let mut a = on_log(&long_entries(20));
        draw(&a);
        let rows_per_entry = log_message_rows(&long_entries(1)[0], 98).len();
        assert!(rows_per_entry > 1, "the test needs entries that wrap");
        for _ in 0..rows_per_entry * 3 {
            press_key(&mut a, KeyCode::Up);
        }
        let shown = text(&draw(&a));
        assert!(shown.contains("↓ 3 newer"), "{shown}");
        assert!(!shown.contains("entry 19"), "{shown}");
        assert!(shown.contains("entry 16"), "{shown}");
    }

    // --- status bar ---

    #[test]
    fn the_status_bar_names_the_keys_of_the_focused_pane() {
        let mut a = app_on_main(1);
        let shown = text(&draw_sized(&a, 160, 30));
        assert!(shown.contains("[^u] upload"), "{shown}");
        assert!(shown.contains("[space] select"), "{shown}");

        a.active_pane = Pane::Transfers;
        let shown = text(&draw_sized(&a, 160, 30));
        assert!(shown.contains("[c] cancel"), "{shown}");
        assert!(shown.contains("[r/R] resume"), "{shown}");
        assert!(!shown.contains("[space] select"), "{shown}");

        a.active_pane = Pane::Log;
        let shown = text(&draw_sized(&a, 160, 30));
        assert!(shown.contains("[↑↓] scroll"), "{shown}");
        assert!(!shown.contains("[^d] download"), "{shown}");
    }
}
