//! Where a scene goes out and where the input comes back.
//!
//! [`Display`] is what an engine (spython, sgleam) drives. [`Terminal`],
//! [`Window`] and [`Stdio`] implement it, and the engine runs one loop over
//! whichever it opened.
//!
//! [`terminal`] shows a pixmap through Kitty, Sixel or half-blocks,
//! `term_query` probes what the terminal supports, `shm` hands a Kitty image
//! to a terminal on the same machine, `vt_input` reads the
//! keys, with or without the keyboard protocol of Kitty or the
//! win32-input-mode of Windows Terminal, [`sixel`] encodes a pixmap for the
//! terminals that take Sixel and not Kitty, and [`window`] shows a pixmap
//! in a winit window. [`stdio`] shows nothing. It writes the frames and
//! reads the events as Cap'n Proto messages, for a server that runs the
//! engine as a subprocess.
//!
//! The feature `terminal` carries the terminal and the feature `window` the
//! window, and `open_native` needs both. Only `sixel` builds on wasm32. The
//! rest needs threads, a tty, a window or platform FFI. In a browser the page, which
//! hosts the engine, implements `wait_event` itself.

// Most links above go to items that only a native build with both
// features has.
#![cfg_attr(
    not(all(feature = "terminal", feature = "window", not(target_arch = "wasm32"))),
    allow(rustdoc::broken_intra_doc_links)
)]

pub mod sixel;

#[cfg(not(target_arch = "wasm32"))]
mod driver;
#[cfg(not(target_arch = "wasm32"))]
mod inbox;
#[cfg(all(feature = "terminal", unix))]
mod shm;
#[cfg(not(target_arch = "wasm32"))]
pub mod stdio;
#[cfg(all(feature = "terminal", not(target_arch = "wasm32")))]
mod term_query;
#[cfg(all(feature = "terminal", not(target_arch = "wasm32")))]
pub mod terminal;
#[cfg(all(feature = "terminal", not(target_arch = "wasm32")))]
mod vt_input;
#[cfg(all(feature = "window", not(target_arch = "wasm32")))]
pub mod window;

#[cfg(all(feature = "terminal", feature = "window", not(target_arch = "wasm32")))]
pub use driver::open_native;
#[cfg(not(target_arch = "wasm32"))]
pub use driver::{Display, NoGraphics, OpenError, PresentError};
#[cfg(not(target_arch = "wasm32"))]
pub use inbox::{Closed, Sender};
#[cfg(not(target_arch = "wasm32"))]
pub use stdio::Stdio;
#[cfg(all(feature = "terminal", not(target_arch = "wasm32")))]
pub use terminal::{PrintError, Printer, Terminal, TerminalOptions};
#[cfg(all(feature = "window", not(target_arch = "wasm32")))]
pub use window::Window;
