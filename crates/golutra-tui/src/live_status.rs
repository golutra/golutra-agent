//! Ephemeral activity status for the interactive TUI.
//!
//! Runtime events remain the source of truth for durable history. This module
//! projects those events into the short-lived status row above the composer.
//! The projection can be rebuilt after paging/resume and updated incrementally
//! as new events arrive.

use std::time::Duration;

use chrono::{DateTime, Utc};
use golutra_core::{TaskId, TaskStatus, TurnId};
use golutra_protocol::{RuntimeEvent, RuntimeEventType};
const ESTIMATED_CHARS_PER_TOKEN: u64 = 4;
const MIN_RATE_SAMPLE_MILLIS: i64 = 250;

#[derive(Debug, Clone)]
struct ProviderActivity {
    first_output_at: Option<DateTime<Utc>>,
    last_output_at: Option<DateTime<Utc>>,
    output_chars: u64,
    exact_output_tokens: Option<u64>,
    completed: bool,
}

impl ProviderActivity {
    fn new() -> Self {
        Self {
            first_output_at: None,
            last_output_at: None,
            output_chars: 0,
            exact_output_tokens: None,
            completed: false,
        }
    }

    fn record_delta(&mut self, at: DateTime<Utc>, chars: u64) {
        if chars == 0 {
            return;
        }
        self.first_output_at.get_or_insert(at);
        self.last_output_at = Some(at);
        self.output_chars = self.output_chars.saturating_add(chars);
    }

    fn record_completion(&mut self, output_tokens: Option<u64>) {
        self.exact_output_tokens = output_tokens;
        self.completed = true;
    }

    fn output_tokens(&self) -> u64 {
        self.exact_output_tokens.unwrap_or_else(|| {
            self.output_chars
                .saturating_add(ESTIMATED_CHARS_PER_TOKEN - 1)
                / ESTIMATED_CHARS_PER_TOKEN
        })
    }

