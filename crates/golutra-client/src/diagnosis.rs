use std::collections::{HashMap, HashSet, VecDeque};

use golutra_core::{ArtifactId, EventId, EvidenceId, ProviderRequestId, TaskId};
use golutra_eval::{
    CodeTargetRef, DiagnosticSlice, FailureDiagnosis, FailureDomain, FailureTaxonomy,
    ReplayCapsule, ReplayMode, ReplayProviderExchange, ReplayToolResult,
};
use golutra_protocol::{RuntimeEvent, RuntimeEventType};
use serde_json::Value;

pub(crate) fn diagnose_task(
    task_id: TaskId,
    events: &[RuntimeEvent],
    source_digest: Option<String>,
) -> Option<(FailureDiagnosis, DiagnosticSlice)> {
    let (trigger_index, trigger) = select_failure_trigger(events)?;
    let (taxonomy, summary, expected, actual, counterfactual, targets, commands, confidence) =
        classify_failure(events, trigger_index, trigger, source_digest);
    let causal_events = causal_slice(events, trigger.id, 100);
    let mut artifact_refs = HashSet::new();
    let mut evidence_refs = HashSet::new();
    for event in &causal_events {
        if let Some(artifact_id) = event.payload_ref {
            artifact_refs.insert(artifact_id);
        }
        collect_artifact_ids(&event.payload, None, &mut artifact_refs);
        collect_evidence_ids(&event.payload, &mut evidence_refs);
    }
    let event_refs = causal_events
        .iter()
        .map(|event| event.id)
        .collect::<Vec<_>>();
    let diagnosis = FailureDiagnosis {
        diagnosis_id: format!("diagnosis-{task_id}"),
        source_task_id: task_id,
        taxonomy,
        summary,
        trigger_event_refs: vec![trigger.id],
        causal_event_refs: event_refs.clone(),
        expected_behavior: expected,
        actual_behavior: actual,
        counterfactual,
        confidence,
        code_targets: targets,
        regression_commands: commands,
        analyzer_version: "golutra-failure-diagnosis-v1".to_owned(),
        created_at: chrono::Utc::now(),
    };
    let complete = event_refs.iter().all(|event_id| {
        events
            .iter()
            .find(|event| event.id == *event_id)
            .is_some_and(|event| {
                event.parent_event_id.is_none()
                    || event_refs.contains(&event.parent_event_id.expect("checked"))
                    || events.first().is_some_and(|first| first.id == event.id)
            })
    });
    let slice = DiagnosticSlice {
        slice_id: format!("diagnostic-slice-{task_id}"),
        source_task_id: task_id,
        diagnosis: diagnosis.clone(),
        event_refs,
        artifact_refs: sorted(artifact_refs),
        evidence_refs: sorted(evidence_refs),
        omitted_event_count: u64::try_from(events.len().saturating_sub(causal_events.len()))
            .unwrap_or(u64::MAX),
        complete,
        generated_at: chrono::Utc::now(),
    };
    Some((diagnosis, slice))
}

