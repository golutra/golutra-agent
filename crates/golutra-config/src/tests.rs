use std::{
    ffi::OsString,
    fs,
    sync::{Mutex, MutexGuard},
};

use secrecy::ExposeSecret;
use tempfile::{TempDir, tempdir};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

use super::*;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock_test_env() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct IsolatedGolutraHome {
    previous: Option<OsString>,
    _dir: TempDir,
    _guard: MutexGuard<'static, ()>,
}

impl IsolatedGolutraHome {
    fn new() -> Self {
        let guard = lock_test_env();
        let dir = tempdir().expect("home");
        let previous = std::env::var_os(GOLUTRA_HOME_ENV);
        unsafe {
            std::env::set_var(GOLUTRA_HOME_ENV, dir.path());
        }
        Self {
            previous,
            _dir: dir,
            _guard: guard,
        }
    }
}

struct ScopedEnvVar {
    key: &'static str,
    previous: Option<OsString>,
}

impl ScopedEnvVar {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }
}

impl Drop for ScopedEnvVar {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe {
                std::env::set_var(self.key, value);
            },
            None => unsafe {
                std::env::remove_var(self.key);
            },
        }
    }
}

impl Drop for IsolatedGolutraHome {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe {
                std::env::set_var(GOLUTRA_HOME_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(GOLUTRA_HOME_ENV);
            },
        }
    }
}

async fn spawn_probe_server(body: &'static str) -> String {
    spawn_probe_server_response("200 OK", body).await
}

async fn spawn_probe_server_response(status: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut buffer = [0_u8; 2048];
        let _ = stream.read(&mut buffer).await.expect("read request");
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });
    format!("http://{address}/v1")
}

fn env_credential(key: &str) -> CredentialRef {
    CredentialRef::environment(key, SecretKind::ApiKey).expect("credential")
}

fn disk_credential() -> CredentialRef {
    CredentialRef::disk(SecretKind::ApiKey)
}

fn unreadable_provider_settings() -> &'static [u8] {
    br#"{
  "version": 2,
  "active_profile": "legacy",
  "profiles": [
    {
      "name": "legacy",
      "protocol": "openai-compatible",
      "model_id": "legacy-model",
      "base_url": "https://legacy.example.com/v1",
      "credential_ref": {
        "id": "cred_legacy",
        "source": {"kind": "removed-backend"},
        "secret_kind": "api-key",
        "revision": "rev_legacy"
      },
      "enabled": true
    }
  ]
}
"#
}

fn oauth_credential() -> CredentialRef {
    CredentialRef::ephemeral(SecretKind::OAuthTokenSet)
}

fn oauth_descriptor() -> OAuthProviderDescriptor {
    OAuthProviderDescriptor {
        provider_id: "test-provider".to_owned(),
        client_id: "test-client".to_owned(),
        authorization_endpoint: "https://auth.example.com/authorize".to_owned(),
        token_endpoint: "https://auth.example.com/token".to_owned(),
        device_authorization_endpoint: None,
        revocation_endpoint: None,
        scopes: vec!["model.invoke".to_owned()],
        audience: None,
        browser_redirect_uri: None,
        authorization_params: std::collections::BTreeMap::new(),
        authorization_nonce: false,
        openai_device_authorization: None,
        flows: vec![golutra_auth::OAuthFlow::BrowserPkce],
    }
}

fn store_test_oauth_credential(
    store: &dyn SecretStore,
    reference: &CredentialRef,
    access_token: &str,
) {
    let secret = SecretString::from(
        serde_json::json!({
            "revision": "oauth_test_revision",
            "refresh_token": "refresh-test",
            "access_token": access_token,
            "expires_at": null,
            "token_type": "Bearer",
            "scopes": ["model.invoke"]
        })
        .to_string(),
    );
    store.set(reference, &secret).expect("OAuth token set");
}

#[test]
fn runtime_config_roundtrips() {
    let dir = tempdir().expect("dir");
    let path = dir.path().join("golutra.json");
    let config = RuntimeConfig::p1_default();

    config.save(&path).expect("save");
    let loaded = RuntimeConfig::load(&path).expect("load");

    assert_eq!(loaded.protocol_version, "v0.1");
    assert!(loaded.model_catalog.route_default().is_some());
}

#[test]
fn empty_golutra_home_is_rejected() {
    let _guard = lock_test_env();
    let _home = ScopedEnvVar::set(GOLUTRA_HOME_ENV, "");

    let error = golutra_home().expect_err("empty home must be rejected");

    assert!(error.to_string().contains("cannot be empty"));
}

#[test]
fn provider_settings_v2_persists_only_credential_reference() {
    let _home = IsolatedGolutraHome::new();
    let paths = ProviderConfigPaths::global().expect("paths");
    let profile = ProviderProfile::openai_compatible(
        "golutra",
        "api.golutra.cn",
        "gpt-test",
        env_credential("GOLUTRA_PROVIDER_API_KEY"),
    )
    .expect("profile");
    ProviderInstallPlan {
        scope: ProviderConfigScope::User,
        profile,
        activate: true,
        pending_secret: None,
    }
    .apply(&paths)
    .expect("install");

    let loaded = ProviderSettings::load(&paths.user_config).expect("load");
    let serialized = fs::read_to_string(&paths.user_config).expect("serialized");

    assert_eq!(loaded.version, PROVIDER_SETTINGS_VERSION);
    assert!(serialized.contains("credential_ref"));
    assert!(!serialized.contains("secret-value"));
}

