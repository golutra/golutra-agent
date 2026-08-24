//! Artifact-backed deterministic replay of provider/tool control flow.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex as StdMutex},
};

use async_trait::async_trait;
use golutra_context::{ContextBudgetPolicy, ContextBuilder};
use golutra_core::{
    ArtifactId, ArtifactRecord, EventId, LoopAction, ProviderContract, ProviderResponseId,
    RedactionStatus, SessionId, TaskContract, TaskId, ToolContract, ToolResultEnvelope, TurnId,
    VerificationResult,
};
use golutra_eval::{ReplayCapsule, ReplayExecution, ReplayExecutionStatus};
use golutra_llm::{LlmProvider, ProviderError, ProviderRequest, ProviderResponse};
use golutra_protocol::{RuntimeEvent, RuntimeEventSource, RuntimeEventType};
use golutra_runtime::{
    AgentExecutionHandle, AgentHarness, AgentReplayContext, AgentTaskRequest, ConfiguredAgentRun,
    ConfiguredPendingAgentTurn, agent_execution_channel,
};
use golutra_tools::{ToolError, ToolReplayBackend, ToolRequest, ToolRuntime};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::*;

// Replay artifacts share the 16 MiB tool-artifact contract. The aggregate
// limit matches the 160 MiB observation ingress budget so a capsule cannot
// retain more replay payload than one bounded terminal observation queue.
const MAX_REPLAY_ARTIFACT_BYTES: u64 = golutra_tools::MAX_TOOL_ARTIFACT_CONTENT_BYTES as u64;
const MAX_REPLAY_CAPSULE_ARTIFACT_BYTES: u64 = 160 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub(super) struct CanonicalReplayProjection {
    pub(super) capsules: Vec<ReplayCapsule>,
    pub(super) executions: Vec<ReplayExecution>,
}

#[derive(Debug)]
pub(super) struct CanonicalReplayState {
    pub(super) events: Vec<RuntimeEvent>,
    pub(super) projection: CanonicalReplayProjection,
    pub(super) reconciliation_error: Option<String>,
}

impl CanonicalReplayProjection {
    pub(super) fn latest_execution(&self) -> Option<ReplayExecution> {
        self.executions.last().cloned()
    }
}

#[derive(Debug, Clone)]
struct RecordedProviderExchange {
    request: ProviderRequest,
    response: ProviderResponse,
    pending_turns_after_response: Vec<ConfiguredPendingAgentTurn>,
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
    execution: AgentExecutionHandle,
}

impl ArtifactReplayProvider {
    fn new(
        exchanges: Vec<RecordedProviderExchange>,
        execution: AgentExecutionHandle,
    ) -> Result<Self, ClientError> {
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
            execution,
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
        let recorded = {
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
            recorded
        };
        for turn in recorded.pending_turns_after_response {
            if let Err(error) = self.execution.append_configured_turn(turn).await {
                self.state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .mismatches
                    .push(format!(
                        "recorded pending turn could not be injected: {error}"
                    ));
                return Err(ProviderError::Failed {
                    message: "provider replay could not inject a recorded pending turn".to_owned(),
                });
            }
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

#[derive(Debug)]
struct ReplayTurnPlan {
    initial_payload: Value,
    pending_after_response: HashMap<ProviderResponseId, Vec<ConfiguredPendingAgentTurn>>,
}

#[derive(Debug)]
enum ReplayTurnPlanError {
    IncompleteBoundary(String),
    InvalidInput(String),
}

impl std::fmt::Display for ReplayTurnPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IncompleteBoundary(reason) | Self::InvalidInput(reason) => {
                formatter.write_str(reason)
            }
        }
    }
}

fn replay_tool_contract_union(
    exchanges: &[RecordedProviderExchange],
) -> Result<Vec<ToolContract>, String> {
    let mut contracts = BTreeMap::<String, ToolContract>::new();
    for exchange in exchanges {
        for contract in &exchange.request.tools {
            match contracts.get(&contract.tool_name).cloned() {
                None => {
                    contracts.insert(contract.tool_name.clone(), contract.clone());
                }
                Some(previous) if previous == *contract => {}
                Some(previous) => {
                    let merged = merge_optional_tool_contract_projection(&previous, contract)
                        .ok_or_else(|| {
                            format!(
                                "tool contract for `{}` changed across replay turns",
                                contract.tool_name
                            )
                        })?;
                    contracts.insert(contract.tool_name.clone(), merged);
                }
            }
        }
    }
    Ok(contracts.into_values().collect())
}

/// 允许工具合同只投影掉可选输入字段。
///
/// Runtime profile 可能隐藏可选参数，重放需要把两次合同合并，才能验证任一记录中的
/// 工具调用；但不能借此掩盖语义变化。输入 schema 除顶层 `properties` 外，以及工具合同
/// 的其余字段都必须完全相同；共享属性定义和必填字段也必须保持一致。
fn merge_optional_tool_contract_projection(
    first: &ToolContract,
    second: &ToolContract,
) -> Option<ToolContract> {
    if first.tool_name != second.tool_name
        || first.output_schema != second.output_schema
        || first.error_schema != second.error_schema
        || first.side_effect_type != second.side_effect_type
        || first.idempotency_key_policy != second.idempotency_key_policy
        || first.timeout_policy != second.timeout_policy
        || first.cancellation_policy != second.cancellation_policy
        || first.retry_policy != second.retry_policy
        || first.artifact_policy != second.artifact_policy
        || first.permission_policy_ref != second.permission_policy_ref
    {
        return None;
    }

    let first_properties = optional_input_properties(&first.input_schema)?;
    let second_properties = optional_input_properties(&second.input_schema)?;
    if input_schema_without_properties(&first.input_schema)?
        != input_schema_without_properties(&second.input_schema)?
    {
        return None;
    }

    let first_required = required_input_properties(&first.input_schema)?;
    let second_required = required_input_properties(&second.input_schema)?;
    if first_required != second_required {
        return None;
    }

    for name in first_properties.keys().chain(second_properties.keys()) {
        match (first_properties.get(name), second_properties.get(name)) {
            (Some(first_definition), Some(second_definition))
                if first_definition == second_definition => {}
            (Some(_), Some(_)) => return None,
            (Some(_), None) | (None, Some(_))
                if first_required.contains(name) || second_required.contains(name) =>
            {
                return None;
            }
            (Some(_), None) | (None, Some(_)) => {}
            (None, None) => unreachable!("property key came from one of the property maps"),
        }
    }

    let mut merged = first.clone();
    let merged_properties = merged
        .input_schema
        .as_object_mut()
        .expect("validated input schema object")
        .entry("properties")
        .or_insert_with(|| serde_json::json!({}));
    let merged_properties = merged_properties
        .as_object_mut()
        .expect("validated input schema properties object");
    for (name, definition) in second_properties {
        merged_properties
            .entry(name)
            .or_insert_with(|| definition.clone());
    }
    for (name, definition) in first_properties {
        merged_properties.entry(name).or_insert(definition);
    }
    Some(merged)
}

fn optional_input_properties(schema: &Value) -> Option<serde_json::Map<String, Value>> {
    let object = schema.as_object()?;
    match object.get("properties") {
        None => Some(serde_json::Map::new()),
        Some(Value::Object(properties)) => Some(properties.clone()),
        Some(_) => None,
    }
}

fn input_schema_without_properties(schema: &Value) -> Option<Value> {
    let mut object = schema.as_object()?.clone();
    object.remove("properties");
    Some(Value::Object(object))
}

fn required_input_properties(schema: &Value) -> Option<HashSet<String>> {
    let required = match schema.get("required") {
        None => Vec::new(),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| value.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?,
        Some(_) => return None,
    };
    Some(required.into_iter().collect())
}

