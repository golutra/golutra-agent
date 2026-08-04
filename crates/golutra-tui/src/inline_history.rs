//! Native terminal scrollback for finalized transcript content.

use std::{collections::HashSet, io};

use golutra_core::{EventId, SessionId};
use ratatui::{
    Terminal,
    backend::Backend,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

use super::*;

const SESSION_CARD_MAX_WIDTH: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineHistoryMode {
    Transcript,
    Developer { facts_expanded: bool },
}

impl InlineHistoryMode {
    fn from_app(app: &TuiApp) -> Self {
        if app.debug_mode && app.body_view_mode != BodyViewMode::Transcript {
            Self::Developer {
                facts_expanded: app.developer_facts_expanded,
            }
        } else {
            Self::Transcript
        }
    }
}

#[derive(Debug, Clone)]
struct RenderedHistoryEntry {
    id: EventId,
    lines: Vec<Line<'static>>,
}

#[derive(Debug)]
pub(crate) struct InlineHistoryState {
    session_id: SessionId,
    generation: u64,
    mode: InlineHistoryMode,
    rendered_width: u16,
    initialized: bool,
    header_emitted: bool,
    committed_event_ids: HashSet<EventId>,
}

impl InlineHistoryState {
    pub(crate) fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            generation: 0,
            mode: InlineHistoryMode::Transcript,
            rendered_width: 0,
            initialized: false,
            header_emitted: false,
            committed_event_ids: HashSet::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn flush<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        app: &mut TuiApp,
    ) -> io::Result<bool> {
        self.flush_with_rebuild(terminal, app, clear_history_terminal)
    }

    pub(crate) fn flush_interactive(
        &mut self,
        terminal: &mut InteractiveTerminal,
        app: &mut TuiApp,
    ) -> io::Result<bool> {
        self.flush_with_rebuild(terminal, app, clear_inline_scrollback)
    }

    fn flush_with_rebuild<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        app: &mut TuiApp,
        rebuild_terminal: impl FnOnce(&mut Terminal<B>) -> io::Result<()>,
    ) -> io::Result<bool> {
        let mode = InlineHistoryMode::from_app(app);
        let width = terminal.size()?.width.max(1);
        let target_changed = self.initialized
            && (self.session_id != app.session_id
                || self.generation != app.history_replay_generation
                || self.mode != mode
                || self.rendered_width != width);
        let clear_previous_history =
            target_changed && (self.header_emitted || !self.committed_event_ids.is_empty());

        if clear_previous_history {
            rebuild_terminal(terminal)?;
        }
        if !self.initialized || target_changed {
            self.session_id = app.session_id;
            self.generation = app.history_replay_generation;
            self.mode = mode;
            self.rendered_width = width;
            self.initialized = true;
            self.header_emitted = false;
            self.committed_event_ids.clear();
            app.set_inline_history_committed_event_ids(HashSet::new());
        }

        let source_ready = app.history_replay_ready
            && (!matches!(mode, InlineHistoryMode::Developer { .. })
                || app.developer_projection.is_some()
                || app.developer_error.is_some());
        if !source_ready {
            return Ok(clear_previous_history);
        }

        let all_stable_entries = rendered_history_entries(app, width, mode);
        let stable_entries = all_stable_entries
            .iter()
            .filter(|entry| !self.committed_event_ids.contains(&entry.id))
            .cloned()
            .collect::<Vec<_>>();
        let mut lines = Vec::new();
        let emit_header = !self.header_emitted;
        if emit_header {
            lines.extend(session_history_lines(app, width));
            if matches!(
                mode,
                InlineHistoryMode::Developer {
                    facts_expanded: true
                }
            ) {
                lines.extend(developer_fact_history_lines(app, width));
                lines.push(Line::default());
            }
        }

        if !stable_entries.is_empty() {
            lines.extend(stable_entries.iter().flat_map(|entry| entry.lines.clone()));
        }

        if lines.is_empty() {
            return Ok(false);
        }

        let height = history_height(&lines, width);
        terminal.insert_before(height, |buffer| {
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .render(buffer.area, buffer);
        })?;

        self.header_emitted = true;
        self.committed_event_ids
            .extend(stable_entries.into_iter().map(|entry| entry.id));
        app.set_inline_history_committed_event_ids(self.committed_event_ids.clone());
        Ok(true)
    }
}

