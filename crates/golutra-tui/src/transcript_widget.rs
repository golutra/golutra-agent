//! Ratatui rendering primitives for transcript view models.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use super::{
    OperationId, TranscriptItem, TranscriptRole, TuiApp, detail_line, markdown_lines,
    rendered_transcript_operation_projections, transcript_operation_projections,
    transcript_visible_window,
};

#[derive(Debug, Clone)]
pub(crate) struct TranscriptRenderRow {
    pub(crate) line: Line<'static>,
    pub(crate) operation_id: Option<OperationId>,
    pub(crate) toggle: bool,
    projection_index: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TranscriptLayout {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) row_count: usize,
    rows: Vec<TranscriptRowLayout>,
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptLayoutCache {
    pub(crate) revision: u64,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) layout: TranscriptLayout,
}

#[derive(Debug, Clone)]
struct TranscriptRowLayout {
    start: usize,
    end: usize,
    operation_id: Option<OperationId>,
    toggle: bool,
    projection_index: usize,
}

impl TranscriptLayout {
    pub(crate) fn visible_window(
        &self,
        visible_rows: usize,
        offset_from_bottom: usize,
        top_row_override: Option<usize>,
    ) -> std::ops::Range<usize> {
        if let Some(top_row) = top_row_override.filter(|_| self.row_count > 0) {
            let start = top_row.min(self.row_count.saturating_sub(1));
            return start..start.saturating_add(visible_rows).min(self.row_count);
        }
        transcript_visible_window(self.row_count, visible_rows, offset_from_bottom)
    }

    pub(crate) fn first_visible_projection(&self, visual: std::ops::Range<usize>) -> Option<usize> {
        self.rows
            .iter()
            .find(|row| row.end > visual.start)
            .map(|row| row.projection_index)
    }

    pub(crate) fn visual_start_for_projection(&self, projection_index: usize) -> Option<usize> {
        self.rows
            .iter()
            .find(|row| row.projection_index == projection_index)
            .map(|row| row.start)
    }

    pub(crate) fn visual_range_for_projection(
        &self,
        projection_index: usize,
    ) -> Option<std::ops::Range<usize>> {
        let start = self
            .rows
            .iter()
            .find(|row| row.projection_index == projection_index)?
            .start;
        let end = self
            .rows
            .iter()
            .rev()
            .find(|row| row.projection_index == projection_index)?
            .end;
        Some(start..end)
    }

    pub(crate) fn first_visible_row_anchor(
        &self,
        visual: std::ops::Range<usize>,
    ) -> Option<(usize, usize)> {
        self.rows
            .iter()
            .enumerate()
            .find(|(_, row)| row.end > visual.start)
            .map(|(row_index, row)| (row_index, visual.start.saturating_sub(row.start)))
    }

    pub(crate) fn visual_row_for_row_anchor(
        &self,
        row_index: usize,
        offset: usize,
    ) -> Option<usize> {
        self.rows.get(row_index).map(|row| {
            row.start
                .saturating_add(offset.min(row.end.saturating_sub(row.start).saturating_sub(1)))
        })
    }

