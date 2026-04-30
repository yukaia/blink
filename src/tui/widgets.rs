//! Custom render widgets used by the views.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::tui::app::{App, Pane};

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
                Style::default().fg(border_color).add_modifier(Modifier::BOLD),
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

        // Path line, with the active filter (if any) shown inline.
        let mut path_spans = vec![Span::styled(
            format!(" {}", state.path),
            Style::default().fg(app.theme.dim),
        )];
        if let Some(filter) = &state.filter {
            path_spans.push(Span::raw("  "));
            path_spans.push(Span::styled(
                format!("/{filter}"),
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(path_spans)), inner_layout[0]);

        // Entry list, windowed around cursor.
        let list_area = inner_layout[1];
        let h = list_area.height as usize;
        let len = state.entries.len();
        let cursor = state.cursor.min(len.saturating_sub(1));
        let window_start = if cursor + 1 > h { cursor + 1 - h } else { 0 };
        let window_end = (window_start + h).min(len);

        let mut lines = Vec::with_capacity(window_end.saturating_sub(window_start));
        for i in window_start..window_end {
            let e = &state.entries[i];
            let is_cursor = i == cursor;

            let (icon, base_style) = if e.is_dir {
                ("▸ ", Style::default().fg(app.theme.directory))
            } else if e.previewable_image {
                ("  ", Style::default().fg(app.theme.image))
            } else {
                ("  ", Style::default().fg(app.theme.fg))
            };
            let row_style = if e.selected {
                Style::default().fg(app.theme.selected)
            } else {
                base_style
            };
            let sel_marker = if e.selected { "☑ " } else { "" };

            let mut row = vec![
                Span::raw(" "),
                Span::raw(icon),
                Span::styled(sel_marker.to_string(), row_style),
                Span::styled(e.name.clone(), row_style),
            ];

            // Right-align the size column.
            let size_str = if e.is_dir {
                "—".to_string()
            } else {
                crate::transfer::format_bytes(e.size)
            };
            let used: usize = row.iter().map(|s| s.content.chars().count()).sum();
            let avail = list_area.width as usize;
            let pad = avail.saturating_sub(used + size_str.chars().count() + 2);
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
        let selected_count = state.entries.iter().filter(|e| e.selected).count();
        let selected_size: u64 = state
            .entries
            .iter()
            .filter(|e| e.selected)
            .map(|e| e.size)
            .sum();
        let footer_text = if selected_count == 0 {
            format!(" {} items", state.entries.len())
        } else {
            format!(
                " {} items · {} selected ({})",
                state.entries.len(),
                selected_count,
                crate::transfer::format_bytes(selected_size),
            )
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                footer_text,
                Style::default().fg(app.theme.dim),
            )),
            inner_layout[2],
        );
    }
}

// ---------------------------------------------------------------------------
// Bottom pane (Transfers + Log, switchable via Tab)
// ---------------------------------------------------------------------------

pub mod bottom_pane {
    use super::*;
    use crate::tui::app::{BottomPane, LogLevel};

