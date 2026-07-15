use chrono::{DateTime, Utc};
use golutra_core::{EvidenceId, SessionId, TaskId};
use golutra_eval::{CapabilityFrontier, CurriculumItem, GeneratedTask};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpenEndedRunStatus {
    Planned,
    Running,
    Completed,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OpenEndedBudget {
    pub max_generated_tasks: u32,
    pub max_selected_tasks: u32,
    pub max_tool_calls_per_task: u32,
    pub max_runtime_ms_per_task: u64,
}

impl Default for OpenEndedBudget {
    fn default() -> Self {
        Self {
            max_generated_tasks: 20,
            max_selected_tasks: 3,
            max_tool_calls_per_task: 8,
            max_runtime_ms_per_task: 120_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EnvironmentRecipe {
    pub recipe_id: String,
    pub generated_task_id: String,
    pub repo_ref: String,
    pub fixture_refs: Vec<String>,
    pub dependency_snapshot: String,
    pub permission_profile: String,
    pub provider_profile: String,
    pub replay_seed: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NoveltyRecord {
    pub task_id: String,
    pub similar_tasks: Vec<String>,
    pub novelty_score: u8,
    pub duplicate_risk: String,
    pub explanation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OpenEndedRun {
    pub run_id: String,
    pub objective: String,
    pub source_scope: String,
    pub budget: OpenEndedBudget,
    pub status: OpenEndedRunStatus,
    pub generated_task_ids: Vec<String>,
    pub selected_task_ids: Vec<String>,
    pub promoted_skill_ids: Vec<String>,
    pub promoted_benchmark_ids: Vec<String>,
    pub blocked_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GeneratedTaskExecution {
    pub execution_id: String,
    pub run_id: String,
    pub generated_task_id: String,
    pub runtime_session_id: SessionId,
    pub runtime_task_id: Option<TaskId>,
    pub sandbox_workspace: String,
    pub status: String,
    pub verification_ref: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillLifecycleStatus {
    Proposed,
    Reviewed,
    Rejected,
    Installed,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillManifest {
    pub skill_id: String,
    pub name: String,
    pub description: String,
    pub source_task_id: TaskId,
    pub source_trajectory: String,
    pub prerequisites: Vec<String>,
    pub steps: Vec<String>,
    pub failure_cases: Vec<String>,
    pub evidence_refs: Vec<EvidenceId>,
    pub regression_refs: Vec<String>,
    pub scope: String,
    pub rollback_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillLifecycleRecord {
    pub manifest: SkillManifest,
    pub status: SkillLifecycleStatus,
    pub candidate_path: String,
    pub installed_path: Option<String>,
    pub checksum: String,
    pub reviewer: Option<String>,
    pub review_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub reviewed_at: Option<DateTime<Utc>>,
    pub installed_at: Option<DateTime<Utc>>,
    pub rolled_back_at: Option<DateTime<Utc>>,
    pub rollback_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EvolutionState {
    pub runs: Vec<OpenEndedRun>,
    pub generated_tasks: Vec<GeneratedTask>,
    pub curriculum: Vec<CurriculumItem>,
    pub novelty: Vec<NoveltyRecord>,
    pub recipes: Vec<EnvironmentRecipe>,
    pub executions: Vec<GeneratedTaskExecution>,
    pub frontier: Option<CapabilityFrontier>,
    pub skills: Vec<SkillLifecycleRecord>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvolutionPlan {
    pub run: OpenEndedRun,
    pub generated_tasks: Vec<GeneratedTask>,
    pub curriculum: Vec<CurriculumItem>,
    pub novelty: Vec<NoveltyRecord>,
    pub recipes: Vec<EnvironmentRecipe>,
    pub frontier: CapabilityFrontier,
}
