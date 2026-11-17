use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use golutra_agent_policy::WorkspacePolicy;

#[tokio::test]
async fn external_children_share_delivery_but_never_reserve_tool_execution_slots() {
    let supervisor = BackgroundToolSupervisor::new();
    let observer = Arc::new(Observer::default());
    let session = SessionId::new();
    let mut registrations = Vec::new();
    for _ in 0..MAX_ACTIVE {
        let call = request(session, "subagent", json!({"task":"inspect"}));
        let registration = supervisor
            .register_external(call.clone(), CancellationToken::new(), observer.clone())
            .await
            .unwrap();
        registrations.push((call, registration));
    }
    assert_eq!(supervisor.inner.slots.available_permits(), MAX_ACTIVE);
    assert!(supervisor.has_pending(session, None));
    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new(WorkspacePolicy::new(root.path()).unwrap());
    for (call, registration) in registrations {
        let policy = runtime
            .evaluate(&call)
            .unwrap_or_else(|_| PolicyEvaluation {
                policy_ref: golutra_agent_core::PolicyId::new(),
                subject: "tool".into(),
                action: "subagent".into(),
                resource: "child".into(),
                decision: PolicyDecision::Allow,
                block_disposition: None,
                reason: "fixture".into(),
                evidence_refs: vec![],
            });
        registration
            .complete(success_report(
                call,
                "done",
                json!({"child_terminal":true}),
                "done".into(),
                vec![],
                policy,
            ))
            .await;
    }
    assert!(!supervisor.has_pending(session, None));
    assert_eq!(observer.records.lock().unwrap().len(), MAX_ACTIVE);
    assert_eq!(supervisor.inner.slots.available_permits(), MAX_ACTIVE);
}

#[derive(Debug, Default)]
struct Observer {
    admissions: AtomicUsize,
    deliveries: AtomicUsize,
    failures: AtomicUsize,
    reject: AtomicBool,
    records: Mutex<Vec<ToolExecutionReport>>,
    cancellations: Mutex<Vec<(SessionId, Option<TurnId>, ToolCallId)>>,
    admission_gate: Option<Arc<Semaphore>>,
}

#[async_trait]
impl BackgroundToolObserver for Observer {
    async fn admit(&self, _: &ToolRequest) -> Result<(), ToolError> {
        self.admissions.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.admission_gate {
            gate.acquire().await.unwrap().forget();
        }
        if self.reject.load(Ordering::SeqCst) {
            return Err(ToolError::Execution("durable admission rejected".into()));
        }
        Ok(())
    }

    async fn complete(
        &self,
        _: &ToolRequest,
        report: &ToolExecutionReport,
    ) -> Result<(), ToolError> {
        self.deliveries.fetch_add(1, Ordering::SeqCst);
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(ToolError::Execution(
                "storage temporarily unavailable".into(),
            ));
        }
        self.records.lock().unwrap().push(report.clone());
        Ok(())
    }

    async fn seal(&self, _: SessionId, _: Option<TurnId>) -> Result<(), ToolError> {
        Ok(())
    }

    async fn cancel(
        &self,
        session: SessionId,
        turn: Option<TurnId>,
        tool_call: ToolCallId,
    ) -> Result<(), ToolError> {
        self.cancellations
            .lock()
            .unwrap()
            .push((session, turn, tool_call));
        Ok(())
    }
}

#[derive(Debug)]
struct Browser {
    calls: AtomicUsize,
    started: Semaphore,
    release: Semaphore,
    cancellation_needs_cleanup: bool,
}

impl Browser {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            started: Semaphore::new(0),
            release: Semaphore::new(0),
            cancellation_needs_cleanup: false,
        }
    }
}

#[async_trait]
impl ExternalToolBackend for Browser {
    fn contracts(&self) -> Vec<ToolContract> {
        let mut config = crate::browser::browser_contract("browser_config");
        config.input_schema = json!({"type":"object","properties":{"browser_backend":{"type":"string"}},"additionalProperties":false});
        vec![crate::browser::browser_contract("browser_run"), config]
    }

    fn timeout(&self, _: &ToolRequest) -> Duration {
        Duration::from_secs(10)
    }

