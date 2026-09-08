//! Sixel encoder, for a terminal that supports DEC Sixel and not the Kitty
//! protocol, such as Windows Terminal 1.22, mlterm, foot and mintty.
//!
//! The encoder quantizes each pixel to the 216 colors of the web palette,
//! six levels per channel, which is cheap and enough for a flat-color
//! drawing. The palette in the output holds only the colors the image uses.
//! The image goes out in bands of six rows, and a run of four or more equal
//! sixels is run-length encoded, so a flat background stays small. Sixel has
//! no transparency that keeps the previous frame, so the caller passes a
//! background and the encoder composites every pixel over it.

use tiny_skia::Pixmap;

/// Returns `true` if the terminal supports DEC Sixel, `false` otherwise. The
/// answer comes from a DA1 query to the terminal, because the environment
/// variables are wrong over ssh and under a multiplexer. The probe runs once
/// per process and needs a tty, so the function is native only.
#[cfg(not(target_arch = "wasm32"))]
pub fn sixel_supported() -> bool {
    crate::term_query::graphics_caps().sixel
}

/// Encode `pixmap` as Sixel, with the DCS introducer and the string
/// terminator. The encoder composites every pixel over `bg` before it
/// quantizes.
pub fn encode(pixmap: &Pixmap, bg: (u8, u8, u8)) -> Vec<u8> {
    let w = pixmap.width() as usize;
    let h = pixmap.height() as usize;

    let (palette, indices) = quantize(pixmap, bg);

    let mut out: Vec<u8> = Vec::with_capacity(w * h / 4);
    out.extend_from_slice(b"\x1bPq");

    // Raster attributes, aspect ratio 1:1 and the size, so the terminal
    // reserves the cell area before it paints.
    write_u32(&mut out, b'"', 1);
    out.push(b';');
    push_dec(&mut out, 1);
    out.push(b';');
    push_dec(&mut out, w as u64);
    out.push(b';');
    push_dec(&mut out, h as u64);

    // #N;2;R;G;B with the channels in 0..100.
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

    // Most images have flat regions, so a band visits only the colors it
    // holds.
    let bands = h.div_ceil(6);
    let mut band_buf: Vec<u8> = Vec::with_capacity(w);
    for band in 0..bands {
        let y0 = band * 6;
        let y_end = (y0 + 6).min(h);
        let band_rows = y_end - y0;

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
            rle_emit(&band_buf, &mut out);
        }
        // '-' moves to the next band. The last band has none, or the image
        // would end with a blank line.
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

/// Emit the sixels, with `!N<char>` for a run of four or more.
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
    // Round, so 255 gives 100 and 51 gives 20.
    ((u32::from(channel_0_255) * 100 + 127) / 255) as u64
}

/// Quantize the pixmap to the web palette. Returns the palette of the colors
/// the image uses and the palette index of each pixel.
fn quantize(pixmap: &Pixmap, bg: (u8, u8, u8)) -> (Vec<(u8, u8, u8)>, Vec<u8>) {
    let w = pixmap.width() as usize;
    let h = pixmap.height() as usize;
    // Index into the 6×6×6 cube. 0xFF marks a color not seen yet.
    let mut lut = [0xFFu8; 216];
    let mut palette: Vec<(u8, u8, u8)> = Vec::new();
    let mut indices: Vec<u8> = Vec::with_capacity(w * h);

    let pixels = pixmap.pixels();
    for y in 0..h {
        for x in 0..w {
            let p = pixels[y * w + x];
            // composite covers alpha 0 and 255, so there is no special case.
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
    // Thresholds at 25, 76, 127, 178, 229.
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
        // One color, palette index 0, declared as #0;2;100;0;0.
        assert!(window_contains(&bytes, b"#0;2;100;0;0"));
        // Eight equal columns give !8 and the sixel.
        assert!(window_contains(&bytes, b"!8"));
    }

    #[test]
    fn encode_transparent_uses_background() {
        let pm = make_solid(4, 4, [0, 0, 0, 0]);
        let bytes = encode(&pm, (255, 255, 255));
        // A white background declares 100;100;100.
        assert!(window_contains(&bytes, b";100;100;100"));
    }

    fn window_contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
