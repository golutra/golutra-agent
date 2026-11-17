use super::*;
use chrono::Utc;
use golutra_agent_runtime::ConfiguredAgentRun;
use golutra_agent_tools::TaskDelegationBackend;

#[derive(Debug)]
struct MixedBrowser {
    calls: std::sync::atomic::AtomicUsize,
}
#[async_trait]
impl golutra_agent_tools::ExternalToolBackend for MixedBrowser {
    fn capabilities(&self) -> HashMap<String, golutra_agent_tools::ToolCapabilities> {
        HashMap::from([(
            "browser_run".into(),
            golutra_agent_tools::ToolCapabilities {
                available_in_coding_profile: true,
                ..Default::default()
            },
        )])
    }
    fn contracts(&self) -> Vec<golutra_agent_core::ToolContract> {
        let mut contract = golutra_agent_tools::ToolRegistry::default()
            .contract("shell")
            .unwrap()
            .clone();
        contract.tool_name = "browser_run".into();
        contract.side_effect_type = golutra_agent_core::SideEffectType::ExternalSystem;
        contract.input_schema = json!({"type":"object","properties":{"code":{"type":"string"}},"required":["code"],"additionalProperties":false});
        vec![contract]
    }
    async fn call(
        &self,
        _request: &ToolRequest,
        _cancel: CancellationToken,
    ) -> Result<golutra_agent_tools::ExternalToolOutput, golutra_agent_tools::ToolError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(golutra_agent_tools::ExternalToolOutput {
            summary: "browser fixture failed once".into(),
            content: "no browser side effect was repeated".into(),
            structured_facts: json!({"fixture_failure":true}),
            is_error: true,
            images: vec![],
        })
    }
}

struct MixedProvider {
    calls: std::sync::atomic::AtomicUsize,
    requests: Arc<std::sync::Mutex<Vec<golutra_agent_llm::ProviderRequest>>>,
}
#[async_trait]
impl golutra_agent_llm::LlmProvider for MixedProvider {
    fn contract(&self) -> golutra_agent_core::ProviderContract {
        golutra_agent_llm::LlmProvider::contract(&MockProvider::text_response("done"))
    }
    async fn complete(
        &self,
        request: golutra_agent_llm::ProviderRequest,
    ) -> Result<golutra_agent_llm::ProviderResponse, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        let mut response = golutra_agent_llm::LlmProvider::complete(
            &MockProvider::text_response("Collected the available observations."),
            request,
        )
        .await?;
        let step = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(step < 6, "completion must not repeatedly wake the parent");
        if step == 0 {
            response.message = None;
            response.finish_reason = golutra_agent_llm::ProviderFinishReason::ToolCalls;
            response.tool_calls = vec![
                ProviderToolCall {
                    tool_call_id: "shell".into(),
                    tool_name: "shell".into(),
                    arguments: json!({"command":"printf x >> effect.txt","background":true}),
                },
                ProviderToolCall {
                    tool_call_id: "browser".into(),
                    tool_name: "browser_run".into(),
                    arguments: json!({"code":"return 'fixture'","background":true}),
                },
                ProviderToolCall {
                    tool_call_id: "child-a".into(),
                    tool_name: "subagent".into(),
                    arguments: json!({"name":"alpha","task":"Summarize alpha","run_in_background":true}),
                },
                ProviderToolCall {
                    tool_call_id: "child-b".into(),
                    tool_name: "subagent".into(),
                    arguments: json!({"name":"beta","task":"List the directory","run_in_background":true}),
                },
            ];
        }
        Ok(response)
    }
}

