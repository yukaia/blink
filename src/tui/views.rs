//! View rendering: one inline submodule per screen.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::tui::app::{App, Pane};
use crate::tui::widgets;

/// Centre an inner rect within `r`, sized as a percentage of the outer area.
pub(crate) fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup[1])[1]
}

// ---------------------------------------------------------------------------
// Session select
// ---------------------------------------------------------------------------

pub mod session_select {
    use super::*;
    use crate::session::Protocol;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        f.render_widget(
            Block::default().style(Style::default().bg(app.theme.bg).fg(app.theme.fg)),
            area,
        );

        // title (6) | sep (1) | list (flex) | details (5) | status (1)
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6),
                Constraint::Length(1),
                Constraint::Min(8),
                Constraint::Length(5),
                Constraint::Length(1),
            ])
            .split(area);

        // Title block
        let title_lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "blink",
                Style::default()
                    .fg(app.theme.border_active)
                    .add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Center),
            Line::from(Span::styled(
                "terminal sftp / scp / ftp / ftps client",
                Style::default().fg(app.theme.dim),
            ))
            .alignment(Alignment::Center),
        ];
        f.render_widget(Paragraph::new(title_lines), layout[0]);

        // Separator
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(area.width as usize),
                Style::default().fg(app.theme.border_inactive),
            ))),
            layout[1],
        );

        render_session_list(f, app, layout[2]);
        render_session_details(f, app, layout[3]);
        widgets::status_bar::render_session_select(f, app, layout[4]);
    }

    fn render_session_list(f: &mut Frame, app: &App, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "SAVED SESSIONS",
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));

        if app.sessions.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no saved sessions — press [n] to create one)",
                Style::default().fg(app.theme.dim),
            )));
        } else {
            for (i, s) in app.sessions.iter().enumerate() {
                let selected = i == app.session_cursor;

                let prefix = if selected {
                    Span::styled(
                        " ▸ ",
                        Style::default()
                            .fg(app.theme.border_active)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::raw("   ")
                };
                let name_style = if selected {
                    Style::default()
                        .fg(app.theme.border_active)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(app.theme.fg)
                };
                let host_style = if selected {
                    Style::default().fg(app.theme.fg)
                } else {
                    Style::default().fg(app.theme.dim)
                };
                let proto_style = match s.protocol {
                    Protocol::Sftp => Style::default().fg(app.theme.success),
                    Protocol::Scp => Style::default().fg(app.theme.directory),
                    Protocol::Ftp | Protocol::Ftps => Style::default().fg(app.theme.warning),
                };

                let line_spans = vec![
                    prefix,
                    Span::styled(format!("{:<14}", s.name), name_style),
                    Span::styled(
                        format!("  {}@{}:{}", s.username, s.host, s.port),
                        host_style,
                    ),
                    Span::raw("   "),
                    Span::styled(
                        format!("[{}]", s.protocol.as_str()),
                        proto_style.add_modifier(Modifier::BOLD),
                    ),
                ];
                let bg = if selected {
                    Style::default().bg(app.theme.cursor_bg)
                } else {
                    Style::default()
                };
                lines.push(Line::from(line_spans).style(bg));
            }
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    fn render_session_details(f: &mut Frame, app: &App, area: Rect) {
        let s = match app.sessions.get(app.session_cursor) {
            Some(s) => s,
            None => return,
        };

        let lines = vec![
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("DETAILS · {}", s.name),
                    Style::default()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("protocol  ", Style::default().fg(app.theme.dim)),
                Span::raw(s.protocol.as_str()),
                Span::raw("    "),
                Span::styled("auth      ", Style::default().fg(app.theme.dim)),
                Span::raw(s.auth.label()),
            ]),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("remote    ", Style::default().fg(app.theme.dim)),
                Span::raw(s.remote_dir.clone()),
                Span::raw("    "),
                Span::styled("local     ", Style::default().fg(app.theme.dim)),
                Span::raw(
                    s.local_dir
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(home)".to_string()),
                ),
            ]),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("parallel  ", Style::default().fg(app.theme.dim)),
                Span::raw(
                    s.parallel_downloads
                        .map(|n| format!("{n} streams (override)"))
                        .unwrap_or_else(|| {
                            format!("{} (global)", app.config.general.parallel_downloads)
                        }),
                ),
            ]),
        ];
        f.render_widget(Paragraph::new(lines), area);
    }
}

// ---------------------------------------------------------------------------
// Main 3-pane view
// ---------------------------------------------------------------------------

pub mod main {
    use super::*;
    use crate::tui::widgets::{bottom_pane, file_pane, status_bar};

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        f.render_widget(
            Block::default().style(Style::default().bg(app.theme.bg).fg(app.theme.fg)),
            area,
        );

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),  // header
                Constraint::Min(10),    // panes (flex)
                Constraint::Length(10), // bottom (Transfers + Log)
                Constraint::Length(1),  // status
            ])
            .split(area);

        render_header(f, app, layout[0]);

        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(layout[1]);
        file_pane::render(f, app, panes[0], Pane::Local);
        file_pane::render(f, app, panes[1], Pane::Remote);

        bottom_pane::render(f, app, layout[2]);
        status_bar::render_main(f, app, layout[3]);
    }

    fn render_header(f: &mut Frame, app: &App, area: Rect) {
        let connected = app.current_session.is_some();
        let status_dot = if connected { "●" } else { "○" };
        let status_color = if connected {
            app.theme.success
        } else {
            app.theme.warning
        };

        let session_label = app
            .current_session
            .as_ref()
            .map(|s| {
                format!(
                    " {} · {}@{}:{} ",
                    s.protocol.as_str(),
                    s.username,
                    s.host,
                    s.port
                )
            })
            .unwrap_or_else(|| " not connected ".to_string());

        let line = Line::from(vec![
            Span::styled(
                " blink ",
                Style::default()
                    .fg(app.theme.border_active)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {status_dot}"), Style::default().fg(status_color)),
            Span::raw(session_label),
            Span::raw(" "),
            Span::styled(
                format!("theme: {}", app.theme.name),
                Style::default().fg(app.theme.dim),
            ),
        ]);
        f.render_widget(
            Paragraph::new(line).style(Style::default().bg(app.theme.cursor_bg)),
            area,
        );
    }
}

