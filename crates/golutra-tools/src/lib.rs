use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, RwLock},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use golutra_core::{
    ArtifactId, ArtifactRecord, EvidenceRecord, EvidenceStrength, FileContentKind,
    FileStateMetadata, PolicyBlockDisposition, PolicyDecision, PolicyEvaluation, RedactionStatus,
    SessionId, SideEffectType, ToolCallId, ToolContract, ToolExecutionMetrics, ToolProgress,
    ToolProgressPhase, ToolResultEnvelope, ToolResultStatus, TurnId,
};
use golutra_policy::{
    WorkspacePolicy, contains_shell_metacharacter, explicit_shell_script,
    parse_shell_command_with_input, shell_command_is_strictly_read_only,
};
use golutra_sandbox::{SystemSandbox, WorkspaceAccess};
use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const DEFAULT_EXCERPT_LIMIT: usize = 2048;
const DEFAULT_READ_EXCERPT_BYTES: usize = 16 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Maximum raw content retained by a built-in tool artifact.
pub const MAX_TOOL_ARTIFACT_CONTENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_FILE_CONTENT_BYTES: u64 = MAX_TOOL_ARTIFACT_CONTENT_BYTES as u64;
/// Maximum file-body bytes retained by one side of an opaque workspace scan.
///
/// Hosts that queue complete tool reports use this contract to reserve room
/// for both the before and after snapshots without inspecting tool internals.
pub const MAX_WORKSPACE_SNAPSHOT_CONTENT_BYTES: usize = 32 * 1024 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 10_000;
const MAX_DIRECTORY_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_PATH_ARGUMENT_CHARS: usize = 4 * 1024;
const MAX_PATTERN_ARGUMENT_CHARS: usize = 64 * 1024;
const MAX_SHELL_COMMAND_CHARS: usize = 64 * 1024;
const MAX_SHELL_ARGV_ITEMS: usize = 1_024;
const MAX_PROCESS_INPUT_CHARS: usize = 64 * 1024;
const MAX_PATCH_BYTES: usize = 16 * 1024 * 1024;
const MAX_DELEGATED_TASK_CHARS: usize = 64 * 1024;
const DEFAULT_READ_MAX_LINES: usize = 200;
const MAX_READ_LINES: usize = 2_000;
const MAX_FILE_EDITS: usize = 128;
const MAX_BACKGROUND_PROCESS_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1_000;
const MAX_TOOL_ERROR_CHARS: usize = 4 * 1024;
const MAX_AUDIT_RESOURCE_CHARS: usize = 64 * 1024;
pub const MAX_TOOL_ARGUMENT_DISPLAY_BYTES: usize = 8 * 1024;
const MAX_TOOL_ARGUMENT_DISPLAY_STRING_BYTES: usize = 1024;
const MAX_TOOL_ARGUMENT_COMPACT_STRING_BYTES: usize = 96;
const MAX_TOOL_ARGUMENT_DISPLAY_ITEMS: usize = 24;
const MAX_TOOL_ARGUMENT_DISPLAY_DEPTH: usize = 4;
pub const MAX_MODEL_TOOL_RESULT_BYTES: usize = 16 * 1024;
const DEFAULT_MODEL_TOOL_RESULT_BYTES: usize = 8 * 1024;
const MIN_MODEL_TOOL_RESULT_BYTES: usize = 1024;
const MAX_MODEL_TOOL_RESULT_SUMMARY_CHARS: usize = 2 * 1024;
const MAX_MODEL_TOOL_RESULT_EXCERPT_BYTES: usize = 16 * 1024;
const MAX_MODEL_TOOL_RESULT_COMPACT_EXCERPT_CHARS: usize = 512;
const MAX_MODEL_TOOL_RESULT_FACT_STRING_CHARS: usize = 4 * 1024;
const MAX_MODEL_TOOL_RESULT_ITEMS: usize = 32;
const MAX_MODEL_TOOL_RESULT_DEPTH: usize = 5;
const MAX_MODEL_CHANGE_PREVIEW_BYTES: usize = 3 * 1024;
const MAX_MODEL_CHANGE_PREVIEW_LINES: usize = 24;
const MAX_MODEL_CHANGE_PREVIEW_FILES: usize = 8;
const MAX_MODEL_CHANGE_PREVIEW_LINE_BYTES: usize = 512;
const MAX_MODEL_TOOL_OUTPUT_CHARS: usize = 2 * 1024;
const MAX_MODEL_TOOL_REASON_CHARS: usize = 512;
const MAX_MODEL_TOOL_SEARCH_QUERY_CHARS: usize = 512;
const MAX_MODEL_TOOL_SEARCH_RESULTS: usize = 8;
const MAX_MODEL_TOOL_SEARCH_TITLE_CHARS: usize = 160;
const MAX_MODEL_TOOL_SEARCH_URL_CHARS: usize = 512;
const MAX_MODEL_TOOL_SEARCH_SNIPPET_CHARS: usize = 320;
// 缺失文件提示只服务于下一次模型调用，必须有界，不能把错误恢复变成一次
// 无界的工作区扫描或把宿主目录结构泄漏到 provider。
const MAX_READ_FILE_HINT_DEPTH: usize = 6;
const MAX_READ_FILE_HINT_ENTRIES: usize = 2_048;
const MAX_READ_FILE_HINT_CANDIDATES: usize = 8;
const EXTERNAL_TOOL_TIMEOUT_MS: u64 = if cfg!(test) { 100 } else { 30_000 };
const MAX_VERIFIER_TIMEOUT_MS: u64 = 30 * 60 * 1_000;
const MAX_VERIFIER_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const DEADLINE_CLEANUP_TIMEOUT: Duration = Duration::from_millis(250);
const CODE_INDEX_BUILD_CONCURRENCY: usize = 1;
static CODE_INDEX_BUILD_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(CODE_INDEX_BUILD_CONCURRENCY)));
// Sandbox capability is process-wide. Detecting the launcher for every task
// adds filesystem probes to the critical path without changing the result.
static DETECTED_SANDBOX: LazyLock<SystemSandbox> = LazyLock::new(SystemSandbox::detect);
pub const CONTRACT_FILE_CONTENT_VERIFIER_TOOL: &str = "contract_file_content_verifier";
pub const CONTRACT_PATH_VERIFIER_TOOL: &str = "contract_path_verifier";

mod builtin;
mod model_patch;
mod process;
mod process_supervisor;
mod project_verifier;
mod text_search;
mod web_search;
mod workspace_scan;

pub(crate) use process::{
    CommandLine, ProcessExecutionRequest, ProcessProgress, ProcessStream, run_process_with_progress,
};
#[cfg(test)]
pub(crate) use process::{MAX_PIPE_OUTPUT_BYTES, join_pipe_reader, run_process, spawn_pipe_reader};
pub use process_supervisor::ProcessSupervisor;
#[cfg(test)]
pub(crate) use process_supervisor::max_terminal_processes;
pub(crate) use process_supervisor::{
    ProcessSnapshot, ProcessStartRequest, ProcessState, ProcessSummary, default_poll_wait_ms,
    default_start_wait_ms, max_poll_wait_ms,
};
pub use project_verifier::{DiscoveredProjectVerifier, discover_project_verifiers};
pub use web_search::HttpWebSearchBackend;

use builtin::BuiltinTool;

/// 面向 provider 的稳定契约，保持默认模型工具面足够小。
pub const PI_PLUS_TOOL_NAMES: [&str; 8] = [
    "read_file",
    "write_file",
    "edit_file",
    "shell",
    "web_search",
    "shell_session",
    "subagent",
    "apply_patch",
];

