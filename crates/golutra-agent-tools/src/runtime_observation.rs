//! 当前会话的轻量运行查询；复用进程监督器和委派后端，不读取数据库原文或接管 lease。

use super::*;

impl ToolRuntime {
    pub(super) async fn runtime_status(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        summary: Option<Value>,
    ) -> Result<ToolExecutionReport, ToolError> {
        let processes = self.process_supervisor.list(request.session_id).await;
        let running = processes
            .iter()
            .filter(|process| !process.state.is_terminal())
            .count();
        let visible = processes
            .iter()
            .filter(|process| !process.state.is_terminal())
            .take(8)
            .map(|process| {
                json!({"process_id":process.process_id, "state":"running",
                "authoritative_pid":process.authoritative_pid, "output_bytes":process.output_bytes})
            })
            .collect::<Vec<_>>();
        let children = match &self.delegation_backend {
            Some(backend) => backend.status(request.session_id).await?,
            None => json!({"available": false}),
        };
        let facts = json!({
            "scope":"current_runtime_execution", "session_id":request.session_id,
            "runtime": summary.unwrap_or_else(|| json!({"available":false})),
            "running_process_count":running, "processes":visible,
            "processes_truncated":running > 8, "children":children,
        });
        // 原始请求、命令、stdout 与凭据不属于该查询；再用统一结果预算限制输出。
        let facts = serde_json::from_str(&redact_sensitive_text(&facts.to_string()).0)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        Ok(success_report(
            request,
            "current runtime status",
            facts,
            String::new(),
            Vec::new(),
            policy,
        ))
    }
}