    async fn call(
        &self,
        request: &ToolRequest,
        cancel: CancellationToken,
    ) -> Result<ExternalToolOutput, ToolError> {
        if request.tool_name == "browser_config" {
            return Ok(ExternalToolOutput {
                summary: "selection fixture".into(),
                content: String::new(),
                structured_facts: json!({"selection_changed":true}),
                is_error: false,
                images: vec![],
            });
        }
        assert!(
            request.arguments.get("background").is_none(),
            "supervisor option must not reach SDK"
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.add_permits(1);
        tokio::select! {
            () = cancel.cancelled() => {
                if self.cancellation_needs_cleanup {
                    self.release.acquire().await.unwrap().forget();
                }
                return Err(ToolError::Execution("cancelled".into()));
            },
            permit = self.release.acquire() => permit.unwrap().forget(),
        }
        Ok(ExternalToolOutput {
            summary: "browser finished".into(),
            content: "observed result".into(),
            structured_facts: json!({"observed":true}),
            is_error: false,
            images: vec![],
        })
    }
}

fn request(session: SessionId, tool: &str, arguments: Value) -> ToolRequest {
    ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: None,
        session_id: session,
        turn_id: Some(TurnId::new()),
        tool_name: tool.into(),
        arguments,
    }
}

fn runtime(
    root: &std::path::Path,
    supervisor: &BackgroundToolSupervisor,
    observer: Arc<Observer>,
    browser: Arc<Browser>,
) -> ToolRuntime {
    ToolRuntime::new(WorkspacePolicy::new(root).unwrap())
        .with_unrestricted_access(true)
        .with_external_backend(browser)
        .unwrap()
        .with_background_supervisor(supervisor.clone(), observer)
        .unwrap()
}

async fn settled(supervisor: &BackgroundToolSupervisor, session: SessionId) {
    let mut activity = supervisor.subscribe();
    tokio::time::timeout(Duration::from_secs(15), async {
        while supervisor.has_pending(session, None) {
            activity.changed().await.unwrap();
        }
    })
    .await
    .expect("background job did not durably settle");
}

async fn started(browser: &Browser) {
    tokio::time::timeout(Duration::from_secs(10), browser.started.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
}

#[tokio::test]
async fn result_delivery_retries_do_not_repeat_browser_effects() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = BackgroundToolSupervisor::new();
    let observer = Arc::new(Observer::default());
    observer.failures.store(2, Ordering::SeqCst);
    let browser = Arc::new(Browser::new());
    let runtime = runtime(root.path(), &supervisor, observer.clone(), browser.clone());
    let call = request(
        SessionId::new(),
        "browser_run",
        json!({"code":"click()","background":true}),
    );
    let initial = runtime
        .execute(call.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        initial.envelope.structured_facts["background_task_id"],
        call.tool_call_id.to_string()
    );
    assert_eq!(initial.envelope.structured_facts["terminal"], false);
    started(&browser).await;
    assert!(supervisor.has_pending(call.session_id, call.turn_id));
    let duplicate = runtime
        .execute(call.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        duplicate.envelope.structured_facts["background_task_id"],
        call.tool_call_id.to_string()
    );
    browser.release.add_permits(1);
    settled(&supervisor, call.session_id).await;
    assert_eq!(browser.calls.load(Ordering::SeqCst), 1);
    assert_eq!(observer.admissions.load(Ordering::SeqCst), 1);
    assert_eq!(observer.deliveries.load(Ordering::SeqCst), 3);
    assert_eq!(observer.records.lock().unwrap().len(), 1);
    assert_eq!(
        supervisor
            .status(call.session_id, call.tool_call_id)
            .unwrap()
            .status,
        BackgroundTaskStatus::Succeeded
    );
    supervisor.shutdown_and_wait().await;
}

