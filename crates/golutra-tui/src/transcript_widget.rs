//! Ratatui rendering primitives for transcript view models.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::ListItem,
};

use super::{TranscriptItem, TranscriptRole, TuiApp, transcript_items};

pub(crate) fn transcript_rows(app: &TuiApp) -> Vec<ListItem<'static>> {
    transcript_items(app)
        .into_iter()
        .flat_map(transcript_list_items)
        .collect()
}

pub(crate) fn transcript_list_items(item: TranscriptItem) -> Vec<ListItem<'static>> {
    let color = role_color(&item.role);
    let mut rows = vec![ListItem::new(Line::from(vec![
        Span::styled(role_marker(&item.role), Style::default().fg(color)),
        Span::styled(
            item.title,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ]))];
    rows.extend(item.body.into_iter().map(|line| {
        ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(line, Style::default().fg(Color::White)),
        ]))
    }));
    rows.push(ListItem::new(Line::from("")));
    rows
}

pub(crate) fn role_marker(role: &TranscriptRole) -> &'static str {
    match role {
        TranscriptRole::User => "› ",
        TranscriptRole::Assistant | TranscriptRole::Status | TranscriptRole::System => "• ",
    }
}

fn role_color(role: &TranscriptRole) -> Color {
    match role {
        TranscriptRole::User => Color::Cyan,
        TranscriptRole::Assistant => Color::Green,
        TranscriptRole::Status => Color::Yellow,
        TranscriptRole::System => Color::DarkGray,
    }
}
