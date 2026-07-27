//! 任务终态后的评估输入、durable job、候选事件与 memory quarantine 编排。

use super::*;

pub(super) fn is_security_policy_violation(evaluation: &PolicyEvaluation) -> bool {
    match evaluation.decision {
        PolicyDecision::Deny => true,
        PolicyDecision::Block => {
            evaluation.effective_block_disposition() == Some(PolicyBlockDisposition::Terminal)
        }
        PolicyDecision::Allow | PolicyDecision::Ask => false,
    }
}

fn policy_violation_count(events: &[RuntimeEvent]) -> usize {
    events
        .iter()
        .filter(|event| event.event_type == RuntimeEventType::PolicyEvaluated)
        .filter_map(|event| event.payload.get("record").cloned())
        .filter_map(|record| serde_json::from_value::<PolicyEvaluation>(record).ok())
        .filter(is_security_policy_violation)
        .count()
}

impl RuntimeHost {
    pub(super) async fn schedule_task_evaluation_best_effort(
        &self,
        task: &HostedAgentTask,
        input: HostedTaskEvaluation<'_>,
    ) {
        let result = async {
            let evaluation_input = self.evaluate_completed_task(task, input).await?;
            self.enqueue_deep_task_evaluation(task, evaluation_input)
                .await
        }
        .await;
        if let Err(error) = result {
            self.record_post_task_governance_failure(task, "evaluation_scheduling", true, &error)
                .await;
        }
    }

