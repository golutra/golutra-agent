//! 无进展门控只阻止相同操作在同一已知环境下反复失败，不封禁整个工具或策略家族。

use golutra_agent_core::ToolResultStatus;
use std::collections::HashMap;

#[derive(Debug, Default)]
pub(super) struct FailureFamilyLedger {
    failures: HashMap<(String, String), u32>,
}

impl FailureFamilyLedger {
    pub(super) fn failures(&self, family: &str, signature: &str) -> u32 {
        self.failures
            .get(&(family.into(), signature.into()))
            .copied()
            .unwrap_or_default()
    }

    pub(super) fn observe(&mut self, family: &str, signature: &str, status: ToolResultStatus) {
        let key = (family.into(), signature.into());
        if status == ToolResultStatus::Ok {
            self.failures.remove(&key);
        } else {
            let count = self.failures.entry(key).or_default();
            *count = count.saturating_add(1);
        }
    }

    pub(super) fn workspace_changed(&mut self) {
        // 有确认的状态变化后允许复验；取消、未知效果及单纯读取不会解除门控。
        self.failures.clear();
    }
}
