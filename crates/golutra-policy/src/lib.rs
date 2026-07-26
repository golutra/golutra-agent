use std::path::{Path, PathBuf};

use golutra_core::{
    EvidenceId, PolicyBlockDisposition, PolicyDecision, PolicyEvaluation, PolicyId,
};
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
                Some(PolicyBlockDisposition::Terminal),
                "sensitive path",
            ),
            Ok(resolved_path) if resolved_path.starts_with(&self.workspace_root) => self
                .evaluation(
                    action,
                    &resolved_path,
                    PolicyDecision::Allow,
                    None,
                    "path is inside workspace",
                ),
            Ok(resolved_path) => self.evaluation(
                action,
                &resolved_path,
                PolicyDecision::Block,
                Some(PolicyBlockDisposition::Terminal),
                "path is outside workspace",
            ),
            Err(error) => self.evaluation(
                action,
                path,
                PolicyDecision::Block,
                Some(PolicyBlockDisposition::Recoverable),
                &format!("path resolution failed: {error}"),
            ),
        }
    }

    pub fn evaluate_shell(&self, command: &str) -> PolicyEvaluation {
        let parsed = parse_shell_command(command);
        let explicit_wrapper = parsed.as_deref().and_then(explicit_shell_script).is_some();
        let terminal_violation = self
            .denied_shell_fragments
            .iter()
            .any(|fragment| command.contains(fragment))
            || parsed
                .as_deref()
                .is_some_and(|parts| self.shell_command_is_blocked(parts));
        let recoverable_violation =
            !explicit_wrapper && (contains_shell_metacharacter(command) || parsed.is_none());
        let blocked = terminal_violation || recoverable_violation;
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
            PolicyDecision::Allow => "inert workspace command is pre-approved",
            PolicyDecision::Ask => "process command requires explicit user approval",
            PolicyDecision::Block if terminal_violation => {
                "shell command matched P0 deny-list or sensitive-path guard"
            }
            PolicyDecision::Block => {
                "shell command syntax is not permitted; submit one argv command, or explicitly invoke bash -lc with the complete script as one quoted argument"
            }
            PolicyDecision::Deny => "shell command denied by policy",
        };
        let block_disposition = match decision {
            PolicyDecision::Block if terminal_violation => Some(PolicyBlockDisposition::Terminal),
            PolicyDecision::Block => Some(PolicyBlockDisposition::Recoverable),
            PolicyDecision::Allow | PolicyDecision::Ask | PolicyDecision::Deny => None,
        };

        PolicyEvaluation {
            policy_ref: PolicyId::new(),
            subject: "tool".to_owned(),
            action: "shell".to_owned(),
            resource: command.to_owned(),
            decision,
            block_disposition,
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
        if let Some(script) = explicit_shell_script(parts) {
            return self.shell_script_is_blocked(script);
        }
        self.shell_command_parts_are_blocked(parts)
    }

    fn shell_script_is_blocked(&self, script: &str) -> bool {
        if self
            .denied_shell_fragments
            .iter()
            .any(|fragment| script.contains(fragment))
        {
            return true;
        }
        shlex::split(script).map_or_else(
            || {
                self.sensitive_path_fragments
                    .iter()
                    .any(|fragment| script.to_ascii_lowercase().contains(fragment))
            },
            |parts| self.shell_command_parts_are_blocked(&parts),
        )
    }

    fn shell_command_parts_are_blocked(&self, parts: &[String]) -> bool {
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
        parts == ["pwd"]
    }

    fn evaluation(
        &self,
        action: &str,
        resource: &Path,
        decision: PolicyDecision,
        block_disposition: Option<PolicyBlockDisposition>,
        reason: &str,
    ) -> PolicyEvaluation {
        PolicyEvaluation {
            policy_ref: PolicyId::new(),
            subject: "tool".to_owned(),
            action: action.to_owned(),
            resource: resource.display().to_string(),
            decision,
            block_disposition,
            reason: reason.to_owned(),
            evidence_refs: Vec::new(),
        }
    }
}

