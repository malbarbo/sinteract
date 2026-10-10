//! The [`Renderer`] trait and its sealed half, `Canvas`.
//!
//! A renderer owns its surface and redraws into it frame after frame.
//! `render` takes `&mut self`, so a redraw loop reuses one allocation, and
//! the [`Renderer::Output`] borrows the surface, so the frame is read before
//! the next render and never copied. Sizing the surface is the only step
//! that can fail, and it returns a [`Result`].
//!
//! The primitives live on `Canvas`, in a module private to the crate, so a
//! backend implements them and the scene walker calls them, but an
//! application cannot name them. `render` is provided, so the order of
//! sizing, painting and closing a frame is the trait's and not each
//! backend's. The one public method, `output`, only
//! borrows the last frame.
//!
//! Coordinates are CSS pixels, with y down and the origin at the top left,
//! as on the wire. A backend with another convention applies a transform
//! when it sizes its surface.

#[cfg(all(feature = "render", target_arch = "wasm32"))]
pub mod canvas;
#[cfg(feature = "render")]
pub mod pdf;
#[cfg(feature = "pixmap")]
pub mod pixmap;
pub mod svg;

use crate::outline::PathSink;
use crate::scene::{Rgba, Scene};

/// A renderer that draws a [`Scene`] into a surface it owns. The
/// [module docs](self) describe the lifecycle.
pub trait Renderer: sealed::Canvas<Self::Error> {
    /// Why sizing the surface failed. A backend that allocates no surface
    /// never fails, and its error is [`Infallible`](std::convert::Infallible).
    type Error;

    /// What a rendered frame borrows out, such as `&Pixmap` or `&[u8]`. It
    /// borrows `&mut self`, so the frame is read before the next `render`.
    type Output<'a>
    where
        Self: 'a;

    /// Borrow the last rendered frame.
    fn output(&self) -> Self::Output<'_>;

    /// Render a [`Scene`] and borrow the result. Fails only if sizing the
    /// surface fails.
    fn render(&mut self, scene: &Scene) -> Result<Self::Output<'_>, Self::Error> {
        self.ensure_size(scene.width(), scene.height())?;
        self.paint_elements(scene.elements());
        self.end_frame();
        Ok(self.output())
    }
}

/// Sizing the surface of the pixmap failed.
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

pub(crate) mod sealed {
    use crate::scene::{Bitmap, ClipPath, Element, Path, Text};

    /// The primitives a backend provides and the scene walker calls.
    /// `pub` in a private module, so nothing outside the crate can name or
    /// implement it.
    /// `E` is [`super::Renderer::Error`].
    pub trait Canvas<E>: Sized {
        /// Size the surface for a frame of `width` by `height` and clear it,
        /// reallocating only when the size changed.
        fn ensure_size(&mut self, width: f32, height: f32) -> Result<(), E>;

        fn draw_path(&mut self, path: &Path);

        fn draw_text(&mut self, text: &Text);

        fn draw_bitmap(&mut self, bitmap: &Bitmap);

        /// Run after the frame is painted. The pdf assembles its document
        /// here.
        fn end_frame(&mut self) {}

        /// Run `inside` with `clip` active, pop the clip, and return what
        /// `inside` returned, so a fallible walk passes its `Result` out. A
        /// backend pops in a drop guard, so the clip stack stays balanced if
        /// `inside` unwinds.
        fn with_clip<T>(&mut self, clip: &ClipPath, inside: impl FnOnce(&mut Self) -> T) -> T;

        /// Run `inside` into a transparent layer, draw the layer with
        /// `opacity`, and return what `inside` returned. `opacity` is above 0
        /// and below 1. A backend draws the layer in a drop guard, as
        /// [`Self::with_clip`] pops its clip.
        fn with_layer<T>(&mut self, opacity: f32, inside: impl FnOnce(&mut Self) -> T) -> T;

        /// Paint the elements in order. A `Clipped` or a `Layer` subtree
        /// recurses inside [`Self::with_clip`] or [`Self::with_layer`].
        fn paint_elements(&mut self, elements: &[Element]) {
            for node in elements {
                match node {
                    Element::Path(p) => self.draw_path(p),
                    Element::Text(t) => self.draw_text(t),
                    Element::Bitmap(b) => self.draw_bitmap(b),
                    Element::Clipped { clip, elements } => {
                        self.with_clip(clip, |c| c.paint_elements(elements));
                    }
                    Element::Layer { opacity, elements } => {
                        self.with_layer(*opacity, |c| c.paint_elements(elements));
                    }
                }
            }
        }
    }
}

/// The fill and the stroke of the box that stands for a bitmap whose image
/// does not decode, with a cross from corner to corner.
pub(crate) const MISSING_FILL: Rgba = Rgba {
    r: 200,
    g: 200,
    b: 200,
    a: 255,
};
pub(crate) const MISSING_STROKE: Rgba = Rgba {
    r: 200,
    g: 0,
    b: 0,
    a: 255,
};

/// Feed `out` the outline of the box that stands for a missing image under
/// `t`, the unit square that the bitmap covers.
pub(crate) fn missing_box(t: [f32; 6], out: &mut impl PathSink) {
    let [p0, p1, p2, p3] = unit_square(t);
    out.move_to(p0.0, p0.1);
    for (x, y) in [p1, p2, p3] {
        out.line_to(x, y);
    }
    out.close();
}

/// Feed `out` the cross from corner to corner of the box of
/// [`missing_box`].
pub(crate) fn missing_cross(t: [f32; 6], out: &mut impl PathSink) {
    let [p0, p1, p2, p3] = unit_square(t);
    out.move_to(p0.0, p0.1);
    out.line_to(p2.0, p2.1);
    out.move_to(p1.0, p1.1);
    out.line_to(p3.0, p3.1);
}

/// The corners of the unit square centred on the origin under `t`, in
/// order around it. A bitmap covers this square.
fn unit_square(t: [f32; 6]) -> [(f32, f32); 4] {
    let [a, b, c, d, e, f] = t;
    [(-0.5, -0.5), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)]
        .map(|(x, y)| (a * x + c * y + e, b * x + d * y + f))
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
