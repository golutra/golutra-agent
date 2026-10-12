//! 将后台启动与同一受管进程的后续观察归并为验证视图；原始工具报告保持不变。

use std::borrow::Cow;

use golutra_agent_core::ToolResultStatus;
use golutra_agent_tools::ToolExecutionReport;
use serde_json::Value;

/// 仅归并内置 shell 的运行中报告，绑定进程、命令和工作目录，避免其他进程覆盖结果。
/// 保留启动位置用于检查验证期间的工作区变更；成功终态不能掩盖并发修改。
pub(super) fn resolve_validation_report<'a>(
    report: &'a ToolExecutionReport,
    reports: &[ToolExecutionReport],
) -> Cow<'a, ToolExecutionReport> {
    let facts = &report.envelope.structured_facts;
    if report.envelope.tool_name != "shell"
        || report.envelope.status != ToolResultStatus::Ok
        || report.envelope.risk == "external_mcp_tool"
        || facts["process_state"] != "running"
        || !["process_id", "command", "workdir"]
            .iter()
            .all(|key| facts[*key].as_str().is_some_and(|value| !value.is_empty()))
    {
        return Cow::Borrowed(report);
    }
    let Some(index) = reports
        .iter()
        .position(|candidate| candidate.envelope.tool_call_id == report.envelope.tool_call_id)
    else {
        return Cow::Borrowed(report);
    };
    let observations = reports[index + 1..]
        .iter()
        .filter(|candidate| same_process_observation(report, candidate))
        .collect::<Vec<_>>();
    // wait/read 的最后一段可能没有测试输出，必须保留同一进程此前已经观察到的结果。
    let Some(terminal_index) = observations
        .iter()
        .position(|candidate| candidate.envelope.structured_facts["terminal"] == true)
    else {
        return Cow::Borrowed(report);
    };
    let terminal = observations[terminal_index];
    let mut resolved = report.clone();
    resolved.envelope.status = terminal.envelope.status;
    resolved
        .envelope
        .summary
        .clone_from(&terminal.envelope.summary);
    resolved
        .envelope
        .structured_facts
        .clone_from(&terminal.envelope.structured_facts);
    if let Some(metadata) = facts.get("runtime_objective_validation") {
        resolved.envelope.structured_facts["runtime_objective_validation"] = metadata.clone();
    }
    let mut output_reports = vec![report];
    for (index, observation) in observations.iter().enumerate() {
        // 首个终态决定结果；终态输出被截断时，只接受同一终态事件的后续续读。
        if index > terminal_index && !same_terminal_event(terminal, observation) {
            continue;
        }
        output_reports.push(observation);
        for evidence in &observation.envelope.evidence_refs {
            if !resolved.envelope.evidence_refs.contains(evidence) {
                resolved.envelope.evidence_refs.push(*evidence);
            }
        }
    }
    resolved.artifact_contents = super::validation_output::normalize(&output_reports);
    Cow::Owned(resolved)
}

/// 观察工具本身没有独立验证身份；将它关联回最初的命令，供恢复和进展判断共用。
pub(super) fn validation_origin<'a>(
    report: &'a ToolExecutionReport,
    reports: &'a [ToolExecutionReport],
) -> &'a ToolExecutionReport {
    if report.envelope.tool_name == "shell_session" {
        for candidate in reports {
            if candidate.envelope.tool_call_id == report.envelope.tool_call_id {
                break;
            }
            if candidate.envelope.tool_name == "shell"
                && candidate.envelope.risk != "external_mcp_tool"
                && same_process_observation(candidate, report)
            {
                return candidate;
            }
        }
    }
    report
}

fn same_terminal_event(first: &ToolExecutionReport, later: &ToolExecutionReport) -> bool {
    let facts = &first.envelope.structured_facts;
    facts["terminal_event_id"].as_u64().is_some_and(|id| id > 0)
        && first.envelope.status == later.envelope.status
        && [
            "terminal_event_id",
            "process_state",
            "exit_code",
            "terminal",
        ]
        .iter()
        .all(|key| facts[*key] == later.envelope.structured_facts[*key])
}

fn same_process_observation(start: &ToolExecutionReport, later: &ToolExecutionReport) -> bool {
    let first = &start.envelope.structured_facts;
    let next = &later.envelope.structured_facts;
    later.envelope.tool_name == "shell_session"
        && later.envelope.risk != "external_mcp_tool"
        && ["process_id", "command", "workdir"]
            .iter()
            .all(|key| first[*key].as_str().is_some_and(|value| !value.is_empty()))
        && matches!(
            next["action"].as_str(),
            Some("read" | "write" | "wait" | "terminate")
        )
        && ["process_id", "command", "workdir"]
            .iter()
            .all(|key| first[*key] == next[*key])
        && (first
            .get("authoritative_pid")
            .and_then(Value::as_u64)
            .is_none()
            || first["authoritative_pid"] == next["authoritative_pid"])
}
