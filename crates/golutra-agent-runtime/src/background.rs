//! Passive observations and turn continuation share the user-input admission lock.
//!
//! The durable host owns delivery and wake policy. This module only admits its
//! decisions into an existing loop, so a background completion cannot create a
//! second model loop or reset the task's budget.

use std::collections::{HashMap, HashSet, VecDeque};

use golutra_agent_core::{
    BackgroundResult, BackgroundResultId, BackgroundTaskStatus, MAX_BACKGROUND_PENDING_RESULTS,
    MAX_BACKGROUND_RESULT_BYTES, MAX_BACKGROUND_SUMMARY_BYTES,
};

use super::*;

const OBSERVATION_TOOL: &str = "background_result";
const RESULT_CALL_PREFIX: &str = "background-result-";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BackgroundDeliveryMode {
    /// Retain until a real input is admitted; never request another model step.
    #[default]
    Passive,
    /// The host has established that this finite batch is sealed and settled.
    ContinueAfterBatch,
}

#[derive(Debug)]
struct PendingResult {
    result: BackgroundResult,
    continue_turn: bool,
    attach_to_active: bool,
}

#[derive(Debug)]
pub(super) struct BackgroundInbox {
    session_id: Option<SessionId>,
    active_task_id: Option<TaskId>,
    active_turn_id: Option<TurnId>,
    pending: VecDeque<PendingResult>,
    in_flight: HashMap<BackgroundResultId, BackgroundResult>,
    seen: HashSet<BackgroundResultId>,
    allow_continuation: bool,
}

impl Default for BackgroundInbox {
    fn default() -> Self {
        Self {
            session_id: None,
            active_task_id: None,
            active_turn_id: None,
            pending: VecDeque::new(),
            in_flight: HashMap::new(),
            seen: HashSet::new(),
            allow_continuation: true,
        }
    }
}

impl BackgroundInbox {
    fn take(&mut self, include_passive: bool, attach_active: bool) -> Vec<BackgroundResult> {
        let mut results = Vec::new();
        self.pending.retain_mut(|pending| {
            let current = Some(pending.result.scope.task_id) == self.active_task_id
                && Some(pending.result.scope.turn_id) == self.active_turn_id;
            if results.len() < 16
                && (include_passive
                    || pending.continue_turn
                    || (attach_active && current && pending.attach_to_active))
            {
                self.in_flight
                    .insert(pending.result.result_id, pending.result.clone());
                results.push(pending.result.clone());
                false
            } else {
                true
            }
        });
        results
    }
}

pub(super) enum PendingRuntimeInput {
    Turn(Box<TakenPendingTurn>),
    Background(Vec<BackgroundResult>),
    DeadlineElapsed,
}

enum BoundaryAdmission {
    Ready(PendingRuntimeInput),
    Wait,
    Closed,
}

