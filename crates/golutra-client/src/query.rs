//! Runtime 查询、分页与 projection 用例。

use super::*;

impl RuntimeHost {
    pub async fn task_trace(
        &self,
        request: TaskTraceRequest,
    ) -> Result<TaskTracePage, ClientError> {
        trace::read_task_trace(self, request).await
    }

    pub async fn read_artifact_chunk(
        &self,
        request: ArtifactReadRequest,
    ) -> Result<Option<ArtifactChunk>, ClientError> {
        trace::read_artifact_chunk(self, request).await
    }

    pub(crate) async fn query(&self, query: RuntimeQuery) -> Result<Value, ClientError> {
        self.ensure_session_in_workspace(query.session_id).await?;
        let value = match query.kind {
            RuntimeQueryKind::SessionState | RuntimeQueryKind::TaskState => serde_json::to_value(
                self.repositories
                    .projections
                    .state(query.session_id, query.task_id)
                    .await?,
            )?,
            RuntimeQueryKind::UserProjection => serde_json::to_value(
                self.repositories
                    .projections
                    .user(query.session_id, query.task_id)
                    .await?,
            )?,
            RuntimeQueryKind::DebugProjection => serde_json::to_value(
                self.repositories
                    .projections
                    .debug(query.session_id, query.task_id)
                    .await?,
            )?,
            RuntimeQueryKind::ContextProjection => {
                let task_id = query.task_id.ok_or_else(|| {
                    ClientError::InvalidSession(
                        "context_projection query requires task_id".to_owned(),
                    )
                })?;
                self.ensure_task_in_session(query.session_id, task_id)
                    .await?;
                serde_json::to_value(
                    self.governance
                        .context_projection(query.session_id, task_id)
                        .await?,
                )?
            }
            RuntimeQueryKind::EvaluationProjection => {
                let task_id = query.task_id.ok_or_else(|| {
                    ClientError::InvalidSession(
                        "evaluation_projection query requires task_id".to_owned(),
                    )
                })?;
                self.ensure_task_in_session(query.session_id, task_id)
                    .await?;
                serde_json::to_value(
                    self.governance
                        .evaluation_projection(query.session_id, task_id)
                        .await?,
                )?
            }
            RuntimeQueryKind::ReplayCursor => serde_json::to_value(
                self.repositories
                    .events
                    .load(query.session_id, query.task_id, query.cursor)
                    .await?,
            )?,
            RuntimeQueryKind::MemoryList => {
                let memory_store = self.memory_store.clone();
                serde_json::to_value(run_blocking(move || memory_store.list()).await??)?
            }
            RuntimeQueryKind::EvaluationResults => {
                let evaluation_store = self.evaluation_store.clone();
                let state = run_blocking(move || evaluation_store.snapshot()).await??;
                json!({
                    "cases": state.cases,
                    "runs": state.runs,
                    "results": state.results,
                    "replays": state.replays,
                    "reviews": state.reviews,
                    "benchmark_runs": state.benchmark_runs,
                    "counterfactual_replays": state.counterfactual_replays,
                    "causal_comparisons": state.causal_comparisons,
                })
            }
            RuntimeQueryKind::ImprovementCandidates => {
                let evaluation_store = self.evaluation_store.clone();
                serde_json::to_value(
                    run_blocking(move || evaluation_store.snapshot())
                        .await??
                        .improvement_candidates,
                )?
            }
            RuntimeQueryKind::AutomationCandidates => {
                let evaluation_store = self.evaluation_store.clone();
                let state = run_blocking(move || evaluation_store.snapshot()).await??;
                json!({
                    "candidates": state.automation_candidates,
                    "generated_tasks": state.generated_tasks,
                    "skill_candidates": state.skill_candidates,
                    "benchmark_promotions": state.benchmark_promotions,
                    "regressions": state.regressions,
                    "promotion_decisions": state.promotion_decisions,
                    "applied_candidates": state.applied_candidates,
                })
            }
            RuntimeQueryKind::EvolutionState => {
                let evolution_store = self.evolution_store.clone();
                serde_json::to_value(run_blocking(move || evolution_store.snapshot()).await??)?
            }
            RuntimeQueryKind::ProviderState => {
                let provider =
                    self.runtime_paths.as_ref().map_or_else(
                        ConfiguredProvider::redacted_from_env,
                        |paths| {
                            let paths =
                                ProviderConfigPaths::from_home(&paths.home).map_err(|error| {
                                    ProviderError::NotConfigured {
                                        message: error.to_string(),
                                    }
                                })?;
                            let environment = load_provider_runtime_env_from_paths(&paths)
                                .map_err(|error| ProviderError::NotConfigured {
                                    message: error.to_string(),
                                })?;
                            ConfiguredProvider::redacted_from_reader(|key| environment.get(key))
                        },
                    );
                let latest_runtime_fact = self
                    .repositories
                    .events
                    .load_recent(query.session_id, query.task_id, None, 128)
                    .await?
                    .into_iter()
                    .rev()
                    .find(|event| {
                        matches!(
                            event.event_type,
                            RuntimeEventType::ProviderAuthRequired
                                | RuntimeEventType::ProviderAuthSubmitted
                                | RuntimeEventType::ProviderAuthCancelled
                                | RuntimeEventType::ProviderConfigured
                                | RuntimeEventType::ProviderProbeCompleted
                                | RuntimeEventType::ProviderAuthFailed
                                | RuntimeEventType::ProviderRateLimited
                        )
                    });
                let (provider, error) = match provider {
                    Ok(provider) => (Some(provider), None),
                    Err(error) => (None, Some(error.to_string())),
                };
                json!({
                    "provider": provider,
                    "error": error,
                    "latest_runtime_fact": latest_runtime_fact,
                })
            }
            RuntimeQueryKind::StorageStatus => serde_json::to_value(self.storage_stats().await?)?,
            RuntimeQueryKind::TaskTrace => {
                let task_id = query.task_id.ok_or_else(|| {
                    ClientError::InvalidSession("task_trace query requires task_id".to_owned())
                })?;
                serde_json::to_value(
                    self.task_trace(golutra_protocol::TaskTraceRequest {
                        session_id: query.session_id,
                        task_id,
                        view: golutra_core::TraceView::Full,
                        cursor: query.cursor,
                        limit: MAX_EVENT_PAGE_SIZE,
                        wait_for_evaluation: false,
                    })
                    .await?,
                )?
            }
            RuntimeQueryKind::PostTaskJobs => {
                let task_id = query.task_id.ok_or_else(|| {
                    ClientError::InvalidSession("post_task_jobs query requires task_id".to_owned())
                })?;
                self.ensure_task_in_session(query.session_id, task_id)
                    .await?;
                serde_json::to_value(self.repositories.jobs.list_for_task(task_id).await?)?
            }
            RuntimeQueryKind::ArtifactChunk => {
                return Err(ClientError::InvalidSession(
                    "artifact chunk queries must use ArtifactReadRequest".to_owned(),
                ));
            }
        };
        Ok(value)
    }

