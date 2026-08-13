//! 受治理的 MCP stdio 工具适配层。

use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use golutra_core::{SideEffectType, ToolContract};
use golutra_plugin::{
    EnabledPlugin, PluginError, PluginStore, PluginToolManifest, PluginWorkspaceAccess,
};
use golutra_sandbox::{SandboxRequest, SystemSandbox, WorkspaceAccess};
use golutra_tools::{
    ExternalToolBackend, ExternalToolOutput, ToolCapabilities, ToolError, ToolRequest,
};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ContentBlock, Tool},
    transport::TokioChildProcess,
};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const MAX_MCP_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("plugin registry failed: {0}")]
    Plugin(#[from] PluginError),
    #[error("MCP configuration is invalid: {0}")]
    Configuration(String),
    #[error("MCP sandbox failed: {0}")]
    Sandbox(String),
    #[error("MCP transport failed: {0}")]
    Transport(String),
    #[error("MCP protocol failed: {0}")]
    Protocol(String),
    #[error("MCP output limit exceeded: {0}")]
    OutputLimit(String),
}

#[derive(Debug, Clone)]
struct ToolTarget {
    plugin_id: String,
    revision_id: String,
    remote_name: String,
    contract: ToolContract,
    declared: PluginToolManifest,
}

#[derive(Debug, Clone)]
pub struct McpToolBackend {
    store: PluginStore,
    workspace_root: PathBuf,
    scratch_root: PathBuf,
    sandbox: SystemSandbox,
    require_os_sandbox: bool,
    targets: Arc<HashMap<String, ToolTarget>>,
}

impl McpToolBackend {
    pub fn from_store(
        store: PluginStore,
        workspace_root: impl AsRef<Path>,
        scratch_root: impl AsRef<Path>,
    ) -> Result<Option<Self>, McpError> {
        Self::with_sandbox(store, workspace_root, scratch_root, SystemSandbox::detect())
    }

    /// Load enabled plugins without requiring an OS-enforced child-process sandbox.
    /// Callers must provide an appropriate outer isolation boundary.
    pub fn from_store_unrestricted(
        store: PluginStore,
        workspace_root: impl AsRef<Path>,
        scratch_root: impl AsRef<Path>,
    ) -> Result<Option<Self>, McpError> {
        Self::build(
            store,
            workspace_root,
            scratch_root,
            SystemSandbox::process_only(),
            false,
        )
    }

    pub fn with_sandbox(
        store: PluginStore,
        workspace_root: impl AsRef<Path>,
        scratch_root: impl AsRef<Path>,
        sandbox: SystemSandbox,
    ) -> Result<Option<Self>, McpError> {
        Self::build(store, workspace_root, scratch_root, sandbox, true)
    }

    fn build(
        store: PluginStore,
        workspace_root: impl AsRef<Path>,
        scratch_root: impl AsRef<Path>,
        sandbox: SystemSandbox,
        require_os_sandbox: bool,
    ) -> Result<Option<Self>, McpError> {
        let enabled = store.enabled()?;
        if enabled.is_empty() {
            return Ok(None);
        }
        let workspace_root = canonical_directory(workspace_root.as_ref(), "workspace")?;
        let scratch_root = absolute_path(scratch_root.as_ref())?;
        ensure_private_dir(&scratch_root)?;
        let scratch_root = canonical_directory(&scratch_root, "scratch")?;
        let targets = build_targets(&enabled)?;
        Ok(Some(Self {
            store,
            workspace_root,
            scratch_root,
            sandbox,
            require_os_sandbox,
            targets: Arc::new(targets),
        }))
    }

    #[must_use]
    pub fn sandbox_enforced(&self) -> bool {
        self.sandbox.os_enforced()
    }

