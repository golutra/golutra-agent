//! Host-owned delegated-task limits and durable recovery snapshots.
//!

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use golutra_agent_core::{SessionId, TaskId, ThreadId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(crate) const MAX_DELEGATION_DEPTH: u8 = 1;
pub(crate) const DELEGATION_COST_BUDGET_KEY: &str = "_delegation_cost_budget_microusd";
pub(crate) const DEFAULT_DELEGATED_ACTIVE_CHILDREN: usize = 10;
pub(crate) const DEFAULT_DELEGATED_CHILD_TOKEN_RESERVATION: u64 = 4_096;
pub(crate) const MAX_DELEGATED_TOKEN_BUDGET: u64 = 128_000;
pub(crate) const DELEGATION_TOKEN_BUDGET_SEMANTICS: &str = "aggregate_output_reservation";
// A child can consume at most 75% of its parent's remaining local deadline.
const DELEGATED_PARENT_COMPLETION_RESERVE_DIVISOR: u64 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DelegationLimit {
    Cancelled,
    Elapsed,
    Depth,
    ActiveChildren,
    TokenBudget,
    CostBudget,
}

impl DelegationLimit {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Elapsed => "elapsed_budget_exhausted",
            Self::Depth => "maximum_depth_exceeded",
            Self::ActiveChildren => "child_concurrency_exceeded",
            Self::TokenBudget => "token_admission_budget_exceeded",
            Self::CostBudget => "cost_budget_exceeded",
        }
    }

    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::Cancelled => "delegation was cancelled",
            Self::Elapsed => "delegation elapsed-time budget is exhausted",
            Self::Depth => "delegated child reached the maximum delegation depth",
            Self::ActiveChildren => "delegated child concurrency limit is reached",
            Self::TokenBudget => "delegated child output-token admission budget is exhausted",
            Self::CostBudget => "delegated child cost budget is exhausted",
        }
    }
}

#[derive(Debug, Default)]
struct BudgetState {
    active_children: usize,
    started_children: usize,
    reserved_tokens: u64,
    spent_tokens: u64,
    spent_output_tokens: u64,
    reserved_cost_microusd: u64,
    spent_cost_microusd: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DelegationRecoveryState {
    #[serde(default = "default_max_active_children")]
    pub(crate) max_active_children: usize,
    pub(crate) root_session_id: SessionId,
    pub(crate) parent_session_id: Option<SessionId>,
    pub(crate) parent_task_id: Option<TaskId>,
    pub(crate) parent_thread_id: Option<ThreadId>,
    pub(crate) depth: u8,
    pub(crate) remaining_elapsed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) local_remaining_elapsed_ms: Option<u64>,
    pub(crate) max_tokens: Option<u64>,
    pub(crate) max_cost_microusd: Option<u64>,
    pub(crate) started_children: usize,
    pub(crate) spent_tokens: u64,
    // 旧 checkpoint 只有总用量，恢复时保守按总量扣额度，不能凭空释放预算。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) spent_output_tokens: Option<u64>,
    pub(crate) spent_cost_microusd: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TimedDelegationRecoveryState {
    pub(crate) captured_at: DateTime<Utc>,
    pub(crate) state: DelegationRecoveryState,
}

impl TimedDelegationRecoveryState {
    pub(crate) fn refreshed(mut self, now: DateTime<Utc>) -> Self {
        if self.captured_at > now {
            // 未来 checkpoint 无法证明真实剩余时长；直接耗尽预算，避免时钟偏差或脏数据延长期限。
            self.state.remaining_elapsed_ms = self.state.remaining_elapsed_ms.map(|_| 0);
            self.state.local_remaining_elapsed_ms =
                self.state.local_remaining_elapsed_ms.map(|_| 0);
            self.captured_at = now;
            return self;
        }
        let elapsed_ms = now
            .signed_duration_since(self.captured_at)
            .num_milliseconds()
            .max(0);
        let elapsed_ms = u64::try_from(elapsed_ms).unwrap_or(u64::MAX);
        self.state.remaining_elapsed_ms = self
            .state
            .remaining_elapsed_ms
            .map(|remaining| remaining.saturating_sub(elapsed_ms));
        if let Some(local_remaining_elapsed_ms) = &mut self.state.local_remaining_elapsed_ms {
            *local_remaining_elapsed_ms = local_remaining_elapsed_ms.saturating_sub(elapsed_ms);
        }
        self.captured_at = now;
        self
    }
}

/// A shared live admission budget for one root task and all of its descendants.
///
/// 默认仅限制活动子任务数，不从单请求输出容量推导累计限额。
/// 显式恢复的累计预算独立结算；缓存输入及多轮用量仍完整计入观测。
#[derive(Debug)]
pub(crate) struct DelegationBudget {
    max_active_children: usize,
    deadline: Option<Instant>,
    max_tokens: Option<u64>,
    max_cost_microusd: Option<u64>,
    state: Mutex<BudgetState>,
    checkpoint_lock: Arc<AsyncMutex<()>>,
    cancellation: CancellationToken,
}

impl DelegationBudget {
    fn configured(
        max_elapsed_ms: Option<u64>,
        max_cost_microusd: Option<u64>,
        cancellation: CancellationToken,
        max_active_children: usize,
    ) -> Arc<Self> {
        // 每次生成的输出容量不是整个任务的累计配额；只有显式任务时限才创建截止时间。
        Arc::new(Self {
            max_active_children: max_active_children.max(1),
            deadline: max_elapsed_ms.map(|ms| Instant::now() + Duration::from_millis(ms)),
            max_tokens: None,
            max_cost_microusd,
            state: Mutex::new(BudgetState::default()),
            checkpoint_lock: Arc::new(AsyncMutex::new(())),
            cancellation,
        })
    }

