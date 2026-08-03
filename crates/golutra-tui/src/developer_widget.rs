//! Ratatui widget for the developer projection.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use super::*;

const DEVELOPER_TITLE_PREFIX: &str = "Developer runtime  ";
const DEVELOPER_FACTS_COLLAPSED: &str = "▸ facts";
const DEVELOPER_FACTS_EXPANDED: &str = "▾ facts";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DeveloperEventLayout {
    pub(crate) row_count: usize,
    pub(crate) page_rows: usize,
    content_width: u16,
    expanded: bool,
    event_starts: Vec<(u64, usize)>,
}

impl DeveloperEventLayout {
    pub(crate) fn visible_window(
        &self,
        offset_from_bottom: usize,
        top_row_override: Option<usize>,
    ) -> std::ops::Range<usize> {
        if self.row_count == 0 || self.page_rows == 0 {
            return 0..0;
        }
        let normal =
            transcript_visible_window(self.row_count, self.page_rows.max(1), offset_from_bottom);
        let start = top_row_override
            .unwrap_or(normal.start)
            .min(self.row_count.saturating_sub(1));
        start..start.saturating_add(self.page_rows).min(self.row_count)
    }

    pub(crate) fn first_visible_sequence(
        &self,
        offset_from_bottom: usize,
        top_row_override: Option<usize>,
    ) -> Option<u64> {
        let row = self
            .visible_window(offset_from_bottom, top_row_override)
            .start;
        self.event_starts
            .iter()
            .rev()
            .find(|(_, start)| *start <= row)
            .map(|(sequence_no, _)| *sequence_no)
    }

    pub(crate) fn row_for_sequence(&self, sequence_no: u64) -> Option<usize> {
        self.event_starts
            .iter()
            .find(|(candidate, _)| *candidate == sequence_no)
            .map(|(_, start)| *start)
    }

    pub(crate) fn has_same_flow_as(&self, other: &Self) -> bool {
        self.content_width == other.content_width
            && self.page_rows == other.page_rows
            && self.expanded == other.expanded
    }
}

pub(crate) fn developer_event_layout(app: &TuiApp, area: Rect) -> DeveloperEventLayout {
    let content_width = area.width.saturating_sub(2);
    let visible_rows = area.height.saturating_sub(1) as usize;
    let rows = developer_rows(app);
    let event_count = rows
        .iter()
        .filter(|row| matches!(row, DeveloperPanelRow::Event { .. }))
        .count();
    let summary_count = visible_summary_count(app, &rows, visible_rows, event_count);
    let page_rows = visible_rows.saturating_sub(summary_count);
    let mut row_count = 0_usize;
    let mut event_starts = Vec::with_capacity(event_count);

    for row in rows {
        let DeveloperPanelRow::Event {
            sequence_no,
            label,
            summary,
        } = row
        else {
            continue;
        };
        event_starts.push((sequence_no, row_count));
        let lines = event_lines(
            sequence_no,
            &label,
            &summary,
            usize::from(content_width),
            app.developer_facts_expanded,
        );
        row_count = row_count.saturating_add(event_line_count(
            lines,
            content_width,
            app.developer_facts_expanded,
        ));
    }

    DeveloperEventLayout {
        row_count,
        page_rows,
        content_width,
        expanded: app.developer_facts_expanded,
        event_starts,
    }
}

pub(crate) fn developer_event_page_rows(app: &TuiApp, area: Rect) -> usize {
    developer_event_layout(app, area).page_rows.max(1)
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
    let content_width = area.width.saturating_sub(2);
    let visible_rows = area.height.saturating_sub(1) as usize;
    let rows = developer_rows(app);
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
                Style::default().fg(Color::Cyan),
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
            ),
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
    frame.render_widget(
        Block::default()
            .title(title)
            .borders(Borders::TOP | Borders::LEFT),
        area,
    );

    let content = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        content_width,
        area.height.saturating_sub(1),
    );
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
    let layout = &app.developer_event_layout;
    let window = layout.visible_window(
        app.developer_scroll.offset_from_bottom,
        app.developer_top_row_override,
    );
    let mut events =
        Paragraph::new(event_lines).scroll((u16::try_from(window.start).unwrap_or(u16::MAX), 0));
    if app.developer_facts_expanded {
        events = events.wrap(Wrap { trim: false });
    }
    frame.render_widget(events, event_area);
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
        Span::styled(sequence, Style::default().fg(Color::DarkGray)),
        Span::styled(label.to_owned(), Style::default().fg(Color::Yellow)),
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

fn event_line_count(lines: Vec<Line<'static>>, content_width: u16, expanded: bool) -> usize {
    let mut paragraph = Paragraph::new(lines);
    if expanded {
        paragraph = paragraph.wrap(Wrap { trim: false });
    }
    paragraph.line_count(content_width).max(1)
}
