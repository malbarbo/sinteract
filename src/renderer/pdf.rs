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

use crate::outline::{ElevateQuads, PathSink};
use crate::renderer::{AllocError, Renderer, RestoreOnDrop, sealed::Canvas};
use crate::scene::{
    ClipPath, FillRule, Gradient, GradientGeom, LineCap, LineJoin, Paint, Path, Rgba, Stop, Text,
};
use crate::text::TextLayout;

/// Render a [`crate::scene::Scene`] to PDF bytes.
pub fn render_to_pdf(scene: &crate::scene::Scene) -> Vec<u8> {
    let mut renderer = PdfRenderer::new();
    renderer.render(scene).expect("PDF rendering never fails");
    renderer.into_bytes()
}

/// Accumulates a content stream and its resources, then assembles a one-page
/// document into `bytes`.
pub struct PdfRenderer {
    width: f32,
    height: f32,
    content: Content,
    /// ExtGState index by fill and stroke alpha key.
    gstates: BTreeMap<(u16, u16), u32>,
    /// The gradients of the frame. The index of a gradient is its `/Pn` name
    /// in the content stream and in the pattern dictionary.
    gradients: Vec<Shading>,
    /// The document of the last render.
    bytes: Vec<u8>,
}

impl PdfRenderer {
    /// An empty renderer. The first render sizes the page.
    pub fn new() -> Self {
        PdfRenderer {
            width: 1.0,
            height: 1.0,
            content: Content::new(),
            gstates: BTreeMap::new(),
            gradients: Vec::new(),
            bytes: Vec::new(),
        }
    }

    /// The document of the last render.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl Default for PdfRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Canvas for PdfRenderer {
    /// Starts a new page and writes the base transform. Nothing here
    /// allocates a surface, so it never fails.
    fn ensure_size(&mut self, width: f32, height: f32) -> Result<(), AllocError> {
        self.width = width.max(1.0);
        self.height = height.max(1.0);
        self.gstates.clear();
        self.gradients.clear();
        // The last document holds the last content stream, so its size saves
        // the stream from growing a frame again.
        let mut content = Content::with_capacity(self.bytes.len());
        content.transform(page_transform(self.height));
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
        // A path with no segments must not leave a `q ... Q` with no geometry.
        if path.segments().next().is_none() {
            return;
        }

        self.begin_paint(
            do_fill.then_some(&style.fill),
            do_stroke.then_some(&style.stroke),
        );
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
        // PDF has no quadratic operator.
        let mut out = PdfOutline::new(&mut self.content);
        path.segments().outline(&mut ElevateQuads::new(&mut out));
        if style.closed {
            self.content.close_path();
        }
        paint(&mut self.content, do_fill, do_stroke, style.fill_rule);
        // restore_state also resets the pattern color space.
        self.content.restore_state();
    }

    fn draw_text(&mut self, node: &Text) {
        render_text(node, self);
    }

    fn end_frame(&mut self) {
        self.assemble();
    }

    fn with_clip<T>(&mut self, clip: &ClipPath, inside: impl FnOnce(&mut Self) -> T) -> T {
        self.content.save_state();
        if clip.segments().next().is_some() {
            let mut out = PdfOutline::new(&mut self.content);
            clip.segments().outline(&mut ElevateQuads::new(&mut out));
            self.content.close_path();
        } else {
            // A close with no current point is an error. An empty rectangle
            // covers nothing, so the clip hides what it holds.
            self.content.rect(0.0, 0.0, 0.0, 0.0);
        }
        match clip.fill_rule {
            FillRule::NonZero => {
                self.content.clip_nonzero();
            }
            FillRule::EvenOdd => {
                self.content.clip_even_odd();
            }
        }
        self.content.end_path();
        let guard = RestoreOnDrop {
            canvas: self,
            restore: |c: &mut Self| {
                c.content.restore_state();
            },
        };
        inside(&mut *guard.canvas)
    }
}

impl Renderer for PdfRenderer {
    type Output<'a> = &'a [u8];

    fn output(&self) -> &[u8] {
        &self.bytes
    }
}

impl PdfRenderer {
    /// Saves the graphics state, then sets the alpha and binds the paint of
    /// each side that draws, `None` for a side that does not. PDF forbids a
    /// color operator inside a path object, so this goes before the path.
    fn begin_paint(&mut self, fill: Option<&Paint>, stroke: Option<&Paint>) {
        self.content.save_state();
        // A shading has no alpha, so a gradient takes the alpha of its first
        // stop for the whole path.
        let alpha = |paint: Option<&Paint>| paint.map_or(1.0, |p| p.primary_color().a);
        self.apply_alpha(alpha(fill), alpha(stroke));
        if let Some(paint) = fill {
            self.bind_paint(paint, PaintTarget::Fill);
        }
        if let Some(paint) = stroke {
            self.bind_paint(paint, PaintTarget::Stroke);
        }
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
        self.content.set_parameters(Name(gs_name(idx).as_bytes()));
    }

