//! Runtime-owned verification boundary.
//!
//! The verifier crate implements individual assertion semantics.  This
//! adapter is the runtime policy boundary: the loop only asks this service to
//! plan or evaluate a fixed verification input and never constructs a success
//! result from assistant text.

use golutra_core::{
    TaskContract, TaskId, VerificationAssertionStatus, VerificationCheck, VerificationCheckKind,
    VerificationIndependence, VerificationPlan, VerificationRecord, VerificationResult,
    WorkspaceChangeRequirement,
};
use golutra_verify::{VerificationInput, VerificationRunner};

#[derive(Debug, Default, Clone, Copy)]
pub struct RuntimeVerificationService {
    runner: VerificationRunner,
}

impl RuntimeVerificationService {
    #[must_use]
    pub fn plan(&self, input: &VerificationInput) -> VerificationPlan {
        self.runner.plan(input)
    }

    /// Materialize the verifier plan under an explicit task contract. Legacy
    /// workspace requests still require durable change evidence, but they do
    /// not gain an implicit semantic-validation requirement that the contract
    /// did not request. Any validation the runtime actually observed remains
    /// authoritative, including failures.
    #[must_use]
    pub fn plan_governed(
        &self,
        input: &VerificationInput,
        contract: &TaskContract,
    ) -> VerificationPlan {
        let mut plan = self.runner.plan(input);
        let objective_validation_observed = input
            .command_checks
            .iter()
            .any(|check| check.kind == VerificationCheckKind::ObjectiveValidation);
        if !contract.require_objective_validation && !objective_validation_observed {
            plan.assertions.retain(|assertion| {
                !matches!(
                    assertion.criterion_id.as_str(),
                    "workspace_validation" | "tests_or_diagnostics"
                )
            });
        }
        plan
    }

    #[must_use]
    pub fn verify(
        &self,
        input: VerificationInput,
        plan: VerificationPlan,
    ) -> (VerificationRecord, VerificationPlan) {
        self.runner.verify_with_plan(input, plan)
    }

    /// Apply the task contract after the assertion runner has evaluated
    /// objective facts.  This is the terminal policy gate owned by the Runtime
    /// OS; provider wording cannot bypass it.
    #[must_use]
    pub fn verify_governed(
        &self,
        input: VerificationInput,
        plan: VerificationPlan,
        contract: &TaskContract,
        environment_digest: String,
    ) -> (VerificationRecord, VerificationPlan) {
        let workspace_changed = input.code_files_changed
            || input
                .command_checks
                .iter()
                .any(|check| check.kind == VerificationCheckKind::WorkspaceChange && check.passed);
        let objective_validated = input
            .command_checks
            .iter()
            .any(|check| check.kind == VerificationCheckKind::ObjectiveValidation && check.passed);
        let external_verified = input
            .command_checks
            .iter()
            .any(|check| check.name == "objective:test:external_verifier" && check.passed);
        let (mut record, plan) = self.runner.verify_with_plan(input, plan);
        record.environment_digest = Some(environment_digest);

        let blocking_assertions_satisfied = plan
            .assertions
            .iter()
            .chain(plan.policy_assertions.iter())
            .filter(|assertion| assertion.blocking)
            .all(|assertion| {
                matches!(
                    assertion.status,
                    VerificationAssertionStatus::Pass | VerificationAssertionStatus::NotApplicable
                )
            });

        let policy_status = plan
            .policy_assertions
            .iter()
            .find(|assertion| assertion.criterion_id == "policy")
            .map(|assertion| assertion.status);
        match policy_status {
            Some(VerificationAssertionStatus::Fail) => {
                record.result = VerificationResult::Fail;
                record.policy_status = "policy_blocked".to_owned();
                record
                    .residual_risks
                    .push("a blocking policy decision was recorded".to_owned());
            }
            Some(VerificationAssertionStatus::Unknown) | None => {
                if record.result == VerificationResult::Pass {
                    record.result = VerificationResult::Partial;
                }
                record.policy_status = "policy_unknown".to_owned();
                record
                    .residual_risks
                    .push("policy decision evidence is missing".to_owned());
            }
            Some(VerificationAssertionStatus::Pass)
            | Some(VerificationAssertionStatus::Pending)
            | Some(VerificationAssertionStatus::NotApplicable) => {}
        }

        let mut contract_failures = Vec::new();
        match contract.workspace_change {
            WorkspaceChangeRequirement::Required if !workspace_changed => {
                contract_failures.push("task contract requires a workspace change");
            }
            WorkspaceChangeRequirement::Forbidden if workspace_changed => {
                contract_failures.push("task contract forbids workspace changes");
            }
            WorkspaceChangeRequirement::Optional
            | WorkspaceChangeRequirement::Required
            | WorkspaceChangeRequirement::Forbidden => {}
        }
        if contract.require_objective_validation && !objective_validated {
            contract_failures.push("task contract requires objective validation");
        }
        if contract.requires_independent_verification() && !external_verified {
            contract_failures.push("task contract requires an independent verifier");
        }

        if !contract_failures.is_empty() {
            record.result = VerificationResult::Fail;
            record.policy_status = "task_contract_failed".to_owned();
            record
                .residual_risks
                .extend(contract_failures.into_iter().map(ToOwned::to_owned));
        } else if record.policy_status == "policy_unknown" {
            record
                .residual_risks
                .push("task contract cannot be satisfied without policy evidence".to_owned());
        } else if record.policy_status != "policy_blocked" {
            record.policy_status = "task_contract_satisfied".to_owned();
            if contract.requires_independent_verification() {
                record.independence = VerificationIndependence::Independent;
            }
            if record.result == VerificationResult::Partial
                && workspace_changed
                && !contract.require_objective_validation
                && blocking_assertions_satisfied
            {
                record.result = VerificationResult::Pass;
                record
                    .residual_risks
                    .retain(|risk| risk != "some objective checks failed");
            }
        }
        (record, plan)
    }

