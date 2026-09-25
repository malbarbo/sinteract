//! 2D graphics with terminal, window, PDF and SVG outputs.
//!
//! A front end builds a [`scene::Scene`] and a [`renderer::Renderer`]
//! replays it. `renderer::pixmap` rasterizes a scene to a tiny-skia
//! `Pixmap`, and `renderer::pdf` and [`renderer::svg`] write it as PDF
//! and SVG with the text as glyph outlines. [`text`] holds the embedded
//! Liberation families and measures and outlines glyphs, and [`wire`]
//! converts a scene and an event to and from Cap'n Proto. These build on
//! wasm32 too, except the system font lookup of `text`, which the
//! `native-fonts` feature carries.
//!
//! [`session`] turns the bytes that a server writes to an engine into the
//! events of the engine, with the rules of the protocol. It does no I/O of
//! its own, so it builds on wasm32 too. [`server`] holds the rules of a
//! room for the server, from the players to the messages for the engine,
//! also with no I/O.
//!
//! The feature `render` carries the pixmap and PDF renderers and the Sixel
//! encoder, and the displays turn it on. A server that only encodes and
//! decodes messages leaves it out, and keeps the scene, the text, the SVG
//! renderer and the codec.
//!
//! [`display`] shows a scene and reads the input back, through the
//! terminal or a winit window. Only the Sixel encoder of it builds on
//! wasm32. The rest needs threads, a tty, a window or platform FFI.

// A test that fails on an unwrap reports the failure well enough.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::indexing_slicing))]

pub mod display;
pub mod event;
mod outline;
pub mod renderer;
pub mod scene;
pub mod server;
pub mod session;
pub mod text;
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