impl PendingTurnQueue {
    pub(super) fn set_background_turn(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        turn_id: TurnId,
    ) -> Result<(), AgentLoopError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .background
            .session_id
            .is_some_and(|expected| expected != session_id)
        {
            return Err(AgentLoopError::BackgroundResult(
                "result belongs to another session".into(),
            ));
        }
        state.background.session_id = Some(session_id);
        state.background.active_task_id = Some(task_id);
        state.background.active_turn_id = Some(turn_id);
        for pending in &mut state.background.pending {
            if pending.result.scope.task_id != task_id || pending.result.scope.turn_id != turn_id {
                pending.continue_turn = false;
            }
        }
        Ok(())
    }

    pub(super) fn enqueue_background_results(
        &self,
        results: Vec<BackgroundResult>,
        mode: BackgroundDeliveryMode,
    ) -> Result<(), AgentLoopError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.accepting {
            return Err(AgentLoopError::PendingTurnQueueClosed);
        }
        let expected_session = state
            .background
            .session_id
            .or_else(|| results.first().map(|r| r.scope.session_id));
        let mut added = HashSet::new();
        for result in &results {
            if Some(result.scope.session_id) != expected_session
                || result.output.tool_call_id != result.tool_call_id
                || result.output.tool_name != result.tool_name
                || !(result.status.is_terminal()
                    || result.status == BackgroundTaskStatus::WaitingForUser)
                || result.summary.len() > MAX_BACKGROUND_SUMMARY_BYTES
                || serde_json::to_vec(result)
                    .map_or(true, |bytes| bytes.len() > MAX_BACKGROUND_RESULT_BYTES)
            {
                return Err(AgentLoopError::BackgroundResult(
                    "invalid scope, state or result size".into(),
                ));
            }
            if !state.background.seen.contains(&result.result_id) {
                added.insert(result.result_id);
            }
        }
        if state
            .background
            .pending
            .len()
            .saturating_add(state.background.in_flight.len())
            .saturating_add(added.len())
            > MAX_BACKGROUND_PENDING_RESULTS
        {
            return Err(AgentLoopError::PendingTurnQueueFull);
        }
        state.background.session_id = expected_session;
        let continue_turn = mode == BackgroundDeliveryMode::ContinueAfterBatch
            && state.background.allow_continuation;
        let active_task = state.background.active_task_id;
        let active_turn = state.background.active_turn_id;
        let attach_to_active = state.background.allow_continuation;
        for result in results {
            let continue_turn = continue_turn
                && active_task.is_none_or(|task| task == result.scope.task_id)
                && active_turn.is_none_or(|turn| turn == result.scope.turn_id);
            // A batch completion can promote an already admitted passive result.
            // Once drained, its ID remains seen, so duplicate callbacks cannot wake again.
            if state.background.seen.insert(result.result_id) {
                let continue_turn =
                    continue_turn && result.status != BackgroundTaskStatus::WaitingForUser;
                state.background.pending.push_back(PendingResult {
                    result,
                    continue_turn,
                    attach_to_active,
                });
            } else if continue_turn
                && let Some(pending) = state
                    .background
                    .pending
                    .iter_mut()
                    .find(|p| p.result.result_id == result.result_id)
                && pending.result.status != BackgroundTaskStatus::WaitingForUser
            {
                pending.continue_turn = true;
            }
        }
        drop(state);
        // This only wakes the boundary waiter. Passive arrival cannot admit sampling.
        self.changed.notify_waiters();
        Ok(())
    }

    pub(super) fn suppress_background_continuations(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.background.allow_continuation = false;
        for pending in &mut state.background.pending {
            pending.continue_turn = false;
            pending.attach_to_active = false;
        }
        drop(state);
        self.changed.notify_waiters();
    }

    pub(super) fn suppress_background_batch(&self, batch_id: BackgroundBatchId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for pending in &mut state.background.pending {
            if pending.result.batch_id == batch_id {
                pending.continue_turn = false;
                pending.attach_to_active = false;
            }
        }
        drop(state);
        self.changed.notify_waiters();
    }

    pub(super) fn enable_background_continuations(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .background
            .allow_continuation = true;
    }

    pub(super) fn take_background_results(&self, include_passive: bool) -> Vec<BackgroundResult> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // This is an already scheduled inference boundary, not a wake request.
        state.background.take(include_passive, true)
    }

    pub(super) fn take_ready_turn(&self) -> Option<TakenPendingTurn> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next = state
            .turns
            .iter()
            .position(|entry| entry.turn.turn.steer)
            .unwrap_or(0);
        if !state.accepting || !state.turns.get(next).is_some_and(|entry| entry.durable) {
            return None;
        }
        let entry = state.turns.remove(next)?;
        Some(TakenPendingTurn {
            turn: entry.turn,
            execution_origin: entry.execution_origin,
        })
    }

    pub(super) fn acknowledge_captured_background(&self, ids: &[BackgroundResultId]) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for id in ids {
            state.background.in_flight.remove(id);
        }
    }

    fn restore_compacted_background(&self, ids: &[BackgroundResultId]) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for id in ids {
            if let Some(result) = state.background.in_flight.remove(id) {
                state.background.pending.push_back(PendingResult {
                    result,
                    continue_turn: false,
                    attach_to_active: false,
                });
            }
        }
    }

    fn admit_boundary(&self, keep_open: bool) -> BoundaryAdmission {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.accepting {
            return BoundaryAdmission::Closed;
        }
        let next = state
            .turns
            .iter()
            .position(|entry| entry.turn.turn.steer)
            .unwrap_or(0);
        if let Some(entry) = state.turns.get(next) {
            if !entry.durable {
                return BoundaryAdmission::Wait;
            }
            let entry = state.turns.remove(next).expect("checked pending turn");
            return BoundaryAdmission::Ready(PendingRuntimeInput::Turn(Box::new(
                TakenPendingTurn {
                    turn: entry.turn,
                    execution_origin: entry.execution_origin,
                },
            )));
        }
        let ready = state.background.take(false, false);
        if !ready.is_empty() {
            return BoundaryAdmission::Ready(PendingRuntimeInput::Background(ready));
        }
        if keep_open {
            BoundaryAdmission::Wait
        } else {
            state.accepting = false;
            BoundaryAdmission::Closed
        }
    }
}

