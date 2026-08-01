use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ArtifactId, EvidenceId, PolicyId, ToolCallId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SideEffectType {
    None,
    File,
    Process,
    Network,
    ExternalSystem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InterruptedToolAction {
    ReplaySafe,
    ReconcileBeforeRetry,
    ReplayForbidden,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolRecoveryPolicy {
    pub side_effect_type: SideEffectType,
    pub idempotency_key_policy: String,
    pub retry_policy: String,
    pub interrupted_action: InterruptedToolAction,
}

impl ToolRecoveryPolicy {
    #[must_use]
    pub fn for_side_effect(side_effect_type: SideEffectType) -> Self {
        let (idempotency_key_policy, interrupted_action) = match side_effect_type {
            SideEffectType::None => ("not_required", InterruptedToolAction::ReplaySafe),
            SideEffectType::File | SideEffectType::Process => (
                "required_for_retry",
                InterruptedToolAction::ReconcileBeforeRetry,
            ),
            SideEffectType::Network | SideEffectType::ExternalSystem => {
                ("blocked", InterruptedToolAction::ReplayForbidden)
            }
        };
        Self {
            side_effect_type,
            idempotency_key_policy: idempotency_key_policy.to_owned(),
            retry_policy: if side_effect_type == SideEffectType::None {
                "retry_allowed".to_owned()
            } else {
                "no_implicit_retry_for_side_effects".to_owned()
            },
            interrupted_action,
        }
    }

    #[must_use]
    pub const fn side_effect_possible(&self) -> bool {
        !matches!(self.side_effect_type, SideEffectType::None)
    }
}

impl Default for ToolRecoveryPolicy {
    fn default() -> Self {
        Self::for_side_effect(SideEffectType::ExternalSystem)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    Ok,
    Error,
    Blocked,
    Cancelled,
    Timeout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolProgressPhase {
    Started,
    Output,
    Completed,
}

/// Bounded, presentation-safe progress for one tool call.
///
/// Progress is diagnostic and may be sampled. Durable completion facts live in
/// [`ToolExecutionMetrics`], so consumers must not infer success from the last
/// progress event they happened to receive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolProgress {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub phase: ToolProgressPhase,
    pub elapsed_ms: u64,
    pub output_bytes: u64,
    pub output_lines: u64,
    pub detail: Option<String>,
}

/// Stable execution metrics attached to every terminal tool report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolExecutionMetrics {
    pub duration_ms: u64,
    pub output_bytes: u64,
    pub output_lines: u64,
    pub output_truncated: bool,
    pub exit_code: Option<i32>,
    pub item_count: Option<u64>,
    pub match_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolContract {
    pub tool_name: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub error_schema: Value,
    pub side_effect_type: SideEffectType,
    pub idempotency_key_policy: String,
    pub timeout_policy: String,
    pub cancellation_policy: String,
    pub retry_policy: String,
    pub artifact_policy: String,
    pub permission_policy_ref: Option<PolicyId>,
}

impl From<&ToolContract> for ToolRecoveryPolicy {
    fn from(contract: &ToolContract) -> Self {
        let mut policy = Self::for_side_effect(contract.side_effect_type);
        policy.idempotency_key_policy = contract.idempotency_key_policy.clone();
        policy.retry_policy = contract.retry_policy.clone();
        policy
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResultEnvelope {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub status: ToolResultStatus,
    pub summary: String,
    pub structured_facts: Value,
    pub model_visible_excerpt: Option<String>,
    pub raw_artifact_ref: Option<ArtifactId>,
    pub evidence_refs: Vec<EvidenceId>,
    pub risk: String,
    pub verification_hint: Option<String>,
}

/// Returns a stable operational family for strategies that should share retry
/// and diagnosis state across superficially different tool calls.
#[must_use]
pub fn semantic_tool_failure_family(tool_name: &str, facts: &Value) -> Option<String> {
    let command = facts
        .get("command")
        .map(|command| match command {
            Value::String(command) => command.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        })
        .unwrap_or_default()
        .to_ascii_lowercase();
    if tool_name == "shell" {
        if command.contains("apt-get") && command.contains("install") {
            return Some(format!(
                "dependency_install:apt:{}",
                dependency_install_scope(&command, &["apt-get", "apt"])
            ));
        }
        if (command.contains("pip install") || command.contains("pip3 install"))
            || (command.contains("-m pip") && command.contains("install"))
        {
            return Some(format!(
                "dependency_install:pip:{}",
                dependency_install_scope(&command, &["pip", "pip3"])
            ));
        }
        if command.contains("apt-get") && command.contains("update") {
            return Some("dependency_index:apt".to_owned());
        }
    }
    if matches!(tool_name, "process_poll" | "process_reconnect") {
        let process_id = facts
            .get("process_id")
            .map(|value| match value {
                Value::String(value) => value.clone(),
                value => value.to_string(),
            })
            .unwrap_or_else(|| "unknown".to_owned());
        return Some(format!("process_wait:{process_id}"));
    }
    None
}

fn dependency_install_scope(command: &str, managers: &[&str]) -> String {
    const MAX_TARGETS: usize = 8;
    const MAX_TARGET_CHARS: usize = 64;

    let tokens = command.split_whitespace().collect::<Vec<_>>();
    let Some(manager_index) = tokens.iter().position(|token| {
        let token = token.trim_matches(|character: char| matches!(character, '\'' | '"'));
        token
            .rsplit(['/', '\\'])
            .next()
            .is_some_and(|program| managers.contains(&program))
    }) else {
        return "unspecified".to_owned();
    };
    let Some(install_index) = tokens[manager_index.saturating_add(1)..]
        .iter()
        .position(|token| token.trim_matches(['\'', '"']) == "install")
        .map(|index| manager_index.saturating_add(index).saturating_add(1))
    else {
        return "unspecified".to_owned();
    };

    let mut targets = Vec::new();
    let mut index = install_index.saturating_add(1);
    while let Some(raw) = tokens.get(index) {
        if raw.starts_with(['|', '&', ';', '>', '<']) {
            break;
        }
        let terminal = raw.ends_with(['|', '&', ';']);
        let target = raw.trim_matches(|character: char| {
            matches!(character, '\'' | '"' | ',' | ';' | '|' | '&')
        });
        if dependency_option_takes_value(target) {
            index = index.saturating_add(2);
            continue;
        }
        if !target.starts_with('-')
            && targets.len() < MAX_TARGETS
            && let Some(target) = normalized_dependency_target(target, MAX_TARGET_CHARS)
            && !targets.contains(&target)
        {
            targets.push(target);
        }
        if terminal {
            break;
        }
        index = index.saturating_add(1);
    }
    targets.sort();
    if targets.is_empty() {
        "unspecified".to_owned()
    } else {
        targets.join(",")
    }
}

fn dependency_option_takes_value(option: &str) -> bool {
    matches!(
        option,
        "-c" | "--cert"
            | "--client-cert"
            | "--config-settings"
            | "--constraint"
            | "--extra-index-url"
            | "-f"
            | "--find-links"
            | "--global-option"
            | "-i"
            | "--index-url"
            | "--install-option"
            | "-o"
            | "--option"
            | "--prefix"
            | "--proxy"
            | "-r"
            | "--requirement"
            | "--root"
            | "--target"
            | "--trusted-host"
    )
}

fn normalized_dependency_target(raw: &str, max_chars: usize) -> Option<String> {
    if raw.is_empty() || raw.contains("://") || raw.contains(['/', '\\']) || raw.starts_with('.') {
        return None;
    }
    let direct_name = raw.split_once('@').map_or(raw, |(name, _)| name);
    let name = direct_name
        .split(['<', '>', '=', '!', '~'])
        .next()
        .unwrap_or_default()
        .trim();
    if name.is_empty()
        || !name.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '.' | '_' | '-' | '[' | ']' | ':')
        })
    {
        return None;
    }
    let bounded = name.chars().take(max_chars).collect::<String>();
    (!bounded.is_empty()).then_some(bounded)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::semantic_tool_failure_family;

    #[test]
    fn semantic_failure_family_accepts_string_and_argv_commands() {
        assert_eq!(
            semantic_tool_failure_family(
                "shell",
                &json!({"command": "sudo apt-get install parquet-tools"}),
            )
            .as_deref(),
            Some("dependency_install:apt:parquet-tools")
        );
        assert_eq!(
            semantic_tool_failure_family(
                "shell",
                &json!({"command": ["python", "-m", "pip", "install", "pyarrow"]}),
            )
            .as_deref(),
            Some("dependency_install:pip:pyarrow")
        );
    }

    #[test]
    fn dependency_failure_families_distinguish_alternative_package_targets() {
        let git = semantic_tool_failure_family(
            "shell",
            &json!({"command": "apt-get install -y git build-essential"}),
        );
        let same_git = semantic_tool_failure_family(
            "shell",
            &json!({"command": "apt-get -qq install build-essential git -y"}),
        );
        let simh = semantic_tool_failure_family(
            "shell",
            &json!({"command": "dpkg -l simh || apt-get -qq install -y simh"}),
        );

        assert_eq!(git, same_git);
        assert_ne!(git, simh);
        assert_eq!(simh.as_deref(), Some("dependency_install:apt:simh"));
    }

    #[test]
    fn dependency_failure_family_excludes_repository_options_and_credentials() {
        let family = semantic_tool_failure_family(
            "shell",
            &json!({
                "command": "pip install --index-url https://user:secret@packages.example/simple --trusted-host packages.example 'pyarrow>=18.0'"
            }),
        );

        assert_eq!(family.as_deref(), Some("dependency_install:pip:pyarrow"));
        assert!(!family.as_deref().unwrap_or_default().contains("secret"));
        assert!(!family.as_deref().unwrap_or_default().contains("example"));
    }
}
