//! Turn execution mode and model-visible tool profile normalization.
//!
//! The wire-facing clients opt into the open path explicitly. A compact coding
//! tool surface is the default; full is reserved for callers that need the
//! low-frequency process, search, or extension tools.

use golutra_core::{TaskContract, VerificationRequirement};
use golutra_protocol::{AgentExecutionMode, AgentToolProfile};
use serde_json::Value;

pub(crate) const EXECUTION_MODE_KEY: &str = "execution_mode";
pub(crate) const TOOL_PROFILE_KEY: &str = "tool_profile";
pub(crate) const NORMALIZED_EXECUTION_MODE_KEY: &str = "_execution_mode";
pub(crate) const VERIFY_ON_CHANGE_KEY: &str = "verify_on_change";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NormalizedExecutionMode {
    Legacy,
    Open,
    Strict,
}

impl NormalizedExecutionMode {
    pub(crate) const fn wire_name(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Open => "open",
            Self::Strict => "strict",
        }
    }

    pub(crate) const fn is_strict(self) -> bool {
        // Payloads written before execution_mode existed used the legacy
        // prompt adapter. Preserve that behavior when replaying or admitting
        // those payloads; new clients opt out explicitly with `open`.
        matches!(self, Self::Legacy | Self::Strict)
    }

    pub(crate) const fn explicit(self) -> Option<AgentExecutionMode> {
        match self {
            Self::Legacy => None,
            Self::Open => Some(AgentExecutionMode::Open),
            Self::Strict => Some(AgentExecutionMode::Strict),
        }
    }

    pub(crate) const fn from_explicit(mode: Option<AgentExecutionMode>) -> Self {
        match mode {
            None => Self::Legacy,
            Some(AgentExecutionMode::Open) => Self::Open,
            Some(AgentExecutionMode::Strict) => Self::Strict,
        }
    }
}

pub(crate) fn execution_mode_from_payload(
    payload: &Value,
) -> Result<NormalizedExecutionMode, &'static str> {
    let Some(value) = payload.get(EXECUTION_MODE_KEY) else {
        return Ok(NormalizedExecutionMode::Legacy);
    };
    match value {
        Value::String(value) if value.eq_ignore_ascii_case("open") => {
            Ok(NormalizedExecutionMode::Open)
        }
        Value::String(value) if value.eq_ignore_ascii_case("strict") => {
            Ok(NormalizedExecutionMode::Strict)
        }
        _ => Err("execution_mode must be `open` or `strict`"),
    }
}

pub(crate) fn tool_profile_from_payload(payload: &Value) -> Result<AgentToolProfile, &'static str> {
    let default_profile = match execution_mode_from_payload(payload)? {
        // Omitted execution_mode is the pre-profile wire shape. Preserve its
        // complete model-facing surface for persisted and direct Rust callers.
        NormalizedExecutionMode::Legacy => AgentToolProfile::Full,
        NormalizedExecutionMode::Open | NormalizedExecutionMode::Strict => AgentToolProfile::Coding,
    };
    let Some(value) = payload.get(TOOL_PROFILE_KEY) else {
        return Ok(default_profile);
    };
    match value {
        Value::String(value) if value.eq_ignore_ascii_case("coding") => {
            Ok(AgentToolProfile::Coding)
        }
        Value::String(value) if value.eq_ignore_ascii_case("full") => Ok(AgentToolProfile::Full),
        Value::Null => Ok(default_profile),
        _ => Err("tool_profile must be `coding` or `full`"),
    }
}

pub(crate) fn explicit_task_contract(payload: &Value) -> bool {
    payload
        .get("task_contract")
        .is_some_and(|value| !value.is_null())
}

/// 解析工作区变更后的自动验证开关；未知值必须在命令边界拒绝，避免静默降级。
pub(crate) fn verify_on_change_auto(payload: &Value) -> Result<bool, &'static str> {
    match payload.get(VERIFY_ON_CHANGE_KEY) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::String(value)) if value.eq_ignore_ascii_case("auto") => Ok(true),
        Some(Value::String(value))
            if value.eq_ignore_ascii_case("off") || value.eq_ignore_ascii_case("never") =>
        {
            Ok(false)
        }
        Some(_) => Err("verify_on_change must be `auto`, `off`, or `never`"),
    }
}

/// External evaluation metadata is an explicit request for objective
/// completion, even when a client selected the open model-facing loop.
pub(crate) fn strict_execution_requested(payload: &Value, mode: NormalizedExecutionMode) -> bool {
    mode.is_strict()
        || explicit_task_contract(payload)
        || payload
            .get("external_verifiers")
            .and_then(Value::as_array)
            .is_some_and(|verifiers| !verifiers.is_empty())
}

/// Apply the explicit strict-mode guarantees at the client boundary. Legacy
/// payloads continue to use the prompt adapter; only a wire-level `strict`
/// request receives this contract without relying on prompt wording.
pub(crate) fn apply_execution_mode_contract(
    mode: NormalizedExecutionMode,
    explicit_contract: bool,
    contract: &mut TaskContract,
) {
    if !explicit_contract && matches!(mode, NormalizedExecutionMode::Strict) {
        contract.require_objective_validation = true;
        if matches!(contract.verification, VerificationRequirement::BestEffort) {
            contract.verification = VerificationRequirement::Required;
        }
        contract.max_correction_rounds = contract.max_correction_rounds.max(1);
    }
}

