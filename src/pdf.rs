//! Render a [`crate::scene::Scene`] to a PDF byte stream.
//!
//! `PdfRenderer` writes each element as content-stream operators and
//! assembles a one-page document at the end of the frame. PDF space has y up
//! and the origin at the bottom left, and the draw list has y down and the
//! origin at the top left, so the content stream opens with one `cm` that
//! flips y and scales pixels to points. After it, coordinates go in as they
//! are.
//!
//! Glyphs go in as filled paths from [`crate::text`], as in the raster
//! renderer, so the PDF embeds no font and text measures the same in both.
//! The text is not selectable.

use std::collections::BTreeMap;

use pdf_writer::types::{FunctionShadingType, LineCapStyle, LineJoinStyle};
use pdf_writer::{Content, Finish, Name, Pdf, Rect, Ref};

use crate::renderer::{Renderer, sealed::Paint};
use crate::scene::{
    ClipPath, FillRule, Gradient, GradientGeom, LineCap, LineJoin, Paint as IrPaint, Path, Rgba,
    Segment, Segments, Stop, TextNode,
};

/// Draw-list coordinates are CSS pixels, 96 per inch, and PDF points are 72
/// per inch.
const PX_TO_PT: f32 = 72.0 / 96.0;

/// An alpha in thousandths, so `gstates` can key on it and near-equal alphas
/// share one ExtGState.
fn alpha_key(a: f32) -> u16 {
    (a.clamp(0.0, 1.0) * 1000.0).round() as u16
}

fn alpha_value(key: u16) -> f32 {
    f32::from(key) / 1000.0
}

/// Accumulates a content stream and its resources, then assembles a one-page
/// document into `bytes`.
struct PdfRenderer {
    width: f32,
    height: f32,
    content: Content,
    /// ExtGState index by fill and stroke alpha key.
    gstates: BTreeMap<(u16, u16), u32>,
    /// The gradients of the frame. The index of a gradient is its `/Pn` name
    /// in the content stream and in the pattern dictionary.
    gradients: Vec<Gradient>,
    /// The document of the last render.
    bytes: Vec<u8>,
}

impl PdfRenderer {
    fn new() -> Self {
        PdfRenderer {
            width: 1.0,
            height: 1.0,
            content: Content::new(),
            gstates: BTreeMap::new(),
            gradients: Vec::new(),
            bytes: Vec::new(),
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Returns the `/Pn` name of `g`.
    fn push_gradient(&mut self, g: Gradient) -> String {
        let idx = self.gradients.len();
        self.gradients.push(g);
        format!("P{idx}")
    }

    /// Emits `/Gsn gs` for the alpha pair, allocating the ExtGState the first
    /// time. Both alphas at 1.0 is the PDF default, so that pair emits
    /// nothing.
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

    /// Sets the fill or the stroke paint. A gradient goes through the Pattern
    /// color space and its `/Pn` name.
    fn bind_paint(&mut self, paint: &IrPaint, target: PaintTarget) {
        let gradient = match paint {
            IrPaint::Solid(c) => {
                let [r, g, b] = rgb_components(*c);
                match target {
                    PaintTarget::Fill => self.content.set_fill_rgb(r, g, b),
                    PaintTarget::Stroke => self.content.set_stroke_rgb(r, g, b),
                };
                return;
            }
            IrPaint::Gradient(g) => g.as_ref().clone(),
        };
        let name = self.push_gradient(gradient);
        let name = Name(name.as_bytes());
        let pattern = pdf_writer::types::ColorSpaceOperand::Pattern;
        match target {
            PaintTarget::Fill => {
                self.content.set_fill_color_space(pattern);
                self.content.set_fill_pattern(None, name)
            }
            PaintTarget::Stroke => {
                self.content.set_stroke_color_space(pattern);
                self.content.set_stroke_pattern(None, name)
            }
        };
    }
}

/// Which half of the PDF graphics state a paint binds to.
#[derive(Clone, Copy)]
enum PaintTarget {
    Fill,
    Stroke,
}

/// Emits `restore_state` when dropped, so the `save_state` of a clip in
/// [`Paint::with_clip`] is balanced even when the body panics.
struct RestoreGuard<'a> {
    canvas: &'a mut PdfRenderer,
}

impl Drop for RestoreGuard<'_> {
    fn drop(&mut self) {
        self.canvas.content.restore_state();
    }
}

