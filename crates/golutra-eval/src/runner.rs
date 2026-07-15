use chrono::Utc;
use golutra_core::{EvidenceId, TaskId, TaskStatus, TokenUsageRecord, VerificationResult};
use uuid::Uuid;

use crate::{
    AutomationCandidate, AutomationCandidateKind, BenchmarkCheck, BenchmarkCheckStatus,
    BenchmarkPromotion, BenchmarkRun, BenchmarkSuiteKind, CandidateRisk, CandidateStatus,
    CostRecord, EvaluationCase, EvaluationResult, EvaluationRun, EvaluationVerdict, GeneratedTask,
    ImprovementCandidate, PostTaskReview, PromotionDecision, PromotionDecisionKind,
    PromotionReviewer, RegressionResult, RegressionVerdict, ReviewMode, SecurityUtilityResult,
    SkillCandidate, TaskEvaluationBundle, TaskEvaluationInput, TrajectoryReplay,
};

#[derive(Debug, Default, Clone, Copy)]
pub struct EvaluationRunner;

impl EvaluationRunner {
    #[must_use]
    pub fn evaluate_task(&self, input: TaskEvaluationInput) -> TaskEvaluationBundle {
        self.evaluate(input, ReviewMode::Deep)
    }

    #[must_use]
    pub fn evaluate_minimal(&self, input: TaskEvaluationInput) -> TaskEvaluationBundle {
        self.evaluate(input, ReviewMode::Minimal)
    }

