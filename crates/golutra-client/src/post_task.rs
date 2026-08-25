//! Durable post-task worker.
//!
//! Task execution and post-task evaluation have different lifetimes.  This
//! coordinator owns the worker loop and lease transitions while the host
//! remains the owner of event serialization and evaluation policy.

use std::sync::{Arc, Weak};

use super::*;

#[derive(Debug, Clone)]
pub struct PostTaskCoordinator {
    host: Weak<RuntimeHost>,
}

impl PostTaskCoordinator {
    #[must_use]
    pub(crate) fn for_host(host: Arc<RuntimeHost>) -> Self {
        Self {
            host: Arc::downgrade(&host),
        }
    }

    pub(crate) fn start(host: &Arc<RuntimeHost>) -> tokio::task::JoinHandle<()> {
        let coordinator = Self {
            host: Arc::downgrade(host),
        };
        let worker_id = format!("{}:{}", host.instance_id, std::process::id());
        let shutdown = host.execution.shutdown.clone();
        tokio::spawn(async move {
            coordinator.run(worker_id, shutdown).await;
        })
    }

    pub async fn status(&self, task_id: TaskId) -> Result<Option<PostTaskJob>, ClientError> {
        let Some(host) = self.host.upgrade() else {
            return Err(ClientError::TaskExecution(
                "runtime host is no longer available".to_owned(),
            ));
        };
        let job = host.storage.repositories.jobs.get_for_task(task_id).await?;
        ensure_job_in_workspace(&host, job.as_ref(), task_id)?;
        Ok(job)
    }

    pub async fn wait_for_terminal(
        &self,
        task_id: TaskId,
    ) -> Result<Option<PostTaskJob>, ClientError> {
        let Some(host) = self.host.upgrade() else {
            return Err(ClientError::TaskExecution(
                "runtime host is no longer available".to_owned(),
            ));
        };
        let current = host.storage.repositories.jobs.get_for_task(task_id).await?;
        ensure_job_in_workspace(&host, current.as_ref(), task_id)?;
        host.wait_for_deep_task_evaluation(task_id).await;
        let job = host.storage.repositories.jobs.get_for_task(task_id).await?;
        ensure_job_in_workspace(&host, job.as_ref(), task_id)?;
        Ok(job)
    }

