//! Durable background facts and model delivery. Execution remains owned by the tool supervisor.

use std::sync::Weak;

use golutra_agent_core::{
    BackgroundBatch, BackgroundBatchId, BackgroundResult, BackgroundResultId, BackgroundScope,
    BackgroundTaskId, BackgroundTaskRecord, BackgroundTaskStatus, MAX_BACKGROUND_PENDING_RESULTS,
    MAX_BACKGROUND_SUMMARY_BYTES, ToolCallId,
};
use golutra_agent_runtime::BackgroundDeliveryMode;
use golutra_agent_tools::{
    BackgroundToolObserver, ToolError, ToolExecutionReport, background_report_status,
};

use super::*;

const MAX_INLINE_FACT_BYTES: usize = 32 * 1024;
const MAX_INLINE_EXCERPT_BYTES: usize = 16 * 1024;
const MAX_BACKGROUND_LIFETIME_MS: i64 = 30 * 60 * 1000;

#[derive(Debug, Default)]
struct BatchState {
    batches: HashMap<TurnId, BackgroundBatch>,
    generation: u64,
    // A foreground turn can end while an older turn's operations are still alive.
    // This independent owner lease keeps another host from recovering them as orphans.
    owner_lease: Option<Arc<ExecutionLease>>,
}

#[derive(Debug)]
pub(super) struct HostedBackgroundObserver {
    host: Weak<RuntimeHost>,
    task: HostedAgentTask,
    state: Mutex<BatchState>,
}

impl HostedBackgroundObserver {
    pub(super) fn new(host: &Arc<RuntimeHost>, task: HostedAgentTask) -> Arc<Self> {
        Arc::new(Self {
            host: Arc::downgrade(host),
            task,
            state: Mutex::new(BatchState::default()),
        })
    }

    fn host(&self) -> Result<Arc<RuntimeHost>, ToolError> {
        self.host
            .upgrade()
            .ok_or_else(|| tool_error("background host is unavailable"))
    }

    fn scope(
        &self,
        host: &RuntimeHost,
        request: &ToolRequest,
    ) -> Result<BackgroundScope, ToolError> {
        if request.session_id != self.task.session_id {
            return Err(tool_error(
                "background operation belongs to another session",
            ));
        }
        Ok(BackgroundScope {
            workspace_id: host.workspace_id,
            session_id: request.session_id,
            task_id: self.task.task_id,
            turn_id: request.turn_id.unwrap_or(self.task.turn_id),
        })
    }

    async fn deliver(&self, host: &RuntimeHost, result: BackgroundResult) -> Result<(), ToolError> {
        let control = host
            .execution
            .task_controls
            .lock()
            .await
            .get(&result.scope.session_id)
            .cloned();
        if let Some(control) = control {
            // A closed loop is a normal race. Its successor loads the still-pending durable result.
            match control
                .execution
                .enqueue_background_results(vec![result], BackgroundDeliveryMode::Passive)
            {
                Ok(())
                | Err(AgentLoopError::PendingTurnQueueClosed | AgentLoopError::Cancelled) => {}
                Err(error) => return Err(tool_error(error)),
            }
        }
        Ok(())
    }

