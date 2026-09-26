//! The software clock that paces the ticks of a display.

use std::num::NonZeroU32;
use std::time::{Duration, Instant};

/// When the next tick of a display falls due. The first one is due at
/// once.
pub(crate) struct TickClock {
    period: Duration,
    due: Instant,
}

impl TickClock {
    /// A clock at `rate` thousandths of a hertz, the unit of a refresh rate
    /// in winit.
    pub(crate) fn from_millihertz(rate: NonZeroU32) -> Self {
        Self {
            period: period(rate),
            due: Instant::now(),
        }
    }

    /// Change the rate to `rate` thousandths of a hertz. The next tick
    /// keeps its time.
    #[cfg_attr(not(feature = "window"), allow(dead_code))]
    pub(crate) fn set_millihertz(&mut self, rate: NonZeroU32) {
        self.period = period(rate);
    }

    /// Move the next tick to the nearest time that is a whole number of
    /// periods from `vblank`, the time of a frame of the screen, so the
    /// ticks keep the phase of the screen and not only its rate. The next
    /// tick moves by half a period at most, so no tick is lost or doubled.
    #[cfg_attr(not(feature = "window"), allow(dead_code))]
    pub(crate) fn align(&mut self, vblank: Instant) {
        let period = self.period.as_secs_f64();
        let (ahead, sign) = match self.due.checked_duration_since(vblank) {
            Some(d) => (d.as_secs_f64(), 1.0),
            None => ((vblank - self.due).as_secs_f64(), -1.0),
        };
        let periods = sign * (ahead / period).round();
        let offset = self.period.mul_f64(periods.abs());
        self.due = if periods < 0.0 {
            vblank - offset
        } else {
            vblank + offset
        };
    }

    pub(crate) fn due(&self) -> Instant {
        self.due
    }

    /// Returns `true` if a tick fell due by `now`, `false` otherwise. A
    /// `true` moves the clock to the next tick. The clock keeps its beat,
    /// so a frame that took less than the period loses no time to the
    /// wait. A tick taken more than a period late starts a new beat at
    /// `now`, so a slow engine gets one tick at once and not a second
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

fn period(rate: NonZeroU32) -> Duration {
    Duration::from_nanos(1_000_000_000_000 / u64::from(rate.get()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_60_hz() -> TickClock {
        TickClock::from_millihertz(NonZeroU32::new(60_000).unwrap())
    }

    #[test]
    fn the_period_comes_from_the_rate() {
        assert_eq!(at_60_hz().period, Duration::from_nanos(16_666_666));
    }

    #[test]
    fn aligns_the_next_tick_to_the_nearest_frame_of_the_screen() {
        let mut clock = at_60_hz();
        let t0 = clock.due();
        let period = clock.period;
        assert!(clock.take_due(t0));
        // The screen shows a frame a little after the next tick was due, so
        // the tick moves to that frame.
        let vblank = t0 + period + period / 5;
        clock.align(vblank);
        assert_eq!(clock.due(), vblank);
        // A frame a little after the tick that just went out moves the next
        // one a period after the frame, not to the frame again.
        assert!(clock.take_due(vblank));
        let late = vblank + period / 5;
        clock.align(late);
        assert_eq!(clock.due(), late + period);
        // A frame of the screen after the due time pulls the tick back.
        clock.align(late + period * 2 - period / 5);
        assert_eq!(clock.due(), late + period - period / 5);
    }

    #[test]
    fn keeps_its_beat_after_a_short_frame() {
        let mut clock = at_60_hz();
        let t0 = clock.due();
        let period = clock.period;
        assert!(clock.take_due(t0));
        // The engine comes back a little after the tick was due.
        assert!(clock.take_due(t0 + period + period / 4));
        assert!(!clock.take_due(t0 + period * 2 - period / 8));
        assert!(clock.take_due(t0 + period * 2));
    }

    #[test]
    fn a_late_engine_gets_one_tick_at_once_and_the_next_a_period_later() {
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
