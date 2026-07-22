//! Provider 认证向导的交互控制与配置事务。

use super::*;

pub(crate) async fn handle_auth_dialog_key(
    key: KeyEvent,
    app: &mut TuiApp,
    transport: &RuntimeTransport,
) -> miette::Result<()> {
    match key.code {
        KeyCode::Esc => {
            if let Some(dialog) = &mut app.auth_dialog {
                dialog.go_back();
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(dialog) = &mut app.auth_dialog {
                if key.code == KeyCode::Up || auth_step_accepts_vim_selection_keys(dialog) {
                    dialog.move_selection(ResumeSelectionDirection::Previous);
                } else if let KeyCode::Char(character) = key.code {
                    handle_auth_dialog_character(dialog, character);
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(dialog) = &mut app.auth_dialog {
                if key.code == KeyCode::Down || auth_step_accepts_vim_selection_keys(dialog) {
                    dialog.move_selection(ResumeSelectionDirection::Next);
                } else if let KeyCode::Char(character) = key.code {
                    handle_auth_dialog_character(dialog, character);
                }
            }
        }
        KeyCode::Backspace => {
            if let Some(input) = app
                .auth_dialog
                .as_mut()
                .and_then(AuthDialogState::current_input_mut)
            {
                delete_last_grapheme(input);
            }
        }
        KeyCode::Enter => {
            if let Err(error) = advance_auth_dialog(app, transport).await {
                report_auth_dialog_error(app, error);
            }
        }
        KeyCode::Char(character) if character.is_ascii_digit() => {
            if let Some(dialog) = &mut app.auth_dialog
                && matches!(
                    dialog.step,
                    AuthDialogStep::GroupChoice
                        | AuthDialogStep::ThirdPartyChoice
                        | AuthDialogStep::AuthMethod
                        | AuthDialogStep::Protocol
                        | AuthDialogStep::CredentialStore
                )
                && let Some(index) = character
                    .to_digit(10)
                    .and_then(|digit| digit.checked_sub(1))
            {
                let last_index = match dialog.step {
                    AuthDialogStep::GroupChoice => AUTH_GROUP_ITEMS.len().saturating_sub(1),
                    AuthDialogStep::ThirdPartyChoice => {
                        THIRD_PARTY_PROVIDER_PRESETS.len().saturating_sub(1)
                    }
                    AuthDialogStep::AuthMethod => dialog.auth_method_count().saturating_sub(1),
                    AuthDialogStep::Protocol => dialog.protocol_options().len().saturating_sub(1),
                    AuthDialogStep::CredentialStore => 1,
                    AuthDialogStep::BaseUrl
                    | AuthDialogStep::ApiKey
                    | AuthDialogStep::EnvKey
                    | AuthDialogStep::Model
                    | AuthDialogStep::AdvancedConfig
                    | AuthDialogStep::Review => 0,
                };
                if (index as usize) <= last_index {
                    dialog.selected = index as usize;
                    if let Err(error) = advance_auth_dialog(app, transport).await {
                        report_auth_dialog_error(app, error);
                    }
                }
            } else if let Some(dialog) = &mut app.auth_dialog {
                if dialog.step == AuthDialogStep::Model {
                    dialog.prepare_custom_model_input().push(character);
                } else if dialog.step == AuthDialogStep::AdvancedConfig {
                    if let Some(input) = dialog.current_input_mut() {
                        input.push(character);
                    }
                } else if let Some(input) = dialog.current_input_mut() {
                    input.push(character);
                }
            }
        }
        KeyCode::Char(character) => {
            if let Some(dialog) = &mut app.auth_dialog {
                handle_auth_dialog_character(dialog, character);
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn auth_step_accepts_vim_selection_keys(dialog: &AuthDialogState) -> bool {
    matches!(
        dialog.step,
        AuthDialogStep::GroupChoice
            | AuthDialogStep::ThirdPartyChoice
            | AuthDialogStep::AuthMethod
            | AuthDialogStep::Protocol
            | AuthDialogStep::CredentialStore
            | AuthDialogStep::AdvancedConfig
    )
}

pub(crate) fn handle_auth_dialog_character(dialog: &mut AuthDialogState, character: char) {
    if dialog.step == AuthDialogStep::AdvancedConfig {
        handle_auth_advanced_character(dialog, character);
    } else if dialog.step == AuthDialogStep::Model {
        dialog.prepare_custom_model_input().push(character);
    } else if let Some(input) = dialog.current_input_mut() {
        input.push(character);
    }
}

pub(crate) fn handle_auth_advanced_character(dialog: &mut AuthDialogState, character: char) {
    if dialog.advanced_selected == 4 {
        dialog.custom_headers.push(character);
        dialog.error = None;
        return;
    }
    match character {
        ' ' => dialog.toggle_advanced_item(),
        't' | 'T' => {
            dialog.advanced_selected = 0;
            dialog.toggle_advanced_item();
        }
        'r' | 'R' => {
            dialog.advanced_selected = 1;
            dialog.toggle_advanced_item();
        }
        'c' | 'C' => {
            dialog.advanced_selected = 2;
            dialog.error = None;
        }
        'm' | 'M' => {
            dialog.advanced_selected = 3;
            dialog.error = None;
        }
        character if character.is_ascii_digit() => {
            if let Some(input) = dialog.current_input_mut() {
                input.push(character);
            }
        }
        _ => {}
    }
}

pub(crate) async fn advance_auth_dialog(
    app: &mut TuiApp,
    transport: &RuntimeTransport,
) -> miette::Result<()> {
    let action = {
        let Some(dialog) = &mut app.auth_dialog else {
            return Ok(());
        };
        match dialog.step {
            AuthDialogStep::GroupChoice => match dialog.selected_group_action() {
                AuthGroupAction::Official => {
                    dialog.select_provider(OFFICIAL_PROVIDER_PRESET);
                    AuthAdvanceAction::None
                }
                AuthGroupAction::ThirdParty => {
                    dialog.step = AuthDialogStep::ThirdPartyChoice;
                    dialog.selected = 0;
                    dialog.error = None;
                    AuthAdvanceAction::None
                }
                AuthGroupAction::Custom => {
                    dialog.select_provider(CUSTOM_PROVIDER_PRESET);
                    AuthAdvanceAction::None
                }
                AuthGroupAction::Mock => AuthAdvanceAction::SaveMock,
                AuthGroupAction::Quit => AuthAdvanceAction::Quit,
            },
            AuthDialogStep::ThirdPartyChoice => {
                let provider = dialog.selected_third_party_provider();
                dialog.select_provider(provider);
                AuthAdvanceAction::None
            }
            AuthDialogStep::AuthMethod => {
                if let Some(method) = dialog.selected_oauth_method() {
                    AuthAdvanceAction::StartBuiltinOAuth(Box::new(method))
                } else if dialog.api_key_method_selected() {
                    dialog.step = if dialog.protocol_options().len() > 1 {
                        AuthDialogStep::Protocol
                    } else {
                        AuthDialogStep::BaseUrl
                    };
                    dialog.selected = 0;
                    dialog.error = None;
                    AuthAdvanceAction::None
                } else {
                    dialog.error = Some("No available authentication method".to_owned());
                    AuthAdvanceAction::None
                }
            }
            AuthDialogStep::Protocol => {
                dialog.protocol = dialog.selected_protocol();
                if dialog.base_url.is_empty()
                    && dialog
                        .provider
                        .is_none_or(|provider| provider.source != AuthProviderSource::Custom)
                {
                    dialog.base_url =
                        AuthDialogState::default_base_url_for_protocol(dialog.protocol).to_owned();
                }
                dialog.step = AuthDialogStep::BaseUrl;
                dialog.selected = 0;
                dialog.error = None;
                AuthAdvanceAction::None
            }
            AuthDialogStep::BaseUrl => {
                match validate_auth_base_url(&dialog.base_url) {
                    Ok(base_url) => {
                        dialog.base_url = base_url;
                        dialog.api_key_env = suggested_api_key_env(dialog);
                        dialog.step = if dialog.credential_store == AuthCredentialStore::Ephemeral {
                            AuthDialogStep::ApiKey
                        } else {
                            AuthDialogStep::CredentialStore
                        };
                        dialog.error = None;
                    }
                    Err(error) => {
                        dialog.error = Some(error);
                    }
                }
                AuthAdvanceAction::None
            }
            AuthDialogStep::CredentialStore => {
                dialog.credential_store = if dialog.selected == 1 {
                    AuthCredentialStore::Environment
                } else {
                    AuthCredentialStore::Disk
                };
                if dialog.credential_store == AuthCredentialStore::Environment {
                    dialog.api_key.clear();
                }
                dialog.step = if dialog.credential_store == AuthCredentialStore::Environment {
                    AuthDialogStep::EnvKey
                } else {
                    AuthDialogStep::ApiKey
                };
                dialog.selected = 0;
                dialog.error = None;
                AuthAdvanceAction::None
            }
            AuthDialogStep::ApiKey => {
                if dialog.api_key.trim().is_empty() {
                    dialog.error = Some("API key cannot be empty".to_owned());
                } else {
                    dialog.api_key = dialog.api_key.trim().to_owned();
                    dialog.step = AuthDialogStep::Model;
                    dialog.selected = 0;
                    dialog.error = None;
                }
                AuthAdvanceAction::None
            }
            AuthDialogStep::EnvKey => {
                match CredentialRef::environment(dialog.api_key_env.trim(), SecretKind::ApiKey) {
                    Ok(_) => {
                        dialog.api_key_env = dialog.api_key_env.trim().to_owned();
                        dialog.step = AuthDialogStep::Model;
                        dialog.selected = 0;
                        dialog.error = None;
                    }
                    Err(error) => dialog.error = Some(error.to_string()),
                }
                AuthAdvanceAction::None
            }
            AuthDialogStep::Model => {
                if let Some(model) = dialog.selected_recommended_model() {
                    dialog.model = model.to_owned();
                }
                dialog.model = normalize_model_id(&dialog.model);
                if dialog.model.is_empty() {
                    dialog.error = Some("Model cannot be empty".to_owned());
                    AuthAdvanceAction::None
                } else if !custom_provider_protocol_is_runtime_supported(dialog.protocol) {
                    dialog.error = Some(format!(
                        "{} setup is recognized, but Golutra live runtime currently only supports OpenAI-compatible providers",
                        protocol_option_text(dialog.protocol).0
                    ));
                    AuthAdvanceAction::None
                } else {
                    dialog.step = AuthDialogStep::AdvancedConfig;
                    dialog.error = None;
                    AuthAdvanceAction::None
                }
            }
            AuthDialogStep::AdvancedConfig => {
                match validate_generation_config(dialog).and_then(|_| build_auth_review(dialog)) {
                    Ok(review) => {
                        dialog.review = Some(review);
                        dialog.step = AuthDialogStep::Review;
                        dialog.error = None;
                    }
                    Err(error) => {
                        dialog.error = Some(error);
                    }
                }
                AuthAdvanceAction::None
            }
            AuthDialogStep::Review => match auth_login(dialog) {
                Ok(login) => AuthAdvanceAction::SaveOpenAiCompatible(Box::new(login)),
                Err(error) => {
                    dialog.error = Some(error);
                    AuthAdvanceAction::None
                }
            },
        }
    };
    match action {
        AuthAdvanceAction::None => {}
        AuthAdvanceAction::SaveMock => {
            apply_auth_mock()?;
            notify_runtime_provider_configured(transport, app.session_id).await?;
            app.refresh_provider_status();
            app.auth_dialog = None;
            app.status_message = "using mock provider".to_owned();
        }
        AuthAdvanceAction::SaveOpenAiCompatible(login) => {
            apply_auth_login(transport, *login).await?;
            notify_runtime_provider_configured(transport, app.session_id).await?;
            app.refresh_provider_status();
            app.auth_dialog = None;
            app.status_message = "provider connected".to_owned();
        }
        AuthAdvanceAction::StartBuiltinOAuth(method) => {
            app.auth_dialog = None;
            app.start_builtin_oauth_login(transport, *method)?;
        }
        AuthAdvanceAction::Quit => app.should_quit = true,
    }
    Ok(())
}

pub(crate) fn report_auth_dialog_error(app: &mut TuiApp, error: miette::Report) {
    let message = error.to_string();
    if let Some(dialog) = &mut app.auth_dialog {
        dialog.error = Some(message);
    }
    app.status_message = "provider setup failed".to_owned();
}

pub(crate) fn custom_provider_protocol_is_runtime_supported(protocol: ProviderProtocol) -> bool {
    provider_protocol_has_runtime_adapter(protocol)
}

pub(crate) fn initial_auth_dialog() -> Option<AuthDialogState> {
    match provider_onboarding_state() {
        Ok(state) if state.configured => None,
        Ok(_) | Err(_) => Some(AuthDialogState::new()),
    }
}

pub(crate) fn provider_paths_for_tui() -> miette::Result<ProviderConfigPaths> {
    ProviderConfigPaths::global().map_err(|error| miette::miette!("{error}"))
}

pub(crate) fn provider_cwd_for_tui(
    transport: &RuntimeTransport,
) -> miette::Result<&std::path::Path> {
    transport
        .cwd()
        .ok_or_else(|| miette::miette!("provider config requires a cwd"))
}

pub(crate) fn resolve_auth_descriptor_path(cwd: &std::path::Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

pub(crate) fn load_oauth_descriptor_for_tui(
    path: &std::path::Path,
) -> Result<OAuthProviderDescriptor, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read OAuth descriptor: {error}"))?;
    let descriptor: OAuthProviderDescriptor = serde_json::from_str(&content)
        .map_err(|error| format!("OAuth descriptor JSON is invalid: {error}"))?;
    descriptor.validate().map_err(|error| error.to_string())?;
    Ok(descriptor)
}

pub(crate) fn oauth_credential_source(
    store: AuthCredentialStore,
) -> Result<CredentialSource, String> {
    match store {
        AuthCredentialStore::Disk => Ok(CredentialSource::Disk),
        AuthCredentialStore::Ephemeral => Ok(CredentialSource::Ephemeral),
        AuthCredentialStore::Environment => {
            Err("OAuth login requires disk or ephemeral storage".to_owned())
        }
    }
}

pub(crate) async fn run_oauth_login_task(
    paths: ProviderConfigPaths,
    cwd: PathBuf,
    descriptor: OAuthProviderDescriptor,
    command: OAuthLoginCommand,
    cancellation: CancellationToken,
    progress: mpsc::UnboundedSender<AuthTaskProgress>,
) -> Result<AuthTaskOutcome, String> {
    descriptor.validate().map_err(|error| error.to_string())?;
    if !descriptor.flows.contains(&command.flow) {
        return Err(format!(
            "OAuth descriptor `{}` does not support {:?}",
            descriptor.provider_id, command.flow
        ));
    }
    let validation_reference = CredentialRef::ephemeral(SecretKind::OAuthTokenSet);
    let mut profile = ProviderProfile::live_profile(
        command.profile.clone(),
        command.protocol,
        command.base_url,
        command.model,
        validation_reference,
    )
    .map_err(|error| error.to_string())?;
    profile.oauth = Some(descriptor.clone());
    profile.generation_config = command.generation_config;
    profile.validate().map_err(|error| error.to_string())?;
    let auth = provider_auth_service(&paths).map_err(|error| error.to_string())?;
    let source = oauth_credential_source(command.credential_store)?;
    let login_result = match command.flow {
        OAuthFlow::BrowserPkce => {
            let login = auth
                .begin_browser_login(descriptor.clone(), source)
                .await
                .map_err(|error| error.to_string())?;
            let authorization_url = login.authorization_url().to_owned();
            let _ = progress.send(AuthTaskProgress {
                title: "OAuth authorization".to_owned(),
                body: vec![
                    "Open this URL in your browser:".to_owned(),
                    authorization_url,
                ],
            });
            if !command.no_open_browser
                && let Err(error) = login.open_browser().await
            {
                let _ = progress.send(AuthTaskProgress {
                    title: "OAuth browser".to_owned(),
                    body: vec![format!(
                        "browser could not be opened automatically: {error}"
                    )],
                });
            }
            tokio::select! {
                () = cancellation.cancelled() => return Err("OAuth authorization cancelled".to_owned()),
                result = login.complete() => result.map_err(|error| error.to_string())?,
            }
        }
        OAuthFlow::DeviceCode => {
            let login = auth
                .begin_device_login(descriptor.clone(), source)
                .await
                .map_err(|error| error.to_string())?;
            let verification_url = login
                .verification_uri_complete()
                .unwrap_or_else(|| login.verification_uri());
            let _ = progress.send(AuthTaskProgress {
                title: "OAuth device authorization".to_owned(),
                body: vec![
                    format!("Open {verification_url}"),
                    format!("Enter code {}", login.user_code()),
                ],
            });
            tokio::select! {
                () = cancellation.cancelled() => return Err("OAuth authorization cancelled".to_owned()),
                result = login.complete() => result.map_err(|error| error.to_string())?,
            }
        }
        OAuthFlow::OpenAiDeviceAuth => {
            let login = auth
                .begin_openai_device_login(descriptor.clone(), source)
                .await
                .map_err(|error| error.to_string())?;
            let _ = progress.send(AuthTaskProgress {
                title: "OAuth device authorization".to_owned(),
                body: vec![
                    format!("Open {}", login.verification_uri()),
                    format!("Enter code {}", login.user_code()),
                ],
            });
            if !command.no_open_browser
                && let Err(error) = login.open_browser().await
            {
                let _ = progress.send(AuthTaskProgress {
                    title: "OAuth browser".to_owned(),
                    body: vec![format!(
                        "browser could not be opened automatically: {error}"
                    )],
                });
            }
            tokio::select! {
                () = cancellation.cancelled() => return Err("OAuth authorization cancelled".to_owned()),
                result = login.complete() => result.map_err(|error| error.to_string())?,
            }
        }
    };
    if cancellation.is_cancelled() {
        let _ = auth
            .logout(&login_result.credential_ref, Some(&descriptor))
            .await;
        return Err("OAuth authorization cancelled".to_owned());
    }
    profile.credential_ref = Some(login_result.credential_ref);
    apply_oauth_provider_install_plan_verified(
        &paths,
        &cwd,
        &ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile,
            activate: true,
            pending_secret: None,
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(AuthTaskOutcome {
        title: "Auth updated".to_owned(),
        body: vec![
            format!("provider profile {} connected", command.profile),
            format!("OAuth provider {}", descriptor.provider_id),
            format!(
                "token refresh {}",
                if login_result.metadata.refreshable {
                    "enabled"
                } else {
                    "requires re-login after expiry"
                }
            ),
        ],
    })
}

pub(crate) fn provider_scope(scope: AuthConfigScope) -> ProviderConfigScope {
    match scope {
        AuthConfigScope::User => ProviderConfigScope::User,
        AuthConfigScope::Workspace => ProviderConfigScope::Workspace,
    }
}

pub(crate) fn auth_login(dialog: &AuthDialogState) -> Result<OpenAiCompatibleLogin, String> {
    let provider = dialog.provider.unwrap_or(CUSTOM_PROVIDER_PRESET);
    let api_key_env = if dialog.api_key_env.trim().is_empty() {
        suggested_api_key_env(dialog)
    } else {
        dialog.api_key_env.trim().to_owned()
    };
    Ok(OpenAiCompatibleLogin {
        profile: provider.profile.to_owned(),
        protocol: dialog.protocol,
        base_url: dialog.base_url.trim().to_owned(),
        model: dialog.model.trim().to_owned(),
        api_key_env,
        api_key: (dialog.credential_store != AuthCredentialStore::Environment)
            .then(|| dialog.api_key.trim().to_owned()),
        credential_store: dialog.credential_store,
        credential_ref: dialog
            .review
            .as_ref()
            .map(|review| review.credential_ref.clone()),
        generation_config: validate_generation_config(dialog)?,
        custom_headers: parse_dialog_custom_headers(&dialog.custom_headers)?,
        scope: AuthConfigScope::User,
    })
}

pub(crate) fn suggested_api_key_env(dialog: &AuthDialogState) -> String {
    match dialog.provider.unwrap_or(CUSTOM_PROVIDER_PRESET).source {
        AuthProviderSource::Custom => {
            generate_custom_provider_api_key_env(dialog.protocol, dialog.base_url.trim())
        }
        AuthProviderSource::Official | AuthProviderSource::ThirdParty => {
            "GOLUTRA_PROVIDER_API_KEY".to_owned()
        }
    }
}

pub(crate) fn build_auth_review(dialog: &AuthDialogState) -> Result<AuthReview, String> {
    let provider = dialog.provider.unwrap_or(CUSTOM_PROVIDER_PRESET);
    let login = auth_login(dialog)?;
    let paths = provider_paths_for_tui().map_err(|error| error.to_string())?;
    let scope = provider_scope(login.scope);
    let config_path = paths.user_config.clone();
    let (updates_existing_profile, replaces_unreadable_config) =
        match load_provider_settings(&paths) {
            Ok(settings) => (
                settings
                    .profiles
                    .iter()
                    .any(|profile| profile.name == login.profile),
                false,
            ),
            Err(golutra_config::ConfigError::Json(_)) => (false, true),
            Err(error) => return Err(error.to_string()),
        };

    let (credential_ref, _) = credential_for_login(&login)?;
    let mut preview_profile = ProviderProfile::live_profile(
        login.profile.clone(),
        login.protocol,
        login.base_url.clone(),
        login.model.clone(),
        credential_ref.clone(),
    )
    .map_err(|error| error.to_string())?;
    preview_profile.generation_config = login.generation_config.clone();
    preview_profile.custom_headers = login.custom_headers.clone();
    let preview_plan = ProviderInstallPlan {
        scope,
        profile: preview_profile,
        activate: true,
        pending_secret: None,
    };
    let preview_json =
        serde_json::to_string_pretty(&preview_plan).map_err(|error| error.to_string())?;

    Ok(AuthReview {
        provider_title: provider.title,
        profile: login.profile,
        protocol: login.protocol.id().to_owned(),
        base_url: login.base_url,
        model: login.model,
        credential: match login.credential_store {
            AuthCredentialStore::Environment => format!("env:{}", login.api_key_env),
            AuthCredentialStore::Disk => "disk:$GOLUTRA_HOME/credentials.json".to_owned(),
            AuthCredentialStore::Ephemeral => format!(
                "ephemeral:{}",
                mask_api_key(login.api_key.as_deref().unwrap_or_default())
            ),
        },
        credential_ref,
        advanced: advanced_config_summary(login.generation_config.as_ref(), &login.custom_headers),
        scope,
        config_path,
        updates_existing_profile,
        replaces_unreadable_config,
        preview_json,
    })
}

pub(crate) fn validate_auth_base_url(value: &str) -> Result<String, String> {
    let trimmed = value.trim().trim_end_matches('/').to_owned();
    if trimmed.is_empty() {
        return Err("Base URL cannot be empty".to_owned());
    }
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err("Base URL must start with http:// or https://".to_owned());
    }
    let Some((_, rest)) = trimmed.split_once("://") else {
        return Err("Base URL must start with http:// or https://".to_owned());
    };
    if rest.split('/').next().unwrap_or_default().trim().is_empty() {
        return Err("Base URL host cannot be empty".to_owned());
    }
    Ok(trimmed)
}

pub(crate) fn normalize_model_id(value: &str) -> String {
    value
        .split(',')
        .map(str::trim)
        .find(|model| !model.is_empty())
        .unwrap_or_default()
        .to_owned()
}

pub(crate) fn mask_api_key(value: &str) -> String {
    let length = value.chars().count();
    if length <= 8 {
        return "***".to_owned();
    }
    let prefix = value.chars().take(4).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{prefix}...{suffix}")
}

pub(crate) async fn apply_auth_login(
    transport: &RuntimeTransport,
    login: OpenAiCompatibleLogin,
) -> miette::Result<()> {
    let paths = provider_paths_for_tui()?;
    let cwd = provider_cwd_for_tui(transport)?;
    let scope = provider_scope(login.scope);
    let (credential_ref, pending_secret) =
        credential_for_login(&login).map_err(|error| miette::miette!("{error}"))?;
    let mut profile = ProviderProfile::live_profile(
        login.profile,
        login.protocol,
        login.base_url,
        login.model,
        credential_ref,
    )
    .map_err(|error| miette::miette!("{error}"))?;
    profile.generation_config = login.generation_config;
    profile.custom_headers = login.custom_headers;
    apply_provider_install_plan_verified(
        &paths,
        cwd,
        &ProviderInstallPlan {
            scope,
            profile,
            activate: true,
            pending_secret,
        },
    )
    .await
    .map_err(|error| miette::miette!("{error}"))?;

    Ok(())
}

pub(crate) async fn notify_runtime_provider_configured(
    transport: &RuntimeTransport,
    session_id: SessionId,
) -> miette::Result<()> {
    let ack = transport
        .send_command(session_command(
            session_id,
            SessionCommandKind::ProviderConfigured,
            json!({"verified": true}),
        ))
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    if ack.accepted {
        Ok(())
    } else {
        Err(miette::miette!(
            "runtime rejected provider reload: {}",
            ack.reason.unwrap_or_else(|| "unknown reason".to_owned())
        ))
    }
}

pub(crate) fn credential_for_login(
    login: &OpenAiCompatibleLogin,
) -> Result<(CredentialRef, Option<SecretString>), String> {
    match login
        .api_key
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        Some(api_key) => {
            let reference = match (login.credential_ref.clone(), login.credential_store) {
                (Some(reference), AuthCredentialStore::Disk)
                    if matches!(reference.source, CredentialSource::Disk) =>
                {
                    reference
                }
                (Some(reference), AuthCredentialStore::Ephemeral)
                    if matches!(reference.source, CredentialSource::Ephemeral) =>
                {
                    reference
                }
                (Some(_), _) => {
                    return Err("reviewed credential source no longer matches setup".to_owned());
                }
                (None, AuthCredentialStore::Disk) => CredentialRef::disk(SecretKind::ApiKey),
                (None, AuthCredentialStore::Environment) => {
                    return Err(
                        "an inline API key cannot use environment storage; omit --api-key and provide --api-key-env"
                            .to_owned(),
                    );
                }
                (None, AuthCredentialStore::Ephemeral) => {
                    CredentialRef::ephemeral(SecretKind::ApiKey)
                }
            };
            Ok((reference, Some(SecretString::from(api_key.to_owned()))))
        }
        None => match login.credential_ref.clone() {
            Some(reference) if matches!(reference.source, CredentialSource::Environment { .. }) => {
                Ok((reference, None))
            }
            Some(_) => Err("reviewed credential source no longer matches setup".to_owned()),
            None => CredentialRef::environment(login.api_key_env.clone(), SecretKind::ApiKey)
                .map(|reference| (reference, None))
                .map_err(|error| error.to_string()),
        },
    }
}

pub(crate) fn validate_generation_config(
    dialog: &AuthDialogState,
) -> Result<Option<ProviderGenerationConfig>, String> {
    let context_window_size =
        parse_optional_positive_u64(&dialog.context_window_size, "Context window size")?;
    let max_tokens = parse_optional_positive_u64(&dialog.max_tokens, "Max output tokens")?;
    let config = ProviderGenerationConfig {
        enable_thinking: dialog.enable_thinking,
        reasoning_effort: dialog.reasoning_effort,
        context_window_size,
        max_tokens,
    };
    Ok((!config.is_empty()).then_some(config))
}

pub(crate) fn parse_dialog_custom_headers(
    input: &str,
) -> Result<Vec<ProviderHeaderConfig>, String> {
    let mut headers = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    for assignment in input
        .split(';')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let (name, value) = assignment.split_once('=').ok_or_else(|| {
            "Custom headers must use Name=Value separated by semicolons".to_owned()
        })?;
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() || value.is_empty() {
            return Err("Custom headers require non-empty names and values".to_owned());
        }
        let value = match value.strip_prefix('@') {
            Some(key) if !key.trim().is_empty() => ProviderHeaderValue::Environment {
                key: key.trim().to_owned(),
            },
            Some(_) => return Err("Custom header environment key cannot be empty".to_owned()),
            None => ProviderHeaderValue::Literal {
                value: value.to_owned(),
            },
        };
        let header = ProviderHeaderConfig {
            name: name.to_owned(),
            value,
        };
        header.validate()?;
        if !names.insert(name.to_ascii_lowercase()) {
            return Err(format!(
                "Custom header `{name}` is configured more than once"
            ));
        }
        headers.push(header);
    }
    Ok(headers)
}

fn advanced_config_summary(
    generation_config: Option<&ProviderGenerationConfig>,
    headers: &[ProviderHeaderConfig],
) -> String {
    let generation = generation_config_summary(generation_config);
    if headers.is_empty() {
        generation
    } else {
        let names = headers
            .iter()
            .map(|header| header.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!("{generation}; headers: {names}")
    }
}

pub(crate) fn parse_optional_positive_u64(value: &str, label: &str) -> Result<Option<u64>, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let parsed = trimmed
        .parse::<u64>()
        .map_err(|_| format!("{label} must be a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{label} must be a positive integer"));
    }
    Ok(Some(parsed))
}

pub(crate) fn apply_auth_mock() -> miette::Result<()> {
    let paths = provider_paths_for_tui()?;
    ProviderInstallPlan {
        scope: ProviderConfigScope::User,
        profile: ProviderProfile::mock(),
        activate: true,
        pending_secret: None,
    }
    .apply(&paths)
    .map_err(|error| miette::miette!("{error}"))
}
