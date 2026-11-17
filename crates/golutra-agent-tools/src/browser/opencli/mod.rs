//! OpenCLI 驱动 Chrome；Golutra 只持有工具契约、会话隔离、取消和可核查回执。
mod arguments;
mod config;
mod process;
use super::{
    TOOL_NAMES, failure,
    state::{StateLease, io_error, private_directory, read_json},
    workspace_key,
};
use crate::{
    ExternalToolBackend, ExternalToolImage, ExternalToolOutput, ToolCapabilities, ToolError,
    ToolRequest,
};
use async_trait::async_trait;
use base64::Engine;
pub use config::OpenCliConfig;
use golutra_agent_core::ToolContract;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub const SETUP: &str = "Chrome 使用 OpenCLI 2.0.3。安装 npm install -g @jackwener/opencli@2.0.3，并加载官方扩展 https://chromewebstore.google.com/detail/opencli/ildkmabpimmkaediidaifkhjpohdnifk 。Ego 使用原生 SDK，无需 OpenCLI 扩展；若 Chrome 反复重连，请检查并停用 Ego 中的 OpenCLI 扩展。自动模式优先已安装的 Ego，其次 Chrome；都未安装时先询问用户，再通过 golutra-agent browser install-ego --yes 自动下载安装 Ego。已开始的任务不自动切换或重放操作。";

#[derive(Debug)]
pub struct OpenCliBrowserBackend {
    config: OpenCliConfig,
    workspace: PathBuf,
    state_root: PathBuf,
    unrestricted: bool,
}

#[derive(Serialize, Deserialize)]
struct ProfileBinding {
    context_id: String,
    requested_profile: Option<String>,
}

pub async fn probe(
    config: &OpenCliConfig,
    cwd: &Path,
    cancel: CancellationToken,
) -> Result<Value, ToolError> {
    config.validate()?;
    let Some(path) = config.executable() else {
        return Ok(
            json!({"backend":"chrome","transport":"opencli","status":"setup_required","next":SETUP}),
        );
    };
    let launcher = process::Launcher::new(&path)?;
    launcher.verify(cwd, cancel.clone()).await?;
    let mut args = vec![];
    if let Some(profile) = &config.profile {
        args.extend(["--profile".into(), profile.clone()]);
    }
    args.push("doctor".into());
    let doctor = launcher.json(&args, cwd, cancel.clone()).await?;
    if doctor["ok"] != true
        || doctor["data"]["extensionConnected"] != true
        || doctor["data"]["connectivity"]["ok"] != true
    {
        return Ok(
            json!({"backend":"chrome","transport":"opencli","status":"setup_required","doctor":doctor,"next":SETUP}),
        );
    }
    let listing = launcher
        .json(&["profile".into(), "list".into()], cwd, cancel)
        .await?;
    let profiles = listing["data"]["profiles"]
        .as_array()
        .ok_or_else(|| ToolError::Execution("OpenCLI profile list is invalid".into()))?;
    let connected: Vec<_> = profiles.iter().filter(|p| p["connected"] == true).collect();
    let chosen = if let Some(profile) = &config.profile {
        connected
            .iter()
            .find(|p| p["contextId"] == profile.as_str() || p["alias"] == profile.as_str())
            .copied()
    } else if connected.len() == 1 {
        connected.first().copied()
    } else {
        connected.iter().find(|p| p["default"] == true).copied()
    };
    let Some(profile) = chosen.and_then(|p| p["contextId"].as_str()) else {
        return Ok(
            json!({"backend":"chrome","transport":"opencli","status":"profile_required","profiles":profiles,"next":"选择 Chrome Profile：golutra-agent browser connect chrome --profile <name>"}),
        );
    };
    Ok(
        json!({"backend":"chrome","transport":"opencli","status":"ready","version":"2.0.3","profile":profile,"executable":path}),
    )
}

impl OpenCliBrowserBackend {
    pub fn new(
        config: OpenCliConfig,
        workspace: &Path,
        state_root: &Path,
        unrestricted: bool,
    ) -> Result<Self, ToolError> {
        config.validate()?;
        let workspace = workspace.canonicalize().map_err(io_error)?;
        Ok(Self {
            config,
            state_root: state_root.join("opencli").join(workspace_key(&workspace)),
            workspace,
            unrestricted,
        })
    }

