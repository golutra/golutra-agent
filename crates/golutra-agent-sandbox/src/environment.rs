use std::{collections::BTreeMap, ffi::OsString};

use serde::Deserialize;

use crate::SandboxError;

const POLICY_ENV: &str = "GOLUTRA_AGENT_SHELL_ENVIRONMENT_POLICY";
const INTERNAL_ENVIRONMENT_VARIABLES: &[&str] = &[
    "GOLUTRA_AGENT_TRANSPORT_TOKEN",
    "GOLUTRA_AGENT_PROVIDER_API_KEY",
    "GOLUTRA_AGENT_PROVIDER_CUSTOM_HEADERS",
];

pub fn is_internal_environment_variable(name: &str) -> bool {
    INTERNAL_ENVIRONMENT_VARIABLES
        .iter()
        .any(|internal| name.eq_ignore_ascii_case(internal))
        || name
            .to_ascii_uppercase()
            .starts_with("GOLUTRA_AGENT_CUSTOM_PROVIDER_API_KEY_")
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Inherit {
    #[default]
    All,
    Core,
    None,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct EnvironmentPolicy {
    inherit: Inherit,
    exclude: Vec<String>,
    include_only: Vec<String>,
}

pub(crate) fn inherited_environment(
    variables: impl IntoIterator<Item = (OsString, OsString)>,
) -> Result<BTreeMap<OsString, OsString>, SandboxError> {
    let variables = variables.into_iter().collect::<BTreeMap<_, _>>();
    let policy = match variables.get(std::ffi::OsStr::new(POLICY_ENV)) {
        None => EnvironmentPolicy::default(),
        Some(value) => {
            let value = value
                .to_str()
                .filter(|value| value.len() <= 65_536)
                .ok_or(SandboxError::InvalidEnvironmentPolicy)?;
            serde_json::from_str::<EnvironmentPolicy>(value)
                .map_err(|_| SandboxError::InvalidEnvironmentPolicy)?
        }
    };
    if policy
        .exclude
        .iter()
        .chain(&policy.include_only)
        .any(|name| name.is_empty() || name.contains(['=', '\0', '*', '?']))
    {
        return Err(SandboxError::InvalidEnvironmentPolicy);
    }
    Ok(variables
        .into_iter()
        .filter(|(key, _)| {
            let key = key.to_string_lossy();
            let inherited = match policy.inherit {
                Inherit::All => true,
                Inherit::None => false,
                Inherit::Core => matches!(
                    key.to_ascii_uppercase().as_str(),
                    "PATH"
                        | "HOME"
                        | "USER"
                        | "LOGNAME"
                        | "SHELL"
                        | "LANG"
                        | "LC_ALL"
                        | "LC_CTYPE"
                        | "TMPDIR"
                        | "TMP"
                        | "TEMP"
                        | "SYSTEMROOT"
                        | "COMSPEC"
                        | "PATHEXT"
                ),
            };
            inherited
                && !is_internal_environment_variable(&key)
                && !policy
                    .exclude
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&key))
                && (policy.include_only.is_empty()
                    || policy
                        .include_only
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(&key)))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(policy: Option<&str>) -> Result<BTreeMap<OsString, OsString>, SandboxError> {
        let mut variables = [
            ("PATH", "/usr/bin:/bin"),
            ("GITHUB_TOKEN", "fixture-github"),
            ("OPENAI_API_KEY", "fixture-openai"),
            ("DATABASE_PASSWORD", "fixture-password"),
            ("HTTPS_PROXY", "http://localhost:8080"),
            ("GOLUTRA_COMMAND_SCOPE_TOKEN", "fixture-scope"),
            ("GOLUTRA_RUNTIME_PROFILE", "dev"),
            ("GOLUTRA_COMMAND_IPC_ADDR", "fixture-ipc"),
            ("GOLUTRA_AGENT_TRANSPORT_TOKEN", "fixture-internal"),
            ("golutra_agent_provider_api_key", "fixture-internal"),
            ("GOLUTRA_AGENT_PROVIDER_CUSTOM_HEADERS", "fixture-internal"),
            (
                "GOLUTRA_AGENT_CUSTOM_PROVIDER_API_KEY_EXAMPLE",
                "fixture-internal",
            ),
        ]
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .to_vec();
        if let Some(policy) = policy {
            variables.push((POLICY_ENV.into(), policy.into()));
        }
        inherited_environment(variables)
    }

    #[test]
    fn defaults_inherit_host_and_third_party_environment_only_scrubbing_internal_credentials() {
        let inherited = environment(None).unwrap();
        assert_eq!(inherited.len(), 8);
        assert_eq!(
            inherited[&OsString::from("GOLUTRA_COMMAND_SCOPE_TOKEN")],
            "fixture-scope"
        );
        assert_eq!(
            inherited[&OsString::from("OPENAI_API_KEY")],
            "fixture-openai"
        );
        assert!(inherited.values().all(|value| value != "fixture-internal"));
    }

    #[test]
    fn user_filters_cannot_restore_internal_credentials() {
        let inherited = environment(Some(r#"{"include_only":["path","GITHUB_TOKEN","GOLUTRA_AGENT_TRANSPORT_TOKEN"],"exclude":["github_token"]}"#)).unwrap();
        assert_eq!(
            inherited,
            BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())])
        );
        assert_eq!(
            environment(Some(r#"{"inherit":"core"}"#)).unwrap(),
            inherited
        );
        assert!(
            environment(Some(r#"{"inherit":"none"}"#))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn malformed_policy_fails_without_echoing_values() {
        for policy in [
            r#"{"inherit":"fixture-secret"}"#,
            r#"{"set":{"GOLUTRA_AGENT_TRANSPORT_TOKEN":"fixture-secret"}}"#,
            r#"{"exclude":["*TOKEN*"]}"#,
            "not json",
        ] {
            let error = environment(Some(policy)).unwrap_err().to_string();
            assert!(error.contains(POLICY_ENV));
            assert!(!error.contains("fixture-secret"));
        }
    }
}
