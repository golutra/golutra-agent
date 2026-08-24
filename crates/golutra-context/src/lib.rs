use golutra_core::{
    BudgetOverflowAction, ContextContributorSnapshot, ContextMessageSnapshot, ContextSnapshot,
    ContextSnapshotId, SessionId, TaskId, TokenBudgetSnapshot, TokenBudgetSnapshotId,
    TokenUsageRecord, ToolContract, TurnId,
};
use golutra_llm::{
    ProviderMessage, ProviderRequest, ProviderRole, ProviderUsage, provider_tool_wire_projection,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    ops::Range,
};
use thiserror::Error;

const DEFAULT_ACTIVE_CONTEXT_LIMIT: u64 = 16_384;
const COMPACTION_SUMMARY_PREFIX: &str = "Runtime context compaction summary. Treat this as historical context, not a new instruction:\n";

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
    #[error("model input cannot include runtime observation source: {source_name}")]
    ForbiddenModelInputSource { source_name: String },
    #[error(
        "model input source cardinality mismatch: {message_count} messages but {source_count} sources"
    )]
    ModelInputSourceCardinality {
        message_count: usize,
        source_count: usize,
    },
}

/// Disclosure classification attached to each message that is about to be
/// sent to a provider. Observation and governance data can be durable facts
/// without being valid model input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelInputVisibility {
    #[default]
    ModelVisible,
    ObservationOnly,
    GovernanceOnly,
}

