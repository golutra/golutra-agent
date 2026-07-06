use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use golutra_context::{
    ContextBuilder, ContextContributor, ContextError, provider_request_from_plan,
    token_usage_record,
};
use golutra_core::{
    Actor, BudgetState, BusyPolicy, BusyPolicyDecision, CheckpointId, CheckpointType, CommandId,
    DecisionId, EventId, LaneId, LoopAction, LoopDecision, LoopDecisionId, PolicyId, RuntimeLane,
    SessionId, TaskId, TaskStatus, ToolResultStatus, TurnId, VerificationCheck, VerificationId,
    VerificationRecord, VerificationResult, WorkspaceCheckpoint, WorkspaceId,
};
use golutra_llm::{LlmProvider, ProviderError, ProviderFinishReason};
use golutra_protocol::{RuntimeEvent, RuntimeEventSource, RuntimeEventType};
use golutra_tools::{BasicToolExecutor, ToolError, ToolExecutionReport, ToolRequest};
use golutra_verify::{VerificationInput, VerificationRunner};
use serde_json::json;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub use golutra_protocol::UserProjection;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RuntimeLaneError {
    #[error("session already has an active task")]
    ActiveTaskExists,
    #[error("session has no active runtime lane")]
    LaneNotFound,
    #[error("actor is not the active controller")]
    NonActiveController,
}

