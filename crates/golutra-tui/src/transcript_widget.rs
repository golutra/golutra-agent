//! Ratatui rendering primitives for transcript view models.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::ListItem,
};

use super::{
    OperationId, TranscriptItem, TranscriptRole, TuiApp, transcript_operation_projections,
    transcript_visible_window,
};

#[derive(Debug, Clone)]
pub(crate) struct TranscriptRenderRow {
    pub(crate) item: ListItem<'static>,
    pub(crate) operation_id: Option<OperationId>,
    pub(crate) toggle: bool,
}

pub(crate) fn transcript_rows(app: &TuiApp) -> Vec<ListItem<'static>> {
    transcript_render_rows(app)
        .into_iter()
        .map(|row| row.item)
        .collect()
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
    let rows = transcript_render_rows(app);
    let visible_rows = area.height.saturating_sub(1) as usize;
    let window = transcript_visible_window(
        rows.len(),
        visible_rows,
        app.transcript_scroll.offset_from_bottom,
    );
    let offset = usize::from(row.saturating_sub(area.y + 1));
    let index = window.start.saturating_add(offset);
    rows.get(index)
        .filter(|rendered| rendered.toggle)
        .and_then(|rendered| rendered.operation_id.clone())
}

pub(crate) fn transcript_toggle_regions(app: &TuiApp, area: Rect) -> Vec<(String, Rect)> {
    if area.width == 0 || area.height < 2 {
        return Vec::new();
    }
    let rows = transcript_render_rows(app);
    let visible_rows = area.height.saturating_sub(1) as usize;
    let window = transcript_visible_window(
        rows.len(),
        visible_rows,
        app.transcript_scroll.offset_from_bottom,
    );
    rows.iter()
        .enumerate()
        .skip(window.start)
        .take(window.len())
        .filter_map(|(index, rendered)| {
            let operation_id = rendered.operation_id.as_ref()?;
            if !rendered.toggle {
                return None;
            }
            let row_offset = index.saturating_sub(window.start);
            let y = area
                .y
                .saturating_add(1)
                .saturating_add(u16::try_from(row_offset).unwrap_or(u16::MAX));
            Some((
                format!("transcript_operation_toggle:{}", operation_id.as_str()),
                Rect::new(area.x, y, area.width.min(4), 1),
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
        item: ListItem::new(Line::from(vec![
            Span::styled(marker, Style::default().fg(color)),
            Span::styled(
                item.title.clone(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ])),
        operation_id: operation_id.clone(),
        toggle,
    }];
    rows.extend(item.body.into_iter().map(|line| TranscriptRenderRow {
        item: ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(line, Style::default().fg(Color::White)),
        ])),
        operation_id: None,
        toggle: false,
    }));
    rows.push(TranscriptRenderRow {
        item: ListItem::new(Line::from("")),
        operation_id: None,
        toggle: false,
    });
    rows
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
