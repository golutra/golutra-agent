//! Native terminal scrollback for finalized transcript content.

use std::{
    collections::{HashMap, HashSet},
    io,
};

use golutra_core::{EventId, SessionId};
use ratatui::{
    Terminal,
    backend::Backend,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;

use super::*;

const SESSION_PANEL_MAX_WIDTH: usize = 60;
const SESSION_PANEL_MIN_WIDTH: usize = 40;
const SESSION_OUTER_MARGIN: usize = 2;
const SESSION_LOGO_GAP: usize = 3;
const SESSION_FIELD_LABEL_WIDTH: usize = 7;
const GOLUTRA_LOGO_GLYPHS: [[&str; 6]; 7] = [
    [
        " ██████╗",
        "██╔════╝",
        "██║  ███╗",
        "██║   ██║",
        "╚██████╔╝",
        " ╚═════╝",
    ],
    [
        " ██████╗",
        "██╔═══██╗",
        "██║   ██║",
        "██║   ██║",
        "╚██████╔╝",
        " ╚═════╝",
    ],
    ["██╗", "██║", "██║", "██║", "███████╗", "╚══════╝"],
    [
        "██╗   ██╗",
        "██║   ██║",
        "██║   ██║",
        "██║   ██║",
        "╚██████╔╝",
        " ╚═════╝",
    ],
    [
        "████████╗",
        "╚══██╔══╝",
        "   ██║",
        "   ██║",
        "   ██║",
        "   ╚═╝",
    ],
    [
        "██████╗",
        "██╔══██╗",
        "██████╔╝",
        "██╔══██╗",
        "██║  ██║",
        "╚═╝  ╚═╝",
    ],
    [
        " █████╗",
        "██╔══██╗",
        "███████║",
        "██╔══██║",
        "██║  ██║",
        "╚═╝  ╚═╝",
    ],
];
const SESSION_LOGO_GRADIENT: [[u8; 3]; 3] =
    [[0x0E, 0xA5, 0xE9], [0x10, 0xB9, 0x81], [0xF5, 0x9E, 0x0B]];
const MIN_INLINE_BOTTOM_ROWS: u16 = 3;
const HISTORY_OMISSION_MARKER: &str = "…";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineHistoryMode {
    Transcript,
    Developer { expanded: bool },
    DebugSplit { expanded: bool },
}

impl InlineHistoryMode {
    fn from_app(app: &TuiApp) -> Self {
        if !app.debug_mode {
            return Self::Transcript;
        }
        match app.body_view_mode {
            BodyViewMode::Transcript => Self::Transcript,
            BodyViewMode::Developer => Self::Developer {
                expanded: app.developer_observations_expanded,
            },
            BodyViewMode::Auto | BodyViewMode::Split => Self::DebugSplit {
                expanded: app.developer_observations_expanded,
            },
        }
    }
}

#[derive(Debug, Clone)]
struct RenderedHistoryEntry {
    event_ids: Vec<EventId>,
    lines: Vec<Line<'static>>,
}

impl RenderedHistoryEntry {
    fn is_committed(&self, committed: &HashSet<EventId>) -> bool {
        self.event_ids.iter().all(|id| committed.contains(id))
    }

    fn is_partially_committed(&self, committed: &HashSet<EventId>) -> bool {
        self.event_ids.iter().any(|id| committed.contains(id)) && !self.is_committed(committed)
    }
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
        // History is inserted before the normal frame draw, while ratatui normally performs its
        // autoresize at the start of that draw. Synchronize the internal inline buffer first so
        // wrapping and insert_before use the same physical dimensions after a resize.
        terminal.autoresize()?;
        let mode = InlineHistoryMode::from_app(app);
        let buffer_area = terminal.current_buffer_mut().area;
        let width = buffer_area.width.max(1);
        let viewport_height = buffer_area.height.max(1);
        let target_changed = self.initialized
            && (self.session_id != app.session_id
                || self.generation != app.transcript.history.replay_generation
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
            self.generation = app.transcript.history.replay_generation;
            self.mode = mode;
            self.rendered_width = width;
            self.rendered_height = viewport_height;
            self.initialized = true;
            self.header_emitted = false;
            self.committed_event_ids.clear();
            app.set_inline_history_committed_event_ids(HashSet::new());
        }

        let source_ready = app.transcript.history.replay_ready
            && (!matches!(
                mode,
                InlineHistoryMode::Developer { .. } | InlineHistoryMode::DebugSplit { .. }
            ) || app.developer_projection.is_some()
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
            .flat_map(|entry| entry.event_ids.iter().copied())
            .collect::<HashSet<_>>();
        let grouping_changed = committable_entries
            .iter()
            .any(|entry| entry.is_partially_committed(&self.committed_event_ids));
        if !self.committed_event_ids.is_subset(&committable_ids) || grouping_changed {
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
            .filter(|entry| !entry.is_committed(&self.committed_event_ids))
            .cloned()
            .collect::<Vec<_>>();
        let mut lines = Vec::new();
        let emit_header = !self.header_emitted;
        if emit_header {
            lines.extend(session_history_lines(app, width));
            let fact_lines = match mode {
                InlineHistoryMode::Developer { expanded: true } => {
                    developer_fact_history_lines(app, width)
                }
                InlineHistoryMode::DebugSplit { expanded: true } => {
                    let (_, developer_width) = debug_pane_widths(width);
                    debug_split_history_lines(
                        Vec::new(),
                        developer_fact_history_lines(app, developer_width),
                        width,
                    )
                }
                _ => Vec::new(),
            };
            if !fact_lines.is_empty() {
                lines.extend(fact_lines);
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
            .extend(stable_entries.into_iter().flat_map(|entry| entry.event_ids));
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
                    event_ids: vec![entry.id],
                    lines: render_operation_projection_lines(app, vec![entry.projection], width),
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
        InlineHistoryMode::Developer { expanded } => {
            let mut events = app.events.iter().collect::<Vec<_>>();
            events.sort_by_key(|event| event.sequence_no);
            let projected = developer_event_projections(events);
            let stable_count = projected.len().saturating_sub(usize::from(
                projected
                    .last()
                    .is_some_and(DeveloperEventProjection::is_open_provider_stream),
            ));
            let rendered = projected
                .into_iter()
                .map(|event| RenderedHistoryEntry {
                    event_ids: event.event_ids.clone(),
                    lines: developer_event_history_lines(&event, width, expanded, app.palette()),
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
        InlineHistoryMode::DebugSplit { expanded } => {
            debug_split_history_entries(app, width, live_row_capacity, expanded)
        }
    }
}

fn debug_split_history_entries(
    app: &TuiApp,
    width: u16,
    live_row_capacity: u16,
    expanded: bool,
) -> Vec<RenderedHistoryEntry> {
    let operation_entries = event_operation_entries(&app.events);
    // A streaming message or running tool can still rewrite its earlier transcript row. Keep
    // that event and every later observation live until the operation reaches a stable state.
    let first_unstable_id = operation_entries
        .iter()
        .find(|entry| !entry.stable)
        .map(|entry| entry.id);
    let mut events = app.events.iter().collect::<Vec<_>>();
    events.sort_by_key(|event| event.sequence_no);
    let rendered = debug_split_event_entries(app, events, width, expanded);
    let stable_count = first_unstable_id.map_or(rendered.len(), |id| {
        rendered
            .iter()
            .position(|entry| entry.event_ids.contains(&id))
            .unwrap_or_default()
    });
    let committed_count = committed_prefix_len(
        &rendered,
        stable_count,
        width,
        usize::from(live_row_capacity),
    );
    rendered.into_iter().take(committed_count).collect()
}

fn debug_source_events(app: &TuiApp) -> Vec<&golutra_protocol::RuntimeEvent> {
    let mut events = if app.events.is_empty() {
        app.developer_projection
            .as_ref()
            .map(|projection| projection.events.iter().collect::<Vec<_>>())
            .unwrap_or_default()
    } else {
        app.events.iter().collect::<Vec<_>>()
    };
    events.sort_by_key(|event| event.sequence_no);
    events
}

fn debug_split_event_entries(
    app: &TuiApp,
    events: Vec<&golutra_protocol::RuntimeEvent>,
    width: u16,
    expanded: bool,
) -> Vec<RenderedHistoryEntry> {
    let mut operations = event_operation_entries(&app.events)
        .into_iter()
        .map(|entry| (entry.id, entry.projection))
        .collect::<HashMap<_, _>>();
    let (transcript_width, developer_width) = debug_pane_widths(width);
    developer_event_projections(events)
        .into_iter()
        .map(|event| {
            let transcript = event
                .event_ids
                .iter()
                .find_map(|event_id| operations.remove(event_id))
                .map(|projection| {
                    render_operation_projection_lines(app, vec![projection], transcript_width)
                })
                .unwrap_or_default();
            let developer =
                developer_event_history_lines(&event, developer_width, expanded, app.palette());
            RenderedHistoryEntry {
                event_ids: event.event_ids,
                lines: debug_split_history_lines(transcript, developer, width),
            }
        })
        .collect()
}

pub(crate) fn debug_split_live_lines(
    app: &TuiApp,
    width: u16,
    visible_rows: u16,
) -> Vec<Line<'static>> {
    let (transcript_width, developer_width) = debug_pane_widths(width);
    let facts = if !app.transcript.history.enabled || app.developer_error.is_some() {
        let mut facts = developer_fact_history_lines(app, developer_width);
        if facts.is_empty() && app.developer_projection.is_none() {
            facts.push(Line::from("loading developer projection"));
        }
        debug_split_history_lines(Vec::new(), facts, width)
    } else {
        Vec::new()
    };

    let mut timeline = debug_split_event_entries(
        app,
        debug_source_events(app),
        width,
        app.developer_observations_expanded,
    )
    .into_iter()
    .filter(|entry| !entry.is_committed(&app.transcript.history.committed_event_ids))
    .flat_map(|entry| entry.lines)
    .collect::<Vec<_>>();

    let live_event_operation_count = event_operation_entries(&app.events)
        .into_iter()
        .filter(|entry| {
            !app.transcript
                .history
                .committed_event_ids
                .contains(&entry.id)
        })
        .count();
    let transcript_only = rendered_transcript_operation_projections(app)
        .into_iter()
        .skip(live_event_operation_count)
        .collect::<Vec<_>>();
    timeline.extend(debug_split_history_lines(
        render_operation_projection_lines(app, transcript_only, transcript_width),
        Vec::new(),
        width,
    ));

    let capacity = usize::from(visible_rows);
    // Interactive history can rely on native scrollback. Offscreen snapshots cannot, so reserve
    // space for governance facts and prioritize user-visible transcript plus event headers.
    let fact_budget = if timeline.is_empty() {
        capacity
    } else {
        (capacity / 3).max(1)
    };
    let fact_count = facts.len().min(fact_budget);
    let timeline_count = timeline.len().min(capacity.saturating_sub(fact_count));
    let visible_timeline = if !app.transcript.history.enabled && timeline.len() > timeline_count {
        prioritized_debug_snapshot_lines(timeline, timeline_count, debug_pane_widths(width).0)
    } else {
        timeline
            .drain(timeline.len().saturating_sub(timeline_count)..)
            .collect::<Vec<_>>()
    };
    let gap = capacity.saturating_sub(fact_count + timeline_count);
    let mut lines = facts.into_iter().take(fact_count).collect::<Vec<_>>();
    lines.extend(std::iter::repeat_with(|| Line::from(" ".repeat(usize::from(width)))).take(gap));
    lines.extend(visible_timeline);
    lines
}

fn prioritized_debug_snapshot_lines(
    timeline: Vec<Line<'static>>,
    capacity: usize,
    transcript_width: u16,
) -> Vec<Line<'static>> {
    if timeline.len() <= capacity {
        return timeline;
    }

    let transcript_rows = timeline
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            debug_line_has_content_in_range(line, 0, transcript_width).then_some(index)
        })
        .collect::<Vec<_>>();
    let observation_rows = timeline
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            debug_line_has_content_in_range(line, transcript_width, u16::MAX).then_some(index)
        })
        .collect::<Vec<_>>();
    let observation_headers = observation_rows
        .iter()
        .copied()
        .filter(|index| {
            timeline[*index]
                .spans
                .iter()
                .any(|span| span.content.trim_start().starts_with('#'))
        })
        .collect::<Vec<_>>();
    let transcript_budget = capacity.saturating_sub(usize::from(!observation_rows.is_empty()));
    let mut selected = HashSet::with_capacity(capacity);
    if transcript_rows.len() <= transcript_budget {
        selected.extend(transcript_rows);
    } else {
        let head_count = transcript_budget / 2;
        let tail_count = transcript_budget.saturating_sub(head_count);
        selected.extend(transcript_rows.iter().take(head_count).copied());
        selected.extend(transcript_rows.iter().rev().take(tail_count).copied());
    }
    for index in observation_headers
        .into_iter()
        .rev()
        .chain(observation_rows.into_iter().rev())
    {
        if selected.len() >= capacity {
            break;
        }
        selected.insert(index);
    }
    for index in (0..timeline.len()).rev() {
        if selected.len() >= capacity {
            break;
        }
        selected.insert(index);
    }

    timeline
        .into_iter()
        .enumerate()
        .filter_map(|(index, line)| selected.contains(&index).then_some(line))
        .collect()
}

fn debug_line_has_content_in_range(line: &Line<'_>, start: u16, end: u16) -> bool {
    let mut column = 0_u16;
    for span in &line.spans {
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = u16::try_from(display_width(grapheme)).unwrap_or(u16::MAX);
            let grapheme_end = column.saturating_add(grapheme_width);
            if column < end && grapheme_end > start && !grapheme.chars().all(char::is_whitespace) {
                return true;
            }
            column = grapheme_end;
            if column >= end {
                return false;
            }
        }
    }
    false
}

