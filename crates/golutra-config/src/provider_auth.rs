use std::collections::BTreeMap;

use golutra_auth::{OAuthFlow, OAuthProviderDescriptor, OpenAiDeviceAuthorizationDescriptor};
use golutra_llm::ProviderProtocol;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuiltinOAuthMethod {
    pub provider_id: String,
    pub method_id: String,
    pub label: String,
    pub flow: OAuthFlow,
    pub profile: String,
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub default_model: String,
    pub recommended_models: Vec<String>,
    pub descriptor: OAuthProviderDescriptor,
}

impl BuiltinOAuthMethod {
    pub fn validate(&self) -> Result<(), String> {
        for (value, label) in [
            (&self.provider_id, "provider id"),
            (&self.method_id, "method id"),
            (&self.label, "method label"),
            (&self.profile, "profile"),
            (&self.base_url, "base URL"),
            (&self.default_model, "default model"),
        ] {
            if value.trim().is_empty() {
                return Err(format!("builtin OAuth {label} cannot be empty"));
            }
        }
        if self.descriptor.provider_id != self.provider_id {
            return Err("builtin OAuth descriptor provider id does not match method".to_owned());
        }
        if !self.descriptor.flows.contains(&self.flow) {
            return Err("builtin OAuth descriptor does not support its method flow".to_owned());
        }
        self.descriptor
            .validate()
            .map_err(|error| error.to_string())
    }
}

#[must_use]
pub fn builtin_oauth_methods() -> Vec<BuiltinOAuthMethod> {
    vec![
        openai_chatgpt_browser(),
        openai_chatgpt_headless(),
        xai_browser(),
        xai_device(),
        github_copilot_device(),
    ]
}

#[must_use]
pub fn builtin_oauth_methods_for_provider(provider_id: &str) -> Vec<BuiltinOAuthMethod> {
    builtin_oauth_methods()
        .into_iter()
        .filter(|method| method.provider_id == provider_id)
        .collect()
}

#[must_use]
pub fn builtin_oauth_method(provider_id: &str, method_id: &str) -> Option<BuiltinOAuthMethod> {
    builtin_oauth_methods()
        .into_iter()
        .find(|method| method.provider_id == provider_id && method.method_id == method_id)
}

fn openai_chatgpt_descriptor() -> OAuthProviderDescriptor {
    OAuthProviderDescriptor {
        provider_id: "openai-chatgpt".to_owned(),
        client_id: "app_EMoamEEZ73f0CkXaXp7hrann".to_owned(),
        authorization_endpoint: "https://auth.openai.com/oauth/authorize".to_owned(),
        token_endpoint: "https://auth.openai.com/oauth/token".to_owned(),
        device_authorization_endpoint: None,
        revocation_endpoint: Some("https://auth.openai.com/oauth/revoke".to_owned()),
        scopes: [
            "openid",
            "profile",
            "email",
            "offline_access",
            "api.connectors.read",
            "api.connectors.invoke",
        ]
        .into_iter()
        .map(ToOwned::to_owned)
        .collect(),
        audience: None,
        browser_redirect_uri: Some("http://localhost:1455/auth/callback".to_owned()),
        authorization_params: BTreeMap::from([
            ("id_token_add_organizations".to_owned(), "true".to_owned()),
            ("codex_cli_simplified_flow".to_owned(), "true".to_owned()),
            ("originator".to_owned(), "golutra".to_owned()),
        ]),
        authorization_nonce: false,
        openai_device_authorization: Some(OpenAiDeviceAuthorizationDescriptor {
            user_code_endpoint: "https://auth.openai.com/api/accounts/deviceauth/usercode"
                .to_owned(),
            token_poll_endpoint: "https://auth.openai.com/api/accounts/deviceauth/token".to_owned(),
            verification_uri: "https://auth.openai.com/codex/device".to_owned(),
            redirect_uri: "https://auth.openai.com/deviceauth/callback".to_owned(),
        }),
        flows: vec![OAuthFlow::BrowserPkce, OAuthFlow::OpenAiDeviceAuth],
    }
}