// ---------------------------------------------------------------------------
// Help overlay
// ---------------------------------------------------------------------------

pub mod help {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        let modal = super::centered_rect(70, 78, area);

        f.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.border_active))
            .title(Span::styled(
                " HELP — keyboard shortcuts ",
                Style::default()
                    .fg(app.theme.border_active)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        let dim_s = Style::default().fg(app.theme.dim);
        let key_s = Style::default()
            .fg(app.theme.border_active)
            .add_modifier(Modifier::BOLD);
        let acc_s = Style::default()
            .fg(app.theme.accent)
            .add_modifier(Modifier::BOLD);

        let lines = vec![
            Line::from(""),
            Line::from(Span::styled("  NAVIGATION", acc_s)),
            kv("  ↵         ", "open file or enter directory", key_s),
            kv("  bksp      ", "go up to parent directory", key_s),
            kv("  tab       ", "switch active pane", key_s),
            kv("  ↑ ↓       ", "move cursor (pgup/pgdn for page)", key_s),
            Line::from(""),
            Line::from(Span::styled("  FILES", acc_s)),
            kv("  space     ", "select / deselect", key_s),
            kv("  ctrl+d    ", "download selected items", key_s),
            kv("  ctrl+u    ", "upload selected items (local → remote)", key_s),
            kv("  v         ", "view image (kitty/sixel/iterm2) or text", key_s),
            kv("  /         ", "filter current directory by substring", key_s),
            kv("  F5        ", "refresh active pane", key_s),
            kv("  F2        ", "rename file or folder (remote pane)", key_s),
            kv("  shift+del ", "delete file or folder (remote pane)", key_s),
            Line::from(""),
            Line::from(Span::styled("  SESSION", acc_s)),
            kv("  ctrl+s    ", "save current session", key_s),
            kv("  ctrl+x    ", "disconnect (return to session selector)", key_s),
            kv("  p         ", "pause / resume active downloads", key_s),
            kv("  t         ", "cycle to the next theme", key_s),
            kv("  c         ", "cancel selected transfer (Transfers pane)", key_s),
            kv("  C         ", "cancel whole batch (Transfers pane)", key_s),
            Line::from(""),
            Line::from(Span::styled("  APP", acc_s)),
            kv("  ?         ", "toggle this help", key_s),
            kv("  q · esc   ", "quit (with confirmation)", key_s),
            Line::from(""),
            Line::from(Span::styled("  esc / ?  to close", dim_s))
                .alignment(Alignment::Center),
        ];

        f.render_widget(Paragraph::new(lines), inner);
    }

    fn kv(key: &str, value: &str, key_style: Style) -> Line<'static> {
        Line::from(vec![
            Span::styled(key.to_string(), key_style),
            Span::raw(value.to_string()),
        ])
    }
}

// ---------------------------------------------------------------------------
// Quit confirmation
// ---------------------------------------------------------------------------

pub mod confirm_quit {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        let modal = super::centered_rect(40, 18, area);
        f.render_widget(Clear, modal);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.warning))
            .title(Span::styled(
                " quit blink? ",
                Style::default()
                    .fg(app.theme.warning)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        let lines = vec![
            Line::from(""),
            Line::from("  any in-flight transfers will be cancelled.")
                .alignment(Alignment::Center),
            Line::from(""),
            Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    "[y]",
                    Style::default()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" yes      "),
                Span::styled(
                    "[n/esc]",
                    Style::default()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" no  "),
            ])
            .alignment(Alignment::Center),
        ];
        f.render_widget(Paragraph::new(lines), inner);
    }
}

// ---------------------------------------------------------------------------
// Cancel-transfer confirmation (overlay over Main)
// ---------------------------------------------------------------------------

pub mod confirm_cancel {
    use super::*;
    use crate::tui::app::PendingCancel;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        // Batch modals carry more text; size for the worst case so neither
        // shape gets clipped on small terminals.
        let modal = super::centered_rect(60, 36, area);
        f.render_widget(Clear, modal);