pub(crate) fn debug_split_history_lines(
    transcript: Vec<Line<'static>>,
    developer: Vec<Line<'static>>,
    width: u16,
) -> Vec<Line<'static>> {
    if width == 0 || (transcript.is_empty() && developer.is_empty()) {
        return Vec::new();
    }
    // A one-column terminal cannot represent either half of a strict split. Keep the degraded
    // state explicit instead of silently dropping both projections.
    if width == 1 {
        return vec![Line::from(HISTORY_OMISSION_MARKER)];
    }
    let (transcript_width, developer_width) = debug_pane_widths(width);
    let transcript_rows = wrapped_history_rows(transcript, transcript_width);
    let developer_rows = wrapped_history_rows(developer, developer_width);
    let row_count = transcript_rows.len().max(developer_rows.len());
    let mut rows = Vec::with_capacity(row_count);

    // Keep the two projections on the same physical rows. This makes an observation legible as
    // the counterpart of the transcript event that produced it, while still allowing either
    // side to continue with blank rows after the other side has finished wrapping.
    for index in 0..row_count {
        let mut spans = Vec::new();
        append_debug_pane_row(&mut spans, transcript_rows.get(index), transcript_width);
        append_debug_pane_row(&mut spans, developer_rows.get(index), developer_width);
        rows.push(Line::from(spans));
    }
    rows
}