    #[must_use]
    pub fn verify_unplanned(&self, input: VerificationInput) -> VerificationRecord {
        self.runner.verify(input)
    }

    /// Runtime 自身失败时仍生成验证事实，避免失败任务在 trace 中缺少验证计划和结论。
    #[must_use]
    pub fn verify_runtime_failure(
        &self,
        task_id: TaskId,
        objective: impl Into<String>,
        completion_criteria: Vec<String>,
        requires_workspace_evidence: bool,
        reason: impl Into<String>,
    ) -> (VerificationRecord, VerificationPlan) {
        let objective = objective.into();
        let reason = reason.into();
        let check_kind = if requires_workspace_evidence {
            VerificationCheckKind::ObjectiveValidation
        } else {
            VerificationCheckKind::AssistantResponse
        };
        let input = VerificationInput {
            task_id,
            objective,
            completion_criteria,
            evidence_refs: Vec::new(),
            command_checks: vec![VerificationCheck {
                kind: check_kind,
                name: "runtime_execution".to_owned(),
                command: None,
                passed: false,
                evidence_refs: Vec::new(),
                message: reason.clone(),
            }],
            requires_workspace_evidence,
            code_files_changed: false,
        };
        let plan = self.runner.plan(&input);
        let (mut record, plan) = self.runner.verify_with_plan(input, plan);
        record.result = VerificationResult::Fail;
        record.policy_status = "runtime_failed".to_owned();
        if !record.residual_risks.contains(&reason) {
            record.residual_risks.push(reason);
        }
        (record, plan)
    }
}

#[cfg(test)]
mod tests {
    use golutra_core::{TaskClass, VerificationDimensionStatus};

    use super::*;

    #[test]
    fn runtime_failure_produces_a_failed_record_and_fixed_plan() {
        let task_id = TaskId::new();
        let (record, plan) = RuntimeVerificationService::default().verify_runtime_failure(
            task_id,
            "update the workspace",
            vec!["runtime task produces a verified terminal result".to_owned()],
            true,
            "provider failed",
        );

        assert_eq!(record.task_id, task_id);
        assert_eq!(record.result, VerificationResult::Fail);
        assert_eq!(record.policy_status, "runtime_failed");
        assert!(
            record
                .residual_risks
                .iter()
                .any(|risk| risk == "provider failed")
        );
        assert_eq!(plan.task_id, task_id);
        assert_eq!(plan.task_class, TaskClass::ReadOnlyAnalysis);
        assert_ne!(
            plan.dimensions.objective_status,
            VerificationDimensionStatus::Pass
        );
    }

    #[test]
    fn governed_plan_does_not_invent_unrequested_objective_validation() {
        let evidence = golutra_core::EvidenceId::new();
        let input = VerificationInput {
            task_id: TaskId::new(),
            objective: "update the workspace".to_owned(),
            completion_criteria: Vec::new(),
            evidence_refs: vec![evidence],
            command_checks: vec![
                VerificationCheck {
                    kind: VerificationCheckKind::WorkspaceChange,
                    name: "workspace_diff".to_owned(),
                    command: None,
                    passed: true,
                    evidence_refs: vec![evidence],
                    message: "workspace changed".to_owned(),
                },
                VerificationCheck {
                    kind: VerificationCheckKind::Policy,
                    name: "policy:write_file".to_owned(),
                    command: None,
                    passed: true,
                    evidence_refs: vec![evidence],
                    message: "policy allowed execution".to_owned(),
                },
            ],
            requires_workspace_evidence: true,
            code_files_changed: false,
        };
        let contract = TaskContract {
            workspace_change: WorkspaceChangeRequirement::Required,
            verification: golutra_core::VerificationRequirement::Required,
            ..TaskContract::default()
        };

        let service = RuntimeVerificationService::default();
        let plan = service.plan_governed(&input, &contract);
        assert!(!plan.assertions.iter().any(|assertion| {
            assertion.criterion_id == "workspace_validation"
                || assertion.criterion_id == "tests_or_diagnostics"
        }));
        let (record, _) =
            service.verify_governed(input, plan, &contract, "sha256:test-environment".to_owned());
        assert_eq!(record.result, VerificationResult::Pass);
    }

