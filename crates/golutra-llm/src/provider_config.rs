//! Provider 协议选择、环境配置、URL 校验与错误脱敏。

use super::*;

pub(crate) fn normalize_protocol_value(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('_', "-")
}

pub(crate) fn selected_protocol_from_reader<F>(reader: &F) -> Option<ProviderProtocol>
where
    F: Fn(&str) -> Option<String>,
{
    reader(GOLUTRA_PROVIDER_PROTOCOL)
        .and_then(|value| ProviderProtocol::from_config_value(&value))
        .or_else(|| {
            reader(GOLUTRA_PROVIDER_MODE)
                .and_then(|value| ProviderProtocol::from_config_value(&value))
        })
}

pub(crate) fn env_mapping(protocol: ProviderProtocol) -> ProviderEnvMapping {
    match protocol {
        ProviderProtocol::Mock => ProviderEnvMapping {
            api_key: &[],
            base_url: &[],
            model: &[],
            default_base_url: None,
        },
        ProviderProtocol::OpenAiCompatible => ProviderEnvMapping {
            api_key: &[GOLUTRA_PROVIDER_API_KEY, OPENAI_API_KEY],
            base_url: &[GOLUTRA_PROVIDER_BASE_URL, OPENAI_BASE_URL],
            model: &[GOLUTRA_PROVIDER_MODEL, OPENAI_MODEL],
            default_base_url: Some(DEFAULT_OPENAI_BASE_URL),
        },
        ProviderProtocol::OpenAiResponses => ProviderEnvMapping {
            api_key: &[GOLUTRA_PROVIDER_API_KEY],
            base_url: &[GOLUTRA_PROVIDER_BASE_URL],
            model: &[GOLUTRA_PROVIDER_MODEL],
            default_base_url: Some("https://chatgpt.com/backend-api/codex"),
        },
        ProviderProtocol::Anthropic => ProviderEnvMapping {
            api_key: &[GOLUTRA_PROVIDER_API_KEY, ANTHROPIC_API_KEY],
            base_url: &[GOLUTRA_PROVIDER_BASE_URL, ANTHROPIC_BASE_URL],
            model: &[GOLUTRA_PROVIDER_MODEL, ANTHROPIC_MODEL],
            default_base_url: Some(DEFAULT_ANTHROPIC_BASE_URL),
        },
        ProviderProtocol::Gemini => ProviderEnvMapping {
            api_key: &[GOLUTRA_PROVIDER_API_KEY, GEMINI_API_KEY, GOOGLE_API_KEY],
            base_url: &[GOLUTRA_PROVIDER_BASE_URL],
            model: &[GOLUTRA_PROVIDER_MODEL, GEMINI_MODEL],
            default_base_url: Some(DEFAULT_GEMINI_BASE_URL),
        },
        ProviderProtocol::VertexAi => ProviderEnvMapping {
            api_key: &[
                GOLUTRA_PROVIDER_API_KEY,
                GOOGLE_OAUTH_ACCESS_TOKEN,
                VERTEX_API_KEY,
                GOOGLE_API_KEY,
            ],
            base_url: &[GOLUTRA_PROVIDER_BASE_URL],
            model: &[GOLUTRA_PROVIDER_MODEL, GOOGLE_MODEL],
            default_base_url: None,
        },
        ProviderProtocol::Genai => ProviderEnvMapping {
            api_key: &[
                GOLUTRA_PROVIDER_API_KEY,
                GENAI_API_KEY,
                OPENAI_API_KEY,
                ANTHROPIC_API_KEY,
                GEMINI_API_KEY,
                GOOGLE_API_KEY,
            ],
            base_url: &[
                GOLUTRA_PROVIDER_BASE_URL,
                GENAI_BASE_URL,
                OPENAI_BASE_URL,
                ANTHROPIC_BASE_URL,
            ],
            model: &[
                GOLUTRA_PROVIDER_MODEL,
                GENAI_MODEL,
                OPENAI_MODEL,
                ANTHROPIC_MODEL,
                GEMINI_MODEL,
                GOOGLE_MODEL,
            ],
            default_base_url: None,
        },
    }
}

