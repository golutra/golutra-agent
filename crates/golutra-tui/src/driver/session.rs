use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use golutra_client::{RuntimeClient, RuntimeTransport};
use golutra_core::{SessionId, TaskId, TaskStatus, ThreadId, TurnId};
use golutra_protocol::{
    DriverKey, DriverMouseEvent, DriverMouseKind, EventPageDirection, EventPageRequest,
    RuntimeEvent, TUI_DRIVER_MAX_HEIGHT, TUI_DRIVER_MAX_WIDTH,
};
use uuid::Uuid;

use super::{MAX_DRIVER_INPUT_BYTES, TuiApp};

pub(super) fn validate_dimensions(width: u16, height: u16) -> miette::Result<()> {
    if !(40..=TUI_DRIVER_MAX_WIDTH).contains(&width) {
        return Err(miette::miette!(
            "invalid_dimensions: width must be between 40 and {TUI_DRIVER_MAX_WIDTH}"
        ));
    }
    if !(8..=TUI_DRIVER_MAX_HEIGHT).contains(&height) {
        return Err(miette::miette!(
            "invalid_dimensions: height must be between 8 and {TUI_DRIVER_MAX_HEIGHT}"
        ));
    }
    if u32::from(width) * u32::from(height) > 64_000 {
        return Err(miette::miette!(
            "invalid_dimensions: viewport exceeds the 64K cell limit"
        ));
    }
    Ok(())
}

pub(super) fn validate_input(value: &str) -> miette::Result<()> {
    if value.len() > MAX_DRIVER_INPUT_BYTES {
        return Err(miette::miette!(
            "input_too_large: input exceeds {MAX_DRIVER_INPUT_BYTES} UTF-8 bytes"
        ));
    }
    if value.contains('\0') {
        return Err(miette::miette!("invalid_input: input contains a NUL byte"));
    }
    Ok(())
}

pub(super) async fn resolve_driver_session(
    value: Option<&str>,
    transport: &RuntimeTransport,
) -> miette::Result<(ThreadId, SessionId)> {
    let value = value.map(str::trim).filter(|value| !value.is_empty());
    match value {
        None | Some("new") => Ok((ThreadId::new(), SessionId::new())),
        Some("current") => {
            let session_id = transport.default_session_id();
            let thread_id = transport.default_thread_id();
            Ok((thread_id, session_id))
        }
        Some(value) if value.starts_with("new:") => {
            let session_id = parse_session_id(&value[4..])?;
            if transport
                .thread_for_session(session_id)
                .await
                .map_err(|error| miette::miette!("{error}"))?
                .is_some()
            {
                return Err(miette::miette!(
                    "session_exists: session `{session_id}` already exists; use its UUID without the new prefix"
                ));
            }
            Ok((ThreadId::new(), session_id))
        }
        Some(value) => {
            let session_id = parse_session_id(value)?;
            let thread = transport
                .thread_for_session(session_id)
                .await
                .map_err(|error| miette::miette!("{error}"))?
                .ok_or_else(|| {
                    miette::miette!(
                        "session_not_found: session `{session_id}` does not exist in this workspace"
                    )
                })?;
            Ok((thread.thread_id, thread.session_id))
        }
    }
}

fn parse_session_id(value: &str) -> miette::Result<SessionId> {
    Uuid::parse_str(value)
        .map(SessionId)
        .map_err(|error| miette::miette!("invalid_session: {error}"))
}

pub(super) async fn validate_task_id(
    task_id: Option<TaskId>,
    session_id: SessionId,
    transport: &RuntimeTransport,
) -> miette::Result<()> {
    let Some(task_id) = task_id else {
        return Ok(());
    };
    let page = transport
        .event_page(EventPageRequest {
            session_id,
            task_id: Some(task_id),
            cursor: None,
            direction: EventPageDirection::Forward,
            limit: 1,
        })
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    if page.events.is_empty() {
        return Err(miette::miette!(
            "task_not_found: task `{task_id}` does not exist in session `{session_id}`"
        ));
    }
    Ok(())
}

