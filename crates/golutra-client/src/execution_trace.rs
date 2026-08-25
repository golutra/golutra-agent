//! AgentLoop trace、artifact/checkpoint 持久化与失败终态适配。

use std::collections::HashSet;

use super::*;
use crate::observation_recorder::{CoalescingSummary, ObservationCommand, ObservationReceiver};

const MAX_INLINE_CHANGE_FILES: usize = 32;
const MAX_INLINE_DIFF_PREVIEWS: usize = 8;

pub(super) fn task_outcome_from_verification(
    task: &HostedAgentTask,
    status: TaskStatus,
    verification: &golutra_core::VerificationRecord,
) -> golutra_core::TaskOutcome {
    let defer_external_verification = task
        .payload
        .get("defer_external_verification")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    task_outcome_with_external_verification(
        status,
        verification,
        defer_external_verification,
        false,
    )
}

pub(super) fn task_outcome_with_external_verification(
    status: TaskStatus,
    verification: &golutra_core::VerificationRecord,
    defer_external_verification: bool,
    candidate_ready_for_external_verification: bool,
) -> golutra_core::TaskOutcome {
    let external_verification = if defer_external_verification {
        golutra_core::ExternalVerificationStatus::Pending
    } else {
        golutra_core::ExternalVerificationStatus::NotRequested
    };
    let outcome = if defer_external_verification && candidate_ready_for_external_verification {
        golutra_core::TaskOutcome::candidate_ready(verification)
    } else {
        golutra_core::TaskOutcome::from_verification(status, verification)
    };
    outcome.with_external_verification(external_verification)
}

#[derive(Clone)]
pub(crate) struct CanonicalFactRecorder {
    host: Arc<RuntimeHost>,
    task: HostedAgentTask,
}

impl CanonicalFactRecorder {
    pub(super) fn new(host: Arc<RuntimeHost>, task: HostedAgentTask) -> Self {
        Self { host, task }
    }

    pub(super) async fn commit_with_coalescing(
        &self,
        observation: RuntimeObservation,
        coalescing: CoalescingSummary,
    ) -> Result<(), ClientError> {
        self.host
            .record_trace_observation(&self.task, observation, coalescing)
            .await
    }

    pub(super) async fn drain(self, receiver: ObservationReceiver) -> Result<(), ClientError> {
        while let Some(command) = receiver.next().await {
            match command {
                ObservationCommand::Event {
                    observation,
                    coalescing,
                    ..
                } => {
                    self.commit_with_coalescing(*observation, coalescing)
                        .await?;
                }
                ObservationCommand::Flush(sender) => {
                    let _ = sender.send(Ok(()));
                }
            }
        }
        Ok(())
    }
}

