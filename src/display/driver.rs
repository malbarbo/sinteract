//! [`Display`], the trait an engine drives, and [`open_native`]. The module
//! is private, and [`super`] re-exports both.

use std::fmt;
use std::io;
use std::num::NonZeroU32;
use std::time::Instant;

use super::inbox::Sender;
use crate::asset::RoomFull;
use crate::event::Event;
use crate::renderer::AllocError;
use crate::scene::Scene;
use crate::session::Player;

/// A session that shows scenes and delivers events. Opening is the
/// constructor of the implementation, and the session ends at drop:
///
/// ```no_run
/// # use sinteract::display::Display;
/// # use sinteract::event::{Event, InputEvent, KeyEvent, MouseEvent};
/// # use sinteract::scene::Scene;
/// # fn next_scene() -> Scene { Scene::new(400.0, 300.0) }
/// # fn on_key(_: KeyEvent) {}
/// # fn on_mouse(_: MouseEvent) {}
/// # fn run(display: &mut dyn Display) -> Result<(), Box<dyn std::error::Error>> {
/// loop {
///     match display.wait(None)? {
///         Event::Tick => display.present(next_scene())?,
///         Event::Input(InputEvent::Key(k)) => on_key(k),
///         Event::Input(InputEvent::Mouse(m)) => on_mouse(m),
///         Event::Input(InputEvent::Resize { .. } | InputEvent::Pad(_)) => {}
///         Event::Wake | Event::Timeout => {}
///         Event::Close => break,
///     }
/// }
/// # Ok(())
/// # }
/// ```
///
/// The trait is sealed. Its contract, the order of arrival, one tick
/// pending and a Close that stays, does not fit in its types, and an
/// implementation outside the crate would need a public way to build a
/// [`Sender`].
pub trait Display: sealed::Sealed {
    /// Show `scene`. The display takes it, because a resize draws it
    /// again at the new scale. A failure leaves the session open, and the
    /// caller decides whether to show the error, to try another scene or to
    /// drop the display.
    fn present(&mut self, scene: Scene) -> Result<(), PresentError>;

    /// Block until the next event, or until `deadline` and then return
    /// [`Event::Timeout`], or with no limit when it is `None`. The events
    /// go out in the order of arrival. A read of the input that fails
    /// returns its error, and after the error or a Close every call returns
    /// [`Event::Close`]. So a loop ends at Close, since the wait does not
    /// fail there.
    fn wait(&mut self, deadline: Option<Instant>) -> io::Result<Event>;

    /// A handle that pushes into this queue from any thread.
    fn sender(&self) -> Sender;

    /// The rate of the ticks in thousandths of a hertz, 60000 for 60 Hz.
    /// A window ticks at the rate of its monitor, which changes when the
    /// window moves to another monitor, so a game reads it again at each
    /// tick.
    fn tick_rate(&self) -> NonZeroU32;

    /// The rows of text under the frame. Only the terminal keeps them, as
    /// many as [`TerminalOptions::footer_rows`](super::TerminalOptions::footer_rows).
    fn footer_rows(&self) -> u16 {
        0
    }

    /// Show the last [`Display::footer_rows`] of `lines` under the frame.
    fn set_footer(&mut self, _lines: &[String]) -> Result<(), PresentError> {
        Ok(())
    }
}

pub(super) mod sealed {
    pub trait Sealed {}
}

/// Why a frame did not reach the display.
#[derive(Debug)]
pub enum PresentError {
    /// Rasterizing the scene failed.
    Alloc(AllocError),
    /// A write to the terminal or to the server failed. Part of the frame
    /// may have arrived.
    Io(io::Error),
    /// The surface of the window refused the frame.
    Platform(String),
    /// The images of the frame go over the limits of a room together. Only
    /// a [`super::Stage`] in a session refuses such a frame, and nothing
    /// went out.
    Full(RoomFull),
    /// The frame goes to a player that the [`super::Stage`] does not have,
    /// such as a player of an earlier stage, and nothing went out.
    NoPlayer(Player),
}

impl fmt::Display for PresentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PresentError::Alloc(e) => write!(f, "cannot draw the scene: {e}"),
            PresentError::Io(e) => write!(f, "cannot show the frame: {e}"),
            PresentError::Platform(e) => write!(f, "cannot show the frame: {e}"),
            PresentError::Full(e) => e.fmt(f),
            PresentError::NoPlayer(player) => {
                write!(f, "the stage has no player {}", player.number())
            }
        }
    }
}