pub(crate) fn replay_capsule(
    task_id: TaskId,
    events: &[RuntimeEvent],
    event_chain_digest: String,
    runtime_config_digest: Option<String>,
) -> ReplayCapsule {
    let mut requests = HashMap::<ProviderRequestId, ArtifactId>::new();
    let mut exchanges = Vec::new();
    let mut tool_results = Vec::new();
    let mut missing_inputs = Vec::new();
    for event in events {
        match event.event_type {
            RuntimeEventType::ContextSnapshotCreated => {
                if let (Some(request_id), Some(artifact_id)) = (
                    parse_id::<ProviderRequestId>(
                        event
                            .payload
                            .pointer("/snapshot/provider_request_id")
                            .or_else(|| event.payload.get("provider_request_id")),
                    ),
                    parse_id::<ArtifactId>(
                        event
                            .payload
                            .get("restricted_request_artifact_ref")
                            .or_else(|| {
                                event
                                    .payload
                                    .pointer("/snapshot/restricted_request_artifact_ref")
                            }),
                    ),
                ) {
                    requests.insert(request_id, artifact_id);
                }
            }
            RuntimeEventType::ProviderCompleted => {
                let request_id = event
                    .causal_context
                    .provider_request_id
                    .or_else(|| parse_id(event.payload.get("provider_request_id")));
                let response_id = event
                    .causal_context
                    .provider_response_id
                    .or_else(|| parse_id(event.payload.get("provider_response_id")));
                let response_artifact =
                    parse_id::<ArtifactId>(event.payload.get("response_artifact_ref"));
                match request_id.zip(response_id).zip(response_artifact).and_then(
                    |((request_id, response_id), response_artifact_ref)| {
                        requests
                            .get(&request_id)
                            .copied()
                            .map(|request_artifact_ref| ReplayProviderExchange {
                                request_id,
                                response_id,
                                request_artifact_ref,
                                response_artifact_ref,
                            })
                    },
                ) {
                    Some(exchange) => exchanges.push(exchange),
                    None => missing_inputs.push(format!("provider_exchange:event:{}", event.id)),
                }
            }
            RuntimeEventType::ToolCompleted => {
                let tool_call_id = event
                    .causal_context
                    .tool_call_id
                    .or_else(|| parse_id(event.payload.pointer("/envelope/tool_call_id")));
                let result_artifact_ref =
                    parse_id::<ArtifactId>(event.payload.get("replay_result_artifact_ref"));
                match tool_call_id.zip(result_artifact_ref) {
                    Some((tool_call_id, result_artifact_ref)) => {
                        tool_results.push(ReplayToolResult {
                            tool_call_id,
                            provider_tool_call_id: event
                                .causal_context
                                .provider_tool_call_id
                                .clone(),
                            result_artifact_ref,
                        });
                    }
                    None => missing_inputs.push(format!("tool_result:event:{}", event.id)),
                }
            }
            _ => {}
        }
    }
    if exchanges.is_empty() {
        missing_inputs.push("provider_exchanges".to_owned());
    }
    let runtime_config_digest = runtime_config_digest.unwrap_or_else(|| {
        missing_inputs.push("runtime_config_digest".to_owned());
        "unknown".to_owned()
    });
    missing_inputs.sort();
    missing_inputs.dedup();
    ReplayCapsule {
        capsule_id: format!("replay-capsule-{task_id}"),
        source_task_id: task_id,
        source_run_id: task_id.into(),
        mode: ReplayMode::DeterministicControlFlow,
        provider_exchanges: exchanges,
        tool_results,
        clock_seed: events
            .first()
            .map(|event| event.timestamp.to_rfc3339())
            .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned()),
        random_seed: task_id.0.as_u128() as u64,
        runtime_config_digest,
        fixture_ref: events
            .iter()
            .find(|event| event.event_type == RuntimeEventType::CheckpointCreated)
            .map(|event| format!("event:{}", event.id)),
        event_chain_digest,
        source_last_sequence_no: events.last().map(|event| event.sequence_no),
        complete: missing_inputs.is_empty(),
        missing_inputs,
        limitations: vec![
            "deterministic replay injects recorded provider and tool results".to_owned(),
            "live provider behavior requires live_regression mode".to_owned(),
        ],
        created_at: chrono::Utc::now(),
    }
}

fn select_failure_trigger(events: &[RuntimeEvent]) -> Option<(usize, &RuntimeEvent)> {
    events
        .iter()
        .enumerate()
        .rev()
        .find(|(_, event)| event.event_type == RuntimeEventType::LoopGuardTriggered)
        .or_else(|| {
            events.iter().enumerate().rev().find(|(_, event)| {
                event.event_type == RuntimeEventType::ToolCompleted
                    && event
                        .payload
                        .pointer("/envelope/status")
                        .and_then(Value::as_str)
                        .is_some_and(|status| status != "ok")
            })
        })
        .or_else(|| {
            events.iter().enumerate().rev().find(|(_, event)| {
                event.event_type == RuntimeEventType::VerificationCompleted
                    && event
                        .payload
                        .pointer("/record/result")
                        .and_then(Value::as_str)
                        .is_some_and(|result| result != "pass")
            })
        })
        .or_else(|| {
            events.iter().enumerate().rev().find(|(_, event)| {
                event.event_type.is_task_terminal()
                    && event
                        .payload
                        .get("status")
                        .and_then(Value::as_str)
                        .is_some_and(|status| status != "completed")
            })
        })
}

