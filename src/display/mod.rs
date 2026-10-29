//! Where a scene goes out and where the input comes back.
//!
//! [`Display`] is what an engine (spython, sgleam) drives. [`Terminal`] and
//! [`Window`] implement it, and the engine runs one loop over whichever it
//! opened.
//!
//! [`terminal`] shows a pixmap through Kitty, Sixel or half-blocks,
//! `term_query` probes what the terminal supports, `shm` hands a Kitty image
//! to a terminal on the same machine, `term_input` reads the
//! keys, with or without the keyboard protocol of Kitty or the
//! win32-input-mode of Windows Terminal, `sixel` encodes a pixmap for the
//! terminals that take Sixel and not Kitty, and [`window`] shows a pixmap
//! in a winit window. `headless` ticks and draws nothing, for a stage with
//! no display. [`Stage`] runs a game on one of them, or in a
//! [`crate::session::Session`] with a server, with the same loop, and
//! [`Frames`] runs the world of a teaching language on a stage.
//!
//! The feature `terminal` carries the terminal and the feature `window` the
//! window, and `open_native` needs both. An engine on wasi has neither, and
//! its [`Stage`] opens only with [`StageMode::Session`], with the page that
//! hosts the engine as the server.

// Most links above go to items that only a native build with both
// features has.
#![cfg_attr(
    not(all(feature = "terminal", feature = "window")),
    allow(rustdoc::broken_intra_doc_links)
)]

#[cfg(feature = "terminal")]
mod sixel;

#[cfg(all(feature = "window", target_os = "macos"))]
mod display_link;
mod driver;
mod frames;
#[cfg(any(feature = "terminal", feature = "window"))]
mod headless;
// A build with no display of its own keeps only `Sender`, for the type of
// `Display::sender`.
#[cfg_attr(not(any(feature = "terminal", feature = "window")), allow(dead_code))]
mod inbox;
#[cfg(all(feature = "terminal", unix))]
mod shm;
mod stage;
#[cfg(feature = "terminal")]
mod term_input;
#[cfg(feature = "terminal")]
mod term_query;
#[cfg(feature = "terminal")]
pub mod terminal;
#[cfg_attr(not(any(feature = "terminal", feature = "window")), allow(dead_code))]
mod tick_clock;
#[cfg(feature = "window")]
pub mod window;

#[cfg(all(feature = "terminal", feature = "window"))]
pub use driver::open_native;
pub use driver::{Display, NoGraphics, PresentError, TerminalError, WindowError};
pub use frames::{FrameEvent, Frames, ShowError};
pub use inbox::{Closed, Sender};
pub use stage::{
    MAX_LOCAL_PLAYERS, Stage, StageError, StageEvent, StageMode, TerminalGraphics, TerminalOptions,
};
#[cfg(feature = "terminal")]
pub use terminal::{PrintError, Printer, Terminal};
#[cfg(feature = "window")]
pub use window::Window;