fn replay_turn_plan(events: &[RuntimeEvent]) -> Result<ReplayTurnPlan, ReplayTurnPlanError> {
    let mut initial_payload = None;
    let mut queued = HashMap::<TurnId, RuntimeEvent>::new();
    let mut pending_after_response =
        HashMap::<ProviderResponseId, Vec<ConfiguredPendingAgentTurn>>::new();
    let mut last_provider_response_id = None;

    for event in events {
        match event.event_type {
            RuntimeEventType::TaskCreated => {
                if initial_payload.is_none() {
                    initial_payload = event.payload.get("payload").cloned();
                }
            }
            RuntimeEventType::TurnQueued | RuntimeEventType::TurnUpdated => {
                if is_pending_transfer_event(event) {
                    if let Some(entries) = event
                        .payload
                        .get(RECOVERED_PENDING_TURNS_KEY)
                        .and_then(Value::as_array)
                    {
                        for entry in entries {
                            let synthetic =
                                inline_recovered_pending_event(event, entry).map_err(|error| {
                                    ReplayTurnPlanError::InvalidInput(error.to_string())
                                })?;
                            if let Some(turn_id) = synthetic.turn_id {
                                queued.insert(turn_id, synthetic);
                            }
                        }
                    }
                } else if let (Some(turn_id), Some(_)) =
                    (event.turn_id, event.payload.get("payload"))
                {
                    queued.insert(turn_id, event.clone());
                }
            }
            RuntimeEventType::TurnCancelled => {
                if let Some(turn_id) = event.turn_id {
                    queued.remove(&turn_id);
                }
            }
            RuntimeEventType::ProviderCompleted => {
                last_provider_response_id =
                    event.causal_context.provider_response_id.or_else(|| {
                        event
                            .payload
                            .get("provider_response_id")
                            .and_then(Value::as_str)
                            .and_then(|value| value.parse().ok())
                    });
            }
            RuntimeEventType::TurnStarted => {
                let Some(turn_id) = event.turn_id else {
                    continue;
                };
                let source = if event.payload.get("payload").is_some() {
                    event.clone()
                } else {
                    queued.remove(&turn_id).ok_or_else(|| {
                        ReplayTurnPlanError::IncompleteBoundary(format!(
                            "started turn {turn_id} has no durable queued or updated payload"
                        ))
                    })?
                };
                queued.remove(&turn_id);
                if initial_payload.is_none() && last_provider_response_id.is_none() {
                    initial_payload = source.payload.get("payload").cloned();
                    if initial_payload.is_some() {
                        continue;
                    }
                }
                let recovered = recovered_pending_turn_from_event(&source)
                    .map_err(|error| ReplayTurnPlanError::InvalidInput(error.to_string()))?
                    .ok_or_else(|| {
                        ReplayTurnPlanError::IncompleteBoundary(format!(
                            "started turn {turn_id} could not be reconstructed"
                        ))
                    })?;
                let response_id = last_provider_response_id.ok_or_else(|| {
                    ReplayTurnPlanError::IncompleteBoundary(format!(
                        "started turn {turn_id} has no preceding provider response boundary"
                    ))
                })?;
                pending_after_response.entry(response_id).or_default().push(
                    ConfiguredPendingAgentTurn {
                        turn: recovered.pending,
                        execution: recovered.execution,
                    },
                );
            }
            _ => {}
        }
    }

    let initial_payload = initial_payload.ok_or_else(|| {
        ReplayTurnPlanError::InvalidInput(
            "replay source has no initial task or recovered turn payload".to_owned(),
        )
    })?;
    Ok(ReplayTurnPlan {
        initial_payload,
        pending_after_response,
    })
}

fn legacy_transfer_payload_events(
    session_events: &[RuntimeEvent],
    referenced_sequence_no: u64,
    transfer_sequence_no: u64,
) -> Vec<RuntimeEvent> {
    let Some(queued) = session_events.iter().find(|event| {
        event.sequence_no == referenced_sequence_no
            && event.event_type == RuntimeEventType::TurnQueued
            && event.turn_id.is_some()
            && event.payload.get("payload").is_some()
    }) else {
        return Vec::new();
    };
    let turn_id = queued.turn_id;
    let latest_update = session_events
        .iter()
        .filter(|event| {
            event.sequence_no > referenced_sequence_no
                && event.sequence_no < transfer_sequence_no
                && event.event_type == RuntimeEventType::TurnUpdated
                && event.turn_id == turn_id
                && event.payload.get("payload").is_some()
        })
        .max_by_key(|event| event.sequence_no);
    let mut result = vec![queued.clone()];
    if let Some(update) = latest_update {
        result.push(update.clone());
    }
    result
}

