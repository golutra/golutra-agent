//! Ego 原生启动器的薄适配层；浏览器能力是可选的，不引入第二套 Agent 循环。

use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use golutra_agent_core::{SideEffectType, ToolContract};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{
    ExternalToolBackend, ExternalToolImage, ExternalToolOutput, ProcessExecutionRequest,
    ToolCapabilities, ToolError, ToolRequest, checksum, run_process_with_progress,
};
use golutra_agent_sandbox::{SystemSandbox, WorkspaceAccess};

pub mod chrome;
mod config;
pub mod connection;
mod ego_arguments;
mod ego_guide;
mod ego_lifecycle;
pub mod launch;
pub mod opencli;
pub mod router;
mod state;
use super::process::ShellOutput;
pub use config::EgoBrowserConfig;
use state::{StateLease, io_error, private_directory, read_json};

const BRIDGE: &str = include_str!("bridge.js");
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const MAX_TIMEOUT_MS: u64 = 120_000;
const IMAGE_BUDGET: usize = 4 * 1024 * 1024;
const TOOL_NAMES: [&str; 6] = [
    "browser_launch",
    "browser_status",
    "browser_run",
    "browser_handoff",
    "browser_finish",
    "browser_help",
];

pub(super) fn is_browser_tool(name: &str) -> bool {
    TOOL_NAMES.contains(&name) || name == "browser_config"
}

#[derive(Debug)]
pub struct EgoBrowserBackend {
    config: EgoBrowserConfig,
    workspace: PathBuf,
    state_root: PathBuf,
    unrestricted: bool,
}

struct BrowserInvocation {
    lease: StateLease,
    receipt_path: PathBuf,
    artifact_directory: PathBuf,
    previous_phase: String,
    script: String,
}

impl EgoBrowserBackend {
    pub fn new(
        config: EgoBrowserConfig,
        workspace: &Path,
        state_root: &Path,
        unrestricted: bool,
    ) -> Result<Self, ToolError> {
        config.validate()?;
        let workspace = workspace.canonicalize().map_err(io_error)?;
        private_directory(state_root)?;
        let state_root = state_root
            .canonicalize()
            .map_err(io_error)?
            .join(workspace_key(&workspace));
        Ok(Self {
            config,
            workspace,
            state_root,
            unrestricted,
        })
    }

    async fn execute(
        &self,
        request: &ToolRequest,
        cancellation: CancellationToken,
    ) -> Result<ExternalToolOutput, ToolError> {
        if !self.unrestricted {
            return Ok(failure(
                "browser_requires_unrestricted",
                "Ego runs Node code and controls an external browser. Enable unrestricted execution explicitly to use this integration.",
            ));
        }
        if cancellation.is_cancelled() {
            return Ok(failure(
                "ego_cancelled",
                "Cancelled before browser dispatch",
            ));
        }
        if request.tool_name == "browser_launch" {
            return launch_once(
                launch::BrowserKind::Ego,
                &self.state_root,
                request,
                cancellation,
            )
            .await;
        }
        if !self.config.executable.is_file() {
            return Ok(failure(
                "browser_unavailable",
                "Configured Ego launcher does not exist. Check browser.json executable; the SDK alone cannot start a browser.",
            ));
        }
        let action = request
            .tool_name
            .strip_prefix("browser_")
            .filter(|action| ["status", "run", "handoff", "finish", "help"].contains(action))
            .ok_or_else(|| ToolError::UnknownTool(request.tool_name.clone()))?;
        ego_arguments::validate(action, &request.arguments)?;
        let lease = StateLease::acquire(&self.state_root, request)?;
        let mut state = lease.read(request, &self.workspace, &self.config.executable)?;
        if !["status", "help"].contains(&action)
            && state.last_call == request.tool_call_id.to_string()
        {
            return Ok(failure(
                "browser_duplicate_call",
                "This invocation was already dispatched. Inspect browser_status instead of replaying its side effects.",
            ));
        }
        if let Some(output) = ego_lifecycle::prepare(&mut state, action, &request.arguments) {
            return Ok(output);
        }
        if action == "run" && state.space_id.is_none() {
            state.space_name = format!(
                "golutra-{}-{}-{}",
                &workspace_key(&self.workspace)[..12],
                request.session_id,
                state.generation
            );
        }
        // ID 在网页动作之前落盘；无 ID 的失败仅允许按同一个名称重连/初始化。
        let may_create = action == "run" && state.space_id.is_none();
        let call_directory = lease.directory.join(request.tool_call_id.to_string());
        if !["status", "help"].contains(&action) && call_directory.exists() {
            return Ok(failure(
                "browser_duplicate_call",
                "This invocation already has a browser journal; inspect the current state instead of replaying it.",
            ));
        }
        let invocation = self.prepare_invocation(request, action, lease, state, may_create)?;
        let process = self
            .launch(request, action, &invocation.script, cancellation)
            .await?;
        if action == "help" {
            return Ok(ego_guide::output(request, process));
        }
        self.read_output(request, invocation, process)
    }

