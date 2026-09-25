//! The [`Renderer`] trait and its sealed half, `Canvas`.
//!
//! A renderer owns its surface and redraws into it frame after frame.
//! `render` takes `&mut self`, so a redraw loop reuses one allocation, and
//! the [`Renderer::Output`] borrows the surface, so the frame is read before
//! the next render and never copied. Sizing the surface is the only step
//! that can fail, and it returns a [`Result`].
//!
//! The primitives live on `Canvas`, in a module private to the crate, so a
//! backend implements them and the scene and stream walkers call them, but
//! an application cannot name them. `render` and `render_stream` are
//! provided, so the order of sizing, painting and closing a frame is the
//! trait's and not each backend's. The one public method, `output`, only
//! borrows the last frame.
//!
//! Coordinates are CSS pixels, with y down and the origin at the top left,
//! as on the wire. A backend with another convention applies a transform
//! when it sizes its surface.

#[cfg(feature = "render")]
pub mod pdf;
#[cfg(feature = "render")]
pub mod pixmap;
pub mod svg;

use std::io::Read;

use crate::scene::Scene;

/// A renderer that draws a [`Scene`] or a streamed frame into a surface it
/// owns. The [module docs](self) describe the lifecycle.
pub trait Renderer: sealed::Canvas {
    /// What a rendered frame borrows out, such as `&Pixmap` or `&[u8]`. It
    /// borrows `&mut self`, so the frame is read before the next `render`.
    type Output<'a>
    where
        Self: 'a;

    /// Borrow the last rendered frame.
    fn output(&self) -> Self::Output<'_>;

    /// Render a [`Scene`] and borrow the result. Fails only if sizing the
    /// surface fails.
    fn render(&mut self, scene: &Scene) -> Result<Self::Output<'_>, AllocError> {
        self.ensure_size(scene.width(), scene.height())?;
        self.paint_elements(scene.elements());
        self.end_frame();
        Ok(self.output())
    }

    /// Decode one scene that [`wire::scene::encode`](crate::wire::scene::encode)
    /// wrote from `reader` and render it without building the
    /// [`Element`](crate::scene::Element) tree.
    fn render_stream(
        &mut self,
        reader: impl Read,
    ) -> Result<Self::Output<'_>, crate::wire::StreamError> {
        crate::wire::stream_frame(self, reader)?;
        self.end_frame();
        Ok(self.output())
    }
}

/// Sizing a surface failed. It is the only way a render fails, and only a
/// backend that allocates a surface returns it. The pdf backend never does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocError {
    pub width: u32,
    pub height: u32,
}

impl std::fmt::Display for AllocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let AllocError { width, height } = self;
        write!(f, "could not allocate a {width}×{height} surface")
    }
}

impl std::error::Error for AllocError {}

/// An asset that does not decode. The decoder stays private, so the error
/// does not depend on the renderer that decodes.
#[derive(Debug)]
pub struct AssetError(Box<dyn std::error::Error + Send + Sync>);

impl std::fmt::Display for AssetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cannot decode the image: {}", self.0)
    }
}

impl std::error::Error for AssetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.0)
    }
}

pub(crate) mod sealed {
    use crate::scene::{Bitmap, ClipPath, Element, Path, Text};

    /// The primitives a backend provides and the walkers of the crate call.
    /// `pub` in a private module, so nothing outside the crate can name or
    /// implement it.
    pub trait Canvas: Sized {
        /// Size the surface for a frame of `width` by `height` and clear it,
        /// reallocating only when the size changed.
        fn ensure_size(&mut self, width: f32, height: f32) -> Result<(), super::AllocError>;

        fn draw_path(&mut self, path: &Path);

        fn draw_text(&mut self, text: &Text);

        /// Draw the asset with id `bitmap.id`. Only the pixmap does, so the
        /// default skips it.
        fn draw_bitmap(&mut self, bitmap: &Bitmap) {
            let _ = bitmap;
        }

        /// Run after the frame is painted. The pdf assembles its document
        /// here.
        fn end_frame(&mut self) {}

        /// Run `inside` with `clip` active, pop the clip, and return what
        /// `inside` returned, so a fallible walk passes its `Result` out. A
        /// backend pops in a drop guard, so the clip stack stays balanced if
        /// `inside` unwinds.
        fn with_clip<T>(&mut self, clip: &ClipPath, inside: impl FnOnce(&mut Self) -> T) -> T;

        /// Paint the elements in order. A `Clipped` subtree recurses inside
        /// [`Self::with_clip`].
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

/// The miter limit of a text stroke. A glyph is a closed smooth contour, so
/// the cap and the join do not show, and the limit is the PDF default, which
/// the pdf backend leaves unset.
pub(crate) const TEXT_MITER_LIMIT: f32 = 10.0;

/// A side of a frame in the units a backend draws in, never below 1. A
/// surface of no pixels, a page of no points and a viewBox of no width take
/// no drawing, so the empty frame, which is one of no width or no height,
/// still gets one unit.
pub(crate) fn frame_side(size: f32) -> f32 {
    size.max(1.0)
}

/// Runs `restore` on `canvas` when dropped, so the `with_clip` of a backend
/// undoes its clip even when `inside` unwinds.
pub(crate) struct RestoreOnDrop<'a, C> {
    pub(crate) canvas: &'a mut C,
    pub(crate) restore: fn(&mut C),
}

impl<C> Drop for RestoreOnDrop<'_, C> {
    fn drop(&mut self) {
        (self.restore)(self.canvas);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::scene::{Path, PathStyle};

    /// A `w × h` rectangle at `(x, y)`.
    pub(crate) fn rect(style: PathStyle, x: f32, y: f32, w: f32, h: f32) -> Path {
        Path::builder(style, x, y)
            .line_to(x + w, y)
            .line_to(x + w, y + h)
            .line_to(x, y + h)
            .build()
    }
}
