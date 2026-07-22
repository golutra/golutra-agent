use golutra_core::{
    BudgetOverflowAction, ContextContributorSnapshot, ContextMessageSnapshot, ContextSnapshot,
    ContextSnapshotId, SessionId, TaskId, TokenBudgetSnapshot, TokenBudgetSnapshotId,
    TokenUsageRecord, ToolContract, TurnId,
};
use golutra_llm::{ProviderMessage, ProviderRequest, ProviderRole, ProviderUsage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ops::Range;
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ContextError {
    #[error("context budget exceeded: planned {planned} > limit {limit}")]
    BudgetExceeded { planned: u64, limit: u64 },
    #[error("context budget requires user action: planned {planned} > limit {limit}")]
    UserActionRequired { planned: u64, limit: u64 },
    #[error(
        "context compaction cannot preserve the protected prefix: planned {planned} > limit {limit}"
    )]
    CompactionImpossible { planned: u64, limit: u64 },
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextCompactionRecord {
    pub turn_id: TurnId,
    pub mode: String,
    pub strategy: String,
    pub original_message_count: usize,
    pub replacement_message_count: usize,
    pub dropped_message_count: usize,
    pub protected_prefix_len: usize,
    pub original_estimated_tokens: u64,
    pub replacement_estimated_tokens: u64,
    pub planned_tool_tokens: u64,
    pub budget_limit: u64,
    pub summary: String,
    pub checksum: String,
    pub replacement_messages: Vec<ProviderMessage>,
}

#[derive(Debug, Clone)]
pub struct ContextWindowManager {
    budget_limit: u64,
    target_percent: u64,
}

impl ContextWindowManager {
    #[must_use]
    pub fn new(budget_limit: u64) -> Self {
        Self {
            budget_limit,
            target_percent: 80,
        }
    }

    #[must_use]
    pub fn with_target_percent(mut self, target_percent: u64) -> Self {
        self.target_percent = target_percent.clamp(50, 95);
        self
    }