#[test]
fn explicit_provider_install_replaces_unreadable_credential_source() {
    let home = tempdir().expect("home");
    let paths = ProviderConfigPaths::from_home(home.path()).expect("paths");
    fs::write(&paths.user_config, unreadable_provider_settings()).expect("legacy config");

    ProviderInstallPlan {
        scope: ProviderConfigScope::User,
        profile: ProviderProfile::mock(),
        activate: true,
        pending_secret: None,
    }
    .apply(&paths)
    .expect("replace unreadable config");

    let settings = ProviderSettings::load(&paths.user_config).expect("settings");
    let persisted = fs::read_to_string(&paths.user_config).expect("config");
    assert_eq!(settings.active_profile().expect("active").name, "mock");
    assert!(!persisted.contains("removed-backend"));
}

#[test]
fn provider_settings_reject_shared_credential_ids() {
    let reference = disk_credential();
    let first = ProviderProfile::openai_compatible(
        "first",
        "https://api.example.com/v1",
        "model-first",
        reference.clone(),
    )
    .expect("first profile");
    let second = ProviderProfile::openai_compatible(
        "second",
        "https://api.example.com/v1",
        "model-second",
        reference,
    )
    .expect("second profile");
    let settings = ProviderSettings {
        version: PROVIDER_SETTINGS_VERSION,
        active_profile: Some("first".to_owned()),
        profiles: vec![first, second],
    };

    let error = settings.validate().expect_err("shared ref rejected");

    assert!(error.to_string().contains("multiple provider profiles"));
}

#[test]
fn concurrent_global_provider_updates_preserve_both_profiles() {
    let _home = IsolatedGolutraHome::new();
    let paths = ProviderConfigPaths::global().expect("paths");
    let plan = |name: &str| {
        let mut profile = ProviderProfile::mock();
        profile.name = name.to_owned();
        ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile,
            activate: false,
            pending_secret: None,
        }
    };
    let first_paths = paths.clone();
    let second_paths = paths.clone();
    std::thread::scope(|scope| {
        scope.spawn(move || plan("first").apply(&first_paths).expect("first update"));
        scope.spawn(move || plan("second").apply(&second_paths).expect("second update"));
    });

    let settings = ProviderSettings::load(&paths.user_config).expect("settings");
    let names = settings
        .profiles
        .iter()
        .map(|profile| profile.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(names, std::collections::HashSet::from(["first", "second"]));
}

#[test]
fn legacy_env_map_migrates_shared_keys_to_disk_secret_store_and_v2_references() {
    let dir = tempdir().expect("dir");
    let paths = ProviderConfigPaths::from_home(dir.path()).expect("paths");
    fs::write(
        &paths.user_config,
        r#"{
  "version": 1,
  "active_profile": "golutra",
  "env": {"GOLUTRA_PROVIDER_API_KEY": "legacy-secret"},
  "profiles": [
    {
      "name": "golutra",
      "protocol": "openai-compatible",
      "model_id": "gpt-test",
      "base_url": "https://api.golutra.cn/v1",
      "api_key_env": "GOLUTRA_PROVIDER_API_KEY",
      "enabled": true
    },
    {
      "name": "backup",
      "protocol": "openai-compatible",
      "model_id": "gpt-backup",
      "base_url": "https://backup.golutra.cn/v1",
      "api_key_env": "GOLUTRA_PROVIDER_API_KEY",
      "enabled": true
    }
  ]
}
"#,
    )
    .expect("write");

    let loaded = load_provider_settings(&paths).expect("migrate");
    let references = loaded
        .profiles
        .iter()
        .map(|profile| profile.credential_ref.as_ref().expect("credential ref"))
        .collect::<Vec<_>>();
    let store = DefaultSecretStore::new(&paths.home).expect("disk store");

    assert_eq!(loaded.version, PROVIDER_SETTINGS_VERSION);
    assert_eq!(references.len(), 2);
    assert_ne!(references[0].id, references[1].id);
    for reference in references {
        assert_eq!(reference.source, CredentialSource::Disk);
        assert_eq!(
            store
                .get(reference)
                .expect("read")
                .expect("secret")
                .expose_secret(),
            "legacy-secret"
        );
    }
    assert!(
        paths
            .home
            .join(golutra_auth::CREDENTIALS_FILE_NAME)
            .is_file()
    );
    assert!(
        !fs::read_to_string(&paths.user_config)
            .expect("v2")
            .contains("legacy-secret")
    );
}

#[test]
fn provider_profile_serialization_contains_no_secret_field() {
    let profile = ProviderProfile::openai_compatible(
        "golutra",
        "api.golutra.cn",
        "gpt-test",
        disk_credential(),
    )
    .expect("profile");

    let serialized = serde_json::to_string(&profile).expect("serialize");
    assert!(!serialized.contains("api_key"));
    assert!(serialized.contains("credential_ref"));
}

#[test]
fn workspace_provider_config_scope_is_rejected() {
    let _home = IsolatedGolutraHome::new();
    let paths = ProviderConfigPaths::global().expect("paths");
    let profile = ProviderProfile::openai_compatible(
        "golutra",
        "api.golutra.cn",
        "gpt-test",
        env_credential("GOLUTRA_PROVIDER_API_KEY"),
    )
    .expect("profile");
    let plan = ProviderInstallPlan {
        scope: ProviderConfigScope::Workspace,
        profile,
        activate: true,
        pending_secret: None,
    };

    let error = plan.apply(&paths).expect_err("workspace scope rejected");

    assert!(matches!(error, ConfigError::Validation(_)));
    assert!(
        error
            .to_string()
            .contains("workspace provider config is no longer supported")
    );
}

