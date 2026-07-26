use golutra_core::{
    ArtifactRecord, BusyPolicyDecision, ContextSnapshot, EvidenceRecord, LoopDecision, PostTaskJob,
    RuntimeLane, SessionId, TaskId, TaskStatus, ToolResultEnvelope, VerificationRecord,
};
use golutra_eval::{
    AutomationCandidate, CausalComparison, DiagnosticSlice, EvaluationResult,
    ExternalEvaluationRecord, FailureDiagnosis, FailureEpisode, FrozenCandidatePatch,
    ImprovementCandidate, PostTaskReview, PromotionDecision, RegressionResult, ReplayCapsule,
    ReplayExecution,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::RuntimeEvent;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StateProjection {
    pub session_id: SessionId,
    pub active_task_id: Option<TaskId>,
    pub task_status: TaskStatus,
    pub runtime_lane: Option<RuntimeLane>,
    pub last_sequence_no: u64,
    pub visible_steps: Vec<VisibleStep>,
    pub pending_approval: Option<String>,
    pub final_message: Option<String>,
    pub last_loop_decision: Option<LoopDecision>,
    pub last_verification: Option<VerificationRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VisibleStep {
    pub label: String,
    pub status: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UserProjection {
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub status: TaskStatus,
    pub visible_steps: Vec<VisibleStep>,
    pub pending_approval: Option<String>,
    pub final_message: Option<String>,
    pub residual_risks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DebugProjection {
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub events: Vec<RuntimeEvent>,
    pub event_window: DebugEventWindow,
    pub busy_policy_decisions: Vec<BusyPolicyDecision>,
    pub tool_results: Vec<ToolResultEnvelope>,
    pub artifacts: Vec<ArtifactRecord>,
    pub evidence: Vec<EvidenceRecord>,
    pub verification: Option<VerificationRecord>,
    pub loop_decisions: Vec<LoopDecision>,
    #[serde(default)]
    pub post_task_jobs: Vec<PostTaskJob>,
    #[serde(default)]
    pub failure_diagnosis: Option<FailureDiagnosis>,
    #[serde(default)]
    pub failure_episodes: Vec<FailureEpisode>,
    #[serde(default)]
    pub diagnostic_slice: Option<DiagnosticSlice>,
    #[serde(default)]
    pub replay_execution: Option<ReplayExecution>,
    #[serde(default)]
    pub external_evaluations: Vec<ExternalEvaluationRecord>,
    #[serde(default)]
    pub causal_comparisons: Vec<CausalComparison>,
    #[serde(default)]
    pub trace_complete: bool,
    #[serde(default)]
    pub missing_sections: Vec<String>,
    #[serde(default)]
    pub retention_losses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DebugEventWindow {
    pub start_cursor: Option<u64>,
    pub end_cursor: Option<u64>,
    pub has_more_before: bool,
    pub limit: u32,
}

/// 一个任务实际发送给 provider 的模型输入审计投影。
///
/// 这是对 `ModelInputEnvelope` 的脱敏、可查询读模型，不是 provider request 本身，也不会
/// 因为被持久化或被开发者读取而自动进入下一轮模型上下文。provider 原始请求仍受 artifact
/// 权限控制。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ContextProjection {
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub snapshots: Vec<ContextSnapshot>,
    pub latest: Option<ContextSnapshot>,
    pub complete: bool,
    pub integrity_warnings: Vec<String>,
}

/// 一个任务完成后治理生命周期的类型化读模型。
///
/// 开发工具无需解析事件文案即可区分 review、candidate、regression 和 promotion。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EvaluationProjection {
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub reviews: Vec<PostTaskReview>,
    pub results: Vec<EvaluationResult>,
    pub improvement_candidates: Vec<ImprovementCandidate>,
    #[serde(default)]
    pub frozen_candidate_patches: Vec<FrozenCandidatePatch>,
    pub automation_candidates: Vec<AutomationCandidate>,
    pub regressions: Vec<RegressionResult>,
    pub promotion_decisions: Vec<PromotionDecision>,
    #[serde(default)]
    pub failure_diagnoses: Vec<FailureDiagnosis>,
    #[serde(default)]
    pub failure_episodes: Vec<FailureEpisode>,
    #[serde(default)]
    pub diagnostic_slices: Vec<DiagnosticSlice>,
    #[serde(default)]
    pub replay_capsules: Vec<ReplayCapsule>,
    #[serde(default)]
    pub replay_executions: Vec<ReplayExecution>,
    #[serde(default)]
    pub external_evaluations: Vec<ExternalEvaluationRecord>,
    #[serde(default)]
    pub causal_comparisons: Vec<CausalComparison>,
    pub post_task_jobs: Vec<PostTaskJob>,
    pub terminal: bool,
    pub integrity_warnings: Vec<String>,
}
