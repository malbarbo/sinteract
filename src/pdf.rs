//! Render a [`crate::ir::DrawList`] to a PDF byte stream. Native-only;
//! the WASM build does not link against `pdf-writer`.
//!
//! The draw list is replayed via [`crate::ir::DrawList::play_into`]; this
//! module implements [`PdfSink`] which translates each command into PDF
//! content-stream operators.
//!
//! Coordinate system: PDF native space is y-up with the origin at the
//! bottom-left, while the draw list uses y-down with the origin at the
//! top-left. We apply a global transform `cm 1 0 0 -1 0 H` once at the top of
//! the content stream so all subsequent coordinates can be emitted verbatim
//! from the draw list.
//!
//! Text: glyphs are emitted as filled paths via [`crate::text::outline`] —
//! this matches the raster path and avoids embedding a font in the PDF. The
//! trade-off is that text is not selectable; for v1 of the export this is
//! acceptable and removes a class of measurement bugs (the rsvg-based pipeline
//! used the system fontconfig font, not Liberation Sans, and positions did
//! not line up).

#![cfg(not(target_arch = "wasm32"))]

use std::collections::BTreeMap;

use pdf_writer::types::{LineCapStyle, LineJoinStyle};
use pdf_writer::{Content, Finish, Name, Pdf, Rect, Ref};

use crate::ir::{ClipBox, FillRule, FontItalic, LineCap, LineJoin, PathStyle, Rgba, TextNode};
use crate::sink::DrawSink;

const ITALIC_SHEAR: f32 = 0.207;

/// Conversion from CSS pixels (the implicit unit of draw-list coordinates,
/// matching the canvas/SVG renderer) to PDF points: 1 px = 1/96 in,
/// 1 pt = 1/72 in, so 1 px = 72/96 = 0.75 pt. Without this factor the same
/// `rectangle(W, H)` would render ~33% larger in the PDF than on screen.
const PX_TO_PT: f32 = 72.0 / 96.0;

/// Quantize an alpha value into a stable integer key (range 0..=1000) so that
/// the `BTreeMap` of allocated ExtGState resources de-duplicates near-equal
/// alphas without floating-point key comparisons.
fn alpha_key(a: f32) -> u16 {
    (a.clamp(0.0, 1.0) * 1000.0).round() as u16
}

fn alpha_value(key: u16) -> f32 {
    f32::from(key) / 1000.0
}

#[derive(Clone, Copy)]
enum PathOp {
    Move(f32, f32),
    Line(f32, f32),
    Cubic(f32, f32, f32, f32, f32, f32),
    Close,
}

struct PendingPath {
    style: PathStyle,
    ops: Vec<PathOp>,
    last_point: Option<(f32, f32)>,
}

struct PdfSink {
    width: f32,
    height: f32,
    content: Content,
    /// (fill_alpha_key, stroke_alpha_key) -> graphics-state index.
    gstates: BTreeMap<(u16, u16), u32>,
    pending: Option<PendingPath>,
}

impl PdfSink {
    fn new() -> Self {
        Self {
            width: 0.0,
            height: 0.0,
            content: Content::new(),
            gstates: BTreeMap::new(),
            pending: None,
        }
    }

    /// Ensure an ExtGState resource exists for `(fa, sa)` and emit `/GSn gs`.
    /// Skip emission entirely when both alphas are 1.0 (the PDF default), so
    /// fully-opaque output stays free of gstate noise.
    fn apply_alpha(&mut self, fa: f32, sa: f32) {
        let fk = alpha_key(fa);
        let sk = alpha_key(sa);
        if fk == 1000 && sk == 1000 {
            return;
        }
        let next = self.gstates.len() as u32;
        let idx = *self.gstates.entry((fk, sk)).or_insert(next);
        let name = format!("Gs{idx}");
        self.content.set_parameters(Name(name.as_bytes()));
    }

