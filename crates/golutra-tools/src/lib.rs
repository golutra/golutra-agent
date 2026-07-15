use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use golutra_core::{
    ArtifactId, ArtifactRecord, EvidenceRecord, EvidenceStrength, PolicyDecision, PolicyEvaluation,
    RedactionStatus, SessionId, SideEffectType, ToolCallId, ToolContract, ToolResultEnvelope,
    ToolResultStatus, TurnId,
};
use golutra_policy::WorkspacePolicy;
use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

const DEFAULT_EXCERPT_LIMIT: usize = 2048;
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_FILE_CONTENT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 10_000;
const MAX_DIRECTORY_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_PATH_ARGUMENT_CHARS: usize = 4 * 1024;
const MAX_PATTERN_ARGUMENT_CHARS: usize = 64 * 1024;
const MAX_SHELL_COMMAND_CHARS: usize = 64 * 1024;
const MAX_TOOL_ERROR_CHARS: usize = 4 * 1024;
const MAX_AUDIT_RESOURCE_CHARS: usize = 64 * 1024;

mod process;

pub(crate) use process::{CommandLine, run_process};
#[cfg(test)]
pub(crate) use process::{MAX_PIPE_OUTPUT_BYTES, join_pipe_reader, spawn_pipe_reader};

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
    pub unix_mode: Option<u32>,
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

        let mut policy = match request.tool_name.as_str() {
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
        let contract = self
            .registry
            .contract(&request.tool_name)
            .ok_or_else(|| ToolError::UnknownTool(request.tool_name.clone()))?;
        validate_tool_arguments(contract, &request.arguments)?;

        match request.tool_name.as_str() {
            "write_file" => {
                let path = string_arg(&request.arguments, "path")?;
                let resolved_path = self.resolve_tool_path("write_file", &path, false)?;
                Ok(vec![read_optional_file(&resolved_path).await?])
            }
            "edit_file" => {
                let path = string_arg(&request.arguments, "path")?;
                let resolved_path = self.resolve_tool_path("edit_file", &path, true)?;
                let before_image = read_optional_file(&resolved_path).await?;
                if before_image.content.is_none() {
                    return Err(ToolError::Execution(
                        "edit target does not exist".to_owned(),
                    ));
                }
                Ok(vec![before_image])
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
    pub fn invalid_request_report(
        &self,
        request: ToolRequest,
        reason: impl Into<String>,
    ) -> ToolExecutionReport {
        let reason = bounded_text(&reason.into(), MAX_TOOL_ERROR_CHARS);
        let policy = execution_policy(&request, PolicyDecision::Block, &reason);
        error_report(
            request,
            "tool request is invalid",
            json!({"error": reason}),
            reason,
            policy,
        )
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
        if !original.contains(&search) {
            return Ok(error_report(
                request,
                "edit target not found",
                json!({"path": resolved_path, "search_found": false}),
                original,
                policy,
            ));
        }
        let edited = original.replacen(&search, &replace, 1);
        if edited.len() as u64 > MAX_FILE_CONTENT_BYTES {
            return Ok(error_report(
                request,
                "edited content exceeds file size limit",
                json!({"path": resolved_path, "max_bytes": MAX_FILE_CONTENT_BYTES}),
                String::new(),
                policy,
            ));
        }
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
        let resolved_path = self.resolve_tool_path("list_dir", &path, true)?;
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
        let resolved_path = self.resolve_tool_path("rg_search", &path, true)?;
        let output = run_process(
            "rg",
            &[
                "--line-number".to_owned(),
                "--no-heading".to_owned(),
                "--".to_owned(),
                pattern.clone(),
                resolved_path.display().to_string(),
            ],
            self.policy.workspace_root(),
            DEFAULT_TIMEOUT_MS,
            cancellation,
        )
        .await?;
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
        Ok(report(
            request,
            status,
            "rg search completed",
            json!({"path": resolved_path, "pattern": redacted_pattern, "exit_code": output.exit_code}),
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
        let redacted_command = redact_sensitive_text(&command).0;
        Ok(report(
            request,
            status,
            "shell command completed",
            json!({
                "command": redacted_command,
                "exit_code": shell_output.exit_code,
                "timed_out": shell_output.timed_out,
                "cancelled": shell_output.cancelled,
            }),
            shell_output.raw_output,
            Vec::new(),
            policy,
        ))
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

fn contract(tool_name: &str, side_effect_type: SideEffectType) -> ToolContract {
    let input_schema = match tool_name {
        "read_file" => object_schema(&[("path", MAX_PATH_ARGUMENT_CHARS)], &["path"], &["path"]),
        "write_file" => object_schema(
            &[
                ("path", MAX_PATH_ARGUMENT_CHARS),
                ("content", MAX_FILE_CONTENT_BYTES as usize),
            ],
            &["path", "content"],
            &["path"],
        ),
        "edit_file" => object_schema(
            &[
                ("path", MAX_PATH_ARGUMENT_CHARS),
                ("search", MAX_FILE_CONTENT_BYTES as usize),
                ("replace", MAX_FILE_CONTENT_BYTES as usize),
            ],
            &["path", "search", "replace"],
            &["path", "search"],
        ),
        "list_dir" => object_schema(&[("path", MAX_PATH_ARGUMENT_CHARS)], &[], &[]),
        "rg_search" => object_schema(
            &[
                ("pattern", MAX_PATTERN_ARGUMENT_CHARS),
                ("path", MAX_PATH_ARGUMENT_CHARS),
            ],
            &["pattern"],
            &["pattern"],
        ),
        "shell" => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "command": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_SHELL_COMMAND_CHARS
                },
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

fn object_schema(properties: &[(&str, usize)], required: &[&str], non_empty: &[&str]) -> Value {
    let properties = properties
        .iter()
        .map(|(name, max_length)| {
            let mut schema = json!({"type": "string", "maxLength": max_length});
            if non_empty.contains(name) {
                schema["minLength"] = json!(1);
            }
            ((*name).to_owned(), schema)
        })
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
    let (redacted_output, redaction_status) = redact_sensitive_text(&raw_output);
    let redacted_summary = redact_sensitive_text(summary).0;
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
    let envelope = ToolResultEnvelope {
        tool_call_id: request.tool_call_id,
        tool_name: request.tool_name,
        status,
        summary: redacted_summary,
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

fn redact_sensitive_value(mut value: Value) -> Value {
    redact_sensitive_value_in_place(&mut value, false);
    value
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
    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FileBeforeImage {
                path: path.to_path_buf(),
                content: None,
                unix_mode: None,
            });
        }
        Err(error) => return Err(ToolError::Execution(error.to_string())),
    };
    let metadata = file
        .metadata()
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    if metadata.len() > MAX_FILE_CONTENT_BYTES {
        return Err(ToolError::Execution(format!(
            "file {} exceeds {MAX_FILE_CONTENT_BYTES} byte limit",
            path.display()
        )));
    }
    let mut content = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.read_to_end(&mut content)
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    if content.len() as u64 > MAX_FILE_CONTENT_BYTES {
        return Err(ToolError::Execution(format!(
            "file {} grew beyond {MAX_FILE_CONTENT_BYTES} byte limit while reading",
            path.display()
        )));
    }
    Ok(FileBeforeImage {
        path: path.to_path_buf(),
        content: Some(content),
        unix_mode: unix_mode(&metadata),
    })
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

fn excerpt(raw_output: &str, limit: usize) -> String {
    raw_output.chars().take(limit).collect()
}

fn checksum(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

#[cfg(test)]
mod tests;
