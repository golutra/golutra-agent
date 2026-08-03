//! Ratatui adapter for the live activity view model.

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

use super::{TuiApp, activity_status_text};

pub(crate) fn live_status_text(app: &TuiApp, width: usize) -> Option<String> {
    if app.auth_dialog.is_some() || app.resume_picker.is_some() || app.export_flow.is_some() {
        return None;
    }
    let snapshot = if app.activity_snapshot_captured {
        app.activity_snapshot
    } else {
        app.activity_projection.snapshot(
            app.projection.as_ref().map(|projection| projection.status),
            chrono::Utc::now(),
        )
    };
    snapshot.map(|snapshot| activity_status_text(snapshot, width))
}

pub(crate) fn live_status_line(app: &TuiApp, width: usize) -> Option<Line<'static>> {
    let text = live_status_text(app, width)?;
    let rest = text.strip_prefix("• ").unwrap_or(&text).to_owned();
    Some(Line::from(vec![
        Span::styled("• ", Style::default().fg(Color::Cyan)),
        Span::styled(rest, Style::default().fg(Color::DarkGray)),
    ]))
}
