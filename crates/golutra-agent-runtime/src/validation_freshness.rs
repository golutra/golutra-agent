//! 判断既有验证是否仍适用于工作区；消费可信工具的变动证据，不从命令名猜测副作用。

use std::collections::HashSet;

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
        if direct_file_check_is_unaffected(report, later) {
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

/// 直接文件状态检查有明确且窄的依赖集合；无关文件变化不使它过期。
/// 通用测试和构建的传递依赖未知，继续使用保守策略。
fn direct_file_check_is_unaffected(
    validation: &ToolExecutionReport,
    later: &ToolExecutionReport,
) -> bool {
    let Some(paths) = direct_file_check_paths(validation) else {
        return false;
    };
    !later.changed_files.iter().any(|path| {
        let normalized = path.to_string_lossy().replace('\\', "/");
        !is_documentation_only_file(path)
            && paths.iter().any(|scope| {
                normalized == *scope
                    || normalized.starts_with(&format!("{scope}/"))
                    || scope.starts_with(&format!("{normalized}/"))
            })
    })
}

fn direct_file_check_paths(report: &ToolExecutionReport) -> Option<HashSet<String>> {
    let command = report.envelope.structured_facts.get("command")?.as_str()?;
    let parts = command.split_whitespace().collect::<Vec<_>>();
    let mut paths = HashSet::new();
    match parts.first().copied() {
        Some("test") => {
            let path = parts.last()?.trim_matches(['\'', '"']);
            if path != "test" && !path.starts_with('-') {
                paths.insert(path.replace('\\', "/"));
            }
        }
        Some("cmp") | Some("diff") if parts.len() >= 3 => {
            for path in parts.iter().rev().take(2) {
                let path = path.trim_matches(['\'', '"']);
                if !path.starts_with('-') {
                    paths.insert(path.replace('\\', "/"));
                }
            }
        }
        _ => return None,
    }
    (!paths.is_empty()).then_some(paths)
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