        let is_batch = matches!(
            app.pending_cancel.as_ref(),
            Some(PendingCancel::Batch { .. })
        );
        let title = if is_batch {
            " cancel batch? "
        } else {
            " cancel transfer? "
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.warning))
            .title(Span::styled(
                title,
                Style::default()
                    .fg(app.theme.warning)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        let mut lines: Vec<Line> = vec![Line::from("")];
        match app.pending_cancel.as_ref() {
            Some(PendingCancel::Single { name, .. }) => {
                lines.push(
                    Line::from("  cancel this transfer:").alignment(Alignment::Center),
                );
                lines.push(
                    Line::from(Span::styled(
                        name.clone(),
                        Style::default()
                            .fg(app.theme.fg)
                            .add_modifier(Modifier::BOLD),
                    ))
                    .alignment(Alignment::Center),
                );
                lines.push(Line::from(""));
                lines.push(
                    Line::from(Span::styled(
                        "any partial file is left on disk.",
                        Style::default().fg(app.theme.dim),
                    ))
                    .alignment(Alignment::Center),
                );
            }
            Some(PendingCancel::Batch {
                active,
                pending,
                cursor_name,
                ..
            }) => {
                let total = active + pending;
                let plural = if total == 1 { "" } else { "s" };
                lines.push(
                    Line::from(format!(
                        "  cancel {total} transfer{plural} in this batch?"
                    ))
                    .alignment(Alignment::Center),
                );
                lines.push(Line::from(""));
                lines.push(
                    Line::from(Span::styled(
                        format!("(includes {cursor_name})"),
                        Style::default().fg(app.theme.dim),
                    ))
                    .alignment(Alignment::Center),
                );
                lines.push(Line::from(""));
                lines.push(
                    Line::from(format!("    {active} active   {pending} queued"))
                        .alignment(Alignment::Center),
                );
                lines.push(Line::from(""));
                lines.push(
                    Line::from(Span::styled(
                        "in-flight files leave partial data on disk.",
                        Style::default().fg(app.theme.dim),
                    ))
                    .alignment(Alignment::Center),
                );
                lines.push(
                    Line::from(Span::styled(
                        "queued files are dropped without ever starting.",
                        Style::default().fg(app.theme.dim),
                    ))
                    .alignment(Alignment::Center),
                );
            }
            None => {}
        }

        lines.push(Line::from(""));
        lines.push(
            Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    "[y]",
                    Style::default()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" yes      "),
                Span::styled(
                    "[n/esc]",
                    Style::default()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" no  "),
            ])
            .alignment(Alignment::Center),
        );
        f.render_widget(Paragraph::new(lines), inner);
    }
}

// ---------------------------------------------------------------------------
// Password prompt (overlay over SessionSelect)
// ---------------------------------------------------------------------------

pub mod password_prompt {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        let modal = super::centered_rect(50, 24, area);
        f.render_widget(Clear, modal);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.border_active))
            .title(Span::styled(
                " password ",
                Style::default()
                    .fg(app.theme.border_active)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        let target = app
            .pending_session
            .as_ref()
            .map(|s| {
                format!(
                    "{}@{}:{} ({})",
                    s.username,
                    s.host,
                    s.port,
                    s.protocol.as_str()
                )
            })
            .unwrap_or_default();
        let masked: String = "•".repeat(app.password_input.chars().count());

        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled(target, Style::default().fg(app.theme.dim)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::raw("  password: "),
                Span::styled(masked, Style::default().fg(app.theme.fg)),
                Span::styled(
                    "█",
                    Style::default().fg(app.theme.border_active),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  [↵] connect    [esc] cancel    [^u] clear",
                Style::default().fg(app.theme.dim),
            )),
        ];
        f.render_widget(Paragraph::new(lines), inner);
    }
}

// ---------------------------------------------------------------------------
// Connection in flight (overlay over Main)
// ---------------------------------------------------------------------------

pub mod connection {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        let modal = super::centered_rect(50, 18, area);
        f.render_widget(Clear, modal);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.warning))
            .title(Span::styled(
                " connecting ",
                Style::default()
                    .fg(app.theme.warning)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        let target = app
            .pending_session
            .as_ref()
            .or(app.current_session.as_ref())
            .map(|s| {
                format!(
                    "{}@{}:{} via {}",
                    s.username,
                    s.host,
                    s.port,
                    s.protocol.as_str()
                )
            })
            .unwrap_or_else(|| "…".into());

        let lines = vec![
            Line::from(""),
            Line::from(format!("  → {target}")).alignment(Alignment::Center),
            Line::from(""),
            Line::from(Span::styled(
                "press [esc] to cancel",
                Style::default().fg(app.theme.dim),
            ))
            .alignment(Alignment::Center),
        ];
        f.render_widget(Paragraph::new(lines), inner);
    }
}

// ---------------------------------------------------------------------------
// Viewer (text or image, overlay over Main)
// ---------------------------------------------------------------------------

pub mod viewer {
    use super::*;
    use crate::tui::app::ViewerKind;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        let modal = super::centered_rect(85, 85, area);

        let viewer = match app.viewer.as_ref() {
            Some(v) => v,
            None => return,
        };

        // Title shows the file name plus a kind hint.
        let kind_label = match &viewer.kind {
            ViewerKind::Loading => " · loading…",
            ViewerKind::Text { .. } => "",
            ViewerKind::Image { .. } => " · image",
            ViewerKind::Unsupported(_) => " · unavailable",
        };
        let title = format!(" {}{} ", viewer.name, kind_label);

        f.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.border_active))
            .title(Span::styled(
                title,
                Style::default()
                    .fg(app.theme.border_active)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        // Reserve the bottom row for the hint strip.
        let body = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(1),
        );
        let hint = Rect::new(
            inner.x,
            inner.y.saturating_add(inner.height.saturating_sub(1)),
            inner.width,
            inner.height.min(1),
        );

        match &viewer.kind {
            ViewerKind::Loading => {
                let p = Paragraph::new(
                    Line::from(Span::styled(
                        "loading…",
                        Style::default().fg(app.theme.dim),
                    ))
                    .alignment(Alignment::Center),
                );
                f.render_widget(p, body);
            }
            ViewerKind::Text { lines, scroll } => {
                render_text(f, body, app, lines, *scroll);
            }
            ViewerKind::Image { .. } => {
                // Body is intentionally left blank — graphics escape codes are
                // emitted on top of these cells from `App::after_draw`.
                let p = Paragraph::new(Line::from(""));
                f.render_widget(p, body);
            }
            ViewerKind::Unsupported(reason) => {
                let p = Paragraph::new(
                    Line::from(Span::styled(
                        format!("✗ {reason}"),
                        Style::default().fg(app.theme.error),
                    ))
                    .alignment(Alignment::Center),
                );
                f.render_widget(p, body);
            }
        }

        // Hint strip at the bottom.
        let hint_text = match &viewer.kind {
            ViewerKind::Text { lines, scroll } => format!(
                "  line {}/{}    [↑↓] scroll  [pgup/pgdn] page  [g/G] top/bottom  [q/esc] close",
                scroll.saturating_add(1).min(lines.len().max(1)),
                lines.len(),
            ),
            _ => "  [q/esc] close".to_string(),
        };
        let hint_style = Style::default().fg(app.theme.dim);
        f.render_widget(
            Paragraph::new(Span::styled(hint_text, hint_style)),
            hint,
        );
    }

    fn render_text(f: &mut Frame, area: Rect, app: &App, lines: &[String], scroll: usize) {
        let h = area.height as usize;
        if h == 0 || lines.is_empty() {
            return;
        }
        let start = scroll.min(lines.len().saturating_sub(1));
        let end = (start + h).min(lines.len());
        let visible = &lines[start..end];

        // Width of the line-number column.
        let max_lineno = end;
        let lineno_width = max_lineno.to_string().len();

        let rendered: Vec<Line> = visible
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let n = start + i + 1;
                Line::from(vec![
                    Span::styled(
                        format!("{:>width$} │ ", n, width = lineno_width),
                        Style::default().fg(app.theme.dim),
                    ),
                    Span::styled(line.clone(), Style::default().fg(app.theme.fg)),
                ])
            })
            .collect();

        f.render_widget(Paragraph::new(rendered), area);
    }
}

