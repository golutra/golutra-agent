//! 交接草稿独立于聊天输入框；异步生成可取消，确认创建后才切换会话。

use super::*;
use golutra_agent_client::{HandoffRequest, HandoffResult};
use ratatui::{
    text::Line,
    widgets::{Block, Borders, Paragraph},
};
#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Generating,
    Review,
    Creating,
}

#[derive(Debug)]
pub(crate) struct HandoffFlow {
    source_thread: ThreadId,
    source_session: SessionId,
    destination_thread: ThreadId,
    destination_session: SessionId,
    stage: Stage,
    draft: ComposerInput,
    error: Option<String>,
    cancellation: CancellationToken,
    operation: Option<JoinHandle<Result<HandoffResult, golutra_agent_client::ClientError>>>,
}

impl Drop for HandoffFlow {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if self.stage == Stage::Creating
            && let Some(operation) = &self.operation
        {
            operation.abort();
        }
    }
}

impl TuiApp {
    pub(crate) async fn shutdown_handoff(&mut self) {
        if let Some(mut flow) = self.handoff.take() {
            flow.cancellation.cancel();
            if let Some(mut task) = flow.operation.take()
                && tokio::time::timeout(Duration::from_secs(5), &mut task)
                    .await
                    .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
    }

    pub(crate) fn restore_handoff_draft(&mut self) {
        if !self.input.is_empty()
            || self.events.iter().any(|event| {
                matches!(
                    event.event_type,
                    RuntimeEventType::TurnStarted
                        | RuntimeEventType::TaskCreated
                        | RuntimeEventType::TurnQueued
                )
            })
        {
            return;
        }
        if let Some(draft) = self
            .events
            .iter()
            .find(|event| event.event_type == RuntimeEventType::SessionCreated)
            .and_then(|event| event.payload.get("handoff_draft"))
            .and_then(Value::as_str)
        {
            self.input.set_text(draft);
        }
    }
    pub(crate) fn start_handoff(&mut self, transport: &RuntimeTransport, goal: Option<String>) {
        if has_active_task(self) {
            self.status_message = "interrupt the active task before handoff".to_owned();
            return;
        }
        let source_thread = self.thread_id;
        let provider = self.runtime_prompt_payload(String::new());
        let transport = transport.clone();
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let operation_id = Uuid::now_v7();
        self.handoff = Some(HandoffFlow {
            source_thread,
            source_session: self.session_id,
            destination_thread: ThreadId::new(),
            destination_session: SessionId::new(),
            stage: Stage::Generating,
            draft: ComposerInput::default(),
            error: None,
            cancellation,
            operation: Some(tokio::spawn(async move {
                tokio::select! {
                    biased;
                    _ = task_cancellation.cancelled() => {
                        // 明确通知远端，不能仅断开本地 HTTP future 而留下生成任务持有租约。
                        let _ = tokio::time::timeout(Duration::from_secs(5), transport.handoff_thread(source_thread,
                            HandoffRequest::Cancel { operation_id })).await;
                        Ok(HandoffResult::Cancelled)
                    }
                    result = transport.handoff_thread(source_thread, HandoffRequest::Prepare { operation_id, goal, provider }) => result,
                }
            })),
        });
        self.status_message = "preparing handoff draft".to_owned();
    }

    pub(crate) async fn poll_handoff(&mut self) -> bool {
        let Some(flow) = &self.handoff else {
            return false;
        };
        if flow.source_session != self.session_id || flow.source_thread != self.thread_id {
            self.handoff = None;
            return true;
        }
        if !flow
            .operation
            .as_ref()
            .is_some_and(|operation| operation.is_finished())
        {
            return false;
        }
        let flow = self.handoff.as_mut().expect("handoff flow");
        let result = flow.operation.take().expect("finished operation").await;
        match result {
            Ok(Ok(HandoffResult::Cancelled)) => {
                self.handoff = None;
                self.status_message = "handoff cancelled; original session retained".to_owned();
            }
            Ok(Ok(HandoffResult::Draft { draft })) if flow.stage == Stage::Generating => {
                flow.draft.set_text(draft);
                flow.draft.move_to_start();
                flow.stage = Stage::Review;
                self.status_message = "review and edit the handoff draft".to_owned();
            }
            Ok(Ok(HandoffResult::Created { thread }))
                if flow.stage == Stage::Creating
                    && thread.thread_id == flow.destination_thread
                    && thread.session_id == flow.destination_session =>
            {
                let draft = flow.draft.text().to_owned();
                self.handoff = None;
                self.start_new_session();
                self.thread_id = thread.thread_id;
                self.session_id = thread.session_id;
                self.input.set_text(draft);
                self.status_message =
                    "handoff ready — review the prompt, then Enter to send".to_owned();
            }
            result => {
                flow.error = Some(match result {
                    Ok(Err(error)) => error.to_string(),
                    Err(error) => format!("handoff operation failed: {error}"),
                    _ => "unexpected handoff response".to_owned(),
                });
                flow.stage = Stage::Review;
                self.status_message = "handoff failed; original session retained".to_owned();
            }
        }
        true
    }
}

pub(crate) fn handle_key(key: KeyEvent, app: &mut TuiApp, transport: &RuntimeTransport) {
    let Some(flow) = app.handoff.as_mut() else {
        return;
    };
    if key.code == KeyCode::Esc {
        if flow.stage == Stage::Creating {
            app.status_message =
                "waiting for session creation; the draft will not be sent".to_owned();
        } else {
            app.handoff = None;
            app.status_message = "handoff cancelled; original session retained".to_owned();
        }
        return;
    }
    if flow.stage != Stage::Review {
        return;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('s') if !flow.draft.trimmed().is_empty() => {
                if has_active_task(app) {
                    app.status_message = "interrupt the active task before handoff".to_owned();
                    return;
                }
                let flow = app.handoff.as_mut().expect("handoff flow");
                let request = HandoffRequest::Create {
                    thread_id: flow.destination_thread,
                    session_id: flow.destination_session,
                    draft: flow.draft.text().to_owned(),
                };
                let source = flow.source_thread;
                let transport = transport.clone();
                flow.operation = Some(tokio::spawn(async move {
                    transport.handoff_thread(source, request).await
                }));
                flow.stage = Stage::Creating;
                flow.error = None;
            }
            KeyCode::Char('a') => flow.draft.move_to_line_start(),
            KeyCode::Char('e') => flow.draft.move_to_line_end(),
            KeyCode::Char('z') => {
                flow.draft.undo();
            }
            KeyCode::Char('y') => {
                flow.draft.redo();
            }
            _ => {}
        }
        return;
    }
    match key.code {
        KeyCode::Enter => flow.draft.insert_newline(),
        KeyCode::Char(ch) => flow.draft.insert_char(ch),
        KeyCode::Backspace => flow.draft.delete_backward(),
        KeyCode::Delete => flow.draft.delete_forward(),
        KeyCode::Left => flow.draft.move_left(),
        KeyCode::Right => flow.draft.move_right(),
        KeyCode::Up => {
            flow.draft.move_line_up();
        }
        KeyCode::Down => {
            flow.draft.move_line_down();
        }
        KeyCode::Home => flow.draft.move_to_start(),
        KeyCode::End => flow.draft.move_to_end(),
        _ => {}
    }
}

