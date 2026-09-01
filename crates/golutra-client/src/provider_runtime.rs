//! Provider 解析与 runtime 执行计划构造。

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use golutra_config::{
    ProviderConfigPaths, ProviderRuntimeEnv, ProviderSettings, load_merged_provider_settings,
    load_provider_runtime_env_for_profile_from_paths, load_provider_runtime_env_from_paths,
};
use golutra_context::{ContextBudgetPolicy, ContextBuilder};
use golutra_llm::{
    ConfiguredProvider, MockProvider, ProviderError, ProviderProtocol, protocol_capabilities,
};
use golutra_runtime::ProviderSessionPolicy;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{LegacyTaskAdapter, file_identity::path_metadata_fingerprint};

#[derive(Debug, Clone)]
pub(crate) struct MockProviderPlan {
    pub(crate) provider: ConfiguredProvider,
    pub(crate) fallback_provider: Option<ConfiguredProvider>,
    pub(crate) touched_code: bool,
    pub(crate) workspace_tools_enabled: bool,
    pub(crate) context_builder: ContextBuilder,
    pub(crate) provider_session_policy: ProviderSessionPolicy,
}

/// Host-local provider route cache.
///
/// Provider construction creates HTTP/genai clients and resolves the provider
/// settings store. Those resources are stable across turns, while the mock
/// outcome and task capability flags are not. Keep only the stable route here
/// and rebuild the task-specific portion for every request.
#[derive(Debug, Default)]
pub(crate) struct ProviderRouteCache {
    entries: HashMap<String, CachedProviderRoute>,
    settings_snapshot: Option<CachedProviderSettings>,
    clock: u64,
    hits: u64,
    misses: u64,
}

#[derive(Debug, Clone)]
struct CachedProviderRoute {
    provider: ConfiguredProvider,
    context_builder: ContextBuilder,
    provider_session_policy: ProviderSessionPolicy,
    fallback_to_mock: bool,
    last_used: u64,
}

#[derive(Debug, Clone)]
struct CachedProviderSettings {
    path: PathBuf,
    fingerprint: String,
    settings: ProviderSettings,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProviderRouteCacheStats {
    pub(crate) entries: usize,
    pub(crate) hits: u64,
    pub(crate) misses: u64,
}

const MAX_CACHED_PROVIDER_ROUTES: usize = 8;

impl ProviderRouteCache {
    fn next_tick(&mut self) -> u64 {
        self.clock = self.clock.saturating_add(1);
        self.clock
    }

    fn get(&mut self, key: &str) -> Option<CachedProviderRoute> {
        let tick = self.next_tick();
        let route = self.entries.get_mut(key)?;
        self.hits = self.hits.saturating_add(1);
        route.last_used = tick;
        Some(route.clone())
    }