    async fn call_target(
        &self,
        target: &ToolTarget,
        request: &ToolRequest,
        cancellation: CancellationToken,
    ) -> Result<ExternalToolOutput, McpError> {
        if self.require_os_sandbox && !self.sandbox.os_enforced() {
            return Err(McpError::Sandbox(
                "external plugins require an OS-enforced sandbox".to_owned(),
            ));
        }
        let plugin = self.resolve_enabled_plugin(target)?;
        let scratch = tempfile::Builder::new()
            .prefix("mcp-")
            .tempdir_in(&self.scratch_root)
            .map_err(|error| McpError::Sandbox(error.to_string()))?;
        let launch = self.launch_plan(&plugin, scratch.path())?;
        let mut command = Command::new(&launch.program);
        command
            .args(&launch.args)
            .current_dir(&plugin.package_root)
            .env_clear()
            .envs(&launch.environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        inject_declared_environment(&mut command, &plugin)?;
        let transport = TokioChildProcess::new(command)
            .map_err(|error| McpError::Transport(error.to_string()))?;
        let mut client = ()
            .serve(transport)
            .await
            .map_err(|error| McpError::Protocol(format!("MCP initialization failed: {error}")))?;
        let service_cancellation = client.cancellation_token();
        let cancellation_watcher = tokio::spawn(async move {
            cancellation.cancelled().await;
            service_cancellation.cancel();
        });

        let result = async {
            let remote_tools = client
                .list_all_tools()
                .await
                .map_err(|error| McpError::Protocol(error.to_string()))?;
            verify_remote_tool(target, &remote_tools)?;
            let arguments = request.arguments.as_object().cloned().ok_or_else(|| {
                McpError::Configuration("MCP tool arguments must be an object".to_owned())
            })?;
            client
                .call_tool(
                    CallToolRequestParams::new(target.remote_name.clone())
                        .with_arguments(arguments),
                )
                .await
                .map_err(|error| McpError::Protocol(error.to_string()))
        }
        .await;
        cancellation_watcher.abort();
        let _ = client.close_with_timeout(CLOSE_TIMEOUT).await;
        convert_result(result?)
    }

    fn resolve_enabled_plugin(&self, target: &ToolTarget) -> Result<EnabledPlugin, McpError> {
        self.store
            .enabled()?
            .into_iter()
            .find(|plugin| {
                plugin.manifest.id == target.plugin_id && plugin.revision_id == target.revision_id
            })
            .ok_or_else(|| {
                McpError::Configuration(format!(
                    "plugin `{}` revision `{}` is no longer enabled; restart the runtime",
                    target.plugin_id, target.revision_id
                ))
            })
    }

    fn launch_plan(
        &self,
        plugin: &EnabledPlugin,
        scratch_dir: &Path,
    ) -> Result<golutra_sandbox::SandboxLaunch, McpError> {
        let program = resolve_program(&plugin.package_root, &plugin.manifest.server.command)?;
        let workspace_access = match plugin.manifest.permissions.workspace_access {
            PluginWorkspaceAccess::ReadOnly => WorkspaceAccess::ReadOnly,
            PluginWorkspaceAccess::ReadWrite => WorkspaceAccess::ReadWrite,
        };
        self.sandbox
            .plan(&SandboxRequest {
                program,
                args: plugin
                    .manifest
                    .server
                    .args
                    .iter()
                    .map(OsString::from)
                    .collect(),
                cwd: plugin.package_root.clone(),
                workspace_root: self.workspace_root.clone(),
                scratch_dir: scratch_dir.to_path_buf(),
                read_only_roots: Vec::new(),
                workspace_access,
                allow_network: plugin.manifest.permissions.allow_network,
            })
            .map_err(|error| McpError::Sandbox(error.to_string()))
    }
}

#[async_trait]
impl ExternalToolBackend for McpToolBackend {
    fn contracts(&self) -> Vec<ToolContract> {
        let mut contracts = self
            .targets
            .values()
            .map(|target| target.contract.clone())
            .collect::<Vec<_>>();
        contracts.sort_by(|left, right| left.tool_name.cmp(&right.tool_name));
        contracts
    }

    fn capabilities(&self) -> HashMap<String, ToolCapabilities> {
        self.targets
            .keys()
            .map(|tool_name| {
                (
                    tool_name.clone(),
                    ToolCapabilities {
                        // MCP targets only enter this backend after their immutable package,
                        // permissions and schemas pass the owner's review/enable lifecycle.
                        available_in_coding_profile: true,
                        // A remote read may still depend on server state, so concurrency stays
                        // serial until the reviewed manifest can express that guarantee.
                        parallel_read_safe: false,
                        coding_profile_hidden_arguments: Vec::new(),
                    },
                )
            })
            .collect()
    }

