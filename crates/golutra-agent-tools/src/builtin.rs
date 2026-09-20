//! Built-in tool identity and contracts.
//!
//! A built-in tool is declared once here. The runtime dispatches on the typed identity,
//! while external adapters continue to use their provider-supplied contracts.

use golutra_agent_core::{SideEffectType, ToolContract};
use serde_json::{Value, json};

use super::{
    MAX_BACKGROUND_PROCESS_TIMEOUT_MS, MAX_DELEGATED_TASK_CHARS, MAX_FILE_CONTENT_BYTES,
    MAX_FILE_EDITS, MAX_PATCH_BYTES, MAX_PATH_ARGUMENT_CHARS, MAX_PATTERN_ARGUMENT_CHARS,
    MAX_PROCESS_INPUT_CHARS, MAX_READ_LINES, MAX_SHELL_ARGV_ITEMS, MAX_SHELL_COMMAND_CHARS,
    ToolCapabilities, max_poll_wait_ms,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum BuiltinTool {
    ReadFile,
    WriteFile,
    EditFile,
    ApplyPatch,
    ListDir,
    RgSearch,
    SymbolSearch,
    FindReferences,
    AskUser,
    Shell,
    ShellSession,
    RuntimeStatus,
    Subagent,
    ProcessList,
    ProcessPoll,
    ProcessWrite,
    ProcessTerminate,
    ProcessReconnect,
    DelegateTask,
}

impl BuiltinTool {
    /// 稳定的 provider 工具面；其他变体仅供 runtime 内部或回放使用。
    pub(super) const P0_DEFAULT: [Self; 8] = [
        Self::ReadFile,
        Self::WriteFile,
        Self::EditFile,
        Self::ApplyPatch,
        Self::Shell,
        Self::ShellSession,
        Self::Subagent,
        Self::RuntimeStatus,
    ];

    /// 为验证和回放保留的 runtime 能力，不得投影给 provider。
    pub(super) const INTERNAL: [Self; 10] = [
        Self::ListDir,
        Self::RgSearch,
        Self::SymbolSearch,
        Self::FindReferences,
        Self::AskUser,
        Self::ProcessList,
        Self::ProcessPoll,
        Self::ProcessWrite,
        Self::ProcessTerminate,
        Self::ProcessReconnect,
    ];

    pub(super) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "read_file" => Self::ReadFile,
            "write_file" => Self::WriteFile,
            "edit_file" => Self::EditFile,
            "apply_patch" => Self::ApplyPatch,
            "list_dir" => Self::ListDir,
            "rg_search" => Self::RgSearch,
            "symbol_search" => Self::SymbolSearch,
            "find_references" => Self::FindReferences,
            "ask_user" => Self::AskUser,
            "shell" => Self::Shell,
            "shell_session" => Self::ShellSession,
            "runtime_status" => Self::RuntimeStatus,
            "subagent" => Self::Subagent,
            "process_list" => Self::ProcessList,
            "process_poll" => Self::ProcessPoll,
            "process_write" => Self::ProcessWrite,
            "process_terminate" => Self::ProcessTerminate,
            "process_reconnect" => Self::ProcessReconnect,
            "delegate_task" => Self::DelegateTask,
            _ => return None,
        })
    }

    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::ReadFile => "read_file",
            Self::WriteFile => "write_file",
            Self::EditFile => "edit_file",
            Self::ApplyPatch => "apply_patch",
            Self::ListDir => "list_dir",
            Self::RgSearch => "rg_search",
            Self::SymbolSearch => "symbol_search",
            Self::FindReferences => "find_references",
            Self::AskUser => "ask_user",
            Self::Shell => "shell",
            Self::ShellSession => "shell_session",
            Self::RuntimeStatus => "runtime_status",
            Self::Subagent => "subagent",
            Self::ProcessList => "process_list",
            Self::ProcessPoll => "process_poll",
            Self::ProcessWrite => "process_write",
            Self::ProcessTerminate => "process_terminate",
            Self::ProcessReconnect => "process_reconnect",
            Self::DelegateTask => "delegate_task",
        }
    }

    pub(super) const fn side_effect_type(self) -> SideEffectType {
        match self {
            Self::WriteFile | Self::EditFile | Self::ApplyPatch => SideEffectType::File,
            Self::Shell
            | Self::ShellSession
            | Self::Subagent
            | Self::ProcessWrite
            | Self::ProcessTerminate
            | Self::DelegateTask => SideEffectType::Process,
            Self::ReadFile
            | Self::RuntimeStatus
            | Self::ListDir
            | Self::RgSearch
            | Self::SymbolSearch
            | Self::FindReferences
            | Self::AskUser
            | Self::ProcessList
            | Self::ProcessPoll
            | Self::ProcessReconnect => SideEffectType::None,
        }
    }

    pub(super) fn contract(self) -> ToolContract {
        contract(self.name(), self.side_effect_type())
    }

    pub(super) fn capabilities(self) -> ToolCapabilities {
        ToolCapabilities {
            // coding profile 只开放稳定的 provider 工具面。
            available_in_coding_profile: matches!(
                self,
                Self::ReadFile
                    | Self::WriteFile
                    | Self::EditFile
                    | Self::ApplyPatch
                    | Self::Shell
                    | Self::ShellSession
                    | Self::Subagent
                    | Self::RuntimeStatus
            ),
            parallel_read_safe: matches!(
                self,
                Self::ReadFile
                    | Self::ListDir
                    | Self::RgSearch
                    | Self::SymbolSearch
                    | Self::FindReferences
            ),
            coding_profile_hidden_arguments: Vec::new(),
        }
    }
}

