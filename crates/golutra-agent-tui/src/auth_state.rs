//! Provider 认证向导的状态模型与内置选项。

use std::path::PathBuf;

use golutra_agent_auth::CredentialRef;
use golutra_agent_config::{
    BuiltinOAuthMethod, ProviderConfigScope, builtin_oauth_methods_for_provider,
};
use golutra_agent_llm::{ProviderGenerationConfig, ProviderProtocol, ProviderReasoningEffort};
use golutra_agent_tui::{AuthCredentialStore, OpenAiCompatibleLogin};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{ComposerInput, ResumeSelectionDirection, cycle_reasoning_effort};

#[derive(Debug)]
pub(crate) struct PendingModelDiscovery {
    pub(crate) id: Uuid,
    pub(crate) task: JoinHandle<Result<Vec<String>, String>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) enum ModelDiscoveryState {
    #[default]
    Idle,
    Loading(Uuid),
    Ready,
    Failed(String),
}

#[derive(Debug)]
pub(crate) struct PendingAuthOperation {
    pub(crate) reopen_setup: bool,
    pub(crate) activate_profile: bool,
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
    pub(crate) scroll: usize,
    pub(crate) manual_scroll: bool,
    pub(crate) provider: Option<AuthProviderPreset>,
    pub(crate) protocol: ProviderProtocol,
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) models: Vec<String>,
    pub(crate) model_discovery: ModelDiscoveryState,
    pub(crate) manual_model_input: bool,
    pub(crate) api_key: String,
    pub(crate) api_key_env: String,
    pub(crate) credential_store: AuthCredentialStore,
    pub(crate) enable_thinking: bool,
    pub(crate) reasoning_effort: Option<ProviderReasoningEffort>,
    pub(crate) context_window_size: String,
    pub(crate) max_tokens: String,
    pub(crate) custom_headers: String,
    pub(crate) advanced_selected: usize,
    pub(crate) advanced_input: Option<ComposerInput>,
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
            scroll: 0,
            manual_scroll: false,
            provider: None,
            protocol: ProviderProtocol::OpenAiCompatible,
            base_url: String::new(),
            model: String::new(),
            models: Vec::new(),
            model_discovery: ModelDiscoveryState::Idle,
            manual_model_input: false,
            api_key: String::new(),
            api_key_env: String::new(),
            credential_store: default_auth_credential_store(),
            enable_thinking: false,
            reasoning_effort: None,
            context_window_size: String::new(),
            max_tokens: String::new(),
            custom_headers: String::new(),
            advanced_selected: 0,
            advanced_input: None,
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
        self.models = provider
            .recommended_models
            .iter()
            .map(|model| (*model).to_owned())
            .collect();
        self.model_discovery = ModelDiscoveryState::Idle;
        self.manual_model_input = false;
        self.api_key.clear();
        self.api_key_env.clear();
        self.credential_store = default_auth_credential_store();
        self.enable_thinking = false;
        self.reasoning_effort = None;
        self.context_window_size.clear();
        self.max_tokens.clear();
        self.custom_headers.clear();
        self.advanced_selected = 0;
        self.advanced_input = None;
        self.review = None;
        self.error = None;
        self.step = if !self.oauth_methods().is_empty() {
            AuthDialogStep::AuthMethod
        } else if provider.protocol_options.len() > 1 {
            AuthDialogStep::Protocol
        } else if provider.source == AuthProviderSource::Official {
            AuthDialogStep::ApiKey
        } else {
            AuthDialogStep::BaseUrl
        };
        self.selected = 0;
        self.scroll = 0;
        self.manual_scroll = false;
    }

    pub(crate) fn protocol_options(&self) -> &'static [ProviderProtocol] {
        self.provider
            .map(|provider| provider.protocol_options)
            .unwrap_or(&[])
    }

    pub(crate) fn toggle_credential_input(&mut self) {
        match self.step {
            AuthDialogStep::ApiKey => {
                self.credential_store = AuthCredentialStore::Environment;
                self.step = AuthDialogStep::EnvKey;
            }
            AuthDialogStep::EnvKey => {
                self.credential_store = AuthCredentialStore::Disk;
                self.step = AuthDialogStep::ApiKey;
            }
            _ => return,
        }
        // 切换来源后不能复用旧密钥或旧确认计划，避免无意保存已放弃的凭据。
        self.api_key.clear();
        self.review = None;
        self.error = None;
        self.selected = 0;
        self.scroll = 0;
        self.manual_scroll = false;
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
            ProviderProtocol::OpenAiCompatible | ProviderProtocol::OpenAiResponses => {
                "https://api.openai.com/v1"
            }
            ProviderProtocol::Anthropic => "https://api.anthropic.com/v1",
            ProviderProtocol::Gemini => "https://generativelanguage.googleapis.com/v1beta",
            _ => "",
        }
    }

    pub(crate) fn model_options(&self) -> &[String] {
        &self.models
    }

    pub(crate) fn custom_model_index(&self) -> usize {
        0
    }

    pub(crate) fn selected_recommended_model(&self) -> Option<&str> {
        self.selected
            .checked_sub(1)
            .and_then(|index| self.models.get(index))
            .map(String::as_str)
    }

    pub(crate) fn is_custom_model_selected(&self) -> bool {
        self.selected == self.custom_model_index()
    }

    pub(crate) fn move_selection(&mut self, direction: ResumeSelectionDirection) {
        if self.advanced_input.is_some() {
            return;
        }
        let last_index = self.last_selection_index();
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
        self.manual_scroll = false;
        self.error = None;
    }

    pub(crate) fn has_interactive_options(&self) -> bool {
        matches!(
            self.step,
            AuthDialogStep::GroupChoice
                | AuthDialogStep::ThirdPartyChoice
                | AuthDialogStep::AuthMethod
                | AuthDialogStep::Protocol
                | AuthDialogStep::Model
                | AuthDialogStep::AdvancedConfig
        )
    }

    pub(crate) fn set_interactive_selection(&mut self, index: usize) {
        if self.advanced_input.is_some() {
            return;
        }
        if self.step == AuthDialogStep::AdvancedConfig {
            self.advanced_selected = index.min(AUTH_ADVANCED_ITEMS.saturating_sub(1));
        } else {
            self.selected = index.min(self.last_selection_index());
        }
        self.manual_scroll = false;
        self.error = None;
    }

    pub(crate) fn scroll_by(&mut self, delta: isize, max_scroll: usize) {
        self.manual_scroll = true;
        let current = self.scroll.min(max_scroll);
        self.scroll = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(max_scroll)
        };
    }

    pub(crate) fn scroll_to(&mut self, position: usize) {
        self.scroll = position;
        self.manual_scroll = true;
    }

    fn last_selection_index(&self) -> usize {
        match self.step {
            AuthDialogStep::GroupChoice => AUTH_GROUP_ITEMS.len().saturating_sub(1),
            AuthDialogStep::ThirdPartyChoice => {
                THIRD_PARTY_PROVIDER_PRESETS.len().saturating_sub(1)
            }
            AuthDialogStep::AuthMethod => self.auth_method_count().saturating_sub(1),
            AuthDialogStep::Protocol => self.protocol_options().len().saturating_sub(1),
            AuthDialogStep::Model => self.model_options().len(),
            AuthDialogStep::AdvancedConfig => AUTH_ADVANCED_ITEMS.saturating_sub(1),
            AuthDialogStep::BaseUrl
            | AuthDialogStep::ApiKey
            | AuthDialogStep::EnvKey
            | AuthDialogStep::Review => 0,
        }
    }

    pub(crate) fn current_input_mut(&mut self) -> Option<&mut String> {
        match self.step {
            AuthDialogStep::BaseUrl => Some(&mut self.base_url),
            AuthDialogStep::ApiKey => Some(&mut self.api_key),
            AuthDialogStep::EnvKey => Some(&mut self.api_key_env),
            AuthDialogStep::Model if self.is_custom_model_selected() => {
                self.manual_model_input = true;
                Some(&mut self.model)
            }
            AuthDialogStep::AdvancedConfig => None,
            AuthDialogStep::GroupChoice
            | AuthDialogStep::ThirdPartyChoice
            | AuthDialogStep::AuthMethod
            | AuthDialogStep::Protocol
            | AuthDialogStep::Model
            | AuthDialogStep::Review => None,
        }
    }

    pub(crate) fn prepare_custom_model_input(&mut self) -> &mut String {
        let was_custom_model_selected = self.is_custom_model_selected();
        let model_matches_preset = self.model_options().contains(&self.model)
            || self
                .provider
                .and_then(|provider| provider.model)
                .is_some_and(|model| model == self.model);
        self.selected = self.custom_model_index();
        if !was_custom_model_selected || (!self.manual_model_input && model_matches_preset) {
            self.model.clear();
        }
        self.manual_model_input = true;
        self.error = None;
        &mut self.model
    }

    pub(crate) fn go_back(&mut self) {
        if self.advanced_input.is_some() {
            self.finish_advanced_edit();
            return;
        }
        self.error = None;
        self.review = None;
        self.scroll = 0;
        self.manual_scroll = false;
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
            AuthDialogStep::ApiKey | AuthDialogStep::EnvKey => {
                if self
                    .provider
                    .is_some_and(|provider| provider.source == AuthProviderSource::Official)
                {
                    AuthDialogStep::GroupChoice
                } else {
                    AuthDialogStep::BaseUrl
                }
            }
            AuthDialogStep::Model => {
                // 退回凭据页即使随后再次进入，也不能接受旧 Key 发起的目录请求。
                self.model_discovery = ModelDiscoveryState::Idle;
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

    pub(crate) fn cycle_advanced_item(&mut self, forward: bool) {
        match self.advanced_selected {
            1 => self.enable_thinking = !self.enable_thinking,
            2 => self.reasoning_effort = cycle_reasoning_effort(self.reasoning_effort, forward),
            _ => {}
        }
        self.error = None;
    }

    pub(crate) fn advanced_text_value(&self, index: usize) -> Option<&str> {
        if index == self.advanced_selected
            && let Some(input) = &self.advanced_input
        {
            return Some(input.text());
        }
        match index {
            3 => Some(&self.context_window_size),
            4 => Some(&self.max_tokens),
            5 => Some(&self.custom_headers),
            _ => None,
        }
    }

    pub(crate) fn start_advanced_edit(&mut self) {
        if let Some(value) = self.advanced_text_value(self.advanced_selected) {
            self.advanced_input = Some(ComposerInput::from_text(value));
            self.error = None;
            self.manual_scroll = false;
        }
    }

    pub(crate) fn finish_advanced_edit(&mut self) {
        let Some(input) = self.advanced_input.take() else {
            return;
        };
        // Enter/Esc 只保留本页草稿，最终校验和落盘仍分别由 Continue 与确认页负责。
        let value = input.trimmed();
        match self.advanced_selected {
            3 => self.context_window_size = value,
            4 => self.max_tokens = value,
            5 => self.custom_headers = value,
            _ => {}
        }
        self.error = None;
    }
}

pub(crate) const AUTH_ADVANCED_ITEMS: usize = 6;
pub(crate) const OPENAI_PROTOCOL_ONLY: &[ProviderProtocol] = &[ProviderProtocol::OpenAiCompatible];
pub(crate) const CUSTOM_PROTOCOL_OPTIONS: &[ProviderProtocol] = &[
    ProviderProtocol::OpenAiResponses,
    ProviderProtocol::Anthropic,
    ProviderProtocol::Gemini,
    ProviderProtocol::OpenAiCompatible,
    ProviderProtocol::VertexAi,
    ProviderProtocol::Genai,
];
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
    base_url: Some("https://api.golutra.cn"),
    model: None,
    recommended_models: &[],
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

pub(crate) fn reasoning_effort_label(value: Option<ProviderReasoningEffort>) -> &'static str {
    match value {
        None => "default",
        Some(ProviderReasoningEffort::Low) => "low",
        Some(ProviderReasoningEffort::Medium) => "medium",
        Some(ProviderReasoningEffort::High) => "high",
        Some(ProviderReasoningEffort::Xhigh) => "xhigh",
        Some(ProviderReasoningEffort::Max) => "max",
        Some(ProviderReasoningEffort::Ultra) => "ultra",
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
