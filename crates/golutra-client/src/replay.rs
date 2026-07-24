//! Artifact-backed deterministic replay of provider/tool control flow.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex as StdMutex},
};

use async_trait::async_trait;
use golutra_context::{ContextBudgetPolicy, ContextBuilder};
use golutra_core::{
    ArtifactId, ArtifactRecord, LoopAction, ProviderContract, RedactionStatus, SessionId, TaskId,
    ToolResultEnvelope, VerificationResult,
};
use golutra_eval::{ReplayCapsule, ReplayExecution, ReplayExecutionStatus};
use golutra_llm::{LlmProvider, ProviderError, ProviderRequest, ProviderResponse};
use golutra_protocol::{RuntimeEvent, RuntimeEventSource, RuntimeEventType};
use golutra_runtime::{AgentLoop, AgentReplayContext, AgentTaskRequest};
use golutra_tools::{BasicToolExecutor, ToolError, ToolReplayBackend, ToolRequest};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::*;

#[derive(Debug, Clone)]
struct RecordedProviderExchange {
    request: ProviderRequest,
    response: ProviderResponse,
}

#[derive(Debug, Default)]
struct ReplayProviderState {
    exchanges: VecDeque<RecordedProviderExchange>,
    consumed: u32,
    mismatches: Vec<String>,
}

#[derive(Debug, Clone)]
struct ArtifactReplayProvider {
    contract: ProviderContract,
    state: Arc<StdMutex<ReplayProviderState>>,
}

impl ArtifactReplayProvider {
    fn new(exchanges: Vec<RecordedProviderExchange>) -> Result<Self, ClientError> {
        let Some(first) = exchanges.first() else {
            return Err(ClientError::TaskExecution(
                "replay capsule has no provider exchanges".to_owned(),
            ));
        };
        Ok(Self {
            contract: ProviderContract {
                provider_id: first.request.provider_id.clone(),
                model_id: first.request.model_id.clone(),
                native_protocol: "artifact_replay".to_owned(),
                stream_event_mapping: "recorded_response".to_owned(),
                tool_call_mapping: "normalized".to_owned(),
                usage_mapping: "recorded".to_owned(),
                reasoning_mapping: "recorded".to_owned(),
                finish_reason_mapping: "recorded".to_owned(),
                error_mapping: "deterministic".to_owned(),
                rate_limit_mapping: "none".to_owned(),
                cost_model: "recorded".to_owned(),
                capability_matrix_ref: None,
                golden_fixture_refs: Vec::new(),
            },
            state: Arc::new(StdMutex::new(ReplayProviderState {
                exchanges: exchanges.into(),
                ..ReplayProviderState::default()
            })),
        })
    }

    fn snapshot(&self) -> (u32, u32, Vec<String>) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            state.consumed,
            u32::try_from(state.exchanges.len()).unwrap_or(u32::MAX),
            state.mismatches.clone(),
        )
    }
}

#[async_trait]
impl LlmProvider for ArtifactReplayProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(recorded) = state.exchanges.pop_front() else {
            state
                .mismatches
                .push("provider emitted more requests than the replay capsule".to_owned());
            return Err(ProviderError::Failed {
                message: "provider replay exchange queue is exhausted".to_owned(),
            });
        };
        state.consumed = state.consumed.saturating_add(1);
        if request.provider_id != recorded.request.provider_id {
            state.mismatches.push(format!(
                "provider_id diverged: expected {}, observed {}",
                recorded.request.provider_id, request.provider_id
            ));
        }
        if request.model_id != recorded.request.model_id {
            state.mismatches.push(format!(
                "model_id diverged: expected {}, observed {}",
                recorded.request.model_id, request.model_id
            ));
        }
        if request.messages != recorded.request.messages {
            state.mismatches.push(format!(
                "provider request {} message sequence diverged",
                recorded.request.request_id
            ));
        }
        if request.tools != recorded.request.tools {
            state.mismatches.push(format!(
                "provider request {} tool contracts diverged",
                recorded.request.request_id
            ));
        }
        Ok(recorded.response)
    }

    fn supports_buffered_transport(&self) -> bool {
        false
    }

    fn contract(&self) -> ProviderContract {
        self.contract.clone()
    }
}