impl RuntimeHost {
    async fn record_trace_observation(
        &self,
        task: &HostedAgentTask,
        trace_event: RuntimeObservation,
        coalescing: CoalescingSummary,
    ) -> Result<(), ClientError> {
        let (trace_event, context_artifacts) = match trace_event {
            AgentLoopTraceEvent::ContextSnapshotCaptured {
                mut snapshot,
                request,
            } => {
                let (redacted_artifact, redacted_bytes) =
                    context_request_artifact(task, &snapshot, &request)?;
                let (restricted_artifact, restricted_bytes) =
                    context_replay_request_artifact(task, &snapshot, &request)?;
                snapshot.redacted_request_artifact_ref = Some(redacted_artifact.artifact_id);
                snapshot.restricted_request_artifact_ref = Some(restricted_artifact.artifact_id);
                for contributor in &mut snapshot.contributor_manifest {
                    if contributor.included {
                        contributor.redacted_content_ref = Some(redacted_artifact.artifact_id);
                    }
                }
                (
                    AgentLoopTraceEvent::ContextSnapshot(snapshot),
                    vec![
                        (
                            redacted_artifact,
                            redacted_bytes,
                            "redacted_request_artifact_ref",
                        ),
                        (
                            restricted_artifact,
                            restricted_bytes,
                            "restricted_request_artifact_ref",
                        ),
                    ],
                )
            }
            AgentLoopTraceEvent::ContextAutoCompacted(record) => {
                let (artifact, bytes) = context_compaction_artifact(task, &record)?;
                (
                    AgentLoopTraceEvent::ContextAutoCompacted(record),
                    vec![(artifact, bytes, "replacement_context_artifact_ref")],
                )
            }
            trace_event => (trace_event, Vec::new()),
        };
        if let AgentLoopTraceEvent::ContextSnapshot(snapshot) = &trace_event {
            self.storage
                .repositories
                .artifacts
                .store_context(snapshot)
                .await?;
        }
        if let AgentLoopTraceEvent::VerificationPlanned(plan) = &trace_event {
            self.storage
                .repositories
                .artifacts
                .store_verification(plan)
                .await?;
        }
        if let AgentLoopTraceEvent::ToolCompleted(report) = &trace_event {
            return self.record_tool_report(task, report).await;
        }
        let queued_turn_start = if let Some(turn) = match &trace_event {
            AgentLoopTraceEvent::PendingTurnStarted(turn) => Some(turn),
            AgentLoopTraceEvent::PendingTurnStartedWithExecution(configured) => {
                Some(&configured.turn)
            }
            _ => None,
        } {
            Some((
                self.execution
                    .lane_manager
                    .lock()
                    .await
                    .prepare_queued_turn_start(task.session_id, turn.turn_id)?,
                turn.turn_id,
            ))
        } else {
            None
        };
        let active_turn_id = self
            .execution
            .lane_manager
            .lock()
            .await
            .lane(task.session_id)
            .and_then(|lane| lane.active_turn_id)
            .unwrap_or(task.turn_id);
        let event_turn_id = match &trace_event {
            AgentLoopTraceEvent::PendingTurnStarted(turn) => Some(turn.turn_id),
            AgentLoopTraceEvent::PendingTurnStartedWithExecution(configured) => {
                Some(configured.turn.turn_id)
            }
            AgentLoopTraceEvent::AssistantMessage { turn_id, .. } => Some(*turn_id),
            AgentLoopTraceEvent::ApprovalRequested(approval) => Some(approval.turn_id),
            AgentLoopTraceEvent::UserQuestionRequested(request) => Some(request.turn_id),
            AgentLoopTraceEvent::TokenUsageRecorded(record) => Some(record.turn_id),
            _ => Some(active_turn_id),
        };
        let provider_artifacts = match &trace_event {
            AgentLoopTraceEvent::ProviderCompleted { response, .. } => {
                vec![
                    (
                        provider_raw_artifact(task, active_turn_id, &response.raw_metadata)?,
                        "raw_metadata_ref",
                    ),
                    (
                        provider_response_replay_artifact(task, active_turn_id, response)?,
                        "response_artifact_ref",
                    ),
                ]
            }
            _ => Vec::new(),
        };
        if let Some((event_type, source, payload)) = trace_event_payload(trace_event) {
            let mut event = agent_event(self.next_sequence_no(), task, event_type, source, payload);
            if matches!(
                event.event_type,
                RuntimeEventType::ProviderStreamed | RuntimeEventType::ToolProgress
            ) {
                let coalesced = coalescing.omitted_events > 0;
                event.payload["coalescing"] = json!({
                    "applied": coalesced,
                    "omitted_event_count": coalescing.omitted_events,
                    "omitted_byte_count": coalescing.omitted_bytes,
                });
                if coalesced {
                    // Keep the original flat fields for older trace consumers.
                    event.payload["coalesced"] = Value::Bool(true);
                    event.payload["coalesced_omitted_event_count"] =
                        json!(coalescing.omitted_events);
                    event.payload["coalesced_omitted_byte_count"] = json!(coalescing.omitted_bytes);
                }
            }
            if let Some(turn_id) = event_turn_id {
                event.turn_id = Some(turn_id);
            }
            for (mut artifact, bytes, payload_key) in context_artifacts {
                artifact.provenance_refs.push(event.id);
                self.storage
                    .repositories
                    .artifacts
                    .store(&artifact, &bytes)
                    .await?;
                event.payload_ref.get_or_insert(artifact.artifact_id);
                event.payload[payload_key] = Value::String(artifact.artifact_id.to_string());
            }
            for ((mut artifact, bytes), payload_key) in provider_artifacts {
                artifact.provenance_refs.push(event.id);
                self.storage
                    .repositories
                    .artifacts
                    .store(&artifact, &bytes)
                    .await?;
                event.payload_ref.get_or_insert(artifact.artifact_id);
                event.payload[payload_key] = Value::String(artifact.artifact_id.to_string());
            }
            self.record_event(event).await?;
            if let Some((lane_id, turn_id)) = queued_turn_start {
                self.execution
                    .lane_manager
                    .lock()
                    .await
                    .commit_queued_turn_start(task.session_id, lane_id, turn_id)?;
            }
        }
        Ok(())
    }