#[test]
fn custom_provider_api_key_env_is_stable_and_disambiguated() {
    let without_trailing_slash = generate_custom_provider_api_key_env(
        ProviderProtocol::OpenAiCompatible,
        "https://api.example.com/v1",
    );
    let with_trailing_slash = generate_custom_provider_api_key_env(
        ProviderProtocol::OpenAiCompatible,
        "https://api.example.com/v1/",
    );
    let different_protocol = generate_custom_provider_api_key_env(
        ProviderProtocol::Anthropic,
        "https://api.example.com/v1",
    );
    let different_url = generate_custom_provider_api_key_env(
        ProviderProtocol::OpenAiCompatible,
        "https://other.example.com/v1",
    );

    assert_eq!(without_trailing_slash, with_trailing_slash);
    assert!(without_trailing_slash.starts_with(CUSTOM_PROVIDER_API_KEY_ENV_PREFIX));
    assert!(without_trailing_slash.contains("OPENAI_COMPATIBLE_HTTPS_API_EXAMPLE_COM_V1"));
    assert_ne!(without_trailing_slash, different_protocol);
    assert_ne!(without_trailing_slash, different_url);
}

#[test]
fn provider_install_plan_accepts_native_live_protocols() {
    let _home = IsolatedGolutraHome::new();
    let paths = ProviderConfigPaths::global().expect("paths");
    let profile = ProviderProfile {
        name: "anthropic".to_owned(),
        protocol: ProviderProtocol::Anthropic,
        model_id: Some("claude-sonnet-4".to_owned()),
        base_url: Some("https://api.anthropic.com/v1".to_owned()),
        credential_ref: Some(env_credential("GOLUTRA_PROVIDER_API_KEY")),
        oauth: None,
        generation_config: None,
        custom_headers: Vec::new(),
        cache_capabilities: Some(golutra_llm::ProviderCacheCapabilities::anthropic()),
        enabled: true,
    };
    let plan = ProviderInstallPlan {
        scope: ProviderConfigScope::User,
        profile,
        activate: true,
        pending_secret: None,
    };

    plan.apply(&paths).expect("native protocol installed");
    let settings = ProviderSettings::load(&paths.user_config).expect("settings");
    assert_eq!(
        settings.active_profile().expect("profile").protocol,
        ProviderProtocol::Anthropic
    );
}

#[test]
fn provider_custom_headers_resolve_env_values_without_exposing_them_in_debug() {
    let _home = IsolatedGolutraHome::new();
    let _api_key = ScopedEnvVar::set("GOLUTRA_TEST_PROVIDER_KEY", "fake-provider-key");
    let _header = ScopedEnvVar::set("GOLUTRA_TEST_HEADER_KEY", "fake-header-secret");
    let paths = ProviderConfigPaths::global().expect("paths");
    let mut profile = ProviderProfile::openai_compatible(
        "custom-headers",
        "https://api.example.com/v1",
        "model-test",
        env_credential("GOLUTRA_TEST_PROVIDER_KEY"),
    )
    .expect("profile");
    profile.custom_headers = vec![
        ProviderHeaderConfig {
            name: "X-Client-Name".to_owned(),
            value: ProviderHeaderValue::Literal {
                value: "golutra-test".to_owned(),
            },
        },
        ProviderHeaderConfig {
            name: "X-Api-Key".to_owned(),
            value: ProviderHeaderValue::Environment {
                key: "GOLUTRA_TEST_HEADER_KEY".to_owned(),
            },
        },
    ];
    ProviderInstallPlan {
        scope: ProviderConfigScope::User,
        profile,
        activate: true,
        pending_secret: None,
    }
    .apply(&paths)
    .expect("install custom headers");

    let runtime = load_provider_runtime_env_from_paths(&paths).expect("runtime env");
    let headers: BTreeMap<String, String> = serde_json::from_str(
        &runtime
            .get(GOLUTRA_PROVIDER_CUSTOM_HEADERS)
            .expect("resolved headers"),
    )
    .expect("header JSON");

    assert_eq!(headers["X-Client-Name"], "golutra-test");
    assert_eq!(headers["X-Api-Key"], "fake-header-secret");
    assert_eq!(
        runtime.redacted_values()[GOLUTRA_PROVIDER_CUSTOM_HEADERS],
        "<redacted>"
    );
    assert!(!format!("{runtime:?}").contains("fake-header-secret"));
}

#[test]
fn provider_custom_headers_reject_literal_secrets_and_transport_headers() {
    let mut profile = ProviderProfile::mock();
    profile.custom_headers = vec![ProviderHeaderConfig {
        name: "X-Api-Key".to_owned(),
        value: ProviderHeaderValue::Literal {
            value: "must-not-enter-provider-json".to_owned(),
        },
    }];
    assert!(profile.validate().is_err());

    profile.custom_headers = vec![ProviderHeaderConfig {
        name: "Host".to_owned(),
        value: ProviderHeaderValue::Literal {
            value: "example.com".to_owned(),
        },
    }];
    assert!(profile.validate().is_err());
}

#[test]
fn provider_profile_rejects_an_invalid_or_credentialed_base_url() {
    for base_url in [
        "",
        "file:///tmp/provider",
        "https://user:secret@api.example.com/v1",
        "https://api.example.com/v1?token=secret",
    ] {
        let error = ProviderProfile::openai_compatible(
            "invalid",
            base_url,
            "model",
            env_credential("GOLUTRA_PROVIDER_API_KEY"),
        )
        .expect_err("invalid provider URL");
        assert!(error.to_string().contains("base URL"));
    }
}

#[test]
fn onboarding_requires_explicit_provider_profile() {
    let _home = IsolatedGolutraHome::new();
    let state = provider_onboarding_state().expect("onboarding");

    assert!(!state.configured);
    assert_eq!(state.source, "none");
    assert_eq!(state.missing_fields, vec!["active_profile"]);
    assert!(state.active_profile.is_none());
}

