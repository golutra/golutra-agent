use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{EvidenceId, LoopDecisionId, PolicyId, TaskId, TurnId, VerificationId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoopAction {
    Continue,
    Compact,
    Retry,
    Fallback,
    AskUser,
    Verify,
    StopSuccess,
    StopPartial,
    StopFailed,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoopGuardTrigger {
    RepeatedToolFailure,
    EmptyResponse,
    ContextOverflow,
    MaxIteration,
    NoProgress,
    RetryCostExceeded,
    OversizedToolOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoopGuardAction {
    Nudge,
    Compact,
    Retry,
    Fallback,
    AskUser,
    SynthesizeFinal,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LoopGuardRule {
    pub rule_id: String,
    pub trigger: LoopGuardTrigger,
    pub threshold: u32,
    pub action: LoopGuardAction,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BudgetState {
    pub planned_input_tokens: Option<u64>,
    pub actual_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub estimated_cost: Option<String>,
    pub budget_remaining: Option<u64>,
    pub compact_recommended: bool,
    pub cost_risk: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LoopDecision {
    pub decision_id: LoopDecisionId,
    pub task_id: TaskId,
    pub turn_id: TurnId,
    pub action: LoopAction,
    pub reason: String,
    pub evidence_refs: Vec<EvidenceId>,
    pub verification_ref: Option<VerificationId>,
    pub policy_ref: Option<PolicyId>,
    pub budget_state: BudgetState,
    pub tool_state: String,
    pub model_state: String,
    pub next_step: Option<String>,
}