// ---------------------------------------------------------------------------
// New session (URL input, overlay over SessionSelect)
// ---------------------------------------------------------------------------

pub mod new_session {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        let modal = super::centered_rect(64, 40, area);
        f.render_widget(Clear, modal);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.border_active))
            .title(Span::styled(
                " new connection ",
                Style::default()
                    .fg(app.theme.border_active)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        let dim = Style::default().fg(app.theme.dim);
        let mut lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("enter a connection URL:", dim),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    app.new_session_input.as_str(),
                    Style::default().fg(app.theme.fg),
                ),
                Span::styled("█", Style::default().fg(app.theme.border_active)),
            ]),
            Line::from(""),
            Line::from(vec![Span::raw("  "), Span::styled("examples:", dim)]),
            Line::from(Span::styled(
                "    sftp://user@host.example.com:22/var/www",
                dim,
            )),
            Line::from(Span::styled("    sftp://me@10.0.0.5", dim)),
            Line::from(Span::styled("    ftp://anon@ftp.example.com", dim)),
        ];
        if let Some(err) = &app.new_session_error {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("✗ {err}"),
                    Style::default().fg(app.theme.error),
                ),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  [↵] connect    [esc] cancel    [^u] clear",
            dim,
        )));

        f.render_widget(Paragraph::new(lines), inner);
    }
}



// ---------------------------------------------------------------------------
// Save current session (overlay over Main)
// ---------------------------------------------------------------------------

pub mod save_session {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        let modal = super::centered_rect(60, 36, area);
        f.render_widget(Clear, modal);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.border_active))
            .title(Span::styled(
                " save session ",
                Style::default()
                    .fg(app.theme.border_active)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        let dim = Style::default().fg(app.theme.dim);
        let target = app
            .current_session
            .as_ref()
            .map(|s| {
                format!(
                    "{}@{}:{} ({})",
                    s.username,
                    s.host,
                    s.port,
                    s.protocol.as_str()
                )
            })
            .unwrap_or_default();

        let mut lines = vec![
            Line::from(""),
            Line::from(vec![Span::raw("  "), Span::styled(target, dim)]),
            Line::from(""),
            Line::from(vec![
                Span::raw("  name: "),
                Span::styled(
                    app.save_session_input.as_str(),
                    Style::default().fg(app.theme.fg),
                ),
                Span::styled("█", Style::default().fg(app.theme.border_active)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("local: ", dim),
                Span::raw(app.local.path.clone()),
            ]),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("remote: ", dim),
                Span::raw(if app.remote.path.is_empty() {
                    "/".into()
                } else {
                    app.remote.path.clone()
                }),
            ]),
        ];
        if let Some(err) = &app.save_session_error {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("✗ {err}"),
                    Style::default().fg(app.theme.error),
                ),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  [↵] save    [esc] cancel    [^u] clear",
            dim,
        )));

        f.render_widget(Paragraph::new(lines), inner);
    }
}

// ---------------------------------------------------------------------------
// Rename (overlay over Main)
// ---------------------------------------------------------------------------

pub mod rename {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        let modal = super::centered_rect(60, 28, area);
        f.render_widget(Clear, modal);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.border_active))
            .title(Span::styled(
                " rename ",
                Style::default()
                    .fg(app.theme.border_active)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        let dim = Style::default().fg(app.theme.dim);
        let mut lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("from: ", dim),
                Span::raw(app.rename_original.clone()),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("  to: ", dim),
                Span::styled(
                    app.rename_input.as_str(),
                    Style::default().fg(app.theme.fg),
                ),
                Span::styled("█", Style::default().fg(app.theme.border_active)),
            ]),
        ];
        if let Some(err) = &app.rename_error {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("✗ {err}"),
                    Style::default().fg(app.theme.error),
                ),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  [↵] rename    [esc] cancel    [^u] clear",
            dim,
        )));

        f.render_widget(Paragraph::new(lines), inner);
    }
}

// ---------------------------------------------------------------------------
// Confirm delete (overlay over Main)
// ---------------------------------------------------------------------------

