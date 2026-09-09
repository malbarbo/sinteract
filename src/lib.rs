//! 2D graphics with terminal, window and PDF outputs.
//!
//! A front end builds a [`scene::Scene`] and a [`renderer::Renderer`]
//! replays it. [`pixmap`] rasterizes a scene to a tiny-skia `Pixmap`,
//! [`pdf`] writes it as PDF with the text as glyph outlines, [`text`] holds
//! the embedded Liberation families and measures and outlines glyphs, and
//! [`sixel`] encodes a `Pixmap` as DEC Sixel. These build on wasm32 too,
//! except the system font lookup of `text`.
//!
//! [`terminal`] shows a pixmap through Kitty, Sixel or half-blocks and runs
//! the animation loop with key polling, [`term_query`] probes what the
//! terminal supports, [`window`] shows a pixmap in a winit window, and
//! [`frontend`] drives the terminal, the window or stdio through one loop.
//! These need a tty, a window or platform FFI, so they are native only.

#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

pub mod event;
pub mod renderer;
pub mod scene;
pub mod wire;

// The generated bindings, one module per schema file. The generated code
// names them from the crate root, so they are mounted here and not inside
// `wire`. The VERB_* constants exist for the JS and Python hosts.
#[path = "wire/event_capnp.rs"]
#[allow(dead_code)]
mod event_capnp;
#[path = "wire/protocol_capnp.rs"]
#[allow(dead_code)]
mod protocol_capnp;
#[path = "wire/scene_capnp.rs"]
#[allow(dead_code)]
mod scene_capnp;

pub mod pdf;
mod pixel;
pub mod pixmap;
pub mod sixel;
pub mod stdio;
pub mod text;

#[cfg(not(target_arch = "wasm32"))]
pub mod frontend;
#[cfg(not(target_arch = "wasm32"))]
pub mod term_query;
#[cfg(not(target_arch = "wasm32"))]
pub mod terminal;
#[cfg(not(target_arch = "wasm32"))]
pub mod window;
