//! Sixel encoder for terminals that don't support the Kitty graphics protocol
//! but do support DEC Sixel (Windows Terminal ≥ 1.22, mlterm, foot, mintty,
//! recent xterm with `--enable-sixel-graphics`, …).
//!
//! Strategy:
//! - Quantize each pixel to the 216-color "web safe" palette (R/G/B each
//!   rounded to 6 levels: 0, 51, 102, 153, 204, 255). Cheap, deterministic,
//!   and good enough for the flat-color SVG output of `spython.image`.
//! - Build a per-image palette from the unique quantized colors actually used,
//!   so the emitted Sixel only declares as many colors as the image needs.
//! - Encode in 6-row bands, run-length-encoding sixel character runs of 4 or
//!   more so a uniform background does not blow up the payload.
//!
//! The output is wrapped in `\x1bPq … \x1b\\` (DCS … ST). Background pixels
//! of the input pixmap should already be opaque — Sixel cannot represent
//! transparent pixels in a way that preserves the previous frame, so the
//! caller composites against a solid background before encoding.

use tiny_skia::Pixmap;

/// Whether the terminal supports DEC Sixel. Asks the terminal directly via
/// a Device Attributes (DA1) query (see `term_query`); env-based heuristics
/// are unreliable across SSH and terminal multiplexers, and the probe is
/// cached so the cost is paid at most once per process.
pub fn sixel_supported() -> bool {
    crate::term_query::graphics_caps().sixel
}

/// Encode `pixmap` as a Sixel byte sequence, including the DCS introducer and
/// String Terminator. The pixmap's pixels are demultiplied and composited
/// against `bg` (RGB) before quantization.
pub fn encode(pixmap: &Pixmap, bg: (u8, u8, u8)) -> Vec<u8> {
    let w = pixmap.width() as usize;
    let h = pixmap.height() as usize;

    // Step 1 — quantize every pixel to the 216-color web palette and build a
    // compact palette of the colors actually used.
    let (palette, indices) = quantize(pixmap, bg);

    // Step 2 — emit the Sixel stream.
    let mut out: Vec<u8> = Vec::with_capacity(w * h / 4);
    out.extend_from_slice(b"\x1bPq");

    // Raster attributes: pixel aspect ratio numerator;denominator;width;height
    // Helps terminals reserve the correct cell area before painting.
    write_u32(&mut out, b'"', 1);
    out.push(b';');
    push_dec(&mut out, 1);
    out.push(b';');
    push_dec(&mut out, w as u64);
    out.push(b';');
    push_dec(&mut out, h as u64);

    // Declare every palette color: #N;2;Pr;Pg;Pb where Pr/Pg/Pb are 0..100.
    for (idx, &(r, g, b)) in palette.iter().enumerate() {
        out.push(b'#');
        push_dec(&mut out, idx as u64);
        out.extend_from_slice(b";2;");
        push_dec(&mut out, scale_to_100(r));
        out.push(b';');
        push_dec(&mut out, scale_to_100(g));
        out.push(b';');
        push_dec(&mut out, scale_to_100(b));
    }

    // Pre-compute which colors appear in each 6-row band — most images have
    // wide flat-color regions, so for each band we only iterate over the
    // colors actually present.
    let bands = h.div_ceil(6);
    let mut band_buf: Vec<u8> = Vec::with_capacity(w);
    for band in 0..bands {
        let y0 = band * 6;
        let y_end = (y0 + 6).min(h);
        let band_rows = y_end - y0;

        // Find colors present in this band.
        let mut used = vec![false; palette.len()];
        for y in y0..y_end {
            let row = &indices[y * w..y * w + w];
            for &idx in row {
                used[idx as usize] = true;
            }
        }

        let mut first_color = true;
        for (color, &is_used) in used.iter().enumerate() {
            if !is_used {
                continue;
            }
            if !first_color {
                out.push(b'$');
            }
            first_color = false;
            out.push(b'#');
            push_dec(&mut out, color as u64);

            band_buf.clear();
            for x in 0..w {
                let mut bits: u8 = 0;
                for i in 0..band_rows {
                    let idx = indices[(y0 + i) * w + x];
                    if idx as usize == color {
                        bits |= 1 << i;
                    }
                }
                band_buf.push(b'?' + bits);
            }
            // Run-length-encode runs of identical sixel chars.
            rle_emit(&band_buf, &mut out);
        }
        // End of band — '-' moves down one band; omit on the very last band
        // so we don't leave an extra blank line below the image.
        if band + 1 < bands {
            out.push(b'-');
        }
    }

    out.extend_from_slice(b"\x1b\\");
    out
}

fn write_u32(out: &mut Vec<u8>, prefix: u8, value: u64) {
    out.push(prefix);
    push_dec(out, value);
}

fn push_dec(out: &mut Vec<u8>, value: u64) {
    let mut buf = [0u8; 20];
    let mut n = value;
    let mut i = buf.len();
    if n == 0 {
        out.push(b'0');
        return;
    }
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    out.extend_from_slice(&buf[i..]);
}