pub mod confirm_delete {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        let modal = super::centered_rect(50, 28, area);
        f.render_widget(Clear, modal);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.error))
            .title(Span::styled(
                " delete? ",
                Style::default()
                    .fg(app.theme.error)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        let (name, is_dir) = match app.pending_delete.as_ref() {
            Some(pd) => (pd.name.clone(), pd.is_dir),
            None => (String::new(), false),
        };

        let lines = vec![
            Line::from(""),
            Line::from(if is_dir {
                "  delete this folder and everything inside it:"
            } else {
                "  delete this file:"
            })
            .alignment(Alignment::Center),
            Line::from(Span::styled(
                name,
                Style::default()
                    .fg(app.theme.fg)
                    .add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Center),
            Line::from(""),
            Line::from(Span::styled(
                "this cannot be undone.",
                Style::default().fg(app.theme.dim),
            ))
            .alignment(Alignment::Center),
            Line::from(""),
            Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    "[y]",
                    Style::default()
                        .fg(app.theme.error)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" yes      "),
                Span::styled(
                    "[n/esc]",
                    Style::default()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" no  "),
            ])
            .alignment(Alignment::Center),
        ];
        f.render_widget(Paragraph::new(lines), inner);
    }
}

// ---------------------------------------------------------------------------
// Confirm overwrite (overlay over Main)
// ---------------------------------------------------------------------------

pub mod confirm_overwrite {
    use super::*;
    use crate::tui::app::{OverwritePending, PlannedJob};

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        let modal = super::centered_rect(60, 56, area);
        f.render_widget(Clear, modal);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.warning))
            .title(Span::styled(
                " overwrite? ",
                Style::default()
                    .fg(app.theme.warning)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        let mut lines = vec![Line::from("")];
        let is_plan = matches!(
            app.pending_overwrite.as_ref(),
            Some(OverwritePending::DownloadPlan { .. })
                | Some(OverwritePending::UploadPlan { .. })
        );
        match app.pending_overwrite.as_ref() {
            Some(OverwritePending::Rename { target_name, .. }) => {
                lines.push(
                    Line::from("  a file or folder with this name already exists:")
                        .alignment(Alignment::Center),
                );
                lines.push(
                    Line::from(Span::styled(
                        target_name.clone(),
                        Style::default()
                            .fg(app.theme.fg)
                            .add_modifier(Modifier::BOLD),
                    ))
                    .alignment(Alignment::Center),
                );
                lines.push(Line::from(""));
                lines.push(
                    Line::from(Span::styled(
                        "renaming will replace it.",
                        Style::default().fg(app.theme.dim),
                    ))
                    .alignment(Alignment::Center),
                );
            }
            Some(OverwritePending::DownloadPlan {
                plan,
                conflict_indices,
            }) => {
                render_plan_body(
                    &mut lines,
                    app,
                    plan,
                    conflict_indices,
                    PlanKind::Download,
                );
            }
            Some(OverwritePending::UploadPlan {
                plan,
                conflict_indices,
            }) => {
                render_plan_body(
                    &mut lines,
                    app,
                    plan,
                    conflict_indices,
                    PlanKind::Upload,
                );
            }
            None => {}
        }

        lines.push(Line::from(""));
        // Hint strip: rename modal has no skip option, plan modals do.
        let key_style = |c| {
            Style::default()
                .fg(c)
                .add_modifier(Modifier::BOLD)
        };
        let mut hint_spans = vec![
            Span::raw("   "),
            Span::styled("[y]", key_style(app.theme.warning)),
            Span::raw(" overwrite  "),
        ];
        if is_plan {
            hint_spans.push(Span::styled("[s]", key_style(app.theme.accent)));
            hint_spans.push(Span::raw(" skip conflicts  "));
        }
        hint_spans.push(Span::styled("[n/esc]", key_style(app.theme.accent)));
        hint_spans.push(Span::raw(" cancel "));
        lines.push(Line::from(hint_spans).alignment(Alignment::Center));
        f.render_widget(Paragraph::new(lines), inner);
    }

    enum PlanKind {
        Download,
        Upload,
    }

    fn render_plan_body(
        lines: &mut Vec<Line<'static>>,
        app: &App,
        plan: &[PlannedJob],
        conflict_indices: &[usize],
        kind: PlanKind,
    ) {
        let total_files: usize = plan
            .iter()
            .filter(|j| !matches!(j, PlannedJob::Mkdir { .. }))
            .count();
        let n = conflict_indices.len();
        let plural = if n == 1 { "" } else { "s" };
        let exists_verb = if n == 1 { "exists" } else { "exist" };
        let target_label = match kind {
            PlanKind::Download => "local",
            PlanKind::Upload => "remote",
        };
        lines.push(
            Line::from(format!(
                "  {n} of {total_files} target file{plural} already \
                 {exists_verb} on the {target_label} side:"
            ))
            .alignment(Alignment::Center),
        );
        lines.push(Line::from(""));

        // Show up to ~6 conflict names. Beyond that, summarise.
        let max_show = 6;
        let shown: usize = n.min(max_show);
        for &idx in conflict_indices.iter().take(max_show) {
            let name = match &plan[idx] {
                PlannedJob::Download { remote_path, .. } => basename(remote_path),
                PlannedJob::Upload { remote_path, .. } => basename(remote_path),
                PlannedJob::Mkdir { .. } => continue, // shouldn't appear
            };
            lines.push(Line::from(vec![
                Span::raw("  • "),
                Span::styled(name, Style::default().fg(app.theme.fg)),
            ]));
        }
        if n > shown {
            let extra = n - shown;
            lines.push(Line::from(Span::styled(
                format!("  + {extra} more"),
                Style::default().fg(app.theme.dim),
            )));
        }
        lines.push(Line::from(""));
        lines.push(
            Line::from(Span::styled(
                match kind {
                    PlanKind::Download => "[y] replaces all conflicting files locally.",
                    PlanKind::Upload => "[y] replaces all conflicting files on the server.",
                },
                Style::default().fg(app.theme.dim),
            ))
            .alignment(Alignment::Center),
        );
        lines.push(
            Line::from(Span::styled(
                "[s] proceeds with non-conflicting files only.",
                Style::default().fg(app.theme.dim),
            ))
            .alignment(Alignment::Center),
        );
    }

    fn basename(path: &str) -> String {
        path.rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or(path)
            .to_string()
    }
}