    pub(crate) fn plain_lines(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    pub(crate) fn plain_text(&self) -> String {
        self.plain_lines().join("\n")
    }

    pub(crate) fn visual_start_for_line(&self, line: usize) -> Option<usize> {
        self.rows.get(line).map(|row| row.start)
    }

    pub(crate) fn logical_window(
        &self,
        visual: std::ops::Range<usize>,
    ) -> (std::ops::Range<usize>, usize) {
        if visual.is_empty() || self.rows.is_empty() {
            return (0..0, 0);
        }
        let start = self
            .rows
            .iter()
            .position(|row| row.end > visual.start)
            .unwrap_or(self.rows.len());
        let end = self
            .rows
            .iter()
            .rposition(|row| row.start < visual.end)
            .map_or(start, |index| index.saturating_add(1));
        let local_scroll = self
            .rows
            .get(start)
            .map_or(0, |row| visual.start.saturating_sub(row.start));
        (start..end, local_scroll)
    }
}

pub(crate) fn transcript_layout(app: &TuiApp, area: Rect) -> TranscriptLayout {
    transcript_layout_from_rows(transcript_render_rows(app), area)
}

pub(crate) fn full_transcript_layout(app: &TuiApp, area: Rect) -> TranscriptLayout {
    transcript_layout_from_rows(
        render_operation_projections(app, transcript_operation_projections(app)),
        area,
    )
}

fn transcript_layout_from_rows(rendered: Vec<TranscriptRenderRow>, area: Rect) -> TranscriptLayout {
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
            projection_index: row.projection_index,
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
    render_operation_projections(app, rendered_transcript_operation_projections(app))
}

pub(crate) fn transcript_top_padding(app: &TuiApp, layout: &TranscriptLayout, area: Rect) -> u16 {
    if !app.inline_history_enabled
        || !app.transcript_scroll.follow_tail
        || app.transcript_top_row_override.is_some()
        || app.transcript_search.is_some()
    {
        return 0;
    }
    area.height.saturating_sub(
        u16::try_from(layout.row_count)
            .unwrap_or(u16::MAX)
            .min(area.height),
    )
}

pub(crate) fn render_operation_projection_lines(
    app: &TuiApp,
    projections: Vec<super::OperationProjection>,
) -> Vec<Line<'static>> {
    projections
        .into_iter()
        .enumerate()
        .flat_map(|(projection_index, projection)| {
            let operation_id = projection.id().cloned();
            let toggle = projection.is_expandable();
            render_item_rows(
                app,
                projection.item(false),
                operation_id,
                toggle,
                false,
                projection_index,
            )
        })
        .map(|row| row.line)
        .collect()
}

fn render_operation_projections(
    app: &TuiApp,
    projections: Vec<super::OperationProjection>,
) -> Vec<TranscriptRenderRow> {
    projections
        .into_iter()
        .enumerate()
        .flat_map(|(projection_index, projection)| {
            let expanded = app.transcript_details_expanded
                || projection
                    .id()
                    .is_some_and(|id| app.expanded_operations.contains(id));
            let operation_id = projection.id().cloned();
            let toggle = projection.is_expandable();
            let item = projection.item(expanded);
            render_item_rows(app, item, operation_id, toggle, expanded, projection_index)
        })
        .collect()
}

