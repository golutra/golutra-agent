//! 补齐通用适配器未保留的 Responses 结束语义和工具就绪时序；仅保存固定大小状态，不缓存原始帧。

use std::sync::Mutex;

use genai::chat::{ChatFrameSink, FrameCtx, RawFrameRef};
use serde::Deserialize;

use super::{ProviderError, ProviderFinishReason};

#[derive(Default)]
pub(super) struct ResponsesTerminal(Mutex<State>);

#[derive(Default)]
struct State {
    reason: Option<ProviderFinishReason>,
    first_tool_us: Option<u64>,
    terminal_us: Option<u64>,
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
}

#[derive(Deserialize)]
struct Incomplete<'a> {
    reason: Option<&'a str>,
}

impl ChatFrameSink for ResponsesTerminal {
    fn on_frame(&self, _: &FrameCtx, frame: RawFrameRef<'_>) {
        let elapsed_us = frame.elapsed_us;
        // 仅反序列化必要字段；output 等大字段由 serde 跳过，避免长任务重复留存正文。
        let Ok(frame) = serde_json::from_str::<Frame<'_>>(frame.data) else {
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
    pub(super) fn finish_reason(&self) -> Result<ProviderFinishReason, ProviderError> {
        self.0
            .lock()
            .ok()
            .and_then(|state| state.reason)
            .ok_or_else(|| ProviderError::Unavailable {
                message: "responses stream ended without a valid terminal event".into(),
            })
    }

    pub(super) fn tool_ready_to_terminal_ms(&self) -> Option<u64> {
        let state = self.0.lock().ok()?;
        Some(state.terminal_us?.checked_sub(state.first_tool_us?)? / 1_000)
    }
}