#[derive(Debug, Error)]
pub enum AgentLoopError {
    #[error("context build failed")]
    Context(#[from] ContextError),
    #[error("provider call failed: {0}")]
    Provider(#[from] ProviderError),
    #[error("tool execution failed")]
    Tool(#[from] ToolError),
}

#[derive(Debug, Error)]
pub enum CheckpointError {
    #[error("checkpoint io failed: {0}")]
    Io(String),
    #[error("changed file is outside workspace: {0}")]
    OutsideWorkspace(String),
    #[error("changed file is excluded from checkpoint: {0}")]
    Excluded(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeTransition {
    pub lane: RuntimeLane,
    pub event: RuntimeEvent,
}

#[derive(Debug, Default)]
pub struct RuntimeLaneManager {
    lanes_by_session: HashMap<SessionId, RuntimeLane>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentTaskRequest {
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub turn_id: TurnId,
    pub objective: String,
    pub completion_criteria: Vec<String>,
    pub touched_code: bool,
    pub contributors: Vec<ContextContributor>,
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentLoopOutcome {
    pub verification: VerificationRecord,
    pub loop_decision: LoopDecision,
    pub tool_reports: Vec<ToolExecutionReport>,
    pub final_message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentLoopTraceEvent {
    ContextBuilt {
        contributors: Vec<String>,
        planned_input_tokens: u64,
    },
    ProviderStarted {
        provider_id: String,
        model_id: String,
    },
    ProviderCompleted {
        provider_id: String,
        model_id: String,
        finish_reason: ProviderFinishReason,
        tool_call_count: usize,
    },
    ToolStarted {
        tool_name: String,
    },
    ToolCompleted {
        tool_name: String,
        summary: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceCheckpointManager {
    workspace_root: PathBuf,
    checkpoint_root: PathBuf,
}

impl WorkspaceCheckpointManager {
    #[must_use]
    pub fn new(workspace_root: impl Into<PathBuf>, checkpoint_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            checkpoint_root: checkpoint_root.into(),
        }
    }

    pub fn create_checkpoint(
        &self,
        workspace_id: WorkspaceId,
        task_id: TaskId,
        turn_id: TurnId,
        changed_files: &[PathBuf],
        created_after_event_id: EventId,
    ) -> Result<WorkspaceCheckpoint, CheckpointError> {
        let checkpoint_id = CheckpointId::new();
        let checkpoint_dir = self.checkpoint_root.join(checkpoint_id.to_string());
        fs::create_dir_all(&checkpoint_dir)
            .map_err(|error| CheckpointError::Io(error.to_string()))?;

        let mut copied_files = Vec::new();
        for changed_file in changed_files {
            let relative_path = self.relative_checkpoint_path(changed_file)?;
            let source_path = self.workspace_root.join(&relative_path);
            if source_path.is_file() {
                let target_path = checkpoint_dir.join(&relative_path);
                if let Some(parent) = target_path.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|error| CheckpointError::Io(error.to_string()))?;
                }
                fs::copy(&source_path, &target_path)
                    .map_err(|error| CheckpointError::Io(error.to_string()))?;
                copied_files.push(relative_path.display().to_string());
            }
        }

        Ok(WorkspaceCheckpoint {
            checkpoint_id,
            workspace_id,
            task_id,
            turn_id,
            checkpoint_type: CheckpointType::Snapshot,
            changed_files: copied_files,
            artifact_refs: Vec::new(),
            created_after_event_id,
            restore_hint: format!("copy files from {}", checkpoint_dir.display()),
            retention_policy: "p0_keep_until_task_cleanup".to_owned(),
        })
    }

    fn relative_checkpoint_path(&self, changed_file: &Path) -> Result<PathBuf, CheckpointError> {
        let path = if changed_file.is_absolute() {
            changed_file.to_path_buf()
        } else {
            self.workspace_root.join(changed_file)
        };
        let canonical_workspace = self
            .workspace_root
            .canonicalize()
            .map_err(|error| CheckpointError::Io(error.to_string()))?;
        let canonical_path = path
            .canonicalize()
            .map_err(|error| CheckpointError::Io(error.to_string()))?;
        let relative = canonical_path
            .strip_prefix(&canonical_workspace)
            .map_err(|_| CheckpointError::OutsideWorkspace(path.display().to_string()))?;

        if is_checkpoint_excluded(relative) {
            return Err(CheckpointError::Excluded(relative.display().to_string()));
        }
        Ok(relative.to_path_buf())
    }
}

#[derive(Debug)]
pub struct AgentLoop<P> {
    provider: P,
    context_builder: ContextBuilder,
    tool_executor: BasicToolExecutor,
    verifier: VerificationRunner,
    max_iterations: u32,
}

impl<P> AgentLoop<P>
where
    P: LlmProvider,
{
    #[must_use]
    pub fn new(
        provider: P,
        context_builder: ContextBuilder,
        tool_executor: BasicToolExecutor,
    ) -> Self {
        Self {
            provider,
            context_builder,
            tool_executor,
            verifier: VerificationRunner,
            max_iterations: 4,
        }
    }

    pub async fn run(&self, request: AgentTaskRequest) -> Result<AgentLoopOutcome, AgentLoopError> {
        self.run_with_trace(request, |_| {}).await
    }

    pub async fn run_with_trace<F>(
        &self,
        request: AgentTaskRequest,
        mut trace: F,
    ) -> Result<AgentLoopOutcome, AgentLoopError>
    where
        F: FnMut(AgentLoopTraceEvent),
    {
        let mut tool_reports = Vec::new();
        let mut last_assistant_message = None;
        let mut last_budget_state = BudgetState {
            planned_input_tokens: None,
            actual_input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            estimated_cost: None,
            budget_remaining: None,
            compact_recommended: false,
            cost_risk: "low".to_owned(),
        };

        for iteration in 0..self.max_iterations {
            if iteration > 0 && !tool_reports.is_empty() {
                break;
            }

            let plan = self.context_builder.build(
                request.task_id,
                request.turn_id,
                request.contributors.clone(),
            )?;
            trace(AgentLoopTraceEvent::ContextBuilt {
                contributors: plan.contributors.clone(),
                planned_input_tokens: plan.budget_snapshot.planned_input_tokens,
            });
            let provider_contract = self.provider.contract();
            let provider_request = provider_request_from_plan(
                &plan,
                request.task_id,
                request.turn_id,
                provider_contract.provider_id.clone(),
                provider_contract.model_id.clone(),
                request.tools.clone(),
            );
            trace(AgentLoopTraceEvent::ProviderStarted {
                provider_id: provider_request.provider_id.clone(),
                model_id: provider_request.model_id.clone(),
            });
            let provider_response = self.provider.complete(provider_request.clone()).await?;
            if let Some(message) = provider_response
                .message
                .as_ref()
                .filter(|message| !message.content.trim().is_empty())
            {
                last_assistant_message = Some(message.content.trim().to_owned());
            }
            trace(AgentLoopTraceEvent::ProviderCompleted {
                provider_id: provider_request.provider_id.clone(),
                model_id: provider_request.model_id.clone(),
                finish_reason: provider_response.finish_reason,
                tool_call_count: provider_response.tool_calls.len(),
            });
            let usage_record = token_usage_record(
                &provider_request,
                provider_response.response_id,
                &plan.budget_snapshot,
                &provider_response.usage,
            );
            last_budget_state = BudgetState {
                planned_input_tokens: Some(plan.budget_snapshot.planned_input_tokens),
                actual_input_tokens: usage_record.input_tokens,
                output_tokens: usage_record.output_tokens,
                total_tokens: usage_record.total_tokens,
                estimated_cost: usage_record.estimated_cost.map(|cost| cost.to_string()),
                budget_remaining: plan
                    .budget_snapshot
                    .budget_limit
                    .checked_sub(plan.budget_snapshot.planned_input_tokens),
                compact_recommended: false,
                cost_risk: "low".to_owned(),
            };

            if provider_response.tool_calls.is_empty() {
                break;
            }

            for tool_call in provider_response.tool_calls {
                trace(AgentLoopTraceEvent::ToolStarted {
                    tool_name: tool_call.tool_name.clone(),
                });
                let report = self.tool_executor.execute(ToolRequest {
                    tool_call_id: golutra_core::ToolCallId::new(),
                    session_id: request.session_id,
                    turn_id: Some(request.turn_id),
                    tool_name: tool_call.tool_name,
                    arguments: tool_call.arguments,
                })?;
                trace(AgentLoopTraceEvent::ToolCompleted {
                    tool_name: report.envelope.tool_name.clone(),
                    summary: report.envelope.summary.clone(),
                });
                tool_reports.push(report);
            }
        }

        let evidence_refs = tool_reports
            .iter()
            .flat_map(|report| report.evidence.iter().map(|evidence| evidence.evidence_id))
            .collect::<Vec<_>>();
        let command_checks = tool_reports
            .iter()
            .map(|report| VerificationCheck {
                name: format!("tool:{}", report.envelope.tool_name),
                command: None,
                passed: report.envelope.status == ToolResultStatus::Ok,
                evidence_refs: report.envelope.evidence_refs.clone(),
                message: report.envelope.summary.clone(),
            })
            .collect::<Vec<_>>();
        let verification = if accepts_text_response_without_evidence(
            &request,
            last_assistant_message.as_deref(),
            &tool_reports,
        ) {
            text_response_verification(&request)
        } else {
            self.verifier.verify(VerificationInput {
                task_id: request.task_id,
                objective: request.objective.clone(),
                completion_criteria: request.completion_criteria.clone(),
                evidence_refs,
                command_checks,
                touched_code: request.touched_code,
            })
        };
        let loop_decision = loop_decision_from_verification(
            request.task_id,
            request.turn_id,
            &verification,
            last_budget_state,
        );

        Ok(AgentLoopOutcome {
            final_message: final_message_from_outcome(
                last_assistant_message,
                &tool_reports,
                &verification,
            ),
            verification,
            loop_decision,
            tool_reports,
        })
    }
}

impl RuntimeLaneManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start_task(
        &mut self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        task_id: TaskId,
        turn_id: TurnId,
        active_controller: Actor,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        if self
            .lanes_by_session
            .get(&session_id)
            .is_some_and(|lane| is_active_status(lane.status))
        {
            return Err(RuntimeLaneError::ActiveTaskExists);
        }

        let lane = RuntimeLane {
            lane_id: LaneId::new(),
            workspace_id,
            session_id,
            task_id,
            active_turn_id: Some(turn_id),
            active_controller,
            status: TaskStatus::Running,
            pending_turns: Vec::new(),
            injected_inputs: Vec::new(),
            busy_policy_default: BusyPolicy::Append,
        };
        self.lanes_by_session.insert(session_id, lane.clone());

        Ok(RuntimeTransition {
            event: lane_event(
                &lane,
                turn_id,
                sequence_no,
                RuntimeEventType::TaskCreated,
                "runtime lane started task",
            ),
            lane,
        })
    }

    pub fn decide_busy_policy(
        &self,
        session_id: SessionId,
        command_id: CommandId,
        actor: &Actor,
        requested_policy: BusyPolicy,
    ) -> Result<BusyPolicyDecision, RuntimeLaneError> {
        let lane = self
            .lanes_by_session
            .get(&session_id)
            .ok_or(RuntimeLaneError::LaneNotFound)?;
        let is_active_controller = lane.active_controller == *actor;

        let (applied_policy, reason) = if is_active_controller {
            (
                BusyPolicy::Append,
                "active controller input is appended to the runtime lane",
            )
        } else {
            (
                BusyPolicy::Reject,
                "non-active controller cannot drive the active task",
            )
        };

        Ok(BusyPolicyDecision {
            decision_id: DecisionId::new(),
            lane_id: lane.lane_id,
            command_id,
            requested_policy,
            applied_policy,
            reason: reason.to_owned(),
            safe_to_inject: false,
            affected_turn_id: lane.active_turn_id,
        })
    }

    pub fn abort(
        &mut self,
        session_id: SessionId,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        self.set_status(
            session_id,
            TaskStatus::Aborting,
            sequence_no,
            RuntimeEventType::TaskAborted,
        )
    }

    pub fn pause(
        &mut self,
        session_id: SessionId,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        self.set_status(
            session_id,
            TaskStatus::Paused,
            sequence_no,
            RuntimeEventType::LoopDecided,
        )
    }

    pub fn resume(
        &mut self,
        session_id: SessionId,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        self.set_status(
            session_id,
            TaskStatus::Running,
            sequence_no,
            RuntimeEventType::TurnStarted,
        )
    }

    pub fn finish_task(
        &mut self,
        session_id: SessionId,
        status: TaskStatus,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        self.set_status(
            session_id,
            status,
            sequence_no,
            RuntimeEventType::TaskCompleted,
        )
    }

    #[must_use]
    pub fn lane(&self, session_id: SessionId) -> Option<&RuntimeLane> {
        self.lanes_by_session.get(&session_id)
    }

    fn set_status(
        &mut self,
        session_id: SessionId,
        status: TaskStatus,
        sequence_no: u64,
        event_type: RuntimeEventType,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        let lane = self
            .lanes_by_session
            .get_mut(&session_id)
            .ok_or(RuntimeLaneError::LaneNotFound)?;
        lane.status = status;
        let turn_id = lane.active_turn_id.unwrap_or_default();
        Ok(RuntimeTransition {
            lane: lane.clone(),
            event: lane_event(
                lane,
                turn_id,
                sequence_no,
                event_type,
                "runtime lane status changed",
            ),
        })
    }
}

#[must_use]
pub fn runtime_boundary() -> &'static str {
    "SessionCommand -> RuntimeEvent -> StateProjection -> LoopDecision"
}

fn loop_decision_from_verification(
    task_id: TaskId,
    turn_id: TurnId,
    verification: &VerificationRecord,
    budget_state: BudgetState,
) -> LoopDecision {
    let action = match verification.result {
        VerificationResult::Pass => LoopAction::StopSuccess,
        VerificationResult::Partial => LoopAction::StopPartial,
        VerificationResult::Fail => LoopAction::StopFailed,
        VerificationResult::Unknown => LoopAction::Blocked,
    };

    LoopDecision {
        decision_id: LoopDecisionId::new(),
        task_id,
        turn_id,
        action,
        reason: format!("verification result: {:?}", verification.result),
        evidence_refs: verification.evidence_refs.clone(),
        verification_ref: Some(verification.verification_id),
        policy_ref: Option::<PolicyId>::None,
        budget_state,
        tool_state: "p0_tool_reports_recorded".to_owned(),
        model_state: "p0_provider_response_recorded".to_owned(),
        next_step: None,
    }
}

fn final_message_from_outcome(
    assistant_message: Option<String>,
    tool_reports: &[ToolExecutionReport],
    verification: &VerificationRecord,
) -> Option<String> {
    let summaries = tool_reports
        .iter()
        .map(|report| report.envelope.summary.trim())
        .filter(|summary| !summary.is_empty())
        .collect::<Vec<_>>();

    if verification.result == VerificationResult::Pass
        && assistant_message
            .as_ref()
            .is_some_and(|message| !message.trim().is_empty())
    {
        return assistant_message;
    }

    if summaries.is_empty() {
        return match verification.result {
            VerificationResult::Pass => Some("Completed.".to_owned()),
            VerificationResult::Partial
            | VerificationResult::Fail
            | VerificationResult::Unknown => {
                Some("Task finished without enough evidence to verify completion.".to_owned())
            }
        };
    }

    match verification.result {
        VerificationResult::Pass => Some(format!("Completed: {}", summaries.join("; "))),
        VerificationResult::Partial | VerificationResult::Fail | VerificationResult::Unknown => {
            Some(format!(
                "Could not fully complete: {}",
                summaries.join("; ")
            ))
        }
    }
}

fn accepts_text_response_without_evidence(
    request: &AgentTaskRequest,
    assistant_message: Option<&str>,
    tool_reports: &[ToolExecutionReport],
) -> bool {
    !request.touched_code
        && tool_reports.is_empty()
        && assistant_message.is_some_and(|message| !message.trim().is_empty())
        && !objective_requires_workspace_evidence(&request.objective)
}

fn text_response_verification(request: &AgentTaskRequest) -> VerificationRecord {
    VerificationRecord {
        verification_id: VerificationId::new(),
        task_id: request.task_id,
        objective: request.objective.clone(),
        completion_criteria: request.completion_criteria.clone(),
        checks: vec![VerificationCheck {
            name: "assistant_response".to_owned(),
            command: None,
            passed: true,
            evidence_refs: Vec::new(),
            message: "assistant response produced".to_owned(),
        }],
        evidence_refs: Vec::new(),
        result: VerificationResult::Pass,
        policy_status: "conversation_response".to_owned(),
        residual_risks: Vec::new(),
    }
}

fn objective_requires_workspace_evidence(objective: &str) -> bool {
    let lower = objective.to_ascii_lowercase();
    const ENGLISH_MARKERS: &[&str] = &[
        "write",
        "create",
        "edit",
        "modify",
        "update",
        "delete",
        "read",
        "list",
        "search",
        "find",
        "inspect",
        "run",
        "test",
        "build",
        "fix",
        "debug",
        "refactor",
        "file",
        "code",
        "workspace",
        "diff",
        "commit",
    ];
    const CJK_MARKERS: &[&str] = &[
        "写",
        "创建",
        "修改",
        "更新",
        "删除",
        "读取",
        "查看",
        "列出",
        "搜索",
        "查找",
        "检查",
        "运行",
        "测试",
        "构建",
        "修复",
        "重构",
        "文件",
        "代码",
        "项目",
        "工作区",
        "提交",
    ];

    ENGLISH_MARKERS.iter().any(|marker| lower.contains(marker))
        || CJK_MARKERS.iter().any(|marker| objective.contains(marker))
}

#[must_use]
pub fn is_active_status(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Running
            | TaskStatus::WaitingApproval
            | TaskStatus::Pausing
            | TaskStatus::Paused
    )
}

fn lane_event(
    lane: &RuntimeLane,
    turn_id: TurnId,
    sequence_no: u64,
    event_type: RuntimeEventType,
    summary: &str,
) -> RuntimeEvent {
    RuntimeEvent {
        id: golutra_core::EventId::new(),
        sequence_no,
        session_id: lane.session_id,
        turn_id: Some(turn_id),
        task_id: Some(lane.task_id),
        parent_event_id: None,
        event_type,
        timestamp: chrono::Utc::now(),
        source: RuntimeEventSource::Runtime,
        payload: json!({
            "summary": summary,
            "lane_id": lane.lane_id.to_string(),
            "status": lane.status,
        }),
        payload_ref: None,
        durable: true,
    }
}

#[must_use]
pub fn checkpoint_fingerprint(checkpoint: &WorkspaceCheckpoint) -> String {
    let mut hasher = Sha256::new();
    hasher.update(checkpoint.checkpoint_id.to_string().as_bytes());
    for changed_file in &checkpoint.changed_files {
        hasher.update(changed_file.as_bytes());
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn is_checkpoint_excluded(relative_path: &Path) -> bool {
    let path_text = relative_path.to_string_lossy();
    path_text.starts_with(".git")
        || path_text.contains(".env")
        || path_text.contains(".ssh")
        || path_text.contains("id_rsa")
        || path_text.contains("id_ed25519")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use golutra_context::ContextBuilder;
    use golutra_core::ActorKind;
    use golutra_llm::MockProvider;
    use golutra_policy::WorkspacePolicy;
    use golutra_tools::BasicToolExecutor;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn prevents_second_active_task_in_same_session() {
        let mut manager = RuntimeLaneManager::new();
        let session_id = SessionId::new();
        let actor = actor("cli");

        manager
            .start_task(
                WorkspaceId::new(),
                session_id,
                TaskId::new(),
                TurnId::new(),
                actor.clone(),
                1,
            )
            .expect("first task starts");
        let result = manager.start_task(
            WorkspaceId::new(),
            session_id,
            TaskId::new(),
            TurnId::new(),
            actor,
            2,
        );

        assert_eq!(
            result.expect_err("second task rejected"),
            RuntimeLaneError::ActiveTaskExists
        );
    }

    #[test]
    fn rejects_non_active_controller_input() {
        let mut manager = RuntimeLaneManager::new();
        let session_id = SessionId::new();
        manager
            .start_task(
                WorkspaceId::new(),
                session_id,
                TaskId::new(),
                TurnId::new(),
                actor("cli"),
                1,
            )
            .expect("task starts");

        let decision = manager
            .decide_busy_policy(
                session_id,
                CommandId::new(),
                &actor("web"),
                BusyPolicy::Append,
            )
            .expect("decision exists");

        assert_eq!(decision.applied_policy, BusyPolicy::Reject);
        assert!(!decision.safe_to_inject);
    }

    #[test]
    fn abort_moves_lane_to_aborting() {
        let mut manager = RuntimeLaneManager::new();
        let session_id = SessionId::new();
        manager
            .start_task(
                WorkspaceId::new(),
                session_id,
                TaskId::new(),
                TurnId::new(),
                actor("cli"),
                1,
            )
            .expect("task starts");

        let transition = manager.abort(session_id, 2).expect("abort works");

        assert_eq!(transition.lane.status, TaskStatus::Aborting);
        assert_eq!(transition.event.event_type, RuntimeEventType::TaskAborted);
    }

    fn actor(id: &str) -> Actor {
        Actor {
            kind: ActorKind::Cli,
            id: id.to_owned(),
        }
    }

    #[tokio::test]
    async fn agent_loop_provider_error_includes_detail() {
        let workspace = tempdir().expect("workspace");
        let provider = golutra_llm::GenaiProviderAdapter::unconfigured();
        let executor =
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

        let error = agent_loop
            .run(AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "你好".to_owned(),
                completion_criteria: vec!["assistant response".to_owned()],
                touched_code: false,
                contributors: Vec::new(),
                tools: Vec::new(),
            })
            .await
            .expect_err("provider error");

        assert!(error.to_string().contains("provider call failed"));
        assert!(error.to_string().contains("provider is not configured"));
    }

    #[tokio::test]
    async fn agent_loop_stops_success_when_tool_evidence_exists() {
        let workspace = tempdir().expect("workspace");
        let provider = MockProvider::tool_call(
            "write_file",
            json!({"path": "result.txt", "content": "done"}),
        );
        let executor =
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let session_id = SessionId::new();

        let outcome = agent_loop
            .run(AgentTaskRequest {
                session_id,
                task_id,
                turn_id,
                objective: "write result".to_owned(),
                completion_criteria: vec!["file written".to_owned()],
                touched_code: true,
                contributors: Vec::new(),
                tools: vec!["write_file".to_owned()],
            })
            .await
            .expect("loop runs");

        assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
        assert_eq!(
            outcome.final_message,
            Some("Completed: file written".to_owned())
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
            "done"
        );
    }

    #[tokio::test]
    async fn agent_loop_blocks_without_evidence() {
        let workspace = tempdir().expect("workspace");
        let provider = MockProvider::text_response("done");
        let executor =
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

        let outcome = agent_loop
            .run(AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "claim done".to_owned(),
                completion_criteria: vec!["objective evidence".to_owned()],
                touched_code: true,
                contributors: Vec::new(),
                tools: Vec::new(),
            })
            .await
            .expect("loop runs");

        assert_eq!(outcome.loop_decision.action, LoopAction::StopFailed);
        assert_eq!(
            outcome.final_message,
            Some("Task finished without enough evidence to verify completion.".to_owned())
        );
    }

    #[tokio::test]
    async fn agent_loop_accepts_plain_conversation_response_without_tool_evidence() {
        let workspace = tempdir().expect("workspace");
        let provider = MockProvider::text_response("你好，我在。");
        let executor =
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

        let outcome = agent_loop
            .run(AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "你好".to_owned(),
                completion_criteria: vec!["assistant response".to_owned()],
                touched_code: false,
                contributors: Vec::new(),
                tools: Vec::new(),
            })
            .await
            .expect("loop runs");

        assert_eq!(outcome.loop_decision.action, LoopAction::StopSuccess);
        assert_eq!(outcome.verification.result, VerificationResult::Pass);
        assert_eq!(outcome.final_message, Some("你好，我在。".to_owned()));
    }

    #[tokio::test]
    async fn agent_loop_still_requires_evidence_for_workspace_objectives() {
        let workspace = tempdir().expect("workspace");
        let provider = MockProvider::text_response("README looks fine.");
        let executor =
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

        let outcome = agent_loop
            .run(AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "read README.md".to_owned(),
                completion_criteria: vec!["file read evidence".to_owned()],
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["read_file".to_owned()],
            })
            .await
            .expect("loop runs");

        assert_eq!(outcome.loop_decision.action, LoopAction::Blocked);
        assert_eq!(outcome.verification.result, VerificationResult::Unknown);
    }

    #[tokio::test]
    async fn agent_loop_does_not_stop_success_when_tool_fails() {
        let workspace = tempdir().expect("workspace");
        let provider = MockProvider::tool_call("read_file", json!({"path": "missing.md"}));
        let executor =
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

        let outcome = agent_loop
            .run(AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "read missing file".to_owned(),
                completion_criteria: vec!["file read evidence".to_owned()],
                touched_code: false,
                contributors: Vec::new(),
                tools: vec!["read_file".to_owned()],
            })
            .await
            .expect("loop runs");

        assert_eq!(outcome.loop_decision.action, LoopAction::StopPartial);
        assert_eq!(outcome.verification.result, VerificationResult::Partial);
    }

    #[test]
    fn checkpoint_copies_changed_files_without_touching_git() {
        let workspace = tempdir().expect("workspace");
        let checkpoint_root = tempdir().expect("checkpoint");
        let source = workspace.path().join("src/lib.rs");
        fs::create_dir_all(source.parent().unwrap()).expect("parent");
        fs::write(&source, "pub fn value() -> u8 { 1 }").expect("source");
        let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());

        let checkpoint = manager
            .create_checkpoint(
                WorkspaceId::new(),
                TaskId::new(),
                TurnId::new(),
                &[PathBuf::from("src/lib.rs")],
                EventId::new(),
            )
            .expect("checkpoint");

        assert_eq!(checkpoint.changed_files, vec!["src/lib.rs"]);
        assert!(checkpoint_fingerprint(&checkpoint).starts_with("sha256:"));
        assert!(!workspace.path().join(".git").exists());
    }

    #[test]
    fn checkpoint_rejects_parent_directory_escape() {
        let workspace = tempdir().expect("workspace");
        let outside = tempdir().expect("outside");
        let outside_file = outside.path().join("outside.txt");
        fs::write(&outside_file, "secret").expect("outside file");
        let checkpoint_root = tempdir().expect("checkpoint");
        let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());

        let result = manager.create_checkpoint(
            WorkspaceId::new(),
            TaskId::new(),
            TurnId::new(),
            &[outside_file],
            EventId::new(),
        );

        assert!(matches!(result, Err(CheckpointError::OutsideWorkspace(_))));
    }
}
