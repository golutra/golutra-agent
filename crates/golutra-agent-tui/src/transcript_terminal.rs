//! 将任务终态和因果错误归并为唯一展示单元；请求重试失败不等于任务失败。

use super::*;

#[derive(Default)]
struct TaskFacts<'a> {
    provider: Option<&'a RuntimeEvent>,
    failure: Option<&'a RuntimeEvent>,
    terminal: Option<&'a RuntimeEvent>,
}

pub(super) fn notices(events: &[&RuntimeEvent]) -> HashMap<EventId, TranscriptItem> {
    let mut tasks = HashMap::<TaskId, TaskFacts<'_>>::new();
    for &event in events {
        let Some(task) = event.task_id else { continue };
        match event.event_type {
            RuntimeEventType::ProviderStarted | RuntimeEventType::ProviderCompleted => {
                if let Some(facts) = tasks.get_mut(&task) {
                    facts.provider = None;
                }
            }
            RuntimeEventType::ProviderFailed => {
                tasks.entry(task).or_default().provider = Some(event)
            }
            RuntimeEventType::LoopDecided if loop_failed(event) => {
                // 首次失败单元可能已进入原生滚动历史，重复事实不能改变它的身份。
                tasks.entry(task).or_default().failure.get_or_insert(event);
            }
            kind if kind.is_task_terminal() => {
                let facts = tasks.entry(task).or_default();
                if facts
                    .terminal
                    .is_none_or(|previous| terminal_status(previous) != terminal_status(event))
                {
                    facts.terminal = Some(event);
                }
            }
            _ => {}
        }
    }
    tasks.into_values().filter_map(TaskFacts::notice).collect()
}

impl TaskFacts<'_> {
    fn notice(self) -> Option<(EventId, TranscriptItem)> {
        if let Some(terminal) = self.terminal
            && terminal_status(terminal) != Some(TaskStatus::Failed)
        {
            return status_event_transcript_item(terminal).map(|item| (terminal.id, item));
        }
        // ProviderFailed 只描述一次请求；等待执行失败或任务终态，避免在重连中归档假失败。
        let cause = self.failure.or(self.terminal)?;
        let anchor = self
            .failure
            .into_iter()
            .chain(self.terminal)
            .min_by_key(|event| event.sequence_no)?;
        let detail = failure_event_error(cause).unwrap_or("Task failed");
        let provider = self.provider.filter(|provider| {
            provider.turn_id == cause.turn_id
                && provider
                    .payload
                    .get("error")
                    .and_then(Value::as_str)
                    .is_some_and(|error| {
                        !error.trim().is_empty()
                            && (self.failure.is_none() || detail.contains(error))
                    })
        });
        let mut body = vec![
            visible_failure_detail(
                provider
                    .and_then(|event| event.payload.get("error").and_then(Value::as_str))
                    .unwrap_or(detail),
            )
            .to_owned(),
        ];
        if let Some(provider) = provider {
            body.extend(provider_failure_details(provider));
        }
        Some((
            anchor.id,
            TranscriptItem {
                role: TranscriptRole::Error,
                title: "Task failed".to_owned(),
                body,
            },
        ))
    }
}

pub(super) fn loop_failed(event: &RuntimeEvent) -> bool {
    // 正常决策 reason 可提及 error/failed（如“已修复错误”）；有结构化 record 时不猜关键词。
    event
        .payload
        .get("error")
        .and_then(Value::as_str)
        .is_some_and(|error| !error.trim().is_empty())
        || (event.payload.get("record").is_none()
            && event_summary(event)
                .is_some_and(|summary| summary.starts_with("runtime task execution failed:")))
}

fn terminal_status(event: &RuntimeEvent) -> Option<TaskStatus> {
    match event.event_type {
        RuntimeEventType::TaskAborted => Some(TaskStatus::Cancelled),
        RuntimeEventType::TaskInterrupted => Some(TaskStatus::Interrupted),
        RuntimeEventType::TaskUncertain => Some(TaskStatus::Uncertain),
        _ => event_task_status(event),
    }
}

pub(super) fn status_item(status: TaskStatus, detail: &str) -> Option<TranscriptItem> {
    let (role, title) = match status {
        TaskStatus::Completed | TaskStatus::Partial => return None,
        TaskStatus::Failed => (TranscriptRole::Error, "Task failed"),
        TaskStatus::Blocked => (TranscriptRole::Warning, "Task blocked"),
        TaskStatus::Cancelled => (TranscriptRole::Status, "Aborted"),
        TaskStatus::Interrupted => (TranscriptRole::Warning, "Task Interrupted"),
        TaskStatus::Uncertain => (
            TranscriptRole::Warning,
            "Task Uncertain / reconciliation required",
        ),
        _ => return None,
    };
    Some(TranscriptItem {
        role,
        title: title.to_owned(),
        body: vec![visible_failure_detail(detail).to_owned()],
    })
}
