//! MCP adapter for the shared Agent Runtime.
//!
//! This module owns only MCP framing and parameter translation.  Thread state,
//! turn execution, approvals and event projection remain in `golutra-client`
//! and `RuntimeHost`.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use golutra_client::{AgentClient, AgentThread, RuntimeTransport};
use golutra_core::{TaskContract, TaskStatus};
use golutra_protocol::{
    AgentExecutionMode, AgentItemKind, AgentStreamEvent, AgentToolProfile,
    AgentTurnExecutionOptions, AgentTurnOptions,
};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ServerCapabilities, ServerInfo},
    schemars::JsonSchema,
    tool, tool_handler, tool_router,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, OnceCell};

const MAX_RETURNED_EVENTS: usize = 128;

#[derive(Debug, Clone)]
pub struct Config {
    pub cwd: PathBuf,
    pub connect: Option<String>,
    pub daemon: bool,
    pub embedded: bool,
}

#[derive(Debug, Clone)]
enum TransportMode {
    Embedded,
    Daemon,
    Remote(String),
}

#[derive(Debug, Clone)]
struct McpRuntime {
    mode: TransportMode,
    clients: Arc<Mutex<HashMap<PathBuf, Arc<OnceCell<AgentClient>>>>>,
}

impl McpRuntime {
    fn new(config: &Config) -> Result<Self, String> {
        if config.embedded && (config.daemon || config.connect.is_some()) {
            return Err(
                "mcp-server --embedded cannot be combined with --daemon or --connect".to_owned(),
            );
        }
        if config.daemon && config.connect.is_some() {
            return Err("mcp-server --daemon cannot be combined with --connect".to_owned());
        }
        let mode = if config.embedded {
            TransportMode::Embedded
        } else if let Some(connect) = config.connect.clone() {
            TransportMode::Remote(connect)
        } else {
            // The default is the long-lived user-level runtime.  `--embedded`
            // is explicit so callers do not accidentally create a second
            // runtime beside their TUI or SDK process.
            TransportMode::Daemon
        };
        Ok(Self {
            mode,
            clients: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    async fn client_for(&self, workspace: &Path) -> Result<AgentClient, String> {
        let cell = self
            .clients
            .lock()
            .await
            .entry(workspace.to_path_buf())
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone();
        let client = cell
            .get_or_try_init(|| async {
                let transport = match &self.mode {
                    TransportMode::Embedded => RuntimeTransport::for_cwd(workspace).await,
                    TransportMode::Daemon => RuntimeTransport::local_daemon(workspace).await,
                    TransportMode::Remote(url) => {
                        RuntimeTransport::connect(url.clone(), workspace).await
                    }
                }
                .map_err(|error| format!("connect runtime for {}: {error}", workspace.display()))?;
                Ok::<_, String>(AgentClient::new(transport))
            })
            .await?;
        Ok(client.clone())
    }
}

#[derive(Debug, Clone)]
struct GolutraMcpServer {
    default_workspace: PathBuf,
    runtime: McpRuntime,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct RunParameters {
    /// The instruction to execute.
    prompt: String,
    /// Optional absolute or default-workspace-relative directory.
    #[serde(default)]
    workspace: Option<String>,
    /// Resume a durable thread.  Omit this field to create a new thread.
    #[serde(default)]
    thread_id: Option<String>,
    /// Optional JSON Schema applied to the final assistant response.
    #[serde(default)]
    output_schema: Option<Value>,
    /// Explicit runtime completion and verification contract.
    #[serde(default)]
    task_contract: Option<TaskContract>,
    /// Additional objective checks for the runtime verification layer.
    #[serde(default)]
    completion_criteria: Vec<String>,
    /// Optional completion policy profile. Defaults to the open model-facing loop.
    #[serde(default)]
    execution_mode: AgentExecutionMode,
    /// Model-visible tool profile. Defaults to the compact coding surface.
    #[serde(default)]
    tool_profile: AgentToolProfile,
    /// Include a bounded normalized event sample in the tool result.
    #[serde(default)]
    include_events: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct ReplyParameters {
    /// Existing durable thread id returned by `golutra`.
    thread_id: String,
    /// The follow-up instruction.
    prompt: String,
    /// Optional absolute or default-workspace-relative directory.
    #[serde(default)]
    workspace: Option<String>,
    /// Optional JSON Schema applied to the final assistant response.
    #[serde(default)]
    output_schema: Option<Value>,
    /// Explicit runtime completion and verification contract.
    #[serde(default)]
    task_contract: Option<TaskContract>,
    /// Additional objective checks for the runtime verification layer.
    #[serde(default)]
    completion_criteria: Vec<String>,
    /// Optional completion policy profile. Defaults to the open model-facing loop.
    #[serde(default)]
    execution_mode: AgentExecutionMode,
    /// Model-visible tool profile. Defaults to the compact coding surface.
    #[serde(default)]
    tool_profile: AgentToolProfile,
    /// Include a bounded normalized event sample in the tool result.
    #[serde(default)]
    include_events: bool,
}

impl GolutraMcpServer {
    fn new(default_workspace: PathBuf, runtime: McpRuntime) -> Self {
        Self {
            default_workspace,
            runtime,
            tool_router: Self::tool_router(),
        }
    }

    async fn run_parameters(
        &self,
        parameters: RunParameters,
        require_thread: Option<String>,
    ) -> CallToolResult {
        if parameters.prompt.trim().is_empty() {
            return error_result("prompt cannot be empty");
        }
        let workspace =
            match resolve_workspace(&self.default_workspace, parameters.workspace.as_deref()) {
                Ok(workspace) => workspace,
                Err(error) => return error_result(error),
            };
        let thread_id = require_thread.or(parameters.thread_id);
        let client = match self.runtime.client_for(&workspace).await {
            Ok(client) => client,
            Err(error) => return error_result(error),
        };
        let thread = match thread_id {
            Some(thread_id) => match thread_id.parse() {
                Ok(thread_id) => match client.resume_thread(thread_id).await {
                    Ok(thread) => thread,
                    Err(error) => return error_result(format!("resume thread failed: {error}")),
                },
                Err(error) => return error_result(format!("invalid thread_id: {error}")),
            },
            None => match client.start_thread().await {
                Ok(thread) => thread,
                Err(error) => return error_result(format!("start thread failed: {error}")),
            },
        };
        execute_turn(
            thread,
            parameters.prompt,
            AgentTurnOptions {
                task_contract: parameters.task_contract,
                output_schema: parameters.output_schema,
                completion_criteria: parameters.completion_criteria,
                max_elapsed_ms: None,
                allow_network: false,
                yolo: false,
                defer_external_verification: false,
                external_verifiers: Vec::new(),
                discover_project_verifiers: false,
            },
            AgentTurnExecutionOptions {
                execution_mode: parameters.execution_mode,
                tool_profile: parameters.tool_profile,
            },
            parameters.include_events,
        )
        .await
    }
}

#[tool_router]
impl GolutraMcpServer {
    /// Execute one turn, creating or resuming a durable thread.
    #[tool(
        name = "golutra",
        description = "Run one instruction through the shared Golutra Agent Runtime and return its verified result."
    )]
    async fn run(&self, Parameters(parameters): Parameters<RunParameters>) -> CallToolResult {
        self.run_parameters(parameters, None).await
    }

    /// Continue an existing durable thread.
    #[tool(
        name = "golutra-reply",
        description = "Continue an existing Golutra thread through the shared Agent Runtime."
    )]
    async fn reply(&self, Parameters(parameters): Parameters<ReplyParameters>) -> CallToolResult {
        self.run_parameters(
            RunParameters {
                prompt: parameters.prompt,
                workspace: parameters.workspace,
                thread_id: None,
                output_schema: parameters.output_schema,
                task_contract: parameters.task_contract,
                completion_criteria: parameters.completion_criteria,
                execution_mode: parameters.execution_mode,
                tool_profile: parameters.tool_profile,
                include_events: parameters.include_events,
            },
            Some(parameters.thread_id),
        )
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for GolutraMcpServer {
    #[allow(deprecated)]
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Golutra MCP adapter. Every tool call uses the shared RuntimeHost; non-interactive approvals are denied by default.",
        )
    }
}

async fn execute_turn(
    thread: AgentThread,
    prompt: String,
    options: AgentTurnOptions,
    execution: AgentTurnExecutionOptions,
    include_events: bool,
) -> CallToolResult {
    let thread_ref = thread.reference().clone();
    let mut handle = match thread
        .start_turn_with_execution_options(prompt, options, execution)
        .await
    {
        Ok(handle) => handle,
        Err(error) => return error_result(format!("start turn failed: {error}")),
    };
    let mut events = Vec::new();
    let mut denied_approvals = Vec::new();
    while let Some(event) = match handle.next_event().await {
        Ok(event) => event,
        Err(error) => return error_result(format!("read turn events failed: {error}")),
    } {
        if let Some(approval_id) = approval_id(&event) {
            denied_approvals.push(approval_id.clone());
            if let Err(error) = handle.resolve_approval(approval_id, false).await {
                return error_result(format!("deny approval failed: {error}"));
            }
        }
        if include_events
            && events.len() < MAX_RETURNED_EVENTS
            && let Ok(value) = serde_json::to_value(&event)
        {
            events.push(value);
        }
    }
    let result = match handle.wait().await {
        Ok(result) => result,
        Err(error) => return error_result(format!("wait for turn failed: {error}")),
    };
    let succeeded = result.status == TaskStatus::Completed;
    let events_truncated = include_events && events.len() >= MAX_RETURNED_EVENTS;
    let payload = json!({
        "thread": thread_ref,
        "turn": result,
        "approvals_denied": denied_approvals,
        "events": if include_events { Value::Array(events) } else { Value::Null },
        "events_truncated": events_truncated,
    });
    if succeeded {
        CallToolResult::structured(payload)
    } else {
        CallToolResult::structured_error(payload)
    }
}

fn approval_id(event: &AgentStreamEvent) -> Option<String> {
    let AgentStreamEvent::ItemStarted { item } = event else {
        return None;
    };
    if item.kind != AgentItemKind::Approval {
        return None;
    }
    item.data
        .pointer("/payload/approval_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn resolve_workspace(default_workspace: &Path, requested: Option<&str>) -> Result<PathBuf, String> {
    let path = requested.map(PathBuf::from).map_or_else(
        || default_workspace.to_path_buf(),
        |path| {
            if path.is_absolute() {
                path
            } else {
                default_workspace.join(path)
            }
        },
    );
    let path = std::fs::canonicalize(&path)
        .map_err(|error| format!("workspace `{}` is unavailable: {error}", path.display()))?;
    if !path.is_dir() {
        return Err(format!("workspace `{}` is not a directory", path.display()));
    }
    Ok(path)
}

fn error_result(message: impl Into<String>) -> CallToolResult {
    CallToolResult::structured_error(json!({"error": message.into()}))
}

pub async fn run(config: Config) -> Result<(), String> {
    let default_workspace = resolve_workspace(&config.cwd, None)?;
    let runtime = McpRuntime::new(&config)?;
    let server = GolutraMcpServer::new(default_workspace, runtime);
    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|error| format!("MCP initialization failed: {error}"))?;
    service
        .waiting()
        .await
        .map_err(|error| format!("MCP server failed: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn workspace_resolution_is_relative_to_default() {
        let root = tempdir().expect("root");
        let nested = root.path().join("nested");
        std::fs::create_dir(&nested).expect("nested");
        assert_eq!(
            resolve_workspace(root.path(), Some("nested")).expect("workspace"),
            nested.canonicalize().expect("canonical")
        );
    }

    #[test]
    fn transport_mode_defaults_to_daemon() {
        let config = Config {
            cwd: PathBuf::from("/tmp"),
            connect: None,
            daemon: false,
            embedded: false,
        };
        assert!(matches!(
            McpRuntime::new(&config).expect("config").mode,
            TransportMode::Daemon
        ));
    }

    #[tokio::test]
    async fn concurrent_workspace_requests_share_one_embedded_runtime() {
        let workspace = tempdir().expect("workspace");
        let runtime = McpRuntime::new(&Config {
            cwd: workspace.path().to_path_buf(),
            connect: None,
            daemon: false,
            embedded: true,
        })
        .expect("runtime");

        let (left, right) = tokio::join!(
            runtime.client_for(workspace.path()),
            runtime.client_for(workspace.path())
        );
        let left = left.expect("left client");
        let right = right.expect("right client");

        assert_eq!(
            left.transport().default_session_id(),
            right.transport().default_session_id()
        );
        assert_eq!(runtime.clients.lock().await.len(), 1);
    }
}