    fn evaluate(&self, input: TaskEvaluationInput, mode: ReviewMode) -> TaskEvaluationBundle {
        let now = Utc::now();
        let case_id = format!("case-{}", input.task_id);
        let run_id = format!("run-{}", input.task_id);
        let evidence_refs = input
            .verification
            .as_ref()
            .map(|record| record.evidence_refs.clone())
            .unwrap_or_default();
        let verification_result = input.verification.as_ref().map(|record| record.result);
        let verdict = evaluation_verdict(input.task_status, verification_result);
        let failure_taxonomy = failure_taxonomy(&input, verdict);
        let residual_risks = input
            .verification
            .as_ref()
            .map(|record| record.residual_risks.clone())
            .unwrap_or_else(|| {
                input
                    .failure_summary
                    .as_deref()
                    .map(sanitize_text)
                    .into_iter()
                    .collect()
            });
        let result_id = format!("result-{}", input.task_id);
        let cost_records = aggregate_cost_records(&input.token_usage);
        let input_tokens =
            sum_optional_tokens(input.token_usage.iter().map(|record| record.input_tokens))
                .or_else(|| Some(estimate_text_tokens(&input.objective)));
        let output_tokens =
            sum_optional_tokens(input.token_usage.iter().map(|record| record.output_tokens));
        let reasoning_tokens = sum_optional_tokens(
            input
                .token_usage
                .iter()
                .map(|record| record.reasoning_tokens),
        );
        let total_tokens =
            sum_optional_tokens(input.token_usage.iter().map(|record| record.total_tokens))
                .or_else(|| add_optional_tokens(input_tokens, output_tokens));
        let cost = sum_optional_cost(cost_records.iter().map(|record| record.estimated_cost_usd));
        let cost_source = aggregate_cost_source(&cost_records);
        let security_utility = SecurityUtilityResult {
            security_score: Some(if input.policy_violation_count == 0 {
                1.0
            } else {
                0.0
            }),
            utility_score: Some(match verdict {
                EvaluationVerdict::Pass => 1.0,
                EvaluationVerdict::Partial => 0.5,
                EvaluationVerdict::Fail => 0.0,
                EvaluationVerdict::Unknown => 0.25,
            }),
            policy_violations: input.policy_violation_count,
            evidence_refs: evidence_refs.clone(),
            verdict: if input.policy_violation_count == 0 {
                verdict
            } else {
                EvaluationVerdict::Fail
            },
        };
        let case = EvaluationCase {
            case_id: case_id.clone(),
            source: "live_task".to_owned(),
            source_task_id: Some(input.task_id),
            task_type: if input.tool_count > 0 {
                "workspace_task".to_owned()
            } else {
                "conversation".to_owned()
            },
            objective: sanitize_text(&input.objective),
            expected_outcome: "verification-backed terminal result".to_owned(),
            success_criteria: input
                .verification
                .as_ref()
                .map(|record| record.completion_criteria.clone())
                .unwrap_or_else(|| vec!["task reaches an explainable terminal state".to_owned()]),
            required_evidence: evidence_refs.iter().map(ToString::to_string).collect(),
            policy_constraints: vec!["no unapproved high-risk side effects".to_owned()],
            fixture_refs: vec![format!("replay-{}", input.task_id)],
            tags: failure_taxonomy.clone(),
        };
        let result = EvaluationResult {
            result_id: result_id.clone(),
            run_id: run_id.clone(),
            case_id: case_id.clone(),
            source_task_id: input.task_id,
            verdict,
            quality_score: Some(match verdict {
                EvaluationVerdict::Pass => 1.0,
                EvaluationVerdict::Partial => 0.5,
                EvaluationVerdict::Fail => 0.0,
                EvaluationVerdict::Unknown => 0.25,
            }),
            cost,
            latency_ms: input.latency_ms,
            evidence_refs: evidence_refs.clone(),
            failure_taxonomy: failure_taxonomy.clone(),
            residual_risks: residual_risks.clone(),
            security_utility: Some(security_utility.clone()),
        };
        let run = EvaluationRun {
            run_id,
            dataset_id: "workspace-history".to_owned(),
            case_ids: vec![case_id],
            system_version: env!("CARGO_PKG_VERSION").to_owned(),
            provider_config_ref: input.provider_config_ref.clone(),
            runtime_config_ref: input.runtime_config_ref.clone(),
            started_at: now,
            completed_at: Utc::now(),
            cost,
            cost_source: cost_source.clone(),
            latency_ms: input.latency_ms,
            input_tokens,
            output_tokens,
            reasoning_tokens,
            total_tokens,
            cost_records: cost_records.clone(),
            result_refs: vec![result_id],
        };
        let replay = replay_summary(input.task_id, input.event_count, input.artifact_count);
        let successful = verdict == EvaluationVerdict::Pass;
        let deep_review = mode == ReviewMode::Deep;
        let improvement_candidate = (deep_review && !successful).then(|| {
            improvement_candidate_from_failure(
                input.task_id,
                evidence_refs.clone(),
                failure_taxonomy.clone(),
                input
                    .failure_summary
                    .as_deref()
                    .unwrap_or("capture the failed trajectory as a regression case"),
            )
        });
        let generated_task = (deep_review && !successful).then(|| GeneratedTask {
            id: format!("generated-task-{}", input.task_id),
            source_task_id: input.task_id,
            source: "failed_trajectory".to_owned(),
            objective: format!("reproduce and resolve: {}", sanitize_text(&input.objective)),
            novelty_score: None,
            difficulty_score: None,
            expected_learning_value: "turn a production failure into a repeatable regression"
                .to_owned(),
            environment_recipe: format!("fixture://task/{}", input.task_id),
            safety_constraints: vec![
                "fixture_only".to_owned(),
                "no_external_side_effects".to_owned(),
            ],
            promotion_status: CandidateStatus::Proposed,
        });
        let benchmark_promotion = (deep_review && !successful).then(|| BenchmarkPromotion {
            id: format!("benchmark-{}", input.task_id),
            source_task_id: input.task_id,
            failure_taxonomy: failure_taxonomy.clone(),
            fixture: format!("replay-{}", input.task_id),
            evaluator: "durable_verification_replay".to_owned(),
            anti_overfit_notes: vec![
                "fixture contains runtime facts and evidence references, not a target answer"
                    .to_owned(),
            ],
            rollback_ref: format!("remove-benchmark-{}", input.task_id),
            promotion_status: CandidateStatus::Proposed,
            accepted_by: None,
        });
        let skill_candidate =
            (deep_review && successful && !evidence_refs.is_empty()).then(|| SkillCandidate {
                id: format!("skill-{}", input.task_id),
                source_task_id: input.task_id,
                source_trajectory: format!("replay-{}", input.task_id),
                reusable_pattern: sanitize_text(&input.objective),
                evidence_refs: evidence_refs.clone(),
                regression_refs: Vec::new(),
                scope: "project".to_owned(),
                rollback_ref: format!("remove-skill-{}", input.task_id),
                promotion_status: CandidateStatus::Proposed,
            });
        let mut automation_candidates = Vec::new();
        if benchmark_promotion.is_some() {
            automation_candidates.push(AutomationCandidate {
                id: format!("automation-benchmark-{}", input.task_id),
                source_task_id: input.task_id,
                kind: AutomationCandidateKind::Benchmark,
                summary: "promote failed trajectory to the workspace regression dataset".to_owned(),
                risk_level: CandidateRisk::Low,
                evidence_refs: evidence_refs.clone(),
                regression_plan: format!("replay source task {}", input.task_id),
                rollback_ref: format!("remove-benchmark-{}", input.task_id),
                status: CandidateStatus::Proposed,
            });
            automation_candidates.push(AutomationCandidate {
                id: format!("automation-generated-task-{}", input.task_id),
                source_task_id: input.task_id,
                kind: AutomationCandidateKind::GeneratedTask,
                summary: "add a fixture-only task to the capability frontier".to_owned(),
                risk_level: CandidateRisk::Medium,
                evidence_refs: evidence_refs.clone(),
                regression_plan: format!("run generated fixture for task {}", input.task_id),
                rollback_ref: format!("remove-generated-task-{}", input.task_id),
                status: CandidateStatus::Proposed,
            });
        }
        if skill_candidate.is_some() {
            automation_candidates.push(AutomationCandidate {
                id: format!("automation-skill-{}", input.task_id),
                source_task_id: input.task_id,
                kind: AutomationCandidateKind::Skill,
                summary: "extract a project-scoped skill from a successful trajectory".to_owned(),
                risk_level: CandidateRisk::Medium,
                evidence_refs: evidence_refs.clone(),
                regression_plan: format!("replay successful task {}", input.task_id),
                rollback_ref: format!("remove-skill-{}", input.task_id),
                status: CandidateStatus::Proposed,
            });
        }
        let promotion_candidates = automation_candidates
            .iter()
            .map(|candidate| candidate.id.clone())
            .collect();
        let review = PostTaskReview {
            task_id: input.task_id,
            mode,
            outcome: format!("{verdict:?}").to_lowercase(),
            success_reasons: if successful {
                vec!["terminal verification passed".to_owned()]
            } else {
                Vec::new()
            },
            failure_reasons: if successful {
                Vec::new()
            } else if residual_risks.is_empty() {
                failure_taxonomy.clone()
            } else {
                residual_risks.clone()
            },
            evidence_quality: if evidence_refs.is_empty() {
                "none".to_owned()
            } else {
                "durable".to_owned()
            },
            policy_issues: taxonomy_matches(&failure_taxonomy, "PolicyFailure"),
            context_issues: taxonomy_matches(&failure_taxonomy, "ContextFailure"),
            tool_issues: taxonomy_matches(&failure_taxonomy, "ToolFailure"),
            provider_issues: taxonomy_matches(&failure_taxonomy, "ProviderFailure"),
            suggested_improvements: if successful {
                Vec::new()
            } else {
                vec!["run the generated regression fixture before promotion".to_owned()]
            },
            promotion_candidates,
        };
        let benchmark_run =
            benchmark_run_from_evaluation(&input, &run, &result, &replay, security_utility);

        TaskEvaluationBundle {
            case,
            run,
            result,
            replay,
            review,
            improvement_candidate,
            generated_task,
            skill_candidate,
            benchmark_promotion,
            automation_candidates,
            benchmark_run,
        }
    }
}

