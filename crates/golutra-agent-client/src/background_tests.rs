use super::*;
use golutra_agent_core::{PolicyId, ToolCallId, ToolResultEnvelope, ToolResultStatus};

#[path = "background_evidence_tests.rs"]
mod evidence_tests;

fn fixture(host: &Arc<RuntimeHost>) -> (Arc<HostedBackgroundObserver>, ToolRequest) {
    let turn_id = TurnId::new();
    let task = HostedAgentTask {
        session_id: host.default_session_id(),
        task_id: TaskId::new(),
        turn_id,
        payload: json!({"prompt":"background fixture", "max_elapsed_ms":60_000}),
    };
    let request = ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: None,
        session_id: task.session_id,
        turn_id: Some(turn_id),
        tool_name: "shell".into(),
        arguments: json!({"command":"printf done", "background":true}),
    };
    (HostedBackgroundObserver::new(host, task), request)
}

fn report(request: &ToolRequest) -> ToolExecutionReport {
    ToolExecutionReport {
        envelope: ToolResultEnvelope {
            tool_call_id: request.tool_call_id,
            tool_name: request.tool_name.clone(),
            status: ToolResultStatus::Ok,
            summary: "done".into(),
            structured_facts: json!({"exit_code":0}),
            model_visible_excerpt: Some("done".into()),
            raw_artifact_ref: None,
            evidence_refs: Vec::new(),
            risk: "none".into(),
            verification_hint: None,
        },
        artifacts: Vec::new(),
        evidence: Vec::new(),
        changed_files: Vec::new(),
        artifact_contents: Vec::new(),
        before_images: Vec::new(),
        after_images: Vec::new(),
        metrics: Default::default(),
        policy_evaluation: PolicyEvaluation {
            policy_ref: PolicyId::new(),
            subject: "tool".into(),
            action: "shell".into(),
            resource: "fixture".into(),
            decision: PolicyDecision::Allow,
            block_disposition: None,
            reason: "fixture".into(),
            evidence_refs: Vec::new(),
        },
    }
}

#[tokio::test]
async fn background_completion_is_durable_quiet_and_idempotent() {
    let host = RuntimeHost::in_memory().await.unwrap();
    let (observer, request) = fixture(&host);
    observer.admit(&request).await.unwrap();
    observer
        .complete(&request, &report(&request))
        .await
        .unwrap();
    observer
        .complete(&request, &report(&request))
        .await
        .unwrap();
    let pending = host
        .storage
        .store
        .background()
        .pending_results(host.workspace_id, request.session_id, 128)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].status, BackgroundTaskStatus::Succeeded);
    let events = host
        .storage
        .repositories
        .events
        .load(request.session_id, None, None)
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::BackgroundTaskUpdated)
            .count(),
        1
    );
    assert!(!events.iter().any(|event| matches!(
        event.event_type,
        RuntimeEventType::AssistantMessage | RuntimeEventType::ToolCompleted
    )));
    let full_artifact: ArtifactId = pending[0].artifact_refs[0].parse().unwrap();
    assert!(
        host.storage
            .repositories
            .artifacts
            .bytes(full_artifact)
            .await
            .unwrap()
            .is_some()
    );
    assert!(observer.admit(&request).await.is_err());
    host.close().await.unwrap();
}