impl RuntimeHost {
    async fn expand_replay_transfer_events(
        &self,
        session_id: SessionId,
        events: &[RuntimeEvent],
    ) -> Result<Vec<RuntimeEvent>, ClientError> {
        let mut present_sequences = events
            .iter()
            .map(|event| event.sequence_no)
            .collect::<HashSet<_>>();
        let mut expanded = events.to_vec();
        let legacy_transfers = events
            .iter()
            .filter(|event| is_pending_transfer_event(event))
            .filter(|event| event.payload.get(RECOVERED_PENDING_TURNS_KEY).is_none())
            .collect::<Vec<_>>();
        let session_events = if legacy_transfers.is_empty() {
            Vec::new()
        } else {
            self.storage
                .repositories
                .events
                .load(session_id, None, None)
                .await?
        };
        for event in legacy_transfers {
            let referenced_sequences = event
                .payload
                .get("recovered_pending_sequence_nos")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64);
            for sequence_no in referenced_sequences {
                for referenced in
                    legacy_transfer_payload_events(&session_events, sequence_no, event.sequence_no)
                {
                    // Keep only the queue payload and its latest pre-transfer
                    // update. Other source-task lifecycle events would create
                    // false provider/turn boundaries in the recovery task.
                    if present_sequences.insert(referenced.sequence_no) {
                        expanded.push(referenced);
                    }
                }
            }
        }
        expanded.sort_by_key(|event| event.sequence_no);
        Ok(expanded)
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
                .storage
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
        let execution =
            Box::pin(self.execute_deterministic_replay(session_id, source_task_id, capsule_id))
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
        let canonical_state = self
            .load_canonical_replay_state(session_id, source_task_id)
            .await?;
        let capsule = select_capsule(
            &canonical_state.projection.capsules,
            source_task_id,
            capsule_id,
        )?;
        let all_events = canonical_state.events;
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
        if let Err(reason) = validate_replay_capsule_identity(&capsule, &events) {
            return self
                .persist_failed_replay_attempt(
                    session_id,
                    &capsule,
                    expected_loop_action,
                    expected_verification,
                    reason,
                    started_at,
                )
                .await;
        }
        let prefix_integrity = if source_last_sequence_no == u64::MAX {
            self.storage
                .repositories
                .events
                .integrity(session_id, source_task_id)
                .await
        } else {
            self.storage
                .repositories
                .events
                .integrity_before(
                    session_id,
                    source_task_id,
                    source_last_sequence_no.saturating_add(1),
                )
                .await
        };
        let prefix_integrity = match prefix_integrity {
            Ok(integrity) => integrity,
            Err(error) => {
                return self
                    .persist_failed_replay_attempt(
                        session_id,
                        &capsule,
                        expected_loop_action,
                        expected_verification,
                        format!("source replay integrity could not be verified: {error}"),
                        started_at,
                    )
                    .await;
            }
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

        let replay_events = match self
            .expand_replay_transfer_events(session_id, &events)
            .await
        {
            Ok(events) => events,
            Err(error) => {
                return self
                    .persist_failed_replay_attempt(
                        session_id,
                        &capsule,
                        expected_loop_action,
                        expected_verification,
                        format!("turn transfer replay input could not be expanded: {error}"),
                        started_at,
                    )
                    .await;
            }
        };
        let turn_plan = match replay_turn_plan(&replay_events) {
            Ok(plan) => plan,
            Err(ReplayTurnPlanError::IncompleteBoundary(reason)) => {
                let execution = replay_terminal_execution(
                    &capsule,
                    ReplayExecutionStatus::Incomplete,
                    expected_loop_action,
                    expected_verification,
                    vec![format!(
                        "turn transition replay input is incomplete: {reason}"
                    )],
                    started_at,
                );
                return self.persist_replay_execution(session_id, execution).await;
            }
            Err(ReplayTurnPlanError::InvalidInput(reason)) => {
                return self
                    .persist_failed_replay_attempt(
                        session_id,
                        &capsule,
                        expected_loop_action,
                        expected_verification,
                        format!("turn transition replay input is invalid: {reason}"),
                        started_at,
                    )
                    .await;
            }
        };
        let capsule_response_ids = capsule
            .provider_exchanges
            .iter()
            .map(|exchange| exchange.response_id)
            .collect::<HashSet<_>>();
        let missing_transition_boundaries = turn_plan
            .pending_after_response
            .keys()
            .filter(|response_id| !capsule_response_ids.contains(response_id))
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        if !missing_transition_boundaries.is_empty() {
            let execution = replay_terminal_execution(
                &capsule,
                ReplayExecutionStatus::Incomplete,
                expected_loop_action,
                expected_verification,
                vec![format!(
                    "recorded turn transitions reference provider responses outside the replay capsule: {}",
                    missing_transition_boundaries.join(", ")
                )],
                started_at,
            );
            return self.persist_replay_execution(session_id, execution).await;
        }
        let ReplayTurnPlan {
            initial_payload: task_payload,
            mut pending_after_response,
        } = turn_plan;
        let prepared = async {
            let replay_artifacts = self
                .preflight_replay_artifacts(session_id, &capsule)
                .await?;
            let mut exchanges = Vec::with_capacity(capsule.provider_exchanges.len());
            for exchange in &capsule.provider_exchanges {
                let request_artifact = replay_artifacts
                    .get(&exchange.request_artifact_ref)
                    .expect("preflight retains every provider request artifact");
                let request: ProviderRequest = self
                    .read_replay_artifact(session_id, request_artifact, "provider_request_replay")
                    .await?;
                let response_artifact = replay_artifacts
                    .get(&exchange.response_artifact_ref)
                    .expect("preflight retains every provider response artifact");
                let response: ProviderResponse = self
                    .read_replay_artifact(session_id, response_artifact, "provider_response_replay")
                    .await?;
                if request.request_id != exchange.request_id
                    || request.task_id != source_task_id
                    || response.response_id != exchange.response_id
                {
                    return Err(ClientError::TaskExecution(format!(
                        "replay exchange {} does not match its artifact identities",
                        exchange.request_id
                    )));
                }
                let pending_turns_after_response = pending_after_response
                    .remove(&response.response_id)
                    .unwrap_or_default();
                exchanges.push(RecordedProviderExchange {
                    request,
                    response,
                    pending_turns_after_response,
                });
            }
            let mut tool_results = Vec::with_capacity(capsule.tool_results.len());
            for result in &capsule.tool_results {
                let result_artifact = replay_artifacts
                    .get(&result.result_artifact_ref)
                    .expect("preflight retains every tool result artifact");
                let envelope: ToolResultEnvelope = self
                    .read_replay_artifact(session_id, result_artifact, "tool_result_replay")
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
            let replay_tools =
                replay_tool_contract_union(&exchanges).map_err(ClientError::TaskExecution)?;
            let pending_turn_capacity = exchanges
                .iter()
                .map(|exchange| exchange.pending_turns_after_response.len())
                .sum::<usize>()
                .max(1);
            let (control_handle, execution_control) =
                agent_execution_channel(pending_turn_capacity);
            let provider = ArtifactReplayProvider::new(exchanges, control_handle)?;
            let tool_backend = ArtifactReplayToolBackend::new(tool_results);
            let workspace_root = self.execution_workspace_root()?;
            let policy = WorkspacePolicy::new(workspace_root)
                .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
            let tool_executor = ToolRuntime::new(policy)
                .with_replay_contracts(replay_tools.clone())
                .map_err(|error| ClientError::TaskExecution(error.to_string()))?
                .with_replay_backend(Arc::new(tool_backend.clone()));
            let contexts = self
                .storage
                .repositories
                .artifacts
                .contexts(source_task_id)
                .await?;
            let context_builder = replay_context_builder(contexts.first());
            let objective = model_prompt_from_payload(&task_payload);
            let execution_mode = execution_mode_from_payload(&task_payload)
                .map_err(|error| ClientError::TaskExecution(error.to_owned()))?;
            let tool_profile = tool_profile_from_payload(&task_payload)
                .map_err(|error| ClientError::TaskExecution(error.to_owned()))?;
            let task_contract = replay_task_contract(&task_payload, &objective)?;
            let max_elapsed_ms = task_payload
                .get("max_elapsed_ms")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0);
            let defer_external_verification = task_payload
                .get("defer_external_verification")
                .and_then(Value::as_bool)
                .unwrap_or(false);
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
            let harness = AgentHarness::new(provider.clone(), context_builder, tool_executor)
                .with_external_verifiers(external_verifiers);
            let run = ConfiguredAgentRun::new(AgentTaskRequest {
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
            })
            .with_replay_context(AgentReplayContext {
                initial_messages: first_request.messages,
                tools: replay_tools,
            })
            .with_task_contract(task_contract)
            .with_execution_mode(execution_mode.explicit())
            .with_tool_profile(tool_profile)
            .with_deferred_external_verification(defer_external_verification);
            let run = match max_elapsed_ms {
                Some(max_elapsed_ms) => run.with_max_elapsed_ms(max_elapsed_ms),
                None => run,
            };
            Ok::<_, ClientError>((harness, run, execution_control, provider, tool_backend))
        };
        let (harness, run, execution_control, provider, tool_backend) = match prepared.await {
            Ok(prepared) => prepared,
            Err(error) => {
                return self
                    .persist_failed_replay_attempt(
                        session_id,
                        &capsule,
                        expected_loop_action,
                        expected_verification,
                        format!("replay setup failed: {error}"),
                        started_at,
                    )
                    .await;
            }
        };
        let outcome = harness
            .execute_configured(run, execution_control, |_| {})
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

    async fn persist_failed_replay_attempt(
        &self,
        session_id: SessionId,
        capsule: &ReplayCapsule,
        expected_loop_action: Option<LoopAction>,
        expected_verification: Option<VerificationResult>,
        mismatch: String,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<ReplayExecution, ClientError> {
        let execution = replay_terminal_execution(
            capsule,
            ReplayExecutionStatus::Failed,
            expected_loop_action,
            expected_verification,
            vec![nonempty_replay_mismatch(mismatch)],
            started_at,
        );
        self.persist_replay_execution(session_id, execution).await
    }

    async fn persist_replay_execution(
        &self,
        session_id: SessionId,
        execution: ReplayExecution,
    ) -> Result<ReplayExecution, ClientError> {
        let event = self
            .ensure_replay_execution_event(session_id, &execution)
            .await?;
        let canonical = replay_execution_from_event(&event)?;
        let evaluation_store = self.storage.evaluation_store.clone();
        let stored = canonical.clone();
        // SQLite owns the replay fact. A projection failure is recoverable from the
        // canonical event and must not turn a committed replay into a reported failure.
        let _projection_result =
            run_blocking(move || evaluation_store.record_replay_execution(stored)).await;
        Ok(canonical)
    }

    pub(super) async fn persist_replay_capsule(
        &self,
        session_id: SessionId,
        turn_id: Option<TurnId>,
        capsule: ReplayCapsule,
    ) -> Result<ReplayCapsule, ClientError> {
        let event = self
            .ensure_replay_capsule_event(session_id, turn_id, &capsule)
            .await?;
        let canonical = replay_capsule_from_event(&event)?;
        let evaluation_store = self.storage.evaluation_store.clone();
        let stored = canonical.clone();
        // SQLite owns the replay fact. This JSON projection can be rebuilt from
        // ReplayCapsuleCreated if projection publication is interrupted.
        let _projection_result =
            run_blocking(move || evaluation_store.record_replay_capsule(stored)).await;
        Ok(canonical)
    }

    pub(super) async fn load_canonical_replay_state(
        &self,
        session_id: SessionId,
        source_task_id: TaskId,
    ) -> Result<CanonicalReplayState, ClientError> {
        let _writer = self.execution.event_writer.lock().await;
        let events = self
            .storage
            .repositories
            .events
            .load(session_id, Some(source_task_id), None)
            .await?;
        let projection = canonical_replay_projection(source_task_id, &events)?;
        let reconciliation_error = self
            .replace_replay_projection(source_task_id, &projection)
            .await
            .err()
            .map(|error| error.to_string());
        Ok(CanonicalReplayState {
            events,
            projection,
            reconciliation_error,
        })
    }

    pub(super) async fn replace_replay_projection(
        &self,
        source_task_id: TaskId,
        canonical: &CanonicalReplayProjection,
    ) -> Result<(), ClientError> {
        let evaluation_store = self.storage.evaluation_store.clone();
        let capsules = canonical.capsules.clone();
        let executions = canonical.executions.clone();
        run_blocking(move || {
            evaluation_store.replace_replay_projection_for_task(
                source_task_id,
                capsules,
                executions,
            )
        })
        .await??;
        Ok(())
    }

    async fn ensure_replay_execution_event(
        &self,
        session_id: SessionId,
        execution: &ReplayExecution,
    ) -> Result<RuntimeEvent, ClientError> {
        let existing = self
            .storage
            .repositories
            .events
            .load(session_id, Some(execution.source_task_id), None)
            .await?;
        if let Some(event) = find_replay_execution_event(&existing, execution) {
            return Ok(event.clone());
        }

        let mut event = host_event(
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
        );
        event.id = replay_execution_event_id(execution)?;
        let event_id = event.id;
        match self.record_event(event).await {
            Ok(()) => {}
            Err(append_error) => {
                // A retry can race with an earlier attempt, or rollout publication can fail
                // after SQLite commits. In both cases the canonical event is authoritative.
                let committed = self
                    .storage
                    .repositories
                    .events
                    .load(session_id, Some(execution.source_task_id), None)
                    .await?;
                if let Some(event) = find_replay_execution_event(&committed, execution) {
                    return Ok(event.clone());
                }
                return Err(append_error);
            }
        }
        self.storage
            .repositories
            .events
            .load(session_id, Some(execution.source_task_id), None)
            .await?
            .into_iter()
            .find(|event| event.id == event_id)
            .ok_or_else(|| {
                ClientError::TaskExecution(format!(
                    "canonical replay event {event_id} was not readable after commit"
                ))
            })
    }

    async fn ensure_replay_capsule_event(
        &self,
        session_id: SessionId,
        turn_id: Option<TurnId>,
        capsule: &ReplayCapsule,
    ) -> Result<RuntimeEvent, ClientError> {
        let existing = self
            .storage
            .repositories
            .events
            .load(session_id, Some(capsule.source_task_id), None)
            .await?;
        if let Some(event) = find_replay_capsule_event(&existing, capsule) {
            return Ok(event.clone());
        }

        let mut event = host_event(
            self.next_sequence_no(),
            session_id,
            Some(capsule.source_task_id),
            RuntimeEventType::ReplayCapsuleCreated,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!(
                    "deterministic replay capsule {} created",
                    capsule.capsule_id
                ),
                "record": capsule,
            }),
        );
        event.turn_id = turn_id;
        event.id = replay_capsule_event_id(capsule)?;
        let event_id = event.id;
        match self.record_event(event).await {
            Ok(()) => {}
            Err(append_error) => {
                // A retry can race with an earlier attempt, or rollout publication can fail
                // after SQLite commits. In both cases the canonical event is authoritative.
                let committed = self
                    .storage
                    .repositories
                    .events
                    .load(session_id, Some(capsule.source_task_id), None)
                    .await?;
                if let Some(event) = find_replay_capsule_event(&committed, capsule) {
                    return Ok(event.clone());
                }
                return Err(append_error);
            }
        }
        self.storage
            .repositories
            .events
            .load(session_id, Some(capsule.source_task_id), None)
            .await?
            .into_iter()
            .find(|event| event.id == event_id)
            .ok_or_else(|| {
                ClientError::TaskExecution(format!(
                    "canonical replay capsule event {event_id} was not readable after commit"
                ))
            })
    }

