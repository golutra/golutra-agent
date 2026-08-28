//! Objective-evidence recognition and normalization.
//!
//! AgentLoop consumes the bounded outcomes exposed here; shell/Python compatibility
//! heuristics stay local to this module and do not participate in loop orchestration.

use std::{collections::HashSet, path::Path, sync::LazyLock};

use golutra_core::{TaskContract, ToolResultStatus, VerificationRequirement};
use golutra_policy::parse_shell_command_with_input;
use golutra_tools::{ToolExecutionReport, ToolRequest, redact_sensitive_text};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ObjectiveValidationKind {
    Test,
    Diagnostic,
    FileState,
}

impl ObjectiveValidationKind {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Diagnostic => "diagnostic",
            Self::FileState => "file_state",
        }
    }

    fn from_label(value: &str) -> Option<Self> {
        match value {
            "test" => Some(Self::Test),
            "diagnostic" => Some(Self::Diagnostic),
            "file_state" => Some(Self::FileState),
            _ => None,
        }
    }
}

const PREPARED_OBJECTIVE_VALIDATION_FACT: &str = "runtime_objective_validation";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ObjectiveValidationOutcome {
    pub(super) kind: ObjectiveValidationKind,
    pub(super) identity: String,
    pub(super) passed: bool,
    pub(super) message: String,
}

pub(super) fn objective_validation_report(
    report: &ToolExecutionReport,
) -> Option<ObjectiveValidationOutcome> {
    if report.envelope.tool_name == "external_verifier" {
        return Some(ObjectiveValidationOutcome {
            kind: ObjectiveValidationKind::Test,
            identity: "external-verifier".to_owned(),
            passed: report.envelope.status == ToolResultStatus::Ok,
            message: report.envelope.summary.clone(),
        });
    }
    if report.envelope.tool_name == "shell" {
        if let Some(outcome) = prepared_objective_validation_report(report) {
            return Some(outcome);
        }
        let command = report
            .envelope
            .structured_facts
            .get("command")
            .and_then(serde_json::Value::as_str)?;
        let kind = objective_validation_command_kind(command)?;
        let identity = objective_validation_command_identity(command)?;
        let exited_cleanly = shell_report_exited_cleanly(report);
        let passed = exited_cleanly
            && (kind != ObjectiveValidationKind::Test || test_report_executed_tests(report));
        let message = match (kind, exited_cleanly, passed) {
            (_, false, _) => "validation command did not exit successfully".to_owned(),
            (ObjectiveValidationKind::Test, true, false) => {
                "test command exited successfully but no executed test was observed".to_owned()
            }
            (ObjectiveValidationKind::Test, true, true) => {
                "test command passed with executed tests".to_owned()
            }
            (ObjectiveValidationKind::FileState, true, true) => {
                "file-state command passed".to_owned()
            }
            (_, true, true) => "diagnostic command passed".to_owned(),
            _ => "objective validation is unresolved".to_owned(),
        };
        return Some(ObjectiveValidationOutcome {
            kind,
            identity,
            passed,
            message,
        });
    }
    None
}

pub(super) fn prepare_objective_validation_metadata(request: &ToolRequest) -> Option<Value> {
    if request.tool_name != "shell" {
        return None;
    }
    let command = request.arguments.get("command").and_then(Value::as_str)?;
    let kind = objective_validation_command_kind(command)?;
    let identity = safe_objective_validation_command_identity(command)?;
    Some(serde_json::json!({
        "kind": kind.label(),
        "identity": identity,
    }))
}

pub(super) fn attach_prepared_objective_validation(
    report: &mut ToolExecutionReport,
    metadata: Option<Value>,
) {
    let Some(metadata) = metadata else {
        return;
    };
    if let Some(facts) = report.envelope.structured_facts.as_object_mut() {
        facts.insert(PREPARED_OBJECTIVE_VALIDATION_FACT.to_owned(), metadata);
    }
}

fn prepared_objective_validation_report(
    report: &ToolExecutionReport,
) -> Option<ObjectiveValidationOutcome> {
    let metadata = report
        .envelope
        .structured_facts
        .get(PREPARED_OBJECTIVE_VALIDATION_FACT)?;
    let kind = ObjectiveValidationKind::from_label(metadata.get("kind")?.as_str()?)?;
    let identity = metadata.get("identity")?.as_str()?.to_owned();
    let exited_cleanly = shell_report_exited_cleanly(report);
    let executed_tests =
        kind != ObjectiveValidationKind::Test || test_report_executed_tests(report);
    let passed = exited_cleanly && executed_tests;
    let message = if !exited_cleanly {
        "validation command did not exit successfully".to_owned()
    } else if kind == ObjectiveValidationKind::Test && !executed_tests {
        "test command exited successfully but no executed test was observed".to_owned()
    } else {
        match kind {
            ObjectiveValidationKind::Test => "test command passed with executed tests".to_owned(),
            ObjectiveValidationKind::FileState => "file-state command passed".to_owned(),
            ObjectiveValidationKind::Diagnostic => "diagnostic command passed".to_owned(),
        }
    };
    Some(ObjectiveValidationOutcome {
        kind,
        identity,
        passed,
        message,
    })
}

