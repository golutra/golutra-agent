//! 认证向导的异步模型发现；请求身份隔离返回、换 Key 和重开向导产生的旧结果。

use super::*;

impl TuiApp {
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