fn aggregate_cost_records(records: &[TokenUsageRecord]) -> Vec<CostRecord> {
    records
        .iter()
        .map(|record| CostRecord {
            provider_id: record.provider_id.clone(),
            model_id: record.model_id.clone(),
            input_tokens: record.input_tokens,
            output_tokens: record.output_tokens,
            reasoning_tokens: record.reasoning_tokens,
            total_tokens: record.total_tokens,
            estimated_cost_usd: record.estimated_cost,
            source: record.usage_source.clone(),
            confidence: if record.estimated_cost.is_some() && record.usage_source == "provider" {
                "provider_reported".to_owned()
            } else if record.estimated_cost.is_some() {
                "estimated".to_owned()
            } else {
                "unknown".to_owned()
            },
        })
        .collect()
}

fn sum_optional_tokens(values: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    let values = values.flatten().collect::<Vec<_>>();
    (!values.is_empty()).then(|| {
        values
            .into_iter()
            .fold(0_u64, |total, value| total.saturating_add(value))
    })
}

fn estimate_text_tokens(value: &str) -> u64 {
    let characters = u64::try_from(value.chars().count()).unwrap_or(u64::MAX);
    characters.saturating_add(3) / 4
}

fn add_optional_tokens(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn sum_optional_cost(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let values = values.flatten().collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.into_iter().sum())
}

