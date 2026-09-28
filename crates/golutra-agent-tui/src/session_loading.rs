//! 会话页面的后台加载与交接；只读任务可取消，已创建分支必须经 UI 接收或回收。

use super::*;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
pub(crate) struct PendingSessionLoad {
    session_id: SessionId,
    pub(crate) target_thread_id: Option<ThreadId>,
    source_identity: Option<(ThreadId, SessionId)>,
    cancellation: CancellationToken,
    updates: mpsc::Receiver<SessionLoadUpdate>,
    task: Option<JoinHandle<Result<(), String>>>,
}

#[derive(Debug)]
enum SessionLoadUpdate {
    Page(Vec<ResumeThreadItem>),
    Turns(LoadedEventHistory),
    Ready(Box<PreparedSession>),
}

#[derive(Debug)]
struct PreparedSession {
    thread_id: ThreadId,
    session_id: SessionId,
    history: LoadedEventHistory,
    snapshot: RuntimeRefreshSnapshot,
    prompt: Option<(HistoricalTurnItem, Vec<ComposerAttachment>)>,
    accepted: Option<oneshot::Sender<()>>,
}

impl Drop for PendingSessionLoad {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl TuiApp {
    fn begin_session_load(
        &mut self,
    ) -> (
        CancellationToken,
        mpsc::Sender<SessionLoadUpdate>,
        mpsc::Receiver<SessionLoadUpdate>,
    ) {
        self.cancel_session_load();
        let (sender, receiver) = mpsc::channel(2);
        (CancellationToken::new(), sender, receiver)
    }

    fn install_session_load(
        &mut self,
        cancellation: CancellationToken,
        updates: mpsc::Receiver<SessionLoadUpdate>,
        task: JoinHandle<Result<(), String>>,
        target_thread_id: Option<ThreadId>,
        source_identity: Option<(ThreadId, SessionId)>,
    ) {
        self.session_load = Some(PendingSessionLoad {
            session_id: self.session_id,
            target_thread_id,
            source_identity,
            cancellation,
            updates,
            task: Some(task),
        });
    }

    pub(crate) fn cancel_session_load(&mut self) {
        if let Some(mut pending) = self.session_load.take() {
            pending.cancellation.cancel();
            // Receiver 必须先释放，未被接受的分支交接据此触发 worker 清理。
            pending.updates.close();
            if let Some(task) = pending.task.take() {
                self.retired_session_loads.push(task);
            }
        }
    }