    async fn read_replay_artifact<T: serde::de::DeserializeOwned>(
        &self,
        session_id: SessionId,
        artifact: &ArtifactRecord,
        expected_type: &str,
    ) -> Result<T, ClientError> {
        validate_replay_artifact_metadata(artifact, session_id, expected_type)?;
        let bytes = self
            .storage
            .store
            .load_artifact_bytes_bounded(artifact, MAX_REPLAY_ARTIFACT_BYTES)
            .await?
            .ok_or_else(|| {
                ClientError::TaskExecution(format!(
                    "replay artifact blob {} is missing",
                    artifact.artifact_id
                ))
            })?;
        decode_replay_artifact_bytes(artifact, &bytes)
    }

    async fn preflight_replay_artifacts(
        &self,
        session_id: SessionId,
        capsule: &ReplayCapsule,
    ) -> Result<HashMap<ArtifactId, ArtifactRecord>, ClientError> {
        let mut artifacts = HashMap::new();
        let mut declared_bytes = 0_u64;

        for exchange in &capsule.provider_exchanges {
            self.preflight_replay_artifact(
                session_id,
                exchange.request_artifact_ref,
                "provider_request_replay",
                &mut artifacts,
                &mut declared_bytes,
            )
            .await?;
            self.preflight_replay_artifact(
                session_id,
                exchange.response_artifact_ref,
                "provider_response_replay",
                &mut artifacts,
                &mut declared_bytes,
            )
            .await?;
        }
        for result in &capsule.tool_results {
            self.preflight_replay_artifact(
                session_id,
                result.result_artifact_ref,
                "tool_result_replay",
                &mut artifacts,
                &mut declared_bytes,
            )
            .await?;
        }

        Ok(artifacts)
    }

    async fn preflight_replay_artifact(
        &self,
        session_id: SessionId,
        artifact_id: ArtifactId,
        expected_type: &str,
        artifacts: &mut HashMap<ArtifactId, ArtifactRecord>,
        declared_bytes: &mut u64,
    ) -> Result<(), ClientError> {
        let artifact = match artifacts.get(&artifact_id) {
            Some(artifact) => artifact.clone(),
            None => self
                .storage
                .repositories
                .artifacts
                .get(artifact_id)
                .await?
                .ok_or_else(|| {
                    ClientError::TaskExecution(format!("replay artifact {artifact_id} is missing"))
                })?,
        };
        validate_replay_artifact_metadata(&artifact, session_id, expected_type)?;
        charge_replay_artifact_size(&artifact, declared_bytes)?;
        artifacts.entry(artifact_id).or_insert(artifact);
        Ok(())
    }
}

fn replay_task_contract(payload: &Value, objective: &str) -> Result<TaskContract, ClientError> {
    let execution_mode = execution_mode_from_payload(payload)
        .map_err(|error| ClientError::TaskExecution(error.to_owned()))?;
    let explicit = explicit_task_contract(payload);
    let mut contract = task_contract_from_payload(payload)?;
    if !explicit && should_apply_legacy_adapter(payload, execution_mode) {
        LegacyTaskAdapter::new(payload, objective).apply_to(&mut contract);
        contract.validate().map_err(ClientError::TaskExecution)?;
    }
    Ok(contract)
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

fn charge_replay_artifact_size(
    artifact: &ArtifactRecord,
    declared_bytes: &mut u64,
) -> Result<(), ClientError> {
    if artifact.size_bytes == 0 || artifact.size_bytes > MAX_REPLAY_ARTIFACT_BYTES {
        return Err(ClientError::TaskExecution(format!(
            "replay artifact {} declared size must be between 1 and {} bytes, got {}",
            artifact.artifact_id, MAX_REPLAY_ARTIFACT_BYTES, artifact.size_bytes
        )));
    }
    let next_total = declared_bytes
        .checked_add(artifact.size_bytes)
        .ok_or_else(|| {
            ClientError::TaskExecution(
                "replay capsule artifact declared size overflowed u64".to_owned(),
            )
        })?;
    if next_total > MAX_REPLAY_CAPSULE_ARTIFACT_BYTES {
        return Err(ClientError::TaskExecution(format!(
            "replay capsule artifacts exceed the {} byte aggregate limit",
            MAX_REPLAY_CAPSULE_ARTIFACT_BYTES
        )));
    }
    *declared_bytes = next_total;
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

fn validate_replay_capsule_identity(
    capsule: &ReplayCapsule,
    events: &[RuntimeEvent],
) -> Result<(), String> {
    let source_run_id = golutra_core::RunId::from(capsule.source_task_id);
    if capsule.source_run_id != source_run_id {
        return Err(format!(
            "replay capsule source run identity diverged: expected {source_run_id}, observed {}",
            capsule.source_run_id
        ));
    }
    let recorded_runtime_config = events
        .iter()
        .find_map(|event| {
            event
                .payload
                .pointer("/run_provenance/runtime_config_digest")
                .and_then(Value::as_str)
        })
        .ok_or_else(|| "replay source has no recorded runtime config identity".to_owned())?;
    if recorded_runtime_config != capsule.runtime_config_digest {
        return Err(format!(
            "replay capsule runtime config identity diverged: expected {}, observed {}",
            capsule.runtime_config_digest, recorded_runtime_config
        ));
    }
    Ok(())
}

fn nonempty_replay_mismatch(mismatch: String) -> String {
    let mismatch = mismatch.trim();
    if mismatch.is_empty() {
        "replay preparation failed without an error detail".to_owned()
    } else {
        mismatch.to_owned()
    }
}

fn replay_execution_event_id(execution: &ReplayExecution) -> Result<EventId, ClientError> {
    let digest = Sha256::digest(execution.execution_id.as_bytes());
    let uuid = Uuid::from_slice(&digest[..16])
        .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
    Ok(EventId(uuid))
}

fn replay_capsule_event_id(capsule: &ReplayCapsule) -> Result<EventId, ClientError> {
    let digest = Sha256::digest(format!("replay-capsule:{}", capsule.capsule_id).as_bytes());
    let uuid = Uuid::from_slice(&digest[..16])
        .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
    Ok(EventId(uuid))
}

fn find_replay_execution_event<'a>(
    events: &'a [RuntimeEvent],
    execution: &ReplayExecution,
) -> Option<&'a RuntimeEvent> {
    events.iter().find(|event| {
        event.event_type == RuntimeEventType::ReplayExecuted
            && event
                .payload
                .pointer("/record/execution_id")
                .and_then(Value::as_str)
                == Some(execution.execution_id.as_str())
    })
}