fn shell_report_exited_cleanly(report: &ToolExecutionReport) -> bool {
    report.envelope.tool_name == "shell"
        && report.envelope.status == ToolResultStatus::Ok
        && report
            .envelope
            .structured_facts
            .get("exit_code")
            .and_then(Value::as_i64)
            == Some(0)
        && !report
            .envelope
            .structured_facts
            .get("timed_out")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        && !report
            .envelope
            .structured_facts
            .get("cancelled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

pub(super) fn explicitly_requested_inspection_validation(
    report: &ToolExecutionReport,
    objective: &str,
    completion_criteria: &[String],
    contract: &TaskContract,
    workspace_root: &Path,
) -> Option<ObjectiveValidationOutcome> {
    if contract.requires_workspace_evidence()
        || contract.require_objective_validation
        || matches!(
            contract.verification,
            VerificationRequirement::Required | VerificationRequirement::Independent
        )
    {
        return None;
    }
    if !matches!(report.envelope.tool_name.as_str(), "read_file" | "list_dir") {
        return None;
    }
    let resource = report
        .envelope
        .structured_facts
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or(&report.policy_evaluation.resource);
    let resource_path = Path::new(resource);
    let relative = resource_path
        .strip_prefix(workspace_root)
        .unwrap_or(resource_path)
        .to_string_lossy()
        .replace('\\', "/");
    if relative.is_empty() || relative == "." {
        return None;
    }
    let requested_text = std::iter::once(objective)
        .chain(completion_criteria.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    let relative_lower = relative.to_ascii_lowercase();
    let file_name = Path::new(&relative)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_ascii_lowercase);
    let explicitly_requested = requested_text.contains(&relative_lower)
        || file_name
            .as_deref()
            .is_some_and(|name| name.len() >= 3 && name != "." && requested_text.contains(name));
    if !explicitly_requested {
        return None;
    }
    let passed = report.envelope.status == ToolResultStatus::Ok;
    Some(ObjectiveValidationOutcome {
        kind: ObjectiveValidationKind::Diagnostic,
        identity: format!("inspection:{:x}", Sha256::digest(relative_lower.as_bytes())),
        passed,
        message: if passed {
            format!("explicitly requested workspace input was inspected: {relative}")
        } else {
            format!("explicitly requested workspace input could not be inspected: {relative}")
        },
    })
}

#[cfg(test)]
pub(super) fn is_objective_validation_command(command: &str) -> bool {
    objective_validation_command_kind(command).is_some()
}

pub(super) fn objective_validation_command_kind(command: &str) -> Option<ObjectiveValidationKind> {
    objective_validation_command_kind_with_depth(command, 0)
}

pub(super) fn objective_validation_command_identity(command: &str) -> Option<String> {
    let atoms = objective_validation_command_atoms_with_depth(command, 0)?;
    if atoms.is_empty() {
        return None;
    }
    let mut digest = Sha256::new();
    for atom in atoms {
        digest.update((atom.len() as u64).to_le_bytes());
        digest.update(atom.as_bytes());
    }
    Some(format!("{:x}", digest.finalize()))
}

fn safe_objective_validation_command_identity(command: &str) -> Option<String> {
    let atoms = objective_validation_command_atoms_with_depth(command, 0)?;
    if atoms.is_empty() {
        return None;
    }
    let mut digest = Sha256::new();
    for atom in atoms {
        let redacted = redact_sensitive_text(&atom).0;
        digest.update((redacted.len() as u64).to_le_bytes());
        digest.update(redacted.as_bytes());
    }
    Some(format!("{:x}", digest.finalize()))
}

fn objective_validation_command_atoms_with_depth(
    command: &str,
    wrapper_depth: u8,
) -> Option<Vec<String>> {
    let parsed = parse_shell_command_with_input(command)?;
    let mut parts = parsed.parts;
    let program = parts.first().map(String::as_str)?;
    let program = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .to_owned();
    if wrapper_depth < 2
        && matches!(program.as_str(), "bash" | "sh" | "zsh")
        && parts.len() == 3
        && matches!(parts[1].as_str(), "-c" | "-lc")
    {
        return objective_validation_shell_script_atoms(
            parts[2].trim(),
            wrapper_depth.saturating_add(1),
        );
    }
    if let Some(stdin) = parsed.stdin {
        objective_validation_command_kind_with_depth(command, wrapper_depth)?;
        return Some(vec![
            serde_json::to_string(&(program, "stdin", stdin)).ok()?,
        ]);
    }
    objective_validation_command_kind_with_depth(command, wrapper_depth)?;
    parts[0] = program;
    Some(vec![serde_json::to_string(&parts).ok()?])
}

fn objective_validation_shell_script_atoms(script: &str, wrapper_depth: u8) -> Option<Vec<String>> {
    objective_validation_shell_script_kind(script, wrapper_depth)?;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(script, None)?;
    let root = tree.root_node();
    let mut atoms = Vec::new();
    if !collect_objective_validation_atoms(root, script.as_bytes(), wrapper_depth, &mut atoms)
        || atoms.is_empty()
    {
        return None;
    }
    Some(atoms)
}

fn collect_objective_validation_atoms(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    wrapper_depth: u8,
    atoms: &mut Vec<String>,
) -> bool {
    match node.kind() {
        "program" | "list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if !collect_objective_validation_atoms(child, source, wrapper_depth, atoms) {
                    return false;
                }
            }
            true
        }
        "command" => {
            let Ok(command) = node.utf8_text(source) else {
                return false;
            };
            if let Some(mut command_atoms) =
                objective_validation_command_atoms_with_depth(command.trim(), wrapper_depth)
            {
                atoms.append(&mut command_atoms);
            }
            true
        }
        "redirected_statement" => {
            if let Some((_, atom)) =
                objective_validation_python_heredoc(node, source, wrapper_depth)
            {
                atoms.push(atom);
            }
            true
        }
        "pipeline" | "test_command" => {
            if objective_validation_statement_kind(node, source, wrapper_depth).is_some() {
                let Ok(statement) = node.utf8_text(source) else {
                    return false;
                };
                let Ok(atom) = serde_json::to_string(&(node.kind(), statement.trim())) else {
                    return false;
                };
                atoms.push(atom);
            }
            true
        }
        "comment" | "variable_assignment" => true,
        _ => false,
    }
}

fn objective_validation_command_kind_with_depth(
    command: &str,
    wrapper_depth: u8,
) -> Option<ObjectiveValidationKind> {
    let parsed = parse_shell_command_with_input(command)?;
    let parts = parsed.parts;
    let program = parts.first().map(String::as_str)?;
    let program = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    if wrapper_depth < 2
        && matches!(program, "bash" | "sh" | "zsh")
        && parts.len() == 3
        && matches!(parts[1].as_str(), "-c" | "-lc")
    {
        return objective_validation_shell_script_kind(
            parts[2].trim(),
            wrapper_depth.saturating_add(1),
        );
    }
    if let Some(stdin) = parsed.stdin {
        return (matches!(program, "python" | "python3")
            && parts.get(1).map(String::as_str) == Some("-")
            && parts.len() == 2
            && python_source_asserts_runtime_state(&stdin))
        .then_some(ObjectiveValidationKind::Diagnostic);
    }
    match program {
        "cargo" => cargo_validation_kind(&parts),
        "npm" | "pnpm" | "yarn" | "bun" => package_manager_validation_kind(&parts),
        "pytest" => Some(ObjectiveValidationKind::Test),
        "python" | "python3" if python_module_runs_tests(&parts) => {
            Some(ObjectiveValidationKind::Test)
        }
        "python" | "python3" if python_inline_asserts_runtime_state(&parts) => {
            Some(ObjectiveValidationKind::Diagnostic)
        }
        "curl" if curl_is_fail_fast_http_probe(&parts) => Some(ObjectiveValidationKind::Diagnostic),
        "cmp" | "diff" if comparison_has_two_operands(&parts) => {
            Some(ObjectiveValidationKind::Diagnostic)
        }
        "test" if test_command_validates_file_state(&parts) => {
            Some(ObjectiveValidationKind::FileState)
        }
        "test" if test_command_validates_runtime_comparison(&parts) => {
            Some(ObjectiveValidationKind::Diagnostic)
        }
        "grep" | "rg" if quiet_content_check(&parts, false) => {
            Some(ObjectiveValidationKind::Diagnostic)
        }
        "git" if git_command_validates_result(&parts) => Some(ObjectiveValidationKind::Diagnostic),
        "go" if parts.get(1).is_some_and(|part| part == "test") => {
            Some(ObjectiveValidationKind::Test)
        }
        "make" => build_task_validation_kind(&parts, MAKE_OPTIONS_WITH_VALUES),
        "mvn" | "mvnw" => build_task_validation_kind(&parts, MAVEN_OPTIONS_WITH_VALUES),
        "gradle" | "gradlew" => build_task_validation_kind(&parts, GRADLE_OPTIONS_WITH_VALUES),
        "swift" => swift_validation_kind(&parts),
        _ => None,
    }
}

const CARGO_OPTIONS_WITH_VALUES: &[&str] = &[
    "--color",
    "--config",
    "--manifest-path",
    "--target-dir",
    "-C",
    "-Z",
];

