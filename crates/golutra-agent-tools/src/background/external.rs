//! Register host-owned execution without acquiring a tool execution permit.
use super::*;

#[derive(Debug)]
pub struct ExternalBackgroundTask {
    supervisor: BackgroundToolSupervisor,
    observer: Arc<dyn BackgroundToolObserver>,
    job: Arc<Job>,
}

impl ExternalBackgroundTask {
    /// Retries durable delivery only; the caller retains execution ownership.
    pub async fn complete(self, report: ToolExecutionReport) {
        self.supervisor
            .finish_job(&self.job.request.clone(), report, self.observer, self.job)
            .await;
    }
}

impl BackgroundToolSupervisor {
    /// The caller must retain a host-owned task across admission and completion.
    /// External execution shares result bounds and activity signals, not tool permits.
    pub async fn register_external(
        &self,
        request: ToolRequest,
        cancellation: CancellationToken,
        observer: Arc<dyn BackgroundToolObserver>,
    ) -> Result<ExternalBackgroundTask, ToolError> {
        if self.inner.shutdown.is_cancelled() {
            return Err(ToolError::Execution("background host is stopping".into()));
        }
        let key = (request.session_id, request.tool_call_id);
        let job = {
            let mut jobs = self
                .inner
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if jobs.contains_key(&key) {
                return Err(ToolError::Execution(
                    "external execution is already registered".into(),
                ));
            }
            if jobs.len() >= MAX_RETAINED {
                let oldest = jobs
                    .iter()
                    .filter_map(|(key, job)| {
                        let state = job
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        state
                            .completed_report
                            .is_none()
                            .then_some(state.finished_at)
                            .flatten()
                            .map(|at| (*key, at))
                    })
                    .min_by_key(|(_, at)| *at)
                    .map(|(key, _)| key);
                if let Some(oldest) = oldest {
                    jobs.remove(&oldest);
                }
            }
            if jobs.len() >= MAX_RETAINED {
                return Err(ToolError::Execution(
                    "background task capacity reached; consume existing results".into(),
                ));
            }
            let job = Arc::new(Job {
                request: request.clone(),
                state: Mutex::new(JobState {
                    snapshot: BackgroundToolSnapshot {
                        background_task_id: request.tool_call_id,
                        session_id: request.session_id,
                        turn_id: request.turn_id,
                        tool_name: request.tool_name.clone(),
                        status: BackgroundTaskStatus::Queued,
                        pending: true,
                        summary: "host-owned task starting".into(),
                        result: None,
                        delivery_error: None,
                    },
                    finished_at: None,
                    admitted: false,
                    completed_report: None,
                    image_retention: None,
                }),
                cancellation,
                handle: Mutex::new(None),
            });
            jobs.insert(key, job.clone());
            job
        };
        if let Err(error) = observer.admit(&request).await {
            self.inner
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&key);
            self.changed();
            return Err(error);
        }
        {
            let mut state = job
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.admitted = true;
            state.snapshot.status = BackgroundTaskStatus::Running;
        }
        self.changed();
        Ok(ExternalBackgroundTask {
            supervisor: self.clone(),
            observer,
            job,
        })
    }
}
