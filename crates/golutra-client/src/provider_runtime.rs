//! Provider 解析与 runtime 执行计划构造。

use golutra_config::{
    ProviderConfigPaths, ProviderRuntimeEnv, load_provider_runtime_env_from_paths,
};
use golutra_context::{ContextBudgetPolicy, ContextBuilder};
use golutra_llm::{
    ConfiguredProvider, MockProvider, ProviderError, ProviderProtocol, protocol_capabilities,
};
use serde_json::{Value, json};

use super::RuntimePaths;

#[derive(Debug, Clone)]
pub(crate) struct MockProviderPlan {
    pub(crate) provider: ConfiguredProvider,
    pub(crate) fallback_provider: Option<ConfiguredProvider>,
    pub(crate) touched_code: bool,
    pub(crate) workspace_tools_enabled: bool,
    pub(crate) context_builder: ContextBuilder,
}

pub(crate) fn mock_provider_plan(
    runtime_paths: Option<&RuntimePaths>,
    payload: &Value,
    objective: &str,
) -> Result<MockProviderPlan, ProviderError> {
    let provider_env = runtime_paths
        .map(|paths| {
            let config_paths = ProviderConfigPaths::from_home(&paths.home)?;
            load_provider_runtime_env_from_paths(&config_paths)
        })
        .transpose()
        .map_err(|error| ProviderError::NotConfigured {
            message: format!("provider configuration could not be loaded: {error}"),
        })?;
    #[cfg(test)]
    if payload
        .get("mock_provider_failure")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return configured_provider_plan(
            provider_env.as_ref(),
            MockProvider::failure("forced mock provider failure"),
            false,
            prompt_requests_workspace_tools(payload, objective),
        );
    }
    let lower = objective.to_ascii_lowercase();
    if lower.contains("write") || lower.contains("create") || payload.get("content").is_some() {
        let write_args = mock_write_file_args(payload, objective);
        return configured_provider_plan(
            provider_env.as_ref(),
            MockProvider::tool_call(
                "write_file",
                json!({
                    "path": write_args.path,
                    "content": write_args.content,
                }),
            ),
            true,
            true,
        );
    }

    if lower.contains("read") {
        return configured_provider_plan(
            provider_env.as_ref(),
            MockProvider::tool_call(
                "read_file",
                json!({"path": string_payload(payload, "path", "README.md")}),
            ),
            false,
            true,
        );
    }

    if lower.contains("sleep") {
        return configured_provider_plan(
            provider_env.as_ref(),
            MockProvider::tool_call("shell", json!({"command": "sleep 5"})),
            false,
            true,
        );
    }

    if lower.contains("list") || lower.contains("ls") {
        return configured_provider_plan(
            provider_env.as_ref(),
            MockProvider::tool_call(
                "list_dir",
                json!({"path": string_payload(payload, "path", ".")}),
            ),
            false,
            true,
        );
    }

    configured_provider_plan(
        provider_env.as_ref(),
        MockProvider::text_response("mock provider completed without tool calls"),
        false,
        prompt_requests_workspace_tools(payload, objective),
    )
}

pub(crate) fn isolated_mock_provider_plan(
    payload: &Value,
    objective: &str,
) -> Result<MockProviderPlan, ProviderError> {
    let lower = objective.to_ascii_lowercase();
    let (mock, touched_code, workspace_tools_enabled) = if lower.contains("write")
        || lower.contains("create")
        || payload.get("content").is_some()
    {
        let write_args = mock_write_file_args(payload, objective);
        (
            MockProvider::tool_call(
                "write_file",
                json!({"path": write_args.path, "content": write_args.content}),
            ),
            true,
            true,
        )
    } else if lower.contains("read") {
        (
            MockProvider::tool_call(
                "read_file",
                json!({"path": string_payload(payload, "path", "README.md")}),
            ),
            false,
            true,
        )
    } else if lower.contains("list") || lower.contains("ls") {
        (
            MockProvider::tool_call(
                "list_dir",
                json!({"path": string_payload(payload, "path", ".")}),
            ),
            false,
            true,
        )
    } else {
        (
            MockProvider::text_response("isolated mock provider completed the generated task"),
            false,
            prompt_requests_workspace_tools(payload, objective),
        )
    };
    Ok(MockProviderPlan {
        provider: ConfiguredProvider::Mock(Box::new(mock)),
        fallback_provider: None,
        touched_code,
        workspace_tools_enabled,
        context_builder: ContextBuilder::default(),
    })
}

