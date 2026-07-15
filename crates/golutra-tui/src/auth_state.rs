//! Provider 认证向导的状态模型与内置选项。

use std::path::PathBuf;

use golutra_auth::CredentialRef;
use golutra_config::{BuiltinOAuthMethod, ProviderConfigScope, builtin_oauth_methods_for_provider};
use golutra_llm::{ProviderGenerationConfig, ProviderProtocol, ProviderReasoningEffort};
use golutra_tui::{AuthCredentialStore, OpenAiCompatibleLogin};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use super::ResumeSelectionDirection;

#[derive(Debug)]
pub(crate) struct PendingAuthOperation {
    pub(crate) cancellation: CancellationToken,
    pub(crate) progress: mpsc::UnboundedReceiver<AuthTaskProgress>,
    pub(crate) task: JoinHandle<Result<AuthTaskOutcome, String>>,
}

#[derive(Debug)]
pub(crate) struct AuthTaskProgress {
    pub(crate) title: String,
    pub(crate) body: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct AuthTaskOutcome {
    pub(crate) title: String,
    pub(crate) body: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AuthDialogState {
    pub(crate) step: AuthDialogStep,
    pub(crate) selected: usize,
    pub(crate) provider: Option<AuthProviderPreset>,
    pub(crate) protocol: ProviderProtocol,
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) api_key: String,
    pub(crate) api_key_env: String,
    pub(crate) credential_store: AuthCredentialStore,
    pub(crate) enable_thinking: bool,
    pub(crate) reasoning_effort: Option<ProviderReasoningEffort>,
    pub(crate) context_window_size: String,
    pub(crate) max_tokens: String,
    pub(crate) advanced_selected: usize,
    pub(crate) review: Option<AuthReview>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthDialogStep {
    GroupChoice,
    ThirdPartyChoice,
    AuthMethod,
    Protocol,
    BaseUrl,
    CredentialStore,
    ApiKey,
    EnvKey,
    Model,
    AdvancedConfig,
    Review,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthGroupAction {
    Official,
    ThirdParty,
    Custom,
    Mock,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthProviderSource {
    Official,
    ThirdParty,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AuthProviderPreset {
    pub(crate) profile: &'static str,
    pub(crate) title: &'static str,
    pub(crate) detail: &'static str,
    pub(crate) source: AuthProviderSource,
    pub(crate) protocol_options: &'static [ProviderProtocol],
    pub(crate) base_url: Option<&'static str>,
    pub(crate) model: Option<&'static str>,
    pub(crate) recommended_models: &'static [&'static str],
    pub(crate) oauth_provider_id: Option<&'static str>,
    pub(crate) api_key_supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthReview {
    pub(crate) provider_title: &'static str,
    pub(crate) profile: String,
    pub(crate) protocol: String,
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) credential: String,
    pub(crate) credential_ref: CredentialRef,
    pub(crate) advanced: String,
    pub(crate) scope: ProviderConfigScope,
    pub(crate) config_path: PathBuf,
    pub(crate) updates_existing_profile: bool,
    pub(crate) replaces_unreadable_config: bool,
    pub(crate) preview_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthAdvanceAction {
    None,
    SaveMock,
    SaveOpenAiCompatible(Box<OpenAiCompatibleLogin>),
    StartBuiltinOAuth(Box<BuiltinOAuthMethod>),
    Quit,
}

pub(crate) fn default_auth_credential_store() -> AuthCredentialStore {
    #[cfg(test)]
    {
        AuthCredentialStore::Ephemeral
    }
    #[cfg(not(test))]
    {
        AuthCredentialStore::Disk
    }
}

impl AuthDialogState {
    pub(crate) fn new() -> Self {
        Self {
            step: AuthDialogStep::GroupChoice,
            selected: 0,
            provider: None,
            protocol: ProviderProtocol::OpenAiCompatible,
            base_url: String::new(),
            model: String::new(),
            api_key: String::new(),
            api_key_env: String::new(),
            credential_store: default_auth_credential_store(),
            enable_thinking: false,
            reasoning_effort: None,
            context_window_size: String::new(),
            max_tokens: String::new(),
            advanced_selected: 0,
            review: None,
            error: None,
        }
    }

    pub(crate) fn selected_group_action(&self) -> AuthGroupAction {
        match self.selected {
            1 => AuthGroupAction::ThirdParty,
            2 => AuthGroupAction::Custom,
            3 => AuthGroupAction::Mock,
            4 => AuthGroupAction::Quit,
            _ => AuthGroupAction::Official,
        }
    }

    pub(crate) fn selected_third_party_provider(&self) -> AuthProviderPreset {
        THIRD_PARTY_PROVIDER_PRESETS[self
            .selected
            .min(THIRD_PARTY_PROVIDER_PRESETS.len().saturating_sub(1))]
    }

    pub(crate) fn select_provider(&mut self, provider: AuthProviderPreset) {
        self.provider = Some(provider);
        self.protocol = provider
            .protocol_options
            .first()
            .copied()
            .unwrap_or(ProviderProtocol::OpenAiCompatible);
        self.base_url = provider.base_url.unwrap_or_default().to_owned();
        self.model = provider.model.unwrap_or_default().to_owned();
        self.api_key.clear();
        self.api_key_env.clear();
        self.credential_store = default_auth_credential_store();
        self.enable_thinking = false;
        self.reasoning_effort = None;
        self.context_window_size.clear();
        self.max_tokens.clear();
        self.advanced_selected = 0;
        self.review = None;
        self.error = None;
        self.step = if !self.oauth_methods().is_empty() {
            AuthDialogStep::AuthMethod
        } else if provider.protocol_options.len() > 1 {
            AuthDialogStep::Protocol
        } else {
            AuthDialogStep::BaseUrl
        };
        self.selected = 0;
    }

    pub(crate) fn protocol_options(&self) -> &'static [ProviderProtocol] {
        self.provider
            .map(|provider| provider.protocol_options)
            .unwrap_or(&[])
    }

    pub(crate) fn oauth_methods(&self) -> Vec<BuiltinOAuthMethod> {
        self.provider
            .and_then(|provider| provider.oauth_provider_id)
            .map(builtin_oauth_methods_for_provider)
            .unwrap_or_default()
    }

    pub(crate) fn auth_method_count(&self) -> usize {
        self.oauth_methods().len()
            + usize::from(
                self.provider
                    .is_some_and(|provider| provider.api_key_supported),
            )
    }

    pub(crate) fn selected_oauth_method(&self) -> Option<BuiltinOAuthMethod> {
        self.oauth_methods().get(self.selected).cloned()
    }

    pub(crate) fn api_key_method_selected(&self) -> bool {
        let methods = self.oauth_methods();
        self.provider
            .is_some_and(|provider| provider.api_key_supported)
            && self.selected >= methods.len()
    }

    pub(crate) fn selected_protocol(&self) -> ProviderProtocol {
        self.protocol_options()
            .get(self.selected)
            .copied()
            .unwrap_or(self.protocol)
    }

    pub(crate) fn default_base_url_for_protocol(protocol: ProviderProtocol) -> &'static str {
        match protocol {
            ProviderProtocol::OpenAiCompatible => "https://api.openai.com/v1",
            ProviderProtocol::Anthropic => "https://api.anthropic.com/v1",
            ProviderProtocol::Gemini => "https://generativelanguage.googleapis.com/v1beta",
            _ => "",
        }
    }

    pub(crate) fn model_options(&self) -> &'static [&'static str] {
        self.provider
            .map(|provider| provider.recommended_models)
            .unwrap_or(&[])
    }

    pub(crate) fn custom_model_index(&self) -> usize {
        self.model_options().len()
    }

    pub(crate) fn selected_recommended_model(&self) -> Option<&'static str> {
        self.model_options().get(self.selected).copied()
    }

    pub(crate) fn is_custom_model_selected(&self) -> bool {
        self.selected >= self.custom_model_index()
    }

    pub(crate) fn move_selection(&mut self, direction: ResumeSelectionDirection) {
        let last_index = match self.step {
            AuthDialogStep::GroupChoice => AUTH_GROUP_ITEMS.len().saturating_sub(1),
            AuthDialogStep::ThirdPartyChoice => {
                THIRD_PARTY_PROVIDER_PRESETS.len().saturating_sub(1)
            }
            AuthDialogStep::AuthMethod => self.auth_method_count().saturating_sub(1),
            AuthDialogStep::Protocol => self.protocol_options().len().saturating_sub(1),
            AuthDialogStep::CredentialStore => 1,
            AuthDialogStep::Model => self.custom_model_index(),
            AuthDialogStep::AdvancedConfig => AUTH_ADVANCED_ITEMS.saturating_sub(1),
            AuthDialogStep::BaseUrl
            | AuthDialogStep::ApiKey
            | AuthDialogStep::EnvKey
            | AuthDialogStep::Review => 0,
        };
        let current = if self.step == AuthDialogStep::AdvancedConfig {
            self.advanced_selected
        } else {
            self.selected
        };
        let target = match direction {
            ResumeSelectionDirection::Previous => current.saturating_sub(1),
            ResumeSelectionDirection::Next => (current + 1).min(last_index),
        };
        if self.step == AuthDialogStep::AdvancedConfig {
            self.advanced_selected = target;
        } else {
            self.selected = target;
        }
        self.error = None;
    }

    pub(crate) fn current_input_mut(&mut self) -> Option<&mut String> {
        match self.step {
            AuthDialogStep::BaseUrl => Some(&mut self.base_url),
            AuthDialogStep::ApiKey => Some(&mut self.api_key),
            AuthDialogStep::EnvKey => Some(&mut self.api_key_env),
            AuthDialogStep::Model if self.is_custom_model_selected() => Some(&mut self.model),
            AuthDialogStep::AdvancedConfig => match self.advanced_selected {
                2 => Some(&mut self.context_window_size),
                3 => Some(&mut self.max_tokens),
                _ => None,
            },
            AuthDialogStep::GroupChoice
            | AuthDialogStep::ThirdPartyChoice
            | AuthDialogStep::AuthMethod
            | AuthDialogStep::Protocol
            | AuthDialogStep::CredentialStore
            | AuthDialogStep::Model
            | AuthDialogStep::Review => None,
        }
    }

    pub(crate) fn prepare_custom_model_input(&mut self) -> &mut String {
        let was_custom_model_selected = self.is_custom_model_selected();
        let model_matches_preset = self
            .model_options()
            .iter()
            .any(|model| *model == self.model)
            || self
                .provider
                .and_then(|provider| provider.model)
                .is_some_and(|model| model == self.model);
        self.selected = self.custom_model_index();
        if !was_custom_model_selected || model_matches_preset {
            self.model.clear();
        }
        self.error = None;
        &mut self.model
    }

    pub(crate) fn go_back(&mut self) {
        self.error = None;
        self.review = None;
        self.step = match self.step {
            AuthDialogStep::GroupChoice => AuthDialogStep::GroupChoice,
            AuthDialogStep::ThirdPartyChoice => AuthDialogStep::GroupChoice,
            AuthDialogStep::AuthMethod => match self.provider.map(|provider| provider.source) {
                Some(AuthProviderSource::ThirdParty) => AuthDialogStep::ThirdPartyChoice,
                _ => AuthDialogStep::GroupChoice,
            },
            AuthDialogStep::BaseUrl => match self.provider.map(|provider| provider.source) {
                Some(_) if !self.oauth_methods().is_empty() => AuthDialogStep::AuthMethod,
                Some(AuthProviderSource::Custom) if self.protocol_options().len() > 1 => {
                    AuthDialogStep::Protocol
                }
                Some(AuthProviderSource::ThirdParty) => AuthDialogStep::ThirdPartyChoice,
                _ => AuthDialogStep::GroupChoice,
            },
            AuthDialogStep::CredentialStore => AuthDialogStep::BaseUrl,
            AuthDialogStep::ApiKey => {
                if self.credential_store == AuthCredentialStore::Ephemeral {
                    AuthDialogStep::BaseUrl
                } else {
                    AuthDialogStep::CredentialStore
                }
            }
            AuthDialogStep::EnvKey => AuthDialogStep::CredentialStore,
            AuthDialogStep::Model => {
                if self.credential_store == AuthCredentialStore::Environment {
                    AuthDialogStep::EnvKey
                } else {
                    AuthDialogStep::ApiKey
                }
            }
            AuthDialogStep::AdvancedConfig => AuthDialogStep::Model,
            AuthDialogStep::Review => AuthDialogStep::AdvancedConfig,
            AuthDialogStep::Protocol => AuthDialogStep::GroupChoice,
        };
    }

    pub(crate) fn toggle_advanced_item(&mut self) {
        match self.advanced_selected {
            0 => self.enable_thinking = !self.enable_thinking,
            1 => self.reasoning_effort = next_reasoning_effort(self.reasoning_effort),
            _ => {}
        }
        self.error = None;
    }
}

pub(crate) const AUTH_ADVANCED_ITEMS: usize = 4;
pub(crate) const OPENAI_PROTOCOL_ONLY: &[ProviderProtocol] = &[ProviderProtocol::OpenAiCompatible];
pub(crate) const CUSTOM_PROTOCOL_OPTIONS: &[ProviderProtocol] = &[
    ProviderProtocol::OpenAiCompatible,
    ProviderProtocol::Anthropic,
    ProviderProtocol::Gemini,
    ProviderProtocol::VertexAi,
    ProviderProtocol::Genai,
];
pub(crate) const OFFICIAL_MODELS: &[&str] = &["gpt-test", "gpt-4.1", "qwen-coder-plus"];
pub(crate) const OPENAI_MODELS: &[&str] = &["gpt-5.5", "gpt-5.4", "gpt-4.1"];
pub(crate) const OPENROUTER_MODELS: &[&str] = &[
    "openai/gpt-4.1",
    "anthropic/claude-sonnet-4",
    "qwen/qwen3-coder",
];
pub(crate) const DEEPSEEK_MODELS: &[&str] = &["deepseek-chat", "deepseek-reasoner"];
pub(crate) const QWEN_MODELS: &[&str] = &["qwen-coder-plus", "qwen-plus", "qwen-max"];
pub(crate) const LOCAL_MODELS: &[&str] = &["qwen2.5-coder", "llama3.1", "deepseek-coder"];
pub(crate) const XAI_MODELS: &[&str] = &[
    "grok-4-1-fast-reasoning",
    "grok-4-1-fast-non-reasoning",
    "grok-4",
];
pub(crate) const COPILOT_MODELS: &[&str] = &["gpt-5.5", "gpt-5.3-codex", "gpt-5-mini"];
pub(crate) const CUSTOM_MODELS: &[&str] = &[];

pub(crate) const OFFICIAL_PROVIDER_PRESET: AuthProviderPreset = AuthProviderPreset {
    profile: "golutra",
    title: "Golutra API",
    detail: "Official OpenAI-compatible endpoint",
    source: AuthProviderSource::Official,
    protocol_options: OPENAI_PROTOCOL_ONLY,
    base_url: Some("https://api.golutra.cn/v1"),
    model: Some("gpt-test"),
    recommended_models: OFFICIAL_MODELS,
    oauth_provider_id: None,
    api_key_supported: true,
};

pub(crate) const CUSTOM_PROVIDER_PRESET: AuthProviderPreset = AuthProviderPreset {
    profile: "custom",
    title: "Custom Provider",
    detail: "Manually connect a local server, proxy, or unsupported provider",
    source: AuthProviderSource::Custom,
    protocol_options: CUSTOM_PROTOCOL_OPTIONS,
    base_url: None,
    model: None,
    recommended_models: CUSTOM_MODELS,
    oauth_provider_id: None,
    api_key_supported: true,
};

pub(crate) const THIRD_PARTY_PROVIDER_PRESETS: &[AuthProviderPreset] = &[
    AuthProviderPreset {
        profile: "openai",
        title: "OpenAI",
        detail: "https://api.openai.com/v1",
        source: AuthProviderSource::ThirdParty,
        protocol_options: OPENAI_PROTOCOL_ONLY,
        base_url: Some("https://api.openai.com/v1"),
        model: Some("gpt-5.5"),
        recommended_models: OPENAI_MODELS,
        oauth_provider_id: Some("openai-chatgpt"),
        api_key_supported: true,
    },
    AuthProviderPreset {
        profile: "openrouter",
        title: "OpenRouter",
        detail: "https://openrouter.ai/api/v1",
        source: AuthProviderSource::ThirdParty,
        protocol_options: OPENAI_PROTOCOL_ONLY,
        base_url: Some("https://openrouter.ai/api/v1"),
        model: Some("openai/gpt-4.1"),
        recommended_models: OPENROUTER_MODELS,
        oauth_provider_id: None,
        api_key_supported: true,
    },
    AuthProviderPreset {
        profile: "deepseek",
        title: "DeepSeek",
        detail: "https://api.deepseek.com/v1",
        source: AuthProviderSource::ThirdParty,
        protocol_options: OPENAI_PROTOCOL_ONLY,
        base_url: Some("https://api.deepseek.com/v1"),
        model: Some("deepseek-chat"),
        recommended_models: DEEPSEEK_MODELS,
        oauth_provider_id: None,
        api_key_supported: true,
    },
    AuthProviderPreset {
        profile: "qwen",
        title: "Qwen / DashScope compatible",
        detail: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        source: AuthProviderSource::ThirdParty,
        protocol_options: OPENAI_PROTOCOL_ONLY,
        base_url: Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
        model: Some("qwen-coder-plus"),
        recommended_models: QWEN_MODELS,
        oauth_provider_id: None,
        api_key_supported: true,
    },
    AuthProviderPreset {
        profile: "xai",
        title: "xAI",
        detail: "SuperGrok OAuth or xAI API key",
        source: AuthProviderSource::ThirdParty,
        protocol_options: OPENAI_PROTOCOL_ONLY,
        base_url: Some("https://api.x.ai/v1"),
        model: Some("grok-4-1-fast-reasoning"),
        recommended_models: XAI_MODELS,
        oauth_provider_id: Some("xai"),
        api_key_supported: true,
    },
    AuthProviderPreset {
        profile: "github-copilot",
        title: "GitHub Copilot",
        detail: "GitHub device authorization",
        source: AuthProviderSource::ThirdParty,
        protocol_options: OPENAI_PROTOCOL_ONLY,
        base_url: Some("https://api.githubcopilot.com/v1"),
        model: Some("gpt-5.5"),
        recommended_models: COPILOT_MODELS,
        oauth_provider_id: Some("github-copilot"),
        api_key_supported: false,
    },
    AuthProviderPreset {
        profile: "local",
        title: "Local OpenAI-compatible",
        detail: "Ollama, LM Studio, vLLM or a local proxy",
        source: AuthProviderSource::ThirdParty,
        protocol_options: OPENAI_PROTOCOL_ONLY,
        base_url: Some("http://localhost:11434/v1"),
        model: Some("qwen2.5-coder"),
        recommended_models: LOCAL_MODELS,
        oauth_provider_id: None,
        api_key_supported: true,
    },
];

pub(crate) const AUTH_GROUP_ITEMS: &[(&str, &str)] = &[
    ("Golutra API", "Official recommended setup with an API key"),
    (
        "Third-party Providers",
        "Choose a known OpenAI-compatible provider",
    ),
    (
        "Custom Provider",
        "Manually connect a local server, proxy, or unsupported provider",
    ),
    ("Continue with mock", "Use local deterministic provider"),
    ("Quit", "Leave without changing provider settings"),
];

pub(crate) fn next_reasoning_effort(
    value: Option<ProviderReasoningEffort>,
) -> Option<ProviderReasoningEffort> {
    match value {
        None => Some(ProviderReasoningEffort::Low),
        Some(ProviderReasoningEffort::Low) => Some(ProviderReasoningEffort::Medium),
        Some(ProviderReasoningEffort::Medium) => Some(ProviderReasoningEffort::High),
        Some(ProviderReasoningEffort::High) => Some(ProviderReasoningEffort::Xhigh),
        Some(ProviderReasoningEffort::Xhigh) => None,
    }
}

pub(crate) fn reasoning_effort_label(value: Option<ProviderReasoningEffort>) -> &'static str {
    match value {
        None => "default",
        Some(ProviderReasoningEffort::Low) => "low",
        Some(ProviderReasoningEffort::Medium) => "medium",
        Some(ProviderReasoningEffort::High) => "high",
        Some(ProviderReasoningEffort::Xhigh) => "xhigh",
    }
}

pub(crate) fn generation_config_summary(config: Option<&ProviderGenerationConfig>) -> String {
    let Some(config) = config else {
        return "default".to_owned();
    };
    let mut parts = Vec::new();
    if config.enable_thinking {
        parts.push("thinking=on".to_owned());
    }
    if let Some(reasoning_effort) = config.reasoning_effort {
        parts.push(format!(
            "effort={}",
            reasoning_effort_label(Some(reasoning_effort))
        ));
    }
    if let Some(context_window_size) = config.context_window_size {
        parts.push(format!("context={context_window_size}"));
    }
    if let Some(max_tokens) = config.max_tokens {
        parts.push(format!("max_tokens={max_tokens}"));
    }
    if parts.is_empty() {
        "default".to_owned()
    } else {
        parts.join(", ")
    }
}