    async fn wake_if_ready(
        &self,
        host: &RuntimeHost,
        batch: &BackgroundBatch,
    ) -> Result<(), ToolError> {
        let repository = host.storage.store.background();
        let Some(control) = host
            .execution
            .task_controls
            .lock()
            .await
            .get(&batch.scope.session_id)
            .cloned()
        else {
            return Ok(());
        };
        if control.task_id != batch.scope.task_id
            || control.execution.cancellation_token().is_cancelled()
        {
            return Ok(());
        }
        // Stop may wait for this worker while holding command admission. Cancellation
        // must break that wait rather than deadlocking finalization behind the command.
        let cancellation = control.execution.cancellation_token();
        let _admission = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            _ = host.execution.shutdown.cancelled() => return Ok(()),
            guard = host.execution.command_mutex.lock() => guard,
        };
        let Some(claim) = repository
            .claim_batch_wake(
                batch.batch_id,
                batch.generation,
                &host.instance_id,
                chrono::Utc::now(),
                chrono::Duration::seconds(30),
            )
            .await
            .map_err(tool_error)?
        else {
            return Ok(());
        };
        let results = repository
            .pending_results(
                host.workspace_id,
                batch.scope.session_id,
                MAX_BACKGROUND_PENDING_RESULTS,
            )
            .await
            .map_err(tool_error)?
            .into_iter()
            .filter(|result| result.batch_id == batch.batch_id)
            .collect::<Vec<_>>();
        if results.is_empty() {
            repository.release_wake(&claim).await.map_err(tool_error)?;
            return Ok(());
        }
        match control
            .execution
            .enqueue_background_results(results, BackgroundDeliveryMode::ContinueAfterBatch)
        {
            Ok(()) => {
                repository.commit_wake(&claim).await.map_err(tool_error)?;
            }
            Err(AgentLoopError::PendingTurnQueueClosed) => {
                repository.release_wake(&claim).await.map_err(tool_error)?;
            }
            Err(error) => {
                repository.release_wake(&claim).await.map_err(tool_error)?;
                return Err(tool_error(error));
            }
        }
        Ok(())
    }

    async fn persist_result(
        &self,
        host: &RuntimeHost,
        task: &BackgroundTaskRecord,
        report: &ToolExecutionReport,
    ) -> Result<BackgroundResult, ToolError> {
        let repository = host.storage.store.background();
        if let Some(result) = repository
            .result_for_task(task.background_task_id)
            .await
            .map_err(tool_error)?
        {
            return Ok(result);
        }
        // The terminal audit bundle commits tool artifacts and evidence atomically below,
        // before the inbox result can expose their references to a model.
        let result_id =
            BackgroundResultId(Uuid::new_v5(&task.tool_call_id.0, b"background-result"));
        let artifact_id = ArtifactId(Uuid::new_v5(
            &task.tool_call_id.0,
            b"background-full-result",
        ));
        let bytes = serde_json::to_vec(&report.envelope).map_err(tool_error)?;
        let artifact = ArtifactRecord {
            artifact_id,
            session_id: task.scope.session_id,
            turn_id: Some(task.scope.turn_id),
            tool_call_id: Some(task.tool_call_id),
            artifact_type: "background_result".into(),
            uri: format!("artifact://background/{artifact_id}"),
            checksum: format!("sha256:{:x}", Sha256::digest(&bytes)),
            size_bytes: bytes.len() as u64,
            created_at: task.created_at,
            producer: "background-runtime".into(),
            redaction_status: RedactionStatus::Raw,
            retention_policy: "debug_30d".into(),
            provenance_refs: Vec::new(),
        };
        host.storage
            .repositories
            .artifacts
            .store(&artifact, &bytes)
            .await
            .map_err(tool_error)?;
        let mut output = report.envelope.clone();
        output.summary = bounded(&output.summary, MAX_BACKGROUND_SUMMARY_BYTES);
        output.model_visible_excerpt = output
            .model_visible_excerpt
            .as_deref()
            .map(|text| bounded(text, MAX_INLINE_EXCERPT_BYTES));
        output.risk = bounded(&output.risk, 1024);
        output.verification_hint = output
            .verification_hint
            .as_deref()
            .map(|text| bounded(text, 2048));
        output.evidence_refs.truncate(32);
        if serde_json::to_vec(&output.structured_facts)
            .map_err(tool_error)?
            .len()
            > MAX_INLINE_FACT_BYTES
        {
            // Reuse foreground projection priorities: a large page body or log
            // must not erase exit status, browser ownership, or recovery controls.
            // The original unabridged envelope is already stored in the artifact.
            let mut facts_only = output.clone();
            facts_only.model_visible_excerpt = None;
            let projected = golutra_agent_tools::model_visible_tool_result_with_limit(
                &facts_only,
                MAX_INLINE_FACT_BYTES / 2,
            );
            let mut projected: Value = serde_json::from_str(&projected).map_err(tool_error)?;
            output.structured_facts = projected["structured_facts"].take();
            if !output.structured_facts.is_object() {
                output.structured_facts = json!({"tool_facts":output.structured_facts});
            }
            output.structured_facts["inline_truncated"] = json!(true);
            output.structured_facts["full_result_artifact_ref"] = json!(artifact_id);
        }
        // Preserve a bounded image manifest even when the full report is no longer
        // resident. Recovery can then distinguish retained artifacts from pixels
        // actually supplied to the model instead of silently dropping screenshots.
        let image_artifacts = report
            .artifacts
            .iter()
            .filter_map(|artifact| {
                artifact.artifact_type.strip_prefix("tool_image:").map(|mime| {
                    json!({"artifact_id":artifact.artifact_id,"mime_type":bounded(mime, 128)})
                })
            })
            .take(8)
            .collect::<Vec<_>>();
        if !image_artifacts.is_empty() {
            if !output.structured_facts.is_object() {
                output.structured_facts = json!({"tool_facts":output.structured_facts});
            }
            output.structured_facts["background_image_count"] = json!(
                report
                    .artifacts
                    .iter()
                    .filter(|artifact| artifact.artifact_type.starts_with("tool_image:"))
                    .count()
            );
            output.structured_facts["background_image_artifacts"] = json!(image_artifacts);
        }
        let mut artifact_refs = vec![artifact_id.to_string()];
        artifact_refs.extend(
            report
                .artifacts
                .iter()
                .take(62)
                .map(|artifact| artifact.artifact_id.to_string()),
        );
        if let Some(raw) = output.raw_artifact_ref {
            artifact_refs.push(raw.to_string());
        }
        artifact_refs.sort();
        artifact_refs.dedup();
        let result = BackgroundResult {
            result_id,
            background_task_id: task.background_task_id,
            batch_id: task.batch_id,
            scope: task.scope,
            tool_call_id: task.tool_call_id,
            tool_name: task.tool_name.clone(),
            status: background_report_status(report),
            summary: output.summary.clone(),
            output,
            artifact_refs,
            created_at: chrono::Utc::now(),
        };
        let origin = HostedAgentTask {
            turn_id: task.scope.turn_id,
            ..self.task.clone()
        };
        host.record_background_tool_report(&origin, report, &result)
            .await
            .map_err(tool_error)?;
        repository
            .result_for_task(task.background_task_id)
            .await
            .map_err(tool_error)?
            .ok_or_else(|| tool_error("background result was not persisted"))
    }
}