pub(crate) fn configured_provider_plan(
    provider_env: Option<&golutra_config::ProviderRuntimeEnv>,
    mock: MockProvider,
    touched_code: bool,
    workspace_tools_enabled: bool,
) -> Result<MockProviderPlan, ProviderError> {
    let provider = resolve_configured_provider(provider_env, mock.clone())?;
    let workspace_tools_enabled =
        workspace_tools_enabled || !matches!(&provider, ConfiguredProvider::Mock(_));
    let fallback_provider = provider_env
        .and_then(|environment| environment.get("GOLUTRA_PROVIDER_FALLBACK_PROTOCOL"))
        .or_else(|| std::env::var("GOLUTRA_PROVIDER_FALLBACK_PROTOCOL").ok())
        .filter(|protocol| protocol.eq_ignore_ascii_case("mock"))
        .filter(|_| !matches!(&provider, ConfiguredProvider::Mock(_)))
        .map(|_| ConfiguredProvider::Mock(Box::new(mock)));
    Ok(MockProviderPlan {
        provider,
        fallback_provider,
        touched_code,
        workspace_tools_enabled,
        context_builder: context_builder_from_provider_env(provider_env)?,
    })
}

fn context_builder_from_provider_env(
    provider_env: Option<&golutra_config::ProviderRuntimeEnv>,
) -> Result<ContextBuilder, ProviderError> {
    let protocol = provider_env
        .and_then(|environment| environment.get("GOLUTRA_PROVIDER_PROTOCOL"))
        .and_then(|value| ProviderProtocol::from_config_value(&value))
        .or_else(|| {
            std::env::var("GOLUTRA_PROVIDER_PROTOCOL")
                .ok()
                .and_then(|value| ProviderProtocol::from_config_value(&value))
        });
    let declared_capabilities = protocol.map(protocol_capabilities);
    let Some(raw_config) = provider_env
        .and_then(|environment| environment.get("GOLUTRA_PROVIDER_GENERATION_CONFIG"))
        .or_else(|| std::env::var("GOLUTRA_PROVIDER_GENERATION_CONFIG").ok())
        .filter(|value| !value.trim().is_empty())
    else {
        let Some(capabilities) = declared_capabilities else {
            return Ok(ContextBuilder::default());
        };
        let context_window = capabilities
            .context_window
            .ok_or_else(missing_context_window_error)?;
        let max_output = capabilities.max_output_tokens.unwrap_or(1_024);
        let budget_limit = context_window
            .checked_sub(max_output)
            .filter(|budget| *budget > 0)
            .ok_or_else(|| ProviderError::NotConfigured {
                message: "provider context window cannot retain the configured output budget"
                    .to_owned(),
            })?;
        return Ok(ContextBuilder::new(ContextBudgetPolicy {
            context_window,
            max_output,
            budget_limit,
            ..ContextBudgetPolicy::default()
        }));
    };
    let config: golutra_llm::ProviderGenerationConfig =
        serde_json::from_str(&raw_config).map_err(|error| ProviderError::NotConfigured {
            message: format!("provider generation config is invalid JSON: {error}"),
        })?;
    config
        .validate()
        .map_err(|message| ProviderError::NotConfigured { message })?;
    let mut policy = ContextBudgetPolicy::default();
    policy.context_window = config
        .context_window_size
        .or_else(|| {
            declared_capabilities
                .as_ref()
                .and_then(|capabilities| capabilities.context_window)
        })
        .ok_or_else(missing_context_window_error)?;
    policy.max_output = config
        .max_tokens
        .or_else(|| {
            declared_capabilities
                .as_ref()
                .and_then(|capabilities| capabilities.max_output_tokens)
        })
        .unwrap_or(1_024);
    policy.budget_limit = policy
        .context_window
        .checked_sub(policy.max_output)
        .filter(|budget| *budget > 0)
        .ok_or_else(|| ProviderError::NotConfigured {
            message: "provider max_tokens must be smaller than the effective context window"
                .to_owned(),
        })?;
    Ok(ContextBuilder::new(policy))
}

