use std::{collections::BTreeMap, fs};

use chrono::Utc;
use golutra_core::{
    ArtifactId, EvidenceId, RegressionCampaign, RegressionCampaignId, RegressionExecution,
    RegressionExecutionId, RegressionExecutionRole, RegressionExecutionStatus, RunId, TaskId,
    TaskStatus, VerificationCheck, VerificationCheckKind, VerificationId, VerificationRecord,
    VerificationResult,
};
use tempfile::tempdir;

use super::*;
use crate::{runner::sanitize_text, store::MAX_EVALUATION_STATE_BYTES};

fn replay_projection_fixture(task_id: TaskId, suffix: &str) -> (ReplayCapsule, ReplayExecution) {
    let now = Utc::now();
    let capsule = ReplayCapsule {
        capsule_id: format!("capsule-{suffix}"),
        source_task_id: task_id,
        source_run_id: RunId::from(task_id),
        mode: ReplayMode::DeterministicControlFlow,
        provider_exchanges: Vec::new(),
        tool_results: Vec::new(),
        clock_seed: "2026-08-13T00:00:00Z".to_owned(),
        random_seed: 7,
        runtime_config_digest: "sha256:test-runtime".to_owned(),
        fixture_ref: None,
        event_chain_digest: "sha256:test-events".to_owned(),
        source_last_sequence_no: None,
        complete: false,
        missing_inputs: vec!["fixture input is unavailable".to_owned()],
        limitations: Vec::new(),
        created_at: now,
    };
    let execution = ReplayExecution {
        execution_id: format!("execution-{suffix}"),
        capsule_id: capsule.capsule_id.clone(),
        source_task_id: task_id,
        mode: capsule.mode,
        status: ReplayExecutionStatus::Incomplete,
        provider_exchanges_total: 0,
        provider_exchanges_consumed: 0,
        tool_results_total: 0,
        tool_results_consumed: 0,
        expected_loop_action: None,
        observed_loop_action: None,
        expected_verification: None,
        observed_verification: None,
        mismatches: Vec::new(),
        started_at: now,
        completed_at: now,
    };
    (capsule, execution)
}

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
            plan_id: None,
            assertions: Vec::new(),
            source: Default::default(),
            independence: Default::default(),
            environment_digest: None,
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
        trajectory_summary: TrajectorySummary::default(),
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
            candidate_artifact_ref: None,
            baseline_version: "baseline-test".to_owned(),
            environment_recipe: "isolated-test".to_owned(),
            case_refs: vec![case_ref.clone()],
            case_partitions: BTreeMap::from([(case_ref.clone(), EvaluationPartitionKind::Source)]),
            required_partitions: vec![EvaluationPartitionKind::Source],
            replay_modes: vec!["live_execution".to_owned()],
            provider_matrix: vec!["mock".to_owned()],
            seeds: vec![1],
            minimum_trusted_external_pairs: 0,
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
                partition: EvaluationPartitionKind::Source,
                provider_variant: "mock".to_owned(),
                seed: 1,
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

fn complete_coverage() -> RegressionCoverage {
    RegressionCoverage {
        required_partitions: vec![EvaluationPartitionKind::Source],
        observed_partitions: vec![EvaluationPartitionKind::Source],
        required_providers: vec!["mock".to_owned()],
        observed_providers: vec!["mock".to_owned()],
        required_seeds: vec![1],
        observed_seeds: vec![1],
        expected_cells: 1,
        completed_cells: 1,
        ..RegressionCoverage::default()
    }
}

#[test]
fn trajectory_summary_surfaces_context_and_repeated_tool_failures() {
    let task_id = TaskId::new();
    let mut input = failed_input(task_id, EvidenceId::new());
    input.trajectory_summary = TrajectorySummary {
        provider_calls: 21,
        tool_calls: 21,
        failed_tool_calls: 12,
        initial_context_tokens: Some(45),
        final_context_tokens: Some(5_314),
        max_context_tokens: Some(5_314),
        context_growth_tokens: 5_269,
        context_pressure: true,
        workspace_changes_observed: false,
        failure_clusters: vec![TrajectoryFailureCluster {
            family: "dependency_install:apt".to_owned(),
            failures: 4,
            duration_ms: 120_000,
            output_bytes: 8_000,
        }],
        ..TrajectorySummary::default()
    };

    let bundle = EvaluationRunner.evaluate_task(input);

    assert!(!bundle.review.context_issues.is_empty());
    assert!(
        bundle
            .review
            .tool_issues
            .iter()
            .any(|issue| issue.contains("dependency_install:apt"))
    );
    assert!(
        bundle
            .review
            .suggested_improvements
            .iter()
            .any(|improvement| improvement.contains("compact superseded tool results"))
    );
    assert!(
        bundle
            .review
            .suggested_improvements
            .iter()
            .any(|improvement| improvement.contains("materially different strategy"))
    );
}