pub(super) fn contract(tool_name: &str, side_effect_type: SideEffectType) -> ToolContract {
    let input_schema = match tool_name {
        "read_file" => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_PATH_ARGUMENT_CHARS,
                    "description": "Workspace file path."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based line; next_offset continues."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_READ_LINES,
                    "description": "Max lines."
                }
            },
            "required": ["path"]
        }),
        "write_file" => object_schema(
            &[
                ("path", MAX_PATH_ARGUMENT_CHARS),
                ("content", MAX_FILE_CONTENT_BYTES as usize),
            ],
            &["path", "content"],
            &["path"],
        ),
        "edit_file" => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_PATH_ARGUMENT_CHARS,
                    "description": "Workspace file path."
                },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_FILE_EDITS,
                    "description": "Exact non-overlapping replacements; batch disjoint edits.",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "old_text": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": MAX_FILE_CONTENT_BYTES,
                                "description": "Exact text, including whitespace/newlines."
                            },
                            "new_text": {
                                "type": "string",
                                "maxLength": MAX_FILE_CONTENT_BYTES,
                                "description": "Replacement text."
                            }
                        },
                        "required": ["old_text", "new_text"]
                    }
                }
            },
            "required": ["path", "edits"]
        }),
        "apply_patch" => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "patch": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_PATCH_BYTES,
                    "description": "Atomic unified or Begin/Update/Add/Delete patch."
                }
            },
            "required": ["patch"]
        }),
        "list_dir" => object_schema(&[("path", MAX_PATH_ARGUMENT_CHARS)], &[], &[]),
        "rg_search" => object_schema(
            &[
                ("pattern", MAX_PATTERN_ARGUMENT_CHARS),
                ("path", MAX_PATH_ARGUMENT_CHARS),
            ],
            &["pattern"],
            &["pattern"],
        ),
        "symbol_search" => query_schema("query"),
        "find_references" => query_schema("symbol"),
        "ask_user" => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 3,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "id": {"type": "string", "minLength": 1, "maxLength": 128},
                            "header": {"type": "string", "minLength": 1, "maxLength": 128},
                            "question": {"type": "string", "minLength": 1, "maxLength": 2048},
                            "mode": {"type": "string", "enum": ["single", "multiple"]},
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 8,
                                "items": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "id": {"type": "string", "minLength": 1, "maxLength": 128},
                                        "label": {"type": "string", "minLength": 1, "maxLength": 256},
                                        "description": {"type": "string", "minLength": 1, "maxLength": 2048}
                                    },
                                    "required": ["id", "label"]
                                }
                            }
                        },
                        "required": ["id", "header", "question", "options"]
                    }
                }
            },
            "required": ["questions"]
        }),
        "delegate_task" | "subagent" => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {"type": "string", "enum": ["spawn", "status", "wait", "send_input", "resume", "cancel"], "description": "Default spawn requires task. Reuse child_session_id: status/wait reads progress or result; send_input adds task text to active work; resume starts a new turn with task text in the same child history; cancel requests termination. Never respawn to retry a wait."},
                "child_session_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "System-assigned handle returned by spawn, not a name. Omit on spawn; reuse the returned value for other actions."},
                "child_session_ids": {"type": "array", "minItems": 1, "maxItems": 1024, "items": {"type":"string", "minLength":1, "maxLength":128}, "description": "For wait only, instead of child_session_id. Wait on multiple children concurrently; results remain readable."},
                "wait_mode": {"type":"string", "enum":["any","all"], "description":"Multi-child wait defaults to any completed child; all waits for every target within the shared wait_ms deadline. Timeout never cancels children."},
                "offset": {"type": "integer", "minimum": 0, "description": "0-based result character offset. When child_result_has_more, read action=status with child_result_next_offset."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 2048, "description": "Result characters per page; default 1024. Reduce if model_visible_truncated."},
                "agent_type": {"type": "string", "enum": ["general", "explore"], "description": "Use explore for read-only repository questions; general inherits the parent execution surface."},
                "context": {"type":"string", "enum":["independent","fork"], "description":"Spawn only. Default independent uses task and project instructions. fork inherits the parent's last complete provider request snapshot, then appends task; unfinished tool calls are excluded. Resume always uses the child's own history."},
                "isolation": {"type":"string", "enum":["shared","worktree"], "description":"Default shared workspace. worktree creates a separate Git checkout of HEAD, excluding uncommitted parent changes. Retained for review/resume; no automatic merge. Resume preserves the original isolation."},
                "run_in_background": {"type": "boolean", "description": "Return a child handle after startup; use wait for results."},
                "wait_ms": {"type": "integer", "minimum": 0, "maximum": 60000, "description": "Maximum wait; expiration leaves the child running."},
                "task": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_DELEGATED_TASK_CHARS,
                    "description": "Child task. Include needed context for the default independent mode; fork explicitly inherits the parent request snapshot."
                },
                "model": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 256,
                    "description": "Optional model override; omit to inherit."
                },
                "reasoning_effort": {
                    "type": "string",
                    "enum": ["low", "medium", "high", "xhigh", "max", "ultra"],
                    "description": "Optional reasoning override; omit to inherit."
                }
            },
            "required": []
        }),
        "shell" => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "command": {
                    "type": "string",
                    "minLength": 0,
                    "maxLength": MAX_SHELL_COMMAND_CHARS,
                    "description": "Non-empty command to execute. Use bash -lc for pipes, redirects, compound commands, or heredoc."
                },
                "argv": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_SHELL_ARGV_ITEMS,
                    "items": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_SHELL_COMMAND_CHARS
                    },
                    "description": "Argument vector; prefer omitting command. If both are supplied, command must parse to an argv prefix with matching argument values."
                },
                "workdir": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_PATH_ARGUMENT_CHARS,
                    "description": "workspace-relative directory."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_BACKGROUND_PROCESS_TIMEOUT_MS,
                    "description": format!("Hard process lifetime in ms, 1..={MAX_BACKGROUND_PROCESS_TIMEOUT_MS}; omit normally, set only to intentionally terminate.")
                },
                "background": {
                    "type": "boolean",
                    "description": "Start a runtime-owned process; return immediately by default, or wait up to yield_time_ms if set. Normally omit timeout_ms; use shell_session while it is running."
                },
                "tty": {"type":"boolean", "description":"Allocate a PTY for interactive commands on Unix; default false uses ordinary pipes."},
                "yield_time_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "description": format!("Initial wait in ms, capped at {}; default 10000, or 0 with background=true. Does not set or extend process lifetime.", max_poll_wait_ms())
                },
                "max_output_bytes": {"type": "integer", "minimum": 256, "description": "Optional preview budget in bytes (minimum 256); normally omit. Default 12288; larger requests are capped by runtime/context policy. Continue only if more output is needed."}
            },
            "required": []
        }),
        "shell_session" => shell_session_schema(),
        "process_list" | "runtime_status" => object_schema(&[], &[], &[]),
        "process_poll" => process_session_schema(false, true),
        "process_write" => process_session_schema(true, true),
        "process_terminate" => process_session_schema(false, false),
        "process_reconnect" => process_session_schema(false, false),
        _ => json!({"type": "object", "additionalProperties": false}),
    };
    ToolContract {
        tool_name: tool_name.to_owned(),
        input_schema,
        output_schema: json!({
            "type": "object",
            "additionalProperties": true,
            "required": ["status", "summary"]
        }),
        error_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "code": {"type": "string"},
                "message": {"type": "string"}
            },
            "required": ["code", "message"]
        }),
        side_effect_type,
        idempotency_key_policy: match side_effect_type {
            SideEffectType::None => "not_required",
            SideEffectType::File | SideEffectType::Process => "required_for_retry",
            SideEffectType::Network | SideEffectType::ExternalSystem => "blocked_in_p0",
        }
        .to_owned(),
        timeout_policy: "bounded_by_tool_or_default_timeout".to_owned(),
        cancellation_policy: "returns_cancelled_envelope".to_owned(),
        retry_policy: if side_effect_type == SideEffectType::None {
            "retry_allowed"
        } else {
            "no_implicit_retry_for_side_effects"
        }
        .to_owned(),
        artifact_policy: "raw_output_to_artifact_ref".to_owned(),
        permission_policy_ref: None,
    }
}

