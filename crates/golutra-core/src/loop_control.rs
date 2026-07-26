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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationReason {
    VerificationFailed,
    MissingArtifact,
    ToolFailed,
    ProviderRetry,
    ContextCompaction,
    UserSteer,
    BudgetExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    Running,
    CandidateComplete,
    Verifying,
    Correcting,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TurnState {
    pub turn_id: TurnId,
    pub phase: TurnPhase,
    pub correction_attempt: u32,
    pub last_verification_id: Option<VerificationId>,
    pub continuation_reason: Option<ContinuationReason>,
}

impl TurnState {
    #[must_use]
    pub fn new(turn_id: TurnId) -> Self {
        Self {
            turn_id,
            phase: TurnPhase::Running,
            correction_attempt: 0,
            last_verification_id: None,
            continuation_reason: None,
        }
    }

    pub fn candidate_complete(&mut self) {
        self.phase = TurnPhase::CandidateComplete;
        self.continuation_reason = None;
    }

    pub fn begin_verification(&mut self, verification_id: VerificationId) {
        self.phase = TurnPhase::Verifying;
        self.last_verification_id = Some(verification_id);
    }

    pub fn issue_correction(&mut self, reason: ContinuationReason) {
        self.correction_attempt = self.correction_attempt.saturating_add(1);
        self.phase = TurnPhase::Correcting;
        self.continuation_reason = Some(reason);
    }

    pub fn terminal(&mut self) {
        self.phase = TurnPhase::Terminal;
        self.continuation_reason = None;
    }
}

/// A bounded, model-visible subset of a failed verification.  The full
/// VerificationRecord remains an OS/observation fact and is never passed to
/// the provider directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CorrectionEnvelope {
    pub verification_id: VerificationId,
    pub attempt: u32,
    pub remaining_attempts: u32,
    pub failed_requirements: Vec<String>,
    pub evidence_refs: Vec<EvidenceId>,
    pub requested_action: String,
}

impl CorrectionEnvelope {
    #[must_use]
    pub fn as_model_instruction(&self) -> String {
        let requirements = if self.failed_requirements.is_empty() {
            "the required verification evidence".to_owned()
        } else {
            self.failed_requirements.join("; ")
        };
        format!(
            "Runtime verification did not pass. Correct the following before claiming completion: {requirements}. Requested action: {}. Remaining correction attempts: {}.",
            self.requested_action, self.remaining_attempts
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_state_records_candidate_verification_and_correction() {
        let turn_id = TurnId::new();
        let verification_id = VerificationId::new();
        let mut state = TurnState::new(turn_id);
        state.candidate_complete();
        state.begin_verification(verification_id);
        state.issue_correction(ContinuationReason::VerificationFailed);

        assert_eq!(state.phase, TurnPhase::Correcting);
        assert_eq!(state.correction_attempt, 1);
        assert_eq!(state.last_verification_id, Some(verification_id));
        assert_eq!(
            state.continuation_reason,
            Some(ContinuationReason::VerificationFailed)
        );
    }

    #[test]
    fn correction_envelope_exposes_only_bounded_feedback() {
        let envelope = CorrectionEnvelope {
            verification_id: VerificationId::new(),
            attempt: 1,
            remaining_attempts: 0,
            failed_requirements: vec!["tests must pass".to_owned()],
            evidence_refs: vec![EvidenceId::new()],
            requested_action: "run the failing test and fix the result".to_owned(),
        };
        let instruction = envelope.as_model_instruction();

        assert!(instruction.contains("tests must pass"));
        assert!(instruction.contains("Remaining correction attempts: 0"));
        assert!(!instruction.contains("RuntimeEvent"));
    }
}
