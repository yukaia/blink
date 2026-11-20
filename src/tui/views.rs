//! View rendering: one inline submodule per screen.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::session::Protocol;
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

/// Where a modal goes in `area`: `percent_x` of its width, widened to fit
/// `content_width` columns, and exactly tall enough for `content_height`
/// rows, each plus a border and both clamped to `area`; centred.
///
/// The height used to be a percentage of the terminal too, whatever the
/// modal held. On anything smaller than the size a modal was tuned on, the
/// bottom rows were cut off, and the bottom row is where the key hint is.
pub(crate) fn modal_rect(
    percent_x: u16,
    content_width: usize,
    content_height: usize,
    area: Rect,
) -> Rect {
    let border = |n: usize| u16::try_from(n.saturating_add(2)).unwrap_or(u16::MAX);
    let wanted = u16::try_from(u32::from(area.width) * u32::from(percent_x) / 100).unwrap_or(0);
    let width = wanted.max(border(content_width)).min(area.width);
    let height = border(content_height).min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Draw `lines` in a bordered modal titled `title`, sized by [`modal_rect`].
pub(crate) fn render_modal(
    f: &mut Frame,
    title: &str,
    color: ratatui::style::Color,
    percent_x: u16,
    lines: Vec<Line>,
) {
    let width = lines
        .iter()
        .map(Line::width)
        .chain(std::iter::once(title.chars().count()))
        .max()
        .unwrap_or(0);
    // A column of space either side, so a line the modal widened to fit
    // does not run into the border.
    let modal = modal_rect(percent_x, width + 2, lines.len(), f.area());
    f.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(Span::styled(
            title.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(modal);
    f.render_widget(block, modal);
    f.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------------------
// Session select
// ---------------------------------------------------------------------------

pub mod session_select {
    use super::*;

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
                "  (no saved sessions — press [n] to connect, then save it)",
                Style::default().fg(app.theme.dim),
            )));
        } else {
            // Only the sessions that fit, scrolled to keep the cursor in
            // view, as a file pane does.
            let window = crate::tui::state::scrolled_window(
                app.session_view_offset.get(),
                app.session_cursor,
                app.sessions.len(),
                area.height.saturating_sub(2) as usize,
            );
            app.session_view_offset.set(window.start);
            for (i, s) in app
                .sessions
                .iter()
                .enumerate()
                .skip(window.start)
                .take(window.len())
            {
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
                let proto_style = protocol_style(&s.protocol, &app.theme);

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
        // Sessions start below the heading and its blank line, one per row;
        // row `r` is session `session_view_offset + r`.
        app.hit.set(|a| {
            a.session_list = Some(Rect {
                y: area.y + 2,
                height: area.height.saturating_sub(2),
                ..area
            })
        });
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
        let body = lines(app);
        let width = body.iter().map(Line::width).max().unwrap_or(0);
        // The body plus one footer row, which stays put while the body
        // scrolls: help is taller than an 80x24 terminal.
        let modal = super::modal_rect(70, width + 2, body.len() + 1, f.area());

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

        let body_area = Rect {
            height: inner.height.saturating_sub(1),
            ..inner
        };
        let footer_area = Rect {
            y: inner.y + body_area.height,
            height: inner.height.min(1),
            ..inner
        };
        app.hit.set(|a| a.help_body = Some(body_area));

        let max = scroll_limit(body.len(), body_area.height);
        let scroll = u16::try_from(app.help_scroll.min(max)).unwrap_or(u16::MAX);
        f.render_widget(Paragraph::new(body).scroll((scroll, 0)), body_area);

        let footer = if max > 0 {
            "[↑↓] scroll  ·  esc / ?  to close"
        } else {
            "esc / ?  to close"
        };
        f.render_widget(
            Paragraph::new(
                Line::from(Span::styled(footer, Style::default().fg(app.theme.dim)))
                    .alignment(Alignment::Center),
            ),
            footer_area,
        );
    }

    /// The furthest help can scroll, as last drawn. Zero before it has been.
    pub(crate) fn max_scroll(app: &App) -> usize {
        app.hit
            .get()
            .help_body
            .map_or(0, |r| scroll_limit(lines(app).len(), r.height))
    }

    fn scroll_limit(lines: usize, rows: u16) -> usize {
        lines.saturating_sub(usize::from(rows))
    }

    fn lines(app: &App) -> Vec<Line<'static>> {
        let key_s = Style::default()
            .fg(app.theme.border_active)
            .add_modifier(Modifier::BOLD);
        let acc_s = Style::default()
            .fg(app.theme.accent)
            .add_modifier(Modifier::BOLD);

        vec![
            Line::from(""),
            Line::from(Span::styled("  NAVIGATION", acc_s)),
            kv("  ↵         ", "enter directory", key_s),
            kv("  bksp      ", "go up to parent directory", key_s),
            kv("  tab       ", "switch active pane", key_s),
            kv("  ↑ ↓       ", "move cursor (pgup/pgdn for page)", key_s),
            kv("            ", "in the log pane, scroll it", key_s),
            Line::from(""),
            Line::from(Span::styled("  FILES", acc_s)),
            kv("  space     ", "select / deselect", key_s),
            kv("  ctrl+d    ", "download selected items", key_s),
            kv(
                "  ctrl+u    ",
                "upload selected items (local → remote)",
                key_s,
            ),
            kv(
                "  v         ",
                "view image (kitty/sixel/iterm2) or text",
                key_s,
            ),
            kv(
                "  /         ",
                "filter current directory by substring",
                key_s,
            ),
            kv("  F5        ", "refresh active pane", key_s),
            kv("  F2        ", "rename file or folder (remote pane)", key_s),
            kv("  F7        ", "create new remote directory", key_s),
            kv("  shift+del ", "delete file or folder (remote pane)", key_s),
            kv(
                "  D         ",
                "delete, where shift+del doesn't reach",
                key_s,
            ),
            Line::from(""),
            Line::from(Span::styled("  SESSION", acc_s)),
            kv("  ctrl+s    ", "save current session", key_s),
            kv(
                "  ctrl+x    ",
                "disconnect (return to session selector)",
                key_s,
            ),
            kv("  t         ", "cycle to the next theme", key_s),
            Line::from(""),
            Line::from(Span::styled("  TRANSFERS", acc_s)),
            kv("  p         ", "pause / resume all transfers", key_s),
            kv(
                "  c         ",
                "cancel selected transfer (Transfers pane)",
                key_s,
            ),
            kv("  C         ", "cancel whole batch (Transfers pane)", key_s),
            kv(
                "  r · R     ",
                "resume an interrupted download · upload batch",
                key_s,
            ),
            Line::from(""),
            Line::from(Span::styled("  VIEWER", acc_s)),
            kv("  ↑ ↓ · j k ", "scroll a line", key_s),
            kv("  pgup pgdn ", "scroll a page (space too)", key_s),
            kv("  g · G     ", "top · bottom", key_s),
            Line::from(""),
            Line::from(Span::styled("  SESSION LIST", acc_s)),
            kv("  ↵         ", "connect", key_s),
            kv("  n · e · d ", "new · edit · delete", key_s),
            Line::from(""),
            Line::from(Span::styled("  APP", acc_s)),
            kv("  ?         ", "toggle this help", key_s),
            kv("  ctrl+c    ", "quit, from anywhere", key_s),
            kv("  q · esc   ", "quit (with confirmation)", key_s),
        ]
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

/// One line describing what ending the session right now would throw away,
/// or `None` when there is nothing in flight.
///
/// Shared by the quit and disconnect confirmations so they can't disagree
/// about the same situation. Both count queued jobs alongside running ones:
/// each ending shuts the dispatcher down, so pending work is discarded too,
/// and a warning that mentioned only the running jobs understated it.
fn cancellation_warning(active: usize, pending: usize) -> Option<String> {
    let total = active + pending;
    if total == 0 {
        return None;
    }
    let plural = if total == 1 { "" } else { "s" };
    Some(match (active, pending) {
        (0, p) => format!("{p} queued transfer{plural} will be discarded."),
        (a, 0) => format!("{a} in-flight transfer{plural} will be cancelled."),
        (a, p) => {
            format!("{a} in-flight and {p} queued transfer{plural} will be cancelled.")
        }
    })
}

/// Colour for a session's protocol tag in the selector.
///
/// `theme.warning` is reserved for the protocols where
/// [`Protocol::is_cleartext`] holds. FTPS used to share that arm with FTP,
/// which told the user the encrypted protocol was as risky as the cleartext
/// one and left no way to tell them apart in the list.
///
/// SCP is SFTP underneath (see `transport/scp.rs`), so it reads as SFTP
/// does; it used to have a colour of its own, as if it were a different
/// kind of connection.
pub(crate) fn protocol_style(protocol: &Protocol, theme: &crate::theme::Theme) -> Style {
    match protocol {
        Protocol::Sftp | Protocol::Scp | Protocol::Ftps => Style::default().fg(theme.success),
        Protocol::Ftp => Style::default().fg(theme.warning),
    }
}

#[cfg(test)]
mod protocol_style_tests {
    use super::protocol_style;
    use crate::session::Protocol;
    use crate::theme::Theme;
    use ratatui::style::Style;

    /// FTP and FTPS shared one arm, so the selector told the user the
    /// encrypted protocol was exactly as risky as the cleartext one — and
    /// gave no way to tell them apart at a glance. They must not read the
    /// same.
    #[test]
    fn ftp_and_ftps_do_not_share_a_colour() {
        let theme = Theme::load("dracula").unwrap();
        assert_ne!(
            protocol_style(&Protocol::Ftp, &theme),
            protocol_style(&Protocol::Ftps, &theme),
            "the cleartext protocol must not look like the encrypted one",
        );
    }

    /// Every encrypted protocol, SCP included, reads like SFTP.
    #[test]
    fn the_encrypted_protocols_share_sftps_colour() {
        let theme = Theme::load("dracula").unwrap();
        for p in [Protocol::Scp, Protocol::Ftps] {
            assert_eq!(
                protocol_style(&p, &theme),
                protocol_style(&Protocol::Sftp, &theme),
                "{}",
                p.as_str()
            );
        }
    }

    /// The cleartext protocol is the one that gets the caution colour, and
    /// the verified ones read like SFTP.
    #[test]
    fn only_the_cleartext_protocol_is_coloured_as_a_caution() {
        let theme = Theme::load("dracula").unwrap();
        let caution = Style::default().fg(theme.warning);
        for p in [Protocol::Sftp, Protocol::Scp, Protocol::Ftp, Protocol::Ftps] {
            assert_eq!(
                protocol_style(&p, &theme) == caution,
                p.is_cleartext(),
                "{} is coloured as a caution iff it is cleartext",
                p.as_str(),
            );
        }
    }
}

#[cfg(test)]
mod cancellation_warning_tests {
    use super::cancellation_warning;

    #[test]
    fn silent_when_nothing_is_in_flight() {
        // The bug this addresses: the quit modal warned about transfers being
        // cancelled even when none existed — including when quitting straight
        // from the session selector, with no connection at all.
        assert_eq!(cancellation_warning(0, 0), None);
    }

    #[test]
    fn counts_queued_work_not_just_running_work() {
        // A batch sitting behind the concurrency limit is discarded too, so
        // reporting only the running jobs understated the loss.
        let msg = cancellation_warning(0, 40).expect("40 queued is worth saying");
        assert!(msg.contains("40"), "{msg}");
        assert!(msg.contains("queued"), "{msg}");
    }

    #[test]
    fn reports_both_when_both_exist() {
        let msg = cancellation_warning(2, 40).unwrap();
        assert!(msg.contains('2') && msg.contains("40"), "{msg}");
        assert!(msg.contains("in-flight") && msg.contains("queued"), "{msg}");
    }

    #[test]
    fn singular_for_exactly_one() {
        assert_eq!(
            cancellation_warning(1, 0).unwrap(),
            "1 in-flight transfer will be cancelled.",
        );
        assert_eq!(
            cancellation_warning(0, 1).unwrap(),
            "1 queued transfer will be discarded.",
        );
    }

    #[test]
    fn plural_for_more_than_one_across_both_buckets() {
        // One of each is two transfers total, so it must read "transfers".
        assert!(cancellation_warning(1, 1).unwrap().contains("transfers"));
        assert!(cancellation_warning(3, 0).unwrap().contains("transfers"));
    }
}

pub mod confirm_quit {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        // Quit is reachable from the session selector too, where there is no
        // connection at all — so the warning is conditional. It used to be
        // unconditional, telling users their transfers would be cancelled
        // when nothing was running.
        let (active, pending) = app.queue_counts();

        let mut lines = vec![Line::from("")];
        if let Some(warning) = super::cancellation_warning(active, pending) {
            lines.push(
                Line::from(Span::styled(warning, Style::default().fg(app.theme.error)))
                    .alignment(Alignment::Center),
            );
            lines.push(Line::from(""));
        }
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
        super::render_modal(f, " quit blink? ", app.theme.warning, 40, lines);
    }
}

// ---------------------------------------------------------------------------
// Cancel-transfer confirmation (overlay over Main)
// ---------------------------------------------------------------------------

pub mod confirm_cancel {
    use super::*;
    use crate::tui::state::PendingCancel;

    pub fn render(f: &mut Frame, app: &App) {
        let is_batch = matches!(
            app.pending_cancel.as_ref(),
            Some(PendingCancel::Batch { .. })
        );
        let title = if is_batch {
            " cancel batch? "
        } else {
            " cancel transfer? "
        };

        let mut lines: Vec<Line> = vec![Line::from("")];
        match app.pending_cancel.as_ref() {
            Some(PendingCancel::Single { name, .. }) => {
                lines.push(Line::from("  cancel this transfer:").alignment(Alignment::Center));
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
                    Line::from(format!("  cancel {total} transfer{plural} in this batch?"))
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
        super::render_modal(f, title, app.theme.warning, 60, lines);
    }
}

// ---------------------------------------------------------------------------
// Password prompt (overlay over SessionSelect)
// ---------------------------------------------------------------------------

pub mod password_prompt {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
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
                Span::styled("█", Style::default().fg(app.theme.border_active)),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  [↵] connect    [esc] cancel    [^u] clear",
                Style::default().fg(app.theme.dim),
            )),
        ];
        super::render_modal(f, " password ", app.theme.border_active, 50, lines);
    }
}

