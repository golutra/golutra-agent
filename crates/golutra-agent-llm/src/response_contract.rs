//! 统一适配器的结束状态与工具完整性契约；续写、重试和任务预算仍由运行时决定。

use genai::chat::StopReason;

use crate::{ProviderError, ProviderFinishReason, ProviderResponse};

/// 完整响应中的参数错误保留为原始值，由执行前 schema 校验生成可纠正工具结果。
/// 此处不修补 JSON；断流、未知终态和未捕获调用仍由各自的完整性边界拒绝。
pub(crate) fn parse_tool_arguments(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_owned()))
}

/// 依赖库可能在未闭合工具块时丢弃调用；不能把剩余文本误当纯文本完成或续写。
pub(crate) fn ensure_tools_captured<'a>(
    response: &ProviderResponse,
    observed_ids: impl Iterator<Item = &'a str>,
) -> Result<(), ProviderError> {
    if observed_ids.filter(|id| !id.is_empty()).any(|id| {
        !response
            .tool_calls
            .iter()
            .any(|call| call.tool_call_id == id)
    }) {
        return Err(ProviderError::Malformed {
            message: "provider stream ended with uncaptured tool calls; tools were not executed"
                .into(),
        });
    }
    Ok(())
}

pub(crate) fn from_openai(value: &str) -> ProviderFinishReason {
    match value {
        "stop" => ProviderFinishReason::Stop,
        "tool_calls" | "function_call" => ProviderFinishReason::ToolCalls,
        "length" => ProviderFinishReason::Length,
        "content_filter" => ProviderFinishReason::ContentFilter,
        _ => ProviderFinishReason::Unknown,
    }
}

pub(crate) fn from_genai(reason: Option<&StopReason>) -> ProviderFinishReason {
    match reason {
        Some(StopReason::Completed(_) | StopReason::StopSequence(_)) => ProviderFinishReason::Stop,
        // incomplete 本身不说明截断原因；Responses 必须再读取 incomplete_details。
        Some(StopReason::MaxTokens(reason)) if reason == "incomplete" => {
            ProviderFinishReason::Error
        }
        Some(StopReason::MaxTokens(_)) => ProviderFinishReason::Length,
        Some(StopReason::ToolCall(_)) => ProviderFinishReason::ToolCalls,
        Some(StopReason::ContentFilter(_)) => ProviderFinishReason::ContentFilter,
        Some(StopReason::Other(reason)) => match reason.as_str() {
            "pause_turn" => ProviderFinishReason::Continue,
            "refusal" => ProviderFinishReason::ContentFilter,
            "failed"
            | "cancelled"
            | "ERROR"
            | "MALFORMED_FUNCTION_CALL"
            | "UNEXPECTED_TOOL_CALL" => ProviderFinishReason::Error,
            _ => ProviderFinishReason::Unknown,
        },
        None => ProviderFinishReason::Unknown,
    }
}

impl ProviderFinishReason {
    /// Gemini 等协议允许 STOP 携带完整工具调用；失败或缺失状态不能因有工具而升级成功。
    pub(crate) fn with_tool_calls(self, has_tools: bool) -> Self {
        if has_tools && matches!(self, Self::Stop | Self::Continue) {
            Self::ToolCalls
        } else {
            self
        }
    }
}