    fn flush_path(&mut self) {
        let Some(p) = self.pending.take() else {
            return;
        };
        if p.ops.is_empty() {
            return;
        }
        let do_fill = p.style.fill.a > 0.0;
        let do_stroke = p.style.stroke.a > 0.0 && p.style.stroke_width > 0.0;
        if !do_fill && !do_stroke {
            return;
        }

        self.content.save_state();
        self.apply_alpha(
            if do_fill { p.style.fill.a } else { 1.0 },
            if do_stroke { p.style.stroke.a } else { 1.0 },
        );
        if do_fill {
            let Rgba { r, g, b, .. } = p.style.fill;
            self.content
                .set_fill_rgb(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
        }
        if do_stroke {
            let Rgba { r, g, b, .. } = p.style.stroke;
            self.content
                .set_stroke_rgb(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
            self.content.set_line_width(p.style.stroke_width);
            self.content.set_line_cap(pdf_line_cap(p.style.line_cap));
            self.content.set_line_join(pdf_line_join(p.style.line_join));
        }
        emit_path_ops(&p.ops, &mut self.content);
        if p.style.closed {
            self.content.close_path();
        }
        paint(&mut self.content, do_fill, do_stroke, p.style.fill_rule);
        self.content.restore_state();
    }
}

impl DrawSink for PdfSink {
    fn begin(&mut self, width: f32, height: f32) {
        self.width = width.max(1.0);
        self.height = height.max(1.0);
        // Combined transform: y-flip and px→pt scale. Draw-list coords are CSS
        // pixels with y-down/top-left origin; PDF points are y-up/bottom-left.
        // PDF's [a b c d e f] cm means [x' y' 1] = [x y 1] * [[a b 0][c d 0][e f 1]],
        // so for x' = s*x and y' = -s*y + s*h (where s = PX_TO_PT) we need
        // a=s, d=-s, f=s*h.
        let s = PX_TO_PT;
        self.content.transform([s, 0.0, 0.0, -s, 0.0, s * self.height]);
    }

    fn path_begin(&mut self, style: &PathStyle) {
        self.flush_path();
        self.pending = Some(PendingPath {
            style: *style,
            ops: Vec::new(),
            last_point: None,
        });
    }

    fn move_to(&mut self, x: f32, y: f32) {
        if let Some(p) = self.pending.as_mut() {
            p.ops.push(PathOp::Move(x, y));
            p.last_point = Some((x, y));
        }
    }

    fn line_to(&mut self, x: f32, y: f32) {
        if let Some(p) = self.pending.as_mut() {
            p.ops.push(PathOp::Line(x, y));
            p.last_point = Some((x, y));
        }
    }

    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let Some(p) = self.pending.as_mut() else {
            return;
        };
        let Some((p0x, p0y)) = p.last_point else {
            return;
        };
        // PDF has no quadratic operator; convert to a cubic.
        let c1x = p0x + 2.0 / 3.0 * (cx - p0x);
        let c1y = p0y + 2.0 / 3.0 * (cy - p0y);
        let c2x = x + 2.0 / 3.0 * (cx - x);
        let c2y = y + 2.0 / 3.0 * (cy - y);
        p.ops.push(PathOp::Cubic(c1x, c1y, c2x, c2y, x, y));
        p.last_point = Some((x, y));
    }

    fn cubic_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        if let Some(p) = self.pending.as_mut() {
            p.ops.push(PathOp::Cubic(c1x, c1y, c2x, c2y, x, y));
            p.last_point = Some((x, y));
        }
    }

    fn path_end(&mut self) {
        self.flush_path();
    }

    fn clip_push(&mut self, clip: &ClipBox) {
        self.flush_path();
        let hw = clip.w / 2.0;
        let hh = clip.h / 2.0;
        let cos = (clip.angle * std::f32::consts::PI / 180.0).cos();
        let sin = (clip.angle * std::f32::consts::PI / 180.0).sin();
        let corner =
            |x: f32, y: f32| -> (f32, f32) { (clip.cx + x * cos - y * sin, clip.cy + x * sin + y * cos) };
        let p0 = corner(-hw, -hh);
        let p1 = corner(hw, -hh);
        let p2 = corner(hw, hh);
        let p3 = corner(-hw, hh);

        self.content.save_state();
        self.content.move_to(p0.0, p0.1);
        self.content.line_to(p1.0, p1.1);
        self.content.line_to(p2.0, p2.1);
        self.content.line_to(p3.0, p3.1);
        self.content.close_path();
        self.content.clip_nonzero();
        self.content.end_path();
    }

    fn clip_pop(&mut self) {
        self.flush_path();
        self.content.restore_state();
    }