#[allow(clippy::type_complexity)]
fn classify_failure(
    events: &[RuntimeEvent],
    trigger_index: usize,
    trigger: &RuntimeEvent,
    source_digest: Option<String>,
) -> (
    FailureTaxonomy,
    String,
    String,
    String,
    String,
    Vec<CodeTargetRef>,
    Vec<String>,
    u8,
) {
    let trigger_kind = trigger.payload.get("trigger").and_then(Value::as_str);
    if trigger_kind == Some("repeated_tool_failure")
        && repeated_failure_is_single_round(events, trigger_index, trigger)
    {
        return (
            FailureTaxonomy {
                domain: FailureDomain::RuntimeControlFlow,
                code: "loop_guard_false_positive".to_owned(),
            },
            "repeated-tool-failure guard counted duplicate failures from one provider round"
                .to_owned(),
            "failures from one provider response are counted as one retry opportunity".to_owned(),
            "the guard stopped before the provider could observe the tool results and recover"
                .to_owned(),
            "advance to another provider round before incrementing the repeated-failure streak"
                .to_owned(),
            vec![runtime_target(
                "update_repeated_failure_streak",
                source_digest,
            )],
            vec![
                "cargo test -p golutra-runtime duplicate_failures_in_one_provider_round_can_recover_on_the_next_round"
                    .to_owned(),
            ],
            98,
        );
    }
    if let Some(trigger_kind) = trigger_kind {
        return (
            FailureTaxonomy {
                domain: FailureDomain::RuntimeControlFlow,
                code: trigger_kind.to_owned(),
            },
            format!("runtime loop guard stopped execution: {trigger_kind}"),
            "the runtime continues while bounded progress or recovery remains possible".to_owned(),
            trigger
                .payload
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("loop guard stopped execution")
                .to_owned(),
            "replay the same control-flow facts with the candidate guard policy".to_owned(),
            vec![runtime_target("AgentLoop::run_with_trace", source_digest)],
            vec!["cargo test -p golutra-runtime".to_owned()],
            90,
        );
    }
    if trigger.event_type == RuntimeEventType::ToolCompleted {
        let tool_name = trigger
            .payload
            .pointer("/envelope/tool_name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return (
            FailureTaxonomy {
                domain: FailureDomain::Tool,
                code: format!("{tool_name}_failure"),
            },
            format!("tool {tool_name} failed"),
            "the tool returns a successful, bounded result or a recoverable error".to_owned(),
            trigger
                .payload
                .pointer("/envelope/summary")
                .and_then(Value::as_str)
                .unwrap_or("tool execution failed")
                .to_owned(),
            "replay the tool envelope against the owning tool adapter".to_owned(),
            vec![CodeTargetRef {
                crate_name: "golutra-tools".to_owned(),
                module_path: tool_name.to_owned(),
                symbol: None,
                source_path: Some("crates/golutra-tools/src".to_owned()),
                source_digest,
                owner: "tools".to_owned(),
            }],
            vec!["cargo test -p golutra-tools".to_owned()],
            82,
        );
    }
    (
        FailureTaxonomy {
            domain: FailureDomain::Verification,
            code: "objective_verification_failed".to_owned(),
        },
        "task completion criteria were not objectively verified".to_owned(),
        "all blocking verification assertions pass with durable evidence".to_owned(),
        trigger
            .payload
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("verification failed")
            .to_owned(),
        "rerun the recorded fixture with the candidate verifier and runtime".to_owned(),
        vec![CodeTargetRef {
            crate_name: "golutra-verify".to_owned(),
            module_path: "verification".to_owned(),
            symbol: Some("RuntimeVerificationService".to_owned()),
            source_path: Some("crates/golutra-verify/src/lib.rs".to_owned()),
            source_digest,
            owner: "verification".to_owned(),
        }],
        vec!["cargo test -p golutra-verify".to_owned()],
        75,
    )
}

