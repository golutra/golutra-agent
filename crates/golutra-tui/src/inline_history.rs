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
const MIN_INLINE_BOTTOM_ROWS: u16 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineHistoryMode {
    Transcript,
    Developer,
}

impl InlineHistoryMode {
    fn from_app(app: &TuiApp) -> Self {
        if app.debug_mode && app.body_view_mode == BodyViewMode::Developer {
            Self::Developer
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
    rendered_height: u16,
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
            rendered_height: 0,
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
        mut rebuild_terminal: impl FnMut(&mut Terminal<B>) -> io::Result<()>,
    ) -> io::Result<bool> {
        let mode = InlineHistoryMode::from_app(app);
        let width = terminal.size()?.width.max(1);
        let viewport_height = terminal.current_buffer_mut().area.height.max(1);
        let target_changed = self.initialized
            && (self.session_id != app.session_id
                || self.generation != app.history_replay_generation
                || self.mode != mode
                || self.rendered_width != width
                || self.rendered_height != viewport_height);
        let clear_previous_history =
            target_changed && (self.header_emitted || !self.committed_event_ids.is_empty());

        let mut history_cleared = false;
        if clear_previous_history {
            rebuild_terminal(terminal)?;
            history_cleared = true;
        }
        if !self.initialized || target_changed {
            self.session_id = app.session_id;
            self.generation = app.history_replay_generation;
            self.mode = mode;
            self.rendered_width = width;
            self.rendered_height = viewport_height;
            self.initialized = true;
            self.header_emitted = false;
            self.committed_event_ids.clear();
            app.set_inline_history_committed_event_ids(HashSet::new());
        }

        let source_ready = app.history_replay_ready
            && (!matches!(mode, InlineHistoryMode::Developer)
                || app.developer_projection.is_some()
                || app.developer_error.is_some());
        if !source_ready {
            return Ok(history_cleared);
        }

        // Retain enough event rows to fill the largest possible live body. A larger bottom pane
        // may clip the oldest retained row, but it cannot expose padding between scrollback and
        // the live tail when the composer shrinks again.
        let live_row_capacity = viewport_height
            .saturating_sub(MIN_INLINE_BOTTOM_ROWS)
            .max(1);
        let committable_entries = rendered_history_entries(app, width, live_row_capacity, mode);
        let committable_ids = committable_entries
            .iter()
            .map(|entry| entry.id)
            .collect::<HashSet<_>>();
        if !self.committed_event_ids.is_subset(&committable_ids) {
            if !history_cleared {
                rebuild_terminal(terminal)?;
                history_cleared = true;
            }
            self.header_emitted = false;
            self.committed_event_ids.clear();
            app.set_inline_history_committed_event_ids(HashSet::new());
        }
        let stable_entries = committable_entries
            .iter()
            .filter(|entry| !self.committed_event_ids.contains(&entry.id))
            .cloned()
            .collect::<Vec<_>>();
        let mut lines = Vec::new();
        let emit_header = !self.header_emitted;
        if emit_header {
            lines.extend(session_history_lines(app, width));
            if matches!(mode, InlineHistoryMode::Developer) {
                lines.extend(developer_fact_history_lines(app, width));
                lines.push(Line::default());
            }
        }

        if !stable_entries.is_empty() {
            lines.extend(stable_entries.iter().flat_map(|entry| entry.lines.clone()));
        }

        if lines.is_empty() {
            return Ok(history_cleared);
        }

        insert_history_lines(terminal, lines, width)?;

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
    live_row_capacity: u16,
    mode: InlineHistoryMode,
) -> Vec<RenderedHistoryEntry> {
    match mode {
        InlineHistoryMode::Transcript => {
            let entries = event_operation_entries(&app.events);
            let stable_count = entries.iter().take_while(|entry| entry.stable).count();
            let rendered = entries
                .into_iter()
                .map(|entry| RenderedHistoryEntry {
                    id: entry.id,
                    lines: render_operation_projection_lines(app, vec![entry.projection]),
                })
                .collect::<Vec<_>>();
            let committed_count = committed_prefix_len(
                &rendered,
                stable_count,
                width,
                usize::from(live_row_capacity),
            );
            rendered.into_iter().take(committed_count).collect()
        }
        InlineHistoryMode::Developer => {
            let mut events = app.events.iter().collect::<Vec<_>>();
            events.sort_by_key(|event| event.sequence_no);
            let rendered = events
                .into_iter()
                .map(|event| RenderedHistoryEntry {
                    id: event.id,
                    lines: developer_event_history_lines(event, width, false, app.palette()),
                })
                .collect::<Vec<_>>();
            let committed_count = committed_prefix_len(
                &rendered,
                rendered.len(),
                width,
                usize::from(live_row_capacity),
            );
            rendered.into_iter().take(committed_count).collect()
        }
    }
}

fn committed_prefix_len(
    entries: &[RenderedHistoryEntry],
    stable_count: usize,
    width: u16,
    live_row_capacity: usize,
) -> usize {
    let stable_count = stable_count.min(entries.len());
    let mut committed_count = stable_count;
    let mut live_rows = entries[stable_count..]
        .iter()
        .map(|entry| history_lines_height(&entry.lines, width))
        .sum::<usize>();

    while committed_count > 0 && live_rows < live_row_capacity {
        committed_count = committed_count.saturating_sub(1);
        live_rows =
            live_rows.saturating_add(history_lines_height(&entries[committed_count].lines, width));
    }

    committed_count
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

fn insert_history_lines<B: Backend>(
    terminal: &mut Terminal<B>,
    lines: Vec<Line<'static>>,
    width: u16,
) -> io::Result<()> {
    let width = width.max(1);
    // Ratatui 0.28 clamps Buffer area to u16::MAX while indexing the full rectangle.
    let max_rows = usize::from(u16::MAX / width).max(1);
    let mut batch = Vec::new();
    let mut batch_rows = 0_usize;

    for line in lines {
        let line_rows = history_line_height(&line, width);
        if line_rows > max_rows {
            insert_history_batch(terminal, std::mem::take(&mut batch), batch_rows)?;
            batch_rows = 0;
            insert_tall_history_line(terminal, line, line_rows, max_rows)?;
            continue;
        }
        if batch_rows.saturating_add(line_rows) > max_rows {
            insert_history_batch(terminal, std::mem::take(&mut batch), batch_rows)?;
            batch_rows = 0;
        }
        batch_rows = batch_rows.saturating_add(line_rows);
        batch.push(line);
    }
    insert_history_batch(terminal, batch, batch_rows)
}

fn insert_history_batch<B: Backend>(
    terminal: &mut Terminal<B>,
    lines: Vec<Line<'static>>,
    rows: usize,
) -> io::Result<()> {
    if lines.is_empty() || rows == 0 {
        return Ok(());
    }
    let height = u16::try_from(rows)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "history batch is too tall"))?;
    terminal.insert_before(height, move |buffer| {
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(buffer.area, buffer);
    })
}

fn insert_tall_history_line<B: Backend>(
    terminal: &mut Terminal<B>,
    line: Line<'static>,
    rows: usize,
    max_rows: usize,
) -> io::Result<()> {
    if rows > usize::from(u16::MAX) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "one history line exceeds the terminal scroll limit",
        ));
    }
    let mut offset = 0_usize;
    while offset < rows {
        let chunk_rows = (rows - offset).min(max_rows);
        let height = u16::try_from(chunk_rows).expect("history chunk is bounded by u16::MAX");
        let scroll = u16::try_from(offset).expect("history line height was bounded by u16::MAX");
        let line = line.clone();
        terminal.insert_before(height, move |buffer| {
            Paragraph::new(line)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0))
                .render(buffer.area, buffer);
        })?;
        offset = offset.saturating_add(chunk_rows);
    }
    Ok(())
}

fn history_line_height(line: &Line<'static>, width: u16) -> usize {
    Paragraph::new(line.clone())
        .wrap(Wrap { trim: false })
        .line_count(width.max(1))
        .max(1)
}

fn history_lines_height(lines: &[Line<'static>], width: u16) -> usize {
    lines
        .iter()
        .map(|line| history_line_height(line, width))
        .sum()
}
