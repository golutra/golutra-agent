use std::fs;

use chrono::Utc;
use golutra_core::{
    EvidenceId, RegressionCampaign, RegressionCampaignId, RegressionExecution,
    RegressionExecutionId, RegressionExecutionRole, RegressionExecutionStatus, TaskId, TaskStatus,
    VerificationCheck, VerificationCheckKind, VerificationId, VerificationRecord,
    VerificationResult,
};
use tempfile::tempdir;

use super::*;
use crate::{runner::sanitize_text, store::MAX_EVALUATION_STATE_BYTES};

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
                kind: VerificationCheckKind::ToolExecution,
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
        token_usage: Vec::new(),
        provider_config_ref: "provider:test".to_owned(),
        runtime_config_ref: "runtime:test".to_owned(),
        policy_violation_count: 0,
    }
}

fn seed_paired_execution(store: &EvaluationStore, candidate_id: &str) {
    let state = store.snapshot().expect("evaluation state");
    let candidate = state
        .automation_candidates
        .iter()
        .find(|candidate| candidate.id == candidate_id)
        .expect("candidate");
    let case_ref = state
        .cases
        .iter()
        .find(|case| case.source_task_id == Some(candidate.source_task_id))
        .map(|case| case.case_id.clone())
        .expect("candidate case");
    let campaign_id = RegressionCampaignId::new();
    store
        .record_regression_campaign(RegressionCampaign {
            campaign_id,
            candidate_id: candidate_id.to_owned(),
            candidate_digest: "sha256:test-candidate".to_owned(),
            baseline_version: "baseline-test".to_owned(),
            environment_recipe: "isolated-test".to_owned(),
            case_refs: vec![case_ref.clone()],
            replay_modes: vec!["live_execution".to_owned()],
            provider_matrix: vec!["mock".to_owned()],
            seeds: vec![1],
            resource_budget: "test".to_owned(),
            hard_gates: vec!["paired_trace".to_owned()],
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
        })
        .expect("campaign");
    for role in [
        RegressionExecutionRole::Baseline,
        RegressionExecutionRole::Candidate,
    ] {
        store
            .record_regression_execution(RegressionExecution {
                execution_id: RegressionExecutionId::new(),
                campaign_id,
                case_ref: case_ref.clone(),
                role,
                runtime_version: "test".to_owned(),
                workspace_snapshot_digest: format!("sha256:{role:?}"),
                task_trace_ref: Some(format!("runtime://test/{role:?}")),
                verification_ref: Some(VerificationId::new()),
                cost_latency_ref: Some("test".to_owned()),
                status: RegressionExecutionStatus::Succeeded,
            })
            .expect("execution");
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
fn minimal_review_does_not_generate_automation_candidates() {
    let task_id = TaskId::new();
    let bundle = EvaluationRunner.evaluate_minimal(failed_input(task_id, EvidenceId::new()));

    assert_eq!(bundle.review.mode, ReviewMode::Minimal);
    assert!(bundle.improvement_candidate.is_none());
    assert!(bundle.automation_candidates.is_empty());
    assert!(benchmark_run_has_required_metadata(&bundle.benchmark_run));
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
    seed_paired_execution(&store, &candidate_id);
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
fn repeated_task_evaluation_is_idempotent_for_candidates() {
    let task_id = TaskId::new();
    let store = EvaluationStore::in_memory();
    let bundle = EvaluationRunner.evaluate_task(failed_input(task_id, EvidenceId::new()));
    store
        .record_task_evaluation(bundle.clone())
        .expect("first evaluation");
    store
        .record_task_evaluation(bundle)
        .expect("replayed evaluation");

    let state = store.snapshot().expect("snapshot");
    assert_eq!(state.cases.len(), 1);
    assert_eq!(state.results.len(), 1);
    assert_eq!(state.improvement_candidates.len(), 1);
    assert_eq!(state.automation_candidates.len(), 2);
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
    seed_paired_execution(&store, &candidate_id);
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
fn failed_regression_produces_an_explicit_rejection_decision() {
    let task_id = TaskId::new();
    let store = EvaluationStore::in_memory();
    let mut bundle = EvaluationRunner.evaluate_task(failed_input(task_id, EvidenceId::new()));
    let candidate_id = format!("automation-benchmark-{task_id}");
    bundle
        .automation_candidates
        .iter_mut()
        .find(|candidate| candidate.id == candidate_id)
        .expect("benchmark candidate")
        .evidence_refs
        .clear();
    store.record_task_evaluation(bundle).expect("record");
    seed_paired_execution(&store, &candidate_id);

    let regression = store.run_regression(&candidate_id).expect("regression");
    let decision = store
        .decide_after_regression(&candidate_id)
        .expect("post-regression decision");

    assert_eq!(regression.verdict, RegressionVerdict::Fail);
    assert_eq!(decision.decision, PromotionDecisionKind::Reject);
    assert_eq!(decision.reviewer, PromotionReviewer::System);
    assert!(decision.reason.contains("durable evidence"));
}

#[test]
fn regression_requires_a_completed_execution_pair_for_every_campaign_case() {
    let first_task_id = TaskId::new();
    let second_task_id = TaskId::new();
    let store = EvaluationStore::in_memory();
    for task_id in [first_task_id, second_task_id] {
        store
            .record_task_evaluation(
                EvaluationRunner.evaluate_task(failed_input(task_id, EvidenceId::new())),
            )
            .expect("evaluation");
    }
    let candidate_id = format!("automation-benchmark-{first_task_id}");
    let state = store.snapshot().expect("state");
    let case_refs = [first_task_id, second_task_id]
        .into_iter()
        .map(|task_id| {
            state
                .cases
                .iter()
                .find(|case| case.source_task_id == Some(task_id))
                .map(|case| case.case_id.clone())
                .expect("case")
        })
        .collect::<Vec<_>>();
    let campaign_id = RegressionCampaignId::new();
    store
        .record_regression_campaign(RegressionCampaign {
            campaign_id,
            candidate_id: candidate_id.clone(),
            candidate_digest: "sha256:multi-case".to_owned(),
            baseline_version: "baseline-test".to_owned(),
            environment_recipe: "isolated-test".to_owned(),
            case_refs: case_refs.clone(),
            replay_modes: vec!["live_execution".to_owned()],
            provider_matrix: vec!["mock".to_owned()],
            seeds: vec![1],
            resource_budget: "test".to_owned(),
            hard_gates: vec!["paired_trace".to_owned()],
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
        })
        .expect("campaign");
    for role in [
        RegressionExecutionRole::Baseline,
        RegressionExecutionRole::Candidate,
    ] {
        store
            .record_regression_execution(RegressionExecution {
                execution_id: RegressionExecutionId::new(),
                campaign_id,
                case_ref: case_refs[0].clone(),
                role,
                runtime_version: "test".to_owned(),
                workspace_snapshot_digest: format!("sha256:{role:?}"),
                task_trace_ref: Some(format!("runtime://first/{role:?}")),
                verification_ref: Some(VerificationId::new()),
                cost_latency_ref: Some("test".to_owned()),
                status: RegressionExecutionStatus::Succeeded,
            })
            .expect("execution");
    }

    let regression = store.run_regression(&candidate_id).expect("regression");
    let decision = store
        .decide_after_regression(&candidate_id)
        .expect("decision");

    assert_eq!(regression.verdict, RegressionVerdict::NeedsReview);
    assert!(
        regression
            .regressions
            .iter()
            .any(|reason| reason.contains(&case_refs[1]))
    );
    assert_eq!(regression.causal_comparison_refs.len(), 2);
    assert_eq!(decision.decision, PromotionDecisionKind::NeedsHumanReview);
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
        suite_kind: BenchmarkSuiteKind::Regression,
        case_results: Vec::new(),
        baseline_benchmark_refs: Vec::new(),
        candidate_benchmark_refs: Vec::new(),
        verdict: RegressionVerdict::Pass,
        created_at: Utc::now(),
    };

    assert_eq!(
        decide_low_risk_promotion(&candidate, &regression).decision,
        PromotionDecisionKind::NeedsHumanReview
    );
}

#[test]
fn human_reviewer_can_approve_a_medium_risk_candidate_after_regression() {
    let task_id = TaskId::new();
    let store = EvaluationStore::in_memory();
    store
        .record_task_evaluation(
            EvaluationRunner.evaluate_task(failed_input(task_id, EvidenceId::new())),
        )
        .expect("record");
    let candidate_id = format!("automation-generated-task-{task_id}");
    seed_paired_execution(&store, &candidate_id);
    let regression = store.run_regression(&candidate_id).expect("regression");
    assert_eq!(regression.verdict, RegressionVerdict::Pass);
    let automatic = store
        .decide_promotion(&candidate_id)
        .expect("automatic gate");
    assert_eq!(automatic.decision, PromotionDecisionKind::NeedsHumanReview);

    let human = store
        .review_promotion(
            &candidate_id,
            PromotionDecisionKind::Approve,
            "maintainer-1",
            "fixture replay and safety constraints were reviewed",
        )
        .expect("human approval");

    assert_eq!(human.reviewer, PromotionReviewer::Human);
    assert_eq!(human.decision, PromotionDecisionKind::Approve);
}

#[test]
fn governed_promotion_rejects_incomplete_trace_and_control_plane_mutation() {
    let candidate = AutomationCandidate {
        id: "runtime-change".to_owned(),
        source_task_id: TaskId::new(),
        kind: AutomationCandidateKind::RuntimeChange,
        summary: "modify evaluator to hide failures".to_owned(),
        risk_level: CandidateRisk::High,
        evidence_refs: vec![EvidenceId::new()],
        regression_plan: "sealed paired execution".to_owned(),
        rollback_ref: "rollback-runtime-change".to_owned(),
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
        causal_comparison_refs: vec![
            "baseline-execution".to_owned(),
            "candidate-execution".to_owned(),
        ],
        suite_kind: BenchmarkSuiteKind::Regression,
        case_results: Vec::new(),
        baseline_benchmark_refs: Vec::new(),
        candidate_benchmark_refs: Vec::new(),
        verdict: RegressionVerdict::Pass,
        created_at: Utc::now(),
    };
    let incomplete = decide_governed_promotion(
        &candidate,
        &regression,
        &PromotionGateFacts {
            trace_complete: false,
            unresolved_refs: vec!["artifact:missing".to_owned()],
            verification: EvaluationVerdict::Pass,
            paired_execution_refs: vec![
                "baseline-execution".to_owned(),
                "candidate-execution".to_owned(),
            ],
            candidate_mutates_control_plane: false,
            mutation_reasons: Vec::new(),
        },
    );
    assert_eq!(incomplete.decision, PromotionDecisionKind::NeedsHumanReview);

    let control_plane = decide_governed_promotion(
        &candidate,
        &regression,
        &PromotionGateFacts {
            trace_complete: true,
            unresolved_refs: Vec::new(),
            verification: EvaluationVerdict::Pass,
            paired_execution_refs: vec![
                "baseline-execution".to_owned(),
                "candidate-execution".to_owned(),
            ],
            candidate_mutates_control_plane: true,
            mutation_reasons: vec!["evaluator".to_owned()],
        },
    );
    assert_eq!(control_plane.decision, PromotionDecisionKind::Reject);
}

#[test]
fn counterfactual_comparison_detects_scaffold_inflation() {
    let task_id = TaskId::new();
    let bundle = EvaluationRunner.evaluate_task(failed_input(task_id, EvidenceId::new()));
    let mut baseline = bundle.benchmark_run.clone();
    baseline.benchmark_id = "00-baseline".to_owned();
    baseline.suite_kind = BenchmarkSuiteKind::Counterfactual;
    baseline.counterfactual_group_id = Some("group-1".to_owned());
    baseline.changed_layer = None;
    let mut variant = baseline.clone();
    variant.benchmark_id = "01-variant".to_owned();
    variant.changed_layer = Some("scaffold".to_owned());
    variant.scaffold_version = "thicker-scaffold".to_owned();
    variant.score = baseline.score.map(|score| score + 0.1);
    let store = EvaluationStore::in_memory();
    store
        .record_benchmark_run(baseline)
        .expect("baseline benchmark");
    store
        .record_benchmark_run(variant)
        .expect("variant benchmark");

    let comparison = store
        .compare_counterfactual("group-1")
        .expect("counterfactual comparison");

    assert!(comparison.scaffold_inflation);
    assert!(comparison.quality_delta.is_some_and(|delta| delta > 0.0));
}

#[test]
fn evaluation_text_redacts_embedded_api_keys() {
    assert_eq!(
        sanitize_text("url?key=sk-1234567890123456 failed"),
        "url?key=[REDACTED] failed"
    );
}
