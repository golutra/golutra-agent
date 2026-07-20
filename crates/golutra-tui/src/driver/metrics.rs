use std::time::{Duration, Instant};

use golutra_protocol::{DriverLatencyMetrics, DriverMetrics};

#[derive(Debug, Default)]
pub(crate) struct DriverMetricsAccumulator {
    connections: u64,
    reconnects: u64,
    rejected_connections: u64,
    requests: u64,
    request_errors: u64,
    snapshot_requests: u64,
    snapshot_renders: u64,
    frozen_frame_hits: u64,
    frozen_frame_misses: u64,
    snapshot_latency: LatencyAccumulator,
    wait_requests: u64,
    wait_results: u64,
    wait_timeouts: u64,
    wait_cancelled: u64,
    wait_latency: LatencyAccumulator,
    sync_attempts: u64,
    sync_errors: u64,
    sync_latency: LatencyAccumulator,
}

#[derive(Debug, Default)]
struct LatencyAccumulator {
    samples: u64,
    total_ms: u64,
    max_ms: u64,
    last_ms: u64,
}

impl LatencyAccumulator {
    fn record(&mut self, elapsed: Duration) {
        let millis = elapsed.as_millis().try_into().unwrap_or(u64::MAX);
        self.samples = self.samples.saturating_add(1);
        self.total_ms = self.total_ms.saturating_add(millis);
        self.max_ms = self.max_ms.max(millis);
        self.last_ms = millis;
    }

    fn snapshot(&self) -> DriverLatencyMetrics {
        DriverLatencyMetrics {
            samples: self.samples,
            total_ms: self.total_ms,
            max_ms: self.max_ms,
            last_ms: self.last_ms,
        }
    }
}

impl DriverMetricsAccumulator {
    pub(crate) fn record_connection(&mut self) {
        if self.connections > 0 {
            self.reconnects = self.reconnects.saturating_add(1);
        }
        self.connections = self.connections.saturating_add(1);
    }

    pub(crate) fn record_rejected_connection(&mut self) {
        self.rejected_connections = self.rejected_connections.saturating_add(1);
    }

    pub(crate) fn record_request(&mut self) {
        self.requests = self.requests.saturating_add(1);
    }

    pub(crate) fn record_request_error(&mut self) {
        self.request_errors = self.request_errors.saturating_add(1);
    }

    pub(crate) fn record_snapshot_request(&mut self) -> Instant {
        self.snapshot_requests = self.snapshot_requests.saturating_add(1);
        Instant::now()
    }

    pub(crate) fn record_snapshot_render(&mut self) {
        self.snapshot_renders = self.snapshot_renders.saturating_add(1);
    }

    pub(crate) fn record_frozen_frame_lookup(&mut self, hit: bool) {
        if hit {
            self.frozen_frame_hits = self.frozen_frame_hits.saturating_add(1);
        } else {
            self.frozen_frame_misses = self.frozen_frame_misses.saturating_add(1);
        }
    }

    pub(crate) fn finish_snapshot(&mut self, started_at: Instant) {
        self.snapshot_latency.record(started_at.elapsed());
    }

    pub(crate) fn start_wait(&mut self) -> Instant {
        self.wait_requests = self.wait_requests.saturating_add(1);
        Instant::now()
    }

    pub(crate) fn finish_wait_result(&mut self, started_at: Instant) {
        self.wait_results = self.wait_results.saturating_add(1);
        self.wait_latency.record(started_at.elapsed());
    }

    pub(crate) fn finish_wait_timeout(&mut self, started_at: Instant) {
        self.wait_timeouts = self.wait_timeouts.saturating_add(1);
        self.wait_latency.record(started_at.elapsed());
    }

    pub(crate) fn cancel_wait(&mut self, started_at: Instant) {
        self.wait_cancelled = self.wait_cancelled.saturating_add(1);
        self.wait_latency.record(started_at.elapsed());
    }

    pub(crate) fn start_sync(&mut self) -> Instant {
        self.sync_attempts = self.sync_attempts.saturating_add(1);
        Instant::now()
    }

    pub(crate) fn finish_sync(&mut self, started_at: Instant, success: bool) {
        if !success {
            self.sync_errors = self.sync_errors.saturating_add(1);
        }
        self.sync_latency.record(started_at.elapsed());
    }

    pub(crate) fn snapshot(
        &self,
        instance_id: &str,
        pending_waits: usize,
        frame_cache_entries: usize,
    ) -> DriverMetrics {
        DriverMetrics {
            instance_id: instance_id.to_owned(),
            connections: self.connections,
            reconnects: self.reconnects,
            rejected_connections: self.rejected_connections,
            requests: self.requests,
            request_errors: self.request_errors,
            snapshot_requests: self.snapshot_requests,
            snapshot_renders: self.snapshot_renders,
            frozen_frame_hits: self.frozen_frame_hits,
            frozen_frame_misses: self.frozen_frame_misses,
            snapshot_latency: self.snapshot_latency.snapshot(),
            wait_requests: self.wait_requests,
            wait_results: self.wait_results,
            wait_timeouts: self.wait_timeouts,
            wait_cancelled: self.wait_cancelled,
            pending_waits: pending_waits.try_into().unwrap_or(u64::MAX),
            wait_latency: self.wait_latency.snapshot(),
            sync_attempts: self.sync_attempts,
            sync_errors: self.sync_errors,
            sync_latency: self.sync_latency.snapshot(),
            frame_cache_entries: frame_cache_entries.try_into().unwrap_or(u64::MAX),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_aggregates_saturate_and_keep_the_latest_sample() {
        let mut latency = LatencyAccumulator::default();
        latency.record(Duration::from_millis(u64::MAX));
        latency.record(Duration::from_millis(7));
        latency.record(Duration::from_millis(11));

        assert_eq!(
            latency.snapshot(),
            DriverLatencyMetrics {
                samples: 3,
                total_ms: u64::MAX,
                max_ms: u64::MAX,
                last_ms: 11,
            }
        );
    }

    #[test]
    fn connection_and_wait_counters_are_monotonic() {
        let mut metrics = DriverMetricsAccumulator::default();
        metrics.record_connection();
        metrics.record_connection();
        metrics.record_rejected_connection();
        let wait = metrics.start_wait();
        metrics.finish_wait_result(wait);
        let timeout = metrics.start_wait();
        metrics.finish_wait_timeout(timeout);
        let cancelled = metrics.start_wait();
        metrics.cancel_wait(cancelled);

        let snapshot = metrics.snapshot("instance", 2, 3);
        assert_eq!(snapshot.connections, 2);
        assert_eq!(snapshot.reconnects, 1);
        assert_eq!(snapshot.rejected_connections, 1);
        assert_eq!(snapshot.wait_requests, 3);
        assert_eq!(snapshot.wait_results, 1);
        assert_eq!(snapshot.wait_timeouts, 1);
        assert_eq!(snapshot.wait_cancelled, 1);
        assert_eq!(snapshot.pending_waits, 2);
        assert_eq!(snapshot.frame_cache_entries, 3);
    }

    #[test]
    fn snapshot_has_no_sensitive_dimensions() {
        let metrics = DriverMetricsAccumulator::default().snapshot("instance", 0, 0);
        let encoded = serde_json::to_string(&metrics).expect("metrics JSON");
        assert!(!encoded.contains("workspace"));
        assert!(!encoded.contains("prompt"));
        assert!(!encoded.contains("secret"));
        assert!(!encoded.contains("provider"));
    }
}