#[allow(clippy::too_many_arguments)]
fn external_evaluation(
    task_id: TaskId,
    evaluation_id: &str,
    case_id: &str,
    campaign_id: RegressionCampaignId,
    candidate_id: &str,
    role: RegressionExecutionRole,
    partition: EvaluationPartitionKind,
    provider_variant: &str,
    seed: u64,
    trust: ExternalEvaluationTrust,
) -> ExternalEvaluationRecord {
    let mut record = ExternalEvaluationRecord {
        evaluation_id: evaluation_id.to_owned(),
        source_task_id: task_id,
        evaluator_id: "test-evaluator".to_owned(),
        evaluator_version: "1".to_owned(),
        harness_id: "test-harness".to_owned(),
        harness_version: "1".to_owned(),
        dataset_id: "test-dataset".to_owned(),
        dataset_version: "1".to_owned(),
        case_id: case_id.to_owned(),
        verdict: EvaluationVerdict::Pass,
        score: Some(if role == RegressionExecutionRole::Candidate {
            0.9
        } else {
            0.8
        }),
        score_max: Some(1.0),
        assertions: Vec::new(),
        phases: Vec::new(),
        terminal_cause: None,
        artifact_refs: Vec::new(),
        imported_artifacts: Vec::new(),
        imported_evidence_refs: Vec::new(),
        partition,
        seed: Some(seed),
        provider_variant: Some(provider_variant.to_owned()),
        holdout_protected: false,
        comparison_group_id: Some("external-pair".to_owned()),
        candidate_id: Some(candidate_id.to_owned()),
        campaign_id: Some(campaign_id),
        role: Some(role),
        base_trace_digest: "sha256:base-trace".to_owned(),
        runtime_identity: "runtime:test".to_owned(),
        result_digest: String::new(),
        trust,
        attestation: None,
        ingested_at: Utc::now(),
    };
    record.result_digest = external_evaluation_result_digest(&record);
    record
}

#[test]
fn external_evaluation_rejects_inconsistent_phase_outcomes() {
    let task_id = TaskId::new();
    let mut record = external_evaluation(
        task_id,
        "phase-invalid",
        "case",
        RegressionCampaignId::new(),
        "candidate",
        RegressionExecutionRole::Candidate,
        EvaluationPartitionKind::Source,
        "mock",
        1,
        ExternalEvaluationTrust::OwnerLocal,
    );
    let now = Utc::now();
    record.phases = vec![ExternalEvaluationPhase {
        phase_id: "test".to_owned(),
        kind: ExternalEvaluationPhaseKind::Test,
        status: ExternalEvaluationPhaseStatus::Failed,
        started_at: Some(now),
        completed_at: Some(now),
        duration_ms: Some(0),
        assertion_refs: Vec::new(),
        evidence_refs: Vec::new(),
    }];
    record.result_digest = external_evaluation_result_digest(&record);

    assert!(matches!(
        EvaluationStore::in_memory().record_external_evaluation(record),
        Err(EvaluationError::Invariant(_))
    ));
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
    assert_eq!(state.automation_candidates.len(), 3);
}

#[test]
fn replay_projection_replacement_is_task_scoped_and_removes_stale_records() {
    let target_task_id = TaskId::new();
    let other_task_id = TaskId::new();
    let store = EvaluationStore::in_memory();
    let (stale_capsule, stale_execution) =
        replay_projection_fixture(target_task_id, "target-stale");
    let (canonical_capsule, canonical_execution) =
        replay_projection_fixture(target_task_id, "target-canonical");
    let (other_capsule, other_execution) = replay_projection_fixture(other_task_id, "other");
    for (capsule, execution) in [
        (stale_capsule, stale_execution),
        (other_capsule.clone(), other_execution.clone()),
    ] {
        store.record_replay_capsule(capsule).expect("seed capsule");
        store
            .record_replay_execution(execution)
            .expect("seed execution");
    }

    store
        .replace_replay_projection_for_task(
            target_task_id,
            vec![canonical_capsule.clone()],
            vec![canonical_execution.clone()],
        )
        .expect("replace target projection");

    let state = store.snapshot().expect("state");
    assert_eq!(
        state
            .replay_capsules
            .iter()
            .filter(|capsule| capsule.source_task_id == target_task_id)
            .cloned()
            .collect::<Vec<_>>(),
        vec![canonical_capsule]
    );
    assert_eq!(
        state
            .replay_executions
            .iter()
            .filter(|execution| execution.source_task_id == target_task_id)
            .cloned()
            .collect::<Vec<_>>(),
        vec![canonical_execution]
    );
    assert!(state.replay_capsules.contains(&other_capsule));
    assert!(state.replay_executions.contains(&other_execution));
}

