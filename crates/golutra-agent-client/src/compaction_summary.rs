//! 显式压缩的异步执行与审计适配；预算和结果验收复用 runtime 的公共策略。

use super::*;
use golutra_agent_context::ContextBuilder;

impl RuntimeHost {
    /// 返回完整摘要封装或明确降级原因；持久化失败独立返回，取消不安装备用历史。
    pub(super) async fn complete_explicit_summary<P: LlmProvider>(
        &self,
        task: &HostedAgentTask,
        provider: &P,
        builder: &ContextBuilder,
        timeout: Duration,
        plan: &mut CompactionSummaryPlan,
        cancellation: &CancellationToken,
    ) -> Result<Result<String, SummaryFailure>, ClientError> {
        let contract = provider.contract();
        loop {
            if cancellation.is_cancelled() || self.execution.shutdown.is_cancelled() {
                return Ok(Err(SummaryFailure::Cancelled));
            }
            let (request, snapshot) = match plan.prepare(builder, task.session_id) {
                Ok(prepared) => prepared,
                Err(failure) => return Ok(Err(failure)),
            };
            let snapshot_id = snapshot.budget_snapshot.snapshot_id;
            let request_id = request.request_id;
            self.record_auxiliary_trace_observation(
                task,
                AgentLoopTraceEvent::ContextSnapshotCaptured {
                    snapshot,
                    request: request.clone(),
                },
            )
            .await?;
            self.record_auxiliary_trace_observation(
                task,
                AgentLoopTraceEvent::ProviderStarted {
                    request_id,
                    provider_id: contract.provider_id.clone(),
                    model_id: contract.model_id.clone(),
                },
            )
            .await?;
            let result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => Ok(Err(ProviderError::Cancelled)),
                _ = self.execution.shutdown.cancelled() => Ok(Err(ProviderError::Cancelled)),
                result = tokio::time::timeout(timeout, provider.complete(request.clone())) => result,
            };
            match result {
                Ok(Ok(response)) => {
                    let usage = auxiliary_provider_usage_record(
                        &request,
                        &response,
                        Some(task.session_id),
                        snapshot_id,
                        &contract.cost_model,
                        provider.cache_identity_for_request(&request),
                    );
                    self.record_auxiliary_trace_observation(
                        task,
                        AgentLoopTraceEvent::TokenUsageRecorded(usage),
                    )
                    .await?;
                    let summary = plan.accept(&response);
                    self.record_auxiliary_trace_observation(
                        task,
                        AgentLoopTraceEvent::ProviderCompleted {
                            request_id,
                            provider_id: contract.provider_id.clone(),
                            model_id: contract.model_id.clone(),
                            response,
                        },
                    )
                    .await?;
                    if cancellation.is_cancelled() || self.execution.shutdown.is_cancelled() {
                        return Ok(Err(SummaryFailure::Cancelled));
                    }
                    match summary {
                        Ok(text) => {
                            return Ok(plan.envelope(&text).ok_or(SummaryFailure::StorageTooSmall));
                        }
                        Err(failure) if plan.retry(failure) => {}
                        Err(failure) => return Ok(Err(failure)),
                    }
                }
                Ok(Err(error)) => {
                    self.record_auxiliary_trace_observation(
                        task,
                        AgentLoopTraceEvent::ProviderFailed {
                            request_id,
                            provider_id: contract.provider_id.clone(),
                            model_id: contract.model_id.clone(),
                            error: error.to_string(),
                            metadata: error.metadata().cloned(),
                        },
                    )
                    .await?;
                    return Ok(Err(if matches!(error, ProviderError::Cancelled) {
                        SummaryFailure::Cancelled
                    } else {
                        SummaryFailure::ProviderFailed
                    }));
                }
                Err(_) => {
                    self.record_auxiliary_trace_observation(
                        task,
                        AgentLoopTraceEvent::ProviderFailed {
                            request_id,
                            provider_id: contract.provider_id.clone(),
                            model_id: contract.model_id.clone(),
                            error: SummaryFailure::Timeout.to_string(),
                            metadata: None,
                        },
                    )
                    .await?;
                    return Ok(Err(SummaryFailure::Timeout));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use golutra_agent_context::ContextBudgetPolicy;
    use golutra_agent_core::BudgetOverflowAction;
    use golutra_agent_llm::{
        MockProvider, PromptCacheScope, ProviderFinishReason, ProviderRequest, ProviderResponse,
    };

    struct SummaryProvider {
        replies: Vec<(ProviderFinishReason, String)>,
        requests: StdMutex<Vec<ProviderRequest>>,
        cancel: Option<CancellationToken>,
    }

    #[async_trait]
    impl LlmProvider for SummaryProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            let index = {
                let mut requests = self.requests.lock().unwrap();
                requests.push(request.clone());
                requests.len() - 1
            };
            if let Some(token) = &self.cancel {
                token.cancel();
                return Err(ProviderError::Cancelled);
            }
            let (finish, text) = &self.replies[index];
            let mut response = MockProvider::text_response(text).complete(request).await?;
            response.finish_reason = *finish;
            Ok(response)
        }

        fn contract(&self) -> golutra_agent_core::ProviderContract {
            let mut contract = MockProvider::text_response("").contract();
            contract.native_protocol = "anthropic".into();
            contract
        }
    }

    fn plan(task: &HostedAgentTask) -> CompactionSummaryPlan {
        let source = "Preserve API; pending: verify migrations; failed test: missing fixture.";
        let baseline = compaction_summary_envelope(
            source,
            CompactionSourceRange { start: 0, end: 1 },
            100,
            compaction_source_checksum(source),
            DEFAULT_COMPACTION_SUMMARY_TOKENS,
        );
        let request = compaction_summary_request(
            task.task_id,
            task.turn_id,
            &MockProvider::text_response("").contract(),
            PromptCacheScope::session(task.session_id, None),
            Some(source.into()),
            &[],
            DEFAULT_COMPACTION_SUMMARY_TOKENS,
        )
        .unwrap();
        CompactionSummaryPlan::new(request, &baseline, DEFAULT_COMPACTION_SUMMARY_TOKENS).unwrap()
    }

    #[tokio::test]
    async fn explicit_summary_retries_length_and_storage_overflow_with_audited_results() {
        let host = RuntimeHost::in_memory().await.unwrap();
        let builder = ContextBuilder::new(ContextBudgetPolicy {
            context_window: 32_768,
            max_output: 16_384,
            budget_limit: 16_384,
            action_if_exceeded: BudgetOverflowAction::Compact,
        });
        for finish in [ProviderFinishReason::Length, ProviderFinishReason::Stop] {
            let task = HostedAgentTask {
                session_id: host.default_session_id(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                payload: json!({}),
            };
            let mut plan = plan(&task);
            let provider = SummaryProvider {
                replies: vec![
                    (finish, "oversized summary ".repeat(2_000)),
                    (
                        ProviderFinishReason::Stop,
                        "Preserve API. Pending: verify migrations. Tests not run.".into(),
                    ),
                ],
                requests: StdMutex::new(Vec::new()),
                cancel: None,
            };
            let result = host
                .complete_explicit_summary(
                    &task,
                    &provider,
                    &builder,
                    Duration::from_secs(1),
                    &mut plan,
                    &CancellationToken::new(),
                )
                .await
                .unwrap()
                .unwrap();
            let envelope = parse_compaction_summary_envelope(&result).unwrap();
            assert!(envelope.summary.ends_with("Tests not run."));
            assert_eq!(plan.attempts(), 2);
            {
                let requests = provider.requests.lock().unwrap();
                assert_eq!(requests.len(), 2);
                assert_eq!(requests[0].messages[1], requests[1].messages[1]);
                assert_ne!(requests[0].messages[0], requests[1].messages[0]);
            }
            let events = host
                .storage
                .store
                .load_events(task.session_id, Some(task.task_id), None)
                .await
                .unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.event_type == RuntimeEventType::ProviderCompleted)
                    .count(),
                2
            );
        }
    }