    fn rate(&self, now: DateTime<Utc>) -> Option<OutputRate> {
        let first = self.first_output_at?;
        let last = self.last_output_at?;
        let end = if self.completed { last } else { now };
        let elapsed_ms = (end - first).num_milliseconds();
        if elapsed_ms < MIN_RATE_SAMPLE_MILLIS || self.output_tokens() == 0 {
            return None;
        }
        Some(OutputRate {
            tokens_per_second: self.output_tokens() as f64 / (elapsed_ms as f64 / 1_000.0),
            estimated: self.exact_output_tokens.is_none(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct OutputRate {
    pub(crate) tokens_per_second: f64,
    pub(crate) estimated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ActivitySnapshot {
    pub(crate) elapsed: Duration,
    pub(crate) output_rate: Option<OutputRate>,
    pub(crate) can_interrupt: bool,
}

/// A replayable reducer for the current turn's ephemeral activity.
#[derive(Debug, Clone, Default)]
pub(crate) struct ActivityProjection {
    task_id: Option<TaskId>,
    turn_id: Option<TurnId>,
    active_elapsed: Duration,
    running_since: Option<DateTime<Utc>>,
    paused: bool,
    provider_calls: Vec<ProviderActivity>,
}

impl ActivityProjection {
    pub(crate) fn rebuild(&mut self, events: &[RuntimeEvent]) {
        *self = Self::default();
        let mut sorted = events.to_vec();
        sorted.sort_by_key(|event| event.sequence_no);
        for event in sorted {
            self.apply(&event);
        }
    }

    pub(crate) fn apply(&mut self, event: &RuntimeEvent) {
        let Some(event_task_id) = event.task_id else {
            return;
        };

        if self.task_id.is_none() {
            self.reset_task(event_task_id, event.turn_id, event.timestamp);
        } else if self.task_id != Some(event_task_id) {
            if !matches!(
                event.event_type,
                RuntimeEventType::TaskCreated | RuntimeEventType::TurnStarted
            ) {
                // Post-task governance is asynchronous and may append events
                // for an older task after the next task has already started.
                return;
            }
            self.reset_task(event_task_id, event.turn_id, event.timestamp);
        }

        if event.event_type == RuntimeEventType::TurnStarted && self.turn_id != event.turn_id {
            self.reset_turn(event.turn_id, event.timestamp);
        }

        match event.event_type {
            RuntimeEventType::TaskCreated | RuntimeEventType::TurnStarted => {
                self.resume_at(event.timestamp);
            }
            RuntimeEventType::ProviderStarted => {
                self.provider_calls.push(ProviderActivity::new());
            }
            RuntimeEventType::ProviderStreamed => {
                if self.provider_calls.is_empty() {
                    self.provider_calls.push(ProviderActivity::new());
                }
                let chars = provider_delta_chars(event);
                if let Some(provider) = self.provider_calls.last_mut() {
                    provider.record_delta(event.timestamp, chars);
                }
            }
            RuntimeEventType::ProviderCompleted => {
                let output_tokens = event
                    .payload
                    .get("usage")
                    .and_then(|usage| usage.get("output_tokens"))
                    .and_then(|tokens| tokens.as_u64());
                if let Some(provider) = self.provider_calls.last_mut() {
                    provider.record_completion(output_tokens);
                }
            }
            RuntimeEventType::TokenUsageRecorded => {
                let output_tokens = event
                    .payload
                    .get("record")
                    .and_then(|record| record.get("output_tokens"))
                    .and_then(|tokens| tokens.as_u64());
                if let Some(output_tokens) = output_tokens
                    && let Some(provider) = self.provider_calls.last_mut()
                {
                    // The usage record is emitted after ProviderCompleted and
                    // is the fallback source when a provider did not return
                    // usage in its completion payload.
                    provider.exact_output_tokens = Some(output_tokens);
                }
            }
            RuntimeEventType::ApprovalRequested | RuntimeEventType::UserQuestionRequested => {
                self.pause_at(event.timestamp);
            }
            RuntimeEventType::ApprovalResolved | RuntimeEventType::UserQuestionResolved => {
                self.resume_at(event.timestamp);
            }
            RuntimeEventType::ProviderAuthRequired => {
                self.pause_at(event.timestamp);
            }
            RuntimeEventType::ProviderAuthSubmitted
            | RuntimeEventType::ProviderConfigured
            | RuntimeEventType::ProviderCredentialRefreshed => {
                self.resume_at(event.timestamp);
            }
            RuntimeEventType::TaskPaused => {
                self.pause_at(event.timestamp);
            }
            RuntimeEventType::TaskResumed => {
                self.resume_at(event.timestamp);
            }
            event_type if event_type.is_task_terminal() => {
                self.pause_at(event.timestamp);
            }
            _ => {}
        }
    }

    fn reset_task(&mut self, task_id: TaskId, turn_id: Option<TurnId>, at: DateTime<Utc>) {
        self.task_id = Some(task_id);
        self.turn_id = turn_id;
        self.active_elapsed = Duration::ZERO;
        self.running_since = Some(at);
        self.paused = false;
        self.provider_calls.clear();
    }

    fn reset_turn(&mut self, turn_id: Option<TurnId>, at: DateTime<Utc>) {
        self.turn_id = turn_id;
        self.active_elapsed = Duration::ZERO;
        self.running_since = Some(at);
        self.paused = false;
        self.provider_calls.clear();
    }

    fn pause_at(&mut self, at: DateTime<Utc>) {
        if self.paused {
            return;
        }
        self.add_running_duration(at);
        self.paused = true;
        self.running_since = None;
    }

    fn resume_at(&mut self, at: DateTime<Utc>) {
        if !self.paused {
            if self.running_since.is_none() {
                self.running_since = Some(at);
            }
            return;
        }
        self.paused = false;
        self.running_since = Some(at);
    }

    fn add_running_duration(&mut self, end: DateTime<Utc>) {
        let Some(start) = self.running_since else {
            return;
        };
        let millis = (end - start).num_milliseconds().max(0) as u64;
        self.active_elapsed = self
            .active_elapsed
            .saturating_add(Duration::from_millis(millis));
    }

    fn elapsed_at(&self, now: DateTime<Utc>) -> Duration {
        if self.paused {
            return self.active_elapsed;
        }
        let mut elapsed = self.active_elapsed;
        if let Some(start) = self.running_since {
            let millis = (now - start).num_milliseconds().max(0) as u64;
            elapsed = elapsed.saturating_add(Duration::from_millis(millis));
        }
        elapsed
    }

    // A turn may contain several provider requests because of tool loops,
    // retries, or fallback. Use the median request rate so one slow/fast
    // request does not make the live indicator misleading.
    fn output_rate(&self, now: DateTime<Utc>) -> Option<OutputRate> {
        let mut rates = self
            .provider_calls
            .iter()
            .filter_map(|provider| provider.rate(now))
            .collect::<Vec<_>>();
        if rates.is_empty() {
            return None;
        }
        rates.sort_by(|left, right| left.tokens_per_second.total_cmp(&right.tokens_per_second));
        let middle = rates.len() / 2;
        Some(if rates.len() % 2 == 0 {
            OutputRate {
                tokens_per_second: (rates[middle - 1].tokens_per_second
                    + rates[middle].tokens_per_second)
                    / 2.0,
                estimated: rates[middle - 1].estimated || rates[middle].estimated,
            }
        } else {
            rates[middle]
        })
    }

    pub(crate) fn snapshot(
        &self,
        status: Option<TaskStatus>,
        now: DateTime<Utc>,
    ) -> Option<ActivitySnapshot> {
        let status = status?;
        if !matches!(
            status,
            TaskStatus::Running
                | TaskStatus::WaitingApproval
                | TaskStatus::WaitingAuthentication
                | TaskStatus::Pausing
                | TaskStatus::Paused
                | TaskStatus::Aborting
        ) {
            return None;
        }
        Some(ActivitySnapshot {
            elapsed: self.elapsed_at(now),
            output_rate: self.output_rate(now),
            can_interrupt: !matches!(status, TaskStatus::Aborting),
        })
    }

    #[cfg(test)]
    fn provider_output_tokens(&self) -> Option<u64> {
        self.provider_calls
            .last()
            .map(ProviderActivity::output_tokens)
    }
}

fn provider_delta_chars(event: &RuntimeEvent) -> u64 {
    let Some(delta) = event.payload.get("delta") else {
        return 0;
    };
    match delta.get("kind").and_then(|kind| kind.as_str()) {
        Some("text_delta") => delta
            .get("text")
            .and_then(|text| text.as_str())
            .map(|text| text.chars().count() as u64)
            .unwrap_or(0),
        // Reasoning text is intentionally redacted in RuntimeEvent. The
        // byte count is the only available bounded signal for live estimation.
        Some("reasoning_delta") => delta
            .get("byte_count")
            .and_then(|count| count.as_u64())
            .unwrap_or(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use golutra_core::{EventId, SessionId, TurnId};
    use golutra_protocol::{RuntimeEventSource, RuntimeEventType};
    use serde_json::json;

    fn event(
        sequence_no: u64,
        task_id: TaskId,
        turn_id: Option<TurnId>,
        event_type: RuntimeEventType,
        timestamp: DateTime<Utc>,
        payload: serde_json::Value,
    ) -> RuntimeEvent {
        RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: EventId::new(),
            sequence_no,
            session_id: SessionId::new(),
            turn_id,
            task_id: Some(task_id),
            parent_event_id: None,
            event_type,
            timestamp,
            source: RuntimeEventSource::Provider,
            payload,
            payload_ref: None,
            durable: true,
        }
    }

    #[test]
    fn rate_uses_active_sampling_window_and_provider_usage_when_available() {
        let base = Utc::now();
        let task = TaskId::new();
        let turn = TurnId::new();
        let mut status = ActivityProjection::default();
        status.apply(&event(
            1,
            task,
            Some(turn),
            RuntimeEventType::TaskCreated,
            base,
            json!({}),
        ));
        let pre_sample = status
            .snapshot(
                Some(TaskStatus::Running),
                base + chrono::Duration::seconds(1),
            )
            .expect("pre-sample status");
        assert_eq!(pre_sample.elapsed, Duration::from_secs(1));
        assert_eq!(pre_sample.output_rate, None);
        status.apply(&event(
            2,
            task,
            Some(turn),
            RuntimeEventType::ProviderStarted,
            base + chrono::Duration::seconds(1),
            json!({}),
        ));
        status.apply(&event(
            3,
            task,
            Some(turn),
            RuntimeEventType::ProviderStreamed,
            base + chrono::Duration::seconds(6),
            json!({"delta": {"kind": "text_delta", "text": "a".repeat(400)}}),
        ));
        status.apply(&event(
            4,
            task,
            Some(turn),
            RuntimeEventType::ProviderStreamed,
            base + chrono::Duration::seconds(7),
            json!({"delta": {"kind": "text_delta", "text": "b"}}),
        ));
        let live = status
            .output_rate(base + chrono::Duration::seconds(8))
            .expect("live rate");
        assert!((live.tokens_per_second - 50.5).abs() < 0.01);
        assert!(live.estimated);

        status.apply(&event(
            5,
            task,
            Some(turn),
            RuntimeEventType::ProviderCompleted,
            base + chrono::Duration::seconds(8),
            json!({"usage": {"output_tokens": 120}}),
        ));
        assert_eq!(status.provider_output_tokens(), Some(120));
        let completed = status
            .output_rate(base + chrono::Duration::seconds(20))
            .expect("completed rate");
        assert!((completed.tokens_per_second - 120.0).abs() < 0.01);
        assert!(!completed.estimated);
    }

    #[test]
    fn multi_request_turn_uses_the_median_output_rate() {
        let base = Utc::now();
        let task = TaskId::new();
        let turn = TurnId::new();
        let mut status = ActivityProjection::default();
        status.apply(&event(
            1,
            task,
            Some(turn),
            RuntimeEventType::TaskCreated,
            base,
            json!({}),
        ));
        status.apply(&event(
            2,
            task,
            Some(turn),
            RuntimeEventType::ProviderStarted,
            base,
            json!({}),
        ));
        status.apply(&event(
            3,
            task,
            Some(turn),
            RuntimeEventType::ProviderStreamed,
            base + chrono::Duration::seconds(1),
            json!({"delta": {"kind": "text_delta", "text": "a".repeat(200)}}),
        ));
        status.apply(&event(
            4,
            task,
            Some(turn),
            RuntimeEventType::ProviderStreamed,
            base + chrono::Duration::seconds(2),
            json!({"delta": {"kind": "text_delta", "text": "a".repeat(200)}}),
        ));
        status.apply(&event(
            5,
            task,
            Some(turn),
            RuntimeEventType::ProviderCompleted,
            base + chrono::Duration::seconds(3),
            json!({"usage": {"output_tokens": 100}}),
        ));
        status.apply(&event(
            6,
            task,
            Some(turn),
            RuntimeEventType::ProviderStarted,
            base + chrono::Duration::seconds(4),
            json!({}),
        ));
        status.apply(&event(
            7,
            task,
            Some(turn),
            RuntimeEventType::ProviderStreamed,
            base + chrono::Duration::seconds(5),
            json!({"delta": {"kind": "text_delta", "text": "b".repeat(400)}}),
        ));
        status.apply(&event(
            8,
            task,
            Some(turn),
            RuntimeEventType::ProviderStreamed,
            base + chrono::Duration::seconds(9),
            json!({"delta": {"kind": "text_delta", "text": "b".repeat(400)}}),
        ));
        status.apply(&event(
            9,
            task,
            Some(turn),
            RuntimeEventType::ProviderCompleted,
            base + chrono::Duration::seconds(10),
            json!({"usage": {"output_tokens": 200}}),
        ));

        let rate = status
            .output_rate(base + chrono::Duration::seconds(20))
            .expect("multi-request rate");
        // First request: 100 / 1s = 100; second: 200 / 4s = 50.
        assert!((rate.tokens_per_second - 75.0).abs() < 0.01);
    }

    #[test]
    fn late_governance_events_do_not_replace_the_active_task_or_freeze_it() {
        let base = Utc::now();
        let first_task = TaskId::new();
        let active_task = TaskId::new();
        let turn = TurnId::new();
        let mut status = ActivityProjection::default();
        status.apply(&event(
            1,
            first_task,
            Some(turn),
            RuntimeEventType::TaskCreated,
            base,
            json!({}),
        ));
        status.apply(&event(
            2,
            first_task,
            Some(turn),
            RuntimeEventType::TaskCompleted,
            base + chrono::Duration::seconds(1),
            json!({}),
        ));
        status.apply(&event(
            3,
            active_task,
            Some(turn),
            RuntimeEventType::TaskCreated,
            base + chrono::Duration::seconds(2),
            json!({}),
        ));
        status.apply(&event(
            4,
            active_task,
            Some(turn),
            RuntimeEventType::CommandRejected,
            base + chrono::Duration::seconds(3),
            json!({"summary": "busy policy rejected another prompt"}),
        ));
        status.apply(&event(
            5,
            first_task,
            Some(turn),
            RuntimeEventType::PostTaskReviewed,
            base + chrono::Duration::seconds(4),
            json!({"summary": "late post-task review"}),
        ));

        assert_eq!(status.task_id, Some(active_task));
        assert_eq!(
            status.elapsed_at(base + chrono::Duration::seconds(7)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn uncertain_recovery_stops_the_live_activity_timer() {
        let base = Utc::now();
        let task = TaskId::new();
        let turn = TurnId::new();
        let mut status = ActivityProjection::default();
        status.apply(&event(
            1,
            task,
            Some(turn),
            RuntimeEventType::TaskCreated,
            base,
            json!({}),
        ));
        status.apply(&event(
            2,
            task,
            Some(turn),
            RuntimeEventType::TaskUncertain,
            base + chrono::Duration::seconds(3),
            json!({"status": "uncertain"}),
        ));

        assert_eq!(
            status.elapsed_at(base + chrono::Duration::seconds(30)),
            Duration::from_secs(3)
        );
        assert!(status.paused);
    }

    #[test]
    fn turn_and_pause_boundaries_reset_and_freeze_the_timer() {
        let base = Utc::now();
        let task = TaskId::new();
        let turn = TurnId::new();
        let mut status = ActivityProjection::default();
        status.apply(&event(
            1,
            task,
            Some(turn),
            RuntimeEventType::TaskCreated,
            base,
            json!({}),
        ));
        status.apply(&event(
            2,
            task,
            Some(turn),
            RuntimeEventType::TaskPaused,
            base + chrono::Duration::seconds(5),
            json!({}),
        ));
        assert_eq!(
            status.elapsed_at(base + chrono::Duration::seconds(30)),
            Duration::from_secs(5)
        );
        status.apply(&event(
            3,
            task,
            Some(turn),
            RuntimeEventType::TaskResumed,
            base + chrono::Duration::seconds(30),
            json!({}),
        ));
        assert_eq!(
            status.elapsed_at(base + chrono::Duration::seconds(35)),
            Duration::from_secs(10)
        );

        let next_turn = TurnId::new();
        status.apply(&event(
            4,
            task,
            Some(next_turn),
            RuntimeEventType::TurnStarted,
            base + chrono::Duration::seconds(40),
            json!({}),
        ));
        assert_eq!(status.turn_id, Some(next_turn));
        assert_eq!(
            status.elapsed_at(base + chrono::Duration::seconds(41)),
            Duration::from_secs(1)
        );
    }
}
