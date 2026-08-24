//! Coalesced redraw scheduling for the interactive terminal.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use golutra_core::TurnId;
use golutra_protocol::{DriverLatencyMetrics, DriverRenderMetrics};

/// Requests may arrive faster, but the terminal is never asked to render more
/// than 120 frames per second.
pub(crate) const MIN_FRAME_INTERVAL: Duration = Duration::from_nanos(8_333_334);

/// 低基数的交互渲染计数与延迟聚合。
///
/// 不保存文本、任务标识或绝对时间戳；流状态只保留到对应帧绘制完成，
/// 用于计算首个 delta 到首帧、最后一个 delta 到末帧的单调时钟延迟。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RenderMetrics {
    pub(crate) redraw_requests: u64,
    pub(crate) redraws: u64,
    pub(crate) coalesced_redraws: u64,
    pub(crate) pending_redraws: u64,
    pub(crate) delta_events: u64,
    pub(crate) first_deltas: u64,
    pub(crate) last_deltas: u64,
    pub(crate) final_messages: u64,
    pub(crate) stream_gaps: u64,
    pub(crate) duplicate_frames: u64,
    pub(crate) dropped_frames: u64,
    pub(crate) pending_streams: u64,
    pub(crate) first_token_latency: RenderLatencyMetrics,
    pub(crate) final_frame_latency: RenderLatencyMetrics,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RenderLatencyMetrics {
    pub(crate) samples: u64,
    pub(crate) total_ms: u64,
    pub(crate) max_ms: u64,
    pub(crate) last_ms: u64,
}

impl RenderLatencyMetrics {
    fn record(&mut self, elapsed: Duration) {
        let millis = elapsed.as_millis().try_into().unwrap_or(u64::MAX);
        self.samples = self.samples.saturating_add(1);
        self.total_ms = self.total_ms.saturating_add(millis);
        self.max_ms = self.max_ms.max(millis);
        self.last_ms = millis;
    }

    fn driver_snapshot(self) -> DriverLatencyMetrics {
        DriverLatencyMetrics {
            samples: self.samples,
            total_ms: self.total_ms,
            max_ms: self.max_ms,
            last_ms: self.last_ms,
        }
    }
}

impl RenderMetrics {
    pub(crate) fn driver_snapshot(self) -> DriverRenderMetrics {
        DriverRenderMetrics {
            redraw_requests: self.redraw_requests,
            redraws: self.redraws,
            coalesced_redraws: self.coalesced_redraws,
            pending_redraws: self.pending_redraws,
            delta_events: self.delta_events,
            first_deltas: self.first_deltas,
            last_deltas: self.last_deltas,
            final_messages: self.final_messages,
            stream_gaps: self.stream_gaps,
            duplicate_frames: self.duplicate_frames,
            dropped_frames: self.dropped_frames,
            pending_streams: self.pending_streams,
            first_token_latency: self.first_token_latency.driver_snapshot(),
            final_frame_latency: self.final_frame_latency.driver_snapshot(),
        }
    }
}

#[derive(Debug, Default)]
struct StreamState {
    last_sequence: Option<u64>,
    has_delta: bool,
    completed: bool,
    first_delta_at: Option<Instant>,
    last_delta_at: Option<Instant>,
    first_frame_pending: bool,
    final_frame_pending: bool,
}

#[derive(Debug, Default)]
pub(crate) struct FrameScheduler {
    last_drawn_at: Option<Instant>,
    deadline: Option<Instant>,
    metrics: RenderMetrics,
    streams: HashMap<TurnId, StreamState>,
}

impl FrameScheduler {
    pub(crate) fn request_at(&mut self, now: Instant) {
        self.metrics.redraw_requests = self.metrics.redraw_requests.saturating_add(1);
        if self.deadline.is_some() {
            self.metrics.coalesced_redraws = self.metrics.coalesced_redraws.saturating_add(1);
        }
        // 一个 deadline 代表一个待绘制帧；多个生产者在绘制前请求时只保留一个。
        self.metrics.pending_redraws = 1;
        let earliest = self
            .last_drawn_at
            .and_then(|drawn| drawn.checked_add(MIN_FRAME_INTERVAL))
            .map_or(now, |allowed| allowed.max(now));
        self.deadline = Some(
            self.deadline
                .map_or(earliest, |current| current.min(earliest)),
        );
    }

