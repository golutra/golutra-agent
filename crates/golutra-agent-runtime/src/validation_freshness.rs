//! 判断既有验证是否仍适用于工作区；消费可信工具的变动证据，不从命令名猜测副作用。

use golutra_agent_core::ToolResultStatus;
use golutra_agent_tools::ToolExecutionReport;

use super::is_documentation_only_file;

/// 行为输入修改或无法确认的副作用要求复验；已完成的读取仍保留为历史观察。
pub(super) fn validation_is_current(
    report: &ToolExecutionReport,
    reports: &[ToolExecutionReport],
) -> bool {
    if matches!(report.envelope.tool_name.as_str(), "read_file" | "list_dir") {
        return true;
    }
    let Some(index) = reports
        .iter()
        .position(|candidate| candidate.envelope.tool_call_id == report.envelope.tool_call_id)
    else {
        return false;
    };
    !reports[index + 1..].iter().any(|later| {
        if only_proven_derived_outputs_changed(later) {
            return false;
        }
        later
            .changed_files
            .iter()
            .any(|path| !is_documentation_only_file(path))
            || (later.envelope.structured_facts["workspace_mutation_detected"] == true
                && later.envelope.structured_facts["workspace_changes_known"] != true)
    })
}

fn only_proven_derived_outputs_changed(report: &ToolExecutionReport) -> bool {
    // 只有内置进程扫描能给出这个事实；文件写入、外部工具、失败及并发不完整扫描不豁免。
    matches!(
        report.envelope.tool_name.as_str(),
        "shell" | "shell_session"
    ) && report.envelope.risk != "external_mcp_tool"
        && report.envelope.status == ToolResultStatus::Ok
        && report.envelope.structured_facts["workspace_changes_known"] == true
        && report.envelope.structured_facts["workspace_only_derived_changes"] == true
        && !report.changed_files.is_empty()
}