    fn text(&mut self, node: &TextNode) {
        self.flush_path();
        render_text(node, self);
    }

    fn bitmap(&mut self) {
        // Bitmaps are not supported in PDF v1 (consistent with the
        // terminal renderer); silently skip.
        self.flush_path();
    }

    fn end(&mut self) {
        self.flush_path();
    }
}

fn pdf_line_cap(c: LineCap) -> LineCapStyle {
    match c {
        LineCap::Round => LineCapStyle::RoundCap,
        LineCap::Square => LineCapStyle::ProjectingSquareCap,
        LineCap::Butt => LineCapStyle::ButtCap,
    }
}

fn pdf_line_join(j: LineJoin) -> LineJoinStyle {
    match j {
        LineJoin::Round => LineJoinStyle::RoundJoin,
        LineJoin::Bevel => LineJoinStyle::BevelJoin,
        LineJoin::Miter => LineJoinStyle::MiterJoin,
    }
}

/// Render a [`crate::ir::DrawList`] to PDF bytes.
pub fn render_to_pdf_dl(dl: &crate::ir::DrawList) -> Vec<u8> {
    let mut sink = PdfSink::new();
    dl.play_into(&mut sink);
    finish_pdf(sink)
}

fn finish_pdf(mut sink: PdfSink) -> Vec<u8> {
    let w = sink.width;
    let h = sink.height;
    let gstates = std::mem::take(&mut sink.gstates);
    let buf = sink.content.finish();

    // Indirect-reference IDs.
    let catalog_id = Ref::new(1);
    let pages_id = Ref::new(2);
    let page_id = Ref::new(3);
    let content_id = Ref::new(4);
    let mut next_id: i32 = 5;
    let gstate_refs: Vec<((u16, u16), Ref, u32)> = gstates
        .iter()
        .map(|(&(fk, sk), &idx)| {
            let r = Ref::new(next_id);
            next_id += 1;
            ((fk, sk), r, idx)
        })
        .collect();

    let mut pdf = Pdf::new();
    // Match the LaTeX/tectonic default output level so embedded images don't
    // trip "newer than current output PDF setting" warnings. We don't use
    // anything that requires 1.6+.
    pdf.set_version(1, 5);
    pdf.catalog(catalog_id).pages(pages_id);
    pdf.pages(pages_id).kids([page_id]).count(1);

    {
        let mut page = pdf.page(page_id);
        page.parent(pages_id);
        page.media_box(Rect::new(0.0, 0.0, w * PX_TO_PT, h * PX_TO_PT));
        page.contents(content_id);
        if !gstate_refs.is_empty() {
            let mut resources = page.resources();
            let mut gs_dict = resources.ext_g_states();
            for ((_fk, _sk), r, idx) in &gstate_refs {
                let name = format!("Gs{idx}");
                gs_dict.pair(Name(name.as_bytes()), *r);
            }
            gs_dict.finish();
            resources.finish();
        } else {
            // Empty Resources is required for a valid page.
            page.resources();
        }
        page.finish();
    }

    pdf.stream(content_id, buf.as_slice());

    for ((fk, sk), r, _idx) in &gstate_refs {
        let mut gs = pdf.ext_graphics(*r);
        if *fk != 1000 {
            gs.non_stroking_alpha(alpha_value(*fk));
        }
        if *sk != 1000 {
            gs.stroking_alpha(alpha_value(*sk));
        }
        gs.finish();
    }

    pdf.finish()
}

fn emit_path_ops(ops: &[PathOp], content: &mut Content) {
    for op in ops {
        match *op {
            PathOp::Move(x, y) => {
                content.move_to(x, y);
            }
            PathOp::Line(x, y) => {
                content.line_to(x, y);
            }
            PathOp::Cubic(c1x, c1y, c2x, c2y, x, y) => {
                content.cubic_to(c1x, c1y, c2x, c2y, x, y);
            }
            PathOp::Close => {
                content.close_path();
            }
        }
    }
}

