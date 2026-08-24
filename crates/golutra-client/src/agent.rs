//! High-level Agent thread/turn facade shared by CLI, SDK and MCP adapters.
//!
//! The facade translates the durable RuntimeEvent stream into a small
//! lifecycle stream.  It does not own execution state; RuntimeHost remains the
//! only component allowed to start, queue, pause or finish work.

use std::collections::VecDeque;

use crate::{
    AgentEventProjector, ClientError, RuntimeClient, RuntimeEventStream, RuntimeTransport,
};
use golutra_core::{
    Actor, ActorKind, CommandId, SessionId, TaskId, TaskReconciliationDecision, ThreadId,
};
use golutra_protocol::{
    AgentSteerOptions, AgentStreamEvent, AgentThreadRef, AgentTurnExecutionOptions,
    AgentTurnOptions, AgentTurnResult, AgentTurnStart, EventFilter, RuntimeQuery, RuntimeQueryKind,
    SessionCommand, SessionCommandKind,
};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct AgentClient {
    transport: RuntimeTransport,
    actor: Actor,
}

impl AgentClient {
    #[must_use]
    pub fn new(transport: RuntimeTransport) -> Self {
        Self::with_actor(
            transport,
            Actor {
                kind: ActorKind::Sdk,
                id: format!("golutra-agent-{}", Uuid::now_v7()),
            },
        )
    }

    #[must_use]
    pub fn with_actor(transport: RuntimeTransport, actor: Actor) -> Self {
        Self { transport, actor }
    }

    #[must_use]
    pub fn transport(&self) -> &RuntimeTransport {
        &self.transport
    }

    #[must_use]
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    pub async fn start_thread(&self) -> Result<AgentThread, ClientError> {
        let session_id = SessionId::new();
        let thread_id = ThreadId::new();
        let ack = self
            .transport
            .send_command(command(
                session_id,
                SessionCommandKind::Create,
                json!({
                    "_thread_id": thread_id,
                    "title": "Agent thread",
                }),
                &self.actor,
            ))
            .await?;
        if !ack.accepted {
            return Err(ClientError::TaskExecution(
                ack.reason
                    .unwrap_or_else(|| "thread creation was rejected".to_owned()),
            ));
        }
        Ok(AgentThread {
            client: self.clone(),
            thread: AgentThreadRef {
                thread_id,
                session_id,
                workspace_root: self.transport.cwd().map(|path| path.display().to_string()),
            },
        })
    }

    pub async fn resume_thread(&self, thread_id: ThreadId) -> Result<AgentThread, ClientError> {
        let record = self.transport.resume_thread(thread_id).await?;
        Ok(AgentThread {
            client: self.clone(),
            thread: AgentThreadRef {
                thread_id: record.thread_id,
                session_id: record.session_id,
                workspace_root: record.workspace_root,
            },
        })
    }

