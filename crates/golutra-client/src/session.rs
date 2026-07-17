//! Thread、session、fork、resume、rebind 与 rollout export 用例。

use super::*;

impl RuntimeHost {
    pub async fn list_threads(&self, limit: u32) -> Result<Vec<ThreadRecord>, ClientError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let workspace_root = self.workspace_root_string();
        let threads = self
            .repositories
            .threads
            .list(workspace_root.as_deref(), limit)
            .await?
            .into_iter()
            .take(limit as usize)
            .collect();
        Ok(threads)
    }

    pub async fn thread_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<ThreadRecord>, ClientError> {
        let thread = self.repositories.threads.by_session(session_id).await?;
        if let Some(thread) = &thread {
            self.ensure_thread_in_workspace(thread)?;
        }
        Ok(thread)
    }

    pub async fn resume_thread(&self, thread_id: ThreadId) -> Result<ThreadRecord, ClientError> {
        let thread = self
            .repositories
            .threads
            .by_id(thread_id)
            .await?
            .ok_or_else(|| {
                ClientError::InvalidSession(format!("thread `{thread_id}` not found"))
            })?;
        self.ensure_thread_in_workspace(&thread)?;
        Ok(thread)
    }

    pub async fn fork_thread(
        &self,
        thread_id: ThreadId,
        from_turn_id: Option<TurnId>,
    ) -> Result<ThreadRecord, ClientError> {
        let parent = self
            .repositories
            .threads
            .by_id(thread_id)
            .await?
            .ok_or_else(|| {
                ClientError::InvalidSession(format!("thread `{thread_id}` not found"))
            })?;
        self.ensure_thread_in_workspace(&parent)?;
        let parent_state = self
            .repositories
            .projections
            .state(parent.session_id, None)
            .await?;
        if is_active_status(parent_state.task_status) {
            return Err(ClientError::InvalidSession(format!(
                "thread `{thread_id}` cannot be forked while its task is active"
            )));
        }
        let parent_events = self
            .repositories
            .events
            .load(parent.session_id, None, None)
            .await?;
        let through_sequence_no = match from_turn_id {
            Some(turn_id) => fork_sequence_for_turn(&parent_events, turn_id).ok_or_else(|| {
                ClientError::InvalidSession(format!(
                    "turn `{turn_id}` was not found in thread `{thread_id}`"
                ))
            })?,
            None => parent_events
                .last()
                .map(|event| event.sequence_no)
                .unwrap_or_default(),
        };
        let now = chrono::Utc::now();
        let child_thread_id = ThreadId::new();
        let child_session_id = SessionId::new();
        let child = ThreadRecord {
            thread_id: child_thread_id,
            session_id: child_session_id,
            parent_thread_id: Some(parent.thread_id),
            forked_from_turn_id: from_turn_id,
            forked_from_sequence_no: Some(through_sequence_no),
            workspace_root: parent.workspace_root.clone(),
            rebound_from_workspace_root: None,
            rollout_path: self
                .runtime_paths
                .as_ref()
                .map(|paths| paths.rollout_path(child_thread_id).display().to_string()),
            title: format!("Fork of {}", parent.title),
            preview: parent.preview.clone(),
            created_at: now,
            updated_at: now,
            recency_at: now,
            archived: false,
        };
        let _writer = self.event_writer.lock().await;
        let forked_events = self
            .repositories
            .threads
            .fork(&child, parent.session_id, through_sequence_no)
            .await?;
        for event in &forked_events {
            let _ = self.event_bus.send(event.clone());
        }
        drop(_writer);
        let child_state = self
            .repositories
            .projections
            .state(child.session_id, None)
            .await?;
        if is_active_status(child_state.task_status) {
            self.record_event(host_event(
                self.next_sequence_no(),
                child.session_id,
                child_state.active_task_id,
                RuntimeEventType::TaskCompleted,
                RuntimeEventSource::Runtime,
                json!({
                    "summary": "fork history closed at the selected turn boundary",
                    "status": TaskStatus::Completed,
                    "fork_boundary": true,
                }),
            ))
            .await?;
        }
        self.record_event(host_event(
            self.next_sequence_no(),
            child.session_id,
            None,
            RuntimeEventType::ThreadForked,
            RuntimeEventSource::Runtime,
            json!({
                "summary": format!("thread forked from {}", parent.thread_id),
                "parent_thread_id": parent.thread_id,
                "forked_from_turn_id": from_turn_id,
                "forked_from_sequence_no": through_sequence_no,
            }),
        ))
        .await?;
        self.rebuild_thread_rollout(&child).await?;
        Ok(child)
    }

    pub async fn export_thread_rollout(
        &self,
        thread_id: ThreadId,
    ) -> Result<RolloutExport, ClientError> {
        let mut thread = self
            .repositories
            .threads
            .by_id(thread_id)
            .await?
            .ok_or_else(|| {
                ClientError::InvalidSession(format!("thread `{thread_id}` not found"))
            })?;
        self.ensure_thread_in_workspace(&thread)?;
        self.ensure_thread_rollout_path(&mut thread).await?;
        let _writer = self.event_writer.lock().await;
        let events = self
            .repositories
            .events
            .load(thread.session_id, None, None)
            .await?;
        let lines = events
            .iter()
            .map(|event| rollout_line(&thread, event))
            .collect::<Result<Vec<_>, _>>()?;
        let last_sequence_no = events.last().map(|event| event.sequence_no);
        let event_count = events.len();
        let exports_dir = self
            .runtime_paths
            .as_ref()
            .map(|paths| paths.rollouts_dir.join("exports"))
            .ok_or_else(|| {
                ClientError::InvalidSession("rollout export requires a durable runtime".to_owned())
            })?;
        ensure_private_dir(&exports_dir)?;
        let path = exports_dir.join(format!(
            "{}-{}-{}.jsonl",
            thread.thread_id,
            last_sequence_no.unwrap_or_default(),
            Uuid::now_v7()
        ));
        let export_path = path.display().to_string();
        run_blocking(move || rebuild_rollout_file(&path, &lines)).await??;
        Ok(RolloutExport {
            thread_id: thread.thread_id,
            session_id: thread.session_id,
            path: export_path,
            event_count,
            last_sequence_no,
        })
    }

    pub async fn rebind_thread(
        &self,
        thread_id: ThreadId,
        from_workspace_root: impl AsRef<Path>,
    ) -> Result<ThreadRebindResult, ClientError> {
        let new_workspace_root = self.workspace_root_string().ok_or_else(|| {
            ClientError::InvalidSession("thread rebind requires a cwd runtime".to_owned())
        })?;
        let source_workspace_root = normalize_rebind_source(from_workspace_root.as_ref())?;
        let from_workspace_root = source_workspace_root.display().to_string();
        let mut thread = self
            .repositories
            .threads
            .by_id(thread_id)
            .await?
            .ok_or_else(|| {
                ClientError::InvalidSession(format!("thread `{thread_id}` not found"))
            })?;
        if thread.workspace_root.as_deref() != Some(from_workspace_root.as_str()) {
            return Err(ClientError::InvalidSession(format!(
                "thread `{thread_id}` belongs to `{}`, not `{from_workspace_root}`",
                thread.workspace_root.as_deref().unwrap_or("<none>")
            )));
        }
        let state = self
            .repositories
            .projections
            .state(thread.session_id, None)
            .await?;
        if is_active_status(state.task_status) {
            return Err(ClientError::InvalidSession(format!(
                "thread `{thread_id}` cannot be rebound while its task is active"
            )));
        }
        let SessionLeaseAttempt::Acquired(_lease) =
            self.try_acquire_session_lease(thread.session_id)?
        else {
            return Err(ClientError::InvalidSession(format!(
                "thread `{thread_id}` is owned by another runtime"
            )));
        };
        let expected_old_rollout_path = self.runtime_paths.as_ref().map(|paths| {
            rollout_path_for_workspace(paths, &source_workspace_root, thread.thread_id)
        });
        let old_rollout_path = match (&thread.rollout_path, expected_old_rollout_path) {
            (Some(configured), Some(expected)) if Path::new(configured) == expected => {
                Some(expected)
            }
            (Some(configured), Some(expected)) => {
                return Err(ClientError::InvalidSession(format!(
                    "thread `{thread_id}` rollout path `{configured}` does not match source workspace path `{}`",
                    expected.display()
                )));
            }
            (Some(_), None) => {
                return Err(ClientError::InvalidSession(
                    "thread rebind requires durable runtime paths".to_owned(),
                ));
            }
            (None, _) => None,
        };
        thread.workspace_root = Some(new_workspace_root.clone());
        thread.rebound_from_workspace_root = Some(from_workspace_root.clone());
        thread.rollout_path = self
            .runtime_paths
            .as_ref()
            .map(|paths| paths.rollout_path(thread.thread_id).display().to_string());
        thread.updated_at = chrono::Utc::now();
        thread.recency_at = thread.updated_at;
        self.repositories.threads.upsert(&thread).await?;
        let rollout = self.rebuild_thread_rollout(&thread).await?;
        self.record_event(host_event(
            self.next_sequence_no(),
            thread.session_id,
            state.active_task_id,
            RuntimeEventType::ThreadRebound,
            RuntimeEventSource::Runtime,
            json!({
                "summary": format!("thread rebound from {from_workspace_root} to {new_workspace_root}"),
                "thread_id": thread.thread_id,
                "from_workspace_root": from_workspace_root,
                "to_workspace_root": new_workspace_root,
                "checkpoint_compatibility": "historical_only",
            }),
        ))
        .await?;
        if let Some(old_path) = old_rollout_path
            && thread.rollout_path.as_deref() != Some(old_path.to_string_lossy().as_ref())
            && old_path.exists()
        {
            fs::remove_file(&old_path)
                .map_err(|error| ClientError::Io(format!("{}: {error}", old_path.display())))?;
        }
        Ok(ThreadRebindResult {
            thread,
            previous_workspace_root: from_workspace_root,
            rollout_rebuilt: rollout.event_count > 0,
            checkpoint_compatibility: "historical_only".to_owned(),
        })
    }

    pub(super) async fn upsert_current_thread(
        &self,
        session_id: SessionId,
        payload: &Value,
    ) -> Result<(), ClientError> {
        let now = chrono::Utc::now();
        let existing = self.repositories.threads.by_session(session_id).await?;
        if let Some(existing) = &existing {
            self.ensure_thread_in_workspace(existing)?;
        }
        let payload_thread_id = thread_id_from_payload(payload);
        if let (Some(existing), Some(payload_thread_id)) = (&existing, payload_thread_id)
            && existing.thread_id != payload_thread_id
        {
            return Err(ClientError::InvalidSession(format!(
                "session `{session_id}` already belongs to thread `{}`",
                existing.thread_id
            )));
        }
        let payload_thread = match payload_thread_id {
            Some(thread_id) => self.repositories.threads.by_id(thread_id).await?,
            None => None,
        };
        if let Some(payload_thread) = &payload_thread {
            self.ensure_thread_in_workspace(payload_thread)?;
        }
        if let Some(payload_thread) = &payload_thread
            && payload_thread.session_id != session_id
        {
            return Err(ClientError::InvalidSession(format!(
                "thread `{}` belongs to another session",
                payload_thread.thread_id
            )));
        }
        let source_thread = existing.as_ref().or(payload_thread.as_ref());
        let thread_id = source_thread
            .map(|thread| thread.thread_id)
            .or(payload_thread_id)
            .unwrap_or_else(|| {
                if session_id == self.default_session_id {
                    self.default_thread_id
                } else {
                    ThreadId::new()
                }
            });
        let thread = ThreadRecord {
            thread_id,
            session_id,
            parent_thread_id: source_thread.and_then(|thread| thread.parent_thread_id),
            forked_from_turn_id: source_thread.and_then(|thread| thread.forked_from_turn_id),
            forked_from_sequence_no: source_thread
                .and_then(|thread| thread.forked_from_sequence_no),
            workspace_root: self.workspace_root_string(),
            rebound_from_workspace_root: source_thread
                .and_then(|thread| thread.rebound_from_workspace_root.clone()),
            rollout_path: source_thread
                .and_then(|thread| thread.rollout_path.clone())
                .or_else(|| {
                    self.runtime_paths
                        .as_ref()
                        .map(|paths| paths.rollout_path(thread_id).display().to_string())
                }),
            title: thread_title_for_prompt(source_thread, payload),
            preview: preview_from_payload(payload),
            created_at: source_thread.map(|thread| thread.created_at).unwrap_or(now),
            updated_at: now,
            recency_at: now,
            archived: false,
        };
        self.repositories.threads.upsert(&thread).await?;
        Ok(())
    }
}
