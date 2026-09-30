//! 2D graphics with terminal, window, PDF and SVG outputs.
//!
//! A front end builds a [`scene::Scene`] and a [`renderer::Renderer`]
//! replays it. `renderer::pixmap` rasterizes a scene to a tiny-skia
//! `Pixmap`, and `renderer::pdf` and [`renderer::svg`] write it as PDF
//! and SVG with the text as glyph outlines. [`text`] holds the embedded
//! Sinteract families and measures and outlines glyphs. These build on
//! wasm32 too, except the system font lookup of `text`, which the
//! `native-fonts` feature carries. On wasm32, `renderer::canvas` draws a
//! scene on an HTML canvas.
//!
//! [`asset`] holds the limits on the images of a room.
//!
//! [`session`] turns the bytes that a server writes to an engine into the
//! events of the engine, with the rules of the protocol. It does no I/O of
//! its own, so it builds on wasm32 too. [`server`] holds the rules of a
//! room for the server, from the players to the messages for the engine,
//! also with no I/O. [`view`] reads the frames that a server sends to a
//! view and encodes its input. The three convert to and from Cap'n Proto
//! with the codec of `wire`, whose [`wire::Error`] they report.
//!
//! The feature `render` carries the pixmap and PDF renderers, and the
//! displays turn it on. A server that only encodes and
//! decodes messages leaves it out, and keeps the scene, the text, the SVG
//! renderer and the codec.
//!
//! `display` shows a scene and reads the input back, through the terminal
//! or a winit window. It needs the feature `terminal` or `window`, and it
//! does not build on wasm32, because it needs threads, a tty, a window or
//! platform FFI.

// A test that fails on an unwrap or a panic reports the failure well
// enough.
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)
)]

pub mod asset;
#[cfg(all(
    any(feature = "terminal", feature = "window"),
    not(target_arch = "wasm32")
))]
pub mod display;
pub mod event;
mod outline;
pub mod renderer;
pub mod scene;
pub mod server;
pub mod session;
pub mod text;
pub mod view;
pub mod wire;

// The generated bindings, one module per schema file. The generated code
// names them from the crate root, so they are mounted here and not inside
// `wire`. The VERB_* constants exist for the readers that are not Rust.
#[path = "wire/event_capnp.rs"]
#[allow(dead_code, clippy::unwrap_used, clippy::indexing_slicing)]
mod event_capnp;
#[path = "wire/protocol_capnp.rs"]
#[allow(dead_code, clippy::unwrap_used, clippy::indexing_slicing)]
mod protocol_capnp;
#[path = "wire/scene_capnp.rs"]
#[allow(dead_code, clippy::unwrap_used, clippy::indexing_slicing)]
mod scene_capnp;

// A wrong path in the example of the README fails the doctests. The example
// shows the scene in the terminal.
#[cfg(all(doctest, feature = "terminal"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