/// Emit sixel chars with RLE (`!N<char>`) for runs of length ≥ 4.
fn rle_emit(chars: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let mut run = 1;
        while i + run < chars.len() && chars[i + run] == c {
            run += 1;
        }
        if run >= 4 {
            out.push(b'!');
            push_dec(out, run as u64);
            out.push(c);
        } else {
            for _ in 0..run {
                out.push(c);
            }
        }
        i += run;
    }
}

fn scale_to_100(channel_0_255: u8) -> u64 {
    // Round to nearest, not floor, so 255 → 100 and 51 → 20.
    ((u32::from(channel_0_255) * 100 + 127) / 255) as u64
}

/// Quantize the pixmap to the 216-color web palette, building a compact
/// per-image palette of the colors actually used.
fn quantize(pixmap: &Pixmap, bg: (u8, u8, u8)) -> (Vec<(u8, u8, u8)>, Vec<u8>) {
    let w = pixmap.width() as usize;
    let h = pixmap.height() as usize;
    // 6×6×6 lookup → palette index, sentinel 0xFF for "not seen yet".
    let mut lut = [0xFFu8; 216];
    let mut palette: Vec<(u8, u8, u8)> = Vec::new();
    let mut indices: Vec<u8> = Vec::with_capacity(w * h);

    let pixels = pixmap.pixels();
    for y in 0..h {
        for x in 0..w {
            let p = pixels[y * w + x];
            // Straight color, then composite over bg; `composite` folds in the
            // a==0 (→bg) and a==255 (→straight) ends.
            let (sr, sg, sb) = crate::pixel::unpremultiply(p);
            let (r, g, b) = composite(sr, sg, sb, p.alpha(), bg);

            let qr = quant6(r);
            let qg = quant6(g);
            let qb = quant6(b);
            let lut_idx = qr * 36 + qg * 6 + qb;
            let pal_idx = if lut[lut_idx] == 0xFF {
                let next = palette.len();
                debug_assert!(next < 216);
                lut[lut_idx] = next as u8;
                palette.push((level6(qr), level6(qg), level6(qb)));
                next as u8
            } else {
                lut[lut_idx]
            };
            indices.push(pal_idx);
        }
    }
    (palette, indices)
}

fn composite(r: u8, g: u8, b: u8, a: u8, bg: (u8, u8, u8)) -> (u8, u8, u8) {
    let af = f32::from(a) / 255.0;
    let mix = |fg: u8, bg: u8| -> u8 {
        (f32::from(fg) * af + f32::from(bg) * (1.0 - af))
            .round()
            .clamp(0.0, 255.0) as u8
    };
    (mix(r, bg.0), mix(g, bg.1), mix(b, bg.2))
}

/// Map 0..255 to the closest of 6 levels (0, 51, 102, 153, 204, 255).
fn quant6(v: u8) -> usize {
    // Round each channel; thresholds at 25, 76, 127, 178, 229.
    ((u32::from(v) * 5 + 127) / 255) as usize
}

fn level6(idx: usize) -> u8 {
    debug_assert!(idx < 6);
    [0u8, 51, 102, 153, 204, 255][idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_solid(w: u32, h: u32, rgba: [u8; 4]) -> Pixmap {
        let mut pm = Pixmap::new(w, h).unwrap();
        let mut color = tiny_skia::ColorU8::from_rgba(rgba[0], rgba[1], rgba[2], rgba[3]);
        for px in pm.pixels_mut() {
            *px = color.premultiply();
        }
        let _ = &mut color;
        pm
    }

    #[test]
    fn quant6_endpoints() {
        assert_eq!(quant6(0), 0);
        assert_eq!(quant6(51), 1);
        assert_eq!(quant6(102), 2);
        assert_eq!(quant6(153), 3);
        assert_eq!(quant6(204), 4);
        assert_eq!(quant6(255), 5);
    }

    #[test]
    fn scale_to_100_rounds() {
        assert_eq!(scale_to_100(0), 0);
        assert_eq!(scale_to_100(255), 100);
        assert_eq!(scale_to_100(127), 50);
    }

    #[test]
    fn rle_threshold() {
        let mut out = Vec::new();
        rle_emit(b"???", &mut out);
        assert_eq!(out, b"???");
        out.clear();
        rle_emit(b"????", &mut out);
        assert_eq!(out, b"!4?");
        out.clear();
        rle_emit(b"???A?????", &mut out);
        assert_eq!(out, b"???A!5?");
    }

    #[test]
    fn encode_solid_red_pixmap_is_well_formed() {
        let pm = make_solid(8, 6, [255, 0, 0, 255]);
        let bytes = encode(&pm, (255, 255, 255));
        assert!(bytes.starts_with(b"\x1bPq"));
        assert!(bytes.ends_with(b"\x1b\\"));
        // Single color → palette index 0, declared as #0;2;100;0;0
        assert!(window_contains(&bytes, b"#0;2;100;0;0"));
        // RLE: 8 columns of identical sixels → "!8" + char.
        assert!(window_contains(&bytes, b"!8"));
    }

    #[test]
    fn encode_transparent_uses_background() {
        let pm = make_solid(4, 4, [0, 0, 0, 0]);
        let bytes = encode(&pm, (255, 255, 255));
        // Background white → palette declares "100;100;100".
        assert!(window_contains(&bytes, b";100;100;100"));
    }

    fn window_contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