#[must_use]
pub fn is_pi_plus_tool(tool_name: &str) -> bool {
    PI_PLUS_TOOL_NAMES.contains(&tool_name)
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool is not registered: {0}")]
    UnknownTool(String),
    #[error("tool arguments are invalid: {0}")]
    InvalidArguments(String),
    #[error("tool execution failed: {0}")]
    Execution(String),
    #[error("external tool registration failed: {0}")]
    ExternalRegistration(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolRequest {
    pub tool_call_id: ToolCallId,
    pub provider_tool_call_id: Option<String>,
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub tool_name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolExecutionReport {
    pub envelope: ToolResultEnvelope,
    pub artifacts: Vec<ArtifactRecord>,
    pub evidence: Vec<EvidenceRecord>,
    pub changed_files: Vec<PathBuf>,
    pub policy_evaluation: PolicyEvaluation,
    pub artifact_contents: Vec<ArtifactContent>,
    pub before_images: Vec<FileBeforeImage>,
    pub after_images: Vec<FileBeforeImage>,
    pub metrics: ToolExecutionMetrics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactContent {
    pub artifact_id: ArtifactId,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBeforeImage {
    pub path: PathBuf,
    pub content: Option<Vec<u8>>,
    pub unix_mode: Option<u32>,
    pub metadata: Option<FileStateMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SideEffectPreparation {
    pub before_images: Vec<FileBeforeImage>,
    pub complete: bool,
    workspace_snapshot: Option<workspace_scan::WorkspaceSnapshot>,
}

impl SideEffectPreparation {
    /// 只有存在 workspace snapshot 时，调用方才需要为 opaque 工具持久化
    /// before-image；严格只读 shell 明确没有副作用证据需要保存。
    #[must_use]
    pub fn tracks_workspace_changes(&self) -> bool {
        self.workspace_snapshot.is_some()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolInvocation {
    pub request: ToolRequest,
    pub policy: PolicyEvaluation,
    pub approved: bool,
    preparation: Option<SideEffectPreparation>,
    deadline: Option<tokio::time::Instant>,
    pre_execution_stop: Option<ToolInvocationStop>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolInvocationStop {
    Cancelled,
    TimedOut,
}

enum ToolOperationOutcome<T> {
    Completed(T),
    Cancelled,
    TimedOut(Option<T>),
}

impl ToolInvocation {
    #[must_use]
    pub fn new(request: ToolRequest, policy: PolicyEvaluation, approved: bool) -> Self {
        Self {
            request,
            policy,
            approved,
            preparation: None,
            deadline: None,
            pre_execution_stop: None,
        }
    }

    #[must_use]
    pub fn with_preparation(mut self, preparation: SideEffectPreparation) -> Self {
        self.preparation = Some(preparation);
        self
    }

    #[must_use]
    pub fn with_deadline(mut self, deadline: tokio::time::Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExternalToolOutput {
    pub summary: String,
    pub content: String,
    pub structured_facts: Value,
    pub is_error: bool,
}

/// Result returned by the host-owned delegated-agent backend.
///
/// Delegation is kept separate from [`ExternalToolBackend`]: it may outlive a
/// normal tool-call timeout and must retain the enclosing runtime's lifecycle,
/// cancellation and workspace accounting.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskDelegationOutput {
    pub status: ToolResultStatus,
    pub summary: String,
    pub content: String,
    pub structured_facts: Value,
}

#[async_trait]
pub trait TaskDelegationBackend: std::fmt::Debug + Send + Sync {
    async fn delegate(
        &self,
        request: &ToolRequest,
        cancellation: CancellationToken,
    ) -> Result<TaskDelegationOutput, ToolError>;
}

/// 由宿主拥有的搜索适配器；替换具体 provider 时不改变 agent loop 的工具契约。
#[async_trait]
pub trait WebSearchBackend: std::fmt::Debug + Send + Sync {
    async fn search(
        &self,
        request: &ToolRequest,
        cancellation: CancellationToken,
    ) -> Result<ExternalToolOutput, ToolError>;
}

#[async_trait]
pub trait ExternalToolBackend: std::fmt::Debug + Send + Sync {
    fn contracts(&self) -> Vec<ToolContract>;

    /// Optional host-reviewed execution capabilities keyed by tool name.
    /// External tools default to the full profile and serial execution.
    fn capabilities(&self) -> HashMap<String, ToolCapabilities> {
        HashMap::new()
    }

    async fn call(
        &self,
        request: &ToolRequest,
        cancellation: CancellationToken,
    ) -> Result<ExternalToolOutput, ToolError>;
}

/// Runtime-only capabilities kept adjacent to a tool contract.
///
/// These properties affect scheduling and model-visible profiles, not the
/// provider protocol, so they intentionally remain outside [`ToolContract`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCapabilities {
    pub available_in_coding_profile: bool,
    pub parallel_read_safe: bool,
    pub coding_profile_hidden_arguments: Vec<String>,
}

/// Owner-provided source for deterministic tool replay.
///
/// A replay backend returns the recorded model-visible result and never
/// performs the original side effect. `BasicToolExecutor` still emits the
/// ordinary policy/progress/report contract around the injected result.
#[async_trait]
pub trait ToolReplayBackend: std::fmt::Debug + Send + Sync {
    async fn replay(&self, request: &ToolRequest) -> Result<ToolResultEnvelope, ToolError>;
}

#[derive(Debug, Clone)]
pub struct ToolRegistry {
    contracts: HashMap<String, ToolContract>,
    capabilities: HashMap<String, ToolCapabilities>,
    /// evaluate/prepare/invoke 共享已编译 schema，避免同一工具轮次重复编译。
    compiled_validators: Arc<RwLock<HashMap<String, Arc<jsonschema::Validator>>>>,
}

impl ToolRegistry {
    #[must_use]
    pub fn p0_default() -> Self {
        let mut contracts = HashMap::new();
        let mut capabilities = HashMap::new();
        for tool in BuiltinTool::P0_DEFAULT
            .into_iter()
            .chain(BuiltinTool::INTERNAL)
        {
            let contract = tool.contract();
            capabilities.insert(contract.tool_name.clone(), tool.capabilities());
            contracts.insert(contract.tool_name.clone(), contract);
        }
        Self {
            contracts,
            capabilities,
            compiled_validators: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    #[must_use]
    pub fn contracts(&self) -> Vec<&ToolContract> {
        let mut contracts = self.contracts.values().collect::<Vec<_>>();
        contracts.sort_by(|left, right| left.tool_name.cmp(&right.tool_name));
        contracts
    }

    /// 返回允许进入 provider 请求的稳定工具契约。
    #[must_use]
    pub fn provider_contracts(&self) -> Vec<&ToolContract> {
        let mut contracts = self
            .contracts
            .values()
            .filter(|contract| is_pi_plus_tool(&contract.tool_name))
            .collect::<Vec<_>>();
        contracts.sort_by(|left, right| left.tool_name.cmp(&right.tool_name));
        contracts
    }

    #[must_use]
    pub fn contract(&self, tool_name: &str) -> Option<&ToolContract> {
        self.contracts.get(tool_name)
    }

    #[must_use]
    pub fn capabilities(&self, tool_name: &str) -> Option<&ToolCapabilities> {
        self.capabilities.get(tool_name)
    }

    fn validate_tool_arguments(
        &self,
        contract: &ToolContract,
        arguments: &Value,
    ) -> Result<(), ToolError> {
        let validator = if let Some(validator) = self
            .compiled_validators
            .read()
            .expect("tool validator cache lock poisoned")
            .get(&contract.tool_name)
            .cloned()
        {
            validator
        } else {
            let compiled = Arc::new(jsonschema::validator_for(&contract.input_schema).map_err(
                |error| {
                    ToolError::InvalidArguments(format!(
                        "tool `{}` has an invalid input schema: {error}",
                        contract.tool_name
                    ))
                },
            )?);
            let mut cache = self
                .compiled_validators
                .write()
                .expect("tool validator cache lock poisoned");
            cache
                .entry(contract.tool_name.clone())
                .or_insert(compiled)
                .clone()
        };
        let errors = validator
            .iter_errors(arguments)
            .map(|error| {
                bounded_text(
                    &error.masked_with("<redacted-value>").to_string(),
                    MAX_TOOL_ERROR_CHARS,
                )
            })
            .collect::<Vec<_>>();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ToolError::InvalidArguments(format!(
                "tool `{}` arguments do not match its contract: {}",
                contract.tool_name,
                errors.join("; ")
            )))
        }
    }

    fn invalidate_validator(&self, tool_name: &str) {
        self.compiled_validators
            .write()
            .expect("tool validator cache lock poisoned")
            .remove(tool_name);
    }

    fn register_external(
        &mut self,
        contracts: impl IntoIterator<Item = ToolContract>,
        mut declared_capabilities: HashMap<String, ToolCapabilities>,
    ) -> Result<(), ToolError> {
        let contracts = contracts.into_iter().collect::<Vec<_>>();
        for tool_name in declared_capabilities.keys() {
            if !contracts
                .iter()
                .any(|contract| contract.tool_name == *tool_name)
            {
                return Err(ToolError::ExternalRegistration(format!(
                    "capabilities declared for unknown tool `{tool_name}`"
                )));
            }
        }
        for contract in contracts {
            if contract.tool_name.trim().is_empty() {
                return Err(ToolError::ExternalRegistration(
                    "external tool name cannot be empty".to_owned(),
                ));
            }
            if self.contracts.contains_key(&contract.tool_name) {
                return Err(ToolError::ExternalRegistration(format!(
                    "tool `{}` conflicts with an existing contract",
                    contract.tool_name
                )));
            }
            let capabilities = declared_capabilities
                .remove(&contract.tool_name)
                .unwrap_or_default();
            if capabilities.parallel_read_safe && contract.side_effect_type != SideEffectType::None
            {
                return Err(ToolError::ExternalRegistration(format!(
                    "tool `{}` cannot be parallel-read-safe because its contract admits side effects",
                    contract.tool_name
                )));
            }
            if !capabilities.available_in_coding_profile
                && !capabilities.coding_profile_hidden_arguments.is_empty()
            {
                return Err(ToolError::ExternalRegistration(format!(
                    "tool `{}` hides coding arguments but is not available in the coding profile",
                    contract.tool_name
                )));
            }
            self.capabilities
                .insert(contract.tool_name.clone(), capabilities);
            self.invalidate_validator(&contract.tool_name);
            self.contracts.insert(contract.tool_name.clone(), contract);
        }
        Ok(())
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::p0_default()
    }
}

#[derive(Debug, Clone)]
pub struct ToolRuntime {
    policy: WorkspacePolicy,
    registry: ToolRegistry,
    sandbox: SystemSandbox,
    allow_network: bool,
    external_backend: Option<Arc<dyn ExternalToolBackend>>,
    web_search_backend: Option<Arc<dyn WebSearchBackend>>,
    delegation_backend: Option<Arc<dyn TaskDelegationBackend>>,
    replay_backend: Option<Arc<dyn ToolReplayBackend>>,
    process_supervisor: ProcessSupervisor,
}

impl ToolRuntime {
    #[must_use]
    pub fn new(policy: WorkspacePolicy) -> Self {
        Self {
            policy,
            registry: ToolRegistry::p0_default(),
            sandbox: (*DETECTED_SANDBOX).clone(),
            allow_network: false,
            external_backend: None,
            web_search_backend: None,
            delegation_backend: None,
            replay_backend: None,
            process_supervisor: ProcessSupervisor::new(),
        }
    }

    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        self.policy.workspace_root()
    }

    #[must_use]
    pub fn sandbox_os_enforced(&self) -> bool {
        self.sandbox.os_enforced()
    }

    /// Resolve and compare a required workspace file without bypassing the
    /// workspace policy or reading more bytes than the expected value.
    pub async fn compare_workspace_file_content(
        &self,
        path: impl AsRef<Path>,
        expected: &[u8],
    ) -> Result<(PathBuf, bool), ToolError> {
        let resolved_path = self.resolve_tool_path("verify_file_content", path, true)?;
        let comparison = compare_file_content(&resolved_path, expected).await?;
        Ok((resolved_path, comparison.matches))
    }

    /// Verify an exact-content task contract as a durable internal tool step.
    /// The report records only sizes and digests, never the required content.
    pub async fn verify_workspace_file_content(
        &self,
        request: ToolRequest,
        expected: &[u8],
    ) -> ToolExecutionReport {
        let Some(path) = optional_string_arg(&request.arguments, "path") else {
            let policy = execution_policy(
                &request,
                PolicyDecision::Block,
                "required file content verification requires a path",
            );
            return report(
                request,
                ToolResultStatus::Error,
                "required file content verification is missing a path",
                json!({"matches": false, "error": "path is required"}),
                r#"{"matches":false,"error":"path is required"}"#.to_owned(),
                Vec::new(),
                policy,
            );
        };
        let policy = self
            .policy
            .evaluate_path("verify_file_content", &path, true);
        if policy.decision != PolicyDecision::Allow {
            let facts = json!({
                "path": path,
                "matches": false,
                "error": policy.reason,
                "expected_bytes": expected.len(),
                "expected_checksum": checksum(expected),
            });
            return file_content_verification_report(
                request,
                ToolResultStatus::Blocked,
                "required file content could not be inspected",
                facts,
                policy,
            );
        }
        let resolved_path = PathBuf::from(&policy.resource);
        match compare_file_content(&resolved_path, expected).await {
            Ok(comparison) => {
                let facts = json!({
                    "path": path,
                    "resolved_path": resolved_path,
                    "matches": comparison.matches,
                    "expected_bytes": expected.len(),
                    "actual_bytes": comparison.actual_bytes,
                    "expected_checksum": checksum(expected),
                    "actual_checksum": comparison.actual_checksum,
                });
                file_content_verification_report(
                    request,
                    ToolResultStatus::Ok,
                    if comparison.matches {
                        "required file content matches the task contract"
                    } else {
                        "required file content does not match the task contract"
                    },
                    facts,
                    policy,
                )
            }
            Err(error) => {
                let facts = json!({
                    "path": path,
                    "resolved_path": resolved_path,
                    "matches": false,
                    "expected_bytes": expected.len(),
                    "expected_checksum": checksum(expected),
                    "error": error.to_string(),
                });
                file_content_verification_report(
                    request,
                    ToolResultStatus::Error,
                    "required file content inspection failed",
                    facts,
                    policy,
                )
            }
        }
    }

    /// Verify a required task-contract path as a durable internal tool step.
    /// The report records bounded metadata and its digest without reading file
    /// contents or traversing directories.
    pub async fn verify_workspace_path(&self, request: ToolRequest) -> ToolExecutionReport {
        let Some(path) = optional_string_arg(&request.arguments, "path") else {
            let policy = execution_policy(
                &request,
                PolicyDecision::Block,
                "required path verification requires a path",
            );
            return path_verification_report(
                request,
                ToolResultStatus::Error,
                "required path verification is missing a path",
                json!({"exists": false, "error": "path is required"}),
                policy,
            );
        };
        let policy = self.policy.evaluate_path("verify_path", &path, true);
        if policy.decision != PolicyDecision::Allow {
            let reason = policy.reason.clone();
            return path_verification_report(
                request,
                ToolResultStatus::Blocked,
                "required path could not be inspected",
                json!({"path": path, "exists": false, "error": reason}),
                policy,
            );
        }

        let resolved_path = PathBuf::from(&policy.resource);
        match tokio::fs::metadata(&resolved_path).await {
            Ok(metadata) => {
                let file_type = if metadata.is_file() {
                    "file"
                } else if metadata.is_dir() {
                    "directory"
                } else {
                    "other"
                };
                let modified_unix_ms = metadata
                    .modified()
                    .ok()
                    .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                    .and_then(|duration| u64::try_from(duration.as_millis()).ok());
                let metadata_facts = json!({
                    "file_type": file_type,
                    "size_bytes": metadata.len(),
                    "readonly": metadata.permissions().readonly(),
                    "modified_unix_ms": modified_unix_ms,
                });
                let metadata_checksum = checksum(
                    serde_json::to_string(&metadata_facts)
                        .unwrap_or_default()
                        .as_bytes(),
                );
                path_verification_report(
                    request,
                    ToolResultStatus::Ok,
                    "required workspace path exists",
                    json!({
                        "path": path,
                        "resolved_path": resolved_path,
                        "exists": true,
                        "metadata": metadata_facts,
                        "metadata_checksum": metadata_checksum,
                    }),
                    policy,
                )
            }
            Err(error) => path_verification_report(
                request,
                ToolResultStatus::Error,
                "required path inspection failed",
                json!({
                    "path": path,
                    "resolved_path": resolved_path,
                    "exists": false,
                    "error": error.to_string(),
                }),
                policy,
            ),
        }
    }

    #[must_use]
    pub fn with_sandbox(mut self, sandbox: SystemSandbox) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Remove workspace and command policy restrictions for this task while
    /// retaining tool validation, cancellation, timeouts and observations.
    #[must_use]
    pub fn with_unrestricted_access(mut self, enabled: bool) -> Self {
        self.policy = self.policy.with_unrestricted_access(enabled);
        if enabled {
            self.sandbox = SystemSandbox::process_only();
        }
        self
    }

    /// Enable network access for child tools only when the enclosing runtime
    /// explicitly granted that capability. The default remains isolated.
    #[must_use]
    pub fn with_network_access(mut self, allow_network: bool) -> Self {
        self.allow_network = allow_network;
        self
    }

    pub fn with_web_search_backend(mut self, backend: Arc<dyn WebSearchBackend>) -> Self {
        self.web_search_backend = Some(backend);
        self
    }

    pub fn with_external_backend(
        mut self,
        backend: Arc<dyn ExternalToolBackend>,
    ) -> Result<Self, ToolError> {
        let contracts = backend.contracts();
        let capabilities = backend.capabilities();
        self.registry.register_external(contracts, capabilities)?;
        self.external_backend = Some(backend);
        Ok(self)
    }

    /// Register provider-visible external contracts for deterministic replay.
    ///
    /// Replay injects recorded results before dispatch, so these contracts do
    /// not grant access to a live external backend. Runtime-only capability
    /// metadata is not part of the provider protocol; recorded host MCP tools
    /// are owner-reviewed coding tools and remain serial during replay.
    pub fn with_replay_contracts(
        mut self,
        contracts: impl IntoIterator<Item = ToolContract>,
    ) -> Result<Self, ToolError> {
        let contracts = contracts
            .into_iter()
            .filter(|contract| self.registry.contract(&contract.tool_name).is_none())
            .collect::<Vec<_>>();
        let capabilities = contracts
            .iter()
            .map(|contract| {
                (
                    contract.tool_name.clone(),
                    ToolCapabilities {
                        available_in_coding_profile: is_pi_plus_tool(&contract.tool_name),
                        parallel_read_safe: false,
                        coding_profile_hidden_arguments: Vec::new(),
                    },
                )
            })
            .collect();
        self.registry.register_external(contracts, capabilities)?;
        Ok(self)
    }

    pub fn with_task_delegation_backend(
        mut self,
        backend: Arc<dyn TaskDelegationBackend>,
    ) -> Result<Self, ToolError> {
        let tool = BuiltinTool::Subagent;
        if self.registry.contract(tool.name()).is_none() {
            self.registry.invalidate_validator(tool.name());
            self.registry
                .contracts
                .insert(tool.name().to_owned(), tool.contract());
            self.registry
                .capabilities
                .insert(tool.name().to_owned(), tool.capabilities());
        }
        self.delegation_backend = Some(backend);
        Ok(self)
    }

    /// Remove a tool from this runtime's model-visible and executable registry.
    /// This keeps capability reduction explicit for restricted runtimes and child tasks.
    #[must_use]
    pub fn without_tool(mut self, tool_name: &str) -> Self {
        self.registry.invalidate_validator(tool_name);
        self.registry.contracts.remove(tool_name);
        self.registry.capabilities.remove(tool_name);
        if matches!(
            BuiltinTool::from_name(tool_name),
            Some(BuiltinTool::Subagent | BuiltinTool::DelegateTask)
        ) {
            self.delegation_backend = None;
        }
        self
    }

    /// Replace real tool execution with deterministic, artifact-backed
    /// results. This is only intended for explicit replay entrypoints.
    #[must_use]
    pub fn with_replay_backend(mut self, backend: Arc<dyn ToolReplayBackend>) -> Self {
        self.replay_backend = Some(backend);
        self
    }

    #[must_use]
    pub fn with_process_supervisor(mut self, process_supervisor: ProcessSupervisor) -> Self {
        self.process_supervisor = process_supervisor;
        self
    }

    pub async fn execute(
        &self,
        request: ToolRequest,
        cancellation: CancellationToken,
    ) -> Result<ToolExecutionReport, ToolError> {
        let policy = self.evaluate(&request)?;
        self.execute_with_policy(request, policy, false, cancellation)
            .await
    }

    pub async fn invoke(
        &self,
        mut invocation: ToolInvocation,
        cancellation: CancellationToken,
        progress: Option<&mut (dyn FnMut(ToolProgress) + Send)>,
    ) -> Result<ToolExecutionReport, ToolError> {
        let may_execute = !cancellation.is_cancelled()
            && match invocation.policy.decision {
                PolicyDecision::Allow => true,
                PolicyDecision::Ask => invocation.approved,
                PolicyDecision::Deny | PolicyDecision::Block => false,
            };
        let preparation = match invocation.preparation.take() {
            Some(preparation) => preparation,
            None if may_execute => {
                match await_preparation_operation(
                    self.prepare_side_effect_snapshot(&invocation.request),
                    &cancellation,
                    invocation.deadline,
                )
                .await
                {
                    Ok(preparation) => preparation?,
                    Err(stop) => {
                        invocation.pre_execution_stop = Some(stop);
                        SideEffectPreparation::default()
                    }
                }
            }
            None => SideEffectPreparation::default(),
        };
        invocation.preparation = Some(preparation);
        self.invoke_prepared(invocation, cancellation, progress)
            .await
    }

    /// Execute a caller-declared verification command without shell parsing.
    /// The command remains sandboxed, cancellable and scoped to this executor's
    /// workspace, but it does not require model-tool approval.
    pub async fn execute_verifier(
        &self,
        request: VerifierExecutionRequest,
        cancellation: CancellationToken,
    ) -> Result<ToolExecutionReport, ToolError> {
        let preparation = self.prepare_verifier_side_effect(&request).await?;
        self.execute_verifier_with_preparation(request, cancellation, preparation)
            .await
    }

    /// Capture the workspace state that must be persisted before a verifier
    /// process receives write access to the workspace.
    pub async fn prepare_verifier_side_effect(
        &self,
        request: &VerifierExecutionRequest,
    ) -> Result<SideEffectPreparation, ToolError> {
        if request.program.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "verifier program cannot be empty".to_owned(),
            ));
        }
        if self.replay_backend.is_some() {
            return Ok(SideEffectPreparation::default());
        }
        self.resolve_verifier_cwd(request)?;
        let snapshot = workspace_scan::capture(self.policy.workspace_root()).await;
        Ok(SideEffectPreparation {
            before_images: snapshot.before_images(),
            complete: snapshot.is_complete(),
            workspace_snapshot: Some(snapshot),
        })
    }

    /// Execute a verifier using a workspace snapshot already persisted by the
    /// runtime host.
    pub async fn execute_verifier_with_preparation(
        &self,
        request: VerifierExecutionRequest,
        cancellation: CancellationToken,
        preparation: SideEffectPreparation,
    ) -> Result<ToolExecutionReport, ToolError> {
        if request.program.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "verifier program cannot be empty".to_owned(),
            ));
        }
        let tool_request = request.as_tool_request();
        if self.replay_backend.is_some() {
            let policy = execution_policy(
                &tool_request,
                PolicyDecision::Allow,
                "deterministic replay injects a recorded verifier result",
            );
            return self
                .execute_with_policy(tool_request, policy, false, cancellation)
                .await;
        }
        let cwd = self.resolve_verifier_cwd(&request)?;
        let timeout_ms = request.timeout_ms.clamp(1, MAX_VERIFIER_TIMEOUT_MS);
        let workspace_before = match preparation.workspace_snapshot {
            Some(snapshot) => snapshot,
            None => workspace_scan::capture(self.policy.workspace_root()).await,
        };
        let output = run_process_with_progress(
            ProcessExecutionRequest {
                program: &request.program,
                args: &request.args,
                cwd: &cwd,
                workspace_root: self.policy.workspace_root(),
                timeout_ms,
                cancellation,
                sandbox: &self.sandbox,
                workspace_access: WorkspaceAccess::ReadWrite,
                allow_network: false,
                stdin: None,
                isolated_home: false,
            },
            None,
        )
        .await?;
        let workspace_changes =
            workspace_scan::compare(self.policy.workspace_root(), workspace_before).await;
        let retained_limit = request.max_output_bytes.clamp(1, MAX_VERIFIER_OUTPUT_BYTES);
        let raw_output = bounded_text(&output.raw_output, retained_limit);
        let process_passed = !output.cancelled
            && !output.timed_out
            && output.exit_code == Some(request.expected_exit_code);
        let workspace_mutation_detected = !workspace_changes.changed_files.is_empty();
        let passed = process_passed && !workspace_mutation_detected;
        let status = if output.cancelled {
            ToolResultStatus::Cancelled
        } else if output.timed_out {
            ToolResultStatus::Timeout
        } else if passed {
            ToolResultStatus::Ok
        } else {
            ToolResultStatus::Error
        };
        let command = command_display(
            tool_request.arguments["program"]
                .as_str()
                .unwrap_or_default(),
            tool_request.arguments["args"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        );
        let policy = execution_policy(
            &tool_request,
            PolicyDecision::Allow,
            "caller-declared verifier runs in the workspace sandbox",
        );
        let mut report = report(
            tool_request,
            status,
            if workspace_mutation_detected {
                "external verification modified tracked workspace files"
            } else if passed {
                "external verification passed"
            } else {
                "external verification failed"
            },
            json!({
                "command": command,
                "cwd": cwd,
                "exit_code": output.exit_code,
                "expected_exit_code": request.expected_exit_code,
                "timed_out": output.timed_out,
                "cancelled": output.cancelled,
                "output_truncated": output.output_truncated || output.raw_output.len() > retained_limit,
                "workspace_changes_known": workspace_changes.complete,
                "workspace_change_count": workspace_changes.changed_files.len(),
                "workspace_mutation_detected": workspace_mutation_detected,
                "sandbox_backend": output.sandbox_backend,
                "sandbox_os_enforced": output.sandbox_os_enforced,
                "network_access": output.network_access,
            }),
            raw_output,
            workspace_changes.changed_files.clone(),
            policy,
        );
        report.before_images = workspace_changes.before_images;
        report.after_images = workspace_changes.after_images;
        report.metrics = process_metrics(&output);
        report.envelope.risk = "caller_declared_workspace_verifier".to_owned();
        report.envelope.verification_hint =
            Some("objective test result from a caller-declared command".to_owned());
        Ok(report)
    }

    /// Convert a verifier setup or launch failure into the same durable report
    /// shape as a verifier process result.
    #[must_use]
    pub fn verifier_execution_error_report(
        &self,
        request: VerifierExecutionRequest,
        error: impl Into<String>,
    ) -> ToolExecutionReport {
        let command = command_display(&request.program, request.args.iter().map(String::as_str));
        let tool_request = request.as_tool_request();
        let reason = bounded_text(&error.into(), MAX_TOOL_ERROR_CHARS);
        let policy = execution_policy(
            &tool_request,
            PolicyDecision::Allow,
            "caller-declared verifier failed before producing a process result",
        );
        let mut report = error_report(
            tool_request,
            "external verification could not run",
            json!({
                "command": command,
                "error": reason,
                "exit_code": null,
                "expected_exit_code": request.expected_exit_code,
                "timed_out": false,
                "cancelled": false,
                "workspace_changes_known": false,
                "workspace_change_count": 0,
                "workspace_mutation_detected": false,
                "sandbox_backend": self.sandbox.backend(),
                "sandbox_os_enforced": self.sandbox.os_enforced(),
            }),
            reason,
            policy,
        );
        report.envelope.risk = "caller_declared_workspace_verifier".to_owned();
        report.envelope.verification_hint =
            Some("objective test result from a caller-declared command".to_owned());
        report
    }

    fn resolve_verifier_cwd(
        &self,
        request: &VerifierExecutionRequest,
    ) -> Result<PathBuf, ToolError> {
        let cwd = self
            .policy
            .resolve_path(&request.cwd, true)
            .map_err(|error| {
                ToolError::InvalidArguments(format!("invalid verifier cwd: {error}"))
            })?;
        if (self.policy.mode() != golutra_policy::WorkspacePolicyMode::Unrestricted
            && !cwd.starts_with(self.policy.workspace_root()))
            || !cwd.is_dir()
        {
            return Err(ToolError::InvalidArguments(
                "verifier cwd must be a directory inside the workspace".to_owned(),
            ));
        }
        Ok(cwd)
    }

    pub fn evaluate(&self, request: &ToolRequest) -> Result<PolicyEvaluation, ToolError> {
        if self.replay_backend.is_some() {
            return Ok(execution_policy(
                request,
                PolicyDecision::Allow,
                "deterministic replay injects a recorded tool result",
            ));
        }
        let contract = self
            .registry
            .contract(&request.tool_name)
            .ok_or_else(|| ToolError::UnknownTool(request.tool_name.clone()))?;
        self.registry
            .validate_tool_arguments(contract, &request.arguments)?;

        let builtin = BuiltinTool::from_name(&request.tool_name);
        let mut policy = match builtin {
            Some(BuiltinTool::ReadFile) => self.policy.evaluate_path(
                BuiltinTool::ReadFile.name(),
                string_arg(&request.arguments, "path")?,
                true,
            ),
            Some(BuiltinTool::WriteFile) => self.policy.evaluate_path(
                BuiltinTool::WriteFile.name(),
                string_arg(&request.arguments, "path")?,
                false,
            ),
            Some(BuiltinTool::EditFile) => self.policy.evaluate_path(
                BuiltinTool::EditFile.name(),
                string_arg(&request.arguments, "path")?,
                true,
            ),
            Some(BuiltinTool::ApplyPatch) => {
                self.policy
                    .evaluate_path(BuiltinTool::ApplyPatch.name(), ".", true)
            }
            Some(BuiltinTool::ListDir) => self.policy.evaluate_path(
                BuiltinTool::ListDir.name(),
                optional_string_arg(&request.arguments, "path").unwrap_or_else(|| ".".to_owned()),
                true,
            ),
            Some(BuiltinTool::RgSearch) => self.policy.evaluate_path(
                BuiltinTool::RgSearch.name(),
                optional_string_arg(&request.arguments, "path").unwrap_or_else(|| ".".to_owned()),
                true,
            ),
            Some(BuiltinTool::SymbolSearch | BuiltinTool::FindReferences) => {
                self.policy.evaluate_path(&request.tool_name, ".", true)
            }
            Some(BuiltinTool::Shell) => {
                let shell_command = shell_command_for_request(&request.arguments)?;
                let shell_policy = self.policy.evaluate_shell(&shell_command);
                optional_string_arg(&request.arguments, "workdir")
                    .map(|workdir| {
                        self.policy
                            .evaluate_path(BuiltinTool::Shell.name(), workdir, true)
                    })
                    .filter(|workdir_policy| workdir_policy.decision != PolicyDecision::Allow)
                    .unwrap_or(shell_policy)
            }
            Some(BuiltinTool::WebSearch) => web_search_policy(self.allow_network, request),
            Some(BuiltinTool::ShellSession) => process_control_policy(request),
            Some(BuiltinTool::ProcessList) => process_list_policy(request),
            Some(
                BuiltinTool::ProcessPoll
                | BuiltinTool::ProcessWrite
                | BuiltinTool::ProcessTerminate
                | BuiltinTool::ProcessReconnect,
            ) => process_control_policy(request),
            Some(BuiltinTool::Subagent | BuiltinTool::DelegateTask)
                if self.delegation_backend.is_some() =>
            {
                delegation_policy(request)
            }
            None if self.external_backend.is_some() => {
                let mut policy = execution_policy(
                    request,
                    PolicyDecision::Ask,
                    "external MCP tool execution requires explicit approval",
                );
                policy.resource = format!("external-tool:{}", request.tool_name);
                policy
            }
            Some(BuiltinTool::AskUser | BuiltinTool::Subagent | BuiltinTool::DelegateTask)
            | None => {
                return Err(ToolError::UnknownTool(request.tool_name.clone()));
            }
        };
        if self.policy.mode() == golutra_policy::WorkspacePolicyMode::Unrestricted {
            policy.decision = PolicyDecision::Allow;
            policy.block_disposition = None;
            policy.reason = "unrestricted workspace policy enabled".to_owned();
        }
        policy.resource = bounded_text(
            &redact_sensitive_text(&policy.resource).0,
            MAX_AUDIT_RESOURCE_CHARS,
        );
        Ok(policy)
    }

    pub async fn prepare_side_effect(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<FileBeforeImage>, ToolError> {
        Ok(self
            .prepare_side_effect_snapshot(request)
            .await?
            .before_images)
    }

    pub async fn prepare_side_effect_snapshot(
        &self,
        request: &ToolRequest,
    ) -> Result<SideEffectPreparation, ToolError> {
        if self.replay_backend.is_some() {
            return Ok(SideEffectPreparation::default());
        }
        let contract = self
            .registry
            .contract(&request.tool_name)
            .ok_or_else(|| ToolError::UnknownTool(request.tool_name.clone()))?;
        self.registry
            .validate_tool_arguments(contract, &request.arguments)?;

        match BuiltinTool::from_name(&request.tool_name) {
            Some(BuiltinTool::WriteFile) => {
                let path = string_arg(&request.arguments, "path")?;
                let resolved_path = self.resolve_tool_path("write_file", &path, false)?;
                Ok(SideEffectPreparation {
                    before_images: vec![read_optional_file(&resolved_path).await?],
                    complete: true,
                    workspace_snapshot: None,
                })
            }
            Some(BuiltinTool::EditFile) => {
                let path = string_arg(&request.arguments, "path")?;
                let resolved_path = self.resolve_tool_path("edit_file", &path, true)?;
                let before_image = read_optional_file(&resolved_path).await?;
                if before_image.content.is_none() {
                    return Err(ToolError::Execution(
                        "edit target does not exist".to_owned(),
                    ));
                }
                Ok(SideEffectPreparation {
                    before_images: vec![before_image],
                    complete: true,
                    workspace_snapshot: None,
                })
            }
            Some(BuiltinTool::ApplyPatch) => {
                let patch = string_arg(&request.arguments, "patch")?;
                let paths = self.resolved_patch_paths(&patch).await?;
                let mut before_images = Vec::with_capacity(paths.len());
                let mut retained_bytes = 0_usize;
                for path in paths {
                    let image = read_optional_file(&path).await?;
                    retained_bytes =
                        retained_bytes.saturating_add(file_image_content_bytes(&image));
                    if retained_bytes > MAX_WORKSPACE_SNAPSHOT_CONTENT_BYTES {
                        return Err(ToolError::InvalidArguments(format!(
                            "patch checkpoint exceeds {MAX_WORKSPACE_SNAPSHOT_CONTENT_BYTES} retained bytes"
                        )));
                    }
                    before_images.push(image);
                }
                Ok(SideEffectPreparation {
                    before_images,
                    complete: true,
                    workspace_snapshot: None,
                })
            }
            Some(BuiltinTool::Shell) => {
                if shell_request_is_strictly_read_only(&request.arguments) {
                    return Ok(SideEffectPreparation {
                        before_images: Vec::new(),
                        complete: true,
                        workspace_snapshot: None,
                    });
                }
                let snapshot = workspace_scan::capture(self.policy.workspace_root()).await;
                Ok(SideEffectPreparation {
                    before_images: snapshot.before_images(),
                    complete: snapshot.is_complete(),
                    workspace_snapshot: Some(snapshot),
                })
            }
            Some(BuiltinTool::Subagent | BuiltinTool::DelegateTask) => {
                let snapshot = workspace_scan::capture(self.policy.workspace_root()).await;
                Ok(SideEffectPreparation {
                    before_images: snapshot.before_images(),
                    complete: snapshot.is_complete(),
                    workspace_snapshot: Some(snapshot),
                })
            }
            None if matches!(
                contract.side_effect_type,
                SideEffectType::ExternalSystem | SideEffectType::Network
            ) =>
            {
                Ok(SideEffectPreparation::default())
            }
            _ => Ok(SideEffectPreparation {
                before_images: Vec::new(),
                complete: true,
                workspace_snapshot: None,
            }),
        }
    }

    /// Resolve the workspace identities used by the runtime keyed-write
    /// scheduler. The returned paths use the same policy resolution as the
    /// actual mutation implementation, so lexical aliases and existing
    /// symlink boundaries cannot be scheduled as unrelated writes. An empty
    /// vector means the request is not a keyed mutation and must be treated as
    /// an exclusive operation by the caller.
    pub async fn resolve_keyed_write_paths(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<PathBuf>, ToolError> {
        if !matches!(
            request.tool_name.as_str(),
            "write_file" | "edit_file" | "apply_patch"
        ) {
            return Ok(Vec::new());
        }
        let contract = self
            .registry
            .contract(&request.tool_name)
            .ok_or_else(|| ToolError::UnknownTool(request.tool_name.clone()))?;
        self.registry
            .validate_tool_arguments(contract, &request.arguments)?;
        match request.tool_name.as_str() {
            "write_file" => {
                let path = string_arg(&request.arguments, "path")?;
                Ok(vec![self.resolve_tool_path("write_file", path, false)?])
            }
            "edit_file" => {
                let path = string_arg(&request.arguments, "path")?;
                Ok(vec![self.resolve_tool_path("edit_file", path, true)?])
            }
            "apply_patch" => {
                let patch = string_arg(&request.arguments, "patch")?;
                self.resolved_patch_paths(&patch).await
            }
            _ => Ok(Vec::new()),
        }
    }

    pub async fn execute_with_policy(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        approved: bool,
        cancellation: CancellationToken,
    ) -> Result<ToolExecutionReport, ToolError> {
        self.invoke(
            ToolInvocation::new(request, policy, approved),
            cancellation,
            None,
        )
        .await
    }

    pub async fn execute_with_policy_and_before_images(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        approved: bool,
        cancellation: CancellationToken,
        before_images: Vec<FileBeforeImage>,
    ) -> Result<ToolExecutionReport, ToolError> {
        self.execute_with_policy_and_before_images_with_progress(
            request,
            policy,
            approved,
            cancellation,
            before_images,
            None,
        )
        .await
    }

    pub async fn execute_with_policy_and_before_images_with_progress(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        approved: bool,
        cancellation: CancellationToken,
        before_images: Vec<FileBeforeImage>,
        progress: Option<&mut (dyn FnMut(ToolProgress) + Send)>,
    ) -> Result<ToolExecutionReport, ToolError> {
        self.invoke(
            ToolInvocation::new(request, policy, approved).with_preparation(
                SideEffectPreparation {
                    before_images,
                    complete: true,
                    workspace_snapshot: None,
                },
            ),
            cancellation,
            progress,
        )
        .await
    }

    async fn invoke_prepared(
        &self,
        invocation: ToolInvocation,
        cancellation: CancellationToken,
        mut progress: Option<&mut (dyn FnMut(ToolProgress) + Send)>,
    ) -> Result<ToolExecutionReport, ToolError> {
        let ToolInvocation {
            request,
            policy,
            approved,
            preparation,
            deadline,
            pre_execution_stop,
        } = invocation;
        let Some(preparation) = preparation else {
            return Err(ToolError::Execution(
                "tool invocation was not prepared".to_owned(),
            ));
        };
        let started_at = Instant::now();
        let tool_call_id = request.tool_call_id;
        let tool_name = request.tool_name.clone();
        let SideEffectPreparation {
            before_images,
            workspace_snapshot,
            ..
        } = preparation;
        validate_checkpoint_content_limit(&before_images)?;
        emit_tool_progress(
            &mut progress,
            ToolProgress {
                tool_call_id,
                tool_name: tool_name.clone(),
                phase: ToolProgressPhase::Started,
                elapsed_ms: 0,
                output_bytes: 0,
                output_lines: 0,
                detail: None,
                output_excerpt: None,
            },
        );
        let deadline_request = request.clone();
        let deadline_policy = policy.clone();
        let execution_cancellation = cancellation.child_token();
        let result = {
            let operation = async {
                if let Some(replay_backend) = &self.replay_backend {
                    if execution_cancellation.is_cancelled() {
                        return Ok(cancelled_report_with_policy(
                            request,
                            policy,
                            "tool replay cancelled before result injection",
                        ));
                    }
                    let mut envelope = replay_backend.replay(&request).await?;
                    envelope.tool_call_id = request.tool_call_id;
                    envelope.tool_name = request.tool_name.clone();
                    let output_bytes = envelope
                        .model_visible_excerpt
                        .as_deref()
                        .map_or(0, str::len);
                    let output_lines = envelope
                        .model_visible_excerpt
                        .as_deref()
                        .map_or(0, |output| output.lines().count());
                    return Ok(ToolExecutionReport {
                        envelope,
                        artifacts: Vec::new(),
                        evidence: Vec::new(),
                        changed_files: Vec::new(),
                        policy_evaluation: policy,
                        artifact_contents: Vec::new(),
                        before_images: Vec::new(),
                        after_images: Vec::new(),
                        metrics: ToolExecutionMetrics {
                            output_bytes: u64::try_from(output_bytes).unwrap_or(u64::MAX),
                            output_lines: u64::try_from(output_lines).unwrap_or(u64::MAX),
                            ..ToolExecutionMetrics::default()
                        },
                    });
                }
                let contract = self
                    .registry
                    .contract(&request.tool_name)
                    .ok_or_else(|| ToolError::UnknownTool(request.tool_name.clone()))?;
                self.registry
                    .validate_tool_arguments(contract, &request.arguments)?;
                if execution_cancellation.is_cancelled() {
                    return Ok(cancelled_report_with_policy(
                        request,
                        policy,
                        "tool call cancelled before execution",
                    ));
                }
                match policy.decision {
                    PolicyDecision::Allow => {}
                    PolicyDecision::Ask if approved => {}
                    PolicyDecision::Ask => {
                        return Ok(denied_report(
                            request,
                            policy,
                            "tool execution requires approval",
                        ));
                    }
                    PolicyDecision::Deny | PolicyDecision::Block => {
                        return Ok(self.blocked_report_with_hints(request, policy).await);
                    }
                }

                match BuiltinTool::from_name(&request.tool_name) {
                    Some(BuiltinTool::ReadFile) => self.read_file(request, policy).await,
                    Some(BuiltinTool::WriteFile) => {
                        self.write_file(request, policy, before_images).await
                    }
                    Some(BuiltinTool::EditFile) => {
                        self.edit_file(request, policy, before_images).await
                    }
                    Some(BuiltinTool::ApplyPatch) => {
                        self.apply_patch(
                            request,
                            policy,
                            before_images,
                            execution_cancellation.clone(),
                        )
                        .await
                    }
                    Some(BuiltinTool::ListDir) => self.list_dir(request, policy).await,
                    Some(BuiltinTool::RgSearch) => {
                        self.rg_search(
                            request,
                            policy,
                            execution_cancellation.clone(),
                            started_at,
                            &mut progress,
                        )
                        .await
                    }
                    Some(BuiltinTool::SymbolSearch) => {
                        self.symbol_search(request, policy, execution_cancellation.clone())
                            .await
                    }
                    Some(BuiltinTool::FindReferences) => {
                        self.find_references(request, policy, execution_cancellation.clone())
                            .await
                    }
                    Some(BuiltinTool::Shell) => {
                        self.shell(
                            request,
                            policy,
                            execution_cancellation.clone(),
                            workspace_snapshot,
                            started_at,
                            &mut progress,
                        )
                        .await
                    }
                    Some(BuiltinTool::WebSearch) => {
                        self.web_search(request, policy, execution_cancellation.clone())
                            .await
                    }
                    Some(BuiltinTool::ShellSession) => {
                        self.shell_session(
                            request,
                            policy,
                            cancellation.clone(),
                            execution_cancellation.clone(),
                        )
                        .await
                    }
                    Some(BuiltinTool::Subagent | BuiltinTool::DelegateTask) => {
                        self.delegate_task(
                            request,
                            policy,
                            execution_cancellation.clone(),
                            workspace_snapshot,
                        )
                        .await
                    }
                    Some(BuiltinTool::ProcessList) => self.process_list(request, policy).await,
                    Some(BuiltinTool::ProcessPoll) => self.process_poll(request, policy).await,
                    Some(BuiltinTool::ProcessWrite) => self.process_write(request, policy).await,
                    Some(BuiltinTool::ProcessTerminate) => {
                        self.process_terminate(request, policy).await
                    }
                    Some(BuiltinTool::ProcessReconnect) => {
                        self.process_reconnect(request, policy).await
                    }
                    Some(BuiltinTool::AskUser) => Err(ToolError::UnknownTool(
                        BuiltinTool::AskUser.name().to_owned(),
                    )),
                    None => {
                        self.execute_external(request, policy, execution_cancellation.clone())
                            .await
                    }
                }
            };
            match pre_execution_stop {
                Some(ToolInvocationStop::Cancelled) => Ok(cancelled_report_with_policy(
                    deadline_request,
                    deadline_policy,
                    "tool call cancelled during side-effect preparation",
                )),
                Some(ToolInvocationStop::TimedOut) => Ok(self.deadline_exceeded_report(
                    deadline_request,
                    deadline_policy,
                    "side-effect preparation",
                )),
                None => {
                    match await_tool_operation(
                        operation,
                        &cancellation,
                        &execution_cancellation,
                        deadline,
                    )
                    .await
                    {
                        ToolOperationOutcome::Completed(result) => result,
                        ToolOperationOutcome::Cancelled => {
                            execution_cancellation.cancel();
                            Ok(cancelled_report_with_policy(
                                deadline_request,
                                deadline_policy,
                                "tool call cancelled during execution",
                            ))
                        }
                        ToolOperationOutcome::TimedOut(completed) => {
                            execution_cancellation.cancel();
                            match completed {
                                Some(Ok(report)) => Ok(mark_report_deadline_exceeded(report)),
                                Some(Err(error)) => Err(error),
                                None => Ok(self.deadline_exceeded_report(
                                    deadline_request,
                                    deadline_policy,
                                    "execution",
                                )),
                            }
                        }
                    }
                }
            }
        };
        match result {
            Ok(mut report) => {
                report.metrics.duration_ms = elapsed_millis(started_at);
                emit_tool_progress(
                    &mut progress,
                    ToolProgress {
                        tool_call_id,
                        tool_name,
                        phase: ToolProgressPhase::Completed,
                        elapsed_ms: report.metrics.duration_ms,
                        output_bytes: report.metrics.output_bytes,
                        output_lines: report.metrics.output_lines,
                        detail: Some(format!("{:?}", report.envelope.status).to_ascii_lowercase()),
                        output_excerpt: report.envelope.model_visible_excerpt.clone(),
                    },
                );
                Ok(report)
            }
            Err(error) => {
                emit_tool_progress(
                    &mut progress,
                    ToolProgress {
                        tool_call_id,
                        tool_name,
                        phase: ToolProgressPhase::Completed,
                        elapsed_ms: elapsed_millis(started_at),
                        output_bytes: 0,
                        output_lines: 0,
                        detail: Some("error".to_owned()),
                        output_excerpt: None,
                    },
                );
                Err(error)
            }
        }
    }

    #[must_use]
    pub fn invalid_request_report(
        &self,
        request: ToolRequest,
        reason: impl Into<String>,
    ) -> ToolExecutionReport {
        let reason = bounded_text(&reason.into(), MAX_TOOL_ERROR_CHARS);
        let mut policy = execution_policy(&request, PolicyDecision::Block, &reason);
        policy.block_disposition = Some(PolicyBlockDisposition::Recoverable);
        error_report(
            request,
            "tool request is invalid",
            json!({"error": reason}),
            reason,
            policy,
        )
    }

    #[must_use]
    pub fn execution_error_report(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        error: impl Into<String>,
    ) -> ToolExecutionReport {
        let reason = bounded_text(&error.into(), MAX_TOOL_ERROR_CHARS);
        let structured_facts = execution_error_facts(&request, &reason);
        error_report(
            request,
            "tool execution failed",
            structured_facts,
            reason,
            policy,
        )
    }

    /// 在真实执行错误上补充有限的 workspace-relative 候选路径。
    /// 该辅助信息只改变模型可见事实，不改变原始错误、状态或 artifact。
    pub async fn execution_error_report_with_hints(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        error: impl Into<String>,
    ) -> ToolExecutionReport {
        let reason = bounded_text(&error.into(), MAX_TOOL_ERROR_CHARS);
        let structured_facts = self
            .execution_error_facts_with_hints(&request, &reason)
            .await;
        error_report(
            request,
            "tool execution failed",
            structured_facts,
            reason,
            policy,
        )
    }

    async fn blocked_report_with_hints(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
    ) -> ToolExecutionReport {
        let mut report = blocked_report(request.clone(), policy.clone());
        if request.tool_name != "read_file"
            || !read_file_hint_reason(&policy.reason)
            || !self.read_file_hint_request_is_safe(&request)
        {
            return report;
        }
        let model_reason = "path resolution failed for the requested workspace path".to_owned();
        report.envelope.summary = model_reason.clone();
        let Value::Object(facts) = &mut report.envelope.structured_facts else {
            return report;
        };
        facts.insert("reason".to_owned(), Value::String(model_reason));
        if let Some(path) = request.arguments.get("path").and_then(Value::as_str) {
            facts.insert(
                "path".to_owned(),
                Value::String(bounded_text(
                    &redact_sensitive_text(path).0,
                    MAX_PATH_ARGUMENT_CHARS,
                )),
            );
        }
        facts.insert(
            "path_resolution".to_owned(),
            Value::String(read_file_path_resolution(&policy.reason).to_owned()),
        );
        if let Some(candidates) = self
            .read_file_path_candidates(&request, &policy.reason)
            .await
        {
            facts.insert("candidates".to_owned(), json!(candidates));
            facts.insert(
                "next_action".to_owned(),
                Value::String("retry read_file with one of the candidate paths".to_owned()),
            );
        } else {
            facts.insert(
                "next_action".to_owned(),
                Value::String(
                    "run one bounded read-only shell discovery such as rg --files, then read the exact file path"
                        .to_owned(),
                ),
            );
        }
        report
    }

    async fn execution_error_facts_with_hints(&self, request: &ToolRequest, reason: &str) -> Value {
        let mut facts = execution_error_facts(request, reason);
        let Some(candidates) = self.read_file_path_candidates(request, reason).await else {
            return facts;
        };
        if let Value::Object(object) = &mut facts {
            object.insert("candidates".to_owned(), json!(candidates));
            object.insert(
                "next_action".to_owned(),
                Value::String("retry read_file with one of the candidate paths".to_owned()),
            );
        }
        facts
    }

    async fn read_file_path_candidates(
        &self,
        request: &ToolRequest,
        reason: &str,
    ) -> Option<Vec<String>> {
        if request.tool_name != "read_file" || !read_file_hint_reason(reason) {
            return None;
        }
        let requested_path = request.arguments.get("path").and_then(Value::as_str)?;
        if !self.read_file_hint_request_is_safe(request) {
            return None;
        }
        let basename = Path::new(requested_path)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty() && *name != "." && *name != "..")?;
        let workspace_root = self.policy.workspace_root().to_path_buf();
        let mut pending = vec![(workspace_root.clone(), 0_usize)];
        let mut visited_entries = 0_usize;
        let mut candidates = BTreeSet::new();

        while let Some((directory, depth)) = pending.pop() {
            if visited_entries >= MAX_READ_FILE_HINT_ENTRIES {
                break;
            }
            let mut reader = match tokio::fs::read_dir(&directory).await {
                Ok(reader) => reader,
                Err(_) => continue,
            };
            let mut entries = Vec::new();
            while visited_entries < MAX_READ_FILE_HINT_ENTRIES {
                match reader.next_entry().await {
                    Ok(Some(entry)) => {
                        visited_entries = visited_entries.saturating_add(1);
                        entries.push(entry);
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            entries.sort_by_key(|entry| entry.file_name().to_string_lossy().into_owned());

            for entry in entries {
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                let file_type = match entry.file_type().await {
                    Ok(file_type) => file_type,
                    Err(_) => continue,
                };
                if file_type.is_dir() {
                    if depth < MAX_READ_FILE_HINT_DEPTH
                        && !read_file_hint_ignored_directory(&file_name)
                    {
                        pending.push((entry.path(), depth.saturating_add(1)));
                    }
                    continue;
                }
                // 不跟随符号链接，避免候选提示跨越 workspace 边界或暴露宿主路径。
                if !file_type.is_file() || file_type.is_symlink() || file_name != basename {
                    continue;
                }
                let path = entry.path();
                let Some(relative) = path.strip_prefix(&workspace_root).ok() else {
                    continue;
                };
                let candidate_policy = self.policy.evaluate_path("read_file", relative, true);
                if candidate_policy.decision != PolicyDecision::Allow {
                    continue;
                }
                let relative = relative.to_string_lossy().replace('\\', "/");
                if !relative.is_empty() {
                    candidates.insert(bounded_text(&relative, MAX_PATH_ARGUMENT_CHARS));
                }
            }
        }

        (!candidates.is_empty()).then(|| {
            candidates
                .into_iter()
                .take(MAX_READ_FILE_HINT_CANDIDATES)
                .collect()
        })
    }

    fn read_file_hint_request_is_safe(&self, request: &ToolRequest) -> bool {
        let Some(requested_path) = request.arguments.get("path").and_then(Value::as_str) else {
            return false;
        };
        let resolved_request = self
            .policy
            .evaluate_path("read_file", requested_path, false);
        resolved_request.decision == PolicyDecision::Allow
            && Path::new(&resolved_request.resource).starts_with(self.policy.workspace_root())
    }

    /// Build the terminal report used when an enclosing runtime deadline wins.
    /// The report is data rather than an error so hosts can persist a balanced
    /// tool completion even when no tool implementation reached its own timeout.
    #[must_use]
    pub fn deadline_exceeded_report(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        stage: &str,
    ) -> ToolExecutionReport {
        let workspace_changes_known = self
            .registry
            .contract(&request.tool_name)
            .is_some_and(|contract| contract.side_effect_type == SideEffectType::None);
        let mut result = report(
            request,
            ToolResultStatus::Timeout,
            "tool call exceeded its enclosing runtime deadline",
            json!({
                "timed_out": true,
                "deadline_stage": stage,
                "workspace_changes_known": workspace_changes_known,
            }),
            String::new(),
            Vec::new(),
            policy,
        );
        match BuiltinTool::from_name(&result.envelope.tool_name) {
            Some(BuiltinTool::Subagent | BuiltinTool::DelegateTask) => {
                result.envelope.risk = "delegated_agent".to_owned()
            }
            None => result.envelope.risk = "external_mcp_tool".to_owned(),
            _ => {}
        }
        result
    }

    /// Build a terminal cancellation report while retaining the evaluated
    /// policy that authorized or rejected the original request.
    #[must_use]
    pub fn cancelled_execution_report(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        reason: &str,
    ) -> ToolExecutionReport {
        cancelled_report_with_policy(request, policy, reason)
    }

    #[must_use]
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    async fn read_file(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
    ) -> Result<ToolExecutionReport, ToolError> {
        let path = string_arg(&request.arguments, "path")?;
        let resolved_path = self.resolve_tool_path("read_file", &path, true)?;
        let content = read_optional_file(&resolved_path)
            .await?
            .content
            .ok_or_else(|| ToolError::Execution("read target does not exist".to_owned()))?;
        let content =
            String::from_utf8(content).map_err(|error| ToolError::Execution(error.to_string()))?;
        let total_bytes = content.len();
        let total_lines = output_line_count(&content);
        let content_digest = checksum(content.as_bytes());
        let offset = read_offset(&request.arguments);
        let limit = read_limit(&request.arguments);
        let window = select_read_window(&content, offset, limit, total_lines);
        let mut structured_facts = json!({
            // Provider 可见事实使用调用方给出的 workspace 相对路径。解析后的
            // 绝对路径仍保留为持久执行事实，但每轮重复会浪费 token，并暴露
            // 对模型决策没有价值的宿主路径前缀。
            "path": path,
            "resolved_path": resolved_path,
            "content_digest": content_digest,
            "bytes": window.content.len(),
            "lines": window.lines,
            "offset": offset,
            "limit": limit,
            "total_bytes": total_bytes,
            "total_lines": total_lines,
            "has_more": window.has_more,
            "truncated": window.has_more,
            "eof": !window.has_more,
        });
        if window.has_more {
            structured_facts["continuation"] = json!({
                "next_cursor": window.next_offset,
                "next_offset": window.next_offset,
            });
        }
        Ok(with_item_count(
            success_report(
                request,
                "file read",
                structured_facts,
                window.content,
                Vec::new(),
                policy,
            ),
            1,
        ))
    }

    async fn write_file(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        before_images: Vec<FileBeforeImage>,
    ) -> Result<ToolExecutionReport, ToolError> {
        let path = string_arg(&request.arguments, "path")?;
        let content = string_arg(&request.arguments, "content")?;
        if content.len() as u64 > MAX_FILE_CONTENT_BYTES {
            return Err(ToolError::InvalidArguments(format!(
                "write content exceeds {MAX_FILE_CONTENT_BYTES} byte limit"
            )));
        }
        let resolved_path = self.resolve_tool_path("write_file", &path, false)?;
        if !before_image_still_current(&resolved_path, &before_images).await? {
            return Ok(error_report(
                request,
                "write target changed after checkpoint",
                json!({"path": resolved_path, "conflict": true}),
                String::new(),
                policy,
            ));
        }
        let existing_content = before_images
            .iter()
            .find(|image| image.path == resolved_path)
            .and_then(|image| image.content.as_deref());
        if existing_content == Some(content.as_bytes()) {
            let digest = checksum(content.as_bytes());
            let mut report = success_report(
                request,
                "file already contains requested content",
                json!({
                    "path": resolved_path,
                    "bytes": content.len(),
                    "content_digest": digest,
                    "changed": false,
                    "idempotent": true,
                    "workspace_changes_known": true,
                }),
                String::new(),
                Vec::new(),
                policy,
            );
            report.before_images = before_images.clone();
            report.after_images = before_images;
            report.metrics.item_count = Some(1);
            return Ok(report);
        }
        if let Err(error) = tokio::fs::write(&resolved_path, content.as_bytes()).await {
            let parent = resolved_path.parent().map(PathBuf::from);
            let parent_missing = match parent.as_deref() {
                Some(parent) => match tokio::fs::metadata(parent).await {
                    Ok(metadata) => !metadata.is_dir(),
                    Err(_) => true,
                },
                None => false,
            };
            let reason = error.to_string();
            return Ok(error_report(
                request,
                "file write failed",
                json!({
                    "path": resolved_path,
                    "parent": parent,
                    "parent_missing": parent_missing,
                    "error_kind": io_error_kind_name(error.kind()),
                    "errno": error.raw_os_error(),
                    "error": reason,
                    "action_required": if parent_missing {
                        "create_parent_directory_explicitly"
                    } else {
                        "inspect_write_error"
                    },
                    "workspace_changes_known": true,
                }),
                reason,
                policy,
            ));
        }
        let mut report = success_report(
            request,
            "file written",
            json!({
                "path": resolved_path,
                "bytes": content.len(),
                "content_digest": checksum(content.as_bytes()),
                "changed": true,
                "workspace_changes_known": true,
            }),
            content.clone(),
            vec![resolved_path.clone()],
            policy,
        );
        report.before_images = before_images;
        report.after_images = vec![FileBeforeImage {
            path: resolved_path.clone(),
            content: Some(content.as_bytes().to_vec()),
            unix_mode: report
                .before_images
                .first()
                .and_then(|image| image.unix_mode),
            metadata: Some(file_state_metadata(
                content.as_bytes(),
                report
                    .before_images
                    .first()
                    .and_then(|image| image.unix_mode),
                true,
            )),
        }];
        attach_model_visible_change_preview(&mut report, self.policy.workspace_root());
        report.metrics.item_count = Some(1);
        Ok(report)
    }

    async fn edit_file(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        before_images: Vec<FileBeforeImage>,
    ) -> Result<ToolExecutionReport, ToolError> {
        let path = string_arg(&request.arguments, "path")?;
        let edits = parse_file_edits(&request.arguments)?;
        let resolved_path = self.resolve_tool_path("edit_file", &path, true)?;
        if !before_image_still_current(&resolved_path, &before_images).await? {
            return Ok(error_report(
                request,
                "edit target changed after checkpoint",
                json!({"path": resolved_path, "conflict": true}),
                String::new(),
                policy,
            ));
        }
        let original_bytes = if let Some(before_image) = before_images
            .iter()
            .find(|before_image| before_image.path == resolved_path)
        {
            before_image
                .content
                .clone()
                .ok_or_else(|| ToolError::Execution("edit target does not exist".to_owned()))?
        } else {
            tokio::fs::read(&resolved_path)
                .await
                .map_err(|error| ToolError::Execution(error.to_string()))?
        };
        let original = String::from_utf8(original_bytes)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let (edited, replacement_count) = match apply_file_edits(&original, &edits) {
            Ok(result) => result,
            Err(failure) => {
                return Ok(error_report(
                    request,
                    &failure.summary,
                    json!({
                        "path": resolved_path,
                        "edit_index": failure.edit_index,
                        "search_found": failure.search_found,
                        "overlap": failure.overlap,
                        "replacements": 0,
                    }),
                    original,
                    policy,
                ));
            }
        };
        if edited.len() as u64 > MAX_FILE_CONTENT_BYTES {
            return Ok(error_report(
                request,
                "edited content exceeds file size limit",
                json!({
                    "path": resolved_path,
                    "max_bytes": MAX_FILE_CONTENT_BYTES,
                    "replacements": replacement_count,
                }),
                String::new(),
                policy,
            ));
        }
        if let Err(error) = tokio::fs::write(&resolved_path, edited.as_bytes()).await {
            let parent = resolved_path.parent().map(PathBuf::from);
            let parent_missing = match parent.as_deref() {
                Some(parent) => match tokio::fs::metadata(parent).await {
                    Ok(metadata) => !metadata.is_dir(),
                    Err(_) => true,
                },
                None => false,
            };
            let reason = error.to_string();
            return Ok(error_report(
                request,
                "file edit write failed",
                json!({
                    "path": resolved_path,
                    "parent": parent,
                    "parent_missing": parent_missing,
                    "error_kind": io_error_kind_name(error.kind()),
                    "errno": error.raw_os_error(),
                    "error": reason,
                    "action_required": if parent_missing {
                        "create_parent_directory_explicitly"
                    } else {
                        "inspect_write_error"
                    },
                    "workspace_changes_known": true,
                }),
                reason,
                policy,
            ));
        }
        let mut report = success_report(
            request,
            "file edited",
            json!({
                "path": resolved_path,
                "replacements": replacement_count,
                "edit_count": edits.len(),
                "bytes": edited.len(),
                "content_digest": checksum(edited.as_bytes()),
                "changed": true,
                "workspace_changes_known": true,
            }),
            edited.clone(),
            vec![resolved_path.clone()],
            policy,
        );
        report.before_images = before_images;
        report.after_images = vec![FileBeforeImage {
            path: resolved_path.clone(),
            content: Some(edited.as_bytes().to_vec()),
            unix_mode: report
                .before_images
                .first()
                .and_then(|image| image.unix_mode),
            metadata: Some(file_state_metadata(
                edited.as_bytes(),
                report
                    .before_images
                    .first()
                    .and_then(|image| image.unix_mode),
                true,
            )),
        }];
        attach_model_visible_change_preview(&mut report, self.policy.workspace_root());
        report.metrics.item_count = Some(1);
        Ok(report)
    }

    async fn patch_paths(&self, patch: &str) -> Result<Vec<PathBuf>, ToolError> {
        validate_patch_input(patch)?;
        if model_patch::looks_like_model_patch(patch) {
            return model_patch::parse(patch)
                .map(|parsed| {
                    parsed
                        .files
                        .into_iter()
                        .flat_map(|file| {
                            std::iter::once(file.path)
                                .chain(file.move_path)
                                .collect::<Vec<_>>()
                        })
                        .collect()
                })
                .map_err(ToolError::InvalidArguments);
        }
        let mut args = vec!["apply".to_owned()];
        if self.policy.mode() == golutra_policy::WorkspacePolicyMode::Unrestricted {
            args.push("--unsafe-paths".to_owned());
        }
        args.extend([
            "--numstat".to_owned(),
            "-z".to_owned(),
            "--whitespace=nowarn".to_owned(),
            "-".to_owned(),
        ]);
        let output = self
            .run_patch_command(
                &args,
                patch,
                WorkspaceAccess::ReadOnly,
                CancellationToken::new(),
            )
            .await?;
        if output.exit_code != Some(0) || output.stdin_error.is_some() {
            return Err(ToolError::InvalidArguments(format!(
                "patch is invalid: {}",
                bounded_text(
                    output.stdin_error.as_deref().unwrap_or(&output.raw_output),
                    MAX_TOOL_ERROR_CHARS,
                )
            )));
        }
        parse_git_numstat_paths(&output.raw_output, output.output_truncated)
    }

    async fn resolved_patch_paths(&self, patch: &str) -> Result<Vec<PathBuf>, ToolError> {
        self.patch_paths(patch)
            .await?
            .into_iter()
            .map(|path| self.resolve_tool_path("apply_patch", path, false))
            .collect::<Result<BTreeSet<_>, _>>()
            .map(|paths| paths.into_iter().collect())
    }

    async fn apply_patch(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        before_images: Vec<FileBeforeImage>,
        cancellation: CancellationToken,
    ) -> Result<ToolExecutionReport, ToolError> {
        let patch = string_arg(&request.arguments, "patch")?;
        validate_patch_input(&patch)?;
        let checkpointed_paths = before_images
            .iter()
            .map(|image| image.path.clone())
            .collect::<BTreeSet<_>>();
        let resolved_paths = self
            .resolved_patch_paths(&patch)
            .await?
            .into_iter()
            .collect::<BTreeSet<_>>();
        if resolved_paths != checkpointed_paths {
            let mut report = error_report(
                request,
                "patch target paths changed after checkpoint",
                json!({
                    "conflict": true,
                    "checkpointed_paths": checkpointed_paths,
                    "resolved_paths": resolved_paths,
                }),
                String::new(),
                policy,
            );
            report.before_images = before_images;
            return Ok(report);
        }
        for before_image in &before_images {
            if !before_image_still_current(&before_image.path, &before_images).await? {
                return Ok(error_report(
                    request,
                    "patch target changed after checkpoint",
                    json!({"path": before_image.path, "conflict": true}),
                    String::new(),
                    policy,
                ));
            }
        }
        let patch = self.normalize_model_patch(&patch).await?;
        let mut args = vec!["apply".to_owned()];
        if self.policy.mode() == golutra_policy::WorkspacePolicyMode::Unrestricted {
            args.push("--unsafe-paths".to_owned());
        }
        args.extend(["--whitespace=nowarn".to_owned(), "-".to_owned()]);
        let output = self
            .run_patch_command(&args, &patch, WorkspaceAccess::ReadWrite, cancellation)
            .await?;
        let (changed_files, after_images) = file_changes_since(&before_images).await?;
        let changed_count = changed_files.len();
        if output.cancelled {
            let mut report = report(
                request,
                ToolResultStatus::Cancelled,
                "patch application cancelled",
                json!({
                    "cancelled": true,
                    "workspace_changes_known": true,
                    "workspace_change_count": changed_count,
                    "workspace_mutation_detected": changed_count > 0,
                    "changed_files": &changed_files,
                }),
                bounded_text(&output.raw_output, MAX_TOOL_ERROR_CHARS),
                changed_files,
                policy,
            );
            report.before_images = before_images;
            report.after_images = after_images;
            report.metrics = process_metrics(&output);
            return Ok(report);
        }
        if output.timed_out || output.exit_code != Some(0) || output.stdin_error.is_some() {
            let mut report = report(
                request,
                ToolResultStatus::Error,
                "patch could not be applied atomically",
                json!({
                    "exit_code": output.exit_code,
                    "timed_out": output.timed_out,
                    "stdin_error": &output.stdin_error,
                    "workspace_changes_known": true,
                    "workspace_change_count": changed_count,
                    "workspace_mutation_detected": changed_count > 0,
                    "changed_files": &changed_files,
                }),
                bounded_text(&output.raw_output, MAX_TOOL_ERROR_CHARS),
                changed_files,
                policy,
            );
            report.before_images = before_images;
            report.after_images = after_images;
            report.metrics = process_metrics(&output);
            return Ok(report);
        }

        let summary = format!("patch applied to {changed_count} file(s)");
        let mut report = success_report(
            request,
            "patch applied",
            json!({
                "changed_files": &changed_files,
                "changed_file_count": changed_count,
                "patch_digest": checksum(patch.as_bytes()),
                "workspace_changes_known": true,
            }),
            summary,
            changed_files,
            policy,
        );
        report.before_images = before_images;
        report.after_images = after_images;
        attach_model_visible_change_preview(&mut report, self.policy.workspace_root());
        report.metrics = process_metrics(&output);
        report.metrics.item_count = Some(u64::try_from(changed_count).unwrap_or(u64::MAX));
        Ok(report)
    }

    async fn normalize_model_patch(&self, patch: &str) -> Result<String, ToolError> {
        if !model_patch::looks_like_model_patch(patch) {
            return Ok(patch.to_owned());
        }
        let parsed = model_patch::parse(patch).map_err(ToolError::InvalidArguments)?;
        let mut originals = BTreeMap::new();
        for file in &parsed.files {
            let source_requires_existing = matches!(
                file.kind,
                model_patch::ModelPatchFileKind::Update(_)
                    | model_patch::ModelPatchFileKind::Delete
            );
            let source_path =
                self.resolve_tool_path("apply_patch", &file.path, source_requires_existing)?;
            match tokio::fs::symlink_metadata(&source_path).await {
                Ok(_) if source_requires_existing => {
                    let content = tokio::fs::read(&source_path)
                        .await
                        .map_err(|error| ToolError::Execution(error.to_string()))?;
                    originals.insert(file.path.clone(), content);
                }
                Ok(_) => {
                    originals.insert(file.path.clone(), Vec::new());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if source_requires_existing {
                        return Err(ToolError::InvalidArguments(format!(
                            "patch target does not exist: {}",
                            file.path.display()
                        )));
                    }
                }
                Err(error) => return Err(ToolError::Execution(error.to_string())),
            }
            if let Some(move_path) = &file.move_path {
                let destination_path = self.resolve_tool_path("apply_patch", move_path, false)?;
                match tokio::fs::symlink_metadata(&destination_path).await {
                    Ok(_) => {
                        originals.insert(move_path.clone(), Vec::new());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(ToolError::Execution(format!(
                            "could not inspect patch move destination `{}`: {error}",
                            move_path.display()
                        )));
                    }
                }
            }
        }
        model_patch::render(&parsed, &originals).map_err(ToolError::InvalidArguments)
    }

    async fn run_patch_command(
        &self,
        args: &[String],
        patch: &str,
        workspace_access: WorkspaceAccess,
        cancellation: CancellationToken,
    ) -> Result<process::ShellOutput, ToolError> {
        run_process_with_progress(
            ProcessExecutionRequest {
                program: "git",
                args,
                cwd: self.policy.workspace_root(),
                workspace_root: self.policy.workspace_root(),
                timeout_ms: 30_000,
                cancellation,
                sandbox: &self.sandbox,
                workspace_access,
                allow_network: false,
                stdin: Some(patch.as_bytes()),
                isolated_home: true,
            },
            None,
        )
        .await
    }

    async fn list_dir(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
    ) -> Result<ToolExecutionReport, ToolError> {
        let path =
            optional_string_arg(&request.arguments, "path").unwrap_or_else(|| ".".to_owned());
        let resolved_path = self.resolve_tool_path("list_dir", &path, true)?;
        let entries = directory_entries(&resolved_path).await?;
        let entry_count = u64::try_from(entries.len()).unwrap_or(u64::MAX);
        Ok(with_item_count(
            success_report(
                request,
                "directory listed",
                json!({"path": resolved_path, "entries": entries, "entry_count": entry_count}),
                entries.join("\n"),
                Vec::new(),
                policy,
            ),
            entry_count,
        ))
    }

    async fn rg_search(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        cancellation: CancellationToken,
        started_at: Instant,
        progress: &mut Option<&mut (dyn FnMut(ToolProgress) + Send)>,
    ) -> Result<ToolExecutionReport, ToolError> {
        let pattern = string_arg(&request.arguments, "pattern")?;
        let path =
            optional_string_arg(&request.arguments, "path").unwrap_or_else(|| ".".to_owned());
        let resolved_path = self.resolve_tool_path("rg_search", &path, true)?;
        let tool_call_id = request.tool_call_id;
        let tool_name = request.tool_name.clone();
        let mut process_progress = |process: ProcessProgress| {
            emit_process_progress(progress, tool_call_id, &tool_name, started_at, process);
        };
        let fallback_cancellation = cancellation.clone();
        let output = run_process_with_progress(
            ProcessExecutionRequest {
                program: "rg",
                args: &[
                    "--line-number".to_owned(),
                    "--no-heading".to_owned(),
                    "--".to_owned(),
                    pattern.clone(),
                    resolved_path.display().to_string(),
                ],
                cwd: self.policy.workspace_root(),
                workspace_root: self.policy.workspace_root(),
                timeout_ms: DEFAULT_TIMEOUT_MS,
                cancellation,
                sandbox: &self.sandbox,
                workspace_access: WorkspaceAccess::ReadOnly,
                allow_network: false,
                stdin: None,
                isolated_home: false,
            },
            Some(&mut process_progress),
        )
        .await;
        let output = match output {
            Ok(output) => output,
            Err(error) if error.to_string().contains("No such file or directory") => {
                return self
                    .native_rg_search(
                        request,
                        policy,
                        pattern,
                        resolved_path,
                        fallback_cancellation,
                        started_at,
                    )
                    .await;
            }
            Err(error) => return Err(error),
        };
        let redacted_pattern = redact_sensitive_text(&pattern).0;
        let status = if output.cancelled {
            ToolResultStatus::Cancelled
        } else if output.timed_out {
            ToolResultStatus::Timeout
        } else if output.exit_code == Some(0) || output.exit_code == Some(1) {
            ToolResultStatus::Ok
        } else {
            ToolResultStatus::Error
        };
        let mut metrics = process_metrics(&output);
        metrics.match_count = Some(output.output_lines);
        Ok(with_metrics(
            report(
                request,
                status,
                "rg search completed",
                json!({
                    "path": resolved_path,
                    "pattern": redacted_pattern,
                    "exit_code": output.exit_code,
                    "matches": output.output_lines,
                    "output_bytes": output.output_bytes,
                    "output_lines": output.output_lines,
                    "output_truncated": output.output_truncated,
                    "timed_out": output.timed_out,
                    "cancelled": output.cancelled,
                    "sandbox_backend": output.sandbox_backend,
                    "sandbox_os_enforced": output.sandbox_os_enforced,
                    "network_access": output.network_access,
                }),
                output.raw_output,
                Vec::new(),
                policy,
            ),
            metrics,
        ))
    }

    async fn native_rg_search(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        pattern: String,
        resolved_path: PathBuf,
        cancellation: CancellationToken,
        started_at: Instant,
    ) -> Result<ToolExecutionReport, ToolError> {
        let workspace_root = self.policy.workspace_root().to_path_buf();
        let root_for_search = resolved_path.clone();
        let pattern_for_search = pattern.clone();
        let result = tokio::task::spawn_blocking(move || {
            text_search::search(
                &pattern_for_search,
                root_for_search,
                workspace_root,
                DEFAULT_TIMEOUT_MS,
                cancellation,
            )
        })
        .await
        .map_err(|error| ToolError::Execution(format!("native search task failed: {error}")))?
        .map_err(ToolError::Execution)?;
        let redacted_pattern = redact_sensitive_text(&pattern).0;
        let status = if result.cancelled {
            ToolResultStatus::Cancelled
        } else if result.timed_out {
            ToolResultStatus::Timeout
        } else {
            ToolResultStatus::Ok
        };
        let mut report = report(
            request,
            status,
            "rg search completed with native workspace fallback",
            json!({
                "path": resolved_path,
                "pattern": redacted_pattern,
                "exit_code": if result.matches > 0 { 0 } else { 1 },
                "matches": result.matches,
                "scanned_files": result.scanned_files,
                "output_bytes": result.output_bytes,
                "output_lines": result.matches,
                "output_truncated": result.output_truncated,
                "scan_truncated": result.scan_truncated,
                "timed_out": result.timed_out,
                "cancelled": result.cancelled,
                "native_fallback": true,
                    "sandbox_backend": self.sandbox.backend(),
                    "sandbox_os_enforced": self.sandbox.os_enforced(),
                    "network_access": false,
            }),
            result.output,
            Vec::new(),
            policy,
        );
        report.metrics = ToolExecutionMetrics {
            duration_ms: started_at
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            output_bytes: result.output_bytes,
            output_lines: result.matches,
            output_truncated: result.output_truncated,
            exit_code: Some(if result.matches > 0 { 0 } else { 1 }),
            match_count: Some(result.matches),
            ..ToolExecutionMetrics::default()
        };
        report.envelope.verification_hint =
            Some("native bounded workspace search evidence".to_owned());
        Ok(report)
    }

    async fn symbol_search(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        cancellation: CancellationToken,
    ) -> Result<ToolExecutionReport, ToolError> {
        let query = string_arg(&request.arguments, "query")?;
        let limit = bounded_query_limit(&request.arguments);
        let graph = self.build_code_graph(cancellation).await?;
        let result =
            golutra_code_intelligence::CodeIntelligence::query_symbols(&graph, &query, limit);
        let match_count = u64::try_from(result.matches.len()).unwrap_or(u64::MAX);
        let output = serde_json::to_string_pretty(&result)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        Ok(with_match_count(
            success_report(
                request,
                "symbol search completed",
                json!({"query": query, "matches": result.matches.len()}),
                output,
                Vec::new(),
                policy,
            ),
            match_count,
        ))
    }

    async fn find_references(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        cancellation: CancellationToken,
    ) -> Result<ToolExecutionReport, ToolError> {
        let symbol_name = string_arg(&request.arguments, "symbol")?;
        let limit = bounded_query_limit(&request.arguments);
        let graph = self.build_code_graph(cancellation).await?;
        let result = golutra_code_intelligence::CodeIntelligence::query_references(
            &graph,
            &symbol_name,
            limit,
        );
        let reference_count = u64::try_from(result.references.len()).unwrap_or(u64::MAX);
        let output = serde_json::to_string_pretty(&result)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        Ok(with_match_count(
            success_report(
                request,
                "reference search completed",
                json!({"symbol": symbol_name, "references": result.references.len()}),
                output,
                Vec::new(),
                policy,
            ),
            reference_count,
        ))
    }

    async fn build_code_graph(
        &self,
        cancellation: CancellationToken,
    ) -> Result<golutra_code_intelligence::CodeGraph, ToolError> {
        if cancellation.is_cancelled() {
            return Err(ToolError::Execution(
                "code intelligence query was cancelled".to_owned(),
            ));
        }
        let permit = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(ToolError::Execution(
                    "code intelligence query was cancelled".to_owned(),
                ));
            }
            permit = CODE_INDEX_BUILD_PERMITS.clone().acquire_owned() => {
                permit.map_err(|error| ToolError::Execution(error.to_string()))?
            }
        };
        let workspace_root = self.policy.workspace_root().to_path_buf();
        let graph = tokio::task::spawn_blocking(move || {
            // The permit lives inside the blocking task. Cancelling the async
            // waiter cannot admit another expensive index build before this
            // worker has actually stopped.
            let _permit = permit;
            golutra_code_intelligence::CodeIntelligence::new(workspace_root)?.build()
        })
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?
        .map_err(|error| ToolError::Execution(error.to_string()))?;
        if cancellation.is_cancelled() {
            return Err(ToolError::Execution(
                "code intelligence query was cancelled".to_owned(),
            ));
        }
        Ok(graph)
    }

    async fn shell(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        cancellation: CancellationToken,
        workspace_before: Option<workspace_scan::WorkspaceSnapshot>,
        started_at: Instant,
        progress: &mut Option<&mut (dyn FnMut(ToolProgress) + Send)>,
    ) -> Result<ToolExecutionReport, ToolError> {
        let command = shell_command_for_request(&request.arguments)?;
        let strictly_read_only = shell_request_is_strictly_read_only(&request.arguments);
        let background = request
            .arguments
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let timeout_ms = request
            .arguments
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(if background {
                60 * 60 * 1_000
            } else {
                DEFAULT_TIMEOUT_MS
            });
        let effective_timeout_ms = effective_shell_timeout(timeout_ms);
        let command_line = CommandLine::parse_for_execution(
            &command,
            self.policy.mode() == golutra_policy::WorkspacePolicyMode::Unrestricted,
        )?;
        let cwd = match optional_string_arg(&request.arguments, "workdir") {
            Some(workdir) => self.resolve_tool_path("shell", workdir, true)?,
            None => self.policy.workspace_root().to_path_buf(),
        };
        if !cwd.is_dir() {
            return Err(ToolError::InvalidArguments(format!(
                "shell workdir is not a directory: {}",
                cwd.display()
            )));
        }
        if background && command_line.stdin.is_some() {
            return Err(ToolError::InvalidArguments(
                "quoted Python heredocs are supported only for foreground commands".to_owned(),
            ));
        }
        let workspace_before = if strictly_read_only {
            None
        } else {
            Some(match workspace_before {
                Some(snapshot) => snapshot,
                None => workspace_scan::capture(self.policy.workspace_root()).await,
            })
        };
        if background {
            let wait_ms = request
                .arguments
                .get("yield_time_ms")
                .and_then(Value::as_u64)
                .unwrap_or_else(default_start_wait_ms)
                .min(max_poll_wait_ms());
            let process_id = format!("proc-{}", request.tool_call_id);
            let snapshot = self
                .process_supervisor
                .start(ProcessStartRequest {
                    process_id,
                    session_id: request.session_id,
                    program: &command_line.program,
                    args: &command_line.args,
                    command_display: redact_sensitive_text(&command).0,
                    cwd: &cwd,
                    workspace_root: self.policy.workspace_root(),
                    timeout_ms: effective_timeout_ms,
                    wait_ms,
                    cancellation,
                    sandbox: &self.sandbox,
                    workspace_access: WorkspaceAccess::ReadWrite,
                    allow_network: self.allow_network,
                    workspace_before: workspace_before
                        .expect("background shell calls have a workspace baseline"),
                })
                .await?;
            return Ok(supervised_process_report(request, policy, snapshot));
        }
        let tool_call_id = request.tool_call_id;
        let tool_name = request.tool_name.clone();
        let shell_output = {
            let mut process_progress = |process: ProcessProgress| {
                emit_process_progress(progress, tool_call_id, &tool_name, started_at, process);
            };
            run_process_with_progress(
                ProcessExecutionRequest {
                    program: &command_line.program,
                    args: &command_line.args,
                    cwd: &cwd,
                    workspace_root: self.policy.workspace_root(),
                    timeout_ms: effective_timeout_ms,
                    cancellation,
                    sandbox: &self.sandbox,
                    workspace_access: if strictly_read_only {
                        WorkspaceAccess::ReadOnly
                    } else {
                        WorkspaceAccess::ReadWrite
                    },
                    allow_network: self.allow_network,
                    stdin: command_line.stdin.as_deref(),
                    isolated_home: false,
                },
                Some(&mut process_progress),
            )
            .await?
        };
        let workspace_changes = if strictly_read_only {
            workspace_scan::WorkspaceMutationScan {
                complete: true,
                ..workspace_scan::WorkspaceMutationScan::default()
            }
        } else {
            workspace_scan::compare(
                self.policy.workspace_root(),
                workspace_before.expect("mutable shell calls have a workspace baseline"),
            )
            .await
        };
        let status = if shell_output.cancelled {
            ToolResultStatus::Cancelled
        } else if shell_output.timed_out {
            ToolResultStatus::Timeout
        } else if shell_output.exit_code == Some(0) {
            ToolResultStatus::Ok
        } else {
            ToolResultStatus::Error
        };
        let redacted_command = redact_sensitive_text(&command).0;
        let metrics = process_metrics(&shell_output);
        let mut report = with_metrics(
            report(
                request,
                status,
                "shell command completed",
                json!({
                    "command": redacted_command,
                    "requested_timeout_ms": timeout_ms,
                    "effective_timeout_ms": effective_timeout_ms,
                    "exit_code": shell_output.exit_code,
                    "timed_out": shell_output.timed_out,
                    "cancelled": shell_output.cancelled,
                    "output_bytes": shell_output.output_bytes,
                    "output_lines": shell_output.output_lines,
                    "output_truncated": shell_output.output_truncated,
                    "workspace_changes_known": workspace_changes.complete,
                    "workspace_change_count": workspace_changes.changed_files.len(),
                    "sandbox_backend": shell_output.sandbox_backend,
                    "sandbox_os_enforced": shell_output.sandbox_os_enforced,
                    "network_access": shell_output.network_access,
                }),
                shell_output.raw_output,
                workspace_changes.changed_files.clone(),
                policy,
            ),
            metrics,
        );
        report.before_images = workspace_changes.before_images;
        report.after_images = workspace_changes.after_images;
        Ok(report)
    }

    async fn delegate_task(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        cancellation: CancellationToken,
        workspace_before: Option<workspace_scan::WorkspaceSnapshot>,
    ) -> Result<ToolExecutionReport, ToolError> {
        let backend = self
            .delegation_backend
            .as_ref()
            .ok_or_else(|| ToolError::UnknownTool("subagent".to_owned()))?;
        let output = backend
            .delegate(&request, cancellation.child_token())
            .await?;
        let workspace_changes = match workspace_before {
            Some(snapshot) => workspace_scan::compare(self.policy.workspace_root(), snapshot).await,
            None => {
                workspace_scan::compare(
                    self.policy.workspace_root(),
                    workspace_scan::capture(self.policy.workspace_root()).await,
                )
                .await
            }
        };
        let mut structured_facts = match output.structured_facts {
            Value::Object(facts) => Value::Object(facts),
            value => json!({"child_result": value}),
        };
        if let Some(facts) = structured_facts.as_object_mut() {
            facts.insert(
                "workspace_changes_known".to_owned(),
                Value::Bool(workspace_changes.complete),
            );
            facts.insert(
                "workspace_change_count".to_owned(),
                json!(workspace_changes.changed_files.len()),
            );
        }
        let mut report = report(
            request,
            output.status,
            &output.summary,
            structured_facts,
            output.content,
            if workspace_changes.complete {
                workspace_changes.changed_files.clone()
            } else {
                Vec::new()
            },
            policy,
        );
        report.envelope.risk = "delegated_agent".to_owned();
        report.envelope.verification_hint =
            Some("delegated child result and enclosing workspace diff".to_owned());
        if workspace_changes.complete {
            report.before_images = workspace_changes.before_images;
            report.after_images = workspace_changes.after_images;
        }
        Ok(report)
    }

    async fn process_list(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
    ) -> Result<ToolExecutionReport, ToolError> {
        let summaries = self.process_supervisor.list(request.session_id).await;
        let running_count = summaries
            .iter()
            .filter(|summary| summary.state == ProcessState::Running)
            .count();
        let processes = summaries
            .into_iter()
            .map(process_summary_value)
            .collect::<Vec<_>>();
        let process_count = processes.len();
        let output = serde_json::to_string_pretty(&processes)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        Ok(with_item_count(
            success_report(
                request,
                "managed processes listed",
                json!({
                    "process_count": process_count,
                    "running_count": running_count,
                    "processes": processes,
                }),
                output,
                Vec::new(),
                policy,
            ),
            u64::try_from(process_count).unwrap_or(u64::MAX),
        ))
    }

    async fn process_poll(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
    ) -> Result<ToolExecutionReport, ToolError> {
        let process_id = string_arg(&request.arguments, "process_id")?;
        self.validate_authoritative_pid(&request, &process_id)
            .await?;
        let cursor = process_cursor(&request.arguments);
        let wait_ms = process_wait_ms(&request.arguments, default_poll_wait_ms());
        let snapshot = self
            .process_supervisor
            .poll(request.session_id, &process_id, cursor, wait_ms)
            .await?;
        Ok(supervised_process_report(request, policy, snapshot))
    }

    async fn process_reconnect(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
    ) -> Result<ToolExecutionReport, ToolError> {
        let process_id = string_arg(&request.arguments, "process_id")?;
        self.validate_authoritative_pid(&request, &process_id)
            .await?;
        let cursor = process_cursor(&request.arguments);
        let snapshot = self
            .process_supervisor
            .reconnect(request.session_id, &process_id, cursor)
            .await?;
        Ok(supervised_process_report(request, policy, snapshot))
    }

    async fn process_write(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
    ) -> Result<ToolExecutionReport, ToolError> {
        let process_id = string_arg(&request.arguments, "process_id")?;
        self.validate_authoritative_pid(&request, &process_id)
            .await?;
        let input = string_arg(&request.arguments, "input")?;
        let cursor = process_cursor(&request.arguments);
        let wait_ms = process_wait_ms(&request.arguments, 250);
        let snapshot = self
            .process_supervisor
            .write(request.session_id, &process_id, &input, cursor, wait_ms)
            .await?;
        Ok(supervised_process_report(request, policy, snapshot))
    }

    async fn process_terminate(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
    ) -> Result<ToolExecutionReport, ToolError> {
        let process_id = string_arg(&request.arguments, "process_id")?;
        self.validate_authoritative_pid(&request, &process_id)
            .await?;
        let cursor = process_cursor(&request.arguments);
        let snapshot = self
            .process_supervisor
            .terminate(request.session_id, &process_id, cursor)
            .await?;
        Ok(supervised_process_report(request, policy, snapshot))
    }

    async fn shell_session(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        cancellation: CancellationToken,
        execution_cancellation: CancellationToken,
    ) -> Result<ToolExecutionReport, ToolError> {
        let action = string_arg(&request.arguments, "action")?;
        let process_id = string_arg(&request.arguments, "process_id")?;
        let authoritative_pid =
            process_authoritative_pid(&request.arguments)?.ok_or_else(|| {
                ToolError::InvalidArguments(
                    "shell_session requires authoritative_pid from the start response".to_owned(),
                )
            })?;
        self.process_supervisor
            .validate_authoritative_pid(request.session_id, &process_id, authoritative_pid)
            .await?;
        let cursor = process_cursor(&request.arguments);
        let snapshot = match action.as_str() {
            "wait" => {
                let wait_ms = process_wait_ms(&request.arguments, default_poll_wait_ms());
                let wait_for_terminal = request
                    .arguments
                    .get("wait_for_terminal")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if wait_for_terminal {
                    let result = self
                        .process_supervisor
                        .wait_until_terminal(
                            request.session_id,
                            &process_id,
                            cursor,
                            wait_ms,
                            &execution_cancellation,
                        )
                        .await?;
                    if result.cancelled {
                        if cancellation.is_cancelled() {
                            return Ok(cancelled_report_with_policy(
                                request,
                                policy,
                                "background terminal wait was cancelled",
                            ));
                        }
                        return Ok(self.deadline_exceeded_report(
                            request,
                            policy,
                            "background terminal wait",
                        ));
                    }
                    result.snapshot
                } else {
                    self.process_supervisor
                        .poll(request.session_id, &process_id, cursor, wait_ms)
                        .await?
                }
            }
            "write" => {
                let input = string_arg(&request.arguments, "input")?;
                let wait_ms = process_wait_ms(&request.arguments, 250);
                self.process_supervisor
                    .write(request.session_id, &process_id, &input, cursor, wait_ms)
                    .await?
            }
            "terminate" => {
                self.process_supervisor
                    .terminate(request.session_id, &process_id, cursor)
                    .await?
            }
            _ => {
                return Err(ToolError::InvalidArguments(
                    "shell_session action must be wait, write, or terminate".to_owned(),
                ));
            }
        };
        Ok(supervised_process_report(request, policy, snapshot))
    }

    async fn validate_authoritative_pid(
        &self,
        request: &ToolRequest,
        process_id: &str,
    ) -> Result<(), ToolError> {
        let Some(authoritative_pid) = process_authoritative_pid(&request.arguments)? else {
            return Ok(());
        };
        self.process_supervisor
            .validate_authoritative_pid(request.session_id, process_id, authoritative_pid)
            .await
    }

    async fn web_search(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        cancellation: CancellationToken,
    ) -> Result<ToolExecutionReport, ToolError> {
        let backend = self.web_search_backend.as_ref().ok_or_else(|| {
            ToolError::Execution("web search backend is not configured".to_owned())
        })?;
        let result = tokio::select! {
            () = cancellation.cancelled() => {
                return Ok(cancelled_report_with_policy(
                    request,
                    policy,
                    "web search cancelled",
                ));
            }
            result = backend.search(&request, cancellation.clone()) => result,
        }?;
        let status = if result.is_error {
            ToolResultStatus::Error
        } else {
            ToolResultStatus::Ok
        };
        let mut report = external_report(
            request,
            status,
            &result.summary,
            result.structured_facts,
            result.content,
            policy,
        );
        // 搜索结果已经在 structured_facts 中按字段保留，模型摘要不再重复回显原始 JSON。
        report.envelope.model_visible_excerpt = Some(result.summary.clone());
        report.envelope.risk = "web_search".to_owned();
        report.envelope.verification_hint =
            Some("web search output is external, time-sensitive evidence".to_owned());
        Ok(report)
    }

    async fn execute_external(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        cancellation: CancellationToken,
    ) -> Result<ToolExecutionReport, ToolError> {
        let backend = self
            .external_backend
            .as_ref()
            .ok_or_else(|| ToolError::UnknownTool(request.tool_name.clone()))?;
        let call = backend.call(&request, cancellation.clone());
        let result = tokio::select! {
            () = cancellation.cancelled() => {
                return Ok(external_report(
                    request,
                    ToolResultStatus::Cancelled,
                    "external tool call cancelled",
                    json!({"cancelled": true, "workspace_changes_known": false}),
                    String::new(),
                    policy,
                ));
            }
            result = tokio::time::timeout(
                Duration::from_millis(EXTERNAL_TOOL_TIMEOUT_MS),
                call,
            ) => result,
        };

        match result {
            Ok(Ok(output)) => Ok(external_report(
                request,
                if output.is_error {
                    ToolResultStatus::Error
                } else {
                    ToolResultStatus::Ok
                },
                &output.summary,
                mark_workspace_changes_unknown(output.structured_facts),
                output.content,
                policy,
            )),
            Ok(Err(error)) => {
                let error = bounded_text(&error.to_string(), MAX_TOOL_ERROR_CHARS);
                Ok(external_report(
                    request,
                    ToolResultStatus::Error,
                    "external tool execution failed",
                    json!({"error": error, "workspace_changes_known": false}),
                    error,
                    policy,
                ))
            }
            Err(_) => Ok(external_report(
                request,
                ToolResultStatus::Timeout,
                "external tool call timed out",
                json!({
                    "timed_out": true,
                    "timeout_ms": EXTERNAL_TOOL_TIMEOUT_MS,
                    "workspace_changes_known": false
                }),
                String::new(),
                policy,
            )),
        }
    }

    fn resolve_tool_path(
        &self,
        action: &str,
        path: impl AsRef<Path>,
        requires_existing_path: bool,
    ) -> Result<PathBuf, ToolError> {
        let evaluation = self
            .policy
            .evaluate_path(action, path, requires_existing_path);
        if evaluation.decision != PolicyDecision::Allow {
            return Err(ToolError::Execution(format!(
                "path policy rejected tool execution: {}",
                evaluation.reason
            )));
        }
        Ok(PathBuf::from(evaluation.resource))
    }
}

const fn effective_shell_timeout(requested_timeout_ms: u64) -> u64 {
    if requested_timeout_ms > MAX_BACKGROUND_PROCESS_TIMEOUT_MS {
        MAX_BACKGROUND_PROCESS_TIMEOUT_MS
    } else {
        requested_timeout_ms
    }
}

/// Compatibility name for integrations compiled against the original tool
/// executor. New runtime code should use [`ToolRuntime`] and [`ToolInvocation`].
pub type BasicToolExecutor = ToolRuntime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifierExecutionRequest {
    pub tool_call_id: ToolCallId,
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub timeout_ms: u64,
    pub expected_exit_code: i32,
    pub max_output_bytes: usize,
}

impl VerifierExecutionRequest {
    #[must_use]
    pub fn as_tool_request(&self) -> ToolRequest {
        ToolRequest {
            tool_call_id: self.tool_call_id,
            provider_tool_call_id: None,
            session_id: self.session_id,
            turn_id: self.turn_id,
            tool_name: "external_verifier".to_owned(),
            arguments: json!({
                "program": self.program,
                "args": self.args,
                "cwd": self.cwd,
                "timeout_ms": self.timeout_ms.clamp(1, MAX_VERIFIER_TIMEOUT_MS),
                "expected_exit_code": self.expected_exit_code,
            }),
        }
    }
}

fn command_display<'a>(program: &'a str, args: impl Iterator<Item = &'a str>) -> String {
    std::iter::once(program)
        .chain(args)
        .map(|part| shlex::try_quote(part).map_or_else(|_| part.to_owned(), Into::into))
        .collect::<Vec<_>>()
        .join(" ")
}

fn validate_patch_input(patch: &str) -> Result<(), ToolError> {
    if patch.trim().is_empty() {
        return Err(ToolError::InvalidArguments(
            "patch cannot be empty".to_owned(),
        ));
    }
    if patch.len() > MAX_PATCH_BYTES {
        return Err(ToolError::InvalidArguments(format!(
            "patch exceeds {MAX_PATCH_BYTES} bytes"
        )));
    }
    if patch.contains('\0') {
        return Err(ToolError::InvalidArguments(
            "patch cannot contain NUL bytes".to_owned(),
        ));
    }
    Ok(())
}

fn parse_git_numstat_paths(
    output: &str,
    output_truncated: bool,
) -> Result<Vec<PathBuf>, ToolError> {
    if output_truncated {
        return Err(ToolError::InvalidArguments(
            "patch touches too many paths to checkpoint safely".to_owned(),
        ));
    }
    let records = output.split('\0').collect::<Vec<_>>();
    let mut paths = BTreeSet::new();
    let mut index = 0_usize;
    while index < records.len() {
        let record = records[index];
        index = index.saturating_add(1);
        if record.is_empty() {
            continue;
        }
        let mut fields = record.splitn(3, '\t');
        let added = fields.next();
        let deleted = fields.next();
        let path = fields.next();
        if !added.is_some_and(is_git_numstat_count)
            || !deleted.is_some_and(is_git_numstat_count)
            || path.is_none()
        {
            // The bounded process collector merges stderr after stdout. Ignore
            // diagnostics that are not NUL-delimited numstat records.
            continue;
        }
        let path = path.unwrap_or_default();
        if path.is_empty() {
            let old_path = records.get(index).copied().unwrap_or_default();
            let new_path = records
                .get(index.saturating_add(1))
                .copied()
                .unwrap_or_default();
            if old_path.is_empty() || new_path.is_empty() {
                return Err(ToolError::InvalidArguments(
                    "git returned incomplete rename metadata".to_owned(),
                ));
            }
            paths.insert(PathBuf::from(old_path));
            paths.insert(PathBuf::from(new_path));
            index = index.saturating_add(2);
        } else {
            paths.insert(PathBuf::from(path));
        }
    }
    if paths.is_empty() {
        return Err(ToolError::InvalidArguments(
            "patch does not contain any file changes".to_owned(),
        ));
    }
    Ok(paths.into_iter().collect())
}

fn is_git_numstat_count(value: &str) -> bool {
    value == "-" || (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
}

async fn directory_entries(path: &Path) -> Result<Vec<String>, ToolError> {
    let mut directory = tokio::fs::read_dir(path)
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let mut entries = Vec::new();
    let mut output_bytes = 0_usize;
    while let Some(entry) = directory
        .next_entry()
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?
    {
        let name = entry.file_name().to_string_lossy().to_string();
        let separator_bytes = usize::from(!entries.is_empty());
        if entries.len() >= MAX_DIRECTORY_ENTRIES
            || output_bytes
                .saturating_add(separator_bytes)
                .saturating_add(name.len())
                > MAX_DIRECTORY_OUTPUT_BYTES
        {
            entries.push(format!(
                "[directory listing truncated at {} entries / {MAX_DIRECTORY_OUTPUT_BYTES} bytes]",
                entries.len()
            ));
            break;
        }
        output_bytes = output_bytes
            .saturating_add(separator_bytes)
            .saturating_add(name.len());
        entries.push(name);
    }
    entries.sort();
    Ok(entries)
}

fn bounded_query_limit(arguments: &Value) -> usize {
    arguments
        .get("limit")
        .and_then(Value::as_u64)
        .and_then(|limit| usize::try_from(limit).ok())
        .unwrap_or(20)
        .clamp(1, 100)
}

#[derive(Debug)]
struct ReadWindow {
    content: String,
    lines: u64,
    next_offset: usize,
    has_more: bool,
}

fn read_offset(arguments: &Value) -> usize {
    arguments
        .get("offset")
        .and_then(Value::as_u64)
        .and_then(|offset| usize::try_from(offset).ok())
        .filter(|offset| *offset > 0)
        .unwrap_or(1)
}

fn read_limit(arguments: &Value) -> usize {
    arguments
        .get("limit")
        .and_then(Value::as_u64)
        .and_then(|limit| usize::try_from(limit).ok())
        .unwrap_or(DEFAULT_READ_MAX_LINES)
        .clamp(1, MAX_READ_LINES)
}

/// 选择稳定且可按行定位的窗口。默认值有意设置为有界，模型只需文件头部时，
/// 不必为整个生成文件或 vendor 文件付出输入 token。
fn select_read_window(content: &str, offset: usize, limit: usize, total_lines: u64) -> ReadWindow {
    if content.is_empty() {
        return ReadWindow {
            content: String::new(),
            lines: 0,
            next_offset: offset,
            has_more: false,
        };
    }

    let start = offset.saturating_sub(1);
    let total_lines_usize = usize::try_from(total_lines).unwrap_or(usize::MAX);
    let end = start.saturating_add(limit).min(total_lines_usize);
    let mut selected = String::new();
    let mut selected_lines = 0_usize;
    for (index, line) in content.split_inclusive('\n').enumerate() {
        if index < start {
            continue;
        }
        if index >= end {
            break;
        }
        selected.push_str(line);
        selected_lines = selected_lines.saturating_add(1);
    }
    ReadWindow {
        lines: u64::try_from(selected_lines).unwrap_or(u64::MAX),
        content: selected,
        next_offset: end.saturating_add(1),
        has_more: end < total_lines_usize,
    }
}

#[derive(Debug, Clone)]
struct FileEdit {
    old_text: String,
    new_text: String,
}

#[derive(Debug)]
struct EditFailure {
    summary: String,
    edit_index: usize,
    search_found: bool,
    overlap: bool,
}

fn parse_file_edits(arguments: &Value) -> Result<Vec<FileEdit>, ToolError> {
    let edits = arguments
        .get("edits")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::InvalidArguments("edits must be a non-empty array".to_owned()))?;
    if edits.is_empty() || edits.len() > MAX_FILE_EDITS {
        return Err(ToolError::InvalidArguments(format!(
            "edits must contain between 1 and {MAX_FILE_EDITS} replacements"
        )));
    }
    edits
        .iter()
        .enumerate()
        .map(|(index, edit)| {
            let old_text = edit
                .get("old_text")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ToolError::InvalidArguments(format!(
                        "edits[{index}].old_text must be a non-empty string"
                    ))
                })?;
            if old_text.is_empty() {
                return Err(ToolError::InvalidArguments(format!(
                    "edits[{index}].old_text must be a non-empty string"
                )));
            }
            let new_text = edit
                .get("new_text")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ToolError::InvalidArguments(format!("edits[{index}].new_text must be a string"))
                })?;
            if old_text.len() as u64 > MAX_FILE_CONTENT_BYTES
                || new_text.len() as u64 > MAX_FILE_CONTENT_BYTES
            {
                return Err(ToolError::InvalidArguments(format!(
                    "edits[{index}] exceeds the file content limit"
                )));
            }
            Ok(FileEdit {
                old_text: old_text.to_owned(),
                new_text: new_text.to_owned(),
            })
        })
        .collect()
}