#[tokio::test]
async fn large_background_results_keep_full_artifact_and_bounded_inline_context() {
    let host = RuntimeHost::in_memory().await.unwrap();
    let (observer, request) = fixture(&host);
    observer.admit(&request).await.unwrap();
    let mut output = report(&request);
    output.envelope.summary = "完成".repeat(10_000);
    output.envelope.structured_facts = json!({
        "large":"字".repeat(60_000),
        "exit_code":17,
        "process_id":"retained-process",
        "terminal":true,
        "outcome_unknown":true,
        "automatic_retry":false,
        "workspace_changes_known":false,
    });
    output.envelope.status = ToolResultStatus::Error;
    output.envelope.model_visible_excerpt = Some("数据".repeat(30_000));
    let image_id = ArtifactId::new();
    let image_bytes = b"retained image fixture".to_vec();
    output.artifacts.push(ArtifactRecord {
        artifact_id: image_id,
        session_id: request.session_id,
        turn_id: request.turn_id,
        tool_call_id: Some(request.tool_call_id),
        artifact_type: "tool_image:image/png".into(),
        uri: format!("artifact://test/{image_id}"),
        checksum: format!("sha256:{:x}", Sha256::digest(&image_bytes)),
        size_bytes: image_bytes.len() as u64,
        created_at: chrono::Utc::now(),
        producer: "background-test".into(),
        redaction_status: RedactionStatus::NotRequired,
        retention_policy: "test".into(),
        provenance_refs: Vec::new(),
    });
    output
        .artifact_contents
        .push(golutra_agent_tools::ArtifactContent {
            artifact_id: image_id,
            bytes: image_bytes.clone(),
        });
    observer.complete(&request, &output).await.unwrap();
    let result = host
        .storage
        .store
        .background()
        .pending_results(host.workspace_id, request.session_id, 128)
        .await
        .unwrap()
        .remove(0);
    assert!(result.summary.len() <= MAX_BACKGROUND_SUMMARY_BYTES);
    assert!(
        result.output.model_visible_excerpt.as_ref().unwrap().len() <= MAX_INLINE_EXCERPT_BYTES
    );
    assert_eq!(result.output.structured_facts["inline_truncated"], true);
    for key in [
        "exit_code",
        "process_id",
        "terminal",
        "outcome_unknown",
        "automatic_retry",
        "workspace_changes_known",
    ] {
        assert_eq!(
            result.output.structured_facts[key], output.envelope.structured_facts[key],
            "{key}"
        );
    }
    assert_eq!(result.output.structured_facts["background_image_count"], 1);
    assert_eq!(
        result.output.structured_facts["background_image_artifacts"],
        json!([{"artifact_id":image_id,"mime_type":"image/png"}])
    );
    assert_eq!(
        host.storage
            .repositories
            .artifacts
            .bytes(image_id)
            .await
            .unwrap(),
        Some(image_bytes)
    );
    let full: ArtifactId = result.output.structured_facts["full_result_artifact_ref"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let full = host
        .storage
        .repositories
        .artifacts
        .bytes(full)
        .await
        .unwrap()
        .unwrap();
    let envelope: ToolResultEnvelope = serde_json::from_slice(&full).unwrap();
    assert_eq!(envelope, output.envelope);
    host.close().await.unwrap();
}

#[tokio::test]
async fn large_browser_result_keeps_recovery_facts_before_arbitrary_page_data() {
    let host = RuntimeHost::in_memory().await.unwrap();
    let (observer, mut request) = fixture(&host);
    request.tool_name = "browser_run".into();
    observer.admit(&request).await.unwrap();
    let mut output = report(&request);
    output.envelope.status = ToolResultStatus::Error;
    output.envelope.structured_facts = json!({
        "backend":"ego", "phase":"outcome_unknown", "code":"browser_outcome_unknown",
        "outcome_unknown":true, "automatic_retry":false, "requires_reload":true,
        "next_action":{"tool":"browser_run","arguments":{"observation":"snapshot"}},
    });
    for index in 0..64 {
        output.envelope.structured_facts[format!("a_page_data_{index:02}")] =
            json!("字".repeat(1024));
    }
    observer.complete(&request, &output).await.unwrap();
    let result = host
        .storage
        .store
        .background()
        .pending_results(host.workspace_id, request.session_id, 128)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(result.status, BackgroundTaskStatus::OutcomeUnknown);
    assert_eq!(result.output.structured_facts["inline_truncated"], true);
    assert!(
        serde_json::to_vec(&result.output.structured_facts)
            .unwrap()
            .len()
            <= MAX_INLINE_FACT_BYTES
    );
    for key in [
        "backend",
        "phase",
        "code",
        "outcome_unknown",
        "automatic_retry",
        "requires_reload",
        "next_action",
    ] {
        assert_eq!(
            result.output.structured_facts[key], output.envelope.structured_facts[key],
            "{key}"
        );
    }
    host.close().await.unwrap();
}

#[tokio::test]
async fn stopped_batch_stays_suppressed_and_new_work_gets_new_batch() {
    let host = RuntimeHost::in_memory().await.unwrap();
    let (observer, request) = fixture(&host);
    observer.admit(&request).await.unwrap();
    let repository = host.storage.store.background();
    let original = repository
        .find_task_by_tool_call(host.workspace_id, request.session_id, request.tool_call_id)
        .await
        .unwrap()
        .unwrap();
    host.suppress_background_continuation(request.session_id)
        .await
        .unwrap();
    observer
        .complete(&request, &report(&request))
        .await
        .unwrap();
    let original_batch = repository.batch(original.batch_id).await.unwrap().unwrap();
    assert!(original_batch.sealed);
    assert!(
        repository
            .claim_batch_wake(
                original.batch_id,
                original_batch.generation,
                "test",
                chrono::Utc::now(),
                chrono::Duration::seconds(10)
            )
            .await
            .unwrap()
            .is_none()
    );
    let next = ToolRequest {
        tool_call_id: ToolCallId::new(),
        ..request.clone()
    };
    observer.admit(&next).await.unwrap();
    let next_task = repository
        .find_task_by_tool_call(host.workspace_id, next.session_id, next.tool_call_id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(next_task.batch_id, original.batch_id);
    host.close().await.unwrap();
}

#[tokio::test]
async fn reconnect_recovers_abandoned_operations_without_dispatch() {
    let host = RuntimeHost::in_memory().await.unwrap();
    for (index, tool) in ["shell", "subagent"].into_iter().enumerate() {
        let (observer, mut request) = fixture(&host);
        request.tool_name = tool.into();
        observer.admit(&request).await.unwrap();
        let results = host
            .pending_background_results(request.session_id)
            .await
            .unwrap();
        assert_eq!(results.len(), index + 1);
        let result = results
            .iter()
            .find(|result| result.output.tool_call_id == request.tool_call_id)
            .unwrap();
        assert_eq!(result.status, BackgroundTaskStatus::OutcomeUnknown);
        assert_eq!(result.scope.turn_id, request.turn_id.unwrap());
        assert!(observer.admit(&request).await.is_err());
    }
    host.close().await.unwrap();
}

#[tokio::test]
async fn nested_subagent_background_results_stay_in_the_child_scope() {
    let host = RuntimeHost::in_memory().await.unwrap();
    let (parent, mut child_call) = fixture(&host);
    child_call.tool_name = "subagent".into();
    parent.admit(&child_call).await.unwrap();
    let child_task = HostedAgentTask {
        session_id: SessionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        payload: json!({"prompt":"nested background"}),
    };
    let child = HostedBackgroundObserver::new(&host, child_task.clone());
    let mut nested = child_call.clone();
    nested.tool_call_id = ToolCallId::new();
    nested.tool_name = "shell".into();
    nested.session_id = child_task.session_id;
    nested.turn_id = Some(child_task.turn_id);
    child.admit(&nested).await.unwrap();
    child.complete(&nested, &report(&nested)).await.unwrap();
    let repository = host.storage.store.background();
    assert!(
        repository
            .pending_results(host.workspace_id, child_call.session_id, 128)
            .await
            .unwrap()
            .is_empty()
    );
    let results = repository
        .pending_results(host.workspace_id, nested.session_id, 128)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].scope.task_id, child_task.task_id);
    parent
        .complete(&child_call, &report(&child_call))
        .await
        .unwrap();
    let parent_results = repository
        .pending_results(host.workspace_id, child_call.session_id, 128)
        .await
        .unwrap();
    assert_eq!(parent_results.len(), 1);
    assert_eq!(parent_results[0].output.tool_name, "subagent");
    assert_ne!(parent_results[0].batch_id, results[0].batch_id);
    host.close().await.unwrap();
}