fn rendered_history_entries(
    app: &TuiApp,
    width: u16,
    mode: InlineHistoryMode,
) -> Vec<RenderedHistoryEntry> {
    match mode {
        InlineHistoryMode::Transcript => event_operation_entries(&app.events)
            .into_iter()
            .take_while(|entry| entry.stable)
            .map(|entry| RenderedHistoryEntry {
                id: entry.id,
                lines: render_operation_projection_lines(app, vec![entry.projection]),
            })
            .collect(),
        InlineHistoryMode::Developer { facts_expanded } => {
            let mut events = app.events.iter().collect::<Vec<_>>();
            events.sort_by_key(|event| event.sequence_no);
            events
                .into_iter()
                .map(|event| RenderedHistoryEntry {
                    id: event.id,
                    lines: developer_event_history_lines(
                        event,
                        width,
                        facts_expanded,
                        app.palette(),
                    ),
                })
                .collect()
        }
    }
}

#[cfg(test)]
fn clear_history_terminal<B: Backend>(terminal: &mut Terminal<B>) -> io::Result<()> {
    let size = terminal.size()?;
    terminal.set_cursor_position(ratatui::layout::Position::ORIGIN)?;
    terminal
        .backend_mut()
        .clear_region(ratatui::backend::ClearType::All)?;
    terminal.resize(ratatui::layout::Rect::new(0, 0, size.width, size.height))
}

pub(crate) fn inline_viewport_height(app: &TuiApp, width: u16, screen_height: u16) -> u16 {
    let history_rows = u16::try_from(session_history_lines(app, width).len()).unwrap_or(u16::MAX);
    let minimum = bottom_pane_height_for_width(app, width).saturating_add(1);
    screen_height
        .saturating_sub(history_rows)
        .max(minimum.min(screen_height))
        .max(1)
}

pub(crate) fn session_history_lines(app: &TuiApp, width: u16) -> Vec<Line<'static>> {
    let palette = app.palette();
    let available = usize::from(width);
    if available < 8 {
        return vec![Line::from(Span::styled(
            truncate_end_to_width("Golutra", available),
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        ))];
    }

    let card_width = available.min(SESSION_CARD_MAX_WIDTH);
    let content_width = card_width.saturating_sub(4);
    let model = app.runtime_controls.effective_model().trim();
    let model = if model.is_empty() {
        "unconfigured"
    } else {
        model
    };
    let directory = workspace_path_label(&app.workspace_path);
    let mut content = vec![
        (format!(">_ Golutra (v{})", env!("CARGO_PKG_VERSION")), true),
        (String::new(), false),
        (format!("model:     {model}   /model to change"), false),
        (format!("directory: {directory}"), false),
    ];
    if app.runtime_controls.permission_mode == PermissionMode::Unrestricted {
        content.push(("permissions: unrestricted".to_owned(), false));
    }

    let border_style = Style::default().fg(palette.muted);
    let mut lines = vec![Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(card_width.saturating_sub(2))),
        border_style,
    ))];
    lines.extend(content.into_iter().map(|(value, bold)| {
        let value = truncate_end_to_width(&value, content_width);
        let padding = " ".repeat(content_width.saturating_sub(display_width(&value)));
        let mut value_style = Style::default().fg(palette.text);
        if bold {
            value_style = value_style.add_modifier(Modifier::BOLD);
        }
        Line::from(vec![
            Span::styled("│ ", border_style),
            Span::styled(value, value_style),
            Span::raw(padding),
            Span::styled(" │", border_style),
        ])
    }));
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(card_width.saturating_sub(2))),
        border_style,
    )));
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("  Tip:", Style::default().fg(palette.accent)),
        Span::styled(
            truncate_end_to_width(
                " Use /help to view commands and interaction options.",
                available.saturating_sub(6),
            ),
            Style::default().fg(palette.muted),
        ),
    ]));
    lines.push(Line::default());
    lines
}

fn history_height(lines: &[Line<'static>], width: u16) -> u16 {
    let rows = Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width.max(1));
    u16::try_from(rows).unwrap_or(u16::MAX).max(1)
}
