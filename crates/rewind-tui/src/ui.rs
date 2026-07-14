use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use rewind_domain::{EventPayload, RunStatus, SnapshotEntry, SnapshotEntryKind};
use rewind_snapshot::EntryChange;

use crate::model::{
    App, Focus, WorkspaceState, entry_kind, event_detail, event_name, format_duration, format_exit,
};

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

pub(crate) fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .split(area);
    draw_header(frame, rows[0], app);
    if area.width >= 110 && area.height >= 28 {
        draw_dashboard(frame, rows[1], app);
    } else {
        draw_compact(frame, rows[1], app);
    }
    draw_footer(frame, rows[2], app);
    if app.show_help {
        draw_help(frame, area);
    }
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let id = app.run.id.to_string();
    let duration = app.run.monotonic_duration.map_or_else(
        || "recording".to_owned(),
        |value| format_duration(value.as_nanoseconds()),
    );
    let mut line = vec![
        Span::styled(" REWIND ", Style::default().fg(Color::Black).bg(ACCENT)),
        Span::raw("  "),
        Span::styled(&id[..12], Style::default().fg(Color::White)),
        Span::raw("  "),
        Span::styled(
            safe_text(&app.run.command),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(app.run.status.as_str(), status_style(app.run.status)),
        Span::raw("  "),
        Span::styled(duration, Style::default().fg(MUTED)),
    ];
    if let Some(status) = &app.status {
        line.push(Span::raw("  "));
        line.push(Span::styled(
            safe_text(status),
            Style::default().fg(Color::Yellow),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(line)).block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

fn draw_dashboard(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(27),
            Constraint::Percentage(47),
            Constraint::Percentage(26),
        ])
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
        .split(columns[0]);
    let center = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(61), Constraint::Percentage(39)])
        .split(columns[1]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(columns[2]);
    draw_runs(frame, left[0], app);
    draw_timeline(frame, left[1], app);
    draw_terminal(frame, center[0], app);
    draw_workspace(frame, center[1], app);
    draw_processes(frame, right[0], app);
    draw_details(frame, right[1], app);
}

fn draw_compact(frame: &mut Frame<'_>, area: Rect, app: &App) {
    match app.focus {
        Focus::Runs => draw_runs(frame, area, app),
        Focus::Timeline => draw_timeline(frame, area, app),
        Focus::Terminal => draw_terminal(frame, area, app),
        Focus::Workspace => draw_workspace(frame, area, app),
        Focus::Processes => draw_processes(frame, area, app),
        Focus::Details => draw_details(frame, area, app),
    }
}

fn draw_runs(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let selected = app
        .run_rows
        .iter()
        .position(|row| row.run_id == app.run.id)
        .unwrap_or(0);
    let (start, end) = visible_window(app.run_rows.len(), selected, area.height.saturating_sub(2));
    let items = app.run_rows[start..end]
        .iter()
        .map(|row| {
            let id = row.run_id.to_string();
            let connector = if row.depth == 0 { "" } else { "└─" };
            let indent = "  ".repeat(row.depth.min(8));
            let parent = row.parent.map_or_else(String::new, |(parent, checkpoint)| {
                format!(
                    " ← {}@{}",
                    &parent.to_string()[..8],
                    &checkpoint.to_string()[..8]
                )
            });
            let duration = row
                .duration_ns
                .map_or_else(|| "active".to_owned(), format_duration);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{indent}{connector}"), Style::default().fg(MUTED)),
                Span::styled(id[..8].to_owned(), Style::default().fg(Color::White)),
                Span::raw(" "),
                Span::styled(row.status.as_str(), status_style(row.status)),
                Span::raw(" "),
                Span::raw(safe_text(&row.command)),
                Span::styled(format!(" {duration}"), Style::default().fg(MUTED)),
                Span::styled(parent, Style::default().fg(MUTED)),
            ]))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(Some(selected.saturating_sub(start)));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::default().add_modifier(Modifier::BOLD))
            .block(panel("Run tree", app.focus == Focus::Runs)),
        area,
        &mut state,
    );
}

