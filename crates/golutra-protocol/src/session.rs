use golutra_core::{SessionId, ThreadId, Timestamp, TurnId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionRangeDirection {
    Single,
    Newer,
    Older,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionRangeSpec {
    pub direction: SessionRangeDirection,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSummary {
    pub thread_id: ThreadId,
    pub session_id: SessionId,
    pub parent_thread_id: Option<ThreadId>,
    pub forked_from_turn_id: Option<TurnId>,
    pub title: String,
    pub preview: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub recency_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionCursor {
    pub recency_at: Timestamp,
    pub thread_id: ThreadId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionPageRequest {
    pub cursor: Option<SessionCursor>,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionPage {
    pub sessions: Vec<SessionSummary>,
    pub next_cursor: Option<SessionCursor>,
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionWindowRequest {
    pub anchor_thread_id: ThreadId,
    pub range: SessionRangeSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionWindow {
    pub anchor_thread_id: ThreadId,
    pub range: SessionRangeSpec,
    pub sessions: Vec<SessionSummary>,
    pub reached_boundary: bool,
}
