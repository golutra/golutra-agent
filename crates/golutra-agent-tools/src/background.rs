//! Host-scoped tool execution. Delivery retries never repeat the external operation.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures_util::FutureExt;
use golutra_agent_core::{BackgroundTaskStatus, SessionId, ToolCallId, TurnId};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    ExternalToolBackend, ExternalToolOutput, PolicyDecision, PolicyEvaluation, SideEffectType,
    ToolCapabilities, ToolContract, ToolError, ToolExecutionReport, ToolInvocation, ToolRequest,
    ToolResultStatus, ToolRuntime, bounded_text, cancelled_report_with_policy, error_report,
    model_visible_tool_result, success_report, supervised_process_report,
};

const MAX_ACTIVE: usize = 16;
const MAX_RETAINED: usize = 64;
const MAX_VERIFICATION_OUTPUT_BYTES: usize = 2 * 1024 * 1024 + 4096;
const MAX_RETAINED_IMAGE_BYTES: usize = 128 * 1024 * 1024;
const RETENTION: Duration = Duration::from_secs(15 * 60);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

mod external;
pub use external::ExternalBackgroundTask;

/// The host commits admission before execution and the full result before acknowledging delivery.
/// Implementations must reject an already-recorded call after host restart, rather than replay it.
#[async_trait]
pub trait BackgroundToolObserver: std::fmt::Debug + Send + Sync {
    async fn admit(&self, request: &ToolRequest) -> Result<(), ToolError>;
    async fn complete(
        &self,
        request: &ToolRequest,
        report: &ToolExecutionReport,
    ) -> Result<(), ToolError>;
    async fn seal(&self, session_id: SessionId, turn_id: Option<TurnId>) -> Result<(), ToolError>;
    async fn handoff(
        &self,
        _session_id: SessionId,
        _turn_id: Option<TurnId>,
    ) -> Result<(), ToolError> {
        Ok(())
    }
    async fn cancel(
        &self,
        _session_id: SessionId,
        _turn_id: Option<TurnId>,
        _tool_call_id: ToolCallId,
    ) -> Result<(), ToolError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BackgroundToolSnapshot {
    pub background_task_id: ToolCallId,
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub tool_name: String,
    pub status: BackgroundTaskStatus,
    /// Includes admission and durable result delivery, not only physical execution.
    pub pending: bool,
    pub summary: String,
    pub result: Option<Value>,
    pub delivery_error: Option<String>,
}

#[derive(Debug)]
struct JobState {
    snapshot: BackgroundToolSnapshot,
    finished_at: Option<Instant>,
    admitted: bool,
    completed_report: Option<Arc<ToolExecutionReport>>,
    image_retention: Option<ImageRetention>,
}

#[derive(Debug)]
struct ImageRetention {
    used: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for ImageRetention {
    fn drop(&mut self) {
        self.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

impl ImageRetention {
    fn reserve(used: &Arc<AtomicUsize>, bytes: usize) -> Option<Self> {
        used.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current
                .checked_add(bytes)
                .filter(|next| *next <= MAX_RETAINED_IMAGE_BYTES)
        })
        .ok()
        .map(|_| Self {
            used: Arc::clone(used),
            bytes,
        })
    }
}

#[derive(Debug)]
struct Job {
    request: ToolRequest,
    state: Mutex<JobState>,
    cancellation: CancellationToken,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Debug)]
struct Inner {
    jobs: Mutex<HashMap<(SessionId, ToolCallId), Arc<Job>>>,
    slots: Arc<Semaphore>,
    browser_slots: Arc<Semaphore>,
    image_bytes: Arc<AtomicUsize>,
    activity: watch::Sender<u64>,
    shutdown: CancellationToken,
}

#[derive(Debug, Clone)]
pub struct BackgroundToolSupervisor {
    inner: Arc<Inner>,
}

impl Default for BackgroundToolSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundToolSupervisor {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                jobs: Mutex::new(HashMap::new()),
                slots: Arc::new(Semaphore::new(MAX_ACTIVE)),
                browser_slots: Arc::new(Semaphore::new(1)),
                image_bytes: Arc::new(AtomicUsize::new(0)),
                activity: watch::channel(0).0,
                shutdown: CancellationToken::new(),
            }),
        }
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.activity.subscribe()
    }

    fn changed(&self) {
        self.inner
            .activity
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    #[must_use]
    pub fn has_pending(&self, session_id: SessionId, turn_id: Option<TurnId>) -> bool {
        self.inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|job| {
                job.request.session_id == session_id
                    && turn_id.is_none_or(|turn| job.request.turn_id == Some(turn))
                    && job
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .snapshot
                        .pending
            })
    }

    #[must_use]
    pub fn status(&self, session_id: SessionId, id: ToolCallId) -> Option<BackgroundToolSnapshot> {
        self.inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(session_id, id))
            .and_then(|job| {
                let state = job
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.admitted.then(|| state.snapshot.clone())
            })
    }

    #[must_use]
    pub fn list(&self, session_id: SessionId) -> Vec<BackgroundToolSnapshot> {
        let mut jobs = self
            .inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|job| job.request.session_id == session_id)
            .filter_map(|job| {
                let state = job
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.admitted.then(|| state.snapshot.clone())
            })
            .collect::<Vec<_>>();
        jobs.sort_by_key(|job| job.background_task_id);
        jobs
    }

    pub fn take_completed_report(
        &self,
        session_id: SessionId,
        id: ToolCallId,
    ) -> Option<Arc<ToolExecutionReport>> {
        self.inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(session_id, id))
            .and_then(|job| {
                let mut state = job
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.image_retention.take();
                state.completed_report.take()
            })
    }

    /// The host may release verification bodies after the source task's terminal
    /// audit is durable. Pending execution/delivery is never discarded here.
    pub fn discard_completed_reports(
        &self,
        session_id: SessionId,
        turn_id: Option<TurnId>,
    ) -> usize {
        let jobs = self
            .inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut discarded = 0;
        for job in jobs.values().filter(|job| {
            job.request.session_id == session_id
                && turn_id.is_none_or(|turn| job.request.turn_id == Some(turn))
        }) {
            let mut state = job
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !state.snapshot.pending && state.completed_report.take().is_some() {
                state.image_retention.take();
                discarded += 1;
            }
        }
        discarded
    }

    pub fn cancel(&self, session_id: SessionId, id: ToolCallId) -> bool {
        let jobs = self
            .inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(job) = jobs.get(&(session_id, id)) else {
            return false;
        };
        if !job
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .pending
        {
            return false;
        }
        job.cancellation.cancel();
        true
    }

    pub fn cancel_session(&self, session_id: SessionId) {
        self.cancel_matching(session_id, |_| true);
    }

    pub fn cancel_turn(&self, session_id: SessionId, turn_id: Option<TurnId>) {
        self.cancel_matching(session_id, |request| {
            turn_id.is_none_or(|turn| request.turn_id == Some(turn))
        });
    }

    pub(crate) fn cancel_browser_session(&self, session_id: SessionId) {
        self.cancel_matching(session_id, |request| {
            request.tool_name.starts_with("browser_")
        });
    }

    fn cancel_matching(&self, session_id: SessionId, matches: impl Fn(&ToolRequest) -> bool) {
        let jobs = self
            .inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for job in jobs
            .values()
            .filter(|job| job.request.session_id == session_id && matches(&job.request))
        {
            job.cancellation.cancel();
        }
    }

    pub fn shutdown(&self) {
        self.inner.shutdown.cancel();
        for job in self
            .inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            job.cancellation.cancel();
        }
    }

    pub async fn shutdown_and_wait(&self) {
        self.shutdown();
        let has_browser_jobs = self
            .inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|job| job.request.tool_name.starts_with("browser_"));
        let handles = self
            .inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter_map(|job| {
                job.handle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
            })
            .collect::<Vec<_>>();
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        for mut handle in handles {
            if tokio::time::timeout_at(deadline, &mut handle)
                .await
                .is_err()
            {
                handle.abort();
                let _ = handle.await;
            }
        }
        if has_browser_jobs {
            // Browser cleanup can outlive its caller's short cancellation window.
            // Do not wait past the host's bounded shutdown grace or cancel other owners.
            let _ =
                tokio::time::timeout_at(deadline, crate::BROWSER_EXECUTION_PERMITS.acquire()).await;
        }
    }

    pub(crate) async fn submit(
        &self,
        runtime: ToolRuntime,
        invocation: ToolInvocation,
        observer: Arc<dyn BackgroundToolObserver>,
        cancellation: CancellationToken,
    ) -> Result<ToolExecutionReport, ToolError> {
        let admission_started = Instant::now();
        if self.inner.shutdown.is_cancelled() {
            return Err(ToolError::Execution(
                "background runtime is shutting down".into(),
            ));
        }
        let request = invocation.request.clone();
        let policy = invocation.policy.clone();
        let key = (request.session_id, request.tool_call_id);
        let job = {
            let mut jobs = self
                .inner
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // A duplicate call can observe its first execution, but cannot dispatch again.
            if let Some(job) = jobs.get(&key) {
                if job.request != request {
                    return Err(ToolError::InvalidArguments(
                        "background call ID was reused with different arguments".into(),
                    ));
                }
                let state = job
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !state.admitted {
                    return Err(ToolError::Execution(
                        "background call admission is in progress; no handle is available yet"
                            .into(),
                    ));
                }
                return Ok(receipt(&request, policy, &state.snapshot));
            }
            jobs.retain(|_, job| {
                let state = job
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.completed_report.is_some()
                    || state
                        .finished_at
                        .is_none_or(|finished| finished.elapsed() < RETENTION)
            });
            if jobs.len() >= MAX_RETAINED {
                // Only consumed, durably delivered entries may be evicted.
                let oldest = jobs
                    .iter()
                    .filter_map(|(key, job)| {
                        let state = job
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if state.completed_report.is_some() {
                            None
                        } else {
                            state.finished_at.map(|time| (*key, time))
                        }
                    })
                    .min_by_key(|(_, time)| *time)
                    .map(|(key, _)| key);
                if let Some(oldest) = oldest {
                    jobs.remove(&oldest);
                }
            }
            if jobs.len() >= MAX_RETAINED {
                return Err(ToolError::Execution(
                    "background task capacity reached; wait for existing results".into(),
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
                        summary: "background task queued".into(),
                        result: None,
                        delivery_error: None,
                    },
                    finished_at: None,
                    admitted: false,
                    completed_report: None,
                    image_retention: None,
                }),
                cancellation: cancellation.child_token(),
                handle: Mutex::new(None),
            });
            jobs.insert(key, Arc::clone(&job));
            job
        };
        // A caller may disappear while SQLite commits admission. The spawned owner
        // finishes that boundary and records cancellation instead of orphaning a job.
        let (admitted_tx, admitted_rx) = oneshot::channel();
        let supervisor = self.clone();
        let worker_job = Arc::clone(&job);
        let handle = tokio::spawn(async move {
            if let Err(error) = observer.admit(&worker_job.request).await {
                supervisor
                    .inner
                    .jobs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&key);
                supervisor.changed();
                let _ = admitted_tx.send(Err(error));
                return;
            }
            worker_job
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .admitted = true;
            supervisor.changed();
            let _ = admitted_tx.send(Ok(()));
            supervisor
                .run(runtime, invocation, observer, worker_job)
                .await;
        });
        *job.handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
        admitted_rx
            .await
            .map_err(|_| ToolError::Execution("background admission worker stopped".into()))??;
        let mut receipt = receipt(
            &request,
            policy,
            &job.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .snapshot,
        );
        receipt.metrics.duration_ms = crate::elapsed_millis(admission_started);
        Ok(receipt)
    }

    async fn run(
        &self,
        mut runtime: ToolRuntime,
        mut invocation: ToolInvocation,
        observer: Arc<dyn BackgroundToolObserver>,
        job: Arc<Job>,
    ) {
        let execution_started = Instant::now();
        let request = invocation.request.clone();
        let policy = invocation.policy.clone();
        let deadline = invocation.deadline;
        // Browser jobs serialize; waiting for their resource must not occupy all
        // general execution slots and starve unrelated shell work.
        let browser_job = request.tool_name == "browser_run";
        let browser_slot = if browser_job {
            tokio::select! {
                biased;
                () = job.cancellation.cancelled() => None,
                () = self.inner.shutdown.cancelled() => { job.cancellation.cancel(); None },
                () = wait_deadline(deadline) => None,
                permit = self.inner.browser_slots.clone().acquire_owned() => permit.ok(),
            }
        } else {
            None
        };
        let permit = if !browser_job || browser_slot.is_some() {
            tokio::select! {
                biased;
                () = job.cancellation.cancelled() => None,
                () = self.inner.shutdown.cancelled() => { job.cancellation.cancel(); None },
                () = wait_deadline(deadline) => None,
                permit = self.inner.slots.clone().acquire_owned() => permit.ok(),
            }
        } else {
            None
        };
        // Keep the permit until the durable observer accepts the result so failed
        // storage cannot accumulate an unbounded set of full artifact reports.
        let queue_wait_ms = crate::elapsed_millis(execution_started);
        let operation_started = Instant::now();
        let mut report = if permit.is_some() {
            {
                let mut state = job
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.snapshot.status = BackgroundTaskStatus::Running;
                state.snapshot.summary = "background task running".into();
            }
            self.changed();
            runtime.background = None;
            // The control option belongs to the supervisor, never to a browser SDK or CLI.
            if request.tool_name == "browser_run"
                && let Some(arguments) = invocation.request.arguments.as_object_mut()
            {
                arguments.remove("background");
            }
            let result = std::panic::AssertUnwindSafe(runtime.invoke_foreground(
                invocation,
                job.cancellation.clone(),
                None,
            ))
            .catch_unwind()
            .await;
            match result {
                Ok(Ok(report))
                    if request.tool_name == "shell"
                        && report.envelope.structured_facts["terminal"] == false
                        && report.envelope.structured_facts["process_id"].is_string() =>
                {
                    follow_process(
                        &runtime,
                        &request,
                        policy.clone(),
                        report,
                        job.cancellation.clone(),
                        deadline,
                    )
                    .await
                }
                Ok(Ok(report)) => report,
                Ok(Err(error)) => error_report(
                    request.clone(),
                    "background tool failed",
                    json!({"error":bounded_text(&error.to_string(), 4096)}),
                    error.to_string(),
                    policy.clone(),
                ),
                Err(_) => {
                    job.cancellation.cancel();
                    error_report(
                        request.clone(),
                        "background execution stopped unexpectedly",
                        json!({"outcome_unknown":true,"automatic_retry":false}),
                        "Inspect external state before retrying.".into(),
                        policy.clone(),
                    )
                }
            }
        } else if job.cancellation.is_cancelled() || self.inner.shutdown.is_cancelled() {
            cancelled_report_with_policy(
                request.clone(),
                policy.clone(),
                "background task cancelled before dispatch",
            )
        } else {
            runtime.deadline_exceeded_report(
                request.clone(),
                policy.clone(),
                "background admission",
            )
        };
        if let Some(facts) = report.envelope.structured_facts.as_object_mut() {
            facts.insert("background_queue_wait_ms".into(), json!(queue_wait_ms));
            facts.insert(
                "background_execution_ms".into(),
                json!(crate::elapsed_millis(operation_started)),
            );
        }
        report.metrics.duration_ms = crate::elapsed_millis(execution_started);
        self.finish_job(&request, report, observer, job).await;
    }

    async fn finish_job(
        &self,
        request: &ToolRequest,
        report: ToolExecutionReport,
        observer: Arc<dyn BackgroundToolObserver>,
        job: Arc<Job>,
    ) {
        {
            let mut state = job
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.snapshot.status = background_report_status(&report);
            state.snapshot.summary = bounded_text(&report.envelope.summary, 2048);
            let projection = model_visible_tool_result(&report.envelope);
            state.snapshot.result =
                Some(serde_json::from_str(&projection).unwrap_or(Value::String(projection)));
            // The observer persists full artifacts/checkpoints. Runtime verification
            // needs their metadata and changes, not another copy of their byte bodies.
            let mut compact = verification_report(&report);
            let image_bytes = compact
                .artifact_contents
                .iter()
                .filter(|content| is_image_content(&compact, content.artifact_id))
                .map(|content| content.bytes.len())
                .sum::<usize>();
            state.image_retention = ImageRetention::reserve(&self.inner.image_bytes, image_bytes);
            if image_bytes > 0 && state.image_retention.is_none() {
                let image_ids = compact
                    .artifacts
                    .iter()
                    .filter(|artifact| artifact.artifact_type.starts_with("tool_image:"))
                    .map(|artifact| artifact.artifact_id)
                    .collect::<std::collections::HashSet<_>>();
                compact
                    .artifact_contents
                    .retain(|content| !image_ids.contains(&content.artifact_id));
                compact.envelope.structured_facts["background_image_omitted"] =
                    json!("retained_image_budget_exhausted");
            }
            state.completed_report = Some(Arc::new(compact));
        }
        self.changed();
        // Cancellation stops execution, not durable delivery. Retry only this callback.
        let mut backoff = Duration::from_millis(50);
        loop {
            match observer.complete(request, &report).await {
                Ok(()) => {
                    let mut state = job
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.snapshot.pending = false;
                    state.snapshot.delivery_error = None;
                    state.finished_at = Some(Instant::now());
                    drop(state);
                    self.changed();
                    return;
                }
                Err(error) => {
                    job.state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .snapshot
                        .delivery_error = Some(bounded_text(&error.to_string(), 1024));
                    self.changed();
                }
            }
            tokio::select! {
                () = self.inner.shutdown.cancelled() => return,
                () = tokio::time::sleep(backoff) => {},
            }
            backoff = (backoff * 2).min(Duration::from_secs(1));
        }
    }
}