// ---------------------------------------------------------------------------
// Connection in flight (overlay over Main)
// ---------------------------------------------------------------------------

pub mod connection {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
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
        super::render_modal(f, " connecting ", app.theme.warning, 50, lines);
    }
}

// ---------------------------------------------------------------------------
// Viewer (text or image, overlay over Main)
// ---------------------------------------------------------------------------

pub mod viewer {
    use super::*;
    use crate::tui::state::ViewerKind;

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
        app.hit.set(|a| a.viewer_body = Some(body));
        let hint = Rect::new(
            inner.x,
            inner.y.saturating_add(inner.height.saturating_sub(1)),
            inner.width,
            inner.height.min(1),
        );

        match &viewer.kind {
            ViewerKind::Loading => {
                let p = Paragraph::new(
                    Line::from(Span::styled("loading…", Style::default().fg(app.theme.dim)))
                        .alignment(Alignment::Center),
                );
                f.render_widget(p, body);
            }
            ViewerKind::Text { tokens, scroll } => {
                render_text(f, body, app, tokens, *scroll);
            }
            ViewerKind::Image { render, .. } => {
                // Once rendered, the body is left blank: graphics escape
                // codes are emitted on top of these cells from
                // `App::after_draw`. Until then, say what is happening.
                let p = if render.done.is_none() {
                    Paragraph::new(
                        Line::from(Span::styled(
                            "rendering image…",
                            Style::default().fg(app.theme.dim),
                        ))
                        .alignment(Alignment::Center),
                    )
                } else {
                    Paragraph::new(Line::from(""))
                };
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
            ViewerKind::Text { tokens, scroll } => format!(
                "  line {}/{}    [↑↓] scroll  [pgup/pgdn] page  [g/G] top/bottom  [q/esc] close",
                scroll.saturating_add(1).min(tokens.len().max(1)),
                tokens.len(),
            ),
            _ => "  [q/esc] close".to_string(),
        };
        let hint_style = Style::default().fg(app.theme.dim);
        f.render_widget(Paragraph::new(Span::styled(hint_text, hint_style)), hint);
    }

