//! 补齐 Responses 结束语义、完整工具参数和就绪时序；只保留有界的最终调用，不缓存原始帧。

use std::sync::Mutex;

use genai::chat::{ChatFrameSink, FrameCtx, RawFrameRef};
use serde::Deserialize;

use super::{ProviderError, ProviderFinishReason};
use crate::ProviderToolCall;

#[derive(Default)]
pub(super) struct ResponsesTerminal(Mutex<State>, crate::stream_error::StreamErrorCapture);

#[derive(Default)]
struct State {
    reason: Option<ProviderFinishReason>,
    first_tool_us: Option<u64>,
    terminal_us: Option<u64>,
    completed_tools: Option<Result<Vec<ProviderToolCall>, ProviderError>>,
    frame_count: u64,
    frame_bytes: u64,
    unparsed_frames: u64,
    output_items: usize,
    message_items: usize,
    tool_items: usize,
    reasoning_items: usize,
    end_turn: Option<bool>,
}

#[derive(Deserialize)]
struct Frame<'a> {
    #[serde(rename = "type", borrow)]
    kind: &'a str,
    #[serde(borrow)]
    response: Option<Response<'a>>,
    #[serde(borrow)]
    item: Option<Item<'a>>,
}

#[derive(Deserialize)]
struct Item<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    status: Option<&'a str>,
}

#[derive(Deserialize)]
struct Response<'a> {
    status: Option<&'a str>,
    end_turn: Option<bool>,
    #[serde(borrow)]
    incomplete_details: Option<Incomplete<'a>>,
    #[serde(default)]
    output: Vec<OutputItem>,
}

#[derive(Deserialize)]
struct OutputItem {
    #[serde(rename = "type")]
    kind: String,
    call_id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
}

fn completed_tool_calls(output: Vec<OutputItem>) -> Result<Vec<ProviderToolCall>, ProviderError> {
    let mut ids = std::collections::HashSet::new();
    output
        .into_iter()
        .filter(|item| item.kind == "function_call")
        .map(|item| {
            let call = crate::provider_tool_call_from_openai(&serde_json::json!({
                "id": item.call_id, "function": {"name": item.name, "arguments": item.arguments}
            }))?;
            if !ids.insert(call.tool_call_id.clone()) {
                return Err(ProviderError::Malformed {
                    message: "responses completed output contains duplicate tool call ids".into(),
                });
            }
            Ok(call)
        })
        .collect()
}

#[derive(Deserialize)]
struct Incomplete<'a> {
    reason: Option<&'a str>,
}

impl ChatFrameSink for ResponsesTerminal {
    fn on_frame(&self, _: &FrameCtx, frame: RawFrameRef<'_>) {
        let elapsed_us = frame.elapsed_us;
        if let Ok(mut state) = self.0.lock() {
            state.frame_count = state.frame_count.saturating_add(1);
            state.frame_bytes = state.frame_bytes.saturating_add(frame.data.len() as u64);
        }
        self.1.capture(frame.data);
        if frame.data.len() > crate::MAX_PROVIDER_RESPONSE_BYTES {
            if let Ok(mut state) = self.0.lock() {
                state.completed_tools = Some(Err(ProviderError::Malformed {
                    message: "responses frame exceeds provider response byte limit".into(),
                }));
            }
            return;
        }
        // 仅反序列化必要字段；reasoning/text 正文仍跳过，完整调用只在成功终帧交付。
        let Ok(frame) = serde_json::from_str::<Frame<'_>>(frame.data) else {
            if let Ok(mut state) = self.0.lock() {
                state.unparsed_frames = state.unparsed_frames.saturating_add(1);
            }
            return;
        };
        if frame.kind == "response.output_item.done"
            && frame.item.is_some_and(|item| {
                item.kind == "function_call" && item.status == Some("completed")
            })
            && let Ok(mut state) = self.0.lock()
        {
            state.first_tool_us.get_or_insert(elapsed_us);
        }
        let reason = match (frame.kind, frame.response) {
            ("response.completed", Some(response)) if response.status == Some("completed") => {
                if let Ok(mut state) = self.0.lock() {
                    state.end_turn = response.end_turn;
                    state.output_items = response.output.len();
                    state.message_items = response
                        .output
                        .iter()
                        .filter(|i| i.kind == "message")
                        .count();
                    state.tool_items = response
                        .output
                        .iter()
                        .filter(|i| i.kind == "function_call")
                        .count();
                    state.reasoning_items = response
                        .output
                        .iter()
                        .filter(|i| i.kind == "reasoning")
                        .count();
                }
                if !response.output.is_empty()
                    && let Ok(mut state) = self.0.lock()
                {
                    let calls = completed_tool_calls(response.output);
                    if !matches!(&state.completed_tools, Some(Err(_))) {
                        state.completed_tools = Some(calls);
                    }
                }
                if response.end_turn == Some(false) {
                    ProviderFinishReason::Continue
                } else {
                    ProviderFinishReason::Stop
                }
            }
            ("response.incomplete", Some(response)) if response.status == Some("incomplete") => {
                match response
                    .incomplete_details
                    .and_then(|details| details.reason)
                {
                    Some("max_output_tokens") => ProviderFinishReason::Length,
                    Some("content_filter") => ProviderFinishReason::ContentFilter,
                    _ => ProviderFinishReason::Error,
                }
            }
            ("response.failed" | "response.completed" | "response.incomplete", _) => {
                ProviderFinishReason::Error
            }
            _ => return,
        };
        if let Ok(mut terminal) = self.0.lock() {
            terminal.reason = Some(reason);
            terminal.terminal_us = Some(elapsed_us);
        }
    }
}

impl ResponsesTerminal {
    /// 只保留固定大小的边界统计，不捕获正文、工具参数、认证信息或原始 SSE。
    pub(super) fn diagnostics(&self) -> serde_json::Value {
        let Ok(state) = self.0.lock() else {
            return serde_json::Value::Null;
        };
        serde_json::json!({
            "frame_count": state.frame_count, "frame_bytes": state.frame_bytes,
            "unparsed_frames": state.unparsed_frames, "terminal_seen": state.reason.is_some(),
            "finish_reason": state.reason, "end_turn": state.end_turn,
            "output_items": state.output_items, "message_items": state.message_items,
            "tool_items": state.tool_items, "reasoning_items": state.reasoning_items,
        })
    }
    pub(super) fn error(&self) -> Option<ProviderError> {
        self.1.error().or_else(|| {
            self.0.lock().ok().and_then(|state| {
                state
                    .completed_tools
                    .as_ref()
                    .and_then(|result| result.as_ref().err().cloned())
            })
        })
    }

    pub(super) fn take_completed_tools(&self) -> Option<Vec<ProviderToolCall>> {
        self.0.lock().ok()?.completed_tools.take()?.ok()
    }

    pub(super) fn finish_reason(&self) -> Result<ProviderFinishReason, ProviderError> {
        self.0
            .lock()
            .ok()
            .and_then(|state| state.reason)
            .ok_or_else(|| {
                ProviderError::Unavailable {
                    message: "responses stream ended without a valid terminal event".into(),
                }
                .with_stream_interrupted()
            })
    }

    pub(super) fn tool_ready_to_terminal_ms(&self) -> Option<u64> {
        let state = self.0.lock().ok()?;
        Some(state.terminal_us?.checked_sub(state.first_tool_us?)? / 1_000)
    }
}
