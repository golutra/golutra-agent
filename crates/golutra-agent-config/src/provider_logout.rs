//! 退出当前 Provider：原子移除配置与本地凭据，其他配置及历史不属于清理范围。

use super::*;

#[derive(Debug)]
pub struct ProviderLogoutOutcome {
    pub profile: Option<String>,
    pub revocation_warning: Option<String>,
}

/// 清除当前配置；无活动配置时幂等成功。远端撤销失败不阻止本地退出，作为警告返回。
pub async fn forget_active_provider_verified(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
) -> Result<ProviderLogoutOutcome, ProviderInstallError> {
    let store = default_secret_store(paths)
        .map_err(|error| provider_install_error("secret-store", error.to_string()))?;
    forget_active_provider_with_store(paths, workspace_root, store).await
}

async fn forget_active_provider_with_store(
    paths: &ProviderConfigPaths,
    workspace_root: impl AsRef<Path>,
    store: Arc<dyn SecretStore>,
) -> Result<ProviderLogoutOutcome, ProviderInstallError> {
    let settings = load_provider_settings(paths)
        .map_err(|error| provider_install_error("load", error.to_string()))?;
    let Some(profile) = settings.active_profile().cloned() else {
        return Ok(ProviderLogoutOutcome {
            profile: None,
            revocation_warning: None,
        });
    };
    let revocation_warning = if let Some(reference) = &profile.credential_ref {
        let auth = AuthService::new(paths.home.clone(), Arc::clone(&store))
            .map_err(|error| provider_install_error("oauth", error.to_string()))?;
        auth.revoke(reference, profile.oauth.as_ref())
            .await
            .err()
            .map(|error| error.to_string())
    } else {
        None
    };
    let name = profile.name.clone();
    run_provider_settings_transaction(paths, workspace_root, store, move |settings| {
        // 撤销请求期间可能有其他进程修改配置；不能删除它刚登录或切换的新配置。
        if settings.active_profile() != Some(&profile) {
            return Err(ConfigError::Validation(
                "active provider changed during logout; retry /logout".to_owned(),
            ));
        }
        settings.profiles.retain(|entry| entry.name != profile.name);
        settings.active_profile = None;
        Ok(profile
            .credential_ref
            .into_iter()
            .filter(|reference| !matches!(reference.source, CredentialSource::Environment { .. }))
            .map(|reference| SecretMutation {
                reference,
                action: SecretMutationAction::Delete,
            })
            .collect())
    })
    .await?;
    Ok(ProviderLogoutOutcome {
        profile: Some(name),
        revocation_warning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn logout_deletes_only_active_profile_and_its_disk_secret() {
        let home = tempfile::tempdir().unwrap();
        let paths = ProviderConfigPaths::from_home(home.path()).unwrap();
        let store = DefaultSecretStore::new(paths.home.clone()).unwrap();
        let mut settings = ProviderSettings::default();
        let mut references = Vec::new();
        for name in ["spare", "current"] {
            let reference = CredentialRef::disk(SecretKind::ApiKey);
            store
                .set(&reference, &SecretString::from(format!("test-{name}")))
                .unwrap();
            settings.upsert_profile(
                ProviderProfile::openai_compatible(
                    name,
                    "https://example.com/v1",
                    "test-model",
                    reference.clone(),
                )
                .unwrap(),
                true,
            );
            references.push(reference);
        }
        settings.save(&paths.user_config).unwrap();
        let unrelated = paths.home.join("settings.json");
        fs::write(&unrelated, b"untouched").unwrap();
        let result = forget_active_provider_verified(&paths, home.path())
            .await
            .unwrap();
        assert_eq!(result.profile.as_deref(), Some("current"));
        assert!(result.revocation_warning.is_none());
        let saved = load_provider_settings(&paths).unwrap();
        assert!(saved.active_profile.is_none());
        assert_eq!(saved.profiles, settings.profiles[..1]);
        assert!(store.get(&references[0]).unwrap().is_some());
        assert!(store.get(&references[1]).unwrap().is_none());
        assert_eq!(fs::read(unrelated).unwrap(), b"untouched");
        assert!(
            forget_active_provider_verified(&paths, home.path())
                .await
                .unwrap()
                .profile
                .is_none()
        );
        assert_eq!(load_provider_settings(&paths).unwrap(), saved);
    }

    #[tokio::test]
    async fn logout_supports_mock_and_read_only_environment_references() {
        let home = tempfile::tempdir().unwrap();
        let paths = ProviderConfigPaths::from_home(home.path()).unwrap();
        let env_ref =
            CredentialRef::environment("GOLUTRA_AGENT_LOGOUT_TEST_KEY", SecretKind::ApiKey)
                .unwrap();
        for profile in [
            ProviderProfile::mock(),
            ProviderProfile::openai_compatible(
                "env",
                "https://example.com/v1",
                "test-model",
                env_ref,
            )
            .unwrap(),
        ] {
            let name = profile.name.clone();
            let mut settings = ProviderSettings::default();
            settings.upsert_profile(profile, true);
            settings.save(&paths.user_config).unwrap();
            let result = forget_active_provider_verified(&paths, home.path())
                .await
                .unwrap();
            assert_eq!(result.profile, Some(name));
            assert_eq!(
                load_provider_settings(&paths).unwrap(),
                ProviderSettings::default()
            );
            assert!(!paths.home.join("credentials.json").exists());
        }
    }
}
