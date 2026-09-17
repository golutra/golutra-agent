//! 从持久 artifact 加载用户详情；不执行工具、不读取当前工作区，也不回灌模型上下文。

use base64::Engine;
use golutra_agent_client::{RuntimeTransport, TaskTraceClient};
use golutra_agent_core::ArtifactId;
use golutra_agent_protocol::ArtifactReadRequest;
use sha2::{Digest, Sha256};

use super::*;

const CHUNK_BYTES: u64 = 64 * 1024;
// 与运行时有界证据配合；多工具合并详情不能无限累积内存。
const DETAIL_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetailSource {
    pub(crate) artifact_id: ArtifactId,
    label: String,
    diff: bool,
    truncated: bool,
}

pub(crate) fn sources(app: &TuiApp, id: &OperationId) -> Vec<DetailSource> {
    let Some(entry) = history_event_operations(app)
        .into_iter()
        .find(|entry| entry.projection.id() == Some(id))
    else {
        return Vec::new();
    };
    let mut calls = entry
        .event_ids
        .iter()
        .filter_map(|event_id| app.events.iter().find(|event| &event.id == event_id))
        .filter_map(call_key)
        .collect::<HashSet<_>>();
    // UserStep 批次可能替换投影事件身份；实际 artifact 仍挂在各 ToolCompleted 上。
    for event in app
        .events
        .iter()
        .filter(|event| entry.event_ids.contains(&event.id))
    {
        if let Some(step) =
            event.payload.get("step").cloned().and_then(|value| {
                serde_json::from_value::<golutra_agent_core::UserStep>(value).ok()
            })
            && let golutra_agent_core::UserStepKind::ToolBatch { tools, .. } = step.kind
        {
            calls.extend(tools.into_iter().map(|tool| tool.tool_call_id.to_string()));
        }
    }
    calls.insert(
        id.as_str()
            .strip_prefix("terminal:proc-")
            .unwrap_or(id.as_str())
            .to_owned(),
    );
    let mut result: Vec<(String, DetailSource)> = Vec::new();
    let completed_outputs = app
        .events
        .iter()
        .filter(|event| event.payload.get("output_artifact_ref").is_some())
        .filter_map(call_key)
        .collect::<HashSet<_>>();
    for event in &app.events {
        let key = call_key(event).unwrap_or_else(|| event.id.to_string());
        if !entry.event_ids.contains(&event.id) && !calls.contains(&key) {
            continue;
        }
        let diff = event.payload.get("diff_artifact_ref");
        if diff.is_none()
            && event
                .payload
                .pointer("/envelope/structured_facts/process_state")
                .and_then(Value::as_str)
                == Some("running")
        {
            continue;
        }
        if diff.is_none()
            && event.payload.get("output_artifact_ref").is_none()
            && completed_outputs.contains(&key)
        {
            continue;
        }
        let raw = event
            .payload
            .get("output_artifact_ref")
            .or_else(|| event.payload.pointer("/envelope/raw_artifact_ref"));
        let Some(artifact_id) = diff
            .or(raw)
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<Uuid>().ok())
            .map(ArtifactId)
        else {
            continue;
        };
        let label = event
            .payload
            .get("command")
            .or_else(|| event.payload.pointer("/envelope/structured_facts/command"))
            .and_then(Value::as_str)
            .unwrap_or(if diff.is_some() { "Diff" } else { "Output" })
            .to_owned();
        let truncated = event.payload.get("diff_artifact_truncated") == Some(&json!(true))
            || event.payload.get("output_artifact_truncated") == Some(&json!(true))
            || event
                .payload
                .pointer("/envelope/structured_facts/output_truncated")
                == Some(&json!(true));
        let source = DetailSource {
            artifact_id,
            label,
            diff: diff.is_some(),
            truncated,
        };
        if let Some((_, previous)) = result
            .iter_mut()
            .find(|(previous_key, previous)| previous_key == &key && previous.diff == source.diff)
        {
            *previous = source;
        } else {
            result.push((key, source));
        }
    }
    result.into_iter().map(|(_, source)| source).collect()
}

