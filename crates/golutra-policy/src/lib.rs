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
    path_mapper: WorkspacePathMapper,
    sensitive_path_fragments: Vec<String>,
    mode: WorkspacePolicyMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkspacePolicyMode {
    #[default]
    Guarded,
    Unrestricted,
}

/// Maps well-known model/container workspace roots onto the host workspace.
/// Unrecognized absolute paths remain absolute and are rejected by the normal
/// workspace boundary check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePathMapper {
    workspace_root: PathBuf,
    aliases: Vec<PathBuf>,
}

impl WorkspacePathMapper {
    #[must_use]
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            aliases: vec![PathBuf::from("/app"), PathBuf::from("/workspace")],
        }
    }

    #[must_use]
    pub fn map(&self, path: &Path) -> PathBuf {
        if !path.is_absolute() || path.starts_with(&self.workspace_root) {
            return path.to_path_buf();
        }
        self.aliases
            .iter()
            .find_map(|alias| {
                path.strip_prefix(alias)
                    .ok()
                    .map(|relative| self.workspace_root.join(relative))
            })
            .unwrap_or_else(|| path.to_path_buf())
    }

    pub fn add_alias(&mut self, alias: impl Into<PathBuf>) -> Result<(), PolicyError> {
        let alias = alias.into();
        if !alias.is_absolute() {
            return Err(PolicyError::Canonicalization(format!(
                "workspace path alias must be absolute: {}",
                alias.display()
            )));
        }
        if !self.aliases.contains(&alias) {
            self.aliases.push(alias);
        }
        Ok(())
    }
}

impl WorkspacePolicy {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Result<Self, PolicyError> {
        let workspace_root = workspace_root.into();
        if !workspace_root.exists() {
            return Err(PolicyError::MissingWorkspace(
                workspace_root.display().to_string(),
            ));
        }

        let workspace_root = canonicalize_existing(&workspace_root)?;
        Ok(Self {
            path_mapper: WorkspacePathMapper::new(workspace_root.clone()),
            workspace_root,
            sensitive_path_fragments: vec![
                ".env".to_owned(),
                "id_rsa".to_owned(),
                "id_ed25519".to_owned(),
                ".ssh".to_owned(),
                "secrets".to_owned(),
            ],
            mode: WorkspacePolicyMode::Guarded,
        })
    }

    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    #[must_use]
    pub fn mode(&self) -> WorkspacePolicyMode {
        self.mode
    }

    #[must_use]
    pub fn with_unrestricted_access(mut self, enabled: bool) -> Self {
        self.mode = if enabled {
            WorkspacePolicyMode::Unrestricted
        } else {
            WorkspacePolicyMode::Guarded
        };
        self
    }

    pub fn with_path_alias(mut self, alias: impl Into<PathBuf>) -> Result<Self, PolicyError> {
        self.path_mapper.add_alias(alias)?;
        Ok(self)
    }

