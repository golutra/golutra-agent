use super::*;
use golutra_agent_core::{EventId, RUNTIME_EVENT_SCHEMA_VERSION};
use golutra_agent_protocol::RuntimeEventSource;
use golutra_agent_tools::DelegationNotification;

// 缓存只保留通知/恢复需要的事实；游标仍跨过所有事件，避免每轮重读流式历史。
const MAX_CACHED_SESSIONS: usize = 32;
const MAX_CACHED_FACTS: usize = 4096;
const MAX_CACHED_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Default)]
pub(crate) struct NotificationCache {
    sessions:
        std::collections::HashMap<SessionId, (u64, Arc<tokio::sync::Mutex<NotificationIndex>>)>,
    clock: u64,
}

#[derive(Debug, Default, Clone)]
struct NotificationIndex {
    cursor: Option<u64>,
    facts: Arc<Vec<RuntimeEvent>>,
    bytes: usize,
}

impl NotificationIndex {
    fn apply(&mut self, event: RuntimeEvent) {
        self.cursor = Some(event.sequence_no);
        if matches!(
            event.event_type,
            RuntimeEventType::TaskCreated | RuntimeEventType::CompactionCompleted
        ) {
            let published = self
                .facts
                .iter()
                .filter(|fact| fact.event_type == RuntimeEventType::SubagentUpdated)
                .filter_map(|fact| fact.payload["tool_call_id"].as_str().map(str::to_owned))
                .collect::<std::collections::HashSet<_>>();
            Arc::make_mut(&mut self.facts).retain(|fact| {
                !matches!(
                    fact.event_type,
                    RuntimeEventType::TaskCreated
                        | RuntimeEventType::CompactionCompleted
                        | RuntimeEventType::SubagentUpdated
                ) && !fact.payload["tool_call_id"]
                    .as_str()
                    .is_some_and(|id| published.contains(id))
                    && !fact.payload["envelope"]["tool_call_id"]
                        .as_str()
                        .is_some_and(|id| published.contains(id))
            });
            self.bytes = self
                .facts
                .iter()
                .map(|fact| fact.payload.to_string().len() + 256)
                .sum();
        }
        let managed = match event.event_type {
            RuntimeEventType::SubagentUpdated => event.payload["facts"]["background"] == true,
            RuntimeEventType::ToolCompleted => {
                event.payload["envelope"]["tool_name"] == "subagent"
                    && event.payload["envelope"]["structured_facts"]["background"] == true
            }
            _ => false,
        };
        if managed {
            let call = event.payload["tool_call_id"]
                .as_str()
                .or_else(|| event.payload["envelope"]["tool_call_id"].as_str());
            if let Some(call) = call {
                Arc::make_mut(&mut self.facts).retain(|fact| {
                    fact.payload["tool_call_id"] != call
                        && fact.payload["envelope"]["tool_call_id"] != call
                });
                self.bytes = self
                    .facts
                    .iter()
                    .map(|fact| fact.payload.to_string().len() + 256)
                    .sum();
            }
            return;
        }
        let relevant = match event.event_type {
            RuntimeEventType::TaskCreated
            | RuntimeEventType::CompactionCompleted
            | RuntimeEventType::SubagentUpdated => true,
            RuntimeEventType::ToolStarted => event.payload["tool_name"] == "subagent",
            RuntimeEventType::ToolCompleted => event.payload["envelope"]["tool_name"] == "subagent",
            _ => false,
        };
        if relevant {
            self.bytes = self
                .bytes
                .saturating_add(event.payload.to_string().len() + 256);
            Arc::make_mut(&mut self.facts).push(event);
        }
    }
}

