//! Render a [`crate::scene::Scene`] to an SVG document.
//!
//! SVG and the draw list share one space, CSS pixels with y down and the
//! origin at the top left, so coordinates go in as they are.
//!
//! Glyphs go in as filled paths from [`crate::text`], as in the other
//! renderers, so the document needs no font and text measures the same in
//! all of them. The SVG of Racket's `2htdp/image` does the same. Each glyph
//! of a face at a size goes into `<defs>` once, and every occurrence is a
//! `<use>`. A text with a gradient goes in as one path instead, because a
//! gradient on a `<use>` starts again at each glyph. The text is not
//! selectable.
//!
//! A bitmap is an `<image>` with the file of its image in a data URL, in
//! `<defs>` once a frame and a `<use>` for each bitmap. A PNG and an
//! upright JPEG go in as they are. With the feature `render`, any other
//! image goes in as a PNG, so a GIF does not move and every viewer shows
//! it.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::{self, Write};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

use crate::asset::{Embed, embed};
use crate::outline::PathSink;
use crate::renderer::{
    AllocError, MISSING_FILL, MISSING_STROKE, Renderer, RestoreOnDrop, TEXT_MITER_LIMIT,
    frame_side, missing_box, missing_cross, sealed::Canvas,
};
use crate::scene::{
    Bitmap, ClipPath, DEFAULT_MITER_LIMIT, FillRule, Gradient, GradientGeom, Image, LineCap,
    LineJoin, Paint, Path, Rgba, Sampling, SpreadMode, Text,
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
    /// The number of each image of the frame in `defs`, and `None` for one
    /// that does not go in.
    images: HashMap<Image, Option<usize>>,
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
            images: HashMap::new(),
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

/// The file of `image` as a data URL, or `None` if it does not decode.
fn data_url(image: &Image) -> Option<String> {
    let file = image.file();
    let (mime, file) = match embed(file).ok()? {
        Embed::Png => ("image/png", Cow::Borrowed(file)),
        Embed::Jpeg { .. } => ("image/jpeg", Cow::Borrowed(file)),
        Embed::Decode { mime } => as_png(file, mime)?,
    };
    let mut url = format!("data:{mime};base64,");
    B64.encode_string(&file, &mut url);
    Some(url)
}

/// The image in `file` as a PNG, the way up that the pixmap draws it.
#[cfg(feature = "render")]
fn as_png<'a>(file: &'a [u8], _mime: &'static str) -> Option<(&'static str, Cow<'a, [u8]>)> {
    let image = crate::asset::decode(file, crate::asset::MAX_IMAGE_PIXELS).ok()?;
    Some(("image/png", Cow::Owned(image.encode_png().ok()?)))
}

/// Without a decoder, the image goes in as it is, of the media type `mime`.
#[cfg(not(feature = "render"))]
fn as_png<'a>(file: &'a [u8], mime: &'static str) -> Option<(&'static str, Cow<'a, [u8]>)> {
    Some((mime, Cow::Borrowed(file)))
}

