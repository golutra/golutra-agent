//! Coalesced redraw scheduling for the interactive terminal.

use std::time::{Duration, Instant};

/// Requests may arrive faster, but the terminal is never asked to render more
/// than 120 frames per second.
pub(crate) const MIN_FRAME_INTERVAL: Duration = Duration::from_nanos(8_333_334);

#[derive(Debug, Default)]
pub(crate) struct FrameScheduler {
    last_drawn_at: Option<Instant>,
    deadline: Option<Instant>,
}

impl FrameScheduler {
    pub(crate) fn request_at(&mut self, now: Instant) {
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
}