#[async_trait]
impl BackgroundToolObserver for HostedBackgroundObserver {
    async fn admit(&self, request: &ToolRequest) -> Result<(), ToolError> {
        let host = self.host()?;
        if host.execution.shutdown.is_cancelled() {
            return Err(tool_error("background host is stopping"));
        }
        let scope = self.scope(&host, request)?;
        let mut state = self.state.lock().await;
        if state.owner_lease.is_none() {
            state.owner_lease = match host
                .try_acquire_background_lease(scope.session_id, scope.task_id)
                .map_err(tool_error)?
            {
                SessionLeaseAttempt::Acquired(lease) => lease,
                SessionLeaseAttempt::Busy => {
                    return Err(tool_error("background task is owned by another host"));
                }
            };
        }
        let repository = host.storage.store.background();
        if repository
            .find_task_by_tool_call(host.workspace_id, request.session_id, request.tool_call_id)
            .await
            .map_err(tool_error)?
            .is_some()
        {
            return Err(tool_error(
                "background operation was already admitted; inspect its result before retrying",
            ));
        }
        let existing = if let Some(batch) = state.batches.get(&scope.turn_id) {
            repository
                .batch(batch.batch_id)
                .await
                .map_err(tool_error)?
                .filter(|batch| !batch.sealed)
        } else {
            None
        };
        let now = chrono::Utc::now();
        let batch = if let Some(batch) = existing {
            batch
        } else {
            state.generation = state.generation.saturating_add(1);
            let lifetime_ms = self
                .task
                .payload
                .get("max_elapsed_ms")
                .and_then(Value::as_u64)
                .and_then(|value| i64::try_from(value).ok())
                .unwrap_or(MAX_BACKGROUND_LIFETIME_MS)
                .clamp(1, MAX_BACKGROUND_LIFETIME_MS);
            let batch = BackgroundBatch {
                batch_id: BackgroundBatchId::new(),
                scope,
                generation: state.generation,
                created_at: now,
                deadline_at: now + chrono::Duration::milliseconds(lifetime_ms),
                sealed: false,
            };
            repository.create_batch(&batch).await.map_err(tool_error)?;
            state.batches.insert(scope.turn_id, batch.clone());
            batch
        };
        let task = BackgroundTaskRecord {
            background_task_id: BackgroundTaskId(request.tool_call_id.0),
            batch_id: batch.batch_id,
            scope,
            tool_call_id: request.tool_call_id,
            tool_name: request.tool_name.clone(),
            backend: if request.tool_name.starts_with("browser_") {
                "browser"
            } else if request.tool_name == "subagent" {
                "subagent"
            } else {
                "process"
            }
            .into(),
            status: BackgroundTaskStatus::Queued,
            created_at: now,
            deadline_at: batch.deadline_at,
        };
        if !repository.register_task(&task).await.map_err(tool_error)? {
            return Err(tool_error("background operation is already registered"));
        }
        repository
            .start_task(task.background_task_id)
            .await
            .map_err(tool_error)?;
        Ok(())
    }