fn aggregate_cost_source(records: &[CostRecord]) -> String {
    if records.is_empty()
        || records
            .iter()
            .all(|record| record.estimated_cost_usd.is_none())
    {
        return "unknown".to_owned();
    }
    if records
        .iter()
        .all(|record| record.source == "provider" && record.estimated_cost_usd.is_some())
    {
        "provider".to_owned()
    } else {
        "estimated".to_owned()
    }
}

fn benchmark_run_from_evaluation(
    input: &TaskEvaluationInput,
    run: &EvaluationRun,
    result: &EvaluationResult,
    replay: &TrajectoryReplay,
    security_utility: SecurityUtilityResult,
) -> BenchmarkRun {
    let provider_id = run
        .cost_records
        .last()
        .map(|record| record.provider_id.clone())
        .unwrap_or_else(|| "unknown".to_owned());
    let model_id = run
        .cost_records
        .last()
        .map(|record| record.model_id.clone())
        .unwrap_or_else(|| "unknown".to_owned());
    let required_evidence_present = input.tool_count == 0
        || input
            .verification
            .as_ref()
            .is_some_and(|record| !record.evidence_refs.is_empty());
    let sanitized_objective = sanitize_text(&input.objective);
    let fixture_exposed = ["hidden_fixture", "expected_answer", "golden_answer"]
        .iter()
        .any(|marker| sanitized_objective.to_ascii_lowercase().contains(marker));
    let leakage_checks = vec![
        benchmark_check(
            "answer_leakage",
            if fixture_exposed {
                BenchmarkCheckStatus::Fail
            } else {
                BenchmarkCheckStatus::Pass
            },
            if fixture_exposed {
                "objective contains a reserved answer or hidden-fixture marker"
            } else {
                "no reserved answer marker appears in the sanitized objective"
            },
            vec![replay.replay_id.clone()],
        ),
        benchmark_check(
            "test_hook_injection",
            if sanitized_objective
                .to_ascii_lowercase()
                .contains("bypass evaluator")
            {
                BenchmarkCheckStatus::Fail
            } else {
                BenchmarkCheckStatus::Pass
            },
            "evaluation instructions are checked for explicit evaluator bypasses",
            vec![replay.replay_id.clone()],
        ),
        benchmark_check(
            "hidden_fixture_exposure",
            if fixture_exposed {
                BenchmarkCheckStatus::Fail
            } else {
                BenchmarkCheckStatus::Pass
            },
            "fixture references remain identifiers and do not contain target answers",
            vec![replay.replay_id.clone()],
        ),
    ];
    let judge_checks = vec![
        benchmark_check(
            "judge_input_sanitization",
            BenchmarkCheckStatus::Pass,
            "the evaluation input is redacted and whitespace-normalized before scoring",
            vec![result.result_id.clone()],
        ),
        benchmark_check(
            "evidence_backed_grading",
            if required_evidence_present {
                BenchmarkCheckStatus::Pass
            } else {
                BenchmarkCheckStatus::Fail
            },
            if required_evidence_present {
                "workspace grading is backed by durable verification evidence"
            } else {
                "workspace grading has no durable verification evidence"
            },
            result
                .evidence_refs
                .iter()
                .map(ToString::to_string)
                .collect(),
        ),
        benchmark_check(
            "no_single_model_sentence_verdict",
            BenchmarkCheckStatus::Pass,
            "the verdict is produced by deterministic task and verification records",
            vec![result.result_id.clone()],
        ),
    ];
    let scaffold_checks = vec![benchmark_check(
        "scaffold_version_pinned",
        BenchmarkCheckStatus::Pass,
        "runtime and scaffold versions are recorded for comparison",
        vec![run.system_version.clone()],
    )];

    BenchmarkRun {
        benchmark_id: format!("benchmark-run-{}", input.task_id),
        suite_kind: BenchmarkSuiteKind::Regression,
        dataset_version: "workspace-history-v1".to_owned(),
        harness_version: env!("CARGO_PKG_VERSION").to_owned(),
        scaffold_id: "golutra-runtime".to_owned(),
        scaffold_version: env!("CARGO_PKG_VERSION").to_owned(),
        model_id,
        provider_id,
        tool_budget: u32::try_from(input.tool_count.max(1)).unwrap_or(u32::MAX),
        attempt_count: u32::try_from(input.token_usage.len().max(1)).unwrap_or(u32::MAX),
        runtime_ms: input.latency_ms.unwrap_or(1).max(1),
        input_tokens: run.input_tokens,
        output_tokens: run.output_tokens,
        reasoning_tokens: run.reasoning_tokens,
        total_tokens: run.total_tokens,
        cost_usd: run.cost,
        cost_source: run.cost_source.clone(),
        security_score: security_utility.security_score,
        utility_score: security_utility.utility_score,
        artifact_delivery_status: if input.artifact_count > 0 {
            "delivered".to_owned()
        } else {
            "not_required".to_owned()
        },
        score: result.quality_score,
        failure_taxonomy: result.failure_taxonomy.clone(),
        counterfactual_group_id: None,
        changed_layer: None,
        leakage_checks,
        judge_checks,
        scaffold_checks,
    }
}

