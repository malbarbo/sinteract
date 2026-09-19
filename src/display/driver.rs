//! [`Display`], the trait an engine drives, and [`open_native`]. The module
//! is private, and [`super`] re-exports both.

use std::fmt;
use std::io;
use std::time::Instant;

use super::inbox::Sender;
use crate::event::{Event, NoEvent};
use crate::scene::Scene;

/// A session that shows scenes and delivers events. Opening is the
/// constructor of the implementation, and the session ends at
/// [`Display::close`] or at drop:
///
/// ```ignore
/// let options = TerminalOptions::default();
/// let mut fr = sinteract::display::open_native("My game", 400.0, 300.0, options)?;
/// loop {
///     match fr.wait_event(None) {
///         Ok(Event::Input(InputEvent::Vsync)) => fr.present(&next_scene()),
///         Ok(Event::Input(InputEvent::Key(k))) => on_key(k),
///         Ok(Event::Input(InputEvent::Mouse(m))) => on_mouse(m),
///         Ok(Event::Input(InputEvent::Resize { .. })) => {}
///         Ok(Event::Reply { id, body }) => on_reply(id, body),
///         Err(NoEvent::Wake | NoEvent::Timeout) => {}
///         Err(NoEvent::Close) => break,
///     }
/// }
/// fr.close();
/// ```
///
/// The trait is sealed. Its contract, the order of arrival, one Vsync
/// pending and a Close that stays, does not fit in its types, and an
/// implementation outside the crate would need a public way to build a
/// [`Sender`].
pub trait Display: sealed::Sealed {
    /// Show `scene`. After [`Display::close`] it does nothing.
    fn present(&mut self, scene: &Scene);

    /// Block until the next event, or until `deadline` and then return
    /// [`NoEvent::Timeout`], or with no limit when it is `None`. The events
    /// go out in the order of arrival. After a Close, every call returns
    /// [`NoEvent::Close`]. A `while let Ok(ev)` over it also stops at the
    /// first Timeout.
    fn wait_event(&mut self, deadline: Option<Instant>) -> Result<Event, NoEvent>;

    /// A handle that pushes into this queue from any thread.
    fn sender(&self) -> Sender;

    /// Upload a bitmap for `Bitmap.id`. A display that draws without
    /// bitmaps drops it.
    fn push_asset(&mut self, id: u32, blob: &[u8], mime: Option<&str>);

    /// End the session. A second call does nothing, and drop calls it.
    fn close(&mut self);
}

pub(super) mod sealed {
    pub trait Sealed {}
}

/// The terminal when stdout is a tty with graphics, and a window of `width`
/// by `height` logical pixels otherwise. `title` only matters for a window,
/// because a terminal keeps the title of the shell, and `options` only for a
/// terminal.
#[cfg(all(feature = "terminal", feature = "window"))]
pub fn open_native(
    title: &str,
    width: f32,
    height: f32,
    options: super::TerminalOptions,
) -> Result<Box<dyn Display>, OpenError> {
    match super::Terminal::open_with(options) {
        Ok(terminal) => Ok(Box::new(terminal)),
        Err(OpenError::NoGraphics) => Ok(Box::new(super::Window::open(title, width, height)?)),
        Err(e) => Err(e),
    }
}

/// Why a display did not open.
#[derive(Debug)]
pub enum OpenError {
    /// Another session holds the resource of the process: the terminal, the
    /// event loop of the windows or stdin.
    Busy,
    /// The terminal shows neither Kitty, Sixel nor truecolor.
    NoGraphics,
    /// A read or a write failed, or a thread did not start.
    Io(io::Error),
    /// The platform has no window for us.
    Platform(String),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::Busy => f.write_str("another session is open"),
            OpenError::NoGraphics => f.write_str(
                "the terminal shows no graphics; try Kitty, Ghostty, WezTerm, Konsole, \
                 a Sixel terminal (Windows Terminal 1.22 or later, mlterm, foot, mintty) \
                 or a truecolor terminal (set COLORTERM=truecolor)",
            ),
            OpenError::Io(e) => write!(f, "cannot open the display: {e}"),
            OpenError::Platform(e) => write!(f, "cannot open a window: {e}"),
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OpenError::Io(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(any(feature = "terminal", feature = "window"))]
pub(super) const fn period_from_hz(hz: u32) -> std::time::Duration {
    std::time::Duration::from_nanos(1_000_000_000 / hz as u64)
}

/// Say once per display that this backend drops the bitmaps of the frame. A
/// process-global flag would stay silent for every session after the first,
/// and a server hosts many sessions.
#[cfg(any(feature = "terminal", feature = "window"))]
pub(super) fn warn_bitmaps_once(warned: &mut bool, scene: &Scene, backend: &str) {
    if !*warned && scene.has_bitmaps() {
        *warned = true;
        eprintln!(
            "[sinteract] the {backend} renderer does not support bitmaps; drawing without them."
        );
    }
}