fn call_key(event: &RuntimeEvent) -> Option<String> {
    event
        .payload
        .get("process_id")
        .and_then(Value::as_str)
        .map(|value| value.strip_prefix("proc-").unwrap_or(value).to_owned())
        .or_else(|| {
            event
                .payload
                .get("tool_call_id")
                .or_else(|| event.payload.pointer("/envelope/tool_call_id"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}

pub(crate) async fn load(transport: RuntimeTransport, sources: Vec<DetailSource>) -> Vec<String> {
    let mut lines = Vec::new();
    let mut remaining = DETAIL_BYTES;
    for source in sources {
        lines.push(source.label);
        match read_text(&transport, source.artifact_id, remaining).await {
            Ok(text) => {
                remaining = remaining.saturating_sub(text.len());
                let text = terminal_text(&text);
                if source.diff {
                    lines.extend(tool_preview::numbered_diff(
                        text.split('\n').map(str::to_owned),
                    ));
                } else {
                    lines.extend(text.split('\n').map(str::to_owned));
                }
                if source.truncated {
                    lines.push("[Capture was truncated; this is all retained content.]".to_owned());
                }
            }
            Err(error) => lines.push(format!("[Content unavailable: {error}]")),
        }
        lines.push(String::new());
    }
    lines
}

async fn read_text(
    transport: &impl TaskTraceClient,
    id: ArtifactId,
    limit: usize,
) -> Result<String, String> {
    read_chunks(id, limit, |request| transport.read_artifact_chunk(request)).await
}

async fn read_chunks<F, Fut>(id: ArtifactId, limit: usize, mut read: F) -> Result<String, String>
where
    F: FnMut(ArtifactReadRequest) -> Fut,
    Fut: std::future::Future<
            Output = Result<
                Option<golutra_agent_protocol::ArtifactChunk>,
                golutra_agent_client::ClientError,
            >,
        >,
{
    let mut bytes = Vec::new();
    let mut identity = None;
    loop {
        let chunk = read(ArtifactReadRequest {
            artifact_id: id,
            offset: bytes.len() as u64,
            length: CHUNK_BYTES,
        })
        .await
        .map_err(|e| e.to_string())?
        .ok_or("artifact missing or expired")?;
        if chunk.total_size > limit as u64 {
            return Err("saved content exceeds the 16 MiB detail budget".to_owned());
        }
        if chunk.artifact_id != id || chunk.offset != bytes.len() as u64 {
            return Err("artifact cursor mismatch".to_owned());
        }
        let key = (chunk.total_size, chunk.checksum.clone());
        if identity.as_ref().is_some_and(|previous| previous != &key) {
            return Err("artifact changed during reading".to_owned());
        }
        identity = Some(key);
        let part = base64::engine::general_purpose::STANDARD
            .decode(chunk.content_base64)
            .map_err(|e| e.to_string())?;
        if part.len() as u64 != chunk.length
            || chunk.length > CHUNK_BYTES
            || bytes.len().saturating_add(part.len()) > limit
            || (bytes.len() as u64).saturating_add(chunk.length) > chunk.total_size
        {
            return Err("invalid artifact length".to_owned());
        }
        if part.is_empty() && !chunk.eof {
            return Err("artifact read made no progress".to_owned());
        }
        bytes.extend(part);
        if chunk.eof {
            if bytes.len() as u64 != chunk.total_size
                || format!("sha256:{:x}", Sha256::digest(&bytes)) != chunk.checksum
            {
                return Err("artifact integrity check failed".to_owned());
            }
            return String::from_utf8(bytes)
                .map_err(|_| "saved content is not UTF-8 text".to_owned());
        }
    }
}

// artifact 已脱敏；显示时还须去掉 ANSI/OSC，避免日志控制终端或剪贴板。
pub(crate) fn terminal_text(text: &str) -> String {
    let mut result = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') | Some('P') | Some('_') | Some('^') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.peek() == Some(&'\\')) {
                            if c == '\u{1b}' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if ch == '\t' {
            result.push_str("    ");
        } else if ch == '\n' || !ch.is_control() {
            result.push(ch);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use golutra_agent_protocol::ArtifactChunk;

    fn event(sequence_no: u64, event_type: RuntimeEventType, payload: Value) -> RuntimeEvent {
        RuntimeEvent {
            schema_version: golutra_agent_core::RUNTIME_EVENT_SCHEMA_VERSION,
            id: EventId::new(),
            sequence_no,
            session_id: SessionId::new(),
            task_id: None,
            turn_id: None,
            parent_event_id: None,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            event_type,
            timestamp: chrono::Utc::now(),
            source: golutra_agent_protocol::RuntimeEventSource::Tool,
            payload,
            payload_ref: None,
            durable: true,
        }
    }

    #[test]
    fn final_process_artifact_wins_over_a_late_initial_result() {
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready".to_owned(),
            None,
        );
        let call = golutra_agent_core::ToolCallId::new();
        let complete = ArtifactId::new();
        app.events = vec![
            event(
                1,
                RuntimeEventType::ProcessUpdated,
                json!({"process_id":format!("proc-{call}"),"terminal":true,"process_state":"exited","output_artifact_ref":complete,"command":"long job"}),
            ),
            event(
                2,
                RuntimeEventType::ToolCompleted,
                json!({"envelope":{"tool_call_id":call,"tool_name":"shell","status":"ok","structured_facts":{"command":"long job"},"raw_artifact_ref":ArtifactId::new()}}),
            ),
        ];
        let id = history_event_operations(&app)[0]
            .projection
            .id()
            .unwrap()
            .clone();
        let result = sources(&app, &id);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].artifact_id, complete);
    }

    #[test]
    fn summarized_batch_keeps_each_original_artifact() {
        use golutra_agent_core::{
            ToolCallId, ToolResultStatus, UserStep, UserStepId, UserStepKind, UserStepTool,
        };
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready".to_owned(),
            None,
        );
        let calls = [ToolCallId::new(), ToolCallId::new()];
        let artifacts = [ArtifactId::new(), ArtifactId::new()];
        let turn = TurnId::new();
        for index in 0..2 {
            let mut output = event(
                index as u64 + 1,
                RuntimeEventType::ToolCompleted,
                json!({"envelope":{"tool_call_id":calls[index],"tool_name":"read_file","status":"ok","structured_facts":{"path":format!("{index}.txt")},"raw_artifact_ref":artifacts[index]}}),
            );
            output.turn_id = Some(turn);
            app.events.push(output);
        }
        let mut summary = event(
            3,
            RuntimeEventType::UserStep,
            json!({"step":UserStep { step_id: UserStepId::new(), turn_id:turn,
            kind: UserStepKind::ToolBatch { summary: "read 2 files".to_owned(), tools: calls.iter().map(|call| UserStepTool { tool_call_id:*call, tool_name:"read_file".to_owned(),status:ToolResultStatus::Ok,object:None }).collect() } }}),
        );
        summary.turn_id = Some(turn);
        app.events.push(summary);
        let entries = history_event_operations(&app);
        assert_eq!(entries.len(), 1);
        let found = sources(&app, entries[0].projection.id().unwrap());
        assert_eq!(
            found
                .iter()
                .map(|source| source.artifact_id)
                .collect::<Vec<_>>(),
            artifacts
        );
    }

    fn chunk(id: ArtifactId, bytes: &[u8], offset: usize, end: usize) -> ArtifactChunk {
        ArtifactChunk {
            artifact_id: id,
            offset: offset as u64,
            length: (end - offset) as u64,
            total_size: bytes.len() as u64,
            checksum: format!("sha256:{:x}", Sha256::digest(bytes)),
            redaction_status: golutra_agent_core::RedactionStatus::NotRequired,
            content_base64: base64::engine::general_purpose::STANDARD.encode(&bytes[offset..end]),
            eof: end == bytes.len(),
        }
    }

    #[tokio::test]
    async fn chunked_utf8_is_verified_before_decoding() {
        let id = ArtifactId::new();
        let value = "a中🙂\n\nlast";
        let result = read_chunks(id, 100, |request| {
            let offset = request.offset as usize;
            std::future::ready(Ok(Some(chunk(
                id,
                value.as_bytes(),
                offset,
                (offset + 2).min(value.len()),
            ))))
        })
        .await
        .unwrap();
        assert_eq!(result, value);
    }

    #[tokio::test]
    async fn corrupt_missing_and_oversized_artifacts_are_explicit_errors() {
        let id = ArtifactId::new();
        let mut corrupt = chunk(id, b"original", 0, 8);
        corrupt.content_base64 = base64::engine::general_purpose::STANDARD.encode(b"tampered");
        assert!(
            read_chunks(id, 100, |_| std::future::ready(Ok(Some(corrupt.clone()))))
                .await
                .unwrap_err()
                .contains("integrity")
        );
        assert!(
            read_chunks(id, 100, |_| std::future::ready(Ok(None)))
                .await
                .unwrap_err()
                .contains("missing")
        );
        let large = chunk(id, b"too large", 0, 9);
        assert!(
            read_chunks(id, 2, |_| std::future::ready(Ok(Some(large.clone()))))
                .await
                .unwrap_err()
                .contains("budget")
        );
        let mut stalled = chunk(id, b"text", 0, 0);
        stalled.eof = false;
        assert!(
            read_chunks(id, 100, |_| std::future::ready(Ok(Some(stalled.clone()))))
                .await
                .unwrap_err()
                .contains("no progress")
        );
        let mut wrong_cursor = chunk(id, b"text", 0, 4);
        wrong_cursor.offset = 1;
        assert!(
            read_chunks(id, 100, |_| std::future::ready(Ok(Some(
                wrong_cursor.clone()
            ))))
            .await
            .unwrap_err()
            .contains("cursor")
        );
    }

    #[test]
    fn terminal_controls_cannot_set_clipboard_or_inject_escape_sequences() {
        assert_eq!(
            terminal_text("\x1b[31m中文\x1b[0m\n\n\x1b]52;c;secret\x07🙂\x1bPignored\x1b\\\tend"),
            "中文\n\n🙂    end"
        );
    }
}
