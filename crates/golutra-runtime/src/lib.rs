use std::{
    collections::{HashMap, VecDeque},
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use golutra_context::{
    ContextBuilder, ContextContributor, ContextError, estimate_tokens, provider_request_from_plan,
    token_usage_record,
};
use golutra_core::{
    Actor, ApprovalDecision, ApprovalId, ApprovalRequest, ApprovalResolution, BudgetState,
    BusyPolicy, BusyPolicyDecision, CheckpointId, CheckpointType, CommandId, DecisionId, LaneId,
    LoopAction, LoopDecision, LoopDecisionId, PolicyDecision, PolicyId, RuntimeLane, SessionId,
    TaskId, TaskStatus, ToolCallId, ToolResultStatus, TurnId, VerificationCheck, VerificationId,
    VerificationRecord, VerificationResult, WorkspaceCheckpoint, WorkspaceId,
};
use golutra_governor::{
    GoalLedger, GovernorAction, GovernorObservation, GovernorPhase, RuntimeGovernor,
    RuntimeGovernorDecision,
};
use golutra_llm::{
    LlmProvider, ProviderError, ProviderFinishReason, ProviderMessage, ProviderRequest,
    ProviderResponse, ProviderRole,
};
use golutra_protocol::{RuntimeEvent, RuntimeEventSource, RuntimeEventType};
use golutra_tools::{
    BasicToolExecutor, FileBeforeImage, ToolError, ToolExecutionReport, ToolRequest,
};
use golutra_verify::{VerificationInput, VerificationRunner};
use ignore::gitignore::GitignoreBuilder;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

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
    #[error("checkpoint persistence failed: {0}")]
    Checkpoint(String),
    #[error("agent task was cancelled")]
    Cancelled,
    #[error("agent task no longer accepts queued turns")]
    PendingTurnQueueClosed,
    #[error("agent pending turn queue is full")]
    PendingTurnQueueFull,
}

#[derive(Debug, Error)]
pub enum CheckpointError {
    #[error("checkpoint io failed: {0}")]
    Io(String),
    #[error("changed file is outside workspace: {0}")]
    OutsideWorkspace(String),
    #[error("changed file is excluded from checkpoint: {0}")]
    Excluded(String),
    #[error("checkpoint manifest is invalid: {0}")]
    InvalidManifest(String),
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
    pub final_turn_id: TurnId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAgentTurn {
    pub command_id: CommandId,
    pub turn_id: TurnId,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct AgentExecutionHandle {
    cancellation: CancellationToken,
    pause: watch::Sender<bool>,
    pending_turns: Arc<PendingTurnQueue>,
    approvals: mpsc::Sender<ApprovalResolution>,
}

impl AgentExecutionHandle {
    pub fn cancel(&self) {
        self.pending_turns.close();
        self.cancellation.cancel();
    }

    pub fn pause(&self) {
        self.pause.send_replace(true);
    }

    pub fn resume(&self) {
        self.pause.send_replace(false);
    }

    pub async fn append_turn(&self, turn: PendingAgentTurn) -> Result<(), AgentLoopError> {
        self.pending_turns.push(turn)
    }

    pub async fn resolve_approval(
        &self,
        resolution: ApprovalResolution,
    ) -> Result<(), AgentLoopError> {
        self.approvals
            .send(resolution)
            .await
            .map_err(|_| AgentLoopError::Cancelled)
    }

    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

#[derive(Debug)]
pub struct AgentExecutionControl {
    cancellation: CancellationToken,
    pause: watch::Receiver<bool>,
    pending_turns: Arc<PendingTurnQueue>,
    approvals: mpsc::Receiver<ApprovalResolution>,
}

#[derive(Debug)]
struct PendingTurnQueue {
    capacity: usize,
    state: StdMutex<PendingTurnQueueState>,
}

#[derive(Debug, Default)]
struct PendingTurnQueueState {
    accepting: bool,
    turns: VecDeque<PendingAgentTurn>,
}

impl PendingTurnQueue {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            state: StdMutex::new(PendingTurnQueueState {
                accepting: true,
                turns: VecDeque::new(),
            }),
        }
    }

    fn push(&self, turn: PendingAgentTurn) -> Result<(), AgentLoopError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.accepting {
            return Err(AgentLoopError::PendingTurnQueueClosed);
        }
        if state.turns.len() >= self.capacity {
            return Err(AgentLoopError::PendingTurnQueueFull);
        }
        state.turns.push_back(turn);
        Ok(())
    }

    fn take_or_close(&self) -> Option<PendingAgentTurn> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match state.turns.pop_front() {
            Some(turn) => Some(turn),
            None => {
                state.accepting = false;
                None
            }
        }
    }

    fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .accepting = false;
    }
}

impl Drop for AgentExecutionControl {
    fn drop(&mut self) {
        self.pending_turns.close();
    }
}

#[async_trait]
pub trait BeforeSideEffectRecorder: std::fmt::Debug + Send + Sync {
    async fn persist_before_side_effect(
        &self,
        request: &ToolRequest,
        before_images: &[FileBeforeImage],
    ) -> Result<(), AgentLoopError>;
}