impl std::error::Error for PresentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PresentError::Alloc(e) => Some(e),
            PresentError::Io(e) => Some(e),
            PresentError::Full(e) => Some(e),
            PresentError::Platform(_) | PresentError::NoPlayer(_) => None,
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

/// A window of `width` by `height` logical pixels, and the terminal when no
/// window opens, as over ssh. `title` only matters for a window, because a
/// terminal keeps the title of the shell, and `options` only for a
/// terminal. When neither opens, the error is the one of the terminal,
/// which names the terminals that show graphics.
///
/// ```no_run
/// # use sinteract::display::{Display, TerminalOptions, open_native};
/// let options = TerminalOptions::default();
/// let mut display = open_native("My game", 400.0, 300.0, options)?;
/// # drop(display);
/// # Ok::<(), sinteract::display::TerminalError>(())
/// ```
#[cfg(all(feature = "terminal", feature = "window"))]
pub fn open_native(
    title: &str,
    width: f32,
    height: f32,
    options: super::TerminalOptions,
) -> Result<Box<dyn Display>, TerminalError> {
    window_or_terminal(|| super::Window::open(title, width, height), options)
}

/// The window of `window`, and the terminal when no window opens, as
/// [`open_native`] says.
#[cfg(all(feature = "terminal", feature = "window"))]
pub(crate) fn window_or_terminal(
    window: impl FnOnce() -> Result<super::Window, WindowError>,
    options: super::TerminalOptions,
) -> Result<Box<dyn Display>, TerminalError> {
    match window() {
        Ok(window) => Ok(Box::new(window)),
        Err(_) => Ok(Box::new(super::Terminal::open(options, None)?)),
    }
}

/// Why a [`Window`](super::Window) did not open.
#[derive(Debug)]
pub enum WindowError {
    /// Another window is open.
    Busy,
    /// The session has no graphical display, as in a terminal over ssh.
    NoWindowSystem,
    /// Another thread opened the first window, and winit keeps the event
    /// loop of the process on that one.
    WrongThread,
    /// The event loop of the windows ended, and the platform starts no
    /// other one in this process.
    LoopEnded,
    /// The window did not open in time.
    Timeout,
    /// The platform has no window for us.
    Platform(String),
}

impl fmt::Display for WindowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WindowError::Busy => f.write_str("another window is open"),
            WindowError::NoWindowSystem => f.write_str("this session has no graphical display"),
            WindowError::WrongThread => {
                f.write_str("a window opens only on the thread of the first window")
            }
            WindowError::LoopEnded => {
                f.write_str("the window event loop ended, and it cannot start again")
            }
            WindowError::Timeout => f.write_str("the window did not open in time"),
            WindowError::Platform(e) => write!(f, "cannot open a window: {e}"),
        }
    }
}

impl std::error::Error for WindowError {}

/// Why a [`Terminal`](super::Terminal) did not open.
#[derive(Debug)]
pub enum TerminalError {
    /// Another session holds the terminal.
    Busy,
    /// The terminal shows neither Kitty, Sixel nor truecolor.
    NoGraphics,
    /// A read or a write failed, or a thread did not start.
    Io(io::Error),
}

impl fmt::Display for TerminalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TerminalError::Busy => f.write_str("another session holds the terminal"),
            TerminalError::NoGraphics => NoGraphics.fmt(f),
            TerminalError::Io(e) => write!(f, "cannot open the terminal: {e}"),
        }
    }
}

impl std::error::Error for TerminalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TerminalError::Io(e) => Some(e),
            TerminalError::Busy | TerminalError::NoGraphics => None,
        }
    }
}

/// The terminal shows neither Kitty, Sixel nor truecolor, or stdout is not
/// a terminal, so it takes no image.
#[derive(Debug)]
pub struct NoGraphics;

impl fmt::Display for NoGraphics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "the terminal shows no graphics; try Kitty, Ghostty, WezTerm, Konsole, \
             a Sixel terminal (Windows Terminal 1.22 or later, mlterm, foot, mintty) \
             or a truecolor terminal (set COLORTERM=truecolor)",
        )
    }
}

impl std::error::Error for NoGraphics {}

impl From<NoGraphics> for TerminalError {
    fn from(_: NoGraphics) -> Self {
        TerminalError::NoGraphics
    }
}