impl AgentExecutionControl {
    pub(super) async fn next_input_or_wait_for_background(
        &mut self,
        executor: &ToolRuntime,
        session_id: SessionId,
        turn_id: TurnId,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Option<PendingRuntimeInput>, AgentLoopError> {
        let mut activity = executor.subscribe_background_activity();
        loop {
            // Register before inspecting either queue. Completion or user input may
            // arrive between the inspection and the actual wait.
            let changed = self.pending_turns.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.cancellation.is_cancelled() {
                return Err(AgentLoopError::Cancelled);
            }
            if !*self.pause.borrow() {
                match self.pending_turns.admit_boundary(
                    executor.has_pending_background_tasks(session_id, Some(turn_id)),
                ) {
                    BoundaryAdmission::Ready(input) => return Ok(Some(input)),
                    BoundaryAdmission::Closed => return Ok(None),
                    BoundaryAdmission::Wait => {}
                }
            }
            tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => return Err(AgentLoopError::Cancelled),
                _ = wait_deadline(deadline) => return Ok(Some(PendingRuntimeInput::DeadlineElapsed)),
                _ = &mut changed => {},
                update = self.pause.changed() => {
                    if update.is_err() { return Err(AgentLoopError::Cancelled); }
                }
                _ = async {
                    match &mut activity {
                        Some(receiver) => { let _ = receiver.changed().await; }
                        None => std::future::pending::<()>().await,
                    }
                } => {},
            }
        }
    }
}

async fn wait_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

pub(super) fn seed_result_ids(messages: &[ProviderMessage]) -> HashSet<String> {
    messages
        .iter()
        .filter_map(|message| {
            (message.role == ProviderRole::Tool
                && message.tool_name.as_deref() == Some(OBSERVATION_TOOL))
            .then(|| message.tool_call_id.clone())
            .flatten()
        })
        .collect()
}

