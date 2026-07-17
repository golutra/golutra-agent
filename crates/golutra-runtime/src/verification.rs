//! Runtime-owned verification boundary.
//!
//! The verifier crate implements individual assertion semantics.  This
//! adapter is the runtime policy boundary: the loop only asks this service to
//! plan or evaluate a fixed verification input and never constructs a success
//! result from assistant text.

use golutra_core::{
    TaskId, VerificationCheck, VerificationCheckKind, VerificationPlan, VerificationRecord,
    VerificationResult,
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

    #[must_use]
    pub fn verify(
        &self,
        input: VerificationInput,
        plan: VerificationPlan,
    ) -> (VerificationRecord, VerificationPlan) {
        self.runner.verify_with_plan(input, plan)
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
}