#[test]
fn onboarding_does_not_implicitly_activate_the_first_enabled_profile() {
    let _home = IsolatedGolutraHome::new();
    let paths = ProviderConfigPaths::global().expect("paths");
    ProviderInstallPlan {
        scope: ProviderConfigScope::User,
        profile: ProviderProfile::mock(),
        activate: false,
        pending_secret: None,
    }
    .apply(&paths)
    .expect("install inactive profile");

    let state = provider_onboarding_state().expect("onboarding");

    assert!(!state.configured);
    assert_eq!(state.missing_fields, vec!["active_profile"]);
    assert!(state.active_profile.is_none());
    assert!(
        load_provider_runtime_env()
            .expect("runtime env")
            .get("GOLUTRA_PROVIDER_PROTOCOL")
            .is_none()
    );
}

#[test]
fn disabled_profile_cannot_be_active() {
    let mut profile = ProviderProfile::mock();
    profile.enabled = false;
    let mut settings = ProviderSettings {
        profiles: vec![profile],
        ..ProviderSettings::default()
    };

    let error = settings
        .set_active_profile("mock")
        .expect_err("disabled profile cannot be selected");
    assert!(error.to_string().contains("disabled"));

    settings.active_profile = Some("mock".to_owned());
    let error = settings
        .validate()
        .expect_err("disabled active profile is invalid");
    assert!(error.to_string().contains("disabled"));
}

#[test]
fn onboarding_accepts_explicit_mock_provider() {
    let _home = IsolatedGolutraHome::new();
    let paths = ProviderConfigPaths::global().expect("paths");
    ProviderInstallPlan {
        scope: ProviderConfigScope::User,
        profile: ProviderProfile::mock(),
        activate: true,
        pending_secret: None,
    }
    .apply(&paths)
    .expect("install mock");

    let state = provider_onboarding_state().expect("onboarding");

    assert!(state.configured);
    assert_eq!(state.source, "user");
    assert!(state.missing_fields.is_empty());
    assert_eq!(
        state.active_profile.expect("profile").protocol,
        ProviderProtocol::Mock
    );
}

#[test]
fn onboarding_accepts_disk_backed_provider_without_exposing_secret() {
    let _home = IsolatedGolutraHome::new();
    let paths = ProviderConfigPaths::global().expect("paths");
    let store = Arc::new(golutra_auth::MemorySecretStore::default());
    let reference = disk_credential();
    store
        .set(&reference, &SecretString::from("secret".to_owned()))
        .expect("secret");
    let profile =
        ProviderProfile::openai_compatible("golutra", "api.golutra.cn", "gpt-test", reference)
            .expect("profile");
    ProviderSettings {
        version: PROVIDER_SETTINGS_VERSION,
        active_profile: Some("golutra".to_owned()),
        profiles: vec![profile],
    }
    .save(&paths.user_config)
    .expect("settings");

    let state = provider_onboarding_state_with_store(&paths, store).expect("onboarding");

    assert!(state.configured);
    assert_eq!(state.source, "user");
    assert!(state.missing_fields.is_empty());
    assert!(
        serde_json::to_string(&state)
            .expect("serialize")
            .contains("credential_ref")
    );
    assert!(
        !serde_json::to_string(&state)
            .expect("serialize")
            .contains("profile-secret")
    );
}

#[test]
fn onboarding_accepts_key_from_configured_env_var() {
    let _home = IsolatedGolutraHome::new();
    let paths = ProviderConfigPaths::global().expect("paths");
    let _api_key = ScopedEnvVar::set("GOLUTRA_PROVIDER_API_KEY", "secret-from-env");
    let profile = ProviderProfile::openai_compatible(
        "golutra",
        "api.golutra.cn",
        "gpt-test",
        env_credential("GOLUTRA_PROVIDER_API_KEY"),
    )
    .expect("profile");
    ProviderInstallPlan {
        scope: ProviderConfigScope::User,
        profile,
        activate: true,
        pending_secret: None,
    }
    .apply(&paths)
    .expect("install provider");

    let state = provider_onboarding_state().expect("onboarding");

    assert!(state.configured);
    assert!(state.missing_fields.is_empty());
}

#[tokio::test]
async fn runtime_env_uses_dynamic_secret_provider_instead_of_serializing_key() {
    let home = tempdir().expect("home");
    let store = Arc::new(golutra_auth::MemorySecretStore::default());
    let reference = disk_credential();
    store
        .set(&reference, &SecretString::from("profile-secret".to_owned()))
        .expect("secret");
    let profile = ProviderProfile::openai_compatible(
        "custom",
        "https://api.golutra.cn/v1",
        "gpt-5.5",
        reference,
    )
    .expect("profile");
    let settings = ProviderSettings {
        version: PROVIDER_SETTINGS_VERSION,
        active_profile: Some("custom".to_owned()),
        profiles: vec![profile],
    };
    let auth = AuthService::new(home.path(), store).expect("auth");

    let runtime_env = runtime_env_from_settings(&settings, &auth).expect("runtime env");
    let resolved = runtime_env
        .credential_provider()
        .expect("provider")
        .credential(false)
        .await
        .expect("credential");

    assert_eq!(
        runtime_env.get("GOLUTRA_PROVIDER_API_KEY"),
        Some("<resolved-credential>".to_owned())
    );
    assert_eq!(
        runtime_env.get(golutra_llm::GOLUTRA_PROVIDER_ROUTE_ID),
        Some("custom".to_owned())
    );
    assert_eq!(resolved.expose_secret(), "profile-secret");
}

