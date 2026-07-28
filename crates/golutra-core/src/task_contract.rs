use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};

pub const TASK_CONTRACT_SCHEMA_VERSION: u32 = 1;
pub const MAX_TASK_CORRECTION_ROUNDS: u32 = 8;
const MAX_TASK_CONTRACT_PATH_CHARS: usize = 1_024;
const MAX_TASK_CONTRACT_CRITERION_CHARS: usize = 4_096;

/// Explicitly describes what a turn must deliver.  The runtime uses this
/// contract for verification; it never infers the requirement from prompt
/// wording.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceChangeRequirement {
    #[default]
    Optional,
    Required,
    Forbidden,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerificationRequirement {
    #[default]
    BestEffort,
    Required,
    Independent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskContract {
    #[serde(default = "default_task_contract_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub workspace_change: WorkspaceChangeRequirement,
    #[serde(default)]
    pub required_paths: Vec<String>,
    #[serde(default)]
    pub completion_criteria: Vec<String>,
    #[serde(default)]
    pub require_objective_validation: bool,
    #[serde(default)]
    pub verification: VerificationRequirement,
    #[serde(default = "default_correction_rounds")]
    pub max_correction_rounds: u32,
}

const fn default_task_contract_schema_version() -> u32 {
    TASK_CONTRACT_SCHEMA_VERSION
}

const fn default_correction_rounds() -> u32 {
    1
}

impl Default for TaskContract {
    fn default() -> Self {
        Self {
            schema_version: default_task_contract_schema_version(),
            workspace_change: WorkspaceChangeRequirement::Optional,
            required_paths: Vec::new(),
            completion_criteria: Vec::new(),
            require_objective_validation: false,
            verification: VerificationRequirement::BestEffort,
            max_correction_rounds: default_correction_rounds(),
        }
    }
}

impl TaskContract {
    #[must_use]
    pub fn conversational(completion_criteria: Vec<String>) -> Self {
        Self {
            completion_criteria,
            max_correction_rounds: 0,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn requires_workspace_evidence(&self) -> bool {
        matches!(self.workspace_change, WorkspaceChangeRequirement::Required)
            || !self.required_paths.is_empty()
    }

    #[must_use]
    pub fn requires_independent_verification(&self) -> bool {
        matches!(self.verification, VerificationRequirement::Independent)
    }

    #[must_use]
    pub fn allows_correction(&self, attempt: u32) -> bool {
        attempt < self.max_correction_rounds
    }

    /// Reject malformed adapter input before it reaches the execution loop.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != TASK_CONTRACT_SCHEMA_VERSION {
            return Err(format!(
                "unsupported task contract schema_version {}; expected {TASK_CONTRACT_SCHEMA_VERSION}",
                self.schema_version
            ));
        }
        if self.max_correction_rounds > MAX_TASK_CORRECTION_ROUNDS {
            return Err(format!(
                "task contract max_correction_rounds exceeds {MAX_TASK_CORRECTION_ROUNDS}"
            ));
        }
        if self.required_paths.len() > 64 {
            return Err("task contract contains too many required paths".to_owned());
        }
        if self.completion_criteria.len() > 32 {
            return Err("task contract contains too many completion criteria".to_owned());
        }
        if self
            .required_paths
            .iter()
            .any(|path| !is_valid_workspace_relative_path(path))
        {
            return Err(
                "task contract paths must be non-empty workspace-relative paths".to_owned(),
            );
        }
        if self.completion_criteria.iter().any(|criterion| {
            criterion.trim().is_empty()
                || criterion.chars().count() > MAX_TASK_CONTRACT_CRITERION_CHARS
        }) {
            return Err(format!(
                "task contract completion criteria must contain 1..={MAX_TASK_CONTRACT_CRITERION_CHARS} characters"
            ));
        }
        if matches!(self.workspace_change, WorkspaceChangeRequirement::Forbidden)
            && !self.required_paths.is_empty()
        {
            return Err("forbidden workspace changes cannot require delivery paths".to_owned());
        }
        Ok(())
    }
}

pub(crate) fn is_valid_workspace_relative_path(path: &str) -> bool {
    let trimmed = path.trim();
    !trimmed.is_empty()
        && trimmed.chars().count() <= MAX_TASK_CONTRACT_PATH_CHARS
        && !path.contains('\0')
        && is_portable_workspace_relative_path(path)
        && !Path::new(path).components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

fn is_portable_workspace_relative_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    if normalized.starts_with('/')
        || normalized
            .as_bytes()
            .get(1)
            .is_some_and(|separator| *separator == b':')
    {
        return false;
    }
    !normalized.split('/').any(|component| component == "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_makes_workspace_and_correction_policy_explicit() {
        let contract = TaskContract {
            workspace_change: WorkspaceChangeRequirement::Required,
            required_paths: vec!["src/main.rs".to_owned()],
            verification: VerificationRequirement::Independent,
            max_correction_rounds: 2,
            ..TaskContract::default()
        };

        assert!(contract.requires_workspace_evidence());
        assert!(contract.requires_independent_verification());
        assert!(contract.allows_correction(0));
        assert!(contract.allows_correction(1));
        assert!(!contract.allows_correction(2));
        assert!(contract.validate().is_ok());
    }

    #[test]
    fn contract_rejects_absolute_or_conflicting_delivery_paths() {
        let absolute = TaskContract {
            required_paths: vec!["/tmp/result".to_owned()],
            ..TaskContract::default()
        };
        assert!(absolute.validate().is_err());

        let conflicting = TaskContract {
            workspace_change: WorkspaceChangeRequirement::Forbidden,
            required_paths: vec!["result.txt".to_owned()],
            ..TaskContract::default()
        };
        assert!(conflicting.validate().is_err());

        let future_schema = TaskContract {
            schema_version: TASK_CONTRACT_SCHEMA_VERSION + 1,
            ..TaskContract::default()
        };
        assert!(future_schema.validate().is_err());

        let unbounded_correction = TaskContract {
            max_correction_rounds: MAX_TASK_CORRECTION_ROUNDS + 1,
            ..TaskContract::default()
        };
        assert!(unbounded_correction.validate().is_err());
    }

    #[test]
    fn contract_paths_have_the_same_boundary_on_unix_and_windows() {
        for path in [
            r"C:\workspace\result.txt",
            r"..\outside.txt",
            r"\\server\share\result.txt",
        ] {
            let contract = TaskContract {
                required_paths: vec![path.to_owned()],
                ..TaskContract::default()
            };
            assert!(contract.validate().is_err(), "accepted unsafe path {path}");
        }
    }
}