#[derive(Debug, Clone)]
struct RecordedToolResult {
    provider_tool_call_id: Option<String>,
    envelope: ToolResultEnvelope,
}

#[derive(Debug, Default)]
struct ReplayToolState {
    results: VecDeque<RecordedToolResult>,
    consumed: u32,
    mismatches: Vec<String>,
}

#[derive(Debug, Clone)]
struct ArtifactReplayToolBackend {
    state: Arc<StdMutex<ReplayToolState>>,
}

impl ArtifactReplayToolBackend {
    fn new(results: Vec<RecordedToolResult>) -> Self {
        Self {
            state: Arc::new(StdMutex::new(ReplayToolState {
                results: results.into(),
                ..ReplayToolState::default()
            })),
        }
    }

    fn snapshot(&self) -> (u32, u32, Vec<String>) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            state.consumed,
            u32::try_from(state.results.len()).unwrap_or(u32::MAX),
            state.mismatches.clone(),
        )
    }
}

#[async_trait]
impl ToolReplayBackend for ArtifactReplayToolBackend {
    async fn replay(&self, request: &ToolRequest) -> Result<ToolResultEnvelope, ToolError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(recorded) = state.results.pop_front() else {
            state
                .mismatches
                .push("AgentLoop emitted more tool calls than the replay capsule".to_owned());
            return Err(ToolError::Execution(
                "tool replay result queue is exhausted".to_owned(),
            ));
        };
        state.consumed = state.consumed.saturating_add(1);
        if recorded.envelope.tool_name != request.tool_name {
            state.mismatches.push(format!(
                "tool name diverged: expected {}, observed {}",
                recorded.envelope.tool_name, request.tool_name
            ));
        }
        if recorded.provider_tool_call_id != request.provider_tool_call_id {
            state.mismatches.push(format!(
                "provider tool-call id diverged: expected {:?}, observed {:?}",
                recorded.provider_tool_call_id, request.provider_tool_call_id
            ));
        }
        Ok(recorded.envelope)
    }
}

