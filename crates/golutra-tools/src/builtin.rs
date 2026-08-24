//! Built-in tool identity and contracts.
//!
//! A built-in tool is declared once here. The runtime dispatches on the typed identity,
//! while external adapters continue to use their provider-supplied contracts.

use golutra_core::{SideEffectType, ToolContract};
use serde_json::{Value, json};

use super::{
    MAX_BACKGROUND_PROCESS_TIMEOUT_MS, MAX_DELEGATED_TASK_CHARS, MAX_FILE_CONTENT_BYTES,
    MAX_PATCH_BYTES, MAX_PATH_ARGUMENT_CHARS, MAX_PATTERN_ARGUMENT_CHARS, MAX_PROCESS_INPUT_CHARS,
    MAX_SHELL_COMMAND_CHARS, ToolCapabilities, max_poll_wait_ms,
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
    ProcessList,
    ProcessPoll,
    ProcessWrite,
    ProcessTerminate,
    ProcessReconnect,
    DelegateTask,
}

impl BuiltinTool {
    pub(super) const P0_DEFAULT: [Self; 15] = [
        Self::ReadFile,
        Self::WriteFile,
        Self::EditFile,
        Self::ApplyPatch,
        Self::ListDir,
        Self::RgSearch,
        Self::SymbolSearch,
        Self::FindReferences,
        Self::AskUser,
        Self::Shell,
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
            Self::Shell | Self::ProcessWrite | Self::ProcessTerminate | Self::DelegateTask => {
                SideEffectType::Process
            }
            Self::ReadFile
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
            // coding 面保留常见文件/搜索和进程观察能力；代码图、进程写入/终止
            // 以及扩展委派仍由 full profile 显式开启，避免牺牲常用工作流。
            available_in_coding_profile: matches!(
                self,
                Self::ReadFile
                    | Self::WriteFile
                    | Self::EditFile
                    | Self::ApplyPatch
                    | Self::AskUser
                    | Self::Shell
                    | Self::ListDir
                    | Self::RgSearch
                    | Self::ProcessList
                    | Self::ProcessPoll
                    | Self::ProcessReconnect
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
        "read_file" => object_schema(&[("path", MAX_PATH_ARGUMENT_CHARS)], &["path"], &["path"]),
        "write_file" => object_schema(
            &[
                ("path", MAX_PATH_ARGUMENT_CHARS),
                ("content", MAX_FILE_CONTENT_BYTES as usize),
            ],
            &["path", "content"],
            &["path"],
        ),
        "edit_file" => object_schema(
            &[
                ("path", MAX_PATH_ARGUMENT_CHARS),
                ("search", MAX_FILE_CONTENT_BYTES as usize),
                ("replace", MAX_FILE_CONTENT_BYTES as usize),
            ],
            &["path", "search", "replace"],
            &["path", "search"],
        ),
        "apply_patch" => object_schema(&[("patch", MAX_PATCH_BYTES)], &["patch"], &["patch"]),
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
        "delegate_task" => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "task": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_DELEGATED_TASK_CHARS,
                    "description": "A complete, self-contained task for one child agent. Include the relevant goal, constraints, and expected result; the child does not receive the parent conversation history."
                },
                "model": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 256,
                    "description": "Optional model override. Omit it to inherit the parent agent's effective model."
                },
                "reasoning_effort": {
                    "type": "string",
                    "enum": ["low", "medium", "high", "xhigh"],
                    "description": "Optional reasoning effort override. Omit it to inherit the parent agent's effective setting."
                }
            },
            "required": ["task"]
        }),
        "shell" => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "command": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_SHELL_COMMAND_CHARS,
                    "description": "A single argv command parsed without a shell. A complete quoted foreground Python heredoc such as python - <<'PY' is passed directly on stdin. Unquoted operators such as |, >, &&, and ; are otherwise rejected; for a pipeline, redirection, or compound script, invoke bash -lc and pass the entire script as one quoted argument."
                },
                "workdir": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_PATH_ARGUMENT_CHARS,
                    "description": "Optional working directory resolved from the workspace root. It changes only the command cwd; sandbox permissions and workspace change tracking remain rooted at the workspace root."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_BACKGROUND_PROCESS_TIMEOUT_MS,
                    "description": "The absolute process lifetime from launch in milliseconds, not an initial wait. Expiry terminates the process with a timed_out state. Defaults to 5000 for foreground commands and 3600000 for background commands."
                },
                "background": {
                    "type": "boolean",
                    "description": "When true, start a runtime-scoped managed process and return its process_id after yield_time_ms. The process stops when the runtime ends. If another process or evaluator must connect after the final response, do not use background=true; use a platform-appropriate lifecycle mechanism outside runtime ownership, detach standard streams as required, and verify availability before returning."
                },
                "yield_time_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": max_poll_wait_ms(),
                    "description": "For a background command, wait at most this long for initial output or termination before returning. This only controls the initial wait and does not extend timeout_ms or the process lifetime."
                }
            },
            "required": ["command"]
        }),
        "process_list" => object_schema(&[], &[], &[]),
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