fn apply_file_edits(original: &str, edits: &[FileEdit]) -> Result<(String, usize), EditFailure> {
    let mut matches = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let mut occurrences = original.match_indices(&edit.old_text);
        let Some((start, matched)) = occurrences.next() else {
            return Err(EditFailure {
                summary: format!("edit target not found at edits[{index}]"),
                edit_index: index,
                search_found: false,
                overlap: false,
            });
        };
        if occurrences.next().is_some() {
            return Err(EditFailure {
                summary: format!("edit target is not unique at edits[{index}]"),
                edit_index: index,
                search_found: true,
                overlap: false,
            });
        }
        matches.push((start, start + matched.len(), index));
    }

    matches.sort_by_key(|(start, _, _)| *start);
    for pair in matches.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(EditFailure {
                summary: "file edits overlap; merge the affected text into one edit".to_owned(),
                edit_index: pair[1].2,
                search_found: true,
                overlap: true,
            });
        }
    }

    let mut output = String::with_capacity(original.len());
    let mut cursor = 0;
    for (start, end, index) in matches {
        output.push_str(&original[cursor..start]);
        output.push_str(&edits[index].new_text);
        cursor = end;
    }
    output.push_str(&original[cursor..]);
    Ok((output, edits.len()))
}

fn success_report(
    request: ToolRequest,
    summary: &str,
    structured_facts: Value,
    raw_output: String,
    changed_files: Vec<PathBuf>,
    policy_evaluation: PolicyEvaluation,
) -> ToolExecutionReport {
    report(
        request,
        ToolResultStatus::Ok,
        summary,
        structured_facts,
        raw_output,
        changed_files,
        policy_evaluation,
    )
}