    pub(super) async fn record_post_task_governance_failure(
        &self,
        task: &HostedAgentTask,
        phase: &str,
        terminal: bool,
        error: &ClientError,
    ) {
        let _ = self
            .record_event(agent_event(
                self.next_sequence_no(),
                task,
                RuntimeEventType::PostTaskStageFailed,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": "post-task governance failed after runtime terminal decision",
                    "phase": phase,
                    "terminal": terminal,
                    "error": compact_event_summary(&error.to_string()),
                    "execution_outcome_unchanged": true,
                }),
            ))
            .await;
    }

    pub(super) async fn evaluate_completed_task(
        &self,
        task: &HostedAgentTask,
        input: HostedTaskEvaluation<'_>,
    ) -> Result<TaskEvaluationInput, ClientError> {
        let events = self
            .repositories
            .events
            .load(task.session_id, Some(task.task_id), None)
            .await?;
        let artifact_count = input
            .tool_reports
            .iter()
            .map(|report| report.artifacts.len())
            .sum();
        let token_usage = events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::TokenUsageRecorded)
            .filter_map(|event| event.payload.get("record").cloned())
            .filter_map(|record| serde_json::from_value::<TokenUsageRecord>(record).ok())
            .collect::<Vec<_>>();
        let policy_violation_count = policy_violation_count(&events);
        let provider_config_ref = token_usage.last().map_or_else(
            || "runtime-active-profile".to_owned(),
            |record| format!("{}:{}", record.provider_id, record.model_id),
        );
        let evaluation_input = TaskEvaluationInput {
            task_id: task.task_id,
            objective: input.objective.to_owned(),
            task_status: input.task_status,
            verification: input.verification,
            event_count: events.len(),
            artifact_count,
            tool_count: input.tool_reports.len(),
            latency_ms: Some(u64::try_from(input.latency.as_millis()).unwrap_or(u64::MAX)),
            failure_summary: input.failure_summary,
            token_usage,
            provider_config_ref,
            runtime_config_ref: format!("golutra-runtime:{}", env!("CARGO_PKG_VERSION")),
            policy_violation_count: u32::try_from(policy_violation_count).unwrap_or(u32::MAX),
        };
        let bundle = self.governance.evaluate_minimal(evaluation_input.clone());
        // Post-task governance is durable but does not rewrite the already-decided user task
        // status. The deep worker will retry a failed persistence attempt independently.
        let _ = self.record_task_evaluation(task, bundle).await?;
        Ok(evaluation_input)
    }

    pub(super) async fn enqueue_deep_task_evaluation(
        &self,
        task: &HostedAgentTask,
        input: TaskEvaluationInput,
    ) -> Result<bool, ClientError> {
        if let Some(existing) = self.repositories.jobs.get_for_task(task.task_id).await? {
            match existing.status {
                PostTaskJobStatus::Queued
                | PostTaskJobStatus::Leased
                | PostTaskJobStatus::Running => {
                    self.deep_evaluation_inputs
                        .lock()
                        .await
                        .insert(existing.job_id, input);
                    return Ok(false);
                }
                PostTaskJobStatus::Succeeded => return Ok(false),
                PostTaskJobStatus::Failed | PostTaskJobStatus::Cancelled => {
                    let retried = self.repositories.jobs.retry(existing.job_id).await?;
                    if retried {
                        self.deep_evaluation_inputs
                            .lock()
                            .await
                            .insert(existing.job_id, input);
                    }
                    return Ok(retried);
                }
            }
        }

        let now = chrono::Utc::now();
        let job = PostTaskJob {
            job_id: PostTaskJobId::new(),
            kind: PostTaskJobKind::DeepEvaluation,
            workspace_id: self.workspace_id.to_string(),
            session_id: task.session_id.to_string(),
            task_id: task.task_id,
            input_refs: vec![
                format!("session:{}", task.session_id),
                format!("task:{}", task.task_id),
                format!("turn:{}", task.turn_id),
            ],
            status: PostTaskJobStatus::Queued,
            attempt: 0,
            max_attempts: POST_TASK_JOB_MAX_ATTEMPTS,
            lease_owner: None,
            lease_expires_at: None,
            result_refs: Vec::new(),
            last_error: None,
            created_at: now,
            started_at: None,
            completed_at: None,
        };
        let event = agent_event(
            0,
            task,
            RuntimeEventType::PostTaskJobQueued,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": "durable post-task evaluation queued",
                "job": job,
                "mode": "deep",
            }),
        );
        let _writer = self.event_writer.lock().await;
        let causal_before = self.causal_ledger.lock().await.clone();
        let event = match self.prepare_canonical_event(event).await {
            Ok(event) => event,
            Err(error) => {
                *self.causal_ledger.lock().await = causal_before;
                return Err(error);
            }
        };
        let event = match self.repositories.jobs.enqueue_with_event(&job, event).await {
            Ok(Some(event)) => event,
            Ok(None) => {
                *self.causal_ledger.lock().await = causal_before;
                return Ok(false);
            }
            Err(error) => {
                *self.causal_ledger.lock().await = causal_before;
                return Err(error.into());
            }
        };
        self.next_sequence_no
            .fetch_max(event.sequence_no.saturating_add(1), Ordering::SeqCst);
        self.deep_evaluation_inputs
            .lock()
            .await
            .insert(job.job_id, input);
        self.publish_committed_event(event).await?;
        Ok(true)
    }

    /// Recreate a missing durable post-task job after a process exits between
    /// terminal event commit and job enqueue. The store query is workspace
    /// scoped and the enqueue transaction is idempotent across processes.
    pub(super) async fn recover_unscheduled_post_task_jobs(&self) -> Result<usize, ClientError> {
        let workspace_root = self.workspace_root_string();
        let terminal_events = self
            .repositories
            .jobs
            .unscheduled_terminal_events(workspace_root.as_deref())
            .await?;
        let mut recovered = 0usize;
        for terminal_event in terminal_events {
            let Some(task_id) = terminal_event.task_id else {
                continue;
            };
            let synthetic_job = PostTaskJob {
                job_id: PostTaskJobId::new(),
                kind: PostTaskJobKind::DeepEvaluation,
                workspace_id: self.workspace_id.to_string(),
                session_id: terminal_event.session_id.to_string(),
                task_id,
                input_refs: Vec::new(),
                status: PostTaskJobStatus::Queued,
                attempt: 0,
                max_attempts: POST_TASK_JOB_MAX_ATTEMPTS,
                lease_owner: None,
                lease_expires_at: None,
                result_refs: Vec::new(),
                last_error: None,
                created_at: terminal_event.timestamp,
                started_at: None,
                completed_at: None,
            };
            let (task, input) = self.reconstruct_post_task_context(&synthetic_job).await?;
            if self.enqueue_deep_task_evaluation(&task, input).await? {
                recovered = recovered.saturating_add(1);
            }
        }
        Ok(recovered)
    }

    pub(super) async fn reconstruct_post_task_context(
        &self,
        job: &PostTaskJob,
    ) -> Result<(HostedAgentTask, TaskEvaluationInput), ClientError> {
        let queued_input = self.deep_evaluation_inputs.lock().await.remove(&job.job_id);
        let session_id = job.session_id.parse().map_err(|error: uuid::Error| {
            ClientError::InvalidSession(format!("post-task job session id is invalid: {error}"))
        })?;
        let events = self
            .repositories
            .events
            .load(session_id, Some(job.task_id), None)
            .await?;
        let objective = events
            .iter()
            .rev()
            .find_map(|event| {
                event
                    .payload
                    .get("prompt")
                    .or_else(|| event.payload.get("objective"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or("recovered post-task evaluation")
            .to_owned();
        let turn_id = events
            .iter()
            .rev()
            .find_map(|event| event.turn_id)
            .unwrap_or_else(TurnId::new);
        let task = HostedAgentTask {
            session_id,
            task_id: job.task_id,
            turn_id,
            payload: json!({"prompt": objective}),
        };
        let status = events
            .iter()
            .rev()
            .find(|event| {
                event.event_type.is_task_terminal()
                    || event.event_type == RuntimeEventType::LoopDecided
            })
            .and_then(|event| event.payload.get("status"))
            .cloned()
            .and_then(|value| serde_json::from_value::<TaskStatus>(value).ok())
            .unwrap_or(TaskStatus::Failed);
        let verification = events
            .iter()
            .rev()
            .find(|event| event.event_type == RuntimeEventType::VerificationCompleted)
            .and_then(|event| event.payload.get("record"))
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok());
        let token_usage = events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::TokenUsageRecorded)
            .filter_map(|event| event.payload.get("record").cloned())
            .filter_map(|value| serde_json::from_value::<TokenUsageRecord>(value).ok())
            .collect::<Vec<_>>();
        let policy_violation_count = policy_violation_count(&events);
        let tool_events = events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::ToolCompleted)
            .collect::<Vec<_>>();
        let artifact_count = tool_events
            .iter()
            .filter_map(|event| event.payload.get("envelope"))
            .filter_map(|envelope| envelope.get("raw_artifact_ref"))
            .count();
        let failure_summary = events
            .iter()
            .rev()
            .find(|event| event.event_type == RuntimeEventType::LoopDecided)
            .and_then(|event| {
                event
                    .payload
                    .get("summary")
                    .or_else(|| event.payload.get("error"))
                    .and_then(Value::as_str)
            })
            .map(ToOwned::to_owned);
        let latency_ms = events.first().zip(events.last()).and_then(|(first, last)| {
            u64::try_from(
                last.timestamp
                    .signed_duration_since(first.timestamp)
                    .num_milliseconds(),
            )
            .ok()
        });
        let provider_config_ref = token_usage.last().map_or_else(
            || "runtime-active-profile".to_owned(),
            |record| format!("{}:{}", record.provider_id, record.model_id),
        );
        let input = queued_input.unwrap_or(TaskEvaluationInput {
            task_id: job.task_id,
            objective,
            task_status: status,
            verification,
            event_count: events.len(),
            artifact_count,
            tool_count: tool_events.len(),
            latency_ms,
            failure_summary,
            token_usage,
            provider_config_ref,
            runtime_config_ref: format!("golutra-runtime:{}", env!("CARGO_PKG_VERSION")),
            policy_violation_count: u32::try_from(policy_violation_count).unwrap_or(u32::MAX),
        });
        Ok((task, input))
    }

    pub(super) async fn wait_for_candidate_evaluation(&self, candidate_id: &str) {
        let Some(task_id) = task_id_from_candidate_id(candidate_id) else {
            return;
        };
        self.wait_for_deep_task_evaluation(task_id).await;
    }

    pub(super) async fn wait_for_deep_task_evaluation(&self, task_id: TaskId) {
        let deadline = Instant::now() + Duration::from_secs(10);

        // TaskCompleted is deliberately published before post-task governance starts. If a
        // settled observer arrives in that window, wait for the task supervisor to finish
        // scheduling (or recording the scheduling failure) before treating an absent job as
        // terminal. The user-visible task result remains independent of this barrier.
        let mut task_completion = self
            .task_controls
            .lock()
            .await
            .values()
            .find(|control| control.task_id == task_id)
            .map(|control| control.completion.clone());
        if let Some(completion) = task_completion.as_mut() {
            while !*completion.borrow() && Instant::now() < deadline {
                tokio::select! {
                    changed = completion.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                    () = tokio::time::sleep(Duration::from_millis(POST_TASK_JOB_POLL_MILLIS)) => {}
                }
            }
        }

        let mut session_id = self
            .task_controls
            .lock()
            .await
            .iter()
            .find(|(_, control)| control.task_id == task_id)
            .map(|(session_id, _)| *session_id);
        loop {
            let job = self.repositories.jobs.get_for_task(task_id).await;
            if session_id.is_none() {
                session_id = job
                    .as_ref()
                    .ok()
                    .and_then(|job| job.as_ref())
                    .and_then(|job| job.session_id.parse().ok());
            }
            if session_id.is_none() {
                session_id = self
                    .repositories
                    .events
                    .session_for_task(task_id)
                    .await
                    .ok()
                    .flatten();
            }
            let events = match session_id {
                Some(session_id) => self
                    .repositories
                    .events
                    .load(session_id, Some(task_id), None)
                    .await
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            let execution_terminal = events
                .iter()
                .any(|event| event.event_type.is_task_terminal());
            let scheduling_queued = events
                .iter()
                .any(|event| event.event_type == RuntimeEventType::PostTaskJobQueued);
            let scheduling_failed = events.iter().any(|event| {
                event.event_type == RuntimeEventType::PostTaskStageFailed
                    && event.payload.get("terminal").and_then(Value::as_bool) == Some(true)
            });
            let governance_pending = events
                .iter()
                .rev()
                .find(|event| event.event_type.is_task_terminal())
                .and_then(|event| {
                    event
                        .payload
                        .pointer("/post_task_governance/status")
                        .and_then(Value::as_str)
                })
                == Some("pending");
            let job_terminal = matches!(
                job,
                Ok(Some(PostTaskJob {
                    status: PostTaskJobStatus::Succeeded
                        | PostTaskJobStatus::Failed
                        | PostTaskJobStatus::Cancelled,
                    ..
                }))
            );
            if job_terminal
                || (execution_terminal
                    && (scheduling_failed || (!governance_pending && !scheduling_queued)))
                || Instant::now() >= deadline
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(POST_TASK_JOB_POLL_MILLIS)).await;
        }
    }

    pub(super) async fn record_task_evaluation(
        &self,
        task: &HostedAgentTask,
        bundle: TaskEvaluationBundle,
    ) -> Result<bool, ClientError> {
        let recorded = match self.governance.persist_evaluation(bundle).await {
            Ok(recorded) => recorded,
            Err(error) => {
                self.record_event(agent_event(
                    self.next_sequence_no(),
                    task,
                    RuntimeEventType::EvaluationCompleted,
                    RuntimeEventSource::Evaluator,
                    json!({
                        "summary": "durable task evaluation failed",
                        "error": error.to_string(),
                    }),
                ))
                .await?;
                return Ok(false);
            }
        };
        let result = recorded.result;
        let review = recorded.review;
        let deep_review = review.mode == ReviewMode::Deep;
        let improvement_candidate = recorded.improvement_candidate;
        let improvement_candidate_id = improvement_candidate
            .as_ref()
            .map(|candidate| candidate.id.clone());
        let automation_candidates = recorded.automation_candidates;
        self.record_event(agent_event(
            self.next_sequence_no(),
            task,
            RuntimeEventType::PostTaskReviewed,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!("{:?} post-task review outcome: {}", review.mode, review.outcome),
                "record": review,
            }),
        ))
        .await?;
        self.record_event(agent_event(
            self.next_sequence_no(),
            task,
            RuntimeEventType::EvaluationCompleted,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!("task evaluation verdict: {:?}", result.verdict),
                "record": result,
            }),
        ))
        .await?;
        if let Some(candidate) = improvement_candidate {
            self.record_event(agent_event(
                self.next_sequence_no(),
                task,
                RuntimeEventType::ImprovementCandidateCreated,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!("improvement candidate {} proposed", candidate.id),
                    "record": candidate,
                }),
            ))
            .await?;
        }
        if !automation_candidates.is_empty() {
            self.record_event(agent_event(
                self.next_sequence_no(),
                task,
                RuntimeEventType::AutomationCandidateCreated,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!("{} governed automation candidates proposed", automation_candidates.len()),
                    "records": automation_candidates,
                }),
            ))
            .await?;
        }
        if deep_review {
            self.record_observation_products(task).await?;
            if let Some(candidate_id) = improvement_candidate_id {
                Box::pin(
                    self.automatically_process_improvement_candidate(
                        task.session_id,
                        &candidate_id,
                    ),
                )
                .await?;
            }
        }
        Ok(true)
    }

    async fn record_observation_products(&self, task: &HostedAgentTask) -> Result<(), ClientError> {
        let events = self
            .repositories
            .events
            .load(task.session_id, Some(task.task_id), None)
            .await?;
        let integrity = self
            .repositories
            .events
            .integrity(task.session_id, task.task_id)
            .await?;
        let run_provenance = events
            .first()
            .and_then(|event| event.payload.get("run_provenance"))
            .cloned()
            .and_then(|value| serde_json::from_value::<RunProvenance>(value).ok());
        let capsule = diagnosis::replay_capsule(
            task.task_id,
            &events,
            integrity.event_chain_digest,
            run_provenance
                .as_ref()
                .and_then(|provenance| provenance.runtime_config_digest.clone()),
        );
        let evaluation_store = self.evaluation_store.clone();
        let capsule_for_store = capsule.clone();
        let replay_inserted =
            run_blocking(move || evaluation_store.record_replay_capsule(capsule_for_store))
                .await??;
        if replay_inserted {
            self.record_event(agent_event(
                self.next_sequence_no(),
                task,
                RuntimeEventType::ReplayCapsuleCreated,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!("deterministic replay capsule {} created", capsule.capsule_id),
                    "record": capsule,
                }),
            ))
            .await?;
        }

        let source_digest = run_provenance.and_then(|provenance| provenance.build.source_digest);
        let projected_episodes = diagnosis::task_failure_episodes(task.task_id, &events);
        let Some(analysis) = diagnosis::diagnose_task(task.task_id, &events, source_digest) else {
            if !projected_episodes.is_empty() {
                let evaluation_store = self.evaluation_store.clone();
                let changed = run_blocking(move || {
                    evaluation_store.record_failure_episodes(projected_episodes)
                })
                .await??;
                for episode in changed {
                    self.record_event(agent_event(
                        self.next_sequence_no(),
                        task,
                        RuntimeEventType::FailureEpisodeRecorded,
                        RuntimeEventSource::Evaluator,
                        json!({
                            "summary": format!(
                                "failure episode {} is {:?}",
                                episode.episode_id, episode.status
                            ),
                            "record": episode,
                        }),
                    ))
                    .await?;
                }
            }
            return Ok(());
        };
        let evaluation_store = self.evaluation_store.clone();
        let analysis_for_store = analysis.clone();
        let update = run_blocking(move || {
            evaluation_store.record_failure_products(
                analysis_for_store.diagnosis,
                analysis_for_store.slice,
                analysis_for_store.episodes,
                Some(analysis_for_store.candidate),
            )
        })
        .await??;
        if update.diagnosis_inserted {
            self.record_event(agent_event(
                self.next_sequence_no(),
                task,
                RuntimeEventType::FailureDiagnosed,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": &analysis.diagnosis.summary,
                    "record": analysis.diagnosis,
                }),
            ))
            .await?;
            self.record_event(agent_event(
                self.next_sequence_no(),
                task,
                RuntimeEventType::DiagnosticSliceCreated,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!("bounded diagnostic slice {} created", analysis.slice.slice_id),
                    "record": analysis.slice,
                }),
            ))
            .await?;
        }
        for episode in update.changed_episodes {
            self.record_event(agent_event(
                self.next_sequence_no(),
                task,
                RuntimeEventType::FailureEpisodeRecorded,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!(
                        "failure episode {} is {:?}",
                        episode.episode_id, episode.status
                    ),
                    "record": episode,
                }),
            ))
            .await?;
        }
        if let Some(candidate) = update.improvement_candidate {
            self.record_event(agent_event(
                self.next_sequence_no(),
                task,
                RuntimeEventType::ImprovementCandidateCreated,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!(
                        "actionable improvement candidate {} projected from failure diagnosis",
                        candidate.id
                    ),
                    "record": candidate,
                }),
            ))
            .await?;
        }
        if let Some(candidate) = update.automation_candidate {
            self.record_event(agent_event(
                self.next_sequence_no(),
                task,
                RuntimeEventType::AutomationCandidateCreated,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!(
                        "runtime automation candidate {} synchronized from failure diagnosis",
                        candidate.id
                    ),
                    "record": candidate,
                }),
            ))
            .await?;
        }
        Ok(())
    }

    pub(super) async fn promote_successful_task_memory(
        &self,
        task: &HostedAgentTask,
        objective: &str,
        outcome: &golutra_runtime::AgentLoopOutcome,
        terminal_status: TaskStatus,
    ) -> Result<(), ClientError> {
        if terminal_status != TaskStatus::Completed || outcome.verification.evidence_refs.is_empty()
        {
            return Ok(());
        }
        let tool_facts = outcome
            .tool_reports
            .iter()
            .map(|report| report.envelope.summary.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        let final_message = outcome
            .final_message
            .as_deref()
            .unwrap_or("verified completion");
        let promotion = self
            .governance
            .quarantine_verified_memory(
                task.task_id,
                objective,
                final_message,
                &tool_facts,
                outcome.verification.evidence_refs.clone(),
            )
            .await?;
        match promotion {
            Ok(record) => {
                self.record_event(agent_event(
                    self.next_sequence_no(),
                    task,
                    RuntimeEventType::MemoryCandidateQuarantined,
                    RuntimeEventSource::Memory,
                    json!({
                        "summary": format!("project memory {} quarantined", record.memory_id),
                        "record": record,
                    }),
                ))
                .await?;
            }
            Err(error) => {
                self.record_event(agent_event(
                    self.next_sequence_no(),
                    task,
                    RuntimeEventType::MemoryPromotionRejected,
                    RuntimeEventSource::Memory,
                    json!({
                        "summary": "project memory promotion rejected",
                        "reason": error.to_string(),
                    }),
                ))
                .await?;
            }
        }
        Ok(())
    }
}