fn append_debug_pane_row(
    destination: &mut Vec<Span<'static>>,
    row: Option<&Vec<Span<'static>>>,
    width: u16,
) {
    let used = row
        .map(|spans| spans.iter().map(Span::width).sum::<usize>())
        .unwrap_or_default();
    if let Some(spans) = row {
        destination.extend(spans.iter().cloned());
    }
    let padding = usize::from(width).saturating_sub(used);
    if padding > 0 {
        destination.push(Span::raw(" ".repeat(padding)));
    }
}

const RATATUI_MAX_BUFFER_CELLS: usize = u16::MAX as usize;
const RATATUI_MAX_SCROLL_ROWS: usize = u16::MAX as usize - 1;

fn max_history_chunk_rows(width: u16) -> usize {
    let width = usize::from(width.max(1));
    (RATATUI_MAX_BUFFER_CELLS / width).clamp(1, RATATUI_MAX_SCROLL_ROWS)
}

fn wrapped_history_rows(lines: Vec<Line<'static>>, width: u16) -> Vec<Vec<Span<'static>>> {
    if lines.is_empty() || width == 0 {
        return Vec::new();
    }
    // Ratatui 0.28 indexes Buffer rectangles with u16 arithmetic. Render pane rows in bounded
    // chunks, and segment extreme logical lines so neither area nor scroll offset can overflow.
    let max_chunk_rows = max_history_chunk_rows(width);
    let mut rows = Vec::new();

    for line in lines
        .into_iter()
        .flat_map(|line| bounded_history_line_segments(line, width))
    {
        let row_count = Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(width)
            .max(1);
        let mut offset = 0_usize;
        while offset < row_count {
            let chunk_rows = (row_count - offset).min(max_chunk_rows);
            let height = u16::try_from(chunk_rows).expect("debug history chunk is u16-bounded");
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);
            Paragraph::new(line.clone())
                .wrap(Wrap { trim: false })
                .scroll((ratatui_vertical_scroll(offset, height), 0))
                .render(area, &mut buffer);
            rows.extend((0..height).map(|row| styled_buffer_row(&buffer, row, width)));
            offset = offset.saturating_add(chunk_rows);
        }
    }

    rows
}