fn shell_session_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": {"type": "string", "enum": ["wait", "read", "write", "terminate", "list"], "description": "Wait, read available output, write stdin, terminate, or list processes. Unused read/list/wait hints are ignored and reported. Non-empty input is write-only; list has no process target. Unread output does not mean still running."},
            "offset": {"type":"integer", "minimum":0, "description":"List offset; follow next_offset while has_more."},
            "limit": {"type":"integer", "minimum":1, "maximum":64, "description":"For list only: 1..64 processes per page; default 4. Omit for other actions."},
            "process_id": {"type": "string", "maxLength": 128, "description": "Non-empty ID returned by shell; required for wait/read/write/terminate. Omit for list."},
            "authoritative_pid": {"type": "integer", "minimum": 0, "maximum": u32::MAX, "description": "Optional positive OS PID check; must match start if supplied. Omit for list."},
            "cursor": {"type": "integer", "minimum": 0, "description": "Optional byte cursor for repeatable reads; omitted continues after the last delivered page."},
            "max_output_bytes": {"type": "integer", "minimum": 256, "description": "Optional preview budget in bytes (minimum 256); normally omit. Default 12288; larger requests are capped by runtime/context policy. Continue only if more output is needed."},
            "input": {"type": "string", "maxLength": MAX_PROCESS_INPUT_CHARS, "description": "Stdin text for write."},
            "wait_ms": {"type": "integer", "minimum": 0, "description": format!("Event-driven wait in ms, capped at {}. Reaching this deadline does not stop the process. read always returns immediately; omit on terminate/list.", max_poll_wait_ms())},
            "wait_for_terminal": {"type": "boolean", "description": "Wait for one terminal state or deadline."}
        },
        "required": ["action"]
    })
}