#[must_use]
pub fn default_workspace_policy_name() -> &'static str {
    "workspace-path-guard"
}

/// Parse the shell tool's command field as argv without invoking a shell.
///
/// Most commands use ordinary `shlex` parsing.  Explicit interpreter wrappers
/// get a narrow fallback because model-produced multiline scripts commonly
/// contain quotes that conflict with the outer quote used to carry the script
/// (for example a heredoc delimiter).  The fallback only accepts the complete
/// `bash|sh|zsh -c|-lc <one argument>` shape and preserves that last argument
/// verbatim for the real interpreter.
#[must_use]
pub fn parse_shell_command(command: &str) -> Option<Vec<String>> {
    if let Some((parts, quote)) = parse_explicit_wrapper_raw(command)
        && quote == '\''
    {
        // A single-quoted script is intentionally opaque to the outer argv
        // parser.  Keeping it verbatim preserves heredoc delimiters and
        // embedded Python/JSON quotes produced by model callers.
        return Some(parts);
    }
    shlex::split(command).or_else(|| parse_explicit_wrapper_raw(command).map(|(parts, _)| parts))
}

/// Return the script argument for a deliberately invoked shell interpreter.
#[must_use]
pub fn explicit_shell_script(parts: &[String]) -> Option<&str> {
    if parts.len() != 3 || !is_shell_program(&parts[0]) {
        return None;
    }
    matches!(parts[1].as_str(), "-c" | "-lc").then_some(parts[2].as_str())
}

fn parse_explicit_wrapper_raw(command: &str) -> Option<(Vec<String>, char)> {
    let trimmed = command.trim();
    let program_end = trimmed.find(char::is_whitespace)?;
    let program = &trimmed[..program_end];
    if !is_shell_program(program) {
        return None;
    }
    let rest = trimmed[program_end..].trim_start();
    let option_end = rest.find(char::is_whitespace)?;
    let option = &rest[..option_end];
    if !matches!(option, "-c" | "-lc") {
        return None;
    }
    let script = rest[option_end..].trim_start();
    let quote = script.chars().next()?;
    if !matches!(quote, '\'' | '"') || !script.ends_with(quote) {
        return None;
    }
    let body = &script[quote.len_utf8()..script.len() - quote.len_utf8()];
    Some((
        vec![program.to_owned(), option.to_owned(), body.to_owned()],
        quote,
    ))
}

fn is_shell_program(program: &str) -> bool {
    matches!(
        program.rsplit(['/', '\\']).next(),
        Some("bash" | "sh" | "zsh")
    )
}

