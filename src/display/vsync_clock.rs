//! The software clock that paces the Vsync of a display.

use std::num::NonZeroU32;
use std::time::{Duration, Instant};

/// When the next Vsync of a display falls due. The first one is due at
/// once.
pub(crate) struct VsyncClock {
    period: Duration,
    due: Instant,
}

impl VsyncClock {
    /// A clock at `rate` thousandths of a hertz, the unit of a refresh rate
    /// in winit.
    pub(crate) fn from_millihertz(rate: NonZeroU32) -> Self {
        Self {
            period: Duration::from_nanos(1_000_000_000_000 / u64::from(rate.get())),
            due: Instant::now(),
        }
    }

    pub(crate) fn due(&self) -> Instant {
        self.due
    }

    /// Returns `true` if a Vsync fell due by `now`, `false` otherwise. A
    /// `true` moves the clock to the next Vsync. The clock keeps its beat,
    /// so a frame that took less than the period loses no time to the
    /// wait. A Vsync taken more than a period late starts a new beat at
    /// `now`, so a slow engine gets one Vsync at once and not a second
    /// one right behind it.
    pub(crate) fn take_due(&mut self, now: Instant) -> bool {
        if self.due > now {
            return false;
        }
        self.due += self.period;
        if self.due <= now {
            self.due = now + self.period;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_60_hz() -> VsyncClock {
        VsyncClock::from_millihertz(NonZeroU32::new(60_000).unwrap())
    }

    #[test]
    fn the_period_comes_from_the_rate() {
        assert_eq!(at_60_hz().period, Duration::from_nanos(16_666_666));
    }

    #[test]
    fn keeps_its_beat_after_a_short_frame() {
        let mut clock = at_60_hz();
        let t0 = clock.due();
        let period = clock.period;
        assert!(clock.take_due(t0));
        // The engine comes back a little after the Vsync was due.
        assert!(clock.take_due(t0 + period + period / 4));
        assert!(!clock.take_due(t0 + period * 2 - period / 8));
        assert!(clock.take_due(t0 + period * 2));
    }

    #[test]
    fn a_late_engine_gets_one_vsync_at_once_and_the_next_a_period_later() {
        let mut clock = at_60_hz();
        let t0 = clock.due();
        let period = clock.period;
        assert!(clock.take_due(t0));
        let late = t0 + period * 3;
        assert!(clock.take_due(late));
        assert!(!clock.take_due(late));
        assert!(!clock.take_due(late + period - period / 8));
        assert!(clock.take_due(late + period));
    }
}
