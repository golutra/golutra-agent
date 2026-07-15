use chrono::Utc;
use golutra_core::{EventId, SessionId, TaskId, TurnId};
use golutra_protocol::{RuntimeEvent, RuntimeEventSource, RuntimeEventType};
use serde_json::{Value, json};

use super::*;

#[test]
fn otel_mapping_builds_session_task_turn_and_redacted_event_spans() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    let events = vec![
        event(
            1,
            session_id,
            task_id,
            turn_id,
            RuntimeEventType::ProviderStarted,
            RuntimeEventSource::Provider,
            json!({"provider_id": "openai", "model_id": "gpt-test"}),
        ),
        event(
            2,
            session_id,
            task_id,
            turn_id,
            RuntimeEventType::ProviderCompleted,
            RuntimeEventSource::Provider,
            json!({"summary": "authorization: bearer sk-example-secret-value"}),
        ),
    ];

    let trace = otel_trace(&events);

    assert_eq!(trace.trace_id_length(), 32);
    assert_eq!(trace.spans.len(), 5);
    let provider = trace
        .spans
        .iter()
        .find(|span| span.name == "golutra.provider_call")
        .expect("provider span");
    assert_eq!(provider.parent_span_id.as_deref().map(str::len), Some(16));
    assert!(
        trace
            .spans
            .iter()
            .flat_map(|span| span.attributes.values())
            .all(|value| !value.to_string().contains("sk-example-secret-value"))
    );
}

impl OtelTraceExport {
    fn trace_id_length(&self) -> usize {
        self.spans.first().map_or(0, |span| span.trace_id.len())
    }
}

fn event(
    sequence_no: u64,
    session_id: SessionId,
    task_id: TaskId,
    turn_id: TurnId,
    event_type: RuntimeEventType,
    source: RuntimeEventSource,
    payload: Value,
) -> RuntimeEvent {
    RuntimeEvent {
        id: EventId::new(),
        sequence_no,
        session_id,
        turn_id: Some(turn_id),
        task_id: Some(task_id),
        parent_event_id: None,
        event_type,
        timestamp: Utc::now(),
        source,
        payload,
        payload_ref: None,
        durable: true,
    }
}