    #[must_use]
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub(crate) fn mark_drawn_at(&mut self, now: Instant) {
        self.last_drawn_at = Some(now);
        self.deadline = None;
        self.metrics.redraws = self.metrics.redraws.saturating_add(1);
        self.metrics.pending_redraws = 0;

        let mut completed_turns = Vec::new();
        for (turn_id, state) in &mut self.streams {
            if state.first_frame_pending {
                if let Some(started_at) = state.first_delta_at {
                    self.metrics
                        .first_token_latency
                        .record(now.saturating_duration_since(started_at));
                }
                state.first_frame_pending = false;
            }
            if state.final_frame_pending {
                if let Some(started_at) = state.last_delta_at {
                    self.metrics
                        .final_frame_latency
                        .record(now.saturating_duration_since(started_at));
                }
                state.final_frame_pending = false;
                completed_turns.push(*turn_id);
            }
        }
        for turn_id in completed_turns {
            if self
                .streams
                .remove(&turn_id)
                .is_some_and(|state| state.has_delta)
            {
                self.metrics.pending_streams = self.metrics.pending_streams.saturating_sub(1);
            }
        }
    }

    pub(crate) fn record_delta_at(
        &mut self,
        turn_id: TurnId,
        stream_sequence_no: Option<u64>,
        now: Instant,
    ) {
        let state = self.streams.entry(turn_id).or_default();
        let first = !state.has_delta;
        // RuntimeEvent.sequence_no 是会话级序号；只有 provider 明确提供
        // stream-local 序号时，gap/duplicate 统计才有意义。
        let (gap, duplicate) = match (state.last_sequence, stream_sequence_no) {
            (Some(last), Some(sequence_no)) => {
                (sequence_no > last.saturating_add(1), sequence_no <= last)
            }
            _ => (false, false),
        };
        if first {
            state.has_delta = true;
            state.first_delta_at = Some(now);
            state.first_frame_pending = true;
        }
        if let Some(sequence_no) = stream_sequence_no {
            state.last_sequence = Some(sequence_no);
        }
        state.last_delta_at = Some(now);
        self.metrics.delta_events = self.metrics.delta_events.saturating_add(1);
        if first {
            self.metrics.first_deltas = self.metrics.first_deltas.saturating_add(1);
            self.metrics.pending_streams = self.metrics.pending_streams.saturating_add(1);
        }
        if gap {
            self.metrics.stream_gaps = self.metrics.stream_gaps.saturating_add(1);
            self.metrics.dropped_frames = self.metrics.dropped_frames.saturating_add(1);
        }
        if duplicate {
            self.metrics.duplicate_frames = self.metrics.duplicate_frames.saturating_add(1);
        }
    }

    pub(crate) fn record_stream_completed(&mut self, turn_id: TurnId) {
        let should_record = self.streams.get_mut(&turn_id).is_some_and(|state| {
            if state.has_delta && !state.completed {
                state.completed = true;
                true
            } else {
                false
            }
        });
        if should_record {
            self.metrics.last_deltas = self.metrics.last_deltas.saturating_add(1);
        }
    }

    pub(crate) fn record_stream_ended(&mut self, turn_id: TurnId) {
        if let Some(state) = self.streams.get_mut(&turn_id) {
            if state.has_delta {
                state.final_frame_pending = true;
            } else {
                self.streams.remove(&turn_id);
            }
        }
    }

    pub(crate) fn record_final_message(&mut self, turn_id: TurnId) {
        if let Some(state) = self.streams.get_mut(&turn_id)
            && state.has_delta
            && !state.final_frame_pending
        {
            if !state.completed {
                self.metrics.last_deltas = self.metrics.last_deltas.saturating_add(1);
            }
            self.metrics.final_messages = self.metrics.final_messages.saturating_add(1);
            state.final_frame_pending = true;
        }
    }

