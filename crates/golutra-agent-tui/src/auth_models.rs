//! 认证向导的异步模型发现与协议探测；用请求身份隔离返回、换 Key 和重开向导产生的旧结果。

use super::*;
use sha2::{Digest, Sha256};

fn protocol_detection_input(
    dialog: &AuthDialogState,
) -> Result<(OpenAiCompatibleLogin, String, ProtocolDetectionFingerprint), String> {
    let login = auth_login(dialog)?;
    let key = if dialog.credential_store == AuthCredentialStore::Environment {
        std::env::var(dialog.api_key_env.trim()).unwrap_or_default()
    } else {
        dialog.api_key.clone()
    };
    // 比较环境引用的当前值，防止引用名未变但内容已变时误用缓存。
    // 原始引用仍传给适配器，不能把敏感 header 转为配置层禁止的明文值。
    let mut resolved_headers = std::collections::BTreeMap::new();
    for header in &login.custom_headers {
        header.validate()?;
        let value = match &header.value {
            ProviderHeaderValue::Environment { key } => std::env::var(key)
                .ok()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("Provider header environment variable {key} is not set"))?,
            ProviderHeaderValue::Literal { value } => value.clone(),
        };
        resolved_headers.insert(&header.name, value);
    }
    let input = serde_json::to_vec(&(
        &login.base_url,
        &login.model,
        key.trim(),
        &login.generation_config,
        resolved_headers,
    ))
    .expect("protocol detection input serializes");
    let fingerprint = ProtocolDetectionFingerprint(Sha256::digest(input).into());
    Ok((login, key, fingerprint))
}

fn open_detected_protocol_review(dialog: &mut AuthDialogState, protocol: ProviderProtocol) {
    dialog.protocol = protocol;
    dialog.protocol_detection = ModelDiscoveryState::Ready;
    dialog.error = None;
    match build_auth_review(dialog) {
        Ok(review) => {
            dialog.review = Some(review);
            dialog.step = AuthDialogStep::Review;
            dialog.scroll = 0;
            dialog.manual_scroll = false;
        }
        Err(error) => dialog.error = Some(error),
    }
}

impl TuiApp {
    pub(crate) fn cancel_auth_protocol_detection(&mut self) {
        if let Some(pending) = self.auth_protocol_detection.take() {
            pending.task.abort();
            if let Some(dialog) = &mut self.auth_dialog
                && matches!(dialog.protocol_detection, ModelDiscoveryState::Loading(id) if id == pending.id)
            {
                dialog.protocol_detection = ModelDiscoveryState::Idle;
            }
        }
    }

    pub(crate) fn start_auth_protocol_detection(&mut self) -> miette::Result<()> {
        self.cancel_auth_protocol_detection();
        let Some(dialog) = &mut self.auth_dialog else {
            return Ok(());
        };
        let (login, key, fingerprint) = match protocol_detection_input(dialog) {
            Ok(input) => input,
            Err(error) => {
                dialog.error = Some(error);
                return Ok(());
            }
        };
        if let Some(cached) = &dialog.successful_protocol_detection
            && cached.fingerprint == fingerprint
        {
            open_detected_protocol_review(dialog, cached.protocol);
            return Ok(());
        }
        dialog.successful_protocol_detection = None;
        dialog.review = None;
        let id = Uuid::new_v4();
        dialog.protocol_detection = ModelDiscoveryState::Loading(id);
        dialog.error = None;
        self.auth_protocol_detection = Some(PendingProtocolDetection {
            id,
            fingerprint,
            task: tokio::spawn(async move {
                golutra_agent_llm::detect_provider_protocol(
                    AUTH_PROTOCOL_OPTIONS,
                    &login.base_url,
                    &login.model,
                    key.trim(),
                    login.generation_config,
                    login.custom_headers,
                )
                .await
            }),
        });
        Ok(())
    }

