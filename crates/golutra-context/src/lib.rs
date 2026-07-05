use golutra_core::{
    BudgetOverflowAction, TaskId, TokenBudgetSnapshot, TokenBudgetSnapshotId, TokenUsageRecord,
    TurnId,
};
use golutra_llm::{ProviderMessage, ProviderRequest, ProviderRole, ProviderUsage};
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ContextError {
    #[error("context budget exceeded: planned {planned} > limit {limit}")]
    BudgetExceeded { planned: u64, limit: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextContributor {
    pub name: String,
    pub role: ProviderRole,
    pub content: String,
    pub token_budget_hint: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBuildPlan {
    pub contributors: Vec<String>,
    pub messages: Vec<ProviderMessage>,
    pub budget_snapshot: TokenBudgetSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBudgetPolicy {
    pub context_window: u64,
    pub max_output: u64,
    pub budget_limit: u64,
    pub action_if_exceeded: BudgetOverflowAction,
}

impl Default for ContextBudgetPolicy {
    fn default() -> Self {
        Self {
            context_window: 8_192,
            max_output: 1_024,
            budget_limit: 6_000,
            action_if_exceeded: BudgetOverflowAction::Trim,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContextBuilder {
    policy: ContextBudgetPolicy,
}

impl ContextBuilder {
    #[must_use]
    pub fn new(policy: ContextBudgetPolicy) -> Self {
        Self { policy }
    }

    pub fn build(
        &self,
        task_id: TaskId,
        turn_id: TurnId,
        contributors: Vec<ContextContributor>,
    ) -> Result<ContextBuildPlan, ContextError> {
        let planned_input_tokens = contributors
            .iter()
            .map(|contributor| estimate_tokens(&contributor.content))
            .sum::<u64>();
        if planned_input_tokens > self.policy.budget_limit
            && self.policy.action_if_exceeded == BudgetOverflowAction::Block
        {
            return Err(ContextError::BudgetExceeded {
                planned: planned_input_tokens,
                limit: self.policy.budget_limit,
            });
        }

        let messages = contributors
            .iter()
            .map(|contributor| ProviderMessage {
                role: contributor.role,
                content: contributor.content.clone(),
            })
            .collect::<Vec<_>>();
        let contributor_names = contributors
            .into_iter()
            .map(|contributor| contributor.name)
            .collect::<Vec<_>>();

        Ok(ContextBuildPlan {
            contributors: contributor_names,
            messages,
            budget_snapshot: TokenBudgetSnapshot {
                snapshot_id: TokenBudgetSnapshotId::new(),
                task_id,
                turn_id,
                context_window: self.policy.context_window,
                max_output: self.policy.max_output,
                reserved_output_tokens: self.policy.max_output,
                planned_input_tokens,
                planned_tool_tokens: 0,
                planned_summary_tokens: 0,
                budget_limit: self.policy.budget_limit,
                budget_policy: "p0_static_budget".to_owned(),
                action_if_exceeded: self.policy.action_if_exceeded,
            },
        })
    }
}

impl Default for ContextBuilder {
    fn default() -> Self {
        Self::new(ContextBudgetPolicy::default())
    }
}

pub fn provider_request_from_plan(
    plan: &ContextBuildPlan,
    task_id: TaskId,
    turn_id: TurnId,
    provider_id: impl Into<String>,
    model_id: impl Into<String>,
    tools: Vec<String>,
) -> ProviderRequest {
    ProviderRequest {
        request_id: golutra_core::ProviderRequestId::new(),
        task_id,
        turn_id,
        provider_id: provider_id.into(),
        model_id: model_id.into(),
        messages: plan.messages.clone(),
        tools,
    }
}

#[must_use]
pub fn token_usage_record(
    request: &ProviderRequest,
    response_event_id: golutra_core::ProviderResponseId,
    budget_snapshot: &TokenBudgetSnapshot,
    usage: &ProviderUsage,
) -> TokenUsageRecord {
    TokenUsageRecord {
        task_id: request.task_id,
        turn_id: request.turn_id,
        provider_id: request.provider_id.clone(),
        model_id: request.model_id.clone(),
        request_event_id: request.request_id,
        response_event_id,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        tool_result_tokens: Some(0),
        total_tokens: usage.total_tokens,
        estimated_cost: None,
        budget_snapshot_ref: budget_snapshot.snapshot_id,
        attribution_ref: None,
        usage_source: format!("{:?}", usage.usage_source),
    }
}

#[must_use]
pub fn estimate_tokens(content: &str) -> u64 {
    content.chars().count().div_ceil(4) as u64
}

#[cfg(test)]
mod tests {
    use golutra_llm::UsageSource;

    use super::*;

    #[test]
    fn builds_context_plan_with_budget_snapshot() {
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let plan = ContextBuilder::default()
            .build(
                task_id,
                turn_id,
                vec![ContextContributor {
                    name: "objective".to_owned(),
                    role: ProviderRole::User,
                    content: "summarize repository".to_owned(),
                    token_budget_hint: 16,
                }],
            )
            .expect("context builds");

        assert_eq!(plan.contributors, vec!["objective"]);
        assert_eq!(plan.budget_snapshot.task_id, task_id);
        assert!(plan.budget_snapshot.planned_input_tokens > 0);
    }

    #[test]
    fn turns_provider_usage_into_record() {
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let plan = ContextBuilder::default()
            .build(task_id, turn_id, Vec::new())
            .expect("context builds");
        let request =
            provider_request_from_plan(&plan, task_id, turn_id, "mock", "mock-model", Vec::new());
        let usage = ProviderUsage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            reasoning_tokens: Some(0),
            cached_input_tokens: Some(0),
            total_tokens: Some(15),
            usage_source: UsageSource::Provider,
            raw: serde_json::json!({}),
        };

        let record = token_usage_record(
            &request,
            golutra_core::ProviderResponseId::new(),
            &plan.budget_snapshot,
            &usage,
        );

        assert_eq!(record.total_tokens, Some(15));
        assert_eq!(record.budget_snapshot_ref, plan.budget_snapshot.snapshot_id);
    }
}
