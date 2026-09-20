//! 只投影当前执行已确认的事实；原始日志与完整错误留在审计层，不重复注入模型上下文。

use super::*;

#[derive(Default)]
pub(super) struct RunObservation {
    completed_provider_requests: u64,
    known_input_tokens: u64,
    known_output_tokens: u64,
    usage_complete: bool,
}

impl RunObservation {
    pub(super) fn observe(&mut self, record: &golutra_agent_core::TokenUsageRecord) {
        let complete = record.input_tokens.is_some()
            && record.output_tokens.is_some()
            && record.usage_source == "provider";
        self.usage_complete =
            complete && (self.completed_provider_requests == 0 || self.usage_complete);
        self.completed_provider_requests = self.completed_provider_requests.saturating_add(1);
        if record.usage_source == "provider" {
            self.known_input_tokens = self
                .known_input_tokens
                .saturating_add(record.input_tokens.unwrap_or(0));
            self.known_output_tokens = self
                .known_output_tokens
                .saturating_add(record.output_tokens.unwrap_or(0));
        }
    }

    pub(super) fn snapshot(
        &self,
        task_id: TaskId,
        elapsed_ms: u64,
        reports: &[ToolExecutionReport],
        attempts: &[ToolAttemptMetadata],
    ) -> Value {
        let unresolved = reports
            .iter()
            .rev()
            .filter(|report| !tool_execution_check_status(report, attempts).0)
            .take(8)
            .map(|report| {
                json!({
                    "tool":report.envelope.tool_name, "status":report.envelope.status,
                    "tool_call_id":report.envelope.tool_call_id,
                    "error_kind":report.envelope.structured_facts.get("error_kind"),
                    "artifact_ref":report.envelope.raw_artifact_ref,
                })
            })
            .collect::<Vec<_>>();
        let validations = reports
            .iter()
            .rev()
            .filter_map(|report| {
                let check = objective_validation_report(report)?;
                let current = validation_is_current(report, reports);
                Some(json!({"kind":check.kind.label(), "passed":check.passed,
                "current":current, "tool_call_id":report.envelope.tool_call_id}))
            })
            .take(8)
            .collect::<Vec<_>>();
        json!({"task_id":task_id, "elapsed_ms":elapsed_ms,
            "completed_provider_requests":self.completed_provider_requests,
            "known_input_tokens":self.known_input_tokens, "known_output_tokens":self.known_output_tokens,
            "usage_complete":self.usage_complete, "tool_results":reports.len(),
            "recent_unresolved_tool_failures":unresolved, "recent_validations":validations,
            "window_limit":8, "scope":"this_execution_only; resumed history remains in trace"})
    }
}