// ---------------------------------------------------------------------------
// Edit session (overlay over SessionSelect)
// ---------------------------------------------------------------------------

pub mod edit_session {
    use super::*;
    use crate::tui::app::EditField;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        let modal = super::centered_rect(64, 70, area);
        f.render_widget(Clear, modal);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.border_active))
            .title(Span::styled(
                " edit session ",
                Style::default()
                    .fg(app.theme.border_active)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        let form = match app.edit_session_form.as_ref() {
            Some(f) => f,
            None => return,
        };

        let dim = Style::default().fg(app.theme.dim);
        let fg = Style::default().fg(app.theme.fg);
        let cursor = Style::default().fg(app.theme.border_active);

        // Render one row per editable field. The focused field gets a block
        // cursor at the end of its value.
        let row = |label: &str, value: &str, focused: bool| -> Line<'static> {
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(format!("{label:<12}"), dim),
                Span::styled(value.to_string(), fg),
            ];
            if focused {
                spans.push(Span::styled("█", cursor));
            }
            Line::from(spans)
        };

        let mut lines = vec![Line::from("")];
        lines.push(row("name:", &form.name, form.focused == EditField::Name));
        lines.push(row("host:", &form.host, form.focused == EditField::Host));
        lines.push(row("port:", &form.port, form.focused == EditField::Port));
        lines.push(row(
            "username:",
            &form.username,
            form.focused == EditField::Username,
        ));
        lines.push(row(
            "remote dir:",
            &form.remote_dir,
            form.focused == EditField::RemoteDir,
        ));
        lines.push(row(
            "local dir:",
            &form.local_dir,
            form.focused == EditField::LocalDir,
        ));

        // Parallel transfers row, with an inline hint showing the global
        // default when the user leaves it blank ("blank = use 4 (global)").
        let parallel_focused = form.focused == EditField::Parallel;
        let mut parallel_spans = vec![
            Span::raw("  "),
            Span::styled(format!("{:<12}", "parallel:"), dim),
            Span::styled(form.parallel.clone(), fg),
        ];
        if parallel_focused {
            parallel_spans.push(Span::styled("█", cursor));
        }
        if form.parallel.trim().is_empty() {
            parallel_spans.push(Span::styled(
                format!(
                    "  blank = use {} (global)",
                    app.config.general.parallel_downloads
                ),
                dim,
            ));
        }
        lines.push(Line::from(parallel_spans));

        // Accept-invalid-certs toggle. Rendered as a checkbox row; the
        // dangerous state ([x] true) gets a red warning string trailing.
        let aic_focused = form.focused == EditField::AcceptInvalidCerts;
        let aic_box = if form.accept_invalid_certs {
            "[x]"
        } else {
            "[ ]"
        };
        let aic_box_style = if form.accept_invalid_certs {
            Style::default()
                .fg(app.theme.error)
                .add_modifier(Modifier::BOLD)
        } else {
            fg
        };
        let mut aic_spans = vec![
            Span::raw("  "),
            Span::styled(format!("{:<12}", "tls verify:"), dim),
            Span::styled(aic_box.to_string(), aic_box_style),
        ];
        if aic_focused {
            // Cursor block lands after the checkbox; feels right for a
            // non-text field that you toggle with space.
            aic_spans.push(Span::raw(" "));
            aic_spans.push(Span::styled("◀", cursor));
        }
        aic_spans.push(Span::styled(
            "  accept invalid certs (FTPS only)",
            dim,
        ));
        lines.push(Line::from(aic_spans));
        if form.accept_invalid_certs {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::raw(" ".repeat(12)),
                Span::styled(
                    "    ⚠  this disables TLS protections. use only for self-signed dev servers.",
                    Style::default()
                        .fg(app.theme.error)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        }

        // Show the immutable fields so the user knows what they are without
        // having to leave the modal.
        lines.push(Line::from(""));
        if let Some(s) = app.sessions.iter().find(|s| s.name == form.original_name) {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{:<12}", "protocol:"), dim),
                Span::styled(s.protocol.as_str().to_string(), fg),
                Span::styled("  (delete + recreate to change)", dim),
            ]));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{:<12}", "auth:"), dim),
                Span::styled(s.auth.label(), fg),
            ]));
        }

        if let Some(err) = &form.error {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("✗ {err}"),
                    Style::default().fg(app.theme.error),
                ),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  [↑↓/tab] field    [space] toggle    [↵] save    [esc] cancel",
            dim,
        )));

        f.render_widget(Paragraph::new(lines), inner);
    }
}

// ---------------------------------------------------------------------------
// Confirm delete session (overlay over SessionSelect)
// ---------------------------------------------------------------------------

pub mod confirm_delete_session {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        let modal = super::centered_rect(50, 32, area);
        f.render_widget(Clear, modal);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.error))
            .title(Span::styled(
                " delete session? ",
                Style::default()
                    .fg(app.theme.error)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        let dim = Style::default().fg(app.theme.dim);
        let fg = Style::default().fg(app.theme.fg);
        let mut lines = vec![Line::from("")];
        if let Some(s) = app.pending_session_delete.as_ref() {
            lines.push(
                Line::from("  permanently delete this session:")
                    .alignment(Alignment::Center),
            );
            lines.push(Line::from(""));
            lines.push(
                Line::from(Span::styled(
                    s.name.clone(),
                    fg.add_modifier(Modifier::BOLD),
                ))
                .alignment(Alignment::Center),
            );
            lines.push(
                Line::from(Span::styled(
                    format!(
                        "{}@{}:{}  [{}]",
                        s.username,
                        s.host,
                        s.port,
                        s.protocol.as_str()
                    ),
                    dim,
                ))
                .alignment(Alignment::Center),
            );
            lines.push(Line::from(""));
            lines.push(
                Line::from(Span::styled(
                    "this only removes the saved session.",
                    dim,
                ))
                .alignment(Alignment::Center),
            );
            lines.push(
                Line::from(Span::styled(
                    "no remote files are touched.",
                    dim,
                ))
                .alignment(Alignment::Center),
            );
        }
        lines.push(Line::from(""));
        lines.push(
            Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    "[y]",
                    Style::default()
                        .fg(app.theme.error)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" yes      "),
                Span::styled(
                    "[n/esc]",
                    Style::default()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" no  "),
            ])
            .alignment(Alignment::Center),
        );

        f.render_widget(Paragraph::new(lines), inner);
    }
}

