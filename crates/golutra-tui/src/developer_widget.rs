//! Ratatui widget for the developer projection.

use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use super::*;

pub(crate) fn developer_facts_toggle_rect(area: Rect) -> Rect {
    let width = 7.min(area.width);
    let right_padding = 2.min(area.width.saturating_sub(width));
    Rect::new(
        area.right()
            .saturating_sub(width)
            .saturating_sub(right_padding),
        area.bottom().saturating_sub(1),
        width,
        u16::from(area.height > 0),
    )
}

pub(crate) fn developer_facts_toggle_hit_rect(area: Rect) -> Rect {
    let toggle = developer_facts_toggle_rect(area);
    if toggle.width == 0 {
        return toggle;
    }
    let left = toggle.x.saturating_sub(1).max(area.x);
    let area_right = area.x.saturating_add(area.width);
    let right = toggle
        .x
        .saturating_add(toggle.width)
        .saturating_add(1)
        .min(area_right);
    Rect::new(left, toggle.y, right.saturating_sub(left), toggle.height)
}

pub(crate) fn draw_developer_panel(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let palette = app.palette();
    let content_width = area.width;
    let visible_rows = area.height as usize;
    let rows = if app.inline_history_enabled {
        developer_live_rows(app)
    } else {
        developer_rows(app)
    };
    let event_count = rows
        .iter()
        .filter(|row| matches!(row, DeveloperPanelRow::Event { .. }))
        .count();
    let summary_count = visible_summary_count(app, &rows, visible_rows, event_count);
    let summary_lines = rows
        .iter()
        .filter_map(|row| match row {
            DeveloperPanelRow::Summary(summary) => Some(Line::from(Span::styled(
                truncate_end_to_width(summary, usize::from(content_width)),
                Style::default().fg(palette.accent),
            ))),
            DeveloperPanelRow::Event { .. } => None,
        })
        .take(summary_count)
        .collect::<Vec<_>>();
    let event_lines = rows
        .into_iter()
        .flat_map(|row| match row {
            DeveloperPanelRow::Summary(_) => Vec::new(),
            DeveloperPanelRow::Event {
                sequence_no,
                label,
                summary,
            } => event_lines(
                sequence_no,
                &label,
                &summary,
                usize::from(content_width),
                app.developer_facts_expanded,
                palette,
            ),
        })
        .collect::<Vec<_>>();
    let content = area;
    if !summary_lines.is_empty() {
        frame.render_widget(
            Paragraph::new(summary_lines),
            Rect::new(
                content.x,
                content.y,
                content.width,
                u16::try_from(summary_count).unwrap_or(u16::MAX),
            ),
        );
    }

    let event_area = Rect::new(
        content.x,
        content
            .y
            .saturating_add(u16::try_from(summary_count).unwrap_or(u16::MAX)),
        content.width,
        content
            .height
            .saturating_sub(u16::try_from(summary_count).unwrap_or(u16::MAX)),
    );
    let mut events = Paragraph::new(event_lines);
    if app.developer_facts_expanded {
        events = events.wrap(Wrap { trim: false });
    }
    let event_rows = events.line_count(event_area.width.max(1));
    let scroll = event_rows.saturating_sub(usize::from(event_area.height));
    events = events.scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0));
    frame.render_widget(events, event_area);
}

fn developer_live_rows(app: &TuiApp) -> Vec<DeveloperPanelRow> {
    if let Some(error) = &app.developer_error {
        return vec![DeveloperPanelRow::Summary(format!("error {error}"))];
    }
    app.events
        .iter()
        .filter(|event| !app.inline_history_committed_event_ids.contains(&event.id))
        .map(|event| DeveloperPanelRow::Event {
            sequence_no: event.sequence_no,
            label: format!("{:?}/{:?}", event.event_type, event.source),
            summary: developer_event_summary(event),
        })
        .collect()
}

pub(crate) fn developer_fact_history_lines(app: &TuiApp, width: u16) -> Vec<Line<'static>> {
    let palette = app.palette();
    let Some(projection) = &app.developer_projection else {
        return app
            .developer_error
            .as_ref()
            .map(|error| {
                vec![Line::from(Span::styled(
                    format!("error {error}"),
                    Style::default().fg(palette.warning),
                ))]
            })
            .unwrap_or_default();
    };
    let mut projection = projection.clone();
    if !app.events.is_empty() {
        replace_debug_event_history(&mut projection, app.events.clone());
    }
    developer_panel_rows_with_changes(&projection, app.change_projection.summary(), 0)
        .into_iter()
        .filter_map(|row| match row {
            DeveloperPanelRow::Summary(summary) => Some(Line::from(Span::styled(
                truncate_end_to_width(&summary, usize::from(width)),
                Style::default().fg(palette.accent),
            ))),
            DeveloperPanelRow::Event { .. } => None,
        })
        .collect()
}

pub(crate) fn developer_event_history_lines(
    event: &golutra_protocol::RuntimeEvent,
    width: u16,
    expanded: bool,
    palette: TuiPalette,
) -> Vec<Line<'static>> {
    event_lines(
        event.sequence_no,
        &format!("{:?}/{:?}", event.event_type, event.source),
        &developer_event_summary(event),
        usize::from(width),
        expanded,
        palette,
    )
}

fn developer_rows(app: &TuiApp) -> Vec<DeveloperPanelRow> {
    if let Some(error) = &app.developer_error {
        vec![DeveloperPanelRow::Summary(format!("error {error}"))]
    } else if let Some(projection) = &app.developer_projection {
        developer_panel_rows_with_changes(projection, app.change_projection.summary(), usize::MAX)
    } else {
        vec![DeveloperPanelRow::Summary(
            "loading developer projection".to_owned(),
        )]
    }
}

fn visible_summary_count(
    app: &TuiApp,
    rows: &[DeveloperPanelRow],
    visible_rows: usize,
    event_count: usize,
) -> usize {
    if !app.developer_facts_expanded && event_count > 0 {
        return 0;
    }
    let available = if event_count > 0 {
        visible_rows.saturating_sub(1)
    } else {
        visible_rows
    };
    rows.iter()
        .filter(|row| matches!(row, DeveloperPanelRow::Summary(_)))
        .count()
        .min(available)
}

fn event_lines(
    sequence_no: u64,
    label: &str,
    summary: &str,
    content_width: usize,
    expanded: bool,
    palette: TuiPalette,
) -> Vec<Line<'static>> {
    let sequence = format!("#{sequence_no} ");
    let summary_width = content_width
        .saturating_sub(display_width(&sequence))
        .saturating_sub(display_width(label))
        .saturating_sub(2);
    let mut summaries = if expanded {
        summary
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
            .collect::<Vec<_>>()
    } else {
        vec![summary.replace(['\r', '\n'], " ")]
    };
    if summaries.is_empty() {
        summaries.push(String::new());
    }
    let first_summary = if expanded {
        summaries.remove(0)
    } else {
        truncate_end_to_width(&summaries.remove(0), summary_width)
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(sequence, Style::default().fg(palette.muted)),
        Span::styled(label.to_owned(), Style::default().fg(palette.warning)),
        Span::raw("  "),
        Span::raw(first_summary),
    ])];
    if expanded {
        lines.extend(
            summaries
                .into_iter()
                .map(|summary| Line::from(vec![Span::raw("  "), Span::raw(summary)])),
        );
    }
    lines
}