const PACKAGE_MANAGER_OPTIONS_WITH_VALUES: &[&str] = &[
    "--access",
    "--before",
    "--cache",
    "--cache-folder",
    "--cwd",
    "--dir",
    "--filter",
    "--heading",
    "--loglevel",
    "--modules-folder",
    "--mutex",
    "--network-timeout",
    "--otp",
    "--prefix",
    "--registry",
    "--scope",
    "--script-shell",
    "--tag",
    "--userconfig",
    "--use-yarnrc",
    "--viewer",
    "--workspace",
    "-C",
    "-w",
];

const MAKE_OPTIONS_WITH_VALUES: &[&str] = &[
    "--directory",
    "--eval",
    "--file",
    "--include-dir",
    "--jobs",
    "--load-average",
    "--old-file",
    "--what-if",
    "-C",
    "-I",
    "-W",
    "-f",
    "-j",
    "-l",
    "-o",
];

const MAVEN_OPTIONS_WITH_VALUES: &[&str] = &[
    "--builder",
    "--file",
    "--global-settings",
    "--projects",
    "--resume-from",
    "--settings",
    "--threads",
    "--toolchains",
    "-b",
    "-f",
    "-gs",
    "-pl",
    "-rf",
    "-s",
    "-t",
    "-T",
];

const GRADLE_OPTIONS_WITH_VALUES: &[&str] = &[
    "--build-file",
    "--console",
    "--gradle-user-home",
    "--include-build",
    "--init-script",
    "--max-workers",
    "--priority",
    "--project-dir",
    "--settings-file",
    "--warning-mode",
    "-b",
    "-c",
    "-g",
    "-I",
    "-p",
];

const SWIFT_OPTIONS_WITH_VALUES: &[&str] = &[
    "--cache-path",
    "--chdir",
    "--config-path",
    "--package-path",
    "--scratch-path",
    "--security-path",
];

fn cargo_validation_kind(parts: &[String]) -> Option<ObjectiveValidationKind> {
    let mut operands = positional_operands(parts, CARGO_OPTIONS_WITH_VALUES)
        .into_iter()
        .filter(|argument| !argument.starts_with('+'));
    let subcommand = operands.next()?;
    match subcommand {
        "test" => Some(ObjectiveValidationKind::Test),
        "build" | "check" | "clippy" => Some(ObjectiveValidationKind::Diagnostic),
        "fmt" if parts.iter().any(|part| part == "--check") => {
            Some(ObjectiveValidationKind::Diagnostic)
        }
        _ => None,
    }
}

fn package_manager_validation_kind(parts: &[String]) -> Option<ObjectiveValidationKind> {
    let operands = positional_operands(parts, PACKAGE_MANAGER_OPTIONS_WITH_VALUES);
    let (command, remaining) = operands.split_first()?;
    if matches!(*command, "run" | "run-script") {
        return remaining
            .first()
            .and_then(|script| named_validation_task_kind(script));
    }
    named_validation_task_kind(command)
}

fn build_task_validation_kind(
    parts: &[String],
    options_with_values: &[&str],
) -> Option<ObjectiveValidationKind> {
    positional_operands(parts, options_with_values)
        .into_iter()
        .filter(|argument| !argument.contains('='))
        .filter_map(named_validation_task_kind)
        .fold(None, |current, kind| {
            Some(stronger_validation_kind(current, kind))
        })
}

fn swift_validation_kind(parts: &[String]) -> Option<ObjectiveValidationKind> {
    positional_operands(parts, SWIFT_OPTIONS_WITH_VALUES)
        .first()
        .and_then(|subcommand| named_validation_task_kind(subcommand))
}

fn positional_operands<'a>(parts: &'a [String], options_with_values: &[&str]) -> Vec<&'a str> {
    let mut operands = Vec::new();
    let mut index = 1_usize;
    while let Some(argument) = parts.get(index).map(String::as_str) {
        if argument == "--" {
            break;
        }
        if argument.starts_with('-') && argument != "-" {
            index = index.saturating_add(if options_with_values.contains(&argument) {
                2
            } else {
                1
            });
            continue;
        }
        operands.push(argument);
        index = index.saturating_add(1);
    }
    operands
}

fn named_validation_task_kind(task: &str) -> Option<ObjectiveValidationKind> {
    let words = task_name_words(task);
    if words
        .iter()
        .any(|word| matches!(word.as_str(), "test" | "tests"))
    {
        return Some(ObjectiveValidationKind::Test);
    }
    words
        .iter()
        .any(|word| {
            matches!(
                word.as_str(),
                "build" | "check" | "lint" | "typecheck" | "verify"
            )
        })
        .then_some(ObjectiveValidationKind::Diagnostic)
}