pub(crate) fn paste(app: &mut TuiApp, text: &str) {
    if let Some(flow) = app.handoff.as_mut()
        && flow.stage == Stage::Review
    {
        flow.draft.insert_str(text);
    }
}

pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let Some(flow) = &app.handoff else {
        return;
    };
    let title = match flow.stage {
        Stage::Generating => "Handoff — generating draft (Esc cancel)",
        Stage::Review => "Handoff — edit draft · Ctrl+S create session · Esc cancel",
        Stage::Creating => "Handoff — creating session; prompt will not be sent",
    };
    let block = Block::default().borders(Borders::TOP).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if let Some(error) = &flow.error {
        frame.render_widget(
            Paragraph::new(error.as_str()).wrap(ratatui::widgets::Wrap { trim: false }),
            Rect {
                height: inner.height.min(3),
                ..inner
            },
        );
    }
    let error_height = if flow.error.is_some() {
        inner.height.min(3)
    } else {
        0
    };
    let editor = Rect {
        y: inner.y.saturating_add(error_height),
        height: inner.height.saturating_sub(error_height),
        ..inner
    };
    if editor.width == 0 || editor.height == 0 {
        return;
    }
    let viewport = flow.draft.viewport(editor.width, editor.height);
    frame.render_widget(
        Paragraph::new(
            viewport
                .lines
                .into_iter()
                .map(Line::from)
                .collect::<Vec<_>>(),
        ),
        editor,
    );
    if flow.stage == Stage::Review {
        frame.set_cursor_position((editor.x + viewport.cursor.0, editor.y + viewport.cursor.1));
    }
}
