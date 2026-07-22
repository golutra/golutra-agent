//! AgentLoop trace、artifact/checkpoint 持久化与失败终态适配。

use super::*;

#[derive(Clone)]
pub(crate) struct CanonicalFactRecorder {
    host: Arc<RuntimeHost>,
    task: HostedAgentTask,
}

impl CanonicalFactRecorder {
    pub(super) fn new(host: Arc<RuntimeHost>, task: HostedAgentTask) -> Self {
        Self { host, task }
    }

    pub(super) async fn commit(&self, observation: RuntimeObservation) -> Result<(), ClientError> {
        self.host
            .record_trace_observation(&self.task, observation)
            .await
    }
}

impl RuntimeHost {
    async fn record_trace_observation(
        &self,
        task: &HostedAgentTask,
        trace_event: RuntimeObservation,
    ) -> Result<(), ClientError> {
        let (trace_event, context_artifact) = match trace_event {
            AgentLoopTraceEvent::ContextSnapshotCaptured {
                mut snapshot,
                request,
            } => {
                let (artifact, bytes) = context_request_artifact(task, &snapshot, &request)?;
                snapshot.redacted_request_artifact_ref = Some(artifact.artifact_id);
                for contributor in &mut snapshot.contributor_manifest {
                    if contributor.included {
                        contributor.redacted_content_ref = Some(artifact.artifact_id);
                    }
                }
                (
                    AgentLoopTraceEvent::ContextSnapshot(snapshot),
                    Some((artifact, bytes, "redacted_request_artifact_ref")),
                )
            }
            AgentLoopTraceEvent::ContextAutoCompacted(record) => {
                let (artifact, bytes) = context_compaction_artifact(task, &record)?;
                (
                    AgentLoopTraceEvent::ContextAutoCompacted(record),
                    Some((artifact, bytes, "replacement_context_artifact_ref")),
                )
            }
            trace_event => (trace_event, None),
        };
        if let AgentLoopTraceEvent::ContextSnapshot(snapshot) = &trace_event {
            self.repositories.artifacts.store_context(snapshot).await?;
        }
        if let AgentLoopTraceEvent::VerificationPlanned(plan) = &trace_event {
            self.repositories.artifacts.store_verification(plan).await?;
        }
        if let AgentLoopTraceEvent::ToolCompleted(report) = &trace_event {
            return self.record_tool_report(task, report).await;
        }
        if let AgentLoopTraceEvent::PendingTurnStarted(turn) = &trace_event {
            self.lane_manager
                .lock()
                .await
                .start_queued_turn(task.session_id, turn.turn_id)?;
        }
        let active_turn_id = self
            .lane_manager
            .lock()
            .await
            .lane(task.session_id)
            .and_then(|lane| lane.active_turn_id)
            .unwrap_or(task.turn_id);
        let event_turn_id = match &trace_event {
            AgentLoopTraceEvent::PendingTurnStarted(turn) => Some(turn.turn_id),
            AgentLoopTraceEvent::AssistantMessage { turn_id, .. } => Some(*turn_id),
            AgentLoopTraceEvent::ApprovalRequested(approval) => Some(approval.turn_id),
            AgentLoopTraceEvent::TokenUsageRecorded(record) => Some(record.turn_id),
            _ => Some(active_turn_id),
        };
        let provider_artifact = match &trace_event {
            AgentLoopTraceEvent::ProviderCompleted { raw_metadata, .. } => {
                Some(provider_raw_artifact(task, active_turn_id, raw_metadata)?)
            }
            _ => None,
        };
        if let Some((event_type, source, payload)) = trace_event_payload(trace_event) {
            let mut event = agent_event(self.next_sequence_no(), task, event_type, source, payload);
            if let Some(turn_id) = event_turn_id {
                event.turn_id = Some(turn_id);
            }
            if let Some((mut artifact, bytes, payload_key)) = context_artifact {
                artifact.provenance_refs.push(event.id);
                self.repositories.artifacts.store(&artifact, &bytes).await?;
                event.payload_ref = Some(artifact.artifact_id);
                event.payload[payload_key] = Value::String(artifact.artifact_id.to_string());
            }
            if let Some((mut artifact, bytes)) = provider_artifact {
                artifact.provenance_refs.push(event.id);
                self.repositories.artifacts.store(&artifact, &bytes).await?;
                event.payload_ref = Some(artifact.artifact_id);
                event.payload["raw_metadata_ref"] = Value::String(artifact.artifact_id.to_string());
            }
            self.record_event(event).await?;
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
        let owned_before_images = before_images.to_vec();
        let mut checkpoint = run_blocking(move || {
            manager.create_checkpoint(
                workspace_id,
                task_id,
                turn_id,
                &owned_before_images,
                tool_call_id,
            )
        })
        .await?
        .map_err(|error| ClientError::TaskExecution(error.to_string()))?;

        let checkpoint_event_id = EventId::new();
        for before_image in before_images {
            let Some(bytes) = &before_image.content else {
                continue;
            };
            let artifact_id = ArtifactId::new();
            let checksum = Sha256::digest(bytes);
            let artifact = ArtifactRecord {
                artifact_id,
                session_id: task.session_id,
                turn_id: request.turn_id,
                tool_call_id: Some(tool_call_id),
                artifact_type: "checkpoint_before_image".to_owned(),
                uri: format!(
                    "artifact://checkpoint/{}/{artifact_id}",
                    checkpoint.checkpoint_id
                ),
                checksum: format!("sha256:{checksum:x}"),
                size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                created_at: chrono::Utc::now(),
                producer: "runtime-checkpoint".to_owned(),
                redaction_status: RedactionStatus::Raw,
                retention_policy: "restore_only_owner_access".to_owned(),
                provenance_refs: vec![checkpoint_event_id],
            };
            self.repositories.artifacts.store(&artifact, bytes).await?;
            checkpoint.artifact_refs.push(artifact_id);
        }

        let payload_ref = checkpoint.artifact_refs.first().copied();
        let mut event = agent_event(
            self.next_sequence_no(),
            task,
            RuntimeEventType::CheckpointCreated,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "workspace before-image persisted before tool side effect",
                "before_image_complete": complete,
                "checkpoint": checkpoint,
            }),
        );
        event.id = checkpoint_event_id;
        event.turn_id = request.turn_id;
        event.payload_ref = payload_ref;
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
        let change_facts = self.workspace_change_tracker.lock().await.record(
            task.task_id,
            event_turn_id,
            change_samples,
        );
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
        let mut event = agent_event(
            self.next_sequence_no(),
            task,
            RuntimeEventType::ToolCompleted,
            RuntimeEventSource::Tool,
            json!({
                "summary": report.envelope.summary,
                "envelope": report.envelope,
                "metrics": report.metrics,
                "changed_files": report.changed_files,
                "file_changes": change_facts.operation_changes,
                "diff_previews": change_facts.diff_previews,
                "diff_artifact_truncated": change_facts
                    .diff_artifact
                    .as_ref()
                    .is_some_and(|diff| diff.truncated),
                "turn_change_summary": change_facts.turn_summary,
            }),
        );
        event.turn_id = Some(event_turn_id);
        let tool_event_id = event.id;
        if let Some((mut artifact, bytes)) = diff_artifact {
            artifact.provenance_refs.push(tool_event_id);
            event.payload_ref = Some(artifact.artifact_id);
            event.payload["diff_artifact_ref"] = Value::String(artifact.artifact_id.to_string());
            self.repositories.artifacts.store(&artifact, &bytes).await?;
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
            self.repositories
                .artifacts
                .store(&artifact, &content.bytes)
                .await?;
        }
        for evidence in &report.evidence {
            let mut evidence = evidence.clone();
            if !evidence.source_event_refs.contains(&tool_event_id) {
                evidence.source_event_refs.push(tool_event_id);
            }
            self.repositories
                .artifacts
                .store_evidence(&evidence)
                .await?;
        }
        self.record_event(event).await
    }

    pub(super) async fn finish_lane(
        &self,
        task: &HostedAgentTask,
        status: TaskStatus,
    ) -> Result<(), ClientError> {
        self.workspace_change_tracker
            .lock()
            .await
            .remove_task(task.task_id);
        let mut lane_manager = self.lane_manager.lock().await;
        let transition = lane_manager.finish_task(task.session_id, status, self.next_sequence_no());
        drop(lane_manager);
        match transition {
            Ok(mut transition) => {
                transition.event.payload["summary"] =
                    json!(format!("runtime task finished with {status:?}"));
                transition.event.payload["status"] = json!(status);
                self.record_event(transition.event).await
            }
            Err(RuntimeLaneError::LaneNotFound) => {
                let event_type = if status == TaskStatus::Cancelled {
                    RuntimeEventType::TaskAborted
                } else {
                    RuntimeEventType::TaskCompleted
                };
                self.record_event(agent_event(
                    self.next_sequence_no(),
                    task,
                    event_type,
                    RuntimeEventSource::Runtime,
                    json!({
                        "summary": format!("persisted runtime task finished with {status:?}"),
                        "status": status,
                    }),
                ))
                .await
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) async fn record_task_execution_failure(
        self: &Arc<Self>,
        task: &HostedAgentTask,
        error: ClientError,
    ) -> Result<(), ClientError> {
        let active_turn_id = self
            .lane_manager
            .lock()
            .await
            .lane(task.session_id)
            .and_then(|lane| lane.active_turn_id)
            .unwrap_or(task.turn_id);
        let failure_task = HostedAgentTask {
            turn_id: active_turn_id,
            ..task.clone()
        };
        let objective = self.objective_for_task_turn(task, active_turn_id).await?;
        if matches!(error, ClientError::TaskCancelled) {
            let verification = self
                .record_failed_verification(
                    &failure_task,
                    &objective,
                    "task cancelled by controller",
                )
                .await?;
            let evaluation_input = self
                .evaluate_completed_task(
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
                .await?;
            self.enqueue_deep_task_evaluation(&failure_task, evaluation_input)
                .await?;
            self.finish_lane(&failure_task, TaskStatus::Cancelled)
                .await?;
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
        let evaluation_input = self
            .evaluate_completed_task(
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
            .await?;
        self.enqueue_deep_task_evaluation(&failure_task, evaluation_input)
            .await?;
        self.finish_lane(&failure_task, TaskStatus::Failed).await?;
        Ok(())
    }

    pub(super) async fn record_failed_verification(
        &self,
        task: &HostedAgentTask,
        objective: &str,
        reason: &str,
    ) -> Result<golutra_core::VerificationRecord, ClientError> {
        let requires_workspace_evidence =
            provider_runtime::prompt_requests_workspace_tools(&task.payload, objective);
        let (verification, plan) = RuntimeVerificationService::default().verify_runtime_failure(
            task.task_id,
            objective,
            vec!["runtime task produces a verified terminal result".to_owned()],
            requires_workspace_evidence,
            reason,
        );
        self.repositories
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

    pub(super) async fn objective_for_task_turn(
        &self,
        task: &HostedAgentTask,
        turn_id: TurnId,
    ) -> Result<String, ClientError> {
        if turn_id == task.turn_id {
            return Ok(prompt_from_payload(&task.payload));
        }
        let events = self
            .repositories
            .events
            .load_recent(
                task.session_id,
                Some(task.task_id),
                None,
                MAX_HISTORY_SOURCE_EVENTS,
            )
            .await?;
        Ok(events
            .iter()
            .rev()
            .find(|event| {
                event.turn_id == Some(turn_id) && event.event_type == RuntimeEventType::TurnStarted
            })
            .and_then(|event| event.payload.get("prompt"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| prompt_from_payload(&task.payload)))
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
    ) -> Result<BasicToolExecutor, ClientError> {
        let executor = BasicToolExecutor::new(policy);
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
            McpToolBackend::from_store(store, workspace_root, scratch_root)
                .map_err(|error| ClientError::TaskExecution(error.to_string()))
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