fn draw_timeline(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let title = app.page_range().map_or_else(
        || "Timeline · empty".to_owned(),
        |(first, last, more)| {
            format!(
                "Timeline · {}–{}{}",
                first,
                last,
                if more { "+" } else { "" }
            )
        },
    );
    let items = app
        .events
        .iter()
        .map(|event| {
            let marker = match &event.payload {
                EventPayload::CheckpointCommitted { .. } => "◆",
                EventPayload::MarkerCreated { .. } => "●",
                EventPayload::RecorderWarning { .. } | EventPayload::CheckpointFailed { .. } => "!",
                _ => "·",
            };
            let mut spans = vec![
                Span::styled(format!("{marker} "), marker_style(marker)),
                Span::styled(format!("{:>7}", event.sequence), Style::default().fg(MUTED)),
                Span::raw(" "),
                Span::raw(event_name(&event.payload)),
                Span::raw(" "),
                Span::styled(
                    format_duration(event.monotonic_offset.as_nanoseconds()),
                    Style::default().fg(MUTED),
                ),
            ];
            if let Some(checkpoint) = app.checkpoint_at(event.sequence) {
                let label = checkpoint
                    .label
                    .as_deref()
                    .map_or_else(|| checkpoint.reason.to_string(), safe_text);
                spans.push(Span::styled(
                    format!(" · {label}"),
                    Style::default().fg(ACCENT),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(if items.is_empty() {
        None
    } else {
        Some(app.event_selected)
    });
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::default().bg(Color::DarkGray))
            .block(panel(&title, app.focus == Focus::Timeline)),
        area,
        &mut state,
    );
}

fn draw_terminal(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let available = usize::from(area.height.saturating_sub(2));
    let start = app.terminal.lines.len().saturating_sub(available);
    let lines = app.terminal.lines[start..]
        .iter()
        .map(|line| {
            Line::from(
                line.spans
                    .iter()
                    .map(|span| Span::styled(span.text.as_str(), span.style))
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();
    let state = if app.playing { "playing" } else { "paused" };
    let truncated = if app.terminal.truncated {
        " · tail"
    } else {
        ""
    };
    let title = format!("Terminal · {state} · {}{truncated}", app.speed_label());
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel(&title, app.focus == Focus::Terminal)),
        area,
    );
}

fn draw_workspace(frame: &mut Frame<'_>, area: Rect, app: &App) {
    match &app.workspace {
        WorkspaceState::Unloaded { checkpoint } => {
            let target = checkpoint.map_or_else(
                || "initial state".to_owned(),
                |id| format!("checkpoint {}", &id.to_string()[..12]),
            );
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(target, Style::default().fg(Color::White))),
                    Line::from(""),
                    Line::from("Press Enter to load the snapshot diff."),
                    Line::from(Span::styled(
                        "Snapshots are loaded only on demand.",
                        Style::default().fg(MUTED),
                    )),
                ])
                .wrap(Wrap { trim: false })
                .block(panel("Workspace", app.focus == Focus::Workspace)),
                area,
            );
        }
        WorkspaceState::Unavailable { message, .. } => frame.render_widget(
            Paragraph::new(safe_text(message))
                .style(Style::default().fg(Color::Yellow))
                .block(panel(
                    "Workspace · unavailable",
                    app.focus == Focus::Workspace,
                )),
            area,
        ),
        WorkspaceState::Loaded {
            snapshot_id,
            changes,
            total_changes,
            selected,
            preview,
            preview_scroll,
            ..
        } => {
            let title = format!(
                "Workspace · {} change{} · {}…",
                total_changes,
                if *total_changes == 1 { "" } else { "s" },
                &snapshot_id.to_string()[..10]
            );
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
                .split(area);
            let (start, end) = visible_window(
                changes.len(),
                *selected,
                sections[0].height.saturating_sub(2),
            );
            let items = changes[start..end]
                .iter()
                .map(change_line)
                .collect::<Vec<_>>();
            let mut state = ListState::default().with_selected(if items.is_empty() {
                None
            } else {
                Some(selected.saturating_sub(start))
            });
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_symbol("› ")
                    .highlight_style(Style::default().bg(Color::DarkGray))
                    .block(panel(&title, app.focus == Focus::Workspace)),
                sections[0],
                &mut state,
            );
            let details = changes.get(*selected).map_or_else(
                || vec![Line::from("No workspace changes")],
                |change| change_details(change, preview),
            );
            frame.render_widget(
                Paragraph::new(details)
                    .wrap(Wrap { trim: false })
                    .scroll((*preview_scroll, 0))
                    .block(Block::default().borders(Borders::ALL).title(
                        if changes.len() < *total_changes {
                            "Entry · first 10,000 changes"
                        } else {
                            "Entry"
                        },
                    )),
                sections[1],
            );
        }
    }
}

