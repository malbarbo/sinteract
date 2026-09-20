//! [`Display`], the trait an engine drives, and [`open_native`]. The module
//! is private, and [`super`] re-exports both.

use std::fmt;
use std::io;
use std::time::Instant;

use super::inbox::Sender;
use crate::event::{Event, NoEvent};
use crate::renderer::AllocError;
use crate::scene::Scene;

/// A session that shows scenes and delivers events. Opening is the
/// constructor of the implementation, and the session ends at
/// [`Display::close`] or at drop:
///
/// ```no_run
/// # use sinteract::display::Display;
/// # use sinteract::event::{Event, InputEvent, KeyEvent, MouseEvent, NoEvent};
/// # use sinteract::scene::Scene;
/// # fn next_scene() -> Scene { Scene::new(400.0, 300.0) }
/// # fn on_key(_: KeyEvent) {}
/// # fn on_mouse(_: MouseEvent) {}
/// # fn run(fr: &mut dyn Display) -> Result<(), Box<dyn std::error::Error>> {
/// loop {
///     match fr.wait_event(None) {
///         Ok(Event::Input(InputEvent::Vsync)) => fr.present(next_scene())?,
///         Ok(Event::Input(InputEvent::Key(k))) => on_key(k),
///         Ok(Event::Input(InputEvent::Mouse(m))) => on_mouse(m),
///         Ok(Event::Input(InputEvent::Resize { .. })) => {}
///         Err(NoEvent::Wake | NoEvent::Timeout) => {}
///         Err(NoEvent::Damaged(e)) => eprintln!("{e}"),
///         Err(NoEvent::Broken(e)) => eprintln!("{e}"),
///         Err(NoEvent::Close) => break,
///     }
/// }
/// fr.close();
/// # Ok(())
/// # }
/// ```
///
/// The trait is sealed. Its contract, the order of arrival, one Vsync
/// pending and a Close that stays, does not fit in its types, and an
/// implementation outside the crate would need a public way to build a
/// [`Sender`].
pub trait Display: sealed::Sealed {
    /// Show `scene`. The display takes it, because a resize draws it
    /// again at the new scale. A failure leaves the session open, and the
    /// caller decides whether to show the error, to try another scene or to
    /// [`close`](Display::close).
    fn present(&mut self, scene: Scene) -> Result<(), PresentError>;

    /// Block until the next event, or until `deadline` and then return
    /// [`NoEvent::Timeout`], or with no limit when it is `None`. The events
    /// go out in the order of arrival. After a Close, every call returns
    /// [`NoEvent::Close`]. A `while let Ok(ev)` over it also stops at the
    /// first Wake or Timeout.
    fn wait_event(&mut self, deadline: Option<Instant>) -> Result<Event, NoEvent>;

    /// A handle that pushes into this queue from any thread.
    fn sender(&self) -> Sender;

    /// Upload a bitmap for `Bitmap.id`, and say what the display did with
    /// it. Call it before the first [`present`](Display::present) of a
    /// scene that names `id`.
    fn push_asset(
        &mut self,
        id: u32,
        blob: &[u8],
        mime: Option<&str>,
    ) -> Result<Upload, PresentError>;

    /// End the session. A second call does nothing, and drop calls it.
    fn close(&mut self);
}

pub(super) mod sealed {
    pub trait Sealed {}
}

/// Why a frame or an asset did not reach the display.
#[derive(Debug)]
pub enum PresentError {
    /// The session ended, at [`Display::close`] or because the peer stopped
    /// reading. [`Display::wait_event`] also reports it, as
    /// [`NoEvent::Close`].
    Closed,
    /// Rasterizing the scene failed.
    Alloc(AllocError),
    /// A write to the terminal or to the peer failed. Part of the frame may
    /// have arrived.
    Io(io::Error),
    /// The surface of the window refused the frame.
    Platform(String),
}

impl fmt::Display for PresentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PresentError::Closed => f.write_str("the session ended"),
            PresentError::Alloc(e) => write!(f, "cannot draw the scene: {e}"),
            PresentError::Io(e) => write!(f, "cannot show the frame: {e}"),
            PresentError::Platform(e) => write!(f, "cannot show the frame: {e}"),
        }
    }
}

impl std::error::Error for PresentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PresentError::Alloc(e) => Some(e),
            PresentError::Io(e) => Some(e),
            PresentError::Closed | PresentError::Platform(_) => None,
        }
    }
}

impl From<AllocError> for PresentError {
    fn from(e: AllocError) -> Self {
        PresentError::Alloc(e)
    }
}

impl From<io::Error> for PresentError {
    fn from(e: io::Error) -> Self {
        PresentError::Io(e)
    }
}

/// What a display did with an upload. A dropped asset is not a failure.
/// The rest of the scene still draws, and the program decides whether to
/// tell the user that the image will not appear.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Upload {
    /// The display keeps the asset, and a bitmap of that id draws.
    Kept,
    /// The display draws no bitmap, so it dropped the asset.
    Dropped,
}

/// The terminal when stdout is a tty with graphics, and a window of `width`
/// by `height` logical pixels otherwise. `title` only matters for a window,
/// because a terminal keeps the title of the shell, and `options` only for a
/// terminal.
///
/// ```no_run
/// # use sinteract::display::{Display, TerminalOptions, open_native};
/// let options = TerminalOptions::default();
/// let mut fr = open_native("My game", 400.0, 300.0, options)?;
/// fr.close();
/// # Ok::<(), sinteract::display::OpenError>(())
/// ```
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
    /// Another thread opened the first window, and winit keeps the event
    /// loop of the process on that one.
    WrongThread,
    /// The event loop of the windows ended, and the platform starts no
    /// other one in this process.
    LoopEnded,
    /// The window did not open in time.
    Timeout,
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
            OpenError::WrongThread => {
                f.write_str("a window opens only on the thread of the first window")
            }
            OpenError::LoopEnded => {
                f.write_str("the window event loop ended, and it cannot start again")
            }
            OpenError::Timeout => f.write_str("the window did not open in time"),
            OpenError::Io(e) => write!(f, "cannot open the display: {e}"),
            OpenError::Platform(e) => write!(f, "cannot open a window: {e}"),
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OpenError::Io(e) => Some(e),
            OpenError::Busy
            | OpenError::NoGraphics
            | OpenError::WrongThread
            | OpenError::LoopEnded
            | OpenError::Timeout
            | OpenError::Platform(_) => None,
        }
    }
}

#[cfg(any(feature = "terminal", feature = "window"))]
pub(super) const fn period_from_hz(hz: u32) -> std::time::Duration {
    std::time::Duration::from_nanos(1_000_000_000 / hz as u64)
}
