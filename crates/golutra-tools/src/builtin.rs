//! Built-in tool identity and contracts.
//!
//! A built-in tool is declared once here. The runtime dispatches on the typed identity,
//! while external adapters continue to use their provider-supplied contracts.

use golutra_core::{SideEffectType, ToolContract};
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
    WebSearch,
    ShellSession,
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
        Self::WebSearch,
        Self::ShellSession,
        Self::Subagent,
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
            "web_search" => Self::WebSearch,
            "shell_session" => Self::ShellSession,
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
            Self::WebSearch => "web_search",
            Self::ShellSession => "shell_session",
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
            Self::WebSearch => SideEffectType::Network,
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
            // coding profile 只开放稳定的 provider 工具面。
            available_in_coding_profile: matches!(
                self,
                Self::ReadFile
                    | Self::WriteFile
                    | Self::EditFile
                    | Self::ApplyPatch
                    | Self::Shell
                    | Self::WebSearch
                    | Self::ShellSession
                    | Self::Subagent
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
                    "description": "Workspace-relative or absolute path."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based start line; continue with next_offset."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_READ_LINES,
                    "description": "Maximum lines to return."
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
                    "description": "Workspace-relative or absolute path."
                },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_FILE_EDITS,
                    "description": "Unique exact replacements; combine independent edits and do not overlap.",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "old_text": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": MAX_FILE_CONTENT_BYTES,
                                "description": "Unique exact original text, including whitespace/newlines."
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
                    "description": "Unified or Begin/Update/Add/Delete patch; apply atomically."
                }
            },
            "required": ["patch"]
        }),
        "web_search" => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": {"type": "string", "minLength": 1, "maxLength": 2048},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 20},
                "domains": {
                    "type": "array",
                    "maxItems": 10,
                    "items": {"type": "string", "minLength": 1, "maxLength": 253}
                }
            },
            "required": ["query"]
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
                "task": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_DELEGATED_TASK_CHARS,
                    "description": "Self-contained child task; child has no parent history."
                },
                "model": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 256,
                    "description": "Optional model override; omit to inherit."
                },
                "reasoning_effort": {
                    "type": "string",
                    "enum": ["low", "medium", "high", "xhigh"],
                    "description": "Optional reasoning override; omit to inherit."
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
                    "description": "Shell-safe command; heredoc is supported. Use bash -lc for pipes, redirects, or compound commands."
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
                    "description": "Optional direct argv; when combined, it must match command/prefix."
                },
                "workdir": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_PATH_ARGUMENT_CHARS,
                    "description": "Optional workspace-relative directory."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_BACKGROUND_PROCESS_TIMEOUT_MS,
                    "description": "Hard process lifetime in ms. For background work, omit normally; set only to intentionally terminate at this deadline."
                },
                "background": {
                    "type": "boolean",
                    "description": "Start a runtime-owned process and return after yield_time_ms. Normally omit timeout_ms; use shell_session to wait, write, or terminate."
                },
                "yield_time_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": max_poll_wait_ms(),
                    "description": "Initial output/exit wait before returning process_id; it does not set or extend the process lifetime."
                }
            },
            "required": []
        }),
        "shell_session" => shell_session_schema(),
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

fn shell_session_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": {"type": "string", "enum": ["wait", "write", "terminate"], "description": "Wait/read, write stdin, or terminate."},
            "process_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "ID returned by shell(background=true)."},
            "authoritative_pid": {"type": "integer", "minimum": 1, "maximum": u32::MAX, "description": "Required OS PID; must match the start response."},
            "cursor": {"type": "integer", "minimum": 0, "description": "Last output cursor; reuse the returned value."},
            "input": {"type": "string", "maxLength": MAX_PROCESS_INPUT_CHARS, "description": "Stdin text for write."},
            "wait_ms": {"type": "integer", "minimum": 0, "maximum": max_poll_wait_ms(), "description": "Bounded event-driven wait in ms."},
            "wait_for_terminal": {"type": "boolean", "description": "Wait for one terminal state or the deadline."}
        },
        "required": ["action", "process_id", "authoritative_pid"]
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
