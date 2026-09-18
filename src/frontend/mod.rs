//! Where a scene goes out and where the input comes back.
//!
//! [`Frontend`] is what a host (spython, sgleam) drives. [`Terminal`],
//! [`Window`] and [`Stdio`] implement it, and the host runs one loop over
//! whichever it opened.
//!
//! [`terminal`] shows a pixmap through Kitty, Sixel or half-blocks,
//! [`term_query`] probes what the terminal supports, [`sixel`] encodes a
//! pixmap for the terminals that take Sixel and not Kitty, and [`window`]
//! shows a pixmap in a winit window. [`stdio`] has no display at all. It
//! writes the frames and reads the events as Cap'n Proto messages, for a
//! server that runs the host as a subprocess.
//!
//! Only `sixel` builds on wasm32. The rest needs threads, a tty, a window
//! or platform FFI. In a browser the host implements `wait_event` itself.

mod pixel;
pub mod sixel;

#[cfg(not(target_arch = "wasm32"))]
mod driver;
#[cfg(not(target_arch = "wasm32"))]
mod inbox;
#[cfg(not(target_arch = "wasm32"))]
pub mod stdio;
#[cfg(not(target_arch = "wasm32"))]
pub mod term_query;
#[cfg(not(target_arch = "wasm32"))]
pub mod terminal;
#[cfg(not(target_arch = "wasm32"))]
pub mod window;

#[cfg(not(target_arch = "wasm32"))]
pub use driver::{Frontend, open_native};
#[cfg(not(target_arch = "wasm32"))]
pub use inbox::{Closed, Sender};
#[cfg(not(target_arch = "wasm32"))]
pub use stdio::Stdio;
#[cfg(not(target_arch = "wasm32"))]
pub use terminal::Terminal;
#[cfg(not(target_arch = "wasm32"))]
pub use window::Window;