fn error_report(
    request: ToolRequest,
    summary: &str,
    structured_facts: Value,
    raw_output: String,
    policy_evaluation: PolicyEvaluation,
) -> ToolExecutionReport {
    report(
        request,
        ToolResultStatus::Error,
        summary,
        structured_facts,
        raw_output,
        Vec::new(),
        policy_evaluation,
    )
}

/// 解析失败时只向模型补充下一步可执行的事实，不自动改写请求路径。
/// 原始错误仍完整进入 artifact，避免为了减少回合而隐藏权限或路径边界。
fn execution_error_facts(request: &ToolRequest, reason: &str) -> Value {
    let mut facts = serde_json::Map::new();
    facts.insert("error".to_owned(), Value::String(reason.to_owned()));
    if mutation_tool_name(&request.tool_name) && reason.contains("checkpoint") {
        add_checkpoint_error_facts(request, reason, &mut facts);
    }
    if request.tool_name == "read_file"
        && let Some(path) = request.arguments.get("path").and_then(Value::as_str)
    {
        facts.insert(
            "path".to_owned(),
            Value::String(bounded_text(path, MAX_PATH_ARGUMENT_CHARS)),
        );
        facts.insert(
            "path_resolution".to_owned(),
            Value::String(read_file_path_resolution(reason).to_owned()),
        );
        facts.insert(
            "next_action".to_owned(),
            Value::String(
                "run one bounded read-only shell discovery such as rg --files, then read the exact file path"
                    .to_owned(),
            ),
        );
    }
    Value::Object(facts)
}

