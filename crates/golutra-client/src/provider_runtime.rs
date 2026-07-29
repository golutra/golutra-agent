//! Provider 解析与 runtime 执行计划构造。

use std::time::Duration;

use golutra_config::{
    ProviderConfigPaths, ProviderRuntimeEnv, load_provider_runtime_env_from_paths,
};
use golutra_context::{ContextBudgetPolicy, ContextBuilder};
use golutra_llm::{
    ConfiguredProvider, MockProvider, ProviderError, ProviderProtocol, protocol_capabilities,
};
use golutra_runtime::ProviderSessionPolicy;
use serde_json::{Value, json};

use crate::LegacyTaskAdapter;

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
            LegacyTaskAdapter::new(payload, objective).requests_workspace_tools(),
        );
    }
    let lower = objective.to_ascii_lowercase();
    let legacy = LegacyTaskAdapter::new(payload, objective);
    if legacy.requests_workspace_change() {
        let write_args = legacy.write_file_args();
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
        legacy.requests_workspace_tools(),
    )
}

pub(crate) fn isolated_mock_provider_plan(
    payload: &Value,
    objective: &str,
) -> Result<MockProviderPlan, ProviderError> {
    let lower = objective.to_ascii_lowercase();
    let (mock, touched_code, workspace_tools_enabled) =
        if LegacyTaskAdapter::new(payload, objective).requests_workspace_change() {
            let write_args = LegacyTaskAdapter::new(payload, objective).write_file_args();
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
                LegacyTaskAdapter::new(payload, objective).requests_workspace_tools(),
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