fn repeated_failure_is_single_round(
    events: &[RuntimeEvent],
    trigger_index: usize,
    trigger: &RuntimeEvent,
) -> bool {
    let Some(round_id) = trigger.causal_context.provider_round_id.as_deref() else {
        return false;
    };
    let failures = events[..trigger_index]
        .iter()
        .filter(|event| {
            event.event_type == RuntimeEventType::ToolCompleted
                && event.causal_context.provider_round_id.as_deref() == Some(round_id)
                && event
                    .payload
                    .pointer("/envelope/status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| status != "ok")
        })
        .collect::<Vec<_>>();
    if failures.len() < 2 {
        return false;
    }
    let signatures = failures
        .iter()
        .map(|event| tool_failure_signature(event))
        .collect::<HashSet<_>>();
    if signatures.len() != 1 {
        return false;
    }
    let signature = signatures.into_iter().next().expect("one signature");
    !events[..trigger_index].iter().any(|event| {
        event.event_type == RuntimeEventType::ToolCompleted
            && event.causal_context.provider_round_id.as_deref() != Some(round_id)
            && tool_failure_signature(event) == signature
    })
}

fn tool_failure_signature(event: &RuntimeEvent) -> String {
    let tool = event
        .payload
        .pointer("/envelope/tool_name")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let action = event
        .payload
        .pointer("/envelope/structured_facts/command")
        .or_else(|| event.payload.pointer("/envelope/summary"))
        .map(Value::to_string)
        .unwrap_or_default();
    format!("{tool}:{action}")
}

fn runtime_target(symbol: &str, source_digest: Option<String>) -> CodeTargetRef {
    CodeTargetRef {
        crate_name: "golutra-runtime".to_owned(),
        module_path: "agent_loop".to_owned(),
        symbol: Some(symbol.to_owned()),
        source_path: Some("crates/golutra-runtime/src/lib.rs".to_owned()),
        source_digest,
        owner: "runtime".to_owned(),
    }
}

fn causal_slice(events: &[RuntimeEvent], trigger_id: EventId, limit: usize) -> Vec<&RuntimeEvent> {
    let by_id = events
        .iter()
        .map(|event| (event.id, event))
        .collect::<HashMap<_, _>>();
    let mut pending = VecDeque::from([trigger_id]);
    let mut selected = HashSet::new();
    while let Some(event_id) = pending.pop_front() {
        if selected.len() >= limit || !selected.insert(event_id) {
            continue;
        }
        let Some(event) = by_id.get(&event_id) else {
            continue;
        };
        if let Some(parent) = event.parent_event_id {
            pending.push_back(parent);
        }
        pending.extend(event.causal_links.iter().map(|link| link.event_id));
    }
    let mut selected = events
        .iter()
        .filter(|event| selected.contains(&event.id))
        .collect::<Vec<_>>();
    selected.sort_by_key(|event| event.sequence_no);
    selected
}

fn collect_artifact_ids(value: &Value, key: Option<&str>, output: &mut HashSet<ArtifactId>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                collect_artifact_ids(value, Some(key), output);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_artifact_ids(value, key, output);
            }
        }
        Value::String(value)
            if key.is_some_and(|key| key.ends_with("artifact_ref") || key == "artifact_id") =>
        {
            if let Ok(artifact_id) = value.parse() {
                output.insert(artifact_id);
            }
        }
        _ => {}
    }
}

fn collect_evidence_ids(value: &Value, output: &mut HashSet<EvidenceId>) {
    if let Value::Object(object) = value {
        for (key, value) in object {
            if key == "evidence_refs" {
                if let Value::Array(values) = value {
                    output.extend(
                        values
                            .iter()
                            .filter_map(|value| parse_id::<EvidenceId>(Some(value))),
                    );
                }
            } else {
                collect_evidence_ids(value, output);
            }
        }
    } else if let Value::Array(values) = value {
        for value in values {
            collect_evidence_ids(value, output);
        }
    }
}

fn parse_id<T>(value: Option<&Value>) -> Option<T>
where
    T: std::str::FromStr,
{
    value
        .and_then(Value::as_str)
        .and_then(|value| value.parse().ok())
}

fn sorted<T: Ord>(values: HashSet<T>) -> Vec<T> {
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort();
    values
}
