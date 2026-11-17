//! 会话内可修改的选择，稳定工具合同与跨进程固定绑定；不在失败后自动换浏览器。
use super::{
    EgoBrowserBackend, EgoBrowserConfig, TOOL_NAMES, browser_contract,
    connection::{self, BrowserConfig},
    failure,
    opencli::{self, OpenCliBrowserBackend, OpenCliConfig},
    state::{BrowserState, StateLease, io_error, read_json},
    workspace_key,
};
use crate::{ExternalToolBackend, ExternalToolOutput, ToolCapabilities, ToolError, ToolRequest};
use async_trait::async_trait;
use golutra_agent_core::{SideEffectType, ToolContract};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Preference {
    Auto,
    Ego,
    Chrome,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Route {
    version: u32,
    workspace: PathBuf,
    session_id: String,
    browser_backend: Option<Preference>,
    active: Option<BrowserConfig>,
}

#[derive(Debug)]
pub struct BrowserRouter {
    home: PathBuf,
    workspace: PathBuf,
    state_root: PathBuf,
    route_root: PathBuf,
    unrestricted: bool,
    discover: fn() -> BrowserConfig,
}

impl BrowserRouter {
    pub fn new(
        home: &Path,
        workspace: &Path,
        state_root: &Path,
        unrestricted: bool,
    ) -> Result<Self, ToolError> {
        let workspace = workspace.canonicalize().map_err(io_error)?;
        Ok(Self {
            home: home.into(),
            route_root: state_root.join("routing").join(workspace_key(&workspace)),
            workspace,
            state_root: state_root.into(),
            unrestricted,
            discover: BrowserConfig::automatic,
        })
    }

    fn resolve(&self, preference: Preference, host: Option<&BrowserConfig>) -> BrowserConfig {
        match preference {
            Preference::Auto => (self.discover)(),
            Preference::Chrome => match host {
                Some(BrowserConfig::Chrome(config)) => BrowserConfig::Chrome(config.clone()),
                _ => BrowserConfig::Chrome(OpenCliConfig::default()),
            },
            Preference::Ego => match host {
                Some(BrowserConfig::Ego(config)) => BrowserConfig::Ego(config.clone()),
                _ => connection::discover_ego()
                    .map(|executable| {
                        BrowserConfig::Ego(EgoBrowserConfig {
                            executable,
                            sdk_path: None,
                        })
                    })
                    .unwrap_or_else(|| BrowserConfig::Auto(Default::default())),
            },
        }
    }

    fn backend_state(
        &self,
        config: &BrowserConfig,
        request: &ToolRequest,
    ) -> Result<Option<BrowserState>, ToolError> {
        let root = if matches!(config, BrowserConfig::Chrome(_)) {
            self.state_root.join("opencli")
        } else {
            self.state_root.clone()
        };
        let path = root
            .join(workspace_key(&self.workspace))
            .join(request.session_id.to_string())
            .join("state.json");
        if !path.exists() {
            return Ok(None);
        }
        let state: BrowserState = read_json(&path)?;
        if state.version != 1
            || state.workspace != self.workspace
            || state.session_id != request.session_id.to_string()
        {
            return Err(ToolError::Execution(
                "browser_state_mismatch: refusing another workspace/session binding".into(),
            ));
        }
        Ok(Some(state))
    }

    fn load_route(
        &self,
        lease: &StateLease,
        request: &ToolRequest,
        host: Option<&BrowserConfig>,
    ) -> Result<Route, ToolError> {
        if lease.path.exists() {
            let route: Route = read_json(&lease.path)?;
            if route.version != 1
                || route.workspace != self.workspace
                || route.session_id != request.session_id.to_string()
                || route
                    .active
                    .as_ref()
                    .is_some_and(|c| !matches!(c, BrowserConfig::Ego(_) | BrowserConfig::Chrome(_)))
            {
                return Err(ToolError::Execution(
                    "browser_route_mismatch: refusing another session or invalid binding".into(),
                ));
            }
            return Ok(route);
        }
        // 升级后沿用旧会话页面；自动策略不能把未结束的 Chrome 任务迁到 Ego。
        let mut active = None;
        for config in [
            BrowserConfig::Ego(EgoBrowserConfig {
                executable: self.workspace.join("unused"),
                sdk_path: None,
            }),
            BrowserConfig::Chrome(OpenCliConfig::default()),
        ] {
            if let Some(state) = self.backend_state(&config, request)?
                && live_binding(&state)
            {
                if active.is_some() {
                    return Err(ToolError::Execution("browser_binding_ambiguous: both legacy backends have retained tasks; resolve them before switching".into()));
                }
                active = Some(match config {
                    BrowserConfig::Ego(_) => BrowserConfig::Ego(match host {
                        Some(BrowserConfig::Ego(c)) if c.executable == state.executable => {
                            c.clone()
                        }
                        _ => EgoBrowserConfig {
                            executable: state.executable,
                            sdk_path: None,
                        },
                    }),
                    _ => {
                        let binding = self
                            .state_root
                            .join("opencli")
                            .join(workspace_key(&self.workspace))
                            .join(request.session_id.to_string())
                            .join("profile.json");
                        let profile = if binding.exists() {
                            read_json::<Value>(&binding)?["requested_profile"]
                                .as_str()
                                .map(str::to_owned)
                        } else {
                            None
                        };
                        BrowserConfig::Chrome(OpenCliConfig {
                            executable: Some(state.executable),
                            profile,
                            ..Default::default()
                        })
                    }
                });
            }
        }
        Ok(Route {
            version: 1,
            workspace: self.workspace.clone(),
            session_id: request.session_id.to_string(),
            browser_backend: None,
            active,
        })
    }

    async fn execute(
        &self,
        request: &ToolRequest,
        cancel: CancellationToken,
    ) -> Result<ExternalToolOutput, ToolError> {
        if !super::is_browser_tool(&request.tool_name) {
            return Err(ToolError::UnknownTool(request.tool_name.clone()));
        }
        if cancel.is_cancelled() {
            return Ok(failure(
                "browser_cancelled",
                "Cancelled before browser selection or dispatch",
            ));
        }
        let host = BrowserConfig::load(&self.home)?;
        if matches!(host, Some(BrowserConfig::Disabled(_))) {
            let mut output = failure(
                "browser_disabled",
                "Browser tools are disabled. Only the user may reconnect using /connect auto, ego or chrome.",
            );
            output.structured_facts["browser_backend"] = json!("disabled");
            output.structured_facts["resolved_backend"] = Value::Null;
            return Ok(output);
        }
        // 同一会话的配置、前台调用和后台调用共用文件锁，不能中途换路由。
        let lease = StateLease::acquire(&self.route_root, request)?;
        let mut route = self.load_route(&lease, request, host.as_ref())?;
        if let Some(config) = &route.active
            && self
                .backend_state(config, request)?
                .is_none_or(|s| !live_binding(&s))
        {
            route.active = None;
            save_route(&lease, &route)?;
        }
        if request.tool_name == "browser_config" {
            validate(&config_contract(), &request.arguments)?;
            if let Some(value) = request.arguments.get("browser_backend") {
                route.browser_backend = Some(
                    serde_json::from_value(value.clone())
                        .map_err(|e| ToolError::InvalidArguments(e.to_string()))?,
                );
                // 已确认结束的 Ego 结果页可以保留；只有新的明确选择才放弃自动续接它们。
                if let Some(config) = &route.active
                    && self
                        .backend_state(config, request)?
                        .is_some_and(|s| s.phase == "finished" && !s.requires_reload)
                {
                    route.active = None;
                }
                save_route(&lease, &route)?;
            }
        }
        let preference = route.browser_backend.unwrap_or(match host {
            Some(BrowserConfig::Chrome(_)) => Preference::Chrome,
            Some(BrowserConfig::Ego(_)) => Preference::Ego,
            _ => Preference::Auto,
        });
        let desired = self.resolve(preference, host.as_ref());
        let config = route.active.clone().unwrap_or_else(|| desired.clone());
        if request.tool_name == "browser_config" {
            return Ok(with_selection(
                ExternalToolOutput {
                    summary: "Browser selection".into(),
                    content: String::new(),
                    structured_facts: json!({}),
                    is_error: false,
                    images: vec![],
                },
                &route,
                preference,
                &config,
                &desired,
            ));
        }
        let backend: Box<dyn ExternalToolBackend> = match &config {
            BrowserConfig::Chrome(config) => Box::new(OpenCliBrowserBackend::new(
                config.clone(),
                &self.workspace,
                &self.state_root,
                self.unrestricted,
            )?),
            BrowserConfig::Ego(config) => Box::new(EgoBrowserBackend::new(
                config.clone(),
                &self.workspace,
                &self.state_root,
                self.unrestricted,
            )?),
            _ => {
                let mut status = connection::setup_status();
                if matches!(preference, Preference::Ego) && status["status"] == "install_required" {
                    status["message"] = json!(
                        "未找到 Ego 原生启动器。请先询问用户是否安装 Ego；同意后运行 golutra-agent browser install-ego --yes，再检查 browser_status。不会自动切换到 Chrome。"
                    );
                }
                return Ok(with_selection(
                    ExternalToolOutput {
                        summary: "Browser setup required".into(),
                        content: status.to_string(),
                        structured_facts: status,
                        is_error: !matches!(
                            request.tool_name.as_str(),
                            "browser_status" | "browser_help"
                        ),
                        images: vec![],
                    },
                    &route,
                    preference,
                    &config,
                    &desired,
                ));
            }
        };
        // 联合合同只负责稳定注册；派发前再次按实际后端校验，不能默默忽略另一后端的参数。
        let contract = backend
            .contracts()
            .into_iter()
            .find(|c| c.tool_name == request.tool_name)
            .ok_or_else(|| ToolError::UnknownTool(request.tool_name.clone()))?;
        if let Err(error) = validate(&contract, &request.arguments) {
            return Ok(with_selection(
                failure(
                    "browser_arguments_mismatch",
                    &format!("{error}; read browser_help for the resolved backend"),
                ),
                &route,
                preference,
                &config,
                &desired,
            ));
        }
        let was_bound = route.active.is_some();
        if !matches!(
            request.tool_name.as_str(),
            "browser_status" | "browser_help"
        ) {
            let journal = lease.directory.join(request.tool_call_id.to_string());
            match std::fs::create_dir(&journal) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Ok(with_selection(
                        failure(
                            "browser_duplicate_call",
                            "Invocation already recorded; switching browsers must not replay it",
                        ),
                        &route,
                        preference,
                        &config,
                        &desired,
                    ));
                }
                Err(error) => return Err(io_error(error)),
            }
        }
        if request.tool_name == "browser_run" {
            route.active = Some(config.clone());
            save_route(&lease, &route)?;
        }
        let result = backend.call(request, cancel).await;
        let released = request.tool_name == "browser_finish"
            && (request.arguments["release"] == true
                || request.arguments["keep"]
                    .as_array()
                    .is_some_and(Vec::is_empty))
            && result.as_ref().is_ok_and(|output| !output.is_error);
        // 未派发的设置失败可重新选择；未知结果、接管和保留结果仍固定原后端。
        if released
            || (!was_bound
                && route.active.is_some()
                && self
                    .backend_state(&config, request)?
                    .is_none_or(|s| !live_binding(&s)))
        {
            route.active = None;
            save_route(&lease, &route)?;
        } else if was_bound && !lease.path.exists() {
            save_route(&lease, &route)?;
        }
        let mut output =
            result.unwrap_or_else(|error| failure("browser_backend_error", &error.to_string()));
        if output.is_error
            && let Some(state) = self.backend_state(&config, request)?
            && (state.requires_reload
                || matches!(state.phase.as_str(), "running" | "outcome_unknown"))
        {
            output.structured_facts["code"] = json!("browser_outcome_unknown");
            output.structured_facts["phase"] = json!("outcome_unknown");
            output.structured_facts["outcome_unknown"] = json!(true);
        }
        Ok(with_selection(
            output, &route, preference, &config, &desired,
        ))
    }
}

