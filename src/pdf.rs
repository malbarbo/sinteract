//! Render a [`crate::scene::Scene`] to a PDF byte stream. Native-only;
//! the WASM build does not link against `pdf-writer`.
//!
//! The draw list is replayed via [`crate::scene::Scene::render`]; this
//! module implements [`PdfRenderer`] which translates each command into PDF
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

use std::collections::BTreeMap;

use pdf_writer::types::{FunctionShadingType, LineCapStyle, LineJoinStyle};
use pdf_writer::{Content, Finish, Name, Pdf, Rect, Ref};

use crate::renderer::{Renderer, RendererToken};
use crate::scene::{
    Bitmap, ClipPath, FillRule, LineCap, LineJoin, LinearGradient, Paint as IrPaint, PathStyle,
    RadialGradient, Rgba, Segment, Stop, TextNode,
};

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

/// A gradient encountered during draw-list playback. Kept around until
/// [`finish_pdf`] writes out the function/shading/pattern indirect objects.
#[derive(Clone)]
enum GradientShape {
    Linear(LinearGradient),
    Radial(RadialGradient),
}

struct PdfRenderer {
    width: f32,
    height: f32,
    content: Content,
    /// (fill_alpha_key, stroke_alpha_key) -> graphics-state index.
    gstates: BTreeMap<(u16, u16), u32>,
    /// Gradients seen so far; the index in this vec is the `/P{n}` name
    /// used in the content stream and the Resources/Pattern dictionary.
    gradients: Vec<GradientShape>,
    pending: Option<PendingPath>,
}

impl PdfRenderer {
    fn new() -> Self {
        Self {
            width: 0.0,
            height: 0.0,
            content: Content::new(),
            gstates: BTreeMap::new(),
            gradients: Vec::new(),
            pending: None,
        }
    }

    /// Register a gradient and return its content-stream pattern name `Pn`.
    fn push_gradient(&mut self, shape: GradientShape) -> String {
        let idx = self.gradients.len();
        self.gradients.push(shape);
        format!("P{idx}")
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
        // For solid paints: alpha rides the path. For gradients: PDF
        // gradients here are RGB-only; per-stop alpha is dropped, and a
        // uniform alpha is taken from the first stop (best-effort — a soft-
        // mask would be the next step).
        let do_fill = p.style.fill.is_visible();
        let do_stroke = p.style.stroke.is_visible() && p.style.stroke_width > 0.0;
        if !do_fill && !do_stroke {
            return;
        }

        self.content.save_state();
        let fill_alpha = if do_fill {
            p.style.fill.primary_color().a
        } else {
            1.0
        };
        let stroke_alpha = if do_stroke {
            p.style.stroke.primary_color().a
        } else {
            1.0
        };
        self.apply_alpha(fill_alpha, stroke_alpha);

        // Allocate any pattern names *before* writing the path ops so the
        // `cs /Pattern\n /Pn scn` operators land in the right order.
        let fill_pattern = if do_fill {
            self.bind_fill_paint(&p.style.fill)
        } else {
            None
        };
        let stroke_pattern = if do_stroke {
            self.bind_stroke_paint(&p.style.stroke)
        } else {
            None
        };

        if do_stroke {
            self.content.set_line_width(p.style.stroke_width);
            self.content.set_line_cap(pdf_line_cap(p.style.line_cap));
            self.content.set_line_join(pdf_line_join(p.style.line_join));
            if p.style.line_join == LineJoin::Miter {
                self.content.set_miter_limit(p.style.miter_limit);
            }
            if !p.style.dash_array.is_empty() {
                self.content
                    .set_dash_pattern(p.style.dash_array.iter().copied(), p.style.dash_offset);
            }
        }
        emit_path_ops(&p.ops, &mut self.content);
        if p.style.closed {
            self.content.close_path();
        }
        paint(&mut self.content, do_fill, do_stroke, p.style.fill_rule);
        // Pattern color spaces persist on the gstate, so restore_state below
        // is what cleans them up — no explicit reset needed.
        self.content.restore_state();
        let _ = (fill_pattern, stroke_pattern);
    }

