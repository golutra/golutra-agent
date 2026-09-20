//! shell 的动作边界：软等待由执行器限幅，目标与副作用字段在准入前严格校验。

use serde_json::Value;

use crate::{ToolError, shell_command_for_request};

pub(super) fn validate_action_arguments(tool: &str, arguments: &Value) -> Result<(), ToolError> {
    if tool == "shell" {
        shell_command_for_request(arguments)?;
    }
    if tool != "shell_session" {
        return Ok(());
    }
    let action = arguments["action"].as_str().unwrap_or_default();
    if action != "list" {
        if arguments["process_id"]
            .as_str()
            .is_none_or(|id| id.trim().is_empty())
        {
            return Err(ToolError::InvalidArguments(
                "shell_session requires a non-empty process_id except for list".into(),
            ));
        }
        if arguments
            .get("authoritative_pid")
            .is_some_and(|pid| pid.as_u64() == Some(0))
        {
            return Err(ToolError::InvalidArguments(
                "authoritative_pid must be positive when controlling a process".into(),
            ));
        }
    }
    if action == "write" && arguments.get("input").is_none() {
        return Err(ToolError::InvalidArguments("write requires input".into()));
    }
    let allowed = match action {
        "list" => &["action", "offset", "limit"][..],
        "wait" | "read" => &[
            "action",
            "process_id",
            "authoritative_pid",
            "cursor",
            "max_output_bytes",
            "wait_ms",
            "wait_for_terminal",
        ],
        "terminate" => &[
            "action",
            "process_id",
            "authoritative_pid",
            "cursor",
            "max_output_bytes",
        ],
        "write" => &[
            "action",
            "process_id",
            "authoritative_pid",
            "cursor",
            "max_output_bytes",
            "input",
            "wait_ms",
        ],
        _ => {
            return Err(ToolError::InvalidArguments(
                "unknown shell_session action".into(),
            ));
        }
    };
    if let Some(fields) = arguments.as_object() {
        for (field, value) in fields {
            if !allowed.contains(&field.as_str()) && !is_ignored_argument(action, field, value) {
                return Err(ToolError::InvalidArguments(format!(
                    "{field} does not apply to shell_session {action}; omit it"
                )));
            }
        }
    }
    Ok(())
}

pub(super) fn ignored_arguments(arguments: &Value) -> Vec<&'static str> {
    let action = arguments["action"].as_str().unwrap_or_default();
    let candidates = [
        "offset",
        "limit",
        "input",
        "process_id",
        "authoritative_pid",
        "cursor",
        "max_output_bytes",
        "wait_ms",
        "wait_for_terminal",
    ];
    candidates
        .iter()
        .copied()
        .filter(|field| {
            arguments
                .get(*field)
                .is_some_and(|value| is_ignored_argument(action, field, value))
        })
        .collect()
}

/// 只忽略与动作无关的有界读取/等待提示及空占位，绝不丢弃真实目标或非空 stdin。
/// schema 仍校验类型和硬边界；同一判定同时驱动准入与结果说明，避免逐个默认值打补丁。
fn is_ignored_argument(action: &str, field: &str, value: &Value) -> bool {
    match field {
        "process_id" => action == "list" && value.as_str() == Some(""),
        "authoritative_pid" => action == "list" && value.as_u64() == Some(0),
        "input" => action != "write" && value.as_str() == Some(""),
        "offset" | "limit" => action != "list",
        "cursor" | "max_output_bytes" => action == "list",
        "wait_ms" => matches!(action, "list" | "read" | "terminate"),
        "wait_for_terminal" => action != "wait",
        _ => false,
    }
}
