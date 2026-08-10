//! In-process limits for host-owned delegated tasks.
//!
//! The policy is intentionally ephemeral. Durable task/thread events retain
//! the parent identity and the admission snapshot, while this module owns the
//! live counters and cancellation linkage used by the current runtime host.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use golutra_core::{SessionId, TaskId, ThreadId};
use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(crate) const MAX_DELEGATION_DEPTH: u8 = 2;
pub(crate) const DELEGATION_COST_BUDGET_KEY: &str = "_delegation_cost_budget_microusd";
pub(crate) const MAX_DELEGATED_ACTIVE_CHILDREN: usize = 2;
pub(crate) const MAX_DELEGATED_TOTAL_CHILDREN: usize = 8;
pub(crate) const DEFAULT_DELEGATED_ELAPSED_MS: u64 = 30 * 60 * 1_000;
pub(crate) const DEFAULT_DELEGATED_CHILD_TOKEN_RESERVATION: u64 = 4_096;
pub(crate) const MIN_DELEGATED_TOKEN_BUDGET: u64 = 8_192;
pub(crate) const MAX_DELEGATED_TOKEN_BUDGET: u64 = 128_000;
pub(crate) const DELEGATION_TOKEN_BUDGET_SEMANTICS: &str = "aggregate_output_reservation";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DelegationLimit {
    Cancelled,
    Elapsed,
    Depth,
    ActiveChildren,
    TotalChildren,
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
            Self::TotalChildren => "child_count_exceeded",
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
            Self::TotalChildren => "delegated child count limit is reached",
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
    reserved_cost_microusd: u64,
    spent_cost_microusd: u64,
}

/// A shared live admission budget for one root task and all of its descendants.
///
/// `max_tokens` is derived from each child's requested provider output allowance and is used
/// to reserve future children before they start. `spent_tokens` records observed provider usage
/// (which can include input tokens and multiple turns), so it is accounting evidence rather than
/// a strict total-token cancellation cap.
#[derive(Debug)]
pub(crate) struct DelegationBudget {
    deadline: Instant,
    max_tokens: u64,
    max_cost_microusd: Option<u64>,
    state: Mutex<BudgetState>,
    cancellation: CancellationToken,
}

impl DelegationBudget {
    pub(crate) fn root(
        max_elapsed_ms: Option<u64>,
        provider_max_tokens: Option<u64>,
        max_cost_microusd: Option<u64>,
        cancellation: CancellationToken,
    ) -> Arc<Self> {
        let elapsed_ms = max_elapsed_ms
            .unwrap_or(DEFAULT_DELEGATED_ELAPSED_MS)
            .clamp(1, DEFAULT_DELEGATED_ELAPSED_MS);
        let per_child = provider_max_tokens
            .unwrap_or(DEFAULT_DELEGATED_CHILD_TOKEN_RESERVATION)
            .clamp(1, MAX_DELEGATED_TOKEN_BUDGET);
        let max_tokens = per_child
            .saturating_mul(MAX_DELEGATED_TOTAL_CHILDREN as u64)
            .clamp(MIN_DELEGATED_TOKEN_BUDGET, MAX_DELEGATED_TOKEN_BUDGET);
        Arc::new(Self {
            deadline: Instant::now() + Duration::from_millis(elapsed_ms),
            max_tokens,
            max_cost_microusd,
            state: Mutex::new(BudgetState::default()),
            cancellation,
        })
    }

    pub(crate) fn remaining_elapsed_ms(&self) -> u64 {
        u64::try_from(
            self.deadline
                .saturating_duration_since(Instant::now())
                .as_millis(),
        )
        .unwrap_or(u64::MAX)
    }

