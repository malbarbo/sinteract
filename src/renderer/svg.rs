//! Render a [`crate::scene::Scene`] to an SVG document.
//!
//! SVG and the draw list share one space, CSS pixels with y down and the
//! origin at the top left, so coordinates go in as they are.
//!
//! Glyphs go in as filled paths from [`crate::text`], as in the other
//! renderers, so the document needs no font and text measures the same in
//! all of them. The SVG of Racket's `2htdp/image` does the same. Each glyph
//! of a face at a size goes into `<defs>` once, and every occurrence is a
//! `<use>`. The text is not selectable.

use std::collections::HashMap;
use std::fmt::{self, Write};

use crate::outline::PathSink;
use crate::renderer::{Renderer, RestoreOnDrop, TEXT_MITER_LIMIT, sealed::Canvas};
use crate::scene::{
    ClipPath, DEFAULT_MITER_LIMIT, FillRule, Gradient, GradientGeom, LineCap, LineJoin, Paint,
    Path, Rgba, Segments, SpreadMode, Text,
};
use crate::text::{Glyph, TextLayout};

/// Render a [`crate::scene::Scene`] to an SVG document.
pub fn render_to_svg(scene: &crate::scene::Scene) -> String {
    let mut renderer = SvgRenderer::new();
    renderer.render(scene).expect("SVG rendering never fails");
    renderer.into_string()
}

/// Accumulates the body and the definitions of a frame, then joins them into
/// one document at the end of the frame.
pub struct SvgRenderer {
    width: f32,
    height: f32,
    /// The gradients, clip paths and glyphs that the body references.
    defs: String,
    body: String,
    /// The id of each glyph in `defs`. A glyph with an empty outline, such
    /// as a space, has no id.
    glyphs: HashMap<Glyph, Option<usize>>,
    glyph_defs: usize,
    /// The glyphs of the text being written, with their x, kept for the
    /// capacity.
    uses: Vec<(usize, f32)>,
    gradients: usize,
    clips: usize,
    /// The start of every id in the document.
    prefix: String,
    /// The document of the last render.
    svg: String,
}

impl SvgRenderer {
    /// An empty renderer. The first render sizes the document.
    pub fn new() -> Self {
        SvgRenderer {
            width: 1.0,
            height: 1.0,
            defs: String::new(),
            body: String::new(),
            glyphs: HashMap::new(),
            glyph_defs: 0,
            uses: Vec::new(),
            gradients: 0,
            clips: 0,
            prefix: String::new(),
            svg: String::new(),
        }
    }

    /// An empty renderer whose ids start with `prefix`, so documents with
    /// different prefixes can share one HTML page. Returns `None` when the
    /// prefix holds a character other than an ASCII letter, a digit, `-` or
    /// `_`, or starts with a digit or `-`, which cannot start an XML id.
    pub fn with_id_prefix(prefix: &str) -> Option<Self> {
        let chars = prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        let start = !prefix.starts_with(|c: char| c.is_ascii_digit() || c == '-');
        (chars && start).then(|| Self {
            prefix: prefix.to_owned(),
            ..Self::new()
        })
    }

    /// The document of the last render.
    pub fn into_string(self) -> String {
        self.svg
    }
}

impl Default for SvgRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Canvas for SvgRenderer {
    /// Starts a new document. Nothing here allocates a surface, so it never
    /// fails.
    fn ensure_size(&mut self, width: f32, height: f32) -> Result<(), crate::renderer::AllocError> {
        self.width = width.max(1.0);
        self.height = height.max(1.0);
        self.defs.clear();
        self.body.clear();
        self.glyphs.clear();
        self.glyph_defs = 0;
        self.gradients = 0;
        self.clips = 0;
        Ok(())
    }

