use golutra_core::{
    EvidenceId, TaskId, VerificationCheck, VerificationCheckKind, VerificationId,
    VerificationRecord, VerificationResult,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationInput {
    pub task_id: TaskId,
    pub objective: String,
    pub completion_criteria: Vec<String>,
    pub evidence_refs: Vec<EvidenceId>,
    pub command_checks: Vec<VerificationCheck>,
    pub requires_workspace_evidence: bool,
    pub code_files_changed: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct VerificationRunner;

impl VerificationRunner {
    #[must_use]
    pub fn verify(&self, input: VerificationInput) -> VerificationRecord {
        let has_evidence = !input.evidence_refs.is_empty();
        let commands_passed = input.command_checks.iter().all(|check| check.passed);
        let change_recorded = passed_check(
            &input.command_checks,
            VerificationCheckKind::WorkspaceChange,
        );
        let objective_validated = passed_check(
            &input.command_checks,
            VerificationCheckKind::ObjectiveValidation,
        );
        let result = if input.code_files_changed {
            match (
                has_evidence,
                commands_passed,
                change_recorded,
                objective_validated,
            ) {
                (true, true, true, true) => VerificationResult::Pass,
                (true, _, true, _) => VerificationResult::Partial,
                _ => VerificationResult::Fail,
            }
        } else {
            match (has_evidence, commands_passed) {
                (true, true) => VerificationResult::Pass,
                (true, false) => VerificationResult::Partial,
                (false, _) if input.requires_workspace_evidence => VerificationResult::Fail,
                (false, _) => VerificationResult::Unknown,
            }
        };

        VerificationRecord {
            verification_id: VerificationId::new(),
            task_id: input.task_id,
            objective: input.objective,
            completion_criteria: input.completion_criteria,
            checks: input.command_checks,
            evidence_refs: input.evidence_refs,
            result,
            policy_status: "p0_policy_checked".to_owned(),
            residual_risks: residual_risks(result),
        }
    }
}

fn passed_check(checks: &[VerificationCheck], kind: VerificationCheckKind) -> bool {
    checks
        .iter()
        .any(|check| check.kind == kind && check.passed)
}

#[must_use]
pub fn requires_objective_evidence(task_touched_code: bool) -> bool {
    task_touched_code
}

fn residual_risks(result: VerificationResult) -> Vec<String> {
    match result {
        VerificationResult::Pass => Vec::new(),
        VerificationResult::Fail => vec!["objective evidence is missing or failed".to_owned()],
        VerificationResult::Partial => vec!["some objective checks failed".to_owned()],
        VerificationResult::Unknown => vec!["verification evidence is insufficient".to_owned()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_task_without_evidence_fails() {
        let record = VerificationRunner.verify(VerificationInput {
            task_id: TaskId::new(),
            objective: "change code".to_owned(),
            completion_criteria: vec!["evidence exists".to_owned()],
            evidence_refs: Vec::new(),
            command_checks: Vec::new(),
            requires_workspace_evidence: true,
            code_files_changed: true,
        });

        assert_eq!(record.result, VerificationResult::Fail);
    }

    #[test]
    fn evidence_with_passing_checks_passes() {
        let record = VerificationRunner.verify(VerificationInput {
            task_id: TaskId::new(),
            objective: "read file".to_owned(),
            completion_criteria: vec!["evidence exists".to_owned()],
            evidence_refs: vec![EvidenceId::new()],
            command_checks: vec![VerificationCheck {
                kind: VerificationCheckKind::ToolExecution,
                name: "command".to_owned(),
                command: Some("true".to_owned()),
                passed: true,
                evidence_refs: Vec::new(),
                message: "ok".to_owned(),
            }],
            requires_workspace_evidence: false,
            code_files_changed: false,
        });

        assert_eq!(record.result, VerificationResult::Pass);
    }

    #[test]
    fn code_change_requires_diff_and_objective_validation() {
        let evidence = EvidenceId::new();
        let record = VerificationRunner.verify(VerificationInput {
            task_id: TaskId::new(),
            objective: "change code".to_owned(),
            completion_criteria: vec!["tests pass".to_owned()],
            evidence_refs: vec![evidence],
            command_checks: vec![VerificationCheck {
                kind: VerificationCheckKind::WorkspaceChange,
                name: "workspace_diff".to_owned(),
                command: None,
                passed: true,
                evidence_refs: vec![evidence],
                message: "code changed".to_owned(),
            }],
            requires_workspace_evidence: true,
            code_files_changed: true,
        });

        assert_eq!(record.result, VerificationResult::Partial);
        assert!(
            record
                .residual_risks
                .contains(&"some objective checks failed".to_owned())
        );
    }
}