/// 将 checkpoint 失败转换为有限、可执行的模型事实。这里只描述下一步所需的
/// 路径关系，不尝试修复目录或重放请求；完整 provider 错误仍保存在 artifact。
fn add_checkpoint_error_facts(
    request: &ToolRequest,
    reason: &str,
    facts: &mut serde_json::Map<String, Value>,
) {
    let Some(path) = request.arguments.get("path").and_then(Value::as_str) else {
        facts.insert(
            "error_kind".to_owned(),
            Value::String("checkpoint_io".to_owned()),
        );
        facts.insert(
            "action_required".to_owned(),
            Value::String("inspect_checkpoint_error".to_owned()),
        );
        return;
    };

    let (path, _) = redact_sensitive_text(path);
    facts.insert(
        "path".to_owned(),
        Value::String(bounded_text(&path, MAX_PATH_ARGUMENT_CHARS)),
    );
    if let Some(parent) = Path::new(&path).parent()
        && !parent.as_os_str().is_empty()
        && parent != Path::new(".")
    {
        facts.insert(
            "parent".to_owned(),
            Value::String(bounded_text(
                &parent.to_string_lossy(),
                MAX_PATH_ARGUMENT_CHARS,
            )),
        );
    }

    let parent_missing = checkpoint_error_mentions_missing_path(reason);
    facts.insert("parent_missing".to_owned(), Value::Bool(parent_missing));
    facts.insert(
        "error_kind".to_owned(),
        Value::String(
            if parent_missing {
                "checkpoint_parent_missing"
            } else {
                "checkpoint_io"
            }
            .to_owned(),
        ),
    );
    facts.insert(
        "action_required".to_owned(),
        Value::String(
            if parent_missing {
                "create_parent_directory_explicitly"
            } else {
                "inspect_checkpoint_error"
            }
            .to_owned(),
        ),
    );
}

fn checkpoint_error_mentions_missing_path(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    lower.contains("no such file or directory")
        || lower.contains("not found")
        || lower.contains("cannot find the path")
}

fn read_file_hint_reason(reason: &str) -> bool {
    reason.contains("read target does not exist")
        || reason.contains("path canonicalization failed")
        || reason.contains("not a regular file")
}

fn read_file_path_resolution(reason: &str) -> &'static str {
    if reason.contains("not a regular file") {
        "directory_or_non_regular_file"
    } else if reason.contains("canonicalization failed") {
        "missing_or_unresolvable"
    } else {
        "validation_failed"
    }
}

fn read_file_hint_ignored_directory(name: &str) -> bool {
    name.starts_with('.')
        || matches!(
            name,
            "target"
                | "node_modules"
                | "dist"
                | "build"
                | ".next"
                | "__pycache__"
                | ".venv"
                | "venv"
        )
}

fn blocked_report(
    request: ToolRequest,
    policy_evaluation: PolicyEvaluation,
) -> ToolExecutionReport {
    let reason = policy_evaluation.reason.clone();
    report(
        request,
        ToolResultStatus::Blocked,
        &reason,
        json!({"blocked": true, "reason": reason}),
        String::new(),
        Vec::new(),
        policy_evaluation,
    )
}

fn denied_report(
    request: ToolRequest,
    policy_evaluation: PolicyEvaluation,
    reason: &str,
) -> ToolExecutionReport {
    report(
        request,
        ToolResultStatus::Blocked,
        reason,
        json!({"blocked": true, "reason": reason}),
        String::new(),
        Vec::new(),
        policy_evaluation,
    )
}

fn cancelled_report_with_policy(
    request: ToolRequest,
    policy_evaluation: PolicyEvaluation,
    reason: &str,
) -> ToolExecutionReport {
    report(
        request,
        ToolResultStatus::Cancelled,
        reason,
        json!({"cancelled": true}),
        String::new(),
        Vec::new(),
        policy_evaluation,
    )
}

fn mark_report_deadline_exceeded(mut report: ToolExecutionReport) -> ToolExecutionReport {
    report.envelope.status = ToolResultStatus::Timeout;
    report.envelope.summary = "tool call completed after its enclosing runtime deadline".to_owned();
    let workspace_changes_known = report
        .envelope
        .structured_facts
        .get("workspace_changes_known")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let facts = match report.envelope.structured_facts {
        Value::Object(mut facts) => {
            facts.insert("timed_out".to_owned(), Value::Bool(true));
            facts.insert(
                "deadline_stage".to_owned(),
                Value::String("execution".to_owned()),
            );
            facts.insert(
                "completed_during_deadline_cleanup".to_owned(),
                Value::Bool(true),
            );
            facts.insert(
                "workspace_changes_known".to_owned(),
                Value::Bool(workspace_changes_known),
            );
            Value::Object(facts)
        }
        value => json!({
            "result": value,
            "timed_out": true,
            "deadline_stage": "execution",
            "completed_during_deadline_cleanup": true,
            "workspace_changes_known": workspace_changes_known,
        }),
    };
    report.envelope.structured_facts = facts;
    report
}

