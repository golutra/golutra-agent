//! Bounded task handoffs; filesystem authority remains with the existing policy.
use super::*;
use golutra_agent_core::TaskContract;
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Assignment {
    owned_paths: Vec<String>,
    new_paths: Vec<String>,
    interfaces: String,
    checks: Vec<String>,
    acceptance: Vec<String>,
}

pub(super) fn narrowed_profile(
    inherited: AgentToolProfile,
    requested: Option<&Value>,
) -> Result<AgentToolProfile, ClientError> {
    let Some(requested) = requested else {
        return Ok(inherited);
    };
    let requested: AgentToolProfile = serde_json::from_value(requested.clone()).map_err(|_| {
        ClientError::TaskExecution("tool_profile must be none, coding or full".into())
    })?;
    let rank = |profile| match profile {
        AgentToolProfile::None => 0,
        AgentToolProfile::Coding => 1,
        AgentToolProfile::Full => 2,
    };
    if rank(requested) > rank(inherited) {
        return Err(ClientError::TaskExecution(
            "child tool_profile cannot widen inherited tools".into(),
        ));
    }
    Ok(requested)
}

fn parse(value: &Value) -> Result<Assignment, ClientError> {
    if serde_json::to_vec(value)?.len() > 8192 {
        return Err(ClientError::TaskExecution(
            "assignment exceeds 8192 bytes".into(),
        ));
    }
    let assignment: Assignment = serde_json::from_value(value.clone())?;
    let paths = TaskContract {
        required_paths: assignment
            .owned_paths
            .iter()
            .chain(&assignment.new_paths)
            .cloned()
            .collect(),
        ..Default::default()
    };
    paths.validate().map_err(ClientError::TaskExecution)?;
    if assignment.checks.len() > 16
        || assignment.acceptance.len() > 16
        || assignment
            .checks
            .iter()
            .chain(&assignment.acceptance)
            .any(|s| s.trim().is_empty() || s.len() > 1024)
        || assignment.interfaces.len() > 2048
        || assignment
            .new_paths
            .iter()
            .any(|path| !assignment.owned_paths.contains(path))
    {
        return Err(ClientError::TaskExecution("invalid assignment: new_paths must belong to owned_paths; at most 16 bounded checks/criteria".into()));
    }
    Ok(assignment)
}

pub(super) fn validate(arguments: &Value) -> Result<(), ClientError> {
    if let Some(value) = arguments.get("assignment") {
        parse(value)?;
    }
    if arguments.get("tool_profile").is_some() {
        narrowed_profile(AgentToolProfile::Full, arguments.get("tool_profile"))?;
    }
    Ok(())
}

pub(super) fn apply(payload: &mut Value, arguments: &Value) -> Result<(), ClientError> {
    let Some(value) = arguments
        .get("assignment")
        .or_else(|| payload.get("_delegation_assignment"))
        .cloned()
    else {
        return Ok(());
    };
    let assignment = parse(&value)?;
    let mut contract: TaskContract = payload
        .get("task_contract")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    contract.required_paths = assignment.owned_paths;
    // Acceptance is a semantic review checklist. Feeding arbitrary prose into
    // legacy evidence-name matching can create unsatisfiable correction loops.
    // Keep caller-supplied runtime criteria intact; the parent reviews this list.
    contract.validate().map_err(ClientError::TaskExecution)?;
    payload["task_contract"] = serde_json::to_value(contract)?;
    payload["_delegation_assignment"] = value.clone();
    let prompt = payload["prompt"].as_str().unwrap_or_default();
    payload["prompt"] = json!(format!(
        "{prompt}\n\nAssignment contract (owned_paths are deliverables and responsibility, not a filesystem sandbox):\n{value}\nFiles in new_paths are expected to be absent before implementation. Use the declared interfaces; run local checks for this assignment. Report changed files, check outcomes and unresolved integration issues to the parent. Address acceptance criteria in your findings with evidence and remaining limits; they remain pending parent review, not automatically verified by a successful execution. The parent owns integration and user-interface acceptance unless explicitly assigned here."
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profiles_only_narrow_and_malformed_assignments_fail() {
        assert_eq!(
            narrowed_profile(AgentToolProfile::Coding, Some(&json!("none"))).unwrap(),
            AgentToolProfile::None
        );
        assert!(narrowed_profile(AgentToolProfile::None, Some(&json!("coding"))).is_err());
        assert!(narrowed_profile(AgentToolProfile::Coding, Some(&json!("full"))).is_err());
        assert!(validate(&json!({"assignment":{"owned_paths":["../escape"]}})).is_err());
        assert!(validate(&json!({"assignment":{"new_paths":["not-owned"]}})).is_err());
        let mut payload = json!({"prompt":"Implement input","task_contract":{"completion_criteria":["tests pass"]}});
        apply(&mut payload, &json!({"assignment":{"owned_paths":["input.mjs"],"new_paths":["input.mjs"],"checks":["node --check input.mjs"],"acceptance":["release held keys on blur"]}})).unwrap();
        assert_eq!(
            payload["task_contract"]["required_paths"],
            json!(["input.mjs"])
        );
        assert_eq!(
            payload["task_contract"]["completion_criteria"],
            json!(["tests pass"])
        );
        assert!(
            payload["prompt"]
                .as_str()
                .unwrap()
                .contains("expected to be absent")
        );
    }

    #[tokio::test]
    async fn resume_and_explore_preserve_none_profile_and_assignment() {
        let host = RuntimeHost::in_memory().await.unwrap();
        let session = host.default_session_id();
        host.upsert_current_thread(session, &json!({"prompt":"assignment fixture"}))
            .await
            .unwrap();
        let thread = host
            .storage
            .repositories
            .threads
            .by_session(session)
            .await
            .unwrap()
            .unwrap();
        let assignment = json!({"interfaces":"Summarize the supplied API without tools","acceptance":["state any unknowns"]});
        host.record_event(crate::event_codec::host_event(
            1,
            session,
            None,
            RuntimeEventType::TaskCreated,
            golutra_agent_protocol::RuntimeEventSource::Runtime,
            json!({"payload":{"tool_profile":"none","_delegation_assignment":assignment}}),
        ))
        .await
        .unwrap();
        let mut request = ToolRequest {
            tool_call_id: ToolCallId::new(),
            provider_tool_call_id: None,
            session_id: session,
            turn_id: None,
            tool_name: "subagent".into(),
            arguments: json!({"action":"resume","agent_type":"explore"}),
        };
        let mut payload = json!({"tool_profile":"full","prompt":"continue"});
        control::apply_agent_type(&mut payload, &request, Some(&thread), &host)
            .await
            .unwrap();
        apply(&mut payload, &request.arguments).unwrap();
        assert_eq!(payload["tool_profile"], "none");
        assert_eq!(payload["_delegation_assignment"], assignment);
        assert_eq!(payload["task_contract"]["workspace_change"], "forbidden");
        request.arguments["tool_profile"] = json!("coding");
        assert!(
            control::apply_agent_type(&mut payload, &request, Some(&thread), &host)
                .await
                .is_err()
        );
        host.close().await.unwrap();
    }
}
