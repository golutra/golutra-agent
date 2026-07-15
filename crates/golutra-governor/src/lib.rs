use std::collections::HashSet;

use golutra_core::{EvidenceId, PolicyDecision, TaskId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GoalLedger {
    pub task_id: TaskId,
    pub original_objective: String,
    pub success_criteria: Vec<String>,
    pub current_plan: Vec<String>,
    pub completed_steps: Vec<String>,
    pub open_risks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GoalAlignmentCheck {
    pub task_id: TaskId,
    pub aligned: bool,
    pub alignment_score: u8,
    pub drift_type: Option<String>,
    pub reason: String,
    pub evidence_refs: Vec<EvidenceId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GovernorAction {
    Allow,
    Warn,
    AskUser,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GovernorPhase {
    Provider,
    Tool,
    ToolResult,
    Completion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GovernorLimits {
    pub max_iterations: u32,
    pub max_tool_calls: u32,
    pub max_failed_tool_calls: u32,
    pub max_planned_input_tokens: u64,
    pub max_elapsed_ms: u64,
    pub max_estimated_cost_microusd: u64,
}

impl Default for GovernorLimits {
    fn default() -> Self {
        Self {
            max_iterations: 4,
            max_tool_calls: 16,
            max_failed_tool_calls: 2,
            max_planned_input_tokens: 96_000,
            max_elapsed_ms: 10 * 60 * 1_000,
            max_estimated_cost_microusd: 5_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GovernorObservation {
    pub phase: GovernorPhase,
    pub iteration: u32,
    pub tool_calls: u32,
    pub failed_tool_calls: u32,
    pub planned_input_tokens: u64,
    pub elapsed_ms: u64,
    pub latest_action: String,
    pub estimated_cost_microusd: Option<u64>,
    pub policy_decision: Option<PolicyDecision>,
    pub security_risk: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeGovernorDecision {
    pub task_id: TaskId,
    pub phase: GovernorPhase,
    pub action: GovernorAction,
    pub reason: String,
    pub budget_risk: String,
    pub security_risk: String,
    pub iteration: u32,
    pub tool_calls: u32,
    pub failed_tool_calls: u32,
    pub alignment: GoalAlignmentCheck,
}

impl RuntimeGovernorDecision {
    #[must_use]
    pub fn permits_execution(&self) -> bool {
        matches!(self.action, GovernorAction::Allow | GovernorAction::Warn)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeGovernor {
    limits: GovernorLimits,
}

impl RuntimeGovernor {
    #[must_use]
    pub fn new(limits: GovernorLimits) -> Self {
        Self { limits }
    }

    #[must_use]
    pub fn limits(&self) -> &GovernorLimits {
        &self.limits
    }

    #[must_use]
    pub fn evaluate(
        &self,
        ledger: &GoalLedger,
        observation: &GovernorObservation,
    ) -> RuntimeGovernorDecision {
        let alignment = check_goal_alignment(ledger, &observation.latest_action);
        let normalized_security_risk = observation.security_risk.trim().to_ascii_lowercase();
        let (action, reason, budget_risk) = if ledger.original_objective.trim().is_empty() {
            (GovernorAction::Block, "runtime objective is empty", "low")
        } else if observation.policy_decision == Some(PolicyDecision::Block)
            || normalized_security_risk == "critical"
        {
            (
                GovernorAction::Block,
                "runtime security or policy boundary rejected the action",
                "low",
            )
        } else if normalized_security_risk == "high" {
            (
                GovernorAction::AskUser,
                "runtime action has high security risk and requires explicit review",
                "low",
            )
        } else if observation
            .estimated_cost_microusd
            .is_some_and(|cost| cost > self.limits.max_estimated_cost_microusd)
        {
            (
                GovernorAction::AskUser,
                "runtime estimated cost exceeds the configured budget",
                "exceeded",
            )
        } else if observation.iteration > self.limits.max_iterations {
            (
                GovernorAction::Block,
                "runtime iteration budget exceeded",
                "exceeded",
            )
        } else if observation.tool_calls > self.limits.max_tool_calls {
            (
                GovernorAction::Block,
                "runtime tool-call budget exceeded",
                "exceeded",
            )
        } else if observation.failed_tool_calls > 0
            && observation.failed_tool_calls >= self.limits.max_failed_tool_calls
            && observation.phase == GovernorPhase::ToolResult
        {
            (
                GovernorAction::Block,
                "runtime failed-tool budget reached",
                "exceeded",
            )
        } else if observation.planned_input_tokens > self.limits.max_planned_input_tokens {
            (
                GovernorAction::AskUser,
                "planned provider input exceeds the runtime token budget",
                "exceeded",
            )
        } else if observation.elapsed_ms > self.limits.max_elapsed_ms {
            (
                GovernorAction::AskUser,
                "runtime wall-clock budget exceeded",
                "exceeded",
            )
        } else if observation.policy_decision == Some(PolicyDecision::Deny) {
            (
                GovernorAction::Warn,
                "policy denied the requested action; the loop may recover with a safer action",
                "low",
            )
        } else if !alignment.aligned
            && matches!(
                observation.phase,
                GovernorPhase::Tool | GovernorPhase::Completion
            )
        {
            (
                GovernorAction::Warn,
                "action has weak lexical alignment with the goal; execution remains auditable",
                "low",
            )
        } else {
            (
                GovernorAction::Allow,
                "runtime action is within goal and budget boundaries",
                "low",
            )
        };
        let budget_risk = if budget_risk == "low"
            && (observation.iteration.saturating_mul(100)
                >= self.limits.max_iterations.saturating_mul(80)
                || observation.tool_calls.saturating_mul(100)
                    >= self.limits.max_tool_calls.saturating_mul(80)
                || observation.estimated_cost_microusd.is_some_and(|cost| {
                    cost.saturating_mul(100)
                        >= self.limits.max_estimated_cost_microusd.saturating_mul(80)
                })) {
            "high"
        } else {
            budget_risk
        };
        RuntimeGovernorDecision {
            task_id: ledger.task_id,
            phase: observation.phase,
            action,
            reason: reason.to_owned(),
            budget_risk: budget_risk.to_owned(),
            security_risk: if normalized_security_risk.is_empty() {
                "unknown".to_owned()
            } else {
                normalized_security_risk
            },
            iteration: observation.iteration,
            tool_calls: observation.tool_calls,
            failed_tool_calls: observation.failed_tool_calls,
            alignment,
        }
    }
}

impl Default for RuntimeGovernor {
    fn default() -> Self {
        Self::new(GovernorLimits::default())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VerificationTier {
    pub name: String,
    pub required_checks: Vec<String>,
    pub blocks_success: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventSamplingPolicy {
    pub debug_sample_rate: u8,
    pub evaluation_sample_rate: u8,
    pub always_keep_event_types: Vec<String>,
}

#[must_use]
pub fn check_goal_alignment(ledger: &GoalLedger, latest_action: &str) -> GoalAlignmentCheck {
    let goal_terms = terms(
        std::iter::once(ledger.original_objective.as_str())
            .chain(ledger.success_criteria.iter().map(String::as_str))
            .chain(ledger.current_plan.iter().map(String::as_str)),
    );
    let action_terms = terms(std::iter::once(latest_action));
    let matching_terms = goal_terms.intersection(&action_terms).count();
    let denominator = goal_terms.len().min(action_terms.len()).max(1);
    let score = u8::try_from(matching_terms.saturating_mul(100) / denominator).unwrap_or(100);
    let aligned = !latest_action.trim().is_empty() && (goal_terms.is_empty() || score >= 20);
    GoalAlignmentCheck {
        task_id: ledger.task_id,
        aligned,
        alignment_score: score,
        drift_type: (!aligned).then(|| "weak_goal_alignment".to_owned()),
        reason: if aligned {
            "latest action shares meaningful terms with the objective or plan".to_owned()
        } else {
            "latest action has no meaningful lexical overlap with the objective or plan".to_owned()
        },
        evidence_refs: Vec::new(),
    }
}

fn terms<'a>(values: impl Iterator<Item = &'a str>) -> HashSet<String> {
    const STOP_WORDS: &[&str] = &[
        "a", "an", "and", "for", "in", "of", "on", "or", "the", "to", "with", "use", "using",
    ];
    values
        .flat_map(|value| {
            value
                .split(|character: char| {
                    !character.is_alphanumeric() && character != '_' && character != '-'
                })
                .map(str::to_lowercase)
                .collect::<Vec<_>>()
        })
        .filter(|term| term.chars().count() >= 2)
        .filter(|term| !STOP_WORDS.contains(&term.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> GoalLedger {
        GoalLedger {
            task_id: TaskId::new(),
            original_objective: "implement runtime cancellation".to_owned(),
            success_criteria: vec!["cancellation tests pass".to_owned()],
            current_plan: vec!["add cancellation token".to_owned()],
            completed_steps: Vec::new(),
            open_risks: Vec::new(),
        }
    }

    #[test]
    fn detects_goal_alignment_from_plan_step() {
        let check = check_goal_alignment(&ledger(), "add cancellation token tests");

        assert!(check.aligned);
        assert!(check.alignment_score >= 20);
    }

    #[test]
    fn blocks_iteration_and_tool_budgets() {
        let governor = RuntimeGovernor::new(GovernorLimits {
            max_iterations: 1,
            max_tool_calls: 1,
            ..GovernorLimits::default()
        });
        let decision = governor.evaluate(
            &ledger(),
            &GovernorObservation {
                phase: GovernorPhase::Provider,
                iteration: 2,
                tool_calls: 1,
                failed_tool_calls: 0,
                planned_input_tokens: 10,
                elapsed_ms: 1,
                latest_action: "implement runtime cancellation".to_owned(),
                estimated_cost_microusd: None,
                policy_decision: None,
                security_risk: "low".to_owned(),
            },
        );

        assert_eq!(decision.action, GovernorAction::Block);
        assert!(!decision.permits_execution());
    }

    #[test]
    fn asks_user_when_token_budget_is_exceeded() {
        let governor = RuntimeGovernor::new(GovernorLimits {
            max_planned_input_tokens: 10,
            ..GovernorLimits::default()
        });
        let decision = governor.evaluate(
            &ledger(),
            &GovernorObservation {
                phase: GovernorPhase::Provider,
                iteration: 1,
                tool_calls: 0,
                failed_tool_calls: 0,
                planned_input_tokens: 11,
                elapsed_ms: 1,
                latest_action: "implement runtime cancellation".to_owned(),
                estimated_cost_microusd: None,
                policy_decision: None,
                security_risk: "low".to_owned(),
            },
        );

        assert_eq!(decision.action, GovernorAction::AskUser);
    }

    #[test]
    fn weak_alignment_warns_without_blocking_safe_execution() {
        let decision = RuntimeGovernor::default().evaluate(
            &ledger(),
            &GovernorObservation {
                phase: GovernorPhase::Tool,
                iteration: 1,
                tool_calls: 1,
                failed_tool_calls: 0,
                planned_input_tokens: 10,
                elapsed_ms: 1,
                latest_action: "inspect unrelated marketing assets".to_owned(),
                estimated_cost_microusd: None,
                policy_decision: None,
                security_risk: "low".to_owned(),
            },
        );

        assert_eq!(decision.action, GovernorAction::Warn);
        assert!(decision.permits_execution());
    }

    #[test]
    fn blocks_policy_rejection_and_asks_for_high_cost_review() {
        let governor = RuntimeGovernor::new(GovernorLimits {
            max_estimated_cost_microusd: 100,
            ..GovernorLimits::default()
        });
        let blocked = governor.evaluate(
            &ledger(),
            &GovernorObservation {
                phase: GovernorPhase::ToolResult,
                iteration: 1,
                tool_calls: 1,
                failed_tool_calls: 0,
                planned_input_tokens: 10,
                elapsed_ms: 1,
                latest_action: "write runtime file".to_owned(),
                estimated_cost_microusd: Some(10),
                policy_decision: Some(PolicyDecision::Block),
                security_risk: "high".to_owned(),
            },
        );
        let costly = governor.evaluate(
            &ledger(),
            &GovernorObservation {
                phase: GovernorPhase::Provider,
                iteration: 1,
                tool_calls: 0,
                failed_tool_calls: 0,
                planned_input_tokens: 10,
                elapsed_ms: 1,
                latest_action: "implement runtime cancellation".to_owned(),
                estimated_cost_microusd: Some(101),
                policy_decision: None,
                security_risk: "low".to_owned(),
            },
        );

        assert_eq!(blocked.action, GovernorAction::Block);
        assert_eq!(blocked.security_risk, "high");
        assert_eq!(costly.action, GovernorAction::AskUser);
        assert_eq!(costly.budget_risk, "exceeded");
    }
}
