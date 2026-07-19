//! Shared Runtime attachment and event synchronization for interactive and
//! offscreen TUI frontends.

use golutra_client::{RuntimeClient, RuntimeEventStream, RuntimeTransport};
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

    pub(crate) async fn replay_from_cursor(&mut self, app: &TuiApp) -> miette::Result<()> {
        let subscription = subscribe(&self.transport, app).await?;
        self.subscription = subscription;
        self.subscribed_session = app.session_id;
        self.subscribed_task = app.task_id;
        Ok(())
    }

    pub(crate) async fn sync(&mut self, app: &mut TuiApp) -> miette::Result<()> {
        if app.history_load_requested {
            app.load_older_history(&self.transport).await?;
        }
        if app.developer_load_requested {
            app.load_older_debug_history(&self.transport).await?;
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
        }

        let mut reconnect_subscription = false;
        loop {
            match self.subscription.try_recv() {
                Ok(Ok(event)) => {
                    self.pending_events.push(event);
                }
                Ok(Err(error)) => {
                    app.status_message = format!("event stream reconnecting: {error}");
                    reconnect_subscription = true;
                    break;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    app.status_message = "event stream disconnected".to_owned();
                    reconnect_subscription = true;
                    break;
                }
            }
        }
        if !self.pending_events.is_empty() {
            self.refresh_pending = true;
            for event in self.pending_events.drain(..) {
                app.apply_runtime_event(event);
            }
        }
        if reconnect_subscription {
            self.replay_from_cursor(app).await?;
        }
        if self.refresh_pending {
            app.refresh(&self.transport).await?;
            self.refresh_pending = false;
        }
        app.poll_auth_operation(&self.transport).await;
        app.poll_export_operation().await;
        Ok(())
    }
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
