//! Native text measurement and outline extraction backed by an embedded
//! Liberation Sans font.
//!
//! Used by:
//!   - `wasm_ffi::measure_text_*` on native targets, so the SVG fallback gets
//!     the same metrics as the rasterizer.
//!   - The CLI terminal renderer (`terminal::rasterize_draw_list`) which
//!     calls [`outline`] to fill glyph paths into a `tiny_skia::PathBuilder`.
//!
//! The measurements return offsets relative to the *box center* — text spans
//! (-width/2, -height/2) to (width/2, height/2) in box-local coordinates. The
//! caller then composes `translate(cx, cy) * rotate(angle) * scale(sx, sy)`
//! to place the text in world coordinates.
//!
//! WASM targets do not use this module: the JS frontend measures text via
//! `OffscreenCanvas` and supplies metrics through the env imports.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::OnceLock;
use ttf_parser::{Face, GlyphId};

const FONT_BYTES: &[u8] = include_bytes!("../fonts/LiberationSans-Regular.ttf");

fn face() -> &'static Face<'static> {
    static FACE: OnceLock<Face<'static>> = OnceLock::new();
    FACE.get_or_init(|| Face::parse(FONT_BYTES, 0).expect("embedded font is valid"))
}

/// Receiver for outline path commands. Coordinates are in the same space as
/// the values returned by the `measure_*` functions (y increases downward).
pub trait OutlineBuilder {
    fn move_to(&mut self, x: f32, y: f32);
    fn line_to(&mut self, x: f32, y: f32);
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32);
    fn cubic_to(&mut self, cx1: f32, cy1: f32, cx2: f32, cy2: f32, x: f32, y: f32);
    fn close(&mut self);
}

/// Total horizontal advance of `text` rendered at `size_px`.
pub fn measure_width(text: &str, size_px: i32) -> f64 {
    if text.is_empty() || size_px <= 0 {
        return 0.0;
    }
    let f = face();
    let scale = f64::from(size_px) / f64::from(f.units_per_em());
    let mut total: f64 = 0.0;
    for c in text.chars() {
        let gid = f.glyph_index(c).unwrap_or(GlyphId(0));
        total += f64::from(f.glyph_hor_advance(gid).unwrap_or(0));
    }
    total * scale
}

/// Box height (ascent + descent in absolute terms) at `size_px`. Independent
/// of the text content — derived from font metrics.
pub fn measure_height(_text: &str, size_px: i32) -> f64 {
    if size_px <= 0 {
        return 0.0;
    }
    let f = face();
    let scale = f64::from(size_px) / f64::from(f.units_per_em());
    let h = f64::from(f.ascender()) - f64::from(f.descender());
    h * scale
}

/// X offset to the alphabetic baseline START in box-local coords. Negative,
/// equal to -width/2, so text-anchor=start places the text starting at the
/// box's left edge.
pub fn measure_x_offset(text: &str, size_px: i32) -> f64 {
    -measure_width(text, size_px) / 2.0
}

/// Y offset of the alphabetic baseline in box-local coords. Centers the text
/// vertically: top of caps lands at -height/2, descenders reach +height/2.
pub fn measure_y_offset(_text: &str, size_px: i32) -> f64 {
    if size_px <= 0 {
        return 0.0;
    }
    let f = face();
    let scale = f64::from(size_px) / f64::from(f.units_per_em());
    // ttf-parser convention: ascender > 0 (above baseline), descender < 0
    // (below baseline). The midpoint of [descender, ascender] gives the
    // baseline y in a coord system where y=0 is the box center.
    (f64::from(f.ascender()) + f64::from(f.descender())) / 2.0 * scale
}

/// Walk the outline of every glyph in `text` and emit path commands to
/// `out`. Coordinates are in box-local space (y down, origin at box center)
/// matching the values returned by the `measure_*` functions.
pub fn outline(text: &str, size_px: i32, out: &mut dyn OutlineBuilder) {
    if text.is_empty() || size_px <= 0 {
        return;
    }
    let f = face();
    let scale = f64::from(size_px) / f64::from(f.units_per_em());
    let baseline_y = measure_y_offset(text, size_px) as f32;
    let start_x = measure_x_offset(text, size_px) as f32;

    let mut pen_x: f64 = 0.0;
    for c in text.chars() {
        let gid = f.glyph_index(c).unwrap_or(GlyphId(0));
        let mut adapter = OutlineAdapter {
            out,
            scale: scale as f32,
            origin_x: start_x + (pen_x * scale) as f32,
            baseline_y,
        };
        // outline_glyph silently returns None for blank glyphs (e.g. space).
        let _ = f.outline_glyph(gid, &mut adapter);
        pen_x += f64::from(f.glyph_hor_advance(gid).unwrap_or(0));
    }
}

struct OutlineAdapter<'a> {
    out: &'a mut dyn OutlineBuilder,
    scale: f32,
    origin_x: f32,
    baseline_y: f32,
}