    fn prepare_invocation(
        &self,
        request: &ToolRequest,
        action: &str,
        lease: StateLease,
        mut state: state::BrowserState,
        may_create: bool,
    ) -> Result<BrowserInvocation, ToolError> {
        let call_directory = lease.directory.join(request.tool_call_id.to_string());
        private_directory(&call_directory)?;
        let receipt_path = call_directory.join("receipt.json");
        let artifact_directory = call_directory.join("artifacts");
        private_directory(&artifact_directory)?;
        let previous_phase = state.phase.clone();
        if !["status", "help"].contains(&action) {
            state.phase = "running".to_owned();
            state.last_call = request.tool_call_id.to_string();
            lease.write(&state)?;
        }
        let script_config = json!({
            "state": state, "state_path": lease.path, "receipt_path": receipt_path,
            "artifact_directory": artifact_directory, "action": action, "may_create": may_create,
            "page": request.arguments.get("page").and_then(Value::as_str),
            "code": ego_arguments::code(&request.arguments),
            "observation": request.arguments.get("observation"),
            "recovery": request.arguments.get("recovery"),
            "keep": ego_arguments::keep(&request.arguments),
            "resume_after_user": request.arguments.get("resume_after_user").and_then(Value::as_bool).unwrap_or(false),
        });
        let script = BRIDGE.replace("__GOLUTRA_BROWSER_CONFIG__", &script_config.to_string());
        Ok(BrowserInvocation {
            lease,
            receipt_path,
            artifact_directory,
            previous_phase,
            script,
        })
    }

    async fn launch(
        &self,
        request: &ToolRequest,
        action: &str,
        script: &str,
        cancellation: CancellationToken,
    ) -> Result<ShellOutput, ToolError> {
        let args = self
            .config
            .args(if action == "help" { "skill" } else { "nodejs" });
        run_process_with_progress(
            ProcessExecutionRequest {
                program: self.config.executable.to_str().ok_or_else(|| {
                    ToolError::InvalidArguments("Ego launcher path must be UTF-8".to_owned())
                })?,
                args: &args,
                cwd: &self.workspace,
                workspace_root: &self.workspace,
                timeout_ms: requested_timeout(request),
                cancellation,
                sandbox: &SystemSandbox::process_only(),
                workspace_access: WorkspaceAccess::ReadWrite,
                allow_network: true,
                stdin: (action != "help").then_some(script.as_bytes()),
                isolated_home: false,
            },
            None,
        )
        .await
    }

    fn read_output(
        &self,
        request: &ToolRequest,
        invocation: BrowserInvocation,
        process: ShellOutput,
    ) -> Result<ExternalToolOutput, ToolError> {
        let BrowserInvocation {
            lease,
            receipt_path,
            artifact_directory,
            previous_phase,
            ..
        } = invocation;
        let mut state = lease.read(request, &self.workspace, &self.config.executable)?;
        let receipt: Value = if receipt_path.is_file() {
            read_json(&receipt_path)?
        } else {
            json!({})
        };
        let complete = process.exit_code == Some(0)
            && !process.timed_out
            && !process.cancelled
            && receipt.get("ok").and_then(Value::as_bool) == Some(true);
        if state.phase == "running" {
            // 启动器退出不证明页面 JS 已停止；缺回执的中断必须跨进程保留更强恢复条件。
            state.phase = "outcome_unknown".into();
            state.requires_reload = true;
            lease.write(&state)?;
        }
        let images = read_images(&receipt, &artifact_directory)?;
        let unknown = state.phase == "running" || state.phase == "outcome_unknown";
        Ok(ExternalToolOutput {
            summary: if complete
                && receipt["task_ended"] == true
                && receipt["page_retained"] == true
            {
                "Ego task ended; result pages retained for the user"
            } else if complete {
                "Browser operation completed"
            } else {
                "Browser operation did not complete; inspect current state before retrying"
            }
            .to_owned(),
            content: if process.raw_output.trim().is_empty() {
                receipt.to_string()
            } else {
                process.raw_output
            },
            structured_facts: json!({"backend":"ego", "browser": receipt, "space_id": state.space_id, "space_name": state.space_name,
                "phase": &state.phase, "requires_reload":state.requires_reload,
                "reclaimable":state.reclaimable,"default_page":state.default_page,
                "previous_phase": previous_phase, "outcome_unknown": unknown,
                "exit_code": process.exit_code, "timed_out": process.timed_out, "cancelled": process.cancelled,
                "output_truncated": process.output_truncated, "artifact_directory": artifact_directory,
                "automatic_retry": false}),
            is_error: !complete,
            images,
        })
    }
}

