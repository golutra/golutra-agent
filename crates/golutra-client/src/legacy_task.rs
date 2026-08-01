//! Compatibility translation for callers that predate structured task contracts.
//!
//! The runtime consumes [`TaskContract`] exclusively. Prompt heuristics live
//! here at the client boundary so they cannot silently influence a structured
//! task once execution has started.

use golutra_core::{
    RequiredFileContent, TaskContract, VerificationRequirement, WorkspaceChangeRequirement,
    infer_legacy_write_content, infer_legacy_write_path, infer_legacy_write_paths,
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
        let explicit_workspace_delivery = self.payload.get("content").is_some()
            || self.payload.get("patch").is_some()
            || self.payload.get("replacement").is_some()
            || self.payload.get("path").is_some()
            || !infer_legacy_write_paths(self.objective).is_empty();
        explicit_workspace_delivery
            || (contains_workspace_change_intent(self.objective)
                && !contains_installed_environment_change_intent(self.objective))
    }

    /// Return a delivery path only when the legacy request makes it explicit.
    /// Broad coding requests must not invent a path for verification.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn required_path(self) -> Option<String> {
        self.required_paths().into_iter().next()
    }

    #[must_use]
    pub(crate) fn required_paths(self) -> Vec<String> {
        if let Some(path) =
            non_empty_string_payload(self.payload, "path").and_then(normalize_legacy_contract_path)
        {
            return vec![path];
        }
        infer_legacy_write_paths(self.objective)
    }

    /// Adapt an unstructured request once at the command boundary.
    /// Explicit task contracts bypass this adapter and remain authoritative.
    pub(crate) fn apply_to(self, contract: &mut TaskContract) -> bool {
        let requests_workspace_change = self.requests_workspace_change();
        if !requests_workspace_change
            && !contains_installed_environment_change_intent(self.objective)
        {
            return false;
        }
        if requests_workspace_change {
            contract.workspace_change = WorkspaceChangeRequirement::Required;
        }
        contract.require_objective_validation = true;
        if requests_workspace_change {
            let requested_paths = self.required_paths();
            for requested_path in &requested_paths {
                if !contract.required_paths.contains(requested_path) {
                    contract.required_paths.push(requested_path.clone());
                }
            }
            if let Some(requested_path) = requested_paths.first()
                && let Some(content) = self.required_content()
                && !contract
                    .required_file_contents
                    .iter()
                    .any(|requirement| requirement.path == requested_path.as_str())
            {
                contract.required_file_contents.push(RequiredFileContent {
                    path: requested_path.clone(),
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

fn contains_workspace_change_intent(objective: &str) -> bool {
    const STRONG_CHANGE_VERBS: &[&str] = &[
        "implement",
        "implemented",
        "patch",
        "patched",
        "refactor",
        "refactored",
        "rewrite",
        "rewritten",
    ];
    const AMBIGUOUS_CHANGE_VERBS: &[&str] = &[
        "add", "change", "create", "delete", "edit", "fix", "modify", "move", "remove", "rename",
        "update", "write",
    ];
    const WORKSPACE_TARGETS: &[&str] = &[
        "api",
        "application",
        "bug",
        "class",
        "code",
        "crate",
        "file",
        "files",
        "function",
        "method",
        "module",
        "program",
        "programs",
        "project",
        "repo",
        "repository",
        "script",
        "scripts",
        "server",
        "source",
        "test",
        "tests",
        "webpage",
        "website",
    ];
    const CJK_STRONG_CHANGE_MARKERS: &[&str] = &["实现", "重构", "补丁", "重写"];
    const CJK_AMBIGUOUS_CHANGE_MARKERS: &[&str] = &[
        "添加",
        "创建",
        "修复",
        "修改",
        "删除",
        "重命名",
        "更改",
        "更新",
        "移除",
        "移动",
        "写入",
    ];
    const CJK_WORKSPACE_TARGETS: &[&str] = &[
        "代码", "文件", "函数", "方法", "模块", "项目", "仓库", "脚本", "程序", "测试", "服务",
    ];

    let mut in_fence = false;
    for raw_line in objective.lines() {
        let trimmed = raw_line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence
            || raw_line.starts_with('\t')
            || raw_line.len().saturating_sub(trimmed.len()) >= 4
        {
            continue;
        }

        let prose = strip_inline_code(trimmed);
        let lower = prose.to_ascii_lowercase();
        let tokens = lower
            .split(|character: char| !character.is_ascii_alphanumeric())
            .filter(|token| !token.is_empty())
            .collect::<Vec<_>>();
        if tokens
            .iter()
            .any(|token| STRONG_CHANGE_VERBS.contains(token))
        {
            return true;
        }
        if tokens
            .iter()
            .any(|token| AMBIGUOUS_CHANGE_VERBS.contains(token))
            && tokens.iter().any(|token| WORKSPACE_TARGETS.contains(token))
        {
            return true;
        }
        if CJK_STRONG_CHANGE_MARKERS
            .iter()
            .any(|marker| prose.contains(marker))
            || (CJK_AMBIGUOUS_CHANGE_MARKERS
                .iter()
                .any(|marker| prose.contains(marker))
                && CJK_WORKSPACE_TARGETS
                    .iter()
                    .any(|target| prose.contains(target)))
        {
            return true;
        }
    }
    false
}

fn contains_installed_environment_change_intent(objective: &str) -> bool {
    let prose = prose_without_code(objective).to_ascii_lowercase();
    let tokens = prose
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let requests_change = tokens.iter().any(|token| {
        matches!(
            *token,
            "change" | "configure" | "fix" | "modify" | "patch" | "repair" | "replace" | "update"
        )
    });
    let installed_runtime = prose.contains("site-packages")
        || prose.contains("site packages")
        || prose.contains("default python interpreter")
        || prose.contains("system python")
        || (tokens
            .iter()
            .any(|token| matches!(*token, "installed" | "installation"))
            && tokens
                .iter()
                .any(|token| matches!(*token, "dependency" | "package" | "python")))
        || (tokens.contains(&"package") && tokens.contains(&"interpreter"));
    let explicitly_scoped_to_workspace = tokens.iter().any(|token| {
        matches!(
            *token,
            "crate" | "project" | "repo" | "repository" | "source" | "workspace"
        )
    });

    requests_change && installed_runtime && !explicitly_scoped_to_workspace
}

fn prose_without_code(objective: &str) -> String {
    let mut prose = String::with_capacity(objective.len());
    let mut in_fence = false;
    for raw_line in objective.lines() {
        let trimmed = raw_line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence
            || raw_line.starts_with('\t')
            || raw_line.len().saturating_sub(trimmed.len()) >= 4
        {
            continue;
        }
        prose.push_str(&strip_inline_code(trimmed));
        prose.push('\n');
    }
    prose
}

fn strip_inline_code(line: &str) -> String {
    let mut prose = String::with_capacity(line.len());
    let mut in_code = false;
    for character in line.chars() {
        if character == '`' {
            in_code = !in_code;
            prose.push(' ');
        } else if in_code {
            prose.push(' ');
        } else {
            prose.push(character);
        }
    }
    prose
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
