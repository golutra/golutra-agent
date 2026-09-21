//! 终端视图切换仍完整读取持久历史；后台任务隔离取消、会话切换及读取期间的新事件。

use super::*;

type ReloadResult = Result<(LoadedEventHistory, Option<Result<DebugProjection, String>>), String>;

#[derive(Debug)]
pub(crate) struct PendingHistoryReload {
    pub(crate) binding: RuntimeRefreshBinding,
    expanded: bool,
    pub(crate) live_events: Vec<RuntimeEvent>,
    task: JoinHandle<ReloadResult>,
}

impl Drop for PendingHistoryReload {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TuiApp {
    pub(crate) fn start_history_reload(&mut self, transport: &RuntimeTransport) {
        // 替换句柄即取消旧请求；只允许当前会话、视图和展开状态的结果提交。
        self.history_reload = None;
        self.begin_history_replay();
        let binding = self.runtime_refresh_binding();
        let transport = transport.clone();
        self.history_reload = Some(PendingHistoryReload {
            binding,
            expanded: self.developer_observations_expanded,
            live_events: Vec::new(),
            task: tokio::spawn(async move {
                let projection = async {
                    if binding.debug_mode {
                        Some(
                            load_debug_projection(&transport, binding.session_id, binding.task_id)
                                .await,
                        )
                    } else {
                        None
                    }
                };
                let (history, projection) = tokio::join!(
                    load_complete_event_history(&transport, binding.session_id, binding.task_id),
                    projection,
                );
                Ok((history?, projection))
            }),
        });
        self.status_message = "loading complete history… (Esc cancel)".to_owned();
    }

    pub(crate) fn cancel_history_reload(&mut self) {
        if self.history_reload.take().is_some() {
            self.transcript.history.replay_ready = true;
            self.transcript.history.reflow_pending = false;
            self.status_message =
                "history reload cancelled; retained history is still available".to_owned();
        }
    }

    pub(crate) async fn poll_history_reload(&mut self, wait: bool) -> bool {
        let Some(pending) = self.history_reload.as_ref() else {
            return false;
        };
        if pending.binding != self.runtime_refresh_binding()
            || pending.expanded != self.developer_observations_expanded
        {
            self.cancel_history_reload();
            return true;
        }
        if !wait && !pending.task.is_finished() {
            return false;
        }
        let mut pending = self.history_reload.take().expect("pending history");
        let result = (&mut pending.task)
            .await
            .unwrap_or_else(|error| Err(format!("history reload interrupted: {error}")));
        match result {
            Ok((mut history, projection)) => {
                // 第一页是加载开始时的尾部；其后的流事件必须合并，不能让 cursor 倒退。
                history.events.append(&mut pending.live_events);
                history.events.sort_by_key(|event| event.sequence_no);
                history.events.dedup_by_key(|event| event.sequence_no);
                history.end_cursor = history.events.last().map(|event| event.sequence_no);
                self.apply_loaded_history(history);
                if let Some(projection) = projection {
                    self.apply_developer_projection_result(projection);
                }
                if self.developer_error.is_none() {
                    self.status_message = "complete history reloaded".to_owned();
                }
            }
            Err(error) => {
                self.transcript.history.replay_ready = true;
                self.transcript.history.reflow_pending = false;
                self.status_message =
                    format!("history reload failed; retained history kept: {error}");
                if self.debug_mode {
                    self.developer_error = Some(error);
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> TuiApp {
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            true,
            "mock".into(),
            None,
        );
        app.enable_inline_history();
        app
    }

    fn event(app: &TuiApp, sequence: u64) -> RuntimeEvent {
        RuntimeEvent {
            schema_version: golutra_agent_core::RUNTIME_EVENT_SCHEMA_VERSION,
            id: EventId::new(),
            sequence_no: sequence,
            session_id: app.session_id,
            task_id: None,
            turn_id: None,
            parent_event_id: None,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            event_type: RuntimeEventType::CommandAccepted,
            timestamp: chrono::Utc::now(),
            source: golutra_agent_protocol::RuntimeEventSource::Runtime,
            payload: json!({"summary": format!("event {sequence}")}),
            payload_ref: None,
            durable: true,
        }
    }

    fn install_pending(app: &mut TuiApp, task: JoinHandle<ReloadResult>) {
        app.begin_history_replay();
        app.history_reload = Some(PendingHistoryReload {
            binding: app.runtime_refresh_binding(),
            expanded: app.developer_observations_expanded,
            live_events: Vec::new(),
            task,
        });
    }

    #[tokio::test]
    async fn debug_reload_keeps_input_responsive_and_merges_events_received_during_load() {
        let mut app = app();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        install_pending(
            &mut app,
            tokio::spawn(async move { receiver.await.unwrap() }),
        );
        assert!(!app.poll_history_reload(false).await);
        let transport = RuntimeTransport::in_memory().await.unwrap();
        handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &mut app,
            &transport,
        )
        .await
        .unwrap();
        assert_eq!(app.input.text(), "x");
        let first = event(&app, 1);
        let live = event(&app, 2);
        app.apply_runtime_event(live.clone());
        sender
            .send(Ok((
                LoadedEventHistory {
                    events: vec![first, live],
                    end_cursor: Some(2),
                    ..Default::default()
                },
                None,
            )))
            .unwrap();
        assert!(app.poll_history_reload(true).await);
        assert_eq!(
            app.events
                .iter()
                .map(|event| event.sequence_no)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(app.cursor, Some(2));
        assert_eq!(app.input.text(), "x");
        assert!(app.transcript.history.replay_ready);
    }

    #[tokio::test]
    async fn debug_reload_cancels_obsolete_requests_and_session_results() {
        let mut app = app();
        let transport = RuntimeTransport::in_memory().await.unwrap();
        let old = tokio::spawn(std::future::pending::<ReloadResult>());
        let abort = old.abort_handle();
        install_pending(&mut app, old);
        app.start_history_reload(&transport);
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
        let old = tokio::spawn(std::future::pending::<ReloadResult>());
        let abort = old.abort_handle();
        install_pending(&mut app, old);
        app.session_id = SessionId::new();
        assert!(app.poll_history_reload(false).await);
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
        assert!(app.history_reload.is_none());
        assert!(app.events.is_empty());
    }

    #[tokio::test]
    async fn debug_reload_failure_and_escape_keep_retained_history() {
        let mut app = app();
        let first = event(&app, 1);
        app.replace_event_history(vec![first.clone()], false);
        app.transcript.history.reflow_pending = true;
        install_pending(
            &mut app,
            tokio::spawn(async { Err("storage unavailable".to_owned()) }),
        );
        app.poll_history_reload(true).await;
        assert!(!app.transcript.history.reflow_pending);
        assert_eq!(app.events, vec![first]);
        assert!(app.status_message.contains("storage unavailable"));
        install_pending(
            &mut app,
            tokio::spawn(std::future::pending::<ReloadResult>()),
        );
        app.transcript.history.reflow_pending = true;
        let transport = RuntimeTransport::in_memory().await.unwrap();
        handle_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app,
            &transport,
        )
        .await
        .unwrap();
        assert!(app.history_reload.is_none());
        assert!(!app.transcript.history.reflow_pending);
        assert_eq!(app.events.len(), 1);
        assert!(app.transcript.history.replay_ready);
    }
}