#[must_use]
pub fn agent_execution_channel(capacity: usize) -> (AgentExecutionHandle, AgentExecutionControl) {
    let cancellation = CancellationToken::new();
    let (pause_tx, pause_rx) = watch::channel(false);
    let pending_turns = Arc::new(PendingTurnQueue::new(capacity));
    let (approval_tx, approval_rx) = mpsc::channel(capacity.max(1));
    (
        AgentExecutionHandle {
            cancellation: cancellation.clone(),
            pause: pause_tx,
            pending_turns: pending_turns.clone(),
            approvals: approval_tx,
        },
        AgentExecutionControl {
            cancellation,
            pause: pause_rx,
            pending_turns,
            approvals: approval_rx,
        },
    )
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentLoopTraceEvent {
    ContextBuilt {
        contributors: Vec<String>,
        planned_input_tokens: u64,
    },
    ContextCompacted {
        original_input_tokens: u64,
        planned_input_tokens: u64,
        trimmed_contributors: Vec<String>,
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
        usage: golutra_core::ProviderUsage,
        raw_metadata: serde_json::Value,
    },
    TokenUsageRecorded(golutra_core::TokenUsageRecord),
    ToolStarted {
        tool_name: String,
    },
    ToolCompleted(ToolExecutionReport),
    PolicyEvaluated(golutra_core::PolicyEvaluation),
    ApprovalRequested(ApprovalRequest),
    ApprovalResolved(ApprovalResolution),
    RetryScheduled {
        attempt: u32,
        reason: String,
    },
    ProviderFallback {
        from_provider: String,
        to_provider: String,
        reason: String,
    },
    LoopGuardTriggered {
        trigger: golutra_core::LoopGuardTrigger,
        reason: String,
    },
    GovernorDecided(RuntimeGovernorDecision),
    PendingTurnStarted(PendingAgentTurn),
    AssistantMessage {
        turn_id: TurnId,
        content: String,
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
        before_images: &[FileBeforeImage],
        created_before_tool_call_id: ToolCallId,
    ) -> Result<WorkspaceCheckpoint, CheckpointError> {
        let checkpoint_id = CheckpointId::new();
        let checkpoint_dir = self.checkpoint_root.join(checkpoint_id.to_string());
        fs::create_dir_all(&self.checkpoint_root)
            .map_err(|error| CheckpointError::Io(error.to_string()))?;
        set_owner_only_checkpoint_dir(&self.checkpoint_root)?;
        fs::create_dir_all(&checkpoint_dir)
            .map_err(|error| CheckpointError::Io(error.to_string()))?;
        set_owner_only_checkpoint_dir(&checkpoint_dir)?;

        let mut entries = Vec::new();
        for before_image in before_images {
            let relative_path = self.relative_checkpoint_path(&before_image.path)?;
            if let Some(content) = &before_image.content {
                let target_path = checkpoint_dir.join("files").join(&relative_path);
                if let Some(parent) = target_path.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|error| CheckpointError::Io(error.to_string()))?;
                }
                write_checkpoint_file(&target_path, content)?;
                sync_checkpoint_ancestors(
                    target_path.parent().unwrap_or(&checkpoint_dir),
                    &checkpoint_dir,
                )?;
            }
            entries.push(CheckpointManifestEntry {
                path: relative_path.display().to_string(),
                existed: before_image.content.is_some(),
                checksum: before_image.content.as_deref().map(checksum_bytes),
                unix_mode: before_image.unix_mode,
            });
        }
        let manifest = CheckpointManifest { entries };
        let manifest_path = checkpoint_dir.join("manifest.json");
        write_checkpoint_file(
            &manifest_path,
            &serde_json::to_vec_pretty(&manifest)
                .map_err(|error| CheckpointError::InvalidManifest(error.to_string()))?,
        )?;
        sync_checkpoint_directory(&checkpoint_dir)?;
        sync_checkpoint_directory(&self.checkpoint_root)?;
        let changed_files = manifest
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect();

        Ok(WorkspaceCheckpoint {
            checkpoint_id,
            workspace_id,
            task_id,
            turn_id,
            checkpoint_type: CheckpointType::Snapshot,
            changed_files,
            artifact_refs: Vec::new(),
            created_before_tool_call_id,
            restore_hint: format!(
                "restore files using manifest {}",
                checkpoint_dir.join("manifest.json").display()
            ),
            retention_policy: "p0_keep_until_task_cleanup".to_owned(),
        })
    }

    pub fn restore_checkpoint(&self, checkpoint_id: CheckpointId) -> Result<(), CheckpointError> {
        let checkpoint_dir = self.checkpoint_root.join(checkpoint_id.to_string());
        let manifest_bytes = fs::read(checkpoint_dir.join("manifest.json"))
            .map_err(|error| CheckpointError::Io(error.to_string()))?;
        let manifest: CheckpointManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|error| CheckpointError::InvalidManifest(error.to_string()))?;
        let mut prepared = Vec::with_capacity(manifest.entries.len());
        for entry in manifest.entries {
            let declared_path = Path::new(&entry.path);
            if declared_path.as_os_str().is_empty()
                || declared_path
                    .components()
                    .any(|component| !matches!(component, Component::Normal(_)))
            {
                return Err(CheckpointError::InvalidManifest(format!(
                    "checkpoint path must be workspace-relative without traversal: {}",
                    entry.path
                )));
            }
            validate_workspace_file_mode(entry.unix_mode)?;
            let relative_path = self.relative_checkpoint_path(declared_path)?;
            let target = self.workspace_root.join(&relative_path);
            if entry.existed {
                let source = checkpoint_dir.join("files").join(&relative_path);
                let content =
                    fs::read(&source).map_err(|error| CheckpointError::Io(error.to_string()))?;
                let actual_checksum = checksum_bytes(&content);
                if entry.checksum.as_deref() != Some(actual_checksum.as_str()) {
                    return Err(CheckpointError::InvalidManifest(format!(
                        "checkpoint content checksum mismatch: {}",
                        entry.path
                    )));
                }
                prepared.push((target, Some(content), entry.unix_mode));
            } else {
                prepared.push((target, None, None));
            }
        }
        for (target, content, unix_mode) in prepared {
            if let Some(content) = content {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|error| CheckpointError::Io(error.to_string()))?;
                }
                write_workspace_restore_file(&target, &content, unix_mode)?;
            } else {
                match fs::symlink_metadata(&target) {
                    Ok(_) => {
                        fs::remove_file(&target)
                            .map_err(|error| CheckpointError::Io(error.to_string()))?;
                        if let Some(parent) = target.parent() {
                            sync_checkpoint_directory(parent)?;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(CheckpointError::Io(error.to_string())),
                }
            }
        }
        Ok(())
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
        let canonical_path = if path.exists() {
            path.canonicalize()
                .map_err(|error| CheckpointError::Io(error.to_string()))?
        } else {
            let parent = path.parent().ok_or_else(|| {
                CheckpointError::Io(format!("changed file has no parent: {}", path.display()))
            })?;
            let canonical_parent = parent
                .canonicalize()
                .map_err(|error| CheckpointError::Io(error.to_string()))?;
            let file_name = path.file_name().ok_or_else(|| {
                CheckpointError::Io(format!("changed file has no name: {}", path.display()))
            })?;
            canonical_parent.join(file_name)
        };
        let relative = canonical_path
            .strip_prefix(&canonical_workspace)
            .map_err(|_| CheckpointError::OutsideWorkspace(path.display().to_string()))?;

        if is_checkpoint_excluded(relative) {
            return Err(CheckpointError::Excluded(relative.display().to_string()));
        }
        if self.is_gitignored(relative)? {
            return Err(CheckpointError::Excluded(relative.display().to_string()));
        }
        Ok(relative.to_path_buf())
    }

    fn is_gitignored(&self, relative_path: &Path) -> Result<bool, CheckpointError> {
        let mut builder = GitignoreBuilder::new(&self.workspace_root);
        let mut directory = self.workspace_root.clone();
        add_gitignore_file(&mut builder, &directory)?;
        if let Some(parent) = relative_path.parent() {
            for component in parent.components() {
                directory.push(component.as_os_str());
                add_gitignore_file(&mut builder, &directory)?;
            }
        }
        let matcher = builder
            .build()
            .map_err(|error| CheckpointError::Io(error.to_string()))?;
        Ok(matcher
            .matched_path_or_any_parents(relative_path, false)
            .is_ignore())
    }
}