fn task_name_words(task: &str) -> Vec<String> {
    let characters = task.chars().collect::<Vec<_>>();
    let mut words = Vec::new();
    let mut current = String::new();
    for (index, character) in characters.iter().copied().enumerate() {
        if !character.is_ascii_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        let previous = index.checked_sub(1).and_then(|index| characters.get(index));
        let next = characters.get(index.saturating_add(1));
        let starts_word = character.is_ascii_uppercase()
            && !current.is_empty()
            && (previous.is_some_and(|value| value.is_ascii_lowercase() || value.is_ascii_digit())
                || next.is_some_and(char::is_ascii_lowercase));
        if starts_word {
            words.push(std::mem::take(&mut current));
        }
        current.push(character.to_ascii_lowercase());
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn curl_is_fail_fast_http_probe(parts: &[String]) -> bool {
    let fail_fast = parts.iter().skip(1).any(|part| {
        matches!(part.as_str(), "--fail" | "--fail-with-body")
            || (part.starts_with('-')
                && !part.starts_with("--")
                && part.chars().skip(1).any(|flag| flag == 'f'))
    });
    fail_fast
        && parts
            .iter()
            .skip(1)
            .any(|part| part.starts_with("http://") || part.starts_with("https://"))
}

fn objective_validation_shell_script_kind(
    script: &str,
    wrapper_depth: u8,
) -> Option<ObjectiveValidationKind> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(script, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }

    let source = script.as_bytes();
    if root.named_child_count() == 1 {
        let statement = root.named_child(0)?;
        if let Some((kind, _)) =
            objective_validation_python_heredoc(statement, source, wrapper_depth)
        {
            return Some(kind);
        }
        if statement.kind() == "list" {
            return objective_validation_and_chain_kind(statement, source, wrapper_depth);
        }
        if statement.kind() == "command" && !shell_script_has_unsafe_control_flow(statement) {
            let command = statement.utf8_text(source).ok()?.trim();
            let parts = shlex::split(command)?;
            if shell_command_can_change_or_skip_validation(&parts) {
                return None;
            }
            return objective_validation_command_kind_with_depth(command, wrapper_depth);
        }
    }
    let mut validation = None;
    if collect_fail_fast_validation(root, source, wrapper_depth, &mut validation)
        && validation.is_some()
    {
        return validation;
    }
    collect_terminal_statement_validation(root, source, wrapper_depth)
}

fn collect_fail_fast_validation(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    wrapper_depth: u8,
    validation: &mut Option<ObjectiveValidationKind>,
) -> bool {
    let mut fail_fast = false;
    collect_fail_fast_nodes(root, source, wrapper_depth, &mut fail_fast, validation) && fail_fast
}

fn collect_fail_fast_nodes(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    wrapper_depth: u8,
    fail_fast: &mut bool,
    validation: &mut Option<ObjectiveValidationKind>,
) -> bool {
    if matches!(node.kind(), "program" | "list") {
        for index in 0..node.child_count() {
            let Some(child) = node.child(index) else {
                return false;
            };
            if child.is_named() {
                if !collect_fail_fast_nodes(child, source, wrapper_depth, fail_fast, validation) {
                    return false;
                }
                continue;
            }
            let Ok(operator) = child.utf8_text(source) else {
                return false;
            };
            if !matches!(operator.trim(), "" | ";" | "&&") {
                return false;
            }
        }
        return true;
    }

    match node.kind() {
        "comment" => true,
        "command" => {
            let Ok(command) = node.utf8_text(source) else {
                return false;
            };
            let Some(parts) = shlex::split(command.trim()) else {
                return false;
            };
            if !*fail_fast {
                if !shell_command_enables_errexit(&parts) {
                    return false;
                }
                *fail_fast = true;
                return true;
            }
            if shell_command_can_change_or_skip_validation(&parts) {
                return false;
            }
            if let Some(kind) = objective_validation_statement_kind(node, source, wrapper_depth) {
                *validation = Some(stronger_validation_kind(*validation, kind));
                true
            } else {
                validation.is_none() || shell_statement_is_read_only(node, source)
            }
        }
        "variable_assignment" => *fail_fast && shell_assignment_is_safe(node, source),
        "test_command" if *fail_fast => {
            if let Some(kind) = objective_validation_statement_kind(node, source, wrapper_depth) {
                *validation = Some(stronger_validation_kind(*validation, kind));
            }
            true
        }
        "redirected_statement" if *fail_fast => {
            if let Some(kind) = objective_validation_statement_kind(node, source, wrapper_depth) {
                *validation = Some(stronger_validation_kind(*validation, kind));
                true
            } else {
                validation.is_none() && shell_setup_statement_is_allowed(node, source)
            }
        }
        "pipeline" if *fail_fast => {
            if let Some(kind) = objective_validation_statement_kind(node, source, wrapper_depth) {
                *validation = Some(stronger_validation_kind(*validation, kind));
                true
            } else if validation.is_some() {
                shell_statement_is_read_only(node, source)
            } else {
                shell_setup_statement_is_allowed(node, source)
            }
        }
        _ => false,
    }
}

fn collect_terminal_statement_validation(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    wrapper_depth: u8,
) -> Option<ObjectiveValidationKind> {
    for index in 0..root.child_count() {
        let child = root.child(index)?;
        if !child.is_named()
            && !child
                .utf8_text(source)
                .is_ok_and(|operator| matches!(operator.trim(), "" | ";"))
        {
            return None;
        }
    }
    let mut cursor = root.walk();
    let statements = root
        .named_children(&mut cursor)
        .filter(|node| node.kind() != "comment")
        .collect::<Vec<_>>();
    let (last, setup) = statements.split_last()?;
    if setup
        .iter()
        .any(|node| !shell_terminal_setup_statement_is_allowed(*node, source))
    {
        return None;
    }
    objective_validation_statement_kind(*last, source, wrapper_depth)
}

fn shell_terminal_setup_statement_is_allowed(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    match node.kind() {
        "comment" => true,
        "variable_assignment" => shell_assignment_is_safe(node, source),
        "command" | "pipeline" => shell_statement_is_read_only(node, source),
        "redirected_statement" => node
            .child_by_field_name("body")
            .is_some_and(|body| shell_setup_statement_is_allowed(body, source)),
        _ => false,
    }
}

fn objective_validation_statement_kind(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    wrapper_depth: u8,
) -> Option<ObjectiveValidationKind> {
    match node.kind() {
        "command" if !shell_script_has_unsafe_control_flow(node) => {
            objective_validation_command_kind_with_depth(
                node.utf8_text(source).ok()?.trim(),
                wrapper_depth,
            )
        }
        "redirected_statement" => {
            objective_validation_python_heredoc(node, source, wrapper_depth).map(|(kind, _)| kind)
        }
        "pipeline" => objective_validation_pipeline_kind(node, source),
        "test_command" => objective_validation_test_node_kind(node, source),
        _ => None,
    }
}

fn objective_validation_pipeline_kind(
    pipeline: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<ObjectiveValidationKind> {
    let mut cursor = pipeline.walk();
    let commands = pipeline
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "command")
        .collect::<Vec<_>>();
    let (last, inputs) = commands.split_last()?;
    if inputs.is_empty()
        || inputs
            .iter()
            .any(|command| !shell_statement_is_read_only(*command, source))
    {
        return None;
    }
    let parts = shlex::split(last.utf8_text(source).ok()?.trim())?;
    quiet_content_check(&parts, true).then_some(ObjectiveValidationKind::Diagnostic)
}

fn objective_validation_test_node_kind(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<ObjectiveValidationKind> {
    let text = node.utf8_text(source).ok()?.trim();
    let inner = text
        .strip_prefix("[[")
        .and_then(|value| value.strip_suffix("]]"))
        .or_else(|| {
            text.strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
        })?;
    let mut parts = vec!["test".to_owned()];
    parts.extend(shlex::split(inner.trim())?);
    if test_command_validates_file_state(&parts) {
        Some(ObjectiveValidationKind::FileState)
    } else if test_command_validates_runtime_comparison(&parts) {
        Some(ObjectiveValidationKind::Diagnostic)
    } else {
        None
    }
}

fn shell_setup_statement_is_allowed(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    match node.kind() {
        "comment" => true,
        "variable_assignment" => shell_assignment_is_safe(node, source),
        "command" => shlex::split(node.utf8_text(source).unwrap_or_default().trim())
            .is_some_and(|parts| !shell_command_can_change_or_skip_validation(&parts)),
        "redirected_statement" => node
            .child_by_field_name("body")
            .is_some_and(|body| shell_setup_statement_is_allowed(body, source)),
        "pipeline" => {
            let mut cursor = node.walk();
            let commands = node.named_children(&mut cursor).collect::<Vec<_>>();
            !commands.is_empty()
                && commands
                    .iter()
                    .all(|command| shell_setup_statement_is_allowed(*command, source))
        }
        _ => false,
    }
}

fn shell_statement_is_read_only(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    match node.kind() {
        "command" => shlex::split(node.utf8_text(source).unwrap_or_default().trim())
            .is_some_and(|parts| shell_command_is_read_only(&parts)),
        "pipeline" => {
            let mut cursor = node.walk();
            let commands = node.named_children(&mut cursor).collect::<Vec<_>>();
            !commands.is_empty()
                && commands
                    .iter()
                    .all(|command| shell_statement_is_read_only(*command, source))
        }
        _ => false,
    }
}

pub(super) fn shell_command_is_read_only(parts: &[String]) -> bool {
    let Some(program) = parts.first().and_then(|program| {
        Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
    }) else {
        return false;
    };
    match program {
        "cat" | "cut" | "du" | "file" | "grep" | "head" | "ls" | "printf" | "pwd" | "readlink"
        | "rg" | "sort" | "stat" | "strings" | "tail" | "tr" | "uniq" | "wc" => true,
        "find" => !parts.iter().any(|part| {
            matches!(
                part.as_str(),
                "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir"
            )
        }),
        "git" => match parts.get(1).map(String::as_str) {
            Some("branch") => parts.iter().skip(2).all(|part| part == "--show-current"),
            Some("diff" | "log" | "merge-base" | "rev-parse" | "show" | "status") => true,
            _ => false,
        },
        "tmux" => match parts.get(1).map(String::as_str) {
            Some("capture-pane") => {
                parts.iter().any(|part| part == "-p") && !parts.iter().any(|part| part == "-b")
            }
            Some("display-message") => parts.iter().any(|part| part == "-p"),
            Some(
                "has-session"
                | "list-buffers"
                | "list-clients"
                | "list-commands"
                | "list-keys"
                | "list-panes"
                | "list-sessions"
                | "list-windows"
                | "server-info"
                | "show-environment"
                | "show-hooks"
                | "show-messages"
                | "show-options"
                | "show-window-options",
            ) => true,
            _ => false,
        },
        _ => false,
    }
}

fn objective_validation_python_heredoc(
    statement: tree_sitter::Node<'_>,
    source: &[u8],
    wrapper_depth: u8,
) -> Option<(ObjectiveValidationKind, String)> {
    let (program, python_source) =
        objective_validation_python_heredoc_source(statement, source, wrapper_depth)?;
    if !python_source_asserts_runtime_state(python_source) {
        return None;
    }
    let atom = serde_json::to_string(&(program, "stdin", python_source)).ok()?;
    Some((ObjectiveValidationKind::Diagnostic, atom))
}

fn objective_validation_python_heredoc_source<'a>(
    statement: tree_sitter::Node<'_>,
    source: &'a [u8],
    wrapper_depth: u8,
) -> Option<(String, &'a str)> {
    if statement.kind() != "redirected_statement" {
        return None;
    }
    let body = statement.child_by_field_name("body")?;
    let statement_text = statement.utf8_text(source).ok()?;
    let command_prefix = statement_text
        .lines()
        .find_map(|line| line.split_once("<<").map(|(command, _)| command.trim()))?;
    let command = match body.kind() {
        "command" => command_prefix,
        "list" => {
            let mut validation = None;
            if !collect_validation_and_chain(body, source, wrapper_depth, &mut validation)
                && !collect_fail_fast_validation(body, source, wrapper_depth, &mut validation)
            {
                return None;
            }
            command_prefix
                .rsplit_once("&&")
                .map_or(command_prefix, |(_, command)| command.trim())
        }
        _ => return None,
    };
    let parts = shlex::split(command)?;
    let program = parts.first().map(String::as_str)?;
    let program = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .to_owned();
    if !matches!(program.as_str(), "python" | "python3")
        || parts.get(1).map(String::as_str) != Some("-")
        || parts.len() != 2
    {
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
    let mut bodies = Vec::new();
    let mut nodes = vec![*redirect];
    while let Some(node) = nodes.pop() {
        if matches!(
            node.kind(),
            "command_substitution" | "expansion" | "simple_expansion"
        ) {
            return None;
        }
        if node.kind() == "heredoc_body" {
            bodies.push(node);
            continue;
        }
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
    }
    let [python_body] = bodies.as_slice() else {
        return None;
    };
    Some((program, python_body.utf8_text(source).ok()?))
}

fn objective_validation_and_chain_kind(
    list: tree_sitter::Node<'_>,
    source: &[u8],
    wrapper_depth: u8,
) -> Option<ObjectiveValidationKind> {
    let mut validation = None;
    if !collect_validation_and_chain(list, source, wrapper_depth, &mut validation) {
        return None;
    }
    validation
}

fn collect_validation_and_chain(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    wrapper_depth: u8,
    validation: &mut Option<ObjectiveValidationKind>,
) -> bool {
    match node.kind() {
        "list" => {
            for index in 0..node.child_count() {
                let Some(child) = node.child(index) else {
                    return false;
                };
                if child.is_named() {
                    if !collect_validation_and_chain(child, source, wrapper_depth, validation) {
                        return false;
                    }
                } else if child
                    .utf8_text(source)
                    .is_ok_and(|operator| operator.trim() != "&&")
                {
                    return false;
                }
            }
            true
        }
        "command" if !shell_script_has_unsafe_control_flow(node) => {
            let Ok(command) = node.utf8_text(source) else {
                return false;
            };
            let Some(parts) = shlex::split(command.trim()) else {
                return false;
            };
            if shell_command_can_change_or_skip_validation(&parts) {
                return false;
            }
            if let Some(kind) =
                objective_validation_command_kind_with_depth(command.trim(), wrapper_depth)
            {
                *validation = Some(stronger_validation_kind(*validation, kind));
            }
            true
        }
        "redirected_statement" => {
            let Some((kind, _)) = objective_validation_python_heredoc(node, source, wrapper_depth)
            else {
                return false;
            };
            *validation = Some(stronger_validation_kind(*validation, kind));
            true
        }
        "test_command" => true,
        _ => false,
    }
}

fn shell_script_has_unsafe_control_flow(root: tree_sitter::Node<'_>) -> bool {
    let mut nodes = vec![root];
    while let Some(node) = nodes.pop() {
        if matches!(
            node.kind(),
            "case"
                | "case_statement"
                | "compound_statement"
                | "c_style_for_statement"
                | "file_redirect"
                | "for"
                | "for_statement"
                | "function"
                | "function_definition"
                | "heredoc_redirect"
                | "herestring_redirect"
                | "if"
                | "if_statement"
                | "list"
                | "negated_command"
                | "pipeline"
                | "process_substitution"
                | "redirected_statement"
                | "subshell"
                | "until_statement"
                | "while"
                | "while_statement"
        ) {
            return true;
        }
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
    }
    false
}

fn shell_command_enables_errexit(parts: &[String]) -> bool {
    if parts.first().map(String::as_str) != Some("set") {
        return false;
    }
    let mut enables_errexit = false;
    let mut index = 1;
    while let Some(part) = parts.get(index) {
        if part == "+e" || (part.starts_with('+') && part[1..].contains('e')) {
            return false;
        }
        if part == "-o" && parts.get(index + 1).map(String::as_str) == Some("errexit") {
            enables_errexit = true;
            index += 2;
            continue;
        }
        if part == "-e" || (part.starts_with('-') && part[1..].contains('e')) {
            enables_errexit = true;
        }
        index += 1;
    }
    enables_errexit
}

fn shell_command_can_change_or_skip_validation(parts: &[String]) -> bool {
    let Some(program) = parts.first().map(String::as_str) else {
        return true;
    };
    matches!(
        Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(program),
        "." | "alias"
            | "bash"
            | "break"
            | "builtin"
            | "command"
            | "continue"
            | "enable"
            | "eval"
            | "exec"
            | "exit"
            | "hash"
            | "return"
            | "set"
            | "sh"
            | "source"
            | "trap"
            | "unalias"
            | "zsh"
    )
}

fn shell_assignment_is_safe(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let Some(name) = node.child_by_field_name("name") else {
        return false;
    };
    let Ok(name) = name.utf8_text(source) else {
        return false;
    };
    !matches!(
        name,
        "BASHOPTS" | "BASH_ENV" | "CDPATH" | "ENV" | "GIT_EXEC_PATH" | "IFS" | "PATH" | "SHELLOPTS"
    ) && !name.starts_with("GIT_CONFIG_")
}

const fn stronger_validation_kind(
    current: Option<ObjectiveValidationKind>,
    candidate: ObjectiveValidationKind,
) -> ObjectiveValidationKind {
    match (current, candidate) {
        (Some(ObjectiveValidationKind::Test), _) | (_, ObjectiveValidationKind::Test) => {
            ObjectiveValidationKind::Test
        }
        (Some(ObjectiveValidationKind::Diagnostic), _)
        | (_, ObjectiveValidationKind::Diagnostic) => ObjectiveValidationKind::Diagnostic,
        _ => ObjectiveValidationKind::FileState,
    }
}

fn test_command_validates_file_state(parts: &[String]) -> bool {
    parts.get(1).is_some_and(|argument| {
        matches!(
            argument.as_str(),
            "-b" | "-c"
                | "-d"
                | "-e"
                | "-f"
                | "-g"
                | "-h"
                | "-L"
                | "-p"
                | "-r"
                | "-s"
                | "-S"
                | "-u"
                | "-w"
                | "-x"
        )
    }) && parts.get(2).is_some_and(|path| !path.is_empty())
}

fn test_command_validates_runtime_comparison(parts: &[String]) -> bool {
    match parts {
        [program, operator, operand]
            if program == "test"
                && matches!(operator.as_str(), "-n" | "-z")
                && shell_operand_depends_on_runtime(operand) =>
        {
            true
        }
        [program, left, operator, right] => {
            program == "test"
                && matches!(
                    operator.as_str(),
                    "-eq" | "-ne" | "-gt" | "-ge" | "-lt" | "-le"
                )
                && left != right
                && (shell_operand_depends_on_runtime(left)
                    || shell_operand_depends_on_runtime(right))
        }
        _ => false,
    }
}

fn shell_operand_depends_on_runtime(operand: &str) -> bool {
    operand.contains('$') || operand.contains('`')
}

fn quiet_content_check(parts: &[String], piped_input: bool) -> bool {
    let Some(program) = parts.first().and_then(|program| {
        Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
    }) else {
        return false;
    };
    if !matches!(program, "grep" | "rg") {
        return false;
    }
    let quiet = parts.iter().skip(1).any(|part| {
        matches!(part.as_str(), "--quiet" | "--silent")
            || (part.starts_with('-')
                && !part.starts_with("--")
                && part.chars().skip(1).any(|option| option == 'q'))
    });
    if !quiet {
        return false;
    }
    let operands = parts
        .iter()
        .skip(1)
        .filter(|part| !part.starts_with('-'))
        .count();
    operands >= if piped_input { 1 } else { 2 }
}

fn python_module_runs_tests(parts: &[String]) -> bool {
    parts
        .windows(2)
        .any(|window| window[0] == "-m" && matches!(window[1].as_str(), "pytest" | "unittest"))
}

fn python_inline_asserts_runtime_state(parts: &[String]) -> bool {
    let Some(command_index) = parts.iter().position(|part| part == "-c") else {
        return false;
    };
    if parts
        .iter()
        .take(command_index)
        .skip(1)
        .any(|part| matches!(part.as_str(), "-O" | "-OO"))
    {
        return false;
    }
    let Some(source) = parts.get(command_index.saturating_add(1)) else {
        return false;
    };
    python_source_asserts_runtime_state(source)
}

fn python_source_asserts_runtime_state(source: &str) -> bool {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .is_err()
    {
        return false;
    }
    let Some(tree) = parser.parse(source, None) else {
        return false;
    };
    let root = tree.root_node();
    if root.has_error() {
        return false;
    }
    let source = source.as_bytes();
    let mut runtime_bindings = HashSet::new();
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        if let Some((target, value, augmented)) = python_assignment_parts(statement) {
            let depends_on_runtime =
                python_expression_depends_on_runtime_state(value, source, &runtime_bindings)
                    || (augmented
                        && python_expression_depends_on_runtime_state(
                            target,
                            source,
                            &runtime_bindings,
                        ));
            update_python_runtime_bindings(
                target,
                source,
                depends_on_runtime,
                &mut runtime_bindings,
            );
            continue;
        }
        if statement.kind() == "assert_statement"
            && statement.named_child(0).is_some_and(|assertion| {
                python_assertion_is_runtime_check(assertion, source, &runtime_bindings)
            })
        {
            return true;
        }
        if statement.kind() == "if_statement"
            && python_if_has_runtime_failure(statement, source, &runtime_bindings)
        {
            return true;
        }
    }
    false
}

fn python_assignment_parts(
    statement: tree_sitter::Node<'_>,
) -> Option<(tree_sitter::Node<'_>, tree_sitter::Node<'_>, bool)> {
    let assignment = if statement.kind() == "expression_statement" {
        statement.named_child(0)?
    } else {
        statement
    };
    if !matches!(assignment.kind(), "assignment" | "augmented_assignment") {
        return None;
    }
    Some((
        assignment.child_by_field_name("left")?,
        assignment.child_by_field_name("right")?,
        assignment.kind() == "augmented_assignment",
    ))
}

fn update_python_runtime_bindings(
    target: tree_sitter::Node<'_>,
    source: &[u8],
    depends_on_runtime: bool,
    runtime_bindings: &mut HashSet<String>,
) {
    if target.kind() == "identifier" {
        if let Ok(identifier) = target.utf8_text(source) {
            if depends_on_runtime {
                runtime_bindings.insert(identifier.to_owned());
            } else {
                runtime_bindings.remove(identifier);
            }
        }
        return;
    }
    if matches!(
        target.kind(),
        "list" | "list_pattern" | "pattern_list" | "tuple" | "tuple_pattern"
    ) {
        let mut cursor = target.walk();
        for child in target.named_children(&mut cursor) {
            update_python_runtime_bindings(child, source, depends_on_runtime, runtime_bindings);
        }
    }
}

fn python_assertion_is_runtime_check(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    runtime_bindings: &HashSet<String>,
) -> bool {
    match python_constant_truthiness(node, source) {
        Some(true) => false,
        Some(false) => true,
        None => python_expression_depends_on_runtime_state(node, source, runtime_bindings),
    }
}

fn python_if_has_runtime_failure(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    runtime_bindings: &HashSet<String>,
) -> bool {
    let Some(condition) = node.child_by_field_name("condition") else {
        return false;
    };
    if python_constant_truthiness(condition, source).is_some()
        || !python_expression_depends_on_runtime_state(condition, source, runtime_bindings)
    {
        return false;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.id() != condition.id())
        .any(|child| python_suite_has_failure(child, source, runtime_bindings))
}

fn python_expression_depends_on_runtime_state(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    runtime_bindings: &HashSet<String>,
) -> bool {
    match node.kind() {
        "true" | "false" | "none" | "integer" | "float" | "string" => false,
        "identifier" => node
            .utf8_text(source)
            .is_ok_and(|identifier| runtime_bindings.contains(identifier)),
        "attribute" => {
            node.utf8_text(source)
                .is_ok_and(python_attribute_reads_runtime_state)
                || node.child_by_field_name("object").is_some_and(|object| {
                    python_expression_depends_on_runtime_state(object, source, runtime_bindings)
                })
        }
        "subscript" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor).any(|child| {
                python_expression_depends_on_runtime_state(child, source, runtime_bindings)
            })
        }
        "call" => {
            let Some(function_node) = node.child_by_field_name("function") else {
                return false;
            };
            if python_expression_depends_on_runtime_state(function_node, source, runtime_bindings) {
                return true;
            }
            let Some(arguments) = node.child_by_field_name("arguments") else {
                return false;
            };
            let mut cursor = arguments.walk();
            let arguments_depend_on_runtime =
                arguments.named_children(&mut cursor).any(|argument| {
                    python_expression_depends_on_runtime_state(argument, source, runtime_bindings)
                });
            arguments_depend_on_runtime
                || function_node.utf8_text(source).is_ok_and(|function| {
                    python_call_reads_runtime_state(function, arguments, source)
                })
        }
        "class_definition" | "function_definition" | "lambda" => false,
        _ => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor).any(|child| {
                python_expression_depends_on_runtime_state(child, source, runtime_bindings)
            })
        }
    }
}