    pub async fn default_thread(&self) -> Result<AgentThread, ClientError> {
        let thread_id = self.transport.default_thread_id();
        match self.resume_thread(thread_id).await {
            Ok(thread) => Ok(thread),
            Err(_) => {
                let session_id = self.transport.default_session_id();
                let ack = self
                    .transport
                    .send_command(command(
                        session_id,
                        SessionCommandKind::Create,
                        json!({"_thread_id": thread_id}),
                        &self.actor,
                    ))
                    .await?;
                if !ack.accepted {
                    return Err(ClientError::TaskExecution(ack.reason.unwrap_or_else(
                        || "default thread creation was rejected".to_owned(),
                    )));
                }
                Ok(AgentThread {
                    client: self.clone(),
                    thread: AgentThreadRef {
                        thread_id,
                        session_id,
                        workspace_root: self.transport.cwd().map(|path| path.display().to_string()),
                    },
                })
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentThread {
    client: AgentClient,
    thread: AgentThreadRef,
}

impl AgentThread {
    #[must_use]
    pub fn reference(&self) -> &AgentThreadRef {
        &self.thread
    }

    #[must_use]
    pub fn thread_id(&self) -> ThreadId {
        self.thread.thread_id
    }

    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.thread.session_id
    }

    pub async fn start_turn(
        &self,
        prompt: impl Into<String>,
        options: AgentTurnOptions,
    ) -> Result<TurnHandle, ClientError> {
        self.start_turn_inner(prompt, options, Some(AgentTurnExecutionOptions::default()))
            .await
    }

    pub async fn start_turn_legacy(
        &self,
        prompt: impl Into<String>,
        options: AgentTurnOptions,
    ) -> Result<TurnHandle, ClientError> {
        self.start_turn_inner(prompt, options, None).await
    }

    pub async fn start_turn_with_execution_options(
        &self,
        prompt: impl Into<String>,
        options: AgentTurnOptions,
        execution: AgentTurnExecutionOptions,
    ) -> Result<TurnHandle, ClientError> {
        self.start_turn_inner(prompt, options, Some(execution))
            .await
    }

    async fn start_turn_inner(
        &self,
        prompt: impl Into<String>,
        options: AgentTurnOptions,
        execution: Option<AgentTurnExecutionOptions>,
    ) -> Result<TurnHandle, ClientError> {
        let prompt = prompt.into();
        if prompt.trim().is_empty() {
            return Err(ClientError::TaskExecution(
                "turn prompt cannot be empty".to_owned(),
            ));
        }
        let cursor = self
            .client
            .transport
            .query(RuntimeQuery {
                query_id: golutra_core::QueryId::new(),
                session_id: self.thread.session_id,
                task_id: None,
                kind: RuntimeQueryKind::SessionState,
                requester: ActorKind::Sdk,
                cursor: None,
                timestamp: chrono::Utc::now(),
            })
            .await?
            .get("last_sequence_no")
            .and_then(Value::as_u64);
        let stream = self
            .client
            .transport
            .subscribe(EventFilter {
                session_id: self.thread.session_id,
                task_id: None,
                after_sequence_no: cursor,
            })
            .await?;
        let payload = turn_payload(prompt, self.thread.thread_id, &options, execution)?;
        let ack = self
            .client
            .transport
            .send_command(command(
                self.thread.session_id,
                SessionCommandKind::Prompt,
                payload,
                &self.client.actor,
            ))
            .await?;
        let start = AgentTurnStart {
            thread_id: self.thread.thread_id,
            session_id: self.thread.session_id,
            command_id: ack.command_id,
            task_id: None,
            turn_id: None,
            accepted: ack.accepted,
            reason: ack.reason.clone(),
        };
        if !ack.accepted {
            return Err(ClientError::TaskExecution(
                ack.reason.unwrap_or_else(|| "turn was rejected".to_owned()),
            ));
        }
        Ok(TurnHandle::new(
            self.thread.clone(),
            stream,
            start,
            self.client.transport.clone(),
            self.client.actor.clone(),
        ))
    }

    pub async fn steer(
        &self,
        prompt: impl Into<String>,
    ) -> Result<golutra_protocol::CommandAck, ClientError> {
        self.steer_with_options(prompt, AgentSteerOptions::default())
            .await
    }

    pub async fn steer_with_options(
        &self,
        prompt: impl Into<String>,
        options: AgentSteerOptions,
    ) -> Result<golutra_protocol::CommandAck, ClientError> {
        let prompt = prompt.into();
        if prompt.trim().is_empty() {
            return Err(ClientError::TaskExecution(
                "steering prompt cannot be empty".to_owned(),
            ));
        }
        let mut payload = json!({
            "prompt": prompt,
            "_thread_id": self.thread.thread_id,
            "steer": true,
        });
        if let Some(tool_profile) = options.tool_profile {
            payload["tool_profile"] = serde_json::to_value(tool_profile)?;
        }
        self.client
            .transport
            .send_command(command(
                self.thread.session_id,
                SessionCommandKind::Prompt,
                payload,
                &self.client.actor,
            ))
            .await
    }

    pub async fn interrupt(&self) -> Result<golutra_protocol::CommandAck, ClientError> {
        self.client
            .transport
            .send_command(command(
                self.thread.session_id,
                SessionCommandKind::Abort,
                json!({"_thread_id": self.thread.thread_id}),
                &self.client.actor,
            ))
            .await
    }

    pub async fn reconcile_task(
        &self,
        decision: TaskReconciliationDecision,
        task_id: Option<TaskId>,
        note: Option<String>,
    ) -> Result<golutra_protocol::CommandAck, ClientError> {
        self.client
            .transport
            .send_command(command(
                self.thread.session_id,
                SessionCommandKind::ReconcileTask,
                json!({
                    "task_id": task_id,
                    "decision": decision,
                    "note": note,
                    "_thread_id": self.thread.thread_id,
                }),
                &self.client.actor,
            ))
            .await
    }

    pub async fn resolve_approval(
        &self,
        approval_id: impl Into<String>,
        approve: bool,
    ) -> Result<golutra_protocol::CommandAck, ClientError> {
        self.client
            .transport
            .send_command(command(
                self.thread.session_id,
                if approve {
                    SessionCommandKind::Approve
                } else {
                    SessionCommandKind::Deny
                },
                json!({
                    "approval_id": approval_id.into(),
                    "_thread_id": self.thread.thread_id,
                }),
                &self.client.actor,
            ))
            .await
    }
}

fn turn_payload(
    prompt: String,
    thread_id: ThreadId,
    options: &AgentTurnOptions,
    execution: Option<AgentTurnExecutionOptions>,
) -> Result<Value, ClientError> {
    let mut payload = json!({
        "prompt": prompt,
        "_thread_id": thread_id,
        "task_contract": options.task_contract,
        "completion_criteria": options.completion_criteria,
        "output_schema": options.output_schema,
        "allow_network": options.allow_network,
        "yolo": options.yolo,
        "defer_external_verification": options.defer_external_verification,
    });
    if let Some(execution) = execution {
        payload["execution_mode"] = serde_json::to_value(execution.execution_mode)?;
        payload["tool_profile"] = serde_json::to_value(execution.tool_profile)?;
    }
    if let Some(max_elapsed_ms) = options.max_elapsed_ms {
        payload["max_elapsed_ms"] = json!(max_elapsed_ms);
    }
    if !options.external_verifiers.is_empty() || !options.discover_project_verifiers {
        payload["external_verifiers"] = serde_json::to_value(&options.external_verifiers)?;
    }
    Ok(payload)
}

#[derive(Debug)]
pub struct TurnHandle {
    thread: AgentThreadRef,
    stream: RuntimeEventStream,
    pending: VecDeque<AgentStreamEvent>,
    start: AgentTurnStart,
    transport: RuntimeTransport,
    actor: Actor,
    projector: AgentEventProjector,
    finished: bool,
}

impl TurnHandle {
    fn new(
        thread: AgentThreadRef,
        stream: RuntimeEventStream,
        start: AgentTurnStart,
        transport: RuntimeTransport,
        actor: Actor,
    ) -> Self {
        let mut pending = VecDeque::new();
        let projector = AgentEventProjector::new(thread.clone(), Some(start.command_id));
        pending.push_back(projector.thread_started());
        Self {
            thread,
            stream,
            pending,
            start,
            transport,
            actor,
            projector,
            finished: false,
        }
    }

    #[must_use]
    pub fn start(&self) -> &AgentTurnStart {
        &self.start
    }

    pub async fn next_event(&mut self) -> Result<Option<AgentStreamEvent>, ClientError> {
        if self.finished {
            return Ok(None);
        }
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        loop {
            let next = self.stream.recv().await;
            let Some(next) = next else {
                self.finished = true;
                return Ok(None);
            };
            let event = next?;
            let Some(projected) = self.projector.project(event) else {
                continue;
            };
            if self.projector.is_finished() {
                self.finished = true;
            }
            return Ok(Some(projected));
        }
    }

    pub async fn interrupt(&self) -> Result<golutra_protocol::CommandAck, ClientError> {
        self.transport
            .send_command(command(
                self.thread.session_id,
                SessionCommandKind::Abort,
                json!({"_thread_id": self.thread.thread_id}),
                &self.actor,
            ))
            .await
    }

    pub async fn steer(
        &self,
        prompt: impl Into<String>,
    ) -> Result<golutra_protocol::CommandAck, ClientError> {
        self.steer_with_options(prompt, AgentSteerOptions::default())
            .await
    }

    pub async fn steer_with_options(
        &self,
        prompt: impl Into<String>,
        options: AgentSteerOptions,
    ) -> Result<golutra_protocol::CommandAck, ClientError> {
        let prompt = prompt.into();
        if prompt.trim().is_empty() {
            return Err(ClientError::TaskExecution(
                "steering prompt cannot be empty".to_owned(),
            ));
        }
        let mut payload = json!({
            "prompt": prompt,
            "_thread_id": self.thread.thread_id,
            "steer": true,
        });
        if let Some(tool_profile) = options.tool_profile {
            payload["tool_profile"] = serde_json::to_value(tool_profile)?;
        }
        self.transport
            .send_command(command(
                self.thread.session_id,
                SessionCommandKind::Prompt,
                payload,
                &self.actor,
            ))
            .await
    }

    pub async fn resolve_approval(
        &self,
        approval_id: impl Into<String>,
        approve: bool,
    ) -> Result<golutra_protocol::CommandAck, ClientError> {
        self.transport
            .send_command(command(
                self.thread.session_id,
                if approve {
                    SessionCommandKind::Approve
                } else {
                    SessionCommandKind::Deny
                },
                json!({
                    "approval_id": approval_id.into(),
                    "_thread_id": self.thread.thread_id,
                }),
                &self.actor,
            ))
            .await
    }

    pub async fn wait(mut self) -> Result<AgentTurnResult, ClientError> {
        while self.next_event().await?.is_some() {}
        if self.projector.terminal_status().is_none() {
            return Err(ClientError::TaskExecution(
                "runtime event stream ended before turn completion".to_owned(),
            ));
        }
        self.projector.result().ok_or_else(|| {
            ClientError::TaskExecution("runtime turn result was not available".to_owned())
        })
    }
}

fn command(
    session_id: SessionId,
    kind: SessionCommandKind,
    payload: Value,
    actor: &Actor,
) -> SessionCommand {
    SessionCommand {
        command_id: CommandId::new(),
        session_id: Some(session_id),
        kind,
        idempotency_key: format!("agent-{}", CommandId::new()),
        actor: actor.clone(),
        payload,
        timestamp: chrono::Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use golutra_core::{Actor, ActorKind, CommandId, SessionId, ThreadId};
    use golutra_protocol::{
        AgentThreadRef, AgentTurnExecutionOptions, AgentTurnOptions, AgentTurnStart,
    };
    use tokio::sync::mpsc;

    use super::{AgentClient, TurnHandle, turn_payload};
    use crate::{EmbeddedTransport, RuntimeEventStream, RuntimeTransport};

    #[tokio::test]
    async fn turn_handle_rejects_empty_steering_before_sending_a_command() {
        let transport = RuntimeTransport::Embedded(
            EmbeddedTransport::in_memory()
                .await
                .expect("embedded transport"),
        );
        let actor = Actor {
            kind: ActorKind::Sdk,
            id: "agent-test-controller".to_owned(),
        };
        let client = AgentClient::with_actor(transport.clone(), actor.clone());
        assert_eq!(client.actor(), &actor);

        let thread = AgentThreadRef {
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            workspace_root: None,
        };
        let command_id = CommandId::new();
        let (_sender, receiver) = mpsc::channel(1);
        let handle = TurnHandle::new(
            thread.clone(),
            RuntimeEventStream::new(receiver),
            AgentTurnStart {
                thread_id: thread.thread_id,
                session_id: thread.session_id,
                command_id,
                task_id: None,
                turn_id: None,
                accepted: true,
                reason: None,
            },
            transport,
            actor,
        );

        let error = handle
            .steer("   ")
            .await
            .expect_err("empty steering must be rejected");
        assert!(
            error
                .to_string()
                .contains("steering prompt cannot be empty")
        );
    }

    #[test]
    fn turn_payload_distinguishes_new_defaults_from_legacy_server_defaults() {
        let thread_id = ThreadId::new();
        let options = AgentTurnOptions::default();

        let current = turn_payload(
            "current".to_owned(),
            thread_id,
            &options,
            Some(AgentTurnExecutionOptions::default()),
        )
        .expect("current payload");
        assert_eq!(current["execution_mode"], "open");
        assert_eq!(current["tool_profile"], "coding");

        let legacy =
            turn_payload("legacy".to_owned(), thread_id, &options, None).expect("legacy payload");
        assert!(legacy.get("execution_mode").is_none());
        assert!(legacy.get("tool_profile").is_none());
    }
}