impl Canvas for SvgRenderer {
    /// Starts a new document. Nothing here allocates a surface, so it never
    /// fails.
    fn ensure_size(&mut self, width: f32, height: f32) -> Result<(), AllocError> {
        self.width = frame_side(width);
        self.height = frame_side(height);
        self.defs.clear();
        self.body.clear();
        self.glyphs.clear();
        self.glyph_defs = 0;
        self.gradients = 0;
        self.clips = 0;
        self.images.clear();
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
        path.outline(&mut PathData::new(&mut self.body));
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
        let Some(layout) = TextLayout::new(&node.spec) else {
            return;
        };
        let inverse = invert(node.transform);
        let fill = node
            .draws_fill()
            .then(|| self.text_paint(&node.fill, "fill", "fill-opacity", inverse))
            .flatten();
        let stroke = node
            .draws_stroke()
            .then(|| self.text_paint(&node.stroke, "stroke", "stroke-opacity", inverse))
            .flatten()
            .map(|mut attrs| {
                _ = write!(
                    attrs,
                    " stroke-width=\"{}\" stroke-miterlimit=\"{TEXT_MITER_LIMIT}\"",
                    node.stroke_width
                );
                attrs
            });
        if fill.is_none() && stroke.is_none() {
            return;
        }
        let is_gradient = |paint: &Paint, drawn: bool| drawn && matches!(paint, Paint::Gradient(_));
        let gradient =
            is_gradient(&node.fill, fill.is_some()) || is_gradient(&node.stroke, stroke.is_some());
        let (fill, stroke) = (fill.as_deref(), stroke.as_deref());
        let path_attrs = format!(
            "{}{}",
            fill.unwrap_or(" fill=\"none\""),
            stroke.unwrap_or_default()
        );

        let [a, b, c, d, e, f] = node.transform;
        _ = writeln!(
            self.body,
            "<g transform=\"matrix({a} {b} {c} {d} {e} {f})\">"
        );
        if gradient {
            // A gradient on a <use> starts again at the x of each glyph, so
            // the glyphs go into one path.
            self.body.push_str("<path d=\"");
            layout.outline(&mut PathData::new(&mut self.body));
            _ = writeln!(self.body, "\"{path_attrs}/>");
        } else {
            self.write_glyph_uses(&layout, fill, stroke);
        }
        if node.underline {
            // The underline paints on its own, as in the other renderers.
            self.body.push_str("<path d=\"");
            layout.outline_underline(&mut PathData::new(&mut self.body));
            _ = writeln!(self.body, "\"{path_attrs}/>");
        }
        self.body.push_str("</g>\n");
    }

    /// An image that does not decode draws a gray box with a red cross in
    /// its place, as in the pixmap.
    fn draw_bitmap(&mut self, bitmap: &Bitmap) {
        let prefix = &self.prefix;
        let next = self.images.len();
        let id = *self.images.entry(bitmap.image.clone()).or_insert_with(|| {
            let url = data_url(&bitmap.image)?;
            _ = writeln!(
                self.defs,
                "<image id=\"{prefix}i{next}\" x=\"-0.5\" y=\"-0.5\" width=\"1\" height=\"1\" \
                 preserveAspectRatio=\"none\" xlink:href=\"{url}\"/>"
            );
            Some(next)
        });
        let Some(id) = id else {
            write_missing(bitmap.transform, &mut self.body);
            return;
        };
        _ = write!(
            self.body,
            "<use xlink:href=\"#{prefix}i{id}\" transform=\"matrix("
        );
        write_list(&bitmap.transform, &mut self.body);
        self.body.push_str(")\"");
        if bitmap.sampling == Sampling::Nearest {
            // SVG 1.1 names nearest sampling optimizeSpeed, and CSS names it
            // pixelated, which a browser takes over the attribute.
            self.body
                .push_str(" image-rendering=\"optimizeSpeed\" style=\"image-rendering:pixelated\"");
        }
        self.body.push_str("/>\n");
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
    /// that references it. A nested group intersects the clips. A clip with
    /// no segments writes an empty `d`, which covers nothing, so it hides
    /// what it holds, as in the other backends.
    fn with_clip<T>(&mut self, clip: &ClipPath, inside: impl FnOnce(&mut Self) -> T) -> T {
        let id = self.clips;
        self.clips += 1;
        let prefix = &self.prefix;
        _ = write!(self.defs, "<clipPath id=\"{prefix}c{id}\"><path d=\"");
        clip.segments().outline(&mut PathData::new(&mut self.defs));
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

    fn with_layer<T>(&mut self, opacity: f32, inside: impl FnOnce(&mut Self) -> T) -> T {
        _ = writeln!(self.body, "<g opacity=\"{opacity}\">");
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
                let id = self.push_gradient(g, None);
                _ = write!(self.body, " {attr}=\"url(#{}p{id})\"", self.prefix);
            }
        }
    }