impl RuntimeHost {
    pub(super) async fn handle_replay_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let source_task_id = match command.payload.get("task_id").and_then(Value::as_str) {
            Some(task_id) => task_id
                .parse::<TaskId>()
                .map_err(|error| ClientError::InvalidSession(error.to_string()))?,
            None => self
                .repositories
                .projections
                .state(session_id, None)
                .await?
                .active_task_id
                .ok_or_else(|| {
                    ClientError::InvalidSession(
                        "replay requires task_id when the session has no active task".to_owned(),
                    )
                })?,
        };
        let capsule_id = command
            .payload
            .get("capsule_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty());
        let execution = self
            .execute_deterministic_replay(session_id, source_task_id, capsule_id)
            .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!(
                "replay {} finished as {:?}",
                execution.execution_id, execution.status
            )),
        })
    }

    pub(crate) async fn execute_deterministic_replay(
        &self,
        session_id: SessionId,
        source_task_id: TaskId,
        capsule_id: Option<&str>,
    ) -> Result<ReplayExecution, ClientError> {
        let store = self.evaluation_store.clone();
        let evaluation = run_blocking(move || store.snapshot()).await??;
        let capsule = select_capsule(&evaluation.replay_capsules, source_task_id, capsule_id)?;
        let all_events = self
            .repositories
            .events
            .load(session_id, Some(source_task_id), None)
            .await?;
        let started_at = chrono::Utc::now();
        let Some(source_last_sequence_no) = capsule.source_last_sequence_no else {
            let (expected_loop_action, expected_verification) = expected_outcome(&all_events);
            let execution = replay_terminal_execution(
                &capsule,
                ReplayExecutionStatus::Incomplete,
                expected_loop_action,
                expected_verification,
                vec!["replay capsule has no source event boundary".to_owned()],
                started_at,
            );
            return self.persist_replay_execution(session_id, execution).await;
        };
        let events = all_events
            .into_iter()
            .filter(|event| event.sequence_no <= source_last_sequence_no)
            .collect::<Vec<_>>();
        let (expected_loop_action, expected_verification) = expected_outcome(&events);
        if !capsule.complete {
            let execution = replay_terminal_execution(
                &capsule,
                ReplayExecutionStatus::Incomplete,
                expected_loop_action,
                expected_verification,
                capsule.missing_inputs.clone(),
                started_at,
            );
            return self.persist_replay_execution(session_id, execution).await;
        }
        let prefix_integrity = if source_last_sequence_no == u64::MAX {
            self.repositories
                .events
                .integrity(session_id, source_task_id)
                .await?
        } else {
            self.repositories
                .events
                .integrity_before(
                    session_id,
                    source_task_id,
                    source_last_sequence_no.saturating_add(1),
                )
                .await?
        };
        if events.last().map(|event| event.sequence_no) != Some(source_last_sequence_no)
            || prefix_integrity.last_sequence != Some(source_last_sequence_no)
            || prefix_integrity.event_chain_digest != capsule.event_chain_digest
        {
            let execution = replay_terminal_execution(
                &capsule,
                ReplayExecutionStatus::Failed,
                expected_loop_action,
                expected_verification,
                vec![format!(
                    "source event prefix diverged: expected sequence {} and digest {}, observed sequence {:?} and digest {}",
                    source_last_sequence_no,
                    capsule.event_chain_digest,
                    prefix_integrity.last_sequence,
                    prefix_integrity.event_chain_digest,
                )],
                started_at,
            );
            return self.persist_replay_execution(session_id, execution).await;
        }

        let mut exchanges = Vec::with_capacity(capsule.provider_exchanges.len());
        for exchange in &capsule.provider_exchanges {
            let request: ProviderRequest = self
                .read_replay_artifact(
                    session_id,
                    exchange.request_artifact_ref,
                    "provider_request_replay",
                )
                .await?;
            let response: ProviderResponse = self
                .read_replay_artifact(
                    session_id,
                    exchange.response_artifact_ref,
                    "provider_response_replay",
                )
                .await?;
            if request.request_id != exchange.request_id
                || response.response_id != exchange.response_id
            {
                return Err(ClientError::TaskExecution(format!(
                    "replay exchange {} does not match its artifact identities",
                    exchange.request_id
                )));
            }
            exchanges.push(RecordedProviderExchange { request, response });
        }
        let mut tool_results = Vec::with_capacity(capsule.tool_results.len());
        for result in &capsule.tool_results {
            let envelope: ToolResultEnvelope = self
                .read_replay_artifact(session_id, result.result_artifact_ref, "tool_result_replay")
                .await?;
            if envelope.tool_call_id != result.tool_call_id {
                return Err(ClientError::TaskExecution(format!(
                    "tool replay artifact {} has the wrong tool_call_id",
                    result.result_artifact_ref
                )));
            }
            tool_results.push(RecordedToolResult {
                provider_tool_call_id: result.provider_tool_call_id.clone(),
                envelope,
            });
        }
        let first_request = exchanges
            .first()
            .map(|exchange| exchange.request.clone())
            .ok_or_else(|| {
                ClientError::TaskExecution("replay has no initial provider request".to_owned())
            })?;
        let provider = ArtifactReplayProvider::new(exchanges)?;
        let tool_backend = ArtifactReplayToolBackend::new(tool_results);
        let workspace_root = self.execution_workspace_root()?;
        let policy = WorkspacePolicy::new(workspace_root)
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        let tool_executor =
            BasicToolExecutor::new(policy).with_replay_backend(Arc::new(tool_backend.clone()));
        let context_builder = replay_context_builder(
            self.repositories
                .artifacts
                .contexts(source_task_id)
                .await?
                .first(),
        );
        let task_payload = events
            .iter()
            .find(|event| event.event_type == RuntimeEventType::TaskCreated)
            .and_then(|event| event.payload.get("payload").cloned())
            .unwrap_or(Value::Null);
        let objective = prompt_from_payload(&task_payload);
        let external_verifiers = task_payload
            .get("external_verifiers")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| {
                ClientError::TaskExecution(format!(
                    "recorded external verifier contract is invalid: {error}"
                ))
            })?
            .unwrap_or_default();
        let touched_code = task_payload.get("content").is_some()
            || events.iter().any(|event| {
                event.event_type == RuntimeEventType::ToolCompleted
                    && event
                        .payload
                        .get("changed_files")
                        .and_then(Value::as_array)
                        .is_some_and(|files| !files.is_empty())
            });
        let agent_loop = AgentLoop::new(provider.clone(), context_builder, tool_executor)
            .with_external_verifiers(external_verifiers);
        let outcome = agent_loop
            .replay_with_trace(
                AgentTaskRequest {
                    session_id,
                    task_id: source_task_id,
                    turn_id: first_request.turn_id,
                    objective,
                    completion_criteria: completion_criteria_from_payload(&task_payload),
                    output_schema: task_payload.get("output_schema").cloned(),
                    touched_code,
                    contributors: Vec::new(),
                    tools: first_request
                        .tools
                        .iter()
                        .map(|contract| contract.tool_name.clone())
                        .collect(),
                },
                AgentReplayContext {
                    initial_messages: first_request.messages,
                    tools: first_request.tools,
                },
                |_| {},
            )
            .await;
        let (provider_consumed, provider_remaining, mut mismatches) = provider.snapshot();
        let (tool_consumed, tool_remaining, tool_mismatches) = tool_backend.snapshot();
        mismatches.extend(tool_mismatches);
        if provider_remaining > 0 {
            mismatches.push(format!(
                "{provider_remaining} provider exchanges were not consumed"
            ));
        }
        if tool_remaining > 0 {
            mismatches.push(format!("{tool_remaining} tool results were not consumed"));
        }
        let (observed_loop_action, observed_verification, execution_error) = match outcome {
            Ok(outcome) => (
                Some(outcome.loop_decision.action),
                Some(outcome.verification.result),
                None,
            ),
            Err(error) => (None, None, Some(error.to_string())),
        };
        if expected_loop_action != observed_loop_action {
            mismatches.push(format!(
                "loop action diverged: expected {expected_loop_action:?}, observed {observed_loop_action:?}"
            ));
        }
        if expected_verification != observed_verification {
            mismatches.push(format!(
                "verification diverged: expected {expected_verification:?}, observed {observed_verification:?}"
            ));
        }
        if let Some(error) = &execution_error {
            mismatches.push(format!("AgentLoop replay failed: {error}"));
        }
        mismatches.sort();
        mismatches.dedup();
        let status = if execution_error.is_some() {
            ReplayExecutionStatus::Failed
        } else if expected_loop_action.is_none() || expected_verification.is_none() {
            ReplayExecutionStatus::Incomplete
        } else if mismatches.is_empty() {
            ReplayExecutionStatus::Matched
        } else {
            ReplayExecutionStatus::Diverged
        };
        let execution = ReplayExecution {
            execution_id: format!("replay-execution-{}", Uuid::now_v7()),
            capsule_id: capsule.capsule_id.clone(),
            source_task_id,
            mode: capsule.mode,
            status,
            provider_exchanges_total: u32::try_from(capsule.provider_exchanges.len())
                .unwrap_or(u32::MAX),
            provider_exchanges_consumed: provider_consumed,
            tool_results_total: u32::try_from(capsule.tool_results.len()).unwrap_or(u32::MAX),
            tool_results_consumed: tool_consumed,
            expected_loop_action,
            observed_loop_action,
            expected_verification,
            observed_verification,
            mismatches,
            started_at,
            completed_at: chrono::Utc::now(),
        };
        self.persist_replay_execution(session_id, execution).await
    }

    async fn persist_replay_execution(
        &self,
        session_id: SessionId,
        execution: ReplayExecution,
    ) -> Result<ReplayExecution, ClientError> {
        let evaluation_store = self.evaluation_store.clone();
        let stored = execution.clone();
        run_blocking(move || evaluation_store.record_replay_execution(stored)).await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            Some(execution.source_task_id),
            RuntimeEventType::ReplayExecuted,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!(
                    "deterministic replay {} finished as {:?}",
                    execution.execution_id, execution.status
                ),
                "record": execution,
            }),
        ))
        .await?;
        Ok(execution)
    }

    async fn read_replay_artifact<T: serde::de::DeserializeOwned>(
        &self,
        session_id: SessionId,
        artifact_id: ArtifactId,
        expected_type: &str,
    ) -> Result<T, ClientError> {
        let artifact = self
            .repositories
            .artifacts
            .get(artifact_id)
            .await?
            .ok_or_else(|| {
                ClientError::TaskExecution(format!("replay artifact {artifact_id} is missing"))
            })?;
        validate_replay_artifact_metadata(&artifact, session_id, expected_type)?;
        let bytes = self
            .repositories
            .artifacts
            .bytes(artifact_id)
            .await?
            .ok_or_else(|| {
                ClientError::TaskExecution(format!("replay artifact blob {artifact_id} is missing"))
            })?;
        decode_replay_artifact_bytes(&artifact, &bytes)
    }
}

