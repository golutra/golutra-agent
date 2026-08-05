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
        InlineHistoryMode::Developer { expanded } => {
            let mut events = app.events.iter().collect::<Vec<_>>();
            events.sort_by_key(|event| event.sequence_no);
            let rendered = events
                .into_iter()
                .map(|event| RenderedHistoryEntry {
                    id: event.id,
                    lines: developer_event_history_lines(event, width, expanded, app.palette()),
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
    let stable_count = first_unstable_id.map_or(events.len(), |id| {
        events
            .iter()
            .position(|event| event.id == id)
            .unwrap_or_default()
    });
    let rendered = debug_split_event_entries(app, events, width, expanded);
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
    let (_, developer_width) = debug_pane_widths(width);
    events
        .into_iter()
        .map(|event| {
            let transcript = operations
                .remove(&event.id)
                .map(|projection| render_operation_projection_lines(app, vec![projection]))
                .unwrap_or_default();
            let developer =
                developer_event_history_lines(event, developer_width, expanded, app.palette());
            RenderedHistoryEntry {
                id: event.id,
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
    let (_, developer_width) = debug_pane_widths(width);
    let facts = if !app.inline_history_enabled || app.developer_error.is_some() {
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
    .filter(|entry| !app.inline_history_committed_event_ids.contains(&entry.id))
    .flat_map(|entry| entry.lines)
    .collect::<Vec<_>>();

    let live_event_operation_count = event_operation_entries(&app.events)
        .into_iter()
        .filter(|entry| !app.inline_history_committed_event_ids.contains(&entry.id))
        .count();
    let transcript_only = rendered_transcript_operation_projections(app)
        .into_iter()
        .skip(live_event_operation_count)
        .collect::<Vec<_>>();
    timeline.extend(debug_split_history_lines(
        render_operation_projection_lines(app, transcript_only),
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
    let visible_timeline = if !app.inline_history_enabled && timeline.len() > timeline_count {
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
        for character in span.content.chars() {
            let character_width = u16::try_from(display_width(&character.to_string()))
                .unwrap_or(u16::MAX)
                .max(1);
            let character_end = column.saturating_add(character_width);
            if column < end && character_end > start && !character.is_whitespace() {
                return true;
            }
            column = character_end;
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
    let (transcript_width, developer_width) = debug_pane_widths(width);
    let transcript_rows = wrapped_history_rows(transcript, transcript_width);
    let developer_rows = wrapped_history_rows(developer, developer_width);
    let mut rows = Vec::with_capacity(transcript_rows.len() + developer_rows.len());

    rows.extend(transcript_rows.into_iter().map(|mut transcript| {
        transcript.push(Span::raw(" ".repeat(usize::from(developer_width))));
        Line::from(transcript)
    }));
    rows.extend(developer_rows.into_iter().map(|developer| {
        let mut spans = vec![Span::raw(" ".repeat(usize::from(transcript_width)))];
        spans.extend(developer);
        Line::from(spans)
    }));
    rows
}

fn wrapped_history_rows(lines: Vec<Line<'static>>, width: u16) -> Vec<Vec<Span<'static>>> {
    if lines.is_empty() || width == 0 {
        return Vec::new();
    }
    let row_count = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(width)
        .max(1)
        .min(usize::from(u16::MAX));
    // Ratatui 0.28 indexes Buffer rectangles with u16 arithmetic. Render pane rows in bounded
    // chunks so width * height never crosses the representable buffer area.
    let max_chunk_rows = usize::from(u16::MAX / width).max(1);
    let mut rows = Vec::with_capacity(row_count);
    let mut offset = 0_usize;

    while offset < row_count {
        let chunk_rows = (row_count - offset).min(max_chunk_rows);
        let height = u16::try_from(chunk_rows).expect("debug history chunk is u16-bounded");
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .scroll((
                u16::try_from(offset).expect("debug history height is u16-bounded"),
                0,
            ))
            .render(area, &mut buffer);
        rows.extend((0..height).map(|row| styled_buffer_row(&buffer, row, width)));
        offset = offset.saturating_add(chunk_rows);
    }

    rows
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
