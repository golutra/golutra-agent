//! 手动交接只生成草稿；确认后建立零历史分支，不发送 prompt，也不安装压缩边界。

use super::*;
use serde::{Deserialize, Serialize};
mod summary;
use summary::{HandoffGeneration, HandoffSource};
#[cfg(test)]
mod tests;

// 仅限制一次辅助生成的资源占用，不约束主任务运行时间；传输额外留出错误返回时间。
const HANDOFF_GENERATION_TIMEOUT: Duration = Duration::from_secs(600);
pub(crate) const HANDOFF_TRANSPORT_TIMEOUT: Duration = Duration::from_secs(610);
// 对取消先到的短期记录设容量上限，防止无人消费的操作 ID 无界积累。
const MAX_HANDOFF_OPERATIONS: usize = 128;

/// 两阶段交接合同。创建使用调用方固定 ID，网络重试不会产生多个会话。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HandoffRequest {
    Prepare {
        operation_id: Uuid,
        goal: Option<String>,
        #[serde(default)]
        provider: Value,
    },
    Cancel {
        operation_id: Uuid,
    },
    Create {
        thread_id: ThreadId,
        session_id: SessionId,
        draft: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HandoffResult {
    Cancelled,
    Draft { draft: String },
    Created { thread: Box<ThreadRecord> },
}

fn invalid(message: impl Into<String>) -> ClientError {
    ClientError::InvalidSession(message.into())
}

// 包括 cancel 先于 prepare 到达时的短期取消标记；防止快速 Esc 后远端继续占用来源租约。
struct HandoffOperation<'a> {
    execution: &'a RuntimeHostExecutionState,
    key: (ThreadId, Uuid),
    cancellation: CancellationToken,
}

impl Drop for HandoffOperation<'_> {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.execution
            .handoff_operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.key);
    }
}

impl RuntimeHost {
    /// 限制在本工作区的静止会话；独立租约防止生成期间另一进程推进来源历史。
    pub async fn handoff_thread(
        &self,
        source: ThreadId,
        request: HandoffRequest,
    ) -> Result<HandoffResult, ClientError> {
        let parent = self.resume_thread(source).await?;
        let operation = match &request {
            HandoffRequest::Prepare { operation_id, .. }
            | HandoffRequest::Cancel { operation_id } => {
                let key = (source, *operation_id);
                let mut operations = self
                    .execution
                    .handoff_operations
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                operations.retain(|_, (started, _)| started.elapsed() < HANDOFF_TRANSPORT_TIMEOUT);
                if operations.len() >= MAX_HANDOFF_OPERATIONS && !operations.contains_key(&key) {
                    return Err(invalid("too many pending handoff operations"));
                }
                let cancel = matches!(&request, HandoffRequest::Cancel { .. });
                if !cancel
                    && operations
                        .get(&key)
                        .is_some_and(|(_, token)| !token.is_cancelled())
                {
                    return Err(invalid("handoff operation is already running"));
                }
                let (_, token) = operations
                    .entry(key)
                    .or_insert_with(|| (Instant::now(), CancellationToken::new()));
                if cancel {
                    token.cancel();
                    return Ok(HandoffResult::Cancelled);
                }
                Some(HandoffOperation {
                    execution: &self.execution,
                    key,
                    cancellation: token.clone(),
                })
            }
            HandoffRequest::Create { .. } => None,
        };
        if operation
            .as_ref()
            .is_some_and(|operation| operation.cancellation.is_cancelled())
        {
            return Ok(HandoffResult::Cancelled);
        }
        if self.execution.shutdown.is_cancelled() {
            return Err(invalid("runtime host is shutting down"));
        }
        let _lease = match self.try_acquire_session_lease(parent.session_id)? {
            SessionLeaseAttempt::Acquired(lease) => lease,
            SessionLeaseAttempt::Busy => {
                return Err(invalid("source session is active in another runtime"));
            }
        };
        let state = self
            .storage
            .repositories
            .projections
            .state(parent.session_id, None)
            .await?;
        if is_active_status(state.task_status) {
            return Err(invalid("interrupt the active task before handoff"));
        }
        match request {
            HandoffRequest::Prepare { goal, provider, .. } => {
                let operation = operation.expect("prepare operation");
                tokio::select! {
                    biased;
                    _ = operation.cancellation.cancelled() => Ok(HandoffResult::Cancelled),
                    _ = self.execution.shutdown.cancelled() => Ok(HandoffResult::Cancelled),
                    result = tokio::time::timeout(HANDOFF_GENERATION_TIMEOUT,
                        self.prepare_handoff(&parent, goal, provider, &operation.cancellation)) => {
                        result.map_err(|_| invalid("handoff generation timed out"))?
                    }
                }
            }
            HandoffRequest::Cancel { .. } => Ok(HandoffResult::Cancelled),
            HandoffRequest::Create {
                thread_id,
                session_id,
                draft,
            } => {
                self.create_handoff(&parent, thread_id, session_id, draft)
                    .await
            }
        }
    }