#[test]
fn empty_replay_projection_replacement_only_clears_the_target_task() {
    let target_task_id = TaskId::new();
    let other_task_id = TaskId::new();
    let store = EvaluationStore::in_memory();
    let (target_capsule, target_execution) = replay_projection_fixture(target_task_id, "target");
    let (other_capsule, other_execution) = replay_projection_fixture(other_task_id, "other");
    for (capsule, execution) in [
        (target_capsule, target_execution),
        (other_capsule.clone(), other_execution.clone()),
    ] {
        store.record_replay_capsule(capsule).expect("seed capsule");
        store
            .record_replay_execution(execution)
            .expect("seed execution");
    }

    store
        .replace_replay_projection_for_task(target_task_id, Vec::new(), Vec::new())
        .expect("clear target projection");

    let state = store.snapshot().expect("state");
    assert_eq!(state.replay_capsules, vec![other_capsule]);
    assert_eq!(state.replay_executions, vec![other_execution]);
}

#[test]
fn invalid_replay_projection_replacement_is_atomic() {
    let target_task_id = TaskId::new();
    let other_task_id = TaskId::new();
    let store = EvaluationStore::in_memory();
    let (target_capsule, target_execution) = replay_projection_fixture(target_task_id, "target");
    let (other_capsule, other_execution) = replay_projection_fixture(other_task_id, "shared");
    for (capsule, execution) in [
        (target_capsule, target_execution),
        (other_capsule.clone(), other_execution),
    ] {
        store.record_replay_capsule(capsule).expect("seed capsule");
        store
            .record_replay_execution(execution)
            .expect("seed execution");
    }
    let before = store.snapshot().expect("state before conflict");
    let mut conflicting_capsule = other_capsule;
    conflicting_capsule.source_task_id = target_task_id;
    conflicting_capsule.source_run_id = RunId::from(target_task_id);

    assert!(matches!(
        store.replace_replay_projection_for_task(
            target_task_id,
            vec![conflicting_capsule],
            Vec::new()
        ),
        Err(EvaluationError::Invariant(_))
    ));
    assert_eq!(store.snapshot().expect("state after conflict"), before);

    let (wrong_task_capsule, _) = replay_projection_fixture(other_task_id, "wrong-task");
    assert!(matches!(
        store.replace_replay_projection_for_task(
            target_task_id,
            vec![wrong_task_capsule],
            Vec::new()
        ),
        Err(EvaluationError::Invariant(_))
    ));
    assert_eq!(store.snapshot().expect("state after wrong task"), before);
}

#[test]
fn failed_improvement_candidate_settles_as_review_without_an_executable_patch() {
    let task_id = TaskId::new();
    let candidate_id = format!("candidate-{task_id}");
    let store = EvaluationStore::in_memory();
    let bundle = EvaluationRunner.evaluate_task(failed_input(task_id, EvidenceId::new()));
    assert!(
        bundle
            .improvement_candidate
            .as_ref()
            .is_some_and(|candidate| {
                candidate.id == candidate_id && candidate.status == CandidateStatus::Proposed
            })
    );
    assert!(bundle.automation_candidates.iter().any(|candidate| {
        candidate.id == candidate_id
            && candidate.kind == AutomationCandidateKind::RuntimeChange
            && candidate.status == CandidateStatus::Proposed
    }));
    store.record_task_evaluation(bundle).expect("evaluation");

    let regression = store
        .record_blocked_regression(&candidate_id, "no frozen candidate patch")
        .expect("blocked regression");
    let decision = store.decide_after_regression(&candidate_id);
    let state = store.snapshot().expect("state");

    assert_eq!(regression.verdict, RegressionVerdict::NeedsReview);
    assert!(matches!(
        decision,
        Err(EvaluationError::RegressionExecutionRequired(id)) if id == candidate_id
    ));
    assert!(state.improvement_candidates.iter().any(|candidate| {
        candidate.id == candidate_id && candidate.status == CandidateStatus::NeedsHumanReview
    }));
    assert!(state.automation_candidates.iter().any(|candidate| {
        candidate.id == candidate_id && candidate.status == CandidateStatus::NeedsHumanReview
    }));
    assert!(state.applied_candidates.is_empty());
}