    pub(crate) fn start_resume_catalog(&mut self, transport: &RuntimeTransport) {
        let (cancel, sender, receiver) = self.begin_session_load();
        self.backtrack_base = None;
        self.turn_picker = None;
        self.queue_picker = None;
        self.editing_queued_turn = None;
        let mut picker = ResumePickerState::new(Vec::new());
        picker.set_current_thread_id(self.thread_id);
        self.resume_picker = Some(picker);
        let transport = transport.clone();
        let cancellation = cancel.clone();
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = cancellation.cancelled() => Ok(()),
                result = load_catalog_pages(&transport, &sender) => result,
            }
        });
        self.install_session_load(cancel, receiver, task, None, None);
        self.status_message = "Loading sessions… · Esc cancel".to_owned();
    }

    pub(crate) fn start_turn_history(&mut self, transport: &RuntimeTransport) {
        let (cancel, sender, receiver) = self.begin_session_load();
        // 每次打开历史都重新锁定当前来源，避免复用已经切换会话后的旧基线。
        self.backtrack_base = Some((self.thread_id, self.session_id));
        self.resume_picker = None;
        self.queue_picker = None;
        self.editing_queued_turn = None;
        self.turn_picker = Some(TurnPickerState::new(Vec::new()));
        let session = self.session_id;
        let transport = transport.clone();
        let cancellation = cancel.clone();
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = cancellation.cancelled() => Ok(()),
                result = load_complete_event_history(&transport, session, None) => {
                    let _ = sender.send(SessionLoadUpdate::Turns(result?)).await;
                    Ok(())
                }
            }
        });
        self.install_session_load(cancel, receiver, task, None, None);
        self.status_message = "Loading session history… · Esc cancel".to_owned();
    }

    pub(crate) fn start_session_resume(
        &mut self,
        transport: &RuntimeTransport,
        thread_id: ThreadId,
    ) {
        let (cancel, sender, receiver) = self.begin_session_load();
        if self.resume_picker.is_none() {
            self.resume_picker = Some(ResumePickerState::new(Vec::new()));
        }
        let transport = transport.clone();
        let cancellation = cancel.clone();
        let debug = self.debug_mode;
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = cancellation.cancelled() => Ok(()),
                result = async {
                    let thread = transport.resume_thread(thread_id).await.map_err(|e| e.to_string())?;
                    prepare_session(&transport, thread.thread_id, thread.session_id, debug).await
                } => {
                    let _ = sender.send(SessionLoadUpdate::Ready(Box::new(result?))).await;
                    Ok(())
                }
            }
        });
        self.install_session_load(cancel, receiver, task, Some(thread_id), None);
        self.status_message = "Loading session… · Esc cancel".to_owned();
    }

    pub(crate) fn start_prompt_edit(&mut self, transport: &RuntimeTransport) {
        let Some(item) = self
            .turn_picker
            .as_ref()
            .and_then(|picker| picker.items.get(picker.selected))
            .cloned()
        else {
            return;
        };
        let source = self
            .backtrack_base
            .unwrap_or((self.thread_id, self.session_id));
        if source != (self.thread_id, self.session_id) {
            self.status_message =
                "Session changed; reopen history before editing a previous prompt".to_owned();
            return;
        }
        let (cancel, sender, receiver) = self.begin_session_load();
        let transport = transport.clone();
        let cancellation = cancel.clone();
        let workspace = self.workspace_path.clone();
        let debug = self.debug_mode;
        let task = tokio::spawn(async move {
            let attachments = tokio::select! {
                _ = cancellation.cancelled() => return Ok(()),
                result = inspect_prompt(&transport, source.1, &workspace, &item) => result?,
            };
            // mutation 一旦发出就不 abort；先得到 child 身份，取消或加载失败均回收。
            let thread = transport
                .fork_thread_before_turn(source.0, item.turn_id)
                .await
                .map_err(|e| e.to_string())?;
            let prepared = tokio::select! {
                _ = cancellation.cancelled() => None,
                result = prepare_session(&transport, thread.thread_id, thread.session_id, debug) => Some(result),
            };
            let mut error = None;
            if thread.forked_from_turn_id != Some(item.turn_id) {
                error = Some(
                    "Runtime did not honor the prompt edit boundary; update the runtime server"
                        .to_owned(),
                );
            } else if let Some(result) = prepared {
                match result {
                    Ok(mut ready) => {
                        let (accepted, receipt) = oneshot::channel();
                        ready.prompt = Some((item, attachments));
                        ready.accepted = Some(accepted);
                        if sender
                            .send(SessionLoadUpdate::Ready(Box::new(ready)))
                            .await
                            .is_ok()
                            && receipt.await.is_ok()
                        {
                            return Ok(());
                        }
                    }
                    Err(reason) => error = Some(reason),
                }
            }
            cleanup_branch(&transport, source.1, thread.thread_id).await?;
            error.map_or(Ok(()), Err)
        });
        self.install_session_load(cancel, receiver, task, None, Some(source));
        self.status_message = "Restoring previous prompt… · Esc cancel".to_owned();
    }

    pub(crate) async fn poll_session_load(
        &mut self,
        _transport: &RuntimeTransport,
        wait: bool,
    ) -> bool {
        let mut changed = self.reap_session_loads(false).await;
        if self
            .session_load
            .as_ref()
            .is_some_and(|pending| pending.session_id != self.session_id)
        {
            self.cancel_session_load();
            changed = true;
        }
        // 每次维护只合并少量页，持续返回键盘循环；离屏 driver 可显式等待完整结果。
        for _ in 0..if wait { usize::MAX } else { 4 } {
            let Some(pending) = self.session_load.as_mut() else {
                break;
            };
            let target_thread_id = pending.target_thread_id;
            let source_identity = pending.source_identity;
            let update = if wait {
                pending.updates.recv().await
            } else {
                match pending.updates.try_recv() {
                    Ok(update) => Some(update),
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => None,
                }
            };
            changed = true;
            match update {
                Some(SessionLoadUpdate::Page(items)) => {
                    if let Some(picker) = &mut self.resume_picker {
                        picker.append_items(items);
                    }
                    self.status_message = "Select a session · Esc close".to_owned();
                }
                Some(SessionLoadUpdate::Turns(mut history)) => {
                    history.events.extend(
                        self.events
                            .iter()
                            .filter(|event| event.session_id == self.session_id)
                            .cloned(),
                    );
                    history.events.sort_by_key(|event| event.sequence_no);
                    history.events.dedup_by_key(|event| event.sequence_no);
                    let items = historical_turn_items(&history.events);
                    self.status_message = if items.is_empty() {
                        "No previous user turns in this session"
                    } else {
                        "Left/Right select · Up/Down scroll · Enter edit · Esc close"
                    }
                    .to_owned();
                    self.turn_picker = Some(TurnPickerState::new(items));
                }
                Some(SessionLoadUpdate::Ready(ready)) => {
                    let selected_target_matches = target_thread_id.is_none_or(|target| {
                        self.resume_picker
                            .as_ref()
                            .and_then(ResumePickerState::selected_thread_id)
                            .is_none_or(|selected| selected == target)
                    });
                    let source_matches = source_identity
                        .is_none_or(|source| (self.thread_id, self.session_id) == source);
                    if !selected_target_matches {
                        self.cancel_session_load();
                        self.status_message =
                            "Session selection changed; press Enter to load the selected session"
                                .to_owned();
                    } else if !source_matches {
                        self.cancel_session_load();
                        self.status_message =
                            "Session changed; reopen history before editing a previous prompt"
                                .to_owned();
                    } else if ready.prompt.as_ref().is_some_and(|(item, _)| {
                        self.turn_picker
                            .as_ref()
                            .and_then(TurnPickerState::selected_turn_id)
                            != Some(item.turn_id)
                    }) {
                        self.cancel_session_load();
                        self.status_message =
                            "Prompt selection changed; press Enter to edit the selected prompt"
                                .to_owned();
                    } else {
                        self.apply_prepared_session(*ready);
                    }
                    break;
                }
                None => {
                    let mut pending = self.session_load.take().expect("pending session load");
                    if let Err(error) = pending
                        .task
                        .as_mut()
                        .expect("session task")
                        .await
                        .unwrap_or_else(|error| Err(error.to_string()))
                    {
                        self.status_message = if self.turn_picker.is_some() {
                            format!("Could not restore previous prompt: {error}")
                        } else {
                            format!("Could not load session: {error}")
                        };
                    } else if self
                        .resume_picker
                        .as_ref()
                        .is_some_and(|picker| picker.items.is_empty() && picker.search.is_empty())
                    {
                        self.resume_picker = None;
                        self.push_command_result("No sessions in this cwd yet");
                    }
                    break;
                }
            }
        }
        changed
    }

    fn apply_prepared_session(&mut self, mut ready: PreparedSession) {
        // ACK 只在确认 UI 接收时发送；先确认再取消旧句柄，避免已显示分支被后台误删。
        if let Some(accepted) = ready.accepted.take() {
            let _ = accepted.send(());
        }
        self.cancel_session_load();
        if ready.thread_id == self.thread_id && ready.session_id == self.session_id {
            self.resume_picker = None;
            self.status_message =
                format!("Already viewing {}", short_id(&self.thread_id.to_string()));
            return;
        }
        self.reset_session_view(ready.thread_id, ready.session_id);
        self.apply_runtime_refresh_snapshot(ready.snapshot);
        self.apply_loaded_history(ready.history);
        self.prepared_session_history = Some(ready.session_id);
        if let Some((item, attachments)) = ready.prompt {
            self.input.set_text(&item.prompt);
            self.attachments = attachments;
            self.transcript.history.reflow_pending = true;
            self.status_message =
                "Previous prompt restored · edit and press Enter to send".to_owned();
        } else {
            self.record_slash_command("/resume");
            self.push_command_result(format!("Resumed {}", short_id(&self.thread_id.to_string())));
        }
    }

    pub(crate) async fn reap_session_loads(&mut self, wait: bool) -> bool {
        let mut changed = false;
        let mut index = 0;
        while index < self.retired_session_loads.len() {
            if wait || self.retired_session_loads[index].is_finished() {
                let task = self.retired_session_loads.swap_remove(index);
                if let Err(error) = task.await.unwrap_or_else(|error| Err(error.to_string())) {
                    self.status_message = format!("Session cleanup failed: {error}");
                    changed = true;
                }
            } else {
                index += 1;
            }
        }
        changed
    }
}

