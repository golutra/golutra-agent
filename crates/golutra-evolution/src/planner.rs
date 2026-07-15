use std::collections::HashSet;

use chrono::Utc;
use golutra_eval::{
    CapabilityFrontier, CurriculumItem, EvaluationState, EvaluationVerdict, GeneratedTask,
};
use uuid::Uuid;

use crate::{
    EnvironmentRecipe, EvolutionPlan, NoveltyRecord, OpenEndedBudget, OpenEndedRun,
    OpenEndedRunStatus,
};

#[derive(Debug, Default, Clone, Copy)]
pub struct EvolutionPlanner;

impl EvolutionPlanner {
    #[must_use]
    pub fn plan(
        &self,
        evaluation: &EvaluationState,
        objective: &str,
        budget: OpenEndedBudget,
    ) -> EvolutionPlan {
        let mut generated_tasks = evaluation.generated_tasks.clone();
        generated_tasks.truncate(usize::try_from(budget.max_generated_tasks).unwrap_or(usize::MAX));
        let existing_objectives = evaluation
            .cases
            .iter()
            .map(|case| (case.case_id.as_str(), case.objective.as_str()))
            .collect::<Vec<_>>();
        let novelty = generated_tasks
            .iter()
            .map(|task| novelty_record(task, &existing_objectives))
            .collect::<Vec<_>>();
        let mut selected_count = 0_u32;
        let curriculum = generated_tasks
            .iter_mut()
            .zip(novelty.iter())
            .map(|(task, novelty)| {
                let difficulty = task.difficulty_score.unwrap_or(60.0).clamp(0.0, 100.0);
                task.novelty_score = Some(f32::from(novelty.novelty_score));
                task.difficulty_score = Some(difficulty);
                let safe_fixture = task
                    .safety_constraints
                    .iter()
                    .any(|constraint| constraint == "fixture_only")
                    && task
                        .safety_constraints
                        .iter()
                        .any(|constraint| constraint == "no_external_side_effects");
                let selected = safe_fixture
                    && novelty.novelty_score >= 20
                    && (30.0..=80.0).contains(&difficulty)
                    && selected_count < budget.max_selected_tasks;
                if selected {
                    selected_count = selected_count.saturating_add(1);
                }
                CurriculumItem {
                    task_id: task.id.clone(),
                    selected,
                    selected_reason: selected.then(|| {
                        "task is novel, fixture-only, and near the current capability frontier"
                            .to_owned()
                    }),
                    rejected_reason: (!selected).then(|| {
                        if !safe_fixture {
                            "task is not constrained to a side-effect-free fixture".to_owned()
                        } else if novelty.novelty_score < 20 {
                            "task is too similar to existing evaluation cases".to_owned()
                        } else if !(30.0..=80.0).contains(&difficulty) {
                            "task is outside the configured difficulty frontier".to_owned()
                        } else {
                            "open-ended task budget is exhausted".to_owned()
                        }
                    }),
                    frontier_ref: Some("capability-frontier-current".to_owned()),
                }
            })
            .collect::<Vec<_>>();
        let recipes = generated_tasks
            .iter()
            .zip(curriculum.iter())
            .filter(|(_, item)| item.selected)
            .map(|(task, _)| environment_recipe(task))
            .collect::<Vec<_>>();
        let frontier = capability_frontier(evaluation);
        let run_id = format!("evolution-run-{}", Uuid::now_v7());
        let selected_task_ids = curriculum
            .iter()
            .filter(|item| item.selected)
            .map(|item| item.task_id.clone())
            .collect::<Vec<_>>();
        let blocked_reason = selected_task_ids
            .is_empty()
            .then(|| "no generated task passed curriculum and safety gates".to_owned());
        let run = OpenEndedRun {
            run_id,
            objective: objective.to_owned(),
            source_scope: "workspace_evaluation_history".to_owned(),
            budget,
            status: if blocked_reason.is_some() {
                OpenEndedRunStatus::Blocked
            } else {
                OpenEndedRunStatus::Planned
            },
            generated_task_ids: generated_tasks.iter().map(|task| task.id.clone()).collect(),
            selected_task_ids,
            promoted_skill_ids: Vec::new(),
            promoted_benchmark_ids: Vec::new(),
            blocked_reason,
            created_at: Utc::now(),
            completed_at: None,
        };
        EvolutionPlan {
            run,
            generated_tasks,
            curriculum,
            novelty,
            recipes,
            frontier,
        }
    }
}