fn verification_report(report: &ToolExecutionReport) -> ToolExecutionReport {
    let raw = report
        .artifact_contents
        .iter()
        .find(|artifact| Some(artifact.artifact_id) == report.envelope.raw_artifact_ref);
    let limit = if report.envelope.tool_name == "shell" {
        MAX_VERIFICATION_OUTPUT_BYTES
    } else {
        128 * 1024
    };
    let mut envelope = report.envelope.clone();
    let mut artifact_contents = match raw {
        Some(raw) if raw.bytes.len() <= limit => vec![raw.clone()],
        Some(_) => {
            envelope.structured_facts["background_verification_output_incomplete"] = json!(true);
            Vec::new()
        }
        None => Vec::new(),
    };
    let images = report
        .artifact_contents
        .iter()
        .filter(|content| is_image_content(report, content.artifact_id))
        .collect::<Vec<_>>();
    if images.len() <= 2
        && images.iter().map(|image| image.bytes.len()).sum::<usize>() <= 4 * 1024 * 1024
    {
        artifact_contents.extend(images.into_iter().cloned());
    } else {
        envelope.structured_facts["background_image_omitted"] = json!("image_contract_exceeded");
    }
    ToolExecutionReport {
        envelope,
        artifacts: report.artifacts.clone(),
        evidence: report.evidence.clone(),
        changed_files: report.changed_files.clone(),
        policy_evaluation: report.policy_evaluation.clone(),
        metrics: report.metrics.clone(),
        artifact_contents,
        before_images: Vec::new(),
        after_images: Vec::new(),
    }
}