fn bounded_history_line_segments(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let line_style = line.style;
    let line_alignment = line.alignment;
    let mut logical_lines = vec![Line {
        spans: Vec::new(),
        style: line_style,
        alignment: line_alignment,
    }];

    // A provider can put literal newlines inside one styled span. Ratatui treats those as
    // physical rows, so split them before applying the cell/grapheme bound below.
    for span in line.spans {
        let mut parts = span.content.split('\n').peekable();
        while let Some(part) = parts.next() {
            if !part.is_empty() {
                append_history_span(
                    &mut logical_lines
                        .last_mut()
                        .expect("history line accumulator is non-empty")
                        .spans,
                    part,
                    span.style,
                );
            }
            if parts.peek().is_some() {
                logical_lines.push(Line {
                    spans: Vec::new(),
                    style: line_style,
                    alignment: line_alignment,
                });
            }
        }
    }

    logical_lines
        .into_iter()
        .flat_map(|line| bounded_history_cell_segments(line, width))
        .collect()
}

fn append_history_span(spans: &mut Vec<Span<'static>>, content: &str, style: Style) {
    if let Some(previous) = spans.last_mut()
        && previous.style == style
    {
        previous.content.to_mut().push_str(content);
    } else {
        spans.push(Span::styled(content.to_owned(), style));
    }
}

