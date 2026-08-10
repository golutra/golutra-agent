use std::collections::HashMap;

use golutra_core::{
    CausalContext, CausalLink, CausalRelation, EventId, ProviderRequestId, ProviderResponseId,
    RUNTIME_EVENT_SCHEMA_VERSION, RunId, SessionId, TaskId, ToolCallId, VerificationId,
    WorkspaceId,
};
use golutra_protocol::{RuntimeEvent, RuntimeEventType};
use serde_json::Value;

use super::{ClientError, RuntimeHost};

#[derive(Debug, Clone, Default)]
pub(crate) struct CausalLedger {
    session_heads: HashMap<SessionId, EventId>,
    task_heads: HashMap<TaskId, EventId>,
    task_contexts: HashMap<TaskId, CausalContext>,
    provider_starts: HashMap<(TaskId, ProviderRequestId), EventId>,
    tool_starts: HashMap<(TaskId, ToolCallId), EventId>,
    verification_heads: HashMap<TaskId, EventId>,
}

impl CausalLedger {
    fn has_session(&self, session_id: SessionId) -> bool {
        self.session_heads.contains_key(&session_id)
    }

    fn has_task(&self, task_id: TaskId) -> bool {
        self.task_heads.contains_key(&task_id)
    }

    fn seed(&mut self, event: &RuntimeEvent) {
        self.session_heads.insert(event.session_id, event.id);
        if let Some(task_id) = event.task_id {
            self.task_heads.insert(task_id, event.id);
            let mut context = event.causal_context.clone();
            synchronize_envelope_context(&mut context, event, None);
            self.task_contexts.insert(task_id, context);
            self.index_lifecycle(event);
        }
    }

    fn enrich(&mut self, workspace_id: WorkspaceId, event: &mut RuntimeEvent) {
        event.schema_version = RUNTIME_EVENT_SCHEMA_VERSION;
        let previous = event.task_id.and_then(|task_id| {
            self.task_heads
                .get(&task_id)
                .copied()
                .or_else(|| self.session_heads.get(&event.session_id).copied())
        });
        let previous = previous.or_else(|| self.session_heads.get(&event.session_id).copied());
        if event.parent_event_id.is_none() {
            event.parent_event_id = previous;
        }
        if let Some(parent) = event.parent_event_id {
            add_link(event, parent, CausalRelation::Parent);
        }

        if let Some(task_id) = event.task_id {
            let mut context = self
                .task_contexts
                .get(&task_id)
                .cloned()
                .unwrap_or_default();
            synchronize_envelope_context(&mut context, event, Some(workspace_id));
            update_context_from_event(&mut context, event);
            event.causal_context = context.clone();
            self.task_contexts.insert(task_id, context);
            self.add_lifecycle_links(event);
            self.task_heads.insert(task_id, event.id);
        } else {
            let mut context = event.causal_context.clone();
            synchronize_envelope_context(&mut context, event, Some(workspace_id));
            event.causal_context = context;
        }
        self.session_heads.insert(event.session_id, event.id);
        self.index_lifecycle(event);
    }

    fn add_lifecycle_links(&self, event: &mut RuntimeEvent) {
        let Some(task_id) = event.task_id else {
            return;
        };
        match event.event_type {
            RuntimeEventType::ProviderStreamed
            | RuntimeEventType::ProviderCompleted
            | RuntimeEventType::ProviderFailed => {
                if let Some(request_id) = event.causal_context.provider_request_id
                    && let Some(start) = self.provider_starts.get(&(task_id, request_id))
                {
                    add_link(event, *start, CausalRelation::RespondsTo);
                }
            }
            RuntimeEventType::ToolProgress | RuntimeEventType::ToolCompleted => {
                if let Some(tool_call_id) = event.causal_context.tool_call_id
                    && let Some(start) = self.tool_starts.get(&(task_id, tool_call_id))
                {
                    add_link(event, *start, CausalRelation::RespondsTo);
                }
            }
            RuntimeEventType::VerificationAssertionCompleted
            | RuntimeEventType::VerificationCompleted => {
                if let Some(start) = self.verification_heads.get(&task_id) {
                    add_link(event, *start, CausalRelation::Verifies);
                }
            }
            _ => {}
        }
    }

    fn index_lifecycle(&mut self, event: &RuntimeEvent) {
        let Some(task_id) = event.task_id else {
            return;
        };
        match event.event_type {
            RuntimeEventType::ProviderStarted => {
                if let Some(request_id) = event.causal_context.provider_request_id {
                    self.provider_starts.insert((task_id, request_id), event.id);
                }
            }
            RuntimeEventType::ToolStarted => {
                if let Some(tool_call_id) = event.causal_context.tool_call_id {
                    self.tool_starts.insert((task_id, tool_call_id), event.id);
                }
            }
            RuntimeEventType::VerificationPlanned => {
                self.verification_heads.insert(task_id, event.id);
            }
            _ => {}
        }
    }
}

