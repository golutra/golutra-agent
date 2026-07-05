use serde_json::Value;

pub use golutra_protocol::{DebugProjection, RuntimeEvent, UserProjection};

#[must_use]
pub fn tui_boundary() -> &'static str {
    "projection-only terminal UI"
}

#[must_use]
pub fn render_user_projection(projection: &UserProjection) -> String {
    let steps = projection
        .visible_steps
        .iter()
        .map(|step| format!("{} [{}] {}", step.label, step.status, step.summary))
        .collect::<Vec<_>>()
        .join("\n");
    format!("status: {:?}\n{steps}", projection.status)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventTimelineLine {
    pub sequence_no: u64,
    pub label: String,
    pub summary: String,
}

#[must_use]
pub fn event_timeline_lines(events: &[Value]) -> Vec<EventTimelineLine> {
    events
        .iter()
        .filter_map(|value| serde_json::from_value::<RuntimeEvent>(value.clone()).ok())
        .map(|event| EventTimelineLine {
            sequence_no: event.sequence_no,
            label: format!("{:?} / {:?}", event.event_type, event.source),
            summary: event
                .payload
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("runtime event recorded")
                .to_owned(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use golutra_core::{EventId, SessionId};
    use golutra_protocol::{RuntimeEventSource, RuntimeEventType};
    use serde_json::json;

    use super::*;

    #[test]
    fn event_lines_ignore_invalid_json_values() {
        let session_id = SessionId::new();
        let event = RuntimeEvent {
            id: EventId::new(),
            sequence_no: 7,
            session_id,
            turn_id: None,
            task_id: None,
            parent_event_id: None,
            event_type: RuntimeEventType::CommandAccepted,
            timestamp: chrono::Utc::now(),
            source: RuntimeEventSource::User,
            payload: json!({"summary": "accepted prompt"}),
            payload_ref: None,
            durable: true,
        };

        let lines = event_timeline_lines(&[json!({"bad": true}), json!(event)]);

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].sequence_no, 7);
        assert_eq!(lines[0].summary, "accepted prompt");
    }
}
