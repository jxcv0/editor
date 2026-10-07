//! Time-based presentation only. Editing positions never pass through a tween.

use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub(crate) struct Motion {
    from: (f64, f64),
    target: (usize, usize),
    started: Instant,
    duration: Duration,
}

impl Motion {
    pub fn new(target: (usize, usize), now: Instant) -> Self {
        Self {
            from: (target.0 as f64, target.1 as f64),
            target,
            started: now,
            duration: Duration::ZERO,
        }
    }

    fn sample(&self, now: Instant) -> (f64, f64) {
        if !self.active(now) {
            return (self.target.0 as f64, self.target.1 as f64);
        }
        let t =
            now.saturating_duration_since(self.started).as_secs_f64() / self.duration.as_secs_f64();
        let ease = 1.0 - (1.0 - t).powi(3);
        (
            self.from.0 + (self.target.0 as f64 - self.from.0) * ease,
            self.from.1 + (self.target.1 as f64 - self.from.1) * ease,
        )
    }

    pub fn position(&self, now: Instant) -> (usize, usize) {
        let (x, y) = self.sample(now);
        (x.round() as usize, y.round() as usize)
    }

    pub fn retarget(
        &mut self,
        target: (usize, usize),
        now: Instant,
        duration: Duration,
        max_travel: (usize, usize),
    ) {
        if self.target == target {
            return;
        }
        let current = self.sample(now);
        let bound = |value: f64, target: usize, limit: usize| {
            value.clamp(
                target.saturating_sub(limit) as f64,
                target.saturating_add(limit) as f64,
            )
        };
        self.from = (
            bound(current.0, target.0, max_travel.0),
            bound(current.1, target.1, max_travel.1),
        );
        self.target = target;
        self.started = now;
        self.duration = duration;
    }

    pub fn active(&self, now: Instant) -> bool {
        self.from != (self.target.0 as f64, self.target.1 as f64)
            && now.saturating_duration_since(self.started) < self.duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn movement_retargets_from_current_position_and_finishes_by_deadline() {
        let start = Instant::now();
        let mut motion = Motion::new((0, 0), start);
        motion.retarget((20, 10), start, Duration::from_millis(100), (100, 100));
        let middle = start + Duration::from_millis(30);
        let position = motion.position(middle);
        assert!(position.0 > 0 && position.0 < 20);
        motion.retarget((0, 0), middle, Duration::from_millis(100), (100, 100));
        assert_eq!(motion.position(middle), position);
        assert_eq!(motion.position(middle + Duration::from_millis(100)), (0, 0));
        assert!(!motion.active(middle + Duration::from_millis(100)));
    }

    #[test]
    fn large_jumps_have_bounded_travel_and_zero_duration_snaps() {
        let now = Instant::now();
        let mut motion = Motion::new((0, 0), now);
        motion.retarget((100_000, 50_000), now, Duration::from_millis(100), (40, 20));
        assert_eq!(motion.position(now), (99_960, 49_980));
        motion.retarget((1, 2), now, Duration::ZERO, (40, 20));
        assert_eq!(motion.position(now), (1, 2));
        assert!(!motion.active(now));
    }
}
