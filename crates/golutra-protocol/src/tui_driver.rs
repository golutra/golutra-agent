//! Versioned protocol for agent-controlled, offscreen TUI inspection.

use golutra_core::{RedactionStatus, TaskStatus};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const TUI_DRIVER_PROTOCOL_VERSION: u32 = 1;
pub const TUI_DRIVER_MIN_PROTOCOL_VERSION: u32 = 1;
pub const TUI_DRIVER_MAX_WIDTH: u16 = 320;
pub const TUI_DRIVER_MAX_HEIGHT: u16 = 200;
pub const TUI_DRIVER_MAX_RETURNED_ROWS: u32 = 200;

/// Schema root for generated clients. The values are never instantiated at
/// runtime; grouping them keeps the versioned request and response contract in
/// one generated SDK namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TuiDriverProtocolBundle {
    pub request: DriverEnvelope,
    pub response: DriverResponseEnvelope,
    pub snapshot: TuiFrame,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DriverRequest {
    Hello {
        #[serde(default)]
        protocol_version: Option<u32>,
    },
    Capabilities,
    State,
    Ping,
    InputPrompt {
        text: String,
    },
    InputSlash {
        text: String,
    },
    InputKey {
        key: DriverKey,
    },
    InputPaste {
        text: String,
    },
    InputMouse {
        event: DriverMouseEvent,
    },
    Resize {
        width: u16,
        height: u16,
    },
    Wait {
        until: WaitCondition,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    Snapshot {
        #[serde(flatten)]
        request: SnapshotRequest,
    },
    Metrics,
    Takeover,
    Abort,
    Close {
        #[serde(default)]
        abort_active_task: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DriverKey {
    Enter,
    Escape,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Backspace,
    Delete,
    Tab,
    Char(String),
    CtrlC,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DriverMouseEvent {
    pub kind: DriverMouseKind,
    pub column: u16,
    pub row: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DriverMouseKind {
    LeftClick,
    ScrollUp,
    ScrollDown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitCondition {
    Ready,
    Idle,
    TaskStarted,
    TaskTerminal,
    TurnTerminal,
    ApprovalRequired,
    AuthenticationRequired,
    EvaluationTerminal,
    Event {
        event_type: String,
        #[serde(default)]
        sequence_at_least: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotRequest {
    #[serde(default)]
    pub scope: SnapshotScope,
    #[serde(default)]
    pub panes: SnapshotPanes,
    pub width: u16,
    pub height: u16,
    #[serde(default)]
    pub rows: Option<RowRange>,
    #[serde(default)]
    pub frame_id: Option<String>,
    #[serde(default)]
    pub detail: SnapshotDetail,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotScope {
    #[default]
    CurrentTurn,
    Task,
    Session,
    Screen,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotPanes {
    #[default]
    Transcript,
    Developer,
    ResponseAndDeveloper,
    FullScreen,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotDetail {
    #[default]
    Text,
    Cells,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RowRange {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DriverEnvelope {
    pub request_id: String,
    #[serde(flatten)]
    pub request: DriverRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DriverResponse {
    Ready {
        #[serde(flatten)]
        ready: ReadyResponse,
    },
    Capabilities {
        capabilities: Vec<String>,
    },
    State {
        #[serde(flatten)]
        state: DriverState,
    },
    Pong,
    Snapshot {
        #[serde(flatten)]
        frame: TuiFrame,
    },
    Metrics {
        metrics: DriverMetrics,
    },
    Accepted {
        message: String,
    },
    WaitResult {
        condition: WaitCondition,
        state: DriverState,
    },
    WaitTimeout {
        condition: WaitCondition,
        state: DriverState,
    },
    Event {
        event: DriverNotification,
    },
    Closed,
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DriverResponseEnvelope {
    pub request_id: String,
    #[serde(flatten)]
    pub response: DriverResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReadyResponse {
    pub protocol_version: u32,
    pub minimum_protocol_version: u32,
    pub instance_id: String,
    pub workspace_id: String,
    pub workspace_path: String,
    pub thread_id: String,
    pub session_id: String,
    pub controller_mode: DriverControllerMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DriverControllerMode {
    Controller,
    Observer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DriverTaskStatus {
    Connecting,
    Idle,
    Running,
    WaitingApproval,
    WaitingAuthentication,
    Pausing,
    Paused,
    Aborting,
    Completed,
    Partial,
    Failed,
    Blocked,
    Cancelled,
    Interrupted,
    Uncertain,
}

impl From<TaskStatus> for DriverTaskStatus {
    fn from(status: TaskStatus) -> Self {
        match status {
            TaskStatus::Idle => Self::Idle,
            TaskStatus::Running => Self::Running,
            TaskStatus::WaitingApproval => Self::WaitingApproval,
            TaskStatus::WaitingAuthentication => Self::WaitingAuthentication,
            TaskStatus::Pausing => Self::Pausing,
            TaskStatus::Paused => Self::Paused,
            TaskStatus::Aborting => Self::Aborting,
            TaskStatus::Completed => Self::Completed,
            TaskStatus::Partial => Self::Partial,
            TaskStatus::Failed => Self::Failed,
            TaskStatus::Blocked => Self::Blocked,
            TaskStatus::Cancelled => Self::Cancelled,
            TaskStatus::Interrupted => Self::Interrupted,
            TaskStatus::Uncertain => Self::Uncertain,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DriverState {
    pub instance_id: String,
    pub thread_id: String,
    pub session_id: String,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub status: DriverTaskStatus,
    pub width: u16,
    pub height: u16,
    pub facts_expanded: bool,
    pub controller_mode: DriverControllerMode,
    pub closed: bool,
}

/// Redacted, low-cardinality timing aggregates exposed by the native Driver.
///
/// This deliberately contains no request payloads, rendered text, workspace
/// paths, provider identifiers, or credential material.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DriverLatencyMetrics {
    pub samples: u64,
    pub total_ms: u64,
    pub max_ms: u64,
    pub last_ms: u64,
}

/// Operational counters for diagnosing a long-lived Driver instance.
///
/// The values are process-local and cumulative until the Driver exits. The
/// `pending_waits` and `frame_cache_entries` fields are live gauges.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DriverMetrics {
    pub instance_id: String,
    pub connections: u64,
    pub reconnects: u64,
    pub rejected_connections: u64,
    pub requests: u64,
    pub request_errors: u64,
    pub snapshot_requests: u64,
    pub snapshot_renders: u64,
    pub frozen_frame_hits: u64,
    pub frozen_frame_misses: u64,
    pub snapshot_latency: DriverLatencyMetrics,
    pub wait_requests: u64,
    pub wait_results: u64,
    pub wait_timeouts: u64,
    pub wait_cancelled: u64,
    pub pending_waits: u64,
    pub wait_latency: DriverLatencyMetrics,
    pub sync_attempts: u64,
    pub sync_errors: u64,
    pub sync_latency: DriverLatencyMetrics,
    pub frame_cache_entries: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DriverNotification {
    pub kind: DriverNotificationKind,
    pub sequence_no: Option<u64>,
    pub status: Option<DriverTaskStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DriverNotificationKind {
    Heartbeat,
    RuntimeEventAvailable,
    StateChanged,
    TaskTerminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TuiFrame {
    pub frame_id: String,
    pub instance_id: String,
    pub workspace_id: String,
    pub session_id: String,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub event_high_watermark: Option<u64>,
    pub width: u16,
    pub height: u16,
    pub scope: SnapshotScope,
    pub panes: SnapshotPanes,
    pub total_rows: u32,
    pub returned_range: RowRange,
    pub lines: Vec<TuiFrameLine>,
    pub complete: bool,
    pub missing_sections: Vec<String>,
    pub redaction_status: RedactionStatus,
    pub next_range: Option<RowRange>,
    #[serde(default)]
    pub hit_regions: Vec<TuiHitRegion>,
    #[serde(default)]
    pub cells: Option<Vec<TuiFrameCell>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TuiFrameLine {
    pub row: u32,
    pub text: String,
    pub display_width: u16,
    pub pane: TuiFramePane,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TuiFramePane {
    Transcript,
    Developer,
    ResponseAndDeveloper,
    Screen,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TuiHitRegion {
    pub id: String,
    pub pane: TuiHitPane,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TuiHitPane {
    Transcript,
    Bottom,
    Developer,
    Overlay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TuiFrameCell {
    pub row: u16,
    pub column: u16,
    pub symbol: String,
    pub pane: TuiFramePane,
    pub foreground: String,
    pub background: String,
    pub modifiers: String,
}

impl SnapshotRequest {
    pub fn validate(&self) -> Result<(), String> {
        if !(40..=TUI_DRIVER_MAX_WIDTH).contains(&self.width) {
            return Err(format!(
                "width must be between 40 and {TUI_DRIVER_MAX_WIDTH}"
            ));
        }
        if !(8..=TUI_DRIVER_MAX_HEIGHT).contains(&self.height) {
            return Err(format!(
                "height must be between 8 and {TUI_DRIVER_MAX_HEIGHT}"
            ));
        }
        if u32::from(self.width) * u32::from(self.height) > 64_000 {
            return Err("frame exceeds the 64K cell limit".to_owned());
        }
        if let Some(rows) = self.rows {
            if rows.start == 0 || rows.end < rows.start {
                return Err("rows must be a 1-based inclusive range".to_owned());
            }
            if rows.end.saturating_sub(rows.start).saturating_add(1) > TUI_DRIVER_MAX_RETURNED_ROWS
            {
                return Err(format!(
                    "rows may return at most {TUI_DRIVER_MAX_RETURNED_ROWS} lines"
                ));
            }
        }
        Ok(())
    }
}

pub fn response(request_id: impl Into<String>, response: DriverResponse) -> DriverResponseEnvelope {
    DriverResponseEnvelope {
        request_id: request_id.into(),
        response,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_bounds_are_explicit() {
        let valid = SnapshotRequest {
            scope: SnapshotScope::CurrentTurn,
            panes: SnapshotPanes::ResponseAndDeveloper,
            width: 160,
            height: 40,
            rows: Some(RowRange { start: 1, end: 40 }),
            frame_id: None,
            detail: SnapshotDetail::Text,
        };
        assert!(valid.validate().is_ok());

        let mut invalid = valid.clone();
        invalid.width = 321;
        assert!(invalid.validate().is_err());
        invalid.width = 160;
        invalid.rows = Some(RowRange { start: 0, end: 1 });
        assert!(invalid.validate().is_err());
        invalid.rows = Some(RowRange { start: 1, end: 201 });
        assert!(invalid.validate().is_err());
        invalid.rows = None;
        invalid.width = 320;
        invalid.height = 201;
        assert!(invalid.validate().is_err());
        invalid.height = 200;
        assert!(invalid.validate().is_ok());
    }

    #[test]
    fn request_envelopes_roundtrip() {
        let encoded = serde_json::json!({
            "request_id": "request-1",
            "type": "snapshot",
            "scope": "current_turn",
            "panes": "transcript",
            "width": 120,
            "height": 30
        });
        let request: DriverEnvelope = serde_json::from_value(encoded).expect("request");
        assert!(matches!(request.request, DriverRequest::Snapshot { .. }));
        let roundtrip = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(roundtrip["request_id"], "request-1");
        assert_eq!(roundtrip["type"], "snapshot");
    }

    #[test]
    fn protocol_control_and_event_waits_roundtrip() {
        let requests = [
            serde_json::json!({
                "request_id": "hello",
                "type": "hello",
                "protocol_version": TUI_DRIVER_PROTOCOL_VERSION
            }),
            serde_json::json!({
                "request_id": "wait",
                "type": "wait",
                "until": {
                    "kind": "event",
                    "event_type": "task_completed",
                    "sequence_at_least": 42
                },
                "timeout_ms": 1000
            }),
            serde_json::json!({
                "request_id": "mouse",
                "type": "input_mouse",
                "event": {"kind": "scroll_down", "column": 10, "row": 4}
            }),
            serde_json::json!({
                "request_id": "close",
                "type": "close",
                "abort_active_task": false
            }),
            serde_json::json!({
                "request_id": "metrics",
                "type": "metrics"
            }),
        ];
        for request in requests {
            let decoded: DriverEnvelope =
                serde_json::from_value(request.clone()).expect("decode request");
            let encoded = serde_json::to_value(decoded).expect("encode request");
            assert_eq!(encoded, request);
        }

        let response = response(
            "wait",
            DriverResponse::WaitTimeout {
                condition: WaitCondition::EvaluationTerminal,
                state: DriverState {
                    instance_id: "instance".to_owned(),
                    thread_id: "thread".to_owned(),
                    session_id: "session".to_owned(),
                    task_id: None,
                    turn_id: None,
                    status: DriverTaskStatus::Running,
                    width: 120,
                    height: 30,
                    facts_expanded: false,
                    controller_mode: DriverControllerMode::Controller,
                    closed: false,
                },
            },
        );
        let encoded = serde_json::to_vec(&response).expect("response JSON");
        let decoded: DriverResponseEnvelope =
            serde_json::from_slice(&encoded).expect("response roundtrip");
        assert_eq!(decoded, response);

        let metrics = super::response(
            "metrics",
            DriverResponse::Metrics {
                metrics: DriverMetrics {
                    instance_id: "instance".to_owned(),
                    connections: 2,
                    reconnects: 1,
                    rejected_connections: 1,
                    requests: 9,
                    request_errors: 1,
                    snapshot_requests: 2,
                    snapshot_renders: 1,
                    frozen_frame_hits: 1,
                    frozen_frame_misses: 0,
                    snapshot_latency: DriverLatencyMetrics {
                        samples: 2,
                        total_ms: 12,
                        max_ms: 8,
                        last_ms: 8,
                    },
                    wait_requests: 3,
                    wait_results: 1,
                    wait_timeouts: 1,
                    wait_cancelled: 1,
                    pending_waits: 0,
                    wait_latency: DriverLatencyMetrics::default(),
                    sync_attempts: 4,
                    sync_errors: 0,
                    sync_latency: DriverLatencyMetrics::default(),
                    frame_cache_entries: 1,
                },
            },
        );
        let encoded = serde_json::to_vec(&metrics).expect("metrics JSON");
        let decoded: DriverResponseEnvelope =
            serde_json::from_slice(&encoded).expect("metrics roundtrip");
        assert_eq!(decoded, metrics);
        let encoded = String::from_utf8(encoded).expect("metrics UTF-8");
        assert!(!encoded.contains("workspace_path"));
        assert!(!encoded.contains("prompt"));
        assert!(!encoded.contains("secret"));
    }
}