fn query_schema(field: &str) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            (field): {
                "type": "string",
                "minLength": 1,
                "maxLength": 512
            },
            "limit": {"type": "integer", "minimum": 1, "maximum": 100}
        },
        "required": [field]
    })
}

fn process_session_schema(include_input: bool, include_wait: bool) -> Value {
    let mut properties = serde_json::Map::from_iter([
        (
            "process_id".to_owned(),
            json!({"type": "string", "minLength": 1, "maxLength": 128}),
        ),
        (
            "cursor".to_owned(),
            json!({"type": "integer", "minimum": 0}),
        ),
        (
            "authoritative_pid".to_owned(),
            json!({"type": "integer", "minimum": 1, "maximum": u32::MAX}),
        ),
    ]);
    let mut required = vec!["process_id"];
    if include_input {
        properties.insert(
            "input".to_owned(),
            json!({
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_PROCESS_INPUT_CHARS
            }),
        );
        required.push("input");
    }
    if include_wait {
        properties.insert(
            "wait_ms".to_owned(),
            json!({"type": "integer", "minimum": 0, "maximum": max_poll_wait_ms()}),
        );
    }
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": required,
    })
}

fn object_schema(properties: &[(&str, usize)], required: &[&str], non_empty: &[&str]) -> Value {
    let properties = properties
        .iter()
        .map(|(name, max_length)| {
            let mut schema = json!({"type": "string", "maxLength": max_length});
            if non_empty.contains(name) {
                schema["minLength"] = json!(1);
            }
            ((*name).to_owned(), schema)
        })
        .collect::<serde_json::Map<_, _>>();
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": required,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn default_builtins_have_one_typed_contract_each() {
        let names = BuiltinTool::P0_DEFAULT
            .into_iter()
            .map(|tool| {
                let contract = tool.contract();
                assert_eq!(BuiltinTool::from_name(&contract.tool_name), Some(tool));
                contract.tool_name
            })
            .collect::<Vec<_>>();
        assert_eq!(names.len(), names.iter().collect::<HashSet<_>>().len());
    }
}
