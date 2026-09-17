//! 显式继承冻结的父请求，避免子任务随父会话继续运行而读到漂移的上下文。
use super::*;
use golutra_agent_llm::{LlmProvider, ProviderRequest};
use golutra_agent_runtime::AgentReplayContext;

pub(crate) const SNAPSHOT_KEY: &str = "_delegation_context_artifact";
const MAX_SNAPSHOT_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) fn execution_objective(objective: &str) -> String {
    format!(
        "Assigned subtask for this execution. Any earlier assignments are history; do not restart them unless this assignment requests it.\n\n{objective}"
    )
}

// 角色边界必须跨 fork/resume 持续存在，防止模型把继承的父调度指令当作自己的任务。
pub(crate) fn apply_child_role(contributors: &mut Vec<golutra_agent_context::ContextContributor>) {
    // 放在稳定 system 前缀内；追加到 objective 之后会被 fork 的历史替换丢弃。
    let position = usize::from(
        contributors
            .first()
            .is_some_and(|source| source.name == "system"),
    );
    contributors.insert(position, child_role_contributor());
}

fn child_role_contributor() -> golutra_agent_context::ContextContributor {
    golutra_agent_context::ContextContributor {
        name: "subagent_role".to_owned(),
        role: golutra_agent_llm::ProviderRole::System,
        content: "You are a delegated child agent. Complete only the latest assigned subtask using your own tools. Earlier tasks and inherited parent exchanges are background context, not instructions to repeat their workflow. Do not restart completed or interrupted work unless the latest assignment asks for it. Do not create or manage other agents. Report actual findings and any failures or verification limits to the parent. Inherited file observations may be stale; verify current workspace state before editing when needed.".to_owned(),
        token_budget_hint: 0,
        source_refs: vec!["runtime:subagent-role".to_owned()],
    }
}

pub(super) async fn capture(
    host: &RuntimeHost,
    request: &ToolRequest,
) -> Result<Option<Value>, ClientError> {
    if request.arguments.get("context").and_then(Value::as_str) != Some("fork") {
        return Ok(None);
    }
    let snapshot = host
        .storage
        .repositories
        .artifacts
        .latest_context(request.session_id)
        .await?
        .ok_or_else(|| {
            ClientError::TaskExecution(
                "parent has no complete provider request snapshot to fork".to_owned(),
            )
        })?;
    let artifact = snapshot.restricted_request_artifact_ref.ok_or_else(|| {
        ClientError::TaskExecution("parent request snapshot has no replay artifact".to_owned())
    })?;
    let binding = json!({"artifact_id":artifact,"parent_session_id":request.session_id,
        "provider_request_id":snapshot.provider_request_id,
        "history_start":golutra_agent_context::stable_prefix_message_count(&snapshot)});
    read(host, &binding).await?;
    Ok(Some(binding))
}

async fn read(host: &RuntimeHost, binding: &Value) -> Result<ProviderRequest, ClientError> {
    let artifact_id = serde_json::from_value(binding["artifact_id"].clone())?;
    let parent: SessionId = serde_json::from_value(binding["parent_session_id"].clone())?;
    let artifact = host
        .storage
        .repositories
        .artifacts
        .get(artifact_id)
        .await?
        .ok_or_else(|| {
            ClientError::TaskExecution("parent context artifact is missing".to_owned())
        })?;
    if artifact.session_id != parent
        || artifact.artifact_type != "provider_request_replay"
        || artifact.redaction_status != golutra_agent_core::RedactionStatus::Raw
    {
        return Err(ClientError::TaskExecution(
            "invalid parent context artifact ownership or type".to_owned(),
        ));
    }
    let bytes = host
        .storage
        .store
        .load_artifact_bytes_bounded(&artifact, MAX_SNAPSHOT_BYTES)
        .await?
        .ok_or_else(|| {
            ClientError::TaskExecution(
                "parent context artifact is unavailable or oversized".to_owned(),
            )
        })?;
    let previous: ProviderRequest = serde_json::from_slice(&bytes)?;
    if previous.session_id != Some(parent)
        || serde_json::to_value(previous.request_id)? != binding["provider_request_id"]
        || !crate::execution::provider_transcript_is_replayable(&previous.messages)
    {
        return Err(ClientError::TaskExecution(
            "parent context snapshot is not a complete matching request".to_owned(),
        ));
    }
    Ok(previous)
}