pub(crate) fn transcript_toggle_at(
    app: &TuiApp,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<OperationId> {
    if column < area.x || column >= area.x.saturating_add(4) || row < area.y || row >= area.bottom()
    {
        return None;
    }
    let layout = transcript_layout(app, area);
    let top_padding = transcript_top_padding(app, &layout, area);
    let content_top = area.y.saturating_add(top_padding);
    if row < content_top {
        return None;
    }
    let visible_rows = area.height as usize;
    let window = layout.visible_window(
        visible_rows,
        app.transcript_scroll.offset_from_bottom,
        app.transcript_top_row_override,
    );
    let offset = usize::from(row.saturating_sub(content_top));
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
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    let layout = transcript_layout(app, area);
    let top_padding = transcript_top_padding(app, &layout, area);
    let visible_rows = area.height as usize;
    let window = layout.visible_window(
        visible_rows,
        app.transcript_scroll.offset_from_bottom,
        app.transcript_top_row_override,
    );
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
                .saturating_add(top_padding)
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
    app: &TuiApp,
    item: TranscriptItem,
    operation_id: Option<OperationId>,
    toggle: bool,
    expanded: bool,
    projection_index: usize,
) -> Vec<TranscriptRenderRow> {
    let palette = app.palette();
    let color = role_color(app, &item.role);
    let marker = if toggle {
        match (app.preferences.screen_reader, expanded) {
            (true, true) => "v ",
            (true, false) => "> ",
            (false, true) => "▾ ",
            (false, false) => "▸ ",
        }
    } else {
        role_marker(app, &item.role)
    };
    let inline_body = matches!(item.role, TranscriptRole::User | TranscriptRole::Assistant);
    let mut body_lines = match item.role {
        TranscriptRole::Assistant => markdown_lines(&item.body.join("\n")),
        TranscriptRole::User => item
            .body
            .into_iter()
            .flat_map(|value| {
                value
                    .split('\n')
                    .map(|line| {
                        Line::from(Span::styled(
                            line.to_owned(),
                            Style::default().fg(palette.text),
                        ))
                    })
                    .collect::<Vec<_>>()
            })
            .collect(),
        _ => item
            .body
            .into_iter()
            .flat_map(|value| value.split('\n').map(detail_line).collect::<Vec<_>>())
            .collect(),
    };
    for line in &mut body_lines {
        for span in &mut line.spans {
            if let Some(color) = span.style.fg {
                span.style.fg = Some(palette.map_color(color));
            }
        }
    }

    let mut rows = Vec::new();
    if inline_body {
        if body_lines.is_empty() {
            rows.push(TranscriptRenderRow {
                line: Line::from(Span::styled(marker, Style::default().fg(color))),
                operation_id: operation_id.clone(),
                toggle,
                projection_index,
            });
        } else {
            for (index, mut line) in body_lines.into_iter().enumerate() {
                line.spans.insert(
                    0,
                    if index == 0 {
                        Span::styled(marker, Style::default().fg(color))
                    } else {
                        Span::raw("  ")
                    },
                );
                rows.push(TranscriptRenderRow {
                    line,
                    operation_id: if index == 0 {
                        operation_id.clone()
                    } else {
                        None
                    },
                    toggle: index == 0 && toggle,
                    projection_index,
                });
            }
        }
    } else {
        rows.push(TranscriptRenderRow {
            line: Line::from(vec![
                Span::styled(marker, Style::default().fg(color)),
                Span::styled(
                    item.title,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ]),
            operation_id: operation_id.clone(),
            toggle,
            projection_index,
        });
        rows.extend(body_lines.into_iter().map(|mut line| {
            line.spans.insert(0, Span::raw("  "));
            TranscriptRenderRow {
                line,
                operation_id: None,
                toggle: false,
                projection_index,
            }
        }));
    }
    rows.push(TranscriptRenderRow {
        line: Line::from(""),
        operation_id: None,
        toggle: false,
        projection_index,
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

pub(crate) fn role_marker(app: &TuiApp, role: &TranscriptRole) -> &'static str {
    match role {
        TranscriptRole::User if app.preferences.screen_reader => "> ",
        TranscriptRole::User => "› ",
        TranscriptRole::Assistant
        | TranscriptRole::Status
        | TranscriptRole::Activity
        | TranscriptRole::Success
        | TranscriptRole::Warning
        | TranscriptRole::Error
        | TranscriptRole::System
            if app.preferences.screen_reader =>
        {
            "* "
        }
        TranscriptRole::Assistant
        | TranscriptRole::Status
        | TranscriptRole::Activity
        | TranscriptRole::Success
        | TranscriptRole::Warning
        | TranscriptRole::Error
        | TranscriptRole::System => "• ",
    }
}

fn role_color(app: &TuiApp, role: &TranscriptRole) -> Color {
    let palette = app.palette();
    match role {
        TranscriptRole::User => palette.accent,
        TranscriptRole::Assistant => palette.success,
        TranscriptRole::Status => palette.warning,
        TranscriptRole::Activity => palette.accent,
        TranscriptRole::Success => palette.success,
        TranscriptRole::Warning => palette.warning,
        TranscriptRole::Error => palette.error,
        TranscriptRole::System => palette.muted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_window_virtualizes_a_large_transcript() {
        let lines = (0..100_000)
            .map(|index| Line::from(index.to_string()))
            .collect::<Vec<_>>();
        let rows = (0..100_000)
            .map(|index| TranscriptRowLayout {
                start: index,
                end: index + 1,
                operation_id: None,
                toggle: false,
                projection_index: index,
            })
            .collect();
        let layout = TranscriptLayout {
            lines,
            row_count: 100_000,
            rows,
        };

        let (window, local_scroll) = layout.logical_window(99_970..100_000);
        assert_eq!(window, 99_970..100_000);
        assert_eq!(window.len(), 30);
        assert_eq!(local_scroll, 0);
    }

    #[test]
    fn logical_window_starts_inside_a_wrapped_line() {
        let layout = TranscriptLayout {
            lines: vec![Line::from("wrapped"), Line::from("next")],
            row_count: 5,
            rows: vec![
                TranscriptRowLayout {
                    start: 0,
                    end: 4,
                    operation_id: None,
                    toggle: false,
                    projection_index: 0,
                },
                TranscriptRowLayout {
                    start: 4,
                    end: 5,
                    operation_id: None,
                    toggle: false,
                    projection_index: 1,
                },
            ],
        };

        let (window, local_scroll) = layout.logical_window(2..5);
        assert_eq!(window, 0..2);
        assert_eq!(local_scroll, 2);
    }
}
