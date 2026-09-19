//! Sixel encoder, for a terminal that supports DEC Sixel and not the Kitty
//! protocol, such as Windows Terminal 1.22, mlterm, foot and mintty.
//!
//! `icy_sixel` does the encoding. It picks a palette of up to 256 colors for
//! each image, so an anti-aliased edge keeps its shades. Sixel has no
//! transparency that keeps the previous frame, so the caller passes a
//! background and the encoder composites every pixel over it.

use std::io;

use tiny_skia::{Pixmap, PremultipliedColorU8};

/// Encode `pixmap` as Sixel, with the DCS introducer and the string
/// terminator. The encoder composites every pixel over `bg` before it
/// quantizes. An image over 64 megapixels is an error.
pub fn encode(pixmap: &Pixmap, bg: (u8, u8, u8)) -> io::Result<Vec<u8>> {
    let rgba: Vec<u8> = pixmap.pixels().iter().flat_map(|&p| over(p, bg)).collect();
    // Dithering scatters noise over the flat fills of a drawing.
    let options = icy_sixel::EncodeOptions {
        diffusion: 0.0,
        ..Default::default()
    };
    icy_sixel::sixel_encode(
        &rgba,
        pixmap.width() as usize,
        pixmap.height() as usize,
        &options,
    )
    .map(String::into_bytes)
    .map_err(io::Error::other)
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

    #[test]
    fn over_blends_with_the_background() {
        let half_red = tiny_skia::ColorU8::from_rgba(255, 0, 0, 128).premultiply();
        assert_eq!(over(half_red, (0, 0, 255)), [128, 0, 127, 255]);
        let clear = tiny_skia::ColorU8::from_rgba(0, 0, 0, 0).premultiply();
        assert_eq!(over(clear, (1, 2, 3)), [1, 2, 3, 255]);
    }

    #[test]
    fn encode_solid_red_pixmap_is_well_formed() {
        let pm = make_solid(8, 6, [255, 0, 0, 255]);
        let bytes = encode(&pm, (255, 255, 255)).unwrap();
        assert!(bytes.starts_with(b"\x1bP"));
        assert!(bytes.ends_with(b"\x1b\\"));
        assert!(window_contains(&bytes, b";2;100;0;0"));
    }

    #[test]
    fn encode_transparent_uses_background() {
        let pm = make_solid(4, 4, [0, 0, 0, 0]);
        let bytes = encode(&pm, (255, 255, 255)).unwrap();
        assert!(window_contains(&bytes, b";2;100;100;100"));
    }

    fn window_contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
