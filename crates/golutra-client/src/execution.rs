//! Context construction, task supervision, AgentLoop, and provider auth lifecycles.

use super::*;
use golutra_context::{estimate_message_tokens, estimate_tokens, fit_compaction_context_content};
use golutra_llm::{
    LlmProvider, PromptCacheScope, ProviderGenerationConfig, ProviderMessage, ProviderRequest,
    ProviderRole,
};
use tokio::{runtime::Handle, task::JoinHandle};

const ABNORMAL_RECORDER_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const HOST_SHUTDOWN_GRACE_TIMEOUT: Duration = Duration::from_millis(250);
const BACKGROUND_WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const MEMORY_CANDIDATE_MIN: usize = 8;
const MEMORY_CANDIDATE_MAX: usize = 64;
// memory 与 skill 是按需证据，使用绝对上限防止它们挤占持续增长的会话历史；
// 上限不是预留配额，未命中或未用完的 token 会全部回流给活动历史。
const MAX_MEMORY_CONTEXT_TOKENS: u64 = 1_024;
const MAX_SKILL_CONTEXT_TOKENS: u64 = 1_024;
// compaction 保存旧事实，最近尾部保存当前工作状态；绝对边界避免随大窗口膨胀，
// 同时让 summary 未用额度继续留给最近历史。
const MIN_RECENT_HISTORY_TOKENS: u64 = 1_024;
const MAX_WORKING_SUMMARY_TOKENS: u64 = 2_048;
const ACTIVE_PATH_COMPACTION_MAX_DEPTH: u32 = 65_536;
const MAX_RESUME_PROVIDER_REQUEST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RESUME_PROVIDER_MESSAGES: usize = 16_384;
// 精确 replay 只有在能放进当前窗口时才有价值；超出后强行保留会把
// 动态历史标成稳定前缀，阻止 compaction 并让每个后续请求重复支付全文。
const RESUME_REPLAY_HEADROOM_TOKENS: u64 = 1_024;
fn memory_candidate_limit(context_budget: u64) -> usize {
    if context_budget == 0 {
        return 0;
    }
    if context_budget == u64::MAX {
        return MEMORY_CANDIDATE_MAX;
    }
    usize::try_from(context_budget.saturating_div(32))
        .unwrap_or(MEMORY_CANDIDATE_MAX)
        .clamp(MEMORY_CANDIDATE_MIN, MEMORY_CANDIDATE_MAX)
}

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
    /// 读取最近一次主 provider 请求，只有完整 wire 和当前稳定前缀均匹配时
    /// 才用于跨进程 resume；任何损坏、过期或协议不一致都静默回退普通投影。
    async fn resume_provider_context(
        &self,
        session_id: SessionId,
        current_task_id: TaskId,
        objective: &str,
        provider: &ConfiguredProvider,
        cache_scope: &PromptCacheScope,
        context_budget: u64,
    ) -> Result<Option<AgentReplayContext>, ClientError> {
        if objective.trim().is_empty() {
            return Ok(None);
        }
        let Some(snapshot) = self
            .storage
            .repositories
            .artifacts
            .latest_context(session_id)
            .await?
        else {
            return Ok(None);
        };
        if snapshot.budget_snapshot.budget_policy == "auxiliary_compaction_summary"
            || snapshot.session_id != session_id
            || snapshot.provider_request_id.0.is_nil()
        {
            return Ok(None);
        }
        let Some(artifact_id) = snapshot.restricted_request_artifact_ref else {
            return Ok(None);
        };
        let Some(artifact) = self.storage.repositories.artifacts.get(artifact_id).await? else {
            return Ok(None);
        };
        if artifact.session_id != session_id
            || artifact.artifact_type != "provider_request_replay"
            || artifact.redaction_status != RedactionStatus::Raw
        {
            return Ok(None);
        }
        let Some(bytes) = self
            .storage
            .store
            .load_artifact_bytes_bounded(&artifact, MAX_RESUME_PROVIDER_REQUEST_BYTES)
            .await?
        else {
            return Ok(None);
        };
        let Ok(previous) = serde_json::from_slice::<ProviderRequest>(&bytes) else {
            return Ok(None);
        };
        let contract = provider.contract();
        let expected_cache_policy = provider.preferred_cache_policy();
        if previous.request_id != snapshot.provider_request_id
            || previous.session_id != Some(session_id)
            || previous.provider_id != contract.provider_id
            || previous.model_id != contract.model_id
            || previous.cache_policy != expected_cache_policy
            || previous
                .cache_scope
                .as_ref()
                .is_none_or(|scope| scope.key() != cache_scope.key())
            || previous.messages.len() > MAX_RESUME_PROVIDER_MESSAGES
            || !provider_transcript_is_replayable(&previous.messages)
        {
            return Ok(None);
        }

        let Some(messages) = resume_replay_messages_within_budget(
            previous.messages,
            previous.task_id,
            current_task_id,
            objective,
            context_budget,
        ) else {
            return Ok(None);
        };
        Ok(Some(AgentReplayContext::for_resume(
            messages,
            previous.tools,
        )))
    }

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
        // A queued governance request is represented by the durable terminal event and can be
        // recreated by the next host. Shutdown must not wait for a request that is being dropped
        // together with the worker.
        self.execution
            .post_task_schedule_pending
            .store(0, Ordering::SeqCst);
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
            self.signal_active_work_change();

            if !self
                .wait_for_active_work_until(Instant::now() + HOST_SHUTDOWN_GRACE_TIMEOUT)
                .await
            {
                failures.push("runtime task supervisors did not finish shutdown".to_owned());
            }
        }

        let post_task_worker = {
            let mut worker = self
                .execution
                .post_task_worker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            worker.take()
        };
        if let Some(worker) = post_task_worker {
            let mut worker = worker;
            match tokio::time::timeout(BACKGROUND_WORKER_SHUTDOWN_TIMEOUT, &mut worker).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) if error.is_cancelled() => {}
                Ok(Err(error)) => {
                    failures.push(format!("post-task worker stopped unexpectedly: {error}"))
                }
                Err(_) => {
                    worker.abort();
                    let _ = worker.await;
                    failures.push("post-task worker did not finish shutdown".to_owned());
                }
            }
        }

        self.execution
            .post_task_schedule_pending
            .store(0, Ordering::SeqCst);
        self.signal_active_work_change();

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
            let notification = self.execution.active_work_notify.notified();
            tokio::pin!(notification);
            // Register before inspecting the maps so a completion between the
            // check and await cannot leave shutdown asleep until its deadline.
            notification.as_mut().enable();
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
            tokio::select! {
                _ = &mut notification => {}
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    return false;
                }
            }
        }
    }
}

