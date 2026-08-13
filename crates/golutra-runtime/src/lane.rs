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
    #[error("queued turn is not pending on the runtime lane")]
    QueuedTurnNotPending,
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
        let lane_id = self.prepare_queued_turn_start(session_id, turn_id)?;
        self.commit_queued_turn_start(session_id, lane_id, turn_id)
    }

    /// Validates a queued-turn transition without changing the lane.
    ///
    /// The returned lane identity must be passed to [`Self::commit_queued_turn_start`]
    /// after the corresponding `TurnStarted` event is durable.
    pub fn prepare_queued_turn_start(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> Result<LaneId, RuntimeLaneError> {
        let lane = self
            .lanes_by_session
            .get(&session_id)
            .ok_or(RuntimeLaneError::LaneNotFound)?;
        if !lane.pending_turns.contains(&turn_id) {
            return Err(RuntimeLaneError::QueuedTurnNotPending);
        }
        Ok(lane.lane_id)
    }

    pub fn commit_queued_turn_start(
        &mut self,
        session_id: SessionId,
        lane_id: LaneId,
        turn_id: TurnId,
    ) -> Result<(), RuntimeLaneError> {
        let lane = self
            .lanes_by_session
            .get_mut(&session_id)
            .filter(|lane| lane.lane_id == lane_id)
            .ok_or(RuntimeLaneError::LaneNotFound)?;
        let Some(index) = lane
            .pending_turns
            .iter()
            .position(|pending| *pending == turn_id)
        else {
            return Err(RuntimeLaneError::QueuedTurnNotPending);
        };
        lane.pending_turns.remove(index);
        lane.active_turn_id = Some(turn_id);
        Ok(())
    }

    pub fn discard_queued_turn(
        &mut self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> Result<(), RuntimeLaneError> {
        let lane = self
            .lanes_by_session
            .get_mut(&session_id)
            .ok_or(RuntimeLaneError::LaneNotFound)?;
        lane.pending_turns.retain(|pending| *pending != turn_id);
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
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use golutra_core::ActorKind;

    fn actor() -> Actor {
        Actor {
            kind: ActorKind::Runtime,
            id: "lane-test".to_owned(),
        }
    }

    fn manager_with_queued_turns(
        queued_turns: &[TurnId],
    ) -> (RuntimeLaneManager, SessionId, TurnId) {
        let mut manager = RuntimeLaneManager::new();
        let session_id = SessionId::new();
        let active_turn_id = TurnId::new();
        manager
            .start_task(
                WorkspaceId::new(),
                session_id,
                TaskId::new(),
                active_turn_id,
                actor(),
                1,
            )
            .expect("task starts");
        for (index, turn_id) in queued_turns.iter().copied().enumerate() {
            manager
                .queue_turn(
                    session_id,
                    turn_id,
                    u64::try_from(index).expect("sequence") + 2,
                )
                .expect("turn queues");
        }
        (manager, session_id, active_turn_id)
    }

    #[test]
    fn preparing_queued_turn_start_does_not_mutate_lane() {
        let queued_turn_id = TurnId::new();
        let (manager, session_id, active_turn_id) = manager_with_queued_turns(&[queued_turn_id]);
        let before = manager.lane(session_id).expect("lane").clone();

        let lane_id = manager
            .prepare_queued_turn_start(session_id, queued_turn_id)
            .expect("transition prepares");

        assert_eq!(lane_id, before.lane_id);
        assert_eq!(manager.lane(session_id), Some(&before));
        assert_eq!(before.active_turn_id, Some(active_turn_id));
        assert_eq!(before.pending_turns, vec![queued_turn_id]);
    }

    #[test]
    fn committing_queued_turn_start_preserves_turns_queued_after_prepare() {
        let queued_turn_id = TurnId::new();
        let later_turn_id = TurnId::new();
        let (mut manager, session_id, _) = manager_with_queued_turns(&[queued_turn_id]);
        let lane_id = manager
            .prepare_queued_turn_start(session_id, queued_turn_id)
            .expect("transition prepares");
        manager
            .queue_turn(session_id, later_turn_id, 3)
            .expect("later turn queues");

        manager
            .commit_queued_turn_start(session_id, lane_id, queued_turn_id)
            .expect("transition commits");

        let lane = manager.lane(session_id).expect("lane");
        assert_eq!(lane.active_turn_id, Some(queued_turn_id));
        assert_eq!(lane.pending_turns, vec![later_turn_id]);
    }

    #[test]
    fn prepared_start_cannot_mutate_a_replacement_lane() {
        let queued_turn_id = TurnId::new();
        let (mut manager, session_id, _) = manager_with_queued_turns(&[queued_turn_id]);
        let lane_id = manager
            .prepare_queued_turn_start(session_id, queued_turn_id)
            .expect("transition prepares");
        manager
            .finish_task(session_id, TaskStatus::Completed, 3)
            .expect("task finishes");
        let replacement_turn_id = TurnId::new();
        manager
            .start_task(
                WorkspaceId::new(),
                session_id,
                TaskId::new(),
                replacement_turn_id,
                actor(),
                4,
            )
            .expect("replacement task starts");
        let before = manager.lane(session_id).expect("replacement lane").clone();

        assert_eq!(
            manager.commit_queued_turn_start(session_id, lane_id, queued_turn_id),
            Err(RuntimeLaneError::LaneNotFound)
        );
        assert_eq!(manager.lane(session_id), Some(&before));
    }

    #[test]
    fn queued_turn_start_rejects_an_unknown_turn_without_mutating_the_lane() {
        let queued_turn_id = TurnId::new();
        let unknown_turn_id = TurnId::new();
        let (mut manager, session_id, active_turn_id) =
            manager_with_queued_turns(&[queued_turn_id]);
        let before = manager.lane(session_id).expect("lane").clone();

        assert_eq!(
            manager.prepare_queued_turn_start(session_id, unknown_turn_id),
            Err(RuntimeLaneError::QueuedTurnNotPending)
        );
        assert_eq!(
            manager.start_queued_turn(session_id, unknown_turn_id),
            Err(RuntimeLaneError::QueuedTurnNotPending)
        );
        assert_eq!(manager.lane(session_id), Some(&before));
        assert_eq!(
            manager.commit_queued_turn_start(session_id, before.lane_id, unknown_turn_id),
            Err(RuntimeLaneError::QueuedTurnNotPending)
        );
        let lane = manager.lane(session_id).expect("lane");
        assert_eq!(lane.active_turn_id, Some(active_turn_id));
        assert_eq!(lane.pending_turns, vec![queued_turn_id]);
    }
}
