//! 只阻止运行时重复发出没有新验收事实的纠偏，不限制模型正常执行的时长或轮数。

use std::collections::{BTreeMap, VecDeque};

use golutra_agent_core::{CorrectionEnvelope, VerificationCheckKind, VerificationRecord};
use golutra_agent_tools::ToolExecutionReport;
use sha2::{Digest, Sha256};

#[derive(Default)]
pub(crate) struct CorrectionProgress {
    /// 保留短历史，既能阻止相邻重复，也能阻止 A→B→A 循环；有界长度避免
    /// 长任务无限保留候选指纹。
    fingerprints: VecDeque<String>,
}

impl CorrectionProgress {
    pub(crate) fn permits_retry(
        &mut self,
        correction: &CorrectionEnvelope,
        verification: &VerificationRecord,
        reports: &[ToolExecutionReport],
        assistant_message: Option<&str>,
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
        // schema/纯回答纠偏中，回答变化是有效进展；交付或验证失败时，
        // 只改写措辞并不会改变候选，不能据此无限延长循环。
        let answer_fingerprint = (!verification
            .checks
            .iter()
            .any(|check| check.kind == VerificationCheckKind::ObjectiveValidation)
            || verification
                .checks
                .iter()
                .any(|check| check.kind == VerificationCheckKind::Schema && !check.passed))
        .then(|| {
            assistant_message
                .map(str::trim)
                .filter(|message| !message.is_empty())
        });
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(
                    &correction.failed_requirements,
                    checks.into_iter().collect::<Vec<_>>(),
                    candidate,
                    answer_fingerprint,
                ))
                .expect("verification fingerprint is serializable"),
            )
        );
        if self.fingerprints.contains(&fingerprint) {
            return false;
        }
        const MAX_FINGERPRINT_HISTORY: usize = 8;
        self.fingerprints.push_back(fingerprint);
        if self.fingerprints.len() > MAX_FINGERPRINT_HISTORY {
            self.fingerprints.pop_front();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correction_history_stops_a_b_a_cycles() {
        let mut progress = CorrectionProgress::default();
        let correction = CorrectionEnvelope {
            verification_id: golutra_agent_core::VerificationId::new(),
            attempt: 1,
            remaining_attempts: None,
            failed_requirements: vec!["validation".into()],
            evidence_refs: Vec::new(),
            requested_action: "supply evidence".into(),
        };
        let verification = golutra_agent_core::VerificationRecord {
            verification_id: correction.verification_id,
            task_id: golutra_agent_core::TaskId::new(),
            objective: "change".into(),
            completion_criteria: Vec::new(),
            checks: Vec::new(),
            evidence_refs: Vec::new(),
            result: golutra_agent_core::VerificationResult::Partial,
            policy_status: "task_contract_satisfied".into(),
            residual_risks: vec!["missing validation".into()],
            plan_id: None,
            assertions: Vec::new(),
            source: Default::default(),
            independence: Default::default(),
            environment_digest: None,
        };
        assert!(progress.permits_retry(&correction, &verification, &[], Some("A")));
        assert!(progress.permits_retry(&correction, &verification, &[], Some("B")));
        assert!(!progress.permits_retry(&correction, &verification, &[], Some("A")));
    }

    #[test]
    fn changed_final_answer_counts_as_correction_progress() {
        let mut progress = CorrectionProgress::default();
        let correction = CorrectionEnvelope {
            verification_id: golutra_agent_core::VerificationId::new(),
            attempt: 1,
            remaining_attempts: None,
            failed_requirements: vec!["schema".into()],
            evidence_refs: Vec::new(),
            requested_action: "correct response".into(),
        };
        let verification = golutra_agent_core::VerificationRecord {
            verification_id: correction.verification_id,
            task_id: golutra_agent_core::TaskId::new(),
            objective: "answer".into(),
            completion_criteria: Vec::new(),
            checks: Vec::new(),
            evidence_refs: Vec::new(),
            result: golutra_agent_core::VerificationResult::Partial,
            policy_status: "task_contract_satisfied".into(),
            residual_risks: Vec::new(),
            plan_id: None,
            assertions: Vec::new(),
            source: Default::default(),
            independence: Default::default(),
            environment_digest: None,
        };
        assert!(progress.permits_retry(&correction, &verification, &[], Some("first")));
        assert!(progress.permits_retry(&correction, &verification, &[], Some("second")));
    }
}