    pub fn render(f: &mut Frame, app: &App, area: Rect) {
        let focused = matches!(app.active_pane, Pane::Transfers | Pane::Log);
        let border_color = if focused {
            app.theme.border_active
        } else {
            app.theme.border_inactive
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(tab_title(app));
        let inner = block.inner(area);
        f.render_widget(block, area);

        match app.bottom_pane {
            BottomPane::Transfers => render_transfers(f, app, inner),
            BottomPane::Log => render_log(f, app, inner),
        }
    }

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
                " TRANSFERS ",
                if on_transfers { active_style } else { inactive_style },
            ),
            Span::styled("·", inactive_style),
            Span::styled(
                " LOG ",
                if !on_transfers { active_style } else { inactive_style },
            ),
            Span::raw(" "),
        ])
    }

    // ----- Transfers page --------------------------------------------------

    fn render_transfers(f: &mut Frame, app: &App, area: Rect) {
        let jobs = app.active_jobs();
        if jobs.is_empty() {
            let p = Paragraph::new(
                Line::from(Span::styled(
                    " no active transfers",
                    Style::default().fg(app.theme.dim),
                )),
            );
            f.render_widget(p, area);
            return;
        }

        let focused = app.active_pane == Pane::Transfers;
        let cursor = app.transfer_cursor.min(jobs.len().saturating_sub(1));
        let paused = app
            .transfer_manager
            .as_ref()
            .map(|m| m.is_paused())
            .unwrap_or(false);

        let mut lines = Vec::with_capacity(area.height as usize);
        for (i, job) in jobs.iter().enumerate().take(area.height as usize) {
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
            crate::transfer::format_bytes_per_sec(job.bytes_per_sec)
        };

        let percent_str = format!("{percent:3}%");
        let prefix_w = 4; // " ↓ " plus a leading space
        let right_w =
            percent_str.chars().count() + 1 + BAR_WIDTH + 1 + speed_str.chars().count() + 1;
        let avail_for_name = width.saturating_sub(prefix_w + right_w);

        let display_name = display_name_for(job);
        let name_str = truncate_middle(&display_name, avail_for_name);
        let name_pad = avail_for_name.saturating_sub(name_str.chars().count());

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

        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "↓ ",
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

    fn display_name_for(job: &crate::transfer::TransferJob) -> String {
        let from_remote = job
            .remote_path
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or(&job.remote_path);
        if !from_remote.is_empty() {
            return from_remote.to_string();
        }
        job.local_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| job.remote_path.clone())
    }

    /// Truncate `s` to `max` chars by replacing the middle with `…`.
    fn truncate_middle(s: &str, max: usize) -> String {
        if max == 0 {
            return String::new();
        }
        let n = s.chars().count();
        if n <= max {
            return s.to_string();
        }
        if max == 1 {
            return "…".into();
        }
        let keep = max - 1;
        let head = keep / 2;
        let tail = keep - head;
        let chars: Vec<char> = s.chars().collect();
        let head_part: String = chars[..head].iter().collect();
        let tail_part: String = chars[n - tail..].iter().collect();
        format!("{head_part}…{tail_part}")
    }

    // ----- Log page -------------------------------------------------------

    fn render_log(f: &mut Frame, app: &App, area: Rect) {
        let h = area.height as usize;
        let total = app.log.len();
        let start = total.saturating_sub(h);

        let mut lines = Vec::with_capacity(area.height as usize);
        for entry in &app.log[start..] {
            let ts = entry.time.format("%H:%M:%S").to_string();
            let level_color = match entry.level {
                LogLevel::Info => app.theme.dim,
                LogLevel::Success => app.theme.success,
                LogLevel::Warn => app.theme.warning,
                LogLevel::Error => app.theme.error,
            };
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(format!("[{ts}]"), Style::default().fg(app.theme.dim)),
                Span::raw(" "),
                Span::styled(entry.message.clone(), Style::default().fg(level_color)),
            ]));
        }
        f.render_widget(Paragraph::new(lines), area);
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

        let mut spans: Vec<Span<'static>> = vec![
            Span::raw(" "),
            Span::styled("[?]", key_style),
            Span::raw(" help  "),
            Span::styled("[↵]", key_style),
            Span::raw(" open  "),
            Span::styled("[⌫]", key_style),
            Span::raw(" up  "),
            Span::styled("[space]", key_style),
            Span::raw(" select  "),
            Span::styled("[^d]", key_style),
            Span::raw(" download  "),
            Span::styled("[v]", key_style),
            Span::raw(" view  "),
            Span::styled("[^s]", key_style),
            Span::raw(" save  "),
            Span::styled("[tab]", key_style),
            Span::raw(" switch  "),
            Span::styled("[q]", key_style),
            Span::raw(" quit"),
        ];

        // Right-align the version. Drop it silently if the terminal is too
        // narrow for both halves to fit (matches the session-select bar).
        let version_label = format!("v{} ", env!("CARGO_PKG_VERSION"));
        let left_w: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let ver_w = version_label.chars().count();
        let total_w = area.width as usize;
        if left_w + ver_w + 2 <= total_w {
            let pad = total_w - left_w - ver_w;
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(version_label, dim_style));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(bg), area);
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

        let left_w: usize = left_spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum();
        let theme_w = theme_label.chars().count();
        let ver_w = version_label.chars().count();
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