    async fn prepare_handoff(
        &self,
        parent: &ThreadRecord,
        goal: Option<String>,
        provider: Value,
        cancellation: &CancellationToken,
    ) -> Result<HandoffResult, ClientError> {
        let events = self.cached_history_events(parent.session_id).await?;
        let source = HandoffSource::from_events(&events, goal)?;
        // 只接受 provider 路由覆盖，不能借辅助请求注入执行或授权参数。
        let mut overrides = json!({});
        for key in [
            "provider_profile",
            "provider_model",
            "provider_generation_config",
        ] {
            if let Some(value) = provider.get(key) {
                overrides[key] = value.clone();
            }
        }
        let paths = self.provider_config_paths.clone();
        let cache = Arc::clone(&self.execution.provider_route_cache);
        let provider_plan = run_blocking(move || {
            let mut cache = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cached_mock_provider_plan(&mut cache, paths.as_ref(), &overrides, "prepare handoff")
        })
        .await?
        .map_err(|error| invalid(format!("handoff provider setup failed: {error}")))?;
        if self.force_mock_provider
            || provider_plan.provider.contract().native_protocol == "in_memory"
        {
            return Err(invalid("handoff requires a configured language model"));
        }
        self.generate_handoff(
            parent,
            source,
            HandoffGeneration {
                provider: &provider_plan.provider,
                builder: &provider_plan.context_builder,
                timeout: provider_plan.provider_session_policy.request_timeout,
                cancellation,
            },
        )
        .await
    }

    async fn create_handoff(
        &self,
        parent: &ThreadRecord,
        thread_id: ThreadId,
        session_id: SessionId,
        draft: String,
    ) -> Result<HandoffResult, ClientError> {
        if draft.trim().is_empty() || draft.len() > 128 * 1024 {
            return Err(invalid("handoff draft must contain 1–131072 bytes"));
        }
        let _destination_lease = match self.try_acquire_session_lease(session_id)? {
            SessionLeaseAttempt::Acquired(lease) => lease,
            SessionLeaseAttempt::Busy => return Err(invalid("handoff destination is active")),
        };
        let _writer = self.execution.event_writer.lock().await;
        if let Some(thread) = self.storage.repositories.threads.by_id(thread_id).await? {
            if thread.session_id != session_id
                || thread.parent_thread_id != Some(parent.thread_id)
                || thread.forked_from_sequence_no != Some(0)
            {
                return Err(invalid("handoff destination ID is already in use"));
            }
            self.ensure_thread_not_removed(&thread)?;
            let events = self
                .storage
                .repositories
                .events
                .load(session_id, None, None)
                .await?;
            if !events.iter().any(|event| {
                event.event_type == RuntimeEventType::SessionCreated
                    && event.payload.get("handoff_draft").and_then(Value::as_str)
                        == Some(draft.as_str())
            }) {
                return Err(invalid("handoff destination has a different draft"));
            }
            return Ok(HandoffResult::Created {
                thread: Box::new(thread),
            });
        }
        if self
            .storage
            .repositories
            .threads
            .by_session(session_id)
            .await?
            .is_some()
        {
            return Err(invalid("handoff destination session ID is already in use"));
        }
        let now = chrono::Utc::now();
        // 零历史 fork 是普通关联会话，不满足 delegated-child 判定；不复制任务、消息或压缩边界。
        let thread = ThreadRecord {
            thread_id,
            session_id,
            parent_thread_id: Some(parent.thread_id),
            forked_from_turn_id: None,
            forked_from_sequence_no: Some(0),
            workspace_root: parent.workspace_root.clone(),
            rebound_from_workspace_root: None,
            rollout_path: self
                .runtime_paths
                .as_ref()
                .map(|paths| paths.rollout_path(thread_id).display().to_string()),
            title: format!("Handoff of {}", parent.title)
                .chars()
                .take(120)
                .collect(),
            preview: draft.chars().take(120).collect(),
            created_at: now,
            updated_at: now,
            recency_at: now,
            archived: false,
            removed: false,
        };
        let event = host_event(
            self.next_sequence_no(),
            session_id,
            None,
            RuntimeEventType::SessionCreated,
            RuntimeEventSource::Runtime,
            json!({"summary": "handoff draft ready; waiting for user submission",
                "handoff_source_thread_id": parent.thread_id, "handoff_draft": draft}),
        );
        let event = self
            .storage
            .repositories
            .threads
            .create_with_event(&thread, event)
            .await?
            .ok_or_else(|| {
                invalid("handoff destination was created concurrently; retry with the same IDs")
            })?;
        self.publish_committed_event(event).await?;
        Ok(HandoffResult::Created {
            thread: Box::new(thread),
        })
    }
}
