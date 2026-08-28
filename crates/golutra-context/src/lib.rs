use golutra_core::{
    BudgetOverflowAction, CacheIdentity, ContextContributorSnapshot, ContextMessageSnapshot,
    ContextSnapshot, ContextSnapshotId, SessionId, TaskId, TokenBudgetSnapshot,
    TokenBudgetSnapshotId, TokenUsageRecord, ToolContract, TurnId,
};
use golutra_llm::{
    ProviderMessage, ProviderRequest, ProviderRole, ProviderUsage, provider_tool_wire_digest,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    ops::Range,
};
use thiserror::Error;

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
    /// Token estimates captured with the message snapshot.  Keeping these
    /// beside the messages lets the runtime reuse one scan for budgeting,
    /// snapshotting and usage attribution.
    pub message_estimates: Vec<u64>,
    /// Full-message digests captured with the same snapshot. These are used by
    /// attribution and audit manifests so a large message is not serialized a
    /// second time merely to calculate its content identity.
    pub message_digests: Vec<String>,
    pub message_sources: Vec<ContextMessageSource>,
    pub budget_snapshot: TokenBudgetSnapshot,
    pub original_planned_input_tokens: u64,
    pub trimmed_contributors: Vec<String>,
}

/// Return a stable identity for the first `message_count` messages in a plan.
///
/// The provider reports only an aggregate input-token count, so the runtime
/// needs a cheap way to prove that the count still belongs to the same prefix
/// before reusing it after tool results or queued turns are appended. Message
/// digests are already captured while building the plan; hashing those short
/// digests avoids serializing the full message bodies again.
#[must_use]
pub fn context_message_prefix_digest(
    plan: &ContextBuildPlan,
    message_count: usize,
) -> Option<String> {
    if message_count > plan.messages.len() || plan.message_digests.len() != plan.messages.len() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(b"golutra-context-prefix-v1\0");
    hasher.update((message_count as u64).to_le_bytes());
    for digest in &plan.message_digests[..message_count] {
        hasher.update((digest.len() as u64).to_le_bytes());
        hasher.update(digest.as_bytes());
    }
    Some(format!("sha256:{:x}", hasher.finalize()))
}

impl ContextBuildPlan {
    /// Append one model-visible message and capture its estimate in the same
    /// operation.  Runtime code keeps the plan as the single mutable source
    /// of truth, so budgeting, snapshotting and attribution cannot drift.
    pub fn append_message(
        &mut self,
        message: ProviderMessage,
        source: ContextMessageSource,
    ) -> u64 {
        let estimated_tokens = estimate_message_token(&message);
        let digest = provider_message_digest(&message);
        self.messages.push(message);
        self.message_estimates.push(estimated_tokens);
        self.message_digests.push(digest);
        self.message_sources.push(source);
        estimated_tokens
    }

    /// Replace the working set after compaction and return its new estimate.
    pub fn replace_messages(
        &mut self,
        messages: Vec<ProviderMessage>,
        sources: Vec<ContextMessageSource>,
    ) -> u64 {
        let estimates = messages
            .iter()
            .map(estimate_message_token)
            .collect::<Vec<_>>();
        let digests = messages.iter().map(provider_message_digest).collect();
        let total = sum_estimates(&estimates);
        self.messages = messages;
        self.message_estimates = estimates;
        self.message_digests = digests;
        self.message_sources = sources;
        total
    }

    #[must_use]
    pub fn estimated_message_tokens(&self) -> u64 {
        sum_estimates(&self.message_estimates)
    }
}