async fn notification_facts(
    host: &RuntimeHost,
    session: SessionId,
) -> Result<Arc<Vec<RuntimeEvent>>, ClientError> {
    let shared = {
        let mut cache = host.execution.delegation_notification_cache.lock().await;
        cache.clock = cache.clock.saturating_add(1);
        let clock = cache.clock;
        let entry = cache.sessions.entry(session).or_insert_with(|| {
            (
                clock,
                Arc::new(tokio::sync::Mutex::new(NotificationIndex::default())),
            )
        });
        entry.0 = clock;
        let shared = entry.1.clone();
        if cache.sessions.len() > MAX_CACHED_SESSIONS
            && let Some(oldest) = cache
                .sessions
                .iter()
                .min_by_key(|(_, entry)| entry.0)
                .map(|(id, _)| *id)
        {
            cache.sessions.remove(&oldest);
        }
        shared
    };
    let mut index = shared.lock().await;
    loop {
        let page = host
            .storage
            .repositories
            .events
            .load_page(session, None, index.cursor, 256)
            .await?;
        let count = page.len();
        for event in page {
            index.apply(event);
        }
        if count < 256 {
            break;
        }
    }
    let facts = index.facts.clone();
    // 超大历史仍从持久层完整读取，不以丢通知换取缓存命中。
    if index.bytes > MAX_CACHED_BYTES || index.facts.len() > MAX_CACHED_FACTS {
        let mut cache = host.execution.delegation_notification_cache.lock().await;
        if cache
            .sessions
            .get(&session)
            .is_some_and(|(_, cached)| Arc::ptr_eq(cached, &shared))
        {
            cache.sessions.remove(&session);
        }
    }
    Ok(facts)
}

/// Read the durable registry; querying never publishes or consumes notifications.
pub(super) async fn status(host: &RuntimeHost, session: SessionId) -> Result<Value, ClientError> {
    registry::list(host, session, 0, 16).await
}

pub(super) async fn publish(
    host: &RuntimeHost,
    request: &ToolRequest,
    result: &Result<TaskDelegationOutput, ClientError>,
) -> Result<(), ClientError> {
    let _guard = host.execution.delegation_notification_lock.lock().await;
    let id = EventId(deterministic_uuid(
        &request.tool_call_id.to_string(),
        "subagent-completion",
    ));
    if host.storage.store.event_by_id(id).await?.is_some() {
        return Ok(());
    }
    let events = notification_facts(host, request.session_id).await?;
    let origin = events.iter().rev().find(|event| {
        event.event_type == RuntimeEventType::ToolStarted
            && event.payload.get("tool_call_id").and_then(Value::as_str)
                == Some(&request.tool_call_id.to_string())
    });
    let task_id = match origin.and_then(|event| event.task_id) {
        Some(task_id) => Some(task_id),
        None => host
            .storage
            .store
            .background()
            .find_task_by_tool_call(host.workspace_id, request.session_id, request.tool_call_id)
            .await?
            .map(|task| task.scope.task_id),
    };
    let payload = match result {
        Ok(output) => {
            json!({"tool_call_id":request.tool_call_id,"summary":output.summary,"content":output.content,"facts":output.structured_facts})
        }
        Err(error) => {
            json!({"tool_call_id":request.tool_call_id,"summary":error.to_string(),"content":"","facts":{"child_status":"failed","child_terminal":true,"completed":false}})
        }
    };
    host.record_event(RuntimeEvent {
        schema_version: RUNTIME_EVENT_SCHEMA_VERSION,
        id,
        sequence_no: 0,
        session_id: request.session_id,
        task_id,
        turn_id: request.turn_id,
        parent_event_id: origin.map(|event| event.id),
        causal_context: Default::default(),
        causal_links: Vec::new(),
        event_type: RuntimeEventType::SubagentUpdated,
        timestamp: Utc::now(),
        source: RuntimeEventSource::Tool,
        payload,
        payload_ref: None,
        durable: true,
    })
    .await
}

