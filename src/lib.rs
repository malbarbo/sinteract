//! `simage` — typed-IR 2D graphics with terminal and PDF outputs.
//!
//! Front ends build a [`ir::DrawList`] via builder methods and replay it
//! through a [`sink::DrawSink`]. The crate ships two sinks: `PixmapSink`
//! (terminal raster, in [`world_term`]) and `PdfSink` (PDF byte stream,
//! in [`pdf`]).
//!
//! Native targets compile the full pipeline:
//! - [`text`] — Liberation Sans embedded, glyph measurement and outline.
//! - [`pdf`] — render a [`ir::DrawList`] directly to PDF (text as outlined paths).
//! - [`sixel`] — encode a `Pixmap` as DEC Sixel.
//! - [`term_query`] — synchronous Kitty / Sixel capability probe.
//! - [`world_term`] — terminal renderer (Kitty / Sixel / half-blocks),
//!   animation lifecycle, key polling.
//!
//! On `wasm32`, this crate compiles to (approximately) nothing — terminal
//! and PDF pipelines do not apply, and the host (e.g. a browser frontend)
//! is expected to render via its own canvas.

#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

pub mod ir;
pub mod sink;

#[cfg(not(target_arch = "wasm32"))]
pub mod pdf;
#[cfg(not(target_arch = "wasm32"))]
pub mod sixel;
#[cfg(not(target_arch = "wasm32"))]
pub mod term_query;
#[cfg(not(target_arch = "wasm32"))]
pub mod text;
#[cfg(not(target_arch = "wasm32"))]
pub mod world_term;

#[cfg(not(target_arch = "wasm32"))]
pub use world_term::{
    enter_animation, exit_animation, install_panic_hook, kitty_supported, poll_key_event,
    show_svg, text_blocks_supported,
};

#[cfg(not(target_arch = "wasm32"))]
pub use sixel::sixel_supported;