#[test]
fn runtime_env_exports_declared_cache_capabilities() {
    let profile = ProviderProfile::openai_compatible(
        "golutra",
        "https://api.golutra.cn/v1",
        "gpt-5.5",
        env_credential("GOLUTRA_CACHE_CAPABILITY_TEST_KEY"),
    )
    .expect("profile");
    let settings = ProviderSettings {
        version: PROVIDER_SETTINGS_VERSION,
        active_profile: Some("golutra".to_owned()),
        profiles: vec![profile],
    };
    let home = tempdir().expect("home");
    let auth = AuthService::new(
        home.path(),
        Arc::new(golutra_auth::MemorySecretStore::default()),
    )
    .expect("auth");

    let runtime = runtime_env_from_settings(&settings, &auth).expect("runtime env");
    let encoded = runtime
        .get(GOLUTRA_PROVIDER_CACHE_CAPABILITIES)
        .expect("cache capabilities");
    let decoded =
        serde_json::from_str::<ProviderCacheCapabilities>(&encoded).expect("cache capability JSON");

    assert_eq!(decoded, ProviderCacheCapabilities::compatible());
}

#[test]
fn runtime_env_uses_oauth_route_identity_for_cache_capabilities() {
    let mut descriptor = oauth_descriptor();
    descriptor.provider_id = "openai-chatgpt".to_owned();
    let profile = ProviderProfile {
        name: "account-profile".to_owned(),
        protocol: ProviderProtocol::OpenAiResponses,
        model_id: Some("gpt-5.5".to_owned()),
        base_url: Some("https://chatgpt.com/backend-api/codex".to_owned()),
        credential_ref: Some(oauth_credential()),
        oauth: Some(descriptor),
        generation_config: None,
        custom_headers: Vec::new(),
        cache_capabilities: None,
        enabled: true,
    };
    let settings = ProviderSettings {
        version: PROVIDER_SETTINGS_VERSION,
        active_profile: Some("account-profile".to_owned()),
        profiles: vec![profile],
    };
    let home = tempdir().expect("home");
    let auth = AuthService::new(
        home.path(),
        Arc::new(golutra_auth::MemorySecretStore::default()),
    )
    .expect("auth");

    let runtime = runtime_env_from_settings(&settings, &auth).expect("runtime env");
    let capabilities = runtime
        .get(GOLUTRA_PROVIDER_CACHE_CAPABILITIES)
        .and_then(|value| serde_json::from_str::<ProviderCacheCapabilities>(&value).ok())
        .expect("cache capabilities");

    assert_eq!(
        runtime.get(GOLUTRA_PROVIDER_ROUTE_ID).as_deref(),
        Some("openai-chatgpt")
    );
    assert_eq!(capabilities, ProviderCacheCapabilities::codex_responses());
}

#[test]
fn runtime_env_keeps_unknown_compatible_gateway_cache_disabled() {
    let mut profile = ProviderProfile::openai_compatible(
        "private-gateway",
        "https://gateway.example/v1",
        "model",
        env_credential("GOLUTRA_UNKNOWN_GATEWAY_TEST_KEY"),
    )
    .expect("profile");
    profile.cache_capabilities = None;
    let settings = ProviderSettings {
        version: PROVIDER_SETTINGS_VERSION,
        active_profile: Some("private-gateway".to_owned()),
        profiles: vec![profile],
    };
    let home = tempdir().expect("home");
    let auth = AuthService::new(
        home.path(),
        Arc::new(golutra_auth::MemorySecretStore::default()),
    )
    .expect("auth");

    let runtime = runtime_env_from_settings(&settings, &auth).expect("runtime env");
    let capabilities = runtime
        .get(GOLUTRA_PROVIDER_CACHE_CAPABILITIES)
        .and_then(|value| serde_json::from_str::<ProviderCacheCapabilities>(&value).ok())
        .expect("cache capabilities");

    assert_eq!(capabilities, ProviderCacheCapabilities::disabled());
}

#[test]
fn runtime_env_rejects_cache_capabilities_for_the_wrong_protocol() {
    let mut profile = ProviderProfile::mock();
    profile.cache_capabilities = Some(ProviderCacheCapabilities::responses());
    let settings = ProviderSettings {
        version: PROVIDER_SETTINGS_VERSION,
        active_profile: Some("mock".to_owned()),
        profiles: vec![profile],
    };
    let home = tempdir().expect("home");
    let auth = AuthService::new(
        home.path(),
        Arc::new(golutra_auth::MemorySecretStore::default()),
    )
    .expect("auth");

    let error = runtime_env_from_settings(&settings, &auth)
        .expect_err("mock cannot advertise Responses cache fields");

    assert!(error.to_string().contains("prompt_cache_key"));
}

#[test]
fn runtime_env_includes_generation_config_json() {
    let mut profile = ProviderProfile::openai_compatible(
        "custom",
        "https://api.golutra.cn/v1",
        "gpt-5.5",
        env_credential("GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST"),
    )
    .expect("profile");
    profile.generation_config = Some(ProviderGenerationConfig {
        enable_thinking: true,
        reasoning_effort: Some(golutra_llm::ProviderReasoningEffort::High),
        context_window_size: Some(128_000),
        max_tokens: Some(512),
    });
    let settings = ProviderSettings {
        version: PROVIDER_SETTINGS_VERSION,
        active_profile: Some("custom".to_owned()),
        profiles: vec![profile],
    };
    let home = tempdir().expect("home");
    let auth = AuthService::new(
        home.path(),
        Arc::new(golutra_auth::MemorySecretStore::default()),
    )
    .expect("auth");

    let runtime_env = runtime_env_from_settings(&settings, &auth).expect("runtime env");
    let value = runtime_env
        .get(GOLUTRA_PROVIDER_GENERATION_CONFIG)
        .expect("generation config");

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&value).expect("json"),
        serde_json::json!({
            "enable_thinking": true,
            "reasoning_effort": "high",
            "context_window_size": 128000,
            "max_tokens": 512
        })
    );
}