    fn draw_path(&mut self, path: &Path) {
        let style = &path.style;
        let do_fill = style.draws_fill();
        let do_stroke = style.draws_stroke();
        if !do_fill && !do_stroke {
            return;
        }
        // A path with no segments would write an empty `<path>`.
        if path.segments().next().is_none() {
            return;
        }

        self.body.push_str("<path d=\"");
        write_segments(path.segments(), &mut self.body);
        if style.closed {
            self.body.push_str(" Z");
        }
        self.body.push('"');

        if do_fill {
            self.write_paint(&style.fill, "fill", "fill-opacity");
            if style.fill_rule == FillRule::EvenOdd {
                self.body.push_str(" fill-rule=\"evenodd\"");
            }
        } else {
            self.body.push_str(" fill=\"none\"");
        }

        if do_stroke {
            self.write_paint(&style.stroke, "stroke", "stroke-opacity");
            _ = write!(self.body, " stroke-width=\"{}\"", style.stroke_width);
            match style.line_cap {
                LineCap::Butt => {}
                LineCap::Round => self.body.push_str(" stroke-linecap=\"round\""),
                LineCap::Square => self.body.push_str(" stroke-linecap=\"square\""),
            }
            match style.line_join {
                LineJoin::Miter => {
                    if style.miter_limit != DEFAULT_MITER_LIMIT {
                        _ = write!(self.body, " stroke-miterlimit=\"{}\"", style.miter_limit);
                    }
                }
                LineJoin::Round => self.body.push_str(" stroke-linejoin=\"round\""),
                LineJoin::Bevel => self.body.push_str(" stroke-linejoin=\"bevel\""),
            }
            if let Some(dash) = &style.dash {
                self.body.push_str(" stroke-dasharray=\"");
                write_list(dash.array(), &mut self.body);
                self.body.push('"');
                if dash.offset() != 0.0 {
                    _ = write!(self.body, " stroke-dashoffset=\"{}\"", dash.offset());
                }
            }
        }
        self.body.push_str("/>\n");
    }

    fn draw_text(&mut self, node: &Text) {
        render_text(node, self);
    }

    fn end_frame(&mut self) {
        let (w, h) = (self.width, self.height);
        self.svg.clear();
        _ = writeln!(
            self.svg,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" \
             xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
             width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\">"
        );
        if !self.defs.is_empty() {
            self.svg.push_str("<defs>\n");
            self.svg.push_str(&self.defs);
            self.svg.push_str("</defs>\n");
        }
        self.svg.push_str(&self.body);
        self.svg.push_str("</svg>\n");
    }

    /// The clip goes into `defs`, and the elements inside it go into a group
    /// that references it. A nested group intersects the clips.
    fn with_clip<T>(&mut self, clip: &ClipPath, inside: impl FnOnce(&mut Self) -> T) -> T {
        let id = self.clips;
        self.clips += 1;
        let prefix = &self.prefix;
        _ = write!(self.defs, "<clipPath id=\"{prefix}c{id}\"><path d=\"");
        write_segments(clip.segments(), &mut self.defs);
        self.defs.push('"');
        if clip.fill_rule == FillRule::EvenOdd {
            self.defs.push_str(" clip-rule=\"evenodd\"");
        }
        self.defs.push_str("/></clipPath>\n");
        _ = writeln!(self.body, "<g clip-path=\"url(#{prefix}c{id})\">");
        let guard = RestoreOnDrop {
            canvas: self,
            restore: |c: &mut Self| c.body.push_str("</g>\n"),
        };
        inside(&mut *guard.canvas)
    }
}

impl Renderer for SvgRenderer {
    type Output<'a> = &'a str;

    fn output(&self) -> &str {
        &self.svg
    }
}

impl SvgRenderer {
    /// Writes the attribute of `paint`, and the opacity attribute when the
    /// color is translucent. A gradient goes into `defs`, and the attribute
    /// references it.
    fn write_paint(&mut self, paint: &Paint, attr: &str, opacity_attr: &str) {
        match paint {
            Paint::Solid(c) => write_color(*c, attr, opacity_attr, &mut self.body),
            Paint::Gradient(g) => {
                let id = self.push_gradient(g);
                _ = write!(self.body, " {attr}=\"url(#{}p{id})\"", self.prefix);
            }
        }
    }