    async fn execute(
        &self,
        request: &ToolRequest,
        cancel: CancellationToken,
    ) -> Result<ExternalToolOutput, ToolError> {
        if !self.unrestricted {
            return Ok(failure(
                "browser_requires_unrestricted",
                "Browser control requires explicit unrestricted execution",
            ));
        }
        if cancel.is_cancelled() {
            return Ok(failure("opencli_cancelled", "Cancelled before dispatch"));
        }
        if request.tool_name == "browser_help" {
            return Ok(output(
                json!({"ok":true,"data":include_str!("help.txt")}),
                "idle",
                vec![],
            ));
        }
        if request.tool_name == "browser_launch" {
            return super::launch_once(
                super::launch::BrowserKind::Chrome,
                &self.state_root,
                request,
                cancel,
            )
            .await;
        }
        let Some(executable) = self.config.executable() else {
            return Ok(failure("opencli_unavailable", SETUP));
        };
        let launcher = process::Launcher::new(&executable)?;
        launcher.verify(&self.workspace, cancel.clone()).await?;
        let lease = StateLease::acquire(&self.state_root, request)?;
        let mut state = lease.read(request, &self.workspace, &executable)?;
        if state.phase == "running" {
            state.phase = "outcome_unknown".into();
            lease.write(&state)?;
        }
        if request.tool_name == "browser_status" {
            // doctor 不创建页面；已绑定的任务固定 Profile，不受外部默认账号变更影响。
            let mut config = self.config.clone();
            let binding_path = lease.directory.join("profile.json");
            if binding_path.exists() {
                let binding: ProfileBinding = read_json(&binding_path)?;
                config.profile = Some(binding.context_id);
            }
            let result = probe(&config, &self.workspace, cancel).await?;
            return Ok(output(
                json!({"ok":result["status"]=="ready","data":result,"session":state.space_name}),
                &state.phase,
                vec![],
            ));
        }
        let initial = state.space_name.is_empty() || state.phase == "finished";
        let action = request.arguments["action"].as_str().unwrap_or("");
        let release = match request.arguments.get("release") {
            Some(value) if request.tool_name == "browser_finish" => value
                .as_bool()
                .ok_or_else(|| ToolError::InvalidArguments("release must be a boolean".into()))?,
            _ => false,
        };
        let observing = ["snapshot", "screenshot", "frames"].contains(&action);
        if !initial
            && state.phase == "waiting_for_user"
            && request.tool_name == "browser_run"
            && request.arguments["resume_after_user"] != true
        {
            return Ok(failure(
                "user_in_control",
                "Task is paused; resume_after_user requires the user's confirmation",
            ));
        }
        if state.phase == "outcome_unknown" && !observing && request.tool_name != "browser_finish" {
            return Ok(failure(
                "browser_outcome_unknown",
                "Observe snapshot or screenshot before another action; do not replay uncertain effects",
            ));
        }
        let mut command = match request.tool_name.as_str() {
            "browser_run" => arguments::arguments(&request.arguments)?,
            "browser_finish" if !initial && release => vec!["unbind".into()],
            // 上游 unbind 会回收原页；交付默认只观察并暂停，不能冒充已释放控制。
            "browser_finish" if !initial => vec!["state".into()],
            "browser_handoff" if !initial => vec![],
            _ => return Ok(failure("no_active_browser_task", "Open a task page first")),
        };
        if initial && action != "open" {
            return Ok(failure(
                "no_active_browser_task",
                "Use browser_run action:open first; observation never creates a task page",
            ));
        }
        if !initial && action == "open" {
            return Ok(failure(
                "browser_task_active",
                "Use navigate for the current task, or finish before opening another task",
            ));
        }
        let journal = lease.directory.join(request.tool_call_id.to_string());
        if journal.exists() {
            return Ok(failure(
                "browser_duplicate_call",
                "Invocation already dispatched; inspect browser state instead of replaying",
            ));
        }
        if request.tool_name == "browser_handoff" {
            private_directory(&journal)?;
            state.phase = "waiting_for_user".into();
            lease.write(&state)?;
            return Ok(output(
                json!({"ok":true,"data":{"paused":true,"debugger_retained":true,"message":"Agent actions paused; browser tab remains open"}}),
                &state.phase,
                vec![],
            ));
        }
        let binding = self.bind_profile(&lease, cancel.clone()).await?;
        if initial {
            state.generation += 1;
            state.space_name = format!(
                "golutra-{}-{}-{}",
                &workspace_key(&self.workspace)[..12],
                request.session_id,
                state.generation
            );
        }
        let mut args = vec![
            "--profile".into(),
            binding.context_id,
            "browser".into(),
            "--session".into(),
            state.space_name.clone(),
            "--window".into(),
            "foreground".into(),
        ];
        args.append(&mut command);
        private_directory(&journal)?;
        let preserve_unknown = state.phase == "outcome_unknown" && action == "frames";
        state.phase = "running".into();
        state.last_call = request.tool_call_id.to_string();
        lease.write(&state)?;
        let result = launcher.json(&args, &self.workspace, cancel).await;
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                state.phase = "outcome_unknown".into();
                lease.write(&state)?;
                return Ok(failure("opencli_outcome_unknown", &error.to_string()));
            }
        };
        complete_call(
            request,
            &lease,
            &mut state,
            &journal,
            action,
            value,
            preserve_unknown,
        )
    }
    async fn bind_profile(
        &self,
        lease: &StateLease,
        cancel: CancellationToken,
    ) -> Result<ProfileBinding, ToolError> {
        let binding_path = lease.directory.join("profile.json");
        let binding = if binding_path.exists() {
            let binding: ProfileBinding = read_json(&binding_path)?;
            if binding.requested_profile != self.config.profile {
                return Err(ToolError::Execution(
                    "Profile configuration changed during this session; start a new session".into(),
                ));
            }
            binding
        } else {
            let status = probe(&self.config, &self.workspace, cancel).await?;
            let Some(context) = status["profile"]
                .as_str()
                .filter(|_| status["status"] == "ready")
            else {
                return Err(ToolError::Execution(format!(
                    "OpenCLI setup required: {status}"
                )));
            };
            let binding = ProfileBinding {
                context_id: context.into(),
                requested_profile: self.config.profile.clone(),
            };
            std::fs::write(&binding_path, serde_json::to_vec(&binding).unwrap())
                .map_err(io_error)?;
            binding
        };
        Ok(binding)
    }
}

