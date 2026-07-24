use std::collections::{HashMap, HashSet, VecDeque};

use golutra_core::{ArtifactId, CausalRelation, EventId, EvidenceId, ProviderRequestId, TaskId};
use golutra_eval::{
    CandidateRisk, CandidateStatus, CodeTargetRef, DiagnosticSlice, DiagnosticSliceContinuation,
    FailureDiagnosis, FailureDomain, FailureEpisode, FailureEpisodeStatus, FailureRecovery,
    FailureSignalKind, FailureSignalRef, FailureTaxonomy, ImprovementCandidate, ReplayCapsule,
    ReplayMode, ReplayProviderExchange, ReplayToolResult,
};
use golutra_protocol::{RuntimeEvent, RuntimeEventType};
use serde_json::Value;

#[derive(Debug, Clone)]
pub(crate) struct FailureAnalysis {
    pub(crate) diagnosis: FailureDiagnosis,
    pub(crate) slice: DiagnosticSlice,
    pub(crate) episodes: Vec<FailureEpisode>,
    pub(crate) candidate: ImprovementCandidate,
}

pub(crate) fn diagnose_task(
    task_id: TaskId,
    events: &[RuntimeEvent],
    source_digest: Option<String>,
) -> Option<FailureAnalysis> {
    let episodes = failure_episodes(task_id, events);
    let primary_episode = episodes
        .iter()
        .filter(|episode| episode.status == FailureEpisodeStatus::Active)
        .max_by_key(|episode| episode_priority(episode))?;
    let primary_episode_id = primary_episode.episode_id.clone();
    let trigger = events
        .iter()
        .find(|event| event.id == primary_episode.primary_signal.event_ref)?;
    let trigger_index = events.iter().position(|event| event.id == trigger.id)?;
    let (taxonomy, summary, expected, actual, counterfactual, targets, commands, confidence) =
        classify_failure(events, trigger_index, trigger, source_digest);
    let selected = diagnostic_slice_events(events, trigger.id, &episodes, 512);
    let mut artifact_refs = HashSet::new();
    let mut evidence_refs = HashSet::new();
    for event in &selected.events {
        if let Some(artifact_id) = event.payload_ref {
            artifact_refs.insert(artifact_id);
        }
        collect_artifact_ids(&event.payload, None, &mut artifact_refs);
        collect_evidence_ids(&event.payload, &mut evidence_refs);
    }
    let event_refs = selected
        .events
        .iter()
        .map(|event| event.id)
        .collect::<Vec<_>>();
    let diagnosis_id = format!("diagnosis-{task_id}-{}", trigger.id);
    let diagnosis = FailureDiagnosis {
        diagnosis_id: diagnosis_id.clone(),
        source_task_id: task_id,
        taxonomy,
        summary,
        trigger_event_refs: vec![trigger.id],
        causal_event_refs: selected.causal_event_refs.clone(),
        expected_behavior: expected,
        actual_behavior: actual,
        counterfactual,
        confidence,
        code_targets: targets,
        regression_commands: commands,
        analyzer_version: "golutra-failure-diagnosis-v4".to_owned(),
        failure_episode_id: Some(primary_episode_id.clone()),
        revision: u32::try_from(
            events
                .iter()
                .filter(|event| event.event_type == RuntimeEventType::FailureDiagnosed)
                .count()
                .saturating_add(1),
        )
        .unwrap_or(u32::MAX),
        supersedes_diagnosis_id: events
            .iter()
            .rev()
            .find(|event| event.event_type == RuntimeEventType::FailureDiagnosed)
            .and_then(|event| event.payload.pointer("/record/diagnosis_id"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        created_at: chrono::Utc::now(),
    };
    let (continuation_pages, continuation_pages_truncated) =
        diagnostic_continuation_pages(events, &event_refs, 64);
    let slice = DiagnosticSlice {
        slice_id: format!("diagnostic-slice-{task_id}-{}", trigger.id),
        source_task_id: task_id,
        diagnosis: diagnosis.clone(),
        event_refs,
        causal_event_refs: selected.causal_event_refs,
        supporting_event_refs: selected.supporting_event_refs,
        artifact_refs: sorted(artifact_refs),
        evidence_refs: sorted(evidence_refs),
        omitted_event_count: u64::try_from(events.len().saturating_sub(selected.events.len()))
            .unwrap_or(u64::MAX),
        continuation_pages,
        continuation_pages_truncated,
        selection_strategy: "semantic_causal_frontier_then_lifecycle_and_temporal_context_v2"
            .to_owned(),
        complete: selected.causal_complete,
        generated_at: chrono::Utc::now(),
    };
    let mut episodes = episodes;
    for episode in &mut episodes {
        if episode.episode_id == primary_episode_id {
            episode.diagnosis_refs.push(diagnosis_id.clone());
            episode.diagnosis_refs.sort();
            episode.diagnosis_refs.dedup();
        } else if episode.status == FailureEpisodeStatus::Active {
            episode.status = FailureEpisodeStatus::Superseded;
            episode.superseded_by = Some(primary_episode_id.clone());
            episode.updated_at = diagnosis.created_at;
        }
    }
    let candidate = improvement_candidate(&diagnosis, &slice, &episodes);
    Some(FailureAnalysis {
        diagnosis,
        slice,
        episodes,
        candidate,
    })
}

pub(crate) fn task_failure_episodes(
    task_id: TaskId,
    events: &[RuntimeEvent],
) -> Vec<FailureEpisode> {
    failure_episodes(task_id, events)
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

fn failure_episodes(task_id: TaskId, events: &[RuntimeEvent]) -> Vec<FailureEpisode> {
    let mut episodes = Vec::<FailureEpisode>::new();
    let mut active_by_key = HashMap::<String, usize>::new();
    for event in events {
        if let Some((key, signals)) = failure_signals(event) {
            if let Some(index) = active_by_key.get(&key).copied() {
                for signal in signals {
                    push_episode_signal(&mut episodes[index], signal);
                }
                episodes[index].updated_at = event.timestamp;
            } else if let Some(primary_signal) = signals.first().cloned() {
                let mut episode = FailureEpisode {
                    episode_id: format!("failure-episode-{task_id}-{}", event.id),
                    source_task_id: task_id,
                    status: FailureEpisodeStatus::Active,
                    primary_signal,
                    producer_failures: Vec::new(),
                    self_check_failures: Vec::new(),
                    external_assertion_failures: Vec::new(),
                    diagnosis_refs: Vec::new(),
                    recovered_by: None,
                    superseded_by: None,
                    opened_at: event.timestamp,
                    updated_at: event.timestamp,
                };
                for signal in signals {
                    push_episode_signal(&mut episode, signal);
                }
                let index = episodes.len();
                episodes.push(episode);
                active_by_key.insert(key, index);
            }
        }
        for (key, summary) in recovery_signals(event) {
            if let Some(index) = active_by_key.remove(&key) {
                let episode = &mut episodes[index];
                episode.status = FailureEpisodeStatus::Recovered;
                episode.recovered_by = Some(FailureRecovery {
                    event_ref: event.id,
                    signal_key: key,
                    summary,
                });
                episode.updated_at = event.timestamp;
            }
        }
        if let Some(summary) = authoritative_success_summary(event) {
            let recovered_keys = active_by_key
                .iter()
                .filter(|(_, index)| episodes[**index].external_assertion_failures.is_empty())
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            for key in recovered_keys {
                let Some(index) = active_by_key.remove(&key) else {
                    continue;
                };
                let episode = &mut episodes[index];
                episode.status = FailureEpisodeStatus::Recovered;
                episode.recovered_by = Some(FailureRecovery {
                    event_ref: event.id,
                    signal_key: key,
                    summary: summary.clone(),
                });
                episode.updated_at = event.timestamp;
            }
        }
        if event.event_type.is_task_terminal()
            && event
                .payload
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| status != "completed")
            && active_by_key.is_empty()
        {
            let signal = failure_signal_ref(
                event,
                FailureSignalKind::Producer,
                "task:terminal".to_owned(),
                event
                    .payload
                    .get("summary")
                    .or_else(|| event.payload.get("error"))
                    .and_then(Value::as_str)
                    .unwrap_or("task reached a non-success terminal state")
                    .to_owned(),
            );
            let mut episode = FailureEpisode {
                episode_id: format!("failure-episode-{task_id}-{}", event.id),
                source_task_id: task_id,
                status: FailureEpisodeStatus::Active,
                primary_signal: signal.clone(),
                producer_failures: Vec::new(),
                self_check_failures: Vec::new(),
                external_assertion_failures: Vec::new(),
                diagnosis_refs: Vec::new(),
                recovered_by: None,
                superseded_by: None,
                opened_at: event.timestamp,
                updated_at: event.timestamp,
            };
            push_episode_signal(&mut episode, signal);
            episodes.push(episode);
            active_by_key.insert("task:terminal".to_owned(), episodes.len() - 1);
        }
    }
    episodes
}

fn authoritative_success_summary(event: &RuntimeEvent) -> Option<String> {
    match event.event_type {
        RuntimeEventType::VerificationCompleted
            if event
                .payload
                .pointer("/record/result")
                .and_then(Value::as_str)
                == Some("pass") =>
        {
            Some("terminal verification passed after earlier exploratory failures".to_owned())
        }
        RuntimeEventType::ExternalEvaluationIngested
            if event
                .payload
                .pointer("/record/verdict")
                .and_then(Value::as_str)
                == Some("pass") =>
        {
            Some("external evaluator passed the final task output".to_owned())
        }
        _ => None,
    }
}

fn failure_signals(event: &RuntimeEvent) -> Option<(String, Vec<FailureSignalRef>)> {
    match event.event_type {
        RuntimeEventType::ExternalEvaluationIngested => {
            let record = event.payload.get("record")?;
            if record.get("verdict").and_then(Value::as_str) == Some("pass") {
                return None;
            }
            let case_id = record
                .get("case_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let key = format!("external:{case_id}");
            let mut signals = record
                .get("assertions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|assertion| {
                    !assertion
                        .get("passed")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
                .map(|assertion| {
                    let name = assertion
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("external assertion");
                    let message = assertion
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("failed");
                    failure_signal_ref(
                        event,
                        FailureSignalKind::ExternalAssertion,
                        key.clone(),
                        format!("{name}: {message}"),
                    )
                })
                .collect::<Vec<_>>();
            if signals.is_empty() {
                signals.push(failure_signal_ref(
                    event,
                    FailureSignalKind::ExternalAssertion,
                    key.clone(),
                    format!("external evaluator failed case {case_id}"),
                ));
            }
            Some((key, signals))
        }
        RuntimeEventType::VerificationCompleted => {
            let result = event
                .payload
                .pointer("/record/result")
                .and_then(Value::as_str)?;
            (result != "pass").then(|| {
                let key = "self_check:verification".to_owned();
                (
                    key.clone(),
                    vec![failure_signal_ref(
                        event,
                        FailureSignalKind::SelfCheck,
                        key,
                        event
                            .payload
                            .get("summary")
                            .and_then(Value::as_str)
                            .unwrap_or("runtime verification did not pass")
                            .to_owned(),
                    )],
                )
            })
        }
        RuntimeEventType::ToolCompleted => {
            let status = event
                .payload
                .pointer("/envelope/status")
                .and_then(Value::as_str)?;
            (status != "ok").then(|| {
                let key = format!("tool:{}", tool_failure_signature(event));
                (
                    key.clone(),
                    vec![failure_signal_ref(
                        event,
                        FailureSignalKind::Producer,
                        key,
                        event
                            .payload
                            .pointer("/envelope/summary")
                            .and_then(Value::as_str)
                            .unwrap_or("tool execution failed")
                            .to_owned(),
                    )],
                )
            })
        }
        RuntimeEventType::ProviderFailed => {
            let provider = event
                .payload
                .get("provider_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let key = format!("provider:{provider}");
            Some((
                key.clone(),
                vec![failure_signal_ref(
                    event,
                    FailureSignalKind::Producer,
                    key,
                    event
                        .payload
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("provider request failed")
                        .to_owned(),
                )],
            ))
        }
        RuntimeEventType::LoopGuardTriggered => {
            let trigger = event
                .payload
                .get("trigger")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let key = format!("control:{trigger}");
            Some((
                key.clone(),
                vec![failure_signal_ref(
                    event,
                    FailureSignalKind::Producer,
                    key,
                    event
                        .payload
                        .get("summary")
                        .or_else(|| event.payload.get("reason"))
                        .and_then(Value::as_str)
                        .unwrap_or("runtime loop guard stopped execution")
                        .to_owned(),
                )],
            ))
        }
        _ => None,
    }
}

fn recovery_signals(event: &RuntimeEvent) -> Vec<(String, String)> {
    match event.event_type {
        RuntimeEventType::ExternalEvaluationIngested
            if event
                .payload
                .pointer("/record/verdict")
                .and_then(Value::as_str)
                == Some("pass") =>
        {
            let case_id = event
                .payload
                .pointer("/record/case_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            vec![
                (
                    format!("external:{case_id}"),
                    format!("external evaluator passed case {case_id}"),
                ),
                (
                    "self_check:verification".to_owned(),
                    format!("external evaluator passed case {case_id}"),
                ),
            ]
        }
        RuntimeEventType::VerificationCompleted
            if event
                .payload
                .pointer("/record/result")
                .and_then(Value::as_str)
                == Some("pass") =>
        {
            vec![(
                "self_check:verification".to_owned(),
                "runtime verification passed".to_owned(),
            )]
        }
        RuntimeEventType::ToolCompleted
            if event
                .payload
                .pointer("/envelope/status")
                .and_then(Value::as_str)
                == Some("ok") =>
        {
            vec![(
                format!("tool:{}", tool_failure_signature(event)),
                "a later equivalent tool operation succeeded".to_owned(),
            )]
        }
        RuntimeEventType::ProviderCompleted => {
            let provider = event
                .payload
                .get("provider_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            vec![(
                format!("provider:{provider}"),
                "a later provider request completed".to_owned(),
            )]
        }
        _ => Vec::new(),
    }
}

fn failure_signal_ref(
    event: &RuntimeEvent,
    kind: FailureSignalKind,
    signal_key: String,
    summary: String,
) -> FailureSignalRef {
    let mut artifacts = HashSet::new();
    let mut evidence = HashSet::new();
    if let Some(artifact_id) = event.payload_ref {
        artifacts.insert(artifact_id);
    }
    collect_artifact_ids(&event.payload, None, &mut artifacts);
    collect_evidence_ids(&event.payload, &mut evidence);
    FailureSignalRef {
        event_ref: event.id,
        kind,
        signal_key,
        summary,
        evidence_refs: sorted(evidence),
        artifact_refs: sorted(artifacts),
    }
}

fn push_episode_signal(episode: &mut FailureEpisode, signal: FailureSignalRef) {
    let target = match signal.kind {
        FailureSignalKind::Producer => &mut episode.producer_failures,
        FailureSignalKind::SelfCheck => &mut episode.self_check_failures,
        FailureSignalKind::ExternalAssertion => &mut episode.external_assertion_failures,
    };
    if !target.contains(&signal) {
        target.push(signal);
    }
}

fn episode_priority(episode: &FailureEpisode) -> u8 {
    if !episode.external_assertion_failures.is_empty() {
        4
    } else if !episode.self_check_failures.is_empty() {
        3
    } else if episode
        .producer_failures
        .iter()
        .any(|signal| signal.signal_key.starts_with("control:"))
    {
        2
    } else {
        1
    }
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
    if trigger.event_type == RuntimeEventType::ExternalEvaluationIngested {
        let record = trigger.payload.get("record").unwrap_or(&Value::Null);
        let evaluator = record
            .get("evaluator_id")
            .and_then(Value::as_str)
            .unwrap_or("external evaluator");
        let case_id = record
            .get("case_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let failed_assertions = record
            .get("assertions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|assertion| {
                !assertion
                    .get("passed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
            .map(|assertion| {
                format!(
                    "{}: {}",
                    assertion
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("assertion"),
                    assertion
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("failed")
                )
            })
            .collect::<Vec<_>>();
        let actual = if failed_assertions.is_empty() {
            format!("{evaluator} returned a failing verdict for {case_id}")
        } else {
            failed_assertions.join("; ")
        };
        let terminal_cause = record
            .get("terminal_cause")
            .filter(|value| !value.is_null());
        let terminal_code = terminal_cause
            .and_then(|cause| cause.get("code"))
            .and_then(Value::as_str);
        let terminal_message = terminal_cause
            .and_then(|cause| cause.get("message"))
            .and_then(Value::as_str);
        let failed_phase = record
            .get("phases")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|phase| {
                phase
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| matches!(status, "failed" | "timed_out" | "error"))
            });
        let pipeline_failure = terminal_code.is_some_and(|code| code != "assertion_failed")
            || (terminal_code.is_none() && failed_assertions.is_empty() && failed_phase.is_some());
        if pipeline_failure {
            let phase_id = failed_phase
                .and_then(|phase| phase.get("phase_id"))
                .and_then(Value::as_str)
                .or_else(|| {
                    terminal_cause
                        .and_then(|cause| cause.get("phase_id"))
                        .and_then(Value::as_str)
                })
                .unwrap_or("unknown phase");
            let phase_status = failed_phase
                .and_then(|phase| phase.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("failed");
            let cause_code = terminal_code.unwrap_or(phase_status);
            let mut actual_parts = vec![format!(
                "{evaluator} pipeline phase {phase_id} ended as {phase_status}"
            )];
            if let Some(message) = terminal_message {
                actual_parts.push(message.to_owned());
            }
            if !failed_assertions.is_empty() {
                actual_parts.push(actual);
            }
            return (
                FailureTaxonomy {
                    domain: FailureDomain::ExternalEvaluation,
                    code: format!(
                        "{}_{}",
                        normalized_code(evaluator),
                        normalized_code(cause_code)
                    ),
                },
                format!(
                    "{evaluator} could not produce trustworthy assertions for case {case_id}: {}",
                    actual_parts.join("; ")
                ),
                "the external evaluator completes its pipeline and returns trustworthy assertions"
                    .to_owned(),
                actual_parts.join("; "),
                "inspect the imported evaluator artifacts, repair or reprovision the evaluator pipeline, and rerun the same case; do not change task output until an assertion identifies an output defect"
                    .to_owned(),
                vec![CodeTargetRef {
                    crate_name: "evaluation-harness".to_owned(),
                    module_path: evaluator.to_owned(),
                    symbol: None,
                    source_path: None,
                    source_digest,
                    owner: "external-evaluator".to_owned(),
                }],
                vec![format!("rerun external evaluator {evaluator} case {case_id}")],
                96,
            );
        }
        return (
            FailureTaxonomy {
                domain: FailureDomain::ExternalEvaluation,
                code: format!("{}_assertion_failed", normalized_code(evaluator)),
            },
            format!("{evaluator} rejected case {case_id}: {actual}"),
            "all external evaluator assertions pass against the produced workspace result"
                .to_owned(),
            actual,
            "inspect the imported evaluator artifacts, correct the produced workspace output, and rerun the same evaluator case"
                .to_owned(),
            vec![CodeTargetRef {
                crate_name: "workspace".to_owned(),
                module_path: case_id.to_owned(),
                symbol: None,
                source_path: None,
                source_digest,
                owner: "task-workspace".to_owned(),
            }],
            vec![format!("rerun external evaluator {evaluator} case {case_id}")],
            98,
        );
    }
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
    if trigger.event_type == RuntimeEventType::ProviderFailed {
        let provider = trigger
            .payload
            .get("provider_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return (
            FailureTaxonomy {
                domain: FailureDomain::Provider,
                code: format!("{}_request_failed", normalized_code(provider)),
            },
            format!("provider {provider} failed without recovery"),
            "the provider request completes or a configured fallback succeeds".to_owned(),
            trigger
                .payload
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("provider request failed")
                .to_owned(),
            "replay the request with the same provider contract and verify retry/fallback handling"
                .to_owned(),
            vec![CodeTargetRef {
                crate_name: "golutra-llm".to_owned(),
                module_path: provider.to_owned(),
                symbol: None,
                source_path: Some("crates/golutra-llm/src".to_owned()),
                source_digest,
                owner: "provider".to_owned(),
            }],
            vec!["cargo test -p golutra-llm".to_owned()],
            85,
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

fn normalized_code(value: &str) -> String {
    let mut output = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    while output.contains("__") {
        output = output.replace("__", "_");
    }
    output.trim_matches('_').to_owned()
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

struct DiagnosticEventSelection<'a> {
    events: Vec<&'a RuntimeEvent>,
    causal_event_refs: Vec<EventId>,
    supporting_event_refs: Vec<EventId>,
    causal_complete: bool,
}

fn causal_distances(
    events: &[RuntimeEvent],
    roots: &HashSet<EventId>,
) -> (HashMap<EventId, usize>, bool) {
    let by_id = events
        .iter()
        .map(|event| (event.id, event))
        .collect::<HashMap<_, _>>();
    let mut pending = roots
        .iter()
        .copied()
        .map(|event_id| (event_id, 0_usize))
        .collect::<VecDeque<_>>();
    let mut distances = HashMap::new();
    let mut missing_link = false;
    while let Some((event_id, distance)) = pending.pop_front() {
        if distances
            .get(&event_id)
            .is_some_and(|known| *known <= distance)
        {
            continue;
        }
        let Some(event) = by_id.get(&event_id) else {
            missing_link = true;
            continue;
        };
        distances.insert(event_id, distance);
        pending.extend(
            event
                .causal_links
                .iter()
                .filter(|link| link.relation != CausalRelation::Parent)
                .map(|link| (link.event_id, distance.saturating_add(1))),
        );
    }
    (distances, missing_link)
}

fn diagnostic_slice_events<'a>(
    events: &'a [RuntimeEvent],
    trigger_id: EventId,
    episodes: &[FailureEpisode],
    limit: usize,
) -> DiagnosticEventSelection<'a> {
    let mut root_ids = HashSet::from([trigger_id]);
    root_ids.extend(
        episodes
            .iter()
            .filter(|episode| episode.status == FailureEpisodeStatus::Active)
            .flat_map(|episode| {
                std::iter::once(episode.primary_signal.event_ref)
                    .chain(
                        episode
                            .producer_failures
                            .iter()
                            .map(|signal| signal.event_ref),
                    )
                    .chain(
                        episode
                            .self_check_failures
                            .iter()
                            .map(|signal| signal.event_ref),
                    )
                    .chain(
                        episode
                            .external_assertion_failures
                            .iter()
                            .map(|signal| signal.event_ref),
                    )
                    .chain(
                        episode
                            .recovered_by
                            .iter()
                            .map(|recovery| recovery.event_ref),
                    )
            }),
    );
    let (causal_distances, missing_causal_link) = causal_distances(events, &root_ids);
    let mut supporting_ids = HashSet::new();
    if let Some(trigger_index) = events.iter().position(|event| event.id == trigger_id) {
        let start = trigger_index.saturating_sub(64);
        let end = trigger_index.saturating_add(33).min(events.len());
        supporting_ids.extend(events[start..end].iter().map(|event| event.id));
    }
    for event_type in [
        RuntimeEventType::ContextSnapshotCreated,
        RuntimeEventType::VerificationCompleted,
        RuntimeEventType::LoopDecided,
        RuntimeEventType::TaskCompleted,
        RuntimeEventType::TaskAborted,
        RuntimeEventType::TaskInterrupted,
        RuntimeEventType::TaskUncertain,
    ] {
        if let Some(event) = events
            .iter()
            .rev()
            .find(|event| event.event_type == event_type)
        {
            supporting_ids.insert(event.id);
        }
    }
    let trigger_sequence = events
        .iter()
        .find(|event| event.id == trigger_id)
        .map_or(u64::MAX, |event| event.sequence_no);
    let mut candidates = events
        .iter()
        .filter(|event| {
            causal_distances.contains_key(&event.id) || supporting_ids.contains(&event.id)
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|event| {
        let priority = if event.id == trigger_id {
            0
        } else if root_ids.contains(&event.id) {
            1
        } else if causal_distances.contains_key(&event.id) {
            2
        } else if matches!(
            event.event_type,
            RuntimeEventType::ContextSnapshotCreated
                | RuntimeEventType::VerificationCompleted
                | RuntimeEventType::LoopDecided
                | RuntimeEventType::TaskCompleted
                | RuntimeEventType::TaskAborted
                | RuntimeEventType::TaskInterrupted
                | RuntimeEventType::TaskUncertain
        ) {
            3
        } else {
            4
        };
        (
            priority,
            causal_distances
                .get(&event.id)
                .copied()
                .unwrap_or(usize::MAX),
            event.sequence_no.abs_diff(trigger_sequence),
            event.sequence_no,
        )
    });
    candidates.truncate(limit);
    let selected_ids = candidates
        .iter()
        .map(|event| event.id)
        .collect::<HashSet<_>>();
    let causal_complete = !missing_causal_link
        && causal_distances
            .keys()
            .all(|event_id| selected_ids.contains(event_id));
    let mut causal_event_refs = candidates
        .iter()
        .filter(|event| causal_distances.contains_key(&event.id))
        .map(|event| event.id)
        .collect::<Vec<_>>();
    let mut supporting_event_refs = candidates
        .iter()
        .filter(|event| !causal_distances.contains_key(&event.id))
        .map(|event| event.id)
        .collect::<Vec<_>>();
    candidates.sort_by_key(|event| event.sequence_no);
    let sequence_by_id = events
        .iter()
        .map(|event| (event.id, event.sequence_no))
        .collect::<HashMap<_, _>>();
    causal_event_refs.sort_by_key(|event_id| sequence_by_id.get(event_id).copied());
    supporting_event_refs.sort_by_key(|event_id| sequence_by_id.get(event_id).copied());
    DiagnosticEventSelection {
        events: candidates,
        causal_event_refs,
        supporting_event_refs,
        causal_complete,
    }
}

fn diagnostic_continuation_pages(
    events: &[RuntimeEvent],
    selected_event_refs: &[EventId],
    max_pages: usize,
) -> (Vec<DiagnosticSliceContinuation>, bool) {
    let selected = selected_event_refs.iter().copied().collect::<HashSet<_>>();
    let mut pages = Vec::new();
    let mut active: Option<(Option<u64>, u64, u64)> = None;
    let mut previous_sequence = None;
    for event in events {
        if selected.contains(&event.id) {
            if let Some((after_sequence_no, through_sequence_no, omitted_event_count)) =
                active.take()
            {
                pages.push(DiagnosticSliceContinuation {
                    after_sequence_no,
                    through_sequence_no,
                    omitted_event_count,
                });
            }
        } else if let Some((_, through_sequence_no, omitted_event_count)) = active.as_mut() {
            *through_sequence_no = event.sequence_no;
            *omitted_event_count = omitted_event_count.saturating_add(1);
        } else {
            active = Some((previous_sequence, event.sequence_no, 1));
        }
        previous_sequence = Some(event.sequence_no);
    }
    if let Some((after_sequence_no, through_sequence_no, omitted_event_count)) = active {
        pages.push(DiagnosticSliceContinuation {
            after_sequence_no,
            through_sequence_no,
            omitted_event_count,
        });
    }
    let truncated = pages.len() > max_pages;
    if truncated {
        let overflow = pages.split_off(max_pages.saturating_sub(1));
        if let (Some(first), Some(last)) = (overflow.first(), overflow.last()) {
            pages.push(DiagnosticSliceContinuation {
                after_sequence_no: first.after_sequence_no,
                through_sequence_no: last.through_sequence_no,
                omitted_event_count: overflow.iter().fold(0_u64, |total, page| {
                    total.saturating_add(page.omitted_event_count)
                }),
            });
        }
    }
    (pages, truncated)
}

fn improvement_candidate(
    diagnosis: &FailureDiagnosis,
    slice: &DiagnosticSlice,
    episodes: &[FailureEpisode],
) -> ImprovementCandidate {
    let target = diagnosis.code_targets.first();
    let target_id = target.map(|target| {
        target.source_path.clone().unwrap_or_else(|| {
            target.symbol.as_ref().map_or_else(
                || target.module_path.clone(),
                |symbol| format!("{}::{symbol}", target.module_path),
            )
        })
    });
    let mut evidence_refs = slice.evidence_refs.clone();
    evidence_refs.extend(episodes.iter().flat_map(|episode| {
        episode
            .external_assertion_failures
            .iter()
            .flat_map(|signal| signal.evidence_refs.iter().copied())
    }));
    evidence_refs.sort();
    evidence_refs.dedup();
    let mut causal_evidence_refs = vec![
        diagnosis.diagnosis_id.clone(),
        slice.slice_id.clone(),
        format!("replay-{}", diagnosis.source_task_id),
    ];
    causal_evidence_refs.extend(episodes.iter().map(|episode| episode.episode_id.clone()));
    causal_evidence_refs.sort();
    causal_evidence_refs.dedup();
    let mut benchmark_refs = episodes
        .iter()
        .flat_map(|episode| episode.external_assertion_failures.iter())
        .map(|signal| signal.signal_key.clone())
        .collect::<Vec<_>>();
    if benchmark_refs.is_empty() {
        benchmark_refs.push(format!("benchmark-{}", diagnosis.source_task_id));
    }
    benchmark_refs.sort();
    benchmark_refs.dedup();
    ImprovementCandidate {
        id: format!("candidate-{}", diagnosis.source_task_id),
        source_task_id: diagnosis.source_task_id,
        source_failure_ids: diagnosis
            .failure_episode_id
            .iter()
            .cloned()
            .chain(std::iter::once(diagnosis.diagnosis_id.clone()))
            .collect(),
        target_type: target.map_or_else(
            || "runtime_or_workspace".to_owned(),
            |target| match target.owner.as_str() {
                "task-workspace" => "workspace_code".to_owned(),
                "external-evaluator" => "evaluation_harness".to_owned(),
                _ => "runtime_code".to_owned(),
            },
        ),
        target_id,
        proposed_change: format!(
            "{}. Observed failure: {}",
            diagnosis.counterfactual, diagnosis.actual_behavior
        ),
        expected_effect: diagnosis.expected_behavior.clone(),
        risk_level: if diagnosis.taxonomy.domain == FailureDomain::ExternalEvaluation {
            CandidateRisk::Medium
        } else {
            CandidateRisk::Low
        },
        evidence_refs,
        causal_evidence_refs,
        benchmark_refs,
        rollback_plan: if target.is_some_and(|target| target.owner == "external-evaluator") {
            "revert evaluator integration changes and preserve the task workspace".to_owned()
        } else {
            "revert the candidate patch and restore the pre-change checkpoint".to_owned()
        },
        diagnosis_ref: Some(diagnosis.diagnosis_id.clone()),
        proposed_commands: diagnosis.regression_commands.clone(),
        validation_plan: vec![
            diagnosis.expected_behavior.clone(),
            "rerun the originating case and compare verification/evaluator evidence".to_owned(),
        ],
        status: CandidateStatus::Proposed,
    }
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
            if key == "evidence_refs" || key.ends_with("_evidence_refs") {
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use golutra_core::{CausalContext, EvidenceId};
    use golutra_protocol::RuntimeEventSource;
    use serde_json::json;

    use super::*;

    fn event(
        sequence_no: u64,
        task_id: TaskId,
        event_type: RuntimeEventType,
        payload: Value,
    ) -> RuntimeEvent {
        RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: CausalContext::default(),
            causal_links: Vec::new(),
            id: EventId::new(),
            sequence_no,
            session_id: golutra_core::SessionId::new(),
            turn_id: None,
            task_id: Some(task_id),
            parent_event_id: None,
            event_type,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Runtime,
            payload,
            payload_ref: None,
            durable: true,
        }
    }

    #[test]
    fn later_equivalent_tool_success_recovers_the_failure_episode() {
        let task_id = TaskId::new();
        let failed = event(
            1,
            task_id,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {
                    "tool_name": "shell",
                    "status": "error",
                    "summary": "command failed",
                    "structured_facts": {"command": ["cargo", "test"]}
                }
            }),
        );
        let recovered = event(
            2,
            task_id,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {
                    "tool_name": "shell",
                    "status": "ok",
                    "summary": "command passed",
                    "structured_facts": {"command": ["cargo", "test"]}
                }
            }),
        );

        let episodes = task_failure_episodes(task_id, &[failed, recovered.clone()]);

        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].status, FailureEpisodeStatus::Recovered);
        assert_eq!(
            episodes[0]
                .recovered_by
                .as_ref()
                .map(|recovery| recovery.event_ref),
            Some(recovered.id)
        );
        assert!(diagnose_task(task_id, &[recovered], None).is_none());
    }

    #[test]
    fn passing_verification_recovers_non_equivalent_exploratory_failures() {
        let task_id = TaskId::new();
        let failed = event(
            1,
            task_id,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {
                    "tool_name": "shell",
                    "status": "error",
                    "summary": "optional dependency was unavailable",
                    "structured_facts": {"command": ["which", "duckdb"]}
                }
            }),
        );
        let verified = event(
            2,
            task_id,
            RuntimeEventType::VerificationCompleted,
            json!({"summary": "verification passed", "record": {"result": "pass"}}),
        );

        let events = vec![failed, verified.clone()];
        let episodes = task_failure_episodes(task_id, &events);

        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].status, FailureEpisodeStatus::Recovered);
        assert_eq!(
            episodes[0]
                .recovered_by
                .as_ref()
                .map(|recovery| recovery.event_ref),
            Some(verified.id)
        );
        assert!(diagnose_task(task_id, &events, None).is_none());
    }

    #[test]
    fn internal_verification_does_not_recover_external_assertion_failure() {
        let task_id = TaskId::new();
        let external = event(
            1,
            task_id,
            RuntimeEventType::ExternalEvaluationIngested,
            json!({
                "record": {
                    "evaluator_id": "terminal-bench",
                    "case_id": "case-a",
                    "verdict": "fail",
                    "assertions": [{"name": "output", "passed": false, "message": "wrong"}]
                }
            }),
        );
        let verified = event(
            2,
            task_id,
            RuntimeEventType::VerificationCompleted,
            json!({"summary": "runtime verification passed", "record": {"result": "pass"}}),
        );

        let episodes = task_failure_episodes(task_id, &[external, verified]);

        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].status, FailureEpisodeStatus::Active);
    }

    #[test]
    fn external_failure_revises_and_supersedes_the_self_check_diagnosis() {
        let task_id = TaskId::new();
        let verification = event(
            1,
            task_id,
            RuntimeEventType::VerificationCompleted,
            json!({
                "summary": "runtime verification failed",
                "record": {"result": "fail"}
            }),
        );
        let first = diagnose_task(task_id, std::slice::from_ref(&verification), None)
            .expect("initial diagnosis");
        let diagnosed = event(
            2,
            task_id,
            RuntimeEventType::FailureDiagnosed,
            json!({"record": first.diagnosis}),
        );
        let evidence_id = EvidenceId::new();
        let artifact_id = ArtifactId::new();
        let external = event(
            3,
            task_id,
            RuntimeEventType::ExternalEvaluationIngested,
            json!({
                "record": {
                    "evaluator_id": "terminal-bench",
                    "case_id": "csv-to-parquet",
                    "verdict": "fail",
                    "assertions": [{
                        "name": "parquet_dtype",
                        "passed": false,
                        "message": "column dtype does not match",
                        "evidence_refs": ["results.json"]
                    }],
                    "imported_artifacts": [{
                        "source_ref": "results.json",
                        "artifact_ref": artifact_id,
                        "checksum": "sha256:evidence",
                        "size_bytes": 10
                    }],
                    "imported_evidence_refs": [evidence_id]
                }
            }),
        );
        let events = vec![verification, diagnosed, external];

        let revised = diagnose_task(task_id, &events, None).expect("revised diagnosis");

        assert_eq!(
            revised.diagnosis.taxonomy.domain,
            FailureDomain::ExternalEvaluation
        );
        assert_eq!(revised.diagnosis.revision, 2);
        assert_eq!(
            revised.diagnosis.supersedes_diagnosis_id.as_deref(),
            Some(first.diagnosis.diagnosis_id.as_str())
        );
        let active = revised
            .episodes
            .iter()
            .find(|episode| episode.status == FailureEpisodeStatus::Active)
            .expect("active external episode");
        assert!(!active.external_assertion_failures.is_empty());
        assert_eq!(
            active.external_assertion_failures[0].evidence_refs,
            vec![evidence_id]
        );
        assert!(revised.episodes.iter().any(|episode| {
            episode.status == FailureEpisodeStatus::Superseded
                && episode.superseded_by.as_deref() == Some(active.episode_id.as_str())
        }));
        assert!(revised.candidate.evidence_refs.contains(&evidence_id));
        assert!(revised.slice.artifact_refs.contains(&artifact_id));
    }

    #[test]
    fn external_pipeline_failure_does_not_blame_task_workspace_output() {
        let task_id = TaskId::new();
        let external = event(
            1,
            task_id,
            RuntimeEventType::ExternalEvaluationIngested,
            json!({
                "record": {
                    "evaluator_id": "terminal-bench",
                    "case_id": "csv-to-parquet",
                    "verdict": "fail",
                    "assertions": [{
                        "name": "harness_failure_mode",
                        "passed": false,
                        "message": "test_timeout"
                    }],
                    "phases": [{
                        "phase_id": "terminal-bench:test",
                        "kind": "test",
                        "status": "timed_out"
                    }],
                    "terminal_cause": {
                        "code": "test_timeout",
                        "phase_id": "terminal-bench:test",
                        "message": "Terminal-Bench test phase timed out",
                        "retryable": true
                    }
                }
            }),
        );

        let analysis = diagnose_task(task_id, &[external], None).expect("diagnosis");

        assert_eq!(
            analysis.diagnosis.taxonomy.code,
            "terminal_bench_test_timeout"
        );
        assert_eq!(
            analysis.diagnosis.code_targets[0].owner,
            "external-evaluator"
        );
        assert!(
            analysis
                .diagnosis
                .counterfactual
                .contains("do not change task output")
        );
        assert_eq!(analysis.candidate.target_type, "evaluation_harness");
    }

    #[test]
    fn diagnostic_slice_prioritizes_causal_history_and_publishes_trace_pages() {
        let task_id = TaskId::new();
        let causal_root = event(
            1,
            task_id,
            RuntimeEventType::ProviderFailed,
            json!({"summary": "root provider failure"}),
        );
        let causal_root_id = causal_root.id;
        let mut events = vec![causal_root];
        let mut previous_id = causal_root_id;
        for sequence_no in 2..700 {
            let mut unrelated = event(
                sequence_no,
                task_id,
                RuntimeEventType::StepCompleted,
                json!({"summary": "unrelated history"}),
            );
            unrelated.parent_event_id = Some(previous_id);
            unrelated.causal_links.push(golutra_core::CausalLink {
                event_id: previous_id,
                relation: CausalRelation::Parent,
            });
            previous_id = unrelated.id;
            events.push(unrelated);
        }
        let mut trigger = event(
            700,
            task_id,
            RuntimeEventType::VerificationCompleted,
            json!({"summary": "verification failed", "record": {"result": "fail"}}),
        );
        trigger.parent_event_id = Some(previous_id);
        trigger.causal_links.push(golutra_core::CausalLink {
            event_id: previous_id,
            relation: CausalRelation::Parent,
        });
        trigger.causal_links.push(golutra_core::CausalLink {
            event_id: causal_root_id,
            relation: CausalRelation::TriggeredBy,
        });
        let trigger_id = trigger.id;
        events.push(trigger);

        let analysis = diagnose_task(task_id, &events, None).expect("diagnosis");

        assert!(analysis.slice.event_refs.contains(&trigger_id));
        assert!(analysis.slice.causal_event_refs.contains(&causal_root_id));
        assert!(analysis.slice.causal_event_refs.contains(&trigger_id));
        assert_eq!(analysis.slice.causal_event_refs.len(), 2);
        assert_eq!(analysis.slice.supporting_event_refs.len(), 64);
        assert_eq!(analysis.slice.event_refs.len(), 66);
        assert_eq!(analysis.slice.omitted_event_count, 634);
        assert!(analysis.slice.complete);
        assert!(!analysis.slice.continuation_pages.is_empty());
        assert_eq!(
            analysis
                .slice
                .continuation_pages
                .iter()
                .map(|page| page.omitted_event_count)
                .sum::<u64>(),
            analysis.slice.omitted_event_count
        );
        assert_eq!(
            analysis.slice.selection_strategy,
            "semantic_causal_frontier_then_lifecycle_and_temporal_context_v2"
        );
    }
}