#[tokio::test]
async fn cancelled_caller_during_durable_admission_leaves_a_terminal_record() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = BackgroundToolSupervisor::new();
    let gate = Arc::new(Semaphore::new(0));
    let observer = Arc::new(Observer {
        admission_gate: Some(gate.clone()),
        ..Observer::default()
    });
    let browser = Arc::new(Browser::new());
    let runtime = runtime(root.path(), &supervisor, observer.clone(), browser.clone());
    let call = request(
        SessionId::new(),
        "browser_run",
        json!({"code":"click()","background":true}),
    );
    let cancellation = CancellationToken::new();
    let owned_call = call.clone();
    let owned_cancel = cancellation.clone();
    let submitting = tokio::spawn(async move { runtime.execute(owned_call, owned_cancel).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while observer.admissions.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cancellation.cancel();
    submitting.abort();
    let _ = submitting.await;
    gate.add_permits(1);
    settled(&supervisor, call.session_id).await;
    assert_eq!(browser.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        observer.records.lock().unwrap()[0].envelope.status,
        ToolResultStatus::Cancelled
    );
    supervisor.shutdown_and_wait().await;
}

#[tokio::test]
async fn control_handles_are_published_only_after_durable_admission() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = BackgroundToolSupervisor::new();
    let gate = Arc::new(Semaphore::new(0));
    let observer = Arc::new(Observer {
        admission_gate: Some(gate.clone()),
        ..Observer::default()
    });
    let browser = Arc::new(Browser::new());
    let runtime = runtime(root.path(), &supervisor, observer.clone(), browser.clone());
    let call = request(
        SessionId::new(),
        "browser_run",
        json!({"code":"click()","background":true}),
    );
    let submitting = {
        let runtime = runtime.clone();
        let call = call.clone();
        tokio::spawn(async move { runtime.execute(call, CancellationToken::new()).await })
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while observer.admissions.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    // Internal ownership includes the admission phase, but a control handle cannot
    // be published while the host's durable task record may still be absent.
    assert!(supervisor.has_pending(call.session_id, call.turn_id));
    assert!(supervisor.list(call.session_id).is_empty());
    assert!(
        supervisor
            .status(call.session_id, call.tool_call_id)
            .is_none()
    );
    let listed = runtime
        .execute(
            request(call.session_id, "background_task", json!({"action":"list"})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(listed.envelope.structured_facts["tasks"], json!([]));
    assert!(!submitting.is_finished());
    assert!(observer.cancellations.lock().unwrap().is_empty());

    gate.add_permits(1);
    let receipt = submitting.await.unwrap().unwrap();
    assert_eq!(
        receipt.envelope.structured_facts["background_task_id"],
        call.tool_call_id.to_string()
    );
    assert!(
        supervisor
            .status(call.session_id, call.tool_call_id)
            .is_some()
    );
    assert_eq!(supervisor.list(call.session_id).len(), 1);
    started(&browser).await;
    let stopped = runtime
        .execute(
            request(
                call.session_id,
                "background_task",
                json!({"action":"cancel","task_id":call.tool_call_id}),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(stopped.envelope.structured_facts["cancel_requested"], true);
    assert_eq!(
        *observer.cancellations.lock().unwrap(),
        vec![(call.session_id, call.turn_id, call.tool_call_id)]
    );
    settled(&supervisor, call.session_id).await;
    assert!(!supervisor.has_pending(call.session_id, call.turn_id));
    supervisor.shutdown_and_wait().await;
}

#[tokio::test]
async fn failed_admission_never_executes_and_reused_call_id_cannot_change_arguments() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = BackgroundToolSupervisor::new();
    let observer = Arc::new(Observer::default());
    observer.reject.store(true, Ordering::SeqCst);
    let browser = Arc::new(Browser::new());
    let runtime = runtime(root.path(), &supervisor, observer.clone(), browser.clone());
    let call = request(
        SessionId::new(),
        "browser_run",
        json!({"code":"click()","background":true}),
    );
    assert!(
        runtime
            .execute(call.clone(), CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(browser.calls.load(Ordering::SeqCst), 0);
    assert!(!supervisor.has_pending(call.session_id, None));
    observer.reject.store(false, Ordering::SeqCst);
    runtime
        .execute(call.clone(), CancellationToken::new())
        .await
        .unwrap();
    let mut changed = call.clone();
    changed.arguments["code"] = json!("different()");
    assert!(
        runtime
            .execute(changed, CancellationToken::new())
            .await
            .is_err()
    );
    supervisor.cancel_session(call.session_id);
    settled(&supervisor, call.session_id).await;
    assert!(browser.calls.load(Ordering::SeqCst) <= 1);
    supervisor.shutdown_and_wait().await;
}

#[tokio::test]
async fn synchronous_browser_calls_wait_for_background_resource_ownership() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = BackgroundToolSupervisor::new();
    let observer = Arc::new(Observer::default());
    let browser = Arc::new(Browser::new());
    let runtime = runtime(root.path(), &supervisor, observer, browser.clone());
    let first = request(
        SessionId::new(),
        "browser_run",
        json!({"code":"click()","background":true}),
    );
    runtime
        .execute(first.clone(), CancellationToken::new())
        .await
        .unwrap();
    started(&browser).await;
    let other = request(SessionId::new(), "browser_run", json!({"code":"read()"}));
    let next = tokio::spawn(async move { runtime.execute(other, CancellationToken::new()).await });
    tokio::task::yield_now().await;
    assert_eq!(browser.calls.load(Ordering::SeqCst), 1);
    assert!(!next.is_finished());
    browser.release.add_permits(1);
    settled(&supervisor, first.session_id).await;
    started(&browser).await;
    browser.release.add_permits(1);
    assert_eq!(
        next.await.unwrap().unwrap().envelope.status,
        ToolResultStatus::Ok
    );
    assert_eq!(browser.calls.load(Ordering::SeqCst), 2);
    supervisor.shutdown_and_wait().await;
}

#[tokio::test]
async fn queued_browser_work_blocks_selection_before_backend_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = BackgroundToolSupervisor::new();
    let browser = Arc::new(Browser::new());
    let runtime = runtime(
        root.path(),
        &supervisor,
        Arc::new(Observer::default()),
        browser.clone(),
    );
    let occupied = supervisor.inner.browser_slots.acquire().await.unwrap();
    let first = request(
        SessionId::new(),
        "browser_run",
        json!({"code":"queued()","background":true}),
    );
    runtime
        .execute(first.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert!(supervisor.has_pending(first.session_id, None));
    assert_eq!(browser.calls.load(Ordering::SeqCst), 0);
    let select = request(
        first.session_id,
        "browser_config",
        json!({"browser_backend":"ego"}),
    );
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        runtime.execute(select.clone(), CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.envelope.status, ToolResultStatus::Error);
    assert_eq!(result.envelope.structured_facts["code"], "browser_busy");
    assert_eq!(result.envelope.structured_facts["selection_changed"], false);
    supervisor.cancel_session(first.session_id);
    drop(occupied);
    settled(&supervisor, first.session_id).await;
    let result = runtime
        .execute(
            ToolRequest {
                tool_call_id: ToolCallId::new(),
                ..select
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.envelope.structured_facts["selection_changed"], true);
    supervisor.shutdown_and_wait().await;
}

#[tokio::test]
async fn browser_resource_permit_outlives_the_callers_cancellation_cleanup_window() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = BackgroundToolSupervisor::new();
    let observer = Arc::new(Observer::default());
    let mut backend = Browser::new();
    backend.cancellation_needs_cleanup = true;
    let browser = Arc::new(backend);
    let runtime = runtime(root.path(), &supervisor, observer, browser.clone());
    let first = request(
        SessionId::new(),
        "browser_run",
        json!({"code":"click()","background":true}),
    );
    let cancellation = CancellationToken::new();
    runtime
        .execute(first.clone(), cancellation.clone())
        .await
        .unwrap();
    started(&browser).await;
    cancellation.cancel();
    settled(&supervisor, first.session_id).await;
    let next_call = request(SessionId::new(), "browser_run", json!({"code":"read()"}));
    let next =
        tokio::spawn(async move { runtime.execute(next_call, CancellationToken::new()).await });
    tokio::task::yield_now().await;
    assert_eq!(browser.calls.load(Ordering::SeqCst), 1);
    assert!(!next.is_finished());
    browser.release.add_permits(1);
    started(&browser).await;
    browser.release.add_permits(1);
    assert_eq!(
        next.await.unwrap().unwrap().envelope.status,
        ToolResultStatus::Ok
    );
    supervisor.shutdown_and_wait().await;
}

#[tokio::test]
async fn controls_are_session_scoped_and_cancel_pending_delivery_without_replay() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = BackgroundToolSupervisor::new();
    let observer = Arc::new(Observer::default());
    let browser = Arc::new(Browser::new());
    let runtime = runtime(root.path(), &supervisor, observer, browser.clone());
    let call = request(
        SessionId::new(),
        "browser_run",
        json!({"code":"click()","background":true}),
    );
    runtime
        .execute(call.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert!(
        supervisor
            .status(SessionId::new(), call.tool_call_id)
            .is_none()
    );
    assert!(!supervisor.cancel(SessionId::new(), call.tool_call_id));
    let control = request(
        call.session_id,
        "background_task",
        json!({"action":"cancel","task_id":call.tool_call_id}),
    );
    let result = runtime
        .execute(control, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.envelope.structured_facts["cancel_requested"], true);
    settled(&supervisor, call.session_id).await;
    let status = supervisor
        .status(call.session_id, call.tool_call_id)
        .unwrap();
    assert!(matches!(
        status.status,
        BackgroundTaskStatus::Cancelled | BackgroundTaskStatus::OutcomeUnknown
    ));
    assert!(browser.calls.load(Ordering::SeqCst) <= 1);
    supervisor.shutdown_and_wait().await;
}

#[tokio::test]
async fn cancel_after_completion_revokes_wake_without_rewriting_result() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = BackgroundToolSupervisor::new();
    let observer = Arc::new(Observer::default());
    let browser = Arc::new(Browser::new());
    let runtime = runtime(root.path(), &supervisor, observer.clone(), browser.clone());
    let call = request(
        SessionId::new(),
        "browser_run",
        json!({"code":"click()","background":true}),
    );
    browser.release.add_permits(1);
    runtime
        .execute(call.clone(), CancellationToken::new())
        .await
        .unwrap();
    settled(&supervisor, call.session_id).await;
    let result = runtime
        .execute(
            request(
                call.session_id,
                "background_task",
                json!({"action":"cancel","task_id":call.tool_call_id}),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.envelope.structured_facts["cancel_requested"], false);
    assert_eq!(
        result.envelope.structured_facts["task"]["status"],
        "succeeded"
    );
    assert_eq!(
        *observer.cancellations.lock().unwrap(),
        vec![(call.session_id, call.turn_id, call.tool_call_id)]
    );
    assert_eq!(browser.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        supervisor
            .take_completed_report(call.session_id, call.tool_call_id)
            .unwrap()
            .envelope
            .status,
        ToolResultStatus::Ok
    );
    supervisor.shutdown_and_wait().await;
}

#[cfg(unix)]
#[tokio::test]
async fn shell_background_uses_existing_process_and_reports_real_terminal_output() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = BackgroundToolSupervisor::new();
    let observer = Arc::new(Observer::default());
    let runtime = runtime(
        root.path(),
        &supervisor,
        observer.clone(),
        Arc::new(Browser::new()),
    );
    let call = request(
        SessionId::new(),
        "shell",
        json!({"command":"printf x >> effect.txt; sleep 0.05; printf finished","background":true,"timeout_ms":5000}),
    );
    let initial = runtime
        .execute(call.clone(), CancellationToken::new())
        .await
        .unwrap();
    let projection = model_visible_tool_result(&initial.envelope);
    assert!(projection.contains("background_task_id"));
    assert!(projection.contains(&call.tool_call_id.to_string()));
    settled(&supervisor, call.session_id).await;
    {
        let records = observer.records.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].envelope.structured_facts["terminal"], true);
        assert_eq!(records[0].envelope.structured_facts["exit_code"], 0);
        assert!(
            records[0]
                .envelope
                .model_visible_excerpt
                .as_ref()
                .unwrap()
                .contains("finished")
        );
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("effect.txt")).unwrap(),
        "x"
    );
    let retained = runtime
        .take_background_completed_report(call.session_id, call.tool_call_id)
        .unwrap();
    assert!(retained.before_images.is_empty());
    assert!(retained.after_images.is_empty());
    assert!(
        retained
            .artifact_contents
            .iter()
            .any(|artifact| String::from_utf8_lossy(&artifact.bytes).contains("finished"))
    );
    assert!(
        runtime
            .take_background_completed_report(call.session_id, call.tool_call_id)
            .is_none()
    );
    supervisor.shutdown_and_wait().await;
}

#[tokio::test]
async fn expired_job_deadline_prevents_browser_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = BackgroundToolSupervisor::new();
    let observer = Arc::new(Observer::default());
    let browser = Arc::new(Browser::new());
    let runtime = runtime(root.path(), &supervisor, observer.clone(), browser.clone());
    let call = request(
        SessionId::new(),
        "browser_run",
        json!({"code":"click()","background":true}),
    );
    let policy = runtime.evaluate(&call).unwrap();
    runtime
        .invoke(
            ToolInvocation::new(call.clone(), policy, false)
                .with_deadline(tokio::time::Instant::now()),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    settled(&supervisor, call.session_id).await;
    assert_eq!(browser.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        observer.records.lock().unwrap()[0].envelope.status,
        ToolResultStatus::Timeout
    );
    supervisor.shutdown_and_wait().await;
}

#[tokio::test]
async fn registration_order_and_false_background_option_keep_short_calls_synchronous() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = BackgroundToolSupervisor::new();
    let observer = Arc::new(Observer::default());
    let browser = Arc::new(Browser::new());
    browser.release.add_permits(1);
    let runtime = ToolRuntime::new(WorkspacePolicy::new(root.path()).unwrap())
        .with_unrestricted_access(true)
        .with_background_supervisor(supervisor.clone(), observer.clone())
        .unwrap()
        .with_external_backend(browser.clone())
        .unwrap();
    let call = request(
        SessionId::new(),
        "browser_run",
        json!({"code":"read()","background":false}),
    );
    let report = runtime
        .execute(call, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.envelope.status, ToolResultStatus::Ok);
    assert_eq!(browser.calls.load(Ordering::SeqCst), 1);
    assert_eq!(observer.admissions.load(Ordering::SeqCst), 0);
    supervisor.shutdown_and_wait().await;
}