fn find_replay_capsule_event<'a>(
    events: &'a [RuntimeEvent],
    capsule: &ReplayCapsule,
) -> Option<&'a RuntimeEvent> {
    events.iter().find(|event| {
        event.event_type == RuntimeEventType::ReplayCapsuleCreated
            && event
                .payload
                .pointer("/record/capsule_id")
                .and_then(Value::as_str)
                == Some(capsule.capsule_id.as_str())
    })
}

fn replay_execution_from_event(event: &RuntimeEvent) -> Result<ReplayExecution, ClientError> {
    if event.event_type != RuntimeEventType::ReplayExecuted {
        return Err(ClientError::TaskExecution(format!(
            "event {} is not a canonical replay execution",
            event.id
        )));
    }
    event
        .payload
        .get("record")
        .cloned()
        .ok_or_else(|| {
            ClientError::TaskExecution(format!("canonical replay event {} has no record", event.id))
        })
        .and_then(|record| serde_json::from_value(record).map_err(ClientError::Serialization))
}

fn replay_capsule_from_event(event: &RuntimeEvent) -> Result<ReplayCapsule, ClientError> {
    if event.event_type != RuntimeEventType::ReplayCapsuleCreated {
        return Err(ClientError::TaskExecution(format!(
            "event {} is not a canonical replay capsule",
            event.id
        )));
    }
    event
        .payload
        .get("record")
        .cloned()
        .ok_or_else(|| {
            ClientError::TaskExecution(format!(
                "canonical replay capsule event {} has no record",
                event.id
            ))
        })
        .and_then(|record| serde_json::from_value(record).map_err(ClientError::Serialization))
}