fn is_image_content(report: &ToolExecutionReport, artifact_id: crate::ArtifactId) -> bool {
    report.artifacts.iter().any(|artifact| {
        artifact.artifact_id == artifact_id && artifact.artifact_type.starts_with("tool_image:")
    })
}

async fn wait_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn follow_process(
    runtime: &ToolRuntime,
    request: &ToolRequest,
    policy: PolicyEvaluation,
    initial: ToolExecutionReport,
    cancellation: CancellationToken,
    deadline: Option<tokio::time::Instant>,
) -> ToolExecutionReport {
    let process_id = initial.envelope.structured_facts["process_id"]
        .as_str()
        .unwrap_or_default();
    // Subscribe before observing state. A terminal event can race the initial receipt.
    let mut updates = runtime.process_supervisor.subscribe_updates();
    loop {
        let snapshot = match runtime
            .process_supervisor
            .reconnect(request.session_id, process_id, 0)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return error_report(
                    request.clone(),
                    "background process observation failed",
                    json!({"outcome_unknown":true,"error":error.to_string()}),
                    error.to_string(),
                    policy,
                );
            }
        };
        if snapshot.state.is_terminal() {
            let snapshot = runtime
                .process_supervisor
                .wait_for_scan(request.session_id, process_id, 0, 2000)
                .await
                .unwrap_or(snapshot);
            return supervised_process_report(request.clone(), policy, snapshot);
        }
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                let _ = runtime.process_supervisor.terminate(request.session_id, process_id, 0).await;
                return cancelled_report_with_policy(request.clone(), policy, "background process cancelled");
            },
            () = wait_deadline(deadline) => {
                let _ = runtime.process_supervisor.terminate(request.session_id, process_id, 0).await;
                return runtime.deadline_exceeded_report(request.clone(), policy, "background process");
            },
            () = wait_for_process_terminal(&mut updates, request.session_id, process_id) => {},
            () = tokio::time::sleep(Duration::from_secs(5)) => {},
        }
    }
}