    /// Writes `g` into `defs` and returns the number of its `pn` id. The
    /// coordinates are in user space, as in the other renderers.
    fn push_gradient(&mut self, g: &Gradient) -> usize {
        let id = self.gradients;
        self.gradients += 1;
        let prefix = &self.prefix;
        let defs = &mut self.defs;
        match g.geom {
            GradientGeom::Linear { x0, y0, x1, y1 } => {
                _ = write!(
                    defs,
                    "<linearGradient id=\"{prefix}p{id}\" gradientUnits=\"userSpaceOnUse\" \
                     x1=\"{x0}\" y1=\"{y0}\" x2=\"{x1}\" y2=\"{y1}\""
                );
            }
            GradientGeom::Radial { cx, cy, radius } => {
                _ = write!(
                    defs,
                    "<radialGradient id=\"{prefix}p{id}\" gradientUnits=\"userSpaceOnUse\" \
                     cx=\"{cx}\" cy=\"{cy}\" r=\"{radius}\""
                );
            }
        }
        match g.spread {
            SpreadMode::Pad => {}
            SpreadMode::Reflect => defs.push_str(" spreadMethod=\"reflect\""),
            SpreadMode::Repeat => defs.push_str(" spreadMethod=\"repeat\""),
        }
        defs.push('>');
        for stop in &g.stops {
            _ = write!(defs, "<stop offset=\"{}\"", stop.offset);
            write_color(stop.color, "stop-color", "stop-opacity", defs);
            defs.push_str("/>");
        }
        match g.geom {
            GradientGeom::Linear { .. } => defs.push_str("</linearGradient>\n"),
            GradientGeom::Radial { .. } => defs.push_str("</radialGradient>\n"),
        }
        id
    }

    /// The id of `glyph` in `defs`, which receives the outline the first time.
    /// `None` for a glyph with an empty outline.
    fn glyph_id(&mut self, glyph: Glyph) -> Option<usize> {
        if let Some(&id) = self.glyphs.get(&glyph) {
            return id;
        }
        let id = self.glyph_defs;
        let start = self.defs.len();
        _ = write!(self.defs, "<path id=\"{}g{id}\" d=\"", self.prefix);
        let mut data = PathData::new(&mut self.defs);
        glyph.outline(0.0, 0.0, &mut data);
        let id = if data.empty {
            self.defs.truncate(start);
            None
        } else {
            self.defs.push_str("\"/>\n");
            self.glyph_defs += 1;
            Some(id)
        };
        self.glyphs.insert(glyph, id);
        id
    }
}

/// Writes ` attr="#rrggbb"`, and ` opacity_attr="a"` when the alpha is below
/// 1.
fn write_color(c: Rgba, attr: &str, opacity_attr: &str, out: &mut String) {
    _ = write!(out, " {attr}=\"{}\"", Hex(c));
    if c.a < 1.0 {
        _ = write!(out, " {opacity_attr}=\"{}\"", c.a.max(0.0));
    }
}

/// The color of an [`Rgba`] as `#rrggbb`.
struct Hex(Rgba);

impl fmt::Display for Hex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Rgba { r, g, b, .. } = self.0;
        write!(f, "#{r:02x}{g:02x}{b:02x}")
    }
}

fn write_list(values: &[f32], out: &mut String) {
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        _ = write!(out, "{v}");
    }
}

/// Writes `segments` as path data. SVG has a quadratic command, so a
/// quadratic stays one.
fn write_segments(segments: Segments<'_>, out: &mut String) {
    segments.outline(&mut PathData::new(out));
}

fn render_text(node: &Text, canvas: &mut SvgRenderer) {
    let do_fill = node.draws_fill();
    let do_stroke = node.draws_stroke();
    if !do_fill && !do_stroke {
        return;
    }
    let Some(layout) = TextLayout::new(&node.spec) else {
        return;
    };

    let mut uses = std::mem::take(&mut canvas.uses);
    uses.clear();
    uses.extend(
        layout
            .placed_glyphs()
            .filter_map(|(glyph, x)| Some((canvas.glyph_id(glyph)?, x))),
    );
    let y = layout.baseline_y();

    let [a, b, c, d, e, f] = node.transform;
    let body = &mut canvas.body;
    _ = writeln!(body, "<g transform=\"matrix({a} {b} {c} {d} {e} {f})\">");
    // The fills of all the glyphs go down before any stroke, as in the other
    // renderers, which paint the text as one path. A <use> that fills and
    // strokes would cover the stroke of the glyph before it.
    if do_fill {
        body.push_str("<g");
        write_color(node.fill, "fill", "fill-opacity", body);
        body.push_str(">\n");
        write_uses(&uses, &canvas.prefix, y, body);
        body.push_str("</g>\n");
    }
    if do_stroke {
        body.push_str("<g fill=\"none\"");
        write_text_stroke(node, body);
        body.push_str(">\n");
        write_uses(&uses, &canvas.prefix, y, body);
        body.push_str("</g>\n");
    }
    if node.underline {
        // The underline paints on its own, as in the other renderers.
        body.push_str("<path d=\"");
        layout.outline_underline(&mut PathData::new(body));
        body.push('"');
        if do_fill {
            write_color(node.fill, "fill", "fill-opacity", body);
        } else {
            body.push_str(" fill=\"none\"");
        }
        if do_stroke {
            write_text_stroke(node, body);
        }
        body.push_str("/>\n");
    }
    body.push_str("</g>\n");
    canvas.uses = uses;
}

