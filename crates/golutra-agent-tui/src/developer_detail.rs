//! 调试事件的只读键盘详情；快照脱敏后展示，不执行工具、不影响聊天草稿或运行任务。

use super::*;
use ratatui::widgets::{Paragraph, Wrap};

#[derive(Debug)]
pub(crate) struct DeveloperDetailState {
    ids: Vec<EventId>,
    selected: usize,
    text: String,
    scroll: usize,
    page_height: usize,
    row_count: usize,
}

pub(crate) fn open(app: &mut TuiApp) {
    let events = if app.events.is_empty() {
        app.developer_projection
            .as_ref()
            .map(|projection| projection.events.as_slice())
            .unwrap_or_default()
    } else {
        &app.events
    };
    let Some(event) = events.last() else {
        app.status_message = "no recorded debug events".to_owned();
        return;
    };
    app.developer_detail = Some(DeveloperDetailState {
        ids: events.iter().map(|event| event.id).collect(),
        selected: events.len() - 1,
        text: event_text(event),
        scroll: 0,
        page_height: 1,
        row_count: 0,
    });
}

fn event_text(event: &RuntimeEvent) -> String {
    let mut value = serde_json::to_value(event).expect("serializable event");
    golutra_agent_client::redact_runtime_value(&mut value);
    let fields = developer_view::diagnostic_fields(event)
        .into_iter()
        .map(|(name, value)| format!("{name}: {}", value.as_deref().unwrap_or("not recorded")))
        .collect::<Vec<_>>()
        .join("\n");
    developer_view::safe_diagnostic_text(&format!(
        "Event #{} {:?}\nTime: {}\n{}\n\n{}\n\nRecorded event (redacted)\n{}",
        event.sequence_no,
        event.event_type,
        event.timestamp,
        fields,
        developer_event_summary(event),
        serde_json::to_string_pretty(&value).expect("serializable event")
    ))
}

pub(crate) fn handle_key(key: KeyEvent, app: &mut TuiApp) {
    if key.code == KeyCode::Esc {
        app.developer_detail = None;
        return;
    }
    let detail = app.developer_detail.as_mut().expect("event detail");
    match key.code {
        KeyCode::Left | KeyCode::Right => {
            detail.selected = if key.code == KeyCode::Left {
                detail.selected.saturating_sub(1)
            } else {
                (detail.selected + 1).min(detail.ids.len().saturating_sub(1))
            };
            let id = detail.ids[detail.selected];
            let event = app.events.iter().find(|event| event.id == id).or_else(|| {
                app.developer_projection
                    .as_ref()?
                    .events
                    .iter()
                    .find(|event| event.id == id)
            });
            detail.text = event.map(event_text).unwrap_or_else(|| {
                "Event no longer in the active window; reload /debug to read retained history."
                    .to_owned()
            });
            detail.scroll = 0;
        }
        KeyCode::Up => detail.scroll = detail.scroll.saturating_sub(1),
        KeyCode::Down => detail.scroll = detail.scroll.saturating_add(1),
        KeyCode::PageUp => detail.scroll = detail.scroll.saturating_sub(detail.page_height),
        KeyCode::PageDown => detail.scroll = detail.scroll.saturating_add(detail.page_height),
        KeyCode::Home => detail.scroll = 0,
        KeyCode::End => detail.scroll = detail.row_count.saturating_sub(detail.page_height),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::ALT) => {
            app.status_message = match copy_to_terminal_clipboard(&detail.text) {
                Ok((bytes, false)) => format!("copied {bytes} bytes"),
                Ok((bytes, true)) => format!("copied {bytes} bytes (clipboard limit reached)"),
                Err(error) => format!("copy failed: {error}"),
            };
        }
        _ => {}
    }
}

pub(crate) fn draw(frame: &mut Frame<'_>, app: &mut TuiApp) {
    let area = frame.area();
    let detail = app.developer_detail.as_mut().expect("event detail");
    let body = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(2),
    );
    let paragraph = Paragraph::new(detail.text.as_str()).wrap(Wrap { trim: false });
    detail.row_count = paragraph.line_count(body.width.max(1));
    detail.page_height = usize::from(body.height.max(1));
    detail.scroll = detail
        .scroll
        .min(detail.row_count.saturating_sub(detail.page_height));
    frame.render_widget(
        Paragraph::new(format!(
            "Debug event {}/{}",
            detail.selected + 1,
            detail.ids.len()
        )),
        Rect::new(area.x, area.y, area.width, 1),
    );
    frame.render_widget(
        paragraph.scroll((ratatui_vertical_scroll(detail.scroll, body.height), 0)),
        body,
    );
    frame.render_widget(
        Paragraph::new("Left/Right event · Up/Down/PgUp/PgDn scroll · Alt+C copy · Esc close"),
        Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_detail_redacts_secrets_and_marks_missing_fields() {
        let mut event: RuntimeEvent = serde_json::from_value(json!({
            "schema_version": golutra_agent_core::RUNTIME_EVENT_SCHEMA_VERSION,
            "id": EventId::new(), "sequence_no": 1, "session_id": SessionId::new(),
            "turn_id": null, "task_id": null, "parent_event_id": null,
            "event_type": "tool_started", "timestamp": chrono::Utc::now(),
            "source": "runtime", "payload": {}, "payload_ref": null, "durable": true
        }))
        .unwrap();
        event.payload = json!({"api_key": "private-fixture-key", "arguments": {"password": "private-fixture-password"}, "error_metadata": {"http_status": 429, "request_id": "req-fixture"}});
        let text = event_text(&event);
        assert!(!text.contains("private-fixture-key"));
        assert!(!text.contains("private-fixture-password"));
        assert!(text.contains("http_status: 429"));
        assert!(text.contains("request_id: req-fixture"));
        assert!(text.contains("elapsed_ms: not recorded"));
    }
}