pub(super) fn append_results(
    plan: &mut ContextBuildPlan,
    results: Vec<BackgroundResult>,
    reports: &[ToolExecutionReport],
    executor: &ToolRuntime,
    seen: &mut HashSet<String>,
    pending_ack: &mut Vec<BackgroundResultId>,
    message_token_total: &mut u64,
) -> bool {
    let mut appended = false;
    // Reserve space for provenance/IDs as well as the projected body. Admission
    // bounds each page to 16 observations; the rest remain in the same inbox.
    let tokens = (8_192 / results.len().max(1) as u64)
        .saturating_sub(256)
        .clamp(256, 2_048);
    for result in results {
        let call_id = format!("{RESULT_CALL_PREFIX}{}", result.result_id);
        if !seen.insert(call_id.clone()) {
            // Recovery can replay the durable snapshot before its store ack was
            // committed. Re-ack the existing observation without duplicating it.
            if !pending_ack.contains(&result.result_id) {
                pending_ack.push(result.result_id);
            }
            continue;
        }
        let source = ContextMessageSource {
            contributor: "tool_result_excerpt".into(),
            source_refs: vec![format!("background-result:{}", result.result_id)],
            origin: "background_result".into(),
            visibility: ModelInputVisibility::ModelVisible,
        };
        append_plan_message(
            plan,
            ProviderMessage {
                role: ProviderRole::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![ProviderToolCall {
                    tool_call_id: call_id.clone(),
                    tool_name: OBSERVATION_TOOL.into(),
                    arguments: json!({"result_id":result.result_id}),
                }],
                metadata: Default::default(),
            },
            source.clone(),
            message_token_total,
        );
        let output = if subagent_output_already_visible(plan, &result) {
            json!({"output_already_observed":true,"child_session_id":result.output.structured_facts["child_session_id"],
                "child_task_id":result.output.structured_facts["child_task_id"],
                "summary":"This execution's result was already returned by the subagent tool; use those findings."}).to_string()
        } else {
            model_visible_tool_result_with_token_budget(&result.output, tokens)
        };
        let media_report = reports
            .iter()
            .find(|report| {
                report.envelope.tool_call_id == result.tool_call_id
                    && report.envelope.structured_facts["result_pending"] != true
            })
            .map(std::borrow::Cow::Borrowed)
            .or_else(|| {
                executor
                    .take_background_completed_report(result.scope.session_id, result.tool_call_id)
                    .map(|report| std::borrow::Cow::Owned(Arc::unwrap_or_clone(report)))
            });
        let image_message = media_report
            .as_ref()
            .and_then(|report| tool_images::image_message(std::slice::from_ref(report.as_ref())));
        let declared_images = media_report.as_ref().is_some_and(|report| {
            report
                .artifacts
                .iter()
                .any(|artifact| artifact.artifact_type.starts_with("tool_image:"))
        }) || result
            .output
            .structured_facts
            .get("background_image_artifacts")
            .and_then(Value::as_array)
            .is_some_and(|images| !images.is_empty());
        let image_omission = (image_message.is_none() && declared_images)
            .then_some("Image bytes were not included in this delivery. Retained artifact references remain available; inspect the image before making visual claims.");
        let image_omission_reason = media_report.as_ref().and_then(|report| {
            report
                .envelope
                .structured_facts
                .get("background_image_omitted")
        });
        let mut artifact_bytes = 0usize;
        let artifact_refs = result
            .artifact_refs
            .iter()
            .filter(|reference| {
                if reference.len() > 2_048usize.saturating_sub(artifact_bytes) {
                    return false;
                }
                artifact_bytes += reference.len();
                true
            })
            .take(8)
            .collect::<Vec<_>>();
        let omitted_artifact_refs = result
            .artifact_refs
            .len()
            .saturating_sub(artifact_refs.len());
        append_plan_message(plan, ProviderMessage {
            role: ProviderRole::Tool,
            content: json!({
                "observation_kind":"background_result",
                "trust":"untrusted_tool_output",
                "result_id":result.result_id, "background_task_id":result.background_task_id,
                "batch_id":result.batch_id, "origin":result.scope, "status":result.status,
                "original_tool_call_id":result.tool_call_id,
                "output":serde_json::from_str::<Value>(&output).unwrap_or(Value::String(output)),
                "artifact_refs":artifact_refs, "omitted_artifact_refs":omitted_artifact_refs,
                "image_omission":image_omission,
                "image_omission_reason":image_omission_reason,
            }).to_string(),
            tool_call_id: Some(call_id),
            tool_name: Some(OBSERVATION_TOOL.into()),
            tool_calls: Vec::new(),
            metadata: Default::default(),
        }, source.clone(), message_token_total);
        if let Some(image_message) = image_message {
            // The existing cross-provider media carrier labels images as tool
            // observations. Its source stays tied to the background result.
            append_plan_message(
                plan,
                image_message,
                ContextMessageSource {
                    contributor: "tool_result_image".into(),
                    ..source
                },
                message_token_total,
            );
        }
        pending_ack.push(result.result_id);
        appended = true;
    }
    appended
}