async fn prepare_session(
    transport: &RuntimeTransport,
    thread_id: ThreadId,
    session_id: SessionId,
    debug_mode: bool,
) -> Result<PreparedSession, String> {
    let (history, snapshot) = tokio::try_join!(
        load_complete_event_history(transport, session_id, None),
        load_runtime_refresh_snapshot(
            transport,
            RuntimeRefreshBinding {
                session_id,
                task_id: None,
                debug_mode
            }
        ),
    )?;
    Ok(PreparedSession {
        thread_id,
        session_id,
        history,
        snapshot,
        prompt: None,
        accepted: None,
    })
}

async fn inspect_prompt(
    transport: &RuntimeTransport,
    session_id: SessionId,
    workspace: &Path,
    item: &HistoricalTurnItem,
) -> Result<Vec<ComposerAttachment>, String> {
    let history = load_complete_event_history(transport, session_id, None).await?;
    if !historical_turn_items(&history.events)
        .iter()
        .any(|current| {
            current.turn_id == item.turn_id
                && current.prompt == item.prompt
                && current.attachment_paths == item.attachment_paths
        })
    {
        return Err("history changed; reopen session history".to_owned());
    }
    item.attachment_paths
        .iter()
        .map(|path| attachment_from_path(workspace, path))
        .collect()
}

