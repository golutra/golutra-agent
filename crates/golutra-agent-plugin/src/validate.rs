use std::collections::BTreeSet;

use serde_json::Value;

use crate::{PluginError, PluginManifest};

const MAX_TOOLS: usize = 64;
const MAX_TEXT_CHARS: usize = 4 * 1024;

pub(crate) fn validate_manifest(manifest: &PluginManifest) -> Result<(), PluginError> {
    if manifest.schema_version != 1 {
        return Err(invalid("schema_version must be 1"));
    }
    if !valid_identifier(&manifest.id) {
        return Err(invalid("id must match [a-z][a-z0-9_]{0,63}"));
    }
    validate_text("version", &manifest.version, 64)?;
    validate_optional_text("display_name", manifest.display_name.as_deref())?;
    validate_optional_text("description", manifest.description.as_deref())?;
    validate_text("server.command", &manifest.server.command, MAX_TEXT_CHARS)?;
    if manifest.server.args.len() > 128 {
        return Err(invalid("server.args cannot contain more than 128 values"));
    }
    for argument in &manifest.server.args {
        validate_text("server argument", argument, MAX_TEXT_CHARS)?;
    }
    if manifest.server.env.len() > 64 {
        return Err(invalid("server.env cannot contain more than 64 names"));
    }
    let mut env_names = BTreeSet::new();
    for name in &manifest.server.env {
        if !valid_env_name(name) {
            return Err(invalid(format!(
                "invalid environment variable name `{name}`"
            )));
        }
        if !env_names.insert(name) {
            return Err(invalid(format!("duplicate environment variable `{name}`")));
        }
    }
    if manifest.tools.is_empty() || manifest.tools.len() > MAX_TOOLS {
        return Err(invalid(format!(
            "tools must contain between 1 and {MAX_TOOLS} entries"
        )));
    }
    let mut tool_names = BTreeSet::new();
    for tool in &manifest.tools {
        if !valid_tool_name(&tool.name) {
            return Err(invalid(format!("invalid tool name `{}`", tool.name)));
        }
        if !tool_names.insert(&tool.name) {
            return Err(invalid(format!("duplicate tool name `{}`", tool.name)));
        }
        validate_optional_text("tool.description", tool.description.as_deref())?;
        validate_schema("input_schema", &tool.input_schema)?;
        if let Some(schema) = &tool.output_schema {
            validate_schema("output_schema", schema)?;
        }
    }
    Ok(())
}

fn validate_schema(name: &str, schema: &Value) -> Result<(), PluginError> {
    if !schema.is_object() {
        return Err(invalid(format!("{name} must be a JSON object")));
    }
    jsonschema::validator_for(schema)
        .map(|_| ())
        .map_err(|error| invalid(format!("{name} is not valid JSON Schema: {error}")))
}

fn valid_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('a'..='z'))
        && value.len() <= 64
        && chars.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
        })
}

fn valid_tool_name(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('a'..='z' | 'A'..='Z'))
        && value.len() <= 64
        && chars.all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
}

fn valid_env_name(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('a'..='z' | 'A'..='Z' | '_'))
        && value.len() <= 128
        && chars.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn validate_text(name: &str, value: &str, max_chars: usize) -> Result<(), PluginError> {
    if value.is_empty() || value.chars().count() > max_chars || value.contains(['\0', '\n', '\r']) {
        return Err(invalid(format!(
            "{name} must be non-empty, single-line, and at most {max_chars} characters"
        )));
    }
    Ok(())
}

fn validate_optional_text(name: &str, value: Option<&str>) -> Result<(), PluginError> {
    if let Some(value) = value {
        validate_text(name, value, MAX_TEXT_CHARS)?;
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> PluginError {
    PluginError::InvalidManifest(message.into())
}