    fn render_text(
        f: &mut Frame,
        area: Rect,
        app: &App,
        tokens: &[Vec<(crate::highlight::TokenKind, String)>],
        scroll: usize,
    ) {
        let h = area.height as usize;
        if h == 0 || tokens.is_empty() {
            return;
        }

        // One entry per line, so the token count is the line count.
        let start = scroll.min(crate::tui::app::text_scroll_limit(tokens.len(), h));
        let end = (start + h).min(tokens.len());
        let lineno_width = end.to_string().len();

        let mut rendered = Vec::with_capacity(end - start);
        for (i, line_tokens) in tokens[start..end].iter().enumerate() {
            let n = start + i + 1;
            let mut spans = vec![Span::styled(
                format!("{:>width$} │ ", n, width = lineno_width),
                Style::default().fg(app.theme.dim),
            )];
            for (kind, text) in line_tokens {
                spans.push(Span::styled(text.clone(), token_style(*kind, app)));
            }
            rendered.push(Line::from(spans));
        }

        f.render_widget(Paragraph::new(rendered), area);
    }

    fn token_style(kind: crate::highlight::TokenKind, app: &App) -> Style {
        use crate::highlight::TokenKind;
        match kind {
            TokenKind::Keyword => Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
            TokenKind::Type => Style::default().fg(app.theme.directory),
            TokenKind::String => Style::default().fg(app.theme.success),
            TokenKind::Comment => Style::default().fg(app.theme.dim),
            TokenKind::Number => Style::default().fg(app.theme.warning),
            TokenKind::Macro => Style::default()
                .fg(app.theme.image)
                .add_modifier(Modifier::BOLD),
            TokenKind::Plain => Style::default().fg(app.theme.fg),
        }
    }
}

// ---------------------------------------------------------------------------
// New session (URL input, overlay over SessionSelect)
// ---------------------------------------------------------------------------

pub mod new_session {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
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
                Span::styled(format!("✗ {err}"), Style::default().fg(app.theme.error)),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  [↵] connect    [esc] cancel    [^u] clear",
            dim,
        )));

        super::render_modal(f, " new connection ", app.theme.border_active, 64, lines);
    }
}

// ---------------------------------------------------------------------------
// Save current session (overlay over Main)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Offer to save an ad-hoc connection (overlay over Main)
// ---------------------------------------------------------------------------

pub mod offer_save_session {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        let dim = Style::default().fg(app.theme.dim);
        let fg = Style::default().fg(app.theme.fg);

        let target = app
            .current_session
            .as_ref()
            .map(|s| format!("{}@{}:{}", s.username, s.host, s.port))
            .unwrap_or_else(|| "this connection".into());

        let lines = vec![
            Line::from(""),
            Line::from(Span::styled("connected — but this one isn't saved yet", fg))
                .alignment(Alignment::Center),
            Line::from(""),
            Line::from(Span::styled(
                crate::error::sanitize_display(&target).into_owned(),
                fg.add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Center),
            Line::from(""),
            Line::from(Span::styled(
                "saving keeps it in the selector for next time.",
                dim,
            ))
            .alignment(Alignment::Center),
            Line::from(Span::styled("you can always save later with ctrl+s.", dim))
                .alignment(Alignment::Center),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "[y]",
                    Style::default()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" save      "),
                Span::styled(
                    "[n/esc]",
                    Style::default()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" not now  "),
            ])
            .alignment(Alignment::Center),
        ];
        super::render_modal(f, " save this session? ", app.theme.accent, 56, lines);
    }
}

pub mod offer_resume_checkpoint {
    use super::*;
    use crate::tui::state::PostConnectOffer;

    const PERCENT_X: u16 = 60;

    /// Coarse, human-readable age. Precision past the unit is noise here —
    /// the question the user is answering is "is this batch still relevant".
    fn human_age(d: std::time::Duration) -> String {
        let secs = d.as_secs();
        let (n, unit) = if secs < 60 {
            return "just now".to_string();
        } else if secs < 3600 {
            (secs / 60, "minute")
        } else if secs < 86_400 {
            (secs / 3600, "hour")
        } else {
            (secs / 86_400, "day")
        };
        let plural = if n == 1 { "" } else { "s" };
        format!("{n} {unit}{plural} ago")
    }

    pub fn render(f: &mut Frame, app: &App) {
        let Some(PostConnectOffer::ResumeCheckpoint(offer)) = app.pending_offers_front() else {
            return;
        };

        let dim = Style::default().fg(app.theme.dim);
        let fg = Style::default().fg(app.theme.fg);
        // Sample paths are cut to the modal's usual width (less its border
        // and indent) rather than widening it to the longest path.
        let width =
            usize::from(super::modal_rect(PERCENT_X, 0, 0, f.area()).width).saturating_sub(2 + 4);

        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("an interrupted {} batch was found", offer.kind.as_str()),
                fg,
            ))
            .alignment(Alignment::Center),
            Line::from(""),
        ];

        let counts = format!("{} of {} items remaining", offer.remaining, offer.total);
        let headline = match &offer.age {
            Some(age) => format!("{counts} · {}", human_age(*age)),
            None => counts,
        };
        lines.push(
            Line::from(Span::styled(headline, fg.add_modifier(Modifier::BOLD)))
                .alignment(Alignment::Center),
        );
        lines.push(Line::from(""));

        // Already sanitized in `CheckpointOffer`; only width remains.
        for path in &offer.sample_paths {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(crate::tui::widgets::truncate_middle(path, width), dim),
            ]));
        }
        if offer.remaining > offer.sample_paths.len() {
            let more = offer.remaining - offer.sample_paths.len();
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("… and {more} more"), dim),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(
            Line::from(vec![
                Span::styled(
                    "[r]",
                    Style::default()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" resume   ", dim),
                Span::styled(
                    "[d]",
                    Style::default()
                        .fg(app.theme.warning)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" discard   ", dim),
                Span::styled(
                    "[esc]",
                    Style::default()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" later", dim),
            ])
            .alignment(Alignment::Center),
        );

        super::render_modal(
            f,
            " resume interrupted transfer? ",
            app.theme.accent,
            PERCENT_X,
            lines,
        );
    }

    #[cfg(test)]
    mod age_tests {
        use super::human_age;
        use std::time::Duration;

        #[test]
        fn recent_ages_read_as_minutes() {
            assert_eq!(human_age(Duration::from_secs(90)), "1 minute ago");
            assert_eq!(human_age(Duration::from_secs(600)), "10 minutes ago");
        }

        #[test]
        fn hours_and_days_round_down() {
            assert_eq!(
                human_age(Duration::from_secs(3 * 3600 + 59 * 60)),
                "3 hours ago"
            );
            assert_eq!(human_age(Duration::from_secs(50 * 3600)), "2 days ago");
        }

        #[test]
        fn the_first_minute_reads_as_just_now() {
            assert_eq!(human_age(Duration::from_secs(5)), "just now");
        }

        #[test]
        fn singular_and_plural_are_both_handled() {
            assert_eq!(human_age(Duration::from_secs(3600)), "1 hour ago");
            assert_eq!(human_age(Duration::from_secs(24 * 3600)), "1 day ago");
        }
    }
}

pub mod save_session {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
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
                Span::styled(format!("✗ {err}"), Style::default().fg(app.theme.error)),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  [↵] save    [esc] cancel    [^u] clear",
            dim,
        )));

        super::render_modal(f, " save session ", app.theme.border_active, 60, lines);
    }
}

// ---------------------------------------------------------------------------
// Rename (overlay over Main)
// ---------------------------------------------------------------------------

pub mod rename {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
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
                Span::styled(app.rename_input.as_str(), Style::default().fg(app.theme.fg)),
                Span::styled("█", Style::default().fg(app.theme.border_active)),
            ]),
        ];
        if let Some(err) = &app.rename_error {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("✗ {err}"), Style::default().fg(app.theme.error)),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  [↵] rename    [esc] cancel    [^u] clear",
            dim,
        )));

        super::render_modal(f, " rename ", app.theme.border_active, 60, lines);
    }
}

// ---------------------------------------------------------------------------
// Mkdir (overlay over Main)
// ---------------------------------------------------------------------------