    /// The attributes of one side of a text. A gradient takes `inverse`, the
    /// inverse of the text transform, so it stays in canvas space. Returns
    /// `None` for a gradient with no `inverse`, which squashes the text to a
    /// line.
    fn text_paint(
        &mut self,
        paint: &Paint,
        attr: &str,
        opacity_attr: &str,
        inverse: Option<[f32; 6]>,
    ) -> Option<String> {
        let mut attrs = String::new();
        match paint {
            Paint::Solid(c) => write_color(*c, attr, opacity_attr, &mut attrs),
            Paint::Gradient(g) => {
                let id = self.push_gradient(g, Some(inverse?));
                _ = write!(attrs, " {attr}=\"url(#{}p{id})\"", self.prefix);
            }
        }
        Some(attrs)
    }

    /// Writes a `<use>` of each glyph of `layout`, all the fills first and
    /// then all the strokes, as the other renderers paint the text as one
    /// path. A <use> that fills and strokes would cover the stroke of the
    /// glyph before it.
    fn write_glyph_uses(
        &mut self,
        layout: &TextLayout<'_>,
        fill: Option<&str>,
        stroke: Option<&str>,
    ) {
        let mut uses = std::mem::take(&mut self.uses);
        uses.clear();
        uses.extend(
            layout
                .placed_glyphs()
                .filter_map(|(glyph, x)| Some((self.glyph_id(glyph)?, x))),
        );
        let y = layout.baseline_y();
        let body = &mut self.body;
        // The fills of all the glyphs go down before any stroke, as in the
        // other renderers, which paint the text as one path. A <use> that
        // fills and strokes would cover the stroke of the glyph before it.
        if let Some(fill) = fill {
            _ = writeln!(body, "<g{fill}>");
            write_uses(&uses, &self.prefix, y, body);
            body.push_str("</g>\n");
        }
        if let Some(stroke) = stroke {
            _ = writeln!(body, "<g fill=\"none\"{stroke}>");
            write_uses(&uses, &self.prefix, y, body);
            body.push_str("</g>\n");
        }
        self.uses = uses;
    }

