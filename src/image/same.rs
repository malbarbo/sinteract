//! The comparison of two images by their pixels.

use super::{CanvasTooLarge, Image};
use crate::asset::ImageFormat;
use crate::renderer::pixmap::render_to_pixmap;
use crate::scene::{self, Element, Scene};

impl Image {
    /// Returns `true` if `self` and `other` have the same canvas and draw
    /// the same pixels on it, `false` otherwise. Two images that `==` takes
    /// as equal are the same, and two that it takes as different may still
    /// be the same, such as a square and the square turned by 90 degrees.
    /// The canvas is the canvas of [`Image::to_repl_svg`], so an outline on
    /// the right or the bottom edge counts.
    ///
    /// The pixmap decodes only a PNG and draws a mark for any other image,
    /// so the bitmaps of such images also have to show the same images in
    /// the same order. Returns `CanvasTooLarge` when the canvas does not
    /// fit in a pixmap.
    pub fn same(&self, other: &Image) -> Result<bool, CanvasTooLarge> {
        if self == other {
            return Ok(true);
        }
        // A canvas too large for a float differs from one that fits.
        let (w, h) = match (self.canvas_size(), other.canvas_size()) {
            (Ok(a), Ok(b)) if a == b => a,
            (Err(CanvasTooLarge), Err(CanvasTooLarge)) => return Err(CanvasTooLarge),
            _ => return Ok(false),
        };
        // The canvas of to_repl_scene.
        let (w, h) = (w + 1.0, h + 1.0);
        let (a, b) = (self.scene(w, h), other.scene(w, h));
        if not_png(a.elements()) != not_png(b.elements()) {
            return Ok(false);
        }
        Ok(pixels(&a)? == pixels(&b)?)
    }
}

/// The images that are not a PNG in the bitmaps of `elements`, in drawing
/// order.
fn not_png(elements: &[Element]) -> Vec<&scene::Image> {
    let mut images = Vec::new();
    let mut pending = vec![elements.iter()];
    while let Some(elements) = pending.last_mut() {
        let Some(element) = elements.next() else {
            pending.pop();
            continue;
        };
        match element {
            Element::Bitmap(bitmap) => {
                let head = crate::asset::image_head(bitmap.image.blob());
                if head.is_ok_and(|head| head.format != ImageFormat::Png) {
                    images.push(&bitmap.image);
                }
            }
            Element::Clipped { elements, .. } | Element::Layer { elements, .. } => {
                pending.push(elements.iter());
            }
            Element::Path(_) | Element::Text(_) => {}
        }
    }
    images
}

/// The pixels of `scene`, or `CanvasTooLarge` when they do not fit in a pixmap.
fn pixels(scene: &Scene) -> Result<Vec<u8>, CanvasTooLarge> {
    let pixmap = render_to_pixmap(scene, 1.0).map_err(|_| CanvasTooLarge)?;
    Ok(pixmap.take())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{Attribute, Color, Finite, Length, Point, Style, YPlace};

    fn length(v: f64) -> Length {
        Length::new(v).unwrap()
    }

    fn rectangle(w: f64, h: f64, color: Color) -> Image {
        Image::rectangle(length(w), length(h), Style::new([Attribute::Fill(color)]))
    }

    fn red() -> Color {
        Color::rgb(255, 0, 0)
    }

    fn bitmap(mime: &str, blob: &[u8]) -> Image {
        Image::bitmap_data_uri(&crate::asset::data_uri_of_mime(mime, blob)).unwrap()
    }

    #[test]
    fn a_turned_rectangle_is_the_same_as_its_pair() {
        let wide = rectangle(20.0, 10.0, red());
        let turned = wide.rotate(Finite::new(90.0).unwrap()).unwrap();
        assert_ne!(turned, rectangle(10.0, 20.0, red()));
        assert_eq!(turned.same(&rectangle(10.0, 20.0, red())), Ok(true));
    }

    #[test]
    fn two_pieces_are_the_same_as_the_whole() {
        let half = || rectangle(10.0, 10.0, red());
        let pair = Image::beside(YPlace::Middle, [half(), half()]).unwrap();
        assert_eq!(pair.same(&rectangle(20.0, 10.0, red())), Ok(true));
    }

    #[test]
    fn a_different_color_or_size_is_not_the_same() {
        let blue = Color::rgb(0, 0, 255);
        let a = rectangle(10.0, 10.0, red());
        assert_eq!(a.same(&rectangle(10.0, 10.0, blue)), Ok(false));
        assert_eq!(a.same(&rectangle(11.0, 10.0, red())), Ok(false));
    }

    /// A BMP of one pixel with `blue` in its blue channel and `dpi` in the
    /// header, which changes the blob and not the pixels.
    fn bmp(blue: u8, dpi: u32) -> Image {
        let mut blob = b"BM".to_vec();
        for v in [58u32, 0, 54, 40, 1, 1] {
            blob.extend(v.to_le_bytes());
        }
        blob.extend([1, 0, 24, 0]);
        for v in [0u32, 4, dpi, dpi, 0, 0] {
            blob.extend(v.to_le_bytes());
        }
        blob.extend([blue, 0, 0, 0]);
        bitmap("image/bmp", &blob)
    }

    #[test]
    fn an_image_that_is_not_a_png_compares_by_its_blob() {
        let zero = Finite::new(0.0).unwrap();
        let whole = bmp(0, 0).crop(Point::new(zero, zero), length(1.0), length(1.0));
        assert_eq!(whole.same(&bmp(0, 0)), Ok(true));
        assert_eq!(whole.same(&bmp(0, 2835)), Ok(false));
        assert_eq!(bmp(0, 0).same(&bmp(255, 0)), Ok(false));
    }

    #[test]
    fn a_canvas_too_large_for_a_pixmap_is_too_large() {
        let (a, b) = (rectangle(1e5, 1e5, red()), rectangle(1e5, 1e5, Color::NONE));
        assert_eq!(a.same(&b), Err(CanvasTooLarge));
        assert_eq!(a.same(&a.clone()), Ok(true));
    }

    #[test]
    fn a_canvas_too_large_is_not_the_same_as_one_that_fits() {
        let (small, huge) = (rectangle(10.0, 10.0, red()), rectangle(1e39, 10.0, red()));
        assert_eq!(huge.canvas_size(), Err(CanvasTooLarge));
        assert_eq!(small.same(&huge), Ok(false));
        assert_eq!(huge.same(&small), Ok(false));
    }
}