pub mod mkdir {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        let dim = Style::default().fg(app.theme.dim);
        let mut lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("in: ", dim),
                Span::styled(
                    if app.remote.path.is_empty() {
                        "/".to_string()
                    } else {
                        app.remote.path.clone()
                    },
                    dim,
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("name: ", dim),
                Span::styled(app.mkdir_input.as_str(), Style::default().fg(app.theme.fg)),
                Span::styled("█", Style::default().fg(app.theme.border_active)),
            ]),
        ];
        if let Some(err) = &app.mkdir_error {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("✗ {err}"), Style::default().fg(app.theme.error)),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  [↵] create    [esc] cancel    [^u] clear",
            dim,
        )));

        super::render_modal(f, " new directory ", app.theme.border_active, 60, lines);
    }
}

// ---------------------------------------------------------------------------
// Confirm delete (overlay over Main)
// ---------------------------------------------------------------------------

pub mod confirm_delete {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
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
        super::render_modal(f, " delete? ", app.theme.error, 50, lines);
    }
}

// ---------------------------------------------------------------------------
// Confirm overwrite (overlay over Main)
// ---------------------------------------------------------------------------

pub mod confirm_overwrite {
    use super::*;
    use crate::tui::plan::PlannedJob;
    use crate::tui::state::OverwritePending;

    pub fn render(f: &mut Frame, app: &App) {
        let mut lines = vec![Line::from("")];
        let is_plan = matches!(
            app.pending_overwrite.as_ref(),
            Some(OverwritePending::DownloadPlan { .. }) | Some(OverwritePending::UploadPlan { .. })
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
                ..
            }) => {
                render_plan_body(&mut lines, app, plan, conflict_indices, PlanKind::Download);
            }
            Some(OverwritePending::UploadPlan {
                plan,
                conflict_indices,
            }) => {
                render_plan_body(&mut lines, app, plan, conflict_indices, PlanKind::Upload);
            }
            None => {}
        }

        lines.push(Line::from(""));
        // Hint strip: rename modal has no skip option, plan modals do.
        let key_style = |c| Style::default().fg(c).add_modifier(Modifier::BOLD);
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
        super::render_modal(f, " overwrite? ", app.theme.warning, 60, lines);
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
        let paths: Vec<&str> = conflict_indices
            .iter()
            .filter_map(|&idx| match &plan[idx] {
                PlannedJob::Download { remote_path, .. }
                | PlannedJob::Upload { remote_path, .. } => Some(remote_path.as_str()),
                PlannedJob::Mkdir { .. } => None, // shouldn't appear
            })
            .collect();
        for name in conflict_row_names(&paths).into_iter().take(max_show) {
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
}

/// The names shown for conflicting files in the overwrite-confirmation
/// modal: each path below the folder they all share, so two `readme.md` in
/// different subfolders don't read as one file listed twice. A lone
/// conflict is shown by its name.
///
/// Sanitized, like every other site that renders a server-supplied name.
/// `ratatui` would drop the deceptive formatters on its own — `Buffer::set_stringn`
/// filters `char::is_control` and zero-width graphemes — but dropping them
/// and replacing them with a space are different policies, and this is the
/// prompt `is_deceptive_format` was written for: two remote names that differ
/// only by a bidi override must not render identically in the dialog the user
/// clears them through.
pub(crate) fn conflict_row_names(paths: &[&str]) -> Vec<String> {
    let parent = |p: &str| -> Vec<String> {
        let mut parts: Vec<String> = p
            .split('/')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        parts.pop();
        parts
    };
    let mut shared = paths.first().map(|p| parent(p)).unwrap_or_default();
    for p in paths.iter().skip(1) {
        let dir = parent(p);
        let same = shared.iter().zip(&dir).take_while(|(a, b)| a == b).count();
        shared.truncate(same);
    }
    paths
        .iter()
        .map(|p| {
            let rest: Vec<&str> = p
                .split('/')
                .filter(|s| !s.is_empty())
                .skip(shared.len())
                .collect();
            crate::error::sanitize(rest.join("/"))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Edit session (overlay over SessionSelect)
// ---------------------------------------------------------------------------

pub mod edit_session {
    use super::*;
    use crate::tui::state::EditField;

    pub fn render(f: &mut Frame, app: &App) {
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
        aic_spans.push(Span::styled("  accept invalid certs (FTPS only)", dim));
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
                Span::styled(format!("✗ {err}"), Style::default().fg(app.theme.error)),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  [↑↓/tab] field    [space] toggle    [↵] save    [esc] cancel",
            dim,
        )));

        super::render_modal(f, " edit session ", app.theme.border_active, 64, lines);
    }
}

// ---------------------------------------------------------------------------
// Confirm delete session (overlay over SessionSelect)
// ---------------------------------------------------------------------------

pub mod confirm_delete_session {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        let dim = Style::default().fg(app.theme.dim);
        let fg = Style::default().fg(app.theme.fg);
        let mut lines = vec![Line::from("")];
        if let Some(s) = app.pending_session_delete.as_ref() {
            lines.push(
                Line::from("  permanently delete this session:").alignment(Alignment::Center),
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
                Line::from(Span::styled("this only removes the saved session.", dim))
                    .alignment(Alignment::Center),
            );
            lines.push(
                Line::from(Span::styled("no remote files are touched.", dim))
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

        super::render_modal(f, " delete session? ", app.theme.error, 50, lines);
    }
}

// ---------------------------------------------------------------------------
// SSH key passphrase prompt (overlay over SessionSelect)
// ---------------------------------------------------------------------------

pub mod key_passphrase_prompt {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
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
            Line::from(vec![Span::raw("  "), Span::styled(key_label, dim)]),
            Line::from(""),
            Line::from(vec![
                Span::raw("  passphrase: "),
                Span::styled(masked, Style::default().fg(app.theme.fg)),
                Span::styled("█", Style::default().fg(app.theme.border_active)),
            ]),
        ];
        if let Some(err) = &app.passphrase_error {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("✗ {err}"), Style::default().fg(app.theme.error)),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  [↵] unlock    [esc] cancel    [^u] clear",
            dim,
        )));

        super::render_modal(
            f,
            " ssh key passphrase ",
            app.theme.border_active,
            56,
            lines,
        );
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
            Pane::Local => app.local.entries.iter().filter(|e| !e.is_parent()).count(),
            Pane::Remote => app.remote.entries.iter().filter(|e| !e.is_parent()).count(),
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
            Span::styled(app.search_input.clone(), Style::default().fg(app.theme.fg)),
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
        let dim = Style::default().fg(app.theme.dim);
        let fg = Style::default().fg(app.theme.fg);
        let (active, pending) = app.queue_counts();
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
            Line::from("  return to the session selector?").alignment(Alignment::Center),
            Line::from(""),
            Line::from(Span::styled(target, fg.add_modifier(Modifier::BOLD)))
                .alignment(Alignment::Center),
            Line::from(""),
        ];
        if let Some(warning) = super::cancellation_warning(active, pending) {
            lines.push(
                Line::from(Span::styled(warning, Style::default().fg(app.theme.error)))
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
        super::render_modal(f, " disconnect? ", app.theme.warning, 50, lines);
    }
}

// ---------------------------------------------------------------------------
// Host-key confirmation prompt
// ---------------------------------------------------------------------------

pub mod confirm_host_key {
    use super::*;
    use crate::tui::event::TrustSubject;