fn draw_processes(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(area);
    let processes = app.visible_processes();
    let selected = app.process_selected.min(processes.len().saturating_sub(1));
    let (start, end) = visible_window(
        processes.len(),
        selected,
        sections[0].height.saturating_sub(2),
    );
    let sequence = app.selected_sequence().map_or(0, |value| value.get());
    let items = processes[start..end]
        .iter()
        .map(|record| {
            let exited = record
                .exit
                .is_some_and(|(exit_sequence, _)| exit_sequence.get() <= sequence);
            let parent = record
                .process
                .parent_process_id
                .map_or_else(|| "-".to_owned(), |value| value.to_string());
            ListItem::new(Line::from(vec![
                Span::styled(
                    if exited { "■ " } else { "● " },
                    Style::default().fg(if exited { MUTED } else { Color::Green }),
                ),
                Span::styled(
                    format!("{:<7}", record.process.process_id),
                    Style::default().fg(Color::White),
                ),
                Span::raw(safe_text(&record.process.command)),
                Span::styled(format!("  ppid {parent}"), Style::default().fg(MUTED)),
            ]))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(if items.is_empty() {
        None
    } else {
        Some(selected.saturating_sub(start))
    });
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::default().bg(Color::DarkGray))
            .block(panel(
                "Processes · observed pages",
                app.focus == Focus::Processes,
            )),
        sections[0],
        &mut state,
    );
    let detail = app.selected_process().map_or_else(
        || {
            vec![Line::from(Span::styled(
                "No process observations",
                Style::default().fg(MUTED),
            ))]
        },
        |record| {
            let mut lines = vec![
                Line::from(format!("pid        {}", record.process.process_id)),
                Line::from(format!(
                    "parent     {}",
                    record
                        .process
                        .parent_process_id
                        .map_or_else(|| "unavailable".to_owned(), |value| value.to_string())
                )),
                Line::from(format!("command    {}", safe_text(&record.process.command))),
            ];
            if let Some(executable) = &record.process.executable {
                lines.push(Line::from(format!("executable {}", safe_text(executable))));
            }
            if let Some((exit_sequence, status)) = record.exit
                && exit_sequence.get() <= sequence
            {
                lines.push(Line::from(format!("exit       {}", format_exit(status))));
            }
            lines
        },
    );
    frame.render_widget(
        Paragraph::new(detail).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Process details"),
        ),
        sections[1],
    );
}

