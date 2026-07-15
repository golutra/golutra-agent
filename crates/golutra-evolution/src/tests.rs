use std::fs;

use golutra_core::{EvidenceId, TaskId};
use golutra_eval::{
    CandidateStatus, EvaluationCase, EvaluationResult, EvaluationState, EvaluationVerdict,
    GeneratedTask, SkillCandidate,
};
use tempfile::tempdir;

use super::*;

#[test]
fn planner_selects_safe_frontier_tasks_and_persists_plan() {
    let task_id = TaskId::new();
    let evaluation = EvaluationState {
        cases: vec![EvaluationCase {
            case_id: "case-existing".to_owned(),
            source: "live".to_owned(),
            source_task_id: Some(task_id),
            task_type: "workspace".to_owned(),
            objective: "fix provider authentication".to_owned(),
            expected_outcome: "pass".to_owned(),
            success_criteria: vec!["pass".to_owned()],
            required_evidence: Vec::new(),
            policy_constraints: Vec::new(),
            fixture_refs: Vec::new(),
            tags: Vec::new(),
        }],
        results: vec![EvaluationResult {
            result_id: "result".to_owned(),
            run_id: "run".to_owned(),
            case_id: "case-existing".to_owned(),
            source_task_id: task_id,
            verdict: EvaluationVerdict::Fail,
            quality_score: Some(0.0),
            cost: None,
            latency_ms: None,
            evidence_refs: Vec::new(),
            failure_taxonomy: vec!["ProviderFailure".to_owned()],
            residual_risks: Vec::new(),
            security_utility: None,
        }],
        generated_tasks: vec![GeneratedTask {
            id: "generated-1".to_owned(),
            source_task_id: task_id,
            source: "failed_trajectory".to_owned(),
            objective: "reproduce malformed streaming response".to_owned(),
            novelty_score: None,
            difficulty_score: Some(60.0),
            expected_learning_value: "provider robustness".to_owned(),
            environment_recipe: "fixture://provider/malformed".to_owned(),
            safety_constraints: vec![
                "fixture_only".to_owned(),
                "no_external_side_effects".to_owned(),
            ],
            promotion_status: CandidateStatus::Proposed,
        }],
        ..EvaluationState::default()
    };
    let plan = EvolutionPlanner.plan(
        &evaluation,
        "expand provider robustness",
        Default::default(),
    );
    let directory = tempdir().expect("directory");
    let store = EvolutionStore::new(
        directory.path().join("evolution.json"),
        directory.path().join("skills"),
    );
    let state = store.record_plan(plan).expect("record plan");

    assert_eq!(state.runs.len(), 1);
    assert_eq!(state.runs[0].selected_task_ids, vec!["generated-1"]);
    assert_eq!(state.recipes.len(), 1);
    assert_eq!(state.frontier.expect("frontier").failed.len(), 1);
}

#[test]
fn skill_requires_regression_review_and_supports_install_rollback() {
    let directory = tempdir().expect("directory");
    let store = EvolutionStore::new(
        directory.path().join("evolution.json"),
        directory.path().join("skills"),
    );
    let candidate = SkillCandidate {
        id: "skill-runtime-tests".to_owned(),
        source_task_id: TaskId::new(),
        source_trajectory: "replay-1".to_owned(),
        reusable_pattern: "run targeted runtime tests after editing lane state".to_owned(),
        evidence_refs: vec![EvidenceId::new()],
        regression_refs: Vec::new(),
        scope: "project".to_owned(),
        rollback_ref: "remove-skill-runtime-tests".to_owned(),
        promotion_status: CandidateStatus::Proposed,
    };
    let staged = store.stage_skill(&candidate).expect("stage");
    assert!(matches!(
        store.review_skill(&candidate.id, "maintainer", "reviewed", Vec::new(), true,),
        Err(EvolutionError::SkillGate(_))
    ));
    store
        .review_skill(
            &candidate.id,
            "maintainer",
            "regression passed",
            vec!["regression-1".to_owned()],
            true,
        )
        .expect("review");
    let installed = store.install_skill(&candidate.id).expect("install");
    assert!(
        fs::metadata(installed.installed_path.as_deref().expect("installed path"))
            .expect("installed file")
            .is_file()
    );
    assert_eq!(
        store
            .active_skill_context("runtime lane tests", 3)
            .expect("active context")
            .len(),
        1
    );
    let rolled_back = store
        .rollback_skill(&candidate.id, "superseded")
        .expect("rollback");

    assert_eq!(staged.status, SkillLifecycleStatus::Proposed);
    assert_eq!(installed.status, SkillLifecycleStatus::Installed);
    assert_eq!(rolled_back.status, SkillLifecycleStatus::RolledBack);
    assert!(
        store
            .active_skill_context("runtime lane tests", 3)
            .expect("active context")
            .is_empty()
    );
}

#[test]
fn run_lifecycle_is_durable_and_skill_ids_cannot_escape_the_store() {
    let directory = tempdir().expect("directory");
    let store = EvolutionStore::new(
        directory.path().join("evolution.json"),
        directory.path().join("skills"),
    );
    let task_id = TaskId::new();
    let evaluation = EvaluationState {
        generated_tasks: vec![GeneratedTask {
            id: "generated-safe".to_owned(),
            source_task_id: task_id,
            source: "failed_trajectory".to_owned(),
            objective: "reproduce parser failure".to_owned(),
            novelty_score: None,
            difficulty_score: Some(60.0),
            expected_learning_value: "parser robustness".to_owned(),
            environment_recipe: "fixture://parser/failure".to_owned(),
            safety_constraints: vec![
                "fixture_only".to_owned(),
                "no_external_side_effects".to_owned(),
            ],
            promotion_status: CandidateStatus::Proposed,
        }],
        ..EvaluationState::default()
    };
    let plan = EvolutionPlanner.plan(&evaluation, "parser robustness", Default::default());
    let run_id = plan.run.run_id.clone();
    store.record_plan(plan).expect("plan");
    assert_eq!(
        store.start_run(&run_id).expect("start").status,
        OpenEndedRunStatus::Running
    );
    assert_eq!(
        store.finish_run(&run_id, None).expect("finish").status,
        OpenEndedRunStatus::Completed
    );

    let unsafe_candidate = SkillCandidate {
        id: "../outside".to_owned(),
        source_task_id: task_id,
        source_trajectory: "replay".to_owned(),
        reusable_pattern: "safe workflow".to_owned(),
        evidence_refs: vec![EvidenceId::new()],
        regression_refs: Vec::new(),
        scope: "project".to_owned(),
        rollback_ref: "remove".to_owned(),
        promotion_status: CandidateStatus::Proposed,
    };
    assert!(matches!(
        store.stage_skill(&unsafe_candidate),
        Err(EvolutionError::SkillGate(_))
    ));
}