fn paint(content: &mut Content, do_fill: bool, do_stroke: bool, rule: FillRule) {
    match (do_fill, do_stroke, rule) {
        (true, true, FillRule::NonZero) => {
            content.fill_nonzero_and_stroke();
        }
        (true, true, FillRule::EvenOdd) => {
            content.fill_even_odd_and_stroke();
        }
        (true, false, FillRule::NonZero) => {
            content.fill_nonzero();
        }
        (true, false, FillRule::EvenOdd) => {
            content.fill_even_odd();
        }
        (false, true, _) => {
            content.stroke();
        }
        (false, false, _) => {
            content.end_path();
        }
    }
}

#[allow(clippy::similar_names)]
fn render_text(node: &TextNode, sink: &mut PdfSink) {
    let size_i = node.size as i32;
    if size_i <= 0 || node.text.is_empty() {
        return;
    }

    let original_w = crate::text::measure_width(&node.text, size_i) as f32;
    let original_h = crate::text::measure_height(&node.text, size_i) as f32;
    if original_w <= 0.0 || original_h <= 0.0 {
        return;
    }
    let baseline_y = crate::text::measure_y_offset(&node.text, size_i) as f32;
    let x_left = crate::text::measure_x_offset(&node.text, size_i) as f32;
    let scale_x = node.bw / original_w * if node.flip_h { -1.0 } else { 1.0 };
    let scale_y = node.bh / original_h * if node.flip_v { -1.0 } else { 1.0 };

    let do_fill = node.fill.a > 0.0;
    let do_stroke = node.stroke.a > 0.0 && node.stroke_width > 0.0;
    if !do_fill && !do_stroke {
        return;
    }

    sink.content.save_state();
    sink.apply_alpha(
        if do_fill { node.fill.a } else { 1.0 },
        if do_stroke { node.stroke.a } else { 1.0 },
    );
    if do_fill {
        sink.content.set_fill_rgb(
            node.fill.r as f32 / 255.0,
            node.fill.g as f32 / 255.0,
            node.fill.b as f32 / 255.0,
        );
    }
    if do_stroke {
        sink.content.set_stroke_rgb(
            node.stroke.r as f32 / 255.0,
            node.stroke.g as f32 / 255.0,
            node.stroke.b as f32 / 255.0,
        );
        sink.content.set_line_width(node.stroke_width);
        sink.content.set_line_cap(pdf_line_cap(node.line_cap));
        sink.content.set_line_join(pdf_line_join(node.line_join));
    }
    // Compose translate * rotate * scale into a single cm. PDF cm matrix
    // [a b c d e f] applies x' = a*x + c*y + e; y' = b*x + d*y + f.
    let theta = node.angle * std::f32::consts::PI / 180.0;
    let ct = theta.cos();
    let st = theta.sin();
    let a = scale_x * ct;
    let bb = scale_x * st;
    let c = -scale_y * st;
    let d = scale_y * ct;
    sink.content.transform([a, bb, c, d, node.cx, node.cy]);

    let shear = match node.italic {
        FontItalic::Normal => 0.0,
        _ => ITALIC_SHEAR,
    };
    let mut adapter = PdfOutline {
        shear,
        baseline_y,
        ops: Vec::new(),
    };
    crate::text::outline(&node.text, size_i, &mut adapter);
    emit_path_ops(&adapter.ops, &mut sink.content);

    if node.underline {
        let face_units = 2048.0_f32;
        let scale = node.size / face_units;
        let underline_pos = -217.0 * scale;
        let thickness = (150.0 * scale).max(1.0);
        let y_top = baseline_y + underline_pos - thickness / 2.0;
        let y_bot = y_top + thickness;
        let x_l = x_left;
        let x_r = x_l + original_w;
        sink.content.move_to(x_l, y_top);
        sink.content.line_to(x_r, y_top);
        sink.content.line_to(x_r, y_bot);
        sink.content.line_to(x_l, y_bot);
        sink.content.close_path();
    }

    paint(&mut sink.content, do_fill, do_stroke, FillRule::NonZero);
    sink.content.restore_state();
}

struct PdfOutline {
    shear: f32,
    baseline_y: f32,
    ops: Vec<PathOp>,
}

impl PdfOutline {
    fn shear_x(&self, x: f32, y: f32) -> f32 {
        if self.shear == 0.0 {
            x
        } else {
            x + self.shear * (self.baseline_y - y)
        }
    }
}