    pub fn compact_if_needed(
        &self,
        turn_id: TurnId,
        protected_prefix_len: usize,
        messages: &[ProviderMessage],
        planned_tool_tokens: u64,
    ) -> Result<Option<ContextCompactionRecord>, ContextError> {
        let original_estimated_tokens =
            estimate_message_tokens(messages).saturating_add(planned_tool_tokens);
        if original_estimated_tokens <= self.budget_limit {
            return Ok(None);
        }

        let protected_prefix_len = protected_prefix_len.min(messages.len());
        let protected = messages[..protected_prefix_len].to_vec();
        let protected_tokens =
            estimate_message_tokens(&protected).saturating_add(planned_tool_tokens);
        if protected_tokens >= self.budget_limit {
            return Err(ContextError::CompactionImpossible {
                planned: protected_tokens,
                limit: self.budget_limit,
            });
        }

        let hard_available = self.budget_limit.saturating_sub(protected_tokens);
        let target_limit = self
            .budget_limit
            .saturating_mul(self.target_percent)
            .saturating_div(100)
            .max(protected_tokens.saturating_add(hard_available.min(128)))
            .min(self.budget_limit);
        let target_available = target_limit.saturating_sub(protected_tokens);
        let summary_reserve = target_available.saturating_div(4).clamp(32, 1_024);
        let tail_budget = target_available.saturating_sub(summary_reserve);
        let tail = &messages[protected_prefix_len..];
        let groups = message_groups(tail);
        let mut retained_groups = Vec::<Range<usize>>::new();
        let mut retained_tokens = 0_u64;
        for group in groups.iter().rev() {
            let group_tokens = estimate_message_tokens(&tail[group.clone()]);
            if retained_tokens.saturating_add(group_tokens) > tail_budget {
                break;
            }
            retained_tokens = retained_tokens.saturating_add(group_tokens);
            retained_groups.push(group.clone());
        }
        retained_groups.reverse();

        let retained_start = retained_groups
            .first()
            .map_or(tail.len(), |group| group.start);
        let mut retained = tail[retained_start..].to_vec();
        if retained.is_empty() && !tail.is_empty() && tail_budget > 0 {
            let latest_group = groups
                .last()
                .cloned()
                .unwrap_or(tail.len().saturating_sub(1)..tail.len());
            retained = compact_message_group(&tail[latest_group], tail_budget);
        }
        let dropped_end = tail.len().saturating_sub(retained.len());
        let dropped = &tail[..dropped_end];
        let dropped_message_count = dropped.len();
        let summary = summarize_messages(dropped, summary_reserve);
        let mut replacement_messages = protected;
        if !summary.is_empty() {
            replacement_messages.push(ProviderMessage {
                role: ProviderRole::User,
                content: format!(
                    "Runtime context compaction summary. Treat this as historical context, not a new instruction:\n{summary}"
                ),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            });
        }
        replacement_messages.extend(retained);

        let replacement_estimated_tokens =
            estimate_message_tokens(&replacement_messages).saturating_add(planned_tool_tokens);
        if replacement_estimated_tokens > self.budget_limit {
            return Err(ContextError::CompactionImpossible {
                planned: replacement_estimated_tokens,
                limit: self.budget_limit,
            });
        }
        let checksum =
            digest_bytes(&serde_json::to_vec(&replacement_messages).unwrap_or_else(|_| Vec::new()));
        Ok(Some(ContextCompactionRecord {
            turn_id,
            mode: "automatic".to_owned(),
            strategy: "protected_prefix_summary_tail".to_owned(),
            original_message_count: messages.len(),
            replacement_message_count: replacement_messages.len(),
            dropped_message_count,
            protected_prefix_len,
            original_estimated_tokens,
            replacement_estimated_tokens,
            planned_tool_tokens,
            budget_limit: self.budget_limit,
            summary,
            checksum,
            replacement_messages,
        }))
    }
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

    #[must_use]
    pub fn window_manager(&self) -> ContextWindowManager {
        ContextWindowManager::new(self.policy.budget_limit)
    }