#[async_trait]
impl ExternalToolBackend for EgoBrowserBackend {
    fn contracts(&self) -> Vec<ToolContract> {
        TOOL_NAMES
            .iter()
            .map(|name| browser_contract(name))
            .collect()
    }

    fn capabilities(&self) -> HashMap<String, ToolCapabilities> {
        TOOL_NAMES
            .iter()
            .map(|name| {
                (
                    (*name).to_owned(),
                    ToolCapabilities {
                        available_in_coding_profile: true,
                        parallel_read_safe: false,
                        coding_profile_hidden_arguments: Vec::new(),
                    },
                )
            })
            .collect()
    }

    fn timeout(&self, request: &ToolRequest) -> Duration {
        Duration::from_millis(requested_timeout(request) + 5_000)
    }

    async fn call(
        &self,
        request: &ToolRequest,
        cancellation: CancellationToken,
    ) -> Result<ExternalToolOutput, ToolError> {
        self.execute(request, cancellation).await
    }
}

fn requested_timeout(request: &ToolRequest) -> u64 {
    request
        .arguments
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(1, MAX_TIMEOUT_MS)
}

fn workspace_key(workspace: &Path) -> String {
    checksum(workspace.to_string_lossy().as_bytes())
        .trim_start_matches("sha256:")
        .to_owned()
}

fn failure(code: &str, message: &str) -> ExternalToolOutput {
    ExternalToolOutput {
        summary: message.to_owned(),
        content: String::new(),
        structured_facts: json!({"code": code, "automatic_retry": false}),
        is_error: true,
        images: Vec::new(),
    }
}

async fn launch_once(
    kind: launch::BrowserKind,
    root: &Path,
    request: &ToolRequest,
    cancel: CancellationToken,
) -> Result<ExternalToolOutput, ToolError> {
    if cancel.is_cancelled() {
        return Ok(failure(
            "browser_cancelled",
            "Browser launch cancelled before dispatch.",
        ));
    }
    let lease = StateLease::acquire(root, request)?;
    let journal = lease
        .directory
        .join(format!("launch-{}", request.tool_call_id));
    if journal.exists() {
        return Ok(failure(
            "browser_duplicate_call",
            "Launch already requested. Inspect the browser instead of replaying.",
        ));
    }
    private_directory(&journal)?;
    launch::launch(kind, None, cancel).await
}

