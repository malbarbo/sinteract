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
//!
//! A gradient draws with [`SpreadMode::Pad`](crate::scene::SpreadMode),
//! whichever it asks for. A PDF shading extends its two ends and has no
//! other way to repeat, so reflect and repeat need a sampled function,
//! which this backend does not write. A frame that uses them differs here
//! from the raster and the svg.
//!
//! A bitmap is an image XObject, written once for each image and sampling
//! of the frame. An upright JPEG goes in as its file, and any other image
//! goes in decoded, with its alpha in a soft mask.
//!
//! A shading has no alpha. A gradient whose stops differ in alpha draws its
//! colors under a soft mask, a gray shading of its alphas, as Cairo and Skia
//! do.

use std::collections::{BTreeMap, HashMap};

use pdf_writer::types::{FunctionShadingType, LineCapStyle, LineJoinStyle, MaskType};
use pdf_writer::writers::{ColorSpace, Resources};
use pdf_writer::{Content, Filter, Finish, Name, Pdf, Rect, Ref};

use crate::asset::{Embed, JpegColor, MAX_IMAGE_PIXELS, embed};
use crate::outline::PathSink;
use crate::renderer::{
    AllocError, MISSING_FILL, MISSING_STROKE, Renderer, RestoreOnDrop, frame_side, sealed::Canvas,
    unit_square,
};
use crate::scene::{
    Bitmap, ClipPath, FillRule, Gradient, GradientGeom, Image, LineCap, LineJoin, Paint, Path,
    Rgba, Sampling, Stop, Text,
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
    /// The content under each layer in effect, with the opacity that the
    /// layer draws onto it with. `content` is the top layer.
    layer_stack: Vec<(Content, f32)>,
    /// The content of each layer of the frame, as a form XObject. The index
    /// of a layer is its `/Fmn` name.
    forms: Vec<Vec<u8>>,
    /// Each image of the frame, ready for the PDF, and `None` for one that
    /// does not decode.
    prepared: HashMap<Image, Option<XImage>>,
    /// Each image of the frame with a sampling that a bitmap draws it with.
    /// The index of an image is its `/Imn` name.
    images: Vec<(Image, Sampling)>,
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
            layer_stack: Vec::new(),
            forms: Vec::new(),
            prepared: HashMap::new(),
            images: Vec::new(),
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

/// An image XObject, with the width and the height of its samples.
#[derive(Clone, Debug)]
struct XImage {
    size: (u32, u32),
    samples: Samples,
}

#[derive(Clone, Debug)]
enum Samples {
    /// The file of a JPEG, which a PDF decodes.
    Jpeg { file: Vec<u8>, color: JpegColor },
    /// The colors in RGB and the alphas, each compressed, and no alphas when
    /// every pixel is opaque.
    Deflated {
        rgb: Vec<u8>,
        alpha: Option<Vec<u8>>,
    },
}

impl XImage {
    /// `image` ready for a PDF, or `None` if it does not decode.
    fn of(image: &Image) -> Option<Self> {
        let file = image.file();
        Some(match embed(file).ok()? {
            Embed::Jpeg { color, size } => XImage {
                size,
                samples: Samples::Jpeg {
                    file: file.to_vec(),
                    color,
                },
            },
            Embed::Png | Embed::Decode { .. } => {
                XImage::deflated(&crate::asset::decode(file, MAX_IMAGE_PIXELS).ok()?)
            }
        })
    }

    /// The pixels of `pixmap`, out of premultiplied alpha.
    fn deflated(pixmap: &tiny_skia::Pixmap) -> Self {
        let pixels = pixmap.pixels();
        let mut rgb = Vec::with_capacity(3 * pixels.len());
        let mut alpha = Vec::with_capacity(pixels.len());
        for pixel in pixels {
            let c = pixel.demultiply();
            rgb.extend([c.red(), c.green(), c.blue()]);
            alpha.push(c.alpha());
        }
        let compress = |data: &[u8]| miniz_oxide::deflate::compress_to_vec_zlib(data, 6);
        let opaque = alpha.iter().all(|&a| a == u8::MAX);
        XImage {
            size: (pixmap.width(), pixmap.height()),
            samples: Samples::Deflated {
                rgb: compress(&rgb),
                alpha: (!opaque).then(|| compress(&alpha)),
            },
        }
    }
}

impl Canvas for PdfRenderer {
    /// Starts a new page and writes the base transform. Nothing here
    /// allocates a surface, so it never fails.
    fn ensure_size(&mut self, width: f32, height: f32) -> Result<(), AllocError> {
        self.width = frame_side(width);
        self.height = frame_side(height);
        self.gstates.clear();
        self.gradients.clear();
        self.layer_stack.clear();
        self.forms.clear();
        self.prepared.clear();
        self.images.clear();
        // The last document holds the last content stream, so its size is a
        // good guess at the size of this one.
        let mut content = Content::with_capacity(self.bytes.len());
        content.transform(page_transform(self.height));
        self.content = content;
        Ok(())
    }

    fn draw_path(&mut self, path: &Path) {
        let style = &path.style;
        let fill = style.draws_fill().then_some(&style.fill);
        let stroke = style.draws_stroke().then_some(&style.stroke);
        if fill.is_none() && stroke.is_none() {
            return;
        }
        // A path with no segments must not leave a `q ... Q` with no geometry.
        if path.segments().next().is_none() {
            return;
        }
        for (fill, stroke) in passes(fill, stroke) {
            self.paint_path(path, fill, stroke);
        }
    }

    fn draw_text(&mut self, node: &Text) {
        let fill = node.draws_fill().then_some(&node.fill);
        let stroke = node.draws_stroke().then_some(&node.stroke);
        if fill.is_none() && stroke.is_none() {
            return;
        }
        let Some(layout) = TextLayout::new(&node.spec) else {
            return;
        };
        for (fill, stroke) in passes(fill, stroke) {
            self.paint_text(node, &layout, fill, stroke);
        }
    }

    /// An image that does not decode draws a gray box with a red cross in
    /// its place, as in the pixmap.
    fn draw_bitmap(&mut self, bitmap: &Bitmap) {
        let image = &bitmap.image;
        let prepared = self
            .prepared
            .entry(image.clone())
            .or_insert_with(|| XImage::of(image));
        if prepared.is_none() {
            self.draw_missing(bitmap.transform);
            return;
        }
        let key = (image, bitmap.sampling);
        let idx = match self.images.iter().position(|(i, s)| (i, *s) == key) {
            Some(idx) => idx,
            None => {
                self.images.push((image.clone(), bitmap.sampling));
                self.images.len() - 1
            }
        };
        self.content.save_state();
        self.content.transform(bitmap.transform);
        // An image fills the unit square from the origin with its first row
        // at y = 1, and a bitmap covers the unit square centred on the
        // origin with its first row at y = -0.5.
        self.content.transform([1.0, 0.0, 0.0, -1.0, -0.5, 0.5]);
        self.content.x_object(Name(image_name(idx).as_bytes()));
        self.content.restore_state();
    }

    fn end_frame(&mut self) {
        self.assemble();
    }

    fn with_clip<T>(&mut self, clip: &ClipPath, inside: impl FnOnce(&mut Self) -> T) -> T {
        self.content.save_state();
        if clip.segments().next().is_some() {
            clip.segments()
                .outline(&mut PdfOutline::new(&mut self.content));
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

    /// The layer goes into a transparency group, a form XObject that the
    /// content under it draws with `opacity`.
    fn with_layer<T>(&mut self, opacity: f32, inside: impl FnOnce(&mut Self) -> T) -> T {
        let under = std::mem::replace(&mut self.content, Content::new());
        self.layer_stack.push((under, opacity));
        let guard = RestoreOnDrop {
            canvas: self,
            restore: |c: &mut Self| {
                let Some((under, opacity)) = c.layer_stack.pop() else {
                    return;
                };
                let form = std::mem::replace(&mut c.content, under).finish();
                let idx = c.forms.len();
                c.forms.push(form.into_vec());
                c.content.save_state();
                c.apply_alpha(opacity, opacity);
                c.content.x_object(Name(form_name(idx).as_bytes()));
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
    /// Draws `path` with each side that is `Some`.
    fn paint_path(&mut self, path: &Path, fill: Option<&Paint>, stroke: Option<&Paint>) {
        let style = &path.style;
        self.begin_paint(fill, stroke);
        if stroke.is_some() {
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
        path.outline(&mut PdfOutline::new(&mut self.content));
        paint(
            &mut self.content,
            fill.is_some(),
            stroke.is_some(),
            style.fill_rule,
        );
        // restore_state also resets the pattern color space.
        self.content.restore_state();
    }

    /// Draws the glyphs of `layout`, and the underline of `node`, with each
    /// side that is `Some`.
    fn paint_text(
        &mut self,
        node: &Text,
        layout: &TextLayout,
        fill: Option<&Paint>,
        stroke: Option<&Paint>,
    ) {
        let (do_fill, do_stroke) = (fill.is_some(), stroke.is_some());
        // A pattern and a soft mask map to the space in effect before the
        // `cm` of the text below, so a gradient stays in canvas space.
        self.begin_paint(fill, stroke);
        if do_stroke {
            // The default miter limit is TEXT_MITER_LIMIT.
            self.content.set_line_width(node.stroke_width);
        }
        self.content.transform(node.transform);

        let mut out = PdfOutline::new(&mut self.content);
        layout.outline(&mut out);
        // A text of spaces has no outline, and a paint with no path is
        // an error.
        if !out.empty {
            paint(&mut self.content, do_fill, do_stroke, FillRule::NonZero);
        }

        if node.underline {
            // The underline paints on its own. In one path, a glyph that winds
            // the other way from the rectangle would cancel it where the two
            // cross.
            layout.outline_underline(&mut PdfOutline::new(&mut self.content));
            paint(&mut self.content, do_fill, do_stroke, FillRule::NonZero);
        }
        self.content.restore_state();
    }

    /// Draws the gray box with a red cross that stands for a bitmap of
    /// `transform` whose image does not decode.
    fn draw_missing(&mut self, transform: [f32; 6]) {
        let [p0, p1, p2, p3] = unit_square(transform);
        let c = &mut self.content;
        c.save_state();
        let [r, g, b] = rgb_components(MISSING_FILL);
        c.set_fill_rgb(r, g, b);
        let [r, g, b] = rgb_components(MISSING_STROKE);
        c.set_stroke_rgb(r, g, b);
        c.set_line_width(1.0);
        c.move_to(p0.0, p0.1);
        for (x, y) in [p1, p2, p3] {
            c.line_to(x, y);
        }
        c.close_path();
        c.fill_nonzero_and_stroke();
        c.move_to(p0.0, p0.1);
        c.line_to(p2.0, p2.1);
        c.move_to(p1.0, p1.1);
        c.line_to(p3.0, p3.1);
        c.stroke();
        c.restore_state();
    }

    /// Saves the graphics state, then binds the paint of each side that
    /// draws, `None` for a side that does not, and sets its alpha. PDF
    /// forbids a color operator inside a path object, so this goes before the
    /// path.
    fn begin_paint(&mut self, fill: Option<&Paint>, stroke: Option<&Paint>) {
        self.content.save_state();
        let mut mask = None;
        if let Some(paint) = fill {
            mask = mask.or(self.bind_paint(paint, PaintTarget::Fill));
        }
        if let Some(paint) = stroke {
            mask = mask.or(self.bind_paint(paint, PaintTarget::Stroke));
        }
        match mask {
            // `passes` draws a side with a soft mask alone, because the mask
            // applies to both sides.
            Some(idx) => {
                self.content.set_parameters(Name(mask_name(idx).as_bytes()));
            }
            None => {
                // The stops of a gradient here share one alpha.
                let alpha = |paint: Option<&Paint>| paint.map_or(1.0, |p| p.primary_color().a);
                self.apply_alpha(alpha(fill), alpha(stroke));
            }
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
    /// color space and its `/Pn` name. Returns the index of the gradient if
    /// it draws under a soft mask, `None` otherwise.
    fn bind_paint(&mut self, paint: &Paint, target: PaintTarget) -> Option<usize> {
        let gradient = match paint {
            Paint::Solid(c) => {
                let [r, g, b] = rgb_components(*c);
                match target {
                    PaintTarget::Fill => self.content.set_fill_rgb(r, g, b),
                    PaintTarget::Stroke => self.content.set_stroke_rgb(r, g, b),
                };
                return None;
            }
            Paint::Gradient(g) => g,
        };
        let idx = self.push_gradient(gradient);
        let name = pattern_name(idx);
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
        varying_alpha(gradient).then_some(idx)
    }

    /// Returns the index of `g`, the `n` of its `/Pn` name.
    fn push_gradient(&mut self, g: &Gradient) -> usize {
        let idx = self.gradients.len();
        // A pattern maps to the default space of the page, or of the form of
        // a layer. A form draws where the content opens, in pixels with y
        // down, so a pattern in a form needs no transform.
        let matrix = if self.layer_stack.is_empty() {
            page_transform(self.height)
        } else {
            IDENTITY
        };
        self.gradients.push(Shading::new(g, matrix));
        idx
    }

    /// Writes the page into `bytes` and empties the content. The next
    /// `ensure_size` clears the resources and keeps their capacity.
    fn assemble(&mut self) {
        let w = self.width;
        let h = self.height;
        let gstates = &self.gstates;
        let gradients = &self.gradients;
        let forms = &self.forms;
        let prepared = &self.prepared;
        let images = &self.images;
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
                let functions = (0..g.function_count()).map(|_| alloc()).collect();
                let shading = alloc();
                let pattern = alloc();
                let mask = if g.masked {
                    Some(MaskRefs {
                        functions: (0..g.function_count()).map(|_| alloc()).collect(),
                        shading: alloc(),
                        form: alloc(),
                        gstate: alloc(),
                    })
                } else {
                    None
                };
                GradientRefs {
                    functions,
                    shading,
                    pattern,
                    mask,
                }
            })
            .collect();
        let masks = || {
            gradient_refs
                .iter()
                .enumerate()
                .filter_map(|(i, g)| g.mask.as_ref().map(|m| (i, m)))
        };
        // The page and the forms share one dictionary of resources.
        let resources_id = alloc();
        let form_refs: Vec<Ref> = forms.iter().map(|_| alloc()).collect();
        // Each image, and the soft mask of its alphas if it has one.
        let images: Vec<(&XImage, Sampling, Ref, Option<Ref>)> = images
            .iter()
            .map(|(image, sampling)| {
                let image = prepared
                    .get(image)
                    .and_then(Option::as_ref)
                    .expect("a bitmap lists only an image that decodes");
                let alpha = matches!(image.samples, Samples::Deflated { alpha: Some(_), .. });
                (image, *sampling, alloc(), alpha.then(&mut alloc))
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
            page.pair(Name(b"Resources"), resources_id);
            page.finish();
        }
        {
            let mut resources: Resources<'_> = pdf.indirect(resources_id).start();
            if !gstates.is_empty() || masks().next().is_some() {
                let mut gs_dict = resources.ext_g_states();
                for (k, &idx) in gstates.values().enumerate() {
                    gs_dict.pair(Name(gs_name(idx).as_bytes()), gstate_ref(k));
                }
                for (i, m) in masks() {
                    gs_dict.pair(Name(mask_name(i).as_bytes()), m.gstate);
                }
                gs_dict.finish();
            }
            if masks().next().is_some() {
                let mut shadings = resources.shadings();
                for (i, m) in masks() {
                    shadings.pair(Name(alpha_name(i).as_bytes()), m.shading);
                }
                shadings.finish();
            }
            if !gradient_refs.is_empty() {
                let mut pat_dict = resources.patterns();
                for (i, gr) in gradient_refs.iter().enumerate() {
                    pat_dict.pair(Name(pattern_name(i).as_bytes()), gr.pattern);
                }
                pat_dict.finish();
            }
            if !form_refs.is_empty() || !images.is_empty() {
                let mut xobjects = resources.x_objects();
                for (i, &r) in form_refs.iter().enumerate() {
                    xobjects.pair(Name(form_name(i).as_bytes()), r);
                }
                for (i, &(_, _, r, _)) in images.iter().enumerate() {
                    xobjects.pair(Name(image_name(i).as_bytes()), r);
                }
                xobjects.finish();
            }
            resources.finish();
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
            gradient.write(&mut pdf, refs);
        }

        // A soft mask draws the alphas of its gradient in gray, and its
        // luminosity becomes the alpha of what draws under it.
        for (i, m) in masks() {
            let mut content = Content::new();
            content.shading(Name(alpha_name(i).as_bytes()));
            let content = content.finish();
            let mut x = pdf.form_xobject(m.form, &content);
            // The mask maps to the space in effect at its `gs`, which is in
            // pixels on the page and in a form alike.
            x.bbox(Rect::new(0.0, 0.0, w, h));
            x.pair(Name(b"Resources"), resources_id);
            x.group()
                .transparency()
                .isolated(true)
                .color_space()
                .device_gray();
            x.finish();
            pdf.ext_graphics(m.gstate)
                .soft_mask()
                .subtype(MaskType::Luminosity)
                .group(m.form);
        }

        for (form, &r) in forms.iter().zip(&form_refs) {
            let mut x = pdf.form_xobject(r, form);
            x.bbox(Rect::new(0.0, 0.0, w, h));
            x.pair(Name(b"Resources"), resources_id);
            // An isolated group starts transparent, as the layer of the
            // pixmap does.
            x.group().transparency().isolated(true);
            x.finish();
        }

        for &(image, sampling, r, mask) in &images {
            write_image(&mut pdf, image, sampling, r, mask);
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

fn form_name(idx: usize) -> String {
    format!("Fm{idx}")
}

fn image_name(idx: usize) -> String {
    format!("Im{idx}")
}

/// Writes `image` under `id`, and its alphas under `mask`, which is `Some`
/// when the image has alphas.
fn write_image(pdf: &mut Pdf, image: &XImage, sampling: Sampling, id: Ref, mask: Option<Ref>) {
    let side = |s: u32| i32::try_from(s).expect("a side of an image fits an i32");
    let (width, height) = (side(image.size.0), side(image.size.1));
    // A viewer may sample the image as it likes when this is true.
    let interpolate = sampling == Sampling::Smooth;
    let (samples, filter) = match &image.samples {
        Samples::Jpeg { file, .. } => (file, Filter::DctDecode),
        Samples::Deflated { rgb, .. } => (rgb, Filter::FlateDecode),
    };
    let mut x = pdf.image_xobject(id, samples);
    x.filter(filter);
    x.width(width);
    x.height(height);
    match image.samples {
        Samples::Jpeg {
            color: JpegColor::Gray,
            ..
        } => x.color_space().device_gray(),
        _ => x.color_space().device_rgb(),
    }
    x.bits_per_component(8);
    x.interpolate(interpolate);
    if let Some(mask) = mask {
        x.s_mask(mask);
    }
    x.finish();
    if let (
        Samples::Deflated {
            alpha: Some(alpha), ..
        },
        Some(mask),
    ) = (&image.samples, mask)
    {
        let mut m = pdf.image_xobject(mask, alpha);
        m.filter(Filter::FlateDecode);
        m.width(width);
        m.height(height);
        m.color_space().device_gray();
        m.bits_per_component(8);
        m.interpolate(interpolate);
        m.finish();
    }
}

/// The name of the ExtGState with the soft mask of the gradient at `idx`.
fn mask_name(idx: usize) -> String {
    format!("Sm{idx}")
}

/// The name of the shading of the alphas of the gradient at `idx`.
fn alpha_name(idx: usize) -> String {
    format!("Sa{idx}")
}

/// The fill and the stroke of one element, in one pass, or in two when a
/// side is translucent. One pass paints both as a knockout group, so the
/// stroke would hide the fill under it instead of blending over it, as the
/// other renderers do. A soft mask on one side would also mask the other.
fn passes<'a>(
    fill: Option<&'a Paint>,
    stroke: Option<&'a Paint>,
) -> impl Iterator<Item = (Option<&'a Paint>, Option<&'a Paint>)> {
    let translucent = |p: Option<&Paint>| match p {
        Some(Paint::Solid(c)) => alpha_key(c.a) < alpha_key(1.0),
        Some(Paint::Gradient(g)) => g
            .stops()
            .iter()
            .any(|s| alpha_key(s.color.a) < alpha_key(1.0)),
        None => false,
    };
    if fill.is_some() && stroke.is_some() && (translucent(fill) || translucent(stroke)) {
        [(fill, None), (None, stroke)].into_iter().take(2)
    } else {
        [(fill, stroke), (None, None)].into_iter().take(1)
    }
}

/// Returns `true` if the stops of `g` differ in alpha, `false` otherwise. A
/// shading has no alpha, so such a gradient draws under a soft mask.
fn varying_alpha(g: &Gradient) -> bool {
    match g.stops() {
        [first, rest @ ..] => rest
            .iter()
            .any(|s| alpha_key(s.color.a) != alpha_key(first.color.a)),
        [] => false,
    }
}

const IDENTITY: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

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

/// Path and glyph outlines, written straight into the content stream. PDF
/// has no quadratic operator, so a quadratic goes in as the equal cubic.
struct PdfOutline<'a> {
    content: &'a mut Content,
    /// `true` until the first move.
    empty: bool,
    /// The start of the subpath, where `close` returns the current point.
    start: (f32, f32),
    /// The current point, from `(0, 0)` as in a path of the scene.
    last: (f32, f32),
}

impl<'a> PdfOutline<'a> {
    fn new(content: &'a mut Content) -> Self {
        Self {
            content,
            empty: true,
            start: (0.0, 0.0),
            last: (0.0, 0.0),
        }
    }
}

impl PathSink for PdfOutline<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.content.move_to(x, y);
        self.empty = false;
        self.start = (x, y);
        self.last = (x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.content.line_to(x, y);
        self.last = (x, y);
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let (c1x, c1y, c2x, c2y) = quad_to_cubic(self.last, cx, cy, x, y);
        self.cubic_to(c1x, c1y, c2x, c2y, x, y);
    }
    fn cubic_to(&mut self, cx1: f32, cy1: f32, cx2: f32, cy2: f32, x: f32, y: f32) {
        self.content.cubic_to(cx1, cy1, cx2, cy2, x, y);
        self.last = (x, y);
    }
    fn close(&mut self) {
        self.content.close_path();
        self.last = self.start;
    }
}

/// The two control points of the cubic equal to the quadratic from `p0`
/// through the control `(cx, cy)` to `(x, y)`. Each is a third of the way
/// from the control to an end, so it lies between them. The sum is taken in
/// `f64`, where two finite `f32` cannot overflow.
fn quad_to_cubic(p0: (f32, f32), cx: f32, cy: f32, x: f32, y: f32) -> (f32, f32, f32, f32) {
    let third = |end: f32, control: f32| ((f64::from(end) + 2.0 * f64::from(control)) / 3.0) as f32;
    let (p0x, p0y) = p0;
    (third(p0x, cx), third(p0y, cy), third(x, cx), third(y, cy))
}

/// A gradient of the frame, with the stops that its functions take.
struct Shading {
    geom: GradientGeom,
    /// Padded by [`Shading::new`].
    stops: Vec<Stop>,
    /// Maps the gradient to the page, or to the form of the layer that holds
    /// it.
    matrix: [f32; 6],
    /// `true` if the gradient draws under a soft mask of its alphas.
    masked: bool,
}

impl Shading {
    /// `g` with its stops padded so there are at least two, the first at 0
    /// and the last at 1. The pad repeats the boundary color, as in CSS. A
    /// [`Gradient`] already raises and clamps its offsets. No stops give two
    /// transparent ones, a case the visibility check already excludes.
    fn new(g: &Gradient, matrix: [f32; 6]) -> Self {
        let stops = match g.stops() {
            [first, .., last] => {
                let mut stops = Vec::with_capacity(g.stops().len() + 2);
                if first.offset > 0.0 {
                    stops.push(Stop {
                        offset: 0.0,
                        color: first.color,
                    });
                }
                stops.extend_from_slice(g.stops());
                if last.offset < 1.0 {
                    stops.push(Stop {
                        offset: 1.0,
                        color: last.color,
                    });
                }
                stops
            }
            one_or_none => {
                let color = one_or_none.first().map_or(Rgba::default(), |s| s.color);
                vec![Stop { offset: 0.0, color }, Stop { offset: 1.0, color }]
            }
        };
        Self {
            geom: g.geom(),
            stops,
            matrix,
            masked: varying_alpha(g),
        }
    }

    /// One exponential function per interval of the stops, and a stitching
    /// function over them when there is more than one interval.
    fn function_count(&self) -> usize {
        match self.stops.len() - 1 {
            1 => 1,
            intervals => intervals + 1,
        }
    }

    /// Writes the functions, the shading and the pattern of the gradient into
    /// `pdf`, under the ids of `refs`, and the shading of its alphas if it
    /// has a soft mask.
    fn write(&self, pdf: &mut Pdf, refs: &GradientRefs) {
        let function = self.write_functions(pdf, &refs.functions, rgb_components);
        self.write_shading(pdf, refs.shading, function, |cs| cs.device_rgb());
        {
            let mut pat = pdf.shading_pattern(refs.pattern);
            // The `cm` of a content stream does not apply to a pattern.
            pat.matrix(self.matrix);
            pat.shading_ref(refs.shading);
            pat.finish();
        }
        if let Some(mask) = &refs.mask {
            let alpha = |c: Rgba| [alpha_value(alpha_key(c.a))];
            let function = self.write_functions(pdf, &mask.functions, alpha);
            self.write_shading(pdf, mask.shading, function, |cs| cs.device_gray());
        }
    }

    /// Writes the functions of the `N` components that `channel` takes from
    /// the color of each stop, under the ids of `refs`. Returns the id of the
    /// function that a shading references.
    fn write_functions<const N: usize>(
        &self,
        pdf: &mut Pdf,
        refs: &[Ref],
        channel: impl Fn(Rgba) -> [f32; N],
    ) -> Ref {
        let stops = &self.stops;
        let range = || std::iter::repeat_n([0.0, 1.0], N).flatten();

        for (&[from, to], &r) in stops.array_windows().zip(refs) {
            let mut f = pdf.exponential_function(r);
            f.domain([0.0, 1.0]);
            f.range(range());
            f.c0(channel(from.color));
            f.c1(channel(to.color));
            f.n(1.0);
            f.finish();
        }
        let (&main_fn_ref, subs) = refs.split_last().expect("a gradient has a function");
        // With one interval, the only function is the exponential one.
        if let [_, inner @ .., _] = stops.as_slice()
            && !subs.is_empty()
        {
            assert_eq!(
                subs.len(),
                inner.len() + 1,
                "a stitched gradient has one function per interval"
            );
            let mut stitch = pdf.stitching_function(main_fn_ref);
            stitch.domain([0.0, 1.0]);
            stitch.range(range());
            stitch.functions(subs.iter().copied());
            stitch.bounds(inner.iter().map(|s| s.offset));
            // Each sub-function maps its interval back to [0, 1].
            stitch.encode(subs.iter().flat_map(|_| [0.0, 1.0]));
            stitch.finish();
        }
        main_fn_ref
    }

    /// Writes the shading of the gradient under `id`, with the function
    /// `function` in the color space that `color_space` sets.
    fn write_shading(
        &self,
        pdf: &mut Pdf,
        id: Ref,
        function: Ref,
        color_space: impl FnOnce(ColorSpace<'_>),
    ) {
        let mut sh = pdf.function_shading(id);
        color_space(sh.color_space());
        match self.geom {
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
        sh.function(function);
        sh.finish();
    }
}

/// The indirect objects of one gradient.
struct GradientRefs {
    /// The functions, as many as [`Shading::function_count`] says, with the
    /// one the shading references last.
    functions: Vec<Ref>,
    shading: Ref,
    pattern: Ref,
    mask: Option<MaskRefs>,
}

/// The indirect objects of the soft mask of one gradient.
struct MaskRefs {
    /// The functions of the alphas, as in [`GradientRefs::functions`].
    functions: Vec<Ref>,
    /// The shading of the alphas, in gray.
    shading: Ref,
    /// The transparency group that draws `shading`.
    form: Ref,
    /// The ExtGState that sets the soft mask.
    gstate: Ref,
}

fn rgb_components(c: Rgba) -> [f32; 3] {
    [c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0]
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

    fn gradient_fill(gradient: Paint) -> PathStyle {
        PathStyle {
            fill: gradient,
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
            fill: Paint::Solid(opaque(0, 0, 0)),
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
        scene.add_path(rect(red_fill(1.0), 0.0, 0.0, 100.0, 50.0));
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
        scene.add_text(Text {
            fill: Paint::Solid(opaque(0, 0, 0)),
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
        scene.add_path(rect(red_fill(1.0), 0.0, 0.0, 10.0, 10.0));
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
            scene.add_text(Text {
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
        scene.add_path(rect(red_fill(0.5), 0.0, 0.0, 100.0, 50.0));
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
        scene.add_path(rect(style, 0.0, 0.0, 100.0, 50.0));
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
        scene.add_path(rect(style, 5.0, 5.0, 40.0, 40.0));
        let s = pdf_text(&scene);
        assert!(s.contains("12 M"), "expected miter limit op: {s}");
    }

    #[test]
    fn linear_gradient_emits_axial_shading() {
        let mut scene = Scene::new(50.0, 50.0);
        let stops = vec![stop(0.0, opaque(255, 0, 0)), stop(1.0, opaque(0, 0, 255))];
        let style = gradient_fill(Paint::linear(0.0, 0.0, 50.0, 0.0, stops));
        scene.add_path(rect(style, 0.0, 0.0, 50.0, 50.0));
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
        let style = gradient_fill(Paint::radial(25.0, 25.0, 20.0, stops));
        scene.add_path(rect(style, 0.0, 0.0, 50.0, 50.0));
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
        let style = gradient_fill(Paint::linear(0.0, 0.0, 60.0, 0.0, stops));
        scene.add_path(rect(style, 0.0, 0.0, 60.0, 10.0));
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
        let style = gradient_fill(Paint::linear(0.0, 0.0, 100.0, 0.0, stops));
        scene.add_path(rect(style, 0.0, 0.0, 100.0, 40.0));
        let s = pdf_text(&scene);
        // The base transform of the content stream, for a page 40 pixels tall.
        assert!(s.contains("/Matrix [0.75 0 0 -0.75 0 30]"), "{s}");
    }

    #[test]
    fn a_layer_is_a_transparency_group_that_draws_with_its_opacity() {
        let mut scene = Scene::new(100.0, 40.0);
        let stops = vec![stop(0.0, opaque(255, 0, 0)), stop(1.0, opaque(0, 0, 255))];
        scene.layer(0.5, |layer| {
            let style = gradient_fill(Paint::linear(0.0, 0.0, 100.0, 0.0, stops));
            layer.add_path(rect(style, 0.0, 0.0, 100.0, 40.0));
        });
        let s = pdf_text(&scene);
        assert!(s.contains("/Subtype /Form"), "{s}");
        assert!(s.contains("/S /Transparency"), "{s}");
        assert!(s.contains("/BBox [0 0 100 40]"), "{s}");
        assert!(s.contains("q\n/Gs0 gs\n/Fm0 Do\nQ"), "{s}");
        assert!(s.contains("/CA 0.5"), "{s}");
        // A form draws in pixels, so its pattern takes no transform.
        assert!(s.contains("/Matrix [1 0 0 1 0 0]"), "{s}");
        // The page and the form share one dictionary of resources.
        let resources: Vec<_> = s
            .match_indices("/Resources ")
            .map(|(i, _)| &s[i..i + 16])
            .collect();
        assert_eq!(resources.len(), 2, "{s}");
        assert_eq!(resources[0], resources[1], "{s}");
    }

    #[test]
    fn a_gradient_with_varying_alpha_draws_under_a_soft_mask() {
        let mut scene = Scene::new(100.0, 40.0);
        let clear = Rgba {
            a: 0.0,
            ..opaque(0, 0, 255)
        };
        let stops = vec![stop(0.0, opaque(255, 0, 0)), stop(1.0, clear)];
        let style = gradient_fill(Paint::linear(0.0, 0.0, 100.0, 0.0, stops));
        scene.add_path(rect(style, 0.0, 0.0, 100.0, 40.0));
        let s = pdf_text(&scene);
        assert!(s.contains("/Sm0 gs"), "{s}");
        assert!(s.contains("/S /Luminosity"), "{s}");
        assert!(s.contains("/CS /DeviceGray"), "{s}");
        assert!(s.contains("/Sa0 sh"), "{s}");
        assert!(s.contains("/C0 [1]") && s.contains("/C1 [0]"), "{s}");
        assert!(!s.contains("/ca"), "{s}");
    }

    #[test]
    fn a_gradient_with_one_alpha_draws_with_that_alpha() {
        let mut scene = Scene::new(100.0, 40.0);
        let half = |r, g, b| Rgba {
            a: 0.5,
            ..opaque(r, g, b)
        };
        let stops = vec![stop(0.0, half(255, 0, 0)), stop(1.0, half(0, 0, 255))];
        let style = gradient_fill(Paint::linear(0.0, 0.0, 100.0, 0.0, stops));
        scene.add_path(rect(style, 0.0, 0.0, 100.0, 40.0));
        let s = pdf_text(&scene);
        assert!(s.contains("/ca 0.5"), "{s}");
        assert!(!s.contains("/SMask"), "{s}");
    }

    #[test]
    fn a_soft_mask_on_one_side_draws_the_fill_and_the_stroke_apart() {
        let mut scene = Scene::new(100.0, 40.0);
        let clear = Rgba {
            a: 0.0,
            ..opaque(0, 0, 255)
        };
        let stops = vec![stop(0.0, opaque(255, 0, 0)), stop(1.0, clear)];
        let style = PathStyle {
            stroke: Paint::rgba(0, 0, 0, 1.0),
            stroke_width: 2.0,
            ..gradient_fill(Paint::linear(0.0, 0.0, 100.0, 0.0, stops))
        };
        scene.add_path(rect(style, 10.0, 10.0, 80.0, 20.0));
        let s = pdf_text(&scene);
        let lines: Vec<_> = s.lines().collect();
        assert!(lines.contains(&"f") && lines.contains(&"S"), "{s}");
        assert!(!lines.contains(&"B"), "{s}");
    }

    #[test]
    fn a_translucent_fill_and_stroke_draw_apart() {
        let mut scene = Scene::new(100.0, 40.0);
        let style = PathStyle {
            fill: Paint::rgba(255, 0, 0, 0.5),
            stroke: Paint::rgba(0, 0, 255, 0.5),
            stroke_width: 4.0,
            ..PathStyle::default()
        };
        scene.add_path(rect(style, 10.0, 10.0, 80.0, 20.0));
        let s = pdf_text(&scene);
        let lines: Vec<_> = s.lines().collect();
        assert!(lines.contains(&"f") && lines.contains(&"S"), "{s}");
        assert!(!lines.contains(&"B"), "{s}");
    }

    fn bitmap(image: &Image, sampling: Sampling) -> Bitmap {
        Bitmap {
            image: image.clone(),
            transform: [10.0, 0.0, 0.0, 10.0, 10.0, 10.0],
            sampling,
        }
    }

    #[test]
    fn a_png_with_alpha_is_an_image_with_a_soft_mask() {
        let mut pixmap = tiny_skia::Pixmap::new(2, 2).unwrap();
        pixmap.fill(tiny_skia::Color::from_rgba8(255, 0, 0, 128));
        let png = Image::new(pixmap.encode_png().unwrap()).unwrap();
        let mut scene = Scene::new(20.0, 20.0);
        scene.add_bitmap(bitmap(&png, Sampling::Smooth));
        let s = pdf_text(&scene);
        assert!(
            s.contains("q\n10 0 0 10 10 10 cm\n1 0 0 -1 -0.5 0.5 cm\n/Im0 Do\nQ"),
            "{s}"
        );
        assert_eq!(s.matches("/Subtype /Image").count(), 2, "{s}");
        assert!(s.contains("/Filter /FlateDecode"), "{s}");
        assert!(s.contains("/SMask "), "{s}");
        assert!(s.contains("/Interpolate true"), "{s}");
    }

    #[test]
    fn an_upright_jpeg_is_an_image_of_its_file() {
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(2, 2)
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        let jpeg = Image::new(jpeg.into_inner()).unwrap();
        let mut scene = Scene::new(20.0, 20.0);
        scene.add_bitmap(bitmap(&jpeg, Sampling::Smooth));
        scene.add_bitmap(bitmap(&jpeg, Sampling::Nearest));
        scene.add_bitmap(bitmap(&jpeg, Sampling::Nearest));
        let s = pdf_text(&scene);
        // One image for each sampling, since the sampling is in the image.
        assert_eq!(s.matches("/Subtype /Image").count(), 2, "{s}");
        assert_eq!(s.matches("/Filter /DCTDecode").count(), 2, "{s}");
        assert!(s.contains("/Interpolate false"), "{s}");
        assert!(!s.contains("/SMask"), "{s}");
        assert_eq!(s.matches("/Im1 Do").count(), 2, "{s}");
    }

    #[test]
    fn a_bitmap_whose_image_does_not_decode_is_a_gray_box_with_a_red_cross() {
        let mut scene = Scene::new(20.0, 20.0);
        scene.add_bitmap(bitmap(&crate::asset::png_image(1, 1), Sampling::Smooth));
        let s = pdf_text(&scene);
        assert!(
            s.contains("5 5 m\n15 5 l\n15 15 l\n5 15 l\nh\nB\n5 5 m\n15 15 l\n15 5 m\n5 15 l\nS"),
            "{s}"
        );
        assert!(!s.contains("/Subtype /Image"), "{s}");
    }

    #[test]
    fn an_empty_clip_clips_to_an_empty_rectangle() {
        let mut scene = Scene::new(20.0, 20.0);
        scene.clip(ClipPath::default(), |empty| {
            empty.add_path(rect(red_fill(1.0), 0.0, 0.0, 20.0, 20.0));
        });
        let s = pdf_text(&scene);
        assert!(s.contains("q\n0 0 0 0 re\nW\nn\n"), "{s}");
    }

    #[test]
    fn a_quadratic_after_a_close_starts_at_the_start_of_the_subpath() {
        let mut content = Content::new();
        {
            let mut out = PdfOutline::new(&mut content);
            out.move_to(0.0, 0.0);
            out.line_to(6.0, 0.0);
            out.close();
            out.quad_to(3.0, 3.0, 6.0, 0.0);
        }
        let ops = String::from_utf8(content.finish().to_vec()).expect("ascii");
        assert_eq!(ops, "0 0 m\n6 0 l\nh\n2 2 4 2 6 0 c");
    }

    #[test]
    fn a_quadratic_with_no_move_starts_at_the_origin() {
        let mut content = Content::new();
        PdfOutline::new(&mut content).quad_to(3.0, 3.0, 6.0, 0.0);
        let ops = String::from_utf8(content.finish().to_vec()).expect("ascii");
        assert_eq!(ops, "2 2 4 2 6 0 c");
    }

    #[test]
    fn a_quadratic_between_far_points_elevates_to_finite_controls() {
        let mut content = Content::new();
        {
            let mut out = PdfOutline::new(&mut content);
            out.move_to(-f32::MAX, 0.0);
            out.quad_to(f32::MAX, 0.0, 0.0, 9.0);
        }
        let ops = String::from_utf8(content.finish().to_vec()).expect("ascii");
        let controls: Vec<f32> = ops
            .lines()
            .last()
            .expect("a curve")
            .split(' ')
            .take(4)
            .map(|v| v.parse().expect("a number"))
            .collect();
        assert!(controls.iter().all(|v| v.is_finite()), "{ops}");
        assert_eq!(controls[1], 0.0);
        assert_eq!(controls[3], 3.0);
    }

    #[test]
    fn a_text_with_no_outline_paints_nothing() {
        let mut scene = Scene::new(40.0, 20.0);
        scene.add_text(text("   ", 12.0));
        let s = pdf_text(&scene);
        assert!(!s.lines().any(|l| l == "f"), "{s}");
    }

    #[test]
    fn a_gradient_keeps_the_order_of_its_stops() {
        let mut scene = Scene::new(60.0, 10.0);
        let stops = vec![
            stop(0.0, opaque(255, 0, 0)),
            stop(0.8, opaque(0, 255, 0)),
            stop(0.2, opaque(0, 0, 255)),
        ];
        let style = gradient_fill(Paint::linear(0.0, 0.0, 60.0, 0.0, stops));
        scene.add_path(rect(style, 0.0, 0.0, 60.0, 10.0));
        let s = pdf_text(&scene);
        // The third stop moved up to the second, so both bounds are 0.8.
        assert!(s.contains("/Bounds [0.8 0.8]"), "bounds missing: {s}");
    }
}