fn live_binding(state: &BrowserState) -> bool {
    state.requires_reload
        || state.reclaimable
        || !matches!(state.phase.as_str(), "idle" | "finished")
}

fn save_route(lease: &StateLease, route: &Route) -> Result<(), ToolError> {
    let mut file = tempfile::NamedTempFile::new_in(&lease.directory).map_err(io_error)?;
    serde_json::to_writer(&mut file, route).map_err(|e| ToolError::Execution(e.to_string()))?;
    file.flush().map_err(io_error)?;
    file.as_file().sync_all().map_err(io_error)?;
    file.persist(&lease.path).map_err(|e| io_error(e.error))?;
    Ok(())
}

fn resolved(config: &BrowserConfig) -> Option<&'static str> {
    matches!(config, BrowserConfig::Ego(_) | BrowserConfig::Chrome(_))
        .then(|| config.backend_name())
}

fn with_selection(
    mut output: ExternalToolOutput,
    route: &Route,
    preference: Preference,
    config: &BrowserConfig,
    desired: &BrowserConfig,
) -> ExternalToolOutput {
    let selection = json!({
        "browser_backend":preference, "resolved_backend":resolved(config),
        "active_backend":route.active.as_ref().and_then(resolved), "next_backend":resolved(desired),
        "switch_pending":route.active.as_ref().is_some_and(|c| resolved(c) != resolved(desired)),
        "selection_scope":if route.browser_backend.is_some() {"session"} else {"host_default"},
        "selection_guidance":"Agent may set browser_config({browser_backend:'auto'|'ego'|'chrome'}). Active/unknown/handoff tasks stay bound; finish with release:true before switching. Auto prefers installed Ego, then Chrome. Installation requires user consent. Read browser_help for the resolved backend.",
        "extension_guidance":connection::EXTENSION_GUIDANCE,
    });
    if !output.structured_facts.is_object() {
        output.structured_facts = json!({"result":output.structured_facts});
    }
    output
        .structured_facts
        .as_object_mut()
        .unwrap()
        .extend(selection.as_object().unwrap().clone());
    output
        .content
        .push_str(&format!("\nBrowser selection: {selection}"));
    output
}

