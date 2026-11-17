//! In-process coordination across agent sessions. A lease covers checkpoint,
//! mutation and final comparison. Delegation itself never holds a lease.
use golutra_agent_core::SessionId;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex, Weak},
};
use tokio::sync::Notify;

#[derive(Debug, Default)]
struct Coordinator {
    claims: Mutex<BTreeMap<u64, Weak<Claim>>>,
    changed: Notify,
    next: std::sync::atomic::AtomicU64,
}

#[derive(Debug)]
struct Claim {
    id: u64,
    owner: SessionId,
    paths: Vec<PathBuf>,
    coordinator: Arc<Coordinator>,
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.coordinator
            .claims
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
        self.coordinator.changed.notify_waiters();
    }
}

/// Clones keep the same checkpoint batch alive. Same-session ordering remains
/// the AgentLoop's responsibility; the coordinator prevents other sessions from
/// observing a stale before-image or interleaving conflicting mutations.
#[derive(Debug, Clone)]
pub(crate) struct ResourceLease(Arc<Claim>, u64);

impl ResourceLease {
    pub(crate) fn wait_ms(&self) -> u64 {
        self.1
    }
}

impl PartialEq for ResourceLease {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ResourceLease {}

static COORDINATOR: LazyLock<Arc<Coordinator>> = LazyLock::new(|| Arc::new(Coordinator::default()));

pub(crate) async fn acquire(owner: SessionId, mut paths: Vec<PathBuf>) -> Option<ResourceLease> {
    if paths.is_empty() {
        return None;
    }
    paths.sort();
    paths.dedup();
    let started = std::time::Instant::now();
    loop {
        let changed = COORDINATOR.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if let Some(mut lease) = try_acquire(owner, &paths) {
            lease.1 = crate::elapsed_millis(started);
            return Some(lease);
        }
        changed.await;
    }
}

fn try_acquire(owner: SessionId, paths: &[PathBuf]) -> Option<ResourceLease> {
    let mut claims = COORDINATOR
        .claims
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let existing = claims
        .values()
        .filter_map(Weak::upgrade)
        .collect::<Vec<_>>();
    let blocked = existing.iter().any(|claim| {
        claim.owner != owner
            && claim.paths.iter().any(|left| {
                paths
                    .iter()
                    .any(|right| left.starts_with(right) || right.starts_with(left))
            })
    });
    let lease = (!blocked).then(|| {
        let id = COORDINATOR
            .next
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let claim = Arc::new(Claim {
            id,
            owner,
            paths: paths.to_vec(),
            coordinator: COORDINATOR.clone(),
        });
        claims.insert(id, Arc::downgrade(&claim));
        ResourceLease(claim, 0)
    });
    // Claim::drop also acquires this mutex.
    drop(claims);
    drop(existing);
    lease
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn resource_claims_serialize_conflicts_but_allow_independent_worktrees_and_paths() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let first = SessionId::new();
        let second = SessionId::new();
        let lease = acquire(first, vec![root.join("one")]).await.unwrap();
        let independent = acquire(second, vec![root.join("two")]).await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                acquire(second, vec![root.join("one/nested")])
            )
            .await
            .is_err()
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                acquire(second, vec![root.into()])
            )
            .await
            .is_err()
        );
        drop(lease);
        let released = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            acquire(second, vec![root.join("one")]),
        )
        .await
        .unwrap();
        drop(released);
        drop(independent);
    }
}
