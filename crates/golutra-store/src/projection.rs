//! RuntimeEvent 到 StateProjection 的纯归约逻辑。

use golutra_core::{LoopDecision, SessionId, TaskStatus, VerificationRecord};
use golutra_protocol::{RuntimeEvent, RuntimeEventType, StateProjection, VisibleStep};

const MAX_PROJECTION_VISIBLE_STEPS: usize = 512;

pub(crate) fn initial_projection(session_id: SessionId) -> StateProjection {
    StateProjection {
        session_id,
        active_task_id: None,
        task_status: TaskStatus::Idle,
        runtime_lane: None,
        last_sequence_no: 0,
        visible_steps: Vec::new(),
        pending_approval: None,
        final_message: None,
        last_loop_decision: None,
        last_verification: None,
    }
}

pub(crate) fn apply_event_to_state(projection: &mut StateProjection, event: &RuntimeEvent) {
    projection.last_sequence_no = projection.last_sequence_no.max(event.sequence_no);
    if let Some(task_id) = event.task_id {
        projection.active_task_id = Some(task_id);
    }
    apply_event_to_projection(projection, event);
}

pub(crate) fn apply_event_to_projection(projection: &mut StateProjection, event: &RuntimeEvent) {
    if let Some(runtime_lane) = runtime_lane_from_event(event) {
        projection.runtime_lane = Some(runtime_lane);
    }
    match event.event_type {
        RuntimeEventType::TaskCreated => {
            projection.task_status = TaskStatus::Running;
            projection.pending_approval = None;
            projection.final_message = None;
            projection.last_loop_decision = None;
            projection.last_verification = None;
        }
        RuntimeEventType::TurnStarted => {
            projection.task_status = TaskStatus::Running;
            projection.pending_approval = None;
            projection.final_message = None;
        }
        RuntimeEventType::TaskResumed => {
            projection.task_status = TaskStatus::Running;
        }
        RuntimeEventType::TaskCompleted => {
            projection.task_status = event
                .payload
                .get("status")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or(TaskStatus::Completed);
            projection.pending_approval = None;
        }
        RuntimeEventType::TaskAbortRequested => {
            projection.task_status = TaskStatus::Aborting;
        }
        RuntimeEventType::TaskAborted => {
            projection.task_status = event
                .payload
                .get("status")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or(TaskStatus::Cancelled);
            projection.pending_approval = None;
        }
        RuntimeEventType::TaskInterrupted => {
            projection.task_status = TaskStatus::Interrupted;
            projection.pending_approval = None;
        }
        RuntimeEventType::TaskUncertain => {
            projection.task_status = TaskStatus::Uncertain;
            projection.pending_approval = None;
        }
        RuntimeEventType::TaskReconciled => {
            projection.task_status = event
                .payload
                .get("status")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or(TaskStatus::Interrupted);
            projection.pending_approval = None;
        }
        RuntimeEventType::TaskPaused => {
            projection.task_status = TaskStatus::Paused;
        }
        RuntimeEventType::ApprovalRequested => {
            projection.task_status = TaskStatus::WaitingApproval;
            projection.pending_approval = event
                .payload
                .get("approval_id")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned);
        }
        RuntimeEventType::ApprovalResolved => {
            projection.pending_approval = None;
            if projection.task_status == TaskStatus::WaitingApproval {
                projection.task_status = TaskStatus::Running;
            }
        }
        RuntimeEventType::ProviderAuthRequired => {
            projection.task_status = TaskStatus::WaitingAuthentication;
        }
        RuntimeEventType::ProviderAuthSubmitted => {
            if projection.task_status == TaskStatus::WaitingAuthentication {
                projection.task_status = TaskStatus::Running;
            }
        }
        RuntimeEventType::ProviderAuthCancelled => {
            projection.task_status = TaskStatus::Blocked;
        }
        RuntimeEventType::VerificationCompleted => {
            projection.last_verification = verification_from_event(event);
        }
        RuntimeEventType::LoopDecided => {
            projection.last_loop_decision = loop_decision_from_event(event);
        }
        RuntimeEventType::AssistantMessage => {
            projection.final_message = event
                .payload
                .get("content")
                .and_then(|content| content.as_str())
                .map(ToOwned::to_owned);
        }
        _ => {}
    }

    if let Some(runtime_lane) = projection.runtime_lane.as_mut() {
        runtime_lane.status = projection.task_status;
        if event.event_type == RuntimeEventType::TurnStarted {
            runtime_lane.active_turn_id = event.turn_id;
        }
    }

    projection.visible_steps.push(VisibleStep {
        label: format!("{:?}", event.event_type),
        status: format!("{:?}", projection.task_status),
        summary: event
            .payload
            .get("summary")
            .and_then(|summary| summary.as_str())
            .unwrap_or("runtime event recorded")
            .to_owned(),
    });
    let overflow = projection
        .visible_steps
        .len()
        .saturating_sub(MAX_PROJECTION_VISIBLE_STEPS);
    if overflow > 0 {
        projection.visible_steps.drain(..overflow);
    }
}

pub(crate) fn runtime_lane_from_event(event: &RuntimeEvent) -> Option<golutra_core::RuntimeLane> {
    event
        .payload
        .get("runtime_lane")
        .or_else(|| event.payload.pointer("/runtime/runtime_lane"))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

pub(crate) fn verification_from_event(event: &RuntimeEvent) -> Option<VerificationRecord> {
    event
        .payload
        .get("record")
        .cloned()
        .or_else(|| Some(event.payload.clone()))
        .and_then(|value| serde_json::from_value(value).ok())
}

pub(crate) fn loop_decision_from_event(event: &RuntimeEvent) -> Option<LoopDecision> {
    event
        .payload
        .get("record")
        .cloned()
        .or_else(|| Some(event.payload.clone()))
        .and_then(|value| serde_json::from_value(value).ok())
}
