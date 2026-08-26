//! Lifecycle ownership for server-issued runtime attachments.
//!
//! A runtime may be shared by many clients, but each attachment is a temporary
//! controller capability. This module owns its bounded retention, last-seen
//! timestamp, and explicit detach semantics so route adapters do not duplicate
//! lifecycle rules.

use std::{
    collections::HashMap,
    ops::Deref,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use golutra_client::EmbeddedTransport;
use golutra_core::Actor;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub(crate) const DEFAULT_MAX_ATTACHMENTS: usize = 1024;
pub(crate) const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(30 * 60);
const DETACH_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub(crate) struct AttachedAttachment {
    pub(crate) transport: EmbeddedTransport,
    pub(crate) actor: Actor,
    lease: AttachmentLease,
}

impl Deref for AttachedAttachment {
    type Target = EmbeddedTransport;

    fn deref(&self) -> &Self::Target {
        &self.transport
    }
}

impl AttachedAttachment {
    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.lease.cancellation()
    }
}

#[derive(Debug)]
struct AttachmentEntry {
    transport: EmbeddedTransport,
    actor: Actor,
    runtime_key: std::path::PathBuf,
    lease_state: Arc<AttachmentLeaseState>,
}

#[derive(Debug)]
struct RevokedAttachment {
    runtime_key: std::path::PathBuf,
    lease_state: Arc<AttachmentLeaseState>,
}

#[derive(Debug)]
struct AttachmentLeaseState {
    lifecycle: StdMutex<AttachmentLeaseLifecycle>,
    revoked: AtomicBool,
    cancellation: CancellationToken,
    idle: Notify,
}

#[derive(Debug)]
struct AttachmentLeaseLifecycle {
    active: usize,
    idle_since: Instant,
}

impl AttachmentLeaseState {
    fn new(now: Instant) -> Arc<Self> {
        Arc::new(Self {
            lifecycle: StdMutex::new(AttachmentLeaseLifecycle {
                active: 0,
                idle_since: now,
            }),
            revoked: AtomicBool::new(false),
            cancellation: CancellationToken::new(),
            idle: Notify::new(),
        })
    }

    fn acquire(state: &Arc<Self>) -> Option<AttachmentLease> {
        if state.revoked.load(Ordering::Acquire) {
            return None;
        }
        state.with_lifecycle(|lifecycle| {
            lifecycle.active = lifecycle.active.saturating_add(1);
        });
        if state.revoked.load(Ordering::Acquire) {
            state.release();
            return None;
        }
        Some(AttachmentLease {
            state: state.clone(),
        })
    }

    fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
        self.cancellation.cancel();
        self.idle.notify_waiters();
    }

    async fn wait_idle_for(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, self.wait_idle_unbounded())
            .await
            .is_ok()
    }

    async fn wait_idle_unbounded(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            // 先登记等待再检查计数，避免最后一个 lease 在窗口内释放后错过通知。
            notified.as_mut().enable();
            if self.active() == 0 {
                return;
            }
            notified.await;
        }
    }

    fn release(&self) {
        let became_idle = self.with_lifecycle(|lifecycle| {
            debug_assert!(lifecycle.active > 0, "attachment lease count underflow");
            lifecycle.active = lifecycle.active.saturating_sub(1);
            if lifecycle.active == 0 {
                lifecycle.idle_since = Instant::now();
                true
            } else {
                false
            }
        });
        if became_idle {
            self.idle.notify_waiters();
        }
    }

    fn active(&self) -> usize {
        self.with_lifecycle(|lifecycle| lifecycle.active)
    }

    fn idle_expired_at(&self, now: Instant, ttl: Duration) -> bool {
        self.with_lifecycle(|lifecycle| {
            lifecycle.active == 0 && now.saturating_duration_since(lifecycle.idle_since) > ttl
        })
    }

    fn with_lifecycle<T>(&self, f: impl FnOnce(&mut AttachmentLeaseLifecycle) -> T) -> T {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut lifecycle)
    }
}

#[derive(Debug)]
pub(crate) struct AttachmentLease {
    state: Arc<AttachmentLeaseState>,
}

