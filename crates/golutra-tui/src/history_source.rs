//! Canonical event history loading for transcript and developer replay.

use std::{collections::BTreeMap, future::Future};

use golutra_client::{RuntimeClient, RuntimeTransport};
use golutra_core::{SessionId, TaskId};
use golutra_protocol::{EventPage, EventPageDirection, EventPageRequest, RuntimeEvent};

use super::TUI_HISTORY_PAGE_SIZE;

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct LoadedEventHistory {
    pub(crate) events: Vec<RuntimeEvent>,
    pub(crate) start_cursor: Option<u64>,
    pub(crate) end_cursor: Option<u64>,
    pub(crate) has_more_before: bool,
}

const COMPLETE_HISTORY_EVENT_LIMIT: usize = 32_768;
const COMPLETE_HISTORY_BYTE_LIMIT: usize = 32 * 1024 * 1024;

/// 首屏只读取最近一页；更早事件由 TUI 在用户向上滚动时按游标加载。
pub(crate) async fn load_recent_event_history(
    transport: &RuntimeTransport,
    session_id: SessionId,
    task_id: Option<TaskId>,
) -> Result<LoadedEventHistory, String> {
    let page = transport
        .event_page(EventPageRequest {
            session_id,
            task_id,
            cursor: None,
            direction: EventPageDirection::Backward,
            limit: TUI_HISTORY_PAGE_SIZE,
        })
        .await
        .map_err(|error| error.to_string())?;
    Ok(LoadedEventHistory {
        start_cursor: page.start_cursor,
        end_cursor: page.end_cursor,
        events: page.events,
        has_more_before: page.has_more,
    })
}

pub(crate) async fn load_complete_event_history(
    transport: &RuntimeTransport,
    session_id: SessionId,
    task_id: Option<TaskId>,
) -> Result<LoadedEventHistory, String> {
    let transport = transport.clone();
    load_complete_event_history_with(session_id, task_id, move |request| {
        let transport = transport.clone();
        async move {
            transport
                .event_page(request)
                .await
                .map_err(|error| error.to_string())
        }
    })
    .await
}

async fn load_complete_event_history_with<F, Fut>(
    session_id: SessionId,
    task_id: Option<TaskId>,
    mut load_page: F,
) -> Result<LoadedEventHistory, String>
where
    F: FnMut(EventPageRequest) -> Fut,
    Fut: Future<Output = Result<EventPage, String>>,
{
    let mut cursor = None;
    let mut events = BTreeMap::<u64, RuntimeEvent>::new();
    let mut event_bytes = 0_usize;

    loop {
        let page = load_page(EventPageRequest {
            session_id,
            task_id,
            cursor,
            direction: EventPageDirection::Backward,
            limit: TUI_HISTORY_PAGE_SIZE,
        })
        .await?;

        let next_cursor = page.start_cursor;
        for event in page.events {
            if events.contains_key(&event.sequence_no) {
                continue;
            }
            event_bytes = event_bytes.saturating_add(runtime_event_size(&event));
            if events.len() >= COMPLETE_HISTORY_EVENT_LIMIT
                || event_bytes > COMPLETE_HISTORY_BYTE_LIMIT
            {
                return Err(format!(
                    "event history exceeds the bounded replay budget ({} events or {} bytes)",
                    COMPLETE_HISTORY_EVENT_LIMIT, COMPLETE_HISTORY_BYTE_LIMIT
                ));
            }
            events.insert(event.sequence_no, event);
        }

        if !page.has_more {
            break;
        }
        let Some(next_cursor) = next_cursor else {
            return Err("event history page reported more data without a start cursor".to_owned());
        };
        if cursor.is_some_and(|cursor| next_cursor >= cursor) {
            return Err(format!(
                "event history cursor did not move backward: {cursor:?} -> {next_cursor}"
            ));
        }
        cursor = Some(next_cursor);
    }

    let events = events.into_values().collect::<Vec<_>>();
    Ok(LoadedEventHistory {
        start_cursor: events.first().map(|event| event.sequence_no),
        end_cursor: events.last().map(|event| event.sequence_no),
        events,
        has_more_before: false,
    })
}