impl crate::text::OutlineBuilder for PdfOutline {
    fn move_to(&mut self, x: f32, y: f32) {
        self.ops.push(PathOp::Move(self.shear_x(x, y), y));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.ops.push(PathOp::Line(self.shear_x(x, y), y));
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let p0 = self.ops.last().and_then(|op| match *op {
            PathOp::Move(x, y) | PathOp::Line(x, y) => Some((x, y)),
            PathOp::Cubic(_, _, _, _, x, y) => Some((x, y)),
            PathOp::Close => None,
        });
        let (p0x, p0y) = match p0 {
            Some(p) => p,
            None => (self.shear_x(x, y), y),
        };
        let cx_s = self.shear_x(cx, cy);
        let x_s = self.shear_x(x, y);
        let c1x = p0x + 2.0 / 3.0 * (cx_s - p0x);
        let c1y = p0y + 2.0 / 3.0 * (cy - p0y);
        let c2x = x_s + 2.0 / 3.0 * (cx_s - x_s);
        let c2y = y + 2.0 / 3.0 * (cy - y);
        self.ops.push(PathOp::Cubic(c1x, c1y, c2x, c2y, x_s, y));
    }
    fn cubic_to(&mut self, cx1: f32, cy1: f32, cx2: f32, cy2: f32, x: f32, y: f32) {
        self.ops.push(PathOp::Cubic(
            self.shear_x(cx1, cy1),
            cy1,
            self.shear_x(cx2, cy2),
            cy2,
            self.shear_x(x, y),
            y,
        ));
    }
    fn close(&mut self) {
        self.ops.push(PathOp::Close);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::DrawList;

    fn red_fill(a: f32) -> PathStyle {
        PathStyle {
            fill: Rgba {
                r: 255,
                g: 0,
                b: 0,
                a,
            },
            ..PathStyle::default()
        }
    }

    fn rect(dl: &mut DrawList, style: PathStyle, x: f32, y: f32, w: f32, h: f32) {
        dl.path_begin(style);
        dl.move_to(x, y);
        dl.line_to(x + w, y);
        dl.line_to(x + w, y + h);
        dl.line_to(x, y + h);
        dl.path_end();
    }

    #[test]
    fn header_only_emits_pdf_marker() {
        let dl = DrawList::new(100.0, 50.0);
        let out = render_to_pdf_dl(&dl);
        assert!(out.starts_with(b"%PDF-"), "missing PDF header");
        assert!(out.windows(5).any(|w| w == b"%%EOF"), "missing PDF trailer");
    }

    #[test]
    fn rect_path_emits_fill_op() {
        let mut dl = DrawList::new(100.0, 50.0);
        rect(&mut dl, red_fill(1.0), 0.0, 0.0, 100.0, 50.0);
        let out = render_to_pdf_dl(&dl);
        assert!(out.starts_with(b"%PDF-"));
        // Look for the fill op `f` (non-zero winding). pdf-writer emits
        // streams uncompressed by default so we can search byte-wise.
        let needle = b" f\n";
        assert!(
            out.windows(needle.len()).any(|w| w == needle)
                || out.windows(3).any(|w| w == b" f\r")
                || out.windows(3).any(|w| w == b"\nf\n"),
            "expected fill operator in stream; bytes: {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn text_emits_some_path_data() {
        let mut dl = DrawList::new(100.0, 30.0);
        dl.text(TextNode {
            fill: Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 1.0,
            },
            stroke: Rgba::default(),
            stroke_width: 0.0,
            line_cap: LineCap::Butt,
            line_join: LineJoin::Miter,
            cx: 50.0,
            cy: 15.0,
            bw: 80.0,
            bh: 20.0,
            angle: 0.0,
            flip_h: false,
            flip_v: false,
            size: 16.0,
            italic: FontItalic::Normal,
            underline: false,
            text: "Hi".to_owned(),
        });
        let out = render_to_pdf_dl(&dl);
        assert!(out.starts_with(b"%PDF-"));
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains(" cm"), "expected cm transform in content stream");
    }

    #[test]
    fn alpha_creates_extgstate_resource() {
        let mut dl = DrawList::new(100.0, 50.0);
        rect(&mut dl, red_fill(0.5), 0.0, 0.0, 100.0, 50.0);
        let out = render_to_pdf_dl(&dl);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("ExtGState"), "expected ExtGState resource");
        assert!(s.contains("/Gs0"), "expected gs name reference");
    }
}
