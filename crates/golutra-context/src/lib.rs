use golutra_core::{
    BudgetOverflowAction, ContextContributorSnapshot, ContextMessageSnapshot, ContextSnapshot,
    ContextSnapshotId, SessionId, TaskId, TokenBudgetSnapshot, TokenBudgetSnapshotId,
    TokenUsageRecord, ToolContract, TurnId,
};
use golutra_llm::{ProviderMessage, ProviderRequest, ProviderRole, ProviderUsage};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ContextError {
    #[error("context budget exceeded: planned {planned} > limit {limit}")]
    BudgetExceeded { planned: u64, limit: u64 },
    #[error("context budget requires user action: planned {planned} > limit {limit}")]
    UserActionRequired { planned: u64, limit: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextContributor {
    pub name: String,
    pub role: ProviderRole,
    pub content: String,
    pub token_budget_hint: u64,
    pub source_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBuildPlan {
    pub contributors: Vec<String>,
    pub contributor_manifest: Vec<ContextContributorSnapshot>,
    pub messages: Vec<ProviderMessage>,
    pub budget_snapshot: TokenBudgetSnapshot,
    pub original_planned_input_tokens: u64,
    pub trimmed_contributors: Vec<String>,
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
        mut contributors: Vec<ContextContributor>,
    ) -> Result<ContextBuildPlan, ContextError> {
        let original_contributor_tokens = contributors
            .iter()
            .map(|contributor| estimate_tokens(&contributor.content))
            .collect::<Vec<_>>();
        let original_planned_input_tokens = contributors
            .iter()
            .map(|contributor| estimate_tokens(&contributor.content))
            .sum::<u64>();
        let mut trimmed_contributors = Vec::new();
        if original_planned_input_tokens > self.policy.budget_limit {
            match self.policy.action_if_exceeded {
                BudgetOverflowAction::Block => {
                    return Err(ContextError::BudgetExceeded {
                        planned: original_planned_input_tokens,
                        limit: self.policy.budget_limit,
                    });
                }
                BudgetOverflowAction::AskUser => {
                    return Err(ContextError::UserActionRequired {
                        planned: original_planned_input_tokens,
                        limit: self.policy.budget_limit,
                    });
                }
                BudgetOverflowAction::Trim | BudgetOverflowAction::Compact => {
                    trimmed_contributors =
                        trim_contributors(&mut contributors, self.policy.budget_limit);
                }
            }
        }
        let planned_input_tokens = contributors
            .iter()
            .map(|contributor| estimate_tokens(&contributor.content))
            .sum::<u64>();
        if planned_input_tokens > self.policy.budget_limit {
            return Err(ContextError::BudgetExceeded {
                planned: planned_input_tokens,
                limit: self.policy.budget_limit,
            });
        }

        let contributor_manifest = contributors
            .iter()
            .enumerate()
            .map(|(index, contributor)| {
                let retained_estimated_tokens = estimate_tokens(&contributor.content);
                let trimmed = trimmed_contributors
                    .iter()
                    .any(|name| name == &contributor.name);
                ContextContributorSnapshot {
                    name: contributor.name.clone(),
                    role: format!("{:?}", contributor.role).to_lowercase(),
                    source_refs: if contributor.source_refs.is_empty() {
                        vec![format!("contributor:{}", contributor.name)]
                    } else {
                        contributor.source_refs.clone()
                    },
                    included: true,
                    trimmed,
                    original_estimated_tokens: original_contributor_tokens[index],
                    retained_estimated_tokens,
                    strategy: if trimmed {
                        if matches!(contributor.name.as_str(), "conversation_history" | "memory") {
                            "retain_tail".to_owned()
                        } else {
                            "retain_head".to_owned()
                        }
                    } else {
                        "include_full".to_owned()
                    },
                    estimated_tokens: retained_estimated_tokens,
                    content_digest: digest_bytes(contributor.content.as_bytes()),
                    redacted_content_ref: None,
                    invalidation_refs: Vec::new(),
                }
            })
            .collect::<Vec<_>>();
        let messages = contributors
            .iter()
            .map(|contributor| ProviderMessage {
                role: contributor.role,
                content: contributor.content.clone(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            })
            .collect::<Vec<_>>();
        let contributor_names = contributors
            .into_iter()
            .map(|contributor| contributor.name)
            .collect::<Vec<_>>();

        Ok(ContextBuildPlan {
            contributors: contributor_names,
            contributor_manifest,
            messages,
            original_planned_input_tokens,
            trimmed_contributors,
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

fn trim_contributors(contributors: &mut [ContextContributor], budget_limit: u64) -> Vec<String> {
    let mut trimmed = Vec::new();
    for contributor in contributors.iter_mut() {
        let current_tokens = estimate_tokens(&contributor.content);
        if contributor.token_budget_hint > 0 && current_tokens > contributor.token_budget_hint {
            contributor.content = truncate_contributor(
                &contributor.name,
                &contributor.content,
                contributor.token_budget_hint,
            );
            trimmed.push(contributor.name.clone());
        }
    }

    const TRIM_PRIORITY: &[&str] = &[
        "project_skills",
        "memory",
        "conversation_history",
        "environment_context",
        "system",
        "objective",
    ];
    for name in TRIM_PRIORITY {
        let total = contributors
            .iter()
            .map(|contributor| estimate_tokens(&contributor.content))
            .sum::<u64>();
        if total <= budget_limit {
            break;
        }
        if let Some(contributor) = contributors
            .iter_mut()
            .find(|contributor| contributor.name == *name)
        {
            let current_tokens = estimate_tokens(&contributor.content);
            let overflow = total.saturating_sub(budget_limit);
            let target = current_tokens.saturating_sub(overflow).max(16);
            contributor.content = truncate_contributor(name, &contributor.content, target);
            if !trimmed.iter().any(|trimmed_name| trimmed_name == name) {
                trimmed.push((*name).to_owned());
            }
        }
    }
    trimmed
}

fn truncate_contributor(name: &str, content: &str, token_limit: u64) -> String {
    let character_limit = usize::try_from(token_limit.saturating_mul(4)).unwrap_or(usize::MAX);
    let characters = content.chars().collect::<Vec<_>>();
    if characters.len() <= character_limit {
        return content.to_owned();
    }
    if matches!(name, "conversation_history" | "memory") {
        characters[characters.len().saturating_sub(character_limit)..]
            .iter()
            .collect()
    } else {
        characters[..character_limit].iter().collect()
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
    tools: Vec<ToolContract>,
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
pub fn context_snapshot_from_request(
    session_id: SessionId,
    plan: &ContextBuildPlan,
    request: &ProviderRequest,
) -> ContextSnapshot {
    let message_manifest = request
        .messages
        .iter()
        .enumerate()
        .map(|(index, message)| ContextMessageSnapshot {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            role: format!("{:?}", message.role).to_lowercase(),
            content_digest: digest_bytes(message.content.as_bytes()),
            estimated_tokens: estimate_tokens(&message.content),
            tool_call_ids: message
                .tool_calls
                .iter()
                .map(|call| call.tool_call_id.clone())
                .collect(),
        })
        .collect();
    let tool_schema_digests = request
        .tools
        .iter()
        .filter_map(|tool| serde_json::to_vec(tool).ok())
        .map(|bytes| digest_bytes(&bytes))
        .collect();
    let canonical_request_digest = serde_json::to_vec(request)
        .map(|bytes| digest_bytes(&bytes))
        .unwrap_or_else(|_| digest_bytes(request.provider_id.as_bytes()));
    ContextSnapshot {
        snapshot_id: ContextSnapshotId::new(),
        session_id,
        task_id: request.task_id,
        turn_id: request.turn_id,
        provider_request_id: request.request_id,
        provider_id: request.provider_id.clone(),
        model_id: request.model_id.clone(),
        contributor_manifest: plan.contributor_manifest.clone(),
        message_manifest,
        tool_schema_digests,
        generation_config_digest: None,
        budget_snapshot: plan.budget_snapshot.clone(),
        canonical_request_digest,
        redacted_request_artifact_ref: None,
        restricted_request_artifact_ref: None,
        estimate_source: "character_div_4".to_owned(),
        created_at: chrono::Utc::now(),
    }
}

fn digest_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{:x}", digest)
}

#[must_use]
pub fn token_usage_record(
    request: &ProviderRequest,
    response_event_id: golutra_core::ProviderResponseId,
    budget_snapshot: &TokenBudgetSnapshot,
    usage: &ProviderUsage,
    cost_model: &str,
) -> TokenUsageRecord {
    let system_prompt_tokens = message_tokens(request, ProviderRole::System);
    let user_message_tokens = message_tokens(request, ProviderRole::User);
    let assistant_recent_tokens = message_tokens(request, ProviderRole::Assistant);
    let tool_result_tokens = message_tokens(request, ProviderRole::Tool);
    let total_tokens = usage.total_tokens.or_else(|| {
        usage
            .input_tokens
            .zip(usage.output_tokens)
            .map(|(input, output)| input.saturating_add(output))
    });
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
        tool_result_tokens: Some(tool_result_tokens),
        total_tokens,
        estimated_cost: (cost_model == "zero").then_some(0.0),
        budget_snapshot_ref: budget_snapshot.snapshot_id,
        attribution_ref: Some(golutra_core::TokenAttribution {
            system_prompt_tokens: Some(system_prompt_tokens),
            developer_instruction_tokens: None,
            runtime_context_tokens: None,
            policy_tokens: None,
            user_message_tokens: Some(user_message_tokens),
            assistant_recent_tokens: Some(assistant_recent_tokens),
            working_summary_tokens: None,
            memory_tokens: None,
            evidence_tokens: None,
            tool_instruction_tokens: None,
            tool_result_excerpt_tokens: Some(tool_result_tokens),
            output_tokens: usage.output_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            source: match usage.usage_source {
                golutra_core::UsageSource::Provider => "mixed",
                golutra_core::UsageSource::Estimated => "tokenizer",
                golutra_core::UsageSource::Unknown => "tokenizer",
            }
            .to_owned(),
        }),
        usage_source: match usage.usage_source {
            golutra_core::UsageSource::Provider => "provider",
            golutra_core::UsageSource::Estimated => "estimated",
            golutra_core::UsageSource::Unknown => "unknown",
        }
        .to_owned(),
    }
}

fn message_tokens(request: &ProviderRequest, role: ProviderRole) -> u64 {
    request
        .messages
        .iter()
        .filter(|message| message.role == role)
        .map(|message| estimate_tokens(&message.content))
        .fold(0_u64, u64::saturating_add)
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
                    source_refs: Vec::new(),
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
            "zero",
        );

        assert_eq!(record.total_tokens, Some(15));
        assert_eq!(record.estimated_cost, Some(0.0));
        assert_eq!(record.usage_source, "provider");
        assert_eq!(record.tool_result_tokens, Some(0));
        assert_eq!(
            record
                .attribution_ref
                .as_ref()
                .map(|value| value.source.as_str()),
            Some("mixed")
        );
        assert_eq!(record.budget_snapshot_ref, plan.budget_snapshot.snapshot_id);
    }

    #[test]
    fn trims_oversized_history_to_the_declared_budget() {
        let plan = ContextBuilder::new(ContextBudgetPolicy {
            context_window: 128,
            max_output: 16,
            budget_limit: 32,
            action_if_exceeded: BudgetOverflowAction::Trim,
        })
        .build(
            TaskId::new(),
            TurnId::new(),
            vec![ContextContributor {
                name: "conversation_history".to_owned(),
                role: ProviderRole::System,
                content: format!("old {} latest", "history ".repeat(80)),
                token_budget_hint: 24,
                source_refs: Vec::new(),
            }],
        )
        .expect("context trims");

        assert!(plan.budget_snapshot.planned_input_tokens <= 32);
        assert_eq!(plan.trimmed_contributors, vec!["conversation_history"]);
        assert!(plan.messages[0].content.ends_with("latest"));
        assert!(plan.original_planned_input_tokens > plan.budget_snapshot.planned_input_tokens);
    }

    #[test]
    fn rejects_a_budget_that_cannot_retain_minimum_contributor_context() {
        let result = ContextBuilder::new(ContextBudgetPolicy {
            context_window: 32,
            max_output: 16,
            budget_limit: 1,
            action_if_exceeded: BudgetOverflowAction::Trim,
        })
        .build(
            TaskId::new(),
            TurnId::new(),
            vec![ContextContributor {
                name: "objective".to_owned(),
                role: ProviderRole::User,
                content: "a deliberately oversized objective".repeat(20),
                token_budget_hint: 16,
                source_refs: Vec::new(),
            }],
        );

        assert!(matches!(result, Err(ContextError::BudgetExceeded { .. })));
    }
}