fn python_attribute_reads_runtime_state(attribute: &str) -> bool {
    matches!(attribute, "os.environ")
}

fn python_call_reads_runtime_state(
    function: &str,
    arguments: tree_sitter::Node<'_>,
    source: &[u8],
) -> bool {
    if matches!(
        function,
        "input"
            | "open"
            | "os.access"
            | "os.getcwd"
            | "os.getenv"
            | "os.listdir"
            | "os.lstat"
            | "os.readlink"
            | "os.scandir"
            | "os.stat"
            | "os.walk"
            | "os.path.exists"
            | "os.path.getsize"
            | "os.path.isdir"
            | "os.path.isfile"
            | "socket.create_connection"
            | "socket.getaddrinfo"
            | "urllib.request.urlopen"
    ) || matches!(
        function,
        "subprocess.call"
            | "subprocess.check_call"
            | "subprocess.check_output"
            | "subprocess.Popen"
            | "subprocess.run"
            | "requests.delete"
            | "requests.get"
            | "requests.head"
            | "requests.options"
            | "requests.patch"
            | "requests.post"
            | "requests.put"
            | "requests.request"
            | "httpx.delete"
            | "httpx.get"
            | "httpx.head"
            | "httpx.options"
            | "httpx.patch"
            | "httpx.post"
            | "httpx.put"
            | "httpx.request"
    ) {
        return true;
    }

    let leaf = function.rsplit('.').next().unwrap_or(function);
    if matches!(
        leaf,
        "exists"
            | "glob"
            | "is_dir"
            | "is_file"
            | "iterdir"
            | "lstat"
            | "read_bytes"
            | "read_text"
            | "readlink"
            | "recv"
            | "rglob"
            | "samefile"
            | "stat"
    ) {
        return true;
    }

    let resource_consumer = matches!(leaf, "connect" | "load" | "open")
        || leaf.starts_with("load_")
        || leaf.starts_with("open_")
        || leaf.starts_with("read_");
    resource_consumer && python_node_names_external_resource(arguments, source)
}

