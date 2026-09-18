//! [`Frontend`], the trait a host drives, and [`open_native`]. The module
//! is private, and [`super`] re-exports both.

use std::time::{Duration, Instant};

use super::inbox::Sender;
use super::{Terminal, Window};
use crate::event::Event;
use crate::scene::Scene;

/// A session that shows scenes and delivers events. Opening is the
/// constructor of the implementation, and the session ends at
/// [`Frontend::close`] or at drop:
///
/// ```ignore
/// let mut fr = sinteract::frontend::open_native("My game");
/// loop {
///     match fr.wait_event(None) {
///         Event::Input(InputEvent::Vsync) => fr.present(&next_scene()),
///         Event::Input(InputEvent::Key(k)) => on_key(k),
///         Event::Input(InputEvent::Close) => break,
///         Event::Reply { id, body } => on_reply(id, body),
///         Event::Timeout => {}
///     }
/// }
/// fr.close();
/// ```
///
/// The trait is sealed. Its contract, the order of arrival, one Vsync
/// pending and a Close that stays, does not fit in its types, and an
/// implementation outside the crate would need a public way to build a
/// [`Sender`].
pub trait Frontend: sealed::Sealed {
    /// Show `scene`. After [`Frontend::close`] it does nothing.
    fn present(&mut self, scene: &Scene);

    /// Block until the next event or until `deadline`, or with no limit
    /// when it is `None`. The events go out in the order of arrival. After
    /// a Close, every call returns Close.
    fn wait_event(&mut self, deadline: Option<Instant>) -> Event;

    /// A handle that pushes into this queue from any thread.
    fn sender(&self) -> Sender;

    /// Upload a bitmap for `Bitmap.id`.
    fn push_asset(&mut self, id: u32, blob: &[u8], mime: Option<&str>);

    /// End the session. A second call does nothing, and drop calls it.
    fn close(&mut self);
}

pub(super) mod sealed {
    pub trait Sealed {}
}

/// The terminal when stdout is a tty with graphics, and a window
/// otherwise. `title` only matters for a window. A terminal keeps the title
/// of the shell.
pub fn open_native(title: &str) -> Box<dyn Frontend> {
    if super::terminal::kitty_supported()
        || super::sixel::sixel_supported()
        || super::terminal::text_blocks_supported()
    {
        Box::new(Terminal::open())
    } else {
        Box::new(Window::open(title))
    }
}

pub(super) const fn period_from_hz(hz: u32) -> Duration {
    Duration::from_nanos(1_000_000_000 / hz as u64)
}

/// How often the terminal and the window look for a key. Their input does
/// not wake the queue, so [`poll_input`] checks this often.
pub(super) const INPUT_POLL: Duration = Duration::from_millis(8);

/// Wait on `inbox` in steps of [`INPUT_POLL`], and call `forward` before
/// each step so it moves the input of the platform into the queue.
pub(super) fn poll_input(
    inbox: &mut super::inbox::Inbox,
    deadline: Option<Instant>,
    mut forward: impl FnMut(),
) -> Event {
    loop {
        forward();
        let step = Instant::now() + INPUT_POLL;
        let until = deadline.map_or(step, |d| d.min(step));
        match inbox.wait(Some(until)) {
            Event::Timeout if deadline.is_none_or(|d| Instant::now() < d) => {}
            event => return event,
        }
    }
}

/// Say once per frontend that this backend drops the bitmaps of the frame. A
/// process-global flag would stay silent for every session after the first,
/// and a server hosts many sessions.
pub(super) fn warn_bitmaps_once(warned: &mut bool, scene: &Scene, backend: &str) {
    if !*warned && scene.has_bitmaps() {
        *warned = true;
        eprintln!(
            "[sinteract] the {backend} renderer does not support bitmaps; drawing without them."
        );
    }
}