async fn wait_for_process_terminal(
    updates: &mut tokio::sync::broadcast::Receiver<crate::ProcessUpdate>,
    session_id: SessionId,
    process_id: &str,
) {
    loop {
        match updates.recv().await {
            Ok(update)
                if update.session_id == session_id
                    && update.process_id == process_id
                    && update.payload["terminal"] == true =>
            {
                return;
            }
            Ok(_) => {}
            Err(_) => return,
        }
    }
}

#[must_use]
pub fn background_report_status(report: &ToolExecutionReport) -> BackgroundTaskStatus {
    let facts = &report.envelope.structured_facts;
    if facts["outcome_unknown"] == true
        || facts["phase"] == "outcome_unknown"
        || matches!(
            facts["code"].as_str(),
            Some("opencli_outcome_unknown" | "browser_outcome_unknown" | "browser_launch_unknown")
        )
    {
        BackgroundTaskStatus::OutcomeUnknown
    } else if facts["waiting_for_user"] == true
        || matches!(
            facts["code"].as_str(),
            Some(
                "user_in_control"
                    | "browser_user_in_control"
                    | "setup_required"
                    | "profile_required"
            )
        )
        || matches!(
            facts["phase"].as_str(),
            Some("handed_off" | "waiting_for_user" | "user_handoff")
        )
    {
        BackgroundTaskStatus::WaitingForUser
    } else {
        match report.envelope.status {
            ToolResultStatus::Ok => BackgroundTaskStatus::Succeeded,
            ToolResultStatus::Cancelled => BackgroundTaskStatus::Cancelled,
            ToolResultStatus::Timeout => BackgroundTaskStatus::TimedOut,
            _ => BackgroundTaskStatus::Failed,
        }
    }
}