    #[tokio::test]
    async fn explicit_summary_repeated_truncation_empty_response_and_cancel_are_distinct() {
        let host = RuntimeHost::in_memory().await.unwrap();
        for (replies, cancelled, expected, attempts) in [
            (
                vec![(ProviderFinishReason::Length, "partial".into()); 2],
                false,
                SummaryFailure::OutputTruncated,
                2,
            ),
            (
                vec![(ProviderFinishReason::Stop, String::new())],
                false,
                SummaryFailure::Empty,
                1,
            ),
            (Vec::new(), true, SummaryFailure::Cancelled, 1),
        ] {
            let task = HostedAgentTask {
                session_id: host.default_session_id(),
                task_id: TaskId::new(),
                turn_id: TurnId::new(),
                payload: json!({}),
            };
            let mut plan = plan(&task);
            let token = CancellationToken::new();
            let provider = SummaryProvider {
                replies,
                requests: StdMutex::new(Vec::new()),
                cancel: cancelled.then(|| token.clone()),
            };
            let result = host
                .complete_explicit_summary(
                    &task,
                    &provider,
                    &ContextBuilder::default(),
                    Duration::from_secs(1),
                    &mut plan,
                    &token,
                )
                .await
                .unwrap();
            assert_eq!(result, Err(expected));
            assert_eq!(plan.attempts(), attempts);
        }
    }
}