fn python_node_names_external_resource(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    if node.kind() == "string"
        && node
            .utf8_text(source)
            .is_ok_and(python_string_names_external_resource)
    {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| python_node_names_external_resource(child, source))
}

fn python_string_names_external_resource(raw: &str) -> bool {
    let value = raw
        .trim_start_matches(|character: char| {
            matches!(character.to_ascii_lowercase(), 'b' | 'f' | 'r' | 'u')
        })
        .trim_matches(['\'', '"']);
    value.contains("://")
        || value.contains(['/', '\\'])
        || value.rsplit_once('.').is_some_and(|(stem, extension)| {
            !stem.is_empty()
                && !extension.is_empty()
                && extension.len() <= 16
                && extension
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        })
}

fn python_suite_has_failure(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    runtime_bindings: &HashSet<String>,
) -> bool {
    match node.kind() {
        "raise_statement" => return python_raise_is_failure(node, source),
        "call" => return python_call_exits_nonzero(node, source),
        "if_statement" => {
            return python_if_has_runtime_failure(node, source, runtime_bindings);
        }
        "class_definition"
        | "for_statement"
        | "function_definition"
        | "lambda"
        | "try_statement"
        | "while_statement" => return false,
        _ => {}
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| python_suite_has_failure(child, source, runtime_bindings))
}

