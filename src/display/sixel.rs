//! Sixel encoder, for a terminal that supports DEC Sixel and not the Kitty
//! protocol, such as Windows Terminal 1.22, mlterm, foot and mintty.
//!
//! `icy_sixel` does the encoding. It picks a palette of up to 256 colors for
//! each image, so an anti-aliased edge keeps its shades. Sixel has no
//! transparency that keeps the previous frame, so the caller passes a
//! background and the encoder composites every pixel over it.

use std::io;

use tiny_skia::{Pixmap, PremultipliedColorU8};

/// Encodes a pixmap as Sixel, keeping its buffers from one frame to the
/// next. A caller that shows a stream of frames holds one encoder.
pub struct Encoder {
    inner: icy_sixel::SixelEncoder,
    /// The pixels of the frame over the background, which `inner` reads.
    pixels: Vec<u8>,
}

impl Encoder {
    /// An encoder with dithering off, since dithering scatters noise over
    /// the flat fills of a drawing.
    pub fn new() -> Self {
        let options = icy_sixel::EncodeOptions {
            diffusion: 0.0,
            ..Default::default()
        };
        Encoder {
            inner: icy_sixel::SixelEncoder::new().with_options(options),
            pixels: Vec::new(),
        }
    }

    /// Append `pixmap` to `out` as Sixel, with the DCS introducer and the
    /// string terminator. Every pixel goes over `bg` first. An image over 64
    /// megapixels is an error, which leaves `out` as it was.
    pub fn encode(
        &mut self,
        pixmap: &Pixmap,
        bg: (u8, u8, u8),
        out: &mut Vec<u8>,
    ) -> io::Result<()> {
        self.pixels.clear();
        self.pixels
            .extend(pixmap.pixels().iter().flat_map(|&p| over(p, bg)));
        self.inner
            .encode_into(
                &self.pixels,
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

/// The opaque RGBA of `p` over `bg`. A premultiplied pixel already holds its
/// share of the color, so only the background needs the weight.
fn over(p: PremultipliedColorU8, bg: (u8, u8, u8)) -> [u8; 4] {
    let rest = 255 - u32::from(p.alpha());
    let mix = |fg: u8, bg: u8| (u32::from(fg) + (u32::from(bg) * rest + 127) / 255) as u8;
    [
        mix(p.red(), bg.0),
        mix(p.green(), bg.1),
        mix(p.blue(), bg.2),
        255,
    ]
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
    fn over_blends_with_the_background() {
        let half_red = tiny_skia::ColorU8::from_rgba(255, 0, 0, 128).premultiply();
        assert_eq!(over(half_red, (0, 0, 255)), [128, 0, 127, 255]);
        let clear = tiny_skia::ColorU8::from_rgba(0, 0, 0, 0).premultiply();
        assert_eq!(over(clear, (1, 2, 3)), [1, 2, 3, 255]);
    }

    #[test]
    fn encode_solid_red_pixmap_is_well_formed() {
        let bytes = sixel(Encoder::new(), &make_solid(8, 6, [255, 0, 0, 255]));
        assert!(bytes.starts_with(b"\x1bP"));
        assert!(bytes.ends_with(b"\x1b\\"));
        assert!(window_contains(&bytes, b";2;100;0;0"));
    }

    #[test]
    fn encode_transparent_uses_background() {
        let bytes = sixel(Encoder::new(), &make_solid(4, 4, [0, 0, 0, 0]));
        assert!(window_contains(&bytes, b";2;100;100;100"));
    }

    #[test]
    fn encode_appends_to_out() {
        let pm = make_solid(2, 2, [255, 0, 0, 255]);
        let mut out = b"prefix".to_vec();
        Encoder::new().encode(&pm, WHITE, &mut out).unwrap();
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
            reused.encode(&pm, WHITE, &mut out).unwrap();
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

    const WHITE: (u8, u8, u8) = (255, 255, 255);

    /// The Sixel of `pm` over white.
    fn sixel(mut encoder: Encoder, pm: &Pixmap) -> Vec<u8> {
        let mut out = Vec::new();
        encoder.encode(pm, WHITE, &mut out).unwrap();
        out
    }

    fn window_contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
