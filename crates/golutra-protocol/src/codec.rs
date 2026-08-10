//! Versioned wire codec for command and event envelopes.
//!
//! Runtime internals keep using typed `SessionCommand` and `RuntimeEvent`
//! values.  Transports cross this module instead of teaching each adapter how
//! to parse arbitrary JSON.  The legacy raw form is accepted on decode so an
//! older client can be upgraded independently. Protocol v7 retains raw values;
//! protocol v8 and later use the versioned envelope.

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;

use crate::{
    ProtocolVersionRange, RUNTIME_PROTOCOL_NAME, RUNTIME_PROTOCOL_VERSION, RuntimeEvent,
    RuntimeEventType, SessionCommand, SessionCommandKind, VERSIONED_WIRE_PROTOCOL_VERSION,
};

pub const WIRE_CODEC_VERSION: u32 = 1;
pub const MAX_WIRE_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WirePayloadKind {
    Command,
    Event,
}

#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct CommandEnvelope {
    pub protocol: String,
    pub codec_version: u32,
    pub payload_kind: WirePayloadKind,
    pub command_kind: SessionCommandKind,
    pub command: SessionCommand,
}

#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct EventEnvelope {
    pub protocol: String,
    pub codec_version: u32,
    pub payload_kind: WirePayloadKind,
    pub event_type: RuntimeEventType,
    pub event: RuntimeEvent,
}

#[derive(Debug, Error)]
pub enum ProtocolCodecError {
    #[error("wire payload exceeds {MAX_WIRE_MESSAGE_BYTES} bytes")]
    MessageTooLarge,
    #[error("wire payload JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("wire protocol `{actual}` is not `{expected}`")]
    ProtocolMismatch {
        expected: &'static str,
        actual: String,
    },
    #[error("wire codec version {actual} is not supported (expected {expected})")]
    VersionMismatch { expected: u32, actual: u32 },
    #[error("runtime protocol version {actual} is not supported (expected {minimum}..={current})")]
    RuntimeVersionMismatch {
        minimum: u32,
        current: u32,
        actual: u32,
    },
    #[error("wire payload kind is not `{expected:?}`")]
    PayloadKindMismatch {
        expected: WirePayloadKind,
        actual: WirePayloadKind,
    },
    #[error(
        "wire command kind does not match its payload: envelope={envelope:?}, command={command:?}"
    )]
    CommandKindMismatch {
        envelope: SessionCommandKind,
        command: SessionCommandKind,
    },
    #[error("wire event type does not match its payload: envelope={envelope:?}, event={event:?}")]
    EventTypeMismatch {
        envelope: RuntimeEventType,
        event: RuntimeEventType,
    },
}

pub fn encode_command_value(command: &SessionCommand) -> Result<Value, ProtocolCodecError> {
    encode_command_value_for_protocol(command, RUNTIME_PROTOCOL_VERSION)
}

pub fn encode_command_value_for_protocol(
    command: &SessionCommand,
    protocol_version: u32,
) -> Result<Value, ProtocolCodecError> {
    if !uses_versioned_envelope(protocol_version)? {
        return encode_value_bounded(command);
    }
    encode_value_bounded(&CommandEnvelope {
        protocol: RUNTIME_PROTOCOL_NAME.to_owned(),
        codec_version: WIRE_CODEC_VERSION,
        payload_kind: WirePayloadKind::Command,
        command_kind: command.kind,
        command: command.clone(),
    })
}

pub fn encode_command(command: &SessionCommand) -> Result<Vec<u8>, ProtocolCodecError> {
    encode_command_for_protocol(command, RUNTIME_PROTOCOL_VERSION)
}

pub fn encode_command_for_protocol(
    command: &SessionCommand,
    protocol_version: u32,
) -> Result<Vec<u8>, ProtocolCodecError> {
    if !uses_versioned_envelope(protocol_version)? {
        return encode_bounded(command);
    }
    encode_bounded(&CommandEnvelope {
        protocol: RUNTIME_PROTOCOL_NAME.to_owned(),
        codec_version: WIRE_CODEC_VERSION,
        payload_kind: WirePayloadKind::Command,
        command_kind: command.kind,
        command: command.clone(),
    })
}

pub fn decode_command_value(value: Value) -> Result<SessionCommand, ProtocolCodecError> {
    ensure_value_bounded(&value)?;
    if value.get("codec_version").is_none() {
        return Ok(serde_json::from_value(value)?);
    }
    decode_envelope(value, WirePayloadKind::Command).map(|envelope: CommandEnvelope| {
        validate_command_envelope(&envelope)?;
        Ok(envelope.command)
    })?
}

pub fn decode_command(bytes: &[u8]) -> Result<SessionCommand, ProtocolCodecError> {
    ensure_bounded(bytes)?;
    decode_command_value(serde_json::from_slice(bytes)?)
}