    /// Sets the fill or the stroke paint. A gradient goes through the Pattern
    /// color space and its `/Pn` name.
    fn bind_paint(&mut self, paint: &Paint, target: PaintTarget) {
        let gradient = match paint {
            Paint::Solid(c) => {
                let [r, g, b] = rgb_components(*c);
                match target {
                    PaintTarget::Fill => self.content.set_fill_rgb(r, g, b),
                    PaintTarget::Stroke => self.content.set_stroke_rgb(r, g, b),
                };
                return;
            }
            Paint::Gradient(g) => g,
        };
        let name = pattern_name(self.push_gradient(gradient));
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

    /// Returns the index of `g`, the `n` of its `/Pn` name.
    fn push_gradient(&mut self, g: &Gradient) -> usize {
        let idx = self.gradients.len();
        self.gradients.push(Shading {
            geom: g.geom,
            stops: prepare_stops(&g.stops),
        });
        idx
    }

    /// Writes the page into `bytes` and empties the content. The next
    /// `ensure_size` clears the resources and keeps their capacity.
    fn assemble(&mut self) {
        let w = self.width;
        let h = self.height;
        let gstates = &self.gstates;
        let gradients = &self.gradients;
        // An empty content holds no buffer until the next render opens one.
        let buf = std::mem::replace(&mut self.content, Content::with_capacity(0)).finish();

        let catalog_id = Ref::new(1);
        let pages_id = Ref::new(2);
        let page_id = Ref::new(3);
        let content_id = Ref::new(4);
        // The ExtGStates take the ids after the content, in the order of
        // their keys.
        let gstate_ref = |k: usize| Ref::new(5 + k as i32);
        let mut next_id = 5 + gstates.len() as i32;
        let mut alloc = || {
            let r = Ref::new(next_id);
            next_id += 1;
            r
        };

        let gradient_refs: Vec<GradientRefs> = gradients
            .iter()
            .map(|g| {
                let functions = (0..function_count(&g.stops)).map(|_| alloc()).collect();
                GradientRefs {
                    functions,
                    shading: alloc(),
                    pattern: alloc(),
                }
            })
            .collect();

        // The last document is a good guess at the size of this one.
        let mut pdf = Pdf::with_capacity(self.bytes.len());
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
            let mut resources = page.resources();
            if !gstates.is_empty() {
                let mut gs_dict = resources.ext_g_states();
                for (k, &idx) in gstates.values().enumerate() {
                    gs_dict.pair(Name(gs_name(idx).as_bytes()), gstate_ref(k));
                }
                gs_dict.finish();
            }
            if !gradient_refs.is_empty() {
                let mut pat_dict = resources.patterns();
                for (i, gr) in gradient_refs.iter().enumerate() {
                    pat_dict.pair(Name(pattern_name(i).as_bytes()), gr.pattern);
                }
                pat_dict.finish();
            }
            resources.finish();
            page.finish();
        }

        pdf.stream(content_id, buf.as_slice());

        for (k, &(fk, sk)) in gstates.keys().enumerate() {
            let mut gs = pdf.ext_graphics(gstate_ref(k));
            if fk != 1000 {
                gs.non_stroking_alpha(alpha_value(fk));
            }
            if sk != 1000 {
                gs.stroking_alpha(alpha_value(sk));
            }
            gs.finish();
        }

        for (gradient, refs) in gradients.iter().zip(gradient_refs.iter()) {
            emit_gradient_objects(&mut pdf, gradient, refs, page_transform(h));
        }

        self.bytes = pdf.finish();
    }
}

/// Which half of the PDF graphics state a paint binds to.
#[derive(Clone, Copy)]
enum PaintTarget {
    Fill,
    Stroke,
}

/// Draw-list coordinates are CSS pixels, 96 per inch, and PDF points are 72
/// per inch.
const PX_TO_PT: f32 = 72.0 / 96.0;

/// Maps draw-list pixels, with y down, to PDF points, with y up, on a page
/// `height` pixels tall.
fn page_transform(height: f32) -> [f32; 6] {
    [PX_TO_PT, 0.0, 0.0, -PX_TO_PT, 0.0, PX_TO_PT * height]
}

