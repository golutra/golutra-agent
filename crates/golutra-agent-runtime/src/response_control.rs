//! 按 provider 的结束语义决定能否完成、续写或执行工具，不把截断误判为成功。

use golutra_agent_llm::{ProviderError, ProviderFinishReason, ProviderResponse};

pub(crate) const LENGTH_CONTINUATION_PROMPT: &str = concat!(
    "The response reached its output limit. Continue from the retained output ",
    "toward the same objective. Do not repeat completed work or tool calls."
);

// 无正文且无工具的显式续写允许短暂推理，但零输出服务不能在无限任务预算下永久循环。
const MAX_SILENT_CONTINUATIONS: u32 = 8;

#[derive(Default)]
pub(crate) struct ResponseControl {
    empty_finals: u32,
    silent_continuations: u32,
}

impl ResponseControl {
    /// 返回 true 表示保留本次文本后继续请求；续写次数不决定任务是否完成。
    pub(crate) fn observe(&mut self, response: &ProviderResponse) -> Result<bool, ProviderError> {
        let reason = response.finish_reason;
        let has_content = !response.tool_calls.is_empty()
            || response
                .message
                .as_ref()
                .is_some_and(|m| !m.content.trim().is_empty());
        let continuation = matches!(
            reason,
            ProviderFinishReason::Continue | ProviderFinishReason::Length
        );
        if has_content || continuation {
            self.empty_finals = 0;
        }
        if continuation && !has_content {
            self.silent_continuations = self.silent_continuations.saturating_add(1);
            if self.silent_continuations > MAX_SILENT_CONTINUATIONS {
                return Err(invalid(
                    "provider continuation repeatedly produced no visible text or tool calls",
                ));
            }
        } else {
            self.silent_continuations = 0;
        }
        match reason {
            ProviderFinishReason::Stop | ProviderFinishReason::ToolCalls => {
                if reason == ProviderFinishReason::ToolCalls && response.tool_calls.is_empty() {
                    return Err(invalid("provider reported tool calls but returned none"));
                }
                Ok(false)
            }
            ProviderFinishReason::Continue if !response.tool_calls.is_empty() => Ok(false),
            ProviderFinishReason::Continue | ProviderFinishReason::Length => {
                if !response.tool_calls.is_empty() {
                    return Err(invalid(
                        "provider truncated a response containing tool calls; tools were not executed",
                    ));
                }
                Ok(true)
            }
            ProviderFinishReason::ContentFilter => Err(invalid(
                "provider stopped because of content_filter; response is not complete",
            )),
            ProviderFinishReason::Error => Err(invalid(
                "provider reported an unsuccessful terminal response",
            )),
            ProviderFinishReason::Unknown => Err(invalid(
                "provider returned an unknown finish reason; completion cannot be confirmed",
            )),
        }
    }

    /// 只有没有工具、正文和继续语义的 Final 才消耗空回复重试；作用域属于当前轮。
    pub(crate) fn record_empty_final(&mut self) -> u32 {
        self.empty_finals = self.empty_finals.saturating_add(1);
        self.empty_finals
    }
}

fn invalid(message: &str) -> ProviderError {
    ProviderError::Malformed {
        message: message.into(),
    }
}
