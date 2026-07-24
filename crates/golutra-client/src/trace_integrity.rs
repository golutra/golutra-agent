use std::collections::{HashMap, HashSet};

use golutra_core::{
    CausalRelation, ProviderRequestId, RUNTIME_EVENT_SCHEMA_VERSION, RunProvenance, TaskId,
    ToolCallId,
};
use golutra_protocol::{RuntimeEvent, RuntimeEventType};

#[derive(Debug, Default)]
pub(crate) struct CausalIntegrityReport {
    pub missing_causal_links: Vec<String>,
    pub orphan_events: Vec<String>,
    pub broken_lifecycle_pairs: Vec<String>,
    pub provenance_mismatches: Vec<String>,
}

pub(crate) fn validate_causal_trace(
    task_id: TaskId,
    events: &[RuntimeEvent],
    provenance: Option<&RunProvenance>,
) -> CausalIntegrityReport {
    let mut report = CausalIntegrityReport::default();
    let event_ids = events.iter().map(|event| event.id).collect::<HashSet<_>>();
    let mut provider_starts = HashMap::<ProviderRequestId, &RuntimeEvent>::new();
    let mut provider_terminated = HashSet::<ProviderRequestId>::new();
    let mut tool_starts = HashMap::<ToolCallId, &RuntimeEvent>::new();
    let mut tool_completed = HashSet::<ToolCallId>::new();
    let mut verification_planned = false;
    let mut verification_completed = false;
    let mut has_schema_v2 = false;

    for (index, event) in events.iter().enumerate() {
        if event.schema_version < RUNTIME_EVENT_SCHEMA_VERSION {
            continue;
        }
        has_schema_v2 = true;
        if event.task_id != Some(task_id)
            || event.causal_context.task_id != Some(task_id)
            || event.causal_context.session_id != Some(event.session_id)
            || event.causal_context.turn_id != event.turn_id
        {
            report.provenance_mismatches.push(format!(
                "event:{}:envelope_causal_context_mismatch",
                event.id
            ));
        }
        if event.causal_context.run_id != Some(task_id.into()) {
            report
                .provenance_mismatches
                .push(format!("event:{}:run_id_mismatch", event.id));
        }
        match event.parent_event_id {
            None if index > 0 => report.orphan_events.push(event.id.to_string()),
            Some(parent) => {
                let parent_is_external_root = index == 0 && !event_ids.contains(&parent);
                if !parent_is_external_root && !event_ids.contains(&parent) {
                    report
                        .missing_causal_links
                        .push(format!("event:{}:missing_parent:{parent}", event.id));
                }
                if !event
                    .causal_links
                    .iter()
                    .any(|link| link.event_id == parent && link.relation == CausalRelation::Parent)
                {
                    report
                        .missing_causal_links
                        .push(format!("event:{}:parent_link:{parent}", event.id));
                }
            }
            None => {}
        }

        match event.event_type {
            RuntimeEventType::ProviderStarted => {
                if let Some(request_id) = event.causal_context.provider_request_id {
                    provider_starts.insert(request_id, event);
                } else {
                    report
                        .broken_lifecycle_pairs
                        .push(format!("event:{}:provider_start_without_request", event.id));
                }
            }
            RuntimeEventType::ProviderCompleted | RuntimeEventType::ProviderFailed => {
                if let Some(request_id) = event.causal_context.provider_request_id {
                    provider_terminated.insert(request_id);
                    match provider_starts.get(&request_id) {
                        Some(start)
                            if event.causal_links.iter().any(|link| {
                                link.event_id == start.id
                                    && link.relation == CausalRelation::RespondsTo
                            }) => {}
                        Some(_) => report.broken_lifecycle_pairs.push(format!(
                            "event:{}:provider_complete_without_response_link",
                            event.id
                        )),
                        None => report.broken_lifecycle_pairs.push(format!(
                            "event:{}:provider_complete_without_start",
                            event.id
                        )),
                    }
                } else {
                    report.broken_lifecycle_pairs.push(format!(
                        "event:{}:provider_complete_without_request",
                        event.id
                    ));
                }
            }
            RuntimeEventType::ToolStarted => {
                if let Some(tool_call_id) = event.causal_context.tool_call_id {
                    tool_starts.insert(tool_call_id, event);
                } else {
                    report
                        .broken_lifecycle_pairs
                        .push(format!("event:{}:tool_start_without_id", event.id));
                }
            }
            RuntimeEventType::ToolCompleted => {
                if let Some(tool_call_id) = event.causal_context.tool_call_id {
                    tool_completed.insert(tool_call_id);
                    match tool_starts.get(&tool_call_id) {
                        Some(start)
                            if event.causal_links.iter().any(|link| {
                                link.event_id == start.id
                                    && link.relation == CausalRelation::RespondsTo
                            }) => {}
                        Some(_) => report.broken_lifecycle_pairs.push(format!(
                            "event:{}:tool_complete_without_response_link",
                            event.id
                        )),
                        None => report
                            .broken_lifecycle_pairs
                            .push(format!("event:{}:tool_complete_without_start", event.id)),
                    }
                } else {
                    report
                        .broken_lifecycle_pairs
                        .push(format!("event:{}:tool_complete_without_id", event.id));
                }
            }
            RuntimeEventType::VerificationPlanned => verification_planned = true,
            RuntimeEventType::VerificationCompleted => verification_completed = true,
            _ => {}
        }
    }

    if has_schema_v2 {
        for request_id in provider_starts.keys() {
            if !provider_terminated.contains(request_id) {
                report
                    .broken_lifecycle_pairs
                    .push(format!("provider_request:{request_id}:not_completed"));
            }
        }
        for tool_call_id in tool_starts.keys() {
            if !tool_completed.contains(tool_call_id) {
                report
                    .broken_lifecycle_pairs
                    .push(format!("tool_call:{tool_call_id}:not_completed"));
            }
        }
        if verification_planned && !verification_completed {
            report
                .broken_lifecycle_pairs
                .push("verification:planned_without_record".to_owned());
        }
        match provenance {
            Some(provenance) if provenance.run_id != task_id.into() => report
                .provenance_mismatches
                .push("run_provenance:task_id_mismatch".to_owned()),
            Some(provenance)
                if events.iter().any(|event| {
                    event
                        .payload
                        .get("runtime_identity")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|identity| identity != provenance.runtime_identity)
                }) =>
            {
                report
                    .provenance_mismatches
                    .push("run_provenance:runtime_identity_mismatch".to_owned())
            }
            Some(_) => {}
            None => report
                .provenance_mismatches
                .push("run_provenance:missing".to_owned()),
        }
    }

    for values in [
        &mut report.missing_causal_links,
        &mut report.orphan_events,
        &mut report.broken_lifecycle_pairs,
        &mut report.provenance_mismatches,
    ] {
        values.sort();
        values.dedup();
    }
    report
}