#[test]
fn oversized_verification_output_is_explicitly_incomplete_instead_of_sliced() {
    let call = request(SessionId::new(), "shell", json!({"command":"test"}));
    let policy = crate::execution_policy(&call, PolicyDecision::Allow, "test");
    let report = success_report(
        call,
        "success",
        json!({"exit_code":0}),
        "x".repeat(MAX_VERIFICATION_OUTPUT_BYTES + 1),
        vec![],
        policy,
    );
    let compact = verification_report(&report);
    assert!(compact.artifact_contents.is_empty());
    assert_eq!(
        compact.envelope.structured_facts["background_verification_output_incomplete"],
        true
    );
    assert!(!report.artifact_contents.is_empty());
}

#[test]
fn retained_screenshots_keep_bytes_and_image_budget_is_released() {
    let call = request(
        SessionId::new(),
        "browser_run",
        json!({"code":"screenshot()"}),
    );
    let policy = crate::execution_policy(&call, PolicyDecision::Allow, "test");
    let mut report = success_report(
        call.clone(),
        "screenshot",
        json!({}),
        String::new(),
        vec![],
        policy,
    );
    let bytes = b"\x89PNG\r\n\x1a\nfixture".to_vec();
    crate::append_external_images(
        &mut report,
        &call,
        vec![crate::ExternalToolImage {
            mime_type: "image/png".into(),
            bytes: bytes.clone(),
        }],
    )
    .unwrap();
    let compact = verification_report(&report);
    assert!(
        compact
            .artifact_contents
            .iter()
            .any(|content| content.bytes == bytes)
    );
    assert!(
        compact
            .envelope
            .structured_facts
            .get("background_image_omitted")
            .is_none()
    );
    let used = Arc::new(AtomicUsize::new(0));
    let reservation = ImageRetention::reserve(&used, MAX_RETAINED_IMAGE_BYTES).unwrap();
    assert!(ImageRetention::reserve(&used, 1).is_none());
    drop(reservation);
    assert_eq!(used.load(Ordering::Acquire), 0);
    assert!(ImageRetention::reserve(&used, 1).is_some());
}

