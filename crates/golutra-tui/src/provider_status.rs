//! Provider status projection for TUI surfaces.
//!
//! Provider credentials stay inside the runtime host.  This module only
//! converts the host's redacted `ProviderState` query into display data.

use std::time::Duration;

use golutra_client::{RuntimeClient, RuntimeTransport};
use golutra_config::{ProviderProfile, provider_onboarding_state};
use golutra_core::{ActorKind, QueryId, SessionId};
use golutra_llm::{ProviderGenerationConfig, ProviderProtocol};
use golutra_protocol::{RuntimeQuery, RuntimeQueryKind};
use serde_json::Value;

use super::reasoning_effort_label;

#[derive(Debug, Clone)]
pub(crate) struct ProviderUiStatus {
    pub(crate) message: String,
    pub(crate) model: String,
}

pub(crate) async fn initial_provider_ui_status(
    transport: &RuntimeTransport,
    session_id: SessionId,
) -> ProviderUiStatus {
    // Provider status is advisory UI data. A slow durable store or a reconnecting
    // daemon must never prevent the TUI/driver from publishing its control socket.
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        provider_ui_status_from_runtime(transport, session_id),
    )
    .await
    .unwrap_or_else(|_| Err("provider status query timed out".to_owned()));
    match result {
        Ok(status) => status,
        Err(error) if !transport.is_remote() => {
            current_provider_ui_status().unwrap_or(ProviderUiStatus {
                message: format!("provider config error: {error}"),
                model: "unconfigured".to_owned(),
            })
        }
        Err(_) => ProviderUiStatus {
            message: "provider status unavailable".to_owned(),
            model: "unconfigured".to_owned(),
        },
    }
}

pub(crate) async fn provider_ui_status_from_runtime(
    transport: &RuntimeTransport,
    session_id: SessionId,
) -> Result<ProviderUiStatus, String> {
    let value = transport
        .query(RuntimeQuery {
            query_id: QueryId::new(),
            session_id,
            task_id: None,
            kind: RuntimeQueryKind::ProviderState,
            requester: ActorKind::Tui,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .map_err(|error| error.to_string())?;

    let Some(provider) = value.get("provider") else {
        return Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("runtime did not return provider state")
            .to_owned());
    };
    if provider.is_null() {
        return Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("runtime provider state is unavailable")
            .to_owned());
    }

    let provider_id = string_field(provider, "provider_id").unwrap_or("unknown");
    let status = string_field(provider, "status").unwrap_or("unknown");
    let model = provider_model(provider, provider_id);
    let message = match status {
        "ready" => format!("ready ({provider_id})"),
        "missing_env" => {
            let missing = provider
                .get("missing_env")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "provider setup".to_owned());
            format!("missing {missing}; use /auth setup")
        }
        _ => format!("provider {status} ({provider_id})"),
    };

    Ok(ProviderUiStatus { message, model })
}

pub(crate) fn current_provider_ui_status() -> Result<ProviderUiStatus, String> {
    match provider_onboarding_state() {
        Ok(state) if state.configured => {
            let profile_name = state
                .active_profile
                .as_ref()
                .map(|profile| profile.name.clone())
                .unwrap_or_else(|| "default".to_owned());
            let model = state
                .active_profile
                .as_ref()
                .map(provider_profile_footer_label)
                .unwrap_or_else(|| profile_name.clone());
            Ok(ProviderUiStatus {
                message: format!("ready ({profile_name})"),
                model,
            })
        }
        Ok(state) => {
            let missing = if state.missing_fields.is_empty() {
                "provider setup".to_owned()
            } else {
                state.missing_fields.join(", ")
            };
            let model = state
                .active_profile
                .as_ref()
                .map(provider_profile_footer_label)
                .unwrap_or_else(|| "unconfigured".to_owned());
            Ok(ProviderUiStatus {
                message: format!("missing {missing}; use /auth setup"),
                model,
            })
        }
        Err(error) => Err(error.to_string()),
    }
}

pub(crate) fn provider_profile_footer_label(profile: &ProviderProfile) -> String {
    let model = profile.model_id.as_deref().unwrap_or_else(|| {
        if profile.protocol == ProviderProtocol::Mock {
            "mock"
        } else {
            profile.name.as_str()
        }
    });
    let mode = profile.generation_config.as_ref().and_then(|config| {
        config
            .reasoning_effort
            .map(|effort| reasoning_effort_label(Some(effort)))
            .or_else(|| config.enable_thinking.then_some("thinking"))
    });
    mode.map_or_else(|| model.to_owned(), |mode| format!("{model} {mode}"))
}

pub(crate) fn provider_model_from_status(message: &str) -> String {
    message
        .strip_prefix("ready (")
        .and_then(|value| value.strip_suffix(')'))
        .unwrap_or("unconfigured")
        .to_owned()
}

#[cfg(test)]
pub(crate) fn provider_status_message() -> String {
    current_provider_ui_status()
        .map(|status| status.message)
        .unwrap_or_else(|error| format!("provider config error: {error}"))
}

fn provider_model(provider: &Value, provider_id: &str) -> String {
    let model = string_field(provider, "model_id").unwrap_or(provider_id);
    let mode = provider
        .get("generation_config")
        .cloned()
        .and_then(|value| serde_json::from_value::<ProviderGenerationConfig>(value).ok())
        .and_then(|config| {
            config
                .reasoning_effort
                .map(|effort| reasoning_effort_label(Some(effort)).to_owned())
                .or_else(|| config.enable_thinking.then_some("thinking".to_owned()))
        });
    mode.map_or_else(|| model.to_owned(), |mode| format!("{model} {mode}"))
}

fn string_field<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}
