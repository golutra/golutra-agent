use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{EvidenceId, TaskStatus, VerificationResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    Running,
    CandidateReady,
    Completed,
    Partial,
    Failed,
    Aborted,
    Blocked,
    Cancelled,
    Interrupted,
    Uncertain,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExternalVerificationStatus {
    #[default]
    NotRequested,
    Pending,
    Pass,
    Partial,
    Fail,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    RuntimeControlFlow,
    Context,
    Provider,
    Tool,
    Policy,
    Verification,
    ExternalEvaluation,
    Environment,
    Timeout,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskOutcome {
    pub execution: ExecutionOutcome,
    pub verification: VerificationResult,
    #[serde(default)]
    pub evidence_refs: Vec<EvidenceId>,
    #[serde(default)]
    pub external_verification: ExternalVerificationStatus,
    #[serde(default)]
    pub failure_class: Option<FailureClass>,
    pub scorable: bool,
    pub confidence: u8,
    #[serde(default)]
    pub next_action: Option<String>,
}

impl TaskOutcome {
    #[must_use]
    pub fn from_status(status: TaskStatus, verification: VerificationResult) -> Self {
        let execution = match status {
            TaskStatus::Idle => ExecutionOutcome::Running,
            TaskStatus::Running
            | TaskStatus::WaitingApproval
            | TaskStatus::WaitingAuthentication
            | TaskStatus::Pausing
            | TaskStatus::Paused
            | TaskStatus::Aborting => ExecutionOutcome::Running,
            TaskStatus::Completed => ExecutionOutcome::Completed,
            TaskStatus::Partial => ExecutionOutcome::Partial,
            TaskStatus::Failed => ExecutionOutcome::Failed,
            TaskStatus::Blocked => ExecutionOutcome::Blocked,
            TaskStatus::Cancelled => ExecutionOutcome::Cancelled,
            TaskStatus::Interrupted => ExecutionOutcome::Interrupted,
            TaskStatus::Uncertain => ExecutionOutcome::Uncertain,
        };
        let failure_class = match execution {
            ExecutionOutcome::Completed => None,
            ExecutionOutcome::Blocked => Some(FailureClass::Policy),
            ExecutionOutcome::Partial => Some(FailureClass::Verification),
            ExecutionOutcome::Failed => Some(FailureClass::RuntimeControlFlow),
            ExecutionOutcome::Cancelled | ExecutionOutcome::Interrupted => {
                Some(FailureClass::RuntimeControlFlow)
            }
            ExecutionOutcome::Uncertain => Some(FailureClass::Unknown),
            ExecutionOutcome::Running
            | ExecutionOutcome::CandidateReady
            | ExecutionOutcome::Aborted => Some(FailureClass::Unknown),
        };
        Self {
            execution,
            verification,
            evidence_refs: Vec::new(),
            external_verification: ExternalVerificationStatus::NotRequested,
            failure_class,
            scorable: !matches!(
                execution,
                ExecutionOutcome::Running | ExecutionOutcome::Aborted | ExecutionOutcome::Uncertain
            ),
            confidence: if verification == VerificationResult::Unknown {
                0
            } else {
                100
            },
            next_action: None,
        }
    }

    #[must_use]
    pub fn from_verification(status: TaskStatus, verification: &crate::VerificationRecord) -> Self {
        let mut outcome = Self::from_status(status, verification.result);
        if matches!(
            outcome.execution,
            ExecutionOutcome::Failed | ExecutionOutcome::Partial
        ) {
            outcome.failure_class = Some(if verification.policy_status == "policy_blocked" {
                FailureClass::Policy
            } else if verification.policy_status == "runtime_failed" {
                FailureClass::RuntimeControlFlow
            } else {
                FailureClass::Verification
            });
            outcome.next_action = Some(match outcome.failure_class {
                Some(FailureClass::Policy) => {
                    "resolve the policy decision or change the approval mode".to_owned()
                }
                Some(FailureClass::Verification) => {
                    "inspect failed checks and retry with objective evidence".to_owned()
                }
                _ => "inspect the runtime failure and retry the task".to_owned(),
            });
        }
        outcome.evidence_refs = verification.evidence_refs.clone();
        outcome
    }

    /// Build a non-terminal candidate whose authoritative verification is
    /// expected to arrive from an external evaluator.
    #[must_use]
    pub fn candidate_ready(verification: &crate::VerificationRecord) -> Self {
        Self {
            execution: ExecutionOutcome::CandidateReady,
            verification: verification.result,
            evidence_refs: verification.evidence_refs.clone(),
            external_verification: ExternalVerificationStatus::Pending,
            failure_class: None,
            scorable: false,
            confidence: if verification.result == VerificationResult::Unknown {
                0
            } else {
                50
            },
            next_action: Some("await the external evaluator result".to_owned()),
        }
    }

    #[must_use]
    pub fn with_evidence_refs(mut self, evidence_refs: Vec<EvidenceId>) -> Self {
        self.evidence_refs = evidence_refs;
        self
    }

    #[must_use]
    pub fn with_external_verification(mut self, status: ExternalVerificationStatus) -> Self {
        self.external_verification = status;
        let is_candidate = self.execution == ExecutionOutcome::CandidateReady;
        match status {
            ExternalVerificationStatus::Pass => {
                if is_candidate {
                    self.execution = ExecutionOutcome::Completed;
                    self.scorable = true;
                    self.confidence = self.confidence.max(90);
                    self.failure_class = None;
                    self.next_action = None;
                } else {
                    self.scorable = false;
                    self.confidence = self.confidence.min(50);
                }
            }
            ExternalVerificationStatus::Fail => {
                self.scorable = false;
                self.confidence = self.confidence.min(50);
                if is_candidate {
                    self.execution = ExecutionOutcome::Failed;
                    self.failure_class = Some(FailureClass::ExternalEvaluation);
                    self.next_action = Some("inspect external evaluator assertions".to_owned());
                }
            }
            ExternalVerificationStatus::Pending => {
                self.scorable = false;
                self.confidence = self.confidence.min(50);
                if is_candidate {
                    self.next_action = Some("await the external evaluator result".to_owned());
                }
            }
            ExternalVerificationStatus::Partial => {
                if is_candidate {
                    self.execution = ExecutionOutcome::Partial;
                    self.failure_class = Some(FailureClass::ExternalEvaluation);
                    self.next_action =
                        Some("inspect partial external evaluator results".to_owned());
                }
                self.scorable = false;
                self.confidence = self.confidence.min(50);
            }
            ExternalVerificationStatus::Unknown => {
                if is_candidate {
                    self.execution = ExecutionOutcome::Uncertain;
                    self.failure_class = Some(FailureClass::ExternalEvaluation);
                    self.next_action =
                        Some("resolve the inconclusive external evaluation".to_owned());
                }
                self.scorable = false;
                self.confidence = self.confidence.min(50);
            }
            ExternalVerificationStatus::NotRequested => {}
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_failure_keeps_policy_class_and_evidence_refs() {
        let evidence = EvidenceId::new();
        let verification = crate::VerificationRecord {
            verification_id: crate::VerificationId::new(),
            task_id: crate::TaskId::new(),
            objective: "run a command".to_owned(),
            completion_criteria: Vec::new(),
            checks: Vec::new(),
            evidence_refs: vec![evidence],
            result: VerificationResult::Fail,
            policy_status: "policy_blocked".to_owned(),
            residual_risks: Vec::new(),
            plan_id: None,
            assertions: Vec::new(),
            source: crate::VerificationSource::Runtime,
            independence: crate::VerificationIndependence::RuntimeEvidence,
            environment_digest: None,
        };

        let outcome = TaskOutcome::from_verification(TaskStatus::Failed, &verification);
        assert_eq!(outcome.failure_class, Some(FailureClass::Policy));
        assert_eq!(outcome.evidence_refs, vec![evidence]);
        assert!(outcome.next_action.is_some());
    }

    #[test]
    fn external_pass_does_not_upgrade_a_failed_runtime_execution() {
        let outcome = TaskOutcome::from_status(TaskStatus::Failed, VerificationResult::Fail)
            .with_external_verification(ExternalVerificationStatus::Pass);

        assert_eq!(outcome.execution, ExecutionOutcome::Failed);
        assert_eq!(outcome.verification, VerificationResult::Fail);
        assert_eq!(
            outcome.external_verification,
            ExternalVerificationStatus::Pass
        );
        assert!(!outcome.scorable);
        assert_eq!(
            outcome.failure_class,
            Some(FailureClass::RuntimeControlFlow)
        );
    }

    #[test]
    fn external_pass_upgrades_only_an_explicit_candidate() {
        let verification = crate::VerificationRecord {
            verification_id: crate::VerificationId::new(),
            task_id: crate::TaskId::new(),
            objective: "produce a candidate".to_owned(),
            completion_criteria: Vec::new(),
            checks: Vec::new(),
            evidence_refs: vec![EvidenceId::new()],
            result: VerificationResult::Partial,
            policy_status: "task_contract_satisfied".to_owned(),
            residual_risks: vec!["external proof is pending".to_owned()],
            plan_id: None,
            assertions: Vec::new(),
            source: crate::VerificationSource::Runtime,
            independence: crate::VerificationIndependence::RuntimeEvidence,
            environment_digest: None,
        };

        let pending = TaskOutcome::candidate_ready(&verification);
        assert_eq!(pending.execution, ExecutionOutcome::CandidateReady);
        assert_eq!(pending.verification, VerificationResult::Partial);
        assert_eq!(
            pending.external_verification,
            ExternalVerificationStatus::Pending
        );
        assert!(!pending.scorable);
        assert_eq!(pending.failure_class, None);

        let completed = pending.with_external_verification(ExternalVerificationStatus::Pass);
        assert_eq!(completed.execution, ExecutionOutcome::Completed);
        assert_eq!(completed.verification, VerificationResult::Partial);
        assert!(completed.scorable);
        assert_eq!(completed.failure_class, None);
        assert_eq!(completed.next_action, None);
    }

    #[test]
    fn external_failure_closes_a_candidate_without_masking_its_source() {
        let verification = crate::VerificationRecord {
            verification_id: crate::VerificationId::new(),
            task_id: crate::TaskId::new(),
            objective: "produce a candidate".to_owned(),
            completion_criteria: Vec::new(),
            checks: Vec::new(),
            evidence_refs: Vec::new(),
            result: VerificationResult::Unknown,
            policy_status: "task_contract_satisfied".to_owned(),
            residual_risks: Vec::new(),
            plan_id: None,
            assertions: Vec::new(),
            source: crate::VerificationSource::Runtime,
            independence: crate::VerificationIndependence::Unspecified,
            environment_digest: None,
        };

        let failed = TaskOutcome::candidate_ready(&verification)
            .with_external_verification(ExternalVerificationStatus::Fail);

        assert_eq!(failed.execution, ExecutionOutcome::Failed);
        assert_eq!(failed.verification, VerificationResult::Unknown);
        assert_eq!(failed.failure_class, Some(FailureClass::ExternalEvaluation));
        assert!(!failed.scorable);
    }
}