fn missing_context_window_error() -> ProviderError {
    ProviderError::NotConfigured {
        message: "provider context window is unknown; configure context_window_size explicitly"
            .to_owned(),
    }
}

pub(crate) fn prompt_requests_workspace_tools(payload: &Value, objective: &str) -> bool {
    if payload.get("path").is_some()
        || payload.get("content").is_some()
        || payload.get("command").is_some()
    {
        return true;
    }

    let lower = objective.to_ascii_lowercase();
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
        || CJK_MARKERS.iter().any(|marker| objective.contains(marker))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MockWriteFileArgs {
    pub(crate) path: String,
    pub(crate) content: String,
}

pub(crate) fn mock_write_file_args(payload: &Value, objective: &str) -> MockWriteFileArgs {
    let parsed = parse_mock_write_file_prompt(objective);
    MockWriteFileArgs {
        path: non_empty_string_payload(payload, "path")
            .or_else(|| parsed.as_ref().map(|parsed| parsed.path.clone()))
            .unwrap_or_else(|| "golutra-agent-output.txt".to_owned()),
        content: non_empty_string_payload(payload, "content")
            .or_else(|| parsed.map(|parsed| parsed.content))
            .unwrap_or_else(|| "done\n".to_owned()),
    }
}

fn parse_mock_write_file_prompt(objective: &str) -> Option<MockWriteFileArgs> {
    let objective = objective.trim();
    let lower = objective.to_ascii_lowercase();
    let marker = " with content ";
    let marker_index = lower.find(marker)?;
    let (path_part, content_part_with_marker) = objective.split_at(marker_index);
    let content = clean_mock_prompt_segment(&content_part_with_marker[marker.len()..]);
    let path = parse_mock_write_path(path_part)?;
    if content.is_empty() {
        return None;
    }
    Some(MockWriteFileArgs { path, content })
}

fn parse_mock_write_path(path_part: &str) -> Option<String> {
    let tokens = path_part.split_whitespace().collect::<Vec<_>>();
    let command_index = tokens
        .iter()
        .position(|token| matches!(token.to_ascii_lowercase().as_str(), "write" | "create"))?;
    let candidate = match tokens
        .get(command_index + 1)
        .map(|token| token.to_ascii_lowercase())
    {
        Some(value) if value == "file" => tokens.get(command_index + 2),
        Some(_) => tokens.get(command_index + 1),
        None => None,
    }?;
    let path = clean_mock_prompt_segment(candidate);
    if path.is_empty() { None } else { Some(path) }
}

fn clean_mock_prompt_segment(value: &str) -> String {
    value
        .trim()
        .trim_matches(|character| matches!(character, '"' | '\'' | '`' | ',' | ';' | ':'))
        .to_owned()
}

fn resolve_configured_provider(
    provider_env: Option<&ProviderRuntimeEnv>,
    mock: MockProvider,
) -> Result<ConfiguredProvider, ProviderError> {
    if let Some(provider_env) = provider_env {
        ConfiguredProvider::resolve_from_reader_with_credential(
            mock,
            |key| provider_env.get(key),
            provider_env.credential_provider(),
        )
    } else {
        ConfiguredProvider::resolve_from_env(mock)
    }
}

fn string_payload(payload: &Value, key: &str, fallback: &str) -> String {
    non_empty_string_payload(payload, key).unwrap_or_else(|| fallback.to_owned())
}

fn non_empty_string_payload(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}