    pub(crate) fn remaining_elapsed_ms(&self) -> Option<u64> {
        self.deadline.map(|deadline| {
            u64::try_from(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis(),
            )
            .unwrap_or(u64::MAX)
        })
    }

    pub(crate) fn is_expired(&self) -> bool {
        self.remaining_elapsed_ms() == Some(0)
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) fn reserve(
        self: &Arc<Self>,
        requested_tokens: u64,
        requested_cost_microusd: Option<u64>,
        cancellation: &CancellationToken,
    ) -> Result<Arc<DelegationLease>, DelegationLimit> {
        if cancellation.is_cancelled() || self.cancellation.is_cancelled() {
            return Err(DelegationLimit::Cancelled);
        }
        if self.is_expired() {
            return Err(DelegationLimit::Elapsed);
        }
        let requested_tokens = requested_tokens.clamp(1, MAX_DELEGATED_TOKEN_BUDGET);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.active_children >= self.max_active_children {
            return Err(DelegationLimit::ActiveChildren);
        }
        if self.max_tokens.is_some_and(|max| {
            state
                .spent_output_tokens
                .saturating_add(state.reserved_tokens)
                .saturating_add(requested_tokens)
                > max
        }) {
            return Err(DelegationLimit::TokenBudget);
        }
        let committed_cost = state
            .spent_cost_microusd
            .saturating_add(state.reserved_cost_microusd);
        let requested_cost_microusd = match (self.max_cost_microusd, requested_cost_microusd) {
            // Without a trusted estimate, reserve the entire remaining budget.
            // This keeps admission fail-closed while still allowing one child
            // to run instead of making cost budgets unusable in practice.
            (Some(max_cost), None) => max_cost
                .checked_sub(committed_cost)
                .filter(|remaining| *remaining > 0)
                .ok_or(DelegationLimit::CostBudget)?,
            (Some(max_cost), Some(requested)) => {
                if committed_cost.saturating_add(requested) > max_cost {
                    return Err(DelegationLimit::CostBudget);
                }
                requested
            }
            (None, requested) => requested.unwrap_or_default(),
        };
        state.active_children = state.active_children.saturating_add(1);
        state.started_children = state.started_children.saturating_add(1);
        state.reserved_tokens = state.reserved_tokens.saturating_add(requested_tokens);
        state.reserved_cost_microusd = state
            .reserved_cost_microusd
            .saturating_add(requested_cost_microusd);
        drop(state);
        Ok(Arc::new(DelegationLease {
            budget: self.clone(),
            requested_tokens,
            requested_cost_microusd,
            released: AtomicBool::new(false),
        }))
    }

    pub(crate) fn snapshot(&self) -> Value {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let committed_tokens = state
            .spent_output_tokens
            .saturating_add(state.reserved_tokens);
        let committed_cost_microusd = state
            .spent_cost_microusd
            .saturating_add(state.reserved_cost_microusd);
        json!({
            "remaining_elapsed_ms": self.remaining_elapsed_ms(),
            "max_depth": MAX_DELEGATION_DEPTH,
            "max_active_children": self.max_active_children,
            "active_children": state.active_children,
            "started_children": state.started_children,
            "max_tokens": self.max_tokens,
            "reserved_tokens": state.reserved_tokens,
            "spent_tokens": state.spent_tokens,
            "spent_output_tokens": state.spent_output_tokens,
            // 准入余额必须和使用统计分开；provider usage 可能包含输入 token 和多轮请求。
            "token_admission_committed": committed_tokens,
            "token_admission_remaining": self.max_tokens.map(|max| max.saturating_sub(committed_tokens)),
            "token_budget_semantics": DELEGATION_TOKEN_BUDGET_SEMANTICS,
            "spent_tokens_are_usage_accounting": true,
            "max_cost_microusd": self.max_cost_microusd,
            "reserved_cost_microusd": state.reserved_cost_microusd,
            "spent_cost_microusd": state.spent_cost_microusd,
            "cost_admission_committed": committed_cost_microusd,
            "cost_admission_remaining": self
                .max_cost_microusd
                .map(|max| max.saturating_sub(committed_cost_microusd)),
        })
    }
}

#[derive(Debug)]
pub(crate) struct DelegationLease {
    budget: Arc<DelegationBudget>,
    requested_tokens: u64,
    requested_cost_microusd: u64,
    released: AtomicBool,
}

impl DelegationLease {
    pub(crate) fn finish(
        &self,
        actual_tokens: u64,
        actual_cost_microusd: Option<u64>,
        actual_output_tokens: Option<u64>,
    ) {
        // A hard cost budget cannot treat missing provider pricing as free. The admission
        // reservation is the conservative accounting fallback when the response has no cost.
        self.release(
            actual_tokens,
            actual_cost_microusd.unwrap_or(self.requested_cost_microusd),
            actual_output_tokens.unwrap_or(actual_tokens),
        );
    }

    fn release(&self, actual_tokens: u64, actual_cost_microusd: u64, actual_output_tokens: u64) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut state = self
            .budget
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active_children = state.active_children.saturating_sub(1);
        state.reserved_tokens = state.reserved_tokens.saturating_sub(self.requested_tokens);
        state.reserved_cost_microusd = state
            .reserved_cost_microusd
            .saturating_sub(self.requested_cost_microusd);
        state.spent_tokens = state.spent_tokens.saturating_add(actual_tokens);
        state.spent_output_tokens = state
            .spent_output_tokens
            .saturating_add(actual_output_tokens);
        state.spent_cost_microusd = state
            .spent_cost_microusd
            .saturating_add(actual_cost_microusd);
    }
}