pub fn encode_event_value(event: &RuntimeEvent) -> Result<Value, ProtocolCodecError> {
    encode_event_value_for_protocol(event, RUNTIME_PROTOCOL_VERSION)
}

pub fn encode_event_value_for_protocol(
    event: &RuntimeEvent,
    protocol_version: u32,
) -> Result<Value, ProtocolCodecError> {
    if !uses_versioned_envelope(protocol_version)? {
        return encode_value_bounded(event);
    }
    encode_value_bounded(&EventEnvelope {
        protocol: RUNTIME_PROTOCOL_NAME.to_owned(),
        codec_version: WIRE_CODEC_VERSION,
        payload_kind: WirePayloadKind::Event,
        event_type: event.event_type,
        event: event.clone(),
    })
}

pub fn encode_event(event: &RuntimeEvent) -> Result<Vec<u8>, ProtocolCodecError> {
    encode_event_for_protocol(event, RUNTIME_PROTOCOL_VERSION)
}

pub fn encode_event_for_protocol(
    event: &RuntimeEvent,
    protocol_version: u32,
) -> Result<Vec<u8>, ProtocolCodecError> {
    if !uses_versioned_envelope(protocol_version)? {
        return encode_bounded(event);
    }
    encode_bounded(&EventEnvelope {
        protocol: RUNTIME_PROTOCOL_NAME.to_owned(),
        codec_version: WIRE_CODEC_VERSION,
        payload_kind: WirePayloadKind::Event,
        event_type: event.event_type,
        event: event.clone(),
    })
}

pub fn decode_event_value(value: Value) -> Result<RuntimeEvent, ProtocolCodecError> {
    ensure_value_bounded(&value)?;
    if value.get("codec_version").is_none() {
        return Ok(serde_json::from_value(value)?);
    }
    decode_envelope(value, WirePayloadKind::Event).map(|envelope: EventEnvelope| {
        validate_event_envelope(&envelope)?;
        Ok(envelope.event)
    })?
}

pub fn decode_event(bytes: &[u8]) -> Result<RuntimeEvent, ProtocolCodecError> {
    ensure_bounded(bytes)?;
    decode_event_value(serde_json::from_slice(bytes)?)
}

fn encode_bounded<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtocolCodecError> {
    let bytes = serde_json::to_vec(value)?;
    ensure_bounded(&bytes)?;
    Ok(bytes)
}

fn encode_value_bounded<T: Serialize>(value: &T) -> Result<Value, ProtocolCodecError> {
    let encoded = serde_json::to_vec(value)?;
    ensure_bounded(&encoded)?;
    Ok(serde_json::from_slice(&encoded)?)
}

fn uses_versioned_envelope(protocol_version: u32) -> Result<bool, ProtocolCodecError> {
    let supported = ProtocolVersionRange::runtime();
    if !supported.accepts(protocol_version) {
        return Err(ProtocolCodecError::RuntimeVersionMismatch {
            minimum: supported.minimum,
            current: supported.current,
            actual: protocol_version,
        });
    }
    Ok(protocol_version >= VERSIONED_WIRE_PROTOCOL_VERSION)
}

fn ensure_value_bounded(value: &Value) -> Result<(), ProtocolCodecError> {
    ensure_bounded(&serde_json::to_vec(value)?)
}

fn ensure_bounded(bytes: &[u8]) -> Result<(), ProtocolCodecError> {
    (bytes.len() <= MAX_WIRE_MESSAGE_BYTES)
        .then_some(())
        .ok_or(ProtocolCodecError::MessageTooLarge)
}

fn decode_envelope<T: DeserializeOwned>(
    value: Value,
    expected_kind: WirePayloadKind,
) -> Result<T, ProtocolCodecError> {
    let kind = value
        .get("payload_kind")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or(WirePayloadKind::Command);
    if kind != expected_kind {
        return Err(ProtocolCodecError::PayloadKindMismatch {
            expected: expected_kind,
            actual: kind,
        });
    }
    Ok(serde_json::from_value(value)?)
}

fn validate_common(
    protocol: &str,
    codec_version: u32,
    payload_kind: WirePayloadKind,
    expected_kind: WirePayloadKind,
) -> Result<(), ProtocolCodecError> {
    if protocol != RUNTIME_PROTOCOL_NAME {
        return Err(ProtocolCodecError::ProtocolMismatch {
            expected: RUNTIME_PROTOCOL_NAME,
            actual: protocol.to_owned(),
        });
    }
    if codec_version != WIRE_CODEC_VERSION {
        return Err(ProtocolCodecError::VersionMismatch {
            expected: WIRE_CODEC_VERSION,
            actual: codec_version,
        });
    }
    if payload_kind != expected_kind {
        return Err(ProtocolCodecError::PayloadKindMismatch {
            expected: expected_kind,
            actual: payload_kind,
        });
    }
    Ok(())
}