impl Paint for PdfRenderer {
    /// Starts a new page and writes the base transform. Nothing here
    /// allocates a surface, so it never fails.
    fn ensure_size(&mut self, width: f32, height: f32) -> Result<(), crate::renderer::AllocError> {
        self.width = width.max(1.0);
        self.height = height.max(1.0);
        self.gstates.clear();
        self.gradients.clear();
        let s = PX_TO_PT;
        let mut content = Content::new();
        content.transform([s, 0.0, 0.0, -s, 0.0, s * self.height]);
        self.content = content;
        Ok(())
    }

    fn draw_path(&mut self, path: &Path) {
        let style = &path.style;
        let do_fill = style.draws_fill();
        let do_stroke = style.draws_stroke();
        if !do_fill && !do_stroke {
            return;
        }
        // The content stream receives the elevated walk. It is empty for a
        // path with no Move, and that path must not leave a `q ... Q` with no
        // geometry.
        if path.segments().cubics().next().is_none() {
            return;
        }

        self.content.save_state();
        // A shading has no alpha, so a gradient takes the alpha of its first
        // stop for the whole path.
        let fill_alpha = if do_fill {
            style.fill.primary_color().a
        } else {
            1.0
        };
        let stroke_alpha = if do_stroke {
            style.stroke.primary_color().a
        } else {
            1.0
        };
        self.apply_alpha(fill_alpha, stroke_alpha);

        // PDF forbids a color operator inside a path object.
        if do_fill {
            self.bind_paint(&style.fill, PaintTarget::Fill);
        }
        if do_stroke {
            self.bind_paint(&style.stroke, PaintTarget::Stroke);
        }

        if do_stroke {
            self.content.set_line_width(style.stroke_width);
            self.content.set_line_cap(pdf_line_cap(style.line_cap));
            self.content.set_line_join(pdf_line_join(style.line_join));
            if style.line_join == LineJoin::Miter {
                self.content.set_miter_limit(style.miter_limit);
            }
            if let Some(dash) = &style.dash {
                self.content
                    .set_dash_pattern(dash.array().iter().copied(), dash.offset());
            }
        }
        emit_segments(path.segments(), &mut self.content);
        if style.closed {
            self.content.close_path();
        }
        paint(&mut self.content, do_fill, do_stroke, style.fill_rule);
        // restore_state also resets the pattern color space.
        self.content.restore_state();
    }

    fn end_frame(&mut self) {
        self.assemble();
    }

    fn draw_text(&mut self, node: &TextNode) {
        render_text(node, self);
    }

    fn with_clip<T>(&mut self, clip: &ClipPath, inside: impl FnOnce(&mut Self) -> T) -> T {
        self.content.save_state();
        emit_segments(clip.segments(), &mut self.content);
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
        let guard = RestoreGuard { canvas: self };
        inside(&mut *guard.canvas)
    }
}

impl Renderer for PdfRenderer {
    type Output<'a> = &'a [u8];