fn config_contract() -> ToolContract {
    let mut contract = browser_contract("browser_config");
    contract.side_effect_type = SideEffectType::File;
    contract.input_schema = json!({"type":"object","description":"Read or set this session's browser_backend. auto prefers installed Ego, then Chrome. ego/chrome are explicit choices. Returns resolved_backend, active_backend and switch_pending. Does not launch/install a browser or change global settings. Changes take effect immediately before a task; active/unknown/handoff tasks remain bound until explicit finish release:true. Never use selection to bypass user Stop. If both browsers are missing ask for permission to install Ego, then run golutra-agent browser install-ego --yes only after consent. Recheck status after setup in the same turn.","properties":{"browser_backend":{"type":"string","enum":["auto","ego","chrome"]}},"required":[],"additionalProperties":false});
    contract
}

fn validate(contract: &ToolContract, args: &Value) -> Result<(), ToolError> {
    let validator = jsonschema::validator_for(&contract.input_schema)
        .map_err(|e| ToolError::Execution(e.to_string()))?;
    validator
        .validate(args)
        .map_err(|e| ToolError::InvalidArguments(e.to_string()))
}

#[async_trait]
impl ExternalToolBackend for BrowserRouter {
    fn contracts(&self) -> Vec<ToolContract> {
        let mut contracts = opencli::contracts();
        for contract in &mut contracts {
            let ego = browser_contract(&contract.tool_name);
            let properties = contract.input_schema["properties"].as_object_mut().unwrap();
            for (name, schema) in ego.input_schema["properties"].as_object().unwrap() {
                properties
                    .entry(name.clone())
                    .or_insert_with(|| schema.clone());
            }
            contract.input_schema["required"] = json!([]);
            let description = format!(
                "Use browser_config/browser_status to see browser_backend and resolved_backend. Auto prefers Ego, then Chrome; Agent can change the session preference via browser_config. Read browser_help after selecting. Ego: {} Chrome: {}",
                ego.input_schema["description"].as_str().unwrap_or(""),
                contract.input_schema["description"].as_str().unwrap_or("")
            );
            contract.input_schema["description"] = json!(description);
        }
        contracts.push(config_contract());
        contracts
    }
    fn capabilities(&self) -> HashMap<String, ToolCapabilities> {
        TOOL_NAMES
            .iter()
            .copied()
            .chain(["browser_config"])
            .map(|name| {
                (
                    name.into(),
                    ToolCapabilities {
                        available_in_coding_profile: true,
                        parallel_read_safe: false,
                        coding_profile_hidden_arguments: vec![],
                    },
                )
            })
            .collect()
    }
    fn timeout(&self, _: &ToolRequest) -> Duration {
        Duration::from_secs(125)
    }
    async fn call(
        &self,
        request: &ToolRequest,
        cancel: CancellationToken,
    ) -> Result<ExternalToolOutput, ToolError> {
        self.execute(request, cancel).await
    }
}

#[cfg(test)]
mod tests;