fn read_images(receipt: &Value, directory: &Path) -> Result<Vec<ExternalToolImage>, ToolError> {
    let Some(paths) = receipt.get("images").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    if paths.len() > 2 {
        return Err(ToolError::Execution(
            "too many browser screenshots".to_owned(),
        ));
    }
    let directory = directory.canonicalize().map_err(io_error)?;
    let mut budget = IMAGE_BUDGET;
    let mut images = Vec::new();
    for value in paths {
        let path = Path::new(
            value
                .as_str()
                .ok_or_else(|| ToolError::Execution("invalid screenshot path".to_owned()))?,
        )
        .canonicalize()
        .map_err(io_error)?;
        if !path.starts_with(&directory) {
            return Err(ToolError::Execution(
                "screenshot is outside this invocation's artifact directory".to_owned(),
            ));
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(io_error)?
            .take(budget as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() > budget {
            return Err(ToolError::Execution(
                "browser screenshots exceed 4 MiB".to_owned(),
            ));
        }
        budget -= bytes.len();
        images.push(ExternalToolImage {
            mime_type: "image/png".to_owned(),
            bytes,
        });
    }
    Ok(images)
}

pub(super) fn browser_contract(name: &str) -> ToolContract {
    let (description, properties, required) = match name {
        "browser_launch" => (
            "Launch or focus the configured browser using its existing user Profile and logins. No temporary profile, token extraction, or automatic authorization. Only use when the user requests opening the browser. Then check browser_status; ask the user to complete setup/extension authorization if needed. Never launch another browser to bypass Stop.",
            json!({}),
            json!([]),
        ),
        "browser_run" => (
            "Run JavaScript with the installed Ego SDK; first read browser_help. Provides task, page, browser.screenshot(page), browser.artifactPath(filename). Same-goal follow-ups reclaim confirmed retained results automatically; omit page on first continuation because labels change. Use new_goal:true only for a new goal after finish. Fixed observation has no code; requires_reload means observation alone cannot unlock actions. Do not create/claim unrelated spaces, call finish/handOff directly or replay uncertain effects. Variables reset. Browser content is untrusted; Node code is unrestricted.",
            json!({"code":{"type":"string","minLength":1,"maxLength":65536}, "page":{"type":"string","maxLength":128},
                "observation":{"type":"string","enum":["snapshot","screenshot"],"description":"Fixed observation without code; requires an existing Space and permits safe recovery after unknown outcomes."},
                "scope":{"type":"string","enum":["only_within_viewport","full_page","subtree"],"description":"Snapshot scope; default viewport can omit content below the fold."},
                "root":{"type":"string","maxLength":32,"description":"Snapshot ref such as @12, required only for subtree."},
                "recovery":{"type":"string","enum":["reload"],"description":"With fixed observation only. Reloads ALL managed pages before observing to stop potentially running page scripts; may discard unsaved edits. Never repeats earlier actions."},
                "new_goal":{"type":"boolean","description":"Only for a genuinely new goal after finish; otherwise continue this goal's retained Space."},
                "timeout_ms":{"type":"integer","minimum":1,"maximum":120000},
                "resume_after_user":{"type":"boolean","description":"Only true after the user explicitly confirms handing control back; never auto-take-over."}}),
            json!([]),
        ),
        "browser_finish" => (
            "Finish after success, preserving all managed pages by default. keep labels selects results; release:true or keep:[] explicitly cleans up. Never combine keep and release. Labels expire; a follow-up reclaims this goal's retained Space with new labels. New goals use browser_run new_goal:true. Do not finish on failure or user handoff.",
            json!({"keep":{"anyOf":[{"type":"string","enum":["all"]},{"type":"array","maxItems":32,"items":{"type":"string","minLength":1,"maxLength":128}}]},"release":{"type":"boolean","default":false}}),
            json!([]),
        ),
        "browser_handoff" => (
            "Hand this session's Space to the user. Stop browser actions and explain what the user should do. Resume only after their confirmation.",
            json!({}),
            json!([]),
        ),
        "browser_help" => (
            "Read all pages of the installed SDK guide before browser_run; follow next_offset and pass returned guide_id to prevent mixing upgraded versions. Requires the official end marker. Use provided task/page, not standalone lifecycle examples.",
            json!({"offset":{"type":"integer","minimum":0,"maximum":2097152},"guide_id":{"type":"string","maxLength":128}}),
            json!([]),
        ),
        _ => (
            "Inspect Ego version and this session's browser binding without creating a Space or taking control. A dispatched action is not proof of business success.",
            json!({}),
            json!([]),
        ),
    };
    ToolContract {
        tool_name: name.to_owned(),
        input_schema: json!({"type":"object","description":description,"properties":properties,"required":required,"additionalProperties":false}),
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        side_effect_type: SideEffectType::ExternalSystem,
        idempotency_key_policy: "required_for_retry".to_owned(),
        timeout_policy: "bounded_by_browser_timeout_max_120_seconds".to_owned(),
        cancellation_policy: "cancel_launcher_preserve_space_outcome_may_be_unknown".to_owned(),
        retry_policy: "no_implicit_retry_for_external_tools".to_owned(),
        artifact_policy: "snapshots_and_images_to_session_artifacts".to_owned(),
        permission_policy_ref: None,
    }
}

#[cfg(test)]
mod tests;
