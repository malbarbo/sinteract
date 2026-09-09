//! Where a scene goes out and where the input comes back.
//!
//! [`Frontend`] is the driver a host (spython, sgleam) talks to. The
//! terminal, the window and stdio share one method surface, and the host
//! drives whichever it built through one loop.
//!
//! [`terminal`] shows a pixmap through Kitty, Sixel or half-blocks,
//! [`term_query`] probes what the terminal supports, [`sixel`] encodes a
//! pixmap for the terminals that take Sixel and not Kitty, and [`window`]
//! shows a pixmap in a winit window. [`stdio`] has no display at all. It
//! writes the frames and reads the events as Cap'n Proto messages, for a
//! server that runs the host as a subprocess.
//!
//! Only `sixel` and `stdio` build on wasm32. The rest needs a tty, a window
//! or platform FFI.

mod pixel;
pub mod sixel;
pub mod stdio;

#[cfg(not(target_arch = "wasm32"))]
mod driver;
#[cfg(not(target_arch = "wasm32"))]
pub mod term_query;
#[cfg(not(target_arch = "wasm32"))]
pub mod terminal;
#[cfg(not(target_arch = "wasm32"))]
pub mod window;

#[cfg(not(target_arch = "wasm32"))]
pub use driver::{Frontend, TerminalFrontend, WindowFrontend};
