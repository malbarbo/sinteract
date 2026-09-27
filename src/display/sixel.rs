//! Sixel encoder, for a terminal that supports DEC Sixel and not the Kitty
//! protocol, such as Windows Terminal 1.22, mlterm, foot and mintty.
//!
//! `icy_sixel` does the encoding. An image of at most 256 colors keeps every
//! color in its palette, and the quantizer picks up to 256 for a larger one.
//! Sixel has no transparency that keeps the previous frame, so the encoder
//! takes an opaque pixmap. A renderer gives one when it draws over an opaque
//! background. [`PixmapRenderer::set_background`] sets that background.
//!
//! [`PixmapRenderer::set_background`]: crate::renderer::pixmap::PixmapRenderer::set_background

use std::io;

use tiny_skia::Pixmap;

/// Encodes a pixmap as Sixel, keeping its buffers from one frame to the
/// next. A caller that shows a stream of frames holds one encoder.
pub struct Encoder {
    inner: icy_sixel::SixelEncoder,
}

impl Encoder {
    /// An encoder with dithering off, since dithering scatters noise over
    /// the flat fills of a drawing, and with an exact palette, since the
    /// quantizer merges close colors that a drawing keeps apart.
    pub fn new() -> Self {
        let options = icy_sixel::EncodeOptions {
            diffusion: 0.0,
            ..Default::default()
        };
        Encoder {
            inner: icy_sixel::SixelEncoder::new()
                .with_options(options)
                .with_exact_palette(true),
        }
    }

    /// Append `pixmap` to `out` as Sixel, with the DCS introducer and the
    /// string terminator. The encoding fails when the width is over
    /// 1,000,000, when the height rounded up to a multiple of 6 is over
    /// 1,000,000, or when the width times that height is over 2^26. A
    /// failure leaves `out` as it was.
    ///
    /// `pixmap` should be opaque. A pixel with an alpha below 128 goes out
    /// transparent, and any other pixel that is not opaque draws its
    /// premultiplied color, which is darker than the color.
    pub fn encode(&mut self, pixmap: &Pixmap, out: &mut Vec<u8>) -> io::Result<()> {
        // An opaque premultiplied pixel holds its straight color, so the
        // bytes of the pixmap are the RGBA that the encoder reads.
        self.inner
            .encode_into(
                pixmap.data(),
                pixmap.width() as usize,
                pixmap.height() as usize,
                out,
            )
            .map_err(io::Error::other)
    }
}

impl Default for Encoder {
    /// The same as [`Encoder::new`].
    fn default() -> Self {
        Encoder::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_solid(w: u32, h: u32, rgba: [u8; 4]) -> Pixmap {
        let mut pm = Pixmap::new(w, h).unwrap();
        let color = tiny_skia::ColorU8::from_rgba(rgba[0], rgba[1], rgba[2], rgba[3]);
        pm.pixels_mut().fill(color.premultiply());
        pm
    }

    /// A pixmap whose colors fill the palette, unlike a flat fill.
    fn make_gradient(w: u32, h: u32) -> Pixmap {
        let mut pm = Pixmap::new(w, h).unwrap();
        for (i, pixel) in pm.pixels_mut().iter_mut().enumerate() {
            let (x, y) = (i as u32 % w, i as u32 / w);
            let r = (x * 255 / w.max(1)) as u8;
            let g = (y * 255 / h.max(1)) as u8;
            *pixel = tiny_skia::ColorU8::from_rgba(r, g, 128, 255).premultiply();
        }
        pm
    }

    #[test]
    fn encode_solid_red_pixmap_is_well_formed() {
        let bytes = sixel(Encoder::new(), &make_solid(8, 6, [255, 0, 0, 255]));
        assert!(bytes.starts_with(b"\x1bP"));
        assert!(bytes.ends_with(b"\x1b\\"));
        assert!(window_contains(&bytes, b";2;100;0;0"));
    }

    #[test]
    fn an_image_of_256_colors_keeps_every_color() {
        let bytes = sixel(Encoder::new(), &make_gradient(16, 16));
        let palette = bytes.windows(3).filter(|w| w == b";2;").count();
        assert_eq!(palette, 256);
    }

    #[test]
    fn a_translucent_pixel_draws_its_premultiplied_color() {
        let bytes = sixel(Encoder::new(), &make_solid(8, 6, [255, 0, 0, 128]));
        assert!(window_contains(&bytes, b";2;50;0;0"));
    }

    #[test]
    fn encode_appends_to_out() {
        let pm = make_solid(2, 2, [255, 0, 0, 255]);
        let mut out = b"prefix".to_vec();
        Encoder::new().encode(&pm, &mut out).unwrap();
        assert_eq!(
            out.strip_prefix(b"prefix"),
            Some(&*sixel(Encoder::new(), &pm))
        );
    }

    #[test]
    fn a_reused_encoder_matches_a_fresh_one() {
        let mut reused = Encoder::new();
        let mut out = Vec::new();
        // The sizes shrink and grow, so every buffer the encoder keeps has
        // to grow, shrink and be cleared between two frames. A gradient
        // fills the palette, which a flat fill leaves almost empty.
        for (w, h) in [(4, 4), (31, 23), (9, 7), (31, 23), (1, 1)] {
            let pm = make_gradient(w, h);
            out.clear();
            reused.encode(&pm, &mut out).unwrap();
            assert_eq!(out, sixel(Encoder::new(), &pm), "{w}x{h}");
        }
    }

    #[test]
    fn default_encodes_as_new() {
        // A flat fill encodes the same at any diffusion, so only a gradient
        // shows that `default` turns dithering off as `new` does.
        let pm = make_gradient(24, 18);
        assert_eq!(sixel(Encoder::default(), &pm), sixel(Encoder::new(), &pm));
    }

    /// The Sixel of `pm`.
    fn sixel(mut encoder: Encoder, pm: &Pixmap) -> Vec<u8> {
        let mut out = Vec::new();
        encoder.encode(pm, &mut out).unwrap();
        out
    }

    fn window_contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