    pub(super) async fn persist_checkpoint_before_side_effect(
        &self,
        task: &HostedAgentTask,
        request: &ToolRequest,
        before_images: &[FileBeforeImage],
        complete: bool,
    ) -> Result<(), ClientError> {
        let workspace_root = self.execution_workspace_root()?;
        let checkpoint_root = self
            .runtime_paths
            .as_ref()
            .map(|paths| paths.checkpoints_dir.clone())
            .ok_or_else(|| {
                ClientError::TaskExecution(
                    "durable checkpoint path is unavailable for this runtime".to_owned(),
                )
            })?;
        let manager = WorkspaceCheckpointManager::new(workspace_root.clone(), checkpoint_root);
        let workspace_id = self.workspace_id;
        let task_id = task.task_id;
        let turn_id = request.turn_id.unwrap_or(task.turn_id);
        let tool_call_id = request.tool_call_id;
        let partial_checkpoint =
            matches!(request.tool_name.as_str(), "shell" | "external_verifier")
                || task
                    .payload
                    .get("yolo")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
        let owned_before_images = before_images.to_vec();
        let checkpoint_result = run_blocking(move || {
            let (checkpoint_before_images, excluded_count) = if partial_checkpoint {
                manager.filter_checkpointable_before_images(&owned_before_images)?
            } else {
                (owned_before_images, 0)
            };
            let checkpoint = manager.create_checkpoint(
                workspace_id,
                task_id,
                turn_id,
                &checkpoint_before_images,
                tool_call_id,
            )?;
            Ok::<_, golutra_runtime::CheckpointError>((
                checkpoint,
                checkpoint_before_images,
                excluded_count,
            ))
        })
        .await?;
        let (checkpoint, checkpoint_before_images, excluded_count) =
            checkpoint_result.map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        let before_image_complete = complete && excluded_count == 0;

        let checkpoint_event_id = EventId::new();
        let mut event = agent_event(
            self.next_sequence_no(),
            task,
            RuntimeEventType::CheckpointCreated,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "workspace restore checkpoint persisted before tool side effect",
                "before_image_complete": before_image_complete,
                "omitted_before_image_count": excluded_count,
                "candidate_before_image_count": checkpoint_before_images.len(),
                "checkpoint": checkpoint,
            }),
        );
        event.id = checkpoint_event_id;
        event.turn_id = request.turn_id;
        self.record_event(event).await
    }

    pub(super) async fn record_tool_report(
        &self,
        task: &HostedAgentTask,
        report: &golutra_tools::ToolExecutionReport,
    ) -> Result<(), ClientError> {
        let event_turn_id = report
            .artifacts
            .iter()
            .find_map(|artifact| artifact.turn_id)
            .unwrap_or(task.turn_id);
        let change_samples =
            change_tracker::capture_change_samples(self.workspace_root.as_deref(), report).await;
        let change_facts = self.execution.workspace_change_tracker.lock().await.record(
            task.task_id,
            event_turn_id,
            change_samples,
        );
        let actual_changed_files = change_facts
            .operation_changes
            .iter()
            .map(|change| change.path.clone())
            .collect::<Vec<_>>();
        let actual_changed_paths = actual_changed_files.iter().cloned().collect::<HashSet<_>>();
        let mut seen_before_images = HashSet::new();
        let checkpoint_before_image_artifacts = report
            .before_images
            .iter()
            .filter_map(|before_image| {
                let path = change_tracker::display_path(
                    self.workspace_root.as_deref(),
                    &before_image.path,
                );
                if !actual_changed_paths.contains(&path) || !seen_before_images.insert(path.clone())
                {
                    return None;
                }
                let bytes = before_image.content.clone()?;
                let artifact_id = ArtifactId::new();
                let checksum = format!("sha256:{:x}", Sha256::digest(&bytes));
                Some((
                    path,
                    ArtifactRecord {
                        artifact_id,
                        session_id: task.session_id,
                        turn_id: Some(event_turn_id),
                        tool_call_id: Some(report.envelope.tool_call_id),
                        artifact_type: "checkpoint_before_image".to_owned(),
                        uri: format!("artifact://checkpoint-content/{artifact_id}"),
                        checksum,
                        size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                        created_at: chrono::Utc::now(),
                        producer: "runtime-checkpoint".to_owned(),
                        redaction_status: RedactionStatus::Raw,
                        retention_policy: "restore_only_owner_access".to_owned(),
                        provenance_refs: Vec::new(),
                    },
                    bytes,
                ))
            })
            .collect::<Vec<_>>();
        let change_manifest_artifact = if change_facts.operation_changes.is_empty() {
            None
        } else {
            let bytes = serde_json::to_vec(&json!({
                "schema_version": 1,
                "changed_files": actual_changed_files,
                "operation_changes": change_facts.operation_changes,
                "diff_previews": change_facts.diff_previews,
                "turn_change_summary": change_facts.turn_summary,
            }))?;
            let artifact_id = ArtifactId::new();
            let checksum = Sha256::digest(&bytes);
            Some((
                ArtifactRecord {
                    artifact_id,
                    session_id: task.session_id,
                    turn_id: Some(event_turn_id),
                    tool_call_id: Some(report.envelope.tool_call_id),
                    artifact_type: "workspace_change_manifest".to_owned(),
                    uri: format!(
                        "artifact://tool/{}/changes/{artifact_id}",
                        report.envelope.tool_call_id
                    ),
                    checksum: format!("sha256:{checksum:x}"),
                    size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                    created_at: chrono::Utc::now(),
                    producer: "workspace-change-tracker".to_owned(),
                    redaction_status: RedactionStatus::Redacted,
                    retention_policy: "debug_default".to_owned(),
                    provenance_refs: Vec::new(),
                },
                bytes,
            ))
        };
        let diff_artifact = change_facts.diff_artifact.as_ref().map(|diff| {
            let (content, redaction_status) = golutra_tools::redact_sensitive_text(&diff.content);
            let bytes = content.into_bytes();
            let artifact_id = ArtifactId::new();
            let checksum = Sha256::digest(&bytes);
            let artifact = ArtifactRecord {
                artifact_id,
                session_id: task.session_id,
                turn_id: Some(event_turn_id),
                tool_call_id: Some(report.envelope.tool_call_id),
                artifact_type: "workspace_diff".to_owned(),
                uri: format!(
                    "artifact://tool/{}/diff/{artifact_id}",
                    report.envelope.tool_call_id
                ),
                checksum: format!("sha256:{checksum:x}"),
                size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                created_at: chrono::Utc::now(),
                producer: "workspace-change-tracker".to_owned(),
                redaction_status,
                retention_policy: "debug_default".to_owned(),
                provenance_refs: Vec::new(),
            };
            (artifact, bytes)
        });
        let replay_result_artifact = {
            let bytes = serde_json::to_vec(&report.envelope)?;
            let artifact_id = ArtifactId::new();
            let checksum = Sha256::digest(&bytes);
            let artifact = ArtifactRecord {
                artifact_id,
                session_id: task.session_id,
                turn_id: Some(event_turn_id),
                tool_call_id: Some(report.envelope.tool_call_id),
                artifact_type: "tool_result_replay".to_owned(),
                uri: format!(
                    "artifact://replay/tool-result/{}/{artifact_id}",
                    report.envelope.tool_call_id
                ),
                checksum: format!("sha256:{checksum:x}"),
                size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                created_at: chrono::Utc::now(),
                producer: "tool-executor".to_owned(),
                redaction_status: RedactionStatus::Raw,
                retention_policy: "replay_owner_access".to_owned(),
                provenance_refs: Vec::new(),
            };
            (artifact, bytes)
        };
        let inline_changed_files = actual_changed_files
            .iter()
            .take(MAX_INLINE_CHANGE_FILES)
            .collect::<Vec<_>>();
        let inline_operation_changes = change_facts
            .operation_changes
            .iter()
            .take(MAX_INLINE_CHANGE_FILES)
            .collect::<Vec<_>>();
        let inline_diff_previews = change_facts
            .diff_previews
            .iter()
            .take(MAX_INLINE_DIFF_PREVIEWS)
            .collect::<Vec<_>>();
        let mut inline_turn_summary = change_facts.turn_summary.clone();
        inline_turn_summary.files.truncate(MAX_INLINE_CHANGE_FILES);
        inline_turn_summary.files_truncated = inline_turn_summary.file_count
            > u64::try_from(inline_turn_summary.files.len()).unwrap_or(u64::MAX);
        let mut event = agent_event(
            self.next_sequence_no(),
            task,
            RuntimeEventType::ToolCompleted,
            RuntimeEventSource::Tool,
            json!({
                "summary": report.envelope.summary,
                "envelope": report.envelope,
                "metrics": report.metrics,
                "changed_file_count": actual_changed_files.len(),
                "changed_files": inline_changed_files,
                "changed_files_truncated": actual_changed_files.len() > MAX_INLINE_CHANGE_FILES,
                "file_changes": inline_operation_changes,
                "file_changes_truncated": change_facts.operation_changes.len() > MAX_INLINE_CHANGE_FILES,
                "diff_previews": inline_diff_previews,
                "diff_previews_truncated": change_facts.diff_previews.len() > MAX_INLINE_DIFF_PREVIEWS,
                "diff_artifact_truncated": change_facts
                    .diff_artifact
                    .as_ref()
                    .is_some_and(|diff| diff.truncated),
                "turn_change_summary": inline_turn_summary,
            }),
        );
        event.turn_id = Some(event_turn_id);
        let tool_event_id = event.id;
        let (mut replay_artifact, replay_bytes) = replay_result_artifact;
        replay_artifact.provenance_refs.push(tool_event_id);
        event.payload["replay_result_artifact_ref"] =
            Value::String(replay_artifact.artifact_id.to_string());
        event.payload_ref = Some(replay_artifact.artifact_id);
        let mut artifacts = vec![(replay_artifact, replay_bytes)];
        let mut before_image_refs = Vec::new();
        for (path, mut artifact, bytes) in checkpoint_before_image_artifacts {
            artifact.provenance_refs.push(tool_event_id);
            let checksum = artifact.checksum.clone();
            let artifact_ref = artifact.artifact_id;
            before_image_refs.push(json!({
                "path": path,
                "artifact_ref": artifact_ref,
                "checksum": checksum,
            }));
            artifacts.push((artifact, bytes));
        }
        if !before_image_refs.is_empty() {
            event.payload["checkpoint_before_images"] = Value::Array(before_image_refs);
        }
        if let Some((mut artifact, bytes)) = change_manifest_artifact {
            artifact.provenance_refs.push(tool_event_id);
            let artifact_ref = artifact.artifact_id;
            event.payload["change_manifest_artifact_ref"] = Value::String(artifact_ref.to_string());
            artifacts.push((artifact, bytes));
        }
        if let Some((mut artifact, bytes)) = diff_artifact {
            artifact.provenance_refs.push(tool_event_id);
            event.payload["diff_artifact_ref"] = Value::String(artifact.artifact_id.to_string());
            artifacts.push((artifact, bytes));
        }
        for artifact in &report.artifacts {
            let content = report
                .artifact_contents
                .iter()
                .find(|content| content.artifact_id == artifact.artifact_id)
                .ok_or_else(|| {
                    ClientError::TaskExecution(format!(
                        "artifact {} has no durable content",
                        artifact.artifact_id
                    ))
                })?;
            let mut artifact = artifact.clone();
            if !artifact.provenance_refs.contains(&tool_event_id) {
                artifact.provenance_refs.push(tool_event_id);
            }
            artifacts.push((artifact, content.bytes.clone()));
        }
        let mut evidence_records = Vec::with_capacity(report.evidence.len());
        for evidence in &report.evidence {
            let mut evidence = evidence.clone();
            if !evidence.source_event_refs.contains(&tool_event_id) {
                evidence.source_event_refs.push(tool_event_id);
            }
            evidence_records.push(evidence);
        }
        self.record_tool_completed_bundle(event, &artifacts, &evidence_records)
            .await
    }

    pub(super) async fn finish_lane(
        &self,
        task: &HostedAgentTask,
        status: TaskStatus,
    ) -> Result<(), ClientError> {
        self.finish_lane_with_outcome(
            task,
            status,
            golutra_core::TaskOutcome::from_status(
                status,
                golutra_core::VerificationResult::Unknown,
            ),
        )
        .await
    }

    pub(super) async fn finish_lane_with_outcome(
        &self,
        task: &HostedAgentTask,
        status: TaskStatus,
        outcome: golutra_core::TaskOutcome,
    ) -> Result<(), ClientError> {
        let awaiting_external = matches!(
            outcome.external_verification,
            golutra_core::ExternalVerificationStatus::Pending
        );
        self.execution
            .workspace_change_tracker
            .lock()
            .await
            .remove_task(task.task_id);
        let mut lane_manager = self.execution.lane_manager.lock().await;
        let transition = lane_manager.finish_task(task.session_id, status, self.next_sequence_no());
        drop(lane_manager);
        match transition {
            Ok(mut transition) => {
                transition.event.payload["summary"] =
                    json!(format!("runtime task finished with {status:?}"));
                transition.event.payload["status"] = json!(status);
                transition.event.payload["outcome"] = json!(outcome.clone());
                transition.event.payload["post_task_governance"] = json!({"status": "pending"});
                self.record_event(transition.event).await?
            }
            Err(RuntimeLaneError::LaneNotFound) => {
                let event_type = RuntimeEventType::for_terminal_status(status);
                self.record_event(agent_event(
                    self.next_sequence_no(),
                    task,
                    event_type,
                    RuntimeEventSource::Runtime,
                    json!({
                        "summary": format!("persisted runtime task finished with {status:?}"),
                        "status": status,
                        "outcome": outcome.clone(),
                        "post_task_governance": {"status": "pending"},
                    }),
                ))
                .await?
            }
            Err(error) => return Err(error.into()),
        };
        if awaiting_external {
            let candidate_ready =
                outcome.execution == golutra_core::ExecutionOutcome::CandidateReady;
            self.record_event(agent_event(
                self.next_sequence_no(),
                task,
                RuntimeEventType::ExternalVerificationRequested,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": if candidate_ready {
                        "runtime candidate is awaiting an external evaluator"
                    } else {
                        "external evaluation requested for the terminal runtime outcome"
                    },
                    "status": status,
                    "outcome": outcome,
                    "resume_supported": candidate_ready,
                    "next_action": if candidate_ready {
                        "ingest the evaluator record, then resume with structured feedback if assertions fail"
                    } else {
                        "ingest the evaluator record without replacing the runtime failure"
                    },
                }),
            ))
            .await?;
        }
        Ok(())
    }

    pub(super) async fn record_task_execution_failure(
        self: &Arc<Self>,
        task: &HostedAgentTask,
        error: ClientError,
    ) -> Result<(), ClientError> {
        let active_turn_id = self
            .execution
            .lane_manager
            .lock()
            .await
            .lane(task.session_id)
            .and_then(|lane| lane.active_turn_id)
            .unwrap_or(task.turn_id);
        let active_payload = self.payload_for_task_turn(task, active_turn_id).await?;
        let failure_task = HostedAgentTask {
            turn_id: active_turn_id,
            payload: active_payload,
            ..task.clone()
        };
        let objective = prompt_from_payload(&failure_task.payload);
        if matches!(error, ClientError::TaskCancelled) {
            let verification = self
                .record_failed_verification(
                    &failure_task,
                    &objective,
                    "task cancelled by controller",
                )
                .await?;
            self.finish_lane_with_outcome(
                &failure_task,
                TaskStatus::Cancelled,
                task_outcome_from_verification(&failure_task, TaskStatus::Cancelled, &verification),
            )
            .await?;
            self.schedule_task_evaluation_best_effort(
                &failure_task,
                HostedTaskEvaluation {
                    objective: &objective,
                    task_status: TaskStatus::Cancelled,
                    verification: Some(verification),
                    tool_reports: &[],
                    failure_summary: Some("task cancelled by controller".to_owned()),
                    latency: Duration::ZERO,
                },
            )
            .await;
            return Ok(());
        }
        let error_summary = compact_event_summary(&error.to_string());
        if provider_auth_failure_message(&error_summary) {
            self.record_event(agent_event(
                self.next_sequence_no(),
                &failure_task,
                RuntimeEventType::ProviderAuthFailed,
                RuntimeEventSource::Provider,
                json!({
                    "summary": "provider rejected the configured credential",
                    "error": error_summary.clone(),
                }),
            ))
            .await?;
        }
        let verification = self
            .record_failed_verification(&failure_task, &objective, &error_summary)
            .await?;
        self.record_event(agent_event(
            self.next_sequence_no(),
            &failure_task,
            RuntimeEventType::LoopDecided,
            RuntimeEventSource::Runtime,
            json!({
                "summary": format!("runtime task execution failed: {error_summary}"),
                "error": error.to_string(),
            }),
        ))
        .await?;
        self.finish_lane_with_outcome(
            &failure_task,
            TaskStatus::Failed,
            task_outcome_from_verification(&failure_task, TaskStatus::Failed, &verification),
        )
        .await?;
        self.schedule_task_evaluation_best_effort(
            &failure_task,
            HostedTaskEvaluation {
                objective: &objective,
                task_status: TaskStatus::Failed,
                verification: Some(verification),
                tool_reports: &[],
                failure_summary: Some(error.to_string()),
                latency: Duration::ZERO,
            },
        )
        .await;
        Ok(())
    }

    pub(super) async fn record_failed_verification(
        &self,
        task: &HostedAgentTask,
        objective: &str,
        reason: &str,
    ) -> Result<golutra_core::VerificationRecord, ClientError> {
        let execution_mode = execution_mode_from_payload(&task.payload)
            .map_err(|error| ClientError::TaskExecution(error.to_owned()))?;
        let mut task_contract = task_contract_from_payload(&task.payload)?;
        if !explicit_task_contract(&task.payload)
            && should_apply_legacy_adapter(&task.payload, execution_mode)
        {
            LegacyTaskAdapter::new(&task.payload, objective).apply_to(&mut task_contract);
        }
        let requires_workspace_evidence = task_contract.requires_workspace_evidence();
        let (verification, plan) = RuntimeVerificationService::default().verify_runtime_failure(
            task.task_id,
            objective,
            vec!["runtime task produces a verified terminal result".to_owned()],
            requires_workspace_evidence,
            reason,
        );
        self.storage
            .repositories
            .artifacts
            .store_verification(&plan)
            .await?;
        self.record_event(agent_event_for_turn(
            self.next_sequence_no(),
            task,
            task.turn_id,
            RuntimeEventType::VerificationPlanned,
            RuntimeEventSource::Verifier,
            json!({
                "summary": format!("failure verification plan created for {:?}", plan.task_class),
                "plan": plan,
            }),
        ))
        .await?;
        for assertion in plan.assertions.iter().chain(plan.policy_assertions.iter()) {
            self.record_event(agent_event_for_turn(
                self.next_sequence_no(),
                task,
                task.turn_id,
                RuntimeEventType::VerificationAssertionCompleted,
                RuntimeEventSource::Verifier,
                json!({
                    "summary": format!(
                        "failure verification assertion {} completed as {:?}",
                        assertion.criterion_id,
                        assertion.status
                    ),
                    "assertion": assertion,
                }),
            ))
            .await?;
        }
        self.record_event(agent_event_for_turn(
            self.next_sequence_no(),
            task,
            task.turn_id,
            RuntimeEventType::VerificationCompleted,
            RuntimeEventSource::Verifier,
            json!({
                "summary": "runtime failure verified as a failed task",
                "record": verification,
            }),
        ))
        .await?;
        Ok(verification)
    }

    pub(super) async fn payload_for_task_turn(
        &self,
        task: &HostedAgentTask,
        turn_id: TurnId,
    ) -> Result<Value, ClientError> {
        if turn_id == task.turn_id {
            return Ok(task.payload.clone());
        }
        let events = self
            .storage
            .repositories
            .events
            .load(task.session_id, Some(task.task_id), None)
            .await?;
        if let Some(payload) = events
            .iter()
            .rev()
            .find(|event| {
                event.turn_id == Some(turn_id) && event.event_type == RuntimeEventType::TurnQueued
            })
            .and_then(|event| event.payload.get("payload"))
            .filter(|payload| payload.is_object())
        {
            return Ok(payload.clone());
        }
        let mut payload = task.payload.clone();
        if let Some(prompt) = events
            .iter()
            .rev()
            .find(|event| {
                event.turn_id == Some(turn_id) && event.event_type == RuntimeEventType::TurnStarted
            })
            .and_then(|event| event.payload.get("prompt"))
            .and_then(Value::as_str)
        {
            payload["prompt"] = Value::String(prompt.to_owned());
        }
        Ok(payload)
    }

    pub(super) fn execution_workspace_root(&self) -> Result<PathBuf, ClientError> {
        self.workspace_root.clone().map(Ok).unwrap_or_else(|| {
            std::env::current_dir().map_err(|error| ClientError::Io(error.to_string()))
        })
    }

    pub(super) async fn build_tool_executor(
        &self,
        policy: WorkspacePolicy,
        workspace_root: PathBuf,
        requested_network: bool,
        yolo: bool,
    ) -> Result<ToolRuntime, ClientError> {
        let executor = ToolRuntime::new(policy)
            .with_network_access(self.network_access_enabled(requested_network))
            .with_unrestricted_access(yolo)
            .with_process_supervisor(self.execution.process_supervisor.clone());
        let executor = match self.execution.web_search_backend.get_or_init(|| {
            HttpWebSearchBackend::from_env()
                .map(|backend| backend.map(Arc::new))
                .map_err(|error| error.to_string())
        }) {
            Ok(Some(backend)) => executor.with_web_search_backend(backend.clone()),
            Ok(None) => executor,
            Err(error) => {
                return Err(ClientError::TaskExecution(error.clone()));
            }
        };
        let Some(paths) = self
            .runtime_paths
            .as_ref()
            .filter(|_| !self.force_mock_provider)
        else {
            return Ok(executor);
        };
        let home = paths.home.clone();
        let scratch_root = paths.mcp_scratch_dir.clone();
        let backend = run_blocking(move || {
            let store = PluginStore::new(home)
                .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
            let backend = if yolo {
                McpToolBackend::from_store_unrestricted(store, workspace_root, scratch_root)
            } else {
                McpToolBackend::from_store(store, workspace_root, scratch_root)
            };
            backend.map_err(|error| ClientError::TaskExecution(error.to_string()))
        })
        .await??;
        match backend {
            Some(backend) => executor
                .with_external_backend(Arc::new(backend))
                .map_err(|error| ClientError::TaskExecution(error.to_string())),
            None => Ok(executor),
        }
    }
}
