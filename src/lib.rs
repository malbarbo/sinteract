//! 2D graphics with terminal, window, PDF and SVG outputs.
//!
//! A front end builds a [`scene::Scene`] and a [`renderer::Renderer`]
//! replays it. [`renderer::pixmap`] rasterizes a scene to a tiny-skia
//! `Pixmap`, and [`renderer::pdf`] and [`renderer::svg`] write it as PDF
//! and SVG with the text as glyph outlines. [`text`] holds the embedded
//! Liberation families and measures and outlines glyphs, and [`wire`]
//! converts a scene and an event to and from Cap'n Proto. These build on wasm32 too, except the system font
//! lookup of `text`, which the `native-fonts` feature carries.
//!
//! [`display`] shows a scene and reads the input back, through the
//! terminal, a winit window or stdin and stdout. Only the Sixel encoder of
//! it builds on wasm32. The rest needs threads, a tty, a window or platform
//! FFI.

pub mod display;
pub mod event;
mod outline;
pub mod renderer;
pub mod scene;
pub mod text;
pub mod wire;

// The generated bindings, one module per schema file. The generated code
// names them from the crate root, so they are mounted here and not inside
// `wire`. The VERB_* constants exist for the readers that are not Rust.
#[path = "wire/event_capnp.rs"]
#[allow(dead_code)]
mod event_capnp;
#[path = "wire/protocol_capnp.rs"]
#[allow(dead_code)]
mod protocol_capnp;
#[path = "wire/scene_capnp.rs"]
#[allow(dead_code)]
mod scene_capnp;

// A wrong path in the example of the README fails the doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