/// An alpha in thousandths, so `gstates` can key on it and near-equal alphas
/// share one ExtGState.
fn alpha_key(a: f32) -> u16 {
    (a.clamp(0.0, 1.0) * 1000.0).round() as u16
}

fn alpha_value(key: u16) -> f32 {
    f32::from(key) / 1000.0
}

/// The name of the ExtGState at `idx` in the resources of the page.
fn gs_name(idx: u32) -> String {
    format!("Gs{idx}")
}

/// The name of the pattern of the gradient at `idx`.
fn pattern_name(idx: usize) -> String {
    format!("P{idx}")
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

fn render_text(node: &Text, canvas: &mut PdfRenderer) {
    let do_fill = node.draws_fill();
    let do_stroke = node.draws_stroke();
    if !do_fill && !do_stroke {
        return;
    }
    let Some(layout) = TextLayout::new(&node.spec) else {
        return;
    };

    let (fill, stroke) = (Paint::Solid(node.fill), Paint::Solid(node.stroke));
    canvas.begin_paint(do_fill.then_some(&fill), do_stroke.then_some(&stroke));
    if do_stroke {
        // The default miter limit is TEXT_MITER_LIMIT.
        canvas.content.set_line_width(node.stroke_width);
    }
    canvas.content.transform(node.transform);

    // PDF has no quadratic operator.
    let mut adapter = PdfOutline::new(&mut canvas.content);
    layout.outline(&mut ElevateQuads::new(&mut adapter));
    // A text of spaces has no outline, and a paint with no path is an error.
    if !adapter.empty {
        paint(&mut canvas.content, do_fill, do_stroke, FillRule::NonZero);
    }

    if node.underline {
        // The underline paints on its own. In one path, a glyph that winds
        // the other way from the rectangle would cancel it where the two
        // cross.
        layout.outline_underline(&mut PdfOutline::new(&mut canvas.content));
        paint(&mut canvas.content, do_fill, do_stroke, FillRule::NonZero);
    }
    canvas.content.restore_state();
}

/// Path and glyph outlines, written straight into the content stream.
struct PdfOutline<'a> {
    content: &'a mut Content,
    /// `true` until the first move.
    empty: bool,
}

impl<'a> PdfOutline<'a> {
    fn new(content: &'a mut Content) -> Self {
        Self {
            content,
            empty: true,
        }
    }
}

impl PathSink for PdfOutline<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.content.move_to(x, y);
        self.empty = false;
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.content.line_to(x, y);
    }
    fn quad_to(&mut self, _cx: f32, _cy: f32, x: f32, y: f32) {
        // Paths, clips and glyphs come through ElevateQuads, so this never
        // runs. A line is the fallback for a direct caller.
        self.content.line_to(x, y);
    }
    fn cubic_to(&mut self, cx1: f32, cy1: f32, cx2: f32, cy2: f32, x: f32, y: f32) {
        self.content.cubic_to(cx1, cy1, cx2, cy2, x, y);
    }
    fn close(&mut self) {
        self.content.close_path();
    }
}

/// A gradient of the frame, with the stops that its functions take.
struct Shading {
    geom: GradientGeom,
    /// From [`prepare_stops`].
    stops: Vec<Stop>,
}

/// The indirect objects of one gradient.
struct GradientRefs {
    /// The functions, as many as [`function_count`] says, with the one the
    /// shading references last.
    functions: Vec<Ref>,
    shading: Ref,
    pattern: Ref,
}

/// One exponential function per interval of `stops`, and a stitching
/// function over them when there is more than one interval.
fn function_count(stops: &[Stop]) -> usize {
    match stops.len() - 1 {
        1 => 1,
        intervals => intervals + 1,
    }
}