fn canonical_replay_projection(
    source_task_id: TaskId,
    events: &[RuntimeEvent],
) -> Result<CanonicalReplayProjection, ClientError> {
    let mut projection = CanonicalReplayProjection::default();
    for event in events {
        if event.task_id != Some(source_task_id) {
            return Err(ClientError::TaskExecution(format!(
                "canonical replay scope for task {source_task_id} contains event {} owned by {:?}",
                event.id, event.task_id
            )));
        }
        match event.event_type {
            RuntimeEventType::ReplayCapsuleCreated => {
                let capsule = replay_capsule_from_event(event)?;
                if capsule.source_task_id != source_task_id {
                    return Err(ClientError::TaskExecution(format!(
                        "canonical replay capsule {} belongs to task {} instead of {source_task_id}",
                        capsule.capsule_id, capsule.source_task_id
                    )));
                }
                projection.capsules.push(capsule);
            }
            RuntimeEventType::ReplayExecuted => {
                let execution = replay_execution_from_event(event)?;
                if execution.source_task_id != source_task_id {
                    return Err(ClientError::TaskExecution(format!(
                        "canonical replay execution {} belongs to task {} instead of {source_task_id}",
                        execution.execution_id, execution.source_task_id
                    )));
                }
                projection.executions.push(execution);
            }
            _ => {}
        }
    }
    Ok(projection)
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
        ProviderRequestId, ProviderResponseId, ProviderUsage, RunId, ToolCallId, ToolResultStatus,
        TurnId, UsageSource,
    };
    use golutra_eval::{ReplayMode, ReplayProviderExchange};
    use golutra_llm::{
        ProviderFinishReason, ProviderMessage, ProviderMessageMetadata, ProviderRole,
    };
    use golutra_tools::ToolRegistry;
    use serde_json::json;

    use super::*;

    fn provider_request(provider_id: &str, content: &str) -> ProviderRequest {
        ProviderRequest {
            request_id: ProviderRequestId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            session_id: None,
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
            cache_policy: Default::default(),
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

    fn replay_capsule_fixture(
        source_task_id: TaskId,
        source_last_sequence_no: u64,
        event_chain_digest: String,
    ) -> ReplayCapsule {
        ReplayCapsule {
            capsule_id: format!("replay-capsule-{source_task_id}"),
            source_task_id,
            source_run_id: RunId::from(source_task_id),
            mode: ReplayMode::DeterministicControlFlow,
            provider_exchanges: vec![ReplayProviderExchange {
                request_id: ProviderRequestId::new(),
                response_id: ProviderResponseId::new(),
                request_artifact_ref: ArtifactId::new(),
                response_artifact_ref: ArtifactId::new(),
            }],
            tool_results: Vec::new(),
            clock_seed: "2026-08-13T00:00:00Z".to_owned(),
            random_seed: 7,
            runtime_config_digest: "sha256:test-runtime".to_owned(),
            fixture_ref: None,
            event_chain_digest,
            source_last_sequence_no: Some(source_last_sequence_no),
            complete: true,
            missing_inputs: Vec::new(),
            limitations: Vec::new(),
            created_at: chrono::Utc::now(),
        }
    }

    async fn replay_capsule_source_fixture(host: &RuntimeHost) -> ReplayCapsule {
        let session_id = host.default_session_id();
        let source_task_id = TaskId::new();
        host.record_event(host_event(
            host.next_sequence_no(),
            session_id,
            Some(source_task_id),
            RuntimeEventType::TaskCreated,
            RuntimeEventSource::Runtime,
            json!({
                "payload": {"prompt": "replay fixture", "execution_mode": "open"},
                "run_provenance": {"runtime_config_digest": "sha256:test-runtime"},
            }),
        ))
        .await
        .expect("source event");
        let integrity = host
            .storage
            .repositories
            .events
            .integrity(session_id, source_task_id)
            .await
            .expect("source integrity");
        replay_capsule_fixture(
            source_task_id,
            integrity.last_sequence.expect("source sequence"),
            integrity.event_chain_digest,
        )
    }

    async fn record_replay_capsule_fixture(host: &RuntimeHost) -> ReplayCapsule {
        let capsule = replay_capsule_source_fixture(host).await;
        host.persist_replay_capsule(host.default_session_id(), None, capsule.clone())
            .await
            .expect("record capsule");
        capsule
    }

    #[test]
    fn replay_tool_union_preserves_tools_introduced_by_later_profiles() {
        let registry = ToolRegistry::p0_default();
        let read_file = registry
            .contract("read_file")
            .expect("read contract")
            .clone();
        let process_list = registry
            .contract("process_list")
            .expect("process contract")
            .clone();
        let mut first = provider_request("recorded", "initial coding turn");
        first.tools = vec![read_file.clone()];
        let mut second = provider_request("recorded", "full profile turn");
        second.tools = vec![process_list.clone(), read_file];
        let exchanges = vec![
            RecordedProviderExchange {
                request: first,
                response: provider_response(),
                pending_turns_after_response: Vec::new(),
            },
            RecordedProviderExchange {
                request: second,
                response: provider_response(),
                pending_turns_after_response: Vec::new(),
            },
        ];

        let tools = replay_tool_contract_union(&exchanges).expect("tool union");
        assert_eq!(
            tools
                .iter()
                .map(|contract| contract.tool_name.as_str())
                .collect::<Vec<_>>(),
            ["process_list", "read_file"]
        );
    }

    #[test]
    fn replay_tool_union_accepts_the_profile_filtered_shell_contract() {
        let registry = ToolRegistry::p0_default();
        let full_shell = registry.contract("shell").expect("shell contract").clone();
        let mut coding_shell = full_shell.clone();
        coding_shell
            .input_schema
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .expect("shell properties")
            .retain(|name, _| name != "background" && name != "yield_time_ms");

        let mut first = provider_request("recorded", "initial coding turn");
        first.tools = vec![coding_shell];
        let mut second = provider_request("recorded", "full profile turn");
        second.tools = vec![full_shell.clone()];
        let exchanges = vec![
            RecordedProviderExchange {
                request: first,
                response: provider_response(),
                pending_turns_after_response: Vec::new(),
            },
            RecordedProviderExchange {
                request: second,
                response: provider_response(),
                pending_turns_after_response: Vec::new(),
            },
        ];

        let tools = replay_tool_contract_union(&exchanges).expect("profile-compatible union");
        assert_eq!(tools, vec![full_shell]);
    }

    fn optional_projection_tool_contract(tool_name: &str) -> ToolContract {
        let registry = ToolRegistry::p0_default();
        let mut contract = registry
            .contract("read_file")
            .expect("read contract")
            .clone();
        contract.tool_name = tool_name.to_owned();
        contract.input_schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Required query"
                },
                "context": {
                    "type": "string",
                    "description": "Optional context"
                }
            },
            "required": ["query"]
        });
        contract
    }

    #[test]
    fn replay_tool_union_accepts_optional_projection_for_non_shell_tools() {
        let full = optional_projection_tool_contract("query_project");
        let mut projected = full.clone();
        projected
            .input_schema
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .expect("query properties")
            .remove("context");

        let mut first = provider_request("recorded", "first");
        first.tools = vec![projected];
        let mut second = provider_request("recorded", "second");
        second.tools = vec![full.clone()];

        let tools = replay_tool_contract_union(&[
            RecordedProviderExchange {
                request: first,
                response: provider_response(),
                pending_turns_after_response: Vec::new(),
            },
            RecordedProviderExchange {
                request: second,
                response: provider_response(),
                pending_turns_after_response: Vec::new(),
            },
        ])
        .expect("optional projection is replay-compatible");
        assert_eq!(tools, vec![full]);
    }

    #[test]
    fn replay_tool_union_rejects_required_property_projection() {
        let mut full = optional_projection_tool_contract("query_project");
        full.input_schema["required"] = json!(["query", "context"]);
        let mut projected = full.clone();
        projected
            .input_schema
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .expect("query properties")
            .remove("context");

        let mut first = provider_request("recorded", "first");
        first.tools = vec![projected];
        let mut second = provider_request("recorded", "second");
        second.tools = vec![full];
        let error = replay_tool_contract_union(&[
            RecordedProviderExchange {
                request: first,
                response: provider_response(),
                pending_turns_after_response: Vec::new(),
            },
            RecordedProviderExchange {
                request: second,
                response: provider_response(),
                pending_turns_after_response: Vec::new(),
            },
        ])
        .expect_err("required property drift must remain incomplete");
        assert!(error.contains("query_project"));
    }

    #[test]
    fn replay_tool_union_rejects_shared_property_definition_drift() {
        let first_contract = optional_projection_tool_contract("query_project");
        let mut second_contract = first_contract.clone();
        second_contract.input_schema["properties"]["query"]["description"] =
            json!("Different query semantics");

        let mut first = provider_request("recorded", "first");
        first.tools = vec![first_contract];
        let mut second = provider_request("recorded", "second");
        second.tools = vec![second_contract];
        let error = replay_tool_contract_union(&[
            RecordedProviderExchange {
                request: first,
                response: provider_response(),
                pending_turns_after_response: Vec::new(),
            },
            RecordedProviderExchange {
                request: second,
                response: provider_response(),
                pending_turns_after_response: Vec::new(),
            },
        ])
        .expect_err("shared property drift must remain incomplete");
        assert!(error.contains("query_project"));
    }

    #[test]
    fn replay_tool_union_still_rejects_unrelated_shell_contract_drift() {
        let registry = ToolRegistry::p0_default();
        let first_shell = registry.contract("shell").expect("shell contract").clone();
        let mut second_shell = first_shell.clone();
        second_shell.input_schema["properties"]["command"]["description"] =
            json!("different command semantics");
        let mut first = provider_request("recorded", "first");
        first.tools = vec![first_shell];
        let mut second = provider_request("recorded", "second");
        second.tools = vec![second_shell];

        let error = replay_tool_contract_union(&[
            RecordedProviderExchange {
                request: first,
                response: provider_response(),
                pending_turns_after_response: Vec::new(),
            },
            RecordedProviderExchange {
                request: second,
                response: provider_response(),
                pending_turns_after_response: Vec::new(),
            },
        ])
        .expect_err("unrelated contract drift must remain incomplete");
        assert!(error.contains("shell"));
    }

    #[tokio::test]
    async fn replay_provider_reports_request_divergence_and_queue_exhaustion() {
        let recorded = provider_request("recorded", "expected");
        let response = provider_response();
        let (execution, _control) = agent_execution_channel(1);
        let provider = ArtifactReplayProvider::new(
            vec![RecordedProviderExchange {
                request: recorded.clone(),
                response: response.clone(),
                pending_turns_after_response: Vec::new(),
            }],
            execution,
        )
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

    #[test]
    fn replay_turn_plan_reconstructs_updated_turns_and_profile_steers() {
        let session_id = SessionId::new();
        let task_id = TaskId::new();
        let ordinary_turn_id = TurnId::new();
        let steer_turn_id = TurnId::new();
        let first_response_id = ProviderResponseId::new();
        let second_response_id = ProviderResponseId::new();
        let mut events = Vec::new();
        events.push(host_event(
            1,
            session_id,
            Some(task_id),
            RuntimeEventType::TaskCreated,
            RuntimeEventSource::Runtime,
            json!({
                "payload": {
                    "prompt": "initial",
                    "execution_mode": "open",
                    "tool_profile": "coding",
                }
            }),
        ));
        events.push(host_event(
            2,
            session_id,
            Some(task_id),
            RuntimeEventType::ProviderCompleted,
            RuntimeEventSource::Provider,
            json!({"provider_response_id": first_response_id}),
        ));
        let mut queued = host_event(
            3,
            session_id,
            Some(task_id),
            RuntimeEventType::TurnQueued,
            RuntimeEventSource::User,
            json!({
                "command_id": CommandId::new(),
                "payload": {
                    "prompt": "old prompt",
                    "execution_mode": "strict",
                    "tool_profile": "full",
                    "task_contract": TaskContract::conversational(vec!["done".to_owned()]),
                }
            }),
        );
        queued.turn_id = Some(ordinary_turn_id);
        events.push(queued);
        let mut updated = host_event(
            4,
            session_id,
            Some(task_id),
            RuntimeEventType::TurnUpdated,
            RuntimeEventSource::User,
            json!({
                "command_id": CommandId::new(),
                "payload": {
                    "prompt": "updated prompt",
                    "execution_mode": "strict",
                    "tool_profile": "full",
                    "task_contract": TaskContract::conversational(vec!["done".to_owned()]),
                }
            }),
        );
        updated.turn_id = Some(ordinary_turn_id);
        events.push(updated);
        let mut started = host_event(
            5,
            session_id,
            Some(task_id),
            RuntimeEventType::TurnStarted,
            RuntimeEventSource::User,
            json!({}),
        );
        started.turn_id = Some(ordinary_turn_id);
        events.push(started);
        events.push(host_event(
            6,
            session_id,
            Some(task_id),
            RuntimeEventType::ProviderCompleted,
            RuntimeEventSource::Provider,
            json!({"provider_response_id": second_response_id}),
        ));
        let mut steer = host_event(
            7,
            session_id,
            Some(task_id),
            RuntimeEventType::TurnQueued,
            RuntimeEventSource::User,
            json!({
                "command_id": CommandId::new(),
                "payload": {
                    "prompt": "use coding tools",
                    "steer": true,
                    "tool_profile": "coding",
                }
            }),
        );
        steer.turn_id = Some(steer_turn_id);
        events.push(steer);
        let mut steer_started = host_event(
            8,
            session_id,
            Some(task_id),
            RuntimeEventType::TurnStarted,
            RuntimeEventSource::User,
            json!({}),
        );
        steer_started.turn_id = Some(steer_turn_id);
        events.push(steer_started);

        let plan = replay_turn_plan(&events).expect("turn plan");
        assert_eq!(plan.initial_payload["prompt"], "initial");
        let ordinary = &plan.pending_after_response[&first_response_id][0];
        assert_eq!(ordinary.turn.content, "updated prompt");
        assert_eq!(
            ordinary.execution.execution_mode,
            Some(golutra_protocol::AgentExecutionMode::Strict)
        );
        assert_eq!(
            ordinary.execution.tool_profile,
            Some(AgentToolProfile::Full)
        );
        let steer = &plan.pending_after_response[&second_response_id][0];
        assert!(steer.turn.steer);
        assert_eq!(steer.execution.execution_mode, None);
        assert_eq!(steer.execution.tool_profile, Some(AgentToolProfile::Coding));
    }

    #[test]
    fn replay_turn_plan_uses_inline_recovery_transfer_payloads() {
        let session_id = SessionId::new();
        let task_id = TaskId::new();
        let pending_turn_id = TurnId::new();
        let response_id = ProviderResponseId::new();
        let events = vec![
            host_event(
                1,
                session_id,
                Some(task_id),
                RuntimeEventType::TaskCreated,
                RuntimeEventSource::Runtime,
                json!({"payload": {"prompt": "initial", "execution_mode": "open"}}),
            ),
            host_event(
                2,
                session_id,
                Some(task_id),
                RuntimeEventType::ProviderCompleted,
                RuntimeEventSource::Provider,
                json!({"provider_response_id": response_id}),
            ),
            host_event(
                3,
                session_id,
                Some(task_id),
                RuntimeEventType::TurnQueued,
                RuntimeEventSource::Runtime,
                json!({
                    "recovery": "durable_pending_turn_batch",
                    "recovered_pending_turns": [{
                        "sequence_no": 1,
                        "turn_id": pending_turn_id,
                        "command_id": CommandId::new(),
                        "actor": {"kind": "runtime", "id": "replay-test"},
                        "payload": {
                            "prompt": "recovered full turn",
                            "execution_mode": "strict",
                            "tool_profile": "full",
                        },
                    }],
                }),
            ),
            {
                let mut started = host_event(
                    4,
                    session_id,
                    Some(task_id),
                    RuntimeEventType::TurnStarted,
                    RuntimeEventSource::User,
                    json!({"summary": "queued user turn started"}),
                );
                started.turn_id = Some(pending_turn_id);
                started
            },
        ];

        let plan = replay_turn_plan(&events).expect("replay turn plan");
        let pending = &plan.pending_after_response[&response_id][0];
        assert_eq!(pending.turn.content, "recovered full turn");
        assert_eq!(
            pending.execution.execution_mode,
            Some(golutra_protocol::AgentExecutionMode::Strict)
        );
        assert_eq!(pending.execution.tool_profile, Some(AgentToolProfile::Full));
    }

    #[test]
    fn replay_turn_plan_uses_a_referenced_payload_for_the_initial_recovery_turn() {
        let session_id = SessionId::new();
        let source_task_id = TaskId::new();
        let recovery_task_id = TaskId::new();
        let turn_id = TurnId::new();
        let mut source = host_event(
            1,
            session_id,
            Some(source_task_id),
            RuntimeEventType::TurnQueued,
            RuntimeEventSource::User,
            json!({
                "command_id": CommandId::new(),
                "payload": {
                    "prompt": "baseline recovered turn",
                    "execution_mode": "open",
                    "tool_profile": "coding",
                },
            }),
        );
        source.turn_id = Some(turn_id);
        let transfer = host_event(
            2,
            session_id,
            Some(recovery_task_id),
            RuntimeEventType::TurnQueued,
            RuntimeEventSource::Runtime,
            json!({
                "recovery": "durable_pending_turn_batch",
                "recovered_pending_sequence_nos": [1],
            }),
        );
        let mut started = host_event(
            3,
            session_id,
            Some(recovery_task_id),
            RuntimeEventType::TurnStarted,
            RuntimeEventSource::Runtime,
            json!({"recovery": "durable_pending_turn"}),
        );
        started.turn_id = Some(turn_id);

        let plan = replay_turn_plan(&[source, transfer, started]).expect("recovery turn plan");
        assert_eq!(plan.initial_payload["prompt"], "baseline recovered turn");
        assert!(plan.pending_after_response.is_empty());
    }

    #[test]
    fn legacy_recovery_transfer_replays_the_latest_queued_update() {
        let session_id = SessionId::new();
        let source_task_id = TaskId::new();
        let turn_id = TurnId::new();
        let mut queued = host_event(
            10,
            session_id,
            Some(source_task_id),
            RuntimeEventType::TurnQueued,
            RuntimeEventSource::User,
            json!({
                "command_id": CommandId::new(),
                "payload": {"prompt": "before update", "execution_mode": "open"},
            }),
        );
        queued.turn_id = Some(turn_id);
        let mut updated = host_event(
            11,
            session_id,
            Some(source_task_id),
            RuntimeEventType::TurnUpdated,
            RuntimeEventSource::User,
            json!({
                "command_id": CommandId::new(),
                "payload": {"prompt": "after update", "execution_mode": "open"},
            }),
        );
        updated.turn_id = Some(turn_id);
        let events = legacy_transfer_payload_events(&[queued, updated], 10, 12);

        assert_eq!(events.len(), 2);
        assert_eq!(
            events
                .last()
                .and_then(|event| event.payload.get("payload"))
                .and_then(|payload| payload.get("prompt"))
                .and_then(Value::as_str),
            Some("after update"),
            "legacy transfer must use the latest pre-transfer payload"
        );
    }

    #[tokio::test]
    async fn replay_provider_injects_pending_turns_at_the_recorded_response_boundary() {
        let workspace = tempfile::tempdir().expect("workspace");
        let initial_request = provider_request("recorded", "initial");
        let pending_turn_id = TurnId::new();
        let text_response = |content: &str| {
            let mut response = provider_response();
            response.message = Some(ProviderMessage {
                role: ProviderRole::Assistant,
                content: content.to_owned(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: ProviderMessageMetadata::default(),
            });
            response
        };
        let pending = ConfiguredPendingAgentTurn {
            turn: PendingAgentTurn {
                command_id: CommandId::new(),
                turn_id: pending_turn_id,
                content: "follow up".to_owned(),
                task_contract: Some(TaskContract::conversational(Vec::new())),
                output_schema: None,
                external_verifiers: Vec::new(),
                max_elapsed_ms: None,
                defer_external_verification: false,
                external_verifiers_require_os_sandbox: false,
                allow_network: false,
                yolo: false,
                steer: false,
            },
            execution: golutra_runtime::PendingTurnExecutionOptions {
                execution_mode: Some(golutra_protocol::AgentExecutionMode::Open),
                tool_profile: Some(AgentToolProfile::Coding),
            },
        };
        let exchanges = vec![
            RecordedProviderExchange {
                request: initial_request.clone(),
                response: text_response("initial answer"),
                pending_turns_after_response: vec![pending],
            },
            RecordedProviderExchange {
                request: provider_request("recorded", "follow up"),
                response: text_response("follow-up answer"),
                pending_turns_after_response: Vec::new(),
            },
        ];
        let (execution, control) = agent_execution_channel(2);
        let provider = ArtifactReplayProvider::new(exchanges, execution).expect("provider");
        let tools =
            ToolRuntime::new(WorkspacePolicy::new(workspace.path()).expect("workspace policy"));
        let harness = AgentHarness::new(provider, ContextBuilder::default(), tools);
        let run = ConfiguredAgentRun::new(AgentTaskRequest {
            session_id: SessionId::new(),
            task_id: initial_request.task_id,
            turn_id: initial_request.turn_id,
            objective: "initial".to_owned(),
            completion_criteria: Vec::new(),
            output_schema: None,
            touched_code: false,
            contributors: Vec::new(),
            tools: Vec::new(),
        })
        .with_replay_context(AgentReplayContext {
            initial_messages: initial_request.messages,
            tools: Vec::new(),
        })
        .with_task_contract(TaskContract::conversational(Vec::new()))
        .with_tool_profile(AgentToolProfile::Coding);

        let outcome = harness
            .execute_configured(run, control, |_| {})
            .await
            .expect("replay outcome");
        assert_eq!(outcome.final_turn_id, pending_turn_id);
        assert_eq!(outcome.final_message.as_deref(), Some("follow-up answer"));
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

    #[test]
    fn replay_artifact_declared_size_rejects_zero_and_single_artifact_overflow() {
        let session_id = SessionId::new();
        let mut artifact = ArtifactRecord {
            artifact_id: ArtifactId::new(),
            session_id,
            turn_id: None,
            tool_call_id: None,
            artifact_type: "provider_request_replay".to_owned(),
            uri: "artifact://replay".to_owned(),
            checksum: "sha256:fixture".to_owned(),
            size_bytes: 0,
            created_at: chrono::Utc::now(),
            producer: "test".to_owned(),
            redaction_status: RedactionStatus::Raw,
            retention_policy: "test".to_owned(),
            provenance_refs: Vec::new(),
        };
        let mut total = 0;

        let zero_error = charge_replay_artifact_size(&artifact, &mut total)
            .expect_err("zero-byte replay artifact must fail");
        assert!(zero_error.to_string().contains("between 1"));
        assert_eq!(total, 0);

        artifact.size_bytes = MAX_REPLAY_ARTIFACT_BYTES.saturating_add(1);
        let oversized_error = charge_replay_artifact_size(&artifact, &mut total)
            .expect_err("oversized replay artifact must fail");
        assert!(oversized_error.to_string().contains("declared size"));
        assert_eq!(total, 0);
    }

    #[test]
    fn replay_capsule_declared_size_is_aggregated_with_checked_arithmetic() {
        let artifact = ArtifactRecord {
            artifact_id: ArtifactId::new(),
            session_id: SessionId::new(),
            turn_id: None,
            tool_call_id: None,
            artifact_type: "tool_result_replay".to_owned(),
            uri: "artifact://replay".to_owned(),
            checksum: "sha256:fixture".to_owned(),
            size_bytes: MAX_REPLAY_ARTIFACT_BYTES,
            created_at: chrono::Utc::now(),
            producer: "test".to_owned(),
            redaction_status: RedactionStatus::Raw,
            retention_policy: "test".to_owned(),
            provenance_refs: Vec::new(),
        };
        let mut total = 0;
        for _ in 0..10 {
            charge_replay_artifact_size(&artifact, &mut total)
                .expect("the aggregate limit is inclusive");
        }
        assert_eq!(total, MAX_REPLAY_CAPSULE_ARTIFACT_BYTES);

        let aggregate_error = charge_replay_artifact_size(&artifact, &mut total)
            .expect_err("the next artifact must exceed the capsule budget");
        assert!(aggregate_error.to_string().contains("aggregate limit"));
        assert_eq!(total, MAX_REPLAY_CAPSULE_ARTIFACT_BYTES);

        total = u64::MAX;
        let overflow_error = charge_replay_artifact_size(&artifact, &mut total)
            .expect_err("u64 overflow must be rejected");
        assert!(overflow_error.to_string().contains("overflowed u64"));
        assert_eq!(total, u64::MAX);
    }

    #[tokio::test]
    async fn missing_replay_artifact_persists_failed_execution_and_canonical_event() {
        let host = RuntimeHost::in_memory().await.expect("host");
        let session_id = host.default_session_id();
        let capsule = record_replay_capsule_fixture(&host).await;

        let execution = host
            .execute_deterministic_replay(session_id, capsule.source_task_id, None)
            .await
            .expect("failed replay must still produce an execution");

        assert_eq!(execution.status, ReplayExecutionStatus::Failed);
        assert!(
            execution
                .mismatches
                .iter()
                .any(|mismatch| mismatch.contains("artifact") && mismatch.contains("missing"))
        );
        let state = host
            .storage
            .evaluation_store
            .snapshot()
            .expect("evaluation state");
        assert_eq!(state.replay_executions, vec![execution.clone()]);
        let events = host
            .storage
            .repositories
            .events
            .load(session_id, Some(capsule.source_task_id), None)
            .await
            .expect("events");
        assert!(find_replay_execution_event(&events, &execution).is_some());
    }

    #[tokio::test]
    async fn replay_execution_persistence_is_idempotent() {
        let host = RuntimeHost::in_memory().await.expect("host");
        let session_id = host.default_session_id();
        let capsule = record_replay_capsule_fixture(&host).await;
        let execution = replay_terminal_execution(
            &capsule,
            ReplayExecutionStatus::Failed,
            None,
            None,
            vec!["fixture replay failure".to_owned()],
            chrono::Utc::now(),
        );

        host.persist_replay_execution(session_id, execution.clone())
            .await
            .expect("first persistence");
        host.persist_replay_execution(session_id, execution.clone())
            .await
            .expect("retry persistence");

        let state = host
            .storage
            .evaluation_store
            .snapshot()
            .expect("evaluation state");
        assert_eq!(state.replay_executions, vec![execution.clone()]);
        let events = host
            .storage
            .repositories
            .events
            .load(session_id, Some(capsule.source_task_id), None)
            .await
            .expect("events");
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    find_replay_execution_event(std::slice::from_ref(event), &execution).is_some()
                })
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn replay_execution_projection_is_rebuilt_from_canonical_event() {
        let host = RuntimeHost::in_memory().await.expect("host");
        let session_id = host.default_session_id();
        let capsule = record_replay_capsule_fixture(&host).await;
        let execution = replay_terminal_execution(
            &capsule,
            ReplayExecutionStatus::Failed,
            None,
            None,
            vec!["fixture replay failure".to_owned()],
            chrono::Utc::now(),
        );

        host.ensure_replay_execution_event(session_id, &execution)
            .await
            .expect("canonical event");
        assert!(
            host.storage
                .evaluation_store
                .snapshot()
                .expect("state before repair")
                .replay_executions
                .is_empty()
        );

        host.load_canonical_replay_state(session_id, capsule.source_task_id)
            .await
            .expect("projection repair");

        assert_eq!(
            host.storage
                .evaluation_store
                .snapshot()
                .expect("state after repair")
                .replay_executions,
            vec![execution]
        );
    }

    #[tokio::test]
    async fn replay_capsule_persistence_is_idempotent() {
        let host = RuntimeHost::in_memory().await.expect("host");
        let session_id = host.default_session_id();
        let capsule = replay_capsule_source_fixture(&host).await;

        let first = host
            .persist_replay_capsule(session_id, None, capsule.clone())
            .await
            .expect("first persistence");
        let second = host
            .persist_replay_capsule(session_id, None, capsule.clone())
            .await
            .expect("idempotent persistence");

        assert_eq!(first, capsule);
        assert_eq!(second, capsule);
        let events = host
            .storage
            .repositories
            .events
            .load(session_id, Some(capsule.source_task_id), None)
            .await
            .expect("events");
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    find_replay_capsule_event(std::slice::from_ref(event), &capsule).is_some()
                })
                .count(),
            1
        );
        assert_eq!(
            host.storage
                .evaluation_store
                .snapshot()
                .expect("evaluation state")
                .replay_capsules,
            vec![capsule]
        );
    }

    #[tokio::test]
    async fn replay_capsule_projection_is_rebuilt_from_canonical_event() {
        let host = RuntimeHost::in_memory().await.expect("host");
        let session_id = host.default_session_id();
        let capsule = replay_capsule_source_fixture(&host).await;

        host.ensure_replay_capsule_event(session_id, None, &capsule)
            .await
            .expect("canonical event");
        assert!(
            host.storage
                .evaluation_store
                .snapshot()
                .expect("state before repair")
                .replay_capsules
                .is_empty()
        );

        host.load_canonical_replay_state(session_id, capsule.source_task_id)
            .await
            .expect("projection repair");

        assert_eq!(
            host.storage
                .evaluation_store
                .snapshot()
                .expect("state after repair")
                .replay_capsules,
            vec![capsule]
        );
    }

    #[tokio::test]
    async fn replay_capsule_event_failure_does_not_leave_a_store_only_record() {
        let host = RuntimeHost::in_memory().await.expect("host");
        let session_id = host.default_session_id();
        let capsule = replay_capsule_source_fixture(&host).await;
        let mut conflicting = host_event(
            host.next_sequence_no(),
            session_id,
            Some(capsule.source_task_id),
            RuntimeEventType::ToolCompleted,
            RuntimeEventSource::Tool,
            json!({"summary": "event id collision"}),
        );
        conflicting.id = replay_capsule_event_id(&capsule).expect("deterministic event id");
        host.record_event(conflicting)
            .await
            .expect("conflicting event");

        assert!(
            host.persist_replay_capsule(session_id, None, capsule)
                .await
                .is_err()
        );
        assert!(
            host.storage
                .evaluation_store
                .snapshot()
                .expect("evaluation state")
                .replay_capsules
                .is_empty()
        );
    }

    #[tokio::test]
    async fn replay_event_failure_does_not_leave_a_store_only_execution() {
        let host = RuntimeHost::in_memory().await.expect("host");
        let session_id = host.default_session_id();
        let capsule = record_replay_capsule_fixture(&host).await;
        let execution = replay_terminal_execution(
            &capsule,
            ReplayExecutionStatus::Failed,
            None,
            None,
            vec!["fixture replay failure".to_owned()],
            chrono::Utc::now(),
        );
        let mut conflicting = host_event(
            host.next_sequence_no(),
            session_id,
            Some(capsule.source_task_id),
            RuntimeEventType::ToolCompleted,
            RuntimeEventSource::Tool,
            json!({"summary": "event id collision"}),
        );
        conflicting.id = replay_execution_event_id(&execution).expect("deterministic event id");
        host.record_event(conflicting)
            .await
            .expect("conflicting event");

        assert!(
            host.persist_replay_execution(session_id, execution.clone())
                .await
                .is_err()
        );
        let state = host
            .storage
            .evaluation_store
            .snapshot()
            .expect("evaluation state");
        assert!(
            state
                .replay_executions
                .iter()
                .all(|stored| stored.execution_id != execution.execution_id)
        );
    }

    #[test]
    fn replay_restores_the_recorded_task_contract_instead_of_using_a_default() {
        let payload = json!({
            "prompt": "refactor the runtime",
            "task_contract": {
                "workspace_change": "forbidden",
                "verification": "independent",
                "require_objective_validation": true,
                "max_correction_rounds": 0
            }
        });

        let contract = replay_task_contract(&payload, "refactor the runtime").expect("contract");

        assert_eq!(
            contract.workspace_change,
            golutra_core::WorkspaceChangeRequirement::Forbidden
        );
        assert_eq!(
            contract.verification,
            golutra_core::VerificationRequirement::Independent
        );
        assert!(contract.require_objective_validation);
        assert_eq!(contract.max_correction_rounds, 0);
    }

    #[test]
    fn replay_reconstructs_legacy_change_contract_without_a_fake_delivery_path() {
        let payload = json!({"prompt": "修改 runtime 代码"});
        let contract = replay_task_contract(&payload, "修改 runtime 代码").expect("contract");

        assert_eq!(
            contract.workspace_change,
            golutra_core::WorkspaceChangeRequirement::Required
        );
        assert!(contract.required_paths.is_empty());
    }
}