fn bounded_history_cell_segments(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1);
    let width_cells = usize::from(width);
    // Emit physical rows instead of one giant logical line. Ratatui's word wrapper can turn a
    // cell-bounded logical line into many rows when words do not fit together; truncating that
    // line afterward loses its prefix. Hard-splitting here keeps every grapheme and lets the
    // normal paragraph renderer consume one already-bounded row at a time.
    let max_graphemes = max_history_chunk_rows(width)
        .saturating_mul(width_cells)
        .clamp(1, RATATUI_MAX_SCROLL_ROWS);
    let mut segments = Vec::new();
    let mut spans = Vec::<Span<'static>>::new();
    let mut grapheme_count = 0_usize;
    let mut cell_count = 0_usize;
    for span in line.spans {
        for grapheme in span.content.graphemes(true) {
            let raw_width = display_width(grapheme);
            // Ratatui intentionally ignores a grapheme wider than the target pane. Replace it
            // with a visible marker so narrow debug panes have an explicit degradation.
            let (grapheme, grapheme_width) = if raw_width > usize::from(width.max(1)) {
                (HISTORY_OMISSION_MARKER, 1)
            } else {
                (grapheme, raw_width)
            };
            if !spans.is_empty()
                && (cell_count.saturating_add(grapheme_width) > width_cells
                    || grapheme_count >= max_graphemes)
            {
                segments.push(Line {
                    spans: std::mem::take(&mut spans),
                    style: line.style,
                    alignment: line.alignment,
                });
                grapheme_count = 0;
                cell_count = 0;
            }
            append_history_span(&mut spans, grapheme, span.style);
            grapheme_count = grapheme_count.saturating_add(1);
            cell_count = cell_count.saturating_add(grapheme_width);
        }
    }
    if !spans.is_empty() || segments.is_empty() {
        segments.push(Line {
            spans,
            style: line.style,
            alignment: line.alignment,
        });
    }
    segments
}