fn validate_replay_artifact_metadata(
    artifact: &ArtifactRecord,
    session_id: SessionId,
    expected_type: &str,
) -> Result<(), ClientError> {
    if artifact.session_id != session_id
        || artifact.artifact_type != expected_type
        || artifact.redaction_status != RedactionStatus::Raw
    {
        return Err(ClientError::TaskExecution(format!(
            "replay artifact {} has the wrong ownership, type, or disclosure",
            artifact.artifact_id
        )));
    }
    Ok(())
}

fn decode_replay_artifact_bytes<T: serde::de::DeserializeOwned>(
    artifact: &ArtifactRecord,
    bytes: &[u8],
) -> Result<T, ClientError> {
    let checksum = format!("sha256:{:x}", Sha256::digest(bytes));
    if checksum != artifact.checksum
        || u64::try_from(bytes.len()).unwrap_or(u64::MAX) != artifact.size_bytes
    {
        return Err(ClientError::TaskExecution(format!(
            "replay artifact {} failed checksum validation",
            artifact.artifact_id
        )));
    }
    serde_json::from_slice(bytes).map_err(ClientError::Serialization)
}

fn select_capsule(
    capsules: &[ReplayCapsule],
    source_task_id: TaskId,
    capsule_id: Option<&str>,
) -> Result<ReplayCapsule, ClientError> {
    capsules
        .iter()
        .rev()
        .find(|capsule| {
            capsule.source_task_id == source_task_id
                && capsule_id.is_none_or(|capsule_id| capsule.capsule_id == capsule_id)
        })
        .cloned()
        .ok_or_else(|| {
            ClientError::TaskExecution(format!(
                "no replay capsule is available for task {source_task_id}"
            ))
        })
}