pub(crate) fn missing_env_error(keys: &[&str]) -> ProviderError {
    ProviderError::NotConfigured {
        message: format!("required env is not set: {}", keys.join(" or ")),
    }
}

pub(crate) fn redacted_openai_from_reader<F>(reader: &F) -> RedactedProviderConfig
where
    F: Fn(&str) -> Option<String>,
{
    let mapping = env_mapping(ProviderProtocol::OpenAiCompatible);
    let api_key = configured_or_first_env(reader, mapping.api_key);
    let model = first_env(reader, mapping.model);
    let base_url = first_env(reader, mapping.base_url)
        .map(|(_, value)| value)
        .or_else(|| mapping.default_base_url.map(ToOwned::to_owned));
    let mut missing_env = Vec::new();
    if api_key.is_none() {
        missing_env.push(mapping.api_key.join(" or "));
    }
    if model.is_none() {
        missing_env.push(mapping.model.join(" or "));
    }
    let ready = missing_env.is_empty();
    let generation_config = generation_config_from_reader(reader).ok();
    RedactedProviderConfig {
        mode: "live".to_owned(),
        provider_id: "openai_compatible".to_owned(),
        protocol: ProviderProtocol::OpenAiCompatible,
        native_protocol: "openai_chat_completions".to_owned(),
        base_url: base_url.map(|value| normalize_openai_base_url(&value)),
        model_id: model.as_ref().map(|(_, value)| value.clone()),
        api_key_env: api_key.as_ref().map(|(key, _)| key.clone()),
        api_key_configured: api_key.is_some(),
        generation_config: generation_config.filter(|config| !config.is_empty()),
        missing_env,
        supported: true,
        status: if ready { "ready" } else { "missing_env" }.to_owned(),
    }
}

pub(crate) fn redacted_openai_responses_from_reader<F>(reader: &F) -> RedactedProviderConfig
where
    F: Fn(&str) -> Option<String>,
{
    let mapping = env_mapping(ProviderProtocol::OpenAiResponses);
    let api_key = configured_or_first_env(reader, mapping.api_key);
    let model = first_env(reader, mapping.model);
    let base_url = first_env(reader, mapping.base_url)
        .map(|(_, value)| value)
        .or_else(|| mapping.default_base_url.map(ToOwned::to_owned));
    let mut missing_env = Vec::new();
    if api_key.is_none() {
        missing_env.push(mapping.api_key.join(" or "));
    }
    if model.is_none() {
        missing_env.push(mapping.model.join(" or "));
    }
    let ready = missing_env.is_empty();
    RedactedProviderConfig {
        mode: "live".to_owned(),
        provider_id: reader(GOLUTRA_PROVIDER_AUTH_PROVIDER)
            .unwrap_or_else(|| "openai-chatgpt".to_owned()),
        protocol: ProviderProtocol::OpenAiResponses,
        native_protocol: "openai_responses_sse".to_owned(),
        base_url,
        model_id: model.as_ref().map(|(_, value)| value.clone()),
        api_key_env: api_key.as_ref().map(|(key, _)| key.clone()),
        api_key_configured: api_key.is_some(),
        generation_config: generation_config_from_reader(reader)
            .ok()
            .filter(|config| !config.is_empty()),
        missing_env,
        supported: true,
        status: if ready { "ready" } else { "missing_env" }.to_owned(),
    }
}

pub(crate) fn redacted_native_from_reader<F>(
    protocol: ProviderProtocol,
    reader: &F,
) -> RedactedProviderConfig
where
    F: Fn(&str) -> Option<String>,
{
    let mapping = env_mapping(protocol);
    let api_key = configured_or_first_env(reader, mapping.api_key);
    let model = first_env(reader, mapping.model);
    let base_url = first_env(reader, mapping.base_url)
        .map(|(_, value)| value)
        .or_else(|| mapping.default_base_url.map(ToOwned::to_owned));
    let mut missing_env = Vec::new();
    if !mapping.api_key.is_empty() && api_key.is_none() {
        missing_env.push(mapping.api_key.join(" or "));
    }
    if !mapping.model.is_empty() && model.is_none() {
        missing_env.push(mapping.model.join(" or "));
    }
    if !mapping.base_url.is_empty() && mapping.default_base_url.is_none() && base_url.is_none() {
        missing_env.push(mapping.base_url.join(" or "));
    }
    let ready = missing_env.is_empty();
    let generation_config = generation_config_from_reader(reader).ok();

    RedactedProviderConfig {
        mode: "live".to_owned(),
        provider_id: protocol.id().to_owned(),
        protocol,
        native_protocol: protocol.id().to_owned(),
        base_url,
        model_id: model.as_ref().map(|(_, value)| value.clone()),
        api_key_env: api_key.as_ref().map(|(key, _)| key.clone()),
        api_key_configured: api_key.is_some(),
        generation_config: generation_config.filter(|config| !config.is_empty()),
        missing_env,
        supported: true,
        status: if ready { "ready" } else { "missing_env" }.to_owned(),
    }
}

