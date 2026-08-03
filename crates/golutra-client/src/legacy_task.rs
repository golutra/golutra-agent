//! Compatibility translation for callers that predate structured task contracts.
//!
//! The runtime consumes [`TaskContract`] exclusively. Prompt heuristics live
//! here at the client boundary so they cannot silently influence a structured
//! task once execution has started.

use golutra_core::{
    RequiredFileContent, TaskContract, VerificationRequirement, WorkspaceChangeRequirement,
    infer_direct_legacy_write_path, infer_legacy_write_content, infer_legacy_write_objective,
    infer_legacy_write_path, infer_legacy_write_paths,
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
        let non_workspace_environment_change =
            contains_explicit_non_workspace_environment_change(self.objective);
        let explicit_workspace_delivery = self.payload.get("content").is_some()
            || self.payload.get("patch").is_some()
            || self.payload.get("replacement").is_some()
            || self.payload.get("path").is_some()
            || !infer_legacy_write_paths(self.objective).is_empty();
        explicit_workspace_delivery
            || (contains_workspace_change_intent(self.objective)
                && !non_workspace_environment_change)
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
        infer_legacy_write_objective(self.objective)
            .map(|hint| hint.path)
            .or_else(|| infer_direct_legacy_write_path(self.objective))
            .into_iter()
            .collect()
    }

    /// Adapt an unstructured request once at the command boundary.
    /// Explicit task contracts bypass this adapter and remain authoritative.
    pub(crate) fn apply_to(self, contract: &mut TaskContract) -> bool {
        let requests_workspace_change = self.requests_workspace_change();
        if !requests_workspace_change
            && !contains_explicit_non_workspace_environment_change(self.objective)
        {
            return false;
        }
        if requests_workspace_change {
            contract.workspace_change = WorkspaceChangeRequirement::Required;
        }
        let requested_paths = if requests_workspace_change {
            self.required_paths()
        } else {
            Vec::new()
        };
        contract.require_objective_validation = true;
        if requests_workspace_change {
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

    fn required_content(self) -> Option<String> {
        non_empty_string_payload(self.payload, "content")
            .or_else(|| infer_legacy_write_objective(self.objective).and_then(|hint| hint.content))
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
        "add",
        "change",
        "convert",
        "create",
        "delete",
        "edit",
        "fix",
        "generate",
        "modify",
        "move",
        "output",
        "place",
        "produce",
        "remove",
        "rename",
        "save",
        "store",
        "transcode",
        "transform",
        "update",
        "write",
    ];
    const CONCRETE_LOCAL_ARTIFACTS: &[&str] = &[
        "adapter",
        "class",
        "client",
        "code",
        "component",
        "crate",
        "file",
        "files",
        "function",
        "handler",
        "integration",
        "library",
        "method",
        "module",
        "package",
        "parser",
        "plugin",
        "script",
        "scripts",
        "source",
        "test",
        "tests",
        "workflow",
    ];
    const LOCAL_WORKSPACE_TARGETS: &[&str] = &[
        "bug", "program", "programs", "project", "webpage", "website",
    ];
    const AMBIGUOUS_WORKSPACE_TARGETS: &[&str] =
        &["api", "application", "repo", "repository", "server"];
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
        "保存",
        "生成",
        "输出",
        "交付",
    ];
    const CJK_WORKSPACE_TARGETS: &[&str] = &[
        "代码",
        "客户端",
        "文件",
        "函数",
        "方法",
        "集成",
        "模块",
        "插件",
        "项目",
        "仓库",
        "脚本",
        "程序",
        "测试",
        "服务",
        "适配器",
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
        let explicit_local_scope = contains_explicit_local_scope(&lower, &tokens);
        let concrete_local_artifact = tokens
            .iter()
            .any(|token| CONCRETE_LOCAL_ARTIFACTS.contains(token));
        if !explicit_local_scope
            && !concrete_local_artifact
            && contains_external_system_scope(&prose, &tokens)
        {
            continue;
        }
        if tokens
            .iter()
            .any(|token| STRONG_CHANGE_VERBS.contains(token))
        {
            return true;
        }
        if tokens
            .iter()
            .any(|token| AMBIGUOUS_CHANGE_VERBS.contains(token))
            && (concrete_local_artifact
                || tokens
                    .iter()
                    .any(|token| LOCAL_WORKSPACE_TARGETS.contains(token))
                || (tokens
                    .iter()
                    .any(|token| AMBIGUOUS_WORKSPACE_TARGETS.contains(token))
                    && explicit_local_scope))
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

fn contains_external_system_scope(prose: &str, tokens: &[&str]) -> bool {
    const EXTERNAL_SCOPE_TOKENS: &[&str] = &[
        "aws",
        "azure",
        "bitbucket",
        "cloud",
        "gcp",
        "github",
        "gitlab",
        "hosted",
        "managed",
        "remote",
        "saas",
    ];
    const EXTERNAL_RESOURCES: &[&str] = &[
        "account",
        "bucket",
        "cluster",
        "database",
        "gateway",
        "issue",
        "repository",
        "service",
        "tenant",
    ];
    const CJK_MARKERS: &[&str] = &["云账号", "云服务", "远程仓库", "托管服务"];

    let external_scope = tokens
        .iter()
        .any(|token| EXTERNAL_SCOPE_TOKENS.contains(token));
    (external_scope
        && tokens
            .iter()
            .any(|token| EXTERNAL_RESOURCES.contains(token)))
        || CJK_MARKERS.iter().any(|marker| prose.contains(marker))
}

fn contains_explicit_local_scope(lower: &str, tokens: &[&str]) -> bool {
    [
        "this checkout",
        "this codebase",
        "this project",
        "this repo",
        "this repository",
        "working tree",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
        || tokens
            .iter()
            .any(|token| matches!(*token, "checkout" | "codebase" | "workspace"))
}

fn contains_explicit_non_workspace_environment_change(objective: &str) -> bool {
    const CHANGE_VERBS: &[&str] = &[
        "change",
        "configure",
        "fix",
        "install",
        "modify",
        "patch",
        "reinstall",
        "remove",
        "repair",
        "replace",
        "uninstall",
        "update",
        "upgrade",
    ];
    const ENVIRONMENT_SCOPES: &[&str] = &[
        "global",
        "globally",
        "host",
        "installed",
        "machine",
        "operating-system",
        "system",
        "system-wide",
        "systemwide",
    ];
    const ENVIRONMENT_TARGETS: &[&str] = &[
        "binary",
        "command",
        "compiler",
        "configuration",
        "dependency",
        "environment",
        "installation",
        "interpreter",
        "library",
        "package",
        "runtime",
        "service",
        "tool",
    ];
    const LOCAL_ARTIFACTS: &[&str] = &[
        "adapter",
        "client",
        "component",
        "crate",
        "handler",
        "integration",
        "method",
        "module",
        "parser",
        "plugin",
        "source",
        "test",
        "workflow",
    ];

    let prose = prose_without_code(objective).to_ascii_lowercase();
    let tokens = prose
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '-')
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    if contains_explicit_local_scope(&prose, &tokens)
        || [
            "代码库",
            "工作区",
            "当前项目",
            "本仓库",
            "本项目",
            "项目源码",
        ]
        .iter()
        .any(|scope| objective.contains(scope))
    {
        return false;
    }

    let english_scope = tokens.iter().enumerate().any(|(target_index, target)| {
        if !ENVIRONMENT_TARGETS.contains(target) {
            return false;
        }
        if tokens[target_index.saturating_add(1)..tokens.len().min(target_index.saturating_add(3))]
            .iter()
            .any(|token| LOCAL_ARTIFACTS.contains(token))
        {
            return false;
        }
        let change_index = tokens[..target_index]
            .iter()
            .rposition(|token| CHANGE_VERBS.contains(token));
        let scope_start = target_index.saturating_sub(4);
        change_index.is_some_and(|index| target_index.saturating_sub(index) <= 6)
            && tokens[scope_start..target_index]
                .iter()
                .any(|token| ENVIRONMENT_SCOPES.contains(token))
    });
    if english_scope {
        return true;
    }

    let cjk_change = [
        "修复", "修改", "卸载", "安装", "替换", "更新", "升级", "移除", "配置", "重装",
    ]
    .iter()
    .any(|marker| objective.contains(marker));
    let cjk_scope = ["全局安装", "宿主机", "操作系统", "系统安装", "系统环境"]
        .iter()
        .any(|marker| objective.contains(marker));
    let cjk_target = [
        "依赖",
        "二进制",
        "服务",
        "环境",
        "解释器",
        "命令",
        "软件包",
        "运行时",
        "工具",
        "编译器",
    ]
    .iter()
    .any(|marker| objective.contains(marker));
    let cjk_local_artifact = [
        "代码",
        "客户端",
        "模块",
        "插件",
        "测试",
        "适配器",
        "项目源码",
    ]
    .iter()
    .any(|marker| objective.contains(marker));
    cjk_change && cjk_scope && cjk_target && !cjk_local_artifact
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