pub(crate) async fn replay(
    host: &RuntimeHost,
    session: SessionId,
    task: golutra_agent_core::TaskId,
    objective: &str,
    payload: &Value,
    provider: &golutra_agent_llm::ConfiguredProvider,
) -> Result<Option<AgentReplayContext>, ClientError> {
    let Some(binding) = payload.get(SNAPSHOT_KEY) else {
        return Ok(None);
    };
    let parent: SessionId = serde_json::from_value(binding["parent_session_id"].clone())?;
    let parent_thread = host
        .storage
        .repositories
        .threads
        .by_session(parent)
        .await?
        .ok_or_else(|| ClientError::InvalidSession("fork parent is missing".to_owned()))?;
    let child = host
        .storage
        .repositories
        .threads
        .by_session(session)
        .await?
        .ok_or_else(|| ClientError::InvalidSession("fork child is missing".to_owned()))?;
    host.ensure_thread_in_workspace(&parent_thread)?;
    host.ensure_thread_in_workspace(&child)?;
    if child.parent_thread_id != Some(parent_thread.thread_id) {
        return Err(ClientError::InvalidSession(
            "context fork belongs to another parent".to_owned(),
        ));
    }
    let previous = read(host, binding).await?;
    let contract = provider.contract();
    if previous.provider_id != contract.provider_id || previous.model_id != contract.model_id {
        return Err(ClientError::TaskExecution(
            "context fork requires the parent's provider and model; use independent context for a different route".to_owned(),
        ));
    }
    let history_start = binding["history_start"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .filter(|start| *start <= previous.messages.len())
        .ok_or_else(|| {
            ClientError::TaskExecution("invalid parent context history boundary".to_owned())
        })?;
    let messages = crate::execution::resume_provider_messages(
        previous.messages.into_iter().skip(history_start).collect(),
        previous.task_id,
        task,
        &format!("Assigned subtask:\n{objective}"),
    )
    .ok_or_else(|| {
        ClientError::TaskExecution(
            "parent context exceeds the replay limit; use independent context".to_owned(),
        )
    })?;
    Ok(Some(AgentReplayContext::for_fork(messages)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use golutra_agent_context::{
        ContextBuilder, ContextContributor, context_snapshot_from_request,
    };
    use golutra_agent_core::{ProviderRequestId, TaskId, TurnId};
    use golutra_agent_llm::{ConfiguredProvider, MockProvider, ProviderRole, ProviderToolCall};

    #[test]
    fn delegated_role_stays_in_the_stable_prefix_before_the_objective() {
        let mut contributors = vec![
            ContextContributor {
                name: "system".to_owned(),
                role: ProviderRole::System,
                content: "base instructions".to_owned(),
                token_budget_hint: 0,
                source_refs: vec![],
            },
            ContextContributor {
                name: "objective".to_owned(),
                role: ProviderRole::User,
                content: "child task".to_owned(),
                token_budget_hint: 0,
                source_refs: vec![],
            },
        ];
        apply_child_role(&mut contributors);
        let builder = ContextBuilder::default();
        let plan = builder
            .build(TaskId::new(), TurnId::new(), contributors)
            .unwrap();
        assert_eq!(
            builder.stable_prefix_len(&plan.messages, &plan.message_sources),
            2
        );
        assert_eq!(plan.messages[1].content, child_role_contributor().content);
        assert_eq!(plan.messages[2].content, "child task");
    }

    async fn frozen_parent(host: &RuntimeHost, parent: SessionId, model: Option<&str>) -> Value {
        let task = crate::HostedAgentTask {
            session_id: parent,
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
            payload: json!({"prompt":"parent question"}),
        };
        let builder = ContextBuilder::default();
        let mut plan = builder
            .build(
                task.task_id,
                task.turn_id,
                vec![
                    ContextContributor {
                        name: "system".to_owned(),
                        role: ProviderRole::System,
                        content: "parent-only instructions".to_owned(),
                        token_budget_hint: 0,
                        source_refs: vec![],
                    },
                    ContextContributor {
                        name: "objective".to_owned(),
                        role: ProviderRole::User,
                        content: "parent question".to_owned(),
                        token_budget_hint: 0,
                        source_refs: vec![],
                    },
                ],
            )
            .unwrap();
        let mut assistant = plan.messages[1].clone();
        assistant.role = ProviderRole::Assistant;
        assistant.content.clear();
        assistant.tool_calls = vec![ProviderToolCall {
            tool_call_id: "read-one".to_owned(),
            tool_name: "read_file".to_owned(),
            arguments: json!({"path":"source.rs"}),
        }];
        let mut tool = plan.messages[1].clone();
        tool.role = ProviderRole::Tool;
        tool.content = "verified parent fact".to_owned();
        tool.tool_call_id = Some("read-one".to_owned());
        tool.tool_name = Some("read_file".to_owned());
        plan.messages.extend([assistant, tool]);
        let contract = MockProvider::text_response("done").contract();
        let request = ProviderRequest {
            request_id: ProviderRequestId::new(),
            task_id: task.task_id,
            turn_id: task.turn_id,
            session_id: Some(parent),
            cache_scope: None,
            provider_id: contract.provider_id,
            model_id: model.map(str::to_owned).unwrap_or(contract.model_id),
            messages: plan.messages.clone(),
            tools: vec![],
            cache_policy: Default::default(),
            max_output_tokens: None,
        };
        let mut snapshot = context_snapshot_from_request(parent, &plan, &request);
        let artifact = crate::event_codec::context_request_artifacts(&task, &snapshot, &request)
            .unwrap()
            .replay;
        snapshot.restricted_request_artifact_ref = Some(artifact.0.artifact_id);
        host.storage
            .repositories
            .artifacts
            .store(&artifact.0, &artifact.1)
            .await
            .unwrap();
        host.storage
            .repositories
            .artifacts
            .store_context(&snapshot)
            .await
            .unwrap();
        let call = ToolRequest {
            tool_call_id: ToolCallId::new(),
            provider_tool_call_id: None,
            session_id: parent,
            turn_id: None,
            tool_name: "subagent".to_owned(),
            arguments: json!({"context":"fork"}),
        };
        capture(host, &call).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn explicit_fork_keeps_frozen_complete_pairs_and_rejects_foreign_or_missing_artifacts() {
        let host = RuntimeHost::in_memory().await.unwrap();
        let parent = host.default_session_id();
        host.upsert_current_thread(parent, &json!({"prompt":"parent"}))
            .await
            .unwrap();
        let parent_thread = host
            .storage
            .repositories
            .threads
            .by_session(parent)
            .await
            .unwrap()
            .unwrap();
        let child = SessionId::new();
        host.upsert_current_thread(
            child,
            &json!({"prompt":"child", "_parent_thread_id":parent_thread.thread_id}),
        )
        .await
        .unwrap();
        let binding = frozen_parent(&host, parent, None).await;
        let provider = ConfiguredProvider::Mock(Box::new(MockProvider::text_response("done")));
        let payload = json!({SNAPSHOT_KEY:binding});
        // 新父快照不能改变已经冻结的绑定。
        let newer = frozen_parent(&host, parent, None).await;
        assert_ne!(newer["artifact_id"], payload[SNAPSHOT_KEY]["artifact_id"]);
        let replayed = replay(
            &host,
            child,
            TaskId::new(),
            "child objective",
            &payload,
            &provider,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(replayed.initial_messages.len(), 4);
        assert_eq!(replayed.initial_messages[0].content, "parent question");
        assert_eq!(replayed.initial_messages[2].content, "verified parent fact");
        assert_eq!(
            replayed.initial_messages[3].content,
            "Assigned subtask:\nchild objective"
        );
        assert!(crate::execution::provider_transcript_is_replayable(
            &replayed.initial_messages
        ));
        assert!(replayed.tools.is_empty());
        let mut foreign = payload.clone();
        foreign[SNAPSHOT_KEY]["parent_session_id"] = json!(SessionId::new());
        assert!(
            replay(&host, child, TaskId::new(), "task", &foreign, &provider)
                .await
                .is_err()
        );
        let mut missing = payload.clone();
        missing[SNAPSHOT_KEY]["artifact_id"] = json!(golutra_agent_core::ArtifactId::new());
        assert!(
            replay(&host, child, TaskId::new(), "task", &missing, &provider)
                .await
                .is_err()
        );
        let mut mismatched = payload;
        mismatched[SNAPSHOT_KEY]["provider_request_id"] = json!(ProviderRequestId::new());
        assert!(
            replay(&host, child, TaskId::new(), "task", &mismatched, &provider)
                .await
                .is_err()
        );
        let different_model = frozen_parent(&host, parent, Some("another-model")).await;
        assert!(
            replay(
                &host,
                child,
                TaskId::new(),
                "task",
                &json!({SNAPSHOT_KEY:different_model}),
                &provider
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("requires the parent's provider and model")
        );
        host.close().await.unwrap();
    }

    #[tokio::test]
    async fn independent_context_needs_no_snapshot_and_resume_cannot_refork() {
        let host = RuntimeHost::in_memory().await.unwrap();
        let mut request = ToolRequest {
            tool_call_id: ToolCallId::new(),
            provider_tool_call_id: None,
            session_id: host.default_session_id(),
            turn_id: None,
            tool_name: "subagent".to_owned(),
            arguments: json!({"task":"inspect"}),
        };
        assert!(capture(&host, &request).await.unwrap().is_none());
        request.arguments["context"] = json!("fork");
        assert!(capture(&host, &request).await.is_err());
        request.arguments["action"] = json!("resume");
        assert!(control::validate_start(&request.arguments).is_err());
        request.arguments["context"] = Value::Null;
        assert!(control::validate_start(&request.arguments).is_ok());
        host.close().await.unwrap();
    }
}