fn styled_buffer_row(buffer: &Buffer, row: u16, width: u16) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut current_style = None;
    let mut current_text = String::new();
    let mut column = 0_u16;

    while column < width {
        let Some(cell) = buffer.cell((column, row)) else {
            break;
        };
        let style = cell.style();
        if current_style.is_some_and(|current| current != style) {
            spans.push(Span::styled(
                std::mem::take(&mut current_text),
                current_style.expect("debug history row has an active style"),
            ));
        }
        current_style = Some(style);
        let symbol_width = display_width(cell.symbol());
        if symbol_width == 0 {
            current_text.push(' ');
            column = column.saturating_add(1);
        } else {
            current_text.push_str(cell.symbol());
            column = column.saturating_add(u16::try_from(symbol_width).unwrap_or(u16::MAX).max(1));
        }
    }
    if !current_text.is_empty() {
        spans.push(Span::styled(
            current_text,
            current_style.expect("debug history row has an active style"),
        ));
    }
    spans
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
        let candidate = committed_count.saturating_sub(1);
        let candidate_rows = history_lines_height(&entries[candidate].lines, width);
        // A whole operation is the smallest stable scrollback unit. Keeping an operation that is
        // taller than the live body would permanently clip its leading rows, because its event id
        // could never be marked as committed. Commit that operation in full once it stabilizes;
        // shorter operations still form the live tail immediately above the composer.
        if candidate_rows > live_row_capacity {
            break;
        }
        committed_count = candidate;
        live_rows = live_rows.saturating_add(candidate_rows);
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
            truncate_end_to_width("GOLUTRA", available),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))];
    }

    let margin_width = if available >= 16 {
        SESSION_OUTER_MARGIN
    } else {
        0
    };
    let usable_width = available.saturating_sub(margin_width.saturating_mul(2));
    let logo_width = session_logo_width();
    let show_logo = !app.preferences.screen_reader
        && usable_width
            >= logo_width
                .saturating_add(SESSION_LOGO_GAP)
                .saturating_add(SESSION_PANEL_MIN_WIDTH);
    let panel_width = if show_logo {
        usable_width
            .saturating_sub(logo_width)
            .saturating_sub(SESSION_LOGO_GAP)
            .min(SESSION_PANEL_MAX_WIDTH)
    } else {
        usable_width.min(SESSION_PANEL_MAX_WIDTH)
    };

    let model = app.runtime_controls.effective_model().trim();
    let model = if model.is_empty() {
        "unconfigured"
    } else {
        model
    };
    let directory = workspace_path_label(&app.workspace_path);
    let panel = session_panel_lines(app, panel_width, model, &directory);
    let mut lines = if show_logo {
        let gradient =
            app.preferences.theme != ColorTheme::Monochrome && !app.preferences.high_contrast;
        combine_session_logo_and_panel(
            session_logo_lines(palette, gradient),
            panel,
            margin_width,
            logo_width,
        )
    } else {
        panel
            .into_iter()
            .map(|line| prepend_session_margin(line, margin_width))
            .collect()
    };

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

fn session_logo_width() -> usize {
    GOLUTRA_LOGO_GLYPHS
        .iter()
        .map(|glyph| {
            glyph
                .iter()
                .map(|row| display_width(row))
                .max()
                .unwrap_or(0)
        })
        .sum::<usize>()
        .saturating_add(GOLUTRA_LOGO_GLYPHS.len().saturating_sub(1))
}

fn session_logo_lines(palette: TuiPalette, gradient: bool) -> Vec<Line<'static>> {
    let logo_width = session_logo_width();
    (0..GOLUTRA_LOGO_GLYPHS[0].len())
        .map(|row| {
            let mut text = String::with_capacity(logo_width);
            for (index, glyph) in GOLUTRA_LOGO_GLYPHS.iter().enumerate() {
                if index > 0 {
                    text.push(' ');
                }
                let glyph_width = glyph
                    .iter()
                    .map(|line| display_width(line))
                    .max()
                    .unwrap_or(0);
                text.push_str(glyph[row]);
                text.push_str(&" ".repeat(glyph_width.saturating_sub(display_width(glyph[row]))));
            }
            session_logo_line(text, palette, gradient, logo_width)
        })
        .collect()
}

fn session_logo_line(
    text: String,
    palette: TuiPalette,
    gradient: bool,
    logo_width: usize,
) -> Line<'static> {
    let style = Style::default().add_modifier(Modifier::BOLD);
    if !gradient {
        return Line::from(Span::styled(text, style.fg(palette.accent)));
    }

    Line::from(
        text.chars()
            .enumerate()
            .map(|(column, character)| {
                if character == ' ' {
                    Span::raw(" ")
                } else {
                    Span::styled(
                        character.to_string(),
                        style.fg(session_logo_gradient_color(column, logo_width)),
                    )
                }
            })
            .collect::<Vec<_>>(),
    )
}

fn session_logo_gradient_color(column: usize, width: usize) -> Color {
    let last = SESSION_LOGO_GRADIENT.len().saturating_sub(1);
    let denominator = width.saturating_sub(1);
    if denominator == 0 || column >= denominator {
        let [red, green, blue] = SESSION_LOGO_GRADIENT[last];
        return Color::Rgb(red, green, blue);
    }

    let scaled = column.saturating_mul(last);
    let segment = scaled / denominator;
    let numerator = scaled % denominator;
    let start = SESSION_LOGO_GRADIENT[segment];
    let end = SESSION_LOGO_GRADIENT[segment.saturating_add(1)];
    Color::Rgb(
        interpolate_logo_channel(start[0], end[0], numerator, denominator),
        interpolate_logo_channel(start[1], end[1], numerator, denominator),
        interpolate_logo_channel(start[2], end[2], numerator, denominator),
    )
}