pub(crate) fn protocol_spec(protocol: ProviderProtocol) -> ProviderProtocolSpec {
    let mapping = env_mapping(protocol);
    let (display_name, status, supports_tool_calls, supports_probe, notes) = match protocol {
        ProviderProtocol::Mock => (
            "Mock".to_owned(),
            "supported".to_owned(),
            true,
            false,
            "Deterministic local provider for smoke tests, replay, and offline development."
                .to_owned(),
        ),
        ProviderProtocol::OpenAiCompatible => (
            "OpenAI-compatible".to_owned(),
            "supported".to_owned(),
            true,
            true,
            "Live Chat Completions adapter for OpenAI-compatible endpoints.".to_owned(),
        ),
        ProviderProtocol::OpenAiResponses => (
            "OpenAI Responses".to_owned(),
            "supported".to_owned(),
            true,
            true,
            "Responses SSE adapter for explicitly registered subscription OAuth providers."
                .to_owned(),
        ),
        ProviderProtocol::Anthropic => (
            "Anthropic".to_owned(),
            "supported".to_owned(),
            true,
            true,
            "Native Anthropic Messages adapter backed by rust-genai.".to_owned(),
        ),
        ProviderProtocol::Gemini => (
            "Gemini".to_owned(),
            "supported".to_owned(),
            true,
            true,
            "Native Gemini generateContent adapter backed by rust-genai.".to_owned(),
        ),
        ProviderProtocol::VertexAi => (
            "Vertex AI".to_owned(),
            "supported".to_owned(),
            true,
            true,
            "Native Vertex AI adapter using an OAuth access token and project/location endpoint."
                .to_owned(),
        ),
        ProviderProtocol::Genai => (
            "rust-genai".to_owned(),
            "supported".to_owned(),
            true,
            true,
            "Multi-provider adapter selected from the configured rust-genai model namespace."
                .to_owned(),
        ),
    };

    ProviderProtocolSpec {
        protocol,
        display_name,
        status,
        api_key_env: mapping
            .api_key
            .iter()
            .map(|key| (*key).to_owned())
            .collect(),
        base_url_env: mapping
            .base_url
            .iter()
            .map(|key| (*key).to_owned())
            .collect(),
        model_env: mapping.model.iter().map(|key| (*key).to_owned()).collect(),
        default_base_url: mapping.default_base_url.map(ToOwned::to_owned),
        supports_tool_calls,
        supports_probe,
        notes,
    }
}

pub(crate) fn first_env<F>(reader: &F, keys: &[&str]) -> Option<(String, String)>
where
    F: Fn(&str) -> Option<String>,
{
    keys.iter().find_map(|key| {
        reader(key)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(|value| ((*key).to_owned(), value))
    })
}

pub(crate) fn configured_or_first_env<F>(reader: &F, keys: &[&str]) -> Option<(String, String)>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(configured_key) = reader(GOLUTRA_PROVIDER_API_KEY_ENV)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        && let Some(value) = reader(&configured_key).filter(|value| !value.trim().is_empty())
    {
        return Some((configured_key, value));
    }
    first_env(reader, keys)
}

