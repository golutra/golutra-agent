use golutra_core::{
    EvidenceId, TaskClass, TaskId, VerificationAssertion, VerificationAssertionKind,
    VerificationAssertionStatus, VerificationCheck, VerificationCheckKind,
    VerificationDimensionStatus, VerificationDimensions, VerificationId, VerificationPlan,
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
    /// 先固定任务类别和客观断言，再让执行结果填充断言状态，避免验证标准由模型的最终措辞决定。
    #[must_use]
    pub fn plan(&self, input: &VerificationInput) -> VerificationPlan {
        let task_class = classify_task(input);
        let mut assertions = Vec::new();
        match task_class {
            TaskClass::PlainConversation => assertions.push(assertion(
                "assistant_response",
                VerificationAssertionKind::AssistantResponse,
                "assistant response",
                "a non-empty assistant response is emitted",
                false,
            )),
            TaskClass::ReadOnlyAnalysis => {
                assertions.push(assertion(
                    "assistant_response",
                    VerificationAssertionKind::AssistantResponse,
                    "assistant response",
                    "a non-empty answer is emitted",
                    false,
                ));
                if !input.command_checks.is_empty() {
                    assertions.push(assertion(
                        "analysis_evidence",
                        VerificationAssertionKind::Delivery,
                        "analysis evidence",
                        "observations are backed by durable tool evidence",
                        true,
                    ));
                    if input
                        .command_checks
                        .iter()
                        .any(|check| check.kind == VerificationCheckKind::ObjectiveValidation)
                    {
                        assertions.push(assertion(
                            "analysis_objective",
                            VerificationAssertionKind::Diagnostic,
                            "requested observation",
                            "the observed file or command target matches the objective",
                            true,
                        ));
                    }
                }
            }
            TaskClass::WorkspaceChange => {
                assertions.push(assertion(
                    "workspace_diff",
                    VerificationAssertionKind::Diff,
                    "workspace",
                    "the requested workspace change is recorded",
                    true,
                ));
                assertions.push(assertion(
                    "file_state",
                    VerificationAssertionKind::FileState,
                    "changed files",
                    "changed file state is represented by durable evidence",
                    true,
                ));
                assertions.push(assertion(
                    "objective_validation",
                    VerificationAssertionKind::Diagnostic,
                    "objective",
                    "an objective validation command or check succeeds",
                    true,
                ));
            }
            TaskClass::CodeChange => {
                assertions.push(assertion(
                    "workspace_diff",
                    VerificationAssertionKind::Diff,
                    "workspace",
                    "the requested code change is recorded",
                    true,
                ));
                assertions.push(assertion(
                    "file_state",
                    VerificationAssertionKind::FileState,
                    "changed code files",
                    "the changed code files have durable before/after evidence",
                    true,
                ));
                assertions.push(assertion(
                    "tests_or_diagnostics",
                    VerificationAssertionKind::Test,
                    "objective",
                    "a test, check, build, or diagnostic command succeeds",
                    true,
                ));
            }
        }
        for (index, criterion) in input.completion_criteria.iter().enumerate() {
            assertions.push(assertion(
                &format!("criterion-{}", index.saturating_add(1)),
                criterion_assertion_kind(task_class, criterion),
                criterion,
                criterion,
                true,
            ));
        }
        let policy_assertion = assertion(
            "policy",
            VerificationAssertionKind::Policy,
            "runtime policy",
            "no blocking policy decision is present",
            true,
        );
        VerificationPlan {
            plan_id: golutra_core::VerificationPlanId::new(),
            task_id: input.task_id,
            task_class,
            criteria: input.completion_criteria.clone(),
            assertions,
            policy_assertions: vec![policy_assertion],
            required_artifact_types: if matches!(
                task_class,
                TaskClass::WorkspaceChange | TaskClass::CodeChange
            ) {
                vec!["tool_output".to_owned(), "evidence".to_owned()]
            } else {
                Vec::new()
            },
            generated_by: "golutra-verifier/v3".to_owned(),
            verifier_versions: vec!["semantic-assertions-v2".to_owned()],
            dimensions: VerificationDimensions::default(),
            revision: 1,
            created_at: chrono::Utc::now(),
        }
    }

    /// 依据固定计划更新每条断言；缺少客观事实时返回 Unknown/Fail，而不是把模型自述当成完成证据。
    #[must_use]
    pub fn verify_with_plan(
        &self,
        input: VerificationInput,
        mut plan: VerificationPlan,
    ) -> (VerificationRecord, VerificationPlan) {
        let mut record = self.verify_legacy(input.clone());
        let has_evidence = !input.evidence_refs.is_empty();
        for assertion in plan
            .assertions
            .iter_mut()
            .chain(plan.policy_assertions.iter_mut())
        {
            let (status, message, refs) = assertion_status(assertion, &input, has_evidence);
            assertion.status = status;
            assertion.message = message;
            assertion.evidence_refs = refs;
        }
        plan.dimensions = verification_dimensions(&plan, has_evidence);
        let blocking_failed = plan
            .assertions
            .iter()
            .chain(plan.policy_assertions.iter())
            .any(|assertion| {
                assertion.blocking && assertion.status == VerificationAssertionStatus::Fail
            });
        let blocking_unresolved = plan
            .assertions
            .iter()
            .chain(plan.policy_assertions.iter())
            .any(|assertion| {
                assertion.blocking
                    && assertion.status != VerificationAssertionStatus::Pass
                    && assertion.status != VerificationAssertionStatus::Fail
            });
        if blocking_failed {
            record.result = VerificationResult::Fail;
            record
                .residual_risks
                .push("semantic verification has failed blocking assertions".to_owned());
        } else if blocking_unresolved && record.result == VerificationResult::Pass {
            record.result = VerificationResult::Partial;
            record
                .residual_risks
                .push("semantic verification has unresolved blocking assertions".to_owned());
        }
        (record, plan)
    }

    #[must_use]
    pub fn verify(&self, input: VerificationInput) -> VerificationRecord {
        let plan = self.plan(&input);
        self.verify_with_plan(input, plan).0
    }

    fn verify_legacy(&self, input: VerificationInput) -> VerificationRecord {
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
        let assistant_response = passed_check(
            &input.command_checks,
            VerificationCheckKind::AssistantResponse,
        );
        let result = if !input.code_files_changed
            && !input.requires_workspace_evidence
            && assistant_response
            && input
                .command_checks
                .iter()
                .filter(|check| check.kind != VerificationCheckKind::AssistantResponse)
                .all(|check| check.passed)
        {
            VerificationResult::Pass
        } else if input.code_files_changed {
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

fn verification_dimensions(plan: &VerificationPlan, has_evidence: bool) -> VerificationDimensions {
    let objective_assertions = plan
        .assertions
        .iter()
        .filter(|assertion| assertion.blocking);
    let objective_status = aggregate_dimension(objective_assertions);
    let policy_status = aggregate_dimension(
        plan.policy_assertions
            .iter()
            .filter(|assertion| assertion.blocking),
    );
    let evidence_status = if plan.task_class == TaskClass::PlainConversation || has_evidence {
        VerificationDimensionStatus::Pass
    } else if plan.assertions.iter().any(|assertion| assertion.blocking) {
        VerificationDimensionStatus::Fail
    } else {
        VerificationDimensionStatus::Unknown
    };
    VerificationDimensions {
        evidence_status,
        objective_status,
        policy_status,
    }
}

fn aggregate_dimension<'a>(
    assertions: impl Iterator<Item = &'a VerificationAssertion>,
) -> VerificationDimensionStatus {
    let statuses = assertions
        .map(|assertion| assertion.status)
        .collect::<Vec<_>>();
    if statuses.is_empty() {
        return VerificationDimensionStatus::Unknown;
    }
    if statuses.contains(&VerificationAssertionStatus::Fail) {
        VerificationDimensionStatus::Fail
    } else if statuses.contains(&VerificationAssertionStatus::Unknown) {
        VerificationDimensionStatus::Partial
    } else if statuses
        .iter()
        .all(|status| *status == VerificationAssertionStatus::Pass)
    {
        VerificationDimensionStatus::Pass
    } else {
        VerificationDimensionStatus::Partial
    }
}

fn classify_task(input: &VerificationInput) -> TaskClass {
    if input.code_files_changed {
        TaskClass::CodeChange
    } else if input.requires_workspace_evidence
        && input
            .command_checks
            .iter()
            .any(|check| check.kind == VerificationCheckKind::WorkspaceChange)
    {
        TaskClass::WorkspaceChange
    } else if input.requires_workspace_evidence {
        TaskClass::ReadOnlyAnalysis
    } else if input.evidence_refs.is_empty()
        && input
            .command_checks
            .iter()
            .all(|check| check.kind == VerificationCheckKind::AssistantResponse)
    {
        TaskClass::PlainConversation
    } else {
        TaskClass::ReadOnlyAnalysis
    }
}

fn criterion_assertion_kind(task_class: TaskClass, criterion: &str) -> VerificationAssertionKind {
    if task_class == TaskClass::PlainConversation {
        return VerificationAssertionKind::AssistantResponse;
    }
    let lower = criterion.to_ascii_lowercase();
    if ["test", "fixture"]
        .iter()
        .any(|marker| lower.contains(marker))
        || ["测试", "用例"]
            .iter()
            .any(|marker| criterion.contains(marker))
    {
        return VerificationAssertionKind::Test;
    }
    if [
        "check",
        "build",
        "compile",
        "lint",
        "typecheck",
        "diagnostic",
        "schema",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
        || ["检查", "构建", "编译", "诊断", "协议"]
            .iter()
            .any(|marker| criterion.contains(marker))
    {
        return VerificationAssertionKind::Diagnostic;
    }
    if ["content", "contains", "equals", "match"]
        .iter()
        .any(|marker| lower.contains(marker))
        || ["内容", "包含", "等于", "匹配"]
            .iter()
            .any(|marker| criterion.contains(marker))
    {
        return VerificationAssertionKind::Diagnostic;
    }
    if ["file", "path", "diff", "write", "edit", "create"]
        .iter()
        .any(|marker| lower.contains(marker))
        || ["文件", "路径", "差异", "写入", "修改", "创建"]
            .iter()
            .any(|marker| criterion.contains(marker))
    {
        return VerificationAssertionKind::FileState;
    }
    VerificationAssertionKind::Delivery
}

fn assertion(
    criterion_id: &str,
    kind: VerificationAssertionKind,
    subject: &str,
    expected: &str,
    blocking: bool,
) -> VerificationAssertion {
    VerificationAssertion {
        assertion_id: golutra_core::VerificationAssertionId::new(),
        criterion_id: criterion_id.to_owned(),
        kind,
        subject: subject.to_owned(),
        expected: expected.to_owned(),
        verifier_id: "golutra-verifier/semantic".to_owned(),
        required_evidence_strength: if blocking { "medium" } else { "none" }.to_owned(),
        blocking,
        status: VerificationAssertionStatus::Pending,
        evidence_refs: Vec::new(),
        message: "assertion is pending execution facts".to_owned(),
    }
}

fn assertion_status(
    assertion: &VerificationAssertion,
    input: &VerificationInput,
    has_evidence: bool,
) -> (VerificationAssertionStatus, String, Vec<EvidenceId>) {
    let refs = input.evidence_refs.clone();
    let matching_checks = |kinds: &[VerificationCheckKind]| {
        input
            .command_checks
            .iter()
            .filter(|check| kinds.contains(&check.kind))
            .collect::<Vec<_>>()
    };
    match assertion.kind {
        VerificationAssertionKind::AssistantResponse => {
            if input
                .command_checks
                .iter()
                .any(|check| check.kind == VerificationCheckKind::AssistantResponse && check.passed)
                || (input.command_checks.is_empty() && !input.objective.trim().is_empty())
            {
                (
                    VerificationAssertionStatus::Pass,
                    "assistant response exists".to_owned(),
                    refs,
                )
            } else {
                (
                    VerificationAssertionStatus::Unknown,
                    "assistant response fact is missing".to_owned(),
                    refs,
                )
            }
        }
        VerificationAssertionKind::Diff | VerificationAssertionKind::FileState => {
            let checks = matching_checks(&[VerificationCheckKind::WorkspaceChange]);
            if checks
                .iter()
                .any(|check| check.passed && !check.evidence_refs.is_empty())
            {
                (
                    VerificationAssertionStatus::Pass,
                    "workspace change is linked to evidence".to_owned(),
                    checks
                        .iter()
                        .flat_map(|check| check.evidence_refs.iter().copied())
                        .collect(),
                )
            } else if checks.iter().any(|check| check.passed) {
                (
                    VerificationAssertionStatus::Unknown,
                    "workspace change has no linked evidence".to_owned(),
                    refs,
                )
            } else {
                (
                    VerificationAssertionStatus::Fail,
                    "workspace change was not recorded".to_owned(),
                    refs,
                )
            }
        }
        VerificationAssertionKind::Test | VerificationAssertionKind::Diagnostic => {
            let checks = matching_checks(&[VerificationCheckKind::ObjectiveValidation])
                .into_iter()
                .filter(|check| {
                    assertion.criterion_id == "tests_or_diagnostics"
                        || match assertion.kind {
                            VerificationAssertionKind::Test => {
                                check.name.starts_with("objective:test:")
                            }
                            VerificationAssertionKind::Diagnostic => {
                                !check.name.starts_with("objective:test:")
                            }
                            _ => false,
                        }
                })
                .collect::<Vec<_>>();
            if checks.iter().any(|check| !check.passed) {
                (
                    VerificationAssertionStatus::Fail,
                    "at least one objective validation failed".to_owned(),
                    refs,
                )
            } else if checks.iter().any(|check| check.passed) && has_evidence {
                (
                    VerificationAssertionStatus::Pass,
                    "objective validation passed with evidence".to_owned(),
                    checks
                        .iter()
                        .flat_map(|check| check.evidence_refs.iter().copied())
                        .collect(),
                )
            } else {
                (
                    VerificationAssertionStatus::Unknown,
                    "no objective validation fact was recorded".to_owned(),
                    refs,
                )
            }
        }
        VerificationAssertionKind::Delivery => {
            let matching_delivery_checks = input
                .command_checks
                .iter()
                .filter(|check| {
                    check.passed
                        && !check.evidence_refs.is_empty()
                        && delivery_check_matches(assertion, check)
                })
                .collect::<Vec<_>>();
            if has_evidence && !matching_delivery_checks.is_empty() {
                (
                    VerificationAssertionStatus::Pass,
                    "matching delivered evidence is available".to_owned(),
                    matching_delivery_checks
                        .iter()
                        .flat_map(|check| check.evidence_refs.iter().copied())
                        .collect(),
                )
            } else {
                (
                    VerificationAssertionStatus::Unknown,
                    "no matching delivered evidence is available".to_owned(),
                    refs,
                )
            }
        }
        VerificationAssertionKind::Policy => {
            let policy_failed = input
                .command_checks
                .iter()
                .any(|check| check.name.starts_with("policy:") && !check.passed);
            if policy_failed {
                (
                    VerificationAssertionStatus::Fail,
                    "a policy assertion failed".to_owned(),
                    refs,
                )
            } else {
                (
                    VerificationAssertionStatus::Pass,
                    "no blocking policy fact was recorded".to_owned(),
                    refs,
                )
            }
        }
        VerificationAssertionKind::CommandExit | VerificationAssertionKind::Schema => (
            VerificationAssertionStatus::NotApplicable,
            "assertion kind is not emitted by the current tool adapter".to_owned(),
            refs,
        ),
    }
}

fn delivery_check_matches(assertion: &VerificationAssertion, check: &VerificationCheck) -> bool {
    if assertion.criterion_id == "analysis_evidence" {
        return true;
    }
    let criterion = assertion.expected.trim().to_ascii_lowercase();
    let check_text = format!(
        "{} {} {}",
        check.name.to_ascii_lowercase(),
        check.message.to_ascii_lowercase(),
        check
            .command
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
    );
    let tokens = criterion
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|token| token.len() >= 3)
        .filter(|token| {
            !matches!(
                *token,
                "the"
                    | "and"
                    | "for"
                    | "with"
                    | "from"
                    | "that"
                    | "this"
                    | "exists"
                    | "available"
                    | "returned"
            )
        })
        .collect::<Vec<_>>();
    if tokens.is_empty()
        || tokens
            .iter()
            .all(|token| matches!(*token, "evidence" | "artifact" | "output" | "delivery"))
    {
        return true;
    }
    tokens.iter().any(|token| check_text.contains(token))
        || (!criterion.is_ascii() && check_text.contains(&criterion))
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
        let evidence = EvidenceId::new();
        let input = VerificationInput {
            task_id: TaskId::new(),
            objective: "read file".to_owned(),
            completion_criteria: vec!["evidence exists".to_owned()],
            evidence_refs: vec![evidence],
            command_checks: vec![VerificationCheck {
                kind: VerificationCheckKind::ToolExecution,
                name: "command".to_owned(),
                command: Some("true".to_owned()),
                passed: true,
                evidence_refs: vec![evidence],
                message: "ok".to_owned(),
            }],
            requires_workspace_evidence: false,
            code_files_changed: false,
        };
        let record = VerificationRunner.verify(input);

        assert_eq!(record.result, VerificationResult::Pass);
    }

    #[test]
    fn unrelated_evidence_does_not_satisfy_a_delivery_criterion() {
        let evidence = EvidenceId::new();
        let input = VerificationInput {
            task_id: TaskId::new(),
            objective: "verify the provider succeeds".to_owned(),
            completion_criteria: vec!["provider succeeds".to_owned()],
            evidence_refs: vec![evidence],
            command_checks: vec![VerificationCheck {
                kind: VerificationCheckKind::ToolExecution,
                name: "tool:read_file".to_owned(),
                command: None,
                passed: true,
                evidence_refs: vec![evidence],
                message: "README.md was read".to_owned(),
            }],
            requires_workspace_evidence: false,
            code_files_changed: false,
        };
        let plan = VerificationRunner.plan(&input);
        let (record, plan) = VerificationRunner.verify_with_plan(input, plan);

        assert_eq!(record.result, VerificationResult::Partial);
        assert!(plan.assertions.iter().any(|assertion| {
            assertion.criterion_id == "criterion-1"
                && assertion.status == VerificationAssertionStatus::Unknown
        }));
    }

    #[test]
    fn a_diagnostic_check_does_not_satisfy_an_explicit_test_criterion() {
        let evidence = EvidenceId::new();
        let input = VerificationInput {
            task_id: TaskId::new(),
            objective: "change code and run tests".to_owned(),
            completion_criteria: vec!["tests pass".to_owned()],
            evidence_refs: vec![evidence],
            command_checks: vec![
                VerificationCheck {
                    kind: VerificationCheckKind::WorkspaceChange,
                    name: "workspace_diff".to_owned(),
                    command: None,
                    passed: true,
                    evidence_refs: vec![evidence],
                    message: "code changed".to_owned(),
                },
                VerificationCheck {
                    kind: VerificationCheckKind::ObjectiveValidation,
                    name: "objective:diagnostic:shell".to_owned(),
                    command: Some("cargo check".to_owned()),
                    passed: true,
                    evidence_refs: vec![evidence],
                    message: "diagnostic command passed".to_owned(),
                },
            ],
            requires_workspace_evidence: true,
            code_files_changed: true,
        };
        let plan = VerificationRunner.plan(&input);
        let (record, plan) = VerificationRunner.verify_with_plan(input, plan);

        assert_eq!(record.result, VerificationResult::Partial);
        assert!(plan.assertions.iter().any(|assertion| {
            assertion.criterion_id == "criterion-1"
                && assertion.kind == VerificationAssertionKind::Test
                && assertion.status == VerificationAssertionStatus::Unknown
        }));
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

    #[test]
    fn every_completion_criterion_gets_a_blocking_assertion() {
        let input = VerificationInput {
            task_id: TaskId::new(),
            objective: "write result.txt".to_owned(),
            completion_criteria: vec![
                "result.txt exists".to_owned(),
                "result.txt contains done".to_owned(),
                "tests pass".to_owned(),
            ],
            evidence_refs: vec![EvidenceId::new()],
            command_checks: Vec::new(),
            requires_workspace_evidence: true,
            code_files_changed: false,
        };
        let plan = VerificationRunner.plan(&input);

        for index in 1..=input.completion_criteria.len() {
            assert!(plan.assertions.iter().any(|assertion| {
                assertion.criterion_id == format!("criterion-{index}") && assertion.blocking
            }));
        }
    }

    #[test]
    fn a_failed_objective_check_wins_over_another_passing_check() {
        let evidence = EvidenceId::new();
        let record = VerificationRunner.verify(VerificationInput {
            task_id: TaskId::new(),
            objective: "write result.txt with content done".to_owned(),
            completion_criteria: vec!["result.txt contains done".to_owned()],
            evidence_refs: vec![evidence],
            command_checks: vec![
                VerificationCheck {
                    kind: VerificationCheckKind::WorkspaceChange,
                    name: "workspace_diff".to_owned(),
                    command: None,
                    passed: true,
                    evidence_refs: vec![evidence],
                    message: "file changed".to_owned(),
                },
                VerificationCheck {
                    kind: VerificationCheckKind::ObjectiveValidation,
                    name: "objective:path".to_owned(),
                    command: None,
                    passed: true,
                    evidence_refs: vec![evidence],
                    message: "path matches".to_owned(),
                },
                VerificationCheck {
                    kind: VerificationCheckKind::ObjectiveValidation,
                    name: "objective:content".to_owned(),
                    command: None,
                    passed: false,
                    evidence_refs: vec![evidence],
                    message: "content mismatch".to_owned(),
                },
            ],
            requires_workspace_evidence: true,
            code_files_changed: false,
        });

        assert_eq!(record.result, VerificationResult::Fail);
    }
}