pub(super) fn current_task_and_turn(app: &TuiApp) -> (Option<TaskId>, Option<TurnId>) {
    let events = runtime_events(app);
    let task_id = app
        .task_id
        .or_else(|| {
            app.projection
                .as_ref()
                .and_then(|projection| projection.task_id)
        })
        .or_else(|| events.iter().rev().find_map(|event| event.task_id));
    let turn_id = task_id
        .and_then(|task_id| {
            events
                .iter()
                .rev()
                .find(|event| event.task_id == Some(task_id))
                .and_then(|event| event.turn_id)
        })
        .or_else(|| events.iter().rev().find_map(|event| event.turn_id));
    (task_id, turn_id)
}

pub(super) fn runtime_events(app: &TuiApp) -> &[RuntimeEvent] {
    &app.events
}

pub(super) fn event_type_name(event_type: golutra_protocol::RuntimeEventType) -> String {
    serde_json::to_value(event_type)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| format!("{event_type:?}").to_ascii_lowercase())
}

pub(super) fn is_terminal_status(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Completed
            | TaskStatus::Partial
            | TaskStatus::Failed
            | TaskStatus::Blocked
            | TaskStatus::Cancelled
    )
}

pub(super) fn capabilities() -> Vec<String> {
    [
        "input.prompt",
        "input.slash",
        "input.key",
        "input.paste",
        "input.mouse",
        "resize",
        "wait",
        "snapshot.text",
        "snapshot.cells",
        "snapshot.frozen-pagination",
        "diagnostics.metrics",
        "transport.socket.peer-uid",
        "controller.takeover",
        "task.abort",
        "heartbeat",
    ]
    .into_iter()
    .map(ToOwned::to_owned)
    .collect()
}

pub(super) fn driver_key_code(key: DriverKey) -> KeyCode {
    match key {
        DriverKey::Enter => KeyCode::Enter,
        DriverKey::Escape => KeyCode::Esc,
        DriverKey::Up => KeyCode::Up,
        DriverKey::Down => KeyCode::Down,
        DriverKey::Left => KeyCode::Left,
        DriverKey::Right => KeyCode::Right,
        DriverKey::PageUp => KeyCode::PageUp,
        DriverKey::PageDown => KeyCode::PageDown,
        DriverKey::Home => KeyCode::Home,
        DriverKey::End => KeyCode::End,
        DriverKey::Backspace => KeyCode::Backspace,
        DriverKey::Delete => KeyCode::Delete,
        DriverKey::Tab => KeyCode::Tab,
        DriverKey::Char(_) | DriverKey::CtrlC => KeyCode::Null,
    }
}

pub(super) fn driver_mouse_event(event: DriverMouseEvent) -> MouseEvent {
    let kind = match event.kind {
        DriverMouseKind::LeftClick => MouseEventKind::Down(MouseButton::Left),
        DriverMouseKind::ScrollUp => MouseEventKind::ScrollUp,
        DriverMouseKind::ScrollDown => MouseEventKind::ScrollDown,
    };
    MouseEvent {
        kind,
        column: event.column,
        row: event.row,
        modifiers: crossterm::event::KeyModifiers::NONE,
    }
}

pub(super) fn driver_error_code(error: &miette::Report) -> String {
    error
        .to_string()
        .split_once(':')
        .map(|(prefix, _)| prefix.trim())
        .filter(|prefix| {
            !prefix.is_empty()
                && prefix
                    .chars()
                    .all(|character| character.is_ascii_lowercase() || character == '_')
        })
        .unwrap_or("driver_error")
        .to_owned()
}

pub(super) fn bounded_error(value: &str) -> String {
    const MAX_ERROR_CHARS: usize = 2_000;
    let mut bounded = value.chars().take(MAX_ERROR_CHARS).collect::<String>();
    if value.chars().count() > MAX_ERROR_CHARS {
        bounded.push_str("...");
    }
    bounded
}

#[cfg(test)]
mod tests;