#[tokio::test]
async fn live_background_owner_lease_prevents_recovery_after_foreground_owner_ends() {
    let workspace = tempfile::tempdir().unwrap();
    let host = RuntimeHost::ephemeral_for_cwd(workspace.path())
        .await
        .unwrap();
    let (observer, request) = fixture(&host);
    observer.admit(&request).await.unwrap();
    // No local task control or supervisor job exists in this fixture, just as a
    // different host sees only durable rows and the independent file lease.
    assert!(
        !host
            .execution
            .background_supervisor
            .has_pending(request.session_id, None)
    );
    assert!(matches!(
        host.try_acquire_session_lease(request.session_id).unwrap(),
        SessionLeaseAttempt::Acquired(_)
    ));
    assert!(matches!(
        host.try_acquire_background_lease(request.session_id, observer.task.task_id)
            .unwrap(),
        SessionLeaseAttempt::Busy
    ));
    assert!(
        host.pending_background_results(request.session_id)
            .await
            .unwrap()
            .is_empty()
    );
    let repository = host.storage.store.background();
    let task = repository
        .find_task_by_tool_call(host.workspace_id, request.session_id, request.tool_call_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.status, BackgroundTaskStatus::Running);
    // Reproduce the inherited open-description lifetime without racing fork/exec
    // in another test. The owner's last drop must release its flock explicitly.
    let inherited_descriptor = observer
        .state
        .lock()
        .await
        .owner_lease
        .as_ref()
        .unwrap()
        .0
        .try_clone()
        .unwrap();
    drop(observer);
    let recovered = host
        .pending_background_results(request.session_id)
        .await
        .unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].status, BackgroundTaskStatus::OutcomeUnknown);
    drop(inherited_descriptor);
    host.close().await.unwrap();
}