async fn await_tool_operation<F>(
    operation: F,
    cancellation: &CancellationToken,
    operation_cancellation: &CancellationToken,
    deadline: Option<tokio::time::Instant>,
) -> ToolOperationOutcome<F::Output>
where
    F: std::future::Future,
{
    tokio::pin!(operation);
    match deadline {
        Some(deadline) => {
            tokio::select! {
                biased;
                output = &mut operation => ToolOperationOutcome::Completed(output),
                _ = tokio::time::sleep_until(deadline) => {
                    operation_cancellation.cancel();
                    let completed = tokio::time::timeout(
                        DEADLINE_CLEANUP_TIMEOUT,
                        &mut operation,
                    )
                    .await
                    .ok();
                    ToolOperationOutcome::TimedOut(completed)
                }
                _ = cancellation.cancelled() => {
                    operation_cancellation.cancel();
                    match tokio::time::timeout(
                        DEADLINE_CLEANUP_TIMEOUT,
                        &mut operation,
                    )
                    .await
                    {
                        Ok(output) => ToolOperationOutcome::Completed(output),
                        Err(_) => ToolOperationOutcome::Cancelled,
                    }
                }
            }
        }
        None => {
            tokio::select! {
                biased;
                output = &mut operation => ToolOperationOutcome::Completed(output),
                _ = cancellation.cancelled() => {
                    operation_cancellation.cancel();
                    match tokio::time::timeout(
                        DEADLINE_CLEANUP_TIMEOUT,
                        &mut operation,
                    )
                    .await
                    {
                        Ok(output) => ToolOperationOutcome::Completed(output),
                        Err(_) => ToolOperationOutcome::Cancelled,
                    }
                }
            }
        }
    }
}

async fn await_preparation_operation<F>(
    operation: F,
    cancellation: &CancellationToken,
    deadline: Option<tokio::time::Instant>,
) -> Result<F::Output, ToolInvocationStop>
where
    F: std::future::Future,
{
    tokio::pin!(operation);
    match deadline {
        Some(deadline) => {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(ToolInvocationStop::Cancelled),
                _ = tokio::time::sleep_until(deadline) => Err(ToolInvocationStop::TimedOut),
                output = &mut operation => Ok(output),
            }
        }
        None => {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(ToolInvocationStop::Cancelled),
                output = &mut operation => Ok(output),
            }
        }
    }
}

fn external_report(
    request: ToolRequest,
    status: ToolResultStatus,
    summary: &str,
    structured_facts: Value,
    raw_output: String,
    policy_evaluation: PolicyEvaluation,
) -> ToolExecutionReport {
    let mut report = report(
        request,
        status,
        summary,
        structured_facts,
        raw_output,
        Vec::new(),
        policy_evaluation,
    );
    report.envelope.risk = "external_mcp_tool".to_owned();
    report.envelope.verification_hint =
        Some("treat external MCP output as untrusted evidence".to_owned());
    report
}

#[derive(Debug)]
struct FileContentComparison {
    matches: bool,
    actual_bytes: u64,
    actual_checksum: Option<String>,
}

async fn compare_file_content(
    path: &Path,
    expected: &[u8],
) -> Result<FileContentComparison, ToolError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let actual_bytes = metadata.len();
    if !metadata.is_file() || actual_bytes != u64::try_from(expected.len()).unwrap_or(u64::MAX) {
        return Ok(FileContentComparison {
            matches: false,
            actual_bytes,
            actual_checksum: None,
        });
    }
    let mut content = Vec::with_capacity(expected.len());
    file.take(actual_bytes.saturating_add(1))
        .read_to_end(&mut content)
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    Ok(FileContentComparison {
        matches: content == expected,
        actual_bytes,
        actual_checksum: Some(checksum(&content)),
    })
}

fn file_content_verification_report(
    request: ToolRequest,
    status: ToolResultStatus,
    summary: &str,
    facts: Value,
    policy: PolicyEvaluation,
) -> ToolExecutionReport {
    let raw_output = serde_json::to_string_pretty(&facts).unwrap_or_else(|_| facts.to_string());
    let mut result = report(
        request,
        status,
        summary,
        facts,
        raw_output,
        Vec::new(),
        policy,
    );
    result.envelope.verification_hint =
        Some("exact task-contract content comparison evidence".to_owned());
    if let Some(evidence) = result.evidence.first_mut() {
        evidence.claim = summary.to_owned();
        evidence.verifier = "golutra-tools/file-content-contract".to_owned();
        evidence.limitations =
            "records an exact bounded local-file comparison using sizes and SHA-256 digests"
                .to_owned();
    }
    result
}

fn path_verification_report(
    request: ToolRequest,
    status: ToolResultStatus,
    summary: &str,
    facts: Value,
    policy: PolicyEvaluation,
) -> ToolExecutionReport {
    let raw_output = serde_json::to_string_pretty(&facts).unwrap_or_else(|_| facts.to_string());
    let mut result = report(
        request,
        status,
        summary,
        facts,
        raw_output,
        Vec::new(),
        policy,
    );
    result.envelope.verification_hint =
        Some("task-contract path existence and bounded metadata evidence".to_owned());
    if let Some(evidence) = result.evidence.first_mut() {
        evidence.claim = summary.to_owned();
        evidence.verifier = "golutra-tools/path-contract".to_owned();
        evidence.limitations =
            "records path existence and bounded metadata without reading file contents or traversing directories"
                .to_owned();
    }
    result
}

fn report(
    request: ToolRequest,
    status: ToolResultStatus,
    summary: &str,
    structured_facts: Value,
    raw_output: String,
    changed_files: Vec<PathBuf>,
    policy_evaluation: PolicyEvaluation,
) -> ToolExecutionReport {
    let (redacted_output, redaction_status) = redact_sensitive_text(&raw_output);
    let redacted_summary = redact_sensitive_text(summary).0;
    let mut structured_facts = structured_facts;
    if request.tool_name == "read_file"
        && redacted_output.len() > DEFAULT_READ_EXCERPT_BYTES
        && let Value::Object(facts) = &mut structured_facts
    {
        // 请求的行窗口可能完整，但 provider 可见摘录仍受字节上限约束；显式
        // 保留这个区别，避免模型把被截断的行误判为 EOF。
        facts.insert("model_visible_truncated".to_owned(), Value::Bool(true));
        facts.insert(
            "model_visible_bytes".to_owned(),
            Value::Number((DEFAULT_READ_EXCERPT_BYTES as u64).into()),
        );
    }
    let structured_facts = redact_sensitive_value(structured_facts);
    let artifact = artifact_for(&request, &redacted_output, redaction_status);
    let evidence = EvidenceRecord {
        evidence_id: golutra_core::EvidenceId::new(),
        claim: format!("tool {} finished with {status:?}", request.tool_name),
        artifact_refs: vec![artifact.artifact_id],
        source_event_refs: Vec::new(),
        evidence_strength: match status {
            ToolResultStatus::Ok => EvidenceStrength::Medium,
            ToolResultStatus::Error
            | ToolResultStatus::Blocked
            | ToolResultStatus::Cancelled
            | ToolResultStatus::Timeout => EvidenceStrength::Weak,
        },
        verifier: "golutra-tools".to_owned(),
        confidence: 0.8,
        limitations: "P0 tool evidence records local execution facts only".to_owned(),
    };
    let model_visible_excerpt = if mutation_tool_name(&request.tool_name) {
        // 文件副作用的完整内容已经保存在 artifact 中；再次回显会让每个
        // 后续 turn 重复支付相同 token，模型只需状态摘要和结构化 digest。
        Some(redacted_summary.clone())
    } else if request.tool_name == "read_file" {
        Some(bounded_text_bytes(
            &redacted_output,
            DEFAULT_READ_EXCERPT_BYTES,
        ))
    } else {
        Some(excerpt(&redacted_output, DEFAULT_EXCERPT_LIMIT))
    };
    let envelope = ToolResultEnvelope {
        tool_call_id: request.tool_call_id,
        tool_name: request.tool_name,
        status,
        summary: redacted_summary,
        structured_facts,
        model_visible_excerpt,
        raw_artifact_ref: Some(artifact.artifact_id),
        evidence_refs: vec![evidence.evidence_id],
        risk: "p0_local_tool".to_owned(),
        verification_hint: Some("use artifact/evidence refs for verification".to_owned()),
    };
    let artifact_id = artifact.artifact_id;
    let output_bytes = u64::try_from(redacted_output.len()).unwrap_or(u64::MAX);
    let output_lines = output_line_count(&redacted_output);

    ToolExecutionReport {
        envelope,
        artifacts: vec![artifact],
        evidence: vec![evidence],
        changed_files,
        policy_evaluation,
        artifact_contents: vec![ArtifactContent {
            artifact_id,
            bytes: redacted_output.into_bytes(),
        }],
        before_images: Vec::new(),
        after_images: Vec::new(),
        metrics: ToolExecutionMetrics {
            output_bytes,
            output_lines,
            ..ToolExecutionMetrics::default()
        },
    }
}

fn mutation_tool_name(tool_name: &str) -> bool {
    matches!(
        BuiltinTool::from_name(tool_name),
        Some(BuiltinTool::WriteFile | BuiltinTool::EditFile | BuiltinTool::ApplyPatch)
    )
}

/// 为成功文件变更补充精简的 provider 可见说明。
///
/// 完整 diff 仍保存在持久变更 artifact 中。这里利用工具报告尚未释放的前后镜像
/// 生成有界预览，让下一轮无需重复读取即可决策。缺失文件按空内容处理；不可用
/// 或二进制镜像不生成预览，避免猜测内容。
fn attach_model_visible_change_preview(report: &mut ToolExecutionReport, workspace_root: &Path) {
    if report.envelope.status != ToolResultStatus::Ok
        || !mutation_tool_name(&report.envelope.tool_name)
        || report.changed_files.is_empty()
    {
        return;
    }

    let mut previews = Vec::new();
    let mut retained_bytes = 0_usize;
    for path in report
        .changed_files
        .iter()
        .take(MAX_MODEL_CHANGE_PREVIEW_FILES)
    {
        let before = report
            .before_images
            .iter()
            .find(|image| image.path == *path);
        let after = report.after_images.iter().find(|image| image.path == *path);
        let after_image = after;
        let (Some(before), Some(after_text)) = (preview_text(before), preview_text(after_image))
        else {
            continue;
        };

        let mut added_lines = Vec::new();
        let mut removed_lines = Vec::new();
        let mut line_count = 0_usize;
        let mut truncated = false;
        for change in TextDiff::from_lines(before.as_str(), after_text.as_str()).iter_all_changes()
        {
            let (target, prefix) = match change.tag() {
                ChangeTag::Insert => (&mut added_lines, '+'),
                ChangeTag::Delete => (&mut removed_lines, '-'),
                ChangeTag::Equal => continue,
            };
            if line_count >= MAX_MODEL_CHANGE_PREVIEW_LINES {
                truncated = true;
                break;
            }
            let value = change.value().trim_end_matches(['\r', '\n']);
            let redacted = redact_sensitive_text(&format!("{prefix}{value}")).0;
            let line = bounded_text_bytes(&redacted, MAX_MODEL_CHANGE_PREVIEW_LINE_BYTES);
            let line_bytes = line.len().saturating_add(1);
            if retained_bytes
                .saturating_add(line_bytes)
                .saturating_add(path.to_string_lossy().len())
                > MAX_MODEL_CHANGE_PREVIEW_BYTES
            {
                truncated = true;
                break;
            }
            retained_bytes = retained_bytes.saturating_add(line_bytes);
            line_count = line_count.saturating_add(1);
            target.push(line);
        }
        if added_lines.is_empty() && removed_lines.is_empty() && !truncated {
            continue;
        }

        let mut item = serde_json::Map::new();
        let model_path = path
            .strip_prefix(workspace_root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        item.insert("path".to_owned(), Value::String(model_path));
        item.insert(
            "added_lines".to_owned(),
            Value::Array(added_lines.into_iter().map(Value::String).collect()),
        );
        item.insert(
            "removed_lines".to_owned(),
            Value::Array(removed_lines.into_iter().map(Value::String).collect()),
        );
        item.insert("truncated".to_owned(), Value::Bool(truncated));
        if let Some(checksum) = after_image
            .and_then(|image| image.metadata.as_ref())
            .and_then(|metadata| metadata.checksum.clone())
        {
            item.insert("content_digest".to_owned(), Value::String(checksum));
        }
        previews.push(redact_sensitive_value(Value::Object(item)));
        if truncated || retained_bytes >= MAX_MODEL_CHANGE_PREVIEW_BYTES {
            break;
        }
    }

    if let Value::Object(facts) = &mut report.envelope.structured_facts {
        if let Some(Value::String(path)) = facts.get_mut("path") {
            let relative = Path::new(path.as_str())
                .strip_prefix(workspace_root)
                .unwrap_or_else(|_| Path::new(path.as_str()))
                .to_string_lossy()
                .into_owned();
            *path = relative;
        }
        let changed_paths = report
            .changed_files
            .iter()
            .map(|path| {
                Value::String(
                    path.strip_prefix(workspace_root)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .into_owned(),
                )
            })
            .collect::<Vec<_>>();
        if !changed_paths.is_empty() {
            facts.insert("changed_paths".to_owned(), Value::Array(changed_paths));
        }
        if !previews.is_empty() {
            facts.insert("change_preview".to_owned(), Value::Array(previews));
        }
    }
}

fn preview_text(image: Option<&FileBeforeImage>) -> Option<String> {
    let Some(image) = image else {
        return Some(String::new());
    };
    match (image.content.as_deref(), image.metadata.as_ref()) {
        (Some(bytes), _) => std::str::from_utf8(bytes).ok().map(ToOwned::to_owned),
        // 缺失路径没有内容和元数据；存在元数据但内容不可用时，镜像应保持不透明。
        (None, None) => Some(String::new()),
        (None, Some(_)) => None,
    }
}

fn io_error_kind_name(kind: std::io::ErrorKind) -> &'static str {
    match kind {
        std::io::ErrorKind::NotFound => "not_found",
        std::io::ErrorKind::PermissionDenied => "permission_denied",
        std::io::ErrorKind::AlreadyExists => "already_exists",
        std::io::ErrorKind::InvalidInput => "invalid_input",
        std::io::ErrorKind::InvalidData => "invalid_data",
        std::io::ErrorKind::TimedOut => "timed_out",
        std::io::ErrorKind::WouldBlock => "would_block",
        std::io::ErrorKind::Interrupted => "interrupted",
        std::io::ErrorKind::WriteZero => "write_zero",
        _ => "other",
    }
}

fn with_metrics(
    mut report: ToolExecutionReport,
    mut metrics: ToolExecutionMetrics,
) -> ToolExecutionReport {
    metrics.duration_ms = report.metrics.duration_ms;
    report.metrics = metrics;
    report
}

fn with_item_count(mut report: ToolExecutionReport, item_count: u64) -> ToolExecutionReport {
    report.metrics.item_count = Some(item_count);
    report
}

fn with_match_count(mut report: ToolExecutionReport, match_count: u64) -> ToolExecutionReport {
    report.metrics.match_count = Some(match_count);
    report
}

fn process_metrics(output: &process::ShellOutput) -> ToolExecutionMetrics {
    ToolExecutionMetrics {
        output_bytes: output.output_bytes,
        output_lines: output.output_lines,
        output_truncated: output.output_truncated,
        exit_code: output.exit_code,
        ..ToolExecutionMetrics::default()
    }
}

fn process_list_policy(request: &ToolRequest) -> PolicyEvaluation {
    let mut policy = execution_policy(
        request,
        PolicyDecision::Allow,
        "managed process discovery is scoped to the current session",
    );
    policy.resource = format!("process-session:{}", request.session_id);
    policy
}

fn process_control_policy(request: &ToolRequest) -> PolicyEvaluation {
    let process_id = request
        .arguments
        .get("process_id")
        .and_then(Value::as_str)
        .unwrap_or("<invalid-process-id>");
    let mut policy = execution_policy(
        request,
        PolicyDecision::Allow,
        "process session control is scoped to the current session",
    );
    // A write request may contain arbitrary stdin. Keep it out of durable
    // policy/audit resources while retaining the handle needed for review.
    policy.resource = format!("process:{process_id}");
    policy
}

fn web_search_policy(allow_network: bool, request: &ToolRequest) -> PolicyEvaluation {
    let decision = if allow_network {
        PolicyDecision::Allow
    } else {
        PolicyDecision::Block
    };
    let reason = if allow_network {
        "web search is enabled by the enclosing runtime"
    } else {
        "web search requires explicit network access"
    };
    let mut policy = execution_policy(request, decision, reason);
    policy.resource = "web-search".to_owned();
    policy
}

fn delegation_policy(request: &ToolRequest) -> PolicyEvaluation {
    let mut policy = execution_policy(
        request,
        PolicyDecision::Allow,
        "delegated child inherits the enclosing agent capabilities",
    );
    policy.resource = format!("delegated-task:{}", request.session_id);
    policy
}

fn process_cursor(arguments: &Value) -> u64 {
    arguments.get("cursor").and_then(Value::as_u64).unwrap_or(0)
}

fn process_authoritative_pid(arguments: &Value) -> Result<Option<u32>, ToolError> {
    let Some(value) = arguments.get("authoritative_pid") else {
        return Ok(None);
    };
    let Some(raw_pid) = value.as_u64() else {
        return Err(ToolError::InvalidArguments(
            "authoritative_pid must be a positive integer".to_owned(),
        ));
    };
    let pid = u32::try_from(raw_pid).map_err(|_| {
        ToolError::InvalidArguments(
            "authoritative_pid is outside the supported OS PID range".to_owned(),
        )
    })?;
    if pid == 0 {
        return Err(ToolError::InvalidArguments(
            "authoritative_pid must be a positive integer".to_owned(),
        ));
    }
    Ok(Some(pid))
}

fn process_wait_ms(arguments: &Value, default: u64) -> u64 {
    arguments
        .get("wait_ms")
        .and_then(Value::as_u64)
        .unwrap_or(default)
        .min(max_poll_wait_ms())
}

fn process_summary_value(summary: ProcessSummary) -> Value {
    let next_action = process_next_action(
        summary.state,
        &summary.process_id,
        summary.authoritative_pid,
        summary.output_cursor,
        summary.terminal_event_id,
    );
    json!({
        "process_id": summary.process_id,
        "authoritative_pid": summary.authoritative_pid,
        "command": bounded_text(
            &summary.command_display,
            MAX_TOOL_ARGUMENT_DISPLAY_STRING_BYTES,
        ),
        "process_state": process_state_name(summary.state),
        "exit_code": summary.exit_code,
        "output_cursor": summary.output_cursor,
        "output_bytes": summary.output_bytes,
        "output_lines": summary.output_lines,
        "output_truncated": summary.output_truncated,
        "terminal_event_id": summary.terminal_event_id,
        "terminal": summary.state.is_terminal(),
        "next_action": next_action,
    })
}

fn supervised_process_report(
    request: ToolRequest,
    policy: PolicyEvaluation,
    snapshot: ProcessSnapshot,
) -> ToolExecutionReport {
    let state = process_state_name(snapshot.state);
    let requested_termination = request.tool_name == "process_terminate"
        || (request.tool_name == "shell_session"
            && request.arguments.get("action").and_then(Value::as_str) == Some("terminate"));
    let status = match snapshot.state {
        ProcessState::Running | ProcessState::Exited => ToolResultStatus::Ok,
        ProcessState::Failed => ToolResultStatus::Error,
        ProcessState::TimedOut => ToolResultStatus::Timeout,
        ProcessState::Terminated if requested_termination => ToolResultStatus::Ok,
        ProcessState::Cancelled | ProcessState::Terminated => ToolResultStatus::Cancelled,
    };
    let workspace_changes_known = snapshot.workspace_changes_known;
    let terminal = snapshot.state.is_terminal();
    let next_action = process_next_action(
        snapshot.state,
        &snapshot.process_id,
        snapshot.authoritative_pid,
        snapshot.output_cursor,
        snapshot.terminal_event_id,
    );
    let mut result = report(
        request,
        status,
        match snapshot.state {
            ProcessState::Running => {
                "background process is running, but it is runtime-scoped and will stop when the runtime exits; post-runtime consumers require a detached process"
            }
            ProcessState::Exited => "background process exited successfully",
            ProcessState::Failed => "background process exited with an error",
            ProcessState::TimedOut => "background process timed out",
            ProcessState::Cancelled => "background process was cancelled",
            ProcessState::Terminated => "background process was terminated",
        },
        json!({
            "process_id": snapshot.process_id,
            "authoritative_pid": snapshot.authoritative_pid,
            "process_state": state,
            "exit_code": snapshot.exit_code,
            "output_cursor": snapshot.output_cursor,
            "output_bytes": snapshot.output_bytes,
            "output_lines": snapshot.output_lines,
            "output_truncated": snapshot.output_truncated,
            "output_lost": snapshot.output_lost,
            "terminal_event_id": snapshot.terminal_event_id,
            "workspace_changes_known": workspace_changes_known,
            "process_lifetime_scope": "runtime",
            "survives_runtime_exit": false,
            "terminal": terminal,
            "wait_strategy": "event_driven_cursor",
            "next_action": next_action,
            "workspace_change_count": if workspace_changes_known {
                snapshot.changed_files.len()
            } else {
                0
            },
                    "sandbox_backend": snapshot.sandbox_backend,
                    "sandbox_os_enforced": snapshot.sandbox_os_enforced,
                    "network_access": snapshot.network_access,
        }),
        snapshot.output,
        if workspace_changes_known {
            snapshot.changed_files.clone()
        } else {
            Vec::new()
        },
        policy,
    );
    result.before_images = if workspace_changes_known {
        snapshot.before_images
    } else {
        Vec::new()
    };
    result.after_images = if workspace_changes_known {
        snapshot.after_images
    } else {
        Vec::new()
    };
    result.metrics.output_bytes = snapshot.output_bytes;
    result.metrics.output_lines = snapshot.output_lines;
    result.metrics.output_truncated = snapshot.output_truncated;
    result.metrics.exit_code = snapshot.exit_code;
    result
}

fn process_state_name(state: ProcessState) -> &'static str {
    match state {
        ProcessState::Running => "running",
        ProcessState::Exited => "exited",
        ProcessState::TimedOut => "timed_out",
        ProcessState::Cancelled => "cancelled",
        ProcessState::Terminated => "terminated",
        ProcessState::Failed => "failed",
    }
}

fn process_next_action(
    state: ProcessState,
    process_id: &str,
    authoritative_pid: u32,
    cursor: u64,
    terminal_event_id: Option<u64>,
) -> Value {
    if state == ProcessState::Running {
        // 把下一步所需的最小参数直接交给模型，避免它重复读取或重置 cursor。
        json!({
            "kind": "wait",
            "tool": "shell_session",
            "action": "wait",
            "process_id": process_id,
            "authoritative_pid": authoritative_pid,
            "cursor": cursor,
            "wait_ms": default_poll_wait_ms(),
            "wait_for_terminal": true,
        })
    } else {
        json!({
            "kind": "terminal",
            "process_state": process_state_name(state),
            "authoritative_pid": authoritative_pid,
            "terminal_event_id": terminal_event_id,
        })
    }
}

fn emit_tool_progress(
    progress: &mut Option<&mut (dyn FnMut(ToolProgress) + Send)>,
    value: ToolProgress,
) {
    if let Some(sink) = progress.as_mut() {
        sink(value);
    }
}