fn python_raise_is_failure(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let Some(expression) = node.named_child(0) else {
        return true;
    };
    if expression.kind() != "call" {
        return true;
    }
    let function = expression
        .child_by_field_name("function")
        .and_then(|function| function.utf8_text(source).ok())
        .unwrap_or_default();
    function != "SystemExit" || python_call_exits_nonzero(expression, source)
}

fn python_call_exits_nonzero(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let function = node
        .child_by_field_name("function")
        .and_then(|function| function.utf8_text(source).ok())
        .unwrap_or_default();
    if !matches!(function, "exit" | "quit" | "sys.exit" | "SystemExit") {
        return false;
    }
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return false;
    };
    let mut cursor = arguments.walk();
    let values = arguments.named_children(&mut cursor).collect::<Vec<_>>();
    let [value] = values.as_slice() else {
        return false;
    };
    match python_constant_truthiness(*value, source) {
        Some(true) => true,
        Some(false) | None => false,
    }
}

fn python_constant_truthiness(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<bool> {
    match node.kind() {
        "true" => Some(true),
        "false" | "none" => Some(false),
        "parenthesized_expression" => python_constant_truthiness(node.named_child(0)?, source),
        "integer" | "float" => node
            .utf8_text(source)
            .ok()?
            .replace('_', "")
            .parse::<f64>()
            .ok()
            .map(|value| value != 0.0),
        "string" => {
            let text = node.utf8_text(source).ok()?;
            Some(!matches!(text, "''" | "\"\"" | "''''''" | "\"\"\"\"\"\""))
        }
        "list" | "dictionary" | "set" | "tuple" => Some(node.named_child_count() > 0),
        "unary_operator" => node
            .utf8_text(source)
            .ok()?
            .replace('_', "")
            .parse::<f64>()
            .ok()
            .map(|value| value != 0.0),
        _ => None,
    }
}

fn comparison_has_two_operands(parts: &[String]) -> bool {
    parts
        .iter()
        .skip(1)
        .filter(|part| !part.starts_with('-'))
        .take(2)
        .count()
        == 2
}

fn git_command_validates_result(parts: &[String]) -> bool {
    if parts.get(1).is_some_and(|part| part == "merge-base")
        && parts.get(2).is_some_and(|part| part == "--is-ancestor")
    {
        return parts.len() >= 5;
    }
    if parts.get(1).is_none_or(|part| part != "diff")
        || !parts
            .iter()
            .any(|part| matches!(part.as_str(), "--exit-code" | "--quiet"))
    {
        return false;
    }
    let revisions = parts
        .iter()
        .skip(2)
        .take_while(|part| part.as_str() != "--")
        .filter(|part| !part.starts_with('-'))
        .collect::<Vec<_>>();
    let has_explicit_pathspec = parts
        .iter()
        .position(|part| part == "--")
        .is_some_and(|separator| separator + 1 < parts.len());
    revisions.len() >= 2
        || revisions.iter().any(|revision| revision.contains(".."))
        || (revisions.len() == 1 && has_explicit_pathspec)
}

fn test_report_executed_tests(report: &ToolExecutionReport) -> bool {
    let mut evidence = TestOutputEvidence::default();
    for artifact in &report.artifact_contents {
        for line in String::from_utf8_lossy(&artifact.bytes).lines() {
            evidence.explicit |= line_reports_explicit_test_execution(line);
            evidence.go_package_success |= line_reports_weak_test_execution(line);
            evidence.go_package_failure |= line_reports_go_package_failure(line);
            evidence.weak |= line_reports_status_test_execution(line);
            evidence.global_no_test |= line_reports_global_no_test_execution(line);
            evidence.package_no_test |= line_reports_package_no_test_execution(line);
        }
    }

    // Contradictory output is stronger than an auxiliary fact: a stale or
    // malformed structured field must not turn a command that reported no tests
    // (or a failed package) into a successful validation.
    if evidence.global_no_test || evidence.go_package_failure {
        return false;
    }
    if let Some(trusted_result) =
        trusted_structured_test_execution(&report.envelope.structured_facts)
    {
        return trusted_result;
    }

    // A runner can report packages without tests alongside packages that did run
    // tests (notably `go test ./...`). Package-level no-test lines therefore do
    // not cancel a positive package result. Generic status summaries remain
    // weak evidence and are rejected when the same invocation also says that no
    // tests were found.
    !evidence.go_package_failure
        && (evidence.explicit
            || (evidence.go_package_success && !evidence.global_no_test)
            || (evidence.weak && !evidence.global_no_test && !evidence.package_no_test))
}

#[derive(Debug, Default)]
struct TestOutputEvidence {
    explicit: bool,
    go_package_success: bool,
    go_package_failure: bool,
    weak: bool,
    global_no_test: bool,
    package_no_test: bool,
}

#[cfg(test)]
pub(super) fn line_reports_executed_tests(line: &str) -> bool {
    line_reports_explicit_test_execution(line)
        || line_reports_status_test_execution(line)
        || line_reports_weak_test_execution(line)
}

static POSITIVE_RUNNING_TESTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:running|executing)\s+[1-9][0-9]*\s+(?:tests?|specs?)\b")
        .expect("positive running-test regex")
});

