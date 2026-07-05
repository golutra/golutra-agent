use std::path::{Path, PathBuf};

use golutra_core::{EvidenceId, PolicyDecision, PolicyEvaluation, PolicyId};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("workspace root does not exist: {0}")]
    MissingWorkspace(String),
    #[error("path has no parent: {0}")]
    MissingParent(String),
    #[error("path canonicalization failed: {0}")]
    Canonicalization(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePolicy {
    workspace_root: PathBuf,
    sensitive_path_fragments: Vec<String>,
    denied_shell_fragments: Vec<String>,
}

impl WorkspacePolicy {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Result<Self, PolicyError> {
        let workspace_root = workspace_root.into();
        if !workspace_root.exists() {
            return Err(PolicyError::MissingWorkspace(
                workspace_root.display().to_string(),
            ));
        }

        Ok(Self {
            workspace_root: canonicalize_existing(&workspace_root)?,
            sensitive_path_fragments: vec![
                ".env".to_owned(),
                "id_rsa".to_owned(),
                "id_ed25519".to_owned(),
                ".ssh".to_owned(),
                "secrets".to_owned(),
            ],
            denied_shell_fragments: vec![
                "rm -rf /".to_owned(),
                "mkfs".to_owned(),
                "shutdown".to_owned(),
                "reboot".to_owned(),
            ],
        })
    }

    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn evaluate_path(
        &self,
        action: &str,
        path: impl AsRef<Path>,
        requires_existing_path: bool,
    ) -> PolicyEvaluation {
        let path = path.as_ref();
        match self.resolve_path(path, requires_existing_path) {
            Ok(resolved_path) if self.is_sensitive_path(&resolved_path) => self.evaluation(
                action,
                &resolved_path,
                PolicyDecision::Block,
                "sensitive path",
            ),
            Ok(resolved_path) if resolved_path.starts_with(&self.workspace_root) => self
                .evaluation(
                    action,
                    &resolved_path,
                    PolicyDecision::Allow,
                    "path is inside workspace",
                ),
            Ok(resolved_path) => self.evaluation(
                action,
                &resolved_path,
                PolicyDecision::Block,
                "path is outside workspace",
            ),
            Err(error) => self.evaluation(
                action,
                path,
                PolicyDecision::Block,
                &format!("path resolution failed: {error}"),
            ),
        }
    }

    pub fn evaluate_shell(&self, command: &str) -> PolicyEvaluation {
        let blocked = contains_shell_metacharacter(command)
            || self
                .denied_shell_fragments
                .iter()
                .any(|fragment| command.contains(fragment));
        let decision = if blocked {
            PolicyDecision::Block
        } else {
            PolicyDecision::Allow
        };
        let reason = match decision {
            PolicyDecision::Allow => "shell command passed P0 deny-list",
            PolicyDecision::Block => "shell command matched P0 deny-list or metacharacter guard",
            PolicyDecision::Ask | PolicyDecision::Deny => "unused P0 shell policy result",
        };

        PolicyEvaluation {
            policy_ref: PolicyId::new(),
            subject: "tool".to_owned(),
            action: "shell".to_owned(),
            resource: command.to_owned(),
            decision,
            reason: reason.to_owned(),
            evidence_refs: Vec::<EvidenceId>::new(),
        }
    }

    pub fn resolve_path(
        &self,
        path: impl AsRef<Path>,
        requires_existing_path: bool,
    ) -> Result<PathBuf, PolicyError> {
        let path = path.as_ref();
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace_root.join(path)
        };

        if requires_existing_path || candidate.exists() {
            canonicalize_existing(&candidate)
        } else {
            let parent = candidate
                .parent()
                .ok_or_else(|| PolicyError::MissingParent(candidate.display().to_string()))?;
            let canonical_parent = canonicalize_existing(parent)?;
            let file_name = candidate
                .file_name()
                .ok_or_else(|| PolicyError::MissingParent(candidate.display().to_string()))?;
            Ok(canonical_parent.join(file_name))
        }
    }

    fn is_sensitive_path(&self, path: &Path) -> bool {
        let path_text = path.to_string_lossy();
        self.sensitive_path_fragments
            .iter()
            .any(|fragment| path_text.contains(fragment))
    }

    fn evaluation(
        &self,
        action: &str,
        resource: &Path,
        decision: PolicyDecision,
        reason: &str,
    ) -> PolicyEvaluation {
        PolicyEvaluation {
            policy_ref: PolicyId::new(),
            subject: "tool".to_owned(),
            action: action.to_owned(),
            resource: resource.display().to_string(),
            decision,
            reason: reason.to_owned(),
            evidence_refs: Vec::new(),
        }
    }
}

#[must_use]
pub fn default_workspace_policy_name() -> &'static str {
    "workspace-path-guard"
}

#[must_use]
pub fn contains_shell_metacharacter(command: &str) -> bool {
    command.chars().any(|character| {
        matches!(
            character,
            ';' | '|' | '&' | '>' | '<' | '$' | '`' | '\\' | '\n'
        )
    })
}

fn canonicalize_existing(path: &Path) -> Result<PathBuf, PolicyError> {
    path.canonicalize()
        .map_err(|_| PolicyError::Canonicalization(path.display().to_string()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn blocks_paths_outside_workspace() {
        let workspace = tempdir().expect("workspace");
        let outside = tempdir().expect("outside");
        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, "secret").expect("outside file");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        let evaluation = policy.evaluate_path("read_file", &outside_file, true);

        assert_eq!(evaluation.decision, PolicyDecision::Block);
    }

    #[test]
    fn allows_paths_inside_workspace() {
        let workspace = tempdir().expect("workspace");
        let file = workspace.path().join("src.txt");
        fs::write(&file, "ok").expect("file");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        let evaluation = policy.evaluate_path("read_file", &file, true);

        assert_eq!(evaluation.decision, PolicyDecision::Allow);
    }

    #[test]
    fn blocks_shell_metacharacters() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        let evaluation = policy.evaluate_shell("cargo test; cat .env");

        assert_eq!(evaluation.decision, PolicyDecision::Block);
    }
}