    #[must_use]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const fn metrics(&self) -> RenderMetrics {
        self.metrics
    }

    #[cfg(test)]
    pub(crate) fn stream_state_count(&self) -> usize {
        self.streams.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_request_is_immediate() {
        let now = Instant::now();
        let mut scheduler = FrameScheduler::default();

        scheduler.request_at(now);

        assert_eq!(scheduler.deadline(), Some(now));
    }

    #[test]
    fn burst_requests_coalesce_at_the_frame_limit() {
        let first = Instant::now();
        let mut scheduler = FrameScheduler::default();
        scheduler.request_at(first);
        scheduler.mark_drawn_at(first);

        scheduler.request_at(first + Duration::from_millis(1));
        scheduler.request_at(first + Duration::from_millis(2));

        assert_eq!(scheduler.deadline(), Some(first + MIN_FRAME_INTERVAL));
    }

    #[test]
    fn request_after_frame_budget_is_not_delayed() {
        let first = Instant::now();
        let mut scheduler = FrameScheduler::default();
        scheduler.request_at(first);
        scheduler.mark_drawn_at(first);
        let later = first + Duration::from_millis(20);

        scheduler.request_at(later);

        assert_eq!(scheduler.deadline(), Some(later));
    }

    #[test]
    fn requests_are_counted_and_coalesced_deterministically() {
        let first = Instant::now();
        let mut scheduler = FrameScheduler::default();
        scheduler.request_at(first);
        scheduler.request_at(first + Duration::from_millis(1));
        assert_eq!(scheduler.metrics().redraw_requests, 2);
        assert_eq!(scheduler.metrics().coalesced_redraws, 1);
        assert_eq!(scheduler.metrics().pending_redraws, 1);
        scheduler.mark_drawn_at(first);
        assert_eq!(scheduler.metrics().redraws, 1);
        assert_eq!(scheduler.metrics().pending_redraws, 0);
    }

    #[test]
    fn stream_lifecycle_counts_first_last_final_and_gaps() {
        let turn = TurnId::new();
        let mut scheduler = FrameScheduler::default();
        let base = Instant::now();
        scheduler.record_delta_at(turn, Some(10), base);
        scheduler.record_delta_at(turn, Some(12), base + Duration::from_millis(1));
        scheduler.record_stream_completed(turn);
        scheduler.record_final_message(turn);
        scheduler.mark_drawn_at(Instant::now());
        let metrics = scheduler.metrics();
        assert_eq!(metrics.delta_events, 2);
        assert_eq!(metrics.first_deltas, 1);
        assert_eq!(metrics.last_deltas, 1);
        assert_eq!(metrics.final_messages, 1);
        assert_eq!(metrics.stream_gaps, 1);
        assert_eq!(metrics.dropped_frames, 1);
        assert_eq!(metrics.pending_streams, 0);
    }

    #[test]
    fn latency_is_measured_from_delta_to_the_next_real_frame() {
        let base = Instant::now();
        let turn = TurnId::new();
        let mut scheduler = FrameScheduler::default();

        scheduler.record_delta_at(turn, Some(1), base);
        scheduler.mark_drawn_at(base + Duration::from_millis(12));
        scheduler.record_delta_at(turn, Some(2), base + Duration::from_millis(20));
        scheduler.record_stream_completed(turn);
        scheduler.record_final_message(turn);
        scheduler.mark_drawn_at(base + Duration::from_millis(35));

        assert_eq!(scheduler.metrics().first_token_latency.samples, 1);
        assert_eq!(scheduler.metrics().first_token_latency.last_ms, 12);
        assert_eq!(scheduler.metrics().final_frame_latency.samples, 1);
        assert_eq!(scheduler.metrics().final_frame_latency.last_ms, 15);
        assert_eq!(scheduler.metrics().pending_streams, 0);
        assert_eq!(scheduler.stream_state_count(), 0);
    }

    #[test]
    fn duplicate_and_skipped_sequences_are_counted_separately() {
        let base = Instant::now();
        let turn = TurnId::new();
        let mut scheduler = FrameScheduler::default();
        scheduler.record_delta_at(turn, Some(4), base);
        scheduler.record_delta_at(turn, Some(4), base + Duration::from_millis(1));
        scheduler.record_delta_at(turn, Some(7), base + Duration::from_millis(2));

        assert_eq!(scheduler.metrics().duplicate_frames, 1);
        assert_eq!(scheduler.metrics().dropped_frames, 1);
        assert_eq!(scheduler.metrics().stream_gaps, 1);
    }

    #[test]
    fn global_event_order_does_not_create_false_stream_gaps() {
        let base = Instant::now();
        let turn = TurnId::new();
        let mut scheduler = FrameScheduler::default();

        scheduler.record_delta_at(turn, None, base);
        scheduler.record_delta_at(turn, None, base + Duration::from_millis(1));

        assert_eq!(scheduler.metrics().stream_gaps, 0);
        assert_eq!(scheduler.metrics().duplicate_frames, 0);
        assert_eq!(scheduler.metrics().dropped_frames, 0);
    }
}
