use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use golutra_core::{
    ArtifactId, ArtifactRecord, EvidenceRecord, EvidenceStrength, PolicyDecision, PolicyEvaluation,
    RedactionStatus, SessionId, SideEffectType, ToolCallId, ToolContract, ToolResultEnvelope,
    ToolResultStatus, TurnId,
};
use golutra_policy::WorkspacePolicy;
#[cfg(unix)]
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{io::AsyncReadExt, process::Command, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const DEFAULT_EXCERPT_LIMIT: usize = 2048;
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_PIPE_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool is not registered: {0}")]
    UnknownTool(String),
    #[error("tool arguments are invalid: {0}")]
    InvalidArguments(String),
    #[error("tool execution failed: {0}")]
    Execution(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolRequest {
    pub tool_call_id: ToolCallId,
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
}

#[derive(Debug, Clone)]
pub struct ToolRegistry {
    contracts: HashMap<String, ToolContract>,
}

impl ToolRegistry {
    #[must_use]
    pub fn p0_default() -> Self {
        let contracts = [
            contract("read_file", SideEffectType::None),
            contract("write_file", SideEffectType::File),
            contract("edit_file", SideEffectType::File),
            contract("list_dir", SideEffectType::None),
            contract("rg_search", SideEffectType::Process),
            contract("shell", SideEffectType::Process),
        ]
        .into_iter()
        .map(|contract| (contract.tool_name.clone(), contract))
        .collect();
        Self { contracts }
    }

    #[must_use]
    pub fn contracts(&self) -> Vec<&ToolContract> {
        let mut contracts = self.contracts.values().collect::<Vec<_>>();
        contracts.sort_by(|left, right| left.tool_name.cmp(&right.tool_name));
        contracts
    }

    #[must_use]
    pub fn contract(&self, tool_name: &str) -> Option<&ToolContract> {
        self.contracts.get(tool_name)
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::p0_default()
    }
}

#[derive(Debug, Clone)]
pub struct BasicToolExecutor {
    policy: WorkspacePolicy,
    registry: ToolRegistry,
}

impl BasicToolExecutor {
    #[must_use]
    pub fn new(policy: WorkspacePolicy) -> Self {
        Self {
            policy,
            registry: ToolRegistry::p0_default(),
        }
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

    pub fn evaluate(&self, request: &ToolRequest) -> Result<PolicyEvaluation, ToolError> {
        let contract = self
            .registry
            .contract(&request.tool_name)
            .ok_or_else(|| ToolError::UnknownTool(request.tool_name.clone()))?;
        validate_tool_arguments(contract, &request.arguments)?;

        let policy = match request.tool_name.as_str() {
            "read_file" => self.policy.evaluate_path(
                "read_file",
                string_arg(&request.arguments, "path")?,
                true,
            ),
            "write_file" => self.policy.evaluate_path(
                "write_file",
                string_arg(&request.arguments, "path")?,
                false,
            ),
            "edit_file" => self.policy.evaluate_path(
                "edit_file",
                string_arg(&request.arguments, "path")?,
                true,
            ),
            "list_dir" => self.policy.evaluate_path(
                "list_dir",
                optional_string_arg(&request.arguments, "path").unwrap_or_else(|| ".".to_owned()),
                true,
            ),
            "rg_search" => self.policy.evaluate_path(
                "rg_search",
                optional_string_arg(&request.arguments, "path").unwrap_or_else(|| ".".to_owned()),
                true,
            ),
            "shell" => self
                .policy
                .evaluate_shell(&string_arg(&request.arguments, "command")?),
            _ => return Err(ToolError::UnknownTool(request.tool_name.clone())),
        };
        Ok(policy)
    }

    pub async fn prepare_side_effect(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<FileBeforeImage>, ToolError> {
        let contract = self
            .registry
            .contract(&request.tool_name)
            .ok_or_else(|| ToolError::UnknownTool(request.tool_name.clone()))?;
        validate_tool_arguments(contract, &request.arguments)?;

        match request.tool_name.as_str() {
            "write_file" => {
                let path = string_arg(&request.arguments, "path")?;
                let resolved_path = self
                    .policy
                    .resolve_path(&path, false)
                    .map_err(|error| ToolError::Execution(error.to_string()))?;
                Ok(vec![read_optional_file(&resolved_path).await?])
            }
            "edit_file" => {
                let path = string_arg(&request.arguments, "path")?;
                let search = string_arg(&request.arguments, "search")?;
                let resolved_path = self
                    .policy
                    .resolve_path(&path, true)
                    .map_err(|error| ToolError::Execution(error.to_string()))?;
                let before_image = read_optional_file(&resolved_path).await?;
                let original = before_image
                    .content
                    .as_deref()
                    .ok_or_else(|| ToolError::Execution("edit target does not exist".to_owned()))?;
                let original = std::str::from_utf8(original)
                    .map_err(|error| ToolError::Execution(error.to_string()))?;
                if original.contains(&search) {
                    Ok(vec![before_image])
                } else {
                    Ok(Vec::new())
                }
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
        let may_execute = !cancellation.is_cancelled()
            && match policy.decision {
                PolicyDecision::Allow => true,
                PolicyDecision::Ask => approved,
                PolicyDecision::Deny | PolicyDecision::Block => false,
            };
        let before_images = if may_execute {
            self.prepare_side_effect(&request).await?
        } else {
            Vec::new()
        };
        self.execute_with_policy_and_before_images(
            request,
            policy,
            approved,
            cancellation,
            before_images,
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
        let contract = self
            .registry
            .contract(&request.tool_name)
            .ok_or_else(|| ToolError::UnknownTool(request.tool_name.clone()))?;
        validate_tool_arguments(contract, &request.arguments)?;
        if cancellation.is_cancelled() {
            return Ok(cancelled_report(
                request,
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
                return Ok(blocked_report(request, policy));
            }
        }

        match request.tool_name.as_str() {
            "read_file" => self.read_file(request, policy).await,
            "write_file" => self.write_file(request, policy, before_images).await,
            "edit_file" => self.edit_file(request, policy, before_images).await,
            "list_dir" => self.list_dir(request, policy).await,
            "rg_search" => self.rg_search(request, policy, cancellation).await,
            "shell" => self.shell(request, policy, cancellation).await,
            _ => unreachable!("registered tool was checked before dispatch"),
        }
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
        let resolved_path = self
            .policy
            .resolve_path(&path, true)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let content = tokio::fs::read_to_string(&resolved_path)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        Ok(success_report(
            request,
            "file read",
            json!({"path": resolved_path, "bytes": content.len()}),
            content,
            Vec::new(),
            policy,
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
        let resolved_path = self
            .policy
            .resolve_path(&path, false)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        tokio::fs::write(&resolved_path, content.as_bytes())
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let mut report = success_report(
            request,
            "file written",
            json!({"path": resolved_path, "bytes": content.len()}),
            content,
            vec![resolved_path],
            policy,
        );
        report.before_images = before_images;
        Ok(report)
    }

    async fn edit_file(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        before_images: Vec<FileBeforeImage>,
    ) -> Result<ToolExecutionReport, ToolError> {
        let path = string_arg(&request.arguments, "path")?;
        let search = string_arg(&request.arguments, "search")?;
        let replace = string_arg(&request.arguments, "replace")?;
        let resolved_path = self
            .policy
            .resolve_path(&path, true)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
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
        if !original.contains(&search) {
            return Ok(error_report(
                request,
                "edit target not found",
                json!({"path": resolved_path, "search": search}),
                original,
                policy,
            ));
        }
        let edited = original.replacen(&search, &replace, 1);
        tokio::fs::write(&resolved_path, edited.as_bytes())
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let mut report = success_report(
            request,
            "file edited",
            json!({"path": resolved_path, "replacements": 1}),
            edited,
            vec![resolved_path],
            policy,
        );
        report.before_images = before_images;
        Ok(report)
    }

    async fn list_dir(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
    ) -> Result<ToolExecutionReport, ToolError> {
        let path =
            optional_string_arg(&request.arguments, "path").unwrap_or_else(|| ".".to_owned());
        let resolved_path = self
            .policy
            .resolve_path(&path, true)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let entries = directory_entries(&resolved_path).await?;
        Ok(success_report(
            request,
            "directory listed",
            json!({"path": resolved_path, "entries": entries}),
            entries.join("\n"),
            Vec::new(),
            policy,
        ))
    }

    async fn rg_search(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        cancellation: CancellationToken,
    ) -> Result<ToolExecutionReport, ToolError> {
        let pattern = string_arg(&request.arguments, "pattern")?;
        let path =
            optional_string_arg(&request.arguments, "path").unwrap_or_else(|| ".".to_owned());
        let resolved_path = self
            .policy
            .resolve_path(&path, true)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let output = run_process(
            "rg",
            &[
                "--line-number".to_owned(),
                "--no-heading".to_owned(),
                pattern.clone(),
                resolved_path.display().to_string(),
            ],
            self.policy.workspace_root(),
            DEFAULT_TIMEOUT_MS,
            cancellation,
        )
        .await?;
        let status = if output.cancelled {
            ToolResultStatus::Cancelled
        } else if output.timed_out {
            ToolResultStatus::Timeout
        } else if output.exit_code == Some(0) || output.exit_code == Some(1) {
            ToolResultStatus::Ok
        } else {
            ToolResultStatus::Error
        };
        Ok(report(
            request,
            status,
            "rg search completed",
            json!({"path": resolved_path, "pattern": pattern, "exit_code": output.exit_code}),
            output.raw_output,
            Vec::new(),
            policy,
        ))
    }

    async fn shell(
        &self,
        request: ToolRequest,
        policy: PolicyEvaluation,
        cancellation: CancellationToken,
    ) -> Result<ToolExecutionReport, ToolError> {
        let command = string_arg(&request.arguments, "command")?;
        let timeout_ms = request
            .arguments
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        let command_line = CommandLine::parse(&command)?;
        let shell_output = run_process(
            &command_line.program,
            &command_line.args,
            self.policy.workspace_root(),
            timeout_ms.min(30_000),
            cancellation,
        )
        .await?;
        let status = if shell_output.cancelled {
            ToolResultStatus::Cancelled
        } else if shell_output.timed_out {
            ToolResultStatus::Timeout
        } else if shell_output.exit_code == Some(0) {
            ToolResultStatus::Ok
        } else {
            ToolResultStatus::Error
        };
        Ok(report(
            request,
            status,
            "shell command completed",
            json!({
                "command": command,
                "exit_code": shell_output.exit_code,
                "timed_out": shell_output.timed_out,
                "cancelled": shell_output.cancelled,
            }),
            shell_output.raw_output,
            Vec::new(),
            policy,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellOutput {
    exit_code: Option<i32>,
    timed_out: bool,
    cancelled: bool,
    raw_output: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandLine {
    program: String,
    args: Vec<String>,
}

impl CommandLine {
    fn parse(command: &str) -> Result<Self, ToolError> {
        let mut parts = shlex::split(command).ok_or_else(|| {
            ToolError::InvalidArguments("shell command contains invalid quoting".to_owned())
        })?;
        if parts.is_empty() {
            return Err(ToolError::InvalidArguments(
                "shell command cannot be empty".to_owned(),
            ));
        }
        let program = parts.remove(0);
        Ok(Self {
            program,
            args: parts,
        })
    }
}

async fn run_process(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout_ms: u64,
    cancellation: CancellationToken,
) -> Result<ShellOutput, ToolError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::Execution("process stdout pipe is unavailable".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::Execution("process stderr pipe is unavailable".to_owned()))?;
    let stdout_reader = spawn_pipe_reader(stdout);
    let stderr_reader = spawn_pipe_reader(stderr);
    let timeout = tokio::time::sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(timeout);

    let (status, timed_out, cancelled) = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            terminate_process_tree(&mut child);
            let status = child.wait().await
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            (status, false, true)
        }
        _ = &mut timeout => {
            terminate_process_tree(&mut child);
            let status = child.wait().await
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            (status, true, false)
        }
        status = child.wait() => {
            (
                status.map_err(|error| ToolError::Execution(error.to_string()))?,
                false,
                false,
            )
        }
    };
    let stdout = join_pipe_reader(stdout_reader).await?;
    let stderr = join_pipe_reader(stderr_reader).await?;
    let raw_output = if stderr.is_empty() {
        stdout
    } else {
        format!("{stdout}\n{stderr}")
    };
    Ok(ShellOutput {
        exit_code: status.code(),
        timed_out,
        cancelled,
        raw_output,
    })
}

fn terminate_process_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(process_id) = child.id().and_then(|id| i32::try_from(id).ok()) {
        let _ = killpg(Pid::from_raw(process_id), Signal::SIGKILL);
    }
    let _ = child.start_kill();
}

fn spawn_pipe_reader<R>(mut reader: R) -> JoinHandle<std::io::Result<String>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut bytes = Vec::with_capacity(MAX_PIPE_OUTPUT_BYTES.min(64 * 1024));
        let mut buffer = [0_u8; 8192];
        let mut truncated = false;
        loop {
            let read = reader.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            let remaining = MAX_PIPE_OUTPUT_BYTES.saturating_sub(bytes.len());
            let retained = remaining.min(read);
            bytes.extend_from_slice(&buffer[..retained]);
            truncated |= retained < read;
        }
        if truncated {
            bytes.extend_from_slice(b"\n[process output truncated]\n");
        }
        Ok(String::from_utf8_lossy(&bytes).to_string())
    })
}

async fn join_pipe_reader(
    reader: JoinHandle<std::io::Result<String>>,
) -> Result<String, ToolError> {
    reader
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?
        .map_err(|error| ToolError::Execution(error.to_string()))
}

async fn directory_entries(path: &Path) -> Result<Vec<String>, ToolError> {
    let mut directory = tokio::fs::read_dir(path)
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let mut entries = Vec::new();
    while let Some(entry) = directory
        .next_entry()
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?
    {
        entries.push(entry.file_name().to_string_lossy().to_string());
    }
    entries.sort();
    Ok(entries)
}

fn contract(tool_name: &str, side_effect_type: SideEffectType) -> ToolContract {
    let input_schema = match tool_name {
        "read_file" => object_schema(&[("path", "string")], &["path"]),
        "write_file" => object_schema(
            &[("path", "string"), ("content", "string")],
            &["path", "content"],
        ),
        "edit_file" => object_schema(
            &[
                ("path", "string"),
                ("search", "string"),
                ("replace", "string"),
            ],
            &["path", "search", "replace"],
        ),
        "list_dir" => object_schema(&[("path", "string")], &[]),
        "rg_search" => object_schema(&[("pattern", "string"), ("path", "string")], &["pattern"]),
        "shell" => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "command": {"type": "string", "minLength": 1},
                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 30000}
            },
            "required": ["command"]
        }),
        _ => json!({"type": "object", "additionalProperties": false}),
    };
    ToolContract {
        tool_name: tool_name.to_owned(),
        input_schema,
        output_schema: json!({
            "type": "object",
            "additionalProperties": true,
            "required": ["status", "summary"]
        }),
        error_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "code": {"type": "string"},
                "message": {"type": "string"}
            },
            "required": ["code", "message"]
        }),
        side_effect_type,
        idempotency_key_policy: match side_effect_type {
            SideEffectType::None => "not_required",
            SideEffectType::File | SideEffectType::Process => "required_for_retry",
            SideEffectType::Network | SideEffectType::ExternalSystem => "blocked_in_p0",
        }
        .to_owned(),
        timeout_policy: "bounded_by_tool_or_default_timeout".to_owned(),
        cancellation_policy: "returns_cancelled_envelope".to_owned(),
        retry_policy: "no_implicit_retry_for_side_effects".to_owned(),
        artifact_policy: "raw_output_to_artifact_ref".to_owned(),
        permission_policy_ref: None,
    }
}

fn object_schema(properties: &[(&str, &str)], required: &[&str]) -> Value {
    let properties = properties
        .iter()
        .map(|(name, value_type)| ((*name).to_owned(), json!({"type": value_type})))
        .collect::<serde_json::Map<_, _>>();
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
        "required": required,
    })
}