impl RuntimeHost {
    pub(crate) async fn prepare_canonical_event(
        &self,
        mut event: RuntimeEvent,
    ) -> Result<RuntimeEvent, ClientError> {
        let (has_session, has_task) = {
            let ledger = self.execution.causal_ledger.lock().await;
            (
                ledger.has_session(event.session_id),
                event
                    .task_id
                    .is_some_and(|task_id| ledger.has_task(task_id)),
            )
        };
        if !has_task
            && let Some(task_id) = event.task_id
            && let Some(previous) = self
                .storage
                .repositories
                .events
                .load_recent(event.session_id, Some(task_id), None, 1)
                .await?
                .pop()
        {
            self.execution.causal_ledger.lock().await.seed(&previous);
        }
        if !has_session
            && let Some(previous) = self
                .storage
                .repositories
                .events
                .load_recent(event.session_id, None, None, 1)
                .await?
                .pop()
        {
            self.execution.causal_ledger.lock().await.seed(&previous);
        }
        self.execution
            .causal_ledger
            .lock()
            .await
            .enrich(self.workspace_id, &mut event);
        Ok(event)
    }
}

fn synchronize_envelope_context(
    context: &mut CausalContext,
    event: &RuntimeEvent,
    workspace_id: Option<WorkspaceId>,
) {
    context.workspace_id = workspace_id.or(context.workspace_id);
    context.session_id = Some(event.session_id);
    context.task_id = event.task_id;
    context.turn_id = event.turn_id;
    if let Some(task_id) = event.task_id {
        context.run_id = Some(RunId::from(task_id));
    }
}

fn update_context_from_event(context: &mut CausalContext, event: &RuntimeEvent) {
    if let Some(step_no) = event
        .payload
        .pointer("/step/step_no")
        .or_else(|| event.payload.pointer("/step/snapshot/step_no"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
    {
        context.step_no = Some(step_no);
        context.step_id = context
            .run_id
            .zip(context.turn_id)
            .map(|(run_id, turn_id)| format!("step:{run_id}:{turn_id}:{step_no}"));
    }
    if let Some(request_id) = parse_id::<ProviderRequestId>(
        event
            .payload
            .get("provider_request_id")
            .or_else(|| event.payload.pointer("/snapshot/provider_request_id"))
            .or_else(|| event.payload.pointer("/record/request_event_id")),
    ) {
        context.provider_request_id = Some(request_id);
        context.provider_round_id = Some(format!("provider-round:{request_id}"));
    }
    if let Some(response_id) = parse_id::<ProviderResponseId>(
        event
            .payload
            .get("provider_response_id")
            .or_else(|| event.payload.pointer("/record/response_event_id")),
    ) {
        context.provider_response_id = Some(response_id);
    }
    if let Some(provider_tool_call_id) = event
        .payload
        .get("provider_tool_call_id")
        .and_then(Value::as_str)
    {
        context.provider_tool_call_id = Some(provider_tool_call_id.to_owned());
    }
    if let Some(tool_call_id) = parse_id::<ToolCallId>(
        event
            .payload
            .get("tool_call_id")
            .or_else(|| event.payload.pointer("/envelope/tool_call_id")),
    ) {
        context.tool_call_id = Some(tool_call_id);
    }
    if let Some(verification_id) = parse_id::<VerificationId>(
        event
            .payload
            .pointer("/record/verification_id")
            .or_else(|| event.payload.get("verification_id")),
    ) {
        context.verification_id = Some(verification_id);
    }
    if let Some(candidate_id) = event
        .payload
        .get("candidate_id")
        .or_else(|| event.payload.pointer("/record/candidate_id"))
        .or_else(|| event.payload.pointer("/record/id"))
        .and_then(Value::as_str)
    {
        context.candidate_id = Some(candidate_id.to_owned());
    }
}

fn parse_id<T>(value: Option<&Value>) -> Option<T>
where
    T: std::str::FromStr,
{
    value
        .and_then(Value::as_str)
        .and_then(|value| value.parse().ok())
}

fn add_link(event: &mut RuntimeEvent, event_id: EventId, relation: CausalRelation) {
    if event_id == event.id
        || event
            .causal_links
            .iter()
            .any(|link| link.event_id == event_id && link.relation == relation)
    {
        return;
    }
    event.causal_links.push(CausalLink { event_id, relation });
}