impl Drop for DelegationLease {
    fn drop(&mut self) {
        // An aborted child still consumed the admission reservation unless a completed usage
        // record proves otherwise. Settling with zero would make the budget under-report work
        // on every exceptional path and allow later descendants to exceed the cap.
        self.release(
            self.requested_tokens,
            self.requested_cost_microusd,
            self.requested_tokens,
        );
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DelegationContext {
    pub(crate) root_session_id: SessionId,
    pub(crate) parent_session_id: Option<SessionId>,
    pub(crate) parent_task_id: Option<TaskId>,
    pub(crate) parent_thread_id: Option<ThreadId>,
    pub(crate) depth: u8,
    pub(crate) budget: Arc<DelegationBudget>,
    local_deadline: Option<Instant>,
    lease: Option<Arc<DelegationLease>>,
}

impl DelegationContext {
    pub(crate) fn max_active_children(&self) -> usize {
        self.budget.max_active_children
    }

    #[cfg(test)]
    pub(crate) fn root(
        session_id: SessionId,
        max_elapsed_ms: Option<u64>,
        max_cost_microusd: Option<u64>,
        cancellation: CancellationToken,
    ) -> Self {
        Self::configured(
            session_id,
            max_elapsed_ms,
            max_cost_microusd,
            cancellation,
            DEFAULT_DELEGATED_ACTIVE_CHILDREN,
        )
    }

    pub(crate) fn configured(
        session_id: SessionId,
        max_elapsed_ms: Option<u64>,
        max_cost_microusd: Option<u64>,
        cancellation: CancellationToken,
        max_active_children: usize,
    ) -> Self {
        let budget = DelegationBudget::configured(
            max_elapsed_ms,
            max_cost_microusd,
            cancellation,
            max_active_children,
        );
        let local_deadline = budget.deadline;
        Self {
            root_session_id: session_id,
            parent_session_id: None,
            parent_task_id: None,
            parent_thread_id: None,
            depth: 0,
            budget,
            local_deadline,
            lease: None,
        }
    }

    pub(crate) fn recovered(
        recovered: TimedDelegationRecoveryState,
        now: DateTime<Utc>,
        cancellation: CancellationToken,
    ) -> Result<Self, &'static str> {
        let recovered = recovered.refreshed(now).state;
        if recovered.depth > MAX_DELEGATION_DEPTH {
            return Err("recovered delegation depth exceeds the supported maximum");
        }
        if recovered.max_tokens == Some(0) {
            return Err("recovered delegation token budget is invalid");
        }
        if recovered.max_active_children == 0 {
            return Err("recovered delegated concurrency must be positive");
        }
        let now = Instant::now();
        let root_remaining_elapsed_ms = recovered.remaining_elapsed_ms;
        let local_remaining_elapsed_ms = earliest_remaining(
            recovered.local_remaining_elapsed_ms,
            root_remaining_elapsed_ms,
        );
        let budget = Arc::new(DelegationBudget {
            max_active_children: recovered.max_active_children,
            deadline: root_remaining_elapsed_ms.map(|ms| now + Duration::from_millis(ms)),
            max_tokens: recovered.max_tokens,
            max_cost_microusd: recovered.max_cost_microusd,
            state: Mutex::new(BudgetState {
                active_children: 0,
                started_children: recovered.started_children,
                reserved_tokens: 0,
                spent_tokens: recovered.spent_tokens,
                spent_output_tokens: recovered
                    .spent_output_tokens
                    .unwrap_or(recovered.spent_tokens),
                reserved_cost_microusd: 0,
                spent_cost_microusd: recovered.spent_cost_microusd,
            }),
            checkpoint_lock: Arc::new(AsyncMutex::new(())),
            cancellation,
        });
        Ok(Self {
            root_session_id: recovered.root_session_id,
            parent_session_id: recovered.parent_session_id,
            parent_task_id: recovered.parent_task_id,
            parent_thread_id: recovered.parent_thread_id,
            depth: recovered.depth,
            budget,
            local_deadline: local_remaining_elapsed_ms.map(|ms| now + Duration::from_millis(ms)),
            lease: None,
        })
    }

    pub(crate) fn child(
        &self,
        parent_session_id: SessionId,
        parent_task_id: TaskId,
        parent_thread_id: ThreadId,
        requested_tokens: u64,
        requested_cost_microusd: Option<u64>,
        cancellation: &CancellationToken,
    ) -> Result<Self, DelegationLimit> {
        if self.depth >= MAX_DELEGATION_DEPTH {
            return Err(DelegationLimit::Depth);
        }
        let parent_remaining_elapsed_ms = self.remaining_elapsed_ms();
        if parent_remaining_elapsed_ms == Some(0) {
            return Err(DelegationLimit::Elapsed);
        }
        let lease = self
            .budget
            .reserve(requested_tokens, requested_cost_microusd, cancellation)?;
        let local_deadline = parent_remaining_elapsed_ms
            .map(delegated_child_elapsed_ms)
            .map(|ms| Instant::now() + Duration::from_millis(ms));
        Ok(Self {
            root_session_id: self.root_session_id,
            parent_session_id: Some(parent_session_id),
            parent_task_id: Some(parent_task_id),
            parent_thread_id: Some(parent_thread_id),
            depth: self.depth.saturating_add(1),
            budget: self.budget.clone(),
            local_deadline,
            lease: Some(lease),
        })
    }

    pub(crate) fn remaining_elapsed_ms(&self) -> Option<u64> {
        let local = self.local_deadline.map(|deadline| {
            u64::try_from(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis(),
            )
            .unwrap_or(u64::MAX)
        });
        earliest_remaining(local, self.budget.remaining_elapsed_ms())
    }

    pub(crate) fn is_expired(&self) -> bool {
        self.remaining_elapsed_ms() == Some(0)
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.budget.cancellation()
    }

    pub(crate) fn finish(
        &self,
        actual_tokens: u64,
        actual_cost_microusd: Option<u64>,
        actual_output_tokens: Option<u64>,
    ) {
        if let Some(lease) = &self.lease {
            lease.finish(actual_tokens, actual_cost_microusd, actual_output_tokens);
        }
    }

    pub(crate) fn checkpoint_lock(&self) -> Arc<AsyncMutex<()>> {
        self.budget.checkpoint_lock.clone()
    }

    pub(crate) fn canonical_task_id(&self, current_task_id: TaskId) -> TaskId {
        if self.depth == 0 {
            current_task_id
        } else {
            self.parent_task_id.unwrap_or(current_task_id)
        }
    }

    pub(crate) fn metadata(&self) -> Value {
        json!({
            "root_session_id": self.root_session_id,
            "parent_session_id": self.parent_session_id,
            "parent_task_id": self.parent_task_id,
            "parent_thread_id": self.parent_thread_id,
            "depth": self.depth,
            "local_remaining_elapsed_ms": self.remaining_elapsed_ms(),
            "budget": self.budget.snapshot(),
        })
    }

    /// Convert active reservations to spent usage for crash recovery. The old
    /// process can no longer own an active child, and treating an uncertain
    /// reservation as free would permit the recovered task to exceed its cap.
    pub(crate) fn recovery_state(
        &self,
        captured_at: DateTime<Utc>,
    ) -> TimedDelegationRecoveryState {
        self.recovery_state_with_settlement(captured_at, None)
    }

    /// Build the write-ahead state for settling this context's lease. The
    /// child's actual usage replaces its reservation in the aggregate; it is
    /// never added on top of that reservation.
    pub(crate) fn settlement_recovery_state(
        &self,
        captured_at: DateTime<Utc>,
        actual_tokens: u64,
        actual_cost_microusd: Option<u64>,
        actual_output_tokens: Option<u64>,
    ) -> TimedDelegationRecoveryState {
        self.recovery_state_with_settlement(
            captured_at,
            Some((actual_tokens, actual_cost_microusd, actual_output_tokens)),
        )
    }

    fn recovery_state_with_settlement(
        &self,
        captured_at: DateTime<Utc>,
        settlement: Option<(u64, Option<u64>, Option<u64>)>,
    ) -> TimedDelegationRecoveryState {
        let state = self
            .budget
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (
            mut spent_tokens,
            mut spent_cost_microusd,
            mut spent_output_tokens,
            unsettled_children,
        ) = match (&self.lease, settlement) {
            (Some(lease), Some((actual_tokens, actual_cost_microusd, actual_output_tokens)))
                if !lease.released.load(Ordering::Acquire) =>
            {
                (
                    state
                        .spent_tokens
                        .saturating_add(
                            state.reserved_tokens.saturating_sub(lease.requested_tokens),
                        )
                        .saturating_add(actual_tokens),
                    state
                        .spent_cost_microusd
                        .saturating_add(
                            state
                                .reserved_cost_microusd
                                .saturating_sub(lease.requested_cost_microusd),
                        )
                        .saturating_add(
                            actual_cost_microusd.unwrap_or(lease.requested_cost_microusd),
                        ),
                    state
                        .spent_output_tokens
                        .saturating_add(
                            state.reserved_tokens.saturating_sub(lease.requested_tokens),
                        )
                        .saturating_add(actual_output_tokens.unwrap_or(actual_tokens)),
                    state.active_children.saturating_sub(1),
                )
            }
            _ => (
                state.spent_tokens.saturating_add(state.reserved_tokens),
                state
                    .spent_cost_microusd
                    .saturating_add(state.reserved_cost_microusd),
                state
                    .spent_output_tokens
                    .saturating_add(state.reserved_tokens),
                state.active_children,
            ),
        };
        if unsettled_children > 0 {
            // A child's aggregate multi-turn usage can exceed its requested
            // output reservation. Until an actual settlement is durable, the
            // only safe admission checkpoint is to consume the remaining cap.
            if let Some(max_tokens) = self.budget.max_tokens {
                spent_tokens = spent_tokens.max(max_tokens);
                spent_output_tokens = spent_output_tokens.max(max_tokens);
            }
            if let Some(max_cost_microusd) = self.budget.max_cost_microusd {
                spent_cost_microusd = spent_cost_microusd.max(max_cost_microusd);
            }
        }
        let remaining_elapsed_ms = self.budget.remaining_elapsed_ms();
        TimedDelegationRecoveryState {
            captured_at,
            state: DelegationRecoveryState {
                max_active_children: self.budget.max_active_children,
                root_session_id: self.root_session_id,
                parent_session_id: None,
                parent_task_id: None,
                parent_thread_id: None,
                depth: 0,
                remaining_elapsed_ms,
                local_remaining_elapsed_ms: remaining_elapsed_ms,
                max_tokens: self.budget.max_tokens,
                max_cost_microusd: self.budget.max_cost_microusd,
                started_children: state.started_children,
                spent_tokens,
                spent_output_tokens: Some(spent_output_tokens),
                spent_cost_microusd,
            },
        }
    }
}

fn earliest_remaining(local: Option<u64>, root: Option<u64>) -> Option<u64> {
    match (local, root) {
        (Some(local), Some(root)) => Some(local.min(root)),
        (local, root) => local.or(root),
    }
}

fn default_max_active_children() -> usize {
    DEFAULT_DELEGATED_ACTIVE_CHILDREN
}

fn delegated_child_elapsed_ms(parent_remaining_elapsed_ms: u64) -> u64 {
    if parent_remaining_elapsed_ms <= 1 {
        return parent_remaining_elapsed_ms;
    }
    let reserved_for_parent = parent_remaining_elapsed_ms
        .saturating_add(DELEGATED_PARENT_COMPLETION_RESERVE_DIVISOR - 1)
        / DELEGATED_PARENT_COMPLETION_RESERVE_DIVISOR;
    parent_remaining_elapsed_ms
        .saturating_sub(reserved_for_parent)
        .max(1)
}

pub(crate) fn requested_token_reservation(provider_max_tokens: Option<u64>) -> u64 {
    provider_max_tokens
        .unwrap_or(DEFAULT_DELEGATED_CHILD_TOKEN_RESERVATION)
        .clamp(1, MAX_DELEGATED_TOKEN_BUDGET)
}

pub(crate) fn cost_budget_from_payload(payload: &Value) -> Result<Option<u64>, &'static str> {
    match payload.get(DELEGATION_COST_BUDGET_KEY) {
        None | Some(Value::Null) => Ok(None),
        Some(value) if value.as_u64().is_some() => Ok(value.as_u64()),
        Some(_) => Err("_delegation_cost_budget_microusd must be a non-negative integer"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_elapsed_budget_reserves_parent_completion_time() {
        assert_eq!(delegated_child_elapsed_ms(10_000), 7_500);
        assert_eq!(delegated_child_elapsed_ms(4), 3);
        assert_eq!(delegated_child_elapsed_ms(3), 2);
        assert_eq!(delegated_child_elapsed_ms(2), 1);
        assert_eq!(delegated_child_elapsed_ms(1), 1);
        assert_eq!(delegated_child_elapsed_ms(0), 0);
    }

    #[test]
    fn nested_children_receive_strictly_smaller_local_elapsed_budgets() {
        let cancellation = CancellationToken::new();
        let root = DelegationContext::root(
            SessionId::new(),
            Some(1_000_000),
            None,
            cancellation.clone(),
        );
        let child = root
            .child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                1_024,
                None,
                &cancellation,
            )
            .expect("child");
        let root_remaining = root.remaining_elapsed_ms().unwrap();
        let child_remaining = child.remaining_elapsed_ms().unwrap();
        assert!(child_remaining < root_remaining);
        assert!(child_remaining <= delegated_child_elapsed_ms(root_remaining.saturating_add(2)));
        assert!(matches!(
            child.child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                1_024,
                None,
                &cancellation,
            ),
            Err(DelegationLimit::Depth)
        ));
    }