    async fn run(self, worker_id: String, shutdown: tokio_util::sync::CancellationToken) {
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            let Some(host) = self.host.upgrade() else {
                return;
            };
            if host
                .storage
                .repositories
                .jobs
                .recover_expired(&host.workspace_id.to_string(), chrono::Utc::now())
                .await
                .is_err()
            {
                drop(host);
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(POST_TASK_JOB_IDLE_POLL_MILLIS)) => {}
                }
                continue;
            }
            let workspace_id = host.workspace_id.to_string();
            let claimed = host
                .storage
                .repositories
                .jobs
                .claim(
                    &worker_id,
                    &workspace_id,
                    chrono::Utc::now(),
                    chrono::Duration::minutes(POST_TASK_JOB_LEASE_MINUTES),
                )
                .await;
            match claimed {
                Ok(Some(job)) => self.process(job, &worker_id).await,
                Ok(None) | Err(_) => {
                    drop(host);
                    tokio::select! {
                        _ = shutdown.cancelled() => return,
                        _ = tokio::time::sleep(Duration::from_millis(POST_TASK_JOB_IDLE_POLL_MILLIS)) => {}
                    }
                }
            }
        }
    }

    async fn process(&self, job: PostTaskJob, worker_id: &str) {
        let Some(host) = self.host.upgrade() else {
            return;
        };
        if job.workspace_id != host.workspace_id.to_string() {
            let _ = host
                .storage
                .repositories
                .jobs
                .requeue(
                    job.job_id,
                    worker_id,
                    "post-task job was claimed by a worker for another workspace",
                )
                .await;
            return;
        }
        let started_ok = host
            .storage
            .repositories
            .jobs
            .start(job.job_id, worker_id, chrono::Utc::now())
            .await
            .unwrap_or(false);
        if !started_ok {
            host.storage
                .deep_evaluation_inputs
                .lock()
                .await
                .remove(&job.job_id);
            return;
        }
        let context = host.reconstruct_post_task_context(&job).await;
        let (task, input) = match context {
            Ok(context) => context,
            Err(error) => {
                host.storage
                    .deep_evaluation_inputs
                    .lock()
                    .await
                    .remove(&job.job_id);
                self.finish_or_retry(&host, &job, worker_id, error.to_string())
                    .await;
                return;
            }
        };
        let _ = host
            .record_event(agent_event(
                host.next_sequence_no(),
                &task,
                RuntimeEventType::PostTaskJobStarted,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": "durable post-task evaluation started",
                    "job_id": job.job_id,
                    "attempt": job.attempt,
                }),
            ))
            .await;
        let bundle = host.storage.governance.evaluate_deep(input);
        let result = host.record_task_evaluation(&task, bundle).await;
        match result {
            Ok(true) => {
                let result_refs = vec![format!("evaluation:{}", task.task_id)];
                let finished = host
                    .storage
                    .repositories
                    .jobs
                    .finish(
                        job.job_id,
                        worker_id,
                        PostTaskJobStatus::Succeeded,
                        &result_refs,
                        None,
                        chrono::Utc::now(),
                    )
                    .await
                    .unwrap_or(false);
                if finished {
                    let _ = host
                        .record_event(agent_event(
                            host.next_sequence_no(),
                            &task,
                            RuntimeEventType::PostTaskJobCompleted,
                            RuntimeEventSource::Evaluator,
                            json!({
                                "summary": "durable post-task evaluation completed",
                                "job_id": job.job_id,
                                "result_refs": result_refs,
                            }),
                        ))
                        .await;
                }
            }
            Ok(false) => {
                self.finish_or_retry(
                    &host,
                    &job,
                    worker_id,
                    "durable task evaluation was not persisted".to_owned(),
                )
                .await
            }
            Err(error) => {
                self.finish_or_retry(&host, &job, worker_id, error.to_string())
                    .await
            }
        }
        host.storage
            .deep_evaluation_inputs
            .lock()
            .await
            .remove(&job.job_id);
    }

    async fn finish_or_retry(
        &self,
        host: &Arc<RuntimeHost>,
        job: &PostTaskJob,
        worker_id: &str,
        error: String,
    ) {
        let error = compact_event_summary(&error);
        let requeued = host
            .storage
            .repositories
            .jobs
            .requeue(job.job_id, worker_id, &error)
            .await
            .unwrap_or(false);
        if requeued {
            let _ = host
                .record_event(host_event(
                    host.next_sequence_no(),
                    job.session_id.parse().unwrap_or(host.default_session_id),
                    Some(job.task_id),
                    RuntimeEventType::RetryScheduled,
                    RuntimeEventSource::Evaluator,
                    json!({
                        "summary": "post-task evaluation retry scheduled",
                        "job_id": job.job_id,
                        "error": error,
                        "attempt": job.attempt,
                    }),
                ))
                .await;
            return;
        }
        let finished = host
            .storage
            .repositories
            .jobs
            .finish(
                job.job_id,
                worker_id,
                PostTaskJobStatus::Failed,
                &[],
                Some(&error),
                chrono::Utc::now(),
            )
            .await
            .unwrap_or(false);
        if finished {
            let _ = host
                .record_event(host_event(
                    host.next_sequence_no(),
                    job.session_id.parse().unwrap_or(host.default_session_id),
                    Some(job.task_id),
                    RuntimeEventType::PostTaskJobFailed,
                    RuntimeEventSource::Evaluator,
                    json!({
                        "summary": "post-task evaluation exhausted its retry budget",
                        "job_id": job.job_id,
                        "phase": "deep_evaluation",
                        "terminal": true,
                        "execution_outcome_unchanged": true,
                        "error": error,
                        "attempt": job.attempt,
                    }),
                ))
                .await;
        }
    }
}

pub(crate) fn ensure_job_in_workspace(
    host: &RuntimeHost,
    job: Option<&PostTaskJob>,
    task_id: TaskId,
) -> Result<(), ClientError> {
    if job.is_some_and(|job| job.workspace_id != host.workspace_id.to_string()) {
        return Err(ClientError::InvalidSession(format!(
            "task `{task_id}` was not found in the attached workspace"
        )));
    }
    Ok(())
}