fn openai_chatgpt_browser() -> BuiltinOAuthMethod {
    let descriptor = openai_chatgpt_descriptor();
    BuiltinOAuthMethod {
        provider_id: descriptor.provider_id.clone(),
        method_id: "browser".to_owned(),
        label: "ChatGPT Pro/Plus (browser)".to_owned(),
        flow: OAuthFlow::BrowserPkce,
        profile: "openai-chatgpt".to_owned(),
        protocol: ProviderProtocol::OpenAiResponses,
        base_url: "https://chatgpt.com/backend-api/codex".to_owned(),
        default_model: "gpt-5.5".to_owned(),
        recommended_models: ["gpt-5.5", "gpt-5.4", "gpt-5.4-mini"]
            .into_iter()
            .map(ToOwned::to_owned)
            .collect(),
        descriptor,
    }
}

fn openai_chatgpt_headless() -> BuiltinOAuthMethod {
    let descriptor = openai_chatgpt_descriptor();
    BuiltinOAuthMethod {
        provider_id: descriptor.provider_id.clone(),
        method_id: "headless".to_owned(),
        label: "ChatGPT Pro/Plus (headless)".to_owned(),
        flow: OAuthFlow::OpenAiDeviceAuth,
        profile: "openai-chatgpt".to_owned(),
        protocol: ProviderProtocol::OpenAiResponses,
        base_url: "https://chatgpt.com/backend-api/codex".to_owned(),
        default_model: "gpt-5.5".to_owned(),
        recommended_models: ["gpt-5.5", "gpt-5.4", "gpt-5.4-mini"]
            .into_iter()
            .map(ToOwned::to_owned)
            .collect(),
        descriptor,
    }
}

fn xai_descriptor() -> OAuthProviderDescriptor {
    OAuthProviderDescriptor {
        provider_id: "xai".to_owned(),
        client_id: "b1a00492-073a-47ea-816f-4c329264a828".to_owned(),
        authorization_endpoint: "https://auth.x.ai/oauth2/authorize".to_owned(),
        token_endpoint: "https://auth.x.ai/oauth2/token".to_owned(),
        device_authorization_endpoint: Some("https://auth.x.ai/oauth2/device/code".to_owned()),
        revocation_endpoint: Some("https://auth.x.ai/oauth2/revoke".to_owned()),
        scopes: [
            "openid",
            "profile",
            "email",
            "offline_access",
            "grok-cli:access",
            "api:access",
        ]
        .into_iter()
        .map(ToOwned::to_owned)
        .collect(),
        audience: None,
        browser_redirect_uri: Some("http://127.0.0.1:56121/callback".to_owned()),
        authorization_params: BTreeMap::from([
            ("plan".to_owned(), "generic".to_owned()),
            ("referrer".to_owned(), "golutra".to_owned()),
        ]),
        authorization_nonce: true,
        openai_device_authorization: None,
        flows: vec![OAuthFlow::BrowserPkce, OAuthFlow::DeviceCode],
    }
}

fn xai_browser() -> BuiltinOAuthMethod {
    let descriptor = xai_descriptor();
    BuiltinOAuthMethod {
        provider_id: descriptor.provider_id.clone(),
        method_id: "browser".to_owned(),
        label: "xAI Grok OAuth (browser)".to_owned(),
        flow: OAuthFlow::BrowserPkce,
        profile: "xai".to_owned(),
        protocol: ProviderProtocol::OpenAiCompatible,
        base_url: "https://api.x.ai/v1".to_owned(),
        default_model: "grok-4-1-fast-reasoning".to_owned(),
        recommended_models: xai_models(),
        descriptor,
    }
}

fn xai_device() -> BuiltinOAuthMethod {
    let descriptor = xai_descriptor();
    BuiltinOAuthMethod {
        provider_id: descriptor.provider_id.clone(),
        method_id: "device".to_owned(),
        label: "xAI Grok OAuth (headless/device)".to_owned(),
        flow: OAuthFlow::DeviceCode,
        profile: "xai".to_owned(),
        protocol: ProviderProtocol::OpenAiCompatible,
        base_url: "https://api.x.ai/v1".to_owned(),
        default_model: "grok-4-1-fast-reasoning".to_owned(),
        recommended_models: xai_models(),
        descriptor,
    }
}