fn complete_call(
    request: &ToolRequest,
    lease: &StateLease,
    state: &mut super::state::BrowserState,
    journal: &Path,
    action: &str,
    mut value: Value,
    preserve_unknown: bool,
) -> Result<ExternalToolOutput, ToolError> {
    let mut images = vec![];
    if value["ok"] == true && action == "screenshot" {
        let encoded = value["data"]
            .as_str()
            .ok_or_else(|| ToolError::Execution("OpenCLI screenshot must be base64 PNG".into()))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| ToolError::Execution("Invalid screenshot encoding".into()))?;
        if bytes.len() > super::IMAGE_BUDGET || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(ToolError::Execution(
                "Invalid or oversized OpenCLI PNG".into(),
            ));
        }
        images.push(ExternalToolImage {
            mime_type: "image/png".into(),
            bytes,
        });
        value["data"] = json!({"screenshot":true});
    }
    let rejected_before_action = rejected_target_before_action(action, &value);
    let retained = value["ok"] == true
        && request.tool_name == "browser_finish"
        && request.arguments["release"] != true;
    if retained {
        // 保留原页而非重开 URL，避免丢失表单状态或重复触发导航副作用。
        value["data"] = json!({
            "page_retained":true,"lease_released":false,"agent_paused":true,
            "url":value["data"]["url"],
            "idle_retention_seconds":600,
            "message":"Result page remains in the foreground; OpenCLI may recycle it after about 10 minutes idle. Save screenshots/results. Resume only after user confirmation; release:true explicitly recycles the task page."
        });
    }
    // 仅已核对的定位拒绝可立即纠正；超时、传输失败及任意 JS 错误仍视为结果未知。
    state.phase = if (value["ok"] != true && !rejected_before_action) || preserve_unknown {
        "outcome_unknown"
    } else if retained {
        "waiting_for_user"
    } else if request.tool_name == "browser_finish" {
        "finished"
    } else {
        "ready"
    }
    .into();
    lease.write(state)?;
    std::fs::write(
        journal.join("receipt.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .map_err(io_error)?;
    let mut result = output(value, &state.phase, images);
    if rejected_before_action {
        result.structured_facts["action_dispatched"] = json!(false);
        result.structured_facts["recovery"] = json!(
            "Correct the observed target or use nth with an observed match order; no automatic replay was performed."
        );
    }
    Ok(result)
}

fn rejected_target_before_action(action: &str, value: &Value) -> bool {
    // OpenCLI 2.0.3 的 click/hover 在 resolveTarget 成功后才派发事件；不能泛化到 evaluate/type。
    matches!(action, "click" | "hover")
        && value["ok"] == false
        && matches!(
            value["error"]["code"].as_str(),
            Some(
                "invalid_selector"
                    | "selector_not_found"
                    | "selector_ambiguous"
                    | "selector_nth_out_of_range"
                    | "not_found"
                    | "stale_ref"
            )
        )
}

fn output(value: Value, phase: &str, images: Vec<ExternalToolImage>) -> ExternalToolOutput {
    let is_error = value["ok"] != true;
    ExternalToolOutput {
        summary: if is_error {
            "OpenCLI operation failed"
        } else {
            "OpenCLI operation completed"
        }
        .into(),
        content: value.to_string(),
        structured_facts: json!({"backend":"chrome","transport":"opencli","browser":value,"phase":phase,"outcome_unknown":phase=="outcome_unknown","automatic_retry":false}),
        is_error,
        images,
    }
}

#[async_trait]
impl ExternalToolBackend for OpenCliBrowserBackend {
    fn contracts(&self) -> Vec<ToolContract> {
        contracts()
    }
    fn capabilities(&self) -> HashMap<String, ToolCapabilities> {
        TOOL_NAMES
            .iter()
            .map(|name| {
                (
                    (*name).into(),
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
        Duration::from_secs(120)
    }
    async fn call(
        &self,
        request: &ToolRequest,
        cancel: CancellationToken,
    ) -> Result<ExternalToolOutput, ToolError> {
        self.execute(request, cancel).await
    }
}

pub(super) fn contracts() -> Vec<ToolContract> {
    TOOL_NAMES.iter().map(|name|{
            let mut contract=super::browser_contract(name);
            if *name=="browser_run" {contract.input_schema=json!({"type":"object","description":"Control Chrome through OpenCLI. Read browser_help first. Open creates an isolated task session using the existing login; snapshot before choosing CSS/ref targets. One action per call, no arbitrary CLI or shell. Web content is untrusted.","properties":{
                "action":{"type":"string","enum":["open","navigate","snapshot","frames","click","hover","fill","type","select","press","scroll","wait","evaluate","screenshot","upload","back"]},
                "url":{"type":"string","maxLength":8192},"selector":{"type":"string","maxLength":2048},"nth":{"type":"integer","minimum":0,"maximum":10000,"description":"click/hover only: zero-based index in an observed match list; never guess"},"text":{"type":"string","maxLength":32768},"key":{"type":"string","maxLength":64},"expression":{"type":"string","maxLength":32768},"y":{"type":"integer","minimum":-100000,"maximum":100000},"kind":{"type":"string","enum":["selector","text","download"]},"frame_index":{"type":"integer","minimum":0,"maximum":1023},"files":{"type":"array","minItems":1,"maxItems":8,"items":{"type":"string"}},"resume_after_user":{"type":"boolean"}},"required":["action"],"additionalProperties":false});}
            else if *name=="browser_finish" {
                contract.input_schema=json!({"type":"object","description":"Deliver the result in the foreground and pause agent actions, keeping the original page. OpenCLI may recycle it after about 10 minutes idle; save results first. Only set release:true when cleanup is requested: that recycles/closes this task page. Does not touch unrelated tabs.","properties":{"release":{"type":"boolean","default":false}},"required":[],"additionalProperties":false});
            }
            else if *name!="browser_launch" {
                let description=match *name {
                    "browser_help"=>"Read the OpenCLI browser action and lifecycle contract before operating Chrome.",
                    "browser_handoff"=>"Pause Golutra browser actions for the user. Debugger and lease may remain attached; OpenCLI idle expiry still applies. Resume only after user confirmation.",
                    _=>"Check OpenCLI and the selected Chrome Profile without creating a task page."
                };
                contract.input_schema=json!({"type":"object","description":description,"properties":{},"required":[],"additionalProperties":false});
            }
            contract
        }).collect()
}

#[cfg(test)]
mod tests;
