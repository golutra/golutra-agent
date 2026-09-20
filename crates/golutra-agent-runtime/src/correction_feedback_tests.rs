//! 纠偏上下文只包含未解决、可追溯的事实，并保护秘密和模型输入预算。
use super::*;
use crate::correction_feedback::model_instruction;

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