#[test]
fn frozen_candidate_patch_is_idempotent_but_immutable() {
    let task_id = TaskId::new();
    let candidate_id = format!("candidate-{task_id}");
    let store = EvaluationStore::in_memory();
    store
        .record_task_evaluation(
            EvaluationRunner.evaluate_task(failed_input(task_id, EvidenceId::new())),
        )
        .expect("evaluation");
    let patch = FrozenCandidatePatch {
        candidate_id: candidate_id.clone(),
        source_task_id: task_id,
        artifact_ref: ArtifactId::new(),
        digest: "sha256:frozen-patch".to_owned(),
        format: "golutra.candidate-patch.v1".to_owned(),
        file_count: 1,
        total_bytes: 128,
        frozen_at: Utc::now(),
    };

    assert!(
        store
            .record_frozen_candidate_patch(patch.clone())
            .expect("first freeze")
    );
    assert!(
        !store
            .record_frozen_candidate_patch(FrozenCandidatePatch {
                frozen_at: Utc::now(),
                ..patch.clone()
            })
            .expect("idempotent freeze")
    );
    let replacement = FrozenCandidatePatch {
        artifact_ref: ArtifactId::new(),
        digest: "sha256:different-patch".to_owned(),
        ..patch
    };
    assert!(matches!(
        store.record_frozen_candidate_patch(replacement),
        Err(EvaluationError::InvalidCandidateState { .. })
    ));
}