    pub fn render(f: &mut Frame, app: &App) {
        let Some(phk) = app.pending_host_key.as_ref() else {
            return;
        };
        let (title, explanation, type_label) = match phk.subject {
            TrustSubject::SshHostKey => (
                " unknown host key ",
                "The server's host key is not in your known-hosts file.",
                "  Key type:    ",
            ),
            TrustSubject::FtpsCertificate => (
                " unknown certificate ",
                "This session has no certificate pinned for the server.",
                "  Type:        ",
            ),
        };

        let (host, key_type) = (phk.host.as_str(), phk.key_type.as_str());

        // A certificate's fingerprint, `AB:CD:…`, is 95 columns: wider than
        // the modal. Break it, between byte pairs, onto as many lines as it
        // takes rather than let the end be cut off.
        const LABEL: &str = "  Fingerprint: ";
        let inner_width =
            usize::from(super::modal_rect(66, 0, 0, f.area()).width).saturating_sub(2);
        let room = inner_width.saturating_sub(LABEL.len()).max(12);
        let room = if phk.subject == TrustSubject::FtpsCertificate {
            // Whole `AB:` pairs, less the trailing colon: 3k - 1 columns.
            (room + 1) / 3 * 3 - 1
        } else {
            room
        };
        let mut fingerprint_lines = Vec::new();
        let mut rest = phk.fingerprint.as_str();
        while !rest.is_empty() {
            let mut cut = rest.len().min(room);
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            let (piece, tail) = rest.split_at(cut);
            rest = tail.strip_prefix(':').unwrap_or(tail);
            let label = if fingerprint_lines.is_empty() {
                LABEL
            } else {
                "               "
            };
            fingerprint_lines.push(Line::from(vec![
                Span::styled(label, Style::default().fg(app.theme.dim)),
                Span::styled(piece.to_string(), Style::default().fg(app.theme.accent)),
            ]));
        }

        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(explanation, Style::default().fg(app.theme.fg)))
                .alignment(Alignment::Center),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Host:        ", Style::default().fg(app.theme.dim)),
                Span::styled(
                    host,
                    Style::default()
                        .fg(app.theme.fg)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled(type_label, Style::default().fg(app.theme.dim)),
                Span::styled(key_type, Style::default().fg(app.theme.fg)),
            ]),
        ];
        lines.extend(fingerprint_lines);
        lines.extend([
            Line::from(""),
            Line::from(Span::styled(
                "Verify this fingerprint out-of-band before accepting.",
                Style::default().fg(app.theme.warning),
            ))
            .alignment(Alignment::Center),
            Line::from(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "[y]",
                    Style::default()
                        .fg(app.theme.success)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" accept & save    "),
                Span::styled(
                    "[t]",
                    Style::default()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" trust once    "),
                Span::styled(
                    "[n/esc]",
                    Style::default()
                        .fg(app.theme.error)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" reject"),
            ])
            .alignment(Alignment::Center),
        ]);

        super::render_modal(f, title, app.theme.warning, 66, lines);
    }
}

// ---------------------------------------------------------------------------
// Host-key mismatch error screen
// ---------------------------------------------------------------------------

pub mod host_key_changed {
    use super::*;

    pub fn render(f: &mut Frame, app: &App) {
        let info = match app.host_key_changed_info.as_ref() {
            Some(i) => i,
            None => return,
        };

        // Exact, copy-pasteable recovery command. Deliberately something the
        // user has to leave blink to run: this modal is the MITM warning, and
        // a key that forgets the stored fingerprint from inside it would turn
        // the warning into a dismiss button. Same posture as OpenSSH pointing
        // at `ssh-keygen -R` rather than offering to do it for you.
        let remove_cmd = if info.lookup_port == 22 {
            format!("blink known-hosts remove {}", info.lookup_host)
        } else {
            format!(
                "blink known-hosts remove {} --port {}",
                info.lookup_host, info.lookup_port
            )
        };

        // A key trusted once was never stored, so there is nothing to
        // remove: dismissing disconnects, which forgets it, and the next
        // connect asks afresh.
        let (advice, action, command) = if info.trusted_once {
            (
                "You trusted this host's key once, for this session only.",
                "Only if you have confirmed the new fingerprint out of band,",
                "reconnect and accept it when asked.".to_string(),
            )
        } else {
            (
                "Only if you know the key was legitimately replaced, and",
                "have confirmed the new fingerprint out of band, run:",
                remove_cmd,
            )
        };

        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "⚠  WARNING: HOST KEY MISMATCH",
                Style::default()
                    .fg(app.theme.error)
                    .add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Center),
            Line::from(""),
            Line::from(Span::styled(
                "The server presented a different key than expected.",
                Style::default().fg(app.theme.fg),
            ))
            .alignment(Alignment::Center),
            Line::from(Span::styled(
                "This may indicate a man-in-the-middle attack.",
                Style::default().fg(app.theme.warning),
            ))
            .alignment(Alignment::Center),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Host:          ", Style::default().fg(app.theme.dim)),
                Span::styled(
                    info.host.clone(),
                    Style::default()
                        .fg(app.theme.fg)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Expected type: ", Style::default().fg(app.theme.dim)),
                Span::styled(
                    info.stored_key_type.clone(),
                    Style::default().fg(app.theme.fg),
                ),
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
                Span::styled(
                    info.fingerprint.clone(),
                    Style::default().fg(app.theme.accent),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(advice, Style::default().fg(app.theme.dim)))
                .alignment(Alignment::Center),
            Line::from(Span::styled(action, Style::default().fg(app.theme.dim)))
                .alignment(Alignment::Center),
            Line::from(""),
            Line::from(Span::styled(
                command,
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Center),
            Line::from(""),
            Line::from(Span::styled(
                "[enter/esc/q] dismiss",
                Style::default().fg(app.theme.dim),
            ))
            .alignment(Alignment::Center),
        ];

        super::render_modal(f, " host key mismatch ", app.theme.error, 66, lines);
    }
}

// ---------------------------------------------------------------------------
// Rendering smoke tests
// ---------------------------------------------------------------------------
//
// Each modal drawn through `App::draw` into a TestBackend, checking that what
// the user needs to decide is on screen: the title, the state it describes,
// both arms of every conditional line, and the key hint. Secrets are checked
// to stay masked.

#[cfg(test)]
mod render_tests {
    use crate::checkpoint::{CheckpointKind, CheckpointOffer};
    use crate::session::{AuthMethod, Session};
    use crate::tui::app::tests_support::{app_on_main, draw, draw_sized, queue_offer};
    use crate::tui::app::{App, Pane, Screen};
    use crate::tui::plan::PlannedJob;
    use crate::tui::state::{
        EditField, EditSessionForm, OverwritePending, PaneEntry, PendingCancel, PendingDelete,
        PostConnectOffer,
    };

