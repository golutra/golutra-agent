//! 以可控 provider 验证摘要截断恢复、费用审计、协议共用规则及取消边界。

use super::*;
use golutra_agent_context::{
    CompactionSourceRange, compaction_source_checksum, compaction_summary_envelope,
};

const CHECKPOINT: &str = "## Goal and Constraints\nPreserve API.\n## Remaining Work\nVerify migrations.\n## Current State and Evidence\nTests failed: fixture missing.\n## Key Decisions\nKeep existing database.";

struct SummaryScript {
    replies: Vec<(ProviderFinishReason, String)>,
    requests: Mutex<Vec<ProviderRequest>>,
    protocol: &'static str,
    cancel: Option<tokio_util::sync::CancellationToken>,
}

#[async_trait]
impl LlmProvider for SummaryScript {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let index = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request.clone());
            requests.len() - 1
        };
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
            return Err(ProviderError::Cancelled);
        }
        let (finish, text) = &self.replies[index];
        let mut response = MockProvider::text_response(text).complete(request).await?;
        response.finish_reason = *finish;
        Ok(response)
    }

    fn contract(&self) -> ProviderContract {
        let mut contract = MockProvider::text_response("").contract();
        contract.native_protocol = self.protocol.into();
        contract
    }
}

fn summary_task() -> AgentTaskRequest {
    AgentTaskRequest {
        session_id: SessionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        objective: "Preserve API and verify migrations".into(),
        completion_criteria: Vec::new(),
        output_schema: None,
        touched_code: false,
        contributors: Vec::new(),
        tools: Vec::new(),
    }
}

fn history_record(turn_id: TurnId) -> ContextCompactionRecord {
    let messages = (0..60)
        .map(|index| ProviderMessage {
            role: ProviderRole::User,
            content: format!("observation {index}: {}", "detail ".repeat(200)),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            metadata: Default::default(),
        })
        .collect::<Vec<_>>();
    ContextWindowManager::new(16_384)
        .compact_if_needed(turn_id, 0, &messages, &[], 0)
        .unwrap()
        .unwrap()
}

fn summary_builder() -> ContextBuilder {
    ContextBuilder::new(ContextBudgetPolicy {
        context_window: 200_000,
        max_output: 16_384,
        budget_limit: 183_616,
        action_if_exceeded: BudgetOverflowAction::Compact,
    })
}

#[tokio::test]
async fn length_and_storage_failures_retry_from_original_history_for_every_protocol() {
    for protocol in [
        "openai-compatible",
        "openai-responses",
        "anthropic",
        "gemini",
        "vertex-ai",
        "rust-genai",
    ] {
        for finish in [ProviderFinishReason::Length, ProviderFinishReason::Stop] {
            let workspace = tempdir().unwrap();
            let task = summary_task();
            let mut record = history_record(task.turn_id);
            let provider = SummaryScript {
                replies: vec![
                    (finish, "incomplete or oversized checkpoint ".repeat(800)),
                    (ProviderFinishReason::Stop, CHECKPOINT.into()),
                ],
                requests: Mutex::new(Vec::new()),
                protocol,
                cancel: None,
            };
            let agent = AgentLoop::new(
                provider,
                summary_builder(),
                BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).unwrap()),
            );
            let (_handle, mut control) = agent_execution_channel(1);
            let mut events = Vec::new();
            let result = agent
                .semantic_compaction_summary(
                    &task,
                    &PromptCacheScope::session(task.session_id, None),
                    task.turn_id,
                    &mut record,
                    None,
                    &mut control,
                    &mut |event| events.push(event),
                    &mut None,
                )
                .await
                .unwrap();
            assert_eq!(result, CHECKPOINT);
            assert!(record.apply_model_summary(&result));
            assert_eq!(record.summary_attempts, 2);
            assert!(record.summary_failure.is_none());
            let requests = agent.provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_ne!(requests[0].request_id, requests[1].request_id);
            assert_eq!(requests[0].messages[1], requests[1].messages[1]);
            assert_ne!(requests[0].messages[0], requests[1].messages[0]);
            assert!(requests[1].max_output_tokens >= requests[0].max_output_tokens);
            assert!(requests.iter().all(|request| request.tools.is_empty()));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, AgentLoopTraceEvent::TokenUsageRecorded(_)))
                    .count(),
                2
            );
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, AgentLoopTraceEvent::ProviderStreamed { .. }))
            );
        }
    }
}

#[tokio::test]
async fn empty_and_abnormal_summaries_fall_back_without_length_retry() {
    for finish in [
        ProviderFinishReason::Stop,
        ProviderFinishReason::Continue,
        ProviderFinishReason::ToolCalls,
        ProviderFinishReason::ContentFilter,
        ProviderFinishReason::Error,
        ProviderFinishReason::Unknown,
    ] {
        let workspace = tempdir().unwrap();
        let task = summary_task();
        let mut record = history_record(task.turn_id);
        let baseline = record.summary.clone();
        let provider = SummaryScript {
            replies: vec![(finish, String::new())],
            requests: Mutex::new(Vec::new()),
            protocol: "anthropic",
            cancel: None,
        };
        let agent = AgentLoop::new(
            provider,
            summary_builder(),
            BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).unwrap()),
        );
        let (_handle, mut control) = agent_execution_channel(1);
        let result = agent
            .semantic_compaction_summary(
                &task,
                &PromptCacheScope::session(task.session_id, None),
                task.turn_id,
                &mut record,
                None,
                &mut control,
                &mut |_| {},
                &mut None,
            )
            .await;
        assert!(result.is_none());
        assert_eq!(record.summary, baseline);
        assert_eq!(record.summary_attempts, 1);
        assert!(record.summary_failure.is_some());
    }
}