#[test]
fn applied_benchmark_can_be_rolled_back() {
    let task_id = TaskId::new();
    let store = EvaluationStore::in_memory();
    let bundle = EvaluationRunner.evaluate_task(failed_input(task_id, EvidenceId::new()));
    store
        .record_task_evaluation(bundle.clone())
        .expect("record");
    let candidate_id = format!("automation-benchmark-{task_id}");
    seed_paired_execution(&store, &candidate_id);
    store.run_regression(&candidate_id).expect("regression");
    store.decide_promotion(&candidate_id).expect("decision");
    store.apply_candidate(&candidate_id).expect("apply");
    store
        .record_task_evaluation(bundle.clone())
        .expect("repeat applied source evaluation");
    let applied_state = store.snapshot().expect("applied snapshot");
    assert!(applied_state.benchmark_promotions.iter().any(|candidate| {
        candidate.source_task_id == task_id
            && candidate.promotion_status == CandidateStatus::Applied
            && candidate.accepted_by.as_deref() == Some("system")
    }));

    let rolled_back = store
        .rollback_candidate(&candidate_id, "superseded")
        .expect("rollback");
    store
        .record_task_evaluation(bundle)
        .expect("repeat source evaluation");

    assert!(rolled_back.rolled_back_at.is_some());
    let state = store.snapshot().expect("snapshot");
    assert!(state.automation_candidates.iter().any(|candidate| {
        candidate.id == candidate_id && candidate.status == CandidateStatus::RolledBack
    }));
    assert!(state.benchmark_promotions.iter().any(|candidate| {
        candidate.source_task_id == task_id
            && candidate.promotion_status == CandidateStatus::RolledBack
            && candidate.accepted_by.is_none()
    }));
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
            candidate_artifact_ref: None,
            baseline_version: "baseline-test".to_owned(),
            environment_recipe: "isolated-test".to_owned(),
            case_refs: case_refs.clone(),
            case_partitions: case_refs
                .iter()
                .map(|case_ref| (case_ref.clone(), EvaluationPartitionKind::Source))
                .collect(),
            required_partitions: vec![EvaluationPartitionKind::Source],
            replay_modes: vec!["live_execution".to_owned()],
            provider_matrix: vec!["mock".to_owned()],
            seeds: vec![1],
            minimum_trusted_external_pairs: 0,
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
                partition: EvaluationPartitionKind::Source,
                provider_variant: "mock".to_owned(),
                seed: 1,
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
    assert_eq!(regression.paired_execution_refs.len(), 2);
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
        causal_comparison_refs: Vec::new(),
        paired_execution_refs: vec![
            "baseline-execution".to_owned(),
            "candidate-execution".to_owned(),
        ],
        external_evaluation_refs: Vec::new(),
        coverage: complete_coverage(),
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
        causal_comparison_refs: vec!["comparison".to_owned()],
        paired_execution_refs: vec![
            "baseline-execution".to_owned(),
            "candidate-execution".to_owned(),
        ],
        external_evaluation_refs: Vec::new(),
        coverage: complete_coverage(),
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
            trusted_external_evaluation_refs: Vec::new(),
            coverage_complete: true,
            missing_coverage: Vec::new(),
            holdout_disclosure_violations: Vec::new(),
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
            trusted_external_evaluation_refs: Vec::new(),
            coverage_complete: true,
            missing_coverage: Vec::new(),
            holdout_disclosure_violations: Vec::new(),
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
fn trusted_external_pair_is_compared_and_contributes_campaign_coverage() {
    let task_id = TaskId::new();
    let store = EvaluationStore::in_memory();
    store
        .record_task_evaluation(
            EvaluationRunner.evaluate_task(failed_input(task_id, EvidenceId::new())),
        )
        .expect("evaluation");
    let candidate_id = format!("automation-benchmark-{task_id}");
    let case_ref = store
        .snapshot()
        .expect("state")
        .cases
        .into_iter()
        .find(|case| case.source_task_id == Some(task_id))
        .map(|case| case.case_id)
        .expect("case");
    let campaign_id = RegressionCampaignId::new();
    store
        .record_regression_campaign(RegressionCampaign {
            campaign_id,
            candidate_id: candidate_id.clone(),
            candidate_digest: "sha256:external-coverage".to_owned(),
            candidate_artifact_ref: None,
            baseline_version: "baseline-test".to_owned(),
            environment_recipe: "isolated-test".to_owned(),
            case_refs: vec![case_ref.clone()],
            case_partitions: BTreeMap::from([(case_ref.clone(), EvaluationPartitionKind::Source)]),
            required_partitions: vec![EvaluationPartitionKind::Source],
            replay_modes: vec!["live_execution".to_owned()],
            provider_matrix: vec!["external-provider".to_owned(), "mock".to_owned()],
            seeds: vec![1, 7],
            minimum_trusted_external_pairs: 2,
            resource_budget: "test".to_owned(),
            hard_gates: vec!["paired_trace".to_owned()],
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
        })
        .expect("campaign");
    for seed in [1, 7] {
        for role in [
            RegressionExecutionRole::Baseline,
            RegressionExecutionRole::Candidate,
        ] {
            store
                .record_regression_execution(RegressionExecution {
                    execution_id: RegressionExecutionId::new(),
                    campaign_id,
                    case_ref: case_ref.clone(),
                    partition: EvaluationPartitionKind::Source,
                    provider_variant: "mock".to_owned(),
                    seed,
                    role,
                    runtime_version: "test".to_owned(),
                    workspace_snapshot_digest: format!("sha256:{role:?}:{seed}"),
                    task_trace_ref: Some(format!("runtime://test/{role:?}/{seed}")),
                    verification_ref: Some(VerificationId::new()),
                    cost_latency_ref: Some("test".to_owned()),
                    status: RegressionExecutionStatus::Succeeded,
                })
                .expect("execution");
        }
    }
    for seed in [1] {
        for (evaluation_id, role) in [
            (
                format!("external-baseline-{seed}"),
                RegressionExecutionRole::Baseline,
            ),
            (
                format!("external-candidate-{seed}"),
                RegressionExecutionRole::Candidate,
            ),
        ] {
            store
                .record_external_evaluation(external_evaluation(
                    task_id,
                    &evaluation_id,
                    &case_ref,
                    campaign_id,
                    &candidate_id,
                    role,
                    EvaluationPartitionKind::Source,
                    "external-provider",
                    seed,
                    ExternalEvaluationTrust::OwnerLocal,
                ))
                .expect("external evaluation");
        }
    }

    let incomplete = store
        .run_regression(&candidate_id)
        .expect("partial regression");
    assert_eq!(incomplete.verdict, RegressionVerdict::NeedsReview);
    assert_eq!(incomplete.coverage.expected_cells, 4);
    assert_eq!(incomplete.coverage.completed_cells, 3);
    assert!(
        incomplete
            .coverage
            .missing_cells
            .iter()
            .any(|cell| cell.contains("provider:external-provider|seed:7"))
    );

    for seed in [7] {
        for (evaluation_id, role) in [
            (
                format!("external-baseline-{seed}"),
                RegressionExecutionRole::Baseline,
            ),
            (
                format!("external-candidate-{seed}"),
                RegressionExecutionRole::Candidate,
            ),
        ] {
            store
                .record_external_evaluation(external_evaluation(
                    task_id,
                    &evaluation_id,
                    &case_ref,
                    campaign_id,
                    &candidate_id,
                    role,
                    EvaluationPartitionKind::Source,
                    "external-provider",
                    seed,
                    ExternalEvaluationTrust::OwnerLocal,
                ))
                .expect("external evaluation");
        }
    }

    let regression = store.run_regression(&candidate_id).expect("regression");
    assert_eq!(regression.verdict, RegressionVerdict::Pass);
    assert!(regression.coverage.complete());
    assert_eq!(regression.coverage.expected_cells, 4);
    assert_eq!(regression.coverage.trusted_external_pairs, 2);
    assert_eq!(regression.external_evaluation_refs.len(), 4);
    assert_eq!(regression.causal_comparison_refs.len(), 2);
    assert_eq!(store.snapshot().expect("state").causal_comparisons.len(), 2);
}

#[test]
fn holdout_result_rejects_unsigned_or_detailed_disclosure() {
    let task_id = TaskId::new();
    let campaign_id = RegressionCampaignId::new();
    let store = EvaluationStore::in_memory();
    let mut record = external_evaluation(
        task_id,
        "holdout",
        "holdout-case",
        campaign_id,
        "candidate",
        RegressionExecutionRole::Candidate,
        EvaluationPartitionKind::Holdout,
        "sealed-evaluator",
        9,
        ExternalEvaluationTrust::OwnerLocal,
    );
    record.holdout_protected = true;
    record.result_digest = external_evaluation_result_digest(&record);

    assert!(matches!(
        store.record_external_evaluation(record),
        Err(EvaluationError::Invariant(_))
    ));
}

#[test]
fn external_evaluation_rejects_partial_association_and_conflicting_identity() {
    let task_id = TaskId::new();
    let campaign_id = RegressionCampaignId::new();
    let store = EvaluationStore::in_memory();
    let record = external_evaluation(
        task_id,
        "external-id",
        "case-id",
        campaign_id,
        "candidate",
        RegressionExecutionRole::Baseline,
        EvaluationPartitionKind::Source,
        "mock",
        1,
        ExternalEvaluationTrust::OwnerLocal,
    );

    let mut partial = record.clone();
    partial.role = None;
    partial.result_digest = external_evaluation_result_digest(&partial);
    assert!(matches!(
        store.record_external_evaluation(partial),
        Err(EvaluationError::Invariant(_))
    ));

    assert!(
        store
            .record_external_evaluation(record.clone())
            .expect("first record")
    );
    let original_ingested_at = record.ingested_at;
    let mut retry = record.clone();
    retry.ingested_at += chrono::Duration::seconds(1);
    assert!(
        !store
            .record_external_evaluation(retry)
            .expect("idempotent retry")
    );
    assert_eq!(
        store.snapshot().expect("state").external_evaluations[0].ingested_at,
        original_ingested_at,
        "the first imported record remains immutable"
    );

    let mut conflicting = record;
    conflicting.verdict = EvaluationVerdict::Fail;
    conflicting.result_digest = external_evaluation_result_digest(&conflicting);
    assert!(matches!(
        store.record_external_evaluation(conflicting),
        Err(EvaluationError::Invariant(_))
    ));
}

#[test]
fn signed_external_evaluation_rejects_obviously_invalid_attestation_metadata() {
    let task_id = TaskId::new();
    let campaign_id = RegressionCampaignId::new();
    let store = EvaluationStore::in_memory();
    let mut record = external_evaluation(
        task_id,
        "signed-invalid",
        "case-id",
        campaign_id,
        "candidate",
        RegressionExecutionRole::Candidate,
        EvaluationPartitionKind::Source,
        "mock",
        1,
        ExternalEvaluationTrust::Signed,
    );
    record.result_digest = external_evaluation_result_digest(&record);
    record.attestation = Some(EvaluationAttestation {
        algorithm: "rsa".to_owned(),
        key_id: String::new(),
        signature: String::new(),
        signed_digest: "sha256:wrong".to_owned(),
    });

    assert!(matches!(
        store.record_external_evaluation(record),
        Err(EvaluationError::Invariant(_))
    ));
}

#[test]
fn evaluation_text_redacts_embedded_api_keys() {
    assert_eq!(
        sanitize_text("url?key=sk-1234567890123456 failed"),
        "url?key=[REDACTED] failed"
    );
}