fn validate_command_envelope(envelope: &CommandEnvelope) -> Result<(), ProtocolCodecError> {
    validate_common(
        &envelope.protocol,
        envelope.codec_version,
        envelope.payload_kind,
        WirePayloadKind::Command,
    )?;
    if envelope.command_kind != envelope.command.kind {
        return Err(ProtocolCodecError::CommandKindMismatch {
            envelope: envelope.command_kind,
            command: envelope.command.kind,
        });
    }
    Ok(())
}

fn validate_event_envelope(envelope: &EventEnvelope) -> Result<(), ProtocolCodecError> {
    validate_common(
        &envelope.protocol,
        envelope.codec_version,
        envelope.payload_kind,
        WirePayloadKind::Event,
    )?;
    if envelope.event_type != envelope.event.event_type {
        return Err(ProtocolCodecError::EventTypeMismatch {
            envelope: envelope.event_type,
            event: envelope.event.event_type,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use golutra_core::{Actor, ActorKind, SessionId};
    use serde_json::json;

    use super::*;

    fn command() -> SessionCommand {
        SessionCommand {
            command_id: golutra_core::CommandId::new(),
            session_id: Some(SessionId::new()),
            kind: SessionCommandKind::Prompt,
            idempotency_key: "codec-test".to_owned(),
            actor: Actor {
                kind: ActorKind::Api,
                id: "codec-test".to_owned(),
            },
            payload: json!({"prompt": "hello"}),
            timestamp: chrono::Utc::now(),
        }
    }

    fn event() -> RuntimeEvent {
        RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            id: golutra_core::EventId::new(),
            sequence_no: 1,
            session_id: SessionId::new(),
            turn_id: None,
            task_id: None,
            parent_event_id: None,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            event_type: RuntimeEventType::CommandReceived,
            timestamp: chrono::Utc::now(),
            source: crate::RuntimeEventSource::Runtime,
            payload: json!({"summary": "received"}),
            payload_ref: None,
            durable: true,
        }
    }

    #[test]
    fn versioned_command_and_event_envelopes_round_trip() {
        let command = command();
        let encoded = encode_command(&command).expect("command encoding");
        assert_eq!(decode_command(&encoded).expect("command decoding"), command);

        let event = event();
        let encoded = encode_event(&event).expect("event encoding");
        assert_eq!(decode_event(&encoded).expect("event decoding"), event);
    }

    #[test]
    fn protocol_seven_uses_raw_payloads_and_protocol_eight_uses_envelopes() {
        let command = command();
        let legacy_command =
            encode_command_value_for_protocol(&command, 7).expect("legacy command");
        assert!(legacy_command.get("codec_version").is_none());
        assert_eq!(legacy_command["command_id"], json!(command.command_id));

        let event = event();
        let legacy_event = encode_event_value_for_protocol(&event, 7).expect("legacy event");
        assert!(legacy_event.get("codec_version").is_none());
        assert_eq!(legacy_event["sequence_no"], json!(event.sequence_no));

        let current_event =
            encode_event_value_for_protocol(&event, 8).expect("versioned event envelope");
        assert_eq!(current_event["codec_version"], json!(WIRE_CODEC_VERSION));
        assert_eq!(
            current_event["event"]["sequence_no"],
            json!(event.sequence_no)
        );
    }

    #[test]
    fn decoder_accepts_legacy_raw_payloads_but_validates_new_envelopes() {
        let command = command();
        let legacy = serde_json::to_value(&command).expect("legacy command");
        assert_eq!(
            decode_command_value(legacy).expect("legacy decoding"),
            command
        );

        let mut invalid = encode_command_value(&command).expect("encoding");
        invalid["command_kind"] = json!("abort");
        let error = decode_command_value(invalid).expect_err("mismatched kind");
        assert!(matches!(
            error,
            ProtocolCodecError::CommandKindMismatch { .. }
        ));
    }

    #[test]
    fn decoder_rejects_unknown_versions_and_oversized_messages() {
        let command = command();
        let mut invalid = encode_command_value(&command).expect("encoding");
        invalid["codec_version"] = json!(WIRE_CODEC_VERSION + 1);
        let error = decode_command_value(invalid).expect_err("unknown version");
        assert!(matches!(error, ProtocolCodecError::VersionMismatch { .. }));

        let oversized = vec![b' '; MAX_WIRE_MESSAGE_BYTES + 1];
        assert!(matches!(
            decode_command(&oversized),
            Err(ProtocolCodecError::MessageTooLarge)
        ));
    }
}