pub(crate) fn benchmark_check(
    check_id: &str,
    status: BenchmarkCheckStatus,
    reason: &str,
    evidence_refs: Vec<String>,
) -> BenchmarkCheck {
    BenchmarkCheck {
        check_id: check_id.to_owned(),
        status,
        reason: reason.to_owned(),
        evidence_refs,
    }
}

pub fn replay_summary(
    source_task_id: TaskId,
    event_count: usize,
    artifact_count: usize,
) -> TrajectoryReplay {
    TrajectoryReplay {
        replay_id: format!("replay-{source_task_id}"),
        source_task_id,
        event_count,
        artifact_count,
        determinism_level: "event_artifact_replay".to_owned(),
        limitations: vec![
            "replay uses durable runtime facts without re-executing the provider".to_owned(),
        ],
    }
}

#[must_use]
pub fn improvement_candidate_from_failure(
    source_task_id: TaskId,
    evidence_refs: Vec<EvidenceId>,
    source_failure_ids: Vec<String>,
    failure_summary: impl Into<String>,
) -> ImprovementCandidate {
    ImprovementCandidate {
        id: format!("candidate-{source_task_id}"),
        source_task_id,
        source_failure_ids,
        target_type: "benchmark_case".to_owned(),
        target_id: None,
        proposed_change: sanitize_text(&failure_summary.into()),
        expected_effect: "make the failure mode replayable and regression-tested".to_owned(),
        risk_level: CandidateRisk::Low,
        evidence_refs,
        causal_evidence_refs: vec![format!("replay-{source_task_id}")],
        benchmark_refs: vec![format!("benchmark-{source_task_id}")],
        rollback_plan: format!("remove-benchmark-{source_task_id}"),
        status: CandidateStatus::Proposed,
    }
}

#[must_use]
pub fn benchmark_run_has_required_metadata(run: &BenchmarkRun) -> bool {
    !run.dataset_version.is_empty()
        && !run.harness_version.is_empty()
        && !run.scaffold_id.is_empty()
        && !run.scaffold_version.is_empty()
        && !run.model_id.is_empty()
        && !run.provider_id.is_empty()
        && run.tool_budget > 0
        && run.attempt_count > 0
        && run.total_tokens.is_some()
        && run.runtime_ms > 0
        && !run.cost_source.is_empty()
        && required_benchmark_checks_pass(&run.leakage_checks)
        && required_benchmark_checks_pass(&run.judge_checks)
        && required_benchmark_checks_pass(&run.scaffold_checks)
}

fn required_benchmark_checks_pass(checks: &[BenchmarkCheck]) -> bool {
    !checks.is_empty()
        && checks
            .iter()
            .all(|check| check.status == BenchmarkCheckStatus::Pass)
}