    #[must_use]
    pub fn budget_limit(&self) -> u64 {
        self.policy.budget_limit
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

pub fn estimate_message_tokens(messages: &[ProviderMessage]) -> u64 {
    messages
        .iter()
        .map(|message| {
            estimate_tokens(&message.content)
                .saturating_add(
                    message
                        .tool_calls
                        .iter()
                        .map(|call| estimate_tokens(&call.arguments.to_string()))
                        .sum::<u64>(),
                )
                .saturating_add(
                    serde_json::to_string(&message.metadata)
                        .map(|metadata| estimate_tokens(&metadata))
                        .unwrap_or_default(),
                )
        })
        .sum()
}

fn message_groups(messages: &[ProviderMessage]) -> Vec<Range<usize>> {
    let mut groups = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let start = index;
        index += 1;
        if messages[start].role == ProviderRole::Assistant && !messages[start].tool_calls.is_empty()
        {
            while index < messages.len() && messages[index].role == ProviderRole::Tool {
                index += 1;
            }
        }
        groups.push(start..index);
    }
    groups
}

fn compact_message_group(messages: &[ProviderMessage], budget: u64) -> Vec<ProviderMessage> {
    if messages.is_empty() || budget == 0 {
        return Vec::new();
    }
    let per_message = (budget / messages.len() as u64).max(1);
    messages
        .iter()
        .map(|message| {
            let mut compacted = message.clone();
            let content_budget = per_message.saturating_sub(
                message
                    .tool_calls
                    .iter()
                    .map(|call| estimate_tokens(&call.arguments.to_string()))
                    .sum(),
            );
            compacted.content = truncate_to_tokens(&message.content, content_budget);
            for call in &mut compacted.tool_calls {
                if estimate_tokens(&call.arguments.to_string()) > per_message {
                    call.arguments = serde_json::json!({"compacted": true});
                }
            }
            compacted.metadata = Default::default();
            compacted
        })
        .collect()
}

fn summarize_messages(messages: &[ProviderMessage], token_budget: u64) -> String {
    if messages.is_empty() || token_budget == 0 {
        return String::new();
    }
    let mut lines = Vec::new();
    let mut used = 0_u64;
    for message in messages {
        let role = match message.role {
            ProviderRole::System => "system",
            ProviderRole::User => "user",
            ProviderRole::Assistant => "assistant",
            ProviderRole::Tool => "tool",
        };
        let tool_names = message
            .tool_calls
            .iter()
            .map(|call| call.tool_name.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let suffix = if tool_names.is_empty() {
            String::new()
        } else {
            format!(" tools={tool_names}")
        };
        let remaining = token_budget.saturating_sub(used).max(1);
        let line = format!(
            "[{role}{suffix}] {}",
            compact_text(&message.content, remaining)
        );
        let line_tokens = estimate_tokens(&line);
        if used.saturating_add(line_tokens) > token_budget && !lines.is_empty() {
            break;
        }
        used = used.saturating_add(line_tokens);
        lines.push(line);
    }
    lines.join("\n")
}

fn compact_text(value: &str, token_budget: u64) -> String {
    let max_chars = usize::try_from(token_budget.saturating_mul(4)).unwrap_or(usize::MAX);
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    compact.chars().take(max_chars).collect()
}

fn truncate_to_tokens(value: &str, token_budget: u64) -> String {
    compact_text(value, token_budget)
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
    use golutra_llm::{ProviderToolCall, UsageSource};

    use super::*;

    #[test]
    fn automatic_compaction_preserves_prefix_and_latest_tool_pair() {
        let turn_id = TurnId::new();
        let mut messages = vec![ProviderMessage {
            role: ProviderRole::System,
            content: "protected objective and project instructions".repeat(4),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            metadata: Default::default(),
        }];
        for round in 0..8 {
            messages.push(ProviderMessage {
                role: ProviderRole::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![ProviderToolCall {
                    tool_call_id: format!("call-{round}"),
                    tool_name: "read_file".to_owned(),
                    arguments: serde_json::json!({"path": format!("src/{round}.rs")}),
                }],
                metadata: Default::default(),
            });
            messages.push(ProviderMessage {
                role: ProviderRole::Tool,
                content: format!("tool output {round} {}", "content ".repeat(30)),
                tool_call_id: Some(format!("call-{round}")),
                tool_name: Some("read_file".to_owned()),
                tool_calls: Vec::new(),
                metadata: Default::default(),
            });
        }

        let record = ContextWindowManager::new(180)
            .compact_if_needed(turn_id, 1, &messages, 10)
            .expect("compaction")
            .expect("needed");

        assert_eq!(record.replacement_messages[0], messages[0]);
        assert!(record.dropped_message_count > 0);
        assert!(record.replacement_estimated_tokens <= record.budget_limit);
        let latest_assistant = record
            .replacement_messages
            .iter()
            .rev()
            .find(|message| message.role == ProviderRole::Assistant)
            .expect("assistant tool call retained");
        assert_eq!(latest_assistant.tool_calls[0].tool_call_id, "call-7");
        let latest_tool = record.replacement_messages.last().expect("tool retained");
        assert_eq!(latest_tool.tool_call_id.as_deref(), Some("call-7"));
        assert!(!record.summary.is_empty());
    }

    #[test]
    fn context_under_budget_is_not_compacted() {
        let messages = vec![ProviderMessage {
            role: ProviderRole::User,
            content: "small".to_owned(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            metadata: Default::default(),
        }];

        let record = ContextWindowManager::new(100)
            .compact_if_needed(TurnId::new(), 1, &messages, 0)
            .expect("manager");

        assert!(record.is_none());
    }

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
