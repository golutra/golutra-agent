//! Shared Runtime attachment and event synchronization for interactive and
//! offscreen TUI frontends.

use std::time::{Duration, Instant};

use golutra_client::{ClientError, RuntimeClient, RuntimeEventStream, RuntimeTransport};
use golutra_core::{SessionId, TaskId};
use golutra_protocol::{EventFilter, RuntimeEvent};
use tokio::task::JoinHandle;

use super::{RuntimeRefreshBinding, RuntimeRefreshSnapshot, TuiApp, load_runtime_refresh_snapshot};

struct InteractiveRuntimeRefresh {
    binding: RuntimeRefreshBinding,
    generation: u64,
    task: JoinHandle<Result<RuntimeRefreshSnapshot, String>>,
}

const INTERACTIVE_REFRESH_RETRY_DELAY: Duration = Duration::from_millis(500);

pub(crate) struct TuiRuntimeController {
    transport: RuntimeTransport,
    subscription: RuntimeEventStream,
    subscribed_session: SessionId,
    subscribed_task: Option<TaskId>,
    pending_events: Vec<RuntimeEvent>,
    refresh_pending: bool,
    refresh_binding: RuntimeRefreshBinding,
    refresh_generation: u64,
    interactive_refresh: Option<InteractiveRuntimeRefresh>,
    interactive_refresh_retry_at: Option<Instant>,
}

impl TuiRuntimeController {
    pub(crate) async fn attach(
        app: &mut TuiApp,
        transport: RuntimeTransport,
    ) -> miette::Result<Self> {
        app.load_recent_history(&transport).await?;
        let subscription = subscribe(&transport, app).await?;
        app.refresh(&transport).await?;
        let refresh_binding = app.runtime_refresh_binding();
        Ok(Self {
            transport,
            subscription,
            subscribed_session: app.session_id,
            subscribed_task: app.task_id,
            pending_events: Vec::new(),
            refresh_pending: false,
            refresh_binding,
            refresh_generation: 0,
            interactive_refresh: None,
            interactive_refresh_retry_at: None,
        })
    }

    pub(crate) fn transport(&self) -> &RuntimeTransport {
        &self.transport
    }

    pub(crate) async fn recv(&mut self) -> Option<Result<RuntimeEvent, ClientError>> {
        self.subscription.recv().await
    }

    pub(crate) async fn apply_received(
        &mut self,
        app: &mut TuiApp,
        received: Option<Result<RuntimeEvent, ClientError>>,
    ) -> miette::Result<()> {
        let reconnect_subscription = match received {
            Some(Ok(event)) => {
                if event.session_id != app.session_id {
                    return Ok(());
                }
                self.observe_refresh_event(&event);
                self.pending_events.push(event);
                false
            }
            Some(Err(error)) => {
                app.status_message = format!("event stream reconnecting: {error}");
                true
            }
            None => {
                app.status_message = "event stream disconnected".to_owned();
                true
            }
        };
        // Apply event-local state immediately so streaming stays responsive. Projection
        // reconciliation is coalesced by `sync` instead of blocking on every event.
        self.flush_pending(app, reconnect_subscription, false).await
    }

    pub(crate) async fn replay_from_cursor(&mut self, app: &TuiApp) -> miette::Result<()> {
        self.invalidate_refresh(app.runtime_refresh_binding());
        let subscription = subscribe(&self.transport, app).await?;
        self.subscription = subscription;
        self.subscribed_session = app.session_id;
        self.subscribed_task = app.task_id;
        Ok(())
    }

    pub(crate) async fn sync(&mut self, app: &mut TuiApp) -> miette::Result<bool> {
        self.abort_interactive_refresh();
        self.interactive_refresh_retry_at = None;
        let mut changed = self.sync_refresh_binding(app);
        if app.history_load_requested {
            app.load_older_history(&self.transport).await?;
            changed = true;
        }
        if app.session_id != self.subscribed_session || app.task_id != self.subscribed_task {
            self.pending_events.clear();
            app.load_recent_history(&self.transport).await?;
            let subscription = subscribe(&self.transport, app).await?;
            app.refresh(&self.transport).await?;
            self.subscription = subscription;
            self.subscribed_session = app.session_id;
            self.subscribed_task = app.task_id;
            self.refresh_pending = false;
            changed = true;
        }

        let mut reconnect_subscription = false;
        loop {
            match self.subscription.try_recv() {
                Ok(Ok(event)) => {
                    self.observe_refresh_event(&event);
                    self.pending_events.push(event);
                    changed = true;
                }
                Ok(Err(error)) => {
                    app.status_message = format!("event stream reconnecting: {error}");
                    reconnect_subscription = true;
                    changed = true;
                    break;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    app.status_message = "event stream disconnected".to_owned();
                    reconnect_subscription = true;
                    changed = true;
                    break;
                }
            }
        }
        changed |= self.refresh_pending;
        self.flush_pending(app, reconnect_subscription, true)
            .await?;
        let auth_operation_pending = app.auth_operation.is_some();
        let export_operation_pending = app.export_operation.is_some();
        app.poll_auth_operation(&self.transport).await;
        app.poll_export_operation().await;
        Ok(changed || auth_operation_pending || export_operation_pending)
    }

