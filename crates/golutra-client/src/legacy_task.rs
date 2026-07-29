//! Compatibility translation for callers that predate structured task contracts.
//!
//! The runtime consumes [`TaskContract`] exclusively. Prompt heuristics live
//! here at the client boundary so they cannot silently influence a structured
//! task once execution has started.

use golutra_core::{
    RequiredFileContent, TaskContract, VerificationRequirement, WorkspaceChangeRequirement,
    infer_legacy_write_content, infer_legacy_write_path,
};
use serde_json::Value;

#[derive(Debug, Clone, Copy)]
pub(crate) struct LegacyTaskAdapter<'a> {
    payload: &'a Value,
    objective: &'a str,
}

impl<'a> LegacyTaskAdapter<'a> {
    #[must_use]
    pub(crate) const fn new(payload: &'a Value, objective: &'a str) -> Self {
        Self { payload, objective }
    }

    #[must_use]
    pub(crate) fn requests_workspace_change(self) -> bool {
        self.payload.get("content").is_some()
            || self.payload.get("patch").is_some()
            || self.payload.get("replacement").is_some()
            || contains_change_verb(self.objective)
    }

    /// Return a delivery path only when the legacy request makes it explicit.
    /// Broad coding requests must not invent a path for verification.
    #[must_use]
    pub(crate) fn required_path(self) -> Option<String> {
        if !self.requests_workspace_change() {
            return None;
        }
        non_empty_string_payload(self.payload, "path")
            .and_then(normalize_legacy_contract_path)
            .or_else(|| infer_legacy_write_path(self.objective))
    }

    /// Adapt an unstructured request once at the command boundary.
    /// Explicit task contracts bypass this adapter and remain authoritative.
    pub(crate) fn apply_to(self, contract: &mut TaskContract) -> bool {
        if !self.requests_workspace_change() {
            return false;
        }
        contract.workspace_change = WorkspaceChangeRequirement::Required;
        contract.require_objective_validation = true;
        if let Some(requested_path) = self.required_path() {
            if !contract.required_paths.contains(&requested_path) {
                contract.required_paths.push(requested_path.clone());
            }
            if let Some(content) = self.required_content()
                && !contract
                    .required_file_contents
                    .iter()
                    .any(|requirement| requirement.path == requested_path)
            {
                contract.required_file_contents.push(RequiredFileContent {
                    path: requested_path,
                    content,
                });
            }
        }
        if contract.verification == VerificationRequirement::BestEffort {
            contract.verification = VerificationRequirement::Required;
        }
        true
    }

    #[must_use]
    pub(crate) fn requests_workspace_tools(self) -> bool {
        if self.payload.get("path").is_some()
            || self.payload.get("content").is_some()
            || self.payload.get("command").is_some()
        {
            return true;
        }

        let lower = self.objective.to_ascii_lowercase();
        const ENGLISH_MARKERS: &[&str] = &[
            "write",
            "create",
            "edit",
            "modify",
            "update",
            "delete",
            "read",
            "list",
            "search",
            "find",
            "inspect",
            "run",
            "test",
            "build",
            "fix",
            "debug",
            "refactor",
            "file",
            "code",
            "workspace",
            "diff",
            "commit",
            "shell",
        ];
        const CJK_MARKERS: &[&str] = &[
            "写",
            "创建",
            "修改",
            "更新",
            "删除",
            "读取",
            "读",
            "列出",
            "搜索",
            "查找",
            "检查",
            "运行",
            "测试",
            "构建",
            "修复",
            "重构",
            "文件",
            "代码",
            "工作区",
            "提交",
        ];

        ENGLISH_MARKERS.iter().any(|marker| lower.contains(marker))
            || CJK_MARKERS
                .iter()
                .any(|marker| self.objective.contains(marker))
    }

    #[must_use]
    pub(crate) fn write_file_args(self) -> LegacyWriteFileArgs {
        LegacyWriteFileArgs {
            path: non_empty_string_payload(self.payload, "path")
                .or_else(|| infer_legacy_write_path(self.objective))
                .unwrap_or_else(|| "golutra-agent-output.txt".to_owned()),
            content: non_empty_string_payload(self.payload, "content")
                .or_else(|| infer_legacy_write_content(self.objective))
                .unwrap_or_else(|| "done\n".to_owned()),
        }
    }

    fn required_content(self) -> Option<String> {
        non_empty_string_payload(self.payload, "content")
            .or_else(|| infer_legacy_write_content(self.objective))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegacyWriteFileArgs {
    pub(crate) path: String,
    pub(crate) content: String,
}

fn contains_change_verb(objective: &str) -> bool {
    const ENGLISH_CHANGE_VERBS: &[&str] = &[
        "add",
        "change",
        "create",
        "delete",
        "edit",
        "fix",
        "implement",
        "modify",
        "move",
        "patch",
        "refactor",
        "remove",
        "rename",
        "rewrite",
        "update",
        "write",
    ];
    const CJK_CHANGE_MARKERS: &[&str] = &[
        "添加",
        "创建",
        "修复",
        "修改",
        "实现",
        "删除",
        "重构",
        "重命名",
        "更改",
        "更新",
        "移除",
        "移动",
        "补丁",
        "改代码",
        "写入",
    ];
    let lower = objective.to_ascii_lowercase();
    lower
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|token| ENGLISH_CHANGE_VERBS.contains(&token))
        || CJK_CHANGE_MARKERS
            .iter()
            .any(|marker| objective.contains(marker))
}

fn non_empty_string_payload(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn normalize_legacy_contract_path(path: String) -> Option<String> {
    let normalized = path.replace('\\', "/");
    let relative = normalized
        .strip_prefix("/app/")
        .or_else(|| normalized.strip_prefix("/workspace/"))
        .unwrap_or(&normalized);
    (!relative.is_empty()
        && !relative.starts_with('/')
        && relative
            .as_bytes()
            .get(1)
            .is_none_or(|separator| *separator != b':')
        && !relative.split('/').any(|component| component == ".."))
    .then(|| relative.to_owned())
}