fn receipt(
    request: &ToolRequest,
    policy: PolicyEvaluation,
    snapshot: &BackgroundToolSnapshot,
) -> ToolExecutionReport {
    let facts = json!({
        "background_task_id":snapshot.background_task_id,
        "background":true,
        "background_status":snapshot.status,
        "terminal":!snapshot.pending,
        "result_pending":snapshot.pending,
        "next_action":{"tool":"background_task","arguments":{"action":"read","task_id":snapshot.background_task_id}},
        "automatic_retry":false,
        "survives_runtime_exit":false,
    });
    success_report(
        request.clone(),
        &snapshot.summary,
        facts.clone(),
        facts.to_string(),
        Vec::new(),
        policy,
    )
}

#[derive(Debug)]
pub(crate) struct BackgroundControlBackend(
    pub BackgroundToolSupervisor,
    pub Arc<dyn BackgroundToolObserver>,
);

#[async_trait]
impl ExternalToolBackend for BackgroundControlBackend {
    fn contracts(&self) -> Vec<ToolContract> {
        vec![ToolContract {
            tool_name: "background_task".into(),
            input_schema: json!({"type":"object","description":"Inspect or cancel this session's background tools. Results are collected silently; completed finite batches continue the Agent automatically while the host remains running. A running receipt is not success. Never resubmit a tool to poll its result.","properties":{"action":{"type":"string","enum":["list","read","cancel"]},"task_id":{"type":"string","format":"uuid"}},"required":["action"],"additionalProperties":false}),
            output_schema: json!({"type":"object"}),
            error_schema: json!({"type":"object"}),
            side_effect_type: SideEffectType::ExternalSystem,
            idempotency_key_policy: "session_scoped_task_id".into(),
            timeout_policy: "immediate_control".into(),
            cancellation_policy: "cancel_managed_task_only".into(),
            retry_policy: "never_replays_original_operation".into(),
            artifact_policy: "bounded_result_summary".into(),
            permission_policy_ref: None,
        }]
    }