fn emit_process_progress(
    progress: &mut Option<&mut (dyn FnMut(ToolProgress) + Send)>,
    tool_call_id: ToolCallId,
    tool_name: &str,
    started_at: Instant,
    process: ProcessProgress,
) {
    let stream = match process.stream {
        ProcessStream::Stdout => "stdout",
        ProcessStream::Stderr => "stderr",
    };
    emit_tool_progress(
        progress,
        ToolProgress {
            tool_call_id,
            tool_name: tool_name.to_owned(),
            phase: ToolProgressPhase::Output,
            elapsed_ms: elapsed_millis(started_at),
            output_bytes: process.output_bytes,
            output_lines: process.output_lines,
            detail: Some(if process.truncated {
                format!(
                    "{stream} (truncated, {} bytes retained)",
                    process.retained_bytes
                )
            } else {
                stream.to_owned()
            }),
            output_excerpt: process.output_excerpt,
        },
    );
}

fn elapsed_millis(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn output_line_count(output: &str) -> u64 {
    if output.is_empty() {
        return 0;
    }
    let newlines =
        u64::try_from(output.bytes().filter(|byte| *byte == b'\n').count()).unwrap_or(u64::MAX);
    newlines.saturating_add(u64::from(!output.ends_with('\n')))
}

fn redact_sensitive_value(mut value: Value) -> Value {
    redact_sensitive_value_in_place(&mut value, false);
    value
}

fn mark_workspace_changes_unknown(mut facts: Value) -> Value {
    if let Some(object) = facts.as_object_mut() {
        object.insert("workspace_changes_known".to_owned(), Value::Bool(false));
        facts
    } else {
        json!({
            "result": facts,
            "workspace_changes_known": false,
        })
    }
}

/// Redacts tool arguments before they are copied into user-visible lifecycle
/// events. The returned value is detached from the provider request and has a
/// hard serialized-size limit.
#[must_use]
pub fn redact_tool_arguments(arguments: &Value) -> Value {
    let redacted = redact_sensitive_value(arguments.clone());
    let projected = project_tool_argument_value(&redacted, None, 0);
    if serialized_value_len(&projected) <= MAX_TOOL_ARGUMENT_DISPLAY_BYTES {
        return projected;
    }

    let compact = compact_tool_argument_projection(&redacted);
    if serialized_value_len(&compact) <= MAX_TOOL_ARGUMENT_DISPLAY_BYTES {
        compact
    } else {
        json!({"_golutra_truncated": true})
    }
}

/// Project the operational tool envelope into the only representation that
/// may be appended to a provider request. Artifact/evidence references,
/// security risk, and governance hints remain durable runtime facts and are
/// intentionally excluded from model context.
#[must_use]
pub fn model_visible_tool_result(envelope: &ToolResultEnvelope) -> String {
    model_visible_tool_result_with_limit(envelope, DEFAULT_MODEL_TOOL_RESULT_BYTES)
}

/// 在明确的 token 预算下生成 provider 可见结果。
///
/// 内部按字节限制投影，调用方沿用上下文规划中的“字符数除以四”估算。
/// 将换算集中在这里可明确边界，避免误把 token 数直接当作字节上限。
#[must_use]
pub fn model_visible_tool_result_with_token_budget(
    envelope: &ToolResultEnvelope,
    max_tokens: u64,
) -> String {
    let max_bytes = max_tokens.saturating_mul(4).min(usize::MAX as u64) as usize;
    model_visible_tool_result_with_limit(envelope, max_bytes)
}

/// 生成有界的 provider 可见结果。普通投影使用保守的 8 KiB 封套；调用方可按
/// 确定性的上下文预算请求更小上限，硬上限始终固定，避免单个工具占满提示词。
/// 压缩说明文字前先保留状态以及工具专属的续读/错误事实。
#[must_use]
pub fn model_visible_tool_result_with_limit(
    envelope: &ToolResultEnvelope,
    max_bytes: usize,
) -> String {
    let max_bytes = max_bytes.clamp(MIN_MODEL_TOOL_RESULT_BYTES, MAX_MODEL_TOOL_RESULT_BYTES);
    let summary = bounded_text(
        &redact_sensitive_text(&envelope.summary).0,
        MAX_MODEL_TOOL_RESULT_SUMMARY_CHARS,
    );
    let facts = redact_sensitive_value(envelope.structured_facts.clone());
    let excerpt = envelope.model_visible_excerpt.as_deref().map(|value| {
        bounded_text_bytes(
            &redact_sensitive_text(value).0,
            MAX_MODEL_TOOL_RESULT_EXCERPT_BYTES,
        )
    });
    let known = is_pi_plus_tool(&envelope.tool_name);
    let (mut facts, summary, mut excerpt, keep_summary, keep_excerpt) =
        match envelope.tool_name.as_str() {
            "read_file" => {
                let content = excerpt.or_else(|| {
                    facts
                        .get("content")
                        .and_then(Value::as_str)
                        .map(|value| bounded_text_bytes(value, MAX_MODEL_TOOL_RESULT_EXCERPT_BYTES))
                });
                (
                    selected_model_facts(&facts, READ_FILE_MODEL_FACTS),
                    summary,
                    content,
                    envelope.status != ToolResultStatus::Ok,
                    true,
                )
            }
            "write_file" | "edit_file" | "apply_patch" => (
                selected_model_facts(&facts, MUTATION_MODEL_FACTS),
                summary,
                None,
                // mutation 的结构化事实只说明路径和计数，短摘要仍是模型判断
                // 写入是否成功所需的语义；完整内容和 patch 输出继续留在 durable artifact。
                true,
                false,
            ),
            "shell" | "shell_session" => (
                selected_model_facts(&facts, PROCESS_MODEL_FACTS),
                summary,
                excerpt.map(|value| bounded_text(&value, MAX_MODEL_TOOL_OUTPUT_CHARS)),
                envelope.status != ToolResultStatus::Ok,
                true,
            ),
            "web_search" => (
                project_search_model_facts(&facts),
                summary,
                None,
                envelope.status != ToolResultStatus::Ok,
                false,
            ),
            "subagent" => {
                let keep_excerpt = excerpt.as_deref().is_some_and(|value| value != summary);
                (
                    project_subagent_model_facts(&facts),
                    summary,
                    excerpt.map(|value| bounded_text(&value, MAX_MODEL_TOOL_OUTPUT_CHARS)),
                    true,
                    keep_excerpt,
                )
            }
            _ => (
                project_model_tool_value(&facts, 0),
                summary,
                excerpt,
                !known,
                !known,
            ),
        };
    // mutation 调用方原本将路径列表命名为 `changed_files`；这里提供稳定的
    // provider 可见 `changed_paths`，但不复制完整持久封套。
    if matches!(
        envelope.tool_name.as_str(),
        "write_file" | "edit_file" | "apply_patch"
    ) && let Value::Object(object) = &mut facts
        && !object.contains_key("changed_paths")
        && let Some(paths) = object.get("changed_files").cloned()
    {
        object.insert("changed_paths".to_owned(), paths);
    }
    prune_redundant_success_facts(&envelope.tool_name, envelope.status, &mut facts);
    // 成功文件或进程输出使用纯文本更易消费；先保留精简 JSON 事实头，再原样
    // 追加摘录，避免把转义换行再次嵌入 JSON 字符串。
    let inline_excerpt = (keep_excerpt
        && matches!(
            envelope.tool_name.as_str(),
            "read_file" | "shell" | "shell_session"
        ))
    .then(|| excerpt.take())
    .flatten();
    let mut projection = model_tool_result_base(&envelope.tool_name, envelope.status);
    if let Value::Object(object) = &mut projection {
        object.insert("structured_facts".to_owned(), facts);
        if keep_summary {
            object.insert(
                "summary".to_owned(),
                Value::String(bounded_text(
                    &summary,
                    if known {
                        MAX_MODEL_TOOL_REASON_CHARS
                    } else {
                        MAX_MODEL_TOOL_RESULT_SUMMARY_CHARS
                    },
                )),
            );
        }
        if keep_excerpt && let Some(excerpt) = excerpt.filter(|value| !value.is_empty()) {
            object.insert("model_visible_excerpt".to_owned(), Value::String(excerpt));
        }
    }
    let header = serialize_model_tool_projection(
        projection,
        &envelope.tool_name,
        envelope.status,
        &summary,
        max_bytes,
    );
    append_model_visible_excerpt(header, inline_excerpt.as_deref(), max_bytes)
}

const READ_FILE_MODEL_FACTS: &[&str] = &[
    "path",
    "requested_path",
    "path_normalized",
    "content_digest",
    "path_resolution",
    "candidates",
    "next_action",
    "bytes",
    "lines",
    "truncated",
    "continuation",
    "next_cursor",
    "next_offset",
    "cursor",
    "offset",
    "limit",
    "total_bytes",
    "total_lines",
    "has_more",
    "eof",
    "error",
    "reason",
    "timed_out",
    "cancelled",
    "blocked",
    "model_visible_truncated",
    "model_visible_bytes",
];

const MUTATION_MODEL_FACTS: &[&str] = &[
    "path",
    "changed_paths",
    "changed",
    "idempotent",
    "content_digest",
    "changed_files",
    "changed_file_count",
    "workspace_change_count",
    "workspace_changes_known",
    "workspace_mutation_detected",
    "change_preview",
    "bytes",
    "replacements",
    "edit_count",
    "error_kind",
    "conflict",
    "search_found",
    "max_bytes",
    "exit_code",
    "timed_out",
    "cancelled",
    "error",
    "reason",
    "parent",
    "parent_missing",
    "action_required",
    "checkpointed_paths",
    "resolved_paths",
    "output_truncated",
    "blocked",
];

const PROCESS_MODEL_FACTS: &[&str] = &[
    "process_id",
    "authoritative_pid",
    "process_state",
    "exit_code",
    "timed_out",
    "cancelled",
    "output_cursor",
    "output_lost",
    "output_bytes",
    "output_lines",
    "output_truncated",
    "workspace_changes_known",
    "workspace_change_count",
    "process_lifetime_scope",
    "survives_runtime_exit",
    "terminal",
    "wait_strategy",
    "wait_for_terminal",
    "next_action",
    "error",
    "reason",
    "blocked",
];

fn model_tool_result_base(tool_name: &str, status: ToolResultStatus) -> Value {
    json!({
        "tool_name": bounded_text(tool_name, 128),
        "status": status,
    })
}

fn selected_model_facts(value: &Value, keys: &[&str]) -> Value {
    let Some(source) = value.as_object() else {
        return project_model_tool_value(value, 0);
    };
    let mut projected = serde_json::Map::new();
    for key in keys {
        if let Some(value) = source.get(*key) {
            projected.insert((*key).to_owned(), project_model_tool_value(value, 0));
        }
    }
    Value::Object(projected)
}

/// 删除已由状态、配对工具参数或保留内容表达的成功态记账字段。失败态保持不变，
/// 确保恢复流程始终能看到完整且有界的诊断。
fn prune_redundant_success_facts(tool_name: &str, status: ToolResultStatus, facts: &mut Value) {
    if status != ToolResultStatus::Ok {
        return;
    }
    let Some(object) = facts.as_object_mut() else {
        return;
    };
    object.retain(|_, value| !value.is_null());
    match tool_name {
        "read_file" => {
            for key in [
                "bytes",
                "lines",
                "offset",
                "limit",
                "total_bytes",
                "total_lines",
                "model_visible_bytes",
            ] {
                object.remove(key);
            }
            remove_false_fact(object, "has_more");
            remove_false_fact(object, "truncated");
            remove_false_fact(object, "model_visible_truncated");
            // `has_more` 与 `eof` 互补；只保留正向事实，避免每次完整读取重复付费。
            if object.get("eof") == Some(&Value::Bool(false)) {
                object.remove("eof");
            }
        }
        "write_file" | "edit_file" | "apply_patch" => {
            if object.contains_key("changed_paths") {
                object.remove("changed_files");
            }
            remove_true_fact(object, "changed");
            remove_true_fact(object, "workspace_changes_known");
            remove_true_fact(object, "workspace_mutation_detected");
            if object.get("workspace_change_count") == object.get("changed_file_count") {
                object.remove("workspace_change_count");
            }
        }
        "shell" | "shell_session" => {
            object.remove("output_bytes");
            object.remove("output_lines");
            remove_false_fact(object, "timed_out");
            remove_false_fact(object, "cancelled");
            remove_false_fact(object, "output_truncated");
            remove_true_fact(object, "workspace_changes_known");
            if object.get("workspace_change_count") == Some(&Value::from(0_u64)) {
                object.remove("workspace_change_count");
            }
        }
        _ => {}
    }
}

fn remove_false_fact(object: &mut serde_json::Map<String, Value>, key: &str) {
    if object.get(key) == Some(&Value::Bool(false)) {
        object.remove(key);
    }
}

fn remove_true_fact(object: &mut serde_json::Map<String, Value>, key: &str) {
    if object.get(key) == Some(&Value::Bool(true)) {
        object.remove(key);
    }
}

fn append_model_visible_excerpt(header: String, excerpt: Option<&str>, max_bytes: usize) -> String {
    const SEPARATOR: &str = "\n--- output ---\n";
    let Some(excerpt) = excerpt.filter(|value| !value.is_empty()) else {
        return header;
    };
    let available = max_bytes.saturating_sub(header.len().saturating_add(SEPARATOR.len()));
    if available == 0 {
        return header;
    }
    let excerpt = bounded_text_bytes(excerpt, available);
    format!("{header}{SEPARATOR}{excerpt}")
}

fn project_search_model_facts(value: &Value) -> Value {
    let Some(source) = value.as_object() else {
        return project_model_tool_value(value, 0);
    };
    let mut projected = serde_json::Map::new();
    for key in ["query", "result_count", "cached", "error", "reason"] {
        if let Some(value) = source.get(key) {
            let value = if key == "query" {
                value
                    .as_str()
                    .map(|query| {
                        Value::String(bounded_text(query, MAX_MODEL_TOOL_SEARCH_QUERY_CHARS))
                    })
                    .unwrap_or_else(|| project_model_tool_value(value, 0))
            } else {
                project_model_tool_value(value, 0)
            };
            projected.insert(key.to_owned(), value);
        }
    }
    if let Some(results) = source.get("results").and_then(Value::as_array) {
        let results = results
            .iter()
            .take(MAX_MODEL_TOOL_SEARCH_RESULTS)
            .filter_map(|result| {
                let object = result.as_object()?;
                let mut item = serde_json::Map::new();
                for (key, limit) in [
                    ("title", MAX_MODEL_TOOL_SEARCH_TITLE_CHARS),
                    ("url", MAX_MODEL_TOOL_SEARCH_URL_CHARS),
                    ("snippet", MAX_MODEL_TOOL_SEARCH_SNIPPET_CHARS),
                ] {
                    if let Some(value) = object.get(key).and_then(Value::as_str) {
                        item.insert(key.to_owned(), Value::String(bounded_text(value, limit)));
                    }
                }
                (!item.is_empty()).then_some(Value::Object(item))
            })
            .collect::<Vec<_>>();
        projected.insert("results".to_owned(), Value::Array(results));
    }
    Value::Object(projected)
}

fn project_subagent_model_facts(value: &Value) -> Value {
    let Some(source) = value.as_object() else {
        return project_model_tool_value(value, 0);
    };
    let mut projected = serde_json::Map::new();
    for (key, value) in source {
        if key.starts_with("child_")
            || matches!(
                key.as_str(),
                "completed"
                    | "success"
                    | "workspace_changes_known"
                    | "workspace_change_count"
                    | "changed_files"
                    | "artifact_ref"
                    | "artifact_refs"
                    | "evidence_ref"
                    | "evidence_refs"
                    | "result_ref"
                    | "error"
                    | "reason"
                    | "cancelled"
                    | "timed_out"
                    | "blocked"
                    | "partial"
                    | "truncated"
                    | "continuation"
            )
        {
            projected.insert(bounded_text(key, 96), project_model_tool_value(value, 0));
        }
    }
    Value::Object(projected)
}

fn serialize_model_tool_projection(
    mut projection: Value,
    tool_name: &str,
    status: ToolResultStatus,
    summary: &str,
    max_bytes: usize,
) -> String {
    let original_facts = projection
        .get("structured_facts")
        .cloned()
        .unwrap_or(Value::Null);
    let keep_summary = model_tool_summary_is_required(tool_name, status);

    if serialized_value_len(&projection) > max_bytes
        && let Value::Object(object) = &mut projection
    {
        if let Some(facts) = object.get_mut("structured_facts") {
            *facts = compact_model_tool_facts(tool_name, facts);
        }
        if let Some(value) = object.get_mut("model_visible_excerpt")
            && let Some(text) = value.as_str()
        {
            *value = Value::String(bounded_text_bytes(
                text,
                MAX_MODEL_TOOL_RESULT_COMPACT_EXCERPT_CHARS
                    .saturating_mul(4)
                    .min(max_bytes / 2),
            ));
        }
    }
    if serialized_value_len(&projection) > max_bytes
        && let Value::Object(object) = &mut projection
    {
        // 先保留结构化事实和状态。成功输出可以通过 continuation/cursor 再取，
        // 但错误摘要必须保留，模型才能知道如何恢复。
        object.remove("model_visible_excerpt");
        if keep_summary {
            if let Some(value) = object.get_mut("summary")
                && let Some(text) = value.as_str()
            {
                *value = Value::String(bounded_text_bytes(
                    text,
                    MAX_MODEL_TOOL_RESULT_SUMMARY_CHARS.min(max_bytes.saturating_sub(256)),
                ));
            }
        } else {
            object.remove("summary");
        }
    }
    if serialized_value_len(&projection) <= max_bytes {
        return serde_json::to_string(&projection)
            .unwrap_or_else(|_| "{\"status\":\"error\"}".to_owned());
    }
    if let Value::Object(object) = &mut projection {
        object.insert(
            "structured_facts".to_owned(),
            minimal_model_tool_facts(tool_name, &original_facts),
        );
        if !keep_summary {
            object.remove("summary");
        }
    }
    if serialized_value_len(&projection) <= max_bytes {
        return serde_json::to_string(&projection)
            .unwrap_or_else(|_| "{\"status\":\"error\"}".to_owned());
    }

    let mut fallback = model_tool_result_base(tool_name, status);
    fallback["structured_facts"] = minimal_model_tool_facts(tool_name, &original_facts);
    if keep_summary {
        fallback["summary"] = Value::String(bounded_text_bytes(
            summary,
            MAX_MODEL_TOOL_RESULT_SUMMARY_CHARS.min(max_bytes.saturating_sub(256)),
        ));
    }
    let serialized =
        serde_json::to_string(&fallback).unwrap_or_else(|_| "{\"status\":\"error\"}".to_owned());
    if serialized.len() <= max_bytes {
        serialized
    } else {
        // 预算异常紧时仍返回可解析的核心状态；正常调用的最小预算足以容纳
        // tool_name、status 以及上面的专属事实和摘要。
        let mut minimal = model_tool_result_base(tool_name, status);
        if keep_summary {
            minimal["summary"] = Value::String(bounded_text_bytes(
                summary,
                max_bytes.saturating_sub(192).min(256),
            ));
        }
        let serialized =
            serde_json::to_string(&minimal).unwrap_or_else(|_| "{\"status\":\"error\"}".to_owned());
        if serialized.len() <= max_bytes {
            serialized
        } else {
            serde_json::to_string(&model_tool_result_base(tool_name, status))
                .unwrap_or_else(|_| "{\"status\":\"error\"}".to_owned())
        }
    }
}

fn model_tool_summary_is_required(tool_name: &str, status: ToolResultStatus) -> bool {
    status != ToolResultStatus::Ok
        || !is_pi_plus_tool(tool_name)
        || matches!(
            tool_name,
            "write_file" | "edit_file" | "apply_patch" | "subagent"
        )
}

fn model_fact_priority(tool_name: &str) -> &'static [&'static str] {
    match tool_name {
        "read_file" => READ_FILE_MODEL_FACTS,
        "write_file" | "edit_file" | "apply_patch" => MUTATION_MODEL_FACTS,
        "shell" | "shell_session" => PROCESS_MODEL_FACTS,
        "web_search" => &["error", "reason", "query", "result_count", "cached"],
        "subagent" => &[
            "child_status",
            "completed",
            "success",
            "status",
            "error",
            "reason",
            "cancelled",
            "timed_out",
            "blocked",
            "partial",
            "truncated",
            "continuation",
        ],
        _ => &[
            "status",
            "error",
            "reason",
            "continuation",
            "next_offset",
            "next_cursor",
        ],
    }
}

fn model_fact_mandatory(tool_name: &str) -> &'static [&'static str] {
    match tool_name {
        "read_file" => &[
            "path",
            "content_digest",
            "continuation",
            "next_offset",
            "next_cursor",
            "has_more",
            "eof",
            "truncated",
            "error",
            "reason",
            "timed_out",
            "cancelled",
            "blocked",
        ],
        "write_file" | "edit_file" | "apply_patch" => &[
            "path",
            "parent",
            "parent_missing",
            "action_required",
            "error_kind",
            "changed_file_count",
            "workspace_change_count",
            "workspace_changes_known",
            "workspace_mutation_detected",
            "conflict",
            "error",
            "reason",
            "blocked",
        ],
        "shell" | "shell_session" => &[
            "process_id",
            "authoritative_pid",
            "process_state",
            "exit_code",
            "output_cursor",
            "timed_out",
            "cancelled",
            "terminal",
            "next_action",
            "error",
            "reason",
            "blocked",
        ],
        "web_search" => &["error", "reason", "query", "result_count", "cached"],
        "subagent" => &[
            "child_status",
            "completed",
            "success",
            "error",
            "reason",
            "cancelled",
            "timed_out",
            "blocked",
            "partial",
            "truncated",
            "continuation",
        ],
        _ => &[
            "status",
            "error",
            "reason",
            "continuation",
            "next_offset",
            "next_cursor",
        ],
    }
}

fn compact_model_tool_facts(tool_name: &str, value: &Value) -> Value {
    let Some(object) = value.as_object() else {
        return compact_model_tool_value(value, 0);
    };
    let priority = model_fact_priority(tool_name);
    let mandatory = model_fact_mandatory(tool_name);
    let mut projected = serde_json::Map::new();
    for key in mandatory {
        if let Some(value) = object.get(*key) {
            projected.insert((*key).to_owned(), compact_model_fact_value(key, value));
        }
    }
    for key in priority {
        if projected.len() >= 16 || projected.contains_key(*key) {
            continue;
        }
        if let Some(value) = object.get(*key) {
            projected.insert((*key).to_owned(), compact_model_fact_value(key, value));
        }
    }
    let mut remaining = object
        .keys()
        .filter(|key| !projected.contains_key(*key))
        .collect::<Vec<_>>();
    remaining.sort();
    for key in remaining
        .into_iter()
        .take(8usize.saturating_sub(projected.len()))
    {
        if let Some(value) = object.get(key) {
            projected.insert(key.clone(), compact_model_fact_value(key, value));
        }
    }
    if projected.len() < object.len() {
        projected.insert("_golutra_truncated".to_owned(), Value::Bool(true));
    }
    Value::Object(projected)
}

fn minimal_model_tool_facts(tool_name: &str, value: &Value) -> Value {
    let Some(object) = value.as_object() else {
        return compact_model_tool_value(value, 0);
    };
    let mut projected = serde_json::Map::new();
    for key in model_fact_mandatory(tool_name) {
        if let Some(value) = object.get(*key) {
            projected.insert((*key).to_owned(), compact_model_fact_value(key, value));
        }
    }
    if projected.is_empty() && !object.is_empty() {
        projected.insert("_golutra_truncated".to_owned(), Value::Bool(true));
    }
    Value::Object(projected)
}