    /// Decide what to emit for the fill paint. Solid → `set_fill_rgb`; gradient
    /// → `cs /Pattern\n scn /Pn`. Returns the pattern name for diagnostics.
    fn bind_fill_paint(&mut self, paint: &IrPaint) -> Option<String> {
        match paint {
            IrPaint::Solid(c) => {
                let Rgba { r, g, b, .. } = *c;
                self.content
                    .set_fill_rgb(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
                None
            }
            IrPaint::Linear(g) => {
                let name = self.push_gradient(GradientShape::Linear(g.clone()));
                self.content
                    .set_fill_color_space(pdf_writer::types::ColorSpaceOperand::Pattern);
                self.content.set_fill_pattern(None, Name(name.as_bytes()));
                Some(name)
            }
            IrPaint::Radial(g) => {
                let name = self.push_gradient(GradientShape::Radial(g.clone()));
                self.content
                    .set_fill_color_space(pdf_writer::types::ColorSpaceOperand::Pattern);
                self.content.set_fill_pattern(None, Name(name.as_bytes()));
                Some(name)
            }
        }
    }

    fn bind_stroke_paint(&mut self, paint: &IrPaint) -> Option<String> {
        match paint {
            IrPaint::Solid(c) => {
                let Rgba { r, g, b, .. } = *c;
                self.content
                    .set_stroke_rgb(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
                None
            }
            IrPaint::Linear(g) => {
                let name = self.push_gradient(GradientShape::Linear(g.clone()));
                self.content
                    .set_stroke_color_space(pdf_writer::types::ColorSpaceOperand::Pattern);
                self.content.set_stroke_pattern(None, Name(name.as_bytes()));
                Some(name)
            }
            IrPaint::Radial(g) => {
                let name = self.push_gradient(GradientShape::Radial(g.clone()));
                self.content
                    .set_stroke_color_space(pdf_writer::types::ColorSpaceOperand::Pattern);
                self.content.set_stroke_pattern(None, Name(name.as_bytes()));
                Some(name)
            }
        }
    }
}

impl Renderer for PdfRenderer {
    fn begin(&mut self, width: f32, height: f32) {
        self.width = width.max(1.0);
        self.height = height.max(1.0);
        // Combined transform: y-flip and px→pt scale. Draw-list coords are CSS
        // pixels with y-down/top-left origin; PDF points are y-up/bottom-left.
        // PDF's [a b c d e f] cm means [x' y' 1] = [x y 1] * [[a b 0][c d 0][e f 1]],
        // so for x' = s*x and y' = -s*y + s*h (where s = PX_TO_PT) we need
        // a=s, d=-s, f=s*h.
        let s = PX_TO_PT;
        self.content
            .transform([s, 0.0, 0.0, -s, 0.0, s * self.height]);
    }

    fn path_begin(&mut self, _: RendererToken, style: &PathStyle) {
        self.flush_path();
        self.pending = Some(PendingPath {
            style: style.clone(),
            ops: Vec::new(),
            last_point: None,
        });
    }

    fn move_to(&mut self, _: RendererToken, x: f32, y: f32) {
        if let Some(p) = self.pending.as_mut() {
            p.ops.push(PathOp::Move(x, y));
            p.last_point = Some((x, y));
        }
    }

    fn line_to(&mut self, _: RendererToken, x: f32, y: f32) {
        if let Some(p) = self.pending.as_mut() {
            p.ops.push(PathOp::Line(x, y));
            p.last_point = Some((x, y));
        }
    }

    fn quad_to(&mut self, _: RendererToken, cx: f32, cy: f32, x: f32, y: f32) {
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

    fn cubic_to(
        &mut self,
        _: RendererToken,
        c1x: f32,
        c1y: f32,
        c2x: f32,
        c2y: f32,
        x: f32,
        y: f32,
    ) {
        if let Some(p) = self.pending.as_mut() {
            p.ops.push(PathOp::Cubic(c1x, c1y, c2x, c2y, x, y));
            p.last_point = Some((x, y));
        }
    }

    fn path_end(&mut self, _: RendererToken) {
        self.flush_path();
    }

    fn clip_push(&mut self, _: RendererToken, clip: &ClipPath) {
        self.flush_path();
        self.content.save_state();
        let mut last_point: Option<(f32, f32)> = None;
        for seg in clip.segments() {
            match seg {
                Segment::Move { x, y } => {
                    self.content.move_to(x, y);
                    last_point = Some((x, y));
                }
                Segment::Line { x, y } => {
                    self.content.line_to(x, y);
                    last_point = Some((x, y));
                }
                Segment::Quad { cx, cy, x, y } => {
                    if let Some((p0x, p0y)) = last_point {
                        let c1x = p0x + 2.0 / 3.0 * (cx - p0x);
                        let c1y = p0y + 2.0 / 3.0 * (cy - p0y);
                        let c2x = x + 2.0 / 3.0 * (cx - x);
                        let c2y = y + 2.0 / 3.0 * (cy - y);
                        self.content.cubic_to(c1x, c1y, c2x, c2y, x, y);
                    }
                    last_point = Some((x, y));
                }
                Segment::Cubic {
                    c1x,
                    c1y,
                    c2x,
                    c2y,
                    x,
                    y,
                } => {
                    self.content.cubic_to(c1x, c1y, c2x, c2y, x, y);
                    last_point = Some((x, y));
                }
            }
        }
        self.content.close_path();
        match clip.fill_rule {
            FillRule::NonZero => {
                self.content.clip_nonzero();
            }
            FillRule::EvenOdd => {
                self.content.clip_even_odd();
            }
        }
        self.content.end_path();
    }

    fn clip_pop(&mut self, _: RendererToken) {
        self.flush_path();
        self.content.restore_state();
    }

    fn text(&mut self, node: &TextNode) {
        self.flush_path();
        render_text(node, self);
    }

    fn bitmap(&mut self, _node: &Bitmap) {
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

/// Render a [`crate::scene::Scene`] to PDF bytes.
pub fn render_to_pdf(scene: &crate::scene::Scene) -> Vec<u8> {
    let mut renderer = PdfRenderer::new();
    scene.render(&mut renderer);
    finish_pdf(renderer)
}

/// Layout of one gradient as PDF indirect objects: a list of sub-function
/// refs (≥ 1; the last one is the entry function — either an exponential or
/// a stitching), the shading ref, and the pattern ref.
struct GradientRefs {
    /// All function refs in dependency order; the last entry is the function
    /// referenced by the shading dictionary.
    functions: Vec<Ref>,
    shading: Ref,
    pattern: Ref,
}

fn finish_pdf(mut renderer: PdfRenderer) -> Vec<u8> {
    let w = renderer.width;
    let h = renderer.height;
    let gstates = std::mem::take(&mut renderer.gstates);
    let gradients = std::mem::take(&mut renderer.gradients);
    let buf = renderer.content.finish();

    // Indirect-reference IDs.
    let catalog_id = Ref::new(1);
    let pages_id = Ref::new(2);
    let page_id = Ref::new(3);
    let content_id = Ref::new(4);
    let mut next_id: i32 = 5;
    let mut alloc = || {
        let r = Ref::new(next_id);
        next_id += 1;
        r
    };

    let gstate_refs: Vec<((u16, u16), Ref, u32)> = gstates
        .iter()
        .map(|(&(fk, sk), &idx)| ((fk, sk), alloc(), idx))
        .collect();

    // Allocate function/shading/pattern refs for each gradient.
    let gradient_refs: Vec<GradientRefs> = gradients
        .iter()
        .map(|shape| {
            let stops = match shape {
                GradientShape::Linear(g) => &g.stops,
                GradientShape::Radial(g) => &g.stops,
            };
            let prepared = prepare_stops(stops);
            // (stops-1) subfunctions when stitching, or 1 exponential when
            // there are exactly 2 stops.
            let n_subfns = if prepared.len() <= 2 {
                1
            } else {
                prepared.len() - 1
            };
            let need_stitch = prepared.len() > 2;
            let n_fn_refs = n_subfns + if need_stitch { 1 } else { 0 };
            let functions: Vec<Ref> = (0..n_fn_refs).map(|_| alloc()).collect();
            GradientRefs {
                functions,
                shading: alloc(),
                pattern: alloc(),
            }
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
        let need_resources = !gstate_refs.is_empty() || !gradient_refs.is_empty();
        if need_resources {
            let mut resources = page.resources();
            if !gstate_refs.is_empty() {
                let mut gs_dict = resources.ext_g_states();
                for ((_fk, _sk), r, idx) in &gstate_refs {
                    let name = format!("Gs{idx}");
                    gs_dict.pair(Name(name.as_bytes()), *r);
                }
                gs_dict.finish();
            }
            if !gradient_refs.is_empty() {
                let mut pat_dict = resources.patterns();
                for (i, gr) in gradient_refs.iter().enumerate() {
                    let name = format!("P{i}");
                    pat_dict.pair(Name(name.as_bytes()), gr.pattern);
                }
                pat_dict.finish();
            }
            resources.finish();
        } else {
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

    for (shape, refs) in gradients.iter().zip(gradient_refs.iter()) {
        emit_gradient_objects(&mut pdf, shape, refs);
    }

    pdf.finish()
}

/// Normalize stops so they cover [0, 1] and there is at least 2 entries:
///   * Empty stops produce a single black stop at 0 (the caller already
///     filtered out invisible paints, so this should be unreachable; kept
///     defensive).
///   * Single stop: duplicate it so we have a [0, 1] constant gradient.
///   * If the first/last stop is not at 0/1, pad with the boundary color
///     so colors extend past the gradient axis (CSS/SVG semantics).
///   * Force monotonic non-decreasing offsets clamped to [0, 1].
fn prepare_stops(stops: &[Stop]) -> Vec<Stop> {
    let mut out: Vec<Stop> = stops
        .iter()
        .map(|s| Stop {
            offset: s.offset.clamp(0.0, 1.0),
            color: s.color,
        })
        .collect();
    out.sort_by(|a, b| {
        a.offset
            .partial_cmp(&b.offset)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if out.is_empty() {
        out.push(Stop {
            offset: 0.0,
            color: Rgba::default(),
        });
    }
    if out.len() == 1 {
        let only = out[0];
        out = vec![
            Stop {
                offset: 0.0,
                ..only
            },
            Stop {
                offset: 1.0,
                ..only
            },
        ];
    }
    let first = out[0];
    if first.offset > 0.0 {
        out.insert(
            0,
            Stop {
                offset: 0.0,
                color: first.color,
            },
        );
    }
    let last = *out.last().unwrap();
    if last.offset < 1.0 {
        out.push(Stop {
            offset: 1.0,
            color: last.color,
        });
    }
    out
}

fn rgb_components(c: Rgba) -> [f32; 3] {
    [c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0]
}

fn emit_gradient_objects(pdf: &mut Pdf, shape: &GradientShape, refs: &GradientRefs) {
    let stops = match shape {
        GradientShape::Linear(g) => prepare_stops(&g.stops),
        GradientShape::Radial(g) => prepare_stops(&g.stops),
    };

    // 1. Subfunctions + (optional) stitching function.
    let main_fn_ref = if stops.len() == 2 {
        // Single exponential c0 → c1 over [0, 1].
        let r = refs.functions[0];
        let mut f = pdf.exponential_function(r);
        f.domain([0.0, 1.0]);
        f.range([0.0, 1.0, 0.0, 1.0, 0.0, 1.0]);
        f.c0(rgb_components(stops[0].color));
        f.c1(rgb_components(stops[1].color));
        f.n(1.0);
        f.finish();
        r
    } else {
        // (N-1) sub-exponentials + 1 stitching function. The last ref in
        // `functions` is the stitch; the first (N-1) are sub-exps.
        let n_subfns = stops.len() - 1;
        debug_assert_eq!(refs.functions.len(), n_subfns + 1);
        for i in 0..n_subfns {
            let r = refs.functions[i];
            let mut f = pdf.exponential_function(r);
            f.domain([0.0, 1.0]);
            f.range([0.0, 1.0, 0.0, 1.0, 0.0, 1.0]);
            f.c0(rgb_components(stops[i].color));
            f.c1(rgb_components(stops[i + 1].color));
            f.n(1.0);
            f.finish();
        }
        let stitch_ref = *refs.functions.last().unwrap();
        let mut stitch = pdf.stitching_function(stitch_ref);
        stitch.domain([0.0, 1.0]);
        stitch.range([0.0, 1.0, 0.0, 1.0, 0.0, 1.0]);
        stitch.functions(refs.functions[..n_subfns].iter().copied());
        // Bounds: interior stop offsets (exclude first and last).
        stitch.bounds(stops[1..stops.len() - 1].iter().map(|s| s.offset));
        // Encode: each sub-function consumes its slice of [0, 1] and maps
        // it back to its own [0, 1] domain.
        let encode: Vec<f32> = (0..n_subfns).flat_map(|_| [0.0, 1.0]).collect();
        stitch.encode(encode);
        stitch.finish();
        stitch_ref
    };

    // 2. Shading dictionary.
    {
        let mut sh = pdf.function_shading(refs.shading);
        sh.color_space().device_rgb();
        match shape {
            GradientShape::Linear(g) => {
                sh.shading_type(FunctionShadingType::Axial);
                sh.coords([g.x0, g.y0, g.x1, g.y1]);
            }
            GradientShape::Radial(g) => {
                sh.shading_type(FunctionShadingType::Radial);
                // (cx0, cy0, r0, cx1, cy1, r1) — SVG-style single center +
                // radius: r0 = 0, r1 = radius, both centers equal.
                sh.coords([g.cx, g.cy, 0.0, g.cx, g.cy, g.radius]);
            }
        }
        // PDF Type 2/3 shadings only support pad-or-transparent via /Extend.
        // Honoring Reflect/Repeat would require a Type 4 PostScript function
        // that folds/wraps t and inlines color interpolation — punt for now;
        // wire round-trip preserves the mode but PDF renders it as Pad.
        sh.extend([true, true]);
        sh.function(main_fn_ref);
        sh.finish();
    }

    // 3. Shading pattern dictionary.
    {
        let mut pat = pdf.shading_pattern(refs.pattern);
        pat.shading_ref(refs.shading);
        pat.finish();
    }
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
fn render_text(node: &TextNode, renderer: &mut PdfRenderer) {
    let size_i = node.size as i32;
    if size_i <= 0 || node.text.is_empty() {
        return;
    }

    let font = crate::text::resolve(&node.family, node.weight, node.style);
    let face = font.face();

    // Width is still needed for the underline rect (drawn in natural text
    // space); vertical layout uses face metrics directly.
    let original_w = crate::text::measure_width_with(face, &node.text, size_i) as f32;
    if original_w <= 0.0 {
        return;
    }
    let baseline_y = crate::text::measure_y_offset_with(face, &node.text, size_i) as f32;
    let x_left = crate::text::measure_x_offset_with(face, &node.text, size_i) as f32;

    let do_fill = node.fill.a > 0.0;
    let do_stroke = node.stroke.a > 0.0 && node.stroke_width > 0.0;
    if !do_fill && !do_stroke {
        return;
    }

    renderer.content.save_state();
    renderer.apply_alpha(
        if do_fill { node.fill.a } else { 1.0 },
        if do_stroke { node.stroke.a } else { 1.0 },
    );
    if do_fill {
        renderer.content.set_fill_rgb(
            node.fill.r as f32 / 255.0,
            node.fill.g as f32 / 255.0,
            node.fill.b as f32 / 255.0,
        );
    }
    if do_stroke {
        renderer.content.set_stroke_rgb(
            node.stroke.r as f32 / 255.0,
            node.stroke.g as f32 / 255.0,
            node.stroke.b as f32 / 255.0,
        );
        renderer.content.set_line_width(node.stroke_width);
        // Cap/join intentionally omitted: text outlines are closed contours
        // on smooth curves, so the PDF defaults (butt cap, miter join) are
        // visually identical to anything the producer might pick.
    }
    // `node.transform` is already in the PDF `cm` convention.
    renderer.content.transform(node.transform);

    let mut adapter = PdfOutline { ops: Vec::new() };
    crate::text::outline_with(face, &node.text, size_i, &mut adapter);
    emit_path_ops(&adapter.ops, &mut renderer.content);

    if node.underline {
        let face_units = face.units_per_em() as f32;
        let scale = node.size / face_units;
        let metrics = face.underline_metrics();
        let pos_units = metrics.map(|m| m.position as f32).unwrap_or(-217.0);
        let thickness_units = metrics.map(|m| m.thickness as f32).unwrap_or(150.0);
        let underline_pos = -pos_units * scale;
        let thickness = (thickness_units * scale).max(1.0);
        let y_top = baseline_y + underline_pos - thickness / 2.0;
        let y_bot = y_top + thickness;
        let x_l = x_left;
        let x_r = x_l + original_w;
        renderer.content.move_to(x_l, y_top);
        renderer.content.line_to(x_r, y_top);
        renderer.content.line_to(x_r, y_bot);
        renderer.content.line_to(x_l, y_bot);
        renderer.content.close_path();
    }

    paint(&mut renderer.content, do_fill, do_stroke, FillRule::NonZero);
    renderer.content.restore_state();
}

struct PdfOutline {
    ops: Vec<PathOp>,
}

impl crate::text::OutlineBuilder for PdfOutline {
    fn move_to(&mut self, x: f32, y: f32) {
        self.ops.push(PathOp::Move(x, y));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.ops.push(PathOp::Line(x, y));
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let p0 = self.ops.last().and_then(|op| match *op {
            PathOp::Move(x, y) | PathOp::Line(x, y) => Some((x, y)),
            PathOp::Cubic(_, _, _, _, x, y) => Some((x, y)),
            PathOp::Close => None,
        });
        let (p0x, p0y) = p0.unwrap_or((x, y));
        let c1x = p0x + 2.0 / 3.0 * (cx - p0x);
        let c1y = p0y + 2.0 / 3.0 * (cy - p0y);
        let c2x = x + 2.0 / 3.0 * (cx - x);
        let c2y = y + 2.0 / 3.0 * (cy - y);
        self.ops.push(PathOp::Cubic(c1x, c1y, c2x, c2y, x, y));
    }
    fn cubic_to(&mut self, cx1: f32, cy1: f32, cx2: f32, cy2: f32, x: f32, y: f32) {
        self.ops.push(PathOp::Cubic(cx1, cy1, cx2, cy2, x, y));
    }
    fn close(&mut self) {
        self.ops.push(PathOp::Close);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Scene;

    fn red_fill(a: f32) -> PathStyle {
        PathStyle {
            fill: crate::scene::Paint::rgba(255, 0, 0, a),
            ..PathStyle::default()
        }
    }

    fn rect(scene: &mut Scene, style: PathStyle, x: f32, y: f32, w: f32, h: f32) {
        let mut p = scene.path(style);
        p.move_to(x, y);
        p.line_to(x + w, y);
        p.line_to(x + w, y + h);
        p.line_to(x, y + h);
    }

    #[test]
    fn header_only_emits_pdf_marker() {
        let scene = Scene::new(100.0, 50.0);
        let out = render_to_pdf(&scene);
        assert!(out.starts_with(b"%PDF-"), "missing PDF header");
        assert!(out.windows(5).any(|w| w == b"%%EOF"), "missing PDF trailer");
    }

    #[test]
    fn rect_path_emits_fill_op() {
        let mut scene = Scene::new(100.0, 50.0);
        rect(&mut scene, red_fill(1.0), 0.0, 0.0, 100.0, 50.0);
        let out = render_to_pdf(&scene);
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
        let mut scene = Scene::new(100.0, 30.0);
        scene.text(TextNode {
            fill: Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 1.0,
            },
            transform: crate::scene::text_box_affine(
                "",
                400,
                crate::scene::FontStyle::Normal,
                16.0,
                "Hi",
                50.0,
                15.0,
                80.0,
                20.0,
                0.0,
            ),
            size: 16.0,
            text: "Hi".to_owned(),
            ..TextNode::default()
        });
        let out = render_to_pdf(&scene);
        assert!(out.starts_with(b"%PDF-"));
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains(" cm"), "expected cm transform in content stream");
    }

    #[test]
    fn alpha_creates_extgstate_resource() {
        let mut scene = Scene::new(100.0, 50.0);
        rect(&mut scene, red_fill(0.5), 0.0, 0.0, 100.0, 50.0);
        let out = render_to_pdf(&scene);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("ExtGState"), "expected ExtGState resource");
        assert!(s.contains("/Gs0"), "expected gs name reference");
    }

    #[test]
    fn dash_pattern_emits_d_operator() {
        // A stroked rect with dash_array [3, 2] dash_offset 1 should produce
        // the PDF `d` operator with the same numbers in the content stream.
        let mut scene = Scene::new(100.0, 50.0);
        let style = PathStyle {
            stroke: crate::scene::Paint::rgba(0, 0, 0, 1.0),
            stroke_width: 1.0,
            dash_array: vec![3.0, 2.0],
            dash_offset: 1.0,
            ..PathStyle::default()
        };
        rect(&mut scene, style, 0.0, 0.0, 100.0, 50.0);
        let out = render_to_pdf(&scene);
        let s = String::from_utf8_lossy(&out);
        // `set_dash_pattern` emits `[a b] off d`.
        assert!(s.contains(" d\n") || s.contains(" d\r"), "no `d` op: {s}");
        assert!(s.contains("[3 2]"), "dash array not found: {s}");
    }

    #[test]
    fn miter_limit_emits_m_operator() {
        // Miter joins with non-default miter_limit should emit the `M` op.
        let mut scene = Scene::new(50.0, 50.0);
        let style = PathStyle {
            stroke: crate::scene::Paint::rgba(0, 0, 0, 1.0),
            stroke_width: 4.0,
            miter_limit: 12.0,
            line_join: LineJoin::Miter,
            ..PathStyle::default()
        };
        rect(&mut scene, style, 5.0, 5.0, 40.0, 40.0);
        let out = render_to_pdf(&scene);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("12 M"), "expected miter limit op: {s}");
    }

    #[test]
    fn linear_gradient_emits_axial_shading() {
        // 2-stop linear gradient should emit:
        //  - one ExponentialFunction with the two colors,
        //  - one FunctionShading (ShadingType 2 = axial) referencing it,
        //  - one ShadingPattern,
        //  - the content stream using `cs /Pattern\n /P0 scn` for the fill.
        let mut scene = Scene::new(50.0, 50.0);
        let style = PathStyle {
            fill: crate::scene::Paint::Linear(crate::scene::LinearGradient {
                x0: 0.0,
                y0: 0.0,
                x1: 50.0,
                y1: 0.0,
                stops: vec![
                    crate::scene::Stop {
                        offset: 0.0,
                        color: Rgba {
                            r: 255,
                            g: 0,
                            b: 0,
                            a: 1.0,
                        },
                    },
                    crate::scene::Stop {
                        offset: 1.0,
                        color: Rgba {
                            r: 0,
                            g: 0,
                            b: 255,
                            a: 1.0,
                        },
                    },
                ],
                ..crate::scene::LinearGradient::default()
            }),
            ..PathStyle::default()
        };
        rect(&mut scene, style, 0.0, 0.0, 50.0, 50.0);
        let out = render_to_pdf(&scene);
        let s = String::from_utf8_lossy(&out);
        // ShadingType 2 = axial gradient.
        assert!(
            s.contains("/ShadingType 2") || s.contains("/ShadingType  2"),
            "axial shading missing: {s}"
        );
        // FunctionType 2 = exponential interpolation.
        assert!(
            s.contains("/FunctionType 2") || s.contains("/FunctionType  2"),
            "exponential function missing"
        );
        // Pattern resource registered as /P0.
        assert!(s.contains("/P0"), "pattern name missing: {s}");
        // Content stream switches to Pattern colorspace then names /P0.
        assert!(s.contains("/Pattern cs"), "missing pattern colorspace: {s}");
    }

    #[test]
    fn radial_gradient_emits_radial_shading() {
        let mut scene = Scene::new(50.0, 50.0);
        let style = PathStyle {
            fill: crate::scene::Paint::Radial(crate::scene::RadialGradient {
                cx: 25.0,
                cy: 25.0,
                radius: 20.0,
                stops: vec![
                    crate::scene::Stop {
                        offset: 0.0,
                        color: Rgba {
                            r: 255,
                            g: 255,
                            b: 255,
                            a: 1.0,
                        },
                    },
                    crate::scene::Stop {
                        offset: 1.0,
                        color: Rgba {
                            r: 0,
                            g: 0,
                            b: 0,
                            a: 1.0,
                        },
                    },
                ],
                ..crate::scene::RadialGradient::default()
            }),
            ..PathStyle::default()
        };
        rect(&mut scene, style, 0.0, 0.0, 50.0, 50.0);
        let out = render_to_pdf(&scene);
        let s = String::from_utf8_lossy(&out);
        // ShadingType 3 = radial gradient.
        assert!(
            s.contains("/ShadingType 3") || s.contains("/ShadingType  3"),
            "radial shading missing: {s}"
        );
    }

    #[test]
    fn multi_stop_gradient_uses_stitching_function() {
        // 3 stops should produce a Type 3 (stitching) function wrapping two
        // Type 2 sub-functions.
        let mut scene = Scene::new(60.0, 10.0);
        let style = PathStyle {
            fill: crate::scene::Paint::Linear(crate::scene::LinearGradient {
                x0: 0.0,
                y0: 0.0,
                x1: 60.0,
                y1: 0.0,
                stops: vec![
                    crate::scene::Stop {
                        offset: 0.0,
                        color: Rgba {
                            r: 255,
                            g: 0,
                            b: 0,
                            a: 1.0,
                        },
                    },
                    crate::scene::Stop {
                        offset: 0.5,
                        color: Rgba {
                            r: 0,
                            g: 255,
                            b: 0,
                            a: 1.0,
                        },
                    },
                    crate::scene::Stop {
                        offset: 1.0,
                        color: Rgba {
                            r: 0,
                            g: 0,
                            b: 255,
                            a: 1.0,
                        },
                    },
                ],
                ..crate::scene::LinearGradient::default()
            }),
            ..PathStyle::default()
        };
        rect(&mut scene, style, 0.0, 0.0, 60.0, 10.0);
        let out = render_to_pdf(&scene);
        let s = String::from_utf8_lossy(&out);
        // Stitching function present (FunctionType 3).
        assert!(
            s.contains("/FunctionType 3") || s.contains("/FunctionType  3"),
            "stitching function missing: {s}"
        );
        // The interior bound 0.5 should appear in the /Bounds array.
        assert!(s.contains("/Bounds [0.5]"), "bounds missing: {s}");
    }
}