    pub(crate) fn is_expired(&self) -> bool {
        self.remaining_elapsed_ms() == 0
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
        if state.active_children >= MAX_DELEGATED_ACTIVE_CHILDREN {
            return Err(DelegationLimit::ActiveChildren);
        }
        if state.started_children >= MAX_DELEGATED_TOTAL_CHILDREN {
            return Err(DelegationLimit::TotalChildren);
        }
        if state
            .spent_tokens
            .saturating_add(state.reserved_tokens)
            .saturating_add(requested_tokens)
            > self.max_tokens
        {
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
        json!({
            "remaining_elapsed_ms": self.remaining_elapsed_ms(),
            "max_depth": MAX_DELEGATION_DEPTH,
            "max_active_children": MAX_DELEGATED_ACTIVE_CHILDREN,
            "max_total_children": MAX_DELEGATED_TOTAL_CHILDREN,
            "active_children": state.active_children,
            "started_children": state.started_children,
            "max_tokens": self.max_tokens,
            "reserved_tokens": state.reserved_tokens,
            "spent_tokens": state.spent_tokens,
            "token_budget_semantics": DELEGATION_TOKEN_BUDGET_SEMANTICS,
            "spent_tokens_are_usage_accounting": true,
            "max_cost_microusd": self.max_cost_microusd,
            "reserved_cost_microusd": state.reserved_cost_microusd,
            "spent_cost_microusd": state.spent_cost_microusd,
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
    pub(crate) fn finish(&self, actual_tokens: u64, actual_cost_microusd: Option<u64>) {
        // A hard cost budget cannot treat missing provider pricing as free. The admission
        // reservation is the conservative accounting fallback when the response has no cost.
        self.release(
            actual_tokens,
            actual_cost_microusd.unwrap_or(self.requested_cost_microusd),
        );
    }

    fn release(&self, actual_tokens: u64, actual_cost_microusd: u64) {
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
        self.release(self.requested_tokens, self.requested_cost_microusd);
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
    lease: Option<Arc<DelegationLease>>,
}

impl DelegationContext {
    pub(crate) fn root(
        session_id: SessionId,
        max_elapsed_ms: Option<u64>,
        provider_max_tokens: Option<u64>,
        max_cost_microusd: Option<u64>,
        cancellation: CancellationToken,
    ) -> Self {
        let budget = DelegationBudget::root(
            max_elapsed_ms,
            provider_max_tokens,
            max_cost_microusd,
            cancellation,
        );
        Self {
            root_session_id: session_id,
            parent_session_id: None,
            parent_task_id: None,
            parent_thread_id: None,
            depth: 0,
            budget,
            lease: None,
        }
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
        let lease = self
            .budget
            .reserve(requested_tokens, requested_cost_microusd, cancellation)?;
        Ok(Self {
            root_session_id: self.root_session_id,
            parent_session_id: Some(parent_session_id),
            parent_task_id: Some(parent_task_id),
            parent_thread_id: Some(parent_thread_id),
            depth: self.depth.saturating_add(1),
            budget: self.budget.clone(),
            lease: Some(lease),
        })
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.budget.cancellation()
    }

    pub(crate) fn finish(&self, actual_tokens: u64, actual_cost_microusd: Option<u64>) {
        if let Some(lease) = &self.lease {
            lease.finish(actual_tokens, actual_cost_microusd);
        }
    }

    pub(crate) fn metadata(&self) -> Value {
        json!({
            "root_session_id": self.root_session_id,
            "parent_session_id": self.parent_session_id,
            "parent_task_id": self.parent_task_id,
            "parent_thread_id": self.parent_thread_id,
            "depth": self.depth,
            "budget": self.budget.snapshot(),
        })
    }
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
    fn budget_rejects_depth_concurrency_and_total_limits() {
        let cancellation = CancellationToken::new();
        let root = DelegationContext::root(
            SessionId::new(),
            Some(10_000),
            Some(1_024),
            None,
            cancellation.clone(),
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
        let grandchild = child
            .child(
                parent_session,
                parent_task,
                parent_thread,
                1_024,
                Some(0),
                &cancellation,
            )
            .expect("grandchild");
        assert!(matches!(
            grandchild.child(
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
            Some(1_024),
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
        child.finish(900, Some(3));
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
    fn token_budget_exposes_output_reservation_semantics_without_clamping_usage() {
        let cancellation = CancellationToken::new();
        let root = DelegationContext::root(
            SessionId::new(),
            Some(10_000),
            Some(1_024),
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
        child.finish(10_000, None);

        let snapshot = root.budget.snapshot();
        assert_eq!(
            snapshot["token_budget_semantics"],
            DELEGATION_TOKEN_BUDGET_SEMANTICS
        );
        assert_eq!(snapshot["spent_tokens"], 10_000);
        assert_eq!(snapshot["spent_tokens_are_usage_accounting"], true);
    }

    #[test]
    fn hard_cost_budget_conservatively_reserves_unknown_child_cost() {
        let cancellation = CancellationToken::new();
        let root = DelegationContext::root(
            SessionId::new(),
            Some(10_000),
            Some(1_024),
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
            Some(1_024),
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
        child.finish(1_024, None);

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
            Some(1_024),
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
}