fn interpolate_logo_channel(start: u8, end: u8, numerator: usize, denominator: usize) -> u8 {
    let start = usize::from(start);
    let end = usize::from(end);
    let value = start
        .saturating_mul(denominator.saturating_sub(numerator))
        .saturating_add(end.saturating_mul(numerator))
        .saturating_add(denominator / 2)
        / denominator;
    u8::try_from(value).unwrap_or(u8::MAX)
}

fn session_panel_lines(
    app: &TuiApp,
    panel_width: usize,
    model: &str,
    directory: &str,
) -> Vec<Line<'static>> {
    let palette = app.palette();
    let border_style = Style::default().fg(palette.subtle);
    let content_width = panel_width.saturating_sub(4);
    let value_width = content_width.saturating_sub(SESSION_FIELD_LABEL_WIDTH);
    let model_hint = "  /model";
    let show_model_hint =
        display_width(model).saturating_add(display_width(model_hint)) <= value_width;
    let model = if show_model_hint {
        model.to_owned()
    } else {
        truncate_end_to_width(model, value_width)
    };
    let directory = truncate_start_to_width(directory, value_width);
    let (guard, guard_style) = match app.runtime_controls.permission_mode {
        PermissionMode::Unrestricted => (
            "unrestricted",
            Style::default()
                .fg(palette.warning)
                .add_modifier(Modifier::BOLD),
        ),
        PermissionMode::Guarded => (
            "guarded",
            Style::default()
                .fg(palette.success)
                .add_modifier(Modifier::BOLD),
        ),
    };

    let mut engine_spans = vec![session_field_label("engine", palette)];
    engine_spans.push(Span::styled(model, Style::default().fg(palette.text)));
    if show_model_hint {
        engine_spans.push(Span::styled(
            model_hint.to_owned(),
            Style::default().fg(palette.accent),
        ));
    }

    vec![
        Line::from(Span::styled(
            format!("╭{}╮", "─".repeat(panel_width.saturating_sub(2))),
            border_style,
        )),
        session_panel_row(
            vec![
                Span::styled(
                    "GOLUTRA",
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  v{}", env!("CARGO_PKG_VERSION")),
                    Style::default().fg(palette.muted),
                ),
            ],
            content_width,
            border_style,
        ),
        session_panel_row(engine_spans, content_width, border_style),
        session_panel_row(
            vec![
                session_field_label("scope", palette),
                Span::styled(directory, Style::default().fg(palette.text)),
            ],
            content_width,
            border_style,
        ),
        session_panel_row(
            vec![
                session_field_label("guard", palette),
                Span::styled(guard, guard_style),
            ],
            content_width,
            border_style,
        ),
        Line::from(Span::styled(
            format!("╰{}╯", "─".repeat(panel_width.saturating_sub(2))),
            border_style,
        )),
    ]
}

fn session_field_label(label: &str, palette: TuiPalette) -> Span<'static> {
    Span::styled(
        format!("{label:<SESSION_FIELD_LABEL_WIDTH$}"),
        Style::default().fg(palette.muted),
    )
}

fn session_panel_row(
    spans: Vec<Span<'static>>,
    content_width: usize,
    border_style: Style,
) -> Line<'static> {
    let mut fitted = Vec::new();
    let mut remaining = content_width;
    for span in spans {
        if remaining == 0 {
            break;
        }
        let style = span.style;
        let content = span.content.into_owned();
        let content_width = display_width(&content);
        if content_width <= remaining {
            fitted.push(Span::styled(content, style));
            remaining = remaining.saturating_sub(content_width);
        } else {
            let content = truncate_end_to_width(&content, remaining);
            remaining = remaining.saturating_sub(display_width(&content));
            fitted.push(Span::styled(content, style));
            break;
        }
    }
    fitted.push(Span::raw(" ".repeat(remaining)));

    let mut row = vec![Span::styled("│ ", border_style)];
    row.extend(fitted);
    row.push(Span::styled(" │", border_style));
    Line::from(row)
}

fn combine_session_logo_and_panel(
    logo: Vec<Line<'static>>,
    panel: Vec<Line<'static>>,
    margin_width: usize,
    logo_width: usize,
) -> Vec<Line<'static>> {
    let logo_offset = panel.len().saturating_sub(logo.len()) / 2;
    panel
        .into_iter()
        .enumerate()
        .map(|(row, panel_line)| {
            let mut spans = vec![Span::raw(" ".repeat(margin_width))];
            if let Some(logo_line) = row.checked_sub(logo_offset).and_then(|row| logo.get(row)) {
                spans.extend(logo_line.spans.iter().cloned());
            } else {
                spans.push(Span::raw(" ".repeat(logo_width)));
            }
            spans.push(Span::raw(" ".repeat(SESSION_LOGO_GAP)));
            spans.extend(panel_line.spans);
            Line::from(spans)
        })
        .collect()
}