#[tokio::test]
async fn cancelling_one_background_batch_keeps_the_next_batch_wakeable() {
    let host = RuntimeHost::in_memory().await.unwrap();
    let (observer, first) = fixture(&host);
    observer.admit(&first).await.unwrap();
    observer
        .seal(first.session_id, first.turn_id)
        .await
        .unwrap();
    let second = ToolRequest {
        tool_call_id: ToolCallId::new(),
        turn_id: Some(TurnId::new()),
        ..first.clone()
    };
    observer.admit(&second).await.unwrap();
    observer
        .cancel(first.session_id, first.turn_id, first.tool_call_id)
        .await
        .unwrap();
    observer.complete(&first, &report(&first)).await.unwrap();
    observer.complete(&second, &report(&second)).await.unwrap();
    observer
        .seal(second.session_id, second.turn_id)
        .await
        .unwrap();
    let repository = host.storage.store.background();
    for (request, wakeable) in [(&first, false), (&second, true)] {
        let task = repository
            .find_task_by_tool_call(host.workspace_id, request.session_id, request.tool_call_id)
            .await
            .unwrap()
            .unwrap();
        let batch = repository.batch(task.batch_id).await.unwrap().unwrap();
        let claim = repository
            .claim_batch_wake(
                batch.batch_id,
                batch.generation,
                "test",
                chrono::Utc::now(),
                chrono::Duration::seconds(10),
            )
            .await
            .unwrap();
        assert_eq!(claim.is_some(), wakeable);
    }
    assert_eq!(
        repository
            .pending_results(host.workspace_id, first.session_id, 128)
            .await
            .unwrap()
            .len(),
        2
    );
    host.close().await.unwrap();
}