pub(crate) fn generation_config_from_reader<F>(
    reader: &F,
) -> Result<ProviderGenerationConfig, ProviderError>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(value) = reader(GOLUTRA_PROVIDER_GENERATION_CONFIG)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    else {
        return Ok(ProviderGenerationConfig::default());
    };
    let config: ProviderGenerationConfig =
        serde_json::from_str(&value).map_err(|error| ProviderError::NotConfigured {
            message: format!("{GOLUTRA_PROVIDER_GENERATION_CONFIG} must be valid JSON: {error}"),
        })?;
    config
        .validate()
        .map_err(|message| ProviderError::NotConfigured {
            message: format!("{GOLUTRA_PROVIDER_GENERATION_CONFIG} is invalid: {message}"),
        })?;
    Ok(config)
}

pub(crate) fn apply_generation_config_to_openai_body(
    body: &mut Value,
    config: &ProviderGenerationConfig,
) {
    if config.enable_thinking {
        body["enable_thinking"] = Value::Bool(true);
    }
    if let Some(reasoning_effort) = config.reasoning_effort {
        body["reasoning_effort"] = Value::String(reasoning_effort.as_wire_value().to_owned());
    }
    if let Some(max_tokens) = config.max_tokens {
        body["max_tokens"] = Value::Number(max_tokens.into());
    }
}

#[must_use]
pub fn normalize_openai_base_url(value: &str) -> String {
    let trimmed = value.trim().trim_end_matches('/');
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    let without_slash = with_scheme.trim_end_matches('/').to_owned();
    let after_scheme = without_slash
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(without_slash.as_str());
    if after_scheme.contains('/') {
        without_slash
    } else {
        format!("{without_slash}/v1")
    }
}

pub fn validate_openai_base_url(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("provider base URL cannot be empty".to_owned());
    }
    if let Some((scheme, _)) = value.split_once("://")
        && !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https")
    {
        return Err("provider base URL must use http or https".to_owned());
    }
    let lower = value.to_ascii_lowercase();
    if (lower.starts_with("http:") && !lower.starts_with("http://"))
        || (lower.starts_with("https:") && !lower.starts_with("https://"))
    {
        return Err("provider base URL has an invalid HTTP scheme".to_owned());
    }
    let normalized = normalize_openai_base_url(value);
    let parsed = reqwest::Url::parse(&normalized)
        .map_err(|error| format!("provider base URL is invalid: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("provider base URL must use http or https".to_owned());
    }
    if parsed.host_str().is_none() {
        return Err("provider base URL must include a host".to_owned());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("provider base URL must not include user credentials".to_owned());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("provider base URL must not include a query or fragment".to_owned());
    }
    Ok(normalized)
}

pub fn validate_native_base_url(value: &str) -> Result<String, String> {
    let value = value.trim().trim_end_matches('/');
    if value.is_empty() {
        return Err("provider base URL cannot be empty".to_owned());
    }
    let normalized = if value.starts_with("http://") || value.starts_with("https://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    let parsed = reqwest::Url::parse(&normalized)
        .map_err(|error| format!("provider base URL is invalid: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("provider base URL must use http or https".to_owned());
    }
    if parsed.host_str().is_none() {
        return Err("provider base URL must include a host".to_owned());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("provider base URL must not include user credentials".to_owned());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("provider base URL must not include a query or fragment".to_owned());
    }
    Ok(normalized)
}

pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}

pub(crate) fn sanitize_provider_error(message: &str) -> String {
    let single_line = message.replace(['\n', '\r'], " ");
    let redacted = redact_provider_secret_fragments(&single_line);
    let trimmed = redacted.trim();
    if trimmed.chars().count() <= 512 {
        trimmed.to_owned()
    } else {
        format!("{}...", trimmed.chars().take(512).collect::<String>())
    }
}

pub(crate) fn redact_provider_secret_fragments(message: &str) -> String {
    message
        .split_whitespace()
        .map(|token| {
            if provider_error_token_looks_secret(token) {
                "<redacted-api-key>"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn provider_error_token_looks_secret(token: &str) -> bool {
    let trimmed = token.trim_matches(|character: char| {
        !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_' | '*' | '.')
    });
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("sk-") || lower.starts_with("sk_") {
        return true;
    }
    let star_count = trimmed
        .chars()
        .filter(|character| *character == '*')
        .count();
    let alpha_numeric_count = trimmed
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .count();
    star_count >= 6 && alpha_numeric_count >= 6
}