#[must_use]
pub fn contains_shell_metacharacter(command: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;

    for character in command.chars() {
        if character == '\n' {
            return true;
        }
        if escaped {
            escaped = false;
            continue;
        }

        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                }
            }
            Some('"') => match character {
                '"' => quote = None,
                '\\' => escaped = true,
                _ => {}
            },
            Some(_) => unreachable!("quote state only stores shell quote characters"),
            None => match character {
                '\'' | '"' => quote = Some(character),
                '\\' => escaped = true,
                ';' | '|' | '&' | '>' | '<' | '$' | '`' => return true,
                _ => {}
            },
        }
    }

    false
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
        assert_eq!(
            evaluation.block_disposition,
            Some(PolicyBlockDisposition::Terminal)
        );
    }

    #[test]
    fn recoverable_shell_syntax_explains_the_explicit_bash_path() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        let evaluation = policy.evaluate_shell("printf '%s\\n' one two | sort");

        assert_eq!(evaluation.decision, PolicyDecision::Block);
        assert_eq!(
            evaluation.block_disposition,
            Some(PolicyBlockDisposition::Recoverable)
        );
        assert!(evaluation.reason.contains("bash -lc"));
    }

    #[test]
    fn quoted_metacharacters_are_inert_but_unquoted_operators_remain_blocked() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        for command in [
            r#"python -c "import pandas as pd; import pyarrow; print('ok')""#,
            r#"printf '%s' 'a | b & c > d < e $HOME `pwd`'"#,
        ] {
            assert_eq!(
                policy.evaluate_shell(command).decision,
                PolicyDecision::Ask,
                "{command}"
            );
        }

        for command in [
            r#"python -c "print('ok')"; echo second"#,
            r#"python -c "print('ok')" && echo second"#,
            r#"python -c "print('ok')" | cat"#,
            r#"python -c "print('ok')" > output.txt"#,
        ] {
            assert_eq!(
                policy.evaluate_shell(command).decision,
                PolicyDecision::Block,
                "{command}"
            );
            assert_eq!(
                policy.evaluate_shell(command).block_disposition,
                Some(PolicyBlockDisposition::Recoverable),
                "{command}"
            );
        }
    }

    #[test]
    fn explicit_shell_wrappers_allow_pipelines_and_multiline_scripts() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        for command in [
            "bash -lc 'printf one | sort'",
            "sh -c \"printf one > result.txt\"",
            "bash -lc 'python - <<\"PY\"\nprint(\"ok\")\nPY'",
            "bash -lc 'python - <<'PY'\nprint(\"ok\")\nPY'",
        ] {
            let evaluation = policy.evaluate_shell(command);
            assert_eq!(evaluation.decision, PolicyDecision::Ask, "{command}");
            assert_eq!(evaluation.block_disposition, None, "{command}");
        }
    }

    #[test]
    fn explicit_shell_wrappers_still_apply_terminal_guards_to_script_contents() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        for command in [
            "bash -lc 'rm -rf /'",
            "bash -lc 'find . -delete'",
            "bash -lc 'cat .env'",
        ] {
            let evaluation = policy.evaluate_shell(command);
            assert_eq!(evaluation.decision, PolicyDecision::Block, "{command}");
            assert_eq!(
                evaluation.block_disposition,
                Some(PolicyBlockDisposition::Terminal),
                "{command}"
            );
        }
    }

    #[test]
    fn malformed_outer_quotes_are_recovered_for_explicit_wrappers_only() {
        let parsed = parse_shell_command("bash -lc 'python - <<'PY'\nprint('ok')\nPY'")
            .expect("wrapper parser should preserve the script");
        assert_eq!(parsed[0], "bash");
        assert_eq!(parsed[1], "-lc");
        assert!(parsed[2].contains("python - <<'PY'"));
        assert!(parse_shell_command("printf 'unterminated").is_none());
    }

    #[test]
    fn asks_for_unknown_process_commands() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        let evaluation = policy.evaluate_shell("sleep 5");

        assert_eq!(evaluation.decision, PolicyDecision::Ask);
    }

    #[test]
    fn only_preapproves_inert_shell_commands_without_a_sandbox() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        assert_eq!(policy.evaluate_shell("pwd").decision, PolicyDecision::Allow);
        for command in [
            "cargo test -p golutra-core",
            "git status --short",
            "rg 'two words' crates",
            "npm test",
        ] {
            assert_eq!(
                policy.evaluate_shell(command).decision,
                PolicyDecision::Ask,
                "{command} can execute workspace-controlled code or configuration"
            );
        }
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
    fn marks_workspace_escape_and_sensitive_path_blocks_as_terminal() {
        let workspace = tempdir().expect("workspace");
        let outside = tempdir().expect("outside");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        assert_eq!(
            policy
                .evaluate_path("read_file", outside.path(), true)
                .block_disposition,
            Some(PolicyBlockDisposition::Terminal)
        );
        assert_eq!(
            policy.evaluate_shell("rg token .env").block_disposition,
            Some(PolicyBlockDisposition::Terminal)
        );
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
