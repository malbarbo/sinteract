//! [`Headless`], the display of a stage under [`super::StageMode::Headless`].

use std::io;
use std::num::NonZeroU16;
use std::time::Instant;

use super::driver::{PresentError, sealed};
use super::frame_clock::FrameClock;
use super::inbox::{Inbox, Next, Sender};
use crate::event::Event;
use crate::scene::Scene;
use crate::session::FrameRate;

/// A display that starts frames and draws nothing, so a game runs on its clock
/// with no screen, as in a test.
pub(crate) struct Headless {
    inbox: Inbox,
    clock: FrameClock,
}

impl Headless {
    /// The rate of the frames, which no screen paces.
    const FRAME_RATE: FrameRate = FrameRate::from_hz(NonZeroU16::new(60).expect("60 is not zero"));

    pub(crate) fn new() -> Self {
        Self {
            inbox: Inbox::new(None),
            clock: FrameClock::new(Self::FRAME_RATE),
        }
    }
}

impl super::Display for Headless {
    fn present(&mut self, _scene: Scene) -> Result<(), PresentError> {
        Ok(())
    }

    fn wait(&mut self, deadline: Option<Instant>) -> io::Result<Event> {
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

    fn frame_rate(&self) -> FrameRate {
        self.clock.rate()
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
        assert!(matches!(display.wait(Some(deadline)), Ok(Event::Frame)));
        assert!(matches!(display.wait(Some(deadline)), Ok(Event::Frame)));
        assert!(display.present(Scene::new(10.0, 10.0)).is_ok());
    }

    #[test]
    fn a_deadline_before_the_tick_times_out() {
        let mut display = Headless::new();
        assert!(matches!(display.wait(None), Ok(Event::Frame)));
        assert!(matches!(
            display.wait(Some(Instant::now())),
            Ok(Event::Timeout)
        ));
    }
}
