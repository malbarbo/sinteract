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

// Cap'n Proto generates code that references `crate::frame_capnp::*`, so the
// generated module must live at the crate root. The path lives under `wire/`
// for organisational reasons; only `wire::*` should consume it.
#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    dead_code,
    unused_imports,
    unused_qualifications,
    unsafe_op_in_unsafe_fn,
    mismatched_lifetime_syntaxes,
    non_camel_case_types,
    non_snake_case
)]
#[path = "wire/frame_capnp.rs"]
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