#[test]
fn redacted_runtime_env_hides_hashed_custom_provider_keys() {
    let key = "GOLUTRA_CUSTOM_PROVIDER_API_KEY_OPENAI_COMPATIBLE_EXAMPLE_1234";
    let environment = ProviderRuntimeEnv {
        values: BTreeMap::from([
            (key.to_owned(), "private-value".to_owned()),
            ("GOLUTRA_PROVIDER_MODEL".to_owned(), "model".to_owned()),
        ]),
        credential: None,
    };

    let redacted = environment.redacted_values();

    assert_eq!(redacted.get(key).map(String::as_str), Some("<redacted>"));
    assert_eq!(
        redacted.get("GOLUTRA_PROVIDER_MODEL").map(String::as_str),
        Some("model")
    );
}

#[tokio::test]
async fn verified_provider_install_rolls_back_on_probe_failure() {
    let _home = IsolatedGolutraHome::new();
    let dir = tempdir().expect("dir");
    let paths = ProviderConfigPaths::global().expect("paths");
    let store = Arc::new(golutra_auth::MemorySecretStore::default());
    let reference = disk_credential();
    let profile = ProviderProfile::openai_compatible(
        "custom",
        "http://127.0.0.1:9/v1",
        "gpt-5.5",
        reference.clone(),
    )
    .expect("profile");

    let error = apply_provider_install_plan_verified_with_store(
        &paths,
        dir.path(),
        &ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile,
            activate: true,
            pending_secret: Some(SecretString::from("bad-key".to_owned())),
        },
        store.clone(),
    )
    .await
    .expect_err("probe should fail");

    assert_eq!(error.step, "probe");
    assert!(!paths.user_config.exists());
    assert!(!store.contains(&reference));
}

#[tokio::test]
async fn failed_install_restores_unreadable_config_without_persisting_secret() {
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    let paths = ProviderConfigPaths::from_home(home.path()).expect("paths");
    let original = unreadable_provider_settings();
    fs::write(&paths.user_config, original).expect("legacy config");
    let store = Arc::new(golutra_auth::MemorySecretStore::default());
    let reference = disk_credential();
    let profile = ProviderProfile::openai_compatible(
        "custom",
        "http://127.0.0.1:9/v1",
        "gpt-5.5",
        reference.clone(),
    )
    .expect("profile");

    let error = apply_provider_install_plan_verified_with_store(
        &paths,
        workspace.path(),
        &ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile,
            activate: true,
            pending_secret: Some(SecretString::from("bad-key".to_owned())),
        },
        store.clone(),
    )
    .await
    .expect_err("probe should fail");

    assert_eq!(error.step, "probe");
    assert_eq!(fs::read(&paths.user_config).expect("restored"), original);
    assert!(!store.contains(&reference));
}

#[tokio::test]
async fn verified_provider_install_persists_when_probe_succeeds() {
    let _home = IsolatedGolutraHome::new();
    let dir = tempdir().expect("dir");
    let paths = ProviderConfigPaths::global().expect("paths");
    let store = Arc::new(golutra_auth::MemorySecretStore::default());
    let reference = disk_credential();
    let base_url = spawn_probe_server(r#"{"data":[{"id":"gpt-5.5"}]}"#).await;
    let profile =
        ProviderProfile::openai_compatible("custom", base_url, "gpt-5.5", reference.clone())
            .expect("profile");

    apply_provider_install_plan_verified_with_store(
        &paths,
        dir.path(),
        &ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile,
            activate: true,
            pending_secret: Some(SecretString::from("good-key".to_owned())),
        },
        store.clone(),
    )
    .await
    .expect("probe should pass");

    let settings = ProviderSettings::load(&paths.user_config).expect("settings");
    let active = settings.active_profile().expect("active");
    assert_eq!(active.name, "custom");
    assert_eq!(active.model_id.as_deref(), Some("gpt-5.5"));
    assert!(store.contains(&reference));
    assert!(
        !fs::read_to_string(&paths.user_config)
            .expect("config")
            .contains("good-key")
    );
}

#[tokio::test]
async fn verified_provider_install_persists_secret_in_the_disk_store() {
    let _home = IsolatedGolutraHome::new();
    let workspace = tempdir().expect("workspace");
    let paths = ProviderConfigPaths::global().expect("paths");
    let reference = disk_credential();
    let base_url = spawn_probe_server(r#"{"data":[{"id":"gpt-5.5"}]}"#).await;
    let profile =
        ProviderProfile::openai_compatible("custom", base_url, "gpt-5.5", reference.clone())
            .expect("profile");

    apply_provider_install_plan_verified(
        &paths,
        workspace.path(),
        &ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile,
            activate: true,
            pending_secret: Some(SecretString::from("disk-secret".to_owned())),
        },
    )
    .await
    .expect("verified install");

    let store = golutra_auth::DefaultSecretStore::new(&paths.home).expect("disk store");
    assert_eq!(
        store
            .get(&reference)
            .expect("stored credential")
            .expect("secret")
            .expose_secret(),
        "disk-secret"
    );
    assert!(
        paths
            .home
            .join(golutra_auth::CREDENTIALS_FILE_NAME)
            .is_file()
    );
    assert!(
        !fs::read_to_string(&paths.user_config)
            .expect("provider config")
            .contains("disk-secret")
    );
}

