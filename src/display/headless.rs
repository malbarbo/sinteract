//! [`Headless`], the display of a stage under [`super::StageMode::Headless`].

use std::num::NonZeroU32;
use std::time::Instant;

use super::driver::{PresentError, sealed};
use super::inbox::{Inbox, Next, Sender};
use super::tick_clock::TickClock;
use crate::event::{Event, Interrupt};
use crate::scene::Scene;

/// A display that ticks and draws nothing, so a game runs on its clock
/// with no screen, as in a test.
pub(crate) struct Headless {
    inbox: Inbox,
    clock: TickClock,
}

impl Headless {
    /// The rate of the ticks, which no screen paces.
    const TICK_RATE: NonZeroU32 = NonZeroU32::new(60_000).expect("60 Hz is not zero");

    pub(crate) fn new() -> Self {
        Self {
            inbox: Inbox::new(None),
            clock: TickClock::from_millihertz(Self::TICK_RATE),
        }
    }
}

impl super::Display for Headless {
    fn present(&mut self, _scene: Scene) -> Result<(), PresentError> {
        Ok(())
    }

    fn wait_event(&mut self, deadline: Option<Instant>) -> Result<Event, Interrupt> {
        loop {
            match self.inbox.next(&mut self.clock, deadline) {
                Next::Ready(ready) => return ready.map(Event::from),
                Next::Redraw => {}
                Next::Block(timeout) => self.inbox.wait_for_message(timeout),
            }
        }
    }

    fn sender(&self) -> Sender {
        self.inbox.sender()
    }

    fn tick_rate(&self) -> NonZeroU32 {
        self.clock.millihertz()
    }
}

impl sealed::Sealed for Headless {}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::display::Display;

    #[test]
    fn ticks_and_takes_a_scene() {
        let mut display = Headless::new();
        let deadline = Instant::now() + Duration::from_secs(1);
        assert!(matches!(
            display.wait_event(Some(deadline)),
            Ok(Event::Tick)
        ));
        assert!(matches!(
            display.wait_event(Some(deadline)),
            Ok(Event::Tick)
        ));
        assert!(display.present(Scene::new(10.0, 10.0)).is_ok());
    }

    #[test]
    fn a_deadline_before_the_tick_times_out() {
        let mut display = Headless::new();
        assert!(matches!(display.wait_event(None), Ok(Event::Tick)));
        assert!(matches!(
            display.wait_event(Some(Instant::now())),
            Err(Interrupt::Timeout)
        ));
    }
}