pub(super) async fn cleanup_branch(
    transport: &RuntimeTransport,
    source: SessionId,
    thread: ThreadId,
) -> Result<(), String> {
    let ack = transport
        .send_command(session_command(
            source,
            SessionCommandKind::DeleteThread,
            json!({"thread_id": thread, "purge": true, "confirm": "PURGE"}),
        ))
        .await
        .map_err(|e| e.to_string())?;
    if ack.accepted {
        Ok(())
    } else {
        Err(format!(
            "branch {thread} cleanup rejected: {}",
            compact_ack_reason(&ack.reason)
        ))
    }
}

async fn load_catalog_pages(
    transport: &RuntimeTransport,
    sender: &mpsc::Sender<SessionLoadUpdate>,
) -> Result<(), String> {
    let mut cursor = None;
    for page in 0..MAX_RESUME_SESSION_PAGES {
        let result = transport
            .session_page(SessionPageRequest {
                cursor: cursor.clone(),
                limit: RESUME_SESSION_PAGE_SIZE,
            })
            .await;
        let (sessions, next, has_more) = match result {
            Ok(page) => (page.sessions, page.next_cursor, page.has_more),
            Err(error) if page == 0 => {
                let threads = transport.list_threads(50).await.map_err(|fallback| {
                    format!("session pagination failed: {error}; legacy listing failed: {fallback}")
                })?;
                let items = threads
                    .into_iter()
                    .map(|thread| ResumeThreadItem {
                        thread_id: thread.thread_id,
                        session_id: thread.session_id,
                        parent_thread_id: thread.parent_thread_id,
                        forked_from_turn_id: thread.forked_from_turn_id,
                        title: thread.title,
                        preview: thread.preview,
                        metadata: format!("updated={}", thread.updated_at.to_rfc3339()),
                    })
                    .collect();
                let _ = sender.send(SessionLoadUpdate::Page(items)).await;
                return Ok(());
            }
            Err(error) => return Err(error.to_string()),
        };
        let items = sessions
            .into_iter()
            .map(|session| ResumeThreadItem {
                thread_id: session.thread_id,
                session_id: session.session_id,
                parent_thread_id: session.parent_thread_id,
                forked_from_turn_id: session.forked_from_turn_id,
                title: session.title,
                preview: session.preview,
                metadata: format!("updated={}", session.updated_at.to_rfc3339()),
            })
            .collect();
        if sender.send(SessionLoadUpdate::Page(items)).await.is_err() {
            return Ok(());
        }
        if !has_more {
            return Ok(());
        }
        if next.is_none() || next == cursor {
            return Err("session page did not advance its cursor".to_owned());
        }
        cursor = next;
    }
    Err("session history exceeds the page safety limit".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(transport: &RuntimeTransport) -> TuiApp {
        TuiApp::new(
            transport.default_thread_id(),
            transport.default_session_id(),
            None,
            false,
            "mock".into(),
            None,
        )
    }

    fn item(title: &str) -> ResumeThreadItem {
        ResumeThreadItem {
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            parent_thread_id: None,
            forked_from_turn_id: None,
            title: title.into(),
            preview: String::new(),
            metadata: "status=idle".into(),
        }
    }

    #[tokio::test]
    async fn slow_catalog_keeps_search_responsive_and_escape_discards_late_pages() {
        let transport = RuntimeTransport::in_memory().await.unwrap();
        let mut app = app(&transport);
        let (cancel, sender, receiver) = app.begin_session_load();
        let (release, wait) = oneshot::channel();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            tokio::select! { _ = task_cancel.cancelled() => {}, _ = wait => {} }
            Ok(())
        });
        app.resume_picker = Some(ResumePickerState::new(Vec::new()));
        app.install_session_load(cancel, receiver, task, None, None);
        sender
            .send(SessionLoadUpdate::Page(vec![item("first page")]))
            .await
            .unwrap();
        assert!(app.poll_session_load(&transport, false).await);
        assert_eq!(app.resume_picker.as_ref().unwrap().items.len(), 1);
        handle_key(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
            &mut app,
            &transport,
        )
        .await
        .unwrap();
        assert_eq!(app.resume_picker.as_ref().unwrap().search.text(), "f");
        handle_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app,
            &transport,
        )
        .await
        .unwrap();
        assert!(app.resume_picker.is_none());
        assert!(app.session_load.is_none());
        assert!(
            sender
                .send(SessionLoadUpdate::Page(vec![item("late")]))
                .await
                .is_err()
        );
        let _ = release.send(());
        app.reap_session_loads(true).await;
        assert!(app.retired_session_loads.is_empty());
    }

    #[tokio::test]
    async fn history_load_merges_live_events_and_obsolete_session_cannot_replace_the_page() {
        let transport = RuntimeTransport::in_memory().await.unwrap();
        let mut app = app(&transport);
        let make_event = |sequence, session| {
            super::super::tests::transcript_event(
                sequence,
                session,
                TaskId::new(),
                RuntimeEventType::TaskCreated,
                json!({"payload": {"prompt": format!("prompt {sequence}")}}),
            )
        };
        let mut earlier = make_event(1, app.session_id);
        earlier.turn_id = Some(TurnId::new());
        let mut live = make_event(2, app.session_id);
        live.turn_id = Some(TurnId::new());
        let (cancel, sender, receiver) = app.begin_session_load();
        let task = tokio::spawn(async move {
            sender
                .send(SessionLoadUpdate::Turns(LoadedEventHistory {
                    events: vec![earlier],
                    ..Default::default()
                }))
                .await
                .unwrap();
            Ok(())
        });
        app.turn_picker = Some(TurnPickerState::new(Vec::new()));
        app.install_session_load(cancel, receiver, task, None, None);
        app.apply_runtime_event(live);
        app.poll_session_load(&transport, true).await;
        assert_eq!(app.turn_picker.as_ref().unwrap().items.len(), 2);

        let (cancel, sender, receiver) = app.begin_session_load();
        let task_cancel = cancel.clone();
        app.install_session_load(
            cancel,
            receiver,
            tokio::spawn(async move {
                task_cancel.cancelled().await;
                Ok(())
            }),
            None,
            None,
        );
        app.session_id = SessionId::new();
        app.poll_session_load(&transport, false).await;
        assert!(app.session_load.is_none());
        assert!(
            sender
                .send(SessionLoadUpdate::Turns(LoadedEventHistory::default()))
                .await
                .is_err()
        );
        app.reap_session_loads(true).await;
    }

    #[tokio::test]
    async fn cancelled_prompt_edit_reclaims_unaccepted_branch_and_keeps_source() {
        let transport = RuntimeTransport::in_memory().await.unwrap();
        let mut app = app(&transport);
        transport
            .send_command(session_command(
                app.session_id,
                SessionCommandKind::Prompt,
                json!({"prompt": "hello", "defer_external_verification": true}),
            ))
            .await
            .unwrap();
        let history = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let history = load_complete_event_history(&transport, app.session_id, None)
                    .await
                    .unwrap();
                if history
                    .events
                    .iter()
                    .any(|event| event.event_type.is_task_terminal())
                {
                    break history;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        app.turn_picker = Some(TurnPickerState::new(historical_turn_items(&history.events)));
        app.input.set_text("unsent draft");
        app.start_prompt_edit(&transport);
        // 等待实际 child 创建和加载完成，但不让 UI 接收；Esc 应触发回收而非切换。
        tokio::time::timeout(Duration::from_secs(10), async {
            while app.session_load.as_ref().unwrap().updates.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(transport.list_threads(100).await.unwrap().len(), 2);
        app.close_turn_picker();
        app.reap_session_loads(true).await;
        assert_eq!(app.session_id, transport.default_session_id());
        assert_eq!(app.input.text(), "unsent draft");
        assert_eq!(transport.list_threads(100).await.unwrap().len(), 1);
        let after = load_complete_event_history(&transport, app.session_id, None)
            .await
            .unwrap();
        assert!(after.events.starts_with(&history.events));
        transport.close().await.unwrap();
    }

    #[tokio::test]
    async fn missing_resume_target_keeps_current_draft_and_history() {
        let transport = RuntimeTransport::in_memory().await.unwrap();
        let mut app = app(&transport);
        app.input.set_text("keep draft");
        app.start_session_resume(&transport, ThreadId::new());
        app.poll_session_load(&transport, true).await;
        assert_eq!(app.session_id, transport.default_session_id());
        assert_eq!(app.input.text(), "keep draft");
        assert!(app.status_message.contains("Could not load"));
    }

    #[tokio::test]
    async fn stale_backtrack_source_is_rejected_before_forking() {
        let transport = RuntimeTransport::in_memory().await.unwrap();
        let mut app = app(&transport);
        app.turn_picker = Some(TurnPickerState::new(vec![HistoricalTurnItem {
            turn_id: TurnId::new(),
            prompt: "old prompt".to_owned(),
            attachment_paths: Vec::new(),
            preview: Vec::new(),
        }]));
        let before = transport.list_threads(100).await.unwrap().len();
        app.backtrack_base = Some((ThreadId::new(), app.session_id));
        app.start_prompt_edit(&transport);
        assert!(app.session_load.is_none());
        assert!(app.status_message.contains("reopen history"));
        assert_eq!(transport.list_threads(100).await.unwrap().len(), before);
    }

    #[tokio::test]
    async fn late_resume_result_cannot_replace_a_newer_selection() {
        let transport = RuntimeTransport::in_memory().await.unwrap();
        let target_a = ThreadId::new();
        let target_b = ThreadId::new();
        for (thread_id, prompt) in [(target_a, "target a"), (target_b, "target b")] {
            let ack = transport
                .send_command(session_command(
                    SessionId::new(),
                    SessionCommandKind::Create,
                    json!({"_thread_id": thread_id, "prompt": prompt}),
                ))
                .await
                .unwrap();
            assert!(ack.accepted);
        }
        let mut app = app(&transport);
        app.resume_picker = Some(ResumePickerState::new(vec![
            ResumeThreadItem {
                thread_id: target_a,
                session_id: SessionId::new(),
                parent_thread_id: None,
                forked_from_turn_id: None,
                title: "a".to_owned(),
                preview: String::new(),
                metadata: "updated=now".to_owned(),
            },
            ResumeThreadItem {
                thread_id: target_b,
                session_id: SessionId::new(),
                parent_thread_id: None,
                forked_from_turn_id: None,
                title: "b".to_owned(),
                preview: String::new(),
                metadata: "updated=now".to_owned(),
            },
        ]));
        app.start_session_resume(&transport, target_a);
        app.resume_picker.as_mut().unwrap().selected = 1;
        app.poll_session_load(&transport, true).await;
        assert_eq!(app.thread_id, transport.default_thread_id());
        assert!(app.status_message.contains("selection changed"));
    }
}