pub(super) async fn load(
    host: &RuntimeHost,
    session: SessionId,
) -> Result<Vec<DelegationNotification>, ClientError> {
    reconcile(host, session).await?;
    let events = notification_facts(host, session).await?;
    let boundary = events
        .iter()
        .filter(|event| {
            matches!(
                event.event_type,
                RuntimeEventType::TaskCreated | RuntimeEventType::CompactionCompleted
            )
        })
        .map(|event| event.sequence_no)
        .max()
        .unwrap_or(0);
    Ok(events
        .iter()
        .filter(|event| {
            event.event_type == RuntimeEventType::SubagentUpdated
                && event.sequence_no > boundary
                && event.payload["facts"]["background"] != true
        })
        .map(|event| DelegationNotification {
            id: format!("event:{}", event.id),
            child_session_id: event
                .payload
                .pointer("/facts/child_session_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            child_task_id: event
                .payload
                .pointer("/facts/child_task_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            content: model_content(event),
        })
        .collect())
}

async fn reconcile(host: &RuntimeHost, session: SessionId) -> Result<(), ClientError> {
    let events = notification_facts(host, session).await?;
    let origins: std::collections::HashMap<_, _> = events
        .iter()
        .filter(|event| event.event_type == RuntimeEventType::ToolStarted)
        .filter_map(|event| event.payload["tool_call_id"].as_str().map(|id| (id, event)))
        .collect();
    let published: std::collections::HashSet<_> = events
        .iter()
        .filter(|event| event.event_type == RuntimeEventType::SubagentUpdated)
        .filter_map(|event| event.payload.get("tool_call_id").and_then(Value::as_str))
        .collect();
    for event in events.iter() {
        if event.event_type != RuntimeEventType::ToolCompleted
            || event
                .payload
                .pointer("/envelope/tool_name")
                .and_then(Value::as_str)
                != Some("subagent")
        {
            continue;
        }
        let Some(call_id) = event
            .payload
            .pointer("/envelope/tool_call_id")
            .and_then(Value::as_str)
        else {
            continue;
        };
        let origin = origins.get(call_id).copied();
        if origin.is_none_or(|event| {
            !matches!(
                event
                    .payload
                    .pointer("/arguments/action")
                    .and_then(Value::as_str)
                    .unwrap_or("spawn"),
                "spawn" | "resume"
            )
        }) {
            continue;
        }
        if published.contains(call_id) {
            continue;
        }
        let facts = &event.payload["envelope"]["structured_facts"];
        let Some(child) = facts
            .get("child_session_id")
            .and_then(Value::as_str)
            .and_then(|id| id.parse::<SessionId>().ok())
        else {
            continue;
        };
        let active = host
            .execution
            .delegation_operations
            .lock()
            .await
            .values()
            .any(|operation| {
                operation.belongs_to(session)
                    && control::operation_session(operation) == Some(child)
                    && !operation.is_complete()
            });
        if active {
            continue;
        }
        let Some(thread) = host.storage.repositories.threads.by_session(child).await? else {
            continue;
        };
        let Some(parent) = host
            .storage
            .repositories
            .threads
            .by_session(session)
            .await?
        else {
            continue;
        };
        if thread.parent_thread_id != Some(parent.thread_id) {
            continue;
        }
        host.reconcile_replayed_delegated_prompt(child).await?;
        // 通知属于一次执行，不能在 resume 后把最新子任务的结果挂到旧调用上。
        let child_task = match facts.get("child_task_id").and_then(Value::as_str) {
            Some(id) => Some(id.parse::<golutra_agent_core::TaskId>().map_err(|_| {
                ClientError::TaskExecution("invalid persisted child task id".to_owned())
            })?),
            None => host
                .storage
                .repositories
                .events
                .load(child, None, None)
                .await?
                .iter()
                .find(|event| {
                    event.event_type == RuntimeEventType::TaskCreated
                        && event
                            .payload
                            .pointer("/payload/_delegation_parent_tool_call_id")
                            .and_then(Value::as_str)
                            == Some(call_id)
                })
                .and_then(|event| event.task_id),
        };
        let Some(child_task) = child_task else {
            continue;
        };
        let state = host
            .storage
            .repositories
            .projections
            .state(child, Some(child_task))
            .await?;
        if !state.task_status.is_terminal() {
            continue;
        }
        let output = control::state_output(host, &state).await;
        let request = ToolRequest {
            tool_call_id: call_id.parse().map_err(|_| {
                ClientError::TaskExecution("invalid persisted subagent call id".to_owned())
            })?,
            provider_tool_call_id: None,
            session_id: session,
            turn_id: event.turn_id,
            tool_name: "subagent".to_owned(),
            arguments: Value::Null,
        };
        publish(host, &request, &output).await?;
    }
    Ok(())
}

pub(crate) fn model_content(event: &RuntimeEvent) -> String {
    let facts = &event.payload["facts"];
    let content = event
        .payload
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let summary = event
        .payload
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or_default();
    format!(
        "Subagent completion (runtime observation): {}",
        json!({
            "child_session_id":facts.get("child_session_id"),"child_task_id":facts.get("child_task_id"),"child_status":facts.get("child_status"),
            "child_terminal":facts.get("child_terminal"),"completed":facts.get("completed"),
            "summary":summary.chars().take(256).collect::<String>(),"content":content.chars().take(1024).collect::<String>(),
            "child_result_has_more":content.chars().count() > 1024,"child_result_next_offset":1024,
        })
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_codec::host_event;

    #[tokio::test]
    async fn recovered_notification_uses_original_execution_after_child_resumes() {
        let host = RuntimeHost::in_memory().await.unwrap();
        let parent = host.default_session_id();
        host.upsert_current_thread(parent, &json!({"prompt":"parent"}))
            .await
            .unwrap();
        let thread = host
            .storage
            .repositories
            .threads
            .by_session(parent)
            .await
            .unwrap()
            .unwrap();
        let child = SessionId::new();
        host.upsert_current_thread(
            child,
            &json!({"prompt":"child", "_parent_thread_id":thread.thread_id}),
        )
        .await
        .unwrap();
        let first = golutra_agent_core::TaskId::new();
        let second = golutra_agent_core::TaskId::new();
        for (task, content) in [(first, "original findings"), (second, "later findings")] {
            for (kind, payload) in [
                (
                    RuntimeEventType::TaskCreated,
                    json!({"payload":{"prompt":content}}),
                ),
                (
                    RuntimeEventType::AssistantMessage,
                    json!({"content":content}),
                ),
                (
                    RuntimeEventType::TaskCompleted,
                    json!({"status":"completed"}),
                ),
            ] {
                host.record_event(host_event(
                    host.next_sequence_no(),
                    child,
                    Some(task),
                    kind,
                    RuntimeEventSource::Runtime,
                    payload,
                ))
                .await
                .unwrap();
            }
        }
        let call = ToolCallId::new();
        for (kind, payload) in [
            (
                RuntimeEventType::ToolStarted,
                json!({"tool_name":"subagent", "tool_call_id":call, "arguments":{"action":"spawn"}}),
            ),
            (
                RuntimeEventType::ToolCompleted,
                json!({"envelope":{"tool_name":"subagent", "tool_call_id":call,
                "structured_facts":{"child_session_id":child, "child_task_id":first}}}),
            ),
        ] {
            host.record_event(host_event(
                host.next_sequence_no(),
                parent,
                None,
                kind,
                RuntimeEventSource::Tool,
                payload,
            ))
            .await
            .unwrap();
        }
        let notices = load(&host, parent).await.unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].child_task_id, Some(first.to_string()));
        assert!(notices[0].content.contains("original findings"));
        assert!(!notices[0].content.contains("later findings"));
        assert_eq!(load(&host, parent).await.unwrap().len(), 1);
        host.close().await.unwrap();
    }

    #[tokio::test]
    async fn managed_subagent_history_keeps_a_small_incremental_cache_after_5000_events() {
        let host = RuntimeHost::in_memory().await.unwrap();
        let session = host.default_session_id();
        for _ in 0..1700 {
            let call = ToolCallId::new();
            for (kind, payload) in [
                (RuntimeEventType::TaskCreated, json!({})),
                (
                    RuntimeEventType::ToolStarted,
                    json!({"tool_name":"subagent","tool_call_id":call}),
                ),
                (
                    RuntimeEventType::ToolCompleted,
                    json!({"envelope":{"tool_name":"subagent","tool_call_id":call,"structured_facts":{"background":true}}}),
                ),
            ] {
                host.storage
                    .repositories
                    .events
                    .append_assigning_sequence(host_event(
                        0,
                        session,
                        None,
                        kind,
                        RuntimeEventSource::Runtime,
                        payload,
                    ))
                    .await
                    .unwrap();
            }
        }
        let start = std::time::Instant::now();
        let first = notification_facts(&host, session).await.unwrap();
        let cold = start.elapsed();
        assert_eq!(
            first.len(),
            1,
            "settled managed launches are not retained in the legacy index"
        );
        let mut samples = Vec::new();
        for _ in 0..100 {
            let start = std::time::Instant::now();
            let next = notification_facts(&host, session).await.unwrap();
            samples.push(start.elapsed().as_micros());
            assert!(
                Arc::ptr_eq(&first, &next),
                "must not fall back to replaying the full event history"
            );
        }
        samples.sort_unstable();
        let cache = host.execution.delegation_notification_cache.lock().await;
        let shared = cache.sessions.get(&session).unwrap().1.clone();
        drop(cache);
        let index = shared.lock().await;
        assert!(index.cursor.unwrap() >= 5100);
        assert!(index.bytes < 1024);
        eprintln!(
            "managed-history events=5100 warm_samples=100 cold_us={} warm_p50_us={} warm_p95_us={} retained_facts={} retained_payload_bytes={}",
            cold.as_micros(),
            samples[49],
            samples[94],
            index.facts.len(),
            index.bytes
        );
        drop(index);
        host.close().await.unwrap();
    }

    #[tokio::test]
    async fn notification_index_advances_over_irrelevant_events_and_only_appends_new_facts() {
        let host = RuntimeHost::in_memory().await.unwrap();
        let session = host.default_session_id();
        for index in 0..300 {
            host.record_event(host_event(
                host.next_sequence_no(),
                session,
                None,
                if index == 0 {
                    RuntimeEventType::TaskCreated
                } else {
                    RuntimeEventType::ToolStarted
                },
                RuntimeEventSource::Runtime,
                json!({"tool_name":"read_file"}),
            ))
            .await
            .unwrap();
        }
        let first = notification_facts(&host, session).await.unwrap();
        assert_eq!(first.len(), 1);
        let cached = notification_facts(&host, session).await.unwrap();
        assert!(
            Arc::ptr_eq(&first, &cached),
            "unchanged history must reuse the index"
        );
        let call = ToolRequest {
            tool_call_id: ToolCallId::new(),
            provider_tool_call_id: None,
            session_id: session,
            turn_id: None,
            tool_name: "subagent".to_owned(),
            arguments: json!({}),
        };
        publish(
            &host,
            &call,
            &Err(ClientError::TaskExecution("real failure".to_owned())),
        )
        .await
        .unwrap();
        let next = notification_facts(&host, session).await.unwrap();
        assert_eq!(next.len(), 2);
        assert_eq!(first.len(), 1, "a reader's snapshot cannot mutate");
        assert_eq!(next[1].payload["facts"]["completed"], false);
        assert_eq!(next[1].payload["facts"]["child_terminal"], true);
        publish(
            &host,
            &call,
            &Err(ClientError::TaskExecution("real failure".to_owned())),
        )
        .await
        .unwrap();
        assert_eq!(notification_facts(&host, session).await.unwrap().len(), 2);
        host.close().await.unwrap();
    }
}
