//! [`Renderer`] — the public contract every backend implements — and its
//! sealed internal companion `Paint`.
//!
//! A backend is a **persistent, reusable surface**: it owns its drawing buffer
//! and redraws frame after frame into it. The public surface is deliberately
//! tiny:
//!
//! ```text
//! Renderer::render(&mut self, &Scene)        -> Result<Output<'_>, Error>
//! Renderer::render_stream(&mut self, reader)  -> Result<Output<'_>, Error>
//! ```
//!
//! `render` borrows `&mut self`, so the buffer survives across calls (a redraw
//! loop reuses one allocation), and the returned [`Renderer::Output`] borrows
//! that buffer (`&Pixmap`, `&[u8]`) — no per-frame copy, and the borrow keeps
//! the surface locked until the frame is read. Allocation is the one fallible
//! step; it is paid where the backend sizes its surface and surfaced as a
//! [`Result`], never carried as a nullable field threaded through every draw.
//!
//! The draw primitives live on the sealed `Paint` trait, in a private-to-the-crate
//! module: a backend implements them and the crate's own scene/stream walkers
//! call them, but application code can neither name nor invoke them. So "draw
//! outside a render" or "draw before allocating" are not merely discouraged —
//! they cannot be written against this crate from outside. Sealing `Paint`
//! also seals `Renderer`, since every `Renderer` is a `Paint`.
//!
//! Coordinates are CSS pixels, y-down / top-left, matching the draw-list wire
//! format. A backend that needs another convention applies a transform when it
//! sizes its surface (see `pdf` and `terminal`).

use std::io::Read;

use crate::scene::Scene;

pub(crate) mod sealed {
    use crate::scene::{Bitmap, ClipPath, Element, Path, TextNode};

    /// The internal paint protocol: the primitives a backend provides and the
    /// crate's walkers drive. Declared `pub` but housed in a module that is
    /// private to the crate, so it is neither nameable nor implementable from
    /// outside `simage`.
    pub trait Paint: Sized {
        /// Draw one fully-built [`Path`]. Its verb/coord streams agree by
        /// construction, so [`Path::segments`](crate::scene::Path::segments)
        /// cannot desync.
        fn draw_path(&mut self, path: &Path);

        /// Draw one text node.
        fn draw_text(&mut self, text: &TextNode);

        /// Blit a previously-uploaded bitmap referenced by `bitmap.id`.
        /// Defaults to skipping it: no current backend can blit, and the
        /// diagnostic belongs to the host, which knows whether the frame it
        /// is about to show carries any (see
        /// [`Scene::has_bitmaps`](crate::scene::Scene::has_bitmaps)).
        fn draw_bitmap(&mut self, bitmap: &Bitmap) {
            let _ = bitmap;
        }

        /// Run `inside` with `clip` active, then pop the clip, and return
        /// whatever `inside` returned (so a fallible walk threads its `Result`
        /// straight out). Implementors pop in a drop-guard so the clip stack
        /// stays balanced even if `inside` unwinds.
        fn with_clip<T>(&mut self, clip: &ClipPath, inside: impl FnOnce(&mut Self) -> T) -> T;

        /// Walk a scene element slice, painting each node. `Clipped` subtrees
        /// recurse inside [`Self::with_clip`], which hands the closure an
        /// independent walk — a mis-behaving backend corrupts only its own
        /// subtree, never the outer traversal.
        fn paint_elements(&mut self, elements: &[Element]) {
            for node in elements {
                match node {
                    Element::Path(p) => self.draw_path(p),
                    Element::Text(t) => self.draw_text(t),
                    Element::Bitmap(b) => self.draw_bitmap(b),
                    Element::Clipped { clip, elements } => {
                        self.with_clip(clip, |c| c.paint_elements(elements));
                    }
                }
            }
        }
    }
}

/// A configured, reusable renderer that draws whole [`Scene`]s (or streamed
/// Cap'n Proto frames) into a surface it owns. See the [module docs](self) for
/// the lifecycle and why misuse is unrepresentable.
pub trait Renderer: sealed::Paint {
    /// What a rendered frame borrows out — e.g. `&Pixmap` or `&[u8]`. It
    /// borrows `&mut self`, so it cannot outlive the next `render`: read the
    /// frame before drawing the next one.
    type Output<'a>
    where
        Self: 'a;

    /// Render a whole [`Scene`] into the surface and borrow the result. The
    /// backend sizes and clears its buffer here (reallocating only on a size
    /// change), so allocation is the one fallible step — [`Err`] on failure.
    fn render(&mut self, scene: &Scene) -> Result<Self::Output<'_>, crate::wire::Error>;

    /// Decode exactly one Cap'n Proto `Frame` from `reader` and render it
    /// without materializing the [`Element`](crate::scene::Element) tree. The
    /// reader is walked lazily and `Clipped` subtrees recurse via
    /// the sealed `Paint::with_clip`. Non-`Frame` messages return
    /// [`wire::Error::WrongMessageKind`](crate::wire::Error::WrongMessageKind).
    fn render_stream(&mut self, reader: impl Read) -> Result<Self::Output<'_>, crate::wire::Error>;
}