fn subagent_output_already_visible(plan: &ContextBuildPlan, result: &BackgroundResult) -> bool {
    result_was_observed(&captured_subagent_executions(&plan.messages), result)
}

/// Only full terminal findings in the actual captured request can acknowledge a
/// later inbox delivery. A status handle, truncated page or another execution
/// of the same child is not evidence that the model received this result.
pub(super) fn captured_subagent_executions(
    messages: &[ProviderMessage],
) -> HashSet<(String, String)> {
    let mut executions = HashSet::new();
    fn body(output: &Value) -> &str {
        ["model_visible_excerpt", "content", "output", "error"]
            .iter()
            .find_map(|key| output[*key].as_str().filter(|text| !text.trim().is_empty()))
            .unwrap_or_default()
    }
    for output in messages
        .iter()
        .filter(|message| {
            message.role == ProviderRole::Tool && message.tool_name.as_deref() == Some("subagent")
        })
        .filter_map(|message| serde_json::from_str::<Value>(&message.content).ok())
    {
        if output["truncated"] == true || output["model_visible_truncated"] == true {
            continue;
        }
        let facts = &output["structured_facts"];
        if golutra_agent_tools::subagent_findings_complete(facts, body(&output))
            && let (Some(session), Some(task)) = (
                facts["child_session_id"].as_str(),
                facts["child_task_id"].as_str(),
            )
        {
            executions.insert((session.to_owned(), task.to_owned()));
        }
        if facts["model_visible_truncated"] != true
            && facts["truncated"] != true
            && let Some(children) = facts["child_results"].as_array()
        {
            for child in children {
                if golutra_agent_tools::subagent_findings_complete(&child["facts"], body(child))
                    && let (Some(session), Some(task)) = (
                        child["child_session_id"].as_str(),
                        child["facts"]["child_task_id"].as_str(),
                    )
                {
                    executions.insert((session.to_owned(), task.to_owned()));
                }
            }
        }
    }
    executions
}

pub(super) fn result_was_observed(
    executions: &HashSet<(String, String)>,
    result: &BackgroundResult,
) -> bool {
    let facts = &result.output.structured_facts;
    result.tool_name == "subagent"
        && result.status.is_terminal()
        && facts["child_terminal"] == true
        && facts["child_session_id"]
            .as_str()
            .zip(facts["child_task_id"].as_str())
            .is_some_and(|(session, task)| {
                executions.contains(&(session.to_owned(), task.to_owned()))
            })
}