    /// Writes `g` into `defs` and returns the number of its `pn` id. The
    /// coordinates are in user space, as in the other renderers, moved by
    /// `transform` when there is one.
    fn push_gradient(&mut self, g: &Gradient, transform: Option<[f32; 6]>) -> usize {
        let id = self.gradients;
        self.gradients += 1;
        let prefix = &self.prefix;
        let defs = &mut self.defs;
        match g.geom() {
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
        if let Some([a, b, c, d, e, f]) = transform {
            _ = write!(
                defs,
                " gradientTransform=\"matrix({a} {b} {c} {d} {e} {f})\""
            );
        }
        match g.spread() {
            SpreadMode::Pad => {}
            SpreadMode::Reflect => defs.push_str(" spreadMethod=\"reflect\""),
            SpreadMode::Repeat => defs.push_str(" spreadMethod=\"repeat\""),
        }
        defs.push('>');
        for stop in g.stops() {
            _ = write!(defs, "<stop offset=\"{}\"", stop.offset);
            write_color(stop.color, "stop-color", "stop-opacity", defs);
            defs.push_str("/>");
        }
        match g.geom() {
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

/// Writes the gray box with a red cross that stands for a bitmap of
/// `transform` whose image does not decode.
fn write_missing(transform: [f32; 6], out: &mut String) {
    out.push_str("<path d=\"");
    let mut d = PathData::new(out);
    missing_box(transform, &mut d);
    missing_cross(transform, &mut d);
    out.push('"');
    write_color(MISSING_FILL, "fill", "fill-opacity", out);
    write_color(MISSING_STROKE, "stroke", "stroke-opacity", out);
    out.push_str("/>\n");
}

fn write_list(values: &[f32], out: &mut String) {
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        _ = write!(out, "{v}");
    }
}

/// The inverse of the affine `m`, in the convention of [`Text::transform`],
/// or `None` when `m` has none.
fn invert(m: [f32; 6]) -> Option<[f32; 6]> {
    let affine = kurbo::Affine::new(m.map(f64::from));
    if affine.determinant() == 0.0 {
        return None;
    }
    let inverse = affine.inverse().as_coeffs().map(|v| v as f32);
    inverse.iter().all(|v| v.is_finite()).then_some(inverse)
}

fn write_uses(uses: &[(usize, f32)], prefix: &str, y: f32, out: &mut String) {
    for (id, x) in uses {
        _ = writeln!(
            out,
            "<use xlink:href=\"#{prefix}g{id}\" x=\"{x}\" y=\"{y}\"/>"
        );
    }
}

/// Path data, written as it arrives, with a space between two commands. SVG
/// has a quadratic command, so a quadratic stays one.
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

    fn bitmap(image: Image, sampling: Sampling) -> Bitmap {
        Bitmap {
            image,
            transform: [10.0, 0.0, 0.0, 10.0, 10.0, 10.0],
            sampling,
        }
    }

    #[test]
    fn a_bitmap_is_a_use_of_its_image_in_defs() {
        let mut scene = Scene::new(20.0, 20.0);
        scene.add_bitmap(bitmap(crate::asset::png_image(3, 3), Sampling::Smooth));
        scene.add_bitmap(bitmap(crate::asset::png_image(2, 2), Sampling::Smooth));
        scene.add_bitmap(bitmap(crate::asset::png_image(2, 2), Sampling::Nearest));
        let svg = render_to_svg(&scene);
        assert_eq!(svg.matches("<image ").count(), 2, "{svg}");
        assert!(
            svg.contains("xlink:href=\"data:image/png;base64,iVBORw0KGgo"),
            "{svg}"
        );
        let uses = "<use xlink:href=\"#i1\" transform=\"matrix(10 0 0 10 10 10)\"";
        assert_eq!(svg.matches(uses).count(), 2, "{svg}");
        assert_eq!(svg.matches("optimizeSpeed").count(), 1, "{svg}");
    }

    #[cfg(feature = "render")]
    #[test]
    fn a_bitmap_whose_image_does_not_decode_is_a_gray_box_with_a_red_cross() {
        let mut scene = Scene::new(20.0, 20.0);
        let gif = Image::new(b"GIF89a\x01\0\x01\0\0\0\0\x2c\0\0\0\0\x01\0\x01\0".to_vec()).unwrap();
        scene.add_bitmap(bitmap(gif, Sampling::Smooth));
        let svg = render_to_svg(&scene);
        let box_and_cross = "<path d=\"M5 5 L15 5 L15 15 L5 15 Z M5 5 L15 15 M15 5 L5 15\" \
                             fill=\"#c8c8c8\" stroke=\"#c80000\"/>";
        assert!(svg.contains(box_and_cross), "{svg}");
    }

    #[cfg(feature = "render")]
    #[test]
    fn a_gif_goes_in_as_a_png() {
        let mut gif = std::io::Cursor::new(Vec::new());
        image::RgbaImage::new(2, 2)
            .write_to(&mut gif, image::ImageFormat::Gif)
            .unwrap();
        let url = data_url(&Image::new(gif.into_inner()).unwrap()).unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
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
            fill: Paint::Solid(black()),
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
        scene.add_path(rect(red_fill(0.5), 0.0, 0.0, 100.0, 50.0));
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
        scene.add_path(rect(style, 5.0, 5.0, 40.0, 40.0));
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
            scene.add_path(rect(style, 5.0, 5.0, 40.0, 40.0));
            render_to_svg(&scene)
        };
        assert!(svg(12.0).contains(" stroke-miterlimit=\"12\""));
        assert!(!svg(DEFAULT_MITER_LIMIT).contains("miterlimit"));
    }

    #[test]
    fn a_gradient_goes_into_defs_with_its_spread_and_stop_opacity() {
        let mut scene = Scene::new(50.0, 50.0);
        let gradient = Paint::linear(
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
            fill: gradient,
            ..PathStyle::default()
        };
        scene.add_path(rect(style, 0.0, 0.0, 50.0, 50.0));
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
        scene.clip(square(FillRule::EvenOdd), |outer| {
            outer.clip(square(FillRule::NonZero), |inner| {
                inner.add_path(rect(red_fill(1.0), 0.0, 0.0, 20.0, 20.0));
            });
        });
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
        scene.add_text(text("aaa"));
        let svg = render_to_svg(&scene);
        assert_eq!(svg.matches("<path id=\"g").count(), 1, "{svg}");
        assert_eq!(svg.matches("<use ").count(), 3, "{svg}");
        assert!(!svg.contains("<text"), "{svg}");
    }

    #[test]
    fn a_space_places_no_glyph() {
        let mut scene = Scene::new(60.0, 20.0);
        scene.add_text(text("a a"));
        assert_eq!(render_to_svg(&scene).matches("<use ").count(), 2);
    }

    #[test]
    fn a_stroked_text_fills_every_glyph_before_any_stroke() {
        let mut scene = Scene::new(60.0, 20.0);
        scene.add_text(Text {
            stroke: Paint::Solid(black()),
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
    fn a_gradient_text_is_one_path_with_the_inverse_of_its_transform() {
        let mut scene = Scene::new(60.0, 20.0);
        let stops = vec![
            Stop {
                offset: 0.0,
                color: black(),
            },
            Stop {
                offset: 1.0,
                color: Rgba::default(),
            },
        ];
        let node = Text {
            fill: Paint::linear(0.0, 0.0, 60.0, 0.0, stops),
            transform: [2.0, 0.0, 0.0, 2.0, 10.0, 4.0],
            ..text("ab")
        };
        scene.add_text(node);
        let svg = render_to_svg(&scene);
        // A <use> would start the gradient again at each glyph.
        assert!(!svg.contains("<use "), "{svg}");
        assert!(svg.contains("fill=\"url(#p0)\""), "{svg}");
        assert!(
            svg.contains("gradientTransform=\"matrix(0.5 -0 -0 0.5 -5 -2)\""),
            "{svg}"
        );
    }

    #[test]
    fn a_layer_is_a_group_with_its_opacity() {
        let mut scene = Scene::new(20.0, 20.0);
        scene.layer(0.25, |layer| layer.add_text(text("a")));
        let svg = render_to_svg(&scene);
        let open = svg.find("<g opacity=\"0.25\">").expect("a layer group");
        let glyph = svg.find("<use ").expect("a glyph");
        assert!(open < glyph && svg[glyph..].contains("</g>\n</g>"), "{svg}");
    }

    #[test]
    fn a_second_render_starts_a_new_document() {
        let mut scene = Scene::new(20.0, 20.0);
        scene.add_text(text("a"));
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
            scene.clip(clip, |clipped| {
                let style = PathStyle {
                    fill: Paint::radial(20.0, 20.0, 10.0, stops),
                    ..PathStyle::default()
                };
                clipped.add_path(rect(style, 0.0, 0.0, 40.0, 40.0));
                clipped.add_text(text("a"));
            });
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

    #[test]
    fn a_size_that_is_not_finite_draws_as_zero() {
        let zero = render_to_svg(&Scene::new(0.0, 0.0));
        assert_eq!(render_to_svg(&Scene::new(f32::INFINITY, f32::NAN)), zero);
        let (width, height) = (777.0, 778.0);
        let bytes = crate::wire::scene::encode(&Scene::new(width, height), &|_| 0);
        let bytes = crate::wire::with_float(&bytes, width, f32::INFINITY);
        let bytes = crate::wire::with_float(&bytes, height, f32::NAN);
        let scene = crate::wire::scene::decode(&bytes, &|_| None).expect("decode");
        assert_eq!(render_to_svg(&scene), zero);
    }
}
