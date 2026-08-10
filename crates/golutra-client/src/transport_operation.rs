//! Typed operations shared by runtime transports.
//!
//! Adapters own framing, authentication and reconnection. The operation
//! interface keeps the runtime-facing method set in one place so an embedded
//! caller and an app-server caller cannot silently drift in command, query or
//! trace semantics.

use golutra_protocol::{
    ArtifactChunk, ArtifactReadRequest, CommandAck, EventFilter, EventPage, EventPageRequest,
    RuntimeQuery, SessionCommand, TaskTracePage, TaskTraceRequest,
};
use serde_json::Value;

use super::RuntimeEventStream;
use crate::ClientError;

#[derive(Debug)]
pub enum RuntimeOperation {
    SendCommand(SessionCommand),
    Query(RuntimeQuery),
    EventPage(EventPageRequest),
    ReplayEvents(EventFilter),
    Subscribe(EventFilter),
    TaskTrace(TaskTraceRequest),
    ReadArtifactChunk(ArtifactReadRequest),
}

impl RuntimeOperation {
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::SendCommand(_) => "command",
            Self::Query(_) => "query",
            Self::EventPage(_) => "event_page",
            Self::ReplayEvents(_) => "replay_events",
            Self::Subscribe(_) => "subscribe",
            Self::TaskTrace(_) => "task_trace",
            Self::ReadArtifactChunk(_) => "read_artifact_chunk",
        }
    }
}

#[derive(Debug)]
pub enum RuntimeOperationResult {
    CommandAck(CommandAck),
    Query(Value),
    EventPage(EventPage),
    ReplayEvents(Vec<Value>),
    Subscription(RuntimeEventStream),
    TaskTrace(Box<TaskTracePage>),
    ArtifactChunk(Option<ArtifactChunk>),
}

impl RuntimeOperationResult {
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::CommandAck(_) => "command_ack",
            Self::Query(_) => "query",
            Self::EventPage(_) => "event_page",
            Self::ReplayEvents(_) => "replay_events",
            Self::Subscription(_) => "subscription",
            Self::TaskTrace(_) => "task_trace",
            Self::ArtifactChunk(_) => "artifact_chunk",
        }
    }

    pub fn into_json(self) -> Result<Value, ClientError> {
        match self {
            Self::CommandAck(value) => Ok(serde_json::to_value(value)?),
            Self::Query(value) => Ok(value),
            Self::EventPage(value) => Ok(serde_json::to_value(value)?),
            Self::ReplayEvents(value) => Ok(Value::Array(value)),
            Self::TaskTrace(value) => Ok(serde_json::to_value(value.as_ref())?),
            Self::ArtifactChunk(value) => Ok(serde_json::to_value(value)?),
            Self::Subscription(_) => Err(ClientError::TaskExecution(
                "runtime subscription cannot be represented as one JSON response".to_owned(),
            )),
        }
    }

    pub fn into_command_ack(self) -> Result<CommandAck, ClientError> {
        match self {
            Self::CommandAck(ack) => Ok(ack),
            result => Err(result.type_mismatch("command_ack")),
        }
    }

    pub fn into_query(self) -> Result<Value, ClientError> {
        match self {
            Self::Query(value) => Ok(value),
            result => Err(result.type_mismatch("query")),
        }
    }

    pub fn into_event_page(self) -> Result<EventPage, ClientError> {
        match self {
            Self::EventPage(page) => Ok(page),
            result => Err(result.type_mismatch("event_page")),
        }
    }

    pub fn into_replayed_events(self) -> Result<Vec<Value>, ClientError> {
        match self {
            Self::ReplayEvents(events) => Ok(events),
            result => Err(result.type_mismatch("replay_events")),
        }
    }

    pub fn into_subscription(self) -> Result<RuntimeEventStream, ClientError> {
        match self {
            Self::Subscription(stream) => Ok(stream),
            result => Err(result.type_mismatch("subscription")),
        }
    }

    pub fn into_task_trace(self) -> Result<TaskTracePage, ClientError> {
        match self {
            Self::TaskTrace(trace) => Ok(*trace),
            result => Err(result.type_mismatch("task_trace")),
        }
    }

    pub fn into_artifact_chunk(self) -> Result<Option<ArtifactChunk>, ClientError> {
        match self {
            Self::ArtifactChunk(chunk) => Ok(chunk),
            result => Err(result.type_mismatch("artifact_chunk")),
        }
    }

    fn type_mismatch(&self, expected: &str) -> ClientError {
        ClientError::TaskExecution(format!(
            "expected runtime {expected} result, received {}",
            self.kind()
        ))
    }
}
