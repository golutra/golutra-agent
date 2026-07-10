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
        let parsed = shlex::split(command);
        let blocked = contains_shell_metacharacter(command)
            || self
                .denied_shell_fragments
                .iter()
                .any(|fragment| command.contains(fragment))
            || parsed.is_none()
            || parsed
                .as_deref()
                .is_some_and(|parts| self.shell_command_is_blocked(parts));
        let decision = if blocked {
            PolicyDecision::Block
        } else if parsed
            .as_deref()
            .is_some_and(|parts| self.shell_command_is_preapproved(parts))
        {
            PolicyDecision::Allow
        } else {
            PolicyDecision::Ask
        };
        let reason = match decision {
            PolicyDecision::Allow => "read-only or build command is pre-approved",
            PolicyDecision::Ask => "process command requires explicit user approval",
            PolicyDecision::Block => "shell command matched P0 deny-list or metacharacter guard",
            PolicyDecision::Deny => "shell command denied by policy",
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
        let path_text = path.to_string_lossy().to_ascii_lowercase();
        has_internal_runtime_component(path)
            || self
                .sensitive_path_fragments
                .iter()
                .any(|fragment| path_text.contains(fragment))
    }

    fn shell_command_is_blocked(&self, parts: &[String]) -> bool {
        let Some(program) = parts.first().map(String::as_str) else {
            return true;
        };
        let arguments = &parts[1..];
        let sensitive_argument = arguments.iter().any(|argument| {
            let lower = argument.to_ascii_lowercase();
            argument
                .split(['/', '\\'])
                .any(|component| matches!(component, ".git" | ".golutra"))
                || self
                    .sensitive_path_fragments
                    .iter()
                    .any(|fragment| lower.contains(fragment))
        });
        let dangerous_program_option = match program {
            "find" => arguments.iter().any(|argument| {
                matches!(
                    argument.as_str(),
                    "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir"
                )
            }),
            "rg" => arguments
                .iter()
                .any(|argument| argument == "--pre" || argument.starts_with("--pre=")),
            "rm" => {
                let recursive_force = arguments.iter().any(|argument| {
                    argument.starts_with('-') && argument.contains('r') && argument.contains('f')
                });
                recursive_force && arguments.iter().any(|argument| argument == "/")
            }
            "mkfs" | "shutdown" | "reboot" => true,
            _ => false,
        };
        sensitive_argument || dangerous_program_option
    }

    fn shell_command_is_preapproved(&self, parts: &[String]) -> bool {
        let Some(program) = parts.first().map(String::as_str) else {
            return false;
        };
        let arguments = &parts[1..];
        if arguments.iter().any(|argument| {
            let path = Path::new(argument);
            path.is_absolute() && !path.starts_with(&self.workspace_root)
                || path
                    .components()
                    .any(|component| component == std::path::Component::ParentDir)
        }) {
            return false;
        }
        match program {
            "cargo" => arguments.first().is_some_and(|subcommand| {
                matches!(
                    subcommand.as_str(),
                    "build" | "check" | "clippy" | "metadata" | "test"
                )
            }),
            "rustc" => arguments == ["--version"],
            "rg" | "ls" | "head" | "tail" | "wc" => true,
            "pwd" => arguments.is_empty(),
            "git" => arguments.first().is_some_and(|subcommand| {
                matches!(
                    subcommand.as_str(),
                    "status" | "diff" | "log" | "show" | "rev-parse"
                ) && !arguments
                    .iter()
                    .any(|argument| matches!(argument.as_str(), "--ext-diff" | "--textconv"))
            }),
            "npm" | "pnpm" | "yarn" => match arguments.first().map(String::as_str) {
                Some("test") => true,
                Some("run") => arguments.get(1).is_some_and(|script| {
                    matches!(
                        script.as_str(),
                        "build" | "check" | "lint" | "test" | "typecheck"
                    )
                }),
                _ => false,
            },
            _ => false,
        }
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

fn has_internal_runtime_component(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            std::path::Component::Normal(value) if matches!(value.to_str(), Some(".git" | ".golutra"))
        )
    })
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
    fn blocks_internal_runtime_and_git_paths_without_blocking_github_files() {
        let workspace = tempdir().expect("workspace");
        fs::create_dir_all(workspace.path().join(".git")).expect("git directory");
        fs::create_dir_all(workspace.path().join(".golutra")).expect("runtime directory");
        fs::create_dir_all(workspace.path().join(".github")).expect("github directory");
        fs::write(workspace.path().join(".git/config"), "config").expect("git config");
        fs::write(workspace.path().join(".golutra/runtime.sqlite"), "state")
            .expect("runtime state");
        fs::write(workspace.path().join(".github/workflow.yml"), "workflow")
            .expect("github workflow");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        assert_eq!(
            policy
                .evaluate_path("write_file", ".git/config", true)
                .decision,
            PolicyDecision::Block
        );
        assert_eq!(
            policy
                .evaluate_path("write_file", ".golutra/runtime.sqlite", true)
                .decision,
            PolicyDecision::Block
        );
        assert_eq!(
            policy
                .evaluate_path("write_file", ".github/workflow.yml", true)
                .decision,
            PolicyDecision::Allow
        );
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

    #[test]
    fn asks_for_unknown_process_commands() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        let evaluation = policy.evaluate_shell("sleep 5");

        assert_eq!(evaluation.decision, PolicyDecision::Ask);
    }

    #[test]
    fn preapproves_build_and_read_only_commands() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        assert_eq!(
            policy.evaluate_shell("cargo test -p golutra-core").decision,
            PolicyDecision::Allow
        );
        assert_eq!(
            policy.evaluate_shell("git status --short").decision,
            PolicyDecision::Allow
        );
        assert_eq!(
            policy.evaluate_shell("rg 'two words' crates").decision,
            PolicyDecision::Allow
        );
    }

    #[test]
    fn blocks_shell_execution_flags_and_sensitive_paths() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        for command in [
            "find . -delete",
            "find . -exec rm {} +",
            "rg --pre 'cat payload' pattern",
            "rg token .env",
        ] {
            assert_eq!(
                policy.evaluate_shell(command).decision,
                PolicyDecision::Block,
                "{command}"
            );
        }
    }

    #[test]
    fn requires_approval_for_mutating_or_broad_commands() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        for command in [
            "sed -i backup src/lib.rs",
            "find . -name '*.rs'",
            "cargo run",
            "cargo fmt",
            "git branch -D old",
            "npm run deploy",
            "ls /tmp",
        ] {
            assert_eq!(
                policy.evaluate_shell(command).decision,
                PolicyDecision::Ask,
                "{command}"
            );
        }
    }
}
