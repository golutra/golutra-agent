//! Context construction, task supervision, AgentLoop, and provider auth lifecycles.

use super::*;
use golutra_llm::ProviderGenerationConfig;
use tokio::{runtime::Handle, task::JoinHandle};

const ABNORMAL_RECORDER_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const HOST_SHUTDOWN_GRACE_TIMEOUT: Duration = Duration::from_millis(250);

struct AbortOnDropJoinHandle<T> {
    handle: Option<JoinHandle<T>>,
}

impl<T> AbortOnDropJoinHandle<T> {
    fn new(handle: JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    fn abort(&self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }

    async fn wait(&mut self) -> Result<T, tokio::task::JoinError> {
        self.handle
            .as_mut()
            .expect("guarded join handle must be present")
            .await
    }

    fn disarm(&mut self) {
        self.handle = None;
    }
}

impl<T> Drop for AbortOnDropJoinHandle<T> {
    fn drop(&mut self) {
        self.abort();
    }
}

struct ChannelObservationSink {
    sender: observation_recorder::ObservationSender,
    send_error: Arc<StdMutex<Option<observation_recorder::ObservationSendError>>>,
    cancellation: CancellationToken,
}

fn provider_max_tokens(settings: &ProviderTurnSettings) -> Option<u64> {
    settings
        .generation_config
        .as_ref()
        .and_then(|value| serde_json::from_value::<ProviderGenerationConfig>(value.clone()).ok())
        .and_then(|config| config.max_tokens)
}

impl RuntimeObservationSink for ChannelObservationSink {
    fn emit(&mut self, observation: RuntimeObservation) {
        let already_failed = self
            .send_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some();
        if already_failed {
            return;
        }
        if let Err(error) = self.sender.send(observation) {
            let mut send_error = self
                .send_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            send_error.get_or_insert(error);
            self.cancellation.cancel();
        }
    }
}

impl RuntimeHost {
    /// Cancel every host-owned task and delegation operation, then wait until
    /// their supervisors have released the in-process ownership maps. The
    /// process supervisor is shut down by `RuntimeHost::close` after this
    /// coordination step; keeping the two responsibilities separate prevents
    /// task bookkeeping from being coupled to a particular process backend.
    pub(super) async fn shutdown_active_work(&self) -> Result<(), ClientError> {
        let mut failures = Vec::new();

        // Commands acquire this mutex before they can create a task. A short
        // barrier closes the race where shutdown snapshots controls while a
        // Prompt/Create command is still about to insert one.
        let command_guard = tokio::time::timeout(
            TASK_CONTROL_CLEANUP_TIMEOUT,
            self.execution.command_mutex.lock(),
        )
        .await;
        if command_guard.is_err() {
            failures.push("command dispatcher did not quiesce during shutdown".to_owned());
        }
        let deadline = Instant::now() + TASK_CONTROL_CLEANUP_TIMEOUT;
        // Snapshot and signal active work while the dispatcher barrier is held. This prevents a
        // command that was waiting on the barrier from inserting a new task after the snapshot.
        // The guard is released before waiting for supervisors because delegated cleanup can
        // issue an internal archive command.
        self.cancel_active_work().await;
        drop(command_guard);
        if !self.wait_for_active_work_until(deadline).await {
            // A stuck worker must not keep a host-owned delegation task alive
            // indefinitely. Abort its worker, force-complete its waiter, and
            // let the global process shutdown below clean any child tools.
            let controls = self
                .execution
                .task_controls
                .lock()
                .await
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for control in controls {
                control.abort_handle.abort();
            }
            let operations = self
                .execution
                .delegation_operations
                .lock()
                .await
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for operation in operations {
                operation.force_stop();
            }
            self.execution.delegation_admissions.lock().await.clear();
            self.execution.delegation_operations.lock().await.clear();

            if !self
                .wait_for_active_work_until(Instant::now() + HOST_SHUTDOWN_GRACE_TIMEOUT)
                .await
            {
                failures.push("runtime task supervisors did not finish shutdown".to_owned());
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(ClientError::TaskExecution(failures.join("; ")))
        }
    }

    async fn cancel_active_work(&self) {
        let controls = self
            .execution
            .task_controls
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for control in controls {
            control.execution.cancel();
        }
        let operations = self
            .execution
            .delegation_operations
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for operation in operations {
            operation.cancel();
        }
    }

    async fn wait_for_active_work_until(&self, deadline: Instant) -> bool {
        loop {
            let tasks_active = !self.execution.task_controls.lock().await.is_empty();
            let operations_active = {
                let mut operations = self.execution.delegation_operations.lock().await;
                operations.retain(|_, operation| !operation.is_complete());
                !operations.is_empty()
            };
            if !tasks_active && !operations_active {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

pub(crate) struct HostedObservationRecorder {
    sender: observation_recorder::ObservationSender,
    worker: Option<tokio::task::JoinHandle<Result<(), ClientError>>>,
}

impl HostedObservationRecorder {
    pub(crate) fn spawn(host: Arc<RuntimeHost>, task: HostedAgentTask) -> Self {
        let (sender, receiver) = observation_recorder::channel();
        let fact_recorder = CanonicalFactRecorder::new(host, task);
        let worker = tokio::spawn(fact_recorder.drain(receiver));
        Self {
            sender,
            worker: Some(worker),
        }
    }

    pub(crate) fn sender(&self) -> observation_recorder::ObservationSender {
        self.sender.clone()
    }

    pub(crate) async fn close(self) -> Result<(), ClientError> {
        let mut this = self;
        let close_result = this
            .sender
            .close()
            .map_err(|error| ClientError::TaskExecution(error.to_string()));
        // Move the worker into an abort-on-drop guard before awaiting it. If this close future is
        // cancelled, the guard is dropped with the future and cannot silently detach the worker.
        let worker = this
            .worker
            .take()
            .expect("hosted observation recorder worker must be present");
        let mut worker = AbortOnDropJoinHandle::new(worker);
        let recorder_result = worker
            .wait()
            .await
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        worker.disarm();
        close_result?;
        recorder_result
    }
}

impl Drop for HostedObservationRecorder {
    fn drop(&mut self) {
        // Close ingress first so the drain worker can persist the queued lossless facts. The
        // cleanup task owns an abort-on-drop guard: runtime shutdown or task cancellation cannot
        // silently detach the worker and retain the host forever.
        let _ = self.sender.close();
        if let Some(worker) = self.worker.take() {
            let Ok(handle) = Handle::try_current() else {
                worker.abort();
                return;
            };
            handle.spawn(async move {
                let mut worker = AbortOnDropJoinHandle::new(worker);
                match tokio::time::timeout(ABNORMAL_RECORDER_DRAIN_TIMEOUT, worker.wait()).await {
                    Ok(_) => {}
                    Err(_) => {
                        worker.abort();
                        let _ = worker.wait().await;
                    }
                }
                worker.disarm();
            });
        }
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
            token_budget_hint: 192,
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

        let memory_store = self.storage.memory_store.clone();
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
            .storage
            .repositories
            .events
            .load_recent(session_id, None, None, MAX_HISTORY_SOURCE_EVENTS)
            .await?;
        let context_compaction = self
            .storage
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
        let history_events = effective_model_history_events(events.iter().filter(|event| {
            event.sequence_no > compacted_after && event.task_id != Some(current_task_id)
        }));
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
        self.execution
            .next_sequence_no
            .fetch_add(1, Ordering::SeqCst)
    }

    pub(super) fn scoped_idempotency_key(&self, idempotency_key: &str) -> String {
        format!("{}:{idempotency_key}", self.workspace_id)
    }

    pub(super) async fn spawn_agent_task(
        self: Arc<Self>,
        task: HostedAgentTask,
        session_lease: Option<Arc<File>>,
        pending_turns: Vec<PendingAgentTurn>,
        delegation: Option<delegation_policy::DelegationContext>,
    ) -> Result<(), ClientError> {
        if self.execution.shutdown.is_cancelled() {
            return self
                .fail_task_start(
                    &task,
                    ClientError::TaskExecution("runtime host is shutting down".to_owned()),
                )
                .await;
        }
        let provider_settings = ProviderTurnSettings::from_payload(&task.payload);
        let (execution, control, delegation) = match delegation {
            Some(context) => {
                let cancellation = context.cancellation().child_token();
                let (execution, control) =
                    agent_execution_channel_with_cancellation(32, cancellation);
                (execution, control, context)
            }
            None => {
                let cancellation = self.execution.shutdown.child_token();
                let (execution, control) =
                    agent_execution_channel_with_cancellation(32, cancellation);
                let max_cost_microusd =
                    match delegation_policy::cost_budget_from_payload(&task.payload) {
                        Ok(cost) => cost,
                        Err(error) => {
                            return self
                                .fail_task_start(
                                    &task,
                                    ClientError::TaskExecution(error.to_owned()),
                                )
                                .await;
                        }
                    };
                let context = delegation_policy::DelegationContext::root(
                    task.session_id,
                    task.payload.get("max_elapsed_ms").and_then(Value::as_u64),
                    provider_max_tokens(&provider_settings),
                    max_cost_microusd,
                    execution.cancellation_token(),
                );
                (execution, control, context)
            }
        };
        for pending_turn in pending_turns {
            if let Err(error) = execution.append_turn(pending_turn).await {
                return self
                    .fail_task_start(&task, ClientError::TaskExecution(error.to_string()))
                    .await;
            }
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
        let mut task_controls = self.execution.task_controls.lock().await;
        if self.execution.shutdown.is_cancelled() {
            worker.abort();
            drop(task_controls);
            return self
                .fail_task_start(
                    &task,
                    ClientError::TaskExecution("runtime host is shutting down".to_owned()),
                )
                .await;
        }
        task_controls.insert(
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
                provider_settings,
                execution,
                abort_handle,
                completion,
                delegation: Some(delegation),
                _session_lease: session_lease,
            },
        );
        drop(task_controls);
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

    /// Convert a failure before the supervisor owns the task into the same durable terminal
    /// facts used for ordinary worker failures. The task-created event is already persisted by
    /// the caller at this point, so returning directly would leave an active lane with no worker.
    async fn fail_task_start(
        self: &Arc<Self>,
        task: &HostedAgentTask,
        error: ClientError,
    ) -> Result<(), ClientError> {
        let existing_control = self
            .execution
            .task_controls
            .lock()
            .await
            .get(&task.session_id)
            .filter(|control| control.task_id == task.task_id)
            .cloned();
        if let Some(control) = existing_control {
            control.execution.cancel();
            control.abort_handle.abort();
            let mut completion = control.completion.clone();
            let _ =
                wait_for_task_control_cleanup(&mut completion, TASK_CONTROL_CLEANUP_TIMEOUT).await;
            return Err(error);
        }

        let failure = ClientError::TaskExecution(error.to_string());
        if self
            .record_task_execution_failure(task, failure)
            .await
            .is_err()
        {
            let _ = self.finish_lane(task, TaskStatus::Failed).await;
        }
        Err(error)
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
        let control = self
            .execution
            .task_controls
            .lock()
            .await
            .get(&task.session_id)
            .cloned();
        // A delegated context observes its parent's token, while a root context observes this
        // task's token. Freeze the parent state before cancelling local task resources so ordinary
        // root completion cannot be mistaken for delegated-parent cancellation.
        let delegated_parent_cancelled = task
            .payload
            .get(crate::delegation::DELEGATED_TASK_MARKER)
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && control
                .as_ref()
                .and_then(|control| control.delegation.as_ref())
                .is_some_and(|context| context.cancellation().is_cancelled());
        // Normal task completion must not cancel runtime-scoped background processes. Explicit
        // aborts already cancel the execution token before the worker exits; an error or raw
        // worker abort still needs the fallback cleanup so foreground resources cannot leak.
        if result.is_err()
            && let Some(control) = control.as_ref()
        {
            control.execution.cancel();
        }
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
        if delegated_parent_cancelled {
            let _ = delegation::cleanup_cancelled_delegated_task(&self, &task).await;
        }
        completion.send_replace(true);
    }

    pub(super) async fn clear_task_control(&self, session_id: SessionId, task_id: TaskId) {
        let mut controls = self.execution.task_controls.lock().await;
        if controls
            .get(&session_id)
            .is_some_and(|control| control.task_id == task_id)
        {
            controls.remove(&session_id);
        }
        drop(controls);
        self.execution
            .provider_auth_waiters
            .lock()
            .await
            .remove(&session_id);
        self.execution
            .delegation_admissions
            .lock()
            .await
            .remove(&session_id);
        self.execution
            .delegation_operations
            .lock()
            .await
            .retain(|_, operation| !operation.belongs_to(session_id) || !operation.is_complete());
    }

    pub(super) async fn cleanup_delegation_operation(
        &self,
        parent_session_id: SessionId,
        identity: &str,
        operation: &Arc<delegation::DelegationOperation>,
    ) {
        if self
            .execution
            .task_controls
            .lock()
            .await
            .contains_key(&parent_session_id)
        {
            // Completed operations remain available for idempotent retries while the parent
            // task is still alive. The parent cleanup path removes them once its control ends.
            return;
        }
        let mut operations = self.execution.delegation_operations.lock().await;
        if operations
            .get(identity)
            .is_some_and(|current| Arc::ptr_eq(current, operation))
        {
            operations.remove(identity);
        }
    }

    pub(super) async fn run_agent_task(
        self: Arc<Self>,
        task: HostedAgentTask,
        control: AgentExecutionControl,
    ) -> Result<(), ClientError> {
        let started_at = Instant::now();
        let objective = model_prompt_from_payload(&task.payload);
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
        let mut tool_executor = self
            .build_tool_executor(policy, workspace_root.clone(), requested_network, yolo)
            .await?;
        let delegated_task = task
            .payload
            .get(crate::delegation::DELEGATED_TASK_MARKER)
            .and_then(Value::as_bool)
            .unwrap_or(false);
        tool_executor = tool_executor
            .with_task_delegation_backend(Arc::new(
                crate::delegation::RuntimeTaskDelegationBackend::new(Arc::downgrade(&self)),
            ))
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        if delegated_task {
            tool_executor = tool_executor.without_tool("ask_user");
        }
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
        if !explicit_task_contract && defer_external_verification {
            task_contract.require_objective_validation = false;
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
        let trace_recorder = HostedObservationRecorder::spawn(self.clone(), task.clone());
        let trace_tx = trace_recorder.sender();
        let observation_send_error = Arc::new(StdMutex::new(None));
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
        let control_cancellation = control.cancellation_token();
        let outcome = harness
            .execute(
                run,
                control,
                ChannelObservationSink {
                    sender: trace_tx.clone(),
                    send_error: observation_send_error.clone(),
                    cancellation: control_cancellation.clone(),
                },
            )
            .await
            .map_err(|error| match error {
                AgentLoopError::Cancelled => ClientError::TaskCancelled,
                error => ClientError::TaskExecution(error.to_string()),
            });
        drop(harness);
        trace_recorder.close().await?;
        if let Some(error) = observation_send_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            return Err(ClientError::TaskExecution(format!(
                "trace observation delivery failed: {error}"
            )));
        }
        let outcome = outcome?;
        let terminal_status = if outcome.candidate_ready_for_external_verification {
            TaskStatus::Partial
        } else {
            task_status_from_loop_action(outcome.loop_decision.action)
        };
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
            outcome.candidate_ready_for_external_verification,
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
                        self.execution
                            .provider_auth_waiters
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
                    self.execution.provider_auth_waiters.lock().await.remove(&task.session_id);
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
        self.execution.provider_auth_waiters.lock().await.insert(
            task.session_id,
            PendingProviderAuth {
                request_id,
                resolution: sender,
            },
        );
        let mut transition = self
            .execution
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
            .execution
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

#[cfg(test)]
mod lifecycle_tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn cancelling_normal_close_aborts_the_owned_recorder_worker() {
        let (sender, receiver) = observation_recorder::channel();
        let worker_started = tokio::sync::oneshot::channel();
        let worker_started_sender = worker_started.0;
        let worker_started_receiver = worker_started.1;
        let worker_dropped = Arc::new(AtomicBool::new(false));
        let worker_probe = DropProbe(worker_dropped.clone());
        let worker = tokio::spawn(async move {
            let _probe = worker_probe;
            let _receiver = receiver;
            worker_started_sender
                .send(())
                .expect("worker start notification");
            std::future::pending::<Result<(), ClientError>>().await
        });
        worker_started_receiver.await.expect("worker must start");

        let recorder = HostedObservationRecorder {
            sender,
            worker: Some(worker),
        };
        let close_task = tokio::spawn(recorder.close());
        tokio::task::yield_now().await;
        close_task.abort();
        assert!(
            close_task
                .await
                .expect_err("cancelled close must be aborted")
                .is_cancelled()
        );

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !worker_dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled close must abort the recorder worker");
    }
}