fn add_gitignore_file(
    builder: &mut GitignoreBuilder,
    directory: &Path,
) -> Result<(), CheckpointError> {
    let path = directory.join(".gitignore");
    if path.exists()
        && let Some(error) = builder.add(path)
    {
        return Err(CheckpointError::Io(error.to_string()));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CheckpointManifest {
    entries: Vec<CheckpointManifestEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CheckpointManifestEntry {
    path: String,
    existed: bool,
    checksum: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unix_mode: Option<u32>,
}

#[derive(Debug)]
pub struct AgentLoop<P> {
    provider: P,
    fallback_provider: Option<P>,
    context_builder: ContextBuilder,
    tool_executor: BasicToolExecutor,
    verifier: VerificationRunner,
    governor: RuntimeGovernor,
    before_side_effect_recorder: Option<Arc<dyn BeforeSideEffectRecorder>>,
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
            fallback_provider: None,
            context_builder,
            tool_executor,
            verifier: VerificationRunner,
            governor: RuntimeGovernor::default(),
            before_side_effect_recorder: None,
            max_iterations: 4,
        }
    }

    #[must_use]
    pub fn with_fallback(mut self, provider: P) -> Self {
        self.fallback_provider = Some(provider);
        self
    }

    #[must_use]
    pub fn with_governor(mut self, governor: RuntimeGovernor) -> Self {
        self.max_iterations = governor.limits().max_iterations;
        self.governor = governor;
        self
    }

    #[must_use]
    pub fn with_before_side_effect_recorder(
        mut self,
        recorder: Arc<dyn BeforeSideEffectRecorder>,
    ) -> Self {
        self.before_side_effect_recorder = Some(recorder);
        self
    }

    pub async fn run(&self, request: AgentTaskRequest) -> Result<AgentLoopOutcome, AgentLoopError> {
        let (_handle, control) = agent_execution_channel(1);
        self.run_with_control_and_trace(request, control, |_| {})
            .await
    }

    pub async fn run_with_trace<F>(
        &self,
        request: AgentTaskRequest,
        trace: F,
    ) -> Result<AgentLoopOutcome, AgentLoopError>
    where
        F: FnMut(AgentLoopTraceEvent),
    {
        let (_handle, control) = agent_execution_channel(1);
        self.run_with_control_and_trace(request, control, trace)
            .await
    }

    pub async fn run_with_control_and_trace<F>(
        &self,
        request: AgentTaskRequest,
        mut control: AgentExecutionControl,
        mut trace: F,
    ) -> Result<AgentLoopOutcome, AgentLoopError>
    where
        F: FnMut(AgentLoopTraceEvent),
    {
        let mut tool_reports = Vec::new();
        let mut last_assistant_message = None;
        let mut last_emitted_assistant_message = None;
        let mut current_turn_id = request.turn_id;
        let mut current_objective = request.objective.clone();
        let mut current_turn_touched_code = request.touched_code;
        let mut guard_reason = None;
        let mut repeated_failure_signature = None;
        let mut repeated_failure_count = 0_u32;
        let mut empty_response_count = 0_u32;
        let mut terminated = false;
        let started_at = Instant::now();
        let mut tool_call_count = 0_u32;
        let mut failed_tool_call_count = 0_u32;
        let mut governor_action = None;
        let mut goal_ledger = GoalLedger {
            task_id: request.task_id,
            original_objective: request.objective.clone(),
            success_criteria: request.completion_criteria.clone(),
            current_plan: request.completion_criteria.clone(),
            completed_steps: Vec::new(),
            open_risks: Vec::new(),
        };
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
        let provider_tools = request
            .tools
            .iter()
            .map(|tool_name| {
                self.tool_executor
                    .registry()
                    .contract(tool_name)
                    .cloned()
                    .ok_or_else(|| ToolError::UnknownTool(tool_name.clone()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let planned_tool_tokens = provider_tools
            .iter()
            .map(|contract| {
                serde_json::to_string(contract)
                    .map(|value| estimate_tokens(&value))
                    .unwrap_or_default()
            })
            .sum::<u64>();
        let base_plan = match self.context_builder.build(
            request.task_id,
            current_turn_id,
            request.contributors.clone(),
        ) {
            Ok(plan) => plan,
            Err(error) => return Ok(context_guard_outcome(&request, error, &mut trace)),
        };
        if !base_plan.trimmed_contributors.is_empty() {
            trace(AgentLoopTraceEvent::ContextCompacted {
                original_input_tokens: base_plan.original_planned_input_tokens,
                planned_input_tokens: base_plan.budget_snapshot.planned_input_tokens,
                trimmed_contributors: base_plan.trimmed_contributors.clone(),
            });
        }
        let mut messages = base_plan.messages.clone();

        'agent_loop: for iteration in 0..=self.max_iterations {
            control.wait_until_runnable().await?;
            let mut plan = base_plan.clone();
            plan.messages = messages.clone();
            plan.budget_snapshot.turn_id = current_turn_id;
            plan.budget_snapshot.planned_tool_tokens = planned_tool_tokens;
            plan.budget_snapshot.planned_input_tokens = messages
                .iter()
                .map(|message| {
                    estimate_tokens(&message.content)
                        + message
                            .tool_calls
                            .iter()
                            .map(|call| estimate_tokens(&call.arguments.to_string()))
                            .sum::<u64>()
                })
                .sum::<u64>()
                .saturating_add(planned_tool_tokens);
            if plan.budget_snapshot.planned_input_tokens > plan.budget_snapshot.budget_limit {
                let reason = ContextError::BudgetExceeded {
                    planned: plan.budget_snapshot.planned_input_tokens,
                    limit: plan.budget_snapshot.budget_limit,
                }
                .to_string();
                last_budget_state = BudgetState {
                    planned_input_tokens: Some(plan.budget_snapshot.planned_input_tokens),
                    actual_input_tokens: None,
                    output_tokens: None,
                    total_tokens: None,
                    estimated_cost: None,
                    budget_remaining: Some(0),
                    compact_recommended: true,
                    cost_risk: "blocked".to_owned(),
                };
                trace(AgentLoopTraceEvent::LoopGuardTriggered {
                    trigger: golutra_core::LoopGuardTrigger::ContextOverflow,
                    reason: reason.clone(),
                });
                guard_reason = Some(reason);
                governor_action = Some(GovernorAction::AskUser);
                terminated = true;
                break;
            }
            trace(AgentLoopTraceEvent::ContextBuilt {
                contributors: plan.contributors.clone(),
                planned_input_tokens: plan.budget_snapshot.planned_input_tokens,
            });
            let governance = self.governor.evaluate(
                &goal_ledger,
                &GovernorObservation {
                    phase: GovernorPhase::Provider,
                    iteration: iteration.saturating_add(1),
                    tool_calls: tool_call_count,
                    failed_tool_calls: failed_tool_call_count,
                    planned_input_tokens: plan.budget_snapshot.planned_input_tokens,
                    elapsed_ms: elapsed_millis(started_at),
                    latest_action: current_objective.clone(),
                },
            );
            let permits_execution = governance.permits_execution();
            if !permits_execution {
                guard_reason = Some(governance.reason.clone());
                governor_action = Some(governance.action);
            }
            trace(AgentLoopTraceEvent::GovernorDecided(governance));
            if !permits_execution {
                trace(AgentLoopTraceEvent::LoopGuardTriggered {
                    trigger: if iteration >= self.max_iterations {
                        golutra_core::LoopGuardTrigger::MaxIteration
                    } else {
                        golutra_core::LoopGuardTrigger::ContextOverflow
                    },
                    reason: guard_reason
                        .clone()
                        .unwrap_or_else(|| "runtime governor blocked execution".to_owned()),
                });
                terminated = true;
                break;
            }
            let provider_contract = self.provider.contract();
            let provider_request = provider_request_from_plan(
                &plan,
                request.task_id,
                current_turn_id,
                provider_contract.provider_id.clone(),
                provider_contract.model_id.clone(),
                provider_tools.clone(),
            );
            trace(AgentLoopTraceEvent::ProviderStarted {
                provider_id: provider_request.provider_id.clone(),
                model_id: provider_request.model_id.clone(),
            });
            let (provider_response, completed_request) = self
                .complete_with_retry(provider_request.clone(), &mut control, &mut trace)
                .await
                .map_err(|error| {
                    if error == ProviderError::Cancelled {
                        AgentLoopError::Cancelled
                    } else {
                        AgentLoopError::Provider(error)
                    }
                })?;
            if let Some(message) = provider_response
                .message
                .as_ref()
                .filter(|message| !message.content.trim().is_empty())
            {
                last_assistant_message = Some(message.content.trim().to_owned());
            }
            trace(AgentLoopTraceEvent::ProviderCompleted {
                provider_id: completed_request.provider_id.clone(),
                model_id: completed_request.model_id.clone(),
                finish_reason: provider_response.finish_reason,
                tool_call_count: provider_response.tool_calls.len(),
                usage: provider_response.usage.clone(),
                raw_metadata: provider_response.raw_metadata.clone(),
            });
            let usage_record = token_usage_record(
                &completed_request,
                provider_response.response_id,
                &plan.budget_snapshot,
                &provider_response.usage,
            );
            trace(AgentLoopTraceEvent::TokenUsageRecorded(
                usage_record.clone(),
            ));
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

            if let Some(content) = provider_response
                .message
                .as_ref()
                .map(|message| message.content.trim())
                .filter(|content| !content.is_empty())
            {
                let content = content.to_owned();
                trace(AgentLoopTraceEvent::AssistantMessage {
                    turn_id: current_turn_id,
                    content: content.clone(),
                });
                last_emitted_assistant_message = Some((current_turn_id, content));
            }

            if provider_response.tool_calls.is_empty() {
                if let Some(message) = provider_response.message {
                    messages.push(message);
                    empty_response_count = 0;
                } else {
                    empty_response_count = empty_response_count.saturating_add(1);
                    if empty_response_count < 2 {
                        trace(AgentLoopTraceEvent::RetryScheduled {
                            attempt: empty_response_count,
                            reason: "provider returned an empty response".to_owned(),
                        });
                        messages.push(ProviderMessage {
                            role: ProviderRole::User,
                            content: "Return a concrete response or a valid tool call.".to_owned(),
                            tool_call_id: None,
                            tool_name: None,
                            tool_calls: Vec::new(),
                        });
                        continue;
                    }
                    let reason = "provider returned empty responses repeatedly".to_owned();
                    trace(AgentLoopTraceEvent::LoopGuardTriggered {
                        trigger: golutra_core::LoopGuardTrigger::EmptyResponse,
                        reason: reason.clone(),
                    });
                    guard_reason = Some(reason);
                    terminated = true;
                    break;
                }

                if let Some(pending_turn) = control.pending_turns.take_or_close() {
                    current_turn_id = pending_turn.turn_id;
                    current_objective = pending_turn.content.clone();
                    current_turn_touched_code =
                        objective_requires_workspace_evidence(&current_objective);
                    last_assistant_message = None;
                    last_emitted_assistant_message = None;
                    tool_reports.clear();
                    repeated_failure_signature = None;
                    repeated_failure_count = 0;
                    goal_ledger.original_objective = current_objective.clone();
                    goal_ledger.current_plan = request.completion_criteria.clone();
                    goal_ledger.completed_steps.clear();
                    goal_ledger.open_risks.clear();
                    trace(AgentLoopTraceEvent::PendingTurnStarted(
                        pending_turn.clone(),
                    ));
                    messages.push(ProviderMessage {
                        role: ProviderRole::User,
                        content: pending_turn.content,
                        tool_call_id: None,
                        tool_name: None,
                        tool_calls: Vec::new(),
                    });
                    continue;
                }
                terminated = true;
                break;
            }

            messages.push(ProviderMessage {
                role: ProviderRole::Assistant,
                content: provider_response
                    .message
                    .as_ref()
                    .map(|message| message.content.clone())
                    .unwrap_or_default(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: provider_response.tool_calls.clone(),
            });
            for tool_call in provider_response.tool_calls {
                control.wait_until_runnable().await?;
                let tool_action = format!("{} {}", tool_call.tool_name, tool_call.arguments);
                let governance = self.governor.evaluate(
                    &goal_ledger,
                    &GovernorObservation {
                        phase: GovernorPhase::Tool,
                        iteration: iteration.saturating_add(1),
                        tool_calls: tool_call_count.saturating_add(1),
                        failed_tool_calls: failed_tool_call_count,
                        planned_input_tokens: plan.budget_snapshot.planned_input_tokens,
                        elapsed_ms: elapsed_millis(started_at),
                        latest_action: tool_action,
                    },
                );
                let permits_execution = governance.permits_execution();
                if !permits_execution {
                    guard_reason = Some(governance.reason.clone());
                    governor_action = Some(governance.action);
                }
                trace(AgentLoopTraceEvent::GovernorDecided(governance));
                if !permits_execution {
                    terminated = true;
                    break 'agent_loop;
                }
                tool_call_count = tool_call_count.saturating_add(1);
                trace(AgentLoopTraceEvent::ToolStarted {
                    tool_name: tool_call.tool_name.clone(),
                });
                let provider_tool_call_id = tool_call.tool_call_id.clone();
                let failure_signature = format!("{}:{}", tool_call.tool_name, tool_call.arguments);
                let tool_request = ToolRequest {
                    tool_call_id: golutra_core::ToolCallId::new(),
                    session_id: request.session_id,
                    turn_id: Some(current_turn_id),
                    tool_name: tool_call.tool_name,
                    arguments: tool_call.arguments,
                };
                let report = match self.tool_executor.evaluate(&tool_request) {
                    Ok(policy) => {
                        trace(AgentLoopTraceEvent::PolicyEvaluated(policy.clone()));
                        let approved = if policy.decision == PolicyDecision::Ask {
                            let approval = ApprovalRequest {
                                approval_id: ApprovalId::new(),
                                task_id: request.task_id,
                                turn_id: current_turn_id,
                                tool_call_id: tool_request.tool_call_id,
                                tool_name: tool_request.tool_name.clone(),
                                resource: policy.resource.clone(),
                                reason: policy.reason.clone(),
                            };
                            trace(AgentLoopTraceEvent::ApprovalRequested(approval.clone()));
                            let resolution =
                                control.wait_for_approval(approval.approval_id).await?;
                            let approved = resolution.decision == ApprovalDecision::Approved;
                            trace(AgentLoopTraceEvent::ApprovalResolved(resolution));
                            approved
                        } else {
                            false
                        };
                        control.wait_until_runnable().await?;
                        let may_execute = match policy.decision {
                            PolicyDecision::Allow => true,
                            PolicyDecision::Ask => approved,
                            PolicyDecision::Deny | PolicyDecision::Block => false,
                        };
                        let before_images = if may_execute {
                            self.tool_executor
                                .prepare_side_effect(&tool_request)
                                .await?
                        } else {
                            Vec::new()
                        };
                        if !before_images.is_empty()
                            && let Some(recorder) = &self.before_side_effect_recorder
                        {
                            recorder
                                .persist_before_side_effect(&tool_request, &before_images)
                                .await?;
                        }
                        control.wait_until_runnable().await?;
                        self.tool_executor
                            .execute_with_policy_and_before_images(
                                tool_request,
                                policy,
                                approved,
                                control.cancellation.clone(),
                                before_images,
                            )
                            .await?
                    }
                    Err(error) => {
                        let report = self
                            .tool_executor
                            .invalid_request_report(tool_request, error.to_string());
                        trace(AgentLoopTraceEvent::PolicyEvaluated(
                            report.policy_evaluation.clone(),
                        ));
                        report
                    }
                };
                trace(AgentLoopTraceEvent::ToolCompleted(report.clone()));
                if report.envelope.status == ToolResultStatus::Ok {
                    repeated_failure_signature = None;
                    repeated_failure_count = 0;
                } else if repeated_failure_signature.as_deref() == Some(failure_signature.as_str())
                {
                    failed_tool_call_count = failed_tool_call_count.saturating_add(1);
                    repeated_failure_count = repeated_failure_count.saturating_add(1);
                } else {
                    failed_tool_call_count = failed_tool_call_count.saturating_add(1);
                    repeated_failure_signature = Some(failure_signature);
                    repeated_failure_count = 1;
                }
                let result_governance = self.governor.evaluate(
                    &goal_ledger,
                    &GovernorObservation {
                        phase: GovernorPhase::ToolResult,
                        iteration: iteration.saturating_add(1),
                        tool_calls: tool_call_count,
                        failed_tool_calls: failed_tool_call_count,
                        planned_input_tokens: plan.budget_snapshot.planned_input_tokens,
                        elapsed_ms: elapsed_millis(started_at),
                        latest_action: report.envelope.summary.clone(),
                    },
                );
                let permits_continuation = result_governance.permits_execution();
                if !permits_continuation {
                    guard_reason = Some(result_governance.reason.clone());
                    governor_action = Some(result_governance.action);
                }
                trace(AgentLoopTraceEvent::GovernorDecided(result_governance));
                messages.push(ProviderMessage {
                    role: ProviderRole::Tool,
                    content: serde_json::to_string(&report.envelope)
                        .unwrap_or_else(|_| report.envelope.summary.clone()),
                    tool_call_id: Some(provider_tool_call_id),
                    tool_name: Some(report.envelope.tool_name.clone()),
                    tool_calls: Vec::new(),
                });
                tool_reports.push(report);
                if !permits_continuation {
                    terminated = true;
                    break 'agent_loop;
                }
            }
            if repeated_failure_count >= 2 {
                let reason = "the same deterministic tool call failed repeatedly".to_owned();
                trace(AgentLoopTraceEvent::LoopGuardTriggered {
                    trigger: golutra_core::LoopGuardTrigger::RepeatedToolFailure,
                    reason: reason.clone(),
                });
                guard_reason = Some(reason);
                terminated = true;
                break;
            }
        }

        if !terminated && guard_reason.is_none() {
            guard_reason = Some(format!(
                "agent loop reached max iteration {}",
                self.max_iterations
            ));
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
        let touched_code = current_turn_touched_code
            || tool_reports
                .iter()
                .any(|report| !report.changed_files.is_empty());
        let mut verification = if accepts_text_response_without_evidence(
            &current_objective,
            touched_code,
            last_assistant_message.as_deref(),
            &tool_reports,
        ) {
            text_response_verification(
                request.task_id,
                current_objective.clone(),
                request.completion_criteria.clone(),
            )
        } else {
            self.verifier.verify(VerificationInput {
                task_id: request.task_id,
                objective: current_objective.clone(),
                completion_criteria: request.completion_criteria.clone(),
                evidence_refs,
                command_checks,
                touched_code,
            })
        };
        let completion_governance = self.governor.evaluate(
            &goal_ledger,
            &GovernorObservation {
                phase: GovernorPhase::Completion,
                iteration: self.max_iterations.min(tool_call_count.saturating_add(1)),
                tool_calls: tool_call_count,
                failed_tool_calls: failed_tool_call_count,
                planned_input_tokens: last_budget_state.planned_input_tokens.unwrap_or_default(),
                elapsed_ms: elapsed_millis(started_at),
                latest_action: last_assistant_message
                    .clone()
                    .unwrap_or_else(|| current_objective.clone()),
            },
        );
        if !completion_governance.permits_execution() {
            guard_reason = Some(completion_governance.reason.clone());
            governor_action = Some(completion_governance.action);
        }
        trace(AgentLoopTraceEvent::GovernorDecided(completion_governance));
        if let Some(reason) = &guard_reason {
            if verification.result == VerificationResult::Pass {
                verification.result = VerificationResult::Partial;
            }
            verification.residual_risks.push(reason.clone());
        }
        let mut loop_decision = loop_decision_from_verification(
            request.task_id,
            current_turn_id,
            &verification,
            last_budget_state,
        );
        if let Some(action) = governor_action {
            loop_decision.action = match action {
                GovernorAction::AskUser => LoopAction::AskUser,
                GovernorAction::Block => LoopAction::Blocked,
                GovernorAction::Allow | GovernorAction::Warn => loop_decision.action,
            };
            loop_decision.reason = guard_reason
                .clone()
                .unwrap_or_else(|| "runtime governor stopped execution".to_owned());
            loop_decision.next_step =
                Some("user must revise the objective or runtime budget".to_owned());
        }

        let final_message =
            final_message_from_outcome(last_assistant_message, &tool_reports, &verification);
        if let Some(content) = final_message.as_ref().filter(|content| {
            last_emitted_assistant_message.as_ref() != Some(&(current_turn_id, (*content).clone()))
        }) {
            trace(AgentLoopTraceEvent::AssistantMessage {
                turn_id: current_turn_id,
                content: content.clone(),
            });
        }

        Ok(AgentLoopOutcome {
            final_message,
            verification,
            loop_decision,
            tool_reports,
            final_turn_id: current_turn_id,
        })
    }

    async fn complete_with_retry<F>(
        &self,
        request: ProviderRequest,
        control: &mut AgentExecutionControl,
        trace: &mut F,
    ) -> Result<(ProviderResponse, ProviderRequest), ProviderError>
    where
        F: FnMut(AgentLoopTraceEvent),
    {
        match self
            .complete_provider(&self.provider, request.clone(), control, trace)
            .await
        {
            Ok(response) => Ok((response, request)),
            Err(primary_error) => {
                let Some(fallback) = &self.fallback_provider else {
                    return Err(primary_error);
                };
                let from_provider = self.provider.contract().provider_id;
                let to_provider = fallback.contract().provider_id;
                trace(AgentLoopTraceEvent::ProviderFallback {
                    from_provider,
                    to_provider: to_provider.clone(),
                    reason: primary_error.to_string(),
                });
                let mut fallback_request = request;
                fallback_request.provider_id = to_provider;
                fallback_request.model_id = fallback.contract().model_id;
                trace(AgentLoopTraceEvent::ProviderStarted {
                    provider_id: fallback_request.provider_id.clone(),
                    model_id: fallback_request.model_id.clone(),
                });
                self.complete_provider(fallback, fallback_request.clone(), control, trace)
                    .await
                    .map(|response| (response, fallback_request))
            }
        }
    }

    async fn complete_provider<F>(
        &self,
        provider: &P,
        request: ProviderRequest,
        control: &mut AgentExecutionControl,
        trace: &mut F,
    ) -> Result<ProviderResponse, ProviderError>
    where
        F: FnMut(AgentLoopTraceEvent),
    {
        const MAX_ATTEMPTS: u32 = 3;
        for attempt in 1..=MAX_ATTEMPTS {
            control
                .wait_until_runnable()
                .await
                .map_err(|_| ProviderError::Cancelled)?;
            let result = tokio::select! {
                biased;
                _ = control.cancellation.cancelled() => Err(ProviderError::Cancelled),
                result = provider.complete(request.clone()) => result,
            };
            match result {
                Ok(response) => return Ok(response),
                Err(error) if provider_error_is_retryable(&error) && attempt < MAX_ATTEMPTS => {
                    trace(AgentLoopTraceEvent::RetryScheduled {
                        attempt,
                        reason: error.to_string(),
                    });
                    let delay = tokio::time::sleep(Duration::from_millis(100 * u64::from(attempt)));
                    tokio::pin!(delay);
                    tokio::select! {
                        _ = control.cancellation.cancelled() => return Err(ProviderError::Cancelled),
                        _ = &mut delay => {}
                    }
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("provider retry loop always returns")
    }
}

impl AgentExecutionControl {
    async fn wait_until_runnable(&mut self) -> Result<(), AgentLoopError> {
        loop {
            if self.cancellation.is_cancelled() {
                return Err(AgentLoopError::Cancelled);
            }
            if !*self.pause.borrow() {
                return Ok(());
            }
            tokio::select! {
                _ = self.cancellation.cancelled() => return Err(AgentLoopError::Cancelled),
                changed = self.pause.changed() => {
                    if changed.is_err() {
                        return Err(AgentLoopError::Cancelled);
                    }
                }
            }
        }
    }

    async fn wait_for_approval(
        &mut self,
        approval_id: ApprovalId,
    ) -> Result<ApprovalResolution, AgentLoopError> {
        loop {
            tokio::select! {
                _ = self.cancellation.cancelled() => return Err(AgentLoopError::Cancelled),
                resolution = self.approvals.recv() => {
                    let resolution = resolution.ok_or(AgentLoopError::Cancelled)?;
                    if resolution.approval_id == approval_id {
                        return Ok(resolution);
                    }
                }
            }
        }
    }
}

fn provider_error_is_retryable(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::Unavailable { .. }
            | ProviderError::RateLimited { .. }
            | ProviderError::Timeout { .. }
    )
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

        let (applied_policy, reason) = if lane.status == TaskStatus::Aborting {
            (
                BusyPolicy::Reject,
                "active task is aborting and no longer accepts input",
            )
        } else if is_active_controller {
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

    pub fn takeover(
        &mut self,
        session_id: SessionId,
        new_controller: Actor,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        let lane = self
            .lanes_by_session
            .get_mut(&session_id)
            .filter(|lane| is_active_status(lane.status))
            .ok_or(RuntimeLaneError::LaneNotFound)?;
        let previous_controller = std::mem::replace(&mut lane.active_controller, new_controller);
        let turn_id = lane.active_turn_id.unwrap_or_default();
        let mut event = lane_event(
            lane,
            turn_id,
            sequence_no,
            RuntimeEventType::ControllerChanged,
            "active runtime controller changed",
        );
        event.payload["previous_controller"] = json!(previous_controller);
        event.payload["active_controller"] = json!(lane.active_controller);
        Ok(RuntimeTransition {
            lane: lane.clone(),
            event,
        })
    }

    pub fn queue_turn(
        &mut self,
        session_id: SessionId,
        turn_id: TurnId,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        let lane = self
            .lanes_by_session
            .get_mut(&session_id)
            .ok_or(RuntimeLaneError::LaneNotFound)?;
        lane.pending_turns.push(turn_id);
        Ok(RuntimeTransition {
            lane: lane.clone(),
            event: lane_event(
                lane,
                turn_id,
                sequence_no,
                RuntimeEventType::TurnQueued,
                "user turn queued on active runtime lane",
            ),
        })
    }

    pub fn start_queued_turn(
        &mut self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> Result<(), RuntimeLaneError> {
        let lane = self
            .lanes_by_session
            .get_mut(&session_id)
            .ok_or(RuntimeLaneError::LaneNotFound)?;
        lane.pending_turns.retain(|pending| *pending != turn_id);
        lane.active_turn_id = Some(turn_id);
        Ok(())
    }

    pub fn abort(
        &mut self,
        session_id: SessionId,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        if !self
            .lanes_by_session
            .get(&session_id)
            .is_some_and(|lane| is_active_status(lane.status))
        {
            return Err(RuntimeLaneError::LaneNotFound);
        }
        self.set_status(
            session_id,
            TaskStatus::Aborting,
            sequence_no,
            RuntimeEventType::TaskAbortRequested,
        )
    }

    pub fn pause(
        &mut self,
        session_id: SessionId,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        if !self.lanes_by_session.get(&session_id).is_some_and(|lane| {
            matches!(
                lane.status,
                TaskStatus::Running | TaskStatus::WaitingApproval | TaskStatus::Pausing
            )
        }) {
            return Err(RuntimeLaneError::LaneNotFound);
        }
        self.set_status(
            session_id,
            TaskStatus::Paused,
            sequence_no,
            RuntimeEventType::TaskPaused,
        )
    }

    pub fn resume(
        &mut self,
        session_id: SessionId,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        if !self
            .lanes_by_session
            .get(&session_id)
            .is_some_and(|lane| matches!(lane.status, TaskStatus::Pausing | TaskStatus::Paused))
        {
            return Err(RuntimeLaneError::LaneNotFound);
        }
        self.set_status(
            session_id,
            TaskStatus::Running,
            sequence_no,
            RuntimeEventType::TaskResumed,
        )
    }

    pub fn finish_task(
        &mut self,
        session_id: SessionId,
        status: TaskStatus,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        if !self
            .lanes_by_session
            .get(&session_id)
            .is_some_and(|lane| is_active_status(lane.status))
        {
            return Err(RuntimeLaneError::LaneNotFound);
        }
        let event_type = if status == TaskStatus::Cancelled {
            RuntimeEventType::TaskAborted
        } else {
            RuntimeEventType::TaskCompleted
        };
        self.set_status(session_id, status, sequence_no, event_type)
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

fn context_guard_outcome<F>(
    request: &AgentTaskRequest,
    error: ContextError,
    trace: &mut F,
) -> AgentLoopOutcome
where
    F: FnMut(AgentLoopTraceEvent),
{
    let (planned, limit, action) = match error {
        ContextError::BudgetExceeded { planned, limit } => (planned, limit, LoopAction::Blocked),
        ContextError::UserActionRequired { planned, limit } => {
            (planned, limit, LoopAction::AskUser)
        }
    };
    let reason = format!("context budget exceeded: planned {planned} > limit {limit}");
    trace(AgentLoopTraceEvent::LoopGuardTriggered {
        trigger: golutra_core::LoopGuardTrigger::ContextOverflow,
        reason: reason.clone(),
    });
    let verification = VerificationRecord {
        verification_id: VerificationId::new(),
        task_id: request.task_id,
        objective: request.objective.clone(),
        completion_criteria: request.completion_criteria.clone(),
        checks: Vec::new(),
        evidence_refs: Vec::new(),
        result: VerificationResult::Unknown,
        policy_status: "context_guard_blocked".to_owned(),
        residual_risks: vec![reason.clone()],
    };
    let final_message = format!(
        "Cannot continue because the context budget is exhausted ({planned} > {limit}). Compact the conversation or reduce the request."
    );
    trace(AgentLoopTraceEvent::AssistantMessage {
        turn_id: request.turn_id,
        content: final_message.clone(),
    });
    AgentLoopOutcome {
        loop_decision: LoopDecision {
            decision_id: LoopDecisionId::new(),
            task_id: request.task_id,
            turn_id: request.turn_id,
            action,
            reason,
            evidence_refs: Vec::new(),
            verification_ref: Some(verification.verification_id),
            policy_ref: None,
            budget_state: BudgetState {
                planned_input_tokens: Some(planned),
                actual_input_tokens: None,
                output_tokens: None,
                total_tokens: None,
                estimated_cost: None,
                budget_remaining: Some(0),
                compact_recommended: true,
                cost_risk: "blocked".to_owned(),
            },
            tool_state: "not_started_context_guard".to_owned(),
            model_state: "not_started_context_guard".to_owned(),
            next_step: Some("compact context or reduce the request".to_owned()),
        },
        verification,
        tool_reports: Vec::new(),
        final_message: Some(final_message),
        final_turn_id: request.turn_id,
    }
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
    objective: &str,
    touched_code: bool,
    assistant_message: Option<&str>,
    tool_reports: &[ToolExecutionReport],
) -> bool {
    !touched_code
        && tool_reports.is_empty()
        && assistant_message.is_some_and(|message| !message.trim().is_empty())
        && !objective_requires_workspace_evidence(objective)
}

fn text_response_verification(
    task_id: TaskId,
    objective: String,
    completion_criteria: Vec<String>,
) -> VerificationRecord {
    VerificationRecord {
        verification_id: VerificationId::new(),
        task_id,
        objective,
        completion_criteria,
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
            | TaskStatus::Aborting
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
            "active_controller": lane.active_controller,
            "runtime_lane": lane,
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

fn checksum_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

fn is_checkpoint_excluded(relative_path: &Path) -> bool {
    let path_text = relative_path.to_string_lossy();
    relative_path.components().any(|component| {
        matches!(
            component,
            std::path::Component::Normal(value) if matches!(value.to_str(), Some(".git" | ".golutra"))
        )
    })
        || path_text.contains(".env")
        || path_text.contains(".ssh")
        || path_text.contains("id_rsa")
        || path_text.contains("id_ed25519")
}

fn write_checkpoint_file(path: &Path, bytes: &[u8]) -> Result<(), CheckpointError> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(|error| CheckpointError::Io(error.to_string()))?;
    set_owner_only_checkpoint_file(path)?;
    file.write_all(bytes)
        .map_err(|error| CheckpointError::Io(error.to_string()))?;
    file.sync_all()
        .map_err(|error| CheckpointError::Io(error.to_string()))
}

fn write_workspace_restore_file(
    path: &Path,
    bytes: &[u8],
    unix_mode: Option<u32>,
) -> Result<(), CheckpointError> {
    let parent = path.parent().ok_or_else(|| {
        CheckpointError::Io(format!("restore path has no parent: {}", path.display()))
    })?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| CheckpointError::Io(error.to_string()))?;
    temporary
        .write_all(bytes)
        .map_err(|error| CheckpointError::Io(error.to_string()))?;
    set_workspace_file_mode(temporary.path(), unix_mode)?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| CheckpointError::Io(error.to_string()))?;
    temporary
        .persist(path)
        .map_err(|error| CheckpointError::Io(error.error.to_string()))?;
    sync_checkpoint_directory(parent)
}

fn sync_checkpoint_ancestors(start: &Path, stop: &Path) -> Result<(), CheckpointError> {
    let mut directory = Some(start);
    while let Some(current) = directory {
        sync_checkpoint_directory(current)?;
        if current == stop {
            return Ok(());
        }
        directory = current.parent();
    }
    Err(CheckpointError::Io(format!(
        "checkpoint directory {} is not below {}",
        start.display(),
        stop.display()
    )))
}

#[cfg(unix)]
fn sync_checkpoint_directory(path: &Path) -> Result<(), CheckpointError> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| CheckpointError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn sync_checkpoint_directory(_path: &Path) -> Result<(), CheckpointError> {
    Ok(())
}

#[cfg(unix)]
fn set_workspace_file_mode(path: &Path, unix_mode: Option<u32>) -> Result<(), CheckpointError> {
    use std::os::unix::fs::PermissionsExt;

    validate_workspace_file_mode(unix_mode)?;
    if let Some(mode) = unix_mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|error| CheckpointError::Io(error.to_string()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_workspace_file_mode(_path: &Path, _unix_mode: Option<u32>) -> Result<(), CheckpointError> {
    Ok(())
}

fn validate_workspace_file_mode(unix_mode: Option<u32>) -> Result<(), CheckpointError> {
    if unix_mode.is_some_and(|mode| mode > 0o7777) {
        return Err(CheckpointError::InvalidManifest(format!(
            "checkpoint file mode is invalid: {:o}",
            unix_mode.unwrap_or_default()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_checkpoint_dir(path: &Path) -> Result<(), CheckpointError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| CheckpointError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_only_checkpoint_dir(_path: &Path) -> Result<(), CheckpointError> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_checkpoint_file(path: &Path) -> Result<(), CheckpointError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| CheckpointError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_only_checkpoint_file(_path: &Path) -> Result<(), CheckpointError> {
    Ok(())
}

fn elapsed_millis(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use golutra_context::{
        ContextBudgetPolicy, ContextBuilder, ContextContributor, estimate_tokens,
    };
    use golutra_core::{ActorKind, BudgetOverflowAction};
    use golutra_governor::GovernorLimits;
    use golutra_llm::MockProvider;
    use golutra_policy::WorkspacePolicy;
    use golutra_tools::BasicToolExecutor;
    use tempfile::tempdir;

    use super::*;

    #[derive(Debug, Clone)]
    enum FallbackTestProvider {
        Failing(Box<golutra_core::ProviderContract>),
        Success(Box<MockProvider>),
    }

    #[async_trait]
    impl LlmProvider for FallbackTestProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            match self {
                Self::Failing(_) => Err(ProviderError::Failed {
                    message: "primary failed".to_owned(),
                }),
                Self::Success(provider) => provider.complete(request).await,
            }
        }

        fn contract(&self) -> golutra_core::ProviderContract {
            match self {
                Self::Failing(contract) => contract.as_ref().clone(),
                Self::Success(provider) => provider.contract(),
            }
        }
    }

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

    #[tokio::test]
    async fn pending_turn_queue_closes_atomically_when_the_loop_becomes_idle() {
        let (handle, control) = agent_execution_channel(2);
        let first = PendingAgentTurn {
            command_id: CommandId::new(),
            turn_id: TurnId::new(),
            content: "first queued turn".to_owned(),
        };

        handle
            .append_turn(first.clone())
            .await
            .expect("first turn queues");
        assert_eq!(control.pending_turns.take_or_close(), Some(first));
        assert_eq!(control.pending_turns.take_or_close(), None);
        assert!(matches!(
            handle
                .append_turn(PendingAgentTurn {
                    command_id: CommandId::new(),
                    turn_id: TurnId::new(),
                    content: "late turn".to_owned(),
                })
                .await,
            Err(AgentLoopError::PendingTurnQueueClosed)
        ));
    }

    #[test]
    fn completed_lane_allows_next_task_in_same_session() {
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
        manager
            .finish_task(session_id, TaskStatus::Completed, 2)
            .expect("first task finishes");
        let next = manager
            .start_task(
                WorkspaceId::new(),
                session_id,
                TaskId::new(),
                TurnId::new(),
                actor,
                3,
            )
            .expect("next task starts");

        assert_eq!(next.lane.status, TaskStatus::Running);
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
    fn takeover_transfers_the_active_controller_and_records_both_actors() {
        let mut manager = RuntimeLaneManager::new();
        let session_id = SessionId::new();
        let original = actor("original");
        let replacement = actor("replacement");
        manager
            .start_task(
                WorkspaceId::new(),
                session_id,
                TaskId::new(),
                TurnId::new(),
                original.clone(),
                1,
            )
            .expect("task");

        let transition = manager
            .takeover(session_id, replacement.clone(), 2)
            .expect("takeover");

        assert_eq!(transition.lane.active_controller, replacement);
        assert_eq!(
            transition.event.event_type,
            RuntimeEventType::ControllerChanged
        );
        assert_eq!(
            transition.event.payload["previous_controller"],
            json!(original)
        );
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
        assert_eq!(
            transition.event.event_type,
            RuntimeEventType::TaskAbortRequested
        );
        assert!(is_active_status(TaskStatus::Aborting));
    }

    #[test]
    fn terminal_lane_rejects_control_transitions() {
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
        manager
            .finish_task(session_id, TaskStatus::Completed, 2)
            .expect("task finishes");

        assert_eq!(
            manager.abort(session_id, 3),
            Err(RuntimeLaneError::LaneNotFound)
        );
        assert_eq!(
            manager.pause(session_id, 4),
            Err(RuntimeLaneError::LaneNotFound)
        );
        assert_eq!(
            manager.resume(session_id, 5),
            Err(RuntimeLaneError::LaneNotFound)
        );
        assert_eq!(
            manager.lane(session_id).map(|lane| lane.status),
            Some(TaskStatus::Completed)
        );
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
    async fn fallback_completion_and_usage_are_attributed_to_the_actual_provider() {
        let workspace = tempdir().expect("workspace");
        let mut primary_contract = MockProvider::text_response("unused").contract();
        primary_contract.provider_id = "primary".to_owned();
        primary_contract.model_id = "primary-model".to_owned();
        let provider = FallbackTestProvider::Failing(Box::new(primary_contract));
        let fallback =
            FallbackTestProvider::Success(Box::new(MockProvider::text_response("fallback")));
        let executor =
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let agent_loop =
            AgentLoop::new(provider, ContextBuilder::default(), executor).with_fallback(fallback);
        let mut trace = Vec::new();

        let outcome = agent_loop
            .run_with_trace(
                AgentTaskRequest {
                    session_id: SessionId::new(),
                    task_id: TaskId::new(),
                    turn_id: TurnId::new(),
                    objective: "hello".to_owned(),
                    completion_criteria: vec!["assistant response".to_owned()],
                    touched_code: false,
                    contributors: Vec::new(),
                    tools: Vec::new(),
                },
                |event| trace.push(event),
            )
            .await
            .expect("fallback outcome");

        assert_eq!(outcome.final_message.as_deref(), Some("fallback"));
        assert!(trace.iter().any(|event| matches!(
            event,
            AgentLoopTraceEvent::ProviderStarted { provider_id, model_id }
                if provider_id == "mock" && model_id == "mock-model"
        )));
        assert!(trace.iter().any(|event| matches!(
            event,
            AgentLoopTraceEvent::ProviderCompleted { provider_id, model_id, .. }
                if provider_id == "mock" && model_id == "mock-model"
        )));
        assert!(trace.iter().any(|event| matches!(
            event,
            AgentLoopTraceEvent::TokenUsageRecorded(record)
                if record.provider_id == "mock" && record.model_id == "mock-model"
        )));
    }

    #[tokio::test]
    async fn agent_loop_governor_blocks_before_iteration_budget_is_exceeded() {
        let workspace = tempdir().expect("workspace");
        let executor =
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let governor = RuntimeGovernor::new(GovernorLimits {
            max_iterations: 0,
            ..GovernorLimits::default()
        });
        let agent_loop = AgentLoop::new(
            MockProvider::text_response("should not run"),
            ContextBuilder::default(),
            executor,
        )
        .with_governor(governor);
        let mut trace = Vec::new();

        let outcome = agent_loop
            .run_with_trace(
                AgentTaskRequest {
                    session_id: SessionId::new(),
                    task_id: TaskId::new(),
                    turn_id: TurnId::new(),
                    objective: "inspect runtime".to_owned(),
                    completion_criteria: vec!["runtime inspected".to_owned()],
                    touched_code: false,
                    contributors: Vec::new(),
                    tools: Vec::new(),
                },
                |event| trace.push(event),
            )
            .await
            .expect("governed outcome");

        assert_eq!(outcome.loop_decision.action, LoopAction::Blocked);
        assert!(trace.iter().any(|event| matches!(
            event,
            AgentLoopTraceEvent::GovernorDecided(decision)
                if decision.action == GovernorAction::Block
        )));
    }

    #[tokio::test]
    async fn initial_context_overflow_returns_a_structured_blocked_outcome() {
        let workspace = tempdir().expect("workspace");
        let executor =
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let context_builder = ContextBuilder::new(ContextBudgetPolicy {
            context_window: 64,
            max_output: 16,
            budget_limit: 8,
            action_if_exceeded: BudgetOverflowAction::Block,
        });
        let agent_loop = AgentLoop::new(
            MockProvider::text_response("provider must not run"),
            context_builder,
            executor,
        );
        let mut trace = Vec::new();

        let outcome = agent_loop
            .run_with_trace(
                AgentTaskRequest {
                    session_id: SessionId::new(),
                    task_id: TaskId::new(),
                    turn_id: TurnId::new(),
                    objective: "inspect runtime".to_owned(),
                    completion_criteria: vec!["runtime inspected".to_owned()],
                    touched_code: false,
                    contributors: vec![ContextContributor {
                        name: "objective".to_owned(),
                        role: ProviderRole::User,
                        content: "large context ".repeat(20),
                        token_budget_hint: 0,
                    }],
                    tools: Vec::new(),
                },
                |event| trace.push(event),
            )
            .await
            .expect("context guard outcome");

        assert_eq!(outcome.loop_decision.action, LoopAction::Blocked);
        assert_eq!(outcome.verification.result, VerificationResult::Unknown);
        assert!(outcome.loop_decision.budget_state.compact_recommended);
        assert!(trace.iter().any(|event| matches!(
            event,
            AgentLoopTraceEvent::LoopGuardTriggered {
                trigger: golutra_core::LoopGuardTrigger::ContextOverflow,
                ..
            }
        )));
        assert!(
            !trace
                .iter()
                .any(|event| matches!(event, AgentLoopTraceEvent::ProviderStarted { .. }))
        );
    }

    #[tokio::test]
    async fn accumulated_tool_messages_return_an_ask_user_context_outcome() {
        let workspace = tempdir().expect("workspace");
        fs::write(workspace.path().join("large.txt"), "x".repeat(4_096)).expect("fixture");
        let executor =
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let contributor = ContextContributor {
            name: "objective".to_owned(),
            role: ProviderRole::User,
            content: "read large.txt".to_owned(),
            token_budget_hint: 0,
        };
        let tool_tokens = executor
            .registry()
            .contract("read_file")
            .and_then(|contract| serde_json::to_string(contract).ok())
            .map(|contract| estimate_tokens(&contract))
            .expect("read_file contract");
        let initial_tokens = estimate_tokens(&contributor.content).saturating_add(tool_tokens);
        let context_builder = ContextBuilder::new(ContextBudgetPolicy {
            context_window: initial_tokens.saturating_add(1_024),
            max_output: 64,
            budget_limit: initial_tokens.saturating_add(8),
            action_if_exceeded: BudgetOverflowAction::Trim,
        });
        let agent_loop = AgentLoop::new(
            MockProvider::tool_call("read_file", json!({"path": "large.txt"})),
            context_builder,
            executor,
        );
        let mut trace = Vec::new();

        let outcome = agent_loop
            .run_with_trace(
                AgentTaskRequest {
                    session_id: SessionId::new(),
                    task_id: TaskId::new(),
                    turn_id: TurnId::new(),
                    objective: "read large.txt".to_owned(),
                    completion_criteria: vec!["file read".to_owned()],
                    touched_code: false,
                    contributors: vec![contributor],
                    tools: vec!["read_file".to_owned()],
                },
                |event| trace.push(event),
            )
            .await
            .expect("context guard outcome");

        assert_eq!(outcome.tool_reports.len(), 1);
        assert_eq!(outcome.loop_decision.action, LoopAction::AskUser);
        assert!(outcome.loop_decision.budget_state.compact_recommended);
        assert_eq!(
            trace
                .iter()
                .filter(|event| matches!(event, AgentLoopTraceEvent::ProviderStarted { .. }))
                .count(),
            1
        );
        assert!(trace.iter().any(|event| matches!(
            event,
            AgentLoopTraceEvent::LoopGuardTriggered {
                trigger: golutra_core::LoopGuardTrigger::ContextOverflow,
                ..
            }
        )));
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
    async fn agent_loop_returns_invalid_tool_calls_to_the_provider_as_tool_results() {
        let workspace = tempdir().expect("workspace");
        let provider = MockProvider::tool_call("missing_tool", json!({"bad": true}));
        let executor =
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);

        let outcome = agent_loop
            .run(AgentTaskRequest {
                session_id: SessionId::new(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                objective: "try a tool".to_owned(),
                completion_criteria: vec!["tool result returned".to_owned()],
                touched_code: false,
                contributors: Vec::new(),
                tools: Vec::new(),
            })
            .await
            .expect("invalid tool call becomes a report");

        assert_eq!(outcome.tool_reports.len(), 1);
        assert_eq!(
            outcome.tool_reports[0].envelope.status,
            ToolResultStatus::Error
        );
        assert_eq!(
            outcome.tool_reports[0].envelope.summary,
            "tool request is invalid"
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
    async fn queued_plain_turn_does_not_inherit_the_previous_turn_workspace_requirement() {
        let workspace = tempdir().expect("workspace");
        let executor =
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let agent_loop = AgentLoop::new(
            MockProvider::text_response("plain response"),
            ContextBuilder::default(),
            executor,
        );
        let (handle, control) = agent_execution_channel(2);
        let queued_turn_id = TurnId::new();
        handle
            .append_turn(PendingAgentTurn {
                command_id: CommandId::new(),
                turn_id: queued_turn_id,
                content: "hello".to_owned(),
            })
            .await
            .expect("queued turn");

        let outcome = agent_loop
            .run_with_control_and_trace(
                AgentTaskRequest {
                    session_id: SessionId::new(),
                    task_id: TaskId::new(),
                    turn_id: TurnId::new(),
                    objective: "write a file".to_owned(),
                    completion_criteria: vec!["assistant response".to_owned()],
                    touched_code: true,
                    contributors: Vec::new(),
                    tools: Vec::new(),
                },
                control,
                |_| {},
            )
            .await
            .expect("queued turn outcome");

        assert_eq!(outcome.final_turn_id, queued_turn_id);
        assert_eq!(outcome.verification.result, VerificationResult::Pass);
        assert_eq!(outcome.final_message.as_deref(), Some("plain response"));
        assert!(outcome.tool_reports.is_empty());
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

    #[tokio::test]
    async fn agent_loop_waits_for_approval_before_process_execution() {
        let workspace = tempdir().expect("workspace");
        let provider = MockProvider::tool_call("shell", json!({"command": "echo approved"}));
        let executor =
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
        let (handle, control) = agent_execution_channel(4);
        let (trace_tx, mut trace_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            agent_loop
                .run_with_control_and_trace(
                    AgentTaskRequest {
                        session_id: SessionId::new(),
                        task_id: TaskId::new(),
                        turn_id: TurnId::new(),
                        objective: "run approved command".to_owned(),
                        completion_criteria: vec!["command evidence".to_owned()],
                        touched_code: false,
                        contributors: Vec::new(),
                        tools: vec!["shell".to_owned()],
                    },
                    control,
                    move |event| {
                        let _ = trace_tx.send(event);
                    },
                )
                .await
        });
        let approval = loop {
            let event = trace_rx.recv().await.expect("approval trace");
            if let AgentLoopTraceEvent::ApprovalRequested(approval) = event {
                break approval;
            }
        };

        assert!(!task.is_finished());
        handle
            .resolve_approval(ApprovalResolution {
                approval_id: approval.approval_id,
                decision: ApprovalDecision::Approved,
                reason: "approved by test".to_owned(),
            })
            .await
            .expect("approval resolves");
        let outcome = task.await.expect("task joins").expect("loop completes");

        assert_eq!(outcome.tool_reports.len(), 1);
        assert_eq!(
            outcome.tool_reports[0].envelope.status,
            ToolResultStatus::Ok
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn paused_approval_does_not_execute_tool_until_resume() {
        let workspace = tempdir().expect("workspace");
        let output = workspace.path().join("paused.txt");
        let provider = MockProvider::tool_call("shell", json!({"command": "touch paused.txt"}));
        let executor =
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).expect("policy"));
        let agent_loop = AgentLoop::new(provider, ContextBuilder::default(), executor);
        let (handle, control) = agent_execution_channel(4);
        let (trace_tx, mut trace_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            agent_loop
                .run_with_control_and_trace(
                    AgentTaskRequest {
                        session_id: SessionId::new(),
                        task_id: TaskId::new(),
                        turn_id: TurnId::new(),
                        objective: "run command after resume".to_owned(),
                        completion_criteria: vec!["command evidence".to_owned()],
                        touched_code: false,
                        contributors: Vec::new(),
                        tools: vec!["shell".to_owned()],
                    },
                    control,
                    move |event| {
                        let _ = trace_tx.send(event);
                    },
                )
                .await
        });
        let approval = loop {
            let event = trace_rx.recv().await.expect("approval trace");
            if let AgentLoopTraceEvent::ApprovalRequested(approval) = event {
                break approval;
            }
        };

        handle.pause();
        handle
            .resolve_approval(ApprovalResolution {
                approval_id: approval.approval_id,
                decision: ApprovalDecision::Approved,
                reason: "approved while paused".to_owned(),
            })
            .await
            .expect("approval resolves");
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(!output.exists());
        assert!(!task.is_finished());
        handle.resume();
        let outcome = task.await.expect("task joins").expect("loop completes");
        assert!(output.exists());
        assert_eq!(
            outcome.tool_reports[0].envelope.status,
            ToolResultStatus::Ok
        );
    }

    #[test]
    fn checkpoint_restores_file_before_image_without_touching_git() {
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
                &[FileBeforeImage {
                    path: PathBuf::from("src/lib.rs"),
                    content: Some(b"pub fn value() -> u8 { 1 }".to_vec()),
                    unix_mode: Some(0o755),
                }],
                ToolCallId::new(),
            )
            .expect("checkpoint");

        fs::write(&source, "pub fn value() -> u8 { 2 }").expect("updated source");
        manager
            .restore_checkpoint(checkpoint.checkpoint_id)
            .expect("checkpoint restores");

        assert_eq!(checkpoint.changed_files, vec!["src/lib.rs"]);
        assert!(checkpoint_fingerprint(&checkpoint).starts_with("sha256:"));
        assert_eq!(
            fs::read_to_string(&source).expect("restored source"),
            "pub fn value() -> u8 { 1 }"
        );
        assert!(!workspace.path().join(".git").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let checkpoint_dir = checkpoint_root
                .path()
                .join(checkpoint.checkpoint_id.to_string());
            let mode = |path: &Path| {
                fs::metadata(path)
                    .expect("checkpoint metadata")
                    .permissions()
                    .mode()
                    & 0o777
            };
            assert_eq!(mode(checkpoint_root.path()), 0o700);
            assert_eq!(mode(&checkpoint_dir), 0o700);
            assert_eq!(mode(&checkpoint_dir.join("manifest.json")), 0o600);
            assert_eq!(mode(&checkpoint_dir.join("files/src/lib.rs")), 0o600);
            assert_eq!(mode(&source), 0o755);
        }
    }

    #[test]
    fn checkpoint_restore_removes_file_created_by_task() {
        let workspace = tempdir().expect("workspace");
        let checkpoint_root = tempdir().expect("checkpoint");
        let source = workspace.path().join("created.txt");
        let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());

        let checkpoint = manager
            .create_checkpoint(
                WorkspaceId::new(),
                TaskId::new(),
                TurnId::new(),
                &[FileBeforeImage {
                    path: PathBuf::from("created.txt"),
                    content: None,
                    unix_mode: None,
                }],
                ToolCallId::new(),
            )
            .expect("checkpoint");

        fs::write(&source, "created by task").expect("created source");
        manager
            .restore_checkpoint(checkpoint.checkpoint_id)
            .expect("checkpoint restores");

        assert!(!source.exists());
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
            &[FileBeforeImage {
                path: outside_file,
                content: Some(b"secret".to_vec()),
                unix_mode: None,
            }],
            ToolCallId::new(),
        );

        assert!(matches!(result, Err(CheckpointError::OutsideWorkspace(_))));
    }

    #[test]
    fn checkpoint_restore_rejects_traversal_in_a_tampered_manifest() {
        let root = tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let checkpoint_root = root.path().join("checkpoints");
        fs::create_dir(&workspace).expect("workspace");
        let outside = root.path().join("outside.txt");
        fs::write(&outside, "keep").expect("outside file");
        let manager = WorkspaceCheckpointManager::new(&workspace, &checkpoint_root);
        let checkpoint = manager
            .create_checkpoint(
                WorkspaceId::new(),
                TaskId::new(),
                TurnId::new(),
                &[FileBeforeImage {
                    path: PathBuf::from("created.txt"),
                    content: None,
                    unix_mode: None,
                }],
                ToolCallId::new(),
            )
            .expect("checkpoint");
        let manifest = checkpoint_root
            .join(checkpoint.checkpoint_id.to_string())
            .join("manifest.json");
        fs::write(
            manifest,
            serde_json::to_vec(&json!({
                "entries": [{
                    "path": "../outside.txt",
                    "existed": false,
                    "checksum": null
                }]
            }))
            .expect("manifest"),
        )
        .expect("tamper manifest");

        let error = manager
            .restore_checkpoint(checkpoint.checkpoint_id)
            .expect_err("traversal must be rejected");

        assert!(matches!(error, CheckpointError::InvalidManifest(_)));
        assert_eq!(
            fs::read_to_string(outside).expect("outside remains"),
            "keep"
        );
    }

    #[test]
    fn checkpoint_validates_every_entry_before_restoring_any_file() {
        let workspace = tempdir().expect("workspace");
        let checkpoint_root = tempdir().expect("checkpoint");
        let first = workspace.path().join("first.txt");
        let second = workspace.path().join("second.txt");
        fs::write(&first, "first before").expect("first before");
        fs::write(&second, "second before").expect("second before");
        let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());
        let checkpoint = manager
            .create_checkpoint(
                WorkspaceId::new(),
                TaskId::new(),
                TurnId::new(),
                &[
                    FileBeforeImage {
                        path: PathBuf::from("first.txt"),
                        content: Some(b"first before".to_vec()),
                        unix_mode: None,
                    },
                    FileBeforeImage {
                        path: PathBuf::from("second.txt"),
                        content: Some(b"second before".to_vec()),
                        unix_mode: None,
                    },
                ],
                ToolCallId::new(),
            )
            .expect("checkpoint");
        fs::write(&first, "first after").expect("first after");
        fs::write(&second, "second after").expect("second after");
        let manifest = checkpoint_root
            .path()
            .join(checkpoint.checkpoint_id.to_string())
            .join("manifest.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest).expect("manifest")).expect("manifest JSON");
        value["entries"][1]["checksum"] = json!("sha256:tampered");
        fs::write(
            &manifest,
            serde_json::to_vec(&value).expect("manifest JSON"),
        )
        .expect("tamper manifest");

        assert!(matches!(
            manager.restore_checkpoint(checkpoint.checkpoint_id),
            Err(CheckpointError::InvalidManifest(_))
        ));
        assert_eq!(fs::read_to_string(first).unwrap(), "first after");
        assert_eq!(fs::read_to_string(second).unwrap(), "second after");
    }

    #[test]
    fn checkpoint_rejects_gitignored_before_images() {
        let workspace = tempdir().expect("workspace");
        let checkpoint_root = tempdir().expect("checkpoint");
        fs::write(workspace.path().join(".gitignore"), "ignored/\n*.secret\n").expect("gitignore");
        fs::create_dir(workspace.path().join("ignored")).expect("ignored directory");
        let manager = WorkspaceCheckpointManager::new(workspace.path(), checkpoint_root.path());

        for path in ["ignored/new.txt", "token.secret"] {
            let result = manager.create_checkpoint(
                WorkspaceId::new(),
                TaskId::new(),
                TurnId::new(),
                &[FileBeforeImage {
                    path: workspace.path().join(path),
                    content: None,
                    unix_mode: None,
                }],
                ToolCallId::new(),
            );

            assert!(
                matches!(result, Err(CheckpointError::Excluded(_))),
                "{path}"
            );
        }
    }
}
