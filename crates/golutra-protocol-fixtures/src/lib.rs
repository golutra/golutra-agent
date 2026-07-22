use chrono::Utc;
use golutra_core::{
    Actor, ActorKind, BudgetState, EvidenceId, LoopAction, LoopDecision, LoopDecisionId,
    RegressionCampaign, RegressionExecution, SessionId, TaskId, TaskStatus, TurnId, VerificationId,
};
use golutra_eval::{
    AppliedCandidate, AutomationCandidate, BenchmarkPromotion, BenchmarkRun, CausalComparison,
    CostRecord, CounterfactualReplay, EvaluationCase, EvaluationResult, EvaluationRun,
    GeneratedTask, ImprovementCandidate, PostTaskReview, PromotionDecision, RegressionResult,
    SecurityUtilityResult, SkillCandidate,
};
use golutra_evolution::{
    EnvironmentRecipe, EvolutionState, GeneratedTaskExecution, NoveltyRecord, OpenEndedBudget,
    OpenEndedRun, SkillLifecycleRecord, SkillManifest,
};
use golutra_governor::RuntimeGovernorDecision;
use golutra_memory::MemoryRecord;
use golutra_protocol::{
    AgentItem, AgentStreamEvent, AgentThreadRef, AgentTurnOptions, AgentTurnResult, AgentTurnStart,
    AgentTurnStartResponse, ArtifactChunk, ArtifactReadRequest, CommandAck, ContextProjection,
    DebugProjection, EvaluationProjection, EventFilter, EventPage, EventPageRequest,
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, ProtocolHandshake, RuntimeEvent,
    RuntimeEventSource, RuntimeEventType, RuntimeQuery, SessionCommand, SessionCommandKind,
    SessionPage, SessionPageRequest, SessionWindow, SessionWindowRequest, StateProjection,
    StorageMaintenanceReport, StorageStats, TaskTracePage, TaskTraceRequest,
    TuiDriverProtocolBundle, UserProjection, VisibleStep,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ScenarioFixture {
    pub name: String,
    pub commands: Vec<SessionCommand>,
    pub events: Vec<RuntimeEvent>,
    pub state_projection: StateProjection,
    pub user_projection: UserProjection,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SdkProtocolBundle {
    pub agent_thread_ref: AgentThreadRef,
    pub agent_turn_start: AgentTurnStart,
    pub agent_turn_start_response: AgentTurnStartResponse,
    pub agent_turn_options: AgentTurnOptions,
    pub agent_turn_result: AgentTurnResult,
    pub agent_item: AgentItem,
    pub agent_stream_event: AgentStreamEvent,
    pub json_rpc_request: JsonRpcRequest,
    pub json_rpc_response: JsonRpcResponse,
    pub json_rpc_notification: JsonRpcNotification,
    pub command: SessionCommand,
    pub command_ack: CommandAck,
    pub query: RuntimeQuery,
    pub event_filter: EventFilter,
    pub event_page_request: EventPageRequest,
    pub event_page: EventPage,
    pub session_page_request: SessionPageRequest,
    pub session_page: SessionPage,
    pub session_window_request: SessionWindowRequest,
    pub session_window: SessionWindow,
    pub event: RuntimeEvent,
    pub state_projection: StateProjection,
    pub user_projection: UserProjection,
    pub debug_projection: DebugProjection,
    pub context_projection: ContextProjection,
    pub evaluation_projection: EvaluationProjection,
    pub task_trace_request: TaskTraceRequest,
    pub task_trace_page: TaskTracePage,
    pub artifact_read_request: ArtifactReadRequest,
    pub artifact_chunk: ArtifactChunk,
    pub memory_record: MemoryRecord,
    pub evaluation_result: EvaluationResult,
    pub evaluation_case: EvaluationCase,
    pub evaluation_run: EvaluationRun,
    pub post_task_review: PostTaskReview,
    pub improvement_candidate: ImprovementCandidate,
    pub automation_candidate: AutomationCandidate,
    pub generated_task: GeneratedTask,
    pub skill_candidate: SkillCandidate,
    pub benchmark_promotion: BenchmarkPromotion,
    pub benchmark_run: BenchmarkRun,
    pub counterfactual_replay: CounterfactualReplay,
    pub causal_comparison: CausalComparison,
    pub cost_record: CostRecord,
    pub security_utility_result: SecurityUtilityResult,
    pub regression_result: RegressionResult,
    pub regression_campaign: RegressionCampaign,
    pub regression_execution: RegressionExecution,
    pub promotion_decision: PromotionDecision,
    pub applied_candidate: AppliedCandidate,
    pub open_ended_budget: OpenEndedBudget,
    pub open_ended_run: OpenEndedRun,
    pub environment_recipe: EnvironmentRecipe,
    pub novelty_record: NoveltyRecord,
    pub generated_task_execution: GeneratedTaskExecution,
    pub skill_manifest: SkillManifest,
    pub skill_lifecycle_record: SkillLifecycleRecord,
    pub evolution_state: EvolutionState,
    pub governor_decision: RuntimeGovernorDecision,
    pub storage_stats: StorageStats,
    pub storage_maintenance_report: StorageMaintenanceReport,
    pub protocol_handshake: ProtocolHandshake,
    pub tui_driver: TuiDriverProtocolBundle,
}

#[must_use]
pub fn read_only_task() -> ScenarioFixture {
    fixture_with_status(
        "read_only_task",
        TaskStatus::Completed,
        LoopAction::StopSuccess,
    )
}

#[must_use]
pub fn tool_failure_task() -> ScenarioFixture {
    fixture_with_status(
        "tool_failure_task",
        TaskStatus::Blocked,
        LoopAction::Blocked,
    )
}

#[must_use]
pub fn abort_task() -> ScenarioFixture {
    fixture_with_status("abort_task", TaskStatus::Aborting, LoopAction::StopPartial)
}

#[must_use]
pub fn verification_failed_task() -> ScenarioFixture {
    fixture_with_status(
        "verification_failed_task",
        TaskStatus::Failed,
        LoopAction::StopFailed,
    )
}

#[must_use]
pub fn multi_frontend_attach_task() -> ScenarioFixture {
    fixture_with_status(
        "multi_frontend_attach_task",
        TaskStatus::Running,
        LoopAction::Continue,
    )
}

#[must_use]
pub fn all_scenarios() -> Vec<ScenarioFixture> {
    vec![
        read_only_task(),
        tool_failure_task(),
        abort_task(),
        verification_failed_task(),
        multi_frontend_attach_task(),
    ]
}

fn fixture_with_status(name: &str, status: TaskStatus, action: LoopAction) -> ScenarioFixture {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    let actor = Actor {
        kind: ActorKind::Cli,
        id: "fixture-cli".to_owned(),
    };
    let command = SessionCommand {
        command_id: golutra_core::CommandId::new(),
        session_id: Some(session_id),
        kind: SessionCommandKind::Prompt,
        idempotency_key: format!("{name}-prompt"),
        actor,
        payload: json!({
            "objective": format!("fixture objective: {name}")
        }),
        timestamp: Utc::now(),
    };
    let event = RuntimeEvent {
        id: golutra_core::EventId::new(),
        sequence_no: 1,
        session_id,
        turn_id: Some(turn_id),
        task_id: Some(task_id),
        parent_event_id: None,
        event_type: RuntimeEventType::TaskCreated,
        timestamp: Utc::now(),
        source: RuntimeEventSource::Runtime,
        payload: json!({
            "name": name,
            "status": status
        }),
        payload_ref: None,
        durable: true,
    };
    let visible_step = VisibleStep {
        label: "runtime".to_owned(),
        status: format!("{status:?}"),
        summary: format!("scenario {name} reached {status:?}"),
    };
    let loop_decision = LoopDecision {
        decision_id: LoopDecisionId::new(),
        task_id,
        turn_id,
        action,
        reason: format!("fixture loop decision for {name}"),
        evidence_refs: vec![EvidenceId::new()],
        verification_ref: Some(VerificationId::new()),
        policy_ref: None,
        budget_state: BudgetState {
            planned_input_tokens: Some(128),
            actual_input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            estimated_cost: None,
            budget_remaining: Some(2048),
            compact_recommended: false,
            cost_risk: "low".to_owned(),
        },
        tool_state: "fixture".to_owned(),
        model_state: "fixture".to_owned(),
        next_step: None,
    };
    let final_message = terminal_message(status);
    ScenarioFixture {
        name: name.to_owned(),
        commands: vec![command],
        events: vec![event],
        state_projection: StateProjection {
            session_id,
            active_task_id: Some(task_id),
            task_status: status,
            runtime_lane: None,
            last_sequence_no: 1,
            visible_steps: vec![visible_step.clone()],
            pending_approval: None,
            final_message: final_message.clone(),
            last_loop_decision: Some(loop_decision),
            last_verification: None,
        },
        user_projection: UserProjection {
            session_id,
            task_id: Some(task_id),
            status,
            visible_steps: vec![visible_step],
            pending_approval: None,
            final_message,
            residual_risks: residual_risks(status),
        },
    }
}

fn terminal_message(status: TaskStatus) -> Option<String> {
    match status {
        TaskStatus::Completed => Some("fixture completed".to_owned()),
        TaskStatus::Partial | TaskStatus::Failed | TaskStatus::Blocked => {
            Some(format!("fixture ended with {status:?}"))
        }
        _ => None,
    }
}

fn residual_risks(status: TaskStatus) -> Vec<String> {
    match status {
        TaskStatus::Completed => Vec::new(),
        _ => vec![format!("fixture residual risk for {status:?}")],
    }
}

pub fn protocol_schema_names() -> Vec<&'static str> {
    vec![
        "ScenarioFixture",
        "AgentThreadRef",
        "AgentTurnStart",
        "AgentTurnStartResponse",
        "AgentTurnOptions",
        "AgentTurnResult",
        "AgentItem",
        "AgentStreamEvent",
        "JsonRpcRequest",
        "JsonRpcResponse",
        "JsonRpcNotification",
        "SessionCommand",
        "RuntimeEvent",
        "StateProjection",
        "UserProjection",
        "SdkProtocolBundle",
        "TuiDriverProtocolBundle",
    ]
}

pub fn export_sdk_schema(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    let schema = schemars::schema_for!(SdkProtocolBundle);
    let bytes = serde_json::to_vec_pretty(&schema)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if let Some(parent) = path.as_ref().parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::schema_for;

    #[test]
    fn fixtures_roundtrip() {
        for fixture in all_scenarios() {
            let encoded = serde_json::to_string_pretty(&fixture).expect("fixture serializes");
            let decoded: ScenarioFixture =
                serde_json::from_str(&encoded).expect("fixture deserializes");
            assert_eq!(fixture, decoded);
        }
    }

    #[test]
    fn schema_smoke() {
        let scenario_schema = schema_for!(ScenarioFixture);
        let command_schema = schema_for!(SessionCommand);
        let event_schema = schema_for!(RuntimeEvent);

        let scenario_json = serde_json::to_value(&scenario_schema).expect("schema serializes");
        let command_json = serde_json::to_value(&command_schema).expect("schema serializes");
        let event_json = serde_json::to_value(&event_schema).expect("schema serializes");

        assert!(scenario_json.is_object());
        assert!(command_json.is_object());
        assert!(event_json.is_object());
        assert_eq!(protocol_schema_names().len(), 17);
    }
}