    /// Synchronize the real terminal UI without awaiting projection/provider/debug I/O.
    /// The offscreen driver uses `sync` because each request needs a fully reconciled snapshot.
    pub(crate) async fn sync_interactive(&mut self, app: &mut TuiApp) -> miette::Result<bool> {
        let mut changed = self.sync_refresh_binding(app);
        if app.history_load_requested {
            app.load_older_history(&self.transport).await?;
            changed = true;
        }
        if app.session_id != self.subscribed_session || app.task_id != self.subscribed_task {
            self.pending_events.clear();
            app.load_recent_history(&self.transport).await?;
            let subscription = subscribe(&self.transport, app).await?;
            self.subscription = subscription;
            self.subscribed_session = app.session_id;
            self.subscribed_task = app.task_id;
            changed = true;
        }

        changed |= self.poll_interactive_refresh(app).await?;

        let mut reconnect_subscription = false;
        loop {
            match self.subscription.try_recv() {
                Ok(Ok(event)) => {
                    self.observe_refresh_event(&event);
                    self.pending_events.push(event);
                    changed = true;
                }
                Ok(Err(error)) => {
                    app.status_message = format!("event stream reconnecting: {error}");
                    reconnect_subscription = true;
                    changed = true;
                    break;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    app.status_message = "event stream disconnected".to_owned();
                    reconnect_subscription = true;
                    changed = true;
                    break;
                }
            }
        }
        self.flush_pending(app, reconnect_subscription, false)
            .await?;
        self.start_interactive_refresh(app);

        let auth_operation_pending = app.auth_operation.is_some();
        let export_operation_pending = app.export_operation.is_some();
        app.poll_auth_operation(&self.transport).await;
        app.poll_export_operation().await;
        Ok(changed || auth_operation_pending || export_operation_pending)
    }

    fn start_interactive_refresh(&mut self, app: &TuiApp) {
        if self.interactive_refresh.is_some()
            || !self.refresh_pending
            || self
                .interactive_refresh_retry_at
                .is_some_and(|retry_at| Instant::now() < retry_at)
        {
            return;
        }
        self.interactive_refresh_retry_at = None;
        let binding = app.runtime_refresh_binding();
        debug_assert_eq!(binding, self.refresh_binding);
        let generation = self.refresh_generation;
        let transport = self.transport.clone();
        self.refresh_pending = false;
        self.interactive_refresh = Some(InteractiveRuntimeRefresh {
            binding,
            generation,
            task: tokio::spawn(
                async move { load_runtime_refresh_snapshot(&transport, binding).await },
            ),
        });
    }

    async fn poll_interactive_refresh(&mut self, app: &mut TuiApp) -> miette::Result<bool> {
        if !self
            .interactive_refresh
            .as_ref()
            .is_some_and(|refresh| refresh.task.is_finished())
        {
            return Ok(false);
        }
        let Some(refresh) = self.interactive_refresh.take() else {
            return Ok(false);
        };
        let binding_is_current = refresh.binding == self.refresh_binding
            && refresh.binding == app.runtime_refresh_binding();
        let generation_is_current = refresh.generation == self.refresh_generation;
        let result = refresh.task.await;
        if !binding_is_current || !generation_is_current {
            return Ok(false);
        }
        match result {
            Ok(Ok(snapshot)) => {
                self.interactive_refresh_retry_at = None;
                Ok(app.apply_runtime_refresh_snapshot(snapshot))
            }
            Ok(Err(error)) => {
                self.refresh_pending = true;
                self.interactive_refresh_retry_at =
                    Some(Instant::now() + INTERACTIVE_REFRESH_RETRY_DELAY);
                app.status_message = format!("runtime refresh failed: {error}");
                Ok(true)
            }
            Err(error) if error.is_cancelled() => Ok(false),
            Err(error) => Err(miette::miette!("runtime refresh task failed: {error}")),
        }
    }