fn prepend_session_margin(line: Line<'static>, margin_width: usize) -> Line<'static> {
    let mut spans = vec![Span::raw(" ".repeat(margin_width))];
    spans.extend(line.spans);
    Line::from(spans)
}

fn insert_history_lines<B: Backend>(
    terminal: &mut Terminal<B>,
    lines: Vec<Line<'static>>,
    width: u16,
) -> io::Result<()> {
    let width = width.max(1);
    // Ratatui 0.28 clamps Buffer area to u16::MAX while indexing the full rectangle.
    let max_rows = max_history_chunk_rows(width);
    let mut batch = Vec::new();
    let mut batch_rows = 0_usize;

    for line in lines
        .into_iter()
        .flat_map(|line| bounded_history_line_segments(line, width))
    {
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
    if rows > RATATUI_MAX_SCROLL_ROWS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "one history line exceeds the terminal scroll limit",
        ));
    }
    let mut offset = 0_usize;
    while offset < rows {
        let chunk_rows = (rows - offset).min(max_rows);
        let height = u16::try_from(chunk_rows)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "history chunk is too tall"))?;
        let line = line.clone();
        terminal.insert_before(height, move |buffer| {
            Paragraph::new(line)
                .wrap(Wrap { trim: false })
                .scroll((ratatui_vertical_scroll(offset, height), 0))
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

#[cfg(test)]
mod boundary_tests {
    use super::*;

    #[test]
    fn debug_history_does_not_truncate_beyond_the_ratatui_buffer_boundary() {
        let pane_width = 1_u16;
        let expected_rows = usize::from(u16::MAX) + 1;
        let content = "x".repeat(expected_rows * usize::from(pane_width));
        let rows = wrapped_history_rows(vec![Line::from(content)], pane_width);

        assert_eq!(rows.len(), expected_rows);
        assert_eq!(
            rows.iter()
                .flat_map(|row| row.iter())
                .map(|span| span.content.matches('x').count())
                .sum::<usize>(),
            expected_rows * usize::from(pane_width)
        );
        assert!(rows.iter().all(|row| {
            row.iter().map(|span| span.width()).sum::<usize>() == usize::from(pane_width)
        }));
    }

    #[test]
    fn word_spaced_history_preserves_content_when_word_wrapping_would_exceed_the_row_cap() {
        let width = 80_u16;
        let unit = format!("{} ", "x".repeat(41));
        let content = format!("{}TAIL", unit.repeat(1_558));
        assert_eq!(content.len(), 65_440);
        let expected_x_count = content.matches('x').count();

        let rows = wrapped_history_rows(vec![Line::from(content)], width);
        assert!(rows.len() <= max_history_chunk_rows(width));
        let rendered = rows
            .iter()
            .flat_map(|row| row.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!rendered.contains(HISTORY_OMISSION_MARKER));
        assert_eq!(rendered.matches('x').count(), expected_x_count);
        assert!(rendered.contains("TAIL"));
    }

    #[test]
    fn debug_history_splits_embedded_newlines_before_rendering() {
        let pane_width = 8_u16;
        let newline_count = 70_000;
        let content = "x\n".repeat(newline_count);
        let rows = wrapped_history_rows(vec![Line::from(vec![Span::raw(content)])], pane_width);

        assert_eq!(
            rows.iter()
                .flat_map(|row| row.iter())
                .map(|span| span.content.matches('x').count())
                .sum::<usize>(),
            newline_count
        );
        assert!(rows.len() >= newline_count);
        assert!(rows.iter().all(|row| {
            row.iter().map(|span| span.width()).sum::<usize>() == usize::from(pane_width)
        }));
    }

    #[test]
    fn debug_split_content_measurement_ignores_zero_width_joiners() {
        let line = Line::from("x\u{200d}");
        assert!(!debug_line_has_content_in_range(&line, 1, 2));
        assert!(debug_line_has_content_in_range(&line, 0, 1));

        let emoji = Line::from("👩\u{200d}💻");
        assert!(debug_line_has_content_in_range(&emoji, 0, 1));
        assert!(debug_line_has_content_in_range(&emoji, 1, 2));
    }
}