/// The stops sorted, clamped to [0, 1], and padded so there are at least
/// two, the first at 0 and the last at 1. The pad repeats the boundary
/// color, as in CSS. No stops give two transparent ones, a case the
/// visibility check already excludes.
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
    if out.len() <= 1 {
        let color = out.first().map_or(Rgba::default(), |s| s.color);
        out = vec![Stop { offset: 0.0, color }, Stop { offset: 1.0, color }];
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

fn emit_gradient_objects(pdf: &mut Pdf, gradient: &Shading, refs: &GradientRefs, matrix: [f32; 6]) {
    let stops = &gradient.stops;

    let intervals = stops.len() - 1;
    for (pair, &r) in stops.windows(2).zip(&refs.functions) {
        let mut f = pdf.exponential_function(r);
        f.domain([0.0, 1.0]);
        f.range([0.0, 1.0, 0.0, 1.0, 0.0, 1.0]);
        f.c0(rgb_components(pair[0].color));
        f.c1(rgb_components(pair[1].color));
        f.n(1.0);
        f.finish();
    }
    let main_fn_ref = *refs.functions.last().expect("a gradient has a function");
    if intervals > 1 {
        let mut stitch = pdf.stitching_function(main_fn_ref);
        stitch.domain([0.0, 1.0]);
        stitch.range([0.0, 1.0, 0.0, 1.0, 0.0, 1.0]);
        stitch.functions(refs.functions[..intervals].iter().copied());
        stitch.bounds(stops[1..intervals].iter().map(|s| s.offset));
        // Each sub-function maps its interval back to [0, 1].
        stitch.encode((0..intervals).flat_map(|_| [0.0, 1.0]));
        stitch.finish();
    }

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
        // A pattern draws in the space of the page, so the `cm` at the top of
        // the content stream does not apply to it.
        pat.matrix(matrix);
        pat.shading_ref(refs.shading);
        pat.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::tests::rect;
    use crate::scene::{Dash, PathStyle, RotatedRect, Scene, TextSpec};

    fn red_fill(a: f32) -> PathStyle {
        PathStyle {
            fill: Paint::rgba(255, 0, 0, a),
            ..PathStyle::default()
        }
    }

    fn gradient_fill(gradient: Gradient) -> PathStyle {
        PathStyle {
            fill: Paint::gradient(gradient),
            ..PathStyle::default()
        }
    }

    fn opaque(r: u8, g: u8, b: u8) -> Rgba {
        Rgba { r, g, b, a: 1.0 }
    }

    fn stop(offset: f32, color: Rgba) -> Stop {
        Stop { offset, color }
    }

    fn text(s: &str, size: f32) -> Text {
        Text {
            fill: opaque(0, 0, 0),
            spec: TextSpec {
                size,
                text: s.to_owned(),
                ..TextSpec::default()
            },
            ..Text::default()
        }
    }

    /// The document of `scene` as text. pdf-writer does not compress a
    /// stream, so the operators are searchable.
    fn pdf_text(scene: &Scene) -> String {
        String::from_utf8_lossy(&render_to_pdf(scene)).into_owned()
    }

    #[test]
    fn header_only_emits_pdf_marker() {
        let out = render_to_pdf(&Scene::new(100.0, 50.0));
        assert!(out.starts_with(b"%PDF-"), "missing PDF header");
        assert!(out.windows(5).any(|w| w == b"%%EOF"), "missing PDF trailer");
    }

    #[test]
    fn rect_path_emits_fill_op() {
        let mut scene = Scene::new(100.0, 50.0);
        rect(&mut scene, red_fill(1.0), 0.0, 0.0, 100.0, 50.0);
        let s = pdf_text(&scene);
        assert!(s.lines().any(|l| l == "f"), "expected fill operator: {s}");
    }

    #[test]
    fn text_emits_some_path_data() {
        let mut scene = Scene::new(100.0, 30.0);
        let fitted = TextSpec {
            size: 16.0,
            text: "Hi".to_owned(),
            ..TextSpec::default()
        }
        .fit(RotatedRect {
            cx: 50.0,
            cy: 15.0,
            w: 80.0,
            h: 20.0,
            angle_deg: 0.0,
        })
        .expect("text fits");
        scene.text(Text {
            fill: opaque(0, 0, 0),
            ..fitted
        });
        let s = pdf_text(&scene);
        assert!(s.starts_with("%PDF-"));
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
    fn underline_paints_apart_from_the_glyphs() {
        let fills = |underline: bool| {
            let mut scene = Scene::new(100.0, 30.0);
            scene.text(Text {
                underline,
                ..text("Hi", 16.0)
            });
            pdf_text(&scene).lines().filter(|l| *l == "f").count()
        };
        assert_eq!(fills(false), 1, "the glyphs fill once");
        assert_eq!(fills(true), 2, "the underline fills apart from the glyphs");
    }

    #[test]
    fn alpha_creates_extgstate_resource() {
        let mut scene = Scene::new(100.0, 50.0);
        rect(&mut scene, red_fill(0.5), 0.0, 0.0, 100.0, 50.0);
        let s = pdf_text(&scene);
        assert!(s.contains("ExtGState"), "expected ExtGState resource");
        assert!(s.contains("/Gs0"), "expected gs name reference");
    }

    #[test]
    fn dash_pattern_emits_d_operator() {
        let mut scene = Scene::new(100.0, 50.0);
        let style = PathStyle {
            stroke: Paint::rgba(0, 0, 0, 1.0),
            stroke_width: 1.0,
            dash: Dash::new(vec![3.0, 2.0], 1.0).map(Box::new),
            ..PathStyle::default()
        };
        rect(&mut scene, style, 0.0, 0.0, 100.0, 50.0);
        let s = pdf_text(&scene);
        // `set_dash_pattern` emits `[a b] off d`.
        assert!(
            s.lines()
                .any(|l| l.starts_with("[3 2] ") && l.ends_with(" d")),
            "no `d` op: {s}"
        );
    }

    #[test]
    fn miter_limit_emits_m_operator() {
        let mut scene = Scene::new(50.0, 50.0);
        let style = PathStyle {
            stroke: Paint::rgba(0, 0, 0, 1.0),
            stroke_width: 4.0,
            miter_limit: 12.0,
            line_join: LineJoin::Miter,
            ..PathStyle::default()
        };
        rect(&mut scene, style, 5.0, 5.0, 40.0, 40.0);
        let s = pdf_text(&scene);
        assert!(s.contains("12 M"), "expected miter limit op: {s}");
    }

    #[test]
    fn linear_gradient_emits_axial_shading() {
        let mut scene = Scene::new(50.0, 50.0);
        let stops = vec![stop(0.0, opaque(255, 0, 0)), stop(1.0, opaque(0, 0, 255))];
        let style = gradient_fill(Gradient::linear(0.0, 0.0, 50.0, 0.0, stops));
        rect(&mut scene, style, 0.0, 0.0, 50.0, 50.0);
        let s = pdf_text(&scene);
        // ShadingType 2 is axial.
        assert!(s.contains("/ShadingType 2"), "axial shading missing: {s}");
        // FunctionType 2 is exponential.
        assert!(
            s.contains("/FunctionType 2"),
            "exponential function missing"
        );
        assert!(s.contains("/P0"), "pattern name missing: {s}");
        assert!(s.contains("/Pattern cs"), "missing pattern colorspace: {s}");
    }

    #[test]
    fn radial_gradient_emits_radial_shading() {
        let mut scene = Scene::new(50.0, 50.0);
        let stops = vec![stop(0.0, opaque(255, 255, 255)), stop(1.0, opaque(0, 0, 0))];
        let style = gradient_fill(Gradient::radial(25.0, 25.0, 20.0, stops));
        rect(&mut scene, style, 0.0, 0.0, 50.0, 50.0);
        let s = pdf_text(&scene);
        // ShadingType 3 is radial.
        assert!(s.contains("/ShadingType 3"), "radial shading missing: {s}");
    }

    #[test]
    fn multi_stop_gradient_uses_stitching_function() {
        let mut scene = Scene::new(60.0, 10.0);
        let stops = vec![
            stop(0.0, opaque(255, 0, 0)),
            stop(0.5, opaque(0, 255, 0)),
            stop(1.0, opaque(0, 0, 255)),
        ];
        let style = gradient_fill(Gradient::linear(0.0, 0.0, 60.0, 0.0, stops));
        rect(&mut scene, style, 0.0, 0.0, 60.0, 10.0);
        let s = pdf_text(&scene);
        // FunctionType 3 is stitching.
        assert!(
            s.contains("/FunctionType 3"),
            "stitching function missing: {s}"
        );
        assert!(s.contains("/Bounds [0.5]"), "bounds missing: {s}");
    }

    #[test]
    fn a_gradient_pattern_maps_pixels_to_the_page() {
        let mut scene = Scene::new(100.0, 40.0);
        let stops = vec![stop(0.0, opaque(255, 0, 0)), stop(1.0, opaque(0, 0, 255))];
        let style = gradient_fill(Gradient::linear(0.0, 0.0, 100.0, 0.0, stops));
        rect(&mut scene, style, 0.0, 0.0, 100.0, 40.0);
        let s = pdf_text(&scene);
        // The base transform of the content stream, for a page 40 pixels tall.
        assert!(s.contains("/Matrix [0.75 0 0 -0.75 0 30]"), "{s}");
    }

    #[test]
    fn an_empty_clip_clips_to_an_empty_rectangle() {
        let mut scene = Scene::new(20.0, 20.0);
        {
            let mut empty = scene.clip(ClipPath::default());
            rect(&mut empty, red_fill(1.0), 0.0, 0.0, 20.0, 20.0);
        }
        let s = pdf_text(&scene);
        assert!(s.contains("q\n0 0 0 0 re\nW\nn\n"), "{s}");
    }

    #[test]
    fn a_text_with_no_outline_paints_nothing() {
        let mut scene = Scene::new(40.0, 20.0);
        scene.text(text("   ", 12.0));
        let s = pdf_text(&scene);
        assert!(!s.lines().any(|l| l == "f"), "{s}");
    }
}
