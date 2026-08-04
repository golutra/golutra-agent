//! Native terminal scrollback for finalized transcript content.

use std::io;

use golutra_core::SessionId;
use ratatui::{
    Terminal,
    backend::Backend,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

use super::*;

const SESSION_CARD_MAX_WIDTH: usize = 60;

#[derive(Debug)]
pub(crate) struct InlineHistoryState {
    session_id: SessionId,
    header_emitted: bool,
    committed_event_projections: usize,
}

impl InlineHistoryState {
    pub(crate) fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            header_emitted: false,
            committed_event_projections: 0,
        }
    }

    pub(crate) fn flush<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        app: &mut TuiApp,
    ) -> io::Result<bool> {
        if self.session_id != app.session_id {
            self.session_id = app.session_id;
            self.header_emitted = false;
            self.committed_event_projections = 0;
            app.set_inline_history_committed_event_projections(0);
        }

        let width = terminal.size()?.width.max(1);
        let stable_count = stable_event_operation_projection_count(&app.events);
        let mut lines = Vec::new();
        let emit_header = !self.header_emitted;
        if emit_header {
            lines.extend(session_history_lines(app, width));
        }

        if stable_count > self.committed_event_projections {
            let projections = event_operation_projections(&app.events);
            lines.extend(render_operation_projection_lines(
                app,
                projections[self.committed_event_projections..stable_count].to_vec(),
            ));
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

        self.header_emitted |= emit_header;
        self.committed_event_projections = stable_count.max(self.committed_event_projections);
        app.set_inline_history_committed_event_projections(self.committed_event_projections);
        Ok(true)
    }
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