fn novelty_record(task: &GeneratedTask, cases: &[(&str, &str)]) -> NoveltyRecord {
    let task_terms = terms(&task.objective);
    let mut similarities = cases
        .iter()
        .map(|(case_id, objective)| (*case_id, jaccard(&task_terms, &terms(objective))))
        .collect::<Vec<_>>();
    similarities.sort_by(|left, right| right.1.cmp(&left.1));
    let highest_similarity = similarities.first().map(|(_, score)| *score).unwrap_or(0);
    NoveltyRecord {
        task_id: task.id.clone(),
        similar_tasks: similarities
            .into_iter()
            .filter(|(_, score)| *score >= 20)
            .take(5)
            .map(|(case_id, _)| case_id.to_owned())
            .collect(),
        novelty_score: u8::try_from(100_u16.saturating_sub(highest_similarity)).unwrap_or(0),
        duplicate_risk: if highest_similarity >= 80 {
            "high"
        } else if highest_similarity >= 50 {
            "medium"
        } else {
            "low"
        }
        .to_owned(),
        explanation: format!(
            "highest lexical similarity to an existing case is {highest_similarity}%"
        ),
    }
}

fn environment_recipe(task: &GeneratedTask) -> EnvironmentRecipe {
    EnvironmentRecipe {
        recipe_id: format!("recipe-{}", task.id),
        generated_task_id: task.id.clone(),
        repo_ref: "isolated-empty-fixture".to_owned(),
        fixture_refs: vec![task.environment_recipe.clone()],
        dependency_snapshot: "runtime-builtin-tools".to_owned(),
        permission_profile: "fixture-read-write-no-network".to_owned(),
        provider_profile: "deterministic-mock".to_owned(),
        replay_seed: task.source_task_id.to_string(),
    }
}

fn capability_frontier(evaluation: &EvaluationState) -> CapabilityFrontier {
    let mut frontier = CapabilityFrontier {
        mastered: Vec::new(),
        near_miss: Vec::new(),
        failed: Vec::new(),
        blocked: Vec::new(),
        missing_tools: Vec::new(),
        unstable_skills: Vec::new(),
    };
    for result in &evaluation.results {
        let target = match result.verdict {
            EvaluationVerdict::Pass => &mut frontier.mastered,
            EvaluationVerdict::Partial => &mut frontier.near_miss,
            EvaluationVerdict::Fail => &mut frontier.failed,
            EvaluationVerdict::Unknown => &mut frontier.blocked,
        };
        target.push(result.case_id.clone());
        if result
            .failure_taxonomy
            .iter()
            .any(|taxonomy| taxonomy == "ToolFailure")
        {
            frontier.missing_tools.push(result.case_id.clone());
        }
    }
    for values in [
        &mut frontier.mastered,
        &mut frontier.near_miss,
        &mut frontier.failed,
        &mut frontier.blocked,
        &mut frontier.missing_tools,
    ] {
        values.sort();
        values.dedup();
    }
    frontier
}

fn terms(value: &str) -> HashSet<String> {
    value
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .map(str::to_ascii_lowercase)
        .filter(|term| term.len() >= 2)
        .collect()
}

fn jaccard(left: &HashSet<String>, right: &HashSet<String>) -> u16 {
    if left.is_empty() && right.is_empty() {
        return 100;
    }
    let intersection = left.intersection(right).count();
    let union = left.union(right).count().max(1);
    u16::try_from(intersection.saturating_mul(100) / union).unwrap_or(100)
}