    fn capabilities(&self) -> HashMap<String, ToolCapabilities> {
        HashMap::from([(
            "background_task".into(),
            ToolCapabilities {
                available_in_coding_profile: true,
                parallel_read_safe: false,
                coding_profile_hidden_arguments: Vec::new(),
            },
        )])
    }

    async fn call(
        &self,
        request: &ToolRequest,
        _: CancellationToken,
    ) -> Result<ExternalToolOutput, ToolError> {
        let action = request.arguments["action"].as_str().unwrap_or_default();
        let value = if action == "list" {
            let tasks = self
                .0
                .list(request.session_id)
                .into_iter()
                .map(|mut task| {
                    task.result = None;
                    task
                })
                .collect::<Vec<_>>();
            json!({"tasks":tasks})
        } else {
            if !matches!(action, "read" | "cancel") {
                return Err(ToolError::InvalidArguments(
                    "action must be list, read or cancel".into(),
                ));
            }
            let id = request.arguments["task_id"]
                .as_str()
                .ok_or_else(|| ToolError::InvalidArguments("task_id is required".into()))?
                .parse::<ToolCallId>()
                .map_err(|_| ToolError::InvalidArguments("invalid background task_id".into()))?;
            let Some(status) = self.0.status(request.session_id, id) else {
                return Err(ToolError::Execution("background task is not retained in this session; inspect persisted session history".into()));
            };
            // Delivery may already have completed and queued a continuation. The
            // explicit cancel must revoke that batch even when no process remains.
            if action == "cancel" {
                self.1
                    .cancel(request.session_id, status.turn_id, id)
                    .await?;
            }
            let cancelled = action == "cancel" && self.0.cancel(request.session_id, id);
            json!({"task":self.0.status(request.session_id,id),"cancel_requested":cancelled})
        };
        Ok(ExternalToolOutput {
            summary: "background task observation".into(),
            content: value.to_string(),
            structured_facts: value,
            is_error: false,
            images: Vec::new(),
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BackgroundBinding {
    pub supervisor: BackgroundToolSupervisor,
    pub observer: Arc<dyn BackgroundToolObserver>,
}

pub(crate) fn enable_browser_background(contract: &mut ToolContract) {
    if contract.tool_name == "browser_run" {
        contract.input_schema["properties"]["background"] = json!({
            "type":"boolean",
            "description":"Run this bounded operation in the background. Returns a task ID; results are collected silently. Do not resubmit to poll."
        });
    }
}

pub(crate) fn requested(invocation: &ToolInvocation) -> bool {
    matches!(
        invocation.request.tool_name.as_str(),
        "shell" | "browser_run"
    ) && invocation.request.arguments["background"] == true
        && (invocation.policy.decision == PolicyDecision::Allow
            || (invocation.approved && invocation.policy.decision == PolicyDecision::Ask))
}

#[cfg(test)]
mod tests;
