//! 按 provider 的结束语义决定能否完成、续写或执行工具，不把截断误判为成功。

use golutra_agent_llm::{ProviderError, ProviderFinishReason, ProviderResponse};

// 仅约束连续没有工具动作的协议续写；防止异常上游用不同文本耗尽整个任务预算。
const MAX_TEXT_CONTINUATIONS: u32 = 8;

pub(crate) const LENGTH_CONTINUATION_PROMPT: &str = concat!(
    "The response reached its output limit. Continue from the retained output ",
    "toward the same objective. Do not repeat completed work or tool calls."
);

#[derive(Default)]
pub(crate) struct ResponseControl {
    text_continuations: u32,
}

impl ResponseControl {
    /// 返回 true 表示保留本次文本后继续请求；完整工具调用会重置连续文本计数。
    pub(crate) fn observe(&mut self, response: &ProviderResponse) -> Result<bool, ProviderError> {
        let reason = response.finish_reason;
        match reason {
            ProviderFinishReason::Stop | ProviderFinishReason::ToolCalls => {
                if reason == ProviderFinishReason::ToolCalls && response.tool_calls.is_empty() {
                    return Err(invalid("provider reported tool calls but returned none"));
                }
                self.text_continuations = 0;
                Ok(false)
            }
            ProviderFinishReason::Continue if !response.tool_calls.is_empty() => {
                self.text_continuations = 0;
                Ok(false)
            }
            ProviderFinishReason::Continue | ProviderFinishReason::Length => {
                if !response.tool_calls.is_empty() {
                    return Err(invalid(
                        "provider truncated a response containing tool calls; tools were not executed",
                    ));
                }
                self.text_continuations += 1;
                if self.text_continuations > MAX_TEXT_CONTINUATIONS {
                    return Err(invalid(
                        "provider did not finish after 8 consecutive text continuations",
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
}

fn invalid(message: &str) -> ProviderError {
    ProviderError::Malformed {
        message: message.into(),
    }
}