fn runtime_event_size(event: &RuntimeEvent) -> usize {
    // 负载通常占事件内存的大头；固定余量覆盖 ID、时间戳和因果元数据，
    // 避免为预算计算再次序列化完整事件。
    256_usize.saturating_add(
        serde_json::to_vec(&event.payload)
            .map(|payload| payload.len())
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, future::ready};

    use chrono::Utc;
    use golutra_core::{EventId, RUNTIME_EVENT_SCHEMA_VERSION};
    use golutra_protocol::{RuntimeEventSource, RuntimeEventType};
    use serde_json::json;

    use super::*;

    fn event(sequence_no: u64, session_id: SessionId) -> RuntimeEvent {
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
            timestamp: Utc::now(),
            source: RuntimeEventSource::User,
            payload: json!({"summary": sequence_no.to_string()}),
            payload_ref: None,
            durable: true,
        }
    }

    #[tokio::test]
    async fn complete_history_sorts_and_deduplicates_backward_pages() {
        let session_id = SessionId::new();
        let duplicate = event(3, session_id);
        let mut pages = VecDeque::from([
            EventPage {
                direction: EventPageDirection::Backward,
                events: vec![
                    duplicate.clone(),
                    event(5, session_id),
                    event(4, session_id),
                ],
                start_cursor: Some(3),
                end_cursor: Some(5),
                has_more: true,
            },
            EventPage {
                direction: EventPageDirection::Backward,
                events: vec![event(2, session_id), duplicate, event(1, session_id)],
                start_cursor: Some(1),
                end_cursor: Some(3),
                has_more: false,
            },
        ]);

        let history = load_complete_event_history_with(session_id, None, |_| {
            ready(Ok(pages.pop_front().expect("history page")))
        })
        .await
        .expect("complete history");

        assert_eq!(
            history
                .events
                .iter()
                .map(|event| event.sequence_no)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(history.start_cursor, Some(1));
        assert_eq!(history.end_cursor, Some(5));
    }

    #[tokio::test]
    async fn complete_history_loads_more_than_one_runtime_page() {
        let session_id = SessionId::new();
        let events = (1..=600)
            .map(|sequence_no| event(sequence_no, session_id))
            .collect::<Vec<_>>();
        let mut pages = VecDeque::from([
            EventPage {
                direction: EventPageDirection::Backward,
                events: events[512..].to_vec(),
                start_cursor: Some(513),
                end_cursor: Some(600),
                has_more: true,
            },
            EventPage {
                direction: EventPageDirection::Backward,
                events: events[256..513].to_vec(),
                start_cursor: Some(257),
                end_cursor: Some(513),
                has_more: true,
            },
            EventPage {
                direction: EventPageDirection::Backward,
                events: events[..257].to_vec(),
                start_cursor: Some(1),
                end_cursor: Some(257),
                has_more: false,
            },
        ]);

        let history = load_complete_event_history_with(session_id, None, |_| {
            ready(Ok(pages.pop_front().expect("history page")))
        })
        .await
        .expect("complete history");

        assert_eq!(history.events.len(), 600);
        assert_eq!(history.start_cursor, Some(1));
        assert_eq!(history.end_cursor, Some(600));
        assert_eq!(history.events[255].sequence_no, 256);
        assert_eq!(history.events[512].sequence_no, 513);
    }

    #[tokio::test]
    async fn complete_history_rejects_a_non_advancing_cursor() {
        let session_id = SessionId::new();
        let mut calls = 0;
        let error = load_complete_event_history_with(session_id, None, |_| {
            calls += 1;
            ready(Ok(EventPage {
                direction: EventPageDirection::Backward,
                events: vec![event(10, session_id)],
                start_cursor: Some(10),
                end_cursor: Some(10),
                has_more: true,
            }))
        })
        .await
        .expect_err("stalled cursor");

        assert_eq!(calls, 2);
        assert!(error.contains("did not move backward"));
    }
}
