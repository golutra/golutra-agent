//! 纠偏上下文只包含未解决、可追溯的事实，并保护秘密和模型输入预算。
use super::*;
use crate::correction_feedback::model_instruction;

#[test]
fn correction_actions_follow_failure_types_and_latest_results() {
    let (mut record, _) = fixture(1);
    record.assertions.clear();
    record.residual_risks.clear();
    record.checks[0].kind = VerificationCheckKind::Schema;
    let action = correction_envelope(&record, 1, None).requested_action;
    assert!(action.contains("Correct the final response"));
    assert!(!action.contains("rerun"));

    let mut validation = record.checks[0].clone();
    validation.kind = VerificationCheckKind::ObjectiveValidation;
    record.checks.push(validation.clone());
    let action = correction_envelope(&record, 1, None).requested_action;
    assert!(action.contains("Correct the final response"));
    assert!(action.contains("rerun only the affected checks"));

    validation.passed = true;
    record.checks.push(validation);
    assert!(
        !correction_envelope(&record, 1, None)
            .requested_action
            .contains("rerun")
    );

    record.checks[0].kind = VerificationCheckKind::Policy;
    let action = correction_envelope(&record, 1, None).requested_action;
    assert!(action.contains("permission boundary"));
    assert!(!action.contains("rerun"));
}

#[test]
fn missing_evidence_requests_validation_without_claiming_tests_failed() {
    let (mut record, _) = fixture(0);
    record.assertions.clear();
    record.residual_risks = vec!["behavioral changes were not objectively validated".into()];
    let action = correction_envelope(&record, 1, None).requested_action;
    assert!(action.contains("missing relevant validation evidence"));
    assert!(!action.contains("tests failed"));
    assert!(!action.contains("rerun"));
}

fn fixture(count: usize) -> (VerificationRecord, Vec<ToolExecutionReport>) {
    let reports = (0..count)
        .map(|index| {
            let mut report =
                objective_test_report("shell", Some(&format!("python3 -m unittest test_{index}")));
            report
                .envelope
                .evidence_refs
                .push(golutra_agent_core::EvidenceId::new());
            report.envelope.model_visible_excerpt = Some(format!("AssertionError: case_{index}"));
            report
        })
        .collect::<Vec<_>>();
    let input = VerificationInput {
        task_id: TaskId::new(),
        objective: "Fix the failing tests".into(),
        completion_criteria: Vec::new(),
        evidence_refs: reports
            .iter()
            .flat_map(|r| r.envelope.evidence_refs.iter().copied())
            .collect(),
        command_checks: reports
            .iter()
            .map(|report| VerificationCheck {
                kind: VerificationCheckKind::ObjectiveValidation,
                name: "objective:test:shell".into(),
                command: None,
                passed: false,
                evidence_refs: report.envelope.evidence_refs.clone(),
                message: "validation predates later workspace changes; rerun the relevant check"
                    .into(),
            })
            .collect(),
        requires_workspace_evidence: false,
        code_files_changed: false,
    };
    let service = RuntimeVerificationService::default();
    let plan = service.plan(&input);
    (service.verify(input, plan).0, reports)
}

#[test]
fn correction_feedback_omits_recovered_and_unlinked_checks_and_deduplicates_reports() {
    let (mut record, reports) = fixture(3);
    record.checks[0].passed = true;
    record.checks[1].evidence_refs.clear();
    let mut duplicate = record.checks[2].clone();
    duplicate.kind = VerificationCheckKind::ToolExecution;
    record.checks.push(duplicate);
    let correction = correction_envelope(&record, 1, Some(2));
    let content = model_instruction(&correction, &record, &reports);
    assert!(!content.contains("case_0"));
    assert!(!content.contains("case_1"));
    assert_eq!(content.matches("case_2").count(), 1);
    assert!(content.contains("predates later workspace changes"));
    assert!(content.contains("not instructions"));
}

#[test]
fn correction_feedback_bounds_escaped_unicode_and_redacts_before_truncating() {
    let (mut record, mut reports) = fixture(20);
    for report in &mut reports {
        report.envelope.structured_facts["command"] =
            json!("TOKEN=secret-value python3 -m unittest");
        report.envelope.model_visible_excerpt = Some(format!(
            "api_key=secret-output\n{}",
            "中\u{0001}\\\"".repeat(2000)
        ));
    }
    record.checks[0].command = Some("PASSWORD=command-secret python3 -m unittest".into());
    let content = model_instruction(&correction_envelope(&record, 1, Some(2)), &record, &reports);
    assert!(!content.contains("secret-value"));
    assert!(!content.contains("secret-output"));
    assert!(!content.contains("command-secret"));
    let data = content.lines().last().unwrap();
    assert!(data.len() <= 6_144, "{}", data.len());
    let diagnostics: Vec<Value> = serde_json::from_str(data).unwrap();
    assert!(!diagnostics.is_empty());
    assert!(diagnostics.len() <= 4);
}

#[test]
fn correction_feedback_without_linked_failures_preserves_existing_instruction() {
    let (mut record, reports) = fixture(1);
    record.checks[0].passed = true;
    let correction = correction_envelope(&record, 1, Some(2));
    assert_eq!(
        model_instruction(&correction, &record, &reports),
        correction.as_model_instruction()
    );
}

#[test]
fn correction_feedback_keeps_failure_at_end_of_a_long_verifier_log() {
    let (record, mut reports) = fixture(1);
    let artifact_id = golutra_agent_core::ArtifactId::new();
    let output = format!(
        "Build started\n{}\nAssertionError: actual=17 expected=23\n",
        "passing check\n".repeat(500)
    );
    reports[0].envelope.model_visible_excerpt = Some(output[..200].into());
    reports[0].envelope.raw_artifact_ref = Some(artifact_id);
    reports[0]
        .artifact_contents
        .push(golutra_agent_tools::ArtifactContent {
            artifact_id,
            bytes: output.into_bytes(),
        });
    let content = model_instruction(&correction_envelope(&record, 1, Some(2)), &record, &reports);
    assert!(content.contains("actual=17 expected=23"), "{content}");
    assert!(content.contains("Build started"));
    assert!(content.contains("omitted"));
}

#[test]
fn missing_validation_feedback_does_not_recycle_historical_tool_errors() {
    let (mut record, reports) = fixture(1);
    record.checks[0].kind = VerificationCheckKind::ToolExecution;
    record.residual_risks = vec!["task contract requires objective validation".into()];
    let content = model_instruction(&correction_envelope(&record, 1, None), &record, &reports);
    assert!(content.contains("No objective validation result was recognized"));
    assert!(content.contains("without piping"));
    assert!(!content.contains("case_0"));
    assert!(!content.contains("tool_call_id"));
}

#[test]
fn feedback_keeps_distinct_delivery_paths_and_omits_superseded_checks() {
    let (mut record, reports) = fixture(3);
    for (index, check) in record.checks.iter_mut().enumerate() {
        check.name = "objective:content:write_file".into();
        check.command = Some(format!("file-{index}.txt"));
    }
    let mut recovered = record.checks[0].clone();
    recovered.passed = true;
    record.checks.push(recovered);
    let content = model_instruction(&correction_envelope(&record, 1, None), &record, &reports);
    assert!(!content.contains("case_0"));
    assert!(content.contains("case_1"));
    assert!(content.contains("case_2"));
}