fn draw_details(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(area);
    let event_lines = app.selected_event().map_or_else(
        || vec![Line::from("No recorded events")],
        |event| {
            let mut lines = vec![Line::from(Span::styled(
                event_name(&event.payload),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))];
            lines.extend(
                event_detail(event)
                    .into_iter()
                    .map(|line| Line::from(safe_text(&line))),
            );
            lines
        },
    );
    frame.render_widget(
        Paragraph::new(event_lines)
            .wrap(Wrap { trim: false })
            .block(panel("Event details", app.focus == Focus::Details)),
        sections[0],
    );

    let mut warning_lines = app
        .warnings
        .iter()
        .rev()
        .take(8)
        .map(|warning| {
            Line::from(vec![
                Span::styled(
                    format!("{} ", safe_text(&warning.code)),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(safe_text(&warning.message)),
            ])
        })
        .collect::<Vec<_>>();
    if warning_lines.is_empty() {
        warning_lines.push(Line::from(Span::styled(
            "No store warnings",
            Style::default().fg(MUTED),
        )));
    } else if app.warnings_truncated() {
        warning_lines.push(Line::from(Span::styled(
            "Showing newest 1,024 warnings",
            Style::default().fg(MUTED),
        )));
    }
    frame.render_widget(
        Paragraph::new(warning_lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("Warnings")),
        sections[1],
    );
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let line = Line::from(vec![
        Span::styled(" Tab ", key_style()),
        Span::raw(format!("{}  ", app.focus.title())),
        Span::styled("←/→", key_style()),
        Span::raw(" event  "),
        Span::styled("[ / ]", key_style()),
        Span::raw(" checkpoint  "),
        Span::styled("Space", key_style()),
        Span::raw(" play/pause  "),
        Span::styled("-/+", key_style()),
        Span::raw(" speed  "),
        Span::styled("?", key_style()),
        Span::raw(" help  "),
        Span::styled("q", key_style()),
        Span::raw(" quit"),
    ]);
    frame.render_widget(
        Paragraph::new(line)
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::Gray)),
        area,
    );
}

fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered_rect(72, 20, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "Playback",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from("  Space          play / pause"),
            Line::from("  ← / →          previous / next event"),
            Line::from("  [ / ]          previous / next checkpoint"),
            Line::from("  - / +          slower / faster"),
            Line::from(""),
            Line::from(Span::styled(
                "Navigation",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from("  Tab / Shift-Tab  cycle panes"),
            Line::from("  ↑ / ↓ or k / j   move selection"),
            Line::from("  PageUp / PageDown scroll workspace diff"),
            Line::from("  Enter             load workspace diff"),
            Line::from("  ?                 close this help"),
            Line::from("  q / Esc / Ctrl-C  quit"),
            Line::from(""),
            Line::from(Span::styled(
                "Sequence numbers define order; timestamps are presentation metadata.",
                Style::default().fg(MUTED),
            )),
        ])
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(" Key bindings "),
        ),
        popup,
    );
}

fn panel<'a>(title: &'a str, focused: bool) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} "))
        .border_style(Style::default().fg(if focused { ACCENT } else { MUTED }))
}

fn status_style(status: RunStatus) -> Style {
    let color = match status {
        RunStatus::Preparing | RunStatus::Running => Color::Blue,
        RunStatus::Completed => Color::Green,
        RunStatus::Failed | RunStatus::Crashed => Color::Red,
        RunStatus::Interrupted => Color::Yellow,
    };
    Style::default().fg(color)
}

fn marker_style(marker: &str) -> Style {
    Style::default().fg(match marker {
        "◆" => ACCENT,
        "●" => Color::Magenta,
        "!" => Color::Yellow,
        _ => MUTED,
    })
}

fn key_style() -> Style {
    Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

fn change_line(change: &EntryChange) -> ListItem<'static> {
    let (symbol, color) = match change {
        EntryChange::Added { .. } => ("A", Color::Green),
        EntryChange::Removed { .. } => ("D", Color::Red),
        EntryChange::Modified { .. } => ("M", Color::Yellow),
    };
    ListItem::new(Line::from(vec![
        Span::styled(format!("{symbol} "), Style::default().fg(color)),
        Span::raw(safe_text(change.path().as_str())),
    ]))
}

