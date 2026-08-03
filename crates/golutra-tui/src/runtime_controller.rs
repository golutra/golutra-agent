//! Shared Runtime attachment and event synchronization for interactive and
//! offscreen TUI frontends.

use golutra_client::{ClientError, RuntimeClient, RuntimeEventStream, RuntimeTransport};
use golutra_core::{SessionId, TaskId};
use golutra_protocol::{EventFilter, RuntimeEvent};

use super::TuiApp;

pub(crate) struct TuiRuntimeController {
    transport: RuntimeTransport,
    subscription: RuntimeEventStream,
    subscribed_session: SessionId,
    subscribed_task: Option<TaskId>,
    pending_events: Vec<RuntimeEvent>,
    refresh_pending: bool,
}

impl TuiRuntimeController {
    pub(crate) async fn attach(
        app: &mut TuiApp,
        transport: RuntimeTransport,
    ) -> miette::Result<Self> {
        app.load_recent_history(&transport).await?;
        let subscription = subscribe(&transport, app).await?;
        app.refresh(&transport).await?;
        Ok(Self {
            transport,
            subscription,
            subscribed_session: app.session_id,
            subscribed_task: app.task_id,
            pending_events: Vec::new(),
            refresh_pending: false,
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
                self.refresh_pending |= event_requires_full_refresh(&event);
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
        let subscription = subscribe(&self.transport, app).await?;
        self.subscription = subscription;
        self.subscribed_session = app.session_id;
        self.subscribed_task = app.task_id;
        Ok(())
    }

    pub(crate) async fn sync(&mut self, app: &mut TuiApp) -> miette::Result<bool> {
        let mut changed = false;
        if app.history_load_requested {
            app.load_older_history(&self.transport).await?;
            changed = true;
        }
        if app.developer_load_requested {
            app.load_older_debug_history(&self.transport).await?;
            changed = true;
        }

        if app.session_id != self.subscribed_session || app.task_id != self.subscribed_task {
            self.pending_events.clear();
            self.refresh_pending = false;
            app.load_recent_history(&self.transport).await?;
            let subscription = subscribe(&self.transport, app).await?;
            app.refresh(&self.transport).await?;
            self.subscription = subscription;
            self.subscribed_session = app.session_id;
            self.subscribed_task = app.task_id;
            changed = true;
        }

        let mut reconnect_subscription = false;
        loop {
            match self.subscription.try_recv() {
                Ok(Ok(event)) => {
                    self.refresh_pending |= event_requires_full_refresh(&event);
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

    async fn flush_pending(
        &mut self,
        app: &mut TuiApp,
        reconnect_subscription: bool,
        reconcile_projection: bool,
    ) -> miette::Result<()> {
        for event in self.pending_events.drain(..) {
            app.apply_runtime_event(event);
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