// Python unittest 使用 `Ran N tests` 报告执行数量，而不是 `passed` 摘要；数量必须
// 大于零，避免没有发现测试的命令被误判为成功。
static POSITIVE_PYTHON_UNITTEST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^ran\s+[1-9][0-9]*\s+tests?\b").expect("python unittest summary regex")
});

static POSITIVE_TEST_RESULT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^test\s+result\b.*\b[1-9][0-9]*\s+(?:passed|failed|tests?\s+passed|tests?\s+failed)\b",
    )
    .expect("test result regex")
});

static POSITIVE_NAMED_TEST_SUMMARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:tests?|test\s+suites?)\s*[:=].*\b[1-9][0-9]*\s+(?:passed|failed|errors?|error)\b.*\b(?:total|tests?)\b",
    )
    .expect("named test summary regex")
});

static POSITIVE_TESTS_COMPLETED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[^0-9]*[1-9][0-9]*\s+(?:tests?|test\s+cases?|specs?)\s+(?:completed|executed|ran|passed|failed)\b",
    )
    .expect("completed test summary regex")
});

static POSITIVE_MAVEN_TESTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^tests?\s+run\s*:\s*[1-9][0-9]*\b").expect("maven test summary regex")
});

static POSITIVE_SWIFT_TESTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^executed\s+[1-9][0-9]*\s+tests?\b").expect("swift test summary regex")
});

static POSITIVE_TEST_STATUS_SUMMARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|[=:\s])[1-9][0-9]*\s+(?:passed|passing|failed|errors?)(?:$|\s+in\s+[0-9]|\s*[,;()\[\]])",
    )
    .expect("positive test status summary regex")
});

fn line_reports_explicit_test_execution(line: &str) -> bool {
    let line = line.trim().to_ascii_lowercase();
    POSITIVE_RUNNING_TESTS.is_match(&line)
        || POSITIVE_PYTHON_UNITTEST.is_match(&line)
        || POSITIVE_TEST_RESULT.is_match(&line)
        || POSITIVE_NAMED_TEST_SUMMARY.is_match(&line)
        || POSITIVE_TESTS_COMPLETED.is_match(&line)
        || POSITIVE_MAVEN_TESTS.is_match(&line)
        || POSITIVE_SWIFT_TESTS.is_match(&line)
        || line.starts_with("=== run ")
        || line.starts_with("--- pass:")
}

fn line_reports_status_test_execution(line: &str) -> bool {
    POSITIVE_TEST_STATUS_SUMMARY.is_match(&line.trim().to_ascii_lowercase())
}

fn line_reports_weak_test_execution(line: &str) -> bool {
    line_reports_go_package_success(line)
}

fn line_reports_go_package_success(line: &str) -> bool {
    let line = line.trim().to_ascii_lowercase();
    (line.starts_with("ok ") || line.starts_with("ok\t")) && !line.contains("[no test files]")
}

fn line_reports_go_package_failure(line: &str) -> bool {
    let line = line.trim().to_ascii_lowercase();
    line == "fail" || (line.starts_with("fail ") && !line.contains("failure:"))
}

fn line_reports_global_no_test_execution(line: &str) -> bool {
    let line = line.trim().to_ascii_lowercase();
    [
        "no tests to run",
        "no tests found",
        "no matching tests",
        "did not match any tests",
    ]
    .iter()
    .any(|marker| line.contains(marker))
}

fn line_reports_package_no_test_execution(line: &str) -> bool {
    let line = line.trim().to_ascii_lowercase();
    line.contains("[no test files]")
}

const TRUSTED_TEST_EXECUTION_FACT: &str = "golutra_test_execution";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedTestExecutionFacts {
    schema_version: u8,
    status: TrustedTestExecutionStatus,
    executed: u64,
    passed: u64,
    #[serde(default)]
    failed: u64,
    #[serde(default)]
    skipped: u64,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TrustedTestExecutionStatus {
    Passed,
}

fn trusted_structured_test_execution(facts: &Value) -> Option<bool> {
    let value = facts.get(TRUSTED_TEST_EXECUTION_FACT)?;
    let parsed = serde_json::from_value::<TrustedTestExecutionFacts>(value.clone()).ok()?;
    let valid = parsed.schema_version == 1
        && parsed.status == TrustedTestExecutionStatus::Passed
        && parsed.executed > 0
        && parsed.passed > 0
        && parsed.failed == 0
        && parsed
            .passed
            .saturating_add(parsed.failed)
            .saturating_add(parsed.skipped)
            <= parsed.executed;
    Some(valid)
}