fn replay_terminal_execution(
    capsule: &ReplayCapsule,
    status: ReplayExecutionStatus,
    expected_loop_action: Option<LoopAction>,
    expected_verification: Option<VerificationResult>,
    mismatches: Vec<String>,
    started_at: chrono::DateTime<chrono::Utc>,
) -> ReplayExecution {
    ReplayExecution {
        execution_id: format!("replay-execution-{}", Uuid::now_v7()),
        capsule_id: capsule.capsule_id.clone(),
        source_task_id: capsule.source_task_id,
        mode: capsule.mode,
        status,
        provider_exchanges_total: u32::try_from(capsule.provider_exchanges.len())
            .unwrap_or(u32::MAX),
        provider_exchanges_consumed: 0,
        tool_results_total: u32::try_from(capsule.tool_results.len()).unwrap_or(u32::MAX),
        tool_results_consumed: 0,
        expected_loop_action,
        observed_loop_action: None,
        expected_verification,
        observed_verification: None,
        mismatches,
        started_at,
        completed_at: chrono::Utc::now(),
    }
}

fn expected_outcome(events: &[RuntimeEvent]) -> (Option<LoopAction>, Option<VerificationResult>) {
    let loop_action = events.iter().rev().find_map(|event| {
        (event.event_type == RuntimeEventType::LoopDecided)
            .then(|| event.payload.pointer("/record/action").cloned())
            .flatten()
            .and_then(|value| serde_json::from_value(value).ok())
    });
    let verification = events.iter().rev().find_map(|event| {
        (event.event_type == RuntimeEventType::VerificationCompleted)
            .then(|| event.payload.pointer("/record/result").cloned())
            .flatten()
            .and_then(|value| serde_json::from_value(value).ok())
    });
    (loop_action, verification)
}

