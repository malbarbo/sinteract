//! The software clock that paces the frames of a display.

use std::time::{Duration, Instant};

use crate::session::FrameRate;

/// When the next frame of a display falls due, at the next tick of the
/// clock. The first tick is due at once.
pub(crate) struct FrameClock {
    rate: FrameRate,
    due: Instant,
}

impl FrameClock {
    pub(crate) fn new(rate: FrameRate) -> Self {
        Self {
            rate,
            due: Instant::now(),
        }
    }

    /// Change the rate to `rate`. The next tick keeps its time.
    #[cfg_attr(not(feature = "window"), allow(dead_code))]
    pub(crate) fn set_rate(&mut self, rate: FrameRate) {
        self.rate = rate;
    }

    pub(crate) fn rate(&self) -> FrameRate {
        self.rate
    }

    /// Move the next tick to the nearest time that is a whole number of
    /// periods from `vblank`, the time of a frame of the screen, so the
    /// ticks keep the phase of the screen and not only its rate. The next
    /// tick moves by half a period at most, so no tick is lost or doubled.
    #[cfg_attr(not(feature = "window"), allow(dead_code))]
    pub(crate) fn align(&mut self, vblank: Instant) {
        self.due = match self.due.checked_duration_since(vblank) {
            Some(ahead) => vblank + nearest_multiple(ahead, self.rate.period()),
            None => vblank - nearest_multiple(vblank - self.due, self.rate.period()),
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
        self.due += self.rate.period();
        if self.due <= now {
            self.due = now + self.rate.period();
        }
        true
    }
}

/// The whole number of periods nearest to `d`.
fn nearest_multiple(d: Duration, period: Duration) -> Duration {
    period.mul_f64((d.as_secs_f64() / period.as_secs_f64()).round())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_60_hz() -> FrameClock {
        FrameClock::new(FrameRate::from_hz(std::num::NonZeroU16::new(60).unwrap()))
    }

    #[test]
    fn the_period_comes_from_the_rate() {
        assert_eq!(at_60_hz().rate.period(), Duration::from_nanos(16_666_666));
    }

    #[test]
    fn aligns_the_next_tick_to_the_nearest_frame_of_the_screen() {
        let mut clock = at_60_hz();
        let t0 = clock.due();
        let period = clock.rate.period();
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
        let period = clock.rate.period();
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
        let period = clock.rate.period();
        assert!(clock.take_due(t0));
        let late = t0 + period * 3;
        assert!(clock.take_due(late));
        assert!(!clock.take_due(late));
        assert!(!clock.take_due(late + period - period / 8));
        assert!(clock.take_due(late + period));
    }
}