    fn insert(&mut self, key: String, mut route: CachedProviderRoute) {
        route.last_used = self.next_tick();
        self.entries.insert(key, route);
        while self.entries.len() > MAX_CACHED_PROVIDER_ROUTES {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, route)| route.last_used)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    fn record_miss(&mut self) {
        self.misses = self.misses.saturating_add(1);
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> ProviderRouteCacheStats {
        ProviderRouteCacheStats {
            entries: self.entries.len(),
            hits: self.hits,
            misses: self.misses,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.settings_snapshot = None;
    }

    fn settings(&mut self, paths: &ProviderConfigPaths) -> Result<ProviderSettings, ProviderError> {
        let fingerprint = provider_settings_fingerprint(&paths.user_config);
        if let Some(snapshot) = self.settings_snapshot.as_ref()
            && snapshot.path == paths.user_config
            && snapshot.fingerprint == fingerprint
        {
            return Ok(snapshot.settings.clone());
        }
        let settings =
            load_merged_provider_settings(paths).map_err(|error| ProviderError::NotConfigured {
                message: format!("provider configuration could not be loaded: {error}"),
            })?;
        self.settings_snapshot = Some(CachedProviderSettings {
            path: paths.user_config.clone(),
            fingerprint,
            settings: settings.clone(),
        });
        Ok(settings)
    }
}

#[cfg(test)]
pub(crate) fn pin_provider_turn_settings(
    provider_config_paths: Option<&ProviderConfigPaths>,
    payload: &mut Value,
) {
    let mut cache = ProviderRouteCache::default();
    pin_provider_turn_settings_cached(&mut cache, provider_config_paths, payload);
}

/// 从 host 本地配置快照固定入队时的 provider 绑定。配置文件身份变化会精确
/// 失效快照，连续回合无需重复加锁和解析，同时仍能观察外部编辑。
pub(crate) fn pin_provider_turn_settings_cached(
    cache: &mut ProviderRouteCache,
    provider_config_paths: Option<&ProviderConfigPaths>,
    payload: &mut Value,
) {
    let Some(paths) = provider_config_paths else {
        return;
    };
    // 无效或不完整配置由 runtime 的认证流程处理；这里只固定当前可解析的绑定。
    let Ok(settings) = cache.settings(paths) else {
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

#[cfg(test)]
pub(crate) fn mock_provider_plan(
    provider_config_paths: Option<&ProviderConfigPaths>,
    payload: &Value,
    objective: &str,
) -> Result<MockProviderPlan, ProviderError> {
    let provider_env = provider_runtime_env_for_payload(provider_config_paths, payload)?;
    let (mock, touched_code, workspace_tools_enabled) = task_mock_provider(payload, objective);
    configured_provider_plan(
        provider_env.as_ref(),
        mock,
        touched_code,
        workspace_tools_enabled,
    )
}

/// Resolve a task plan while reusing the host-local live provider route.
///
/// The mock response is intentionally created on every call: tests and legacy
/// adapters use it to model the current objective and it must never leak into
/// a later turn. Only a configured live provider is retained in the cache.
pub(crate) fn cached_mock_provider_plan(
    cache: &mut ProviderRouteCache,
    provider_config_paths: Option<&ProviderConfigPaths>,
    payload: &Value,
    objective: &str,
) -> Result<MockProviderPlan, ProviderError> {
    let key = provider_route_cache_key(provider_config_paths, payload)?;
    let (mock, touched_code, workspace_tools_enabled) = task_mock_provider(payload, objective);
    if let Some(route) = cache.get(&key) {
        let fallback_provider = route
            .fallback_to_mock
            .then(|| ConfiguredProvider::Mock(Box::new(mock)));
        let workspace_tools_enabled =
            workspace_tools_enabled || !matches!(&route.provider, ConfiguredProvider::Mock(_));
        return Ok(MockProviderPlan {
            provider: route.provider,
            fallback_provider,
            touched_code,
            workspace_tools_enabled,
            context_builder: route.context_builder,
            provider_session_policy: route.provider_session_policy,
        });
    }
    cache.record_miss();

    let provider_env = provider_runtime_env_for_payload(provider_config_paths, payload)?;
    let plan = configured_provider_plan(
        provider_env.as_ref(),
        mock,
        touched_code,
        workspace_tools_enabled,
    )?;
    if !matches!(&plan.provider, ConfiguredProvider::Mock(_)) {
        cache.insert(
            key,
            CachedProviderRoute {
                provider: plan.provider.clone(),
                context_builder: plan.context_builder.clone(),
                provider_session_policy: plan.provider_session_policy,
                fallback_to_mock: plan.fallback_provider.is_some(),
                last_used: 0,
            },
        );
    }
    Ok(plan)
}

fn task_mock_provider(payload: &Value, objective: &str) -> (MockProvider, bool, bool) {
    #[cfg(test)]
    if payload
        .get("mock_provider_failure")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return (
            MockProvider::failure("forced mock provider failure"),
            false,
            LegacyTaskAdapter::new(payload, objective).requests_workspace_tools(),
        );
    }
    let lower = objective.to_ascii_lowercase();
    let legacy = LegacyTaskAdapter::new(payload, objective);
    if legacy.requests_workspace_change() {
        let write_args = legacy.write_file_args();
        return (
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
        return (
            MockProvider::tool_call(
                "read_file",
                json!({"path": string_payload(payload, "path", "README.md")}),
            ),
            false,
            true,
        );
    }
    if lower.contains("sleep") {
        return (
            // 留出检查点落盘和外部 SIGINT 触发的窗口，同时让依赖该夹具的
            // 守护进程终态测试在有界时间内完成。
            MockProvider::tool_call(
                "shell",
                json!({"command": "sleep 10", "timeout_ms": 20_000}),
            ),
            false,
            true,
        );
    }
    if lower.contains("list") || lower.contains("ls") {
        return (
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
    (
        MockProvider::text_response("mock provider completed without tool calls"),
        false,
        legacy.requests_workspace_tools(),
    )
}

/// Build a bounded digest without placing provider credentials in memory
/// diagnostics or cache keys. The settings file metadata catches profile
/// edits; environment values are hashed so env-only routes also invalidate.
fn provider_route_cache_key(
    provider_config_paths: Option<&ProviderConfigPaths>,
    payload: &Value,
) -> Result<String, ProviderError> {
    let profile_name = optional_bounded_string(payload, "provider_profile", 128)?;
    let model_id = optional_bounded_string(payload, "provider_model", 256)?;
    let generation_config = generation_config_override(payload)?;
    let mut hasher = Sha256::new();
    hasher.update(b"golutra-provider-route-v1\0");
    digest_field(&mut hasher, "profile", profile_name.as_deref());
    digest_field(&mut hasher, "model", model_id.as_deref());
    digest_field(&mut hasher, "generation", generation_config.as_deref());
    if let Some(paths) = provider_config_paths {
        digest_field(
            &mut hasher,
            "settings",
            Some(&provider_settings_fingerprint(&paths.user_config)),
        );
        digest_field(&mut hasher, "home", paths.home.to_str());
    } else {
        digest_field(&mut hasher, "settings", None);
    }
    for key in ROUTE_ENV_KEYS {
        digest_field(&mut hasher, key, std::env::var(key).ok().as_deref());
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

const ROUTE_ENV_KEYS: &[&str] = &[
    "GOLUTRA_PROVIDER_MODE",
    "GOLUTRA_PROVIDER_PROTOCOL",
    "GOLUTRA_PROVIDER_API_KEY",
    "GOLUTRA_PROVIDER_API_KEY_ENV",
    "GOLUTRA_PROVIDER_MODEL",
    "GOLUTRA_PROVIDER_BASE_URL",
    "GOLUTRA_PROVIDER_GENERATION_CONFIG",
    "GOLUTRA_PROVIDER_CUSTOM_HEADERS",
    "GOLUTRA_PROVIDER_ROUTE_ID",
    "GOLUTRA_PROVIDER_AUTH_PROVIDER",
    "GOLUTRA_PROVIDER_FALLBACK_PROTOCOL",
    "GOLUTRA_PROVIDER_STREAM_MAX_RETRIES",
    "GOLUTRA_PROVIDER_REQUEST_MAX_RETRIES",
    "GOLUTRA_PROVIDER_STREAM_IDLE_TIMEOUT_MS",
    "GOLUTRA_PROVIDER_REQUEST_TIMEOUT_MS",
    "GOLUTRA_PROVIDER_TRANSPORT_FALLBACK",
    "OPENAI_API_KEY",
    "OPENAI_MODEL",
    "OPENAI_BASE_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_BASE_URL",
    "GEMINI_API_KEY",
    "GEMINI_MODEL",
    "GOOGLE_API_KEY",
    "GOOGLE_MODEL",
    "GOOGLE_OAUTH_ACCESS_TOKEN",
    "VERTEX_API_KEY",
    "GENAI_API_KEY",
    "GENAI_MODEL",
    "GENAI_BASE_URL",
];

fn digest_field(hasher: &mut Sha256, name: &str, value: Option<&str>) {
    hasher.update((name.len() as u64).to_le_bytes());
    hasher.update(name.as_bytes());
    match value {
        Some(value) => {
            hasher.update([1]);
            hasher.update((value.len() as u64).to_le_bytes());
            hasher.update(value.as_bytes());
        }
        None => hasher.update([0]),
    }
}

fn provider_settings_fingerprint(path: &Path) -> String {
    path_metadata_fingerprint(path)
}

fn provider_runtime_env_for_payload(
    provider_config_paths: Option<&ProviderConfigPaths>,
    payload: &Value,
) -> Result<Option<ProviderRuntimeEnv>, ProviderError> {
    let profile_name = optional_bounded_string(payload, "provider_profile", 128)?;
    let model_id = optional_bounded_string(payload, "provider_model", 256)?;
    let generation_config = generation_config_override(payload)?;

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

fn generation_config_override(payload: &Value) -> Result<Option<String>, ProviderError> {
    payload
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
        .transpose()
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
    use golutra_llm::{LlmProvider, ProviderReasoningEffort};
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
    fn provider_turn_binding_cache_invalidates_after_settings_replacement() {
        let home = tempdir().expect("home");
        let paths = ProviderConfigPaths::from_home(home.path()).expect("paths");
        let mut profile = ProviderProfile::mock();
        profile.name = "primary".to_owned();
        profile.model_id = Some("model-one".to_owned());
        let mut settings = ProviderSettings::default();
        settings.upsert_profile(profile, true);
        settings.save(&paths.user_config).expect("initial settings");

        let mut cache = ProviderRouteCache::default();
        let mut first = json!({"prompt": "first"});
        pin_provider_turn_settings_cached(&mut cache, Some(&paths), &mut first);
        assert_eq!(first["provider_model"], "model-one");

        settings.profiles[0].model_id = Some("model-two".to_owned());
        settings.save(&paths.user_config).expect("updated settings");
        let mut second = json!({"prompt": "second"});
        pin_provider_turn_settings_cached(&mut cache, Some(&paths), &mut second);
        assert_eq!(second["provider_model"], "model-two");
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

    #[test]
    fn provider_route_cache_reuses_live_clients_and_invalidates_on_settings_change() {
        let home = tempdir().expect("home");
        let paths = ProviderConfigPaths::from_home(home.path()).expect("paths");
        let profile = ProviderProfile::openai_compatible(
            "primary",
            "https://api.example.com/v1",
            "model-a",
            CredentialRef::ephemeral(SecretKind::ApiKey),
        )
        .expect("profile");
        let mut settings = ProviderSettings::default();
        settings.upsert_profile(profile, true);
        settings.save(&paths.user_config).expect("save settings");

        let payload = json!({"provider_profile": "primary"});
        let mut cache = ProviderRouteCache::default();
        let first = cached_mock_provider_plan(&mut cache, Some(&paths), &payload, "hello")
            .expect("first route");
        let second = cached_mock_provider_plan(&mut cache, Some(&paths), &payload, "read README")
            .expect("cached route");
        assert_eq!(first.provider.contract().model_id, "model-a");
        assert_eq!(second.provider.contract().model_id, "model-a");
        assert!(!first.touched_code);
        assert!(!second.touched_code);
        let stats = cache.stats();
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);

        let mut changed = ProviderSettings::load(&paths.user_config).expect("load settings");
        let active_name = changed.active_profile.clone().expect("active profile name");
        changed
            .profiles
            .iter_mut()
            .find(|profile| profile.name == active_name)
            .expect("active profile")
            .model_id = Some("model-b".to_owned());
        changed
            .save(&paths.user_config)
            .expect("save changed settings");
        let refreshed = cached_mock_provider_plan(&mut cache, Some(&paths), &payload, "hello")
            .expect("refreshed route");
        assert_eq!(refreshed.provider.contract().model_id, "model-b");
        assert_eq!(cache.stats().misses, 2);
    }
}
