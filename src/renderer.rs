//! [`Renderer`] — the trait every renderer implements.
//!
//! [`crate::scene::Scene::render`] walks a draw-list once and dispatches
//! each element through this trait. Each renderer (raster, PDF, future SVG)
//! is responsible only for its own backend; the scene owns the format and
//! the SVG-arc → cubic conversion, so the trait surface stays small.
//!
//! Coordinates are in CSS pixels with y-down / top-left origin, matching the
//! draw-list wire format. Backends that need a different convention apply a
//! transform once at the start (see `pdf` and `terminal`).
//!
//! ## Scoped, not bracketed
//!
//! The trait is generic (`Sized`, dispatched on `&mut Self`) and therefore
//! **not** object-safe. Every operation that used to be a begin/end or
//! push/pop pair is now a **scoped closure**: [`Renderer::frame`] runs a body
//! between canvas setup and teardown, and [`Renderer::with_clip`] runs an
//! `inside` body with a clip active. A backend cannot leave a frame or clip
//! unbalanced because there is no separate "end" to forget — and because
//! [`crate::scene::Element`] nests the clipped subtree, `with_clip` hands the
//! body an independent walk, so a backend that skips or repeats it corrupts
//! only its own subtree, never the outer traversal.
//!
//! ## Entry points
//!
//! * [`Renderer::render_scene`] — replay a fully-built
//!   [`crate::scene::Scene`].
//! * [`Renderer::render_scene_stream`] — decode one Cap'n Proto `Frame` from
//!   a [`std::io::Read`] source and replay it without materializing the
//!   [`crate::scene::Element`] tree.
//!
//! ## Primitives (implementor surface)
//!
//! * [`Renderer::frame`] — set up the canvas, run `body`, tear down. A backend
//!   that buffers output should run teardown in a drop-guard so it fires even
//!   if `body` unwinds (relevant under `catch_unwind` in server mode).
//! * [`Renderer::with_clip`] — push a clip, run `inside` with it active, pop it
//!   (again via a drop-guard).
//! * [`Renderer::draw_path`] / [`Renderer::draw_text`] /
//!   [`Renderer::draw_bitmap`] — leaves.

use std::io::Read;

use crate::scene::{Bitmap, ClipPath, Element, Path, Scene, TextNode};

pub trait Renderer: Sized {
    /// Set up the canvas at `width × height`, run `body` with the renderer
    /// ready to draw, then finalize. Implementors that buffer output (PDF,
    /// [`tiny_skia::Pixmap`]) should perform teardown in a drop-guard so it
    /// runs even if `body` unwinds.
    fn frame(&mut self, width: f32, height: f32, body: impl FnMut(&mut Self));

    /// Draw one fully-built [`Path`]. The path's verb/coord streams agree by
    /// construction, so [`Path::segments`] cannot desync.
    fn draw_path(&mut self, path: &Path);

    /// Draw one text node.
    fn draw_text(&mut self, text: &TextNode);

    /// Blit a previously-uploaded bitmap referenced by `bitmap.id`. The
    /// current renderers (terminal, pdf) skip with a warning; only the future
    /// canvas / WebGL frontends honor this.
    fn draw_bitmap(&mut self, bitmap: &Bitmap);

    /// Push `clip` onto the clip stack, run `inside` with it active, then pop
    /// it. Implementors pop in a drop-guard so the stack stays balanced even
    /// if `inside` unwinds. The clip path uses the same verb/coord encoding as
    /// a regular path; sub-paths are treated as implicitly closed, and
    /// `clip.fill_rule` decides the inside.
    fn with_clip(&mut self, clip: &ClipPath, inside: impl FnMut(&mut Self));

    /// Replay a complete [`Scene`] in a single [`Self::frame`] envelope. The
    /// canonical entry point — [`Scene::render`](crate::scene::Scene::render)
    /// is a thin alias.
    fn render_scene(&mut self, scene: &Scene) {
        self.frame(scene.width, scene.height, |r| {
            render_elements(r, &scene.elements);
        });
    }

    /// Decode one Cap'n Proto `Frame` message from `reader` and replay it
    /// through the renderer. The Cap'n Proto reader is walked lazily — the
    /// [`Element`] list never materializes into a `Vec<Element>`, and
    /// `Clipped` subtrees recurse via [`Self::with_clip`]. Each path is decoded
    /// into a scratch [`Path`] (bounded by one path at a time) and handed to
    /// [`Self::draw_path`].
    ///
    /// Expects exactly one `Message::Frame` payload; other variants
    /// (`Asset`, `Event`, `SessionClose`) return
    /// [`wire::Error::WrongMessageKind`](crate::wire::Error::WrongMessageKind).
    /// Callers with mixed message streams should peek the kind themselves or
    /// use [`wire::decode`](crate::wire::decode).
    fn render_scene_stream<R: Read>(&mut self, reader: R) -> Result<(), crate::wire::Error> {
        crate::wire::render_scene_stream(self, reader)
    }
}

/// Walk a slice of [`Element`]s, dispatching each through `r`. `Clipped`
/// subtrees recurse inside [`Renderer::with_clip`], which receives the subtree
/// as an independent slice so a mis-behaving backend cannot desync the outer
/// walk.
fn render_elements<R: Renderer>(r: &mut R, elements: &[Element]) {
    for node in elements {
        match node {
            Element::Path(p) => r.draw_path(p),
            Element::Text(t) => r.draw_text(t),
            Element::Bitmap(b) => r.draw_bitmap(b),
            Element::Clipped { clip, elements } => {
                r.with_clip(clip, |r2| render_elements(r2, elements));
            }
        }
    }
}
