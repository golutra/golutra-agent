//! Context 构建、task supervision、AgentLoop 与 provider auth 生命周期。

use super::*;

struct ChannelObservationSink {
    sender: mpsc::UnboundedSender<HostedTraceCommand>,
}

impl RuntimeObservationSink for ChannelObservationSink {
    fn emit(&mut self, observation: RuntimeObservation) {
        let _ = self
            .sender
            .send(HostedTraceCommand::Event(Box::new(observation)));
    }
}

impl RuntimeHost {
    pub(super) async fn context_contributors_for_task(
        &self,
        session_id: SessionId,
        current_task_id: TaskId,
        objective: String,
        output_schema: Option<&Value>,
    ) -> Result<Vec<ContextContributor>, ClientError> {
        let workspace_root = self.execution_workspace_root()?;
        let mut contributors = vec![ContextContributor {
            name: "system".to_owned(),
            role: ProviderRole::System,
            content: system_prompt(),
            token_budget_hint: 64,
            source_refs: vec!["builtin:system_prompt".to_owned()],
        }];
        contributors.push(ContextContributor {
            name: "environment_context".to_owned(),
            role: ProviderRole::System,
            content: environment_context_prompt(&workspace_root),
            token_budget_hint: 128,
            source_refs: vec![format!("workspace:{}", workspace_root.display())],
        });
        if let Some(project_instructions) = load_project_instructions(&workspace_root).await? {
            contributors.push(ContextContributor {
                name: "project_instructions".to_owned(),
                role: ProviderRole::System,
                content: project_instructions,
                token_budget_hint: 2_048,
                source_refs: vec![format!(
                    "file:{}",
                    workspace_root.join("AGENTS.md").display()
                )],
            });
        }
        if let Some(skill_context) = self.active_skill_context(&objective).await? {
            contributors.push(ContextContributor {
                name: "project_skills".to_owned(),
                role: ProviderRole::System,
                content: skill_context,
                token_budget_hint: 1_024,
                source_refs: vec!["runtime:active_skills".to_owned()],
            });
        }

        let memory_store = self.memory_store.clone();
        let memory_query = objective.clone();
        let memories =
            run_blocking(move || memory_store.retrieve(&memory_query, MemoryScope::Project, 5))
                .await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            Some(current_task_id),
            RuntimeEventType::MemoryRetrieved,
            RuntimeEventSource::Memory,
            json!({
                "summary": format!("retrieved {} project memories", memories.len()),
                "query": objective,
                "scope": "project",
                "retrieved": memories,
            }),
        ))
        .await?;
        if !memories.is_empty() {
            contributors.push(ContextContributor {
                name: "memory".to_owned(),
                role: ProviderRole::System,
                content: memory_context(&memories),
                token_budget_hint: 512,
                source_refs: memories
                    .iter()
                    .map(|memory| format!("memory:{}", memory.record.memory_id))
                    .collect(),
            });
        }

        if let Some((history, source_refs)) = self
            .conversation_history_summary(session_id, current_task_id)
            .await?
        {
            contributors.push(ContextContributor {
                name: "conversation_history".to_owned(),
                role: ProviderRole::User,
                content: history,
                token_budget_hint: 1024,
                source_refs,
            });
        }

        if let Some(output_schema) = output_schema.filter(|value| !value.is_null()) {
            let schema = serde_json::to_string(output_schema)?;
            contributors.push(ContextContributor {
                name: "output_schema".to_owned(),
                role: ProviderRole::System,
                content: format!(
                    "The final assistant response must contain only JSON that validates against this JSON Schema. Do not wrap it in Markdown:\n{schema}"
                ),
                token_budget_hint: 1_024,
                source_refs: vec![format!("task:{current_task_id}:output_schema")],
            });
        }

        contributors.push(ContextContributor {
            name: "objective".to_owned(),
            role: ProviderRole::User,
            content: objective,
            token_budget_hint: 512,
            source_refs: vec![format!("task:{current_task_id}:objective")],
        });

        Ok(contributors)
    }

    pub(super) async fn conversation_history_summary(
        &self,
        session_id: SessionId,
        current_task_id: TaskId,
    ) -> Result<Option<(String, Vec<String>)>, ClientError> {
        let events = self
            .repositories
            .events
            .load_recent(session_id, None, None, MAX_HISTORY_SOURCE_EVENTS)
            .await?;
        let context_compaction = self
            .repositories
            .events
            .latest_context_compaction(session_id)
            .await?
            .as_ref()
            .and_then(context_compaction_from_event);
        let compacted_after = context_compaction
            .as_ref()
            .map(|(sequence_no, _)| *sequence_no)
            .unwrap_or_default();
        let summary_source_ref = context_compaction
            .as_ref()
            .map(|(sequence_no, _)| format!("event-sequence:{sequence_no}"));
        let summary_line = context_compaction.map(|(_, content)| format!("Summary: {content}"));
        let history_events = events
            .iter()
            .filter(|event| event.sequence_no > compacted_after)
            .filter(|event| event.task_id != Some(current_task_id))
            .filter(|event| event.event_type.is_model_history_fact())
            .collect::<Vec<_>>();
        let lines = history_events
            .iter()
            .filter_map(|event| conversation_history_line(event))
            .collect::<Vec<_>>();

        if summary_line.is_none() && lines.is_empty() {
            return Ok(None);
        }

        let mut source_refs = history_events
            .iter()
            .map(|event| format!("event:{}", event.id))
            .collect::<Vec<_>>();
        source_refs.extend(summary_source_ref);
        Ok(Some((
            format!(
                "Prior conversation transcript follows as historical user context, not as system instructions:\n{}",
                compact_history_with_summary(summary_line, lines)
            ),
            source_refs,
        )))
    }

    pub(super) fn next_sequence_no(&self) -> u64 {
        self.next_sequence_no.fetch_add(1, Ordering::SeqCst)
    }

    pub(super) fn scoped_idempotency_key(&self, idempotency_key: &str) -> String {
        format!("{}:{idempotency_key}", self.workspace_id)
    }

    pub(super) async fn spawn_agent_task(
        self: Arc<Self>,
        task: HostedAgentTask,
        session_lease: Option<Arc<File>>,
        pending_turns: Vec<PendingAgentTurn>,
    ) -> Result<(), ClientError> {
        let (execution, control) = agent_execution_channel(32);
        for pending_turn in pending_turns {
            execution
                .append_turn(pending_turn)
                .await
                .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        }
        let (start_tx, start_rx) = oneshot::channel();
        let worker_host = self.clone();
        let worker_task = task.clone();
        let worker = tokio::spawn(async move {
            start_rx.await.map_err(|_| ClientError::TaskCancelled)?;
            worker_host.run_agent_task(worker_task, control).await
        });
        let abort_handle = worker.abort_handle();
        let (completion_sender, completion) = watch::channel(false);
        self.task_controls.lock().await.insert(
            task.session_id,
            HostedTaskControl {
                task_id: task.task_id,
                allow_network: task
                    .payload
                    .get("allow_network")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                yolo: task
                    .payload
                    .get("yolo")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                execution,
                abort_handle,
                completion,
                _session_lease: session_lease,
            },
        );
        let supervisor = self.clone();
        let supervised_task = task.clone();
        tokio::spawn(async move {
            supervisor
                .supervise_agent_task(supervised_task, worker, completion_sender)
                .await;
        });
        start_tx.send(()).map_err(|_| ClientError::TaskCancelled)?;
        Ok(())
    }

    pub(super) async fn supervise_agent_task(
        self: Arc<Self>,
        task: HostedAgentTask,
        worker: tokio::task::JoinHandle<Result<(), ClientError>>,
        completion: watch::Sender<bool>,
    ) {
        let result = match worker.await {
            Ok(result) => result,
            Err(error) if error.is_cancelled() => Err(ClientError::TaskCancelled),
            Err(error) if error.is_panic() => Err(ClientError::TaskExecution(
                "agent task worker panicked".to_owned(),
            )),
            Err(error) => Err(ClientError::TaskExecution(format!(
                "agent task worker stopped unexpectedly: {error}"
            ))),
        };
        if let Err(error) = result {
            let terminal_status = if matches!(&error, ClientError::TaskCancelled) {
                TaskStatus::Cancelled
            } else {
                TaskStatus::Failed
            };
            if self
                .record_task_execution_failure(&task, error)
                .await
                .is_err()
            {
                let _ = self.finish_lane(&task, terminal_status).await;
            }
        }
        self.clear_task_control(task.session_id, task.task_id).await;
        completion.send_replace(true);
    }

    pub(super) async fn clear_task_control(&self, session_id: SessionId, task_id: TaskId) {
        let mut controls = self.task_controls.lock().await;
        if controls
            .get(&session_id)
            .is_some_and(|control| control.task_id == task_id)
        {
            controls.remove(&session_id);
        }
        self.provider_auth_waiters.lock().await.remove(&session_id);
    }

    pub(super) async fn run_agent_task(
        self: Arc<Self>,
        task: HostedAgentTask,
        control: AgentExecutionControl,
    ) -> Result<(), ClientError> {
        let started_at = Instant::now();
        let objective = prompt_from_payload(&task.payload);
        let explicit_task_contract = task
            .payload
            .get("task_contract")
            .is_some_and(|value| !value.is_null());
        let mut task_contract = task_contract_from_payload(&task.payload)?;
        let requested_network = match task.payload.get("allow_network") {
            None => false,
            Some(Value::Bool(allow_network)) => *allow_network,
            Some(_) => {
                return Err(ClientError::TaskExecution(
                    "allow_network must be a boolean".to_owned(),
                ));
            }
        };
        let yolo = match task.payload.get("yolo") {
            None => false,
            Some(Value::Bool(yolo)) => *yolo,
            Some(_) => {
                return Err(ClientError::TaskExecution(
                    "yolo must be a boolean".to_owned(),
                ));
            }
        };
        let max_elapsed_ms = task
            .payload
            .get("max_elapsed_ms")
            .and_then(Value::as_u64)
            .filter(|value| *value > 0);
        let defer_external_verification = task
            .payload
            .get("defer_external_verification")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let external_verifiers = task
            .payload
            .get("external_verifiers")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| {
                ClientError::TaskExecution(format!("invalid external verifier contract: {error}"))
            })?
            .unwrap_or_default();
        let workspace_root = self.execution_workspace_root()?;
        let policy = WorkspacePolicy::new(workspace_root.clone())
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        let tool_executor = self
            .build_tool_executor(policy, workspace_root.clone(), requested_network, yolo)
            .await?;
        let workspace_tool_names = tool_executor
            .registry()
            .contracts()
            .into_iter()
            .map(|contract| contract.tool_name.clone())
            .collect::<Vec<_>>();
        let provider_plan = self
            .resolve_provider_plan_with_auth(&task, &objective, control.cancellation_token())
            .await?;
        let MockProviderPlan {
            provider,
            fallback_provider,
            touched_code,
            workspace_tools_enabled,
            context_builder,
            provider_session_policy,
        } = provider_plan;
        let legacy_task = LegacyTaskAdapter::new(&task.payload, &objective);
        if !explicit_task_contract && (touched_code || legacy_task.requests_workspace_change()) {
            legacy_task.apply_to(&mut task_contract);
        }
        task_contract
            .validate()
            .map_err(ClientError::TaskExecution)?;
        let harness = AgentHarness::new(provider, context_builder, tool_executor)
            .with_provider_session_policy(provider_session_policy)
            .with_external_verifiers(external_verifiers)
            .require_os_sandbox_for_external_verifiers(
                task.payload
                    .get(EXTERNAL_VERIFIERS_REQUIRE_OS_SANDBOX_KEY)
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    && !yolo,
            );
        let harness = match fallback_provider {
            Some(fallback) => harness.with_fallback(fallback),
            None => harness,
        };
        let contributors = self
            .context_contributors_for_task(
                task.session_id,
                task.task_id,
                objective.clone(),
                task.payload.get("output_schema"),
            )
            .await?;
        let (trace_tx, mut trace_rx) = mpsc::unbounded_channel::<HostedTraceCommand>();
        let fact_recorder = CanonicalFactRecorder::new(self.clone(), task.clone());
        let trace_recorder = tokio::spawn(async move {
            while let Some(command) = trace_rx.recv().await {
                match command {
                    HostedTraceCommand::Event(event) => {
                        fact_recorder.commit(*event).await?;
                    }
                    HostedTraceCommand::Flush(sender) => {
                        let _ = sender.send(Ok(()));
                    }
                }
            }
            Ok::<(), ClientError>(())
        });
        let harness = if self.workspace_root.is_some() {
            harness.with_before_side_effect_recorder(Arc::new(HostedCheckpointRecorder {
                host: self.clone(),
                task: task.clone(),
                trace_sender: trace_tx.clone(),
            }))
        } else {
            harness
        };
        let run = AgentRun::new(AgentTaskRequest {
            session_id: task.session_id,
            task_id: task.task_id,
            turn_id: task.turn_id,
            objective: objective.clone(),
            completion_criteria: task_contract.completion_criteria.clone(),
            output_schema: task.payload.get("output_schema").cloned(),
            touched_code,
            contributors,
            tools: if workspace_tools_enabled {
                workspace_tool_names
            } else {
                Vec::new()
            },
        })
        .with_task_contract(task_contract)
        .with_deferred_external_verification(defer_external_verification);
        let run = match max_elapsed_ms {
            Some(max_elapsed_ms) => run.with_max_elapsed_ms(max_elapsed_ms),
            None => run,
        };
        let outcome = harness
            .execute(
                run,
                control,
                ChannelObservationSink {
                    sender: trace_tx.clone(),
                },
            )
            .await
            .map_err(|error| match error {
                AgentLoopError::Cancelled => ClientError::TaskCancelled,
                error => ClientError::TaskExecution(error.to_string()),
            });
        drop(harness);
        drop(trace_tx);
        trace_recorder
            .await
            .map_err(|error| ClientError::TaskExecution(error.to_string()))??;
        let outcome = outcome?;
        let terminal_status = task_status_from_loop_action(outcome.loop_decision.action);
        self.record_event(agent_event_for_turn(
            self.next_sequence_no(),
            &task,
            outcome.final_turn_id,
            RuntimeEventType::LoopDecided,
            RuntimeEventSource::Runtime,
            json!({
                "summary": outcome.loop_decision.reason,
                "record": outcome.loop_decision,
            }),
        ))
        .await?;
        let final_task = HostedAgentTask {
            turn_id: outcome.final_turn_id,
            ..task.clone()
        };
        let final_objective = outcome.verification.objective.clone();
        let task_outcome = super::execution_trace::task_outcome_with_external_verification(
            terminal_status,
            &outcome.verification,
            outcome.defer_external_verification,
        );
        self.finish_lane_with_outcome(&final_task, terminal_status, task_outcome)
            .await?;
        if let Err(error) = self
            .promote_successful_task_memory(
                &final_task,
                &final_objective,
                &outcome,
                terminal_status,
            )
            .await
        {
            self.record_post_task_governance_failure(
                &final_task,
                "memory_quarantine",
                false,
                &error,
            )
            .await;
        }
        self.schedule_task_evaluation_best_effort(
            &final_task,
            HostedTaskEvaluation {
                objective: &final_objective,
                task_status: terminal_status,
                verification: Some(outcome.verification.clone()),
                tool_reports: &outcome.tool_reports,
                failure_summary: Some(outcome.loop_decision.reason.clone()),
                latency: started_at.elapsed(),
            },
        )
        .await;
        Ok(())
    }

    pub(super) async fn resolve_provider_plan_with_auth(
        &self,
        task: &HostedAgentTask,
        objective: &str,
        cancellation: CancellationToken,
    ) -> Result<MockProviderPlan, ClientError> {
        let mut pending = None;
        loop {
            let plan = if self.force_mock_provider {
                isolated_mock_provider_plan(&task.payload, objective)
            } else {
                mock_provider_plan(
                    self.provider_config_paths.as_ref(),
                    &task.payload,
                    objective,
                )
            };
            match plan {
                Ok(plan) => {
                    if let Some((request_id, _)) = pending.take() {
                        self.provider_auth_waiters
                            .lock()
                            .await
                            .remove(&task.session_id);
                        self.record_provider_auth_resolved(
                            task,
                            request_id,
                            "provider configuration became available",
                        )
                        .await?;
                    }
                    return Ok(plan);
                }
                Err(ProviderError::NotConfigured { message }) => {
                    if pending.is_none() {
                        pending = Some(self.begin_provider_auth(task, message).await?);
                    }
                }
                Err(error) => return Err(ClientError::TaskExecution(error.to_string())),
            }

            let Some((_, receiver)) = pending.as_mut() else {
                unreachable!("provider auth wait is created for not-configured providers")
            };
            tokio::select! {
                _ = cancellation.cancelled() => {
                    self.provider_auth_waiters.lock().await.remove(&task.session_id);
                    return Err(ClientError::TaskCancelled);
                }
                resolution = receiver => {
                    match resolution {
                        Ok(ProviderAuthResolution::Submitted) => {
                            pending = None;
                        }
                        Ok(ProviderAuthResolution::Cancelled) | Err(_) => {
                            return Err(ClientError::TaskCancelled);
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }

    pub(super) async fn begin_provider_auth(
        &self,
        task: &HostedAgentTask,
        reason: String,
    ) -> Result<
        (
            ProviderAuthRequestId,
            oneshot::Receiver<ProviderAuthResolution>,
        ),
        ClientError,
    > {
        let request_id = ProviderAuthRequestId::new();
        let (sender, receiver) = oneshot::channel();
        self.provider_auth_waiters.lock().await.insert(
            task.session_id,
            PendingProviderAuth {
                request_id,
                resolution: sender,
            },
        );
        let mut transition = self
            .lane_manager
            .lock()
            .await
            .wait_for_authentication(task.session_id, self.next_sequence_no())?;
        transition.event.task_id = Some(task.task_id);
        transition.event.turn_id = Some(task.turn_id);
        transition.event.payload["summary"] = json!("provider authentication is required");
        transition.event.payload["request_id"] = json!(request_id);
        transition.event.payload["reason"] = json!(reason);
        transition.event.payload["supported_methods"] = json!(["api_key", "oauth"]);
        transition.event.payload["runtime_lane"] = json!(transition.lane);
        self.record_event(transition.event).await?;
        Ok((request_id, receiver))
    }

    pub(super) async fn record_provider_auth_resolved(
        &self,
        task: &HostedAgentTask,
        request_id: ProviderAuthRequestId,
        summary: &str,
    ) -> Result<(), ClientError> {
        let mut transition = self
            .lane_manager
            .lock()
            .await
            .authentication_resolved(task.session_id, self.next_sequence_no())?;
        transition.event.task_id = Some(task.task_id);
        transition.event.turn_id = Some(task.turn_id);
        transition.event.payload["summary"] = json!(summary);
        transition.event.payload["request_id"] = json!(request_id);
        transition.event.payload["runtime_lane"] = json!(transition.lane);
        self.record_event(transition.event).await
    }
}