#[tokio::test]
async fn verified_provider_replacement_deletes_superseded_secret() {
    let _home = IsolatedGolutraHome::new();
    let dir = tempdir().expect("dir");
    let paths = ProviderConfigPaths::global().expect("paths");
    let store = Arc::new(golutra_auth::MemorySecretStore::default());
    let previous_reference = disk_credential();
    store
        .set(
            &previous_reference,
            &SecretString::from("previous-key".to_owned()),
        )
        .expect("previous secret");
    let previous_profile = ProviderProfile::openai_compatible(
        "custom",
        "https://api.example.com/v1",
        "old-model",
        previous_reference.clone(),
    )
    .expect("previous profile");
    ProviderSettings {
        version: PROVIDER_SETTINGS_VERSION,
        active_profile: Some("custom".to_owned()),
        profiles: vec![previous_profile],
    }
    .save(&paths.user_config)
    .expect("previous settings");

    let current_reference = disk_credential();
    let base_url = spawn_probe_server(r#"{"data":[{"id":"new-model"}]}"#).await;
    let current_profile = ProviderProfile::openai_compatible(
        "custom",
        base_url,
        "new-model",
        current_reference.clone(),
    )
    .expect("current profile");

    apply_provider_install_plan_verified_with_store(
        &paths,
        dir.path(),
        &ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile: current_profile,
            activate: true,
            pending_secret: Some(SecretString::from("current-key".to_owned())),
        },
        store.clone(),
    )
    .await
    .expect("replacement");

    assert!(!store.contains(&previous_reference));
    assert!(store.contains(&current_reference));
    assert_eq!(
        ProviderSettings::load(&paths.user_config)
            .expect("settings")
            .active_profile()
            .and_then(|profile| profile.credential_ref.as_ref())
            .map(|reference| reference.id.as_str()),
        Some(current_reference.id.as_str())
    );
}

#[tokio::test]
async fn failed_provider_replacement_restores_previous_secret_and_profile() {
    let _home = IsolatedGolutraHome::new();
    let dir = tempdir().expect("dir");
    let paths = ProviderConfigPaths::global().expect("paths");
    let store = Arc::new(golutra_auth::MemorySecretStore::default());
    let previous_reference = disk_credential();
    store
        .set(
            &previous_reference,
            &SecretString::from("previous-key".to_owned()),
        )
        .expect("previous secret");
    let previous_profile = ProviderProfile::openai_compatible(
        "custom",
        "https://api.example.com/v1",
        "old-model",
        previous_reference.clone(),
    )
    .expect("previous profile");
    ProviderSettings {
        version: PROVIDER_SETTINGS_VERSION,
        active_profile: Some("custom".to_owned()),
        profiles: vec![previous_profile],
    }
    .save(&paths.user_config)
    .expect("previous settings");

    let current_reference = disk_credential();
    let base_url = spawn_probe_server_response(
        "401 Unauthorized",
        r#"{"error":{"message":"invalid credential"}}"#,
    )
    .await;
    let current_profile = ProviderProfile::openai_compatible(
        "custom",
        base_url,
        "new-model",
        current_reference.clone(),
    )
    .expect("current profile");

    let error = apply_provider_install_plan_verified_with_store(
        &paths,
        dir.path(),
        &ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile: current_profile,
            activate: true,
            pending_secret: Some(SecretString::from("current-key".to_owned())),
        },
        store.clone(),
    )
    .await
    .expect_err("probe failure");

    assert_eq!(error.step, "probe");
    assert!(store.contains(&previous_reference));
    assert!(!store.contains(&current_reference));
    assert_eq!(
        ProviderSettings::load(&paths.user_config)
            .expect("settings")
            .active_profile()
            .and_then(|profile| profile.credential_ref.as_ref())
            .map(|reference| reference.id.as_str()),
        Some(previous_reference.id.as_str())
    );
}

#[tokio::test]
async fn verified_oauth_install_and_logout_keep_tokens_out_of_config() {
    let _home = IsolatedGolutraHome::new();
    let dir = tempdir().expect("dir");
    let paths = ProviderConfigPaths::global().expect("paths");
    let store = Arc::new(golutra_auth::MemorySecretStore::default());
    let reference = oauth_credential();
    store_test_oauth_credential(store.as_ref(), &reference, "oauth-access-token");
    let base_url = spawn_probe_server(r#"{"data":[{"id":"gpt-oauth"}]}"#).await;
    let mut profile =
        ProviderProfile::openai_compatible("oauth", base_url, "gpt-oauth", reference.clone())
            .expect("profile");
    profile.oauth = Some(oauth_descriptor());

    apply_oauth_provider_install_plan_verified_with_store(
        &paths,
        dir.path(),
        &ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile,
            activate: true,
            pending_secret: None,
        },
        store.clone(),
    )
    .await
    .expect("OAuth install");

    let persisted = fs::read_to_string(&paths.user_config).expect("config");
    assert!(!persisted.contains("oauth-access-token"));
    assert!(!persisted.contains("refresh-test"));
    let settings = ProviderSettings::load(&paths.user_config).expect("settings");
    assert!(settings.active_profile().expect("active").oauth.is_some());

    logout_provider_profile_verified_with_store(&paths, dir.path(), "oauth", store.clone())
        .await
        .expect("logout");

    let settings = ProviderSettings::load(&paths.user_config).expect("settings");
    let profile = settings
        .profiles
        .iter()
        .find(|profile| profile.name == "oauth")
        .expect("profile");
    assert!(settings.active_profile.is_none());
    assert!(profile.credential_ref.is_none());
    assert!(profile.oauth.is_none());
    assert!(!profile.enabled);
    assert!(!store.contains(&reference));
}

