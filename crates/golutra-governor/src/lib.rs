use golutra_core::{EvidenceId, TaskId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GoalLedger {
    pub task_id: TaskId,
    pub original_objective: String,
    pub current_plan: Vec<String>,
    pub completed_steps: Vec<String>,
    pub open_risks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GoalAlignmentCheck {
    pub task_id: TaskId,
    pub aligned: bool,
    pub reason: String,
    pub evidence_refs: Vec<EvidenceId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeGovernorDecision {
    pub task_id: TaskId,
    pub action: String,
    pub reason: String,
    pub budget_risk: String,
    pub security_risk: String,
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
    let aligned = latest_action.contains(&ledger.original_objective)
        || ledger
            .current_plan
            .iter()
            .any(|step| latest_action.contains(step));
    GoalAlignmentCheck {
        task_id: ledger.task_id,
        aligned,
        reason: if aligned {
            "latest action references the objective or current plan".to_owned()
        } else {
            "latest action does not reference the objective or current plan".to_owned()
        },
        evidence_refs: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_goal_alignment_from_plan_step() {
        let ledger = GoalLedger {
            task_id: TaskId::new(),
            original_objective: "implement runtime".to_owned(),
            current_plan: vec!["add tests".to_owned()],
            completed_steps: Vec::new(),
            open_risks: Vec::new(),
        };

        let check = check_goal_alignment(&ledger, "add tests for runtime");

        assert!(check.aligned);
    }
}