fn xai_models() -> Vec<String> {
    [
        "grok-4-1-fast-reasoning",
        "grok-4-1-fast-non-reasoning",
        "grok-4-fast-reasoning",
        "grok-4",
    ]
    .into_iter()
    .map(ToOwned::to_owned)
    .collect()
}

fn github_copilot_device() -> BuiltinOAuthMethod {
    let descriptor = OAuthProviderDescriptor {
        provider_id: "github-copilot".to_owned(),
        client_id: "Ov23li8tweQw6odWQebz".to_owned(),
        authorization_endpoint: "https://github.com/login/oauth/authorize".to_owned(),
        token_endpoint: "https://github.com/login/oauth/access_token".to_owned(),
        device_authorization_endpoint: Some("https://github.com/login/device/code".to_owned()),
        revocation_endpoint: None,
        scopes: vec!["read:user".to_owned()],
        audience: None,
        browser_redirect_uri: None,
        authorization_params: BTreeMap::new(),
        authorization_nonce: false,
        openai_device_authorization: None,
        flows: vec![OAuthFlow::DeviceCode],
    };
    BuiltinOAuthMethod {
        provider_id: descriptor.provider_id.clone(),
        method_id: "device".to_owned(),
        label: "Login with GitHub Copilot".to_owned(),
        flow: OAuthFlow::DeviceCode,
        profile: "github-copilot".to_owned(),
        protocol: ProviderProtocol::OpenAiCompatible,
        base_url: "https://api.githubcopilot.com/v1".to_owned(),
        default_model: "gpt-5.5".to_owned(),
        recommended_models: ["gpt-5.5", "gpt-5.3-codex", "gpt-5-mini"]
            .into_iter()
            .map(ToOwned::to_owned)
            .collect(),
        descriptor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn builtin_oauth_catalog_is_valid_and_unique() {
        let methods = builtin_oauth_methods();
        let mut ids = HashSet::new();
        for method in methods {
            method.validate().expect("valid builtin OAuth method");
            assert!(ids.insert((method.provider_id, method.method_id)));
        }
    }

    #[test]
    fn provider_filter_returns_only_requested_methods() {
        let methods = builtin_oauth_methods_for_provider("xai");

        assert_eq!(methods.len(), 2);
        assert!(methods.iter().all(|method| method.provider_id == "xai"));
    }

    #[test]
    fn builtin_catalog_keeps_oauth_flow_and_runtime_adapter_paired() {
        let openai = builtin_oauth_method("openai-chatgpt", "browser").expect("OpenAI method");
        assert_eq!(openai.flow, OAuthFlow::BrowserPkce);
        assert_eq!(openai.protocol, ProviderProtocol::OpenAiResponses);
        assert_eq!(
            openai.descriptor.browser_redirect_uri.as_deref(),
            Some("http://localhost:1455/auth/callback")
        );
        assert_eq!(
            openai
                .descriptor
                .authorization_params
                .get("codex_cli_simplified_flow")
                .map(String::as_str),
            Some("true")
        );
        let openai_headless =
            builtin_oauth_method("openai-chatgpt", "headless").expect("OpenAI headless method");
        assert_eq!(openai_headless.flow, OAuthFlow::OpenAiDeviceAuth);
        assert_eq!(openai_headless.protocol, ProviderProtocol::OpenAiResponses);
        assert!(
            openai_headless
                .descriptor
                .openai_device_authorization
                .is_some()
        );

        let xai_browser = builtin_oauth_method("xai", "browser").expect("xAI browser method");
        let xai_device = builtin_oauth_method("xai", "device").expect("xAI device method");
        assert_eq!(xai_browser.flow, OAuthFlow::BrowserPkce);
        assert_eq!(xai_device.flow, OAuthFlow::DeviceCode);
        assert!(xai_browser.descriptor.authorization_nonce);
        assert_eq!(xai_browser.protocol, ProviderProtocol::OpenAiCompatible);

        let copilot = builtin_oauth_method("github-copilot", "device").expect("Copilot method");
        assert_eq!(copilot.flow, OAuthFlow::DeviceCode);
        assert!(!copilot.descriptor.flows.contains(&OAuthFlow::BrowserPkce));
        assert_eq!(copilot.protocol, ProviderProtocol::OpenAiCompatible);
    }
}
