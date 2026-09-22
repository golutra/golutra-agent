//! 只阻止运行时重复发出没有新验收事实的纠偏，不限制模型正常执行的时长或轮数。

use std::collections::BTreeMap;

use golutra_agent_core::{CorrectionEnvelope, VerificationCheckKind, VerificationRecord};
use golutra_agent_tools::ToolExecutionReport;
use sha2::{Digest, Sha256};

#[derive(Default)]
pub(crate) struct CorrectionProgress {
    last_fingerprint: Option<String>,
}

impl CorrectionProgress {
    pub(crate) fn permits_retry(
        &mut self,
        correction: &CorrectionEnvelope,
        verification: &VerificationRecord,
        reports: &[ToolExecutionReport],
    ) -> bool {
        // 事件 ID、耗时、轮次不代表验收进展；相同检查只取最新结果。
        let mut checks = BTreeMap::new();
        for check in &verification.checks {
            if !matches!(
                check.kind,
                VerificationCheckKind::ToolExecution
                    | VerificationCheckKind::AssistantResponse
                    | VerificationCheckKind::WorkspaceChange
            ) {
                checks.insert(
                    (&check.name, &check.command),
                    (check.passed, &check.message),
                );
            }
        }
        let mut candidate = BTreeMap::new();
        if verification
            .checks
            .iter()
            .any(|c| c.kind == VerificationCheckKind::ObjectiveValidation)
        {
            // 实际验收失败后，修改候选允许重新验证；缺少验收本身时，改文件不能消除缺失。
            // 使用最终文件内容而非写入次数，避免反复改坏再还原制造虚假进展。
            for report in reports {
                for image in &report.after_images {
                    let checksum = image
                        .metadata
                        .as_ref()
                        .and_then(|m| m.checksum.clone())
                        .or_else(|| {
                            image
                                .content
                                .as_ref()
                                .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
                        });
                    candidate.insert(&image.path, (checksum, image.unix_mode));
                }
            }
        }
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(
                    &correction.failed_requirements,
                    checks.into_iter().collect::<Vec<_>>(),
                    candidate
                ))
                .expect("verification fingerprint is serializable"),
            )
        );
        if self.last_fingerprint.as_ref() == Some(&fingerprint) {
            return false;
        }
        self.last_fingerprint = Some(fingerprint);
        true
    }
}
