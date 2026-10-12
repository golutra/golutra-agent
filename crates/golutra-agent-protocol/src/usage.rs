//! 从持久事件识别未报告用量的尝试，避免把重试成功后的单次用量当作会话全量。

use std::collections::HashSet;

use golutra_agent_core::{ProviderRequestId, TokenUsageRecord};

use crate::{RuntimeEvent, RuntimeEventType};

/// 请求按逻辑身份去重；恢复事件表示此前至少一次尝试的消耗未知。
/// 不从请求次数推算 token，也不把等待与重试两个阶段算成两次失败。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProviderUsageCoverage {
    pub logical_requests: usize,
    pub has_unreported_attempts: bool,
}

impl ProviderUsageCoverage {
    #[must_use]
    pub fn from_events(events: &[RuntimeEvent]) -> Self {
        let mut requests = HashSet::new();
        let mut reported = HashSet::new();
        let mut incomplete = false;
        for event in events {
            match event.event_type {
                RuntimeEventType::ProviderStarted
                | RuntimeEventType::ProviderCompleted
                | RuntimeEventType::ProviderFailed => {
                    if let Some(id) = event
                        .payload
                        .get("provider_request_id")
                        .and_then(|value| value.as_str())
                        .and_then(|value| value.parse::<ProviderRequestId>().ok())
                    {
                        requests.insert(id);
                    } else {
                        incomplete = true;
                    }
                }
                RuntimeEventType::RetryScheduled if event.payload.get("recovery").is_some() => {
                    incomplete = true;
                }
                RuntimeEventType::TokenUsageRecorded => {
                    if let Some(record) = event.payload.get("record").and_then(|value| {
                        serde_json::from_value::<TokenUsageRecord>(value.clone()).ok()
                    }) {
                        // 事件窗口可能只含最终记录；输入输出都有值而完整性仍为 false，
                        // 说明这些数字不能覆盖全部尝试，不能依赖 recovery 恰好在窗口内。
                        incomplete |= !record.usage_complete
                            && record.input_tokens.is_some()
                            && record.output_tokens.is_some();
                        reported.insert(record.request_event_id);
                    }
                }
                _ => {}
            }
        }
        incomplete |= !requests.is_subset(&reported);
        requests.extend(reported);
        Self {
            logical_requests: requests.len(),
            has_unreported_attempts: incomplete,
        }
    }
}
