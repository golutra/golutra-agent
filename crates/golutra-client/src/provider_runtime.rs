//! Provider 解析与 runtime 执行计划构造。

use std::time::Duration;

use golutra_config::{
    ProviderConfigPaths, ProviderRuntimeEnv, load_merged_provider_settings,
    load_provider_runtime_env_for_profile_from_paths, load_provider_runtime_env_from_paths,
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

pub(crate) fn pin_provider_turn_settings(
    provider_config_paths: Option<&ProviderConfigPaths>,
    payload: &mut Value,
) {
    let Some(paths) = provider_config_paths else {
        return;
    };
    // Invalid or incomplete provider configuration is handled by the runtime's
    // authentication flow. Only pin a binding when it can be resolved now.
    let Ok(settings) = load_merged_provider_settings(paths) else {
        return;
    };
    let requested_profile = match payload.get("provider_profile") {
        None => None,
        Some(Value::String(profile)) if !profile.trim().is_empty() => Some(profile.trim()),
        Some(_) => return,
    };
    let profile = match requested_profile {
        Some(requested_profile) => settings
            .profiles
            .iter()
            .find(|profile| profile.enabled && profile.name == requested_profile),
        None => settings.active_profile(),
    };
    let Some(profile) = profile else {
        return;
    };

    payload["provider_profile"] = Value::String(profile.name.clone());
    if payload.get("provider_model").is_none()
        && let Some(model_id) = &profile.model_id
    {
        payload["provider_model"] = Value::String(model_id.clone());
    }
    if payload.get("provider_generation_config").is_none() {
        payload["provider_generation_config"] = profile
            .generation_config
            .as_ref()
            .map_or_else(|| json!({}), |config| json!(config));
    }
}

pub(crate) fn mock_provider_plan(
    provider_config_paths: Option<&ProviderConfigPaths>,
    payload: &Value,
    objective: &str,
) -> Result<MockProviderPlan, ProviderError> {
    let provider_env = provider_runtime_env_for_payload(provider_config_paths, payload)?;
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
            // Keep the deterministic mock below the shell tool's five-second
            // default instead of racing process completion against its timeout.
            MockProvider::tool_call("shell", json!({"command": "sleep 1"})),
            false,
            true,
        );
    }

    if lower.contains("list") || lower.contains("ls") {
        return configured_provider_plan(
            provider_env.as_ref(),
            MockProvider::tool_call(
                "shell",
                json!({
                    "command": "ls -la",
                    "workdir": string_payload(payload, "path", ".")
                }),
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

fn provider_runtime_env_for_payload(
    provider_config_paths: Option<&ProviderConfigPaths>,
    payload: &Value,
) -> Result<Option<ProviderRuntimeEnv>, ProviderError> {
    let profile_name = optional_bounded_string(payload, "provider_profile", 128)?;
    let model_id = optional_bounded_string(payload, "provider_model", 256)?;
    let generation_config = payload
        .get("provider_generation_config")
        .map(|value| {
            serde_json::from_value::<golutra_llm::ProviderGenerationConfig>(value.clone())
                .map_err(|error| ProviderError::NotConfigured {
                    message: format!("provider generation override is invalid: {error}"),
                })
                .and_then(|config| {
                    config
                        .validate()
                        .map_err(|message| ProviderError::NotConfigured { message })?;
                    serde_json::to_string(&config).map_err(|error| ProviderError::NotConfigured {
                        message: format!(
                            "provider generation override could not be encoded: {error}"
                        ),
                    })
                })
        })
        .transpose()?;

    let Some(paths) = provider_config_paths else {
        if profile_name.is_some() || model_id.is_some() || generation_config.is_some() {
            return Err(ProviderError::NotConfigured {
                message: "provider overrides require configured provider paths".to_owned(),
            });
        }
        return Ok(None);
    };
    let mut environment = match profile_name {
        Some(profile_name) => {
            load_provider_runtime_env_for_profile_from_paths(paths, &profile_name)
        }
        None => load_provider_runtime_env_from_paths(paths),
    }
    .map_err(|error| ProviderError::NotConfigured {
        message: format!("provider configuration could not be loaded: {error}"),
    })?;
    if let Some(model_id) = model_id {
        environment = environment.with_runtime_override("GOLUTRA_PROVIDER_MODEL", model_id);
    }
    if let Some(generation_config) = generation_config {
        environment = environment
            .with_runtime_override("GOLUTRA_PROVIDER_GENERATION_CONFIG", generation_config);
    }
    Ok(Some(environment))
}

fn optional_bounded_string(
    payload: &Value,
    key: &str,
    max_chars: usize,
) -> Result<Option<String>, ProviderError> {
    let Some(value) = payload.get(key) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .ok_or_else(|| ProviderError::NotConfigured {
            message: format!("{key} must be a string"),
        })?
        .trim();
    if value.is_empty() || value.chars().count() > max_chars {
        return Err(ProviderError::NotConfigured {
            message: format!("{key} must contain between 1 and {max_chars} characters"),
        });
    }
    Ok(Some(value.to_owned()))
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
                    "shell",
                    json!({
                        "command": "ls -la",
                        "workdir": string_payload(payload, "path", ".")
                    }),
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

#[cfg(test)]
mod tests {
    use golutra_auth::{CredentialRef, SecretKind};
    use golutra_config::{ProviderProfile, ProviderSettings};
    use golutra_llm::ProviderReasoningEffort;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn provider_turn_binding_pins_defaults_without_overwriting_explicit_overrides() {
        let home = tempdir().expect("home");
        let paths = ProviderConfigPaths::from_home(home.path()).expect("paths");
        let mut profile = ProviderProfile::mock();
        profile.name = "primary".to_owned();
        profile.model_id = Some("configured-model".to_owned());
        profile.generation_config = Some(
            serde_json::from_value(json!({"reasoning_effort": "high"})).expect("generation config"),
        );
        let mut settings = ProviderSettings::default();
        settings.upsert_profile(profile, true);
        settings.save(&paths.user_config).expect("save settings");

        let mut defaults = json!({"prompt": "hello"});
        pin_provider_turn_settings(Some(&paths), &mut defaults);
        assert_eq!(defaults["provider_profile"], "primary");
        assert_eq!(defaults["provider_model"], "configured-model");
        assert_eq!(
            defaults["provider_generation_config"],
            json!({"reasoning_effort": "high"})
        );

        let mut overridden = json!({
            "provider_model": "turn-model",
            "provider_generation_config": {"reasoning_effort": "low"},
        });
        pin_provider_turn_settings(Some(&paths), &mut overridden);
        assert_eq!(overridden["provider_profile"], "primary");
        assert_eq!(overridden["provider_model"], "turn-model");
        assert_eq!(
            overridden["provider_generation_config"],
            json!({"reasoning_effort": "low"})
        );

        let mut malformed = json!({"provider_profile": 7});
        pin_provider_turn_settings(Some(&paths), &mut malformed);
        assert_eq!(malformed, json!({"provider_profile": 7}));

        let mut without_paths = json!({"prompt": "hello"});
        pin_provider_turn_settings(None, &mut without_paths);
        assert_eq!(without_paths, json!({"prompt": "hello"}));
    }

    #[test]
    fn task_provider_overrides_are_ephemeral_and_validated() {
        let home = tempdir().expect("home");
        let paths = ProviderConfigPaths::from_home(home.path()).expect("paths");
        let profile = ProviderProfile::openai_compatible(
            "primary",
            "https://api.example.com/v1",
            "configured-model",
            CredentialRef::ephemeral(SecretKind::ApiKey),
        )
        .expect("profile");
        let mut settings = ProviderSettings::default();
        settings.upsert_profile(profile, true);
        settings.save(&paths.user_config).expect("save settings");

        let environment = provider_runtime_env_for_payload(
            Some(&paths),
            &json!({
                "provider_profile": "primary",
                "provider_model": "session-model",
                "provider_generation_config": {
                    "reasoning_effort": "high"
                }
            }),
        )
        .expect("provider environment")
        .expect("configured environment");

        assert_eq!(
            environment.get("GOLUTRA_PROVIDER_MODEL").as_deref(),
            Some("session-model")
        );
        let generation = environment
            .get("GOLUTRA_PROVIDER_GENERATION_CONFIG")
            .and_then(|value| {
                serde_json::from_str::<golutra_llm::ProviderGenerationConfig>(&value).ok()
            })
            .expect("generation config");
        assert_eq!(
            generation.reasoning_effort,
            Some(ProviderReasoningEffort::High)
        );
        assert_eq!(
            ProviderSettings::load(&paths.user_config)
                .expect("persisted settings")
                .active_profile()
                .and_then(|profile| profile.model_id.as_deref()),
            Some("configured-model")
        );

        assert!(
            provider_runtime_env_for_payload(Some(&paths), &json!({"provider_model": ""})).is_err()
        );
        assert!(
            provider_runtime_env_for_payload(
                Some(&paths),
                &json!({"provider_generation_config": {"max_tokens": 0}})
            )
            .is_err()
        );
    }
}