#[test]
fn unknown_effects_are_not_reported_as_success() {
    let call = request(SessionId::new(), "browser_run", json!({"code":"click()"}));
    let policy = crate::execution_policy(&call, PolicyDecision::Allow, "test");
    let report = success_report(
        call,
        "uncertain",
        json!({"outcome_unknown":true}),
        String::new(),
        vec![],
        policy,
    );
    assert_eq!(
        background_report_status(&report),
        BackgroundTaskStatus::OutcomeUnknown
    );
}

#[test]
fn browser_unknown_error_codes_preserve_uncertainty() {
    for code in [
        "opencli_outcome_unknown",
        "browser_outcome_unknown",
        "browser_launch_unknown",
    ] {
        let call = request(SessionId::new(), "browser_run", json!({"action":"open"}));
        let policy = crate::execution_policy(&call, PolicyDecision::Allow, "test");
        let mut report = success_report(
            call,
            "browser may already have acted",
            json!({"code":code,"automatic_retry":false}),
            String::new(),
            vec![],
            policy,
        );
        report.envelope.status = ToolResultStatus::Error;
        assert_eq!(
            background_report_status(&report),
            BackgroundTaskStatus::OutcomeUnknown,
            "{code} must not be flattened into a safely retryable failure"
        );
        report.envelope.structured_facts = json!({"code":"opencli_unavailable"});
        assert_eq!(
            background_report_status(&report),
            BackgroundTaskStatus::Failed
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn queued_browser_jobs_do_not_starve_independent_shell_work() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = BackgroundToolSupervisor::new();
    let observer = Arc::new(Observer::default());
    let browser = Arc::new(Browser::new());
    let runtime = runtime(root.path(), &supervisor, observer, browser.clone());
    let browser_session = SessionId::new();
    let cancel = CancellationToken::new();
    let first = request(
        browser_session,
        "browser_run",
        json!({"code":"click()","background":true}),
    );
    runtime.execute(first, cancel.clone()).await.unwrap();
    started(&browser).await;
    for _ in 1..MAX_ACTIVE {
        runtime
            .execute(
                request(
                    browser_session,
                    "browser_run",
                    json!({"code":"click()","background":true}),
                ),
                cancel.clone(),
            )
            .await
            .unwrap();
    }
    let shell = request(
        SessionId::new(),
        "shell",
        json!({"command":"printf independent","background":true,"timeout_ms":5000}),
    );
    runtime
        .execute(shell.clone(), CancellationToken::new())
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        settled(&supervisor, shell.session_id),
    )
    .await
    .expect("browser queue starved shell execution");
    assert_eq!(
        supervisor
            .status(shell.session_id, shell.tool_call_id)
            .unwrap()
            .status,
        BackgroundTaskStatus::Succeeded
    );
    assert_eq!(browser.calls.load(Ordering::SeqCst), 1);
    cancel.cancel();
    settled(&supervisor, browser_session).await;
    supervisor.shutdown_and_wait().await;
}

#[tokio::test]
async fn capacity_does_not_evict_unconsumed_verification_reports() {
    let root = tempfile::tempdir().unwrap();
    let supervisor = BackgroundToolSupervisor::new();
    let observer = Arc::new(Observer::default());
    let browser = Arc::new(Browser::new());
    browser.release.add_permits(MAX_RETAINED);
    let runtime = runtime(root.path(), &supervisor, observer.clone(), browser.clone());
    let session = SessionId::new();
    let mut first = None;
    for _ in 0..MAX_RETAINED {
        let call = request(
            session,
            "browser_run",
            json!({"code":"read()","background":true}),
        );
        first.get_or_insert(call.tool_call_id);
        runtime
            .execute(call, CancellationToken::new())
            .await
            .unwrap();
    }
    settled(&supervisor, session).await;
    let extra = request(
        session,
        "browser_run",
        json!({"code":"read()","background":true}),
    );
    assert!(
        runtime
            .execute(extra.clone(), CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(observer.admissions.load(Ordering::SeqCst), MAX_RETAINED);
    assert!(
        runtime
            .take_background_completed_report(session, first.unwrap())
            .is_some()
    );
    browser.release.add_permits(1);
    runtime
        .execute(extra, CancellationToken::new())
        .await
        .unwrap();
    settled(&supervisor, session).await;
    assert_eq!(
        supervisor.discard_completed_reports(session, None),
        MAX_RETAINED
    );
    assert_eq!(supervisor.discard_completed_reports(session, None), 0);
    supervisor.shutdown_and_wait().await;
}
