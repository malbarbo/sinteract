//! `simage` — typed-IR 2D graphics with terminal and PDF outputs.
//!
//! Front ends build a [`scene::Scene`] via builder methods and replay it
//! through a [`renderer::Renderer`]. The crate ships two renderers:
//! `PixmapRenderer` (terminal raster, in [`terminal`]) and `PdfRenderer`
//! (PDF byte stream, in [`pdf`]).
//!
//! Pure-computation modules build everywhere, including `wasm32`, so a
//! browser client can measure text and emit PDF without a native host:
//! - [`text`] — embedded Liberation families, glyph measurement and outline
//!   (system-font lookup is native-only).
//! - [`pdf`] — render a [`scene::Scene`] directly to PDF (text as outlined paths).
//! - [`sixel`] — encode a `Pixmap` as DEC Sixel.
//!
//! Native-only modules need a tty, an OS window, or platform FFI:
//! - [`term_query`] — synchronous Kitty / Sixel capability probe.
//! - [`terminal`] — terminal renderer (Kitty / Sixel / half-blocks),
//!   animation lifecycle, key polling.
//! - [`window`] — native OS window renderer (winit + softbuffer), with the
//!   same lifecycle shape as `terminal`.
//! - [`frontend`] — the unified native driver over terminal / window / stdio.

#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

pub mod event;
pub mod renderer;
pub mod scene;
pub mod wire;

// VERB_* constants are emitted for non-Rust hosts (JS, Python); Rust uses
// the `scene::SegmentKind` enum directly.
#[path = "wire/frame_capnp.rs"]
#[allow(dead_code)]
mod frame_capnp;

pub mod pdf;
mod pixel;
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
