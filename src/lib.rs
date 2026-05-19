//! `simage` — typed-IR 2D graphics with terminal and PDF outputs.
//!
//! Front ends build a [`ir::DrawList`] via builder methods and replay it
//! through a [`sink::DrawSink`]. The crate ships two sinks: `PixmapSink`
//! (terminal raster, in [`terminal`]) and `PdfSink` (PDF byte stream,
//! in [`pdf`]).
//!
//! Native targets compile the full pipeline:
//! - [`text`] — Liberation Sans embedded, glyph measurement and outline.
//! - [`pdf`] — render a [`ir::DrawList`] directly to PDF (text as outlined paths).
//! - [`sixel`] — encode a `Pixmap` as DEC Sixel.
//! - [`term_query`] — synchronous Kitty / Sixel capability probe.
//! - [`terminal`] — terminal renderer (Kitty / Sixel / half-blocks),
//!   animation lifecycle, key polling.
//! - [`window`] — native OS window renderer (winit + softbuffer), with the
//!   same lifecycle shape as `terminal`.
//!
//! On `wasm32`, this crate compiles to (approximately) nothing — terminal
//! and PDF pipelines do not apply, and the host (e.g. a browser frontend)
//! is expected to render via its own canvas.

#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

pub mod event;
pub mod ir;
pub mod sink;
pub mod wire;

// VERB_* constants are emitted for non-Rust hosts (JS, Python); Rust uses
// `ir::verb` directly.
#[path = "wire/frame_capnp.rs"]
#[allow(dead_code)]
mod frame_capnp;

#[cfg(not(target_arch = "wasm32"))]
pub mod frontend;
#[cfg(not(target_arch = "wasm32"))]
pub mod pdf;
#[cfg(not(target_arch = "wasm32"))]
pub mod sixel;
#[cfg(not(target_arch = "wasm32"))]
pub mod stdio;
#[cfg(not(target_arch = "wasm32"))]
pub mod term_query;
#[cfg(not(target_arch = "wasm32"))]
pub mod terminal;
#[cfg(not(target_arch = "wasm32"))]
pub mod text;
#[cfg(not(target_arch = "wasm32"))]
pub mod window;
