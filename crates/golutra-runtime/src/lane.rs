//! Runtime lane 状态机与控制权转换。

use std::collections::HashMap;

use golutra_core::{
    Actor, BusyPolicy, BusyPolicyDecision, CommandId, DecisionId, LaneId, RuntimeLane, SessionId,
    TaskId, TaskStatus, TurnId, WorkspaceId,
};
use golutra_protocol::{RuntimeEvent, RuntimeEventSource, RuntimeEventType};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RuntimeLaneError {
    #[error("session already has an active task")]
    ActiveTaskExists,
    #[error("session has no active runtime lane")]
    LaneNotFound,
    #[error("actor is not the active controller")]
    NonActiveController,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeTransition {
    pub lane: RuntimeLane,
    pub event: RuntimeEvent,
}

#[derive(Debug, Default)]
pub struct RuntimeLaneManager {
    lanes_by_session: HashMap<SessionId, RuntimeLane>,
}

impl RuntimeLaneManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start_task(
        &mut self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        task_id: TaskId,
        turn_id: TurnId,
        active_controller: Actor,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        if self
            .lanes_by_session
            .get(&session_id)
            .is_some_and(|lane| is_active_status(lane.status))
        {
            return Err(RuntimeLaneError::ActiveTaskExists);
        }

        let lane = RuntimeLane {
            lane_id: LaneId::new(),
            workspace_id,
            session_id,
            task_id,
            active_turn_id: Some(turn_id),
            active_controller,
            status: TaskStatus::Running,
            pending_turns: Vec::new(),
            injected_inputs: Vec::new(),
            busy_policy_default: BusyPolicy::Append,
        };
        self.lanes_by_session.insert(session_id, lane.clone());

        Ok(RuntimeTransition {
            event: lane_event(
                &lane,
                turn_id,
                sequence_no,
                RuntimeEventType::TaskCreated,
                "runtime lane started task",
            ),
            lane,
        })
    }

    pub fn decide_busy_policy(
        &self,
        session_id: SessionId,
        command_id: CommandId,
        actor: &Actor,
        requested_policy: BusyPolicy,
    ) -> Result<BusyPolicyDecision, RuntimeLaneError> {
        let lane = self
            .lanes_by_session
            .get(&session_id)
            .ok_or(RuntimeLaneError::LaneNotFound)?;
        let is_active_controller = lane.active_controller == *actor;

        let (applied_policy, reason) = if lane.status == TaskStatus::Aborting {
            (
                BusyPolicy::Reject,
                "active task is aborting and no longer accepts input",
            )
        } else if is_active_controller {
            (
                BusyPolicy::Append,
                "active controller input is appended to the runtime lane",
            )
        } else {
            (
                BusyPolicy::Reject,
                "non-active controller cannot drive the active task",
            )
        };

        Ok(BusyPolicyDecision {
            decision_id: DecisionId::new(),
            lane_id: lane.lane_id,
            command_id,
            requested_policy,
            applied_policy,
            reason: reason.to_owned(),
            safe_to_inject: false,
            affected_turn_id: lane.active_turn_id,
        })
    }

    pub fn takeover(
        &mut self,
        session_id: SessionId,
        new_controller: Actor,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        let lane = self
            .lanes_by_session
            .get_mut(&session_id)
            .filter(|lane| is_active_status(lane.status))
            .ok_or(RuntimeLaneError::LaneNotFound)?;
        let previous_controller = std::mem::replace(&mut lane.active_controller, new_controller);
        let turn_id = lane.active_turn_id.unwrap_or_default();
        let mut event = lane_event(
            lane,
            turn_id,
            sequence_no,
            RuntimeEventType::ControllerChanged,
            "active runtime controller changed",
        );
        event.payload["previous_controller"] = json!(previous_controller);
        event.payload["active_controller"] = json!(lane.active_controller);
        Ok(RuntimeTransition {
            lane: lane.clone(),
            event,
        })
    }

    pub fn queue_turn(
        &mut self,
        session_id: SessionId,
        turn_id: TurnId,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        let lane = self
            .lanes_by_session
            .get_mut(&session_id)
            .ok_or(RuntimeLaneError::LaneNotFound)?;
        lane.pending_turns.push(turn_id);
        Ok(RuntimeTransition {
            lane: lane.clone(),
            event: lane_event(
                lane,
                turn_id,
                sequence_no,
                RuntimeEventType::TurnQueued,
                "user turn queued on active runtime lane",
            ),
        })
    }

    pub fn start_queued_turn(
        &mut self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> Result<(), RuntimeLaneError> {
        let lane = self
            .lanes_by_session
            .get_mut(&session_id)
            .ok_or(RuntimeLaneError::LaneNotFound)?;
        lane.pending_turns.retain(|pending| *pending != turn_id);
        lane.active_turn_id = Some(turn_id);
        Ok(())
    }

    pub fn abort(
        &mut self,
        session_id: SessionId,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        if !self
            .lanes_by_session
            .get(&session_id)
            .is_some_and(|lane| is_active_status(lane.status))
        {
            return Err(RuntimeLaneError::LaneNotFound);
        }
        self.set_status(
            session_id,
            TaskStatus::Aborting,
            sequence_no,
            RuntimeEventType::TaskAbortRequested,
        )
    }

    pub fn pause(
        &mut self,
        session_id: SessionId,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        if !self.lanes_by_session.get(&session_id).is_some_and(|lane| {
            matches!(
                lane.status,
                TaskStatus::Running
                    | TaskStatus::WaitingApproval
                    | TaskStatus::WaitingAuthentication
                    | TaskStatus::Pausing
            )
        }) {
            return Err(RuntimeLaneError::LaneNotFound);
        }
        self.set_status(
            session_id,
            TaskStatus::Paused,
            sequence_no,
            RuntimeEventType::TaskPaused,
        )
    }

    pub fn resume(
        &mut self,
        session_id: SessionId,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        if !self
            .lanes_by_session
            .get(&session_id)
            .is_some_and(|lane| matches!(lane.status, TaskStatus::Pausing | TaskStatus::Paused))
        {
            return Err(RuntimeLaneError::LaneNotFound);
        }
        self.set_status(
            session_id,
            TaskStatus::Running,
            sequence_no,
            RuntimeEventType::TaskResumed,
        )
    }

    pub fn finish_task(
        &mut self,
        session_id: SessionId,
        status: TaskStatus,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        if !self
            .lanes_by_session
            .get(&session_id)
            .is_some_and(|lane| is_active_status(lane.status))
        {
            return Err(RuntimeLaneError::LaneNotFound);
        }
        let event_type = RuntimeEventType::for_terminal_status(status);
        self.set_status(session_id, status, sequence_no, event_type)
    }

    pub fn wait_for_authentication(
        &mut self,
        session_id: SessionId,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        self.set_status(
            session_id,
            TaskStatus::WaitingAuthentication,
            sequence_no,
            RuntimeEventType::ProviderAuthRequired,
        )
    }

    pub fn authentication_resolved(
        &mut self,
        session_id: SessionId,
        sequence_no: u64,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        if !self
            .lanes_by_session
            .get(&session_id)
            .is_some_and(|lane| lane.status == TaskStatus::WaitingAuthentication)
        {
            return Err(RuntimeLaneError::LaneNotFound);
        }
        self.set_status(
            session_id,
            TaskStatus::Running,
            sequence_no,
            RuntimeEventType::ProviderAuthSubmitted,
        )
    }

    #[must_use]
    pub fn lane(&self, session_id: SessionId) -> Option<&RuntimeLane> {
        self.lanes_by_session.get(&session_id)
    }

    fn set_status(
        &mut self,
        session_id: SessionId,
        status: TaskStatus,
        sequence_no: u64,
        event_type: RuntimeEventType,
    ) -> Result<RuntimeTransition, RuntimeLaneError> {
        let lane = self
            .lanes_by_session
            .get_mut(&session_id)
            .ok_or(RuntimeLaneError::LaneNotFound)?;
        lane.status = status;
        let turn_id = lane.active_turn_id.unwrap_or_default();
        Ok(RuntimeTransition {
            lane: lane.clone(),
            event: lane_event(
                lane,
                turn_id,
                sequence_no,
                event_type,
                "runtime lane status changed",
            ),
        })
    }
}

#[must_use]
pub fn is_active_status(status: TaskStatus) -> bool {
    status.is_active()
}

fn lane_event(
    lane: &RuntimeLane,
    turn_id: TurnId,
    sequence_no: u64,
    event_type: RuntimeEventType,
    summary: &str,
) -> RuntimeEvent {
    RuntimeEvent {
        id: golutra_core::EventId::new(),
        sequence_no,
        session_id: lane.session_id,
        turn_id: Some(turn_id),
        task_id: Some(lane.task_id),
        parent_event_id: None,
        event_type,
        timestamp: chrono::Utc::now(),
        source: RuntimeEventSource::Runtime,
        payload: json!({
            "summary": summary,
            "lane_id": lane.lane_id.to_string(),
            "status": lane.status,
            "active_controller": lane.active_controller,
            "runtime_lane": lane,
        }),
        payload_ref: None,
        durable: true,
    }
}
