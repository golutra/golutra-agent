use std::collections::{BTreeMap, HashMap};

use golutra_client::{ClientError, RuntimeClient};
use golutra_core::{SessionId, TaskId};
use golutra_protocol::{
    DebugProjection, EventPageDirection, EventPageRequest, RuntimeEvent, RuntimeEventSource,
    RuntimeEventType,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const EVENT_PAGE_SIZE: u32 = 512;
const MAX_ATTRIBUTE_TEXT_CHARS: usize = 2_048;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OtelTraceExport {
    pub schema_url: String,
    pub resource: BTreeMap<String, Value>,
    pub spans: Vec<OtelSpanRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OtelSpanRecord {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub name: String,
    pub kind: String,
    pub start_time_unix_nano: u64,
    pub end_time_unix_nano: u64,
    pub status: OtelStatus,
    pub attributes: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OtelStatus {
    pub code: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AuditReport {
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub event_count: usize,
    pub event_type_counts: BTreeMap<String, u64>,
    pub source_counts: BTreeMap<String, u64>,
    pub provider_calls: u64,
    pub tool_calls: u64,
    pub policy_checks: u64,
    pub token_usage_events: u64,
    pub verification: Option<golutra_core::VerificationRecord>,
    pub artifact_count: usize,
    pub evidence_count: usize,
    pub event_window: golutra_protocol::DebugEventWindow,
}

pub async fn load_all_events<C: RuntimeClient + Sync>(
    client: &C,
    session_id: SessionId,
    task_id: Option<TaskId>,
) -> Result<Vec<RuntimeEvent>, ClientError> {
    let mut events = Vec::new();
    let mut cursor = None;
    loop {
        let page = client
            .event_page(EventPageRequest {
                session_id,
                task_id,
                cursor,
                direction: EventPageDirection::Forward,
                limit: EVENT_PAGE_SIZE,
            })
            .await?;
        cursor = page.end_cursor;
        events.extend(page.events);
        if !page.has_more {
            break;
        }
        if cursor.is_none() {
            return Err(ClientError::TaskExecution(
                "event pagination reported more data without a cursor".to_owned(),
            ));
        }
    }
    Ok(events)
}

#[must_use]
pub fn audit_report(projection: &DebugProjection) -> AuditReport {
    let mut event_type_counts = BTreeMap::new();
    let mut source_counts = BTreeMap::new();
    for event in &projection.events {
        increment(&mut event_type_counts, wire_name(&event.event_type));
        increment(&mut source_counts, wire_name(&event.source));
    }
    AuditReport {
        session_id: projection.session_id,
        task_id: projection.task_id,
        event_count: projection.events.len(),
        provider_calls: count_events(&projection.events, &[RuntimeEventType::ProviderCompleted]),
        tool_calls: count_events(&projection.events, &[RuntimeEventType::ToolCompleted]),
        policy_checks: count_events(&projection.events, &[RuntimeEventType::PolicyEvaluated]),
        token_usage_events: count_events(
            &projection.events,
            &[RuntimeEventType::TokenUsageRecorded],
        ),
        verification: projection.verification.clone(),
        artifact_count: projection.artifacts.len(),
        evidence_count: projection.evidence.len(),
        event_window: projection.event_window.clone(),
        event_type_counts,
        source_counts,
    }
}

#[must_use]
pub fn otel_trace(events: &[RuntimeEvent]) -> OtelTraceExport {
    let trace_id = trace_id(events.first().map(|event| event.session_id));
    let mut spans = Vec::new();
    if events.is_empty() {
        return OtelTraceExport {
            schema_url: "https://opentelemetry.io/schemas/1.27.0".to_owned(),
            resource: resource_attributes(),
            spans,
        };
    }
    let first_nanos = timestamp_nanos(events.first().expect("non-empty events"));
    let last_nanos = timestamp_nanos(events.last().expect("non-empty events"));
    let session_id = events[0].session_id;
    let session_span_id = stable_span_id(&format!("session:{session_id}"));
    spans.push(OtelSpanRecord {
        trace_id: trace_id.clone(),
        span_id: session_span_id.clone(),
        parent_span_id: None,
        name: "golutra.session".to_owned(),
        kind: "INTERNAL".to_owned(),
        start_time_unix_nano: first_nanos,
        end_time_unix_nano: last_nanos.max(first_nanos),
        status: status_for_events(events),
        attributes: BTreeMap::from([
            ("golutra.session.id".to_owned(), json!(session_id)),
            (
                "golutra.event.count".to_owned(),
                json!(u64::try_from(events.len()).unwrap_or(u64::MAX)),
            ),
        ]),
    });

    let task_ranges = ranges_by(events, |event| event.task_id.map(|id| id.to_string()));
    let turn_ranges = ranges_by(events, |event| event.turn_id.map(|id| id.to_string()));
    let mut task_span_ids = HashMap::new();
    for (task_id, range) in &task_ranges {
        let span_id = stable_span_id(&format!("task:{task_id}"));
        task_span_ids.insert(task_id.clone(), span_id.clone());
        spans.push(OtelSpanRecord {
            trace_id: trace_id.clone(),
            span_id,
            parent_span_id: Some(session_span_id.clone()),
            name: "golutra.task".to_owned(),
            kind: "INTERNAL".to_owned(),
            start_time_unix_nano: range.start,
            end_time_unix_nano: range.end,
            status: range.status.clone(),
            attributes: BTreeMap::from([("golutra.task.id".to_owned(), json!(task_id))]),
        });
    }
    let mut turn_span_ids = HashMap::new();
    for (turn_id, range) in &turn_ranges {
        let span_id = stable_span_id(&format!("turn:{turn_id}"));
        turn_span_ids.insert(turn_id.clone(), span_id.clone());
        let parent_span_id = range
            .task_id
            .as_ref()
            .and_then(|task_id| task_span_ids.get(task_id))
            .cloned()
            .unwrap_or_else(|| session_span_id.clone());
        spans.push(OtelSpanRecord {
            trace_id: trace_id.clone(),
            span_id,
            parent_span_id: Some(parent_span_id),
            name: "golutra.turn".to_owned(),
            kind: "INTERNAL".to_owned(),
            start_time_unix_nano: range.start,
            end_time_unix_nano: range.end,
            status: range.status.clone(),
            attributes: BTreeMap::from([("golutra.turn.id".to_owned(), json!(turn_id))]),
        });
    }

    for event in events {
        let parent_span_id = event
            .turn_id
            .and_then(|turn_id| turn_span_ids.get(&turn_id.to_string()).cloned())
            .or_else(|| {
                event
                    .task_id
                    .and_then(|task_id| task_span_ids.get(&task_id.to_string()).cloned())
            })
            .unwrap_or_else(|| session_span_id.clone());
        spans.push(event_span(event, &trace_id, parent_span_id));
    }
    spans.sort_by_key(|span| (span.start_time_unix_nano, span.span_id.clone()));
    OtelTraceExport {
        schema_url: "https://opentelemetry.io/schemas/1.27.0".to_owned(),
        resource: resource_attributes(),
        spans,
    }
}

fn event_span(event: &RuntimeEvent, trace_id: &str, parent_span_id: String) -> OtelSpanRecord {
    let start = timestamp_nanos(event);
    let mut attributes = BTreeMap::from([
        ("golutra.event.id".to_owned(), json!(event.id)),
        (
            "golutra.event.sequence".to_owned(),
            json!(event.sequence_no),
        ),
        (
            "golutra.event.source".to_owned(),
            json!(wire_name(&event.source)),
        ),
        ("golutra.event.durable".to_owned(), json!(event.durable)),
        ("golutra.session.id".to_owned(), json!(event.session_id)),
    ]);
    if let Some(task_id) = event.task_id {
        attributes.insert("golutra.task.id".to_owned(), json!(task_id));
    }
    if let Some(turn_id) = event.turn_id {
        attributes.insert("golutra.turn.id".to_owned(), json!(turn_id));
    }
    copy_payload_attributes(&event.payload, &mut attributes);
    OtelSpanRecord {
        trace_id: trace_id.to_owned(),
        span_id: stable_span_id(&format!("event:{}", event.id)),
        parent_span_id: Some(parent_span_id),
        name: event_span_name(event.event_type).to_owned(),
        kind: span_kind(event.source).to_owned(),
        start_time_unix_nano: start,
        end_time_unix_nano: start,
        status: event_status(event),
        attributes,
    }
}

fn copy_payload_attributes(payload: &Value, attributes: &mut BTreeMap<String, Value>) {
    for source in std::iter::once(payload).chain(
        ["usage", "record", "envelope"]
            .into_iter()
            .filter_map(|key| payload.get(key)),
    ) {
        for key in [
            "provider_id",
            "model_id",
            "tool_name",
            "latency_ms",
            "input_tokens",
            "output_tokens",
            "reasoning_tokens",
            "total_tokens",
            "estimated_cost",
            "cost_usd",
        ] {
            if let Some(value) = source.get(key).filter(|value| is_scalar(value)) {
                attributes.insert(format!("golutra.{key}"), value.clone());
            }
        }
    }
    if let Some(summary) = payload.get("summary").and_then(Value::as_str) {
        let (redacted, _) = golutra_tools::redact_sensitive_text(summary);
        attributes.insert(
            "golutra.summary".to_owned(),
            json!(bounded_text(&redacted, MAX_ATTRIBUTE_TEXT_CHARS)),
        );
    }
}

fn event_span_name(event_type: RuntimeEventType) -> &'static str {
    match event_type {
        RuntimeEventType::ContextBuilt => "golutra.context_build",
        RuntimeEventType::MemoryRetrieved => "golutra.memory_retrieval",
        RuntimeEventType::ProviderStarted
        | RuntimeEventType::ProviderStreamed
        | RuntimeEventType::ProviderCompleted
        | RuntimeEventType::ProviderFallback
        | RuntimeEventType::RetryScheduled => "golutra.provider_call",
        RuntimeEventType::ToolStarted | RuntimeEventType::ToolCompleted => "golutra.tool_execution",
        RuntimeEventType::PolicyEvaluated
        | RuntimeEventType::ApprovalRequested
        | RuntimeEventType::ApprovalResolved => "golutra.safety_check",
        RuntimeEventType::VerificationCompleted => "golutra.verification",
        RuntimeEventType::LoopDecided | RuntimeEventType::GovernorDecided => "golutra.planning",
        RuntimeEventType::PostTaskReviewed
        | RuntimeEventType::EvaluationCompleted
        | RuntimeEventType::ImprovementCandidateCreated
        | RuntimeEventType::RegressionCompleted
        | RuntimeEventType::PromotionDecided => "golutra.post_task_review",
        RuntimeEventType::EvolutionPlanned
        | RuntimeEventType::EvolutionTaskStarted
        | RuntimeEventType::EvolutionTaskCompleted
        | RuntimeEventType::EvolutionCompleted => "golutra.evolution",
        _ => "golutra.runtime_event",
    }
}

fn span_kind(source: RuntimeEventSource) -> &'static str {
    match source {
        RuntimeEventSource::Provider => "CLIENT",
        RuntimeEventSource::Tool => "INTERNAL",
        _ => "INTERNAL",
    }
}

fn event_status(event: &RuntimeEvent) -> OtelStatus {
    let is_error = matches!(
        event.event_type,
        RuntimeEventType::CommandRejected
            | RuntimeEventType::ProviderAuthFailed
            | RuntimeEventType::LoopGuardTriggered
    ) || event
        .payload
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| matches!(status, "failed" | "blocked" | "cancelled"));
    OtelStatus {
        code: if is_error { "ERROR" } else { "OK" }.to_owned(),
        message: is_error.then(|| {
            event
                .payload
                .get("summary")
                .and_then(Value::as_str)
                .map(|value| bounded_text(value, 512))
                .unwrap_or_else(|| wire_name(&event.event_type))
        }),
    }
}

fn status_for_events(events: &[RuntimeEvent]) -> OtelStatus {
    events
        .iter()
        .map(event_status)
        .find(|status| status.code == "ERROR")
        .unwrap_or(OtelStatus {
            code: "OK".to_owned(),
            message: None,
        })
}

#[derive(Debug, Clone)]
struct EventRange {
    start: u64,
    end: u64,
    status: OtelStatus,
    task_id: Option<String>,
}

fn ranges_by(
    events: &[RuntimeEvent],
    key: impl Fn(&RuntimeEvent) -> Option<String>,
) -> BTreeMap<String, EventRange> {
    let mut ranges = BTreeMap::new();
    for event in events {
        let Some(key) = key(event) else {
            continue;
        };
        let timestamp = timestamp_nanos(event);
        let status = event_status(event);
        let entry = ranges.entry(key).or_insert_with(|| EventRange {
            start: timestamp,
            end: timestamp,
            status: OtelStatus {
                code: "OK".to_owned(),
                message: None,
            },
            task_id: event.task_id.map(|task_id| task_id.to_string()),
        });
        entry.start = entry.start.min(timestamp);
        entry.end = entry.end.max(timestamp);
        if status.code == "ERROR" {
            entry.status = status;
        }
    }
    ranges
}

fn trace_id(session_id: Option<SessionId>) -> String {
    stable_hex(
        &format!(
            "session:{}",
            session_id.map_or_else(|| "empty".to_owned(), |value| value.to_string())
        ),
        16,
    )
}

fn stable_span_id(value: &str) -> String {
    stable_hex(value, 8)
}

fn stable_hex(value: &str, bytes: usize) -> String {
    Sha256::digest(value.as_bytes())[..bytes]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn timestamp_nanos(event: &RuntimeEvent) -> u64 {
    u64::try_from(event.timestamp.timestamp_nanos_opt().unwrap_or_default()).unwrap_or_default()
}

fn count_events(events: &[RuntimeEvent], types: &[RuntimeEventType]) -> u64 {
    u64::try_from(
        events
            .iter()
            .filter(|event| types.contains(&event.event_type))
            .count(),
    )
    .unwrap_or(u64::MAX)
}

fn increment(counts: &mut BTreeMap<String, u64>, key: String) {
    let count = counts.entry(key).or_default();
    *count = count.saturating_add(1);
}

fn wire_name<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn resource_attributes() -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("service.name".to_owned(), json!("golutra-agent")),
        (
            "service.version".to_owned(),
            json!(env!("CARGO_PKG_VERSION")),
        ),
        ("telemetry.sdk.language".to_owned(), json!("rust")),
    ])
}

fn is_scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

fn bounded_text(value: &str, limit: usize) -> String {
    let mut output = value.chars().take(limit).collect::<String>();
    if value.chars().count() > limit {
        output.push_str("...[truncated]");
    }
    output
}

#[cfg(test)]
mod tests;