/// A provider-reported input-token checkpoint for the current message prefix.
/// The runtime may use it to account only for messages appended afterwards;
/// callers must invalidate it whenever the prefix or tool contract changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedContextPrefix {
    pub message_count: usize,
    pub input_tokens: u64,
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
        let message_estimates = messages
            .iter()
            .map(estimate_message_token)
            .collect::<Vec<_>>();
        self.required_compaction_limit_with_estimates(
            protected_prefix_len,
            messages,
            &message_estimates,
            planned_tool_tokens,
        )
    }

    /// 使用调用方已经计算的消息 token，避免同一轮重复扫描完整上下文。
    #[must_use]
    pub fn required_compaction_limit_with_estimates(
        &self,
        protected_prefix_len: usize,
        messages: &[ProviderMessage],
        message_estimates: &[u64],
        planned_tool_tokens: u64,
    ) -> Option<u64> {
        self.required_compaction_limit_with_observed_prefix(
            protected_prefix_len,
            messages,
            message_estimates,
            planned_tool_tokens,
            None,
        )
    }

    /// Like [`required_compaction_limit_with_estimates`], but uses a trusted
    /// provider input count for the already-observed message prefix. This
    /// mirrors Pi's usage baseline while retaining a conservative local
    /// estimate for messages appended after that request.
    #[must_use]
    pub fn required_compaction_limit_with_observed_prefix(
        &self,
        _protected_prefix_len: usize,
        messages: &[ProviderMessage],
        message_estimates: &[u64],
        planned_tool_tokens: u64,
        observed_prefix: Option<ObservedContextPrefix>,
    ) -> Option<u64> {
        if self.budget_limit == 0 {
            return None;
        }
        let fallback_estimates;
        let message_estimates = if message_estimates.len() == messages.len() {
            message_estimates
        } else {
            fallback_estimates = messages
                .iter()
                .map(estimate_message_token)
                .collect::<Vec<_>>();
            &fallback_estimates
        };
        let original_estimated_tokens = context_tokens_with_observed_prefix(
            messages,
            message_estimates,
            planned_tool_tokens,
            observed_prefix,
        );
        if original_estimated_tokens <= self.budget_limit {
            return None;
        }

        // 即使稳定前缀本身已超预算，也返回硬上限，让执行层得到明确的
        // CompactionImpossible，而不是把溢出静默当成“无需压缩”。
        Some(self.budget_limit)
    }

    pub fn compact_if_needed(
        &self,
        turn_id: TurnId,
        protected_prefix_len: usize,
        messages: &[ProviderMessage],
        message_sources: &[ContextMessageSource],
        planned_tool_tokens: u64,
    ) -> Result<Option<ContextCompactionRecord>, ContextError> {
        let message_estimates = messages
            .iter()
            .map(estimate_message_token)
            .collect::<Vec<_>>();
        self.compact_if_needed_with_estimates(
            turn_id,
            protected_prefix_len,
            messages,
            message_sources,
            &message_estimates,
            planned_tool_tokens,
        )
    }

    /// 在已知消息 token 的情况下执行压缩；消息列表和估算必须来自同一快照。
    pub fn compact_if_needed_with_estimates(
        &self,
        turn_id: TurnId,
        protected_prefix_len: usize,
        messages: &[ProviderMessage],
        message_sources: &[ContextMessageSource],
        message_estimates: &[u64],
        planned_tool_tokens: u64,
    ) -> Result<Option<ContextCompactionRecord>, ContextError> {
        self.compact_if_needed_with_observed_prefix(
            turn_id,
            protected_prefix_len,
            messages,
            message_sources,
            message_estimates,
            planned_tool_tokens,
            None,
        )
    }

    /// Compact using a provider checkpoint for the current prefix. The
    /// checkpoint affects the trigger and recorded pre-compaction total; the
    /// actual replacement still uses per-message estimates so a compaction
    /// never relies on an unverifiable distribution of provider tokens.
    #[allow(clippy::too_many_arguments)]
    pub fn compact_if_needed_with_observed_prefix(
        &self,
        turn_id: TurnId,
        protected_prefix_len: usize,
        messages: &[ProviderMessage],
        message_sources: &[ContextMessageSource],
        message_estimates: &[u64],
        planned_tool_tokens: u64,
        observed_prefix: Option<ObservedContextPrefix>,
    ) -> Result<Option<ContextCompactionRecord>, ContextError> {
        let fallback_estimates;
        let message_estimates = if message_estimates.len() == messages.len() {
            message_estimates
        } else {
            fallback_estimates = messages
                .iter()
                .map(estimate_message_token)
                .collect::<Vec<_>>();
            &fallback_estimates
        };
        let original_estimated_tokens = context_tokens_with_observed_prefix(
            messages,
            message_estimates,
            planned_tool_tokens,
            observed_prefix,
        );
        if self.budget_limit == 0 || original_estimated_tokens <= self.budget_limit {
            return Ok(None);
        }

        let protected_prefix_len = protected_prefix_len.min(messages.len());
        let protected = messages[..protected_prefix_len].to_vec();
        let protected_message_tokens = sum_estimates(&message_estimates[..protected_prefix_len]);
        let protected_tokens = protected_context_tokens(
            protected_prefix_len,
            message_estimates,
            planned_tool_tokens,
            observed_prefix,
        );
        let compaction_limit = self.budget_limit;
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
        let minimum_summary_reserve = estimate_tokens(COMPACTION_SUMMARY_PREFIX).saturating_add(8);
        let summary_reserve = target_available
            .saturating_div(4)
            .max(minimum_summary_reserve)
            .min(1_024)
            .min(target_available);
        let tail_budget = target_available.saturating_sub(summary_reserve);
        let tail = &messages[protected_prefix_len..];
        let groups = message_groups(tail);
        let mut retained_groups = Vec::<Range<usize>>::new();
        let mut retained_tokens = 0_u64;
        for group in groups.iter().rev() {
            let group_tokens = sum_estimates(
                &message_estimates
                    [protected_prefix_len + group.start..protected_prefix_len + group.end],
            );
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
        let mut retained_message_tokens = retained_tokens;
        if retained.is_empty() && !tail.is_empty() && tail_budget > 0 {
            let latest_group = groups
                .last()
                .cloned()
                .unwrap_or(tail.len().saturating_sub(1)..tail.len());
            retained = compact_message_group(&tail[latest_group], tail_budget);
            // This path rewrites content/tool arguments, so the original
            // estimates no longer describe the replacement messages.
            retained_message_tokens = estimate_message_tokens(&retained);
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
        let mut summary_message_tokens = 0_u64;
        if !summary.is_empty() {
            summary_message.content.push_str(&summary);
            summary_message_tokens = estimate_message_token(&summary_message);
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
            .map(|(index, _message)| {
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
                    original_estimated_tokens: message_estimates[index],
                    retained_estimated_tokens: if retained {
                        message_estimates[index]
                    } else {
                        0
                    },
                }
            })
            .collect();

        let replacement_estimated_tokens = protected_message_tokens
            .saturating_add(summary_message_tokens)
            .saturating_add(retained_message_tokens)
            .saturating_add(planned_tool_tokens);
        if replacement_estimated_tokens > compaction_limit {
            return Err(ContextError::CompactionImpossible {
                planned: replacement_estimated_tokens,
                limit: compaction_limit,
            });
        }
        let checksum = serialized_digest(&replacement_messages);
        Ok(Some(ContextCompactionRecord {
            turn_id,
            mode: "automatic".to_owned(),
            // 压缩上限就是当前 provider budget；保留真实的策略名，避免
            // 已不存在的 active-working-set 分支污染评估和回放指标。
            strategy: "protected_prefix_summary_tail".to_owned(),
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

/// Estimate the protected prefix without charging the provider tool schema
/// twice when the observed input count already includes it. When the observed
/// checkpoint covers only part of the protected prefix, only the newly added
/// message estimates are appended. When it covers more than the protected
/// prefix, subtract the known suffix and keep the local estimate as a
/// conservative floor for provider-specific framing overhead.
fn protected_context_tokens(
    protected_prefix_len: usize,
    message_estimates: &[u64],
    planned_tool_tokens: u64,
    observed_prefix: Option<ObservedContextPrefix>,
) -> u64 {
    let protected_prefix_len = protected_prefix_len.min(message_estimates.len());
    let local_estimate = sum_estimates(&message_estimates[..protected_prefix_len])
        .saturating_add(planned_tool_tokens);
    let Some(observed) =
        observed_prefix.filter(|observed| observed.message_count <= message_estimates.len())
    else {
        return local_estimate;
    };

    let observed_estimate = if observed.message_count < protected_prefix_len {
        observed.input_tokens.saturating_add(sum_estimates(
            &message_estimates[observed.message_count..protected_prefix_len],
        ))
    } else {
        observed.input_tokens.saturating_sub(sum_estimates(
            &message_estimates[protected_prefix_len..observed.message_count],
        ))
    };
    local_estimate.max(observed_estimate)
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
            action_if_exceeded: BudgetOverflowAction::Compact,
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

    /// Return the stable provider prefix length for a projected message list.
    ///
    /// Static instructions stay at the front of every request so a provider
    /// can reuse that prefix. Conversation state and tool results are the
    /// compactable working set and must never be protected accidentally.
    #[must_use]
    pub fn stable_prefix_len(
        &self,
        messages: &[ProviderMessage],
        sources: &[ContextMessageSource],
    ) -> usize {
        messages
            .iter()
            .enumerate()
            .take_while(|(index, message)| {
                !sources.get(*index).is_some_and(is_dynamic_context_source)
                    && !matches!(message.role, ProviderRole::Assistant | ProviderRole::Tool)
            })
            .count()
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
        let original_planned_input_tokens = sum_estimates(&original_contributor_tokens);
        let mut retained_contributor_tokens = original_contributor_tokens.clone();
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
                BudgetOverflowAction::Trim => {
                    trimmed_contributors = trim_contributors_with_estimates(
                        &mut contributors,
                        &mut retained_contributor_tokens,
                        self.policy.budget_limit,
                    );
                }
                // Compact only after the complete provider message sequence
                // (including queued turns and tool calls) is available.
                BudgetOverflowAction::Compact => {}
            }
        }
        let planned_input_tokens = sum_estimates(&retained_contributor_tokens);
        if planned_input_tokens > self.policy.budget_limit
            && !matches!(
                self.policy.action_if_exceeded,
                BudgetOverflowAction::Compact
            )
        {
            return Err(ContextError::BudgetExceeded {
                planned: planned_input_tokens,
                limit: self.policy.budget_limit,
            });
        }

        let trimmed_set = trimmed_contributors
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let mut contributor_manifest = Vec::with_capacity(contributors.len());
        let mut message_sources = Vec::with_capacity(contributors.len());
        let mut messages = Vec::with_capacity(contributors.len());
        let mut message_estimates = Vec::with_capacity(contributors.len());
        let mut message_digests = Vec::with_capacity(contributors.len());
        let mut contributor_names = Vec::with_capacity(contributors.len());
        for (index, contributor) in contributors.into_iter().enumerate() {
            let retained_estimated_tokens = retained_contributor_tokens[index];
            let trimmed = trimmed_set.contains(contributor.name.as_str());
            let source_refs = if contributor.source_refs.is_empty() {
                vec![format!("contributor:{}", contributor.name)]
            } else {
                contributor.source_refs.clone()
            };
            contributor_manifest.push(ContextContributorSnapshot {
                name: contributor.name.clone(),
                role: format!("{:?}", contributor.role).to_lowercase(),
                source_refs: source_refs.clone(),
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
            });
            message_sources.push(ContextMessageSource {
                contributor: contributor.name.clone(),
                source_refs,
                origin: "initial_contributor".to_owned(),
                visibility: ModelInputVisibility::ModelVisible,
            });
            let message = ProviderMessage {
                role: contributor.role,
                content: contributor.content,
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            };
            // `build` creates an empty metadata object for every message, so
            // the contributor estimate is also the exact message estimate.
            message_digests.push(provider_message_digest(&message));
            messages.push(message);
            message_estimates.push(retained_estimated_tokens);
            contributor_names.push(contributor.name);
        }

        Ok(ContextBuildPlan {
            contributors: contributor_names,
            contributor_manifest,
            messages,
            message_estimates,
            message_digests,
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
                budget_policy: "token_window_compaction".to_owned(),
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
        let message_estimates = messages
            .iter()
            .map(estimate_message_token)
            .collect::<Vec<_>>();
        let planned_input_tokens = sum_estimates(&message_estimates);
        if planned_input_tokens > self.policy.budget_limit
            && !matches!(
                self.policy.action_if_exceeded,
                BudgetOverflowAction::Compact
            )
        {
            return Err(ContextError::BudgetExceeded {
                planned: planned_input_tokens,
                limit: self.policy.budget_limit,
            });
        }
        let message_digests = messages
            .iter()
            .map(provider_message_digest)
            .collect::<Vec<_>>();
        let contributor_manifest = messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                let estimated_tokens = message_estimates[index];
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
                    content_digest: message_digests[index].clone(),
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
            message_estimates,
            message_digests,
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

fn trim_contributors_with_estimates(
    contributors: &mut [ContextContributor],
    estimates: &mut [u64],
    budget_limit: u64,
) -> Vec<String> {
    debug_assert_eq!(contributors.len(), estimates.len());
    let mut trimmed = Vec::new();
    let mut total = sum_estimates(estimates);
    for (index, contributor) in contributors.iter_mut().enumerate() {
        let current_tokens = estimates[index];
        if contributor.token_budget_hint > 0 && current_tokens > contributor.token_budget_hint {
            let content = truncate_contributor(
                &contributor.name,
                &contributor.content,
                contributor.token_budget_hint,
            );
            let retained_tokens = estimate_tokens(&content);
            total = total
                .saturating_sub(estimates[index])
                .saturating_add(retained_tokens);
            estimates[index] = retained_tokens;
            contributor.content = content;
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
        if total <= budget_limit {
            break;
        }
        if let Some((index, contributor)) = contributors
            .iter_mut()
            .enumerate()
            .find(|(_, contributor)| contributor.name == *name)
        {
            let current_tokens = estimates[index];
            let overflow = total.saturating_sub(budget_limit);
            let target = current_tokens.saturating_sub(overflow).max(16);
            let content = truncate_contributor(name, &contributor.content, target);
            let retained_tokens = estimate_tokens(&content);
            total = total
                .saturating_sub(estimates[index])
                .saturating_add(retained_tokens);
            estimates[index] = retained_tokens;
            contributor.content = content;
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
    mut tools: Vec<ToolContract>,
) -> ProviderRequest {
    // Registry insertion order is an implementation detail. A canonical tool
    // order keeps the provider prefix byte-stable across turns and processes.
    tools.sort_by(|left, right| left.tool_name.cmp(&right.tool_name));
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
    let message_estimates = plan
        .messages
        .iter()
        .map(estimate_message_token)
        .collect::<Vec<_>>();
    compile_model_input_with_cache_policy_and_estimates(
        session_id,
        plan,
        task_id,
        turn_id,
        provider_id,
        model_id,
        tools,
        cache_policy,
        &message_estimates,
    )
}

/// Compile model input using estimates captured with the current message
/// snapshot.  The provider request and its audit snapshot therefore share one
/// token view, without rescanning message content at the wire boundary.
#[allow(clippy::too_many_arguments)]
pub fn compile_model_input_with_cache_policy_and_estimates(
    session_id: SessionId,
    plan: &ContextBuildPlan,
    task_id: TaskId,
    turn_id: TurnId,
    provider_id: impl Into<String>,
    model_id: impl Into<String>,
    tools: Vec<ToolContract>,
    cache_policy: golutra_core::PromptCachePolicy,
    message_estimates: &[u64],
) -> Result<ModelInputEnvelope, ContextError> {
    let tool_schema_digests = tools
        .iter()
        .map(provider_tool_wire_digest)
        .collect::<Vec<_>>();
    compile_model_input_with_cache_policy_and_estimates_and_tool_digests(
        session_id,
        plan,
        task_id,
        turn_id,
        provider_id,
        model_id,
        tools,
        cache_policy,
        message_estimates,
        &tool_schema_digests,
    )
}

/// Compile model input when the caller already has the exact provider-facing
/// tool digests. This keeps the runtime's stable tool set on one projection
/// cache lookup per contract per turn.
#[allow(clippy::too_many_arguments)]
pub fn compile_model_input_with_cache_policy_and_estimates_and_tool_digests(
    session_id: SessionId,
    plan: &ContextBuildPlan,
    task_id: TaskId,
    turn_id: TurnId,
    provider_id: impl Into<String>,
    model_id: impl Into<String>,
    tools: Vec<ToolContract>,
    cache_policy: golutra_core::PromptCachePolicy,
    message_estimates: &[u64],
    tool_schema_digests: &[String],
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
    let audit_snapshot =
        context_snapshot_from_request_with_estimates_and_tool_digests_and_message_digests(
            session_id,
            plan,
            &provider_request,
            message_estimates,
            tool_schema_digests,
            &plan.message_digests,
        );
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

fn is_dynamic_context_source(source: &ContextMessageSource) -> bool {
    let contributor = source.contributor.as_str();
    matches!(
        contributor,
        "memory"
            | "conversation_history"
            | "project_skills"
            | "objective"
            | "user_message"
            | "assistant_recent"
            | "runtime_context"
            | "verification_feedback"
            | "tool_result_excerpt"
            | "working_summary"
    ) || contributor.starts_with("history:")
        || contributor.starts_with("replay_message_")
        || matches!(
            source.origin.as_str(),
            "pending_turn"
                | "compaction_summary"
                | "runtime_history"
                | "tool_result_compaction"
                | "runtime_recovery"
                | "runtime_deadline_advisory"
                | "runtime_progress_advisory"
                | "verification_feedback"
        )
}

#[must_use]
pub fn context_snapshot_from_request(
    session_id: SessionId,
    plan: &ContextBuildPlan,
    request: &ProviderRequest,
) -> ContextSnapshot {
    let message_estimates = if plan.message_estimates.len() == request.messages.len()
        && plan.messages == request.messages
    {
        plan.message_estimates.as_slice()
    } else {
        // A request may intentionally replace the plan's messages during
        // replay; calculate a fresh snapshot only for that divergent list.
        return context_snapshot_from_request_with_estimates_and_tool_digests_and_message_digests(
            session_id,
            plan,
            request,
            &request
                .messages
                .iter()
                .map(estimate_message_token)
                .collect::<Vec<_>>(),
            &request
                .tools
                .iter()
                .map(provider_tool_wire_digest)
                .collect::<Vec<_>>(),
            &[],
        );
    };
    let tool_schema_digests = request
        .tools
        .iter()
        .map(provider_tool_wire_digest)
        .collect::<Vec<_>>();
    context_snapshot_from_request_with_estimates_and_tool_digests_and_message_digests(
        session_id,
        plan,
        request,
        message_estimates,
        &tool_schema_digests,
        &plan.message_digests,
    )
}

/// Build a context snapshot from estimates belonging to `request.messages`.
/// Callers that already prepared the request for budgeting should use this
/// variant to keep context preparation linear in the number of messages.
#[must_use]
pub fn context_snapshot_from_request_with_estimates(
    session_id: SessionId,
    plan: &ContextBuildPlan,
    request: &ProviderRequest,
    message_estimates: &[u64],
) -> ContextSnapshot {
    let tool_schema_digests = request
        .tools
        .iter()
        .map(provider_tool_wire_digest)
        .collect::<Vec<_>>();
    context_snapshot_from_request_with_estimates_and_tool_digests_and_message_digests(
        session_id,
        plan,
        request,
        message_estimates,
        &tool_schema_digests,
        matching_message_digests(plan, request),
    )
}

/// Build a context snapshot while reusing a caller-owned tool digest list.
/// The list is validated by length and falls back to a local projection when
/// an external caller supplies a stale set.
#[must_use]
pub fn context_snapshot_from_request_with_estimates_and_tool_digests(
    session_id: SessionId,
    plan: &ContextBuildPlan,
    request: &ProviderRequest,
    message_estimates: &[u64],
    tool_schema_digests: &[String],
) -> ContextSnapshot {
    context_snapshot_from_request_with_estimates_and_tool_digests_and_message_digests(
        session_id,
        plan,
        request,
        message_estimates,
        tool_schema_digests,
        matching_message_digests(plan, request),
    )
}

fn context_snapshot_from_request_with_estimates_and_tool_digests_and_message_digests(
    session_id: SessionId,
    plan: &ContextBuildPlan,
    request: &ProviderRequest,
    message_estimates: &[u64],
    tool_schema_digests: &[String],
    message_digests: &[String],
) -> ContextSnapshot {
    let fallback_estimates;
    let message_estimates = if message_estimates.len() == request.messages.len() {
        message_estimates
    } else {
        fallback_estimates = request
            .messages
            .iter()
            .map(estimate_message_token)
            .collect::<Vec<_>>();
        &fallback_estimates
    };
    let normalized_sources;
    let message_sources: &[ContextMessageSource] = if plan.message_sources.len()
        == request.messages.len()
    {
        &plan.message_sources
    } else {
        normalized_sources = normalized_message_sources(&request.messages, &plan.message_sources);
        &normalized_sources
    };
    let message_manifest = request
        .messages
        .iter()
        .zip(message_sources.iter())
        .enumerate()
        .map(|(index, (message, source))| ContextMessageSnapshot {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            role: format!("{:?}", message.role).to_lowercase(),
            content_digest: digest_bytes(message.content.as_bytes()),
            estimated_tokens: message_estimates[index],
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
    let fallback_tool_schema_digests;
    let tool_schema_digests = if tool_schema_digests.len() == request.tools.len() {
        tool_schema_digests
    } else {
        fallback_tool_schema_digests = request
            .tools
            .iter()
            .map(provider_tool_wire_digest)
            .collect::<Vec<_>>();
        &fallback_tool_schema_digests
    };
    let canonical_request_digest = canonical_provider_request_digest(request, tool_schema_digests);
    ContextSnapshot {
        snapshot_id: ContextSnapshotId::new(),
        session_id,
        task_id: request.task_id,
        turn_id: request.turn_id,
        provider_request_id: request.request_id,
        provider_id: request.provider_id.clone(),
        model_id: request.model_id.clone(),
        contributor_manifest: attributed_contributor_manifest_with_estimates(
            plan,
            request,
            message_sources,
            tool_schema_digests,
            message_estimates,
            message_digests,
        ),
        message_manifest,
        tool_schema_digests: tool_schema_digests.to_vec(),
        generation_config_digest: None,
        budget_snapshot: plan.budget_snapshot.clone(),
        canonical_request_digest,
        redacted_request_artifact_ref: None,
        restricted_request_artifact_ref: None,
        estimate_source: "character_div_4".to_owned(),
        created_at: chrono::Utc::now(),
    }
}

fn matching_message_digests<'a>(
    plan: &'a ContextBuildPlan,
    request: &ProviderRequest,
) -> &'a [String] {
    if plan.messages == request.messages && plan.message_digests.len() == request.messages.len() {
        &plan.message_digests
    } else {
        &[]
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

fn attributed_contributor_manifest_with_estimates(
    plan: &ContextBuildPlan,
    request: &ProviderRequest,
    sources: &[ContextMessageSource],
    tool_schema_digests: &[String],
    message_estimates: &[u64],
    message_digests: &[String],
) -> Vec<ContextContributorSnapshot> {
    let base = plan
        .contributor_manifest
        .iter()
        .map(|contributor| (contributor.name.as_str(), contributor))
        .collect::<HashMap<_, _>>();
    // 归因顺序必须跟 provider message 的首次出现顺序一致。按名称排序会
    // 改写审计输出，也会让同一请求在不同回放路径上产生不同前缀。
    let mut grouped = HashMap::<String, ContextContributorSnapshot>::new();
    let mut contributor_order = Vec::<String>::new();
    for (index, (message, source)) in request.messages.iter().zip(sources).enumerate() {
        let estimated_tokens = message_estimates
            .get(index)
            .copied()
            .unwrap_or_else(|| estimate_message_token(message));
        let has_base_contributor = base.contains_key(source.contributor.as_str());
        if !grouped.contains_key(&source.contributor) {
            contributor_order.push(source.contributor.clone());
        }
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
        if let Some(message_digest) = message_digests.get(index) {
            entry.content_digest.push_str(message_digest);
        } else {
            let message_digest = provider_message_digest(message);
            entry.content_digest.push_str(&message_digest);
        }
    }
    for contributor in grouped.values_mut() {
        contributor.content_digest = digest_bytes(contributor.content_digest.as_bytes());
    }
    if plan.budget_snapshot.planned_tool_tokens > 0 {
        let tool_name = "tool_instructions".to_owned();
        if !grouped.contains_key(&tool_name) {
            contributor_order.push(tool_name.clone());
        }
        grouped.insert(
            tool_name,
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
    contributor_order
        .into_iter()
        .filter_map(|name| grouped.remove(&name))
        .collect()
}

fn digest_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{:x}", digest)
}

fn provider_message_digest(message: &ProviderMessage) -> String {
    serialized_digest(message)
}

fn serialized_digest<T: Serialize + ?Sized>(value: &T) -> String {
    let mut writer = DigestWriter(Sha256::new());
    if serde_json::to_writer(&mut writer, value).is_err() {
        return digest_bytes(&[]);
    }
    format!("sha256:{:x}", writer.0.finalize())
}

/// A serde sink that hashes serialized JSON without materializing a second
/// message-sized `Vec<u8>` on the context hot path.
struct DigestWriter(Sha256);

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Hash the provider-facing request without serializing internal tool policy
/// fields. Length-delimited components make the identity deterministic while
/// avoiding a second large JSON allocation for every streamed turn.
fn canonical_provider_request_digest(
    request: &ProviderRequest,
    tool_schema_digests: &[String],
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"golutra-provider-request-v3\0");
    digest_field(&mut digest, request.provider_id.as_bytes());
    digest_field(&mut digest, request.model_id.as_bytes());
    digest_field(
        &mut digest,
        format!("{:?}", request.cache_policy).as_bytes(),
    );
    digest_field(
        &mut digest,
        request
            .session_id
            .map(|id| id.to_string())
            .unwrap_or_default()
            .as_bytes(),
    );
    digest_field(&mut digest, request.task_id.to_string().as_bytes());
    digest_field(&mut digest, request.turn_id.to_string().as_bytes());
    for message in &request.messages {
        digest_field(&mut digest, format!("{:?}", message.role).as_bytes());
        digest_field(&mut digest, message.content.as_bytes());
        digest_field(
            &mut digest,
            message
                .tool_call_id
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
        );
        digest_field(
            &mut digest,
            message.tool_name.as_deref().unwrap_or_default().as_bytes(),
        );
        for call in &message.tool_calls {
            digest_field(&mut digest, call.tool_call_id.as_bytes());
            digest_field(&mut digest, call.tool_name.as_bytes());
            let arguments = serde_json::to_vec(&call.arguments).unwrap_or_default();
            digest_field(&mut digest, &arguments);
        }
        let metadata = serde_json::to_vec(&message.metadata).unwrap_or_default();
        digest_field(&mut digest, &metadata);
    }
    for tool_digest in tool_schema_digests {
        digest_field(&mut digest, tool_digest.as_bytes());
    }
    format!("sha256:{:x}", digest.finalize())
}

fn digest_field(digest: &mut Sha256, field: &[u8]) {
    digest.update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_le_bytes());
    digest.update(field);
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
    let fallback_estimates;
    let message_estimates = if plan.message_estimates.len() == request.messages.len()
        && plan.messages == request.messages
    {
        plan.message_estimates.as_slice()
    } else {
        fallback_estimates = request
            .messages
            .iter()
            .map(estimate_message_token)
            .collect::<Vec<_>>();
        &fallback_estimates
    };
    token_usage_record_with_cache_identity_and_estimates(
        plan,
        request,
        response_event_id,
        budget_snapshot,
        usage,
        cost_model,
        request.cache_identity(),
        message_estimates,
    )
}

/// Build usage attribution while preserving the provider-specific cache route
/// identity used on the wire.
#[must_use]
pub fn token_usage_record_with_cache_identity(
    plan: &ContextBuildPlan,
    request: &ProviderRequest,
    response_event_id: golutra_core::ProviderResponseId,
    budget_snapshot: &TokenBudgetSnapshot,
    usage: &ProviderUsage,
    cost_model: &str,
    cache_identity: Option<CacheIdentity>,
) -> TokenUsageRecord {
    let fallback_estimates;
    let message_estimates = if plan.message_estimates.len() == request.messages.len()
        && plan.messages == request.messages
    {
        plan.message_estimates.as_slice()
    } else {
        fallback_estimates = request
            .messages
            .iter()
            .map(estimate_message_token)
            .collect::<Vec<_>>();
        &fallback_estimates
    };
    token_usage_record_with_cache_identity_and_estimates(
        plan,
        request,
        response_event_id,
        budget_snapshot,
        usage,
        cost_model,
        cache_identity,
        message_estimates,
    )
}

/// Build usage attribution using message estimates already computed by the
/// runtime. Keeping this as a separate entry point avoids rescanning every
/// message for each budget, snapshot, and accounting view in one turn.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn token_usage_record_with_cache_identity_and_estimates(
    plan: &ContextBuildPlan,
    request: &ProviderRequest,
    response_event_id: golutra_core::ProviderResponseId,
    budget_snapshot: &TokenBudgetSnapshot,
    usage: &ProviderUsage,
    cost_model: &str,
    cache_identity: Option<CacheIdentity>,
    message_estimates: &[u64],
) -> TokenUsageRecord {
    let tool_schema_digests = request
        .tools
        .iter()
        .map(provider_tool_wire_digest)
        .collect::<Vec<_>>();
    token_usage_record_with_cache_identity_and_estimates_and_tool_digests(
        plan,
        request,
        response_event_id,
        budget_snapshot,
        usage,
        cost_model,
        cache_identity,
        message_estimates,
        &tool_schema_digests,
    )
}

/// Build usage attribution with both message estimates and provider tool
/// digests supplied by the caller. Runtime uses this path after preparing a
/// stable tool set, so schema projection is not repeated for every provider
/// response.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn token_usage_record_with_cache_identity_and_estimates_and_tool_digests(
    plan: &ContextBuildPlan,
    request: &ProviderRequest,
    response_event_id: golutra_core::ProviderResponseId,
    budget_snapshot: &TokenBudgetSnapshot,
    usage: &ProviderUsage,
    cost_model: &str,
    cache_identity: Option<CacheIdentity>,
    message_estimates: &[u64],
    tool_schema_digests: &[String],
) -> TokenUsageRecord {
    let fallback_estimates;
    let message_estimates = if message_estimates.len() == request.messages.len() {
        message_estimates
    } else {
        fallback_estimates = request
            .messages
            .iter()
            .map(estimate_message_token)
            .collect::<Vec<_>>();
        &fallback_estimates
    };
    let mut system_prompt_tokens = 0_u64;
    let mut user_message_tokens = 0_u64;
    let mut assistant_recent_tokens = 0_u64;
    let mut tool_result_tokens = 0_u64;
    for (index, message) in request.messages.iter().enumerate() {
        let tokens = message_estimates[index];
        match message.role {
            ProviderRole::System => {
                system_prompt_tokens = system_prompt_tokens.saturating_add(tokens)
            }
            ProviderRole::User => user_message_tokens = user_message_tokens.saturating_add(tokens),
            ProviderRole::Assistant => {
                assistant_recent_tokens = assistant_recent_tokens.saturating_add(tokens)
            }
            ProviderRole::Tool => tool_result_tokens = tool_result_tokens.saturating_add(tokens),
        }
    }
    let normalized_sources;
    let message_sources: &[ContextMessageSource] = if plan.message_sources.len()
        == request.messages.len()
    {
        &plan.message_sources
    } else {
        normalized_sources = normalized_message_sources(&request.messages, &plan.message_sources);
        &normalized_sources
    };
    let fallback_tool_schema_digests;
    let tool_schema_digests = if tool_schema_digests.len() == request.tools.len() {
        tool_schema_digests
    } else {
        fallback_tool_schema_digests = request
            .tools
            .iter()
            .map(provider_tool_wire_digest)
            .collect::<Vec<_>>();
        &fallback_tool_schema_digests
    };
    let contributor_manifest = attributed_contributor_manifest_with_estimates(
        plan,
        request,
        message_sources,
        tool_schema_digests,
        message_estimates,
        if plan.messages == request.messages {
            &plan.message_digests
        } else {
            &[]
        },
    );
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
        cache_identity,
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
    messages.iter().map(estimate_message_token).sum()
}

/// Return the estimated input size for a message list, replacing the prefix
/// estimate with a provider-reported checkpoint when it still applies.
#[must_use]
pub fn context_tokens_with_observed_prefix(
    messages: &[ProviderMessage],
    message_estimates: &[u64],
    planned_tool_tokens: u64,
    observed_prefix: Option<ObservedContextPrefix>,
) -> u64 {
    let fallback_estimates;
    let estimates = if message_estimates.len() == messages.len() {
        message_estimates
    } else {
        fallback_estimates = messages
            .iter()
            .map(estimate_message_token)
            .collect::<Vec<_>>();
        &fallback_estimates
    };
    context_tokens_with_observed_prefix_and_total(
        messages,
        estimates,
        sum_estimates(estimates),
        planned_tool_tokens,
        observed_prefix,
    )
}

/// Fast variant for callers that maintain the running message total while
/// appending messages.  A provider input checkpoint already includes the tool
/// schema because it was measured on the complete request; adding
/// `planned_tool_tokens` on that path would double-count it.
#[must_use]
pub fn context_tokens_with_observed_prefix_and_total(
    messages: &[ProviderMessage],
    message_estimates: &[u64],
    total_message_tokens: u64,
    planned_tool_tokens: u64,
    observed_prefix: Option<ObservedContextPrefix>,
) -> u64 {
    let fallback_estimates;
    let estimates = if message_estimates.len() == messages.len() {
        message_estimates
    } else {
        fallback_estimates = messages
            .iter()
            .map(estimate_message_token)
            .collect::<Vec<_>>();
        &fallback_estimates
    };
    if let Some(observed) =
        observed_prefix.filter(|observed| observed.message_count <= messages.len())
    {
        observed
            .input_tokens
            .saturating_add(sum_estimates(&estimates[observed.message_count..]))
    } else {
        total_message_tokens.saturating_add(planned_tool_tokens)
    }
}

#[must_use]
pub fn estimate_message_token(message: &ProviderMessage) -> u64 {
    let metadata_tokens = if message.metadata.openai_responses_replay_items.is_empty() {
        0
    } else {
        serde_json::to_string(&message.metadata)
            .map(|metadata| estimate_tokens(&metadata))
            .unwrap_or_default()
    };
    estimate_tokens(&message.content)
        .saturating_add(
            message
                .tool_calls
                .iter()
                .map(|call| estimate_tokens(&call.arguments.to_string()))
                .sum::<u64>(),
        )
        .saturating_add(metadata_tokens)
}

#[must_use]
fn sum_estimates(estimates: &[u64]) -> u64 {
    estimates.iter().copied().fold(0, u64::saturating_add)
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

#[must_use]
pub fn estimate_tokens(content: &str) -> u64 {
    content.chars().count().div_ceil(4) as u64
}

#[cfg(test)]
mod tests {
    use golutra_llm::{ProviderToolCall, UsageSource};
    use serde_json::json;

    use super::*;

    #[test]
    fn stable_prefix_keeps_task_local_system_sources() {
        let builder = ContextBuilder::default();
        let messages = vec![
            ProviderMessage {
                role: ProviderRole::System,
                content: "system".to_owned(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            },
            ProviderMessage {
                role: ProviderRole::System,
                content: "environment".to_owned(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            },
            ProviderMessage {
                role: ProviderRole::System,
                content: "skill metadata".to_owned(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            },
            ProviderMessage {
                role: ProviderRole::System,
                content: "schema".to_owned(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            },
        ];
        let sources = vec![
            ContextMessageSource {
                contributor: "system".to_owned(),
                source_refs: Vec::new(),
                origin: "initial_contributor".to_owned(),
                visibility: ModelInputVisibility::ModelVisible,
            },
            ContextMessageSource {
                contributor: "environment_context".to_owned(),
                source_refs: Vec::new(),
                origin: "initial_contributor".to_owned(),
                visibility: ModelInputVisibility::ModelVisible,
            },
            ContextMessageSource {
                contributor: "project_skills".to_owned(),
                source_refs: Vec::new(),
                origin: "initial_contributor".to_owned(),
                visibility: ModelInputVisibility::ModelVisible,
            },
            ContextMessageSource {
                contributor: "output_schema".to_owned(),
                source_refs: Vec::new(),
                origin: "initial_contributor".to_owned(),
                visibility: ModelInputVisibility::ModelVisible,
            },
        ];

        // schema 与 skill 是任务级动态输入，不能切断可跨任务复用的静态前缀。
        assert_eq!(builder.stable_prefix_len(&messages, &sources), 2);
    }

    #[test]
    fn provider_tool_order_is_canonical() {
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let plan = ContextBuilder::default()
            .build(task_id, turn_id, Vec::new())
            .expect("context plan");
        let tool = |name: &str| ToolContract {
            tool_name: name.to_owned(),
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            error_schema: json!({"type": "object"}),
            side_effect_type: golutra_core::SideEffectType::None,
            idempotency_key_policy: "none".to_owned(),
            timeout_policy: "bounded".to_owned(),
            cancellation_policy: "cooperative".to_owned(),
            retry_policy: "none".to_owned(),
            artifact_policy: "none".to_owned(),
            permission_policy_ref: None,
        };
        let request = provider_request_from_plan(
            &plan,
            task_id,
            turn_id,
            "mock",
            "model",
            vec![tool("shell"), tool("read_file")],
        );
        assert_eq!(
            request
                .tools
                .iter()
                .map(|tool| tool.tool_name.as_str())
                .collect::<Vec<_>>(),
            vec!["read_file", "shell"]
        );
    }

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
    fn compaction_uses_the_declared_provider_budget() {
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

        let manager = ContextWindowManager::new(16_384);
        let original_tokens = estimate_message_tokens(&messages);
        assert!(original_tokens > 16_384);
        assert_eq!(
            manager.required_compaction_limit(1, &messages, 0),
            Some(16_384)
        );

        let record = manager
            .compact_if_needed(turn_id, 1, &messages, &[], 0)
            .expect("working-set compaction")
            .expect("working set exceeded");

        assert_eq!(record.budget_limit, 16_384);
        assert_eq!(record.compaction_limit, 16_384);
        assert_eq!(record.strategy, "protected_prefix_summary_tail");
        assert!(record.replacement_estimated_tokens <= record.target_input_tokens);
        assert!(record.replacement_estimated_tokens < original_tokens);
    }

    #[test]
    fn observed_prefix_budget_does_not_double_count_tool_schema() {
        let estimates = [8, 12, 20];
        let messages = (0..estimates.len())
            .map(|index| ProviderMessage {
                role: ProviderRole::User,
                content: format!("message-{index}"),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                metadata: Default::default(),
            })
            .collect::<Vec<_>>();
        let observed = ObservedContextPrefix {
            message_count: 1,
            // The provider count already includes the complete tool schema.
            input_tokens: 100,
        };

        assert_eq!(
            context_tokens_with_observed_prefix_and_total(
                &messages,
                &estimates,
                estimates.iter().sum(),
                40,
                Some(observed),
            ),
            132,
            "only messages appended after the checkpoint are estimated locally"
        );
        assert_eq!(
            protected_context_tokens(2, &estimates, 40, Some(observed)),
            112,
            "the observed prefix carries the tool schema exactly once"
        );
    }

    #[test]
    fn observed_prefix_protection_uses_local_floor_when_checkpoint_is_smaller() {
        let estimates = [30, 30, 30];
        let observed = ObservedContextPrefix {
            message_count: 3,
            input_tokens: 40,
        };

        assert_eq!(
            protected_context_tokens(1, &estimates, 20, Some(observed)),
            50,
            "a provider undercount must not reduce the local protected budget"
        );
    }

    #[test]
    fn context_message_prefix_digest_is_bounded_by_the_captured_plan() {
        let plan = ContextBuilder::default()
            .build(
                TaskId::new(),
                TurnId::new(),
                vec![ContextContributor {
                    name: "system".to_owned(),
                    role: ProviderRole::System,
                    content: "stable".to_owned(),
                    token_budget_hint: 0,
                    source_refs: Vec::new(),
                }],
            )
            .expect("context plan");

        let first = context_message_prefix_digest(&plan, 1).expect("prefix digest");
        assert_eq!(first, context_message_prefix_digest(&plan, 1).unwrap());
        assert_ne!(first, context_message_prefix_digest(&plan, 0).unwrap());
        assert!(context_message_prefix_digest(&plan, 2).is_none());
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

        let message_estimates = request
            .messages
            .iter()
            .map(estimate_message_token)
            .collect::<Vec<_>>();
        let manifest = attributed_contributor_manifest_with_estimates(
            &plan,
            &request,
            &sources,
            &[],
            &message_estimates,
            &[],
        );
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