    async fn complete(
        &self,
        request: &ToolRequest,
        report: &ToolExecutionReport,
    ) -> Result<(), ToolError> {
        let host = self.host()?;
        self.scope(&host, request)?;
        let _state = self.state.lock().await;
        let repository = host.storage.store.background();
        let task = repository
            .find_task_by_tool_call(host.workspace_id, request.session_id, request.tool_call_id)
            .await
            .map_err(tool_error)?
            .ok_or_else(|| tool_error("background completion has no admission"))?;
        let result = self.persist_result(&host, &task, report).await?;
        self.deliver(&host, result).await?;
        if let Some(batch) = repository.batch(task.batch_id).await.map_err(tool_error)? {
            self.wake_if_ready(&host, &batch).await?;
        }
        if host
            .execution
            .task_controls
            .lock()
            .await
            .get(&request.session_id)
            .is_none_or(|control| control.task_id != task.scope.task_id)
        {
            // The source task has ended. The durable envelope and artifact manifest
            // suffice for a later user turn; its full verification report is no longer live.
            host.execution
                .background_supervisor
                .take_completed_report(request.session_id, request.tool_call_id);
        }
        Ok(())
    }

    async fn seal(&self, session_id: SessionId, turn_id: Option<TurnId>) -> Result<(), ToolError> {
        if session_id != self.task.session_id {
            return Err(tool_error("background seal belongs to another session"));
        }
        let host = self.host()?;
        let mut state = self.state.lock().await;
        let Some(batch) = state.batches.get_mut(&turn_id.unwrap_or(self.task.turn_id)) else {
            return Ok(());
        };
        host.storage
            .store
            .background()
            .seal_batch(batch.batch_id)
            .await
            .map_err(tool_error)?;
        batch.sealed = true;
        self.wake_if_ready(&host, batch).await
    }

    async fn handoff(
        &self,
        session_id: SessionId,
        _turn_id: Option<TurnId>,
    ) -> Result<(), ToolError> {
        if session_id != self.task.session_id {
            return Err(tool_error("background handoff belongs to another session"));
        }
        let host = self.host()?;
        let _admission = host.execution.command_mutex.lock().await;
        host.suppress_background_continuation(session_id)
            .await
            .map_err(tool_error)
    }

    async fn cancel(
        &self,
        session_id: SessionId,
        turn_id: Option<TurnId>,
        tool_call_id: ToolCallId,
    ) -> Result<(), ToolError> {
        if session_id != self.task.session_id {
            return Err(tool_error(
                "background cancellation belongs to another session",
            ));
        }
        let host = self.host()?;
        let _admission = host.execution.command_mutex.lock().await;
        let repository = host.storage.store.background();
        let task = repository
            .find_task_by_tool_call(host.workspace_id, session_id, tool_call_id)
            .await
            .map_err(tool_error)?
            .ok_or_else(|| tool_error("background cancellation has no admission"))?;
        if turn_id.is_some_and(|turn| turn != task.scope.turn_id) {
            return Err(tool_error(
                "background cancellation belongs to another turn",
            ));
        }
        repository
            .suppress_batch(host.workspace_id, session_id, task.batch_id)
            .await
            .map_err(tool_error)?;
        if let Some(control) = host.execution.task_controls.lock().await.get(&session_id) {
            control.execution.suppress_background_batch(task.batch_id);
        }
        Ok(())
    }
}

