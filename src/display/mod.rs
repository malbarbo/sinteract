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
//! in a winit window. [`Stage`] runs a game on one of them, or in a
//! [`crate::session::Session`] with a server, with the same loop.
//!
//! The feature `terminal` carries the terminal and the feature `window` the
//! window, and `open_native` needs both. Only `sixel`, with the feature
//! `render`, builds on wasm32. The rest needs threads, a tty, a window or
//! platform FFI. In a browser the page, which hosts the engine, implements
//! `wait_event` itself.

// Most links above go to items that only a native build with both
// features has.
#![cfg_attr(
    not(all(feature = "terminal", feature = "window", not(target_arch = "wasm32"))),
    allow(rustdoc::broken_intra_doc_links)
)]

#[cfg(all(feature = "terminal", not(target_arch = "wasm32")))]
mod sixel;

#[cfg(all(feature = "window", target_os = "macos"))]
mod display_link;
#[cfg(all(
    any(feature = "terminal", feature = "window"),
    not(target_arch = "wasm32")
))]
mod driver;
#[cfg(all(
    any(feature = "terminal", feature = "window"),
    not(target_arch = "wasm32")
))]
mod inbox;
#[cfg(all(feature = "terminal", unix))]
mod shm;
#[cfg(all(feature = "terminal", feature = "window", not(target_arch = "wasm32")))]
mod stage;
#[cfg(all(feature = "terminal", not(target_arch = "wasm32")))]
mod term_input;
#[cfg(all(feature = "terminal", not(target_arch = "wasm32")))]
mod term_query;
#[cfg(all(feature = "terminal", not(target_arch = "wasm32")))]
pub mod terminal;
#[cfg(all(
    any(feature = "terminal", feature = "window"),
    not(target_arch = "wasm32")
))]
mod tick_clock;
#[cfg(all(feature = "window", not(target_arch = "wasm32")))]
pub mod window;

#[cfg(all(feature = "terminal", feature = "window", not(target_arch = "wasm32")))]
pub use driver::open_native;
#[cfg(all(
    any(feature = "terminal", feature = "window"),
    not(target_arch = "wasm32")
))]
pub use driver::{Display, NoGraphics, OpenError, PresentError};
#[cfg(all(
    any(feature = "terminal", feature = "window"),
    not(target_arch = "wasm32")
))]
pub use inbox::{Closed, Sender};
#[cfg(all(feature = "terminal", feature = "window", not(target_arch = "wasm32")))]
pub use stage::{Stage, StageError, StageEvent};
#[cfg(all(feature = "terminal", not(target_arch = "wasm32")))]
pub use terminal::{PrintError, Printer, Terminal, TerminalOptions};
#[cfg(all(feature = "window", not(target_arch = "wasm32")))]
pub use window::Window;
