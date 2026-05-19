//! [`DrawSink`] — the trait every renderer implements.
//!
//! The parser in [`crate::parse`] walks a draw-list once and dispatches each
//! command to a sink. Each sink (raster, PDF, future SVG) is responsible only
//! for its own backend; the parser owns the format and the SVG-arc → cubic
//! conversion, so the trait surface stays small.
//!
//! Coordinates are in CSS pixels with y-down / top-left origin, matching the
//! draw-list wire format. Backends that need a different convention apply a
//! transform once at the start (see `pdf` and `terminal`).

use crate::ir::{BitmapNode, ClipPath, PathStyle, TextNode};

pub trait DrawSink {
    /// Called once with the canvas dimensions before any draw command. May be
    /// used by the sink to allocate output buffers (e.g. a [`tiny_skia::Pixmap`]).
    fn begin(&mut self, width: f32, height: f32);

    /// Open a new path with the given style. Subsequent `*_to` calls extend
    /// the path; [`Self::path_end`] commits it.
    fn path_begin(&mut self, style: &PathStyle);
    fn move_to(&mut self, x: f32, y: f32);
    fn line_to(&mut self, x: f32, y: f32);
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32);
    fn cubic_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32);
    fn path_end(&mut self);

    /// Push a clip path onto the clip stack. The path uses the same
    /// verb/coord encoding as a regular path; sub-paths are treated as
    /// implicitly closed, and `clip.fill_rule` decides the inside.
    fn clip_push(&mut self, clip: &ClipPath);
    fn clip_pop(&mut self);

    fn text(&mut self, node: &TextNode);

    /// Blit a previously-uploaded bitmap referenced by `node.id`. The current
    /// renderers (terminal, pdf) skip with a warning; only the future canvas
    /// / WebGL frontends honor this.
    fn bitmap(&mut self, node: &BitmapNode);

    /// Called once after the last command. Sinks that buffer output (PDF,
    /// [`tiny_skia::Pixmap`]) flush here.
    fn end(&mut self) {}
}