fn write_text_stroke(node: &Text, out: &mut String) {
    write_color(node.stroke, "stroke", "stroke-opacity", out);
    _ = write!(
        out,
        " stroke-width=\"{}\" stroke-miterlimit=\"{TEXT_MITER_LIMIT}\"",
        node.stroke_width
    );
}

fn write_uses(uses: &[(usize, f32)], prefix: &str, y: f32, out: &mut String) {
    for (id, x) in uses {
        _ = writeln!(
            out,
            "<use xlink:href=\"#{prefix}g{id}\" x=\"{x}\" y=\"{y}\"/>"
        );
    }
}

/// Path data, written as it arrives, with a space between two commands.
struct PathData<'a> {
    d: &'a mut String,
    /// `true` until the first command.
    empty: bool,
}

impl<'a> PathData<'a> {
    fn new(d: &'a mut String) -> Self {
        Self { d, empty: true }
    }

    /// Writes the space before a command that is not the first.
    fn separate(&mut self) -> &mut String {
        if !self.empty {
            self.d.push(' ');
        }
        self.empty = false;
        self.d
    }
}

impl PathSink for PathData<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        _ = write!(self.separate(), "M{x} {y}");
    }
    fn line_to(&mut self, x: f32, y: f32) {
        _ = write!(self.separate(), "L{x} {y}");
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        _ = write!(self.separate(), "Q{cx} {cy} {x} {y}");
    }
    fn cubic_to(&mut self, cx1: f32, cy1: f32, cx2: f32, cy2: f32, x: f32, y: f32) {
        _ = write!(self.separate(), "C{cx1} {cy1} {cx2} {cy2} {x} {y}");
    }
    fn close(&mut self) {
        self.separate().push('Z');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::tests::rect;
    use crate::scene::{Dash, PathStyle, Scene, Stop, TextSpec};

    fn red_fill(a: f32) -> PathStyle {
        PathStyle {
            fill: Paint::rgba(255, 0, 0, a),
            ..PathStyle::default()
        }
    }

    fn black() -> Rgba {
        Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: 1.0,
        }
    }

    fn text(s: &str) -> Text {
        Text {
            fill: black(),
            spec: TextSpec {
                size: 16.0,
                text: s.to_owned(),
                ..TextSpec::default()
            },
            ..Text::default()
        }
    }

    #[test]
    fn an_empty_scene_is_a_document_of_its_size() {
        assert_eq!(
            render_to_svg(&Scene::new(100.0, 50.0)),
            "<svg xmlns=\"http://www.w3.org/2000/svg\" \
             xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
             width=\"100\" height=\"50\" viewBox=\"0 0 100 50\">\n</svg>\n"
        );
    }

    #[test]
    fn a_path_writes_its_segments_and_its_fill() {
        let mut scene = Scene::new(100.0, 50.0);
        rect(&mut scene, red_fill(0.5), 0.0, 0.0, 100.0, 50.0);
        let svg = render_to_svg(&scene);
        assert!(
            svg.contains(
                "<path d=\"M0 0 L100 0 L100 50 L0 50\" fill=\"#ff0000\" fill-opacity=\"0.5\"/>"
            ),
            "{svg}"
        );
    }

    #[test]
    fn a_quadratic_stays_a_quadratic() {
        let mut scene = Scene::new(10.0, 10.0);
        scene
            .path(red_fill(1.0), 0.0, 0.0)
            .quad_to(5.0, 10.0, 10.0, 0.0);
        let svg = render_to_svg(&scene);
        assert!(svg.contains("d=\"M0 0 Q5 10 10 0\""), "{svg}");
    }

    #[test]
    fn a_stroke_writes_its_width_cap_join_and_dash() {
        let mut scene = Scene::new(50.0, 50.0);
        let style = PathStyle {
            stroke: Paint::rgba(0, 0, 0, 1.0),
            stroke_width: 2.0,
            line_cap: LineCap::Round,
            line_join: LineJoin::Bevel,
            dash: Dash::new(vec![3.0, 2.0], 1.0).map(Box::new),
            closed: true,
            ..PathStyle::default()
        };
        rect(&mut scene, style, 5.0, 5.0, 40.0, 40.0);
        let svg = render_to_svg(&scene);
        assert!(
            svg.contains(
                " Z\" fill=\"none\" stroke=\"#000000\" stroke-width=\"2\" \
                 stroke-linecap=\"round\" stroke-linejoin=\"bevel\" \
                 stroke-dasharray=\"3 2\" stroke-dashoffset=\"1\"/>"
            ),
            "{svg}"
        );
    }

    #[test]
    fn only_a_miter_limit_other_than_the_default_is_written() {
        let svg = |miter_limit: f32| {
            let mut scene = Scene::new(50.0, 50.0);
            let style = PathStyle {
                stroke: Paint::rgba(0, 0, 0, 1.0),
                stroke_width: 4.0,
                miter_limit,
                ..PathStyle::default()
            };
            rect(&mut scene, style, 5.0, 5.0, 40.0, 40.0);
            render_to_svg(&scene)
        };
        assert!(svg(12.0).contains(" stroke-miterlimit=\"12\""));
        assert!(!svg(DEFAULT_MITER_LIMIT).contains("miterlimit"));
    }

    #[test]
    fn a_gradient_goes_into_defs_with_its_spread_and_stop_opacity() {
        let mut scene = Scene::new(50.0, 50.0);
        let gradient = Gradient::linear(
            0.0,
            0.0,
            50.0,
            0.0,
            vec![
                Stop {
                    offset: 0.0,
                    color: Rgba {
                        r: 255,
                        g: 0,
                        b: 0,
                        a: 1.0,
                    },
                },
                Stop {
                    offset: 1.0,
                    color: Rgba {
                        r: 0,
                        g: 0,
                        b: 255,
                        a: 0.5,
                    },
                },
            ],
        )
        .with_spread(SpreadMode::Reflect);
        let style = PathStyle {
            fill: Paint::gradient(gradient),
            ..PathStyle::default()
        };
        rect(&mut scene, style, 0.0, 0.0, 50.0, 50.0);
        let svg = render_to_svg(&scene);
        assert!(
            svg.contains(
                "<linearGradient id=\"p0\" gradientUnits=\"userSpaceOnUse\" \
                 x1=\"0\" y1=\"0\" x2=\"50\" y2=\"0\" spreadMethod=\"reflect\">\
                 <stop offset=\"0\" stop-color=\"#ff0000\"/>\
                 <stop offset=\"1\" stop-color=\"#0000ff\" stop-opacity=\"0.5\"/>\
                 </linearGradient>"
            ),
            "{svg}"
        );
        assert!(svg.contains(" fill=\"url(#p0)\""), "{svg}");
    }

    #[test]
    fn a_nested_clip_nests_its_group() {
        let square = |fill_rule| {
            ClipPath::builder(fill_rule, 0.0, 0.0)
                .line_to(10.0, 0.0)
                .line_to(10.0, 10.0)
                .line_to(0.0, 10.0)
                .build()
        };
        let mut scene = Scene::new(20.0, 20.0);
        {
            let mut outer = scene.clip(square(FillRule::EvenOdd));
            let mut inner = outer.clip(square(FillRule::NonZero));
            rect(&mut inner, red_fill(1.0), 0.0, 0.0, 20.0, 20.0);
        }
        let svg = render_to_svg(&scene);
        assert!(
            svg.contains(
                "<clipPath id=\"c0\"><path d=\"M0 0 L10 0 L10 10 L0 10\" \
                 clip-rule=\"evenodd\"/></clipPath>\n\
                 <clipPath id=\"c1\"><path d=\"M0 0 L10 0 L10 10 L0 10\"/></clipPath>\n"
            ),
            "{svg}"
        );
        assert!(
            svg.contains(
                "<g clip-path=\"url(#c0)\">\n<g clip-path=\"url(#c1)\">\n<path d=\"M0 0 \
                 L20 0 L20 20 L0 20\" fill=\"#ff0000\"/>\n</g>\n</g>\n"
            ),
            "{svg}"
        );
    }

    #[test]
    fn a_repeated_glyph_goes_into_defs_once() {
        let mut scene = Scene::new(60.0, 20.0);
        scene.text(text("aaa"));
        let svg = render_to_svg(&scene);
        assert_eq!(svg.matches("<path id=\"g").count(), 1, "{svg}");
        assert_eq!(svg.matches("<use ").count(), 3, "{svg}");
        assert!(!svg.contains("<text"), "{svg}");
    }

    #[test]
    fn a_space_places_no_glyph() {
        let mut scene = Scene::new(60.0, 20.0);
        scene.text(text("a a"));
        assert_eq!(render_to_svg(&scene).matches("<use ").count(), 2);
    }

    #[test]
    fn a_stroked_text_fills_every_glyph_before_any_stroke() {
        let mut scene = Scene::new(60.0, 20.0);
        scene.text(Text {
            stroke: black(),
            stroke_width: 1.0,
            ..text("ab")
        });
        let svg = render_to_svg(&scene);
        let fills = svg.find("<g fill=\"#000000\">").expect("a fill group");
        let strokes = svg
            .find("<g fill=\"none\" stroke=\"#000000\"")
            .expect("a stroke group");
        assert!(fills < strokes, "{svg}");
        assert_eq!(svg[fills..strokes].matches("<use ").count(), 2, "{svg}");
        assert_eq!(svg[strokes..].matches("<use ").count(), 2, "{svg}");
    }

    #[test]
    fn render_stream_matches_render() {
        let mut scene = Scene::new(40.0, 40.0);
        {
            let clip = ClipPath::builder(FillRule::NonZero, 0.0, 0.0)
                .line_to(20.0, 0.0)
                .line_to(20.0, 20.0)
                .build();
            let mut clipped = scene.clip(clip);
            rect(&mut clipped, red_fill(0.5), 0.0, 0.0, 40.0, 40.0);
            clipped.text(text("Hi"));
        }
        let bytes = crate::wire::encode_frame(&scene);
        let mut r = SvgRenderer::new();
        let streamed = r.render_stream(&bytes[..]).expect("decode + render");
        assert_eq!(streamed, render_to_svg(&scene));
    }

    #[test]
    fn a_second_render_starts_a_new_document() {
        let mut scene = Scene::new(20.0, 20.0);
        scene.text(text("a"));
        let mut r = SvgRenderer::new();
        let first = r.render(&scene).expect("render").to_owned();
        let second = r.render(&scene).expect("render");
        assert_eq!(first, second);
        assert_eq!(second.matches("<path id=\"g0\"").count(), 1);
    }

    #[test]
    fn an_id_prefix_starts_every_id_and_every_reference() {
        let mut scene = Scene::new(40.0, 40.0);
        let stops = vec![Stop {
            offset: 0.0,
            color: black(),
        }];
        {
            let clip = ClipPath::builder(FillRule::NonZero, 0.0, 0.0)
                .line_to(20.0, 0.0)
                .line_to(20.0, 20.0)
                .build();
            let mut clipped = scene.clip(clip);
            let style = PathStyle {
                fill: Paint::gradient(Gradient::radial(20.0, 20.0, 10.0, stops)),
                ..PathStyle::default()
            };
            rect(&mut clipped, style, 0.0, 0.0, 40.0, 40.0);
            clipped.text(text("a"));
        }
        let mut r = SvgRenderer::with_id_prefix("fig1-").expect("a valid prefix");
        let svg = r.render(&scene).expect("render");
        for part in [
            "id=\"fig1-p0\"",
            "id=\"fig1-c0\"",
            "id=\"fig1-g0\"",
            "url(#fig1-p0)",
            "url(#fig1-c0)",
            "href=\"#fig1-g0\"",
        ] {
            assert!(svg.contains(part), "{part} in {svg}");
        }
    }

    #[test]
    fn an_id_prefix_that_cannot_start_an_xml_id_is_rejected() {
        for prefix in ["1a", "-a", "a b", "a\"", "a#"] {
            assert!(SvgRenderer::with_id_prefix(prefix).is_none(), "{prefix}");
        }
        assert!(SvgRenderer::with_id_prefix("_a-1").is_some());
    }
}
