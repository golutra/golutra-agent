//! Provider 解析与 runtime 执行计划构造。

use std::time::Duration;

use golutra_config::{
    ProviderConfigPaths, ProviderRuntimeEnv, load_provider_runtime_env_from_paths,
};
use golutra_context::{ContextBudgetPolicy, ContextBuilder};
use golutra_core::{
    TaskContract, VerificationRequirement, WorkspaceChangeRequirement, infer_legacy_write_content,
    infer_legacy_write_path,
};
use golutra_llm::{
    ConfiguredProvider, MockProvider, ProviderError, ProviderProtocol, protocol_capabilities,
};
use golutra_runtime::ProviderSessionPolicy;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub(crate) struct MockProviderPlan {
    pub(crate) provider: ConfiguredProvider,
    pub(crate) fallback_provider: Option<ConfiguredProvider>,
    pub(crate) touched_code: bool,
    pub(crate) workspace_tools_enabled: bool,
    pub(crate) context_builder: ContextBuilder,
    pub(crate) provider_session_policy: ProviderSessionPolicy,
}

pub(crate) fn mock_provider_plan(
    provider_config_paths: Option<&ProviderConfigPaths>,
    payload: &Value,
    objective: &str,
) -> Result<MockProviderPlan, ProviderError> {
    let provider_env = provider_config_paths
        .map(load_provider_runtime_env_from_paths)
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
    if legacy_task_requests_workspace_change(payload, objective) {
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

/// Preserve the old adapter behavior for clients that do not send a
/// `TaskContract`, while recognizing the coding verbs used by non-English
/// clients as well. This is only a compatibility adapter: explicit contracts
/// remain authoritative.
pub(crate) fn legacy_task_requests_workspace_change(payload: &Value, objective: &str) -> bool {
    payload.get("content").is_some()
        || payload.get("patch").is_some()
        || payload.get("replacement").is_some()
        || contains_change_verb(objective)
}

/// Return a delivery path only when the legacy request makes it explicit.
/// Broad requests such as "refactor the runtime" still require a workspace
/// change, but must not invent `golutra-agent-output.txt` as a contract path.
pub(crate) fn legacy_task_required_path(payload: &Value, objective: &str) -> Option<String> {
    if !legacy_task_requests_workspace_change(payload, objective) {
        return None;
    }
    non_empty_string_payload(payload, "path").or_else(|| infer_legacy_write_path(objective))
}

/// Apply the compatibility contract once at the command boundary and reuse
/// the same rule for recovered tasks and deterministic replay.
pub(crate) fn apply_legacy_task_contract(
    payload: &Value,
    objective: &str,
    contract: &mut TaskContract,
) -> bool {
    if !legacy_task_requests_workspace_change(payload, objective) {
        return false;
    }
    contract.workspace_change = WorkspaceChangeRequirement::Required;
    contract.require_objective_validation = true;
    if let Some(requested_path) = legacy_task_required_path(payload, objective)
        && !contract.required_paths.contains(&requested_path)
    {
        contract.required_paths.push(requested_path);
    }
    if contract.verification == VerificationRequirement::BestEffort {
        contract.verification = VerificationRequirement::Required;
    }
    true
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

pub(crate) fn isolated_mock_provider_plan(
    payload: &Value,
    objective: &str,
) -> Result<MockProviderPlan, ProviderError> {
    let lower = objective.to_ascii_lowercase();
    let (mock, touched_code, workspace_tools_enabled) =
        if legacy_task_requests_workspace_change(payload, objective) {
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
        provider_session_policy: ProviderSessionPolicy::default(),
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
        provider_session_policy: provider_session_policy_from_env(provider_env)?,
    })
}

fn provider_session_policy_from_env(
    provider_env: Option<&ProviderRuntimeEnv>,
) -> Result<ProviderSessionPolicy, ProviderError> {
    let mut policy = ProviderSessionPolicy::default();
    if let Some(value) = provider_runtime_value(provider_env, "GOLUTRA_PROVIDER_STREAM_MAX_RETRIES")
    {
        policy.max_stream_retries = parse_provider_u32("stream max retries", &value)?;
    }
    if let Some(value) =
        provider_runtime_value(provider_env, "GOLUTRA_PROVIDER_REQUEST_MAX_RETRIES")
    {
        policy.max_request_retries = parse_provider_u32("request max retries", &value)?;
    }
    if let Some(value) =
        provider_runtime_value(provider_env, "GOLUTRA_PROVIDER_STREAM_IDLE_TIMEOUT_MS")
    {
        policy.stream_idle_timeout =
            Duration::from_millis(parse_provider_u64("stream idle timeout", &value)?);
    }
    if let Some(value) = provider_runtime_value(provider_env, "GOLUTRA_PROVIDER_REQUEST_TIMEOUT_MS")
    {
        policy.request_timeout =
            Duration::from_millis(parse_provider_u64("request timeout", &value)?);
    }
    if let Some(value) = provider_runtime_value(provider_env, "GOLUTRA_PROVIDER_TRANSPORT_FALLBACK")
    {
        policy.enable_transport_fallback = match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => {
                return Err(ProviderError::NotConfigured {
                    message: "provider transport fallback must be a boolean".to_owned(),
                });
            }
        };
    }
    Ok(policy.bounded())
}

fn provider_runtime_value(provider_env: Option<&ProviderRuntimeEnv>, key: &str) -> Option<String> {
    provider_env
        .and_then(|environment| environment.get(key))
        .or_else(|| std::env::var(key).ok())
        .filter(|value| !value.trim().is_empty())
}

fn parse_provider_u32(label: &str, value: &str) -> Result<u32, ProviderError> {
    value
        .trim()
        .parse::<u32>()
        .map_err(|_| ProviderError::NotConfigured {
            message: format!("provider {label} must be a non-negative integer"),
        })
}

fn parse_provider_u64(label: &str, value: &str) -> Result<u64, ProviderError> {
    let value = value
        .trim()
        .parse::<u64>()
        .map_err(|_| ProviderError::NotConfigured {
            message: format!("provider {label} must be a positive integer"),
        })?;
    if value == 0 {
        return Err(ProviderError::NotConfigured {
            message: format!("provider {label} must be greater than zero"),
        });
    }
    Ok(value)
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
    MockWriteFileArgs {
        path: non_empty_string_payload(payload, "path")
            .or_else(|| infer_legacy_write_path(objective))
            .unwrap_or_else(|| "golutra-agent-output.txt".to_owned()),
        content: non_empty_string_payload(payload, "content")
            .or_else(|| infer_legacy_write_content(objective))
            .unwrap_or_else(|| "done\n".to_owned()),
    }
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