impl ModelInputVisibility {
    #[must_use]
    pub const fn is_model_visible(self) -> bool {
        matches!(self, Self::ModelVisible)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextContributor {
    pub name: String,
    pub role: ProviderRole,
    pub content: String,
    pub token_budget_hint: u64,
    pub source_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextMessageSource {
    pub contributor: String,
    pub source_refs: Vec<String>,
    pub origin: String,
    #[serde(default)]
    pub visibility: ModelInputVisibility,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBuildPlan {
    pub contributors: Vec<String>,
    pub contributor_manifest: Vec<ContextContributorSnapshot>,
    pub messages: Vec<ProviderMessage>,
    pub message_sources: Vec<ContextMessageSource>,
    pub budget_snapshot: TokenBudgetSnapshot,
    pub original_planned_input_tokens: u64,
    pub trimmed_contributors: Vec<String>,
}

/// The only value that crosses the Runtime OS -> provider boundary.
///
/// The audit snapshot deliberately lives beside, rather than inside, the
/// provider request.  RuntimeEvent, debug, evaluation and governance data
/// can therefore be recorded after the call without becoming model input by
/// accident.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInputEnvelope {
    provider_request: ProviderRequest,
    audit_snapshot: ContextSnapshot,
}

impl ModelInputEnvelope {
    #[must_use]
    pub fn provider_request(&self) -> &ProviderRequest {
        &self.provider_request
    }

    #[must_use]
    pub fn audit_snapshot(&self) -> &ContextSnapshot {
        &self.audit_snapshot
    }

    #[must_use]
    pub fn into_parts(self) -> (ProviderRequest, ContextSnapshot) {
        (self.provider_request, self.audit_snapshot)
    }
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
    #[serde(default)]
    pub compaction_limit: u64,
    #[serde(default)]
    pub target_input_tokens: u64,
    pub budget_limit: u64,
    pub summary: String,
    pub checksum: String,
    pub replacement_messages: Vec<ProviderMessage>,
    #[serde(default)]
    pub replacement_sources: Vec<ContextMessageSource>,
    #[serde(default)]
    pub message_decisions: Vec<ContextCompactionDecision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionDecision {
    pub original_index: u32,
    pub contributor: String,
    pub action: String,
    pub original_estimated_tokens: u64,
    pub retained_estimated_tokens: u64,
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

    #[must_use]
    pub fn required_compaction_limit(
        &self,
        protected_prefix_len: usize,
        messages: &[ProviderMessage],
        planned_tool_tokens: u64,
    ) -> Option<u64> {
        let original_estimated_tokens =
            estimate_message_tokens(messages).saturating_add(planned_tool_tokens);
        let active_context_limit = self.budget_limit.min(DEFAULT_ACTIVE_CONTEXT_LIMIT);
        if original_estimated_tokens <= active_context_limit {
            return None;
        }

        let protected_prefix_len = protected_prefix_len.min(messages.len());
        let protected_tokens = estimate_message_tokens(&messages[..protected_prefix_len])
            .saturating_add(planned_tool_tokens);
        if protected_tokens < active_context_limit {
            Some(active_context_limit)
        } else if original_estimated_tokens > self.budget_limit {
            Some(self.budget_limit)
        } else {
            None
        }
    }

    pub fn compact_if_needed(
        &self,
        turn_id: TurnId,
        protected_prefix_len: usize,
        messages: &[ProviderMessage],
        message_sources: &[ContextMessageSource],
        planned_tool_tokens: u64,
    ) -> Result<Option<ContextCompactionRecord>, ContextError> {
        let original_estimated_tokens =
            estimate_message_tokens(messages).saturating_add(planned_tool_tokens);
        let Some(compaction_limit) =
            self.required_compaction_limit(protected_prefix_len, messages, planned_tool_tokens)
        else {
            return Ok(None);
        };

        let protected_prefix_len = protected_prefix_len.min(messages.len());
        let protected = messages[..protected_prefix_len].to_vec();
        let protected_tokens =
            estimate_message_tokens(&protected).saturating_add(planned_tool_tokens);
        if protected_tokens >= compaction_limit {
            return Err(ContextError::CompactionImpossible {
                planned: protected_tokens,
                limit: compaction_limit,
            });
        }

        let hard_available = compaction_limit.saturating_sub(protected_tokens);
        let target_limit = self
            .target_percent
            .saturating_mul(compaction_limit)
            .saturating_div(100)
            .max(protected_tokens.saturating_add(hard_available.min(128)))
            .min(compaction_limit);
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
        let mut summary_message = ProviderMessage {
            role: ProviderRole::User,
            content: COMPACTION_SUMMARY_PREFIX.to_owned(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            metadata: Default::default(),
        };
        let summary_overhead = estimate_message_tokens(std::slice::from_ref(&summary_message));
        let summary = summarize_messages(dropped, summary_reserve.saturating_sub(summary_overhead));
        let normalized_sources = normalized_message_sources(messages, message_sources);
        let mut replacement_messages = protected;
        let mut replacement_sources = normalized_sources[..protected_prefix_len].to_vec();
        if !summary.is_empty() {
            summary_message.content.push_str(&summary);
            replacement_messages.push(summary_message);
            let mut source_refs = normalized_sources
                [protected_prefix_len..protected_prefix_len + dropped_end]
                .iter()
                .flat_map(|source| source.source_refs.iter().cloned())
                .collect::<Vec<_>>();
            source_refs.sort();
            source_refs.dedup();
            replacement_sources.push(ContextMessageSource {
                contributor: "working_summary".to_owned(),
                source_refs,
                origin: "compaction_summary".to_owned(),
                visibility: normalized_sources
                    [protected_prefix_len..protected_prefix_len + dropped_end]
                    .iter()
                    .map(|source| source.visibility)
                    .find(|visibility| !visibility.is_model_visible())
                    .unwrap_or_default(),
            });
        }
        let retained_count = retained.len();
        replacement_messages.extend(retained);
        let retained_source_start = messages.len().saturating_sub(retained_count);
        replacement_sources.extend_from_slice(&normalized_sources[retained_source_start..]);

        let replacement_source_by_original = (0..protected_prefix_len)
            .chain(retained_source_start..messages.len())
            .collect::<HashSet<_>>();
        let message_decisions = messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                let retained = replacement_source_by_original.contains(&index);
                ContextCompactionDecision {
                    original_index: u32::try_from(index).unwrap_or(u32::MAX),
                    contributor: normalized_sources[index].contributor.clone(),
                    action: if index < protected_prefix_len {
                        "protected".to_owned()
                    } else if retained {
                        "retained".to_owned()
                    } else {
                        "summarized".to_owned()
                    },
                    original_estimated_tokens: estimate_message_tokens(std::slice::from_ref(
                        message,
                    )),
                    retained_estimated_tokens: if retained {
                        estimate_message_tokens(std::slice::from_ref(message))
                    } else {
                        0
                    },
                }
            })
            .collect();

        let replacement_estimated_tokens =
            estimate_message_tokens(&replacement_messages).saturating_add(planned_tool_tokens);
        if replacement_estimated_tokens > compaction_limit {
            return Err(ContextError::CompactionImpossible {
                planned: replacement_estimated_tokens,
                limit: compaction_limit,
            });
        }
        let checksum =
            digest_bytes(&serde_json::to_vec(&replacement_messages).unwrap_or_else(|_| Vec::new()));
        Ok(Some(ContextCompactionRecord {
            turn_id,
            mode: "automatic".to_owned(),
            strategy: if compaction_limit < self.budget_limit {
                "active_working_set_summary_tail"
            } else {
                "protected_prefix_summary_tail"
            }
            .to_owned(),
            original_message_count: messages.len(),
            replacement_message_count: replacement_messages.len(),
            dropped_message_count,
            protected_prefix_len,
            original_estimated_tokens,
            replacement_estimated_tokens,
            planned_tool_tokens,
            compaction_limit,
            target_input_tokens: target_limit,
            budget_limit: self.budget_limit,
            summary,
            checksum,
            replacement_messages,
            replacement_sources,
            message_decisions,
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
                    message_indexes: vec![u32::try_from(index).unwrap_or(u32::MAX)],
                }
            })
            .collect::<Vec<_>>();
        let message_sources = contributors
            .iter()
            .map(|contributor| ContextMessageSource {
                contributor: contributor.name.clone(),
                source_refs: if contributor.source_refs.is_empty() {
                    vec![format!("contributor:{}", contributor.name)]
                } else {
                    contributor.source_refs.clone()
                },
                origin: "initial_contributor".to_owned(),
                visibility: ModelInputVisibility::ModelVisible,
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
            message_sources,
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

    /// Rebuild a context plan from an already captured provider message list.
    ///
    /// Deterministic replay uses this path so assistant/tool metadata is not
    /// flattened into ordinary contributors before the AgentLoop runs again.
    pub fn build_from_messages(
        &self,
        task_id: TaskId,
        turn_id: TurnId,
        messages: Vec<ProviderMessage>,
    ) -> Result<ContextBuildPlan, ContextError> {
        let planned_input_tokens = estimate_message_tokens(&messages);
        if planned_input_tokens > self.policy.budget_limit {
            return Err(ContextError::BudgetExceeded {
                planned: planned_input_tokens,
                limit: self.policy.budget_limit,
            });
        }
        let contributor_manifest = messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                let encoded = serde_json::to_vec(message).unwrap_or_default();
                let estimated_tokens = estimate_message_tokens(std::slice::from_ref(message));
                ContextContributorSnapshot {
                    name: format!("replay_message_{index}"),
                    role: format!("{:?}", message.role).to_lowercase(),
                    source_refs: vec![format!("replay:provider-message:{index}")],
                    included: true,
                    trimmed: false,
                    original_estimated_tokens: estimated_tokens,
                    retained_estimated_tokens: estimated_tokens,
                    strategy: "replay_exact".to_owned(),
                    estimated_tokens,
                    content_digest: digest_bytes(&encoded),
                    redacted_content_ref: None,
                    invalidation_refs: Vec::new(),
                    message_indexes: vec![u32::try_from(index).unwrap_or(u32::MAX)],
                }
            })
            .collect::<Vec<_>>();
        let message_sources = messages
            .iter()
            .enumerate()
            .map(|(index, _)| ContextMessageSource {
                contributor: format!("replay_message_{index}"),
                source_refs: vec![format!("replay:provider-message:{index}")],
                origin: "deterministic_replay".to_owned(),
                visibility: ModelInputVisibility::ModelVisible,
            })
            .collect();
        Ok(ContextBuildPlan {
            contributors: (0..messages.len())
                .map(|index| format!("replay_message_{index}"))
                .collect(),
            contributor_manifest,
            messages,
            message_sources,
            original_planned_input_tokens: planned_input_tokens,
            trimmed_contributors: Vec::new(),
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
                budget_policy: "deterministic_replay".to_owned(),
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
        session_id: None,
        provider_id: provider_id.into(),
        model_id: model_id.into(),
        messages: plan.messages.clone(),
        tools,
        cache_policy: golutra_core::PromptCachePolicy::Auto,
    }
}

/// Compile a provider request after applying the model-input disclosure
/// boundary.  Observation projections are intentionally rejected here rather
/// than relying on callers to remember which event-derived contributors are
/// safe to expose.
pub fn compile_model_input(
    session_id: SessionId,
    plan: &ContextBuildPlan,
    task_id: TaskId,
    turn_id: TurnId,
    provider_id: impl Into<String>,
    model_id: impl Into<String>,
    tools: Vec<ToolContract>,
) -> Result<ModelInputEnvelope, ContextError> {
    compile_model_input_with_cache_policy(
        session_id,
        plan,
        task_id,
        turn_id,
        provider_id,
        model_id,
        tools,
        golutra_core::PromptCachePolicy::Auto,
    )
}

/// Compile model input while preserving the caller's explicit cache policy.
/// The default wrapper above remains automatic, but callers that know the
/// provider retention window no longer lose that setting at this boundary.
#[allow(clippy::too_many_arguments)]
pub fn compile_model_input_with_cache_policy(
    session_id: SessionId,
    plan: &ContextBuildPlan,
    task_id: TaskId,
    turn_id: TurnId,
    provider_id: impl Into<String>,
    model_id: impl Into<String>,
    tools: Vec<ToolContract>,
    cache_policy: golutra_core::PromptCachePolicy,
) -> Result<ModelInputEnvelope, ContextError> {
    if plan.messages.len() != plan.message_sources.len() {
        return Err(ContextError::ModelInputSourceCardinality {
            message_count: plan.messages.len(),
            source_count: plan.message_sources.len(),
        });
    }
    for source in &plan.message_sources {
        if !source.visibility.is_model_visible() || is_observation_source(source) {
            return Err(ContextError::ForbiddenModelInputSource {
                source_name: source.origin.clone(),
            });
        }
    }

    let mut provider_request =
        provider_request_from_plan(plan, task_id, turn_id, provider_id, model_id, tools);
    provider_request.session_id = Some(session_id);
    provider_request.cache_policy = cache_policy;
    let audit_snapshot = context_snapshot_from_request(session_id, plan, &provider_request);
    Ok(ModelInputEnvelope {
        provider_request,
        audit_snapshot,
    })
}

fn is_observation_source(source: &ContextMessageSource) -> bool {
    const HIDDEN_ORIGINS: &[&str] = &[
        "observation_projection",
        "debug_projection",
        "evaluation_projection",
        "governance_projection",
        "runtime_event",
    ];
    const HIDDEN_CONTRIBUTORS: &[&str] = &[
        "observation",
        "debug_projection",
        "evaluation_projection",
        "governance_projection",
        "runtime_events",
    ];
    HIDDEN_ORIGINS.contains(&source.origin.as_str())
        || HIDDEN_CONTRIBUTORS.contains(&source.contributor.as_str())
}

#[must_use]
pub fn context_snapshot_from_request(
    session_id: SessionId,
    plan: &ContextBuildPlan,
    request: &ProviderRequest,
) -> ContextSnapshot {
    let message_sources = normalized_message_sources(&request.messages, &plan.message_sources);
    let message_manifest = request
        .messages
        .iter()
        .zip(&message_sources)
        .enumerate()
        .map(|(index, (message, source))| ContextMessageSnapshot {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            role: format!("{:?}", message.role).to_lowercase(),
            content_digest: digest_bytes(message.content.as_bytes()),
            estimated_tokens: estimate_tokens(&message.content),
            tool_call_ids: message
                .tool_calls
                .iter()
                .map(|call| call.tool_call_id.clone())
                .collect(),
            contributor: source.contributor.clone(),
            source_refs: source.source_refs.clone(),
            origin: source.origin.clone(),
        })
        .collect::<Vec<_>>();
    let tool_schema_digests = request
        .tools
        .iter()
        .filter_map(|tool| serde_json::to_vec(&provider_tool_wire_projection(tool)).ok())
        .map(|bytes| digest_bytes(&bytes))
        .collect::<Vec<_>>();
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
        contributor_manifest: attributed_contributor_manifest(
            plan,
            request,
            &message_sources,
            &tool_schema_digests,
        ),
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

fn normalized_message_sources(
    messages: &[ProviderMessage],
    message_sources: &[ContextMessageSource],
) -> Vec<ContextMessageSource> {
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            message_sources.get(index).cloned().unwrap_or_else(|| {
                let contributor = match message.role {
                    ProviderRole::System => "system_prompt",
                    ProviderRole::User => "runtime_context",
                    ProviderRole::Assistant => "assistant_recent",
                    ProviderRole::Tool => "tool_result_excerpt",
                };
                ContextMessageSource {
                    contributor: contributor.to_owned(),
                    source_refs: vec![format!("request:message:{index}")],
                    origin: "inferred_legacy".to_owned(),
                    visibility: ModelInputVisibility::ModelVisible,
                }
            })
        })
        .collect()
}

fn attributed_contributor_manifest(
    plan: &ContextBuildPlan,
    request: &ProviderRequest,
    sources: &[ContextMessageSource],
    tool_schema_digests: &[String],
) -> Vec<ContextContributorSnapshot> {
    let base = plan
        .contributor_manifest
        .iter()
        .map(|contributor| (contributor.name.as_str(), contributor))
        .collect::<BTreeMap<_, _>>();
    let mut grouped = BTreeMap::<String, ContextContributorSnapshot>::new();
    for (index, (message, source)) in request.messages.iter().zip(sources).enumerate() {
        let estimated_tokens = estimate_message_tokens(std::slice::from_ref(message));
        let has_base_contributor = base.contains_key(source.contributor.as_str());
        let entry = grouped
            .entry(source.contributor.clone())
            .or_insert_with(|| {
                base.get(source.contributor.as_str()).map_or_else(
                    || ContextContributorSnapshot {
                        name: source.contributor.clone(),
                        role: format!("{:?}", message.role).to_lowercase(),
                        source_refs: Vec::new(),
                        included: true,
                        trimmed: source.origin.contains("compaction"),
                        original_estimated_tokens: 0,
                        retained_estimated_tokens: 0,
                        strategy: source.origin.clone(),
                        estimated_tokens: 0,
                        content_digest: String::new(),
                        redacted_content_ref: None,
                        invalidation_refs: Vec::new(),
                        message_indexes: Vec::new(),
                    },
                    |base| {
                        let mut base = (*base).clone();
                        base.source_refs.clear();
                        base.retained_estimated_tokens = 0;
                        base.estimated_tokens = 0;
                        base.content_digest.clear();
                        base.message_indexes.clear();
                        base
                    },
                )
            });
        let first_attributed_message = entry.message_indexes.is_empty();
        entry.retained_estimated_tokens = entry
            .retained_estimated_tokens
            .saturating_add(estimated_tokens);
        entry.estimated_tokens = entry.retained_estimated_tokens;
        if !has_base_contributor || !first_attributed_message {
            entry.original_estimated_tokens = entry
                .original_estimated_tokens
                .saturating_add(estimated_tokens);
        }
        entry
            .message_indexes
            .push(u32::try_from(index).unwrap_or(u32::MAX));
        entry.source_refs.extend(source.source_refs.iter().cloned());
        entry.source_refs.sort();
        entry.source_refs.dedup();
        entry.content_digest.push_str(&digest_bytes(
            &serde_json::to_vec(message).unwrap_or_default(),
        ));
    }
    for contributor in grouped.values_mut() {
        contributor.content_digest = digest_bytes(contributor.content_digest.as_bytes());
    }
    if plan.budget_snapshot.planned_tool_tokens > 0 {
        grouped.insert(
            "tool_instructions".to_owned(),
            ContextContributorSnapshot {
                name: "tool_instructions".to_owned(),
                role: "tool_schema".to_owned(),
                source_refs: request
                    .tools
                    .iter()
                    .map(|tool| format!("tool-contract:{}", tool.tool_name))
                    .collect(),
                included: true,
                trimmed: false,
                original_estimated_tokens: plan.budget_snapshot.planned_tool_tokens,
                retained_estimated_tokens: plan.budget_snapshot.planned_tool_tokens,
                strategy: "include_schema".to_owned(),
                estimated_tokens: plan.budget_snapshot.planned_tool_tokens,
                content_digest: digest_bytes(tool_schema_digests.join("").as_bytes()),
                redacted_content_ref: None,
                invalidation_refs: Vec::new(),
                message_indexes: Vec::new(),
            },
        );
    }
    grouped.into_values().collect()
}

fn digest_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{:x}", digest)
}

#[must_use]
pub fn token_usage_record(
    plan: &ContextBuildPlan,
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
    let message_sources = normalized_message_sources(&request.messages, &plan.message_sources);
    let tool_schema_digests = request
        .tools
        .iter()
        .filter_map(|tool| serde_json::to_vec(&provider_tool_wire_projection(tool)).ok())
        .map(|bytes| digest_bytes(&bytes))
        .collect::<Vec<_>>();
    let contributor_manifest =
        attributed_contributor_manifest(plan, request, &message_sources, &tool_schema_digests);
    let attributed_input_tokens = usage.input_tokens.map(|actual| {
        proportional_token_attribution(
            actual,
            &contributor_manifest
                .iter()
                .map(|contributor| contributor.retained_estimated_tokens)
                .collect::<Vec<_>>(),
        )
    });
    let contributors = contributor_manifest
        .iter()
        .enumerate()
        .map(|(index, contributor)| {
            let attributed_input_tokens = attributed_input_tokens
                .as_ref()
                .and_then(|tokens| tokens.get(index).copied());
            golutra_core::TokenContributorAttribution {
                contributor: contributor.name.clone(),
                source_refs: contributor.source_refs.clone(),
                message_indexes: contributor.message_indexes.clone(),
                estimated_input_tokens: contributor.retained_estimated_tokens,
                attributed_input_tokens,
                attribution_method: if usage.input_tokens.is_some() {
                    "proportional_provider_total".to_owned()
                } else {
                    "estimated_request_tokens".to_owned()
                },
            }
        })
        .collect::<Vec<_>>();
    let attributed_total = contributors
        .iter()
        .filter_map(|contributor| contributor.attributed_input_tokens)
        .fold(0_u64, u64::saturating_add);
    let contributor_tokens = |name: &str| {
        contributor_manifest
            .iter()
            .filter(|contributor| contributor.name == name)
            .map(|contributor| contributor.retained_estimated_tokens)
            .sum::<u64>()
    };
    let normalized = usage.normalize();
    let tool_schema_tokens_estimated = normalized
        .tool_schema_tokens_estimated
        .or_else(|| (!request.tools.is_empty()).then_some(budget_snapshot.planned_tool_tokens));
    let tool_result_tokens_estimated = normalized
        .tool_result_tokens_estimated
        .or(Some(tool_result_tokens));
    TokenUsageRecord {
        session_id: request.session_id,
        task_id: request.task_id,
        turn_id: request.turn_id,
        provider_id: request.provider_id.clone(),
        model_id: request.model_id.clone(),
        request_event_id: request.request_id,
        response_event_id,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        estimated_cost: (cost_model == "zero").then_some(0.0),
        budget_snapshot_ref: budget_snapshot.snapshot_id,
        attribution_ref: Some(golutra_core::TokenAttribution {
            system_prompt_tokens: Some(system_prompt_tokens),
            developer_instruction_tokens: Some(contributor_tokens("developer_instructions")),
            runtime_context_tokens: Some(contributor_tokens("runtime_context")),
            policy_tokens: Some(contributor_tokens("policy")),
            user_message_tokens: Some(user_message_tokens),
            assistant_recent_tokens: Some(assistant_recent_tokens),
            working_summary_tokens: Some(contributor_tokens("working_summary")),
            memory_tokens: Some(contributor_tokens("memory")),
            evidence_tokens: Some(contributor_tokens("evidence")),
            tool_instruction_tokens: Some(contributor_tokens("tool_instructions")),
            tool_result_excerpt_tokens: Some(tool_result_tokens),
            output_tokens: usage.output_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            contributors,
            unattributed_input_tokens: usage
                .input_tokens
                .map(|actual| actual.saturating_sub(attributed_total)),
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
        cache_read_tokens: normalized.cache_read_tokens,
        cache_write_tokens: normalized.cache_write_tokens,
        non_cached_input_tokens: normalized.input_tokens_non_cached,
        tool_schema_tokens_estimated,
        tool_result_tokens_estimated,
        tool_estimated_tokens: match (tool_schema_tokens_estimated, tool_result_tokens_estimated) {
            (Some(schema), Some(result)) => Some(schema.saturating_add(result)),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        },
        provider_total_tokens: normalized.provider_total_tokens,
        usage_complete: normalized.usage_complete,
        cache_identity: request.cache_identity(),
    }
}

fn proportional_token_attribution(actual: u64, weights: &[u64]) -> Vec<u64> {
    let total_weight = weights
        .iter()
        .map(|weight| u128::from(*weight))
        .sum::<u128>();
    if total_weight == 0 {
        return vec![0; weights.len()];
    }
    let actual = u128::from(actual);
    let mut shares = Vec::with_capacity(weights.len());
    let mut remainders = Vec::with_capacity(weights.len());
    let mut assigned = 0_u64;
    for (index, weight) in weights.iter().copied().enumerate() {
        let numerator = actual.saturating_mul(u128::from(weight));
        let share = u64::try_from(numerator / total_weight).unwrap_or(u64::MAX);
        shares.push(share);
        assigned = assigned.saturating_add(share);
        remainders.push((index, numerator % total_weight));
    }
    remainders.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let remainder = u64::try_from(actual)
        .unwrap_or(u64::MAX)
        .saturating_sub(assigned);
    for (index, _) in remainders
        .into_iter()
        .take(usize::try_from(remainder).unwrap_or(usize::MAX))
    {
        shares[index] = shares[index].saturating_add(1);
    }
    shares
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
    for message in messages.iter().rev() {
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
        let remaining = token_budget.saturating_sub(used);
        let prefix = format!("[{role}{suffix}] ");
        let prefix_tokens = estimate_tokens(&prefix);
        if remaining <= prefix_tokens {
            break;
        }
        let line = format!(
            "{prefix}{}",
            compact_text(&message.content, remaining.saturating_sub(prefix_tokens))
        );
        let line_tokens = estimate_tokens(&line);
        if line_tokens > remaining {
            break;
        }
        used = used.saturating_add(line_tokens);
        lines.push(line);
    }
    lines.reverse();
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
        let sources = messages
            .iter()
            .enumerate()
            .map(|(index, message)| ContextMessageSource {
                contributor: if index == 0 {
                    "system_prompt".to_owned()
                } else if message.role == ProviderRole::Tool {
                    "tool_result_excerpt".to_owned()
                } else {
                    "assistant_recent".to_owned()
                },
                source_refs: vec![format!("event:source-{index}")],
                origin: "runtime_history".to_owned(),
                visibility: ModelInputVisibility::ModelVisible,
            })
            .collect::<Vec<_>>();

        let record = ContextWindowManager::new(180)
            .compact_if_needed(turn_id, 1, &messages, &sources, 10)
            .expect("compaction")
            .expect("needed");

        assert_eq!(record.replacement_messages[0], messages[0]);
        assert!(record.dropped_message_count > 0);
        assert!(record.replacement_estimated_tokens <= record.target_input_tokens);
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
        assert_eq!(
            record.replacement_sources.len(),
            record.replacement_messages.len()
        );
        assert_eq!(record.message_decisions.len(), messages.len());
        assert_eq!(record.message_decisions[0].action, "protected");
        assert!(
            record
                .message_decisions
                .iter()
                .any(|decision| decision.action == "summarized")
        );
        let summary_source = record
            .replacement_sources
            .iter()
            .find(|source| source.origin == "compaction_summary")
            .expect("summary source");
        assert_eq!(summary_source.contributor, "working_summary");
        assert!(!summary_source.source_refs.is_empty());
        assert_eq!(
            record.replacement_sources.last(),
            sources.last(),
            "the retained tail keeps its original contributor and source"
        );

        let task_id = TaskId::new();
        let mut plan = ContextBuilder::default()
            .build_from_messages(task_id, turn_id, record.replacement_messages.clone())
            .expect("compacted plan");
        plan.message_sources = record.replacement_sources.clone();
        let request =
            provider_request_from_plan(&plan, task_id, turn_id, "mock", "mock-model", Vec::new());
        let snapshot = context_snapshot_from_request(SessionId::new(), &plan, &request);
        assert!(
            snapshot
                .message_manifest
                .iter()
                .zip(&record.replacement_sources)
                .all(|(message, source)| {
                    message.contributor == source.contributor
                        && message.source_refs == source.source_refs
                        && message.origin == source.origin
                })
        );
    }

    #[test]
    fn active_working_set_compacts_before_the_provider_hard_limit() {
        let turn_id = TurnId::new();
        let mut messages = vec![ProviderMessage {
            role: ProviderRole::System,
            content: "protected runtime instructions".to_owned(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            metadata: Default::default(),
        }];
        for round in 0..40 {
            messages.push(ProviderMessage {
                role: ProviderRole::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![ProviderToolCall {
                    tool_call_id: format!("call-{round}"),
                    tool_name: "shell".to_owned(),
                    arguments: serde_json::json!({"command": format!("inspect-{round}")}),
                }],
                metadata: Default::default(),
            });
            messages.push(ProviderMessage {
                role: ProviderRole::Tool,
                content: "observed tool output ".repeat(120),
                tool_call_id: Some(format!("call-{round}")),
                tool_name: Some("shell".to_owned()),
                tool_calls: Vec::new(),
                metadata: Default::default(),
            });
        }

        let manager = ContextWindowManager::new(96_000);
        let original_tokens = estimate_message_tokens(&messages);
        assert!(original_tokens > DEFAULT_ACTIVE_CONTEXT_LIMIT);
        assert!(original_tokens < 96_000);
        assert_eq!(
            manager.required_compaction_limit(1, &messages, 0),
            Some(DEFAULT_ACTIVE_CONTEXT_LIMIT)
        );

        let record = manager
            .compact_if_needed(turn_id, 1, &messages, &[], 0)
            .expect("working-set compaction")
            .expect("working set exceeded");

        assert_eq!(record.budget_limit, 96_000);
        assert_eq!(record.compaction_limit, DEFAULT_ACTIVE_CONTEXT_LIMIT);
        assert_eq!(record.strategy, "active_working_set_summary_tail");
        assert!(record.replacement_estimated_tokens <= record.target_input_tokens);
        assert!(record.replacement_estimated_tokens < original_tokens);
    }

    #[test]
    fn compaction_summary_prefers_the_most_recent_dropped_context() {
        let message = |content: &str| ProviderMessage {
            role: ProviderRole::Tool,
            content: content.repeat(20),
            tool_call_id: None,
            tool_name: Some("shell".to_owned()),
            tool_calls: Vec::new(),
            metadata: Default::default(),
        };
        let summary = summarize_messages(
            &[
                message("oldest-observation "),
                message("newest-observation "),
            ],
            24,
        );

        assert!(summary.contains("newest-observation"));
        assert!(!summary.contains("oldest-observation"));
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
            .compact_if_needed(TurnId::new(), 1, &messages, &[], 0)
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
    fn model_input_envelope_rejects_observation_projection_sources() {
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let mut plan = ContextBuilder::default()
            .build(
                task_id,
                turn_id,
                vec![ContextContributor {
                    name: "objective".to_owned(),
                    role: ProviderRole::User,
                    content: "inspect the workspace".to_owned(),
                    token_budget_hint: 32,
                    source_refs: vec!["task:objective".to_owned()],
                }],
            )
            .expect("context builds");
        plan.message_sources[0].contributor = "debug_projection".to_owned();
        plan.message_sources[0].origin = "observation_projection".to_owned();

        let result = compile_model_input(
            SessionId::new(),
            &plan,
            task_id,
            turn_id,
            "mock",
            "mock-model",
            Vec::new(),
        );

        assert!(matches!(
            result,
            Err(ContextError::ForbiddenModelInputSource { source_name })
                if source_name == "observation_projection"
        ));
    }

    #[test]
    fn model_input_envelope_rejects_typed_hidden_visibility_even_with_safe_labels() {
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let mut plan = ContextBuilder::default()
            .build(
                task_id,
                turn_id,
                vec![ContextContributor {
                    name: "runtime-fact".to_owned(),
                    role: ProviderRole::User,
                    content: "governance candidate details".to_owned(),
                    token_budget_hint: 32,
                    source_refs: vec!["governance:candidate".to_owned()],
                }],
            )
            .expect("context builds");
        plan.message_sources[0].visibility = ModelInputVisibility::GovernanceOnly;
        plan.message_sources[0].origin = "initial_contributor".to_owned();

        let result = compile_model_input(
            SessionId::new(),
            &plan,
            task_id,
            turn_id,
            "mock",
            "mock-model",
            Vec::new(),
        );

        assert!(matches!(
            result,
            Err(ContextError::ForbiddenModelInputSource { source_name })
                if source_name == "initial_contributor"
        ));
    }

    #[test]
    fn model_input_envelope_rejects_messages_without_explicit_sources() {
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let mut plan = ContextBuilder::default()
            .build(
                task_id,
                turn_id,
                vec![ContextContributor {
                    name: "objective".to_owned(),
                    role: ProviderRole::User,
                    content: "inspect the workspace".to_owned(),
                    token_budget_hint: 32,
                    source_refs: vec!["task:objective".to_owned()],
                }],
            )
            .expect("context builds");
        plan.message_sources.clear();

        let result = compile_model_input(
            SessionId::new(),
            &plan,
            task_id,
            turn_id,
            "mock",
            "mock-model",
            Vec::new(),
        );

        assert!(matches!(
            result,
            Err(ContextError::ModelInputSourceCardinality {
                message_count: 1,
                source_count: 0,
            })
        ));
    }

    #[test]
    fn compaction_summary_preserves_hidden_visibility() {
        let turn_id = TurnId::new();
        let mut messages = vec![ProviderMessage {
            role: ProviderRole::System,
            content: "protected".to_owned(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            metadata: Default::default(),
        }];
        let mut sources = vec![ContextMessageSource {
            contributor: "system".to_owned(),
            source_refs: vec!["system".to_owned()],
            origin: "test".to_owned(),
            visibility: ModelInputVisibility::ModelVisible,
        }];
        for index in 0..8 {
            messages.push(ProviderMessage {
                role: ProviderRole::User,
                content: format!("hidden evaluation evidence {index} ").repeat(40),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            });
            sources.push(ContextMessageSource {
                contributor: "evaluation".to_owned(),
                source_refs: vec![format!("evaluation:{index}")],
                origin: "test".to_owned(),
                visibility: ModelInputVisibility::GovernanceOnly,
            });
        }

        let record = ContextWindowManager::new(96)
            .compact_if_needed(turn_id, 1, &messages, &sources, 0)
            .expect("compaction")
            .expect("compaction required");
        let summary_source = record
            .replacement_sources
            .iter()
            .find(|source| source.contributor == "working_summary")
            .expect("summary source");
        assert_eq!(
            summary_source.visibility,
            ModelInputVisibility::GovernanceOnly
        );
    }

    #[test]
    fn model_input_envelope_separates_provider_input_from_audit_snapshot() {
        let session_id = SessionId::new();
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
                    token_budget_hint: 32,
                    source_refs: vec!["task:objective".to_owned()],
                }],
            )
            .expect("context builds");

        let envelope = compile_model_input(
            session_id,
            &plan,
            task_id,
            turn_id,
            "mock",
            "mock-model",
            Vec::new(),
        )
        .expect("model input compiles");

        assert_eq!(envelope.provider_request().messages, plan.messages);
        assert_eq!(envelope.audit_snapshot().session_id, session_id);
        assert_eq!(
            envelope.audit_snapshot().provider_request_id,
            envelope.provider_request().request_id
        );
        assert!(
            serde_json::to_value(envelope.provider_request())
                .expect("provider request json")
                .get("events")
                .is_none()
        );
    }

    #[test]
    fn model_input_preserves_explicit_cache_retention_policy() {
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let plan = ContextBuilder::default()
            .build(task_id, turn_id, Vec::new())
            .expect("context builds");

        let envelope = compile_model_input_with_cache_policy(
            SessionId::new(),
            &plan,
            task_id,
            turn_id,
            "mock",
            "mock-model",
            Vec::new(),
            golutra_core::PromptCachePolicy::Long,
        )
        .expect("model input compiles");

        assert_eq!(
            envelope.provider_request().cache_policy,
            golutra_core::PromptCachePolicy::Long
        );
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
            &plan,
            &request,
            golutra_core::ProviderResponseId::new(),
            &plan.budget_snapshot,
            &usage,
            "zero",
        );

        assert_eq!(record.provider_total_tokens, Some(15));
        assert_eq!(record.estimated_cost, Some(0.0));
        assert_eq!(record.usage_source, "provider");
        assert_eq!(record.tool_result_tokens_estimated, Some(0));
        assert_eq!(record.tool_schema_tokens_estimated, None);
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
    fn missing_provider_total_remains_unknown_in_new_usage_records() {
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
            reasoning_tokens: None,
            cached_input_tokens: None,
            total_tokens: None,
            usage_source: UsageSource::Provider,
            raw: serde_json::json!({}),
        };

        let record = token_usage_record(
            &plan,
            &request,
            golutra_core::ProviderResponseId::new(),
            &plan.budget_snapshot,
            &usage,
            "zero",
        );

        assert_eq!(record.provider_total_tokens, None);
        assert_eq!(record.usage().provider_total_tokens, None);
        assert_eq!(record.usage().aggregate_total(), Some(15));
    }

    #[test]
    fn contributor_attribution_preserves_provider_input_token_total() {
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let mut plan = ContextBuilder::default()
            .build(
                task_id,
                turn_id,
                vec![
                    ContextContributor {
                        name: "objective".to_owned(),
                        role: ProviderRole::User,
                        content: "implement the requested behavior".repeat(3),
                        token_budget_hint: 128,
                        source_refs: vec!["task:objective".to_owned()],
                    },
                    ContextContributor {
                        name: "memory".to_owned(),
                        role: ProviderRole::System,
                        content: "relevant retained fact".repeat(2),
                        token_budget_hint: 128,
                        source_refs: vec!["memory:fact-1".to_owned()],
                    },
                ],
            )
            .expect("context builds");
        plan.budget_snapshot.planned_tool_tokens = 13;
        let request = provider_request_from_plan(
            &plan,
            task_id,
            turn_id,
            "mock",
            "mock-model",
            vec![ToolContract {
                tool_name: "read_file".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: serde_json::json!({"type": "object"}),
                error_schema: serde_json::json!({"type": "object"}),
                side_effect_type: golutra_core::SideEffectType::None,
                idempotency_key_policy: "tool_call_id".to_owned(),
                timeout_policy: "bounded".to_owned(),
                cancellation_policy: "cooperative".to_owned(),
                retry_policy: "none".to_owned(),
                artifact_policy: "bounded".to_owned(),
                permission_policy_ref: None,
            }],
        );
        let usage = ProviderUsage {
            input_tokens: Some(101),
            output_tokens: Some(7),
            reasoning_tokens: Some(2),
            cached_input_tokens: Some(3),
            total_tokens: Some(108),
            usage_source: UsageSource::Provider,
            raw: serde_json::json!({}),
        };

        let record = token_usage_record(
            &plan,
            &request,
            golutra_core::ProviderResponseId::new(),
            &plan.budget_snapshot,
            &usage,
            "zero",
        );
        let attribution = record.attribution_ref.expect("token attribution");
        let attributed_total = attribution
            .contributors
            .iter()
            .filter_map(|contributor| contributor.attributed_input_tokens)
            .sum::<u64>();

        assert_eq!(attributed_total, 101);
        assert_eq!(attribution.unattributed_input_tokens, Some(0));
        assert_eq!(record.tool_schema_tokens_estimated, Some(13));
        assert_eq!(record.tool_result_tokens_estimated, Some(0));
        assert_eq!(record.tool_estimated_tokens, Some(13));
        assert!(attribution.contributors.iter().any(|contributor| {
            contributor.contributor == "objective" && contributor.source_refs == ["task:objective"]
        }));
        assert!(attribution.contributors.iter().any(|contributor| {
            contributor.contributor == "memory" && contributor.source_refs == ["memory:fact-1"]
        }));
        assert!(attribution.contributors.iter().any(|contributor| {
            contributor.contributor == "tool_instructions"
                && contributor.source_refs == ["tool-contract:read_file"]
        }));
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

    #[test]
    fn attributed_manifest_sums_original_tokens_for_grouped_runtime_messages() {
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let plan = ContextBuilder::default()
            .build(task_id, turn_id, Vec::new())
            .expect("empty context");
        let mut request = provider_request_from_plan(
            &plan,
            task_id,
            turn_id,
            "test-provider",
            "test-model",
            Vec::new(),
        );
        request.messages = [
            ("call-1", "first tool result"),
            ("call-2", "second tool result is longer"),
        ]
        .into_iter()
        .map(|(tool_call_id, content)| ProviderMessage {
            role: ProviderRole::Tool,
            content: content.to_owned(),
            tool_call_id: Some(tool_call_id.to_owned()),
            tool_name: Some("shell".to_owned()),
            tool_calls: Vec::new(),
            metadata: Default::default(),
        })
        .collect();
        let sources = vec![
            ContextMessageSource {
                contributor: "tool_result_excerpt".to_owned(),
                source_refs: vec!["tool-call:1".to_owned()],
                origin: "tool_result".to_owned(),
                visibility: ModelInputVisibility::ModelVisible,
            },
            ContextMessageSource {
                contributor: "tool_result_excerpt".to_owned(),
                source_refs: vec!["tool-call:2".to_owned()],
                origin: "tool_result".to_owned(),
                visibility: ModelInputVisibility::ModelVisible,
            },
        ];

        let manifest = attributed_contributor_manifest(&plan, &request, &sources, &[]);
        let tool_results = manifest
            .iter()
            .find(|entry| entry.name == "tool_result_excerpt")
            .expect("tool result attribution");

        assert_eq!(
            tool_results.original_estimated_tokens,
            tool_results.retained_estimated_tokens
        );
        assert_eq!(tool_results.message_indexes, vec![0, 1]);
    }
}