impl RuntimeHost {
    pub(super) async fn release_finished_background_reports(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> Result<(), ClientError> {
        let repository = self.storage.store.background();
        for snapshot in self.execution.background_supervisor.list(session_id) {
            if let Some(task) = repository
                .find_task_by_tool_call(self.workspace_id, session_id, snapshot.background_task_id)
                .await?
                && task.scope.task_id == task_id
                && repository
                    .result_for_task(task.background_task_id)
                    .await?
                    .is_some()
            {
                self.execution
                    .background_supervisor
                    .take_completed_report(session_id, task.tool_call_id);
            }
        }
        Ok(())
    }

    fn try_acquire_background_lease(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> Result<SessionLeaseAttempt, ClientError> {
        let Some(paths) = &self.runtime_paths else {
            return Ok(SessionLeaseAttempt::Acquired(None));
        };
        let path = paths
            .session_locks_dir
            .join(format!("{session_id}.{task_id}.background.lock"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| ClientError::Io(format!("{}: {error}", path.display())))?;
        set_owner_only_file(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(SessionLeaseAttempt::Acquired(Some(Arc::new(
                ExecutionLease(file),
            )))),
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                Ok(SessionLeaseAttempt::Busy)
            }
            Err(error) => Err(ClientError::Io(format!("{}: {error}", path.display()))),
        }
    }

    pub(super) async fn suppress_background_continuation(
        &self,
        session_id: SessionId,
    ) -> Result<(), ClientError> {
        self.storage
            .store
            .background()
            .suppress_session(self.workspace_id, session_id)
            .await?;
        if let Some(control) = self.execution.task_controls.lock().await.get(&session_id) {
            control.execution.suppress_background_continuations();
        }
        Ok(())
    }

    /// Caller owns the session execution lease. Live operations in this host remain attached.
    pub(super) async fn pending_background_results(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<BackgroundResult>, ClientError> {
        let repository = self.storage.store.background();
        if !self
            .execution
            .background_supervisor
            .has_pending(session_id, None)
        {
            let batches = repository
                .session_batches(
                    self.workspace_id,
                    session_id,
                    MAX_BACKGROUND_PENDING_RESULTS,
                )
                .await?;
            let tasks = batches
                .iter()
                .map(|batch| batch.scope.task_id)
                .collect::<HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let mut abandoned = Vec::new();
            let mut owner_leases = Vec::new();
            for task in tasks {
                if let SessionLeaseAttempt::Acquired(lease) =
                    self.try_acquire_background_lease(session_id, task)?
                {
                    abandoned.push(task);
                    owner_leases.push(lease);
                }
            }
            repository
                .recover_abandoned(self.workspace_id, &abandoned)
                .await?;
            drop(owner_leases);
        }
        Ok(repository
            .pending_results(
                self.workspace_id,
                session_id,
                MAX_BACKGROUND_PENDING_RESULTS,
            )
            .await?)
    }

    /// A model/route change or an unresolved tool tail can disable ordinary wire replay.
    /// Preserve the typed background observations from the last captured context in that case.
    pub(super) async fn background_results_for_context_fallback(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<BackgroundResult>, ClientError> {
        let Some(snapshot) = self
            .storage
            .repositories
            .artifacts
            .latest_context(session_id)
            .await?
        else {
            return Ok(Vec::new());
        };
        let Some(artifact_id) = snapshot.restricted_request_artifact_ref else {
            return Ok(Vec::new());
        };
        let Some(artifact) = self.storage.repositories.artifacts.get(artifact_id).await? else {
            return Ok(Vec::new());
        };
        if artifact.session_id != session_id || artifact.artifact_type != "provider_request_replay"
        {
            return Ok(Vec::new());
        }
        let Some(bytes) = self
            .storage
            .store
            .load_artifact_bytes_bounded(&artifact, 16 * 1024 * 1024)
            .await?
        else {
            return Ok(Vec::new());
        };
        let Ok(request) = serde_json::from_slice::<golutra_agent_llm::ProviderRequest>(&bytes)
        else {
            return Ok(Vec::new());
        };
        if request.session_id != Some(session_id)
            || request.request_id != snapshot.provider_request_id
        {
            return Ok(Vec::new());
        }
        let mut seen = HashSet::new();
        let mut results = Vec::new();
        for message in request.messages {
            if message.role != golutra_agent_llm::ProviderRole::Tool
                || message.tool_name.as_deref() != Some("background_result")
            {
                continue;
            }
            let Some(id) = message
                .tool_call_id
                .as_deref()
                .and_then(|id| id.strip_prefix("background-result-"))
                .and_then(|id| id.parse::<BackgroundResultId>().ok())
            else {
                continue;
            };
            if !seen.insert(id) {
                continue;
            }
            if let Some(result) = self
                .storage
                .store
                .background()
                .result_by_id(self.workspace_id, session_id, id)
                .await?
            {
                results.push(result);
            }
            if results.len() == MAX_BACKGROUND_PENDING_RESULTS {
                break;
            }
        }
        Ok(results)
    }
}

fn tool_error(error: impl std::fmt::Display) -> ToolError {
    ToolError::Execution(error.to_string())
}

fn bounded(text: &str, max_bytes: usize) -> String {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
#[path = "background_tests.rs"]
mod tests;