    async fn call(
        &self,
        request: &ToolRequest,
        cancellation: CancellationToken,
    ) -> Result<ExternalToolOutput, ToolError> {
        let target = self
            .targets
            .get(&request.tool_name)
            .ok_or_else(|| ToolError::UnknownTool(request.tool_name.clone()))?;
        self.call_target(target, request, cancellation)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))
    }
}

fn build_targets(plugins: &[EnabledPlugin]) -> Result<HashMap<String, ToolTarget>, McpError> {
    let mut targets = HashMap::new();
    for plugin in plugins {
        for tool in &plugin.manifest.tools {
            let tool_name = format!("mcp__{}__{}", plugin.manifest.id, tool.name);
            let contract = external_contract(&tool_name, tool);
            let target = ToolTarget {
                plugin_id: plugin.manifest.id.clone(),
                revision_id: plugin.revision_id.clone(),
                remote_name: tool.name.clone(),
                contract,
                declared: tool.clone(),
            };
            if targets.insert(tool_name.clone(), target).is_some() {
                return Err(McpError::Configuration(format!(
                    "duplicate namespaced MCP tool `{tool_name}`"
                )));
            }
        }
    }
    Ok(targets)
}

fn external_contract(tool_name: &str, tool: &PluginToolManifest) -> ToolContract {
    ToolContract {
        tool_name: tool_name.to_owned(),
        input_schema: tool.input_schema.clone(),
        output_schema: tool
            .output_schema
            .clone()
            .unwrap_or_else(|| json!({"type": "object"})),
        error_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "code": {"type": "string"},
                "message": {"type": "string"}
            },
            "required": ["code", "message"]
        }),
        side_effect_type: tool.side_effect_type,
        idempotency_key_policy: match tool.side_effect_type {
            SideEffectType::None => "not_required",
            SideEffectType::File
            | SideEffectType::Process
            | SideEffectType::Network
            | SideEffectType::ExternalSystem => "required_for_retry",
        }
        .to_owned(),
        timeout_policy: "bounded_by_external_tool_timeout".to_owned(),
        cancellation_policy: "cancels_MCP_service_and_child_process".to_owned(),
        retry_policy: "no_implicit_retry_for_external_tools".to_owned(),
        artifact_policy: "raw_output_to_redacted_artifact_ref".to_owned(),
        permission_policy_ref: None,
    }
}

fn verify_remote_tool(target: &ToolTarget, remote_tools: &[Tool]) -> Result<(), McpError> {
    let remote = remote_tools
        .iter()
        .find(|tool| tool.name == target.remote_name)
        .ok_or_else(|| {
            McpError::Protocol(format!(
                "MCP server did not expose reviewed tool `{}`",
                target.remote_name
            ))
        })?;
    let remote_input = Value::Object((*remote.input_schema).clone());
    if remote_input != target.declared.input_schema {
        return Err(McpError::Protocol(format!(
            "MCP tool `{}` input schema differs from the reviewed manifest",
            target.remote_name
        )));
    }
    if let Some(declared_output) = &target.declared.output_schema {
        let remote_output = remote
            .output_schema
            .as_ref()
            .map(|schema| Value::Object((**schema).clone()));
        if remote_output.as_ref() != Some(declared_output) {
            return Err(McpError::Protocol(format!(
                "MCP tool `{}` output schema differs from the reviewed manifest",
                target.remote_name
            )));
        }
    }
    Ok(())
}

