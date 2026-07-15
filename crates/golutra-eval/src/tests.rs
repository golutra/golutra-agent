use golutra_core::{VerificationCheck, VerificationId};
use tempfile::tempdir;

use super::*;

fn failed_input(task_id: TaskId, evidence_id: EvidenceId) -> TaskEvaluationInput {
    TaskEvaluationInput {
        task_id,
        objective: "fix provider failure".to_owned(),
        task_status: TaskStatus::Failed,
        verification: Some(VerificationRecord {
            verification_id: VerificationId::new(),
            task_id,
            objective: "fix provider failure".to_owned(),
            completion_criteria: vec!["provider succeeds".to_owned()],
            checks: vec![VerificationCheck {
                name: "provider".to_owned(),
                command: None,
                passed: false,
                evidence_refs: vec![evidence_id],
                message: "provider failed".to_owned(),
            }],
            evidence_refs: vec![evidence_id],
            result: VerificationResult::Fail,
            policy_status: "allowed".to_owned(),
            residual_risks: vec!["provider request failed".to_owned()],
        }),
        event_count: 8,
        artifact_count: 1,
        tool_count: 1,
        latency_ms: Some(20),
        failure_summary: Some("provider failed after tool execution".to_owned()),
    }
}

#[test]
fn oversized_evaluation_state_is_rejected_before_deserialization() {
    let directory = tempdir().expect("directory");
    let path = directory.path().join("evaluation.json");
    let file = fs::File::create(&path).expect("state file");
    file.set_len(MAX_EVALUATION_STATE_BYTES + 1)
        .expect("oversized fixture");
    let store = EvaluationStore::new(path);

    assert!(matches!(store.snapshot(), Err(EvaluationError::Limit(_))));
}

#[test]
fn failed_task_generates_review_and_proposed_candidates() {
    let task_id = TaskId::new();
    let bundle = EvaluationRunner.evaluate_task(failed_input(task_id, EvidenceId::new()));

    assert_eq!(bundle.result.verdict, EvaluationVerdict::Fail);
    assert!(bundle.improvement_candidate.is_some());
    assert!(bundle.generated_task.is_some());
    assert!(bundle.benchmark_promotion.is_some());
    assert!(
        bundle
            .automation_candidates
            .iter()
            .all(|candidate| candidate.status == CandidateStatus::Proposed)
    );
}

#[test]
fn evaluation_store_is_durable_and_requires_regression_before_apply() {
    let directory = tempdir().expect("directory");
    let path = directory.path().join("evaluation.json");
    let task_id = TaskId::new();
    let store = EvaluationStore::new(&path);
    store
        .record_task_evaluation(
            EvaluationRunner.evaluate_task(failed_input(task_id, EvidenceId::new())),
        )
        .expect("record evaluation");
    let candidate_id = format!("automation-benchmark-{task_id}");

    assert!(matches!(
        store.apply_candidate(&candidate_id),
        Err(EvaluationError::PromotionRequired(_))
    ));
    let regression = store.run_regression(&candidate_id).expect("run regression");
    let decision = store
        .decide_promotion(&candidate_id)
        .expect("promotion decision");
    let applied = store.apply_candidate(&candidate_id).expect("apply");

    assert_eq!(regression.verdict, RegressionVerdict::Pass);
    assert_eq!(decision.decision, PromotionDecisionKind::Approve);
    assert_eq!(applied.candidate_id, candidate_id);
    assert_eq!(
        EvaluationStore::new(&path)
            .snapshot()
            .expect("reopen")
            .applied_candidates
            .len(),
        1
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = fs::metadata(&path)
            .expect("evaluation file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn independent_evaluation_instances_refresh_before_writing() {
    let directory = tempdir().expect("directory");
    let path = directory.path().join("evaluation.json");
    let first = EvaluationStore::new(&path);
    let second = EvaluationStore::new(&path);
    assert!(
        second
            .snapshot()
            .expect("initial snapshot")
            .results
            .is_empty()
    );

    first
        .record_task_evaluation(
            EvaluationRunner.evaluate_task(failed_input(TaskId::new(), EvidenceId::new())),
        )
        .expect("first evaluation");
    second
        .record_task_evaluation(
            EvaluationRunner.evaluate_task(failed_input(TaskId::new(), EvidenceId::new())),
        )
        .expect("second evaluation");

    assert_eq!(first.snapshot().expect("shared snapshot").results.len(), 2);
    assert!(path.with_extension("lock").exists());
}

#[test]
fn applied_benchmark_can_be_rolled_back() {
    let task_id = TaskId::new();
    let store = EvaluationStore::in_memory();
    store
        .record_task_evaluation(
            EvaluationRunner.evaluate_task(failed_input(task_id, EvidenceId::new())),
        )
        .expect("record");
    let candidate_id = format!("automation-benchmark-{task_id}");
    store.run_regression(&candidate_id).expect("regression");
    store.decide_promotion(&candidate_id).expect("decision");
    store.apply_candidate(&candidate_id).expect("apply");

    let rolled_back = store
        .rollback_candidate(&candidate_id, "superseded")
        .expect("rollback");

    assert!(rolled_back.rolled_back_at.is_some());
    assert_eq!(
        store.snapshot().expect("snapshot").automation_candidates[0].status,
        CandidateStatus::RolledBack
    );
}

#[test]
fn medium_risk_skill_requires_human_review() {
    let candidate = AutomationCandidate {
        id: "skill".to_owned(),
        source_task_id: TaskId::new(),
        kind: AutomationCandidateKind::Skill,
        summary: "skill".to_owned(),
        risk_level: CandidateRisk::Medium,
        evidence_refs: vec![EvidenceId::new()],
        regression_plan: "replay".to_owned(),
        rollback_ref: "remove-skill".to_owned(),
        status: CandidateStatus::RegressionPassed,
    };
    let regression = RegressionResult {
        regression_id: "regression".to_owned(),
        candidate_id: candidate.id.clone(),
        baseline_version: "base".to_owned(),
        candidate_version: "candidate".to_owned(),
        cases_run: 1,
        passed_cases: 1,
        failed_cases: 0,
        regressions: Vec::new(),
        cost_delta: Some(0.0),
        latency_delta: Some(0),
        quality_delta: Some(0.0),
        security_delta: Some(0.0),
        causal_comparison_refs: vec!["replay".to_owned()],
        verdict: RegressionVerdict::Pass,
        created_at: Utc::now(),
    };

    assert_eq!(
        decide_low_risk_promotion(&candidate, &regression).decision,
        PromotionDecisionKind::NeedsHumanReview
    );
}

#[test]
fn evaluation_text_redacts_embedded_api_keys() {
    assert_eq!(
        sanitize_text("url?key=sk-1234567890123456 failed"),
        "url?key=[REDACTED] failed"
    );
}