    #[test]
    fn exhausted_local_budget_blocks_nested_delegation() {
        let cancellation = CancellationToken::new();
        let now = Instant::now();
        let root =
            DelegationContext::root(SessionId::new(), Some(10_000), None, cancellation.clone());
        let expired = DelegationContext {
            local_deadline: Some(now),
            ..root
        };

        assert!(matches!(
            expired.child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                1_024,
                None,
                &cancellation,
            ),
            Err(DelegationLimit::Elapsed)
        ));
    }

    #[test]
    fn recovery_preserves_local_deadline_without_shrinking_root_budget() {
        let captured_at = Utc::now();
        let recovered = DelegationContext::recovered(
            TimedDelegationRecoveryState {
                captured_at,
                state: DelegationRecoveryState {
                    max_active_children: DEFAULT_DELEGATED_ACTIVE_CHILDREN,
                    root_session_id: SessionId::new(),
                    parent_session_id: Some(SessionId::new()),
                    parent_task_id: Some(TaskId::new()),
                    parent_thread_id: Some(ThreadId::new()),
                    depth: 1,
                    remaining_elapsed_ms: Some(100_000),
                    local_remaining_elapsed_ms: Some(25_000),
                    max_tokens: Some(8_192),
                    max_cost_microusd: None,
                    started_children: 0,
                    spent_tokens: 0,
                    spent_output_tokens: None,
                    spent_cost_microusd: 0,
                },
            },
            captured_at,
            CancellationToken::new(),
        )
        .expect("recovered child context");

        assert!(recovered.remaining_elapsed_ms().unwrap() <= 25_000);
        assert!(recovered.remaining_elapsed_ms().unwrap() > 24_900);
        assert!(recovered.budget.remaining_elapsed_ms().unwrap() > 99_900);

        let canonical = recovered.recovery_state(captured_at);
        assert_eq!(canonical.state.depth, 0);
        assert_eq!(
            canonical.state.local_remaining_elapsed_ms,
            canonical.state.remaining_elapsed_ms
        );
        assert!(canonical.state.remaining_elapsed_ms.unwrap() > 99_900);
    }

    #[test]
    fn refreshing_recovery_consumes_root_and_local_elapsed_budgets() {
        let captured_at = Utc::now();
        let refreshed = TimedDelegationRecoveryState {
            captured_at,
            state: DelegationRecoveryState {
                max_active_children: DEFAULT_DELEGATED_ACTIVE_CHILDREN,
                root_session_id: SessionId::new(),
                parent_session_id: None,
                parent_task_id: None,
                parent_thread_id: None,
                depth: 0,
                remaining_elapsed_ms: Some(10_000),
                local_remaining_elapsed_ms: Some(4_000),
                max_tokens: Some(8_192),
                max_cost_microusd: None,
                started_children: 0,
                spent_tokens: 0,
                spent_output_tokens: None,
                spent_cost_microusd: 0,
            },
        }
        .refreshed(captured_at + chrono::Duration::milliseconds(1_500));

        assert_eq!(refreshed.state.remaining_elapsed_ms, Some(8_500));
        assert_eq!(refreshed.state.local_remaining_elapsed_ms, Some(2_500));
    }

    #[test]
    fn future_recovery_checkpoint_exhausts_elapsed_budget() {
        let now = Utc::now();
        let recovered = DelegationContext::recovered(
            TimedDelegationRecoveryState {
                captured_at: now + chrono::Duration::minutes(1),
                state: DelegationRecoveryState {
                    max_active_children: DEFAULT_DELEGATED_ACTIVE_CHILDREN,
                    root_session_id: SessionId::new(),
                    parent_session_id: None,
                    parent_task_id: None,
                    parent_thread_id: None,
                    depth: 0,
                    remaining_elapsed_ms: Some(10_000),
                    local_remaining_elapsed_ms: None,
                    max_tokens: Some(8_192),
                    max_cost_microusd: None,
                    started_children: 0,
                    spent_tokens: 0,
                    spent_output_tokens: None,
                    spent_cost_microusd: 0,
                },
            },
            now,
            CancellationToken::new(),
        )
        .expect("future checkpoint is recovered with an exhausted deadline");

        assert!(recovered.is_expired());
        assert_eq!(recovered.remaining_elapsed_ms(), Some(0));
        assert_eq!(recovered.budget.remaining_elapsed_ms(), Some(0));
    }

    #[test]
    fn budget_rejects_depth_and_configured_concurrency_limits() {
        let cancellation = CancellationToken::new();
        let root = DelegationContext::configured(
            SessionId::new(),
            Some(10_000),
            None,
            cancellation.clone(),
            2,
        );
        let parent_session = SessionId::new();
        let parent_task = TaskId::new();
        let parent_thread = ThreadId::new();
        let first = root
            .child(
                parent_session,
                parent_task,
                parent_thread,
                1_024,
                Some(0),
                &cancellation,
            )
            .expect("first child");
        let second = root
            .child(
                parent_session,
                parent_task,
                parent_thread,
                1_024,
                Some(0),
                &cancellation,
            )
            .expect("second child");
        assert!(matches!(
            root.child(
                parent_session,
                parent_task,
                parent_thread,
                1_024,
                Some(0),
                &cancellation,
            ),
            Err(DelegationLimit::ActiveChildren)
        ));
        drop(first);
        drop(second);
        let child = root
            .child(
                parent_session,
                parent_task,
                parent_thread,
                1_024,
                Some(0),
                &cancellation,
            )
            .expect("reusable child slot");
        assert!(matches!(
            child.child(
                parent_session,
                parent_task,
                parent_thread,
                1_024,
                Some(0),
                &cancellation,
            ),
            Err(DelegationLimit::Depth)
        ));
    }

    #[test]
    fn budget_tracks_actual_usage_and_cancellation() {
        let cancellation = CancellationToken::new();
        let root = DelegationContext::root(
            SessionId::new(),
            Some(10_000),
            Some(10),
            cancellation.clone(),
        );
        let child = root
            .child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                1_024,
                Some(5),
                &cancellation,
            )
            .expect("child");
        child.finish(900, Some(3), None);
        let snapshot = root.budget.snapshot();
        assert_eq!(snapshot["active_children"], 0);
        assert_eq!(snapshot["spent_tokens"], 900);
        assert_eq!(snapshot["spent_cost_microusd"], 3);

        cancellation.cancel();
        assert!(matches!(
            root.child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                1,
                Some(0),
                &cancellation,
            ),
            Err(DelegationLimit::Cancelled)
        ));
    }

    #[test]
    fn long_input_usage_does_not_exhaust_output_admission_and_recovery_preserves_both() {
        let cancellation = CancellationToken::new();
        let root =
            DelegationContext::root(SessionId::new(), Some(10_000), None, cancellation.clone());
        let launch = |context: &DelegationContext| {
            context.child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                4_096,
                None,
                &cancellation,
            )
        };
        let child = launch(&root).unwrap();
        let now = Utc::now();
        let checkpoint = child.settlement_recovery_state(now, 200_000, None, Some(600));
        child.finish(200_000, None, Some(600));
        assert_eq!(root.budget.snapshot()["spent_tokens"], 200_000);
        assert_eq!(root.budget.snapshot()["spent_output_tokens"], 600);
        assert_eq!(root.budget.snapshot()["token_admission_committed"], 600);
        assert_eq!(checkpoint.state.spent_tokens, 200_000);
        assert_eq!(checkpoint.state.spent_output_tokens, Some(600));
        let recovered =
            DelegationContext::recovered(checkpoint, now, cancellation.clone()).unwrap();
        let resumed =
            launch(&recovered).expect("long prompt must not block same-child continuation");
        resumed.finish(1_000, None, Some(40));
        assert_eq!(recovered.budget.snapshot()["spent_tokens"], 201_000);
        assert_eq!(recovered.budget.snapshot()["spent_output_tokens"], 640);
    }

    #[test]
    fn output_usage_does_not_create_an_implicit_lifetime_budget() {
        for output in [Some(50_000), None] {
            let cancellation = CancellationToken::new();
            let root =
                DelegationContext::root(SessionId::new(), Some(10_000), None, cancellation.clone());
            let child = root
                .child(
                    SessionId::new(),
                    TaskId::new(),
                    ThreadId::new(),
                    4_096,
                    None,
                    &cancellation,
                )
                .unwrap();
            child.finish(100_000, None, output);
            root.child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                4_096,
                None,
                &cancellation,
            )
            .expect("usage accounting must not impose a lifetime cap");
        }
    }

    #[test]
    fn token_budget_exposes_output_reservation_semantics_without_clamping_usage() {
        let cancellation = CancellationToken::new();
        let root = DelegationContext::configured(
            SessionId::new(),
            Some(10_000),
            None,
            cancellation.clone(),
            2,
        );
        let child = root
            .child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                1_024,
                None,
                &cancellation,
            )
            .expect("child");
        child.finish(10_000, None, None);

        let snapshot = root.budget.snapshot();
        assert_eq!(
            snapshot["token_budget_semantics"],
            DELEGATION_TOKEN_BUDGET_SEMANTICS
        );
        assert_eq!(snapshot["spent_tokens"], 10_000);
        assert_eq!(snapshot["spent_tokens_are_usage_accounting"], true);
        assert_eq!(snapshot["token_admission_committed"], 10_000);
        assert_eq!(snapshot["token_admission_remaining"], Value::Null);
    }

    #[test]
    fn recovered_usage_over_admission_cap_blocks_new_children_without_clamping_history() {
        let captured_at = Utc::now();
        let recovered = DelegationContext::recovered(
            TimedDelegationRecoveryState {
                captured_at,
                state: DelegationRecoveryState {
                    max_active_children: DEFAULT_DELEGATED_ACTIVE_CHILDREN,
                    root_session_id: SessionId::new(),
                    parent_session_id: None,
                    parent_task_id: None,
                    parent_thread_id: None,
                    depth: 0,
                    remaining_elapsed_ms: Some(10_000),
                    local_remaining_elapsed_ms: None,
                    max_tokens: Some(8_192),
                    max_cost_microusd: Some(10),
                    started_children: 1,
                    spent_tokens: 8_192 + 500,
                    spent_output_tokens: None,
                    spent_cost_microusd: 12,
                },
            },
            captured_at,
            CancellationToken::new(),
        )
        .expect("recovered usage is valid accounting evidence");

        let snapshot = recovered.budget.snapshot();
        assert_eq!(snapshot["spent_tokens"], 8_192 + 500);
        assert_eq!(snapshot["token_admission_remaining"], 0);
        assert_eq!(snapshot["cost_admission_remaining"], 0);
        assert!(matches!(
            recovered.child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                1,
                Some(0),
                &CancellationToken::new(),
            ),
            Err(DelegationLimit::TokenBudget)
        ));
    }

    #[test]
    fn recovered_cost_usage_at_cap_rejects_unknown_cost_admission() {
        let captured_at = Utc::now();
        let recovered = DelegationContext::recovered(
            TimedDelegationRecoveryState {
                captured_at,
                state: DelegationRecoveryState {
                    max_active_children: DEFAULT_DELEGATED_ACTIVE_CHILDREN,
                    root_session_id: SessionId::new(),
                    parent_session_id: None,
                    parent_task_id: None,
                    parent_thread_id: None,
                    depth: 0,
                    remaining_elapsed_ms: Some(10_000),
                    local_remaining_elapsed_ms: None,
                    max_tokens: Some(8_192),
                    max_cost_microusd: Some(10),
                    started_children: 1,
                    spent_tokens: 0,
                    spent_output_tokens: None,
                    spent_cost_microusd: 10,
                },
            },
            captured_at,
            CancellationToken::new(),
        )
        .expect("recovered cost usage");

        assert!(matches!(
            recovered.child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                1,
                None,
                &CancellationToken::new(),
            ),
            Err(DelegationLimit::CostBudget)
        ));
    }

    #[test]
    fn hard_cost_budget_conservatively_reserves_unknown_child_cost() {
        let cancellation = CancellationToken::new();
        let root = DelegationContext::root(
            SessionId::new(),
            Some(10_000),
            Some(10),
            cancellation.clone(),
        );

        let child = root
            .child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                1_024,
                None,
                &cancellation,
            )
            .expect("unknown cost reserves the remaining budget");
        assert_eq!(root.budget.snapshot()["reserved_cost_microusd"], 10);
        assert!(matches!(
            root.child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                1_024,
                None,
                &cancellation,
            ),
            Err(DelegationLimit::CostBudget)
        ));
        drop(child);
        assert_eq!(root.budget.snapshot()["spent_cost_microusd"], 10);
    }

    #[test]
    fn unknown_actual_cost_consumes_the_admission_reservation() {
        let cancellation = CancellationToken::new();
        let root = DelegationContext::root(
            SessionId::new(),
            Some(10_000),
            Some(10),
            cancellation.clone(),
        );
        let child = root
            .child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                1_024,
                Some(5),
                &cancellation,
            )
            .expect("child");
        child.finish(1_024, None, None);

        let snapshot = root.budget.snapshot();
        assert_eq!(snapshot["reserved_cost_microusd"], 0);
        assert_eq!(snapshot["spent_cost_microusd"], 5);
    }

    #[test]
    fn dropping_a_child_consumes_its_admission_reservation() {
        let cancellation = CancellationToken::new();
        let root = DelegationContext::root(
            SessionId::new(),
            Some(10_000),
            Some(10),
            cancellation.clone(),
        );
        let child = root
            .child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                1_024,
                Some(5),
                &cancellation,
            )
            .expect("child");
        drop(child);

        let snapshot = root.budget.snapshot();
        assert_eq!(snapshot["active_children"], 0);
        assert_eq!(snapshot["reserved_tokens"], 0);
        assert_eq!(snapshot["spent_tokens"], 1_024);
        assert_eq!(snapshot["spent_cost_microusd"], 5);
    }

    #[test]
    fn malformed_cost_budget_is_rejected_instead_of_ignored() {
        for value in [json!("5"), json!(-1), json!(true), json!(1.5)] {
            let payload = json!({DELEGATION_COST_BUDGET_KEY: value});
            assert_eq!(
                cost_budget_from_payload(&payload),
                Err("_delegation_cost_budget_microusd must be a non-negative integer")
            );
        }
        assert_eq!(
            cost_budget_from_payload(&json!({DELEGATION_COST_BUDGET_KEY: 0})),
            Ok(Some(0))
        );
    }

    #[test]
    fn recovered_budget_preserves_spent_usage_and_child_count() {
        let cancellation = CancellationToken::new();
        let root_session_id = SessionId::new();
        let root = DelegationContext::root(
            root_session_id,
            Some(10_000),
            Some(10),
            cancellation.clone(),
        );
        let child = root
            .child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                1_024,
                Some(5),
                &cancellation,
            )
            .expect("child admission");
        child.finish(900, Some(4), None);

        let recovered = DelegationContext::recovered(
            root.recovery_state(Utc::now()),
            Utc::now(),
            CancellationToken::new(),
        )
        .expect("recovered budget");
        let snapshot = recovered.budget.snapshot();
        assert_eq!(recovered.root_session_id, root_session_id);
        assert_eq!(snapshot["started_children"], 1);
        assert_eq!(snapshot["spent_tokens"], 900);
        assert_eq!(snapshot["spent_cost_microusd"], 4);
        assert_eq!(snapshot["max_cost_microusd"], 10);

        recovered
            .child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                1_024,
                Some(6),
                &CancellationToken::new(),
            )
            .expect("remaining cost budget remains available");
        assert!(matches!(
            recovered.child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                1_024,
                Some(1),
                &CancellationToken::new(),
            ),
            Err(DelegationLimit::CostBudget)
        ));
    }

    #[test]
    fn unsettled_usage_is_fail_closed_and_durable_settlement_replaces_the_reservation() {
        let cancellation = CancellationToken::new();
        let root = DelegationContext::root(
            SessionId::new(),
            Some(10_000),
            Some(10_000),
            cancellation.clone(),
        );
        let child = root
            .child(
                SessionId::new(),
                TaskId::new(),
                ThreadId::new(),
                1_024,
                Some(1_000),
                &cancellation,
            )
            .expect("child admission");

        let reservation = root.recovery_state(Utc::now());
        assert_eq!(reservation.state.max_tokens, None);
        assert!(reservation.state.spent_tokens > 0);
        assert_eq!(reservation.state.spent_cost_microusd, 10_000);

        let settlement = child.settlement_recovery_state(Utc::now(), 5_000, Some(2_000), None);
        assert_eq!(settlement.state.started_children, 1);
        assert_eq!(settlement.state.spent_tokens, 5_000);
        assert_eq!(settlement.state.spent_cost_microusd, 2_000);

        child.finish(5_000, Some(2_000), None);
        let after_finish = root.recovery_state(settlement.captured_at);
        assert_eq!(after_finish.state.spent_tokens, 5_000);
        assert_eq!(after_finish.state.spent_cost_microusd, 2_000);
    }

    #[test]
    fn default_ten_slots_are_atomic_and_reusable_without_a_cumulative_limit() {
        let root = DelegationContext::root(SessionId::new(), None, None, CancellationToken::new());
        let barrier = Arc::new(std::sync::Barrier::new(20));
        let handles: Vec<_> = (0..20)
            .map(|_| {
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    root.child(
                        SessionId::new(),
                        TaskId::new(),
                        ThreadId::new(),
                        1,
                        Some(0),
                        &CancellationToken::new(),
                    )
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 10);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(DelegationLimit::ActiveChildren)))
                .count(),
            10
        );
        for child in results.into_iter().flatten() {
            child.finish(0, Some(0), None);
        }
        for _ in 0..60 {
            let child = root
                .child(
                    SessionId::new(),
                    TaskId::new(),
                    ThreadId::new(),
                    1,
                    Some(0),
                    &CancellationToken::new(),
                )
                .unwrap();
            child.finish(100_000, Some(0), Some(50_000));
        }
        let state = root.recovery_state(Utc::now());
        assert_eq!(state.state.started_children, 70);
        assert_eq!(state.state.max_tokens, None);
        assert_eq!(state.state.remaining_elapsed_ms, None);
        assert_eq!(state.state.spent_output_tokens, Some(3_000_000));
        let recovered = DelegationContext::recovered(
            state,
            Utc::now() + chrono::Duration::hours(24),
            CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(recovered.max_active_children(), 10);
        assert_eq!(recovered.remaining_elapsed_ms(), None);
        assert!(
            recovered
                .child(
                    SessionId::new(),
                    TaskId::new(),
                    ThreadId::new(),
                    1,
                    Some(0),
                    &CancellationToken::new()
                )
                .is_ok()
        );
        assert!(
            recovered.metadata()["budget"]
                .get("max_total_children")
                .is_none()
        );
    }

    #[test]
    fn configured_concurrency_survives_recovery_and_legacy_defaults_to_ten() {
        let root = DelegationContext::configured(
            SessionId::new(),
            None,
            None,
            CancellationToken::new(),
            3,
        );
        let snapshot = root.recovery_state(Utc::now());
        let recovered =
            DelegationContext::recovered(snapshot.clone(), Utc::now(), CancellationToken::new())
                .unwrap();
        assert_eq!(recovered.max_active_children(), 3);
        let mut legacy = serde_json::to_value(snapshot).unwrap();
        legacy["state"]
            .as_object_mut()
            .unwrap()
            .remove("max_active_children");
        let legacy = serde_json::from_value(legacy).unwrap();
        assert_eq!(
            DelegationContext::recovered(legacy, Utc::now(), CancellationToken::new())
                .unwrap()
                .max_active_children(),
            10
        );
    }
}