fn replay_context_builder(snapshot: Option<&golutra_core::ContextSnapshot>) -> ContextBuilder {
    snapshot.map_or_else(ContextBuilder::default, |snapshot| {
        ContextBuilder::new(ContextBudgetPolicy {
            context_window: snapshot.budget_snapshot.context_window,
            max_output: snapshot.budget_snapshot.max_output,
            budget_limit: snapshot.budget_snapshot.budget_limit,
            action_if_exceeded: snapshot.budget_snapshot.action_if_exceeded,
        })
    })
}

#[cfg(test)]
mod tests {
    use golutra_core::{
        ProviderRequestId, ProviderResponseId, ProviderUsage, ToolCallId, ToolResultStatus, TurnId,
        UsageSource,
    };
    use golutra_llm::{
        ProviderFinishReason, ProviderMessage, ProviderMessageMetadata, ProviderRole,
    };
    use serde_json::json;

    use super::*;

    fn provider_request(provider_id: &str, content: &str) -> ProviderRequest {
        ProviderRequest {
            request_id: ProviderRequestId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            provider_id: provider_id.to_owned(),
            model_id: "test-model".to_owned(),
            messages: vec![ProviderMessage {
                role: ProviderRole::User,
                content: content.to_owned(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: ProviderMessageMetadata::default(),
            }],
            tools: Vec::new(),
        }
    }

    fn provider_response() -> ProviderResponse {
        ProviderResponse {
            response_id: ProviderResponseId::new(),
            message: None,
            tool_calls: Vec::new(),
            usage: ProviderUsage {
                input_tokens: Some(1),
                output_tokens: Some(1),
                reasoning_tokens: None,
                cached_input_tokens: None,
                total_tokens: Some(2),
                usage_source: UsageSource::Provider,
                raw: json!({}),
            },
            finish_reason: ProviderFinishReason::Stop,
            raw_metadata: json!({}),
        }
    }

    #[tokio::test]
    async fn replay_provider_reports_request_divergence_and_queue_exhaustion() {
        let recorded = provider_request("recorded", "expected");
        let response = provider_response();
        let provider = ArtifactReplayProvider::new(vec![RecordedProviderExchange {
            request: recorded.clone(),
            response: response.clone(),
        }])
        .expect("provider");
        let mut observed = recorded;
        observed.provider_id = "observed".to_owned();
        observed.messages[0].content = "different".to_owned();

        assert_eq!(
            provider.complete(observed.clone()).await.expect("response"),
            response
        );
        assert!(provider.complete(observed).await.is_err());
        let (consumed, remaining, mismatches) = provider.snapshot();
        assert_eq!((consumed, remaining), (1, 0));
        assert!(
            mismatches
                .iter()
                .any(|value| value.contains("provider_id diverged"))
        );
        assert!(
            mismatches
                .iter()
                .any(|value| value.contains("message sequence diverged"))
        );
        assert!(
            mismatches
                .iter()
                .any(|value| value.contains("more requests than the replay capsule"))
        );
    }

    #[tokio::test]
    async fn replay_tool_reports_identity_divergence_and_queue_exhaustion() {
        let tool_call_id = ToolCallId::new();
        let backend = ArtifactReplayToolBackend::new(vec![RecordedToolResult {
            provider_tool_call_id: Some("recorded-call".to_owned()),
            envelope: ToolResultEnvelope {
                tool_call_id,
                tool_name: "read_file".to_owned(),
                status: ToolResultStatus::Ok,
                summary: "recorded".to_owned(),
                structured_facts: json!({}),
                model_visible_excerpt: None,
                raw_artifact_ref: None,
                evidence_refs: Vec::new(),
                risk: "low".to_owned(),
                verification_hint: None,
            },
        }]);
        let request = ToolRequest {
            tool_call_id,
            provider_tool_call_id: Some("observed-call".to_owned()),
            session_id: SessionId::new(),
            turn_id: Some(TurnId::new()),
            tool_name: "write_file".to_owned(),
            arguments: json!({}),
        };

        assert!(backend.replay(&request).await.is_ok());
        assert!(backend.replay(&request).await.is_err());
        let (consumed, remaining, mismatches) = backend.snapshot();
        assert_eq!((consumed, remaining), (1, 0));
        assert!(
            mismatches
                .iter()
                .any(|value| value.contains("tool name diverged"))
        );
        assert!(
            mismatches
                .iter()
                .any(|value| value.contains("tool-call id diverged"))
        );
        assert!(
            mismatches
                .iter()
                .any(|value| value.contains("more tool calls than the replay capsule"))
        );
    }

    #[test]
    fn replay_artifacts_require_owner_type_raw_bytes_checksum_and_size() {
        let session_id = SessionId::new();
        let bytes = serde_json::to_vec(&json!({"ok": true})).expect("bytes");
        let mut artifact = ArtifactRecord {
            artifact_id: ArtifactId::new(),
            session_id,
            turn_id: None,
            tool_call_id: None,
            artifact_type: "provider_request_replay".to_owned(),
            uri: "artifact://replay".to_owned(),
            checksum: format!("sha256:{:x}", Sha256::digest(&bytes)),
            size_bytes: u64::try_from(bytes.len()).expect("size"),
            created_at: chrono::Utc::now(),
            producer: "test".to_owned(),
            redaction_status: RedactionStatus::Raw,
            retention_policy: "test".to_owned(),
            provenance_refs: Vec::new(),
        };
        validate_replay_artifact_metadata(&artifact, session_id, "provider_request_replay")
            .expect("metadata");
        let decoded: Value = decode_replay_artifact_bytes(&artifact, &bytes).expect("decode");
        assert_eq!(decoded, json!({"ok": true}));

        artifact.redaction_status = RedactionStatus::Redacted;
        assert!(
            validate_replay_artifact_metadata(&artifact, session_id, "provider_request_replay")
                .is_err()
        );
        artifact.redaction_status = RedactionStatus::Raw;
        artifact.checksum = "sha256:wrong".to_owned();
        assert!(decode_replay_artifact_bytes::<Value>(&artifact, &bytes).is_err());
        artifact.checksum = format!("sha256:{:x}", Sha256::digest(&bytes));
        artifact.size_bytes = artifact.size_bytes.saturating_add(1);
        assert!(decode_replay_artifact_bytes::<Value>(&artifact, &bytes).is_err());
    }
}