fn convert_result(result: CallToolResult) -> Result<ExternalToolOutput, McpError> {
    let mut content = String::new();
    for block in &result.content {
        let rendered = render_content_block(block)?;
        if !content.is_empty() {
            content.push('\n');
        }
        if content.len().saturating_add(rendered.len()) > MAX_MCP_OUTPUT_BYTES {
            return Err(McpError::OutputLimit(format!(
                "tool content exceeds {MAX_MCP_OUTPUT_BYTES} bytes"
            )));
        }
        content.push_str(&rendered);
    }
    let structured_facts = result
        .structured_content
        .unwrap_or_else(|| json!({"content_blocks": result.content.len()}));
    let structured_bytes = serde_json::to_vec(&structured_facts)
        .map_err(|error| McpError::Protocol(error.to_string()))?;
    if structured_bytes.len() > MAX_MCP_OUTPUT_BYTES {
        return Err(McpError::OutputLimit(format!(
            "structured content exceeds {MAX_MCP_OUTPUT_BYTES} bytes"
        )));
    }
    let is_error = result.is_error.unwrap_or(false);
    Ok(ExternalToolOutput {
        summary: if is_error {
            "MCP tool returned an error"
        } else {
            "MCP tool completed"
        }
        .to_owned(),
        content,
        structured_facts,
        is_error,
    })
}

fn render_content_block(block: &ContentBlock) -> Result<String, McpError> {
    match block {
        ContentBlock::Text(text) => Ok(text.text.clone()),
        ContentBlock::Image(image) => Ok(format!(
            "<image mime_type={} encoded_bytes={}>",
            image.mime_type,
            image.data.len()
        )),
        ContentBlock::Audio(audio) => Ok(format!(
            "<audio mime_type={} encoded_bytes={}>",
            audio.mime_type,
            audio.data.len()
        )),
        ContentBlock::Resource(resource) => {
            let text = resource.get_text();
            if text.is_empty() {
                serde_json::to_string(resource)
                    .map_err(|error| McpError::Protocol(error.to_string()))
            } else {
                Ok(text)
            }
        }
        ContentBlock::ResourceLink(resource) => Ok(format!(
            "<resource uri={} name={}>",
            resource.uri, resource.name
        )),
        _ => serde_json::to_string(block).map_err(|error| McpError::Protocol(error.to_string())),
    }
}

fn inject_declared_environment(
    command: &mut Command,
    plugin: &EnabledPlugin,
) -> Result<(), McpError> {
    for name in &plugin.manifest.server.env {
        let value = env::var_os(name).ok_or_else(|| {
            McpError::Configuration(format!(
                "required plugin environment variable `{name}` is not set"
            ))
        })?;
        command.env(name, value);
    }
    Ok(())
}

fn resolve_program(package_root: &Path, command: &str) -> Result<OsString, McpError> {
    let path = Path::new(command);
    if path.is_absolute() {
        return Ok(path.as_os_str().to_owned());
    }
    if path.components().count() == 1 {
        return Ok(path.as_os_str().to_owned());
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(McpError::Configuration(
            "relative plugin command escapes its package".to_owned(),
        ));
    }
    let package_root = package_root
        .canonicalize()
        .map_err(|error| McpError::Configuration(error.to_string()))?;
    let program = package_root
        .join(path)
        .canonicalize()
        .map_err(|error| McpError::Configuration(error.to_string()))?;
    if !program.starts_with(&package_root) || !program.is_file() {
        return Err(McpError::Configuration(
            "relative plugin command is not a package file".to_owned(),
        ));
    }
    Ok(program.into_os_string())
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, McpError> {
    let canonical = path
        .canonicalize()
        .map_err(|error| McpError::Configuration(format!("{label}: {error}")))?;
    if !canonical.is_dir() {
        return Err(McpError::Configuration(format!(
            "{label} is not a directory: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn absolute_path(path: &Path) -> Result<PathBuf, McpError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| McpError::Configuration(error.to_string()))
    }
}

fn ensure_private_dir(path: &Path) -> Result<(), McpError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(McpError::Configuration(format!(
                "MCP scratch path cannot be a symbolic link: {}",
                path.display()
            )));
        }
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(McpError::Configuration(format!(
                "MCP scratch path is not a directory: {}",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)
                .map_err(|error| McpError::Configuration(error.to_string()))?;
        }
        Err(error) => return Err(McpError::Configuration(error.to_string())),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| McpError::Configuration(error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