#[must_use]
pub fn decide_low_risk_promotion(
    candidate: &AutomationCandidate,
    regression: &RegressionResult,
) -> PromotionDecision {
    let clean_regression = regression.verdict == RegressionVerdict::Pass
        && regression.failed_cases == 0
        && regression.regressions.is_empty();
    let (decision, reason) = if candidate.status != CandidateStatus::RegressionPassed {
        (
            PromotionDecisionKind::Reject,
            "candidate has not reached the regression-passed state",
        )
    } else if !clean_regression {
        (
            PromotionDecisionKind::Reject,
            "regression failed or reported regressions",
        )
    } else if candidate.risk_level != CandidateRisk::Low
        || candidate.kind != AutomationCandidateKind::Benchmark
    {
        (
            PromotionDecisionKind::NeedsHumanReview,
            "candidate is outside the low-risk benchmark automation boundary",
        )
    } else if candidate.rollback_ref.trim().is_empty() || candidate.evidence_refs.is_empty() {
        (
            PromotionDecisionKind::Reject,
            "candidate lacks durable evidence or a rollback reference",
        )
    } else {
        (
            PromotionDecisionKind::Approve,
            "low-risk benchmark candidate passed clean regression",
        )
    };
    PromotionDecision {
        decision_id: format!("promotion-{}", Uuid::now_v7()),
        candidate_id: candidate.id.clone(),
        decision,
        reason: reason.to_owned(),
        reviewer: PromotionReviewer::System,
        applied_version: (decision == PromotionDecisionKind::Approve)
            .then(|| regression.candidate_version.clone()),
        rollback_ref: Some(candidate.rollback_ref.clone()),
        expires_at: None,
        created_at: Utc::now(),
    }
}

fn evaluation_verdict(
    task_status: TaskStatus,
    verification: Option<VerificationResult>,
) -> EvaluationVerdict {
    match (task_status, verification) {
        (TaskStatus::Completed, Some(VerificationResult::Pass)) => EvaluationVerdict::Pass,
        (TaskStatus::Partial, _) | (_, Some(VerificationResult::Partial)) => {
            EvaluationVerdict::Partial
        }
        (TaskStatus::Failed | TaskStatus::Cancelled, _) | (_, Some(VerificationResult::Fail)) => {
            EvaluationVerdict::Fail
        }
        _ => EvaluationVerdict::Unknown,
    }
}

fn failure_taxonomy(input: &TaskEvaluationInput, verdict: EvaluationVerdict) -> Vec<String> {
    if verdict == EvaluationVerdict::Pass {
        return Vec::new();
    }
    let summary = input
        .failure_summary
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut taxonomy = Vec::new();
    if summary.contains("provider") {
        taxonomy.push("ProviderFailure".to_owned());
    }
    if summary.contains("tool") {
        taxonomy.push("ToolFailure".to_owned());
    }
    if summary.contains("policy") || summary.contains("approval") {
        taxonomy.push("PolicyFailure".to_owned());
    }
    if summary.contains("context") || summary.contains("token") {
        taxonomy.push("ContextFailure".to_owned());
    }
    if input
        .verification
        .as_ref()
        .is_some_and(|record| record.result != VerificationResult::Pass)
    {
        taxonomy.push("VerificationFailure".to_owned());
    }
    if taxonomy.is_empty() {
        taxonomy.push(
            match input.task_status {
                TaskStatus::Cancelled => "HumanInteractionFailure",
                TaskStatus::Blocked => "StateFailure",
                _ => "GoalFailure",
            }
            .to_owned(),
        );
    }
    taxonomy.sort();
    taxonomy.dedup();
    taxonomy
}

fn taxonomy_matches(taxonomy: &[String], expected: &str) -> Vec<String> {
    taxonomy
        .iter()
        .filter(|value| value.as_str() == expected)
        .cloned()
        .collect()
}

pub(crate) fn sanitize_text(value: &str) -> String {
    let mut redacted = value.to_owned();
    for prefix in ["github_pat_", "ghp_", "sk-"] {
        redacted = redact_prefixed_secret(&redacted, prefix);
    }
    redacted.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn redact_prefixed_secret(value: &str, prefix: &str) -> String {
    let mut output = value.to_owned();
    let mut search_from = 0;
    loop {
        let lower = output.to_ascii_lowercase();
        let Some(relative_start) = lower[search_from..].find(prefix) else {
            break;
        };
        let start = search_from + relative_start;
        let end = output[start..]
            .char_indices()
            .take_while(|(_, character)| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
            .map(|(offset, character)| start + offset + character.len_utf8())
            .last()
            .unwrap_or(start);
        if end.saturating_sub(start) < 12 {
            search_from = end.max(start + prefix.len());
            continue;
        }
        output.replace_range(start..end, "[REDACTED]");
        search_from = start + "[REDACTED]".len();
    }
    output
}