fn change_details(change: &EntryChange, preview: &[String]) -> Vec<Line<'static>> {
    let mut lines = match change {
        EntryChange::Added { entry } => {
            let mut lines = vec![Line::from(Span::styled(
                "added",
                Style::default().fg(Color::Green),
            ))];
            lines.extend(entry_details(entry, "+"));
            lines
        }
        EntryChange::Removed { entry } => {
            let mut lines = vec![Line::from(Span::styled(
                "deleted",
                Style::default().fg(Color::Red),
            ))];
            lines.extend(entry_details(entry, "-"));
            lines
        }
        EntryChange::Modified { before, after } => {
            let mut lines = vec![Line::from(Span::styled(
                "modified",
                Style::default().fg(Color::Yellow),
            ))];
            lines.push(Line::from(format!(
                "kind       {} → {}",
                entry_kind(&before.kind),
                entry_kind(&after.kind)
            )));
            lines.push(Line::from(format!(
                "mode       {:04o} → {:04o}",
                before.permissions.bits(),
                after.permissions.bits()
            )));
            lines.extend(file_identity_change(&before.kind, &after.kind));
            lines
        }
    };
    if !preview.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Unified diff",
            Style::default().fg(ACCENT),
        )));
        lines.extend(preview.iter().map(|line| {
            let color =
                if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
                    ACCENT
                } else if line.starts_with('+') {
                    Color::Green
                } else if line.starts_with('-') {
                    Color::Red
                } else {
                    Color::White
                };
            Line::from(Span::styled(safe_text(line), Style::default().fg(color)))
        }));
    }
    lines
}

fn entry_details(entry: &SnapshotEntry, prefix: &str) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(format!("kind       {prefix} {}", entry_kind(&entry.kind))),
        Line::from(format!(
            "mode       {prefix} {:04o}",
            entry.permissions.bits()
        )),
    ];
    match &entry.kind {
        SnapshotEntryKind::File {
            object_id,
            size,
            executable,
        } => {
            lines.push(Line::from(format!("bytes      {prefix} {size}")));
            lines.push(Line::from(format!(
                "object     {prefix} {}…",
                &object_id.to_string()[..16]
            )));
            lines.push(Line::from(format!("executable {prefix} {executable}")));
        }
        SnapshotEntryKind::Symlink { target } => {
            lines.push(Line::from(format!(
                "target     {prefix} {}",
                safe_text(target)
            )));
        }
        SnapshotEntryKind::Directory => {}
    }
    lines
}

fn file_identity_change(
    before: &SnapshotEntryKind,
    after: &SnapshotEntryKind,
) -> Vec<Line<'static>> {
    match (before, after) {
        (
            SnapshotEntryKind::File {
                object_id: before_id,
                size: before_size,
                executable: before_executable,
            },
            SnapshotEntryKind::File {
                object_id: after_id,
                size: after_size,
                executable: after_executable,
            },
        ) => vec![
            Line::from(format!("bytes      {before_size} → {after_size}")),
            Line::from(format!(
                "object     {}… → {}…",
                &before_id.to_string()[..12],
                &after_id.to_string()[..12]
            )),
            Line::from(format!(
                "executable {before_executable} → {after_executable}"
            )),
        ],
        (
            SnapshotEntryKind::Symlink { target: before },
            SnapshotEntryKind::Symlink { target: after },
        ) => {
            vec![Line::from(format!(
                "target     {} → {}",
                safe_text(before),
                safe_text(after)
            ))]
        }
        _ => Vec::new(),
    }
}

fn visible_window(len: usize, selected: usize, height: u16) -> (usize, usize) {
    let height = usize::from(height).max(1);
    let selected = selected.min(len.saturating_sub(1));
    let start = selected
        .saturating_sub(height / 2)
        .min(len.saturating_sub(height));
    (start, (start + height).min(len))
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn safe_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_text_cannot_emit_terminal_controls() {
        assert_eq!(safe_text("ok\x1b[31m\n"), "ok\u{fffd}[31m\u{fffd}");
    }

    #[test]
    fn visible_window_is_bounded_and_keeps_selection_visible() {
        assert_eq!(visible_window(1_000, 500, 20), (490, 510));
        assert_eq!(visible_window(3, 2, 20), (0, 3));
        assert_eq!(visible_window(0, 0, 20), (0, 0));
    }
}