/// Replace the runtime verification view with the settled operation's evidence. The
/// original admission receipt remains immutable in its emitted audit event.
/// Final callbacks do not consume a second tool-call budget. Results from an
/// earlier task remain context only.
pub(super) fn reconcile_completed_results(
    executor: &ToolRuntime,
    results: &[BackgroundResult],
    reports: &mut [ToolExecutionReport],
    attempts: &mut [ToolAttemptMetadata],
) -> Vec<ToolResultStatus> {
    let mut statuses = Vec::new();
    for result in results {
        if !(result.status.is_terminal() || result.status == BackgroundTaskStatus::WaitingForUser)
            || reports.iter().any(|report| {
                report.envelope.structured_facts["background_result_id"]
                    == result.result_id.to_string()
            })
        {
            continue;
        }
        let Some(index) = reports.iter().position(|report| {
            report.envelope.tool_call_id == result.tool_call_id
                // A fast operation can settle before submit returns its admission
                // receipt. That receipt is still not the operation's evidence.
                && (report.envelope.structured_facts["result_pending"] == true
                    || report.envelope.structured_facts["background"] == true)
        }) else {
            continue;
        };
        let original = &reports[index];
        let mut completed = executor
            .take_background_completed_report(result.scope.session_id, result.tool_call_id)
            .map(Arc::unwrap_or_clone)
            .unwrap_or_else(|| {
                let mut report = original.clone();
                report.envelope = result.output.clone();
                if !report.envelope.structured_facts.is_object() {
                    report.envelope.structured_facts =
                        json!({"raw_facts":report.envelope.structured_facts});
                }
                report.envelope.structured_facts["workspace_changes_known"] = json!(false);
                report
            });
        if !completed.envelope.structured_facts.is_object() {
            completed.envelope.structured_facts =
                json!({"raw_facts":completed.envelope.structured_facts});
        }
        if let Some(validation) = original
            .envelope
            .structured_facts
            .get("runtime_objective_validation")
        {
            completed.envelope.structured_facts["runtime_objective_validation"] =
                validation.clone();
        }
        completed.envelope.structured_facts["background_result_id"] = json!(result.result_id);
        completed.envelope.structured_facts["background_task_id"] =
            json!(result.background_task_id);
        completed.envelope.structured_facts["result_pending"] = json!(false);
        completed.envelope.structured_facts["background_status"] = json!(result.status);
        completed.envelope.structured_facts["terminal"] = json!(result.status.is_terminal());
        if result.status == BackgroundTaskStatus::WaitingForUser {
            completed.envelope.structured_facts["waiting_for_user"] = json!(true);
            // Successfully handing control to a person does not complete the
            // requested operation. Keep existing failures and block a success receipt.
            if completed.envelope.status == ToolResultStatus::Ok {
                completed.envelope.status = ToolResultStatus::Blocked;
            }
        }
        if result.status == BackgroundTaskStatus::OutcomeUnknown {
            completed.envelope.status = ToolResultStatus::Error;
            completed.envelope.structured_facts["outcome_unknown"] = json!(true);
            completed.envelope.structured_facts["workspace_changes_known"] = json!(false);
        }
        if let Some(attempt) = attempts
            .iter_mut()
            .find(|attempt| attempt.tool_call_id == result.tool_call_id)
        {
            attempt.status = completed.envelope.status;
            attempt.recoverable_failure = tool_failure_is_recoverable(&completed);
        }
        // A synchronous child result already contributed its execution status;
        // the durable delivery is acknowledgement, not another tool attempt.
        if !(original.envelope.tool_name == "subagent"
            && original.envelope.structured_facts["child_terminal"] == true
            && original.envelope.status == completed.envelope.status)
        {
            statuses.push(completed.envelope.status);
        }
        reports[index] = completed;
    }
    statuses
}

/// Only acknowledge observations actually present in the captured request.
/// Compaction may have removed a queued result before sampling was admitted.
pub(super) fn captured_result_ids(
    request: &ProviderRequest,
    pending: &mut Vec<BackgroundResultId>,
) -> Vec<BackgroundResultId> {
    let included = seed_result_ids(&request.messages);
    let mut captured = Vec::new();
    pending.retain(|id| {
        if included.contains(&format!("{RESULT_CALL_PREFIX}{id}")) {
            captured.push(*id);
            false
        } else {
            true
        }
    });
    captured
}

/// An observation removed before request capture is still undelivered. Keep a
/// bounded copy for the next real input; repeated automatic attempts with the
/// same overflowing context would otherwise create a model-sampling loop.
pub(super) fn restore_compacted_results(
    plan: &ContextBuildPlan,
    seen: &mut HashSet<String>,
    pending_ack: &mut Vec<BackgroundResultId>,
    queue: &PendingTurnQueue,
) {
    let included = seed_result_ids(&plan.messages);
    let mut removed = Vec::new();
    pending_ack.retain(|id| {
        let call_id = format!("{RESULT_CALL_PREFIX}{id}");
        if included.contains(&call_id) {
            true
        } else {
            seen.remove(&call_id);
            removed.push(*id);
            false
        }
    });
    queue.restore_compacted_background(&removed);
}