// ---------------------------------------------------------------------------
// SSH key passphrase prompt (overlay over SessionSelect)
// ---------------------------------------------------------------------------

pub mod key_passphrase_prompt {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        let modal = super::centered_rect(56, 32, area);
        f.render_widget(Clear, modal);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.border_active))
            .title(Span::styled(
                " ssh key passphrase ",
                Style::default()
                    .fg(app.theme.border_active)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        // Header line: which key file we're unlocking, where appropriate.
        let key_label = app
            .pending_session
            .as_ref()
            .and_then(|s| match &s.auth {
                crate::session::AuthMethod::Key { path } => {
                    Some(format!("key: {}", path.display()))
                }
                _ => None,
            })
            .unwrap_or_else(|| "key".to_string());

        let dim = Style::default().fg(app.theme.dim);
        let masked: String = "•".repeat(app.passphrase_input.chars().count());

        let mut lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled(key_label, dim),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::raw("  passphrase: "),
                Span::styled(masked, Style::default().fg(app.theme.fg)),
                Span::styled(
                    "█",
                    Style::default().fg(app.theme.border_active),
                ),
            ]),
        ];
        if let Some(err) = &app.passphrase_error {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("✗ {err}"),
                    Style::default().fg(app.theme.error),
                ),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  [↵] unlock    [esc] cancel    [^u] clear",
            dim,
        )));

        f.render_widget(Paragraph::new(lines), inner);
    }
}

// ---------------------------------------------------------------------------
// Search bar (single-row overlay over the status bar on Main)
// ---------------------------------------------------------------------------

pub mod search {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        // Cover the bottom row (which the main view uses for the status bar).
        let bar = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
        f.render_widget(Clear, bar);

        let target_label = match app.search_target {
            Pane::Local => "local",
            Pane::Remote => "remote",
            _ => "?",
        };
        let visible_count = match app.search_target {
            Pane::Local => app.local.entries.len(),
            Pane::Remote => app.remote.entries.len(),
            _ => 0,
        };
        // Subtract one for the always-retained ".." entry where applicable.
        let match_count = match app.search_target {
            Pane::Local => app
                .local
                .entries
                .iter()
                .filter(|e| e.name != "..")
                .count(),
            Pane::Remote => app
                .remote
                .entries
                .iter()
                .filter(|e| e.name != "..")
                .count(),
            _ => visible_count,
        };

        let bar_style = Style::default().bg(app.theme.cursor_bg).fg(app.theme.fg);
        let dim = Style::default().fg(app.theme.dim);
        let key_style = Style::default()
            .fg(app.theme.accent)
            .add_modifier(Modifier::BOLD);
        let label_style = Style::default()
            .fg(app.theme.border_active)
            .add_modifier(Modifier::BOLD);

        let count_label = if match_count == 1 {
            "1 match".to_string()
        } else {
            format!("{match_count} matches")
        };

        let line = Line::from(vec![
            Span::styled(format!(" /{target_label}: "), label_style),
            Span::styled(
                app.search_input.clone(),
                Style::default().fg(app.theme.fg),
            ),
            Span::styled("█", Style::default().fg(app.theme.border_active)),
            Span::raw("  "),
            Span::styled(count_label, dim),
            Span::raw("    "),
            Span::styled("[↵]", key_style),
            Span::raw(" keep  "),
            Span::styled("[esc]", key_style),
            Span::raw(" clear  "),
            Span::styled("[↑↓]", key_style),
            Span::raw(" move "),
        ]);
        f.render_widget(Paragraph::new(line).style(bar_style), bar);
    }
}

// ---------------------------------------------------------------------------
// Confirm disconnect (overlay over Main)
// ---------------------------------------------------------------------------

pub mod confirm_disconnect {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        let modal = super::centered_rect(50, 30, area);
        f.render_widget(Clear, modal);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.warning))
            .title(Span::styled(
                " disconnect? ",
                Style::default()
                    .fg(app.theme.warning)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        let dim = Style::default().fg(app.theme.dim);
        let fg = Style::default().fg(app.theme.fg);
        let active = app
            .active_jobs()
            .len();
        let target = app
            .current_session
            .as_ref()
            .map(|s| {
                format!(
                    "{}@{}:{} ({})",
                    s.username,
                    s.host,
                    s.port,
                    s.protocol.as_str()
                )
            })
            .unwrap_or_else(|| "this session".into());

        let mut lines = vec![
            Line::from(""),
            Line::from("  return to the session selector?")
                .alignment(Alignment::Center),
            Line::from(""),
            Line::from(Span::styled(target, fg.add_modifier(Modifier::BOLD)))
                .alignment(Alignment::Center),
            Line::from(""),
        ];
        if active > 0 {
            let plural = if active == 1 { "" } else { "s" };
            lines.push(
                Line::from(Span::styled(
                    format!(
                        "{active} in-flight transfer{plural} will be cancelled."
                    ),
                    Style::default().fg(app.theme.error),
                ))
                .alignment(Alignment::Center),
            );
        } else {
            lines.push(
                Line::from(Span::styled("the connection will be closed.", dim))
                    .alignment(Alignment::Center),
            );
        }
        lines.push(Line::from(""));
        lines.push(
            Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    "[y]",
                    Style::default()
                        .fg(app.theme.warning)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" yes      "),
                Span::styled(
                    "[n/esc]",
                    Style::default()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" no  "),
            ])
            .alignment(Alignment::Center),
        );
        f.render_widget(Paragraph::new(lines), inner);
    }
}