#[tokio::test]
async fn failed_oauth_probe_rolls_back_profile_and_deletes_token_set() {
    let _home = IsolatedGolutraHome::new();
    let dir = tempdir().expect("dir");
    let paths = ProviderConfigPaths::global().expect("paths");
    let store = Arc::new(golutra_auth::MemorySecretStore::default());
    let reference = oauth_credential();
    store_test_oauth_credential(store.as_ref(), &reference, "oauth-access-token");
    let mut profile = ProviderProfile::openai_compatible(
        "oauth",
        "http://127.0.0.1:9/v1",
        "gpt-oauth",
        reference.clone(),
    )
    .expect("profile");
    profile.oauth = Some(oauth_descriptor());

    let error = apply_oauth_provider_install_plan_verified_with_store(
        &paths,
        dir.path(),
        &ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile,
            activate: true,
            pending_secret: None,
        },
        store.clone(),
    )
    .await
    .expect_err("probe must fail");

    assert_eq!(error.step, "probe");
    assert!(!paths.user_config.exists());
    assert!(!store.contains(&reference));
}

#[tokio::test]
async fn verified_workspace_provider_install_is_rejected() {
    let _home = IsolatedGolutraHome::new();
    let dir = tempdir().expect("dir");
    let paths = ProviderConfigPaths::global().expect("paths");
    let store = Arc::new(golutra_auth::MemorySecretStore::default());
    let base_url = spawn_probe_server(r#"{"data":[{"id":"gpt-5.5"}]}"#).await;
    let profile =
        ProviderProfile::openai_compatible("custom", base_url, "gpt-5.5", disk_credential())
            .expect("profile");

    let error = apply_provider_install_plan_verified_with_store(
        &paths,
        dir.path(),
        &ProviderInstallPlan {
            scope: ProviderConfigScope::Workspace,
            profile,
            activate: true,
            pending_secret: Some(SecretString::from("good-key".to_owned())),
        },
        store,
    )
    .await
    .expect_err("workspace scope rejected");

    assert_eq!(error.step, "mutate");
    assert!(
        error
            .message
            .contains("workspace provider config is no longer supported")
    );
    assert!(!paths.user_config.exists());
}

#[test]
fn provider_env_rejects_dangerous_names() {
    let error = ProviderProfile::openai_compatible(
        "golutra",
        "api.golutra.cn",
        "gpt-test",
        CredentialRef::environment("PATH", SecretKind::ApiKey).expect("reference"),
    )
    .expect_err("dangerous env rejected");

    assert!(matches!(error, ConfigError::Validation(_)));
}

#[test]
fn non_secret_runtime_settings_merge_by_layer_without_accepting_secrets() {
    let global = NonSecretRuntimeSettings {
        model: Some("global-model".to_owned()),
        verify_on_change: Some("off".to_owned()),
        ..Default::default()
    };
    let project = NonSecretRuntimeSettings {
        model: Some("project-model".to_owned()),
        provider_profile: Some("project-profile".to_owned()),
        ..Default::default()
    };
    let session = NonSecretRuntimeSettings {
        verify_on_change: Some("auto".to_owned()),
        ..Default::default()
    };
    let merged = NonSecretRuntimeSettings::merged(&global, &project, &session);
    assert_eq!(merged.model.as_deref(), Some("project-model"));
    assert_eq!(merged.provider_profile.as_deref(), Some("project-profile"));
    assert_eq!(merged.verify_on_change.as_deref(), Some("auto"));

    let error = serde_json::from_str::<NonSecretRuntimeSettings>(
        r#"{"model":"safe","api_key":"must-not-be-accepted"}"#,
    )
    .expect_err("secret fields must not be part of non-secret settings");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn non_secret_runtime_settings_load_project_layer() {
    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    let paths = ProviderConfigPaths::from_home(home.path()).expect("paths");
    fs::create_dir_all(workspace.path().join(".golutra")).expect("project config dir");
    fs::write(
        home.path().join("runtime.json"),
        r#"{"model":"global","verify_on_change":"off"}"#,
    )
    .expect("global settings");
    fs::write(
        workspace.path().join(".golutra/runtime.json"),
        r#"{"model":"project","verify_on_change":"auto"}"#,
    )
    .expect("project settings");

    let settings = load_non_secret_runtime_settings(&paths, workspace.path()).expect("merged");
    assert_eq!(settings.model.as_deref(), Some("project"));
    assert_eq!(settings.verify_on_change.as_deref(), Some("auto"));
}

#[test]
fn non_secret_runtime_settings_allow_an_uninitialized_global_home() {
    let parent = tempdir().expect("parent");
    let home = parent.path().join("new-home");
    let workspace = tempdir().expect("workspace");
    let paths = ProviderConfigPaths::from_home(&home).expect("paths");

    let settings = load_non_secret_runtime_settings(&paths, workspace.path())
        .expect("missing global home should be treated as empty settings");
    assert_eq!(settings, NonSecretRuntimeSettings::default());
}

#[cfg(unix)]
#[test]
fn non_secret_runtime_settings_reject_symlinked_layers() {
    use std::os::unix::fs::symlink;

    let home = tempdir().expect("home");
    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let paths = ProviderConfigPaths::from_home(home.path()).expect("paths");
    fs::create_dir_all(workspace.path().join(".golutra")).expect("project config dir");
    fs::write(
        outside.path().join("runtime.json"),
        r#"{"model":"outside"}"#,
    )
    .expect("outside settings");
    symlink(
        outside.path().join("runtime.json"),
        workspace.path().join(".golutra/runtime.json"),
    )
    .expect("runtime symlink");

    let error = load_non_secret_runtime_settings(&paths, workspace.path())
        .expect_err("symlinked project settings must be rejected");
    assert!(error.to_string().contains("must not be a symlink"));
}

#[test]
fn non_secret_runtime_settings_reject_invalid_reasoning_effort() {
    let error = NonSecretRuntimeSettings {
        reasoning_effort: Some("extreme".to_owned()),
        ..Default::default()
    }
    .validate()
    .expect_err("unknown reasoning effort must be rejected");
    assert!(error.to_string().contains("reasoning_effort"));
}