    #[test]
    fn governed_plan_retains_observed_validation_failures() {
        let evidence = golutra_core::EvidenceId::new();
        let input = VerificationInput {
            task_id: TaskId::new(),
            objective: "update the workspace".to_owned(),
            completion_criteria: Vec::new(),
            evidence_refs: vec![evidence],
            command_checks: vec![
                VerificationCheck {
                    kind: VerificationCheckKind::WorkspaceChange,
                    name: "workspace_diff".to_owned(),
                    command: None,
                    passed: true,
                    evidence_refs: vec![evidence],
                    message: "workspace changed".to_owned(),
                },
                VerificationCheck {
                    kind: VerificationCheckKind::ObjectiveValidation,
                    name: "objective:test:project".to_owned(),
                    command: Some("project-test".to_owned()),
                    passed: false,
                    evidence_refs: vec![evidence],
                    message: "project test failed".to_owned(),
                },
            ],
            requires_workspace_evidence: true,
            code_files_changed: true,
        };
        let contract = TaskContract {
            workspace_change: WorkspaceChangeRequirement::Required,
            verification: golutra_core::VerificationRequirement::Required,
            ..TaskContract::default()
        };

        let service = RuntimeVerificationService::default();
        let plan = service.plan_governed(&input, &contract);
        assert!(plan.assertions.iter().any(|assertion| {
            assertion.criterion_id == "workspace_validation"
                || assertion.criterion_id == "tests_or_diagnostics"
        }));
        let (record, _) =
            service.verify_governed(input, plan, &contract, "sha256:test-environment".to_owned());
        assert_eq!(record.result, VerificationResult::Fail);
    }

    #[test]
    fn governed_verification_preserves_a_blocking_policy_result() {
        let evidence = golutra_core::EvidenceId::new();
        let input = VerificationInput {
            task_id: TaskId::new(),
            objective: "update the workspace".to_owned(),
            completion_criteria: Vec::new(),
            evidence_refs: vec![evidence],
            command_checks: vec![
                VerificationCheck {
                    kind: VerificationCheckKind::WorkspaceChange,
                    name: "workspace_diff".to_owned(),
                    command: None,
                    passed: true,
                    evidence_refs: vec![evidence],
                    message: "workspace changed".to_owned(),
                },
                VerificationCheck {
                    kind: VerificationCheckKind::Policy,
                    name: "policy:write_file".to_owned(),
                    command: None,
                    passed: false,
                    evidence_refs: vec![evidence],
                    message: "policy blocked execution".to_owned(),
                },
            ],
            requires_workspace_evidence: true,
            code_files_changed: false,
        };
        let contract = TaskContract {
            workspace_change: WorkspaceChangeRequirement::Required,
            verification: golutra_core::VerificationRequirement::Required,
            ..TaskContract::default()
        };

        let service = RuntimeVerificationService::default();
        let plan = service.plan_governed(&input, &contract);
        let (record, _) =
            service.verify_governed(input, plan, &contract, "sha256:test-environment".to_owned());

        assert_eq!(record.result, VerificationResult::Fail);
        assert_eq!(record.policy_status, "policy_blocked");
    }

    #[test]
    fn governed_verification_does_not_promote_failed_read_only_execution() {
        let evidence = golutra_core::EvidenceId::new();
        let input = VerificationInput {
            task_id: TaskId::new(),
            objective: "inspect the current state".to_owned(),
            completion_criteria: Vec::new(),
            evidence_refs: vec![evidence],
            command_checks: vec![
                VerificationCheck {
                    kind: VerificationCheckKind::ToolExecution,
                    name: "tool:shell".to_owned(),
                    command: Some("status-command".to_owned()),
                    passed: false,
                    evidence_refs: vec![evidence],
                    message: "status command failed".to_owned(),
                },
                VerificationCheck {
                    kind: VerificationCheckKind::Policy,
                    name: "policy:shell".to_owned(),
                    command: None,
                    passed: true,
                    evidence_refs: vec![evidence],
                    message: "policy allowed execution".to_owned(),
                },
                VerificationCheck {
                    kind: VerificationCheckKind::AssistantResponse,
                    name: "assistant_response".to_owned(),
                    command: None,
                    passed: true,
                    evidence_refs: vec![evidence],
                    message: "assistant responded".to_owned(),
                },
            ],
            requires_workspace_evidence: false,
            code_files_changed: false,
        };
        let contract = TaskContract::default();

        let service = RuntimeVerificationService::default();
        let plan = service.plan_governed(&input, &contract);
        let (record, _) =
            service.verify_governed(input, plan, &contract, "sha256:test-environment".to_owned());

        assert_eq!(record.result, VerificationResult::Partial);
    }
}
