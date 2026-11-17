use super::*;
use golutra_agent_core::{PolicyId, ToolCallId, ToolResultEnvelope, ToolResultStatus};

fn event(
    session_id: SessionId,
    call_id: ToolCallId,
    kind: RuntimeEventType,
    status: &str,
    duration: u64,
) -> RuntimeEvent {
    RuntimeEvent {
        schema_version: golutra_agent_core::RUNTIME_EVENT_SCHEMA_VERSION,
        id: EventId::new(),
        sequence_no: 0,
        session_id,
        turn_id: Some(TurnId::new()),
        task_id: Some(TaskId::new()),
        parent_event_id: None,
        causal_context: Default::default(),
        causal_links: vec![],
        event_type: kind,
        timestamp: chrono::Utc::now(),
        source: RuntimeEventSource::Tool,
        payload: json!({
            "summary":if kind==RuntimeEventType::BackgroundTaskUpdated {"actual completion"} else {"running receipt"},
            "envelope":{"tool_call_id":call_id,"tool_name":"browser_run","status":status,"raw_artifact_ref":null,
                "structured_facts":{"result_pending":kind==RuntimeEventType::ToolCompleted}},
            "metrics":{"duration_ms":duration,"output_bytes":duration*2},
            "changed_files":if kind==RuntimeEventType::BackgroundTaskUpdated {vec!["page.html"]} else {vec![]},
        }),
        payload_ref: None,
        durable: true,
    }
}

#[test]
fn terminal_background_facts_replace_receipts_in_both_completion_orders() {
    for terminal_first in [false, true] {
        let session = SessionId::new();
        let call = ToolCallId::new();
        let receipt = event(session, call, RuntimeEventType::ToolCompleted, "ok", 3);
        let terminal = event(
            session,
            call,
            RuntimeEventType::BackgroundTaskUpdated,
            "error",
            120,
        );
        let terminal_id = terminal.id;
        let events = if terminal_first {
            vec![terminal, receipt]
        } else {
            vec![receipt, terminal]
        };
        let effective = effective_tool_events(&events);
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].id, terminal_id);
        let summary = trajectory_summary(&events);
        assert_eq!(summary.tool_calls, 1);
        assert_eq!(summary.failed_tool_calls, 1);
        assert_eq!(summary.tool_duration_ms, 120);
        assert_eq!(summary.tool_output_bytes, 240);
        assert!(summary.workspace_changes_observed);
        assert_eq!(summary.failure_clusters.len(), 1);
        assert_eq!(summary.failure_clusters[0].failures, 1);
    }
}

#[test]
fn recovery_deduplicates_invocations_and_artifacts_without_counting_null_refs() {
    let session = SessionId::new();
    let first = ToolCallId::new();
    let second = ToolCallId::new();
    let artifact = ArtifactId::new();
    let mut completion = event(
        session,
        first,
        RuntimeEventType::BackgroundTaskUpdated,
        "ok",
        10,
    );
    completion.payload["tool_artifact_refs"] = json!([artifact, artifact]);
    let mut sync = event(session, second, RuntimeEventType::ToolCompleted, "ok", 20);
    sync.payload["envelope"]["raw_artifact_ref"] = json!(artifact);
    let receipt = event(session, first, RuntimeEventType::ToolCompleted, "ok", 1);
    let replay = event(
        session,
        first,
        RuntimeEventType::BackgroundTaskUpdated,
        "ok",
        10,
    );
    let events = vec![receipt, replay, completion, sync];
    let effective = effective_tool_events(&events);
    assert_eq!(effective.len(), 2);
    assert_eq!(tool_event_artifact_count(&effective), 1);
    let summary = trajectory_summary(&events);
    assert_eq!(summary.tool_calls, 2);
    assert_eq!(summary.tool_duration_ms, 30);
    assert_eq!(summary.failed_tool_calls, 0);
    let no_artifact = vec![event(
        session,
        ToolCallId::new(),
        RuntimeEventType::ToolCompleted,
        "ok",
        1,
    )];
    assert_eq!(
        tool_event_artifact_count(&effective_tool_events(&no_artifact)),
        0
    );
}

#[test]
fn malformed_legacy_tool_events_keep_their_own_identity() {
    let session = SessionId::new();
    let call = ToolCallId::new();
    let mut first = event(session, call, RuntimeEventType::ToolCompleted, "ok", 1);
    let mut second = event(session, call, RuntimeEventType::ToolCompleted, "ok", 1);
    first.payload["envelope"]
        .as_object_mut()
        .unwrap()
        .remove("tool_call_id");
    second.payload["envelope"]
        .as_object_mut()
        .unwrap()
        .remove("tool_call_id");
    assert_eq!(effective_tool_events(&[first, second]).len(), 2);
}

#[test]
fn live_report_metrics_count_each_call_and_tool_artifact_once() {
    let call = ToolCallId::new();
    let artifact = ArtifactRecord {
        artifact_id: ArtifactId::new(),
        session_id: SessionId::new(),
        turn_id: None,
        tool_call_id: Some(call),
        artifact_type: "test".into(),
        uri: "artifact://test".into(),
        checksum: "fixture".into(),
        size_bytes: 0,
        created_at: chrono::Utc::now(),
        producer: "test".into(),
        redaction_status: RedactionStatus::NotRequired,
        retention_policy: "test".into(),
        provenance_refs: vec![],
    };
    let receipt = ToolExecutionReport {
        envelope: ToolResultEnvelope {
            tool_call_id: call,
            tool_name: "browser_run".into(),
            status: ToolResultStatus::Ok,
            summary: "running".into(),
            structured_facts: json!({"result_pending":true}),
            model_visible_excerpt: None,
            raw_artifact_ref: None,
            evidence_refs: vec![],
            risk: "none".into(),
            verification_hint: None,
        },
        artifacts: vec![artifact],
        evidence: vec![],
        changed_files: vec![],
        artifact_contents: vec![],
        before_images: vec![],
        after_images: vec![],
        metrics: Default::default(),
        policy_evaluation: PolicyEvaluation {
            policy_ref: PolicyId::new(),
            subject: "test".into(),
            action: "read".into(),
            resource: "test".into(),
            decision: PolicyDecision::Allow,
            block_disposition: None,
            reason: "test".into(),
            evidence_refs: vec![],
        },
    };
    let mut completion = receipt.clone();
    completion.envelope.summary = "finished".into();
    completion.envelope.structured_facts = json!({"result_pending":false});
    assert_eq!(tool_report_counts(&[receipt, completion]), (1, 1));
}