    fn output(&self) -> &[u8] {
        &self.bytes
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
    renderer.render(scene).expect("PDF rendering never fails");
    renderer.into_bytes()
}

/// The indirect objects of one gradient.
struct GradientRefs {
    /// The functions, with the one the shading references last.
    functions: Vec<Ref>,
    shading: Ref,
    pattern: Ref,
}

impl PdfRenderer {
    /// Writes the page into `bytes` and empties the content and the resources
    /// for the next render.
    fn assemble(&mut self) {
        let w = self.width;
        let h = self.height;
        let gstates = std::mem::take(&mut self.gstates);
        let gradients = std::mem::take(&mut self.gradients);
        let buf = std::mem::replace(&mut self.content, Content::new()).finish();

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

        let gradient_refs: Vec<GradientRefs> = gradients
            .iter()
            .map(|g| {
                let prepared = prepare_stops(&g.stops);
                // Two stops need one exponential function. More need one per
                // interval and a stitching function.
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
        // Version 1.5 is what tectonic writes, so a document that embeds this
        // one gets no version warning.
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

        for (gradient, refs) in gradients.iter().zip(gradient_refs.iter()) {
            emit_gradient_objects(&mut pdf, gradient, refs);
        }

        self.bytes = pdf.finish();
    }
}

/// The stops sorted, clamped to [0, 1], and padded so there are at least
/// two, the first at 0 and the last at 1. The pad repeats the boundary
/// color, as in CSS. No stops give one black stop, a case the visibility
/// check already excludes.
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

fn emit_gradient_objects(pdf: &mut Pdf, gradient: &Gradient, refs: &GradientRefs) {
    let stops = prepare_stops(&gradient.stops);

    let main_fn_ref = if stops.len() == 2 {
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
        stitch.bounds(stops[1..stops.len() - 1].iter().map(|s| s.offset));
        // Each sub-function maps its interval back to [0, 1].
        let encode: Vec<f32> = (0..n_subfns).flat_map(|_| [0.0, 1.0]).collect();
        stitch.encode(encode);
        stitch.finish();
        stitch_ref
    };

    {
        let mut sh = pdf.function_shading(refs.shading);
        sh.color_space().device_rgb();
        match gradient.geom {
            GradientGeom::Linear { x0, y0, x1, y1 } => {
                sh.shading_type(FunctionShadingType::Axial);
                sh.coords([x0, y0, x1, y1]);
            }
            GradientGeom::Radial { cx, cy, radius } => {
                sh.shading_type(FunctionShadingType::Radial);
                // One center and a zero inner radius, as in SVG.
                sh.coords([cx, cy, 0.0, cx, cy, radius]);
            }
        }
        // A Type 2 or 3 shading only pads, so Reflect and Repeat render as
        // Pad. They would need a Type 4 function.
        sh.extend([true, true]);
        sh.function(main_fn_ref);
        sh.finish();
    }

    {
        let mut pat = pdf.shading_pattern(refs.pattern);
        pat.shading_ref(refs.shading);
        pat.finish();
    }
}

/// Writes `segments` as path ops. PDF has no quadratic operator, so the walk
/// goes through [`Segments::cubics`].
fn emit_segments(segments: Segments<'_>, content: &mut Content) {
    for seg in segments.cubics() {
        match seg {
            Segment::Move { x, y } => {
                content.move_to(x, y);
            }
            Segment::Line { x, y } => {
                content.line_to(x, y);
            }
            Segment::Cubic {
                c1x,
                c1y,
                c2x,
                c2y,
                x,
                y,
            } => {
                content.cubic_to(c1x, c1y, c2x, c2y, x, y);
            }
            // cubics() yields no quadratic, so this arm never runs.
            Segment::Quad { x, y, .. } => {
                content.line_to(x, y);
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
fn render_text(node: &TextNode, canvas: &mut PdfRenderer) {
    let Some(layout) = crate::text::layout_text(node) else {
        return;
    };

    let do_fill = node.fill.a > 0.0;
    let do_stroke = node.stroke.a > 0.0 && node.stroke_width > 0.0;
    if !do_fill && !do_stroke {
        return;
    }

    canvas.content.save_state();
    canvas.apply_alpha(
        if do_fill { node.fill.a } else { 1.0 },
        if do_stroke { node.stroke.a } else { 1.0 },
    );
    if do_fill {
        let [r, g, b] = rgb_components(node.fill);
        canvas.content.set_fill_rgb(r, g, b);
    }
    if do_stroke {
        let [r, g, b] = rgb_components(node.stroke);
        canvas.content.set_stroke_rgb(r, g, b);
        canvas.content.set_line_width(node.stroke_width);
        // A glyph is a closed smooth contour, so the cap and the join do not
        // show.
    }
    canvas.content.transform(node.transform);

    {
        // PDF has no quadratic operator.
        let mut adapter = PdfOutline {
            content: &mut canvas.content,
        };
        let mut out = crate::text::ElevateQuads::new(&mut adapter);
        crate::text::outline_layout(&layout, &node.text, &mut out);
        if node.underline {
            crate::text::outline_underline(&layout, &mut out);
        }
    }

    paint(&mut canvas.content, do_fill, do_stroke, FillRule::NonZero);
    canvas.content.restore_state();
}

/// Glyph outlines, written straight into the content stream.
struct PdfOutline<'a> {
    content: &'a mut Content,
}

impl crate::text::OutlineBuilder for PdfOutline<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.content.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.content.line_to(x, y);
    }
    fn quad_to(&mut self, _cx: f32, _cy: f32, x: f32, y: f32) {
        // Glyphs come through ElevateQuads, so this never runs. A line is the
        // fallback for a direct caller.
        self.content.line_to(x, y);
    }
    fn cubic_to(&mut self, cx1: f32, cy1: f32, cx2: f32, cy2: f32, x: f32, y: f32) {
        self.content.cubic_to(cx1, cy1, cx2, cy2, x, y);
    }
    fn close(&mut self) {
        self.content.close_path();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{PathStyle, Scene};

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
        // pdf-writer does not compress the stream, so the bytes are searchable.
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
    fn a_path_no_move_opened_emits_nothing() {
        // The elevated walk drops a quadratic with no current point, so the
        // path must not leave a `q ... f Q` with no geometry.
        let mut scene = Scene::new(100.0, 50.0);
        scene.path(red_fill(1.0)).quad_to(10.0, 10.0, 20.0, 20.0);
        assert_eq!(
            render_to_pdf(&scene),
            render_to_pdf(&Scene::new(100.0, 50.0))
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
    fn output_reborrows_the_last_assembled_document() {
        // A second output must not re-assemble an empty document.
        let mut scene = Scene::new(20.0, 20.0);
        rect(&mut scene, red_fill(1.0), 0.0, 0.0, 10.0, 10.0);
        let mut renderer = PdfRenderer::new();
        let rendered = renderer.render(&scene).expect("render").to_vec();
        assert!(!rendered.is_empty());
        assert_eq!(renderer.output(), &rendered[..]);
        assert_eq!(renderer.output(), &rendered[..]);
    }

    #[test]
    fn underline_adds_path_ops() {
        let render = |underline: bool| {
            let mut scene = Scene::new(100.0, 30.0);
            scene.text(TextNode {
                fill: Rgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 1.0,
                },
                size: 16.0,
                text: "Hi".to_owned(),
                underline,
                ..TextNode::default()
            });
            render_to_pdf(&scene).len()
        };
        assert!(
            render(true) > render(false),
            "underline should emit extra content-stream ops"
        );
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
        let mut scene = Scene::new(100.0, 50.0);
        let style = PathStyle {
            stroke: crate::scene::Paint::rgba(0, 0, 0, 1.0),
            stroke_width: 1.0,
            dash: crate::scene::Dash::new(vec![3.0, 2.0], 1.0).map(Box::new),
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
        let mut scene = Scene::new(50.0, 50.0);
        let style = PathStyle {
            fill: crate::scene::Paint::gradient(crate::scene::Gradient::linear(
                0.0,
                0.0,
                50.0,
                0.0,
                vec![
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
            )),
            ..PathStyle::default()
        };
        rect(&mut scene, style, 0.0, 0.0, 50.0, 50.0);
        let out = render_to_pdf(&scene);
        let s = String::from_utf8_lossy(&out);
        // ShadingType 2 is axial.
        assert!(
            s.contains("/ShadingType 2") || s.contains("/ShadingType  2"),
            "axial shading missing: {s}"
        );
        // FunctionType 2 is exponential.
        assert!(
            s.contains("/FunctionType 2") || s.contains("/FunctionType  2"),
            "exponential function missing"
        );
        assert!(s.contains("/P0"), "pattern name missing: {s}");
        assert!(s.contains("/Pattern cs"), "missing pattern colorspace: {s}");
    }

    #[test]
    fn radial_gradient_emits_radial_shading() {
        let mut scene = Scene::new(50.0, 50.0);
        let style = PathStyle {
            fill: crate::scene::Paint::gradient(crate::scene::Gradient::radial(
                25.0,
                25.0,
                20.0,
                vec![
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
            )),
            ..PathStyle::default()
        };
        rect(&mut scene, style, 0.0, 0.0, 50.0, 50.0);
        let out = render_to_pdf(&scene);
        let s = String::from_utf8_lossy(&out);
        // ShadingType 3 is radial.
        assert!(
            s.contains("/ShadingType 3") || s.contains("/ShadingType  3"),
            "radial shading missing: {s}"
        );
    }

    #[test]
    fn multi_stop_gradient_uses_stitching_function() {
        let mut scene = Scene::new(60.0, 10.0);
        let style = PathStyle {
            fill: crate::scene::Paint::gradient(crate::scene::Gradient::linear(
                0.0,
                0.0,
                60.0,
                0.0,
                vec![
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
            )),
            ..PathStyle::default()
        };
        rect(&mut scene, style, 0.0, 0.0, 60.0, 10.0);
        let out = render_to_pdf(&scene);
        let s = String::from_utf8_lossy(&out);
        // FunctionType 3 is stitching.
        assert!(
            s.contains("/FunctionType 3") || s.contains("/FunctionType  3"),
            "stitching function missing: {s}"
        );
        assert!(s.contains("/Bounds [0.5]"), "bounds missing: {s}");
    }
}