    pub(crate) async fn poll_auth_protocol_detection(&mut self) -> bool {
        let Some(pending) = &self.auth_protocol_detection else {
            return false;
        };
        let current = self.auth_dialog.as_ref().is_some_and(|dialog| {
            dialog.step == AuthDialogStep::AdvancedConfig
                && matches!(dialog.protocol_detection, ModelDiscoveryState::Loading(id) if id == pending.id)
        });
        if !current {
            self.cancel_auth_protocol_detection();
            return true;
        }
        if !pending.task.is_finished() {
            return false;
        }
        let pending = self
            .auth_protocol_detection
            .take()
            .expect("pending detection");
        let result = pending
            .task
            .await
            .unwrap_or_else(|_| Err("Protocol detection interrupted".to_owned()));
        let dialog = self.auth_dialog.as_mut().expect("current detection dialog");
        // 交互输入被冻结，但外部环境仍可能改变；旧请求不能认证新配置。
        if !protocol_detection_input(dialog)
            .is_ok_and(|(_, _, fingerprint)| fingerprint == pending.fingerprint)
        {
            let error =
                "Provider settings changed during detection; select Continue to retry".to_owned();
            dialog.protocol_detection = ModelDiscoveryState::Failed(error.clone());
            dialog.error = Some(error);
            return true;
        }
        match result {
            Ok(protocol) => {
                dialog.successful_protocol_detection = Some(SuccessfulProtocolDetection {
                    fingerprint: pending.fingerprint,
                    protocol,
                });
                open_detected_protocol_review(dialog, protocol);
            }
            Err(error) => {
                dialog.protocol_detection = ModelDiscoveryState::Failed(error.clone());
                dialog.error = Some(error);
            }
        }
        true
    }

    pub(crate) fn cancel_auth_model_discovery(&mut self) {
        if let Some(pending) = self.auth_model_discovery.take() {
            pending.task.abort();
            if let Some(dialog) = self.auth_dialog.as_mut()
                && matches!(dialog.model_discovery, ModelDiscoveryState::Loading(id) if id == pending.id)
            {
                dialog.model_discovery = ModelDiscoveryState::Idle;
            }
        }
    }

    pub(crate) fn start_auth_model_discovery(&mut self) {
        if let Some(pending) = self.auth_model_discovery.take() {
            pending.task.abort();
        }
        let Some(dialog) = self.auth_dialog.as_mut() else {
            return;
        };
        if !dialog.provider.is_some_and(|provider| {
            matches!(
                provider.source,
                AuthProviderSource::Official | AuthProviderSource::Custom
            )
        }) {
            return;
        }
        dialog.models.clear();
        dialog.selected = 0;
        dialog.manual_model_input = !dialog.model.is_empty();
        let key = if dialog.credential_store == AuthCredentialStore::Environment {
            std::env::var(&dialog.api_key_env).unwrap_or_default()
        } else {
            dialog.api_key.clone()
        };
        if key.trim().is_empty() {
            dialog.model_discovery = ModelDiscoveryState::Failed(
                "API key is unavailable in the current environment".to_owned(),
            );
            return;
        }
        let id = Uuid::new_v4();
        let base_url = dialog.base_url.clone();
        let protocol = dialog.protocol;
        dialog.model_discovery = ModelDiscoveryState::Loading(id);
        self.auth_model_discovery = Some(PendingModelDiscovery {
            id,
            task: tokio::spawn(async move {
                #[cfg(not(test))]
                let result =
                    golutra_agent_llm::discover_provider_models(protocol, &base_url, key.trim())
                        .await;
                // 本地 HTTP 交互测试验证同一发现路径，但不读取宿主的系统代理配置。
                #[cfg(test)]
                let result = golutra_agent_llm::discover_provider_models_with_client_builder(
                    protocol,
                    &base_url,
                    key.trim(),
                    reqwest::Client::builder().no_proxy(),
                )
                .await;
                result
            }),
        });
    }

    pub(crate) async fn poll_auth_model_discovery(&mut self) -> bool {
        let Some(pending) = self.auth_model_discovery.as_ref() else {
            return false;
        };
        let current = self.auth_dialog.as_ref().is_some_and(|dialog| {
            dialog.step == AuthDialogStep::Model
                && matches!(dialog.model_discovery, ModelDiscoveryState::Loading(id) if id == pending.id)
        });
        if !current {
            self.cancel_auth_model_discovery();
            return false;
        }
        if !pending.task.is_finished() {
            return false;
        }
        let pending = self.auth_model_discovery.take().expect("pending discovery");
        let result = pending
            .task
            .await
            .unwrap_or_else(|_| Err("Model catalog request was interrupted".to_owned()));
        let dialog = self.auth_dialog.as_mut().expect("current discovery dialog");
        match result {
            Ok(models) => {
                // 手动输入恒在索引 0；目录只追加在后，不改用户的草稿、焦点或滚动状态。
                dialog.models = models;
                dialog.model_discovery = ModelDiscoveryState::Ready;
            }
            Err(error) => dialog.model_discovery = ModelDiscoveryState::Failed(error),
        }
        true
    }
}