// ---------------------------------------------------------------------------
// Host-key confirmation prompt
// ---------------------------------------------------------------------------

pub mod confirm_host_key {
    use super::*;
    use crate::known_hosts::display_key;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        let modal = super::centered_rect(66, 52, area);
        f.render_widget(Clear, modal);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.warning))
            .title(Span::styled(
                " unknown host key ",
                Style::default()
                    .fg(app.theme.warning)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        let (host, key_type, key_b64, fingerprint) = match app.pending_host_key.as_ref() {
            Some(phk) => (
                phk.host.as_str(),
                phk.key_type.as_str(),
                phk.key_b64.as_str(),
                phk.fingerprint.as_str(),
            ),
            None => return,
        };

        let lines = vec![
            Line::from(""),
            Line::from(
                Span::styled(
                    "The server's host key is not in your known-hosts file.",
                    Style::default().fg(app.theme.fg),
                )
            ).alignment(Alignment::Center),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Host:        ", Style::default().fg(app.theme.dim)),
                Span::styled(host, Style::default().fg(app.theme.fg).add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled("  Key type:    ", Style::default().fg(app.theme.dim)),
                Span::styled(key_type, Style::default().fg(app.theme.fg)),
            ]),
            Line::from(vec![
                Span::styled("  Fingerprint: ", Style::default().fg(app.theme.dim)),
                Span::styled(fingerprint, Style::default().fg(app.theme.accent)),
            ]),
            Line::from(vec![
                Span::styled("  Key (trunc): ", Style::default().fg(app.theme.dim)),
                Span::styled(
                    display_key(key_b64),
                    Style::default().fg(app.theme.dim),
                ),
            ]),
            Line::from(""),
            Line::from(
                Span::styled(
                    "Verify this fingerprint out-of-band before accepting.",
                    Style::default().fg(app.theme.warning),
                )
            ).alignment(Alignment::Center),
            Line::from(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("[y]", Style::default().fg(app.theme.success).add_modifier(Modifier::BOLD)),
                Span::raw(" accept & save    "),
                Span::styled("[t]", Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD)),
                Span::raw(" trust once    "),
                Span::styled("[n/esc]", Style::default().fg(app.theme.error).add_modifier(Modifier::BOLD)),
                Span::raw(" reject"),
            ]).alignment(Alignment::Center),
        ];

        f.render_widget(Paragraph::new(lines), inner);
    }
}

// ---------------------------------------------------------------------------
// Host-key mismatch error screen
// ---------------------------------------------------------------------------

pub mod host_key_changed {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        let area = f.area();
        let modal = super::centered_rect(66, 52, area);
        f.render_widget(Clear, modal);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.error))
            .title(Span::styled(
                " host key mismatch ",
                Style::default()
                    .fg(app.theme.error)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        let info = match app.host_key_changed_info.as_ref() {
            Some(i) => i,
            None => return,
        };

        let known_hosts_path = crate::known_hosts::known_hosts_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "~/.config/blink/known_hosts".to_string());

        let lines = vec![
            Line::from(""),
            Line::from(
                Span::styled(
                    "⚠  WARNING: HOST KEY MISMATCH",
                    Style::default()
                        .fg(app.theme.error)
                        .add_modifier(Modifier::BOLD),
                )
            ).alignment(Alignment::Center),
            Line::from(""),
            Line::from(
                Span::styled(
                    "The server presented a different key than expected.",
                    Style::default().fg(app.theme.fg),
                )
            ).alignment(Alignment::Center),
            Line::from(
                Span::styled(
                    "This may indicate a man-in-the-middle attack.",
                    Style::default().fg(app.theme.warning),
                )
            ).alignment(Alignment::Center),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Host:          ", Style::default().fg(app.theme.dim)),
                Span::styled(
                    info.host.clone(),
                    Style::default().fg(app.theme.fg).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Expected type: ", Style::default().fg(app.theme.dim)),
                Span::styled(info.stored_key_type.clone(), Style::default().fg(app.theme.fg)),
            ]),
            Line::from(vec![
                Span::styled("  Got type:      ", Style::default().fg(app.theme.dim)),
                Span::styled(
                    info.presented_key_type.clone(),
                    Style::default().fg(app.theme.error),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Fingerprint:   ", Style::default().fg(app.theme.dim)),
                Span::styled(info.fingerprint.clone(), Style::default().fg(app.theme.accent)),
            ]),
            Line::from(""),
            Line::from(
                Span::styled(
                    "If the server key was legitimately replaced, remove the old",
                    Style::default().fg(app.theme.dim),
                )
            ).alignment(Alignment::Center),
            Line::from(
                Span::styled(
                    "entry from your known-hosts file and reconnect.",
                    Style::default().fg(app.theme.dim),
                )
            ).alignment(Alignment::Center),
            Line::from(""),
            Line::from(
                Span::styled(
                    known_hosts_path,
                    Style::default().fg(app.theme.dim).add_modifier(Modifier::ITALIC),
                )
            ).alignment(Alignment::Center),
            Line::from(""),
            Line::from(
                Span::styled(
                    "[any key] dismiss",
                    Style::default().fg(app.theme.dim),
                )
            ).alignment(Alignment::Center),
        ];

        f.render_widget(Paragraph::new(lines), inner);
    }
}
