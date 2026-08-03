//! Ratatui rendering primitives for transcript view models.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use super::{
    OperationId, TranscriptItem, TranscriptRole, TuiApp, transcript_operation_projections,
    transcript_visible_window,
};

#[derive(Debug, Clone)]
pub(crate) struct TranscriptRenderRow {
    pub(crate) line: Line<'static>,
    pub(crate) operation_id: Option<OperationId>,
    pub(crate) toggle: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TranscriptLayout {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) row_count: usize,
    rows: Vec<TranscriptRowLayout>,
}

#[derive(Debug, Clone)]
struct TranscriptRowLayout {
    start: usize,
    end: usize,
    operation_id: Option<OperationId>,
    toggle: bool,
}

impl TranscriptLayout {
    pub(crate) fn visible_window(
        &self,
        visible_rows: usize,
        offset_from_bottom: usize,
    ) -> std::ops::Range<usize> {
        transcript_visible_window(self.row_count, visible_rows, offset_from_bottom)
    }
}

pub(crate) fn transcript_layout(app: &TuiApp, area: Rect) -> TranscriptLayout {
    let rendered = transcript_render_rows(app);
    let mut lines = Vec::with_capacity(rendered.len());
    let mut rows = Vec::with_capacity(rendered.len());
    let mut row_count = 0_usize;

    for row in rendered {
        let visual_rows = wrapped_line_count(&row.line, area.width);
        let start = row_count;
        row_count = row_count.saturating_add(visual_rows);
        rows.push(TranscriptRowLayout {
            start,
            end: row_count,
            operation_id: row.operation_id,
            toggle: row.toggle,
        });
        lines.push(row.line);
    }

    TranscriptLayout {
        lines,
        row_count,
        rows,
    }
}

pub(crate) fn transcript_render_rows(app: &TuiApp) -> Vec<TranscriptRenderRow> {
    transcript_operation_projections(app)
        .into_iter()
        .flat_map(|projection| {
            let expanded = app.transcript_details_expanded
                || projection
                    .id()
                    .is_some_and(|id| app.expanded_operations.contains(id));
            let operation_id = projection.id().cloned();
            let toggle = projection.is_expandable();
            let item = projection.item(expanded);
            render_item_rows(item, operation_id, toggle, expanded)
        })
        .collect()
}

pub(crate) fn transcript_toggle_at(
    app: &TuiApp,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<OperationId> {
    if column < area.x || column >= area.x.saturating_add(4) || row <= area.y {
        return None;
    }
    let layout = transcript_layout(app, area);
    let visible_rows = area.height.saturating_sub(1) as usize;
    let window = layout.visible_window(visible_rows, app.transcript_scroll.offset_from_bottom);
    let offset = usize::from(row.saturating_sub(area.y + 1));
    let visual_row = window.start.saturating_add(offset);
    layout
        .rows
        .iter()
        .find(|rendered| {
            rendered.toggle && visual_row >= rendered.start && visual_row < rendered.end
        })
        .and_then(|rendered| rendered.operation_id.clone())
}

pub(crate) fn transcript_toggle_regions(app: &TuiApp, area: Rect) -> Vec<(String, Rect)> {
    if area.width == 0 || area.height < 2 {
        return Vec::new();
    }
    let layout = transcript_layout(app, area);
    let visible_rows = area.height.saturating_sub(1) as usize;
    let window = layout.visible_window(visible_rows, app.transcript_scroll.offset_from_bottom);
    layout
        .rows
        .iter()
        .filter_map(|rendered| {
            let operation_id = rendered.operation_id.as_ref()?;
            let start = rendered.start.max(window.start);
            let end = rendered.end.min(window.end);
            if !rendered.toggle || start >= end {
                return None;
            }
            let row_offset = start.saturating_sub(window.start);
            let y = area
                .y
                .saturating_add(1)
                .saturating_add(u16::try_from(row_offset).unwrap_or(u16::MAX));
            Some((
                format!("transcript_operation_toggle:{}", operation_id.as_str()),
                Rect::new(
                    area.x,
                    y,
                    area.width.min(4),
                    u16::try_from(end.saturating_sub(start)).unwrap_or(u16::MAX),
                ),
            ))
        })
        .collect()
}

fn render_item_rows(
    item: TranscriptItem,
    operation_id: Option<OperationId>,
    toggle: bool,
    expanded: bool,
) -> Vec<TranscriptRenderRow> {
    let color = role_color(&item.role);
    let marker = if toggle {
        if expanded { "▾ " } else { "▸ " }
    } else {
        role_marker(&item.role)
    };
    let mut rows = vec![TranscriptRenderRow {
        line: Line::from(vec![
            Span::styled(marker, Style::default().fg(color)),
            Span::styled(
                item.title.clone(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ]),
        operation_id: operation_id.clone(),
        toggle,
    }];
    rows.extend(item.body.into_iter().map(|line| TranscriptRenderRow {
        line: Line::from(vec![
            Span::raw("  "),
            Span::styled(line, Style::default().fg(Color::White)),
        ]),
        operation_id: None,
        toggle: false,
    }));
    rows.push(TranscriptRenderRow {
        line: Line::from(""),
        operation_id: None,
        toggle: false,
    });
    rows
}

fn wrapped_line_count(line: &Line<'static>, width: u16) -> usize {
    if width == 0 {
        return 0;
    }
    Paragraph::new(line.clone())
        .wrap(Wrap { trim: false })
        .line_count(width)
        .max(1)
}

pub(crate) fn role_marker(role: &TranscriptRole) -> &'static str {
    match role {
        TranscriptRole::User => "› ",
        TranscriptRole::Assistant
        | TranscriptRole::Status
        | TranscriptRole::Activity
        | TranscriptRole::Success
        | TranscriptRole::Warning
        | TranscriptRole::Error
        | TranscriptRole::System => "• ",
    }
}

fn role_color(role: &TranscriptRole) -> Color {
    match role {
        TranscriptRole::User => Color::Cyan,
        TranscriptRole::Assistant => Color::Green,
        TranscriptRole::Status => Color::Yellow,
        TranscriptRole::Activity => Color::Cyan,
        TranscriptRole::Success => Color::Green,
        TranscriptRole::Warning => Color::Yellow,
        TranscriptRole::Error => Color::Red,
        TranscriptRole::System => Color::DarkGray,
    }
}