    pub fn evaluate_path(
        &self,
        action: &str,
        path: impl AsRef<Path>,
        requires_existing_path: bool,
    ) -> PolicyEvaluation {
        let path = path.as_ref();
        match self.resolve_path(path, requires_existing_path) {
            Ok(resolved_path) if self.mode == WorkspacePolicyMode::Unrestricted => self.evaluation(
                action,
                &resolved_path,
                PolicyDecision::Allow,
                None,
                "unrestricted workspace policy enabled",
            ),
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
        if self.mode == WorkspacePolicyMode::Unrestricted {
            return PolicyEvaluation {
                policy_ref: PolicyId::new(),
                subject: "tool".to_owned(),
                action: "shell".to_owned(),
                resource: command.to_owned(),
                decision: PolicyDecision::Allow,
                block_disposition: None,
                reason: "unrestricted workspace policy enabled".to_owned(),
                evidence_refs: Vec::new(),
            };
        }
        let parsed = parse_shell_command_with_input(command);
        let explicit_wrapper = parsed
            .as_ref()
            .and_then(|parsed| explicit_shell_script(&parsed.parts))
            .is_some();
        let direct_stdin = parsed.as_ref().is_some_and(|parsed| parsed.stdin.is_some());
        let terminal_violation = parsed.as_ref().is_some_and(|parsed| {
            self.shell_command_is_blocked(&parsed.parts)
                || parsed
                    .stdin
                    .as_deref()
                    .is_some_and(|stdin| self.references_sensitive_path(stdin))
        });
        let recoverable_violation = !explicit_wrapper
            && !direct_stdin
            && (contains_shell_metacharacter(command) || parsed.is_none());
        let blocked = terminal_violation || recoverable_violation;
        let decision = if blocked {
            PolicyDecision::Block
        } else if parsed.as_ref().is_some_and(|parsed| {
            parsed.stdin.is_none() && self.shell_command_is_preapproved(&parsed.parts)
        }) {
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
                "shell command syntax is not permitted; submit one argv command, a quoted foreground Python heredoc, or explicitly invoke bash -lc with the complete script as one quoted argument"
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
        let mapped_path = self.path_mapper.map(path.as_ref());
        let path = mapped_path.as_path();
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
        let mut parser = tree_sitter::Parser::new();
        if parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
            .is_err()
        {
            return true;
        }
        let Some(tree) = parser.parse(script, None) else {
            return true;
        };
        let root = tree.root_node();
        root.has_error() || self.shell_ast_is_blocked(root, script.as_bytes())
    }

    fn shell_ast_is_blocked(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
        if node.kind() == "variable_assignment"
            && node
                .utf8_text(source)
                .is_ok_and(|assignment| self.references_sensitive_path(assignment))
        {
            return true;
        }
        if node.kind() == "command"
            && node
                .utf8_text(source)
                .ok()
                .and_then(shlex::split)
                .is_none_or(|parts| self.shell_script_command_parts_are_blocked(&parts))
        {
            return true;
        }

        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .any(|child| self.shell_ast_is_blocked(child, source))
    }

    fn shell_script_command_parts_are_blocked(&self, parts: &[String]) -> bool {
        let command_start = parts
            .iter()
            .position(|part| !is_shell_variable_assignment(part))
            .unwrap_or(parts.len());
        self.shell_command_parts_are_blocked(&parts[command_start..])
    }

    fn shell_command_parts_are_blocked(&self, parts: &[String]) -> bool {
        let mut command = parts;
        for _ in 0..8 {
            let Some(program) = command
                .first()
                .and_then(|program| program.rsplit(['/', '\\']).next())
            else {
                return true;
            };
            let arguments = &command[1..];
            let sensitive_argument = arguments
                .iter()
                .any(|argument| self.references_sensitive_path(argument));
            match program {
                "command" => {
                    let Some(target) = command_builtin_target(arguments) else {
                        return sensitive_argument;
                    };
                    command = target;
                }
                "env" => {
                    if sensitive_argument {
                        return true;
                    }
                    if arguments.iter().any(|argument| {
                        argument.starts_with("-S")
                            || argument == "--split-string"
                            || argument.starts_with("--split-string=")
                    }) {
                        return true;
                    }
                    match env_command_target(arguments) {
                        Ok(Some(target)) => command = target,
                        Ok(None) => return false,
                        Err(()) => return true,
                    }
                }
                "busybox" => {
                    let Some(target) = busybox_command_target(arguments) else {
                        return sensitive_argument;
                    };
                    command = target;
                }
                "sudo" => {
                    if sensitive_argument {
                        return true;
                    }
                    match sudo_command_target(arguments) {
                        Ok(Some(target)) => command = target,
                        Ok(None) => return false,
                        Err(()) => return true,
                    }
                }
                "bash" | "sh" | "zsh" => {
                    return explicit_shell_script(command).map_or_else(
                        || sensitive_argument || shell_uses_unparsed_inline_script(arguments),
                        |script| self.shell_script_is_blocked(script),
                    );
                }
                "find" => {
                    return sensitive_argument
                        || arguments.iter().any(|argument| {
                            matches!(
                                argument.as_str(),
                                "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir"
                            )
                        });
                }
                "rg" => return self.rg_references_sensitive_input(arguments),
                "grep" => return self.grep_references_sensitive_input(arguments),
                "rm" => return sensitive_argument || recursive_force_rm_targets_root(arguments),
                "doas" | "pkexec" | "shutdown" | "reboot" => return true,
                _ if program == "mkfs" || program.starts_with("mkfs.") => return true,
                _ => return sensitive_argument,
            }
        }
        true
    }

    fn references_sensitive_path(&self, value: &str) -> bool {
        path_like_components(value).any(|component| {
            matches_sensitive_component(component, ".git")
                || matches_sensitive_component(component, ".golutra")
                || self
                    .sensitive_path_fragments
                    .iter()
                    .any(|fragment| matches_sensitive_component(component, fragment))
        })
    }

    fn grep_references_sensitive_input(&self, arguments: &[String]) -> bool {
        let mut index = 0_usize;
        let mut options = true;
        let mut pattern_supplied = false;

        while let Some(argument) = arguments.get(index) {
            if options && argument == "--" {
                options = false;
                index = index.saturating_add(1);
                continue;
            }

            if options {
                if matches!(argument.as_str(), "-e" | "--regexp") {
                    pattern_supplied = true;
                    index = index.saturating_add(2);
                    continue;
                }
                if argument.starts_with("--regexp=") {
                    pattern_supplied = true;
                    index = index.saturating_add(1);
                    continue;
                }
                if matches!(argument.as_str(), "--exclude" | "--exclude-dir") {
                    index = index.saturating_add(2);
                    continue;
                }
                if argument.starts_with("--exclude=") || argument.starts_with("--exclude-dir=") {
                    index = index.saturating_add(1);
                    continue;
                }
                if matches!(argument.as_str(), "-f" | "--file" | "--exclude-from") {
                    let sensitive = arguments
                        .get(index.saturating_add(1))
                        .is_some_and(|path| self.references_sensitive_path(path));
                    if sensitive {
                        return true;
                    }
                    pattern_supplied |= argument != "--exclude-from";
                    index = index.saturating_add(2);
                    continue;
                }
                if let Some(path) = argument
                    .strip_prefix("--file=")
                    .or_else(|| argument.strip_prefix("--exclude-from="))
                {
                    if self.references_sensitive_path(path) {
                        return true;
                    }
                    pattern_supplied |= argument.starts_with("--file=");
                    index = index.saturating_add(1);
                    continue;
                }
                if let Some((option, value)) = grep_short_pattern_option(argument) {
                    if option == 'f' && self.references_sensitive_path(value) {
                        return true;
                    }
                    pattern_supplied = true;
                    index = index.saturating_add(1);
                    continue;
                }
                if argument.starts_with('-') && argument != "-" {
                    if self.references_sensitive_path(argument) {
                        return true;
                    }
                    index = index.saturating_add(1);
                    continue;
                }
            }

            if pattern_supplied {
                if self.references_sensitive_path(argument) {
                    return true;
                }
            } else {
                pattern_supplied = true;
            }
            index = index.saturating_add(1);
        }

        false
    }

    fn rg_references_sensitive_input(&self, arguments: &[String]) -> bool {
        let mut index = 0_usize;
        let mut options = true;
        let mut pattern_supplied = false;

        while let Some(argument) = arguments.get(index) {
            if options && argument == "--" {
                options = false;
                index = index.saturating_add(1);
                continue;
            }

            if options {
                if argument == "--pre" || argument.starts_with("--pre=") {
                    return true;
                }
                if matches!(argument.as_str(), "-e" | "--regexp") {
                    pattern_supplied = true;
                    index = index.saturating_add(2);
                    continue;
                }
                if argument.starts_with("--regexp=") {
                    pattern_supplied = true;
                    index = index.saturating_add(1);
                    continue;
                }
                if argument == "--files" {
                    pattern_supplied = true;
                    index = index.saturating_add(1);
                    continue;
                }
                if matches!(argument.as_str(), "-f" | "--file") {
                    if arguments
                        .get(index.saturating_add(1))
                        .is_some_and(|path| self.references_sensitive_path(path))
                    {
                        return true;
                    }
                    pattern_supplied = true;
                    index = index.saturating_add(2);
                    continue;
                }
                if let Some(path) = argument.strip_prefix("--file=") {
                    if self.references_sensitive_path(path) {
                        return true;
                    }
                    pattern_supplied = true;
                    index = index.saturating_add(1);
                    continue;
                }
                if let Some((option, value)) = grep_short_pattern_option(argument) {
                    if option == 'f' && self.references_sensitive_path(value) {
                        return true;
                    }
                    pattern_supplied = true;
                    index = index.saturating_add(1);
                    continue;
                }
                if matches!(argument.as_str(), "-g" | "--glob") {
                    if arguments
                        .get(index.saturating_add(1))
                        .is_some_and(|glob| self.rg_glob_reads_sensitive_path(glob))
                    {
                        return true;
                    }
                    index = index.saturating_add(2);
                    continue;
                }
                if let Some(glob) = argument.strip_prefix("--glob=") {
                    if self.rg_glob_reads_sensitive_path(glob) {
                        return true;
                    }
                    index = index.saturating_add(1);
                    continue;
                }
                if let Some(glob) = argument.strip_prefix("-g").filter(|glob| !glob.is_empty()) {
                    if self.rg_glob_reads_sensitive_path(glob) {
                        return true;
                    }
                    index = index.saturating_add(1);
                    continue;
                }
                if matches!(argument.as_str(), "--ignore-file") {
                    if arguments
                        .get(index.saturating_add(1))
                        .is_some_and(|path| self.references_sensitive_path(path))
                    {
                        return true;
                    }
                    index = index.saturating_add(2);
                    continue;
                }
                if let Some(path) = argument.strip_prefix("--ignore-file=") {
                    if self.references_sensitive_path(path) {
                        return true;
                    }
                    index = index.saturating_add(1);
                    continue;
                }
                if argument.starts_with('-') && argument != "-" {
                    if self.references_sensitive_path(argument) {
                        return true;
                    }
                    index = index.saturating_add(1);
                    continue;
                }
            }

            if pattern_supplied {
                if self.references_sensitive_path(argument) {
                    return true;
                }
            } else {
                pattern_supplied = true;
            }
            index = index.saturating_add(1);
        }

        false
    }

    fn rg_glob_reads_sensitive_path(&self, glob: &str) -> bool {
        !glob.starts_with('!') && self.references_sensitive_path(glob)
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedShellCommand {
    pub parts: Vec<String>,
    pub stdin: Option<String>,
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
    parse_shell_command_with_input(command).map(|parsed| parsed.parts)
}

/// Parse one inert argv command and its optional, explicitly quoted Python stdin.
///
/// The only non-argv surface accepted here is a complete foreground
/// `python - <<'DELIMITER'` command. Tree-sitter establishes that the input is
/// exactly one redirected command; the quoted delimiter prevents shell
/// expansion before the body is passed directly to the child process.
#[must_use]
pub fn parse_shell_command_with_input(command: &str) -> Option<ParsedShellCommand> {
    if let Some(parsed) = parse_direct_quoted_python_heredoc(command) {
        return Some(parsed);
    }
    if let Some((parts, quote)) = parse_explicit_wrapper_raw(command)
        && quote == '\''
    {
        // A single-quoted script is intentionally opaque to the outer argv
        // parser.  Keeping it verbatim preserves heredoc delimiters and
        // embedded Python/JSON quotes produced by model callers.
        return Some(ParsedShellCommand { parts, stdin: None });
    }
    shlex::split(command)
        .or_else(|| parse_explicit_wrapper_raw(command).map(|(parts, _)| parts))
        .map(|parts| ParsedShellCommand { parts, stdin: None })
}

fn parse_direct_quoted_python_heredoc(command: &str) -> Option<ParsedShellCommand> {
    let command = command.trim();
    let source = command.as_bytes();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(command, None)?;
    let root = tree.root_node();
    if root.has_error() || root.named_child_count() != 1 {
        return None;
    }
    let statement = root.named_child(0)?;
    if statement.kind() != "redirected_statement" {
        return None;
    }
    let body = statement.child_by_field_name("body")?;
    if body.kind() != "command" {
        return None;
    }
    let mut cursor = statement.walk();
    let redirects = statement
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "heredoc_redirect")
        .collect::<Vec<_>>();
    let [redirect] = redirects.as_slice() else {
        return None;
    };
    if ["descriptor", "operator", "redirect", "right"]
        .iter()
        .any(|field| redirect.child_by_field_name(field).is_some())
    {
        return None;
    }

    let mut parts = shlex::split(body.utf8_text(source).ok()?.trim())?;
    parts.extend(shlex::split(
        std::str::from_utf8(&source[body.end_byte()..redirect.start_byte()])
            .ok()?
            .trim(),
    )?);
    let program = parts
        .first()
        .and_then(|program| program.rsplit(['/', '\\']).next())?;
    if !matches!(program, "python" | "python3")
        || parts.get(1).map(String::as_str) != Some("-")
        || parts.len() != 2
    {
        return None;
    }

    let mut cursor = redirect.walk();
    let children = redirect.named_children(&mut cursor).collect::<Vec<_>>();
    let start = children
        .iter()
        .find(|child| child.kind() == "heredoc_start")?;
    let heredoc_body = children
        .iter()
        .find(|child| child.kind() == "heredoc_body")?;
    let end = children
        .iter()
        .find(|child| child.kind() == "heredoc_end")?;
    if !source[..start.start_byte()].ends_with(b"<<") {
        return None;
    }
    let start_text = start.utf8_text(source).ok()?;
    let delimiter = start_text
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
        .or_else(|| {
            start_text
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
        })?;
    if delimiter.is_empty()
        || delimiter.len() > 64
        || !delimiter
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        || end.utf8_text(source).ok()?.trim() != delimiter
    {
        return None;
    }
    let mut nodes = vec![*heredoc_body];
    while let Some(node) = nodes.pop() {
        if matches!(
            node.kind(),
            "command_substitution" | "expansion" | "simple_expansion"
        ) {
            return None;
        }
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
    }

    Some(ParsedShellCommand {
        parts,
        stdin: Some(heredoc_body.utf8_text(source).ok()?.to_owned()),
    })
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

fn path_like_components(value: &str) -> impl Iterator<Item = &str> {
    value.split(|character: char| {
        !character.is_ascii_alphanumeric() && !matches!(character, '.' | '_' | '-')
    })
}

fn matches_sensitive_component(component: &str, fragment: &str) -> bool {
    let component = component.to_ascii_lowercase();
    let fragment = fragment.to_ascii_lowercase();
    component == fragment
        || component
            .strip_prefix(&fragment)
            .is_some_and(|suffix| suffix.starts_with(['.', '-', '_']))
}

fn grep_short_pattern_option(argument: &str) -> Option<(char, &str)> {
    if !argument.starts_with('-') || argument.starts_with("--") {
        return None;
    }
    argument[1..].char_indices().find_map(|(offset, option)| {
        if !matches!(option, 'e' | 'f') {
            return None;
        }
        let value_start = 1 + offset + option.len_utf8();
        argument
            .get(value_start..)
            .filter(|value| !value.is_empty())
            .map(|value| (option, value))
    })
}

fn recursive_force_rm_targets_root(arguments: &[String]) -> bool {
    let mut recursive = false;
    let mut force = false;
    let mut options = true;
    let mut targets_root = false;

    for argument in arguments {
        if options && argument == "--" {
            options = false;
            continue;
        }
        if options && argument.starts_with("--") {
            recursive |= argument == "--recursive";
            force |= argument == "--force";
            continue;
        }
        if options && argument.starts_with('-') && argument.len() > 1 {
            recursive |= argument[1..].contains(['r', 'R']);
            force |= argument[1..].contains('f');
            continue;
        }
        targets_root |= shell_path_resolves_to_root(argument) || shell_path_is_dynamic(argument);
    }

    recursive && force && targets_root
}

fn is_shell_variable_assignment(value: &str) -> bool {
    let Some((name, _)) = value.split_once('=') else {
        return false;
    };
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn command_builtin_target(arguments: &[String]) -> Option<&[String]> {
    let mut index = 0_usize;
    while let Some(argument) = arguments.get(index) {
        if argument == "--" {
            index = index.saturating_add(1);
            break;
        }
        if !argument.starts_with('-') || argument == "-" {
            break;
        }
        if argument[1..].contains(['v', 'V']) {
            return None;
        }
        if !argument[1..].chars().all(|character| character == 'p') {
            return None;
        }
        index = index.saturating_add(1);
    }
    arguments.get(index..).filter(|target| !target.is_empty())
}

fn env_command_target(arguments: &[String]) -> Result<Option<&[String]>, ()> {
    let mut index = 0_usize;
    let mut options = true;
    while let Some(argument) = arguments.get(index) {
        if options && argument == "--" {
            options = false;
            index = index.saturating_add(1);
            continue;
        }
        if options
            && matches!(
                argument.as_str(),
                "-u" | "--unset" | "-C" | "--chdir" | "-P" | "-a" | "--argv0"
            )
        {
            if arguments.get(index.saturating_add(1)).is_none() {
                return Err(());
            }
            index = index.saturating_add(2);
            continue;
        }
        if options
            && (argument.starts_with("--unset=")
                || argument.starts_with("--chdir=")
                || argument.starts_with("--argv0="))
        {
            index = index.saturating_add(1);
            continue;
        }
        if options && argument.starts_with('-') && argument != "-" {
            if !matches!(
                argument.as_str(),
                "-i" | "--ignore-environment" | "-0" | "--null" | "--debug" | "-v"
            ) {
                return Err(());
            }
            index = index.saturating_add(1);
            continue;
        }
        if is_shell_variable_assignment(argument) {
            index = index.saturating_add(1);
            continue;
        }
        break;
    }
    Ok(arguments.get(index..).filter(|target| !target.is_empty()))
}

fn busybox_command_target(arguments: &[String]) -> Option<&[String]> {
    let target = arguments.first()?;
    (!target.starts_with('-') && target != "busybox").then_some(arguments)
}

fn sudo_command_target(arguments: &[String]) -> Result<Option<&[String]>, ()> {
    const FLAGS: &[&str] = &[
        "--askpass",
        "--background",
        "--bell",
        "--edit",
        "--help",
        "--login",
        "--non-interactive",
        "--preserve-env",
        "--remove-timestamp",
        "--reset-timestamp",
        "--set-home",
        "--stdin",
        "--validate",
        "--version",
    ];
    const VALUE_OPTIONS: &[&str] = &[
        "--chdir",
        "--chroot",
        "--close-from",
        "--command-timeout",
        "--group",
        "--host",
        "--other-user",
        "--prompt",
        "--role",
        "--type",
        "--user",
    ];
    const SHORT_VALUE_OPTIONS: &[char] = &['C', 'D', 'g', 'h', 'p', 'R', 'r', 'T', 't', 'U', 'u'];
    const SHORT_FLAGS: &[char] = &[
        'A', 'b', 'E', 'e', 'H', 'K', 'k', 'l', 'n', 'S', 's', 'V', 'v',
    ];

    let mut index = 0_usize;
    while let Some(argument) = arguments.get(index) {
        if argument == "--" {
            index = index.saturating_add(1);
            break;
        }
        if !argument.starts_with('-') || argument == "-" {
            break;
        }
        if let Some((option, _value)) = argument.split_once('=') {
            if VALUE_OPTIONS.contains(&option) || option == "--preserve-env" {
                index = index.saturating_add(1);
                continue;
            }
            return Err(());
        }
        if VALUE_OPTIONS.contains(&argument.as_str()) {
            if arguments.get(index.saturating_add(1)).is_none() {
                return Err(());
            }
            index = index.saturating_add(2);
            continue;
        }
        if FLAGS.contains(&argument.as_str()) {
            index = index.saturating_add(1);
            continue;
        }
        let Some(options) = argument.strip_prefix('-') else {
            return Err(());
        };
        let mut characters = options.chars();
        let Some(first) = characters.next() else {
            return Err(());
        };
        if SHORT_VALUE_OPTIONS.contains(&first) {
            if characters.as_str().is_empty() {
                if arguments.get(index.saturating_add(1)).is_none() {
                    return Err(());
                }
                index = index.saturating_add(2);
            } else {
                index = index.saturating_add(1);
            }
            continue;
        }
        if std::iter::once(first)
            .chain(characters)
            .all(|option| SHORT_FLAGS.contains(&option))
        {
            index = index.saturating_add(1);
            continue;
        }
        return Err(());
    }
    while arguments
        .get(index)
        .is_some_and(|argument| is_shell_variable_assignment(argument))
    {
        index = index.saturating_add(1);
    }
    Ok(arguments.get(index..).filter(|target| !target.is_empty()))
}

fn shell_uses_unparsed_inline_script(arguments: &[String]) -> bool {
    arguments.iter().any(|argument| {
        argument.starts_with('-') && !argument.starts_with("--") && argument[1..].contains('c')
    })
}

fn shell_path_resolves_to_root(value: &str) -> bool {
    if !value.starts_with('/') {
        return false;
    }
    let mut depth = 0_usize;
    for component in value.split('/') {
        match component {
            "" | "." => {}
            ".." => depth = depth.saturating_sub(1),
            _ => depth = depth.saturating_add(1),
        }
    }
    depth == 0
}

fn shell_path_is_dynamic(value: &str) -> bool {
    value.contains(['$', '`', '*', '?', '[', ']'])
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
    fn unrestricted_mode_allows_outside_sensitive_paths_and_shell_commands() {
        let workspace = tempdir().expect("workspace");
        let outside = tempdir().expect("outside");
        let sensitive_file = outside.path().join("secrets.7z");
        fs::write(&sensitive_file, "secret").expect("outside file");
        let policy = WorkspacePolicy::new(workspace.path())
            .expect("policy")
            .with_unrestricted_access(true);

        assert_eq!(policy.mode(), WorkspacePolicyMode::Unrestricted);
        assert_eq!(
            policy
                .evaluate_path("write_file", &sensitive_file, true)
                .decision,
            PolicyDecision::Allow
        );
        assert_eq!(
            PathBuf::from(
                policy
                    .evaluate_path("write_file", &sensitive_file, true)
                    .resource
            ),
            sensitive_file
                .canonicalize()
                .expect("canonical outside file")
        );
        assert_eq!(
            PathBuf::from(
                policy
                    .evaluate_path("write_file", "/app/result.txt", false)
                    .resource
            ),
            workspace
                .path()
                .canonicalize()
                .expect("canonical workspace")
                .join("result.txt")
        );
        for command in [
            "cat ~/.ssh/id_ed25519",
            "rm -rf /",
            "printf changed > /tmp/golutra-yolo-test",
            "bash -lc 'cat .git/config; touch /tmp/golutra-yolo-test'",
        ] {
            let evaluation = policy.evaluate_shell(command);
            assert_eq!(evaluation.decision, PolicyDecision::Allow, "{command}");
            assert_eq!(evaluation.block_disposition, None, "{command}");
        }
    }

    #[test]
    fn maps_container_workspace_aliases_without_weakening_the_boundary() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        let mapped = policy.evaluate_path("write_file", "/app/result.txt", false);
        let traversal = policy.evaluate_path("write_file", "/app/../outside.txt", false);

        assert_eq!(mapped.decision, PolicyDecision::Allow);
        assert_eq!(
            PathBuf::from(mapped.resource),
            workspace
                .path()
                .canonicalize()
                .expect("canonical workspace")
                .join("result.txt")
        );
        assert_eq!(traversal.decision, PolicyDecision::Block);
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
    fn quoted_python_heredoc_is_parsed_as_direct_stdin() {
        let source =
            "python3 - <<'PY'\nfrom pathlib import Path\nprint(Path('result.txt').read_text())\nPY";
        let parsed = parse_shell_command_with_input(source).expect("quoted Python heredoc");

        assert_eq!(parsed.parts, ["python3", "-"]);
        assert_eq!(
            parsed.stdin.as_deref(),
            Some("from pathlib import Path\nprint(Path('result.txt').read_text())\n")
        );
    }

    #[test]
    fn quoted_python_heredoc_requires_approval_and_keeps_sensitive_guards() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");
        let allowed = "python - <<'PY'\nprint('ok')\nPY";
        let sensitive = "python - <<'PY'\nprint(open('.env').read())\nPY";

        assert_eq!(policy.evaluate_shell(allowed).decision, PolicyDecision::Ask);
        assert_eq!(
            policy.evaluate_shell(sensitive).decision,
            PolicyDecision::Block
        );
        assert_eq!(
            policy.evaluate_shell(sensitive).block_disposition,
            Some(PolicyBlockDisposition::Terminal)
        );
    }

    #[test]
    fn malformed_or_expanding_direct_heredocs_remain_recoverable_blocks() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        for command in [
            "python - <<PY\nprint('ok')\nPY",
            "python - <<'PY'\nprint('ok')\nWRONG",
            "python - <<'PY'\nprint('ok')\nPY\necho extra",
            "python - <<-'PY'\nprint('ok')\nPY",
        ] {
            let evaluation = policy.evaluate_shell(command);
            assert_eq!(evaluation.decision, PolicyDecision::Block, "{command}");
            assert_eq!(
                evaluation.block_disposition,
                Some(PolicyBlockDisposition::Recoverable),
                "{command}"
            );
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
            "bash -lc 'echo ok && /bin/rm -r -f /tmp/..'",
            "bash -lc 'printf ok; /bin/rm --recursive --force /usr/..'",
            "bash -lc 'echo \"$(/bin/rm -rf /opt/..)\"'",
            "bash -lc 'command /bin/rm -r -f /tmp/..'",
            "bash -lc 'env -i MODE=test /bin/rm --recursive --force /usr/..'",
            "sudo /bin/rm -r -f /tmp/..",
            "sudo -n -u root /bin/rm -r -f /tmp/..",
            "sudo --user=root env MODE=test rm --recursive --force /usr/..",
            "doas rm --recursive --force /usr/..",
            "pkexec rm -r -f /var/..",
            "busybox rm -r -f /tmp/..",
            "busybox env MODE=test rm --recursive --force /usr/..",
            "busybox sh -c 'rm -r -f /tmp/..'",
            "busybox sh -xc 'rm -r -f /tmp/..'",
            r#"bash -lc "env -S 'rm -r -f /tmp/..'""#,
            "env -Srm -rf /",
            "env -a harmless sh -c 'rm -rf /'",
            "env --chdir .git cat config",
            "sudo -e .git/config",
            "env --definitely-unknown printf ok",
            r#"bash -lc 'target=/; rm -r -f "$target"'"#,
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
    fn explicit_shell_guards_do_not_treat_quoted_commands_as_executable() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        for command in [
            r#"bash -lc "printf '%s\n' '/bin/rm -rf /tmp/..'""#,
            r#"bash -lc "printf '%s\n' 'rm -rf /'""#,
            r#"bash -lc "echo 'find . -delete'""#,
            "bash -lc 'command -v rm'",
            "bash -lc 'env MODE=test printf ok'",
            "printf '%s' shutdown",
            "touch reboot-notes.txt",
            "printf '%s' mkfs.ext4",
            "busybox rm -rf /tmp/cache",
            "busybox sh -c 'printf ok'",
            "busybox --list",
            "sh script.sh",
            "sudo -n -u root env MODE=test printf ok",
            "env -a harmless printf ok",
        ] {
            let evaluation = policy.evaluate_shell(command);
            assert_eq!(evaluation.decision, PolicyDecision::Ask, "{command}");
            assert_eq!(evaluation.block_disposition, None, "{command}");
        }
    }

    #[test]
    fn grep_exclusion_patterns_do_not_count_as_sensitive_inputs() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        for command in [
            r#"grep -R "token" -n src --exclude-dir=.git"#,
            r#"grep -R --exclude-dir .git "token" src"#,
            r#"grep ".git" README.md"#,
            r#"grep -e ".git" README.md"#,
            r#"bash -lc 'grep -R "token" -n src --exclude-dir=.git | head -20'"#,
        ] {
            let evaluation = policy.evaluate_shell(command);
            assert_eq!(evaluation.decision, PolicyDecision::Ask, "{command}");
            assert_eq!(evaluation.block_disposition, None, "{command}");
        }
    }

    #[test]
    fn grep_still_blocks_sensitive_input_and_pattern_files() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        for command in [
            r#"grep token .git/config"#,
            r#"grep token .env"#,
            r#"grep -f .env README.md"#,
            r#"grep --file=.git/patterns README.md"#,
            r#"bash -lc 'target=.env; grep token "$target"'"#,
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
    fn search_exclusion_globs_do_not_count_as_sensitive_inputs() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        for command in [
            r#"rg token src --glob '!.git/**'"#,
            r#"rg --glob !.git/** token src"#,
            r#"rg -g!.git/** token src"#,
            r#"bash -lc 'rg token src --glob "!.git/**" | head -20'"#,
        ] {
            let evaluation = policy.evaluate_shell(command);
            assert_eq!(evaluation.decision, PolicyDecision::Ask, "{command}");
            assert_eq!(evaluation.block_disposition, None, "{command}");
        }
    }

    #[test]
    fn search_options_and_paths_cannot_include_sensitive_inputs() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        for command in [
            r#"rg token .git/config"#,
            r#"rg --hidden token .git"#,
            r#"rg --glob .git/** token ."#,
            r#"rg -g.git/** token ."#,
            r#"rg --files .git"#,
            r#"rg --ignore-file .env token src"#,
            r#"rg -f .git/config token src"#,
            r#"rg -f.git/config token src"#,
            r#"rg --file=.git/config token src"#,
            r#"grep --include=.git/config token ."#,
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
    fn root_delete_guard_does_not_match_absolute_subpaths() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        for command in [
            "bash -lc 'rm -rf /git/project'",
            "bash -lc 'rm -rf /var/www/staging && mkdir -p /var/www/staging'",
        ] {
            let evaluation = policy.evaluate_shell(command);
            assert_eq!(evaluation.decision, PolicyDecision::Ask, "{command}");
            assert_eq!(evaluation.block_disposition, None, "{command}");
        }
    }

    #[test]
    fn root_delete_guard_normalizes_absolute_root_aliases() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        for command in [
            "rm -rf //",
            "rm -rf /./",
            "rm -rf /tmp/..",
            "rm -r -f /var/../",
            "rm --recursive --force /usr/..",
            "/bin/rm -rf -- /tmp/..",
            "bash -lc 'rm -rf /opt/..'",
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
    fn root_delete_guard_allows_normalized_absolute_subpaths() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        for command in [
            "rm -rf /git/project/./cache",
            "rm -rf /git/project/tmp/../cache",
            "bash -lc 'rm -r -f /var/www/../staging'",
        ] {
            let evaluation = policy.evaluate_shell(command);
            assert_eq!(evaluation.decision, PolicyDecision::Ask, "{command}");
            assert_eq!(evaluation.block_disposition, None, "{command}");
        }
    }

    #[test]
    fn sensitive_path_guard_matches_path_components_without_blocking_identifiers() {
        let workspace = tempdir().expect("workspace");
        let policy = WorkspacePolicy::new(workspace.path()).expect("policy");

        for command in [
            r#"python -c "import os; print(os.environ)""#,
            "bash -lc 'python3 - <<\"PY\"\nimport os\nenv=os.environ.copy()\nprint(env)\nPY'",
        ] {
            assert_eq!(
                policy.evaluate_shell(command).decision,
                PolicyDecision::Ask,
                "{command}"
            );
        }

        for command in [
            "bash -lc 'cat .env.production'",
            "bash -lc 'cat \"$HOME/.ssh/id_rsa\"'",
            "bash -lc 'cat --config=/tmp/secrets.json'",
        ] {
            assert_eq!(
                policy.evaluate_shell(command).decision,
                PolicyDecision::Block,
                "{command}"
            );
            assert_eq!(
                policy.evaluate_shell(command).block_disposition,
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