    pub(crate) async fn replay_events(
        &self,
        filter: EventFilter,
    ) -> Result<Vec<Value>, ClientError> {
        self.ensure_session_in_workspace(filter.session_id).await?;
        let events = self
            .repositories
            .events
            .load(filter.session_id, filter.task_id, filter.after_sequence_no)
            .await?;
        events
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(ClientError::Serialization)
    }

    pub async fn event_page(&self, request: EventPageRequest) -> Result<EventPage, ClientError> {
        self.ensure_session_in_workspace(request.session_id).await?;
        let limit = request.limit.clamp(1, MAX_EVENT_PAGE_SIZE);
        let fetch_limit = limit.saturating_add(1);
        let mut events = match request.direction {
            EventPageDirection::Forward => {
                self.repositories
                    .events
                    .load_page(
                        request.session_id,
                        request.task_id,
                        request.cursor,
                        fetch_limit,
                    )
                    .await?
            }
            EventPageDirection::Backward => {
                self.repositories
                    .events
                    .load_before(
                        request.session_id,
                        request.task_id,
                        request.cursor,
                        fetch_limit,
                    )
                    .await?
            }
        };
        let has_more = events.len() > limit as usize;
        if has_more {
            match request.direction {
                EventPageDirection::Forward => {
                    events.truncate(limit as usize);
                }
                EventPageDirection::Backward => {
                    events.remove(0);
                }
            }
        }
        Ok(EventPage {
            direction: request.direction,
            start_cursor: events.first().map(|event| event.sequence_no),
            end_cursor: events.last().map(|event| event.sequence_no),
            events,
            has_more,
        })
    }
}