impl<'a> OutlineAdapter<'a> {
    fn map(&self, x: f32, y: f32) -> (f32, f32) {
        // ttf-parser emits font-unit coords with y up; we flip to y-down at
        // the baseline and offset by origin_x.
        (
            self.origin_x + x * self.scale,
            self.baseline_y - y * self.scale,
        )
    }
}

impl<'a> ttf_parser::OutlineBuilder for OutlineAdapter<'a> {
    fn move_to(&mut self, x: f32, y: f32) {
        let (mx, my) = self.map(x, y);
        self.out.move_to(mx, my);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let (mx, my) = self.map(x, y);
        self.out.line_to(mx, my);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (cx, cy) = self.map(x1, y1);
        let (ex, ey) = self.map(x, y);
        self.out.quad_to(cx, cy, ex, ey);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (c1x, c1y) = self.map(x1, y1);
        let (c2x, c2y) = self.map(x2, y2);
        let (ex, ey) = self.map(x, y);
        self.out.cubic_to(c1x, c1y, c2x, c2y, ex, ey);
    }
    fn close(&mut self) {
        self.out.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CountingBuilder {
        moves: u32,
        lines: u32,
        quads: u32,
        cubics: u32,
        closes: u32,
    }

    impl OutlineBuilder for CountingBuilder {
        fn move_to(&mut self, _x: f32, _y: f32) {
            self.moves += 1;
        }
        fn line_to(&mut self, _x: f32, _y: f32) {
            self.lines += 1;
        }
        fn quad_to(&mut self, _cx: f32, _cy: f32, _x: f32, _y: f32) {
            self.quads += 1;
        }
        fn cubic_to(&mut self, _: f32, _: f32, _: f32, _: f32, _: f32, _: f32) {
            self.cubics += 1;
        }
        fn close(&mut self) {
            self.closes += 1;
        }
    }

    #[test]
    fn measure_width_empty_is_zero() {
        assert_eq!(measure_width("", 20), 0.0);
    }

    #[test]
    fn measure_width_grows_with_size() {
        let small = measure_width("hello", 10);
        let big = measure_width("hello", 20);
        assert!(big > small * 1.5, "{big} should be roughly 2x {small}");
    }

    #[test]
    fn measure_width_grows_with_chars() {
        let one = measure_width("h", 20);
        let many = measure_width("hhhh", 20);
        assert!(many > one * 3.5, "{many} should be roughly 4x {one}");
    }

    #[test]
    fn measure_height_uses_font_metrics() {
        let h = measure_height("anything", 20);
        // Liberation Sans at 20px: ascender 1854, descender -434, em 2048
        // → (1854 - (-434)) * 20 / 2048 ≈ 22.34
        assert!(h > 18.0 && h < 26.0, "unexpected height: {h}");
    }

    #[test]
    fn x_offset_centers_text() {
        let w = measure_width("hi", 20);
        let x = measure_x_offset("hi", 20);
        assert!((x + w / 2.0).abs() < 1e-6);
    }

    #[test]
    fn y_offset_is_within_box() {
        let h = measure_height("hi", 20);
        let y = measure_y_offset("hi", 20);
        // baseline lies inside the (-h/2, h/2) box
        assert!(y > -h / 2.0 && y < h / 2.0);
    }

    #[test]
    fn outline_emits_some_commands_for_letters() {
        let mut b = CountingBuilder {
            moves: 0,
            lines: 0,
            quads: 0,
            cubics: 0,
            closes: 0,
        };
        outline("Ag", 30, &mut b);
        // 'A' has straight strokes (lines), 'g' has curves (quads in TT outlines).
        assert!(b.moves > 0, "no moves emitted");
        assert!(b.lines > 0 || b.quads > 0, "no draw segments emitted");
        assert!(b.closes > 0, "outline did not close");
    }

    #[test]
    fn outline_empty_string_emits_nothing() {
        let mut b = CountingBuilder {
            moves: 0,
            lines: 0,
            quads: 0,
            cubics: 0,
            closes: 0,
        };
        outline("", 30, &mut b);
        assert_eq!(b.moves, 0);
        assert_eq!(b.lines, 0);
        assert_eq!(b.closes, 0);
    }

    #[test]
    fn outline_space_only_advances_pen_no_glyphs() {
        let mut b = CountingBuilder {
            moves: 0,
            lines: 0,
            quads: 0,
            cubics: 0,
            closes: 0,
        };
        outline("   ", 30, &mut b);
        assert_eq!(b.moves, 0);
        assert_eq!(b.lines, 0);
        // Width should still be > 0 (spaces have advance).
        assert!(measure_width("   ", 30) > 0.0);
    }

    #[test]
    fn portuguese_chars_have_glyphs() {
        let s = "ção";
        let w = measure_width(s, 20);
        assert!(w > 0.0);
        let mut b = CountingBuilder {
            moves: 0,
            lines: 0,
            quads: 0,
            cubics: 0,
            closes: 0,
        };
        outline(s, 30, &mut b);
        assert!(b.moves > 0);
    }
}