fn compact_model_fact_value(key: &str, value: &Value) -> Value {
    match key {
        "continuation" => match value {
            Value::Object(object) => compact_priority_model_object(
                object,
                &[
                    "next_offset",
                    "next_cursor",
                    "has_more",
                    "eof",
                    "cursor",
                    "offset",
                ],
                8,
            ),
            _ => compact_model_tool_value(value, 0),
        },
        "next_action" => match value {
            Value::Object(object) => compact_priority_model_object(
                object,
                &[
                    "action",
                    "process_id",
                    "authoritative_pid",
                    "cursor",
                    "wait_ms",
                    "wait_for_terminal",
                ],
                8,
            ),
            _ => compact_model_tool_value(value, 0),
        },
        "error" => match value {
            Value::Object(object) => {
                compact_priority_model_object(object, &["code", "message", "reason"], 6)
            }
            _ => compact_model_tool_value(value, 0),
        },
        "change_preview" => compact_change_preview(value),
        // 文件变更列表可非常大；保留少量路径样本和截断标记，把预算留给
        // changed_file_count、conflict 等能决定下一步动作的标量事实。
        "changed_files" | "checkpointed_paths" | "resolved_paths" => match value {
            Value::Array(values) => {
                let mut projected = values
                    .iter()
                    .take(4)
                    .map(|value| match value {
                        Value::String(text) => Value::String(bounded_text_bytes(text, 96)),
                        _ => compact_model_tool_value(value, 1),
                    })
                    .collect::<Vec<_>>();
                if values.len() > 4 {
                    projected.push(Value::String(format!(
                        "<omitted {} additional items>",
                        values.len() - 4
                    )));
                }
                Value::Array(projected)
            }
            _ => compact_model_tool_value(value, 0),
        },
        _ => compact_model_tool_value(value, 0),
    }
}

fn compact_change_preview(value: &Value) -> Value {
    let Some(items) = value.as_array() else {
        return compact_model_tool_value(value, 0);
    };
    Value::Array(
        items
            .iter()
            .take(MAX_MODEL_CHANGE_PREVIEW_FILES)
            .filter_map(|item| {
                let object = item.as_object()?;
                let mut compact = serde_json::Map::new();
                for key in ["path", "content_digest", "truncated"] {
                    if let Some(value) = object.get(key) {
                        compact.insert(key.to_owned(), compact_model_tool_value(value, 0));
                    }
                }
                for key in ["added_lines", "removed_lines"] {
                    if let Some(Value::Array(lines)) = object.get(key) {
                        compact.insert(
                            key.to_owned(),
                            Value::Array(
                                lines
                                    .iter()
                                    .take(12)
                                    .map(|line| match line {
                                        Value::String(line) => Value::String(bounded_text_bytes(
                                            line,
                                            MAX_MODEL_CHANGE_PREVIEW_LINE_BYTES,
                                        )),
                                        other => compact_model_tool_value(other, 0),
                                    })
                                    .collect(),
                            ),
                        );
                    }
                }
                Some(Value::Object(compact))
            })
            .collect(),
    )
}

fn compact_priority_model_object(
    object: &serde_json::Map<String, Value>,
    priority: &[&str],
    max_fields: usize,
) -> Value {
    let mut projected = serde_json::Map::new();
    for key in priority {
        if let Some(value) = object.get(*key) {
            projected.insert((*key).to_owned(), compact_model_tool_value(value, 1));
        }
    }
    let mut remaining = object
        .keys()
        .filter(|key| !projected.contains_key(*key))
        .collect::<Vec<_>>();
    remaining.sort();
    for key in remaining
        .into_iter()
        .take(max_fields.saturating_sub(projected.len()))
    {
        if let Some(value) = object.get(key) {
            projected.insert(key.clone(), compact_model_tool_value(value, 1));
        }
    }
    if projected.len() < object.len() {
        projected.insert("_golutra_truncated".to_owned(), Value::Bool(true));
    }
    Value::Object(projected)
}

fn project_model_tool_value(value: &Value, depth: usize) -> Value {
    if depth >= MAX_MODEL_TOOL_RESULT_DEPTH {
        return omitted_model_tool_value(value);
    }
    match value {
        Value::Object(object) => {
            let mut projected = serde_json::Map::new();
            for (key, value) in object.iter().take(MAX_MODEL_TOOL_RESULT_ITEMS) {
                projected.insert(
                    bounded_text(key, 128),
                    project_model_tool_value(value, depth + 1),
                );
            }
            if object.len() > MAX_MODEL_TOOL_RESULT_ITEMS {
                projected.insert("_golutra_truncated".to_owned(), Value::Bool(true));
            }
            Value::Object(projected)
        }
        Value::Array(values) => {
            let mut projected = values
                .iter()
                .take(MAX_MODEL_TOOL_RESULT_ITEMS)
                .map(|value| project_model_tool_value(value, depth + 1))
                .collect::<Vec<_>>();
            if values.len() > MAX_MODEL_TOOL_RESULT_ITEMS {
                projected.push(Value::String(format!(
                    "<omitted {} additional items>",
                    values.len() - MAX_MODEL_TOOL_RESULT_ITEMS
                )));
            }
            Value::Array(projected)
        }
        Value::String(text) => {
            Value::String(bounded_text(text, MAX_MODEL_TOOL_RESULT_FACT_STRING_CHARS))
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
    }
}

fn compact_model_tool_value(value: &Value, depth: usize) -> Value {
    if depth >= 2 {
        return match value {
            Value::String(text) => Value::String(bounded_text_bytes(text, 256)),
            Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
            Value::Object(object) => {
                Value::String(format!("<omitted object with {} fields>", object.len()))
            }
            Value::Array(values) => {
                Value::String(format!("<omitted array with {} items>", values.len()))
            }
        };
    }
    match value {
        Value::Object(object) => {
            let mut projected = serde_json::Map::new();
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort();
            for key in keys.into_iter().take(8) {
                if let Some(value) = object.get(key) {
                    projected.insert(
                        bounded_text(key, 64),
                        compact_model_tool_value(value, depth + 1),
                    );
                }
            }
            if object.len() > 8 {
                projected.insert("_golutra_truncated".to_owned(), Value::Bool(true));
            }
            Value::Object(projected)
        }
        Value::Array(values) => {
            let mut projected = values
                .iter()
                .take(8)
                .map(|value| compact_model_tool_value(value, depth + 1))
                .collect::<Vec<_>>();
            if values.len() > 8 {
                projected.push(Value::String(format!(
                    "<omitted {} additional items>",
                    values.len() - 8
                )));
            }
            Value::Array(projected)
        }
        Value::String(text) => Value::String(bounded_text_bytes(text, 256)),
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
    }
}

fn omitted_model_tool_value(value: &Value) -> Value {
    let summary = match value {
        Value::Object(object) => format!("<omitted object with {} fields>", object.len()),
        Value::Array(values) => format!("<omitted array with {} items>", values.len()),
        Value::String(text) => format!("<omitted {} characters>", text.chars().count()),
        Value::Null | Value::Bool(_) | Value::Number(_) => "<omitted value>".to_owned(),
    };
    Value::String(summary)
}

const PREFERRED_TOOL_ARGUMENT_KEYS: &[&str] = &[
    "path",
    "offset",
    "limit",
    "command",
    "workdir",
    "pattern",
    "query",
    "symbol",
    "timeout_ms",
    "background",
    "yield-time_ms",
    "cwd",
    "glob",
    "url",
    "method",
    "search",
    "replace",
    "edits",
    "content",
];

fn project_tool_argument_value(value: &Value, key: Option<&str>, depth: usize) -> Value {
    if key.is_some_and(payload_argument_key) {
        return omitted_argument_summary(value);
    }
    if depth >= MAX_TOOL_ARGUMENT_DISPLAY_DEPTH {
        return omitted_argument_summary(value);
    }
    match value {
        Value::Object(object) => {
            let mut projected = serde_json::Map::new();
            for preferred in PREFERRED_TOOL_ARGUMENT_KEYS {
                if let Some(value) = object.get(*preferred) {
                    projected.insert(
                        (*preferred).to_owned(),
                        project_tool_argument_value(value, Some(preferred), depth + 1),
                    );
                }
            }
            for (key, value) in object {
                if projected.len() >= MAX_TOOL_ARGUMENT_DISPLAY_ITEMS {
                    projected.insert("_golutra_truncated".to_owned(), Value::Bool(true));
                    break;
                }
                if projected.contains_key(key) {
                    continue;
                }
                projected.insert(
                    key.clone(),
                    project_tool_argument_value(value, Some(key), depth + 1),
                );
            }
            Value::Object(projected)
        }
        Value::Array(values) => {
            let mut projected = values
                .iter()
                .take(MAX_TOOL_ARGUMENT_DISPLAY_ITEMS)
                .map(|value| project_tool_argument_value(value, None, depth + 1))
                .collect::<Vec<_>>();
            if values.len() > MAX_TOOL_ARGUMENT_DISPLAY_ITEMS {
                projected.push(Value::String(format!(
                    "<omitted {} additional items>",
                    values.len() - MAX_TOOL_ARGUMENT_DISPLAY_ITEMS
                )));
            }
            Value::Array(projected)
        }
        Value::String(text) => Value::String(bounded_argument_string(
            text,
            MAX_TOOL_ARGUMENT_DISPLAY_STRING_BYTES,
        )),
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
    }
}

fn compact_tool_argument_projection(value: &Value) -> Value {
    let Value::Object(object) = value else {
        return compact_tool_argument_value(value, None);
    };
    let mut projected = serde_json::Map::new();
    for key in PREFERRED_TOOL_ARGUMENT_KEYS {
        if let Some(value) = object.get(*key) {
            projected.insert(
                (*key).to_owned(),
                compact_tool_argument_value(value, Some(key)),
            );
        }
    }
    projected.insert("_golutra_truncated".to_owned(), Value::Bool(true));
    Value::Object(projected)
}

fn compact_tool_argument_value(value: &Value, key: Option<&str>) -> Value {
    if key.is_some_and(payload_argument_key) {
        return omitted_argument_summary(value);
    }
    match value {
        Value::String(text) => Value::String(bounded_argument_string(
            text,
            MAX_TOOL_ARGUMENT_COMPACT_STRING_BYTES,
        )),
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
        Value::Array(_) | Value::Object(_) => omitted_argument_summary(value),
    }
}

fn omitted_argument_summary(value: &Value) -> Value {
    let summary = match value {
        Value::String(text) => format!("<omitted {} bytes>", text.len()),
        Value::Array(values) => format!("<omitted array with {} items>", values.len()),
        Value::Object(values) => format!("<omitted object with {} fields>", values.len()),
        Value::Null | Value::Bool(_) | Value::Number(_) => "<omitted value>".to_owned(),
    };
    Value::String(summary)
}

fn payload_argument_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().replace('-', "_").as_str(),
        "body"
            | "content"
            | "data"
            | "input"
            | "new_text"
            | "old_text"
            | "patch"
            | "payload"
            | "replace"
            | "replacement"
            | "search"
    )
}

fn bounded_argument_string(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut boundary = max_bytes.min(value.len());
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!(
        "{}<truncated {} bytes>",
        &value[..boundary],
        value.len().saturating_sub(boundary)
    )
}

fn serialized_value_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

fn redact_sensitive_value_in_place(value: &mut Value, parent_is_sensitive: bool) {
    if parent_is_sensitive {
        *value = Value::String("<redacted-secret>".to_owned());
        return;
    }
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                redact_sensitive_value_in_place(value, sensitive_json_key(key));
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_sensitive_value_in_place(value, false);
            }
        }
        Value::String(text) => {
            *text = redact_sensitive_text(text).0;
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn sensitive_json_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    let collapsed = normalized.replace('_', "");
    matches!(
        normalized.as_str(),
        "api_key" | "authorization" | "token" | "secret" | "password"
    ) || ["_api_key", "_token", "_secret", "_password"]
        .iter()
        .any(|suffix| normalized.ends_with(suffix))
        || ["apikey", "token", "secret", "password"]
            .iter()
            .any(|suffix| collapsed.ends_with(suffix))
}

async fn read_optional_file(path: &Path) -> Result<FileBeforeImage, ToolError> {
    let path_metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FileBeforeImage {
                path: path.to_path_buf(),
                content: None,
                unix_mode: None,
                metadata: None,
            });
        }
        Err(error) => return Err(ToolError::Execution(error.to_string())),
    };

    if path_metadata.file_type().is_symlink() {
        let target = tokio::fs::read_link(path)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let content = target.as_os_str().as_encoded_bytes().to_vec();
        if content.len() as u64 > MAX_FILE_CONTENT_BYTES {
            return Err(ToolError::Execution(format!(
                "symlink target {} exceeds {MAX_FILE_CONTENT_BYTES} byte limit",
                path.display()
            )));
        }
        let unix_mode = unix_mode(&path_metadata);
        let file_metadata = file_state_metadata(&content, unix_mode, true);
        return Ok(FileBeforeImage {
            path: path.to_path_buf(),
            content: Some(content),
            unix_mode,
            metadata: Some(file_metadata),
        });
    }

    if !path_metadata.file_type().is_file() {
        return Err(ToolError::Execution(format!(
            "path {} is not a regular file",
            path.display()
        )));
    }

    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    #[cfg(windows)]
    options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    let mut file = options
        .open(path)
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    if !metadata.file_type().is_file()
        || !workspace_scan::same_file_identity(&path_metadata, &metadata)
    {
        return Err(ToolError::Execution(format!(
            "path {} changed type or identity while opening",
            path.display()
        )));
    }
    if metadata.len() > MAX_FILE_CONTENT_BYTES {
        return Err(ToolError::Execution(format!(
            "file {} exceeds {MAX_FILE_CONTENT_BYTES} byte limit",
            path.display()
        )));
    }
    let mut content = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    let read_limit = MAX_FILE_CONTENT_BYTES.saturating_add(1);
    (&mut file)
        .take(read_limit)
        .read_to_end(&mut content)
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    if content.len() as u64 > MAX_FILE_CONTENT_BYTES {
        return Err(ToolError::Execution(format!(
            "file {} grew beyond {MAX_FILE_CONTENT_BYTES} byte limit while reading",
            path.display()
        )));
    }
    let final_metadata = file
        .metadata()
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let current_metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    if !current_metadata.file_type().is_file()
        || !workspace_scan::same_file_state(&metadata, &final_metadata)
        || !workspace_scan::same_file_state(&final_metadata, &current_metadata)
        || final_metadata.len() != content.len() as u64
    {
        return Err(ToolError::Execution(format!(
            "file {} changed while reading",
            path.display()
        )));
    }
    let unix_mode = unix_mode(&final_metadata);
    let file_metadata = file_state_metadata(&content, unix_mode, true);
    Ok(FileBeforeImage {
        path: path.to_path_buf(),
        content: Some(content),
        unix_mode,
        metadata: Some(file_metadata),
    })
}

fn validate_checkpoint_content_limit(images: &[FileBeforeImage]) -> Result<(), ToolError> {
    let retained_bytes = images
        .iter()
        .map(file_image_content_bytes)
        .fold(0_usize, usize::saturating_add);
    if retained_bytes > MAX_WORKSPACE_SNAPSHOT_CONTENT_BYTES {
        return Err(ToolError::InvalidArguments(format!(
            "side-effect checkpoint exceeds {MAX_WORKSPACE_SNAPSHOT_CONTENT_BYTES} retained bytes"
        )));
    }
    Ok(())
}

fn file_image_content_bytes(image: &FileBeforeImage) -> usize {
    image.content.as_ref().map_or(0, Vec::len)
}

fn file_state_metadata(
    bytes: &[u8],
    unix_mode: Option<u32>,
    content_available: bool,
) -> FileStateMetadata {
    FileStateMetadata {
        size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        checksum: Some(format!("sha256:{:x}", Sha256::digest(bytes))),
        unix_mode,
        content_kind: if std::str::from_utf8(bytes).is_ok() && !bytes.contains(&0) {
            FileContentKind::Text
        } else {
            FileContentKind::Binary
        },
        content_available,
    }
}

async fn before_image_still_current(
    path: &Path,
    before_images: &[FileBeforeImage],
) -> Result<bool, ToolError> {
    let Some(expected) = before_images
        .iter()
        .find(|before_image| before_image.path == path)
    else {
        return Ok(before_images.is_empty());
    };
    let current = read_optional_file(path).await?;
    Ok(current.content == expected.content && current.unix_mode == expected.unix_mode)
}

async fn file_changes_since(
    before_images: &[FileBeforeImage],
) -> Result<(Vec<PathBuf>, Vec<FileBeforeImage>), ToolError> {
    let mut changed_files = Vec::new();
    let mut after_images = Vec::with_capacity(before_images.len());
    for before_image in before_images {
        let after_image = read_optional_file(&before_image.path).await?;
        if after_image.content != before_image.content
            || after_image.unix_mode != before_image.unix_mode
        {
            changed_files.push(before_image.path.clone());
        }
        after_images.push(after_image);
    }
    Ok((changed_files, after_images))
}

#[cfg(unix)]
fn unix_mode(metadata: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;

    Some(metadata.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn unix_mode(_metadata: &std::fs::Metadata) -> Option<u32> {
    None
}

fn artifact_for(
    request: &ToolRequest,
    raw_output: &str,
    redaction_status: RedactionStatus,
) -> ArtifactRecord {
    let artifact_id = ArtifactId::new();
    ArtifactRecord {
        artifact_id,
        session_id: request.session_id,
        turn_id: request.turn_id,
        tool_call_id: Some(request.tool_call_id),
        artifact_type: format!("tool_raw_output:{}", request.tool_name),
        uri: format!("artifact://tool/{}/{artifact_id}", request.tool_name),
        checksum: checksum(raw_output.as_bytes()),
        size_bytes: raw_output.len() as u64,
        created_at: chrono::Utc::now(),
        producer: request.tool_name.clone(),
        redaction_status,
        retention_policy: "p0_default".to_owned(),
        provenance_refs: Vec::new(),
    }
}

fn string_arg(arguments: &Value, key: &str) -> Result<String, ToolError> {
    optional_string_arg(arguments, key)
        .ok_or_else(|| ToolError::InvalidArguments(format!("missing string argument `{key}`")))
}

fn shell_command_for_request(arguments: &Value) -> Result<String, ToolError> {
    let command = optional_string_arg(arguments, "command");
    let argv = arguments
        .get("argv")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    item.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                        ToolError::InvalidArguments("shell argv entries must be strings".to_owned())
                    })
                })
                .collect::<Result<Vec<_>, _>>()
        });
    let argv = match argv {
        Some(result) => Some(result?),
        None if arguments.get("argv").is_some() => {
            return Err(ToolError::InvalidArguments(
                "shell argv must be an array".to_owned(),
            ));
        }
        None => None,
    };

    let Some(argv) = argv else {
        let command = command.ok_or_else(|| {
            ToolError::InvalidArguments("shell requires command or argv".to_owned())
        })?;
        if command.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "shell command cannot be empty".to_owned(),
            ));
        }
        return Ok(command);
    };
    if argv.is_empty() || argv.len() > MAX_SHELL_ARGV_ITEMS {
        return Err(ToolError::InvalidArguments(format!(
            "shell argv must contain between 1 and {MAX_SHELL_ARGV_ITEMS} entries"
        )));
    }
    if argv
        .iter()
        .any(|argument| argument.is_empty() || argument.contains('\0'))
    {
        return Err(ToolError::InvalidArguments(
            "shell argv entries must be non-empty and cannot contain NUL bytes".to_owned(),
        ));
    }
    let canonical = argv
        .iter()
        .map(|argument| shell_quote_argv_item(argument))
        .collect::<Vec<_>>()
        .join(" ");
    if canonical.len() > MAX_SHELL_COMMAND_CHARS {
        return Err(ToolError::InvalidArguments(format!(
            "shell argv exceeds {MAX_SHELL_COMMAND_CHARS} encoded bytes"
        )));
    }

    if let Some(command) = command {
        if command.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "shell command cannot be empty when argv is present".to_owned(),
            ));
        }
        let compatible = match golutra_policy::parse_shell_command_with_input(&command) {
            Some(parsed) if parsed.stdin.is_none() => argv.starts_with(&parsed.parts),
            // 格式错误的重复命令只有在它恰好等于 argv 提供的可执行文件名时才无害；
            // 其他冲突都必须显式报错，不能静默改变请求的任务。
            None => command.trim() == argv[0],
            Some(_) => false,
        };
        if !compatible {
            return Err(ToolError::InvalidArguments(
                "shell command and argv describe different commands".to_owned(),
            ));
        }
    }
    Ok(canonical)
}

/// Return whether a shell request can skip opaque workspace mutation tracking.
///
/// The request-level check keeps command/argv normalization in one place and
/// adds the execution flags that are not part of the parsed argv contract.
/// A false result intentionally falls back to the slower, fully observable
/// process path.
#[must_use]
pub fn shell_request_is_strictly_read_only(arguments: &Value) -> bool {
    if arguments
        .get("background")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return false;
    }
    let Ok(command) = shell_command_for_request(arguments) else {
        return false;
    };
    let Some(parsed) = parse_shell_command_with_input(&command) else {
        return false;
    };
    parsed.stdin.is_none()
        && explicit_shell_script(&parsed.parts).is_none()
        && !contains_shell_metacharacter(&command)
        && shell_command_is_strictly_read_only(&parsed.parts)
}

fn shell_quote_argv_item(argument: &str) -> String {
    if argument.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'_' | b'+' | b'-' | b'=' | b'.' | b'/' | b'@' | b'%' | b':'
            )
    }) {
        return argument.to_owned();
    }
    format!("'{}'", argument.replace('\'', "'\"'\"'"))
}

fn optional_string_arg(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn execution_policy(
    request: &ToolRequest,
    decision: PolicyDecision,
    reason: &str,
) -> PolicyEvaluation {
    PolicyEvaluation {
        policy_ref: golutra_core::PolicyId::new(),
        subject: "tool".to_owned(),
        action: request.tool_name.clone(),
        resource: bounded_text(
            &redact_sensitive_text(&request.arguments.to_string()).0,
            MAX_AUDIT_RESOURCE_CHARS,
        ),
        decision,
        block_disposition: None,
        reason: reason.to_owned(),
        evidence_refs: Vec::new(),
    }
}

#[must_use]
pub fn redact_sensitive_text(raw_output: &str) -> (String, RedactionStatus) {
    static QUOTED_SECRET: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)(["']?(?:api[_-]?key|authorization|access[_-]?token|token|secret|password)["']?\s*:\s*["'])([^"'\r\n]*)(["'])"#,
        )
        .expect("secret redaction regex is valid")
    });
    static AUTHORIZATION_HEADER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?im)(\bauthorization\s*:\s*)([^\r\n]+)")
            .expect("authorization redaction regex is valid")
    });
    static SECRET_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)(\b(?:api[_-]?key|authorization|access[_-]?token|token|secret|password)\b\s*=\s*)([^\s,;]+)",
        )
        .expect("secret assignment regex is valid")
    });
    static PREFIXED_SECRET: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:sk-[a-z0-9_-]{9,}|ghp_[a-z0-9_-]{8,}|github_pat_[a-z0-9_-]{8,}|xox[bp]-[a-z0-9_-]{8,})")
            .expect("prefixed secret redaction regex is valid")
    });

    let redacted = QUOTED_SECRET.replace_all(raw_output, "$1<redacted-secret>$3");
    let redacted = AUTHORIZATION_HEADER.replace_all(&redacted, "$1<redacted-secret>");
    let redacted = SECRET_ASSIGNMENT.replace_all(&redacted, "$1<redacted-secret>");
    let redacted = PREFIXED_SECRET.replace_all(&redacted, "<redacted-secret>");
    let redacted = redacted.into_owned();
    let status = if redacted != raw_output {
        RedactionStatus::Redacted
    } else {
        RedactionStatus::NotRequired
    };
    (redacted, status)
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

/// 在 UTF-8 字节边界上截断 provider 可见文本。这里不追加省略后缀，
/// 因为调用方通常还要按序列化后的 JSON 总大小继续收缩；后缀会让预算
/// 计算产生漂移，也可能把本应保留的状态事实挤出去。
fn bounded_text_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut boundary = max_bytes.min(value.len());
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value[..boundary].to_owned()
}

fn excerpt(raw_output: &str, limit: usize) -> String {
    raw_output.chars().take(limit).collect()
}

fn checksum(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

#[cfg(test)]
mod tests;