    /// Every row of `buf`, right-trimmed and joined with newlines.
    fn text_of(buf: &ratatui::buffer::Buffer) -> String {
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

    fn screen(a: &App) -> String {
        text_of(&draw(a))
    }

    /// `a` drawn on the smallest terminal blink is meant to be usable on.
    fn small_screen(a: &App) -> String {
        text_of(&draw_sized(a, 80, 24))
    }

    fn session(url: &str) -> Session {
        Session::from_url(url).unwrap()
    }

    /// An app on `screen`, over the main view.
    fn on(screen: Screen) -> App {
        let mut a = app_on_main(3);
        a.screen = screen;
        a
    }

    /// An app on `screen`, over the session selector.
    fn over_selector(screen: Screen) -> App {
        let mut a = on(screen);
        a.quit_return = Screen::SessionSelect;
        a.help_return = Screen::SessionSelect;
        a
    }

    fn with_queued_downloads(a: &mut App, n: usize) {
        let (m, _events) = crate::transfer::TransferManager::new(1);
        for i in 0..n {
            m.enqueue_download(format!("/r/f{i}"), format!("/l/f{i}").into())
                .unwrap();
        }
        a.transfer_manager = Some(m);
    }

    // --- session selector and main view details ---

    #[test]
    fn the_selector_shows_a_sessions_local_dir_and_parallel_override() {
        let mut a = over_selector(Screen::SessionSelect);
        let mut s = session("sftp://me@h.example");
        s.name = "work".into();
        s.local_dir = Some("/home/me/dl".into());
        s.parallel_downloads = Some(7);
        a.sessions = vec![s];

        let text = screen(&a);
        assert!(text.contains("/home/me/dl"), "{text}");
        assert!(text.contains("7 streams (override)"), "{text}");
    }

    #[test]
    fn the_selector_falls_back_to_home_and_the_global_parallel_count() {
        let mut a = over_selector(Screen::SessionSelect);
        a.sessions = vec![session("sftp://me@h.example")];

        let text = screen(&a);
        assert!(text.contains("(home)"), "{text}");
        let global = a.config.general.parallel_downloads;
        assert!(text.contains(&format!("{global} (global)")), "{text}");
    }

    #[test]
    fn the_status_bar_names_the_connected_session() {
        let mut a = on(Screen::Main);
        assert!(screen(&a).contains("not connected"));
        a.current_session = Some(session("sftp://me@h.example:2222"));
        assert!(screen(&a).contains("sftp · me@h.example:2222"));
    }

    // --- help ---

    #[test]
    fn help_fits_whole_on_a_tall_terminal() {
        let text = text_of(&draw_sized(&on(Screen::Help), 120, 70));
        assert!(text.contains("HELP — keyboard shortcuts"), "{text}");
        for wanted in [
            "NAVIGATION",
            "ctrl+d",
            "F7",
            "r · R",
            "SESSION LIST",
            "ctrl+c",
            "quit (with confirmation)",
        ] {
            assert!(text.contains(wanted), "missing {wanted:?}:\n{text}");
        }
        assert!(text.contains("esc / ?  to close"), "{text}");
        assert!(!text.contains("[↑↓] scroll"), "nothing to scroll:\n{text}");
    }

    #[test]
    fn help_taller_than_the_terminal_says_it_scrolls() {
        let text = small_screen(&on(Screen::Help));
        assert!(text.contains("NAVIGATION"), "{text}");
        assert!(!text.contains("quit (with confirmation)"), "{text}");
        assert!(text.contains("[↑↓] scroll  ·  esc / ?  to close"), "{text}");
    }

    #[test]
    fn help_drawn_scrolled_past_the_end_stops_at_the_last_line() {
        let mut a = on(Screen::Help);
        a.help_scroll = 1000;
        let text = small_screen(&a);
        assert!(text.contains("quit (with confirmation)"), "{text}");
        assert!(!text.contains("NAVIGATION"), "{text}");
    }

    // --- every modal at 80x24 ---

    /// Each modal with as much in it as it ever holds, and the text that
    /// must stay on screen: its key hint, its last line.
    fn fullest_modals() -> Vec<(&'static str, App, &'static str)> {
        let mut cases = Vec::new();

        let mut a = over_selector(Screen::ConfirmQuit);
        with_queued_downloads(&mut a, 3);
        cases.push(("quit", a, "[n/esc] no"));

        let mut a = on(Screen::ConfirmDisconnect);
        a.current_session = Some(session("sftp://someone@a-long-host.example.com:2222"));
        with_queued_downloads(&mut a, 3);
        cases.push(("disconnect", a, "[n/esc] no"));

        let mut a = on(Screen::ConfirmCancel);
        a.pending_cancel = Some(PendingCancel::Single {
            id: 1,
            name: "x".into(),
        });
        cases.push(("cancel one", a, "[n/esc] no"));

        let mut a = on(Screen::ConfirmCancel);
        a.pending_cancel = Some(PendingCancel::Batch {
            batch_id: 1,
            active: 1,
            pending: 2,
            cursor_name: "x".into(),
        });
        cases.push(("cancel batch", a, "[n/esc] no"));

        let mut a = over_selector(Screen::PasswordPrompt);
        a.pending_session = Some(session("sftp://me@h"));
        cases.push(("password", a, "[^u] clear"));

        let mut a = over_selector(Screen::KeyPassphrasePrompt);
        a.passphrase_error = Some("wrong passphrase".into());
        cases.push(("passphrase", a, "[^u] clear"));

        cases.push((
            "connecting",
            on(Screen::Connection),
            "press [esc] to cancel",
        ));

        let mut a = over_selector(Screen::NewSession);
        a.new_session_error = Some("missing user".into());
        cases.push(("new session", a, "[^u] clear"));

        cases.push((
            "offer save",
            on(Screen::OfferSaveSession),
            "[n/esc] not now",
        ));

        let mut a = on(Screen::OfferResumeCheckpoint);
        queue_offer(
            &mut a,
            PostConnectOffer::ResumeCheckpoint(resume_offer(10, &["/r/a", "/r/b", "/r/c"])),
        );
        cases.push(("offer resume", a, "[esc] later"));

        let mut a = on(Screen::SaveSession);
        a.save_session_error = Some("name taken".into());
        cases.push(("save session", a, "[^u] clear"));

        let mut a = on(Screen::Rename);
        a.rename_error = Some("bad name".into());
        cases.push(("rename", a, "[^u] clear"));

        let mut a = on(Screen::Mkdir);
        a.mkdir_error = Some("exists".into());
        cases.push(("mkdir", a, "[^u] clear"));

        let mut a = on(Screen::ConfirmDelete);
        a.pending_delete = Some(PendingDelete {
            name: "x".into(),
            is_dir: true,
            remote_path: "/x".into(),
        });
        cases.push(("delete", a, "[n/esc] no"));

        let mut a = on(Screen::ConfirmOverwrite);
        a.pending_overwrite = Some(OverwritePending::Rename {
            from: "a".into(),
            to: "b".into(),
            target_name: "b".into(),
        });
        cases.push(("overwrite on rename", a, "[n/esc] cancel"));

        let mut a = on(Screen::ConfirmOverwrite);
        a.pending_overwrite = Some(OverwritePending::DownloadPlan {
            plan: downloads(10),
            conflict_indices: (1..=8).collect(),
            local_dirs: Vec::new(),
        });
        cases.push(("overwrite plan", a, "[n/esc] cancel"));

        let mut a = over_selector(Screen::EditSession);
        let form = edit_form(&mut a);
        form.accept_invalid_certs = true;
        form.error = Some("bad port".into());
        cases.push(("edit session", a, "[esc] cancel"));

        let mut a = over_selector(Screen::ConfirmDeleteSession);
        a.pending_session_delete = Some(session("sftp://me@h"));
        cases.push(("delete session", a, "[n/esc] no"));

        // A certificate fingerprint is 95 columns and wraps.
        let mut a = over_selector(Screen::ConfirmHostKey);
        let (tx, _rx) = tokio::sync::oneshot::channel();
        a.pending_host_key = Some(crate::tui::state::PendingHostKey {
            subject: crate::tui::event::TrustSubject::FtpsCertificate,
            host: "h:21".into(),
            key_type: "TLS certificate, SHA-256".into(),
            fingerprint: vec!["AB"; 32].join(":"),
            decision_tx: Some(tx),
            opened_at: std::time::Instant::now(),
        });
        cases.push(("unknown host key", a, "[n/esc] reject"));

        let mut a = over_selector(Screen::HostKeyChanged);
        a.host_key_changed_info = Some(crate::tui::state::HostKeyChangedInfo {
            host: "[h]:2222".into(),
            lookup_host: "h".into(),
            lookup_port: 2222,
            stored_key_type: "ssh-ed25519".into(),
            presented_key_type: "ssh-rsa".into(),
            fingerprint: "SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG".into(),
            trusted_once: false,
        });
        cases.push(("host key changed", a, "[enter/esc/q] dismiss"));

        cases.push(("help", on(Screen::Help), "esc / ?  to close"));
        cases
    }

    /// Modals were sized as a percentage of the terminal, and on 80x24 all
    /// but two cut off their bottom rows: the key hint, and any error.
    #[test]
    fn every_modal_shows_its_last_line_on_an_80x24_terminal() {
        let clipped: Vec<String> = fullest_modals()
            .into_iter()
            .filter_map(|(name, a, last)| {
                let text = small_screen(&a);
                (!text.contains(last)).then(|| format!("{name}:\n{text}"))
            })
            .collect();
        assert!(clipped.is_empty(), "{}", clipped.join("\n\n"));
    }

    #[test]
    fn a_modal_widens_to_fit_its_longest_line() {
        // 40% of 80 columns is narrower than this warning.
        let mut a = over_selector(Screen::ConfirmQuit);
        with_queued_downloads(&mut a, 3);
        let text = small_screen(&a);
        assert!(
            text.contains("3 queued transfers will be discarded."),
            "{text}"
        );
    }

    #[test]
    fn the_host_key_changed_modal_keeps_its_recovery_command_on_80x24() {
        let (_, a, _) = fullest_modals()
            .into_iter()
            .find(|(name, ..)| *name == "host key changed")
            .unwrap();
        let text = small_screen(&a);
        assert!(
            text.contains("blink known-hosts remove h --port 2222"),
            "{text}"
        );
    }

    #[test]
    fn a_modal_never_outgrows_the_terminal() {
        let r = super::modal_rect(60, 500, 500, ratatui::layout::Rect::new(3, 4, 40, 10));
        assert_eq!(r, ratatui::layout::Rect::new(3, 4, 40, 10));
    }

    #[test]
    fn a_modal_is_as_tall_as_its_content_and_centred() {
        let area = ratatui::layout::Rect::new(0, 0, 100, 30);
        let r = super::modal_rect(50, 10, 6, area);
        assert_eq!((r.width, r.height), (50, 8));
        assert_eq!((r.x, r.y), (25, 11));
    }

    // --- quit / disconnect ---

    #[test]
    fn quit_with_nothing_running_warns_of_nothing() {
        let text = screen(&over_selector(Screen::ConfirmQuit));
        assert!(text.contains("quit blink?"), "{text}");
        assert!(text.contains("[y] yes"), "{text}");
        assert!(!text.contains("will be"), "{text}");
    }

    #[test]
    fn quit_with_queued_work_says_it_will_be_discarded() {
        let mut a = on(Screen::ConfirmQuit);
        a.quit_return = Screen::Main;
        with_queued_downloads(&mut a, 2);
        let text = screen(&a);
        assert!(
            text.contains("2 queued transfers will be discarded."),
            "{text}"
        );
        assert!(text.contains("[y] yes"), "{text}");
    }

    #[test]
    fn disconnect_names_the_session_and_what_ending_it_does() {
        let mut a = on(Screen::ConfirmDisconnect);
        let text = screen(&a);
        assert!(text.contains("this session"), "{text}");

        a.current_session = Some(session("ftp://anon@ftp.example"));
        let text = screen(&a);
        assert!(text.contains("disconnect?"), "{text}");
        assert!(text.contains("anon@ftp.example:21 (ftp)"), "{text}");
        assert!(text.contains("the connection will be closed."), "{text}");

        with_queued_downloads(&mut a, 1);
        let text = screen(&a);
        assert!(
            text.contains("1 queued transfer will be discarded."),
            "{text}"
        );
        assert!(!text.contains("the connection will be closed."), "{text}");
    }

    // --- cancel ---

    #[test]
    fn cancelling_one_transfer_names_it() {
        let mut a = on(Screen::ConfirmCancel);
        a.pending_cancel = Some(PendingCancel::Single {
            id: 1,
            name: "big.iso".into(),
        });
        let text = screen(&a);
        assert!(text.contains("cancel transfer?"), "{text}");
        assert!(text.contains("big.iso"), "{text}");
        assert!(text.contains("any partial file is left on disk."), "{text}");
    }

    #[test]
    fn cancelling_a_batch_counts_its_jobs() {
        let mut a = on(Screen::ConfirmCancel);
        a.pending_cancel = Some(PendingCancel::Batch {
            batch_id: 1,
            active: 1,
            pending: 2,
            cursor_name: "a.bin".into(),
        });
        let text = screen(&a);
        assert!(text.contains("cancel batch?"), "{text}");
        assert!(text.contains("cancel 3 transfers in this batch?"), "{text}");
        assert!(text.contains("(includes a.bin)"), "{text}");
        assert!(text.contains("1 active   2 queued"), "{text}");
        assert!(text.contains("[y] yes"), "{text}");
    }

    // --- credentials ---

    #[test]
    fn the_password_prompt_masks_what_is_typed() {
        let mut a = over_selector(Screen::PasswordPrompt);
        a.pending_session = Some(session("sftp://me@h.example"));
        a.password_input.push_str("hunter2");
        let text = screen(&a);
        assert!(text.contains("me@h.example:22 (sftp)"), "{text}");
        assert!(text.contains("password: •••••••█"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn the_passphrase_prompt_names_the_key_and_masks_what_is_typed() {
        let mut a = over_selector(Screen::KeyPassphrasePrompt);
        let mut s = session("sftp://me@h.example");
        s.auth = AuthMethod::Key {
            path: "/home/me/.ssh/id_ed25519".into(),
        };
        a.pending_session = Some(s);
        a.passphrase_input.push_str("sesame");
        a.passphrase_error = Some("wrong passphrase".into());
        let text = screen(&a);
        assert!(text.contains("key: /home/me/.ssh/id_ed25519"), "{text}");
        assert!(text.contains("passphrase: ••••••█"), "{text}");
        assert!(!text.contains("sesame"), "{text}");
        assert!(text.contains("✗ wrong passphrase"), "{text}");
    }

    #[test]
    fn the_passphrase_prompt_without_a_key_path_says_key() {
        let mut a = over_selector(Screen::KeyPassphrasePrompt);
        a.pending_session = Some(session("sftp://me@h.example"));
        let text = screen(&a);
        assert!(text.contains("ssh key passphrase"), "{text}");
        assert!(text.contains("  key\n") || text.contains(" key "), "{text}");
        assert!(!text.contains("key: "), "{text}");
    }

    // --- connecting ---

    #[test]
    fn connecting_shows_the_target() {
        let mut a = on(Screen::Connection);
        assert!(screen(&a).contains("→ …"));
        a.pending_session = Some(session("ftps://me@h.example"));
        let text = screen(&a);
        assert!(text.contains("connecting"), "{text}");
        assert!(text.contains("→ me@h.example:21 via ftps"), "{text}");
    }

    #[test]
    fn the_new_session_modal_echoes_input_and_shows_errors() {
        let mut a = over_selector(Screen::NewSession);
        a.new_session_input = "sftp://typed".into();
        let text = screen(&a);
        assert!(text.contains("new connection"), "{text}");
        assert!(text.contains("sftp://typed█"), "{text}");
        assert!(!text.contains('✗'), "{text}");

        a.new_session_error = Some("missing user".into());
        assert!(screen(&a).contains("✗ missing user"));
    }

    // --- post-connect offers ---

    #[test]
    fn the_save_offer_names_the_connection() {
        let mut a = on(Screen::OfferSaveSession);
        assert!(screen(&a).contains("this connection"));
        a.current_session = Some(session("sftp://me@h.example:2200"));
        let text = screen(&a);
        assert!(text.contains("save this session?"), "{text}");
        assert!(text.contains("me@h.example:2200"), "{text}");
        assert!(text.contains("[y] save"), "{text}");
    }

    fn resume_offer(remaining: usize, samples: &[&str]) -> CheckpointOffer {
        CheckpointOffer {
            kind: CheckpointKind::Download,
            session: "work".into(),
            origin: "me@h.example:22".into(),
            remaining,
            total: remaining + 5,
            age: Some(std::time::Duration::from_secs(2 * 3600)),
            sample_paths: samples.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn the_resume_offer_summarises_the_batch() {
        let mut a = on(Screen::OfferResumeCheckpoint);
        queue_offer(
            &mut a,
            PostConnectOffer::ResumeCheckpoint(resume_offer(10, &["/r/a", "/r/b", "/r/c"])),
        );
        let text = screen(&a);
        assert!(text.contains("resume interrupted transfer?"), "{text}");
        assert!(
            text.contains("an interrupted download batch was found"),
            "{text}"
        );
        assert!(
            text.contains("10 of 15 items remaining · 2 hours ago"),
            "{text}"
        );
        for p in ["/r/a", "/r/b", "/r/c"] {
            assert!(text.contains(p), "{text}");
        }
        assert!(text.contains("… and 7 more"), "{text}");
    }

    #[test]
    fn the_resume_offer_omits_more_when_every_path_is_shown() {
        let mut a = on(Screen::OfferResumeCheckpoint);
        let mut offer = resume_offer(1, &["/r/only"]);
        offer.age = None;
        queue_offer(&mut a, PostConnectOffer::ResumeCheckpoint(offer));
        let text = screen(&a);
        assert!(
            text.contains("1 of 6 items remaining\n") || !text.contains(" ago"),
            "{text}"
        );
        assert!(!text.contains("more"), "{text}");
    }

    /// The panel's sizing comment promises this: the destructive `[d]` hint
    /// is the line that clips first, and it must survive 80x24.
    #[test]
    fn the_resume_offers_discard_hint_fits_on_an_80x24_terminal() {
        let mut a = on(Screen::OfferResumeCheckpoint);
        queue_offer(
            &mut a,
            PostConnectOffer::ResumeCheckpoint(resume_offer(10, &["/r/a", "/r/b", "/r/c"])),
        );
        let text = small_screen(&a);
        assert!(text.contains("[d] discard"), "{text}");
    }

    #[test]
    fn the_resume_offer_draws_nothing_without_an_offer() {
        let a = on(Screen::OfferResumeCheckpoint);
        assert!(!screen(&a).contains("resume interrupted transfer?"));
    }

    // --- remote file operations ---

    #[test]
    fn save_session_shows_target_name_and_both_dirs() {
        let mut a = on(Screen::SaveSession);
        a.current_session = Some(session("sftp://me@h.example"));
        a.save_session_input = "work".into();
        a.remote.path = String::new();
        let text = screen(&a);
        assert!(text.contains("save session"), "{text}");
        assert!(text.contains("me@h.example:22 (sftp)"), "{text}");
        assert!(text.contains("name: work█"), "{text}");
        assert!(text.contains("local: /l"), "{text}");
        assert!(text.contains("remote: /"), "{text}");

        a.remote.path = "/srv".into();
        a.save_session_error = Some("name taken".into());
        let text = screen(&a);
        assert!(text.contains("remote: /srv"), "{text}");
        assert!(text.contains("✗ name taken"), "{text}");
    }

    #[test]
    fn rename_shows_from_and_to() {
        let mut a = on(Screen::Rename);
        a.rename_original = "old.txt".into();
        a.rename_input = "new.txt".into();
        let text = screen(&a);
        assert!(text.contains("from: old.txt"), "{text}");
        assert!(text.contains("to: new.txt█"), "{text}");

        a.rename_error = Some("bad name".into());
        assert!(screen(&a).contains("✗ bad name"));
    }

    #[test]
    fn mkdir_shows_where_and_the_name() {
        let mut a = on(Screen::Mkdir);
        a.remote.path = String::new();
        a.mkdir_input = "photos".into();
        let text = screen(&a);
        assert!(text.contains("new directory"), "{text}");
        assert!(
            text.contains("in: /\n") || text.contains("in: / "),
            "{text}"
        );
        assert!(text.contains("name: photos█"), "{text}");

        a.remote.path = "/srv/www".into();
        a.mkdir_error = Some("exists".into());
        let text = screen(&a);
        assert!(text.contains("in: /srv/www"), "{text}");
        assert!(text.contains("✗ exists"), "{text}");
    }

    #[test]
    fn delete_says_whether_a_folder_takes_its_contents() {
        let mut a = on(Screen::ConfirmDelete);
        a.pending_delete = Some(PendingDelete {
            name: "notes.txt".into(),
            is_dir: false,
            remote_path: "/r/notes.txt".into(),
        });
        let text = screen(&a);
        assert!(text.contains("delete this file:"), "{text}");
        assert!(text.contains("notes.txt"), "{text}");
        assert!(text.contains("this cannot be undone."), "{text}");

        a.pending_delete = Some(PendingDelete {
            name: "photos".into(),
            is_dir: true,
            remote_path: "/r/photos".into(),
        });
        let text = screen(&a);
        assert!(
            text.contains("delete this folder and everything inside it:"),
            "{text}"
        );
    }

    // --- overwrite ---

    #[test]
    fn a_rename_overwrite_offers_no_skip() {
        let mut a = on(Screen::ConfirmOverwrite);
        a.pending_overwrite = Some(OverwritePending::Rename {
            from: "/r/a".into(),
            to: "/r/b".into(),
            target_name: "b".into(),
        });
        let text = screen(&a);
        assert!(text.contains("overwrite?"), "{text}");
        assert!(
            text.contains("a file or folder with this name already exists:"),
            "{text}"
        );
        assert!(text.contains("renaming will replace it."), "{text}");
        assert!(text.contains("[y] overwrite"), "{text}");
        assert!(!text.contains("[s]"), "{text}");
    }

    fn downloads(n: usize) -> Vec<PlannedJob> {
        let mut plan = vec![PlannedJob::Mkdir {
            remote_path: "/r/d".into(),
        }];
        plan.extend((0..n).map(|i| PlannedJob::Download {
            remote_path: format!("/r/d/f{i}.txt"),
            local_path: format!("/l/d/f{i}.txt").into(),
        }));
        plan
    }

    #[test]
    fn a_download_plan_lists_six_conflicts_and_counts_the_rest() {
        let mut a = on(Screen::ConfirmOverwrite);
        // Index 0 is the mkdir, which must not count toward the file total.
        a.pending_overwrite = Some(OverwritePending::DownloadPlan {
            plan: downloads(10),
            conflict_indices: (1..=8).collect(),
            local_dirs: Vec::new(),
        });
        let text = screen(&a);
        assert!(
            text.contains("8 of 10 target files already exist on the local side:"),
            "{text}"
        );
        for i in 0..6 {
            assert!(text.contains(&format!("• f{i}.txt")), "{text}");
        }
        assert!(!text.contains("f6.txt"), "{text}");
        assert!(text.contains("+ 2 more"), "{text}");
        assert!(
            text.contains("replaces all conflicting files locally."),
            "{text}"
        );
        assert!(text.contains("[s] skip conflicts"), "{text}");
    }

    #[test]
    fn an_upload_plan_with_one_conflict_reads_in_the_singular() {
        let mut a = on(Screen::ConfirmOverwrite);
        a.pending_overwrite = Some(OverwritePending::UploadPlan {
            plan: vec![
                PlannedJob::Upload {
                    local_path: "/l/a.txt".into(),
                    remote_path: "/r/a.txt".into(),
                },
                PlannedJob::Upload {
                    local_path: "/l/b.txt".into(),
                    remote_path: "/r/b.txt".into(),
                },
            ],
            conflict_indices: vec![1],
        });
        let text = screen(&a);
        assert!(
            text.contains("1 of 2 target file already exists on the remote side:"),
            "{text}"
        );
        assert!(text.contains("• b.txt"), "{text}");
        assert!(!text.contains("more"), "{text}");
        assert!(text.contains("on the server."), "{text}");
    }

    // --- session editing ---

    fn edit_form(a: &mut App) -> &mut EditSessionForm {
        let mut s = session("ftps://me@h.example");
        s.name = "work".into();
        a.edit_session_form = Some(EditSessionForm::from_session(&s));
        a.sessions = vec![s];
        a.edit_session_form.as_mut().unwrap()
    }

    #[test]
    fn edit_session_shows_every_field_and_the_fixed_ones() {
        let mut a = over_selector(Screen::EditSession);
        let form = edit_form(&mut a);
        form.focused = EditField::Host;
        form.parallel = String::new();
        let global = a.config.general.parallel_downloads;

        let text = screen(&a);
        assert!(text.contains("edit session"), "{text}");
        assert!(text.contains("name:       work "), "{text}");
        assert!(text.contains("host:       h.example█"), "{text}");
        assert!(text.contains("port:       21"), "{text}");
        assert!(
            text.contains(&format!("blank = use {global} (global)")),
            "{text}"
        );
        assert!(text.contains("tls verify: [ ]"), "{text}");
        assert!(!text.contains("disables TLS protections"), "{text}");
        assert!(text.contains("protocol:   ftps"), "{text}");
        assert!(text.contains("auth:       password"), "{text}");
    }

    #[test]
    fn edit_session_warns_when_certificate_checks_are_off() {
        let mut a = over_selector(Screen::EditSession);
        let form = edit_form(&mut a);
        form.accept_invalid_certs = true;
        form.focused = EditField::AcceptInvalidCerts;
        form.parallel = "3".into();
        form.error = Some("bad port".into());

        let text = screen(&a);
        assert!(text.contains("tls verify: [x] ◀"), "{text}");
        assert!(text.contains("this disables TLS protections"), "{text}");
        assert!(text.contains("parallel:   3"), "{text}");
        assert!(!text.contains("blank = use"), "{text}");
        assert!(text.contains("✗ bad port"), "{text}");
    }

    #[test]
    fn deleting_a_session_says_no_remote_files_are_touched() {
        let mut a = over_selector(Screen::ConfirmDeleteSession);
        let mut s = session("sftp://me@h.example");
        s.name = "work".into();
        a.pending_session_delete = Some(s);
        let text = screen(&a);
        assert!(text.contains("delete session?"), "{text}");
        assert!(text.contains("work"), "{text}");
        assert!(text.contains("me@h.example:22  [sftp]"), "{text}");
        assert!(text.contains("no remote files are touched."), "{text}");
    }

    // --- search ---

    #[test]
    fn the_search_bar_counts_matches_without_the_parent_entry() {
        let mut a = on(Screen::Search);
        a.search_target = Pane::Local;
        a.search_input = "file".into();
        let mut entries = vec![PaneEntry::parent()];
        entries.extend((0..3).map(|i| PaneEntry::new(format!("file{i}"), false, 1)));
        a.local.set_entries(entries);
        let text = screen(&a);
        assert!(text.contains("/local: file█"), "{text}");
        assert!(text.contains("3 matches"), "{text}");

        a.search_target = Pane::Remote;
        a.remote.set_entries(vec![
            PaneEntry::parent(),
            PaneEntry::new("x".into(), false, 1),
        ]);
        let text = screen(&a);
        assert!(text.contains("/remote: file█"), "{text}");
        assert!(text.contains("1 match "), "{text}");
    }
}