pub(crate) fn should_apply_legacy_adapter(payload: &Value, mode: NormalizedExecutionMode) -> bool {
    !explicit_task_contract(payload) && matches!(mode, NormalizedExecutionMode::Legacy)
}

pub(crate) fn normalized_mode_value(mode: NormalizedExecutionMode) -> Value {
    Value::String(mode.wire_name().to_owned())
}

pub(crate) fn write_normalized_execution_mode(payload: &mut Value, mode: NormalizedExecutionMode) {
    if matches!(mode, NormalizedExecutionMode::Legacy) {
        if let Some(object) = payload.as_object_mut() {
            object.remove(EXECUTION_MODE_KEY);
        }
    } else {
        payload[EXECUTION_MODE_KEY] = normalized_mode_value(mode);
    }
    payload[NORMALIZED_EXECUTION_MODE_KEY] = normalized_mode_value(mode);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_clients_default_to_open_with_coding_tools() {
        let open = json!({"execution_mode": "open"});
        assert_eq!(
            execution_mode_from_payload(&open).expect("open mode"),
            NormalizedExecutionMode::Open
        );
        assert_eq!(
            tool_profile_from_payload(&open).expect("coding default"),
            AgentToolProfile::Coding
        );

        let legacy = json!({});
        assert_eq!(
            execution_mode_from_payload(&legacy).expect("legacy mode"),
            NormalizedExecutionMode::Legacy
        );
        assert_eq!(
            tool_profile_from_payload(&legacy).expect("legacy profile default"),
            AgentToolProfile::Full
        );
        assert!(strict_execution_requested(
            &legacy,
            NormalizedExecutionMode::Legacy
        ));
    }

    #[test]
    fn strict_signals_require_an_explicit_mode_contract_or_verifier() {
        assert!(!strict_execution_requested(
            &json!({"execution_mode": "open", "benchmark_id": "tb-1", "ci": true}),
            NormalizedExecutionMode::Open
        ));
        assert!(strict_execution_requested(
            &json!({"execution_mode": "strict"}),
            NormalizedExecutionMode::Strict
        ));
        assert!(strict_execution_requested(
            &json!({"execution_mode": "open", "external_verifiers": [{"program": "make"}]}),
            NormalizedExecutionMode::Open
        ));
        assert!(strict_execution_requested(
            &json!({"execution_mode": "open", "task_contract": {}}),
            NormalizedExecutionMode::Open
        ));
        assert!(!should_apply_legacy_adapter(
            &json!({
                "execution_mode": "open",
                "task_contract": {"completion_criteria": ["done"]}
            }),
            NormalizedExecutionMode::Open
        ));
        assert!(!should_apply_legacy_adapter(
            &json!({"execution_mode": "strict"}),
            NormalizedExecutionMode::Strict
        ));
        assert!(!should_apply_legacy_adapter(
            &json!({
                "execution_mode": "open",
                "external_verifiers": [{"program": "make"}]
            }),
            NormalizedExecutionMode::Open
        ));
        assert!(should_apply_legacy_adapter(
            &json!({"prompt": "write src/lib.rs"}),
            NormalizedExecutionMode::Legacy
        ));
    }

    #[test]
    fn verify_on_change_accepts_only_explicit_modes() {
        assert!(verify_on_change_auto(&json!({"verify_on_change": "auto"})).unwrap());
        assert!(!verify_on_change_auto(&json!({"verify_on_change": "off"})).unwrap());
        assert!(!verify_on_change_auto(&json!({})).unwrap());
        assert!(verify_on_change_auto(&json!({"verify_on_change": true})).is_err());
    }

    #[test]
    fn invalid_mode_and_profile_are_rejected() {
        assert!(execution_mode_from_payload(&json!({"execution_mode": "adaptive"})).is_err());
        assert!(tool_profile_from_payload(&json!({"tool_profile": "everything"})).is_err());
        assert_eq!(
            tool_profile_from_payload(&json!({"tool_profile": null})).expect("null means inherit"),
            AgentToolProfile::Full
        );
        assert_eq!(
            tool_profile_from_payload(&json!({
                "execution_mode": "open",
                "tool_profile": null
            }))
            .expect("open null means coding"),
            AgentToolProfile::Coding
        );
    }

    #[test]
    fn strict_mode_contract_is_independent_of_prompt_wording() {
        let mut contract = TaskContract::conversational(vec!["summarize findings".to_owned()]);
        apply_execution_mode_contract(NormalizedExecutionMode::Strict, false, &mut contract);

        assert!(contract.require_objective_validation);
        assert_eq!(contract.verification, VerificationRequirement::Required);
        assert_eq!(contract.max_correction_rounds, 1);
    }

    #[test]
    fn explicit_contract_remains_authoritative_in_strict_mode() {
        let mut contract = TaskContract::conversational(vec!["answer plainly".to_owned()]);
        apply_execution_mode_contract(NormalizedExecutionMode::Strict, true, &mut contract);

        assert!(!contract.require_objective_validation);
        assert_eq!(contract.verification, VerificationRequirement::BestEffort);
        assert_eq!(contract.max_correction_rounds, 0);
    }
}