fn validate_tool_arguments(contract: &ToolContract, arguments: &Value) -> Result<(), ToolError> {
    let validator = jsonschema::validator_for(&contract.input_schema).map_err(|error| {
        ToolError::InvalidArguments(format!(
            "tool `{}` has an invalid input schema: {error}",
            contract.tool_name
        ))
    })?;
    let errors = validator
        .iter_errors(arguments)
        .map(|error| error.to_string())
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

fn cancelled_report(request: ToolRequest, reason: &str) -> ToolExecutionReport {
    let policy_evaluation = execution_policy(&request, PolicyDecision::Allow, reason);
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

fn report(
    request: ToolRequest,
    status: ToolResultStatus,
    summary: &str,
    structured_facts: Value,
    raw_output: String,
    changed_files: Vec<PathBuf>,
    policy_evaluation: PolicyEvaluation,
) -> ToolExecutionReport {
    let (redacted_output, redaction_status) = redact_output(&raw_output);
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
    let envelope = ToolResultEnvelope {
        tool_call_id: request.tool_call_id,
        tool_name: request.tool_name,
        status,
        summary: summary.to_owned(),
        structured_facts,
        model_visible_excerpt: Some(excerpt(&redacted_output, DEFAULT_EXCERPT_LIMIT)),
        raw_artifact_ref: Some(artifact.artifact_id),
        evidence_refs: vec![evidence.evidence_id],
        risk: "p0_local_tool".to_owned(),
        verification_hint: Some("use artifact/evidence refs for verification".to_owned()),
    };
    let artifact_id = artifact.artifact_id;

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
    }
}

async fn read_optional_file(path: &Path) -> Result<FileBeforeImage, ToolError> {
    match tokio::fs::read(path).await {
        Ok(content) => Ok(FileBeforeImage {
            path: path.to_path_buf(),
            content: Some(content),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FileBeforeImage {
            path: path.to_path_buf(),
            content: None,
        }),
        Err(error) => Err(ToolError::Execution(error.to_string())),
    }
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
        resource: request.arguments.to_string(),
        decision,
        reason: reason.to_owned(),
        evidence_refs: Vec::new(),
    }
}

fn redact_output(raw_output: &str) -> (String, RedactionStatus) {
    let mut changed = false;
    let redacted = raw_output
        .split_inclusive(char::is_whitespace)
        .map(|part| {
            let trimmed = part.trim_end_matches(char::is_whitespace);
            let whitespace = &part[trimmed.len()..];
            let replacement = redact_secret_token(trimmed);
            changed |= replacement != trimmed;
            format!("{replacement}{whitespace}")
        })
        .collect::<String>();
    let status = if changed {
        RedactionStatus::Redacted
    } else {
        RedactionStatus::NotRequired
    };
    (redacted, status)
}

fn redact_secret_token(token: &str) -> &str {
    let normalized = token
        .trim_matches(|character: char| {
            !character.is_ascii_alphanumeric()
                && character != '-'
                && character != '_'
                && character != '='
        })
        .to_ascii_lowercase();
    let looks_like_prefixed_secret = normalized.starts_with("sk-")
        || normalized.starts_with("ghp_")
        || normalized.starts_with("github_pat_")
        || normalized.starts_with("xoxb-")
        || normalized.starts_with("xoxp-");
    let looks_like_assignment = normalized.split_once('=').is_some_and(|(name, value)| {
        !value.is_empty()
            && ["key", "token", "secret", "password"]
                .iter()
                .any(|marker| name.contains(marker))
    });
    if (looks_like_prefixed_secret && normalized.len() >= 12) || looks_like_assignment {
        "<redacted-secret>"
    } else {
        token
    }
}

fn excerpt(raw_output: &str, limit: usize) -> String {
    raw_output.chars().take(limit).collect()
}

fn checksum(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use golutra_policy::WorkspacePolicy;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn registry_contains_p0_tools() {
        let registry = ToolRegistry::p0_default();
        let names = registry
            .contracts()
            .into_iter()
            .map(|contract| contract.tool_name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "edit_file",
                "list_dir",
                "read_file",
                "rg_search",
                "shell",
                "write_file"
            ]
        );
    }

    #[tokio::test]
    async fn read_file_returns_envelope_artifact_and_evidence() {
        let workspace = tempdir().expect("workspace");
        fs::write(workspace.path().join("README.md"), "hello").expect("fixture");
        let executor = executor(workspace.path());

        let report = executor
            .execute(
                request("read_file", json!({"path": "README.md"})),
                CancellationToken::new(),
            )
            .await
            .expect("tool runs");

        assert_eq!(report.envelope.status, ToolResultStatus::Ok);
        assert_eq!(report.artifacts.len(), 1);
        assert_eq!(report.evidence.len(), 1);
    }

    #[tokio::test]
    async fn write_file_records_changed_file() {
        let workspace = tempdir().expect("workspace");
        let executor = executor(workspace.path());

        let report = executor
            .execute(
                request("write_file", json!({"path": "src.txt", "content": "new"})),
                CancellationToken::new(),
            )
            .await
            .expect("tool runs");

        assert_eq!(report.envelope.status, ToolResultStatus::Ok);
        assert_eq!(report.changed_files.len(), 1);
        assert_eq!(
            fs::read_to_string(workspace.path().join("src.txt")).unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn blocks_workspace_escape() {
        let workspace = tempdir().expect("workspace");
        let outside = tempdir().expect("outside");
        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, "secret").expect("fixture");
        let executor = executor(workspace.path());

        let report = executor
            .execute(
                request("read_file", json!({"path": outside_file.to_string_lossy()})),
                CancellationToken::new(),
            )
            .await
            .expect("tool runs");

        assert_eq!(report.envelope.status, ToolResultStatus::Blocked);
    }

    #[tokio::test]
    async fn shell_rejects_metacharacters_before_execution() {
        let workspace = tempdir().expect("workspace");
        let executor = executor(workspace.path());

        let report = executor
            .execute(
                request("shell", json!({"command": "echo ok; cat .env"})),
                CancellationToken::new(),
            )
            .await
            .expect("tool runs");

        assert_eq!(report.envelope.status, ToolResultStatus::Blocked);
    }

    #[tokio::test]
    async fn shell_runs_simple_command_without_shell_interpreter() {
        let workspace = tempdir().expect("workspace");
        let executor = executor(workspace.path());

        let report = execute_approved(
            &executor,
            request("shell", json!({"command": "echo ok"})),
            CancellationToken::new(),
        )
        .await;

        assert_eq!(report.envelope.status, ToolResultStatus::Ok);
        assert_eq!(
            report.envelope.model_visible_excerpt.as_deref(),
            Some("ok\n")
        );
    }

    #[tokio::test]
    async fn shell_parser_preserves_quoted_arguments() {
        let workspace = tempdir().expect("workspace");
        let executor = executor(workspace.path());

        let report = execute_approved(
            &executor,
            request("shell", json!({"command": "printf '%s' 'hello world'"})),
            CancellationToken::new(),
        )
        .await;

        assert_eq!(report.envelope.status, ToolResultStatus::Ok);
        assert_eq!(
            report.envelope.model_visible_excerpt.as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn shell_parser_rejects_unclosed_quotes() {
        assert!(matches!(
            CommandLine::parse("printf 'unterminated"),
            Err(ToolError::InvalidArguments(_))
        ));
    }

    #[tokio::test]
    async fn shell_timeout_and_cancellation_have_distinct_statuses() {
        let workspace = tempdir().expect("workspace");
        let executor = executor(workspace.path());
        let timed_out = execute_approved(
            &executor,
            request("shell", json!({"command": "sleep 1", "timeout_ms": 20})),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(timed_out.envelope.status, ToolResultStatus::Timeout);

        let cancellation = CancellationToken::new();
        let cancel_from_task = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel_from_task.cancel();
        });
        let cancelled = execute_approved(
            &executor,
            request("shell", json!({"command": "sleep 1"})),
            cancellation,
        )
        .await;
        assert_eq!(cancelled.envelope.status, ToolResultStatus::Cancelled);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_cancellation_terminates_descendants_and_drains_pipes() {
        let workspace = tempdir().expect("workspace");
        let cancellation = CancellationToken::new();
        let cancel_from_task = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel_from_task.cancel();
        });

        let output = tokio::time::timeout(
            Duration::from_secs(1),
            run_process(
                "sh",
                &["-c".to_owned(), "sleep 5 & wait".to_owned()],
                workspace.path(),
                DEFAULT_TIMEOUT_MS,
                cancellation,
            ),
        )
        .await
        .expect("process tree terminates promptly")
        .expect("process result");

        assert!(output.cancelled);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_output_is_bounded_while_pipe_is_drained() {
        let workspace = tempdir().expect("workspace");
        let output = run_process(
            "sh",
            &["-c".to_owned(), "yes output".to_owned()],
            workspace.path(),
            20,
            CancellationToken::new(),
        )
        .await
        .expect("process result");

        assert!(output.timed_out);
        assert!(output.raw_output.len() <= MAX_PIPE_OUTPUT_BYTES * 2 + 128);
        assert!(output.raw_output.contains("[process output truncated]"));
    }

    #[tokio::test]
    async fn rejects_arguments_that_do_not_match_tool_contract() {
        let workspace = tempdir().expect("workspace");
        let executor = executor(workspace.path());

        let error = executor
            .execute(
                request("write_file", json!({"path": "src.txt", "unexpected": true})),
                CancellationToken::new(),
            )
            .await
            .expect_err("invalid arguments are rejected");

        assert!(matches!(error, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn ask_policy_requires_explicit_approval() {
        let workspace = tempdir().expect("workspace");
        let executor = executor(workspace.path());
        let tool_request = request("shell", json!({"command": "echo ok"}));

        let report = executor
            .execute(tool_request, CancellationToken::new())
            .await
            .expect("tool evaluates");

        assert_eq!(report.envelope.status, ToolResultStatus::Blocked);
        assert_eq!(report.policy_evaluation.decision, PolicyDecision::Ask);
    }

    async fn execute_approved(
        executor: &BasicToolExecutor,
        request: ToolRequest,
        cancellation: CancellationToken,
    ) -> ToolExecutionReport {
        let policy = executor.evaluate(&request).expect("policy evaluates");
        assert_eq!(policy.decision, PolicyDecision::Ask);
        executor
            .execute_with_policy(request, policy, true, cancellation)
            .await
            .expect("approved tool runs")
    }

    fn executor(path: &Path) -> BasicToolExecutor {
        BasicToolExecutor::new(WorkspacePolicy::new(path).expect("policy"))
    }

    fn request(tool_name: &str, arguments: Value) -> ToolRequest {
        ToolRequest {
            tool_call_id: ToolCallId::new(),
            session_id: SessionId::new(),
            turn_id: Some(TurnId::new()),
            tool_name: tool_name.to_owned(),
            arguments,
        }
    }
}