    fn abort_interactive_refresh(&mut self) {
        if let Some(refresh) = self.interactive_refresh.take() {
            refresh.task.abort();
        }
    }

    fn observe_refresh_event(&mut self, event: &RuntimeEvent) {
        if event_requires_full_refresh(event) {
            self.request_full_refresh();
        }
    }

    fn request_full_refresh(&mut self) {
        self.refresh_generation = self.refresh_generation.wrapping_add(1);
        self.refresh_pending = true;
        self.interactive_refresh_retry_at = None;
    }

    fn invalidate_refresh(&mut self, binding: RuntimeRefreshBinding) {
        self.abort_interactive_refresh();
        self.refresh_binding = binding;
        self.request_full_refresh();
    }

    fn sync_refresh_binding(&mut self, app: &TuiApp) -> bool {
        let binding = app.runtime_refresh_binding();
        if binding == self.refresh_binding {
            return false;
        }
        self.invalidate_refresh(binding);
        true
    }

    async fn flush_pending(
        &mut self,
        app: &mut TuiApp,
        reconnect_subscription: bool,
        reconcile_projection: bool,
    ) -> miette::Result<()> {
        for event in self.pending_events.drain(..) {
            if event.session_id == app.session_id {
                app.apply_runtime_event(event);
            }
        }
        if reconnect_subscription {
            self.replay_from_cursor(app).await?;
        }
        if reconcile_projection && self.refresh_pending {
            app.refresh(&self.transport).await?;
            self.refresh_pending = false;
        }
        Ok(())
    }
}

impl Drop for TuiRuntimeController {
    fn drop(&mut self) {
        self.abort_interactive_refresh();
    }
}

pub(crate) fn event_requires_full_refresh(event: &RuntimeEvent) -> bool {
    event.event_type != golutra_protocol::RuntimeEventType::ProviderStreamed
}