#[tokio::test]
async fn browser_handoff_retains_observation_without_automatic_wake() {
    let host = RuntimeHost::in_memory().await.unwrap();
    let (observer, request) = fixture(&host);
    observer.admit(&request).await.unwrap();
    let mut output = report(&request);
    output.envelope.structured_facts = json!({"phase":"waiting_for_user"});
    output.envelope.status = ToolResultStatus::Blocked;
    observer
        .handoff(request.session_id, request.turn_id)
        .await
        .unwrap();
    observer.complete(&request, &output).await.unwrap();
    let result = host
        .storage
        .store
        .background()
        .pending_results(host.workspace_id, request.session_id, 128)
        .await
        .unwrap()
        .remove(0);
    let batch = host
        .storage
        .store
        .background()
        .batch(result.batch_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        host.storage
            .store
            .background()
            .claim_batch_wake(
                batch.batch_id,
                batch.generation,
                "test",
                chrono::Utc::now(),
                chrono::Duration::seconds(10)
            )
            .await
            .unwrap()
            .is_none()
    );
    host.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn real_background_process_continues_same_task_and_commits_delivery_after_context() {
    let root = tempfile::tempdir().unwrap();
    let host = RuntimeHost::in_memory().await.unwrap();
    let (observer, _) = fixture(&host);
    let task = observer.task.clone();
    let transition = host
        .execution
        .lane_manager
        .lock()
        .await
        .start_task(
            host.workspace_id,
            task.session_id,
            task.task_id,
            task.turn_id,
            Actor {
                kind: golutra_agent_core::ActorKind::User,
                id: "background-test".into(),
            },
            host.next_sequence_no(),
        )
        .unwrap();
    host.record_event(transition.event).await.unwrap();
    let (handle, control) = golutra_agent_runtime::agent_execution_channel(8);
    let completed = tokio::spawn(async {});
    let (_, completion) = watch::channel(false);
    host.execution.task_controls.lock().await.insert(
        task.session_id,
        HostedTaskControl {
            task_id: task.task_id,
            allow_network: false,
            yolo: true,
            provider_settings: Default::default(),
            execution: handle,
            abort_handle: completed.abort_handle(),
            completion,
            delegation: None,
            _session_lease: None,
        },
    );
    let executor = ToolRuntime::new(WorkspacePolicy::new(root.path()).unwrap())
        .with_unrestricted_access(true)
        .with_process_supervisor(host.execution.process_supervisor.clone())
        .with_background_supervisor(host.execution.background_supervisor.clone(), observer)
        .unwrap();
    let provider = golutra_agent_llm::MockProvider::tool_call(
        "shell",
        json!({
            "command":"sleep 0.05; printf x >> effect.txt; printf 'background-proof\\n'",
            "background":true,"timeout_ms":5_000,
        }),
    );
    let harness = AgentHarness::new(
        provider,
        golutra_agent_context::ContextBuilder::default(),
        executor,
    );
    let recorder = crate::execution::HostedObservationRecorder::spawn(host.clone(), task.clone());
    let sender = recorder.sender();
    let run = ConfiguredAgentRun::new(AgentTaskRequest {
        session_id: task.session_id,
        task_id: task.task_id,
        turn_id: task.turn_id,
        objective: "Inspect the output of this background command.".into(),
        completion_criteria: Vec::new(),
        output_schema: None,
        touched_code: false,
        contributors: Vec::new(),
        tools: vec!["shell".into(), "background_task".into()],
    })
    .with_execution_mode(Some(golutra_agent_protocol::AgentExecutionMode::Open))
    .with_max_elapsed_ms(10_000);
    let outcome = tokio::time::timeout(
        Duration::from_secs(15),
        harness.execute_configured(run, control, move |event| {
            sender.send(event).unwrap();
        }),
    )
    .await
    .unwrap()
    .unwrap();
    recorder.close().await.unwrap();
    assert_eq!(outcome.final_turn_id, task.turn_id);
    assert_eq!(
        std::fs::read_to_string(root.path().join("effect.txt")).unwrap(),
        "x"
    );
    assert!(
        host.storage
            .store
            .background()
            .pending_results(host.workspace_id, task.session_id, 128)
            .await
            .unwrap()
            .is_empty()
    );
    let events = host
        .storage
        .repositories
        .events
        .load(task.session_id, None, None)
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::ToolStarted)
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::BackgroundTaskUpdated)
            .count(),
        1,
        "jobs: {:?}; stop: {:?}",
        host.execution.background_supervisor.list(task.session_id),
        outcome.runtime_stop_reason
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::BackgroundResultsConsumed)
            .count(),
        1
    );
    let consumed = events
        .iter()
        .position(|event| event.event_type == RuntimeEventType::BackgroundResultsConsumed)
        .unwrap();
    assert!(
        events[..consumed]
            .iter()
            .any(|event| event.event_type == RuntimeEventType::ContextSnapshotCreated)
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::ProviderStarted)
            .count(),
        3
    );
    // A route/model change may force ordinary history projection; typed results still survive.
    let fallback = host
        .background_results_for_context_fallback(task.session_id)
        .await
        .unwrap();
    assert_eq!(fallback.len(), 1);
    assert_eq!(fallback[0].status, BackgroundTaskStatus::Succeeded);
    host.clear_task_control(task.session_id, task.task_id).await;
    host.close().await.unwrap();
}