/// Build the exact provider transcript used for resume while it fits the real
/// context budget. No soft history cap is applied: preserving the full prior
/// wire keeps tool-call/result pairs intact and lets the next request reuse the
/// previous request as its prefix. Only a true context overflow falls back to
/// the ordinary activity-tree projection and compaction path.
pub(crate) fn resume_replay_messages_within_budget(
    mut messages: Vec<ProviderMessage>,
    previous_task_id: TaskId,
    current_task_id: TaskId,
    objective: &str,
    context_budget: u64,
) -> Option<Vec<ProviderMessage>> {
    let objective = objective.trim();
    if objective.is_empty() {
        return None;
    }
    let already_current_objective = messages
        .last()
        .is_some_and(|message| message.role == ProviderRole::User && message.content == objective);
    if previous_task_id != current_task_id || !already_current_objective {
        messages.push(ProviderMessage {
            role: ProviderRole::User,
            content: objective.to_owned(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            metadata: Default::default(),
        });
    }
    if messages.len() > MAX_RESUME_PROVIDER_MESSAGES {
        return None;
    }
    let replay_tokens = estimate_message_tokens(&messages);
    let replay_over_hard_limit = context_budget != u64::MAX
        && replay_tokens > context_budget.saturating_sub(RESUME_REPLAY_HEADROOM_TOKENS);
    (!replay_over_hard_limit).then_some(messages)
}

pub(crate) fn provider_transcript_is_replayable(messages: &[ProviderMessage]) -> bool {
    let mut pending_tool_calls = std::collections::HashSet::<String>::new();
    let mut seen_tool_call_ids = std::collections::HashSet::<String>::new();
    for message in messages {
        match message.role {
            ProviderRole::Assistant => {
                if !pending_tool_calls.is_empty() {
                    return false;
                }
                for call in &message.tool_calls {
                    if call.tool_call_id.trim().is_empty()
                        || !seen_tool_call_ids.insert(call.tool_call_id.clone())
                        || !pending_tool_calls.insert(call.tool_call_id.clone())
                    {
                        return false;
                    }
                }
            }
            ProviderRole::Tool => {
                let Some(tool_call_id) = message
                    .tool_call_id
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                else {
                    return false;
                };
                if !pending_tool_calls.remove(tool_call_id) {
                    return false;
                }
            }
            ProviderRole::System | ProviderRole::User => {
                if !pending_tool_calls.is_empty() {
                    return false;
                }
            }
        }
    }
    pending_tool_calls.is_empty()
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
    #[cfg(test)]
    pub(super) async fn context_contributors_for_task(
        &self,
        session_id: SessionId,
        current_task_id: TaskId,
        objective: String,
        output_schema: Option<&Value>,
    ) -> Result<Vec<ContextContributor>, ClientError> {
        self.context_contributors_for_task_with_budget(
            session_id,
            current_task_id,
            objective,
            output_schema,
            u64::MAX,
        )
        .await
    }

    pub(super) async fn context_contributors_for_task_with_budget(
        &self,
        session_id: SessionId,
        current_task_id: TaskId,
        objective: String,
        output_schema: Option<&Value>,
        context_budget: u64,
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
        let project_instructions = self.cached_project_instruction_bundle(&workspace_root);
        let skill_context =
            self.active_skill_context_with_budget(&objective, MAX_SKILL_CONTEXT_TOKENS);

        // 四路来源互不依赖且只读取已有状态，并行加载可把首次 provider 请求
        // 的准备时间收敛到最慢一路；MemoryRetrieved 仍在选择完成后有序落库。
        let memory_store = self.storage.memory_store.clone();
        let memory_query = objective.clone();
        let memory_limit = memory_candidate_limit(context_budget.min(MAX_MEMORY_CONTEXT_TOKENS));
        let memories = async move {
            run_blocking(move || {
                memory_store.retrieve(&memory_query, MemoryScope::Project, memory_limit)
            })
            .await?
            .map_err(ClientError::from)
        };
        let history = self.cached_history_events(session_id);
        let (project_instructions, skill_context, memories, history) =
            tokio::join!(project_instructions, skill_context, memories, history);
        let history = history?;
        if let Some(project_instructions) = project_instructions? {
            contributors.push(ContextContributor {
                name: "project_instructions".to_owned(),
                role: ProviderRole::System,
                content: project_instructions.content,
                token_budget_hint: 2_048,
                source_refs: project_instructions.source_refs,
            });
        }
        let objective_contributor = ContextContributor {
            name: "objective".to_owned(),
            role: ProviderRole::User,
            content: objective,
            token_budget_hint: 0,
            source_refs: vec![format!("task:{current_task_id}:objective")],
        };
        let output_schema_contributor = output_schema
            .filter(|value| !value.is_null())
            .map(|output_schema| {
                let schema = serde_json::to_string(output_schema)?;
                Ok::<_, ClientError>(ContextContributor {
                    name: "output_schema".to_owned(),
                    role: ProviderRole::User,
                    content: format!(
                        "The final assistant response must contain only JSON that validates against this JSON Schema. Do not wrap it in Markdown:\n{schema}"
                    ),
                    token_budget_hint: 0,
                    source_refs: vec![format!("task:{current_task_id}:output_schema")],
                })
            })
            .transpose()?;
        let mandatory_tokens = contributors
            .iter()
            .chain(std::iter::once(&objective_contributor))
            .chain(output_schema_contributor.iter())
            .fold(0_u64, |total, contributor| {
                total.saturating_add(estimate_tokens(&contributor.content))
            });
        let mut remaining_budget = context_budget.saturating_sub(mandatory_tokens);
        let recent_history_reserve = self.conversation_history_recent_reserve(
            session_id,
            current_task_id,
            history.as_ref(),
            remaining_budget,
        );
        let mut optional_budget = remaining_budget.saturating_sub(recent_history_reserve);

        let memories = select_memories_for_context_with_budget(
            memories?,
            optional_budget.min(MAX_MEMORY_CONTEXT_TOKENS),
        );
        let memory_contributor = (!memories.is_empty()).then(|| ContextContributor {
            name: "memory".to_owned(),
            // memory 是每个任务变化的证据，把它放在 user context，避免
            // rust-genai 将动态内容拼进静态 system prompt，破坏前缀缓存。
            role: ProviderRole::User,
            content: memory_context_with_budget(
                &memories,
                optional_budget.min(MAX_MEMORY_CONTEXT_TOKENS),
            ),
            token_budget_hint: 0,
            source_refs: memories
                .iter()
                .map(|memory| format!("memory:{}", memory.record.memory_id))
                .collect(),
        });
        if let Some(memory) = memory_contributor.as_ref() {
            let memory_tokens = estimate_tokens(&memory.content);
            optional_budget = optional_budget.saturating_sub(memory_tokens);
            remaining_budget = remaining_budget.saturating_sub(memory_tokens);
        }

        let skill_context = skill_context?
            .map(|content| {
                truncate_to_token_budget(&content, optional_budget.min(MAX_SKILL_CONTEXT_TOKENS))
            })
            .filter(|content| !content.is_empty());
        if let Some(skill_context) = skill_context.as_ref() {
            let skill_tokens = estimate_tokens(skill_context);
            remaining_budget = remaining_budget.saturating_sub(skill_tokens);
        }

        let history = self.conversation_history_projection(
            session_id,
            current_task_id,
            history.as_ref(),
            remaining_budget,
            recent_history_reserve,
        );
        // 没有命中时不写空的 durable 事件。空检索是正常路径，绕过 SQLite、
        // rollout 和通知写入可以把首个 provider 请求留在纯读取热路径上。
        if !memories.is_empty() {
            self.record_event(host_event(
                self.next_sequence_no(),
                session_id,
                Some(current_task_id),
                RuntimeEventType::MemoryRetrieved,
                RuntimeEventSource::Memory,
                json!({
                    "summary": format!("retrieved {} project memories", memories.len()),
                    "scope": "project",
                    // durable event 只保留可审计的索引元数据，记忆正文仍只进入当前模型上下文。
                    "retrieved": memories.iter().map(|memory| json!({
                        "memory_id": memory.record.memory_id,
                        "relevance_score": memory.relevance_score,
                        "scope": memory.record.scope,
                        "confidence": memory.record.confidence,
                        "source_task_id": memory.record.source_task_id,
                        "evidence_ids": memory.record.evidence_ids,
                        "matched_term_count": memory.matched_terms.len(),
                    })).collect::<Vec<_>>(),
                }),
            ))
            .await?;
        }
        // 历史是会话中唯一持续增长的前缀；放在每任务变化的 memory 之前，
        // 让 provider 可以复用从 system/project 到历史的连续 cache prefix。
        contributors.extend(history);
        if let Some(memory) = memory_contributor {
            contributors.push(memory);
        }

        contributors.push(objective_contributor);

        // skill 选择依赖当前目标，因此保留在动态 user 段，避免无关任务变化
        // 破坏 system/project 稳定前缀。
        if let Some(skill_context) = skill_context {
            contributors.push(ContextContributor {
                name: "project_skills".to_owned(),
                role: ProviderRole::User,
                content: skill_context,
                token_budget_hint: 1_024,
                source_refs: vec!["runtime:active_skills".to_owned()],
            });
        }

        // schema 也是任务局部内容，放在动态段末尾以保护可复用前缀。
        if let Some(output_schema) = output_schema_contributor {
            contributors.push(output_schema);
        }

        Ok(contributors)
    }

    /// 最近历史保存当前工作状态，必须先于 memory/skill 获得有限预算。
    /// 这里只投影最多一个 reserve 的尾部，避免为预算决策扫描或复制整段历史。
    fn conversation_history_recent_reserve(
        &self,
        session_id: SessionId,
        current_task_id: TaskId,
        cached_events: &[RuntimeEvent],
        available_budget: u64,
    ) -> u64 {
        let reserve_cap = available_budget.min(MIN_RECENT_HISTORY_TOKENS);
        if reserve_cap == 0 {
            return 0;
        }
        let compacted_after = self
            .execution
            .context_resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .history_compaction(session_id)
            .map(|(sequence_no, _)| sequence_no)
            .unwrap_or_default();
        let cached_facts = self
            .execution
            .context_resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .history_facts(session_id);
        if let Some(facts) = cached_facts.as_ref() {
            let mut reserved = 0_u64;
            for fact in facts.iter().rev().filter(|fact| {
                fact.sequence_no > compacted_after && fact.task_id != Some(current_task_id)
            }) {
                reserved = reserved
                    .saturating_add(estimate_tokens(&fact.contributor.content))
                    .min(reserve_cap);
                if reserved == reserve_cap {
                    break;
                }
            }
            return reserved;
        }

        history_contributors_with_budget(
            cached_events.iter().filter(|event| {
                event.sequence_no > compacted_after && event.task_id != Some(current_task_id)
            }),
            reserve_cap,
        )
        .iter()
        .fold(0_u64, |total, contributor| {
            total.saturating_add(estimate_tokens(&contributor.content))
        })
        .min(reserve_cap)
    }

    async fn cached_project_instruction_bundle(
        &self,
        workspace_root: &Path,
    ) -> Result<Option<ProjectInstructionBundle>, ClientError> {
        let canonical_root = workspace_root
            .canonicalize()
            .map_err(|error| ClientError::Io(format!("{}: {error}", workspace_root.display())))?;
        if let Some(bundle) = self
            .execution
            .context_resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .project_instructions
            .as_ref()
            .filter(|cached| {
                cached.root == canonical_root
                    && cached.checked_at.elapsed()
                        < super::context::PROJECT_INSTRUCTIONS_REFRESH_INTERVAL
            })
            .map(|cached| cached.bundle.clone())
        {
            return Ok(bundle);
        }
        let (_, fingerprint) = project_instruction_fingerprint(&canonical_root).await?;
        {
            let mut resources = self
                .execution
                .context_resources
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = resources
                .project_instructions
                .as_mut()
                .filter(|cached| cached.root == canonical_root && cached.fingerprint == fingerprint)
            {
                cached.checked_at = Instant::now();
                return Ok(cached.bundle.clone());
            }
        }
        let bundle = load_project_instruction_bundle(&canonical_root).await?;
        self.execution
            .context_resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .project_instructions = Some(CachedProjectInstructions {
            root: canonical_root,
            fingerprint,
            bundle: bundle.clone(),
            checked_at: Instant::now(),
        });
        Ok(bundle)
    }

    /// 只从活动因果路径读取 token 预算内的历史。兄弟分支不会进入模型；
    /// 叶节点作为缓存键，parent 不匹配时重新读取完整活动路径。
    fn conversation_history_projection(
        &self,
        session_id: SessionId,
        current_task_id: TaskId,
        cached_events: &[RuntimeEvent],
        token_budget: u64,
        recent_history_reserve: u64,
    ) -> Vec<ContextContributor> {
        let compaction = self
            .execution
            .context_resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .history_compaction(session_id)
            .or_else(|| {
                cached_events
                    .iter()
                    .rev()
                    .find_map(context_compaction_from_event)
            });
        let compacted_after = compaction
            .as_ref()
            .map(|(sequence_no, _)| *sequence_no)
            .unwrap_or_default();
        let cached_facts = self
            .execution
            .context_resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .history_facts(session_id);
        let history_facts = cached_facts.as_ref().map(|facts| {
            facts
                .iter()
                .filter(|fact| {
                    fact.sequence_no > compacted_after && fact.task_id != Some(current_task_id)
                })
                .collect::<Vec<_>>()
        });
        let history_events = history_facts.is_none().then(|| {
            cached_events
                .iter()
                .filter(|event| {
                    event.sequence_no > compacted_after && event.task_id != Some(current_task_id)
                })
                .collect::<Vec<_>>()
        });
        let recent_reserve = recent_history_reserve.min(token_budget);
        let summary_budget = token_budget
            .saturating_sub(recent_reserve)
            .min(MAX_WORKING_SUMMARY_TOKENS);
        let mut contributors = Vec::new();
        let mut summary_tokens = 0;
        if let Some((sequence_no, summary)) = compaction
            && let Some(content) = fit_compaction_context_content(&summary, summary_budget)
        {
            summary_tokens = estimate_tokens(&content);
            contributors.push(ContextContributor {
                name: "working_summary".to_owned(),
                role: ProviderRole::User,
                content,
                token_budget_hint: 0,
                source_refs: vec![format!("event-sequence:{sequence_no}")],
            });
        }
        let history_budget = token_budget.saturating_sub(summary_tokens);
        let history = if let Some(facts) = history_facts {
            history_contributors_from_cached_facts(facts, history_budget)
        } else {
            // 缓存被淘汰时直接从同一 durable 快照投影，保持压力下语义一致。
            history_contributors_with_budget(history_events.into_iter().flatten(), history_budget)
        };
        contributors.extend(history);
        contributors
    }

    /// 每个叶节点只读取一次活动因果路径。短周期刷新用于发现其他 runtime
    /// 的写入；叶节点未变化时不扫描整段 session，也不重建已解析事实。
    pub(super) async fn cached_history_events(
        &self,
        session_id: SessionId,
    ) -> Result<Arc<Vec<RuntimeEvent>>, ClientError> {
        let events_repository = self.storage.repositories.events.clone();
        loop {
            // event writer 只作为短暂的 durable 屏障，不覆盖 SQLite 路径查询，
            // 避免长会话首次读取阻塞流式事件落库。
            let (local_leaf, cached) = {
                let _snapshot = self.execution.event_writer.lock().await;
                let local_leaf = self
                    .execution
                    .causal_ledger
                    .lock()
                    .await
                    .context_head(session_id);
                let cached = self
                    .execution
                    .context_resources
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .history(session_id);
                (local_leaf, cached)
            };
            let cached_leaf = cached.as_ref().and_then(|entry| entry.active_leaf_event_id);
            let cache_dirty = cached.as_ref().is_some_and(|entry| entry.reload_required);
            let refresh_due = cached
                .as_ref()
                .is_none_or(ContextResourceCache::history_refresh_due);
            if let Some(entry) = cached.as_ref()
                && !cache_dirty
                && !refresh_due
                && local_leaf == cached_leaf
            {
                return Ok(Arc::clone(&entry.events));
            }

            // 本地 ledger 已指向 durable 事实时直接使用；只有 ledger 未推进或
            // 到达外部刷新窗口时才查询 SQLite 的最新模型历史事件。
            let latest_event = if local_leaf == cached_leaf || local_leaf.is_none() {
                events_repository.latest_model_history(session_id).await?
            } else {
                None
            };
            let leaf_event_id = latest_event.as_ref().map(|event| event.id).or(local_leaf);

            if cached.is_some()
                && !cache_dirty
                && cached_leaf == leaf_event_id
                && local_leaf == leaf_event_id
            {
                let mut resources = self
                    .execution
                    .context_resources
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(events) = resources.mark_history_checked(session_id) {
                    return Ok(events);
                }
                continue;
            }

            let mut history = if let Some(leaf_event_id) = leaf_event_id {
                events_repository
                    .active_context_window(
                        session_id,
                        leaf_event_id,
                        u32::try_from(MAX_CACHED_HISTORY_EVENTS).unwrap_or(u32::MAX),
                        ACTIVE_PATH_COMPACTION_MAX_DEPTH,
                    )
                    .await?
                    .into_iter()
                    .filter(is_history_cache_event)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };

            history.sort_by_key(|event| event.sequence_no);
            history.dedup_by(|left, right| left.sequence_no == right.sequence_no);
            bound_cached_history(&mut history);
            let history = Arc::new(history);

            // 发布前再次经过 writer 屏障。查询期间若有本地历史事件落库，
            // 重新读取新路径；若只发现外部写入，则同步推进本地上下文叶节点。
            let _publish = self.execution.event_writer.lock().await;
            let mut ledger = self.execution.causal_ledger.lock().await;
            if ledger.context_head(session_id) != local_leaf {
                continue;
            }
            if let Some(event) = latest_event.as_ref() {
                ledger.seed_context_head(event);
            }
            if ledger.context_head(session_id) != leaf_event_id {
                continue;
            }
            drop(ledger);
            self.execution
                .context_resources
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert_history(session_id, Arc::clone(&history), leaf_event_id);
            return Ok(history);
        }
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
        pending_turns: Vec<ConfiguredPendingAgentTurn>,
        delegation: Option<DelegationContextSeed>,
        governor_usage: AgentGovernorUsage,
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
            Some(DelegationContextSeed::Live(context)) => {
                let cancellation = context.cancellation().child_token();
                let (execution, control) =
                    agent_execution_channel_with_cancellation(32, cancellation);
                (execution, control, context)
            }
            Some(DelegationContextSeed::Recovered(recovered)) => {
                let cancellation = self.execution.shutdown.child_token();
                let (execution, control) =
                    agent_execution_channel_with_cancellation(32, cancellation);
                let context = match delegation_policy::DelegationContext::recovered(
                    recovered,
                    chrono::Utc::now(),
                    execution.cancellation_token(),
                ) {
                    Ok(context) => context,
                    Err(error) => {
                        return self
                            .fail_task_start(&task, ClientError::TaskExecution(error.to_owned()))
                            .await;
                    }
                };
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
            if let Err(error) = execution.append_configured_turn(pending_turn).await {
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
            worker_host
                .run_agent_task(worker_task, control, governor_usage)
                .await
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
        self.signal_active_work_change();
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
        self.signal_active_work_change();
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
        drop(operations);
        self.signal_active_work_change();
    }

    pub(super) async fn run_agent_task(
        self: Arc<Self>,
        task: HostedAgentTask,
        control: AgentExecutionControl,
        governor_usage: AgentGovernorUsage,
    ) -> Result<(), ClientError> {
        let started_at = Instant::now();
        let objective = model_prompt_from_payload(&task.payload);
        let execution_mode = execution_mode_from_payload(&task.payload)
            .map_err(|error| ClientError::TaskExecution(error.to_owned()))?;
        let tool_profile = tool_profile_from_payload(&task.payload)
            .map_err(|error| ClientError::TaskExecution(error.to_owned()))?;
        let has_explicit_task_contract = explicit_task_contract(&task.payload);
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
        let prompt_cache_scope = self
            .prompt_cache_scope(task.session_id, delegated_task)
            .await?;
        tool_executor = tool_executor
            .with_task_delegation_backend(Arc::new(
                crate::delegation::RuntimeTaskDelegationBackend::new(Arc::downgrade(&self)),
            ))
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        if delegated_task {
            tool_executor = tool_executor.without_tool("subagent");
        }
        let workspace_tool_names = tool_executor
            .registry()
            .provider_contracts()
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
        let context_budget = context_builder.budget_limit();
        let resume_context = self
            .resume_provider_context(
                task.session_id,
                task.task_id,
                &objective,
                &provider,
                &prompt_cache_scope,
                context_budget,
            )
            .await?;
        let legacy_task = LegacyTaskAdapter::new(&task.payload, &objective);
        if !has_explicit_task_contract && should_apply_legacy_adapter(&task.payload, execution_mode)
        {
            legacy_task.apply_to(&mut task_contract);
        }
        if !has_explicit_task_contract
            && should_apply_legacy_adapter(&task.payload, execution_mode)
            && defer_external_verification
        {
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
            .context_contributors_for_task_with_budget(
                task.session_id,
                task.task_id,
                objective.clone(),
                task.payload.get("output_schema"),
                context_budget,
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
        let run = ConfiguredAgentRun::new(AgentTaskRequest {
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
        .with_cache_scope(prompt_cache_scope)
        .with_execution_mode(execution_mode.explicit())
        .with_tool_profile(tool_profile)
        .with_deferred_external_verification(defer_external_verification)
        .with_governor_usage(governor_usage);
        let run = match resume_context {
            Some(replay_context) => run.with_replay_context(replay_context),
            None => run,
        };
        let run = match max_elapsed_ms {
            Some(max_elapsed_ms) => run.with_max_elapsed_ms(max_elapsed_ms),
            None => run,
        };
        let control_cancellation = control.cancellation_token();
        let outcome = harness
            .execute_configured(
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
        // 记忆隔离和评估都在 host-owned post-task worker 中执行；终态事件提交后立即
        // 释放执行 worker，避免非用户关键的治理 IO 拉长端到端尾延迟。
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
                let provider_config_paths = self.provider_config_paths.clone();
                let payload = task.payload.clone();
                let objective = objective.to_owned();
                let provider_route_cache = Arc::clone(&self.execution.provider_route_cache);
                run_blocking(move || {
                    let mut cache = provider_route_cache
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    cached_mock_provider_plan(
                        &mut cache,
                        provider_config_paths.as_ref(),
                        &payload,
                        &objective,
                    )
                })
                .await?
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