async fn subscribe(
    transport: &RuntimeTransport,
    app: &TuiApp,
) -> miette::Result<RuntimeEventStream> {
    transport
        .subscribe(EventFilter {
            session_id: app.session_id,
            task_id: app.task_id,
            after_sequence_no: app.cursor,
        })
        .await
        .map_err(|error| miette::miette!("{error}"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use golutra_core::{EventId, RUNTIME_EVENT_SCHEMA_VERSION, ThreadId};
    use golutra_protocol::{RuntimeEventSource, RuntimeEventType};
    use serde_json::json;

    use super::*;

    fn runtime_event(sequence_no: u64, session_id: SessionId) -> RuntimeEvent {
        RuntimeEvent {
            schema_version: RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: EventId::new(),
            sequence_no,
            session_id,
            turn_id: None,
            task_id: None,
            parent_event_id: None,
            event_type: RuntimeEventType::CommandAccepted,
            timestamp: chrono::Utc::now(),
            source: RuntimeEventSource::Runtime,
            payload: json!({"summary": sequence_no.to_string()}),
            payload_ref: None,
            durable: true,
        }
    }

    #[tokio::test]
    async fn replay_cursor_accepts_one_live_event_once_and_rejects_old_session_events() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready (mock)".to_owned(),
            None,
        );
        let mut controller = TuiRuntimeController::attach(&mut app, transport)
            .await
            .expect("controller");
        let replay_cursor = app.cursor.unwrap_or(0);
        let live = runtime_event(replay_cursor + 1, app.session_id);

        controller
            .apply_received(&mut app, Some(Ok(live.clone())))
            .await
            .expect("live event");
        controller
            .apply_received(&mut app, Some(Ok(live)))
            .await
            .expect("duplicate live event");
        controller
            .apply_received(
                &mut app,
                Some(Ok(runtime_event(replay_cursor + 2, SessionId::new()))),
            )
            .await
            .expect("stale session event");

        assert_eq!(app.cursor, Some(replay_cursor + 1));
        assert_eq!(
            app.events
                .iter()
                .filter(|event| event.sequence_no == replay_cursor + 1)
                .count(),
            1
        );
        assert!(
            app.events
                .iter()
                .all(|event| event.session_id == app.session_id)
        );
    }

    #[tokio::test]
    async fn interactive_sync_does_not_await_an_inflight_refresh() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready (mock)".to_owned(),
            None,
        );
        let mut controller = TuiRuntimeController::attach(&mut app, transport)
            .await
            .expect("controller");
        let binding = app.runtime_refresh_binding();
        controller.interactive_refresh = Some(InteractiveRuntimeRefresh {
            binding,
            generation: controller.refresh_generation,
            task: tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Err("delayed refresh".to_owned())
            }),
        });

        tokio::time::timeout(
            Duration::from_millis(100),
            controller.sync_interactive(&mut app),
        )
        .await
        .expect("interactive sync must remain responsive")
        .expect("interactive sync");
        assert!(controller.interactive_refresh.is_some());
    }

    #[tokio::test]
    async fn refresh_snapshot_is_ignored_after_the_session_binding_changes() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready (mock)".to_owned(),
            None,
        );
        app.refresh(&transport).await.expect("initial refresh");
        let snapshot = load_runtime_refresh_snapshot(&transport, app.runtime_refresh_binding())
            .await
            .expect("snapshot");
        let original_projection = app.projection.clone();

        app.session_id = SessionId::new();

        assert!(!app.apply_runtime_refresh_snapshot(snapshot));
        assert_eq!(app.projection, original_projection);
    }

    #[tokio::test]
    async fn failed_interactive_refresh_is_retried_after_a_delay() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready (mock)".to_owned(),
            None,
        );
        let mut controller = TuiRuntimeController::attach(&mut app, transport)
            .await
            .expect("controller");
        controller.interactive_refresh = Some(InteractiveRuntimeRefresh {
            binding: app.runtime_refresh_binding(),
            generation: controller.refresh_generation,
            task: tokio::spawn(async { Err("temporary failure".to_owned()) }),
        });
        tokio::task::yield_now().await;

        assert!(
            controller
                .poll_interactive_refresh(&mut app)
                .await
                .expect("poll refresh")
        );
        assert!(controller.refresh_pending);
        assert!(controller.interactive_refresh_retry_at.is_some());

        controller.start_interactive_refresh(&app);
        assert!(controller.interactive_refresh.is_none());
        controller.interactive_refresh_retry_at = Some(Instant::now());
        controller.start_interactive_refresh(&app);
        assert!(controller.interactive_refresh.is_some());
    }

    #[tokio::test]
    async fn stale_refresh_result_and_error_are_ignored_after_a_newer_generation() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready (mock)".to_owned(),
            None,
        );
        let mut controller = TuiRuntimeController::attach(&mut app, transport.clone())
            .await
            .expect("controller");
        let binding = app.runtime_refresh_binding();
        let stale_snapshot = load_runtime_refresh_snapshot(&transport, binding)
            .await
            .expect("snapshot");
        let mut newer_projection = app.projection.clone().expect("projection");
        newer_projection.status = golutra_core::TaskStatus::Running;
        app.projection = Some(newer_projection.clone());
        let generation = controller.refresh_generation;
        controller.interactive_refresh = Some(InteractiveRuntimeRefresh {
            binding,
            generation,
            task: tokio::spawn(async move { Ok(stale_snapshot) }),
        });
        controller.request_full_refresh();
        tokio::task::yield_now().await;

        assert!(
            !controller
                .poll_interactive_refresh(&mut app)
                .await
                .expect("poll stale snapshot")
        );
        assert_eq!(app.projection, Some(newer_projection));
        assert!(controller.refresh_pending);

        app.status_message = "newer state".to_owned();
        let generation = controller.refresh_generation;
        controller.interactive_refresh = Some(InteractiveRuntimeRefresh {
            binding,
            generation,
            task: tokio::spawn(async { Err("stale failure".to_owned()) }),
        });
        controller.request_full_refresh();
        tokio::task::yield_now().await;

        assert!(
            !controller
                .poll_interactive_refresh(&mut app)
                .await
                .expect("poll stale error")
        );
        assert_eq!(app.status_message, "newer state");
        assert!(controller.interactive_refresh_retry_at.is_none());
    }

    #[tokio::test]
    async fn debug_binding_change_schedules_a_nonblocking_interactive_refresh() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready (mock)".to_owned(),
            None,
        );
        let mut controller = TuiRuntimeController::attach(&mut app, transport)
            .await
            .expect("controller");

        app.set_debug_mode(true);
        tokio::time::timeout(
            Duration::from_millis(100),
            controller.sync_interactive(&mut app),
        )
        .await
        .expect("debug switch must not await refresh I/O")
        .expect("interactive sync");

        let refresh = controller
            .interactive_refresh
            .as_ref()
            .expect("debug refresh scheduled");
        assert!(refresh.binding.debug_mode);
        assert_eq!(refresh.binding, controller.refresh_binding);
    }
}