fn plan(storage: u64) -> (SessionId, CompactionSummaryPlan) {
    let task = summary_task();
    let baseline = compaction_summary_envelope(
        CHECKPOINT,
        CompactionSourceRange { start: 0, end: 1 },
        100,
        compaction_source_checksum(CHECKPOINT),
        storage,
    );
    let request = compaction_summary_request(
        task.task_id,
        task.turn_id,
        &MockProvider::text_response("").contract(),
        PromptCacheScope::session(task.session_id, None),
        Some(CHECKPOINT.into()),
        &[],
        storage,
    )
    .unwrap();
    (
        task.session_id,
        CompactionSummaryPlan::new(request, &baseline, storage).unwrap(),
    )
}

#[test]
fn generation_and_storage_budgets_are_separate_and_respect_model_limits() {
    let (session_id, mut plan) = plan(4_096);
    let (request, snapshot) = plan.prepare(&summary_builder(), session_id).unwrap();
    assert_eq!(request.max_output_tokens, Some(8_192));
    assert_eq!(snapshot.budget_snapshot.reserved_output_tokens, 8_192);
    assert!(request.messages[0].content.contains("2976 tokens"));
    assert!(plan.retry(SummaryFailure::OutputTruncated));
    let (retry, snapshot) = plan.prepare(&summary_builder(), session_id).unwrap();
    assert_eq!(retry.max_output_tokens, Some(16_384));
    assert_eq!(snapshot.budget_snapshot.reserved_output_tokens, 16_384);
    assert!(!plan.retry(SummaryFailure::OutputTruncated));

    let (session_id, mut limited) = self::plan(4_096);
    let builder = ContextBuilder::new(ContextBudgetPolicy {
        context_window: 32_768,
        max_output: 512,
        budget_limit: 32_256,
        action_if_exceeded: BudgetOverflowAction::Compact,
    });
    let (request, snapshot) = limited.prepare(&builder, session_id).unwrap();
    assert_eq!(request.max_output_tokens, Some(512));
    assert_eq!(snapshot.budget_snapshot.max_output, 512);
    assert!(
        request.messages[0]
            .content
            .contains("effective body target is at most 256 tokens")
    );
}

#[test]
fn input_overflow_skips_network_and_keeps_the_baseline_available() {
    let (session_id, mut plan) = plan(4_096);
    let builder = ContextBuilder::new(ContextBudgetPolicy {
        context_window: 128,
        max_output: 32,
        budget_limit: 96,
        action_if_exceeded: BudgetOverflowAction::Compact,
    });
    assert_eq!(
        plan.prepare(&builder, session_id).unwrap_err(),
        SummaryFailure::InputTooLarge
    );
    assert_eq!(plan.attempts(), 0);
    assert!(!plan.retry(SummaryFailure::InputTooLarge));
}

#[tokio::test]
async fn cancelling_automatic_summary_does_not_commit_a_compaction_boundary() {
    let workspace = tempdir().unwrap();
    let (handle, control) = agent_execution_channel(1);
    let provider = SummaryScript {
        replies: Vec::new(),
        requests: Mutex::new(Vec::new()),
        protocol: "anthropic",
        cancel: Some(handle.cancellation_token()),
    };
    let builder = ContextBuilder::new(ContextBudgetPolicy {
        context_window: 16_384,
        max_output: 4_096,
        budget_limit: 12_288,
        action_if_exceeded: BudgetOverflowAction::Compact,
    });
    let agent = AgentLoop::new(
        provider,
        builder,
        BasicToolExecutor::new(WorkspacePolicy::new(workspace.path()).unwrap()),
    );
    let mut task = summary_task();
    task.contributors
        .extend((0..24).map(|index| ContextContributor {
            name: format!("history:{index}"),
            role: ProviderRole::User,
            content: "conversation ".repeat(200),
            token_budget_hint: u64::MAX,
            source_refs: Vec::new(),
        }));
    let mut events = Vec::new();
    let result = agent
        .run_with_control_and_trace(task, control, &mut |event| events.push(event))
        .await;
    assert!(
        matches!(result, Err(AgentLoopError::Cancelled)),
        "{result:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentLoopTraceEvent::ContextAutoCompacted(_)))
    );
    let started = events.iter().find_map(|event| match event {
        AgentLoopTraceEvent::ContextCompactionStarted { compaction_id, .. } => {
            Some(compaction_id.clone())
        }
        _ => None,
    });
    let failed = events.iter().find_map(|event| match event {
        AgentLoopTraceEvent::ContextCompactionFailed { compaction_id, .. } => {
            Some(compaction_id.clone())
        }
        _ => None,
    });
    assert!(started.as_ref().is_some_and(|id| !id.is_empty()));
    assert_eq!(started, failed);
}
