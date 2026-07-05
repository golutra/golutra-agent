use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use golutra_core::{
    ArtifactId, ArtifactRecord, EvidenceRecord, EvidenceStrength, PolicyDecision, RedactionStatus,
    SessionId, SideEffectType, ToolCallId, ToolContract, ToolResultEnvelope, ToolResultStatus,
    TurnId,
};
use golutra_policy::WorkspacePolicy;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

const DEFAULT_EXCERPT_LIMIT: usize = 2048;
const DEFAULT_TIMEOUT_MS: u64 = 5_000;

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

    pub fn execute(&self, request: ToolRequest) -> Result<ToolExecutionReport, ToolError> {
        if self.registry.contract(&request.tool_name).is_none() {
            return Err(ToolError::UnknownTool(request.tool_name));
        }

        match request.tool_name.as_str() {
            "read_file" => self.read_file(request),
            "write_file" => self.write_file(request),
            "edit_file" => self.edit_file(request),
            "list_dir" => self.list_dir(request),
            "rg_search" => self.rg_search(request),
            "shell" => self.shell(request),
            _ => unreachable!("registered tool was checked before dispatch"),
        }
    }

    #[must_use]
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    fn read_file(&self, request: ToolRequest) -> Result<ToolExecutionReport, ToolError> {
        let path = string_arg(&request.arguments, "path")?;
        let policy = self.policy.evaluate_path("read_file", &path, true);
        if policy.decision != PolicyDecision::Allow {
            return Ok(blocked_report(request, policy.reason));
        }

        let resolved_path = self
            .policy
            .resolve_path(&path, true)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let content = fs::read_to_string(&resolved_path)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        Ok(success_report(
            request,
            "file read",
            json!({"path": resolved_path, "bytes": content.len()}),
            content,
            Vec::new(),
        ))
    }

    fn write_file(&self, request: ToolRequest) -> Result<ToolExecutionReport, ToolError> {
        let path = string_arg(&request.arguments, "path")?;
        let content = string_arg(&request.arguments, "content")?;
        let policy = self.policy.evaluate_path("write_file", &path, false);
        if policy.decision != PolicyDecision::Allow {
            return Ok(blocked_report(request, policy.reason));
        }

        let resolved_path = self
            .policy
            .resolve_path(&path, false)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        fs::write(&resolved_path, content.as_bytes())
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        Ok(success_report(
            request,
            "file written",
            json!({"path": resolved_path, "bytes": content.len()}),
            content,
            vec![resolved_path],
        ))
    }

    fn edit_file(&self, request: ToolRequest) -> Result<ToolExecutionReport, ToolError> {
        let path = string_arg(&request.arguments, "path")?;
        let search = string_arg(&request.arguments, "search")?;
        let replace = string_arg(&request.arguments, "replace")?;
        let policy = self.policy.evaluate_path("edit_file", &path, true);
        if policy.decision != PolicyDecision::Allow {
            return Ok(blocked_report(request, policy.reason));
        }

        let resolved_path = self
            .policy
            .resolve_path(&path, true)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let original = fs::read_to_string(&resolved_path)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        if !original.contains(&search) {
            return Ok(error_report(
                request,
                "edit target not found",
                json!({"path": resolved_path, "search": search}),
                original,
            ));
        }
        let edited = original.replacen(&search, &replace, 1);
        fs::write(&resolved_path, edited.as_bytes())
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        Ok(success_report(
            request,
            "file edited",
            json!({"path": resolved_path, "replacements": 1}),
            edited,
            vec![resolved_path],
        ))
    }

    fn list_dir(&self, request: ToolRequest) -> Result<ToolExecutionReport, ToolError> {
        let path =
            optional_string_arg(&request.arguments, "path").unwrap_or_else(|| ".".to_owned());
        let policy = self.policy.evaluate_path("list_dir", &path, true);
        if policy.decision != PolicyDecision::Allow {
            return Ok(blocked_report(request, policy.reason));
        }

        let resolved_path = self
            .policy
            .resolve_path(&path, true)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let entries = directory_entries(&resolved_path)?;
        Ok(success_report(
            request,
            "directory listed",
            json!({"path": resolved_path, "entries": entries}),
            entries.join("\n"),
            Vec::new(),
        ))
    }

    fn rg_search(&self, request: ToolRequest) -> Result<ToolExecutionReport, ToolError> {
        let pattern = string_arg(&request.arguments, "pattern")?;
        let path =
            optional_string_arg(&request.arguments, "path").unwrap_or_else(|| ".".to_owned());
        let policy = self.policy.evaluate_path("rg_search", &path, true);
        if policy.decision != PolicyDecision::Allow {
            return Ok(blocked_report(request, policy.reason));
        }

        let resolved_path = self
            .policy
            .resolve_path(&path, true)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let output = Command::new("rg")
            .arg("--line-number")
            .arg("--no-heading")
            .arg(&pattern)
            .arg(&resolved_path)
            .output()
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let raw = command_output_text(&output);
        let status = if output.status.success() || output.status.code() == Some(1) {
            ToolResultStatus::Ok
        } else {
            ToolResultStatus::Error
        };
        Ok(report(
            request,
            status,
            "rg search completed",
            json!({"path": resolved_path, "pattern": pattern, "exit_code": output.status.code()}),
            raw,
            Vec::new(),
        ))
    }

    fn shell(&self, request: ToolRequest) -> Result<ToolExecutionReport, ToolError> {
        let command = string_arg(&request.arguments, "command")?;
        let timeout_ms = request
            .arguments
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        let policy = self.policy.evaluate_shell(&command);
        if policy.decision != PolicyDecision::Allow {
            return Ok(blocked_report(request, policy.reason));
        }

        let command_line = CommandLine::parse(&command)?;
        let shell_output =
            run_command_line(&command_line, self.policy.workspace_root(), timeout_ms)?;
        let status = if shell_output.timed_out {
            ToolResultStatus::Cancelled
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
                "timed_out": shell_output.timed_out
            }),
            shell_output.raw_output,
            Vec::new(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellOutput {
    exit_code: Option<i32>,
    timed_out: bool,
    raw_output: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandLine {
    program: String,
    args: Vec<String>,
}

impl CommandLine {
    fn parse(command: &str) -> Result<Self, ToolError> {
        let mut parts = command
            .split_whitespace()
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
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

fn run_command_line(
    command_line: &CommandLine,
    cwd: &Path,
    timeout_ms: u64,
) -> Result<ShellOutput, ToolError> {
    let mut child = Command::new(&command_line.program)
        .args(&command_line.args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        if child
            .try_wait()
            .map_err(|error| ToolError::Execution(error.to_string()))?
            .is_some()
        {
            let output = child
                .wait_with_output()
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            return Ok(ShellOutput {
                exit_code: output.status.code(),
                timed_out: false,
                raw_output: command_output_text(&output),
            });
        }

        if Instant::now() >= deadline {
            child
                .kill()
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            let output = child
                .wait_with_output()
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            return Ok(ShellOutput {
                exit_code: output.status.code(),
                timed_out: true,
                raw_output: command_output_text(&output),
            });
        }

        thread::sleep(Duration::from_millis(10));
    }
}

fn directory_entries(path: &Path) -> Result<Vec<String>, ToolError> {
    let mut entries = fs::read_dir(path)
        .map_err(|error| ToolError::Execution(error.to_string()))?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .map_err(|error| ToolError::Execution(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    Ok(entries)
}

fn contract(tool_name: &str, side_effect_type: SideEffectType) -> ToolContract {
    ToolContract {
        tool_name: tool_name.to_owned(),
        input_schema: json!({"type": "object"}),
        output_schema: json!({"type": "object"}),
        error_schema: json!({"type": "object"}),
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

fn success_report(
    request: ToolRequest,
    summary: &str,
    structured_facts: Value,
    raw_output: String,
    changed_files: Vec<PathBuf>,
) -> ToolExecutionReport {
    report(
        request,
        ToolResultStatus::Ok,
        summary,
        structured_facts,
        raw_output,
        changed_files,
    )
}

fn error_report(
    request: ToolRequest,
    summary: &str,
    structured_facts: Value,
    raw_output: String,
) -> ToolExecutionReport {
    report(
        request,
        ToolResultStatus::Error,
        summary,
        structured_facts,
        raw_output,
        Vec::new(),
    )
}

fn blocked_report(request: ToolRequest, reason: String) -> ToolExecutionReport {
    report(
        request,
        ToolResultStatus::Blocked,
        &reason,
        json!({"blocked": true, "reason": reason}),
        String::new(),
        Vec::new(),
    )
}

fn report(
    request: ToolRequest,
    status: ToolResultStatus,
    summary: &str,
    structured_facts: Value,
    raw_output: String,
    changed_files: Vec<PathBuf>,
) -> ToolExecutionReport {
    let artifact = artifact_for(&request, &raw_output);
    let evidence = EvidenceRecord {
        evidence_id: golutra_core::EvidenceId::new(),
        claim: format!("tool {} finished with {status:?}", request.tool_name),
        artifact_refs: vec![artifact.artifact_id],
        source_event_refs: Vec::new(),
        evidence_strength: match status {
            ToolResultStatus::Ok => EvidenceStrength::Medium,
            ToolResultStatus::Error | ToolResultStatus::Blocked | ToolResultStatus::Cancelled => {
                EvidenceStrength::Weak
            }
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
        model_visible_excerpt: Some(excerpt(&raw_output, DEFAULT_EXCERPT_LIMIT)),
        raw_artifact_ref: Some(artifact.artifact_id),
        evidence_refs: vec![evidence.evidence_id],
        risk: "p0_local_tool".to_owned(),
        verification_hint: Some("use artifact/evidence refs for verification".to_owned()),
    };

    ToolExecutionReport {
        envelope,
        artifacts: vec![artifact],
        evidence: vec![evidence],
        changed_files,
    }
}

fn artifact_for(request: &ToolRequest, raw_output: &str) -> ArtifactRecord {
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
        redaction_status: RedactionStatus::NotRequired,
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

fn command_output_text(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.is_empty() {
        stdout.to_string()
    } else {
        format!("{stdout}\n{stderr}")
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

    #[test]
    fn registry_contains_p0_tools() {
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

    #[test]
    fn read_file_returns_envelope_artifact_and_evidence() {
        let workspace = tempdir().expect("workspace");
        fs::write(workspace.path().join("README.md"), "hello").expect("fixture");
        let executor = executor(workspace.path());

        let report = executor
            .execute(request("read_file", json!({"path": "README.md"})))
            .expect("tool runs");

        assert_eq!(report.envelope.status, ToolResultStatus::Ok);
        assert_eq!(report.artifacts.len(), 1);
        assert_eq!(report.evidence.len(), 1);
    }

    #[test]
    fn write_file_records_changed_file() {
        let workspace = tempdir().expect("workspace");
        let executor = executor(workspace.path());

        let report = executor
            .execute(request(
                "write_file",
                json!({"path": "src.txt", "content": "new"}),
            ))
            .expect("tool runs");

        assert_eq!(report.envelope.status, ToolResultStatus::Ok);
        assert_eq!(report.changed_files.len(), 1);
        assert_eq!(
            fs::read_to_string(workspace.path().join("src.txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn blocks_workspace_escape() {
        let workspace = tempdir().expect("workspace");
        let outside = tempdir().expect("outside");
        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, "secret").expect("fixture");
        let executor = executor(workspace.path());

        let report = executor
            .execute(request(
                "read_file",
                json!({"path": outside_file.to_string_lossy()}),
            ))
            .expect("tool runs");

        assert_eq!(report.envelope.status, ToolResultStatus::Blocked);
    }

    #[test]
    fn shell_rejects_metacharacters_before_execution() {
        let workspace = tempdir().expect("workspace");
        let executor = executor(workspace.path());

        let report = executor
            .execute(request("shell", json!({"command": "echo ok; cat .env"})))
            .expect("tool runs");

        assert_eq!(report.envelope.status, ToolResultStatus::Blocked);
    }

    #[test]
    fn shell_runs_simple_command_without_shell_interpreter() {
        let workspace = tempdir().expect("workspace");
        let executor = executor(workspace.path());

        let report = executor
            .execute(request("shell", json!({"command": "echo ok"})))
            .expect("tool runs");

        assert_eq!(report.envelope.status, ToolResultStatus::Ok);
        assert_eq!(
            report.envelope.model_visible_excerpt.as_deref(),
            Some("ok\n")
        );
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
