//! Ratatui widget for the developer projection.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
};

use super::*;

const DEVELOPER_TITLE_PREFIX: &str = "Developer runtime  ";
const DEVELOPER_FACTS_COLLAPSED: &str = "▸ facts";
const DEVELOPER_FACTS_EXPANDED: &str = "▾ facts";

pub(crate) fn developer_event_page_rows(app: &TuiApp, area: Rect) -> usize {
    let visible_rows = area.height.saturating_sub(1) as usize;
    let summary_rows = if app.developer_facts_expanded {
        app.developer_projection.as_ref().map_or(1, |projection| {
            developer_panel_rows_with_changes(projection, app.change_projection.summary(), 0)
                .into_iter()
                .filter(|row| matches!(row, DeveloperPanelRow::Summary(_)))
                .count()
        })
    } else {
        0
    };
    visible_rows.saturating_sub(summary_rows)
}

pub(crate) fn developer_facts_toggle_rect(area: Rect) -> Rect {
    let toggle_x = area
        .x
        .saturating_add(1)
        .saturating_add(u16::try_from(display_width(DEVELOPER_TITLE_PREFIX)).unwrap_or(u16::MAX));
    let right = area.x.saturating_add(area.width);
    Rect::new(
        toggle_x,
        area.y,
        right.saturating_sub(toggle_x).min(7),
        u16::from(area.height > 0),
    )
}

pub(crate) fn draw_developer_panel(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let content_width = usize::from(area.width.saturating_sub(2));
    let rows = if let Some(error) = &app.developer_error {
        vec![DeveloperPanelRow::Summary(format!("error {error}"))]
    } else if let Some(projection) = &app.developer_projection {
        developer_panel_rows_with_changes(projection, app.change_projection.summary(), usize::MAX)
    } else {
        vec![DeveloperPanelRow::Summary(
            "loading developer projection".to_owned(),
        )]
    };
    let (summaries, events): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .partition(|row| matches!(row, DeveloperPanelRow::Summary(_)));
    let visible_rows = area.height.saturating_sub(1) as usize;
    let visible_summaries = if app.developer_facts_expanded {
        summaries.into_iter().take(visible_rows).collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let event_rows = developer_event_page_rows(app, area);
    let window = transcript_visible_window(
        events.len(),
        event_rows,
        app.developer_scroll.offset_from_bottom,
    );
    let rows = visible_summaries.into_iter().chain(
        events
            .into_iter()
            .skip(window.start)
            .take(window.end.saturating_sub(window.start)),
    );
    let items = rows
        .map(|row| match row {
            DeveloperPanelRow::Summary(summary) => ListItem::new(Line::from(Span::styled(
                truncate_end_to_width(&summary, content_width),
                Style::default().fg(Color::Cyan),
            ))),
            DeveloperPanelRow::Event {
                sequence_no,
                label,
                summary,
            } => {
                let sequence = format!("#{sequence_no} ");
                let summary_width = content_width
                    .saturating_sub(display_width(&sequence))
                    .saturating_sub(display_width(&label))
                    .saturating_sub(2);
                ListItem::new(Line::from(vec![
                    Span::styled(sequence, Style::default().fg(Color::DarkGray)),
                    Span::styled(label, Style::default().fg(Color::Yellow)),
                    Span::styled("  ", Style::default()),
                    Span::raw(truncate_end_to_width(&summary, summary_width)),
                ]))
            }
        })
        .collect::<Vec<_>>();
    let facts_toggle = if app.developer_facts_expanded {
        DEVELOPER_FACTS_EXPANDED
    } else {
        DEVELOPER_FACTS_COLLAPSED
    };
    let title = Line::from(vec![
        Span::styled(
            DEVELOPER_TITLE_PREFIX,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            facts_toggle,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    let list = List::new(items).block(
        Block::default()
            .title(title)
            .borders(Borders::TOP | Borders::LEFT),
    );
    frame.render_widget(list, area);
}