#[tokio::test]
async fn mixed_subagent_shell_browser_batch_preserves_scope_and_failed_results_without_reexecution()
{
    let _config = IsolatedGlobalMockProvider::install().await;
    let root = tempdir().unwrap();
    let host = RuntimeHost::ephemeral_for_cwd(root.path()).await.unwrap();
    let (task, _, control) = parent_fixture(&host).await;
    let browser = Arc::new(MixedBrowser { calls: 0.into() });
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observer = crate::background::HostedBackgroundObserver::new(&host, task.clone());
    let executor =
        golutra_agent_tools::ToolRuntime::new(WorkspacePolicy::new(root.path()).unwrap())
            .with_unrestricted_access(true)
            .with_process_supervisor(host.execution.process_supervisor.clone())
            .with_external_backend(browser.clone())
            .unwrap()
            .with_task_delegation_backend(Arc::new(
                crate::delegation::RuntimeTaskDelegationBackend::new(Arc::downgrade(&host)),
            ))
            .unwrap()
            .with_background_supervisor(host.execution.background_supervisor.clone(), observer)
            .unwrap();
    let harness = AgentHarness::new(
        MixedProvider {
            calls: 0.into(),
            requests: requests.clone(),
        },
        ContextBuilder::default(),
        executor,
    );
    let recorder = crate::execution::HostedObservationRecorder::spawn(host.clone(), task.clone());
    let sender = recorder.sender();
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        harness.execute_configured(
            ConfiguredAgentRun::new(AgentTaskRequest {
                session_id: task.session_id,
                task_id: task.task_id,
                turn_id: task.turn_id,
                objective: "Collect the independent task results".into(),
                completion_criteria: vec![],
                output_schema: None,
                touched_code: false,
                contributors: vec![],
                tools: vec!["shell".into(), "browser_run".into(), "subagent".into()],
            })
            .with_execution_mode(Some(golutra_agent_protocol::AgentExecutionMode::Open))
            .with_max_elapsed_ms(25_000),
            control,
            move |event| {
                sender.send(event).unwrap();
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    recorder.close().await.unwrap();
    assert_eq!(outcome.final_turn_id, task.turn_id);
    assert_eq!(
        fs::read_to_string(root.path().join("effect.txt")).unwrap(),
        "x"
    );
    assert_eq!(browser.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        outcome
            .tool_reports
            .iter()
            .any(|report| report.envelope.tool_name == "browser_run"
                && report.envelope.status == golutra_agent_core::ToolResultStatus::Error)
    );
    let events = host
        .storage
        .repositories
        .events
        .load(task.session_id, None, None)
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::ToolStarted)
            .count(),
        4
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::BackgroundTaskUpdated)
            .count(),
        4
    );
    assert!(
        host.storage
            .store
            .background()
            .pending_results(host.workspace_id, task.session_id, 128)
            .await
            .unwrap()
            .is_empty()
    );
    {
        let requests = requests.lock().unwrap();
        let delivered = requests
            .last()
            .unwrap()
            .messages
            .iter()
            .filter(|message| message.tool_name.as_deref() == Some("background_result"))
            .collect::<Vec<_>>();
        assert_eq!(delivered.len(), 4);
        let batches = delivered
            .iter()
            .map(|message| {
                serde_json::from_str::<Value>(&message.content).unwrap()["batch_id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<HashSet<_>>();
        assert_eq!(batches.len(), 1);
    }
    host.clear_task_control(task.session_id, task.task_id).await;
    host.close().await.unwrap();
}

fn child_request(parent: SessionId, arguments: Value) -> ToolRequest {
    ToolRequest {
        tool_call_id: ToolCallId::new(),
        provider_tool_call_id: None,
        session_id: parent,
        turn_id: Some(TurnId::new()),
        tool_name: "subagent".into(),
        arguments,
    }
}

#[tokio::test]
async fn structured_assignment_is_admitted_and_preserved_on_same_session_resume() {
    let _config = IsolatedGlobalMockProvider::install().await;
    let root = tempdir().unwrap();
    let host = RuntimeHost::ephemeral_for_cwd(root.path()).await.unwrap();
    let (task, _, _control) = parent_fixture(&host).await;
    let backend = crate::delegation::RuntimeTaskDelegationBackend::new(Arc::downgrade(&host));
    let assignment =
        json!({"acceptance":["Return the requested summary"],"interfaces":"Plain text"});
    let spawned = backend
        .delegate(
            &child_request(
                task.session_id,
                json!({"task":"Summarize seven","name":"assigned",
            "tool_profile":"none","assignment":assignment}),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(spawned.status, golutra_agent_core::ToolResultStatus::Ok);
    let child = spawned.structured_facts["child_session_id"]
        .as_str()
        .unwrap()
        .parse::<SessionId>()
        .unwrap();
    let resumed = backend
        .delegate(
            &child_request(
                task.session_id,
                json!({"action":"resume","child_session_id":child,
            "task":"Summarize nine"}),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(resumed.status, golutra_agent_core::ToolResultStatus::Ok);
    assert_eq!(
        resumed.structured_facts["child_acceptance_status"],
        "requires_parent_review"
    );
    assert_eq!(
        resumed.structured_facts["child_acceptance_criteria"],
        assignment["acceptance"]
    );
    assert_eq!(
        resumed.structured_facts["child_session_id"],
        spawned.structured_facts["child_session_id"]
    );
    assert_ne!(
        resumed.structured_facts["child_task_id"],
        spawned.structured_facts["child_task_id"]
    );
    let events = host
        .storage
        .repositories
        .events
        .load(child, None, None)
        .await
        .unwrap();
    let executions: Vec<_> = events
        .iter()
        .filter(|event| event.event_type == RuntimeEventType::TaskCreated)
        .collect();
    assert_eq!(executions.len(), 2);
    for event in executions {
        assert_eq!(event.payload["payload"]["tool_profile"], "none");
        assert_eq!(
            event.payload["payload"]["_delegation_assignment"],
            assignment
        );
        assert!(
            event.payload["payload"]["prompt"]
                .as_str()
                .unwrap()
                .contains("Assignment contract")
        );
    }
    assert!(
        !events
            .iter()
            .any(|event| event.event_type == RuntimeEventType::ToolStarted)
    );
    host.clear_task_control(task.session_id, task.task_id).await;
    host.close().await.unwrap();
}

#[tokio::test]
async fn assignment_behavioral_acceptance_is_not_certified_by_a_delivery_path() {
    let _config = IsolatedGlobalMockProvider::install().await;
    let root = tempdir().unwrap();
    fs::write(
        root.path().join("audio.mjs"),
        "export const placeholder = true;\n",
    )
    .unwrap();
    let host = RuntimeHost::ephemeral_for_cwd(root.path()).await.unwrap();
    let (task, _, _control) = parent_fixture(&host).await;
    let backend = crate::delegation::RuntimeTaskDelegationBackend::new(Arc::downgrade(&host));
    let criteria = json!([
        "No eager AudioContext; play shoot/hit/kill procedurally; mute silences immediately."
    ]);
    let result = tokio::time::timeout(Duration::from_secs(5), backend.delegate(
        &child_request(task.session_id, json!({"task":"Report the supplied module for parent review",
            "tool_profile":"none","assignment":{"owned_paths":["audio.mjs"],"acceptance":criteria}})),
        CancellationToken::new(),
    )).await.expect("must not loop trying to match arbitrary prose to path evidence").unwrap();
    assert_eq!(result.status, golutra_agent_core::ToolResultStatus::Ok);
    assert_eq!(
        result.structured_facts["child_acceptance_status"],
        "requires_parent_review"
    );
    assert_eq!(
        result.structured_facts["child_acceptance_criteria"],
        criteria
    );
    host.clear_task_control(task.session_id, task.task_id).await;
    host.close().await.unwrap();
}

#[tokio::test]
async fn subagent_cancel_command_checks_parent_controller_and_exact_execution() {
    let _config = IsolatedGlobalMockProvider::install().await;
    let root = tempdir().unwrap();
    let host = RuntimeHost::ephemeral_for_cwd(root.path()).await.unwrap();
    let (task, _, _control) = parent_fixture(&host).await;
    let backend = crate::delegation::RuntimeTaskDelegationBackend::new(Arc::downgrade(&host));
    let child = backend
        .delegate(
            &child_request(
                task.session_id,
                json!({"task":"Summarize","name":"cancel-test"}),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .structured_facts["child_session_id"]
        .as_str()
        .unwrap()
        .parse::<SessionId>()
        .unwrap();
    let (handle, _control) = agent_execution_channel(8);
    let mut control = host
        .execution
        .task_controls
        .lock()
        .await
        .get(&task.session_id)
        .unwrap()
        .clone();
    let child_task = TaskId::new();
    control.task_id = child_task;
    control.execution = handle.clone();
    host.execution
        .task_controls
        .lock()
        .await
        .insert(child, control);
    let make = |actor: &str, expected: TaskId| SessionCommand {
        command_id: CommandId::new(),
        session_id: Some(task.session_id),
        kind: SessionCommandKind::Abort,
        idempotency_key: CommandId::new().to_string(),
        actor: Actor {
            kind: ActorKind::User,
            id: actor.into(),
        },
        payload: json!({"child_session_id":child,"expected_task_id":expected}),
        timestamp: Utc::now(),
    };
    assert!(
        !host
            .clone()
            .handle_command(make("wrong-controller", child_task))
            .await
            .unwrap()
            .accepted
    );
    assert!(
        !host
            .clone()
            .handle_command(make("subagent-integration", TaskId::new()))
            .await
            .unwrap()
            .accepted
    );
    assert!(!handle.cancellation_token().is_cancelled());
    assert!(
        host.clone()
            .handle_command(make("subagent-integration", child_task))
            .await
            .unwrap()
            .accepted
    );
    assert!(handle.cancellation_token().is_cancelled());
    host.execution.task_controls.lock().await.remove(&child);
    host.clear_task_control(task.session_id, task.task_id).await;
    host.close().await.unwrap();
}

#[tokio::test]
async fn subagent_question_reply_checks_execution_question_and_cancelled_state() {
    let _config = IsolatedGlobalMockProvider::install().await;
    let root = tempdir().unwrap();
    let host = RuntimeHost::ephemeral_for_cwd(root.path()).await.unwrap();
    let (parent, _, _) = parent_fixture(&host).await;
    let backend = crate::delegation::RuntimeTaskDelegationBackend::new(Arc::downgrade(&host));
    let first = backend
        .delegate(
            &child_request(
                parent.session_id,
                json!({"task":"Summarize","name":"questions"}),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let child: SessionId = first.structured_facts["child_session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let task = TaskId::new();
    let question = golutra_agent_core::QuestionId::new();
    // Persist the same terminal facts produced by the deferred-question runtime
    // test, then exercise the real admission and same-session continuation path.
    for (kind, payload) in [
        (
            RuntimeEventType::TaskCreated,
            json!({"payload":{"prompt":"Choose scope","_delegated_task":true}}),
        ),
        (
            RuntimeEventType::ToolCompleted,
            json!({"envelope":{"tool_name":"ask_user","tool_call_id":ToolCallId::new(),"structured_facts":{"needs_parent_input":true,"question_id":question,"questions":[{"id":"scope","question":"Which scope?"}]}}}),
        ),
        (RuntimeEventType::TaskCompleted, json!({"status":"blocked"})),
    ] {
        host.record_event(host_event(
            host.next_sequence_no(),
            child,
            Some(task),
            kind,
            RuntimeEventSource::Runtime,
            payload,
        ))
        .await
        .unwrap();
    }
    let reply = child_request(
        parent.session_id,
        json!({"action":"resume","child_session_id":child,"task":"Use the small scope","expected_child_task_id":task,"reply_to_question_id":question}),
    );
    let listing = backend
        .delegate(
            &child_request(parent.session_id, json!({"action":"list"})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        listing.structured_facts["children"][0]["question_id"],
        question.to_string()
    );
    for (field, value) in [
        ("expected_child_task_id", json!(TaskId::new())),
        (
            "reply_to_question_id",
            json!(golutra_agent_core::QuestionId::new()),
        ),
    ] {
        let mut stale = reply.clone();
        stale.tool_call_id = ToolCallId::new();
        stale.arguments[field] = value;
        assert!(
            backend
                .delegate(&stale, CancellationToken::new())
                .await
                .is_err()
        );
    }
    host.record_event(host_event(
        host.next_sequence_no(),
        child,
        Some(task),
        RuntimeEventType::TaskAborted,
        RuntimeEventSource::Runtime,
        json!({"status":"cancelled"}),
    ))
    .await
    .unwrap();
    let mut cancelled = reply.clone();
    cancelled.tool_call_id = ToolCallId::new();
    assert!(
        backend
            .delegate(&cancelled, CancellationToken::new())
            .await
            .is_err()
    );
    host.record_event(host_event(
        host.next_sequence_no(),
        child,
        Some(task),
        RuntimeEventType::TaskCompleted,
        RuntimeEventSource::Runtime,
        json!({"status":"blocked"}),
    ))
    .await
    .unwrap();
    let result = backend
        .delegate(&reply, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        result.structured_facts["child_session_id"],
        child.to_string()
    );
    assert_ne!(result.structured_facts["child_task_id"], task.to_string());
    assert_eq!(result.structured_facts["child_status"], "completed");
    let mut late = reply;
    late.tool_call_id = ToolCallId::new();
    assert!(
        backend
            .delegate(&late, CancellationToken::new())
            .await
            .is_err()
    );
    host.clear_task_control(parent.session_id, parent.task_id)
        .await;
    host.close().await.unwrap();
}

#[tokio::test]
async fn subagent_queue_then_continue_keeps_session_and_acknowledges_the_captured_message_once() {
    let _config = IsolatedGlobalMockProvider::install().await;
    let root = tempdir().unwrap();
    let host = RuntimeHost::ephemeral_for_cwd(root.path()).await.unwrap();
    let (task, _, _control) = parent_fixture(&host).await;
    let backend = crate::delegation::RuntimeTaskDelegationBackend::new(Arc::downgrade(&host));
    let first = backend
        .delegate(
            &child_request(
                task.session_id,
                json!({"task":"Summarize the assignment","name":"reviewer"}),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        first.structured_facts["child_status"], "completed",
        "{first:?}"
    );
    let child = first.structured_facts["child_session_id"]
        .as_str()
        .unwrap()
        .parse::<SessionId>()
        .unwrap();
    let queued = child_request(
        task.session_id,
        json!({"action":"send_input","child_session_id":child,"delivery_mode":"queue_only","task":"Remember the queued sentinel."}),
    );
    let output = backend
        .delegate(&queued, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(output.structured_facts["message_queued"], true);
    assert_eq!(
        backend
            .delegate(&queued, CancellationToken::new())
            .await
            .unwrap()
            .structured_facts["duplicate"],
        true
    );
    assert_eq!(
        host.storage
            .store
            .pending_subagent_messages(child)
            .await
            .unwrap()
            .len(),
        1
    );
    let before = host
        .storage
        .repositories
        .projections
        .state(child, None)
        .await
        .unwrap();
    assert_eq!(
        before
            .active_task_id
            .map(|task| task.to_string())
            .as_deref(),
        first.structured_facts["child_task_id"].as_str()
    );
    let continued = child_request(
        task.session_id,
        json!({"action":"send_input","child_session_id":child,"delivery_mode":"continue","task":"Use the queued information.","expected_child_task_id":first.structured_facts["child_task_id"]}),
    );
    let second = backend
        .delegate(&continued, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        second.structured_facts["child_session_id"],
        child.to_string()
    );
    assert_ne!(
        second.structured_facts["child_task_id"],
        first.structured_facts["child_task_id"]
    );
    assert!(
        host.storage
            .store
            .pending_subagent_messages(child)
            .await
            .unwrap()
            .is_empty()
    );
    let repeated = backend
        .delegate(&continued, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        second.structured_facts["child_task_id"],
        repeated.structured_facts["child_task_id"]
    );
    let mut changed = continued.clone();
    changed.arguments["task"] = json!("different task");
    assert!(
        backend
            .delegate(&changed, CancellationToken::new())
            .await
            .is_err()
    );
    let mut stale = continued.clone();
    stale.tool_call_id = ToolCallId::new();
    assert!(
        backend
            .delegate(&stale, CancellationToken::new())
            .await
            .is_err(),
        "an old execution reply cannot start a new child task"
    );
    let events = host
        .storage
        .repositories
        .events
        .load(child, None, None)
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == RuntimeEventType::TaskCreated)
            .count(),
        2
    );
    assert!(events.iter().any(|event| event.event_type
        == RuntimeEventType::ContextSnapshotCreated
        && event.payload.to_string().contains("subagent_message")));
    let listing = backend
        .delegate(
            &child_request(task.session_id, json!({"action":"list"})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        listing.structured_facts["children"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(listing.structured_facts["children"][0]["name"], "reviewer");
    host.clear_task_control(task.session_id, task.task_id).await;
    host.close().await.unwrap();
}

pub(crate) async fn parent_fixture(
    host: &Arc<RuntimeHost>,
) -> (
    HostedAgentTask,
    AgentExecutionHandle,
    golutra_agent_runtime::AgentExecutionControl,
) {
    let session = host.default_session_id();
    host.upsert_current_thread(session, &json!({"prompt":"parent fixture"}))
        .await
        .unwrap();
    let task = HostedAgentTask {
        session_id: session,
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        payload: json!({"prompt":"parent fixture","max_elapsed_ms":30_000}),
    };
    let transition = host
        .execution
        .lane_manager
        .lock()
        .await
        .start_task(
            host.workspace_id,
            session,
            task.task_id,
            task.turn_id,
            Actor {
                kind: ActorKind::User,
                id: "subagent-integration".into(),
            },
            host.next_sequence_no(),
        )
        .unwrap();
    host.record_event(transition.event).await.unwrap();
    let (handle, control) = agent_execution_channel(8);
    handle.set_active_execution_surface(
        Some(golutra_agent_protocol::AgentExecutionMode::Open),
        golutra_agent_protocol::AgentToolProfile::Coding,
    );
    let worker = tokio::spawn(std::future::pending::<()>());
    let (_, completion) = watch::channel(false);
    host.execution.task_controls.lock().await.insert(
        session,
        HostedTaskControl {
            task_id: task.task_id,
            allow_network: false,
            yolo: true,
            provider_settings: ProviderTurnSettings {
                profile: Some(json!("mock")),
                model: Some(json!("mock-model")),
                ..Default::default()
            },
            execution: handle.clone(),
            abort_handle: worker.abort_handle(),
            completion,
            delegation: Some(crate::delegation_policy::DelegationContext::root(
                session,
                Some(30_000),
                None,
                handle.cancellation_token(),
            )),
            _session_lease: None,
        },
    );
    (task, handle, control)
}

#[tokio::test]
async fn managed_subagent_continues_parent_and_delivers_one_durable_result() {
    let _config = IsolatedGlobalMockProvider::install().await;
    let root = tempdir().unwrap();
    let host = RuntimeHost::ephemeral_for_cwd(root.path()).await.unwrap();
    let (task, _, control) = parent_fixture(&host).await;
    let backend = Arc::new(crate::delegation::RuntimeTaskDelegationBackend::new(
        Arc::downgrade(&host),
    ));
    let observer = crate::background::HostedBackgroundObserver::new(&host, task.clone());
    let executor =
        golutra_agent_tools::ToolRuntime::new(WorkspacePolicy::new(root.path()).unwrap())
            .with_unrestricted_access(true)
            .with_task_delegation_backend(backend.clone())
            .unwrap()
            .with_background_supervisor(host.execution.background_supervisor.clone(), observer)
            .unwrap();
    let harness = AgentHarness::new(
        MockProvider::tool_call(
            "subagent",
            json!({"task":"Summarize seven.","run_in_background":true}),
        ),
        ContextBuilder::default(),
        executor,
    );
    let recorder = crate::execution::HostedObservationRecorder::spawn(host.clone(), task.clone());
    let sender = recorder.sender();
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        harness.execute_configured(
            ConfiguredAgentRun::new(AgentTaskRequest {
                session_id: task.session_id,
                task_id: task.task_id,
                turn_id: task.turn_id,
                objective: "Get the child findings".into(),
                completion_criteria: vec![],
                output_schema: None,
                touched_code: false,
                contributors: vec![],
                tools: vec!["subagent".into()],
            })
            .with_execution_mode(Some(golutra_agent_protocol::AgentExecutionMode::Open))
            .with_max_elapsed_ms(25_000),
            control,
            move |event| {
                sender.send(event).unwrap();
            },
        ),
    )
    .await
    .expect("parent must receive child result without polling")
    .unwrap();
    recorder.close().await.unwrap();
    assert_eq!(outcome.final_turn_id, task.turn_id);
    assert!(
        host.storage
            .store
            .background()
            .pending_results(host.workspace_id, task.session_id, 128)
            .await
            .unwrap()
            .is_empty()
    );
    let events = host
        .storage
        .repositories
        .events
        .load(task.session_id, None, None)
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == RuntimeEventType::BackgroundTaskUpdated)
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == RuntimeEventType::BackgroundResultsConsumed)
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == RuntimeEventType::ToolStarted)
            .count(),
        1
    );
    assert!(
        backend
            .notifications(task.session_id)
            .await
            .unwrap()
            .is_empty(),
        "managed results must not be delivered by the old notification path"
    );
    assert!(
        outcome
            .tool_reports
            .iter()
            .any(|r| r.envelope.structured_facts["child_terminal"] == true)
    );
    host.clear_task_control(task.session_id, task.task_id).await;
    host.close().await.unwrap();
}