impl AttachmentLease {
    fn cancellation(&self) -> CancellationToken {
        self.state.cancellation.clone()
    }
}

#[derive(Debug)]
pub(crate) struct AttachmentRevocation {
    state: Arc<AttachmentLeaseState>,
}

impl AttachmentRevocation {
    pub(crate) async fn wait_idle(self) {
        let _ = self.state.wait_idle_for(DETACH_DRAIN_TIMEOUT).await;
    }
}

impl Drop for AttachmentLease {
    fn drop(&mut self) {
        self.state.release();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachmentInsertError {
    Capacity,
    Duplicate,
}

#[derive(Debug)]
pub(crate) struct AttachmentRegistry {
    entries: HashMap<String, AttachmentEntry>,
    revoked: Vec<RevokedAttachment>,
    max_entries: usize,
    idle_ttl: Duration,
}

impl AttachmentRegistry {
    pub(crate) fn new(max_entries: usize, idle_ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            revoked: Vec::new(),
            max_entries: max_entries.max(1),
            idle_ttl,
        }
    }

    pub(crate) fn insert(
        &mut self,
        attachment_id: String,
        transport: EmbeddedTransport,
        actor: Actor,
        runtime_key: std::path::PathBuf,
        now: Instant,
    ) -> Result<(), AttachmentInsertError> {
        self.prune_expired_at(now);
        if self.entries.contains_key(&attachment_id) {
            return Err(AttachmentInsertError::Duplicate);
        }
        // A detached entry can still be held by an in-flight request. It no
        // longer has a lookup key, but its transport and runtime lease remain
        // live until that request releases it, so it must count against the
        // same bounded registry capacity.
        let active_revoked = self
            .revoked
            .iter()
            .filter(|revoked| revoked.lease_state.active() > 0)
            .count();
        if self.entries.len().saturating_add(active_revoked) >= self.max_entries {
            return Err(AttachmentInsertError::Capacity);
        }
        self.entries.insert(
            attachment_id,
            AttachmentEntry {
                transport,
                actor,
                runtime_key,
                lease_state: AttachmentLeaseState::new(now),
            },
        );
        Ok(())
    }

    pub(crate) fn attachment(
        &mut self,
        attachment_id: &str,
        now: Instant,
    ) -> Option<AttachedAttachment> {
        self.prune_expired_at(now);
        let entry = self.entries.get_mut(attachment_id)?;
        let lease = AttachmentLeaseState::acquire(&entry.lease_state)?;
        Some(AttachedAttachment {
            transport: entry.transport.clone(),
            actor: entry.actor.clone(),
            lease,
        })
    }

    pub(crate) fn detach_attachment(
        &mut self,
        attachment_id: &str,
    ) -> Option<AttachmentRevocation> {
        let entry = self.entries.remove(attachment_id)?;
        entry.lease_state.revoke();
        self.revoked.push(RevokedAttachment {
            runtime_key: entry.runtime_key,
            lease_state: entry.lease_state.clone(),
        });
        Some(AttachmentRevocation {
            state: entry.lease_state,
        })
    }

    pub(crate) fn prune_expired_at(&mut self, now: Instant) -> Vec<std::path::PathBuf> {
        let ttl = self.idle_ttl;
        let mut runtime_keys = Vec::new();
        self.entries.retain(|_, attachment| {
            let keep = !attachment.lease_state.idle_expired_at(now, ttl);
            if !keep {
                attachment.lease_state.revoke();
                runtime_keys.push(attachment.runtime_key.clone());
            }
            keep
        });
        self.revoked.retain(|revoked| {
            let keep = revoked.lease_state.active() > 0;
            if keep {
                runtime_keys.push(revoked.runtime_key.clone());
            }
            keep
        });
        runtime_keys
    }

    pub(crate) fn runtime_keys(&self) -> std::collections::HashSet<std::path::PathBuf> {
        let mut keys = self
            .entries
            .values()
            .map(|attachment| attachment.runtime_key.clone())
            .collect::<std::collections::HashSet<_>>();
        keys.extend(
            self.revoked
                .iter()
                .filter(|revoked| revoked.lease_state.active() > 0)
                .map(|revoked| revoked.runtime_key.clone()),
        );
        keys
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use golutra_client::EmbeddedTransport;
    use golutra_core::{Actor, ActorKind};
    use uuid::Uuid;

    use super::*;

    fn actor(id: &str) -> Actor {
        Actor {
            kind: ActorKind::Api,
            id: id.to_owned(),
        }
    }

    #[tokio::test]
    async fn registry_expires_idle_capabilities_and_keeps_active_ones() {
        let mut registry = AttachmentRegistry::new(4, Duration::from_secs(10));
        let now = Instant::now();
        let active = Uuid::now_v7().to_string();
        let idle = Uuid::now_v7().to_string();
        registry
            .insert(
                active.clone(),
                EmbeddedTransport::in_memory()
                    .await
                    .expect("active transport"),
                actor("active"),
                std::path::PathBuf::from("/workspace"),
                now,
            )
            .expect("active insert");
        registry
            .insert(
                idle.clone(),
                EmbeddedTransport::in_memory()
                    .await
                    .expect("idle transport"),
                actor("idle"),
                std::path::PathBuf::from("/workspace"),
                now.checked_sub(Duration::from_secs(11)).expect("past"),
            )
            .expect("idle insert");

        assert!(
            registry
                .attachment(&active, now + Duration::from_secs(5))
                .is_some()
        );
        assert!(
            registry
                .attachment(&idle, now + Duration::from_secs(5))
                .is_none()
        );
        assert_eq!(registry.len(), 1);
    }

    #[tokio::test]
    async fn registry_rejects_new_entries_at_capacity() {
        let mut registry = AttachmentRegistry::new(1, Duration::from_secs(60));
        let now = Instant::now();
        registry
            .insert(
                "one".to_owned(),
                EmbeddedTransport::in_memory().await.expect("transport"),
                actor("one"),
                std::path::PathBuf::from("/workspace"),
                now,
            )
            .expect("first insert");
        let result = registry.insert(
            "two".to_owned(),
            EmbeddedTransport::in_memory().await.expect("transport"),
            actor("two"),
            std::path::PathBuf::from("/workspace"),
            now,
        );
        assert_eq!(result, Err(AttachmentInsertError::Capacity));
        assert!(registry.detach_attachment("one").is_some());
        assert!(registry.detach_attachment("one").is_none());
    }

    #[tokio::test]
    async fn active_revoked_leases_cannot_be_used_to_bypass_capacity() {
        let mut registry = AttachmentRegistry::new(1, Duration::from_secs(60));
        let now = Instant::now();
        registry
            .insert(
                "one".to_owned(),
                EmbeddedTransport::in_memory().await.expect("transport"),
                actor("one"),
                std::path::PathBuf::from("/workspace"),
                now,
            )
            .expect("first insert");
        let lease = registry.attachment("one", now).expect("active lease");
        let revocation = registry.detach_attachment("one").expect("detach");

        let result = registry.insert(
            "two".to_owned(),
            EmbeddedTransport::in_memory()
                .await
                .expect("replacement transport"),
            actor("two"),
            std::path::PathBuf::from("/workspace"),
            now,
        );
        assert_eq!(result, Err(AttachmentInsertError::Capacity));

        drop(lease);
        revocation.wait_idle().await;
        registry.prune_expired_at(Instant::now());
        registry
            .insert(
                "two".to_owned(),
                EmbeddedTransport::in_memory()
                    .await
                    .expect("replacement transport"),
                actor("two"),
                std::path::PathBuf::from("/workspace"),
                Instant::now(),
            )
            .expect("capacity is released with the final lease");
    }

    #[tokio::test]
    async fn registry_rejects_duplicate_ids_without_replacing_the_live_capability() {
        let mut registry = AttachmentRegistry::new(2, Duration::from_secs(60));
        let now = Instant::now();
        registry
            .insert(
                "one".to_owned(),
                EmbeddedTransport::in_memory().await.expect("transport"),
                actor("original"),
                std::path::PathBuf::from("/workspace"),
                now,
            )
            .expect("first insert");

        let result = registry.insert(
            "one".to_owned(),
            EmbeddedTransport::in_memory().await.expect("transport"),
            actor("replacement"),
            std::path::PathBuf::from("/other-workspace"),
            now,
        );

        assert_eq!(result, Err(AttachmentInsertError::Duplicate));
        assert_eq!(registry.len(), 1);
        let attachment = registry.attachment("one", now).expect("original lookup");
        assert_eq!(attachment.actor.id, "original");
        assert_eq!(
            registry.runtime_keys(),
            std::collections::HashSet::from([std::path::PathBuf::from("/workspace")])
        );
        assert!(registry.detach_attachment("one").is_some());
        assert!(attachment.cancellation().is_cancelled());
    }

    #[tokio::test]
    async fn detaching_waits_for_an_in_flight_lookup() {
        let mut registry = AttachmentRegistry::new(2, Duration::from_secs(60));
        let now = Instant::now();
        registry
            .insert(
                "one".to_owned(),
                EmbeddedTransport::in_memory().await.expect("transport"),
                actor("one"),
                std::path::PathBuf::from("/workspace"),
                now,
            )
            .expect("insert");
        let attachment = registry.attachment("one", now).expect("lookup");
        let revocation = registry.detach_attachment("one").expect("detach");
        let wait = tokio::spawn(async move {
            revocation.wait_idle().await;
        });
        assert!(!wait.is_finished());
        drop(attachment);
        wait.await.expect("wait for lease");
    }

    #[tokio::test]
    async fn detaching_has_a_bounded_wait_for_a_stuck_lease() {
        let mut registry = AttachmentRegistry::new(1, Duration::from_secs(60));
        let now = Instant::now();
        registry
            .insert(
                "one".to_owned(),
                EmbeddedTransport::in_memory().await.expect("transport"),
                actor("one"),
                std::path::PathBuf::from("/workspace"),
                now,
            )
            .expect("insert");
        let attachment = registry.attachment("one", now).expect("lookup");
        let revocation = registry.detach_attachment("one").expect("detach");

        assert!(
            !revocation
                .state
                .wait_idle_for(Duration::from_millis(20))
                .await
        );
        drop(attachment);
    }

    #[tokio::test]
    async fn revoked_active_leases_keep_their_runtime_referenced_after_detach_timeout() {
        let mut registry = AttachmentRegistry::new(1, Duration::from_secs(60));
        let now = Instant::now();
        let runtime_key = std::path::PathBuf::from("/workspace");
        registry
            .insert(
                "one".to_owned(),
                EmbeddedTransport::in_memory().await.expect("transport"),
                actor("one"),
                runtime_key.clone(),
                now,
            )
            .expect("insert");
        let attachment = registry.attachment("one", now).expect("lookup");
        let revocation = registry.detach_attachment("one").expect("detach");

        assert!(registry.runtime_keys().contains(&runtime_key));
        assert!(
            !revocation
                .state
                .wait_idle_for(Duration::from_millis(10))
                .await
        );
        assert!(registry.runtime_keys().contains(&runtime_key));

        drop(attachment);
        assert!(revocation.state.wait_idle_for(Duration::from_secs(1)).await);
        registry.prune_expired_at(Instant::now());
        assert!(!registry.runtime_keys().contains(&runtime_key));
    }

    #[tokio::test]
    async fn final_lease_release_starts_a_fresh_idle_window() {
        let ttl = Duration::from_secs(10);
        let mut registry = AttachmentRegistry::new(1, ttl);
        let now = Instant::now();
        let inserted_at = now.checked_sub(ttl * 2).expect("past");
        registry
            .insert(
                "one".to_owned(),
                EmbeddedTransport::in_memory().await.expect("transport"),
                actor("one"),
                std::path::PathBuf::from("/workspace"),
                inserted_at,
            )
            .expect("insert");
        let attachment = registry
            .attachment("one", inserted_at)
            .expect("active lookup");

        assert_eq!(registry.prune_expired_at(now).len(), 0);
        drop(attachment);
        assert_eq!(registry.prune_expired_at(now).len(), 0);
        assert_eq!(registry.len(), 1);
    }
}
