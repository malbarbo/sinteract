//! Rasterize a [`crate::scene::Scene`] into a tiny-skia [`Pixmap`]. It needs
//! no tty or window, so it builds on wasm. The terminal and the window
//! present the pixels.

use std::collections::HashMap;

use tiny_skia::{
    Color as SkColor, FillRule as SkFillRule, FilterQuality, GradientStop as SkStop,
    LineCap as SkLineCap, LineJoin as SkLineJoin, Mask, Paint as SkPaint, Path as SkPath,
    PathBuilder, PathStroker, Pixmap, PixmapPaint, Point as SkPoint, Rect as SkRect,
    Shader as SkShader, SpreadMode as SkSpread, Stroke, StrokeDash, Transform,
};

use crate::asset::{MAX_IMAGE_PIXELS, MAX_LIVE_PIXELS};
use crate::outline::PathSink;
use crate::renderer::{
    AllocError, MISSING_FILL, MISSING_STROKE, Renderer, RestoreOnDrop, TEXT_MITER_LIMIT,
    frame_side, missing_box, missing_cross, sealed::Canvas,
};
use crate::scene::{
    Bitmap, ClipPath, FillRule, GradientGeom, Image, LineCap, LineJoin, Paint, Path, Rgba,
    Sampling, SpreadMode, Stop, Text,
};
use crate::text::TextLayout;

/// Rasterize a [`crate::scene::Scene`] at `scale`, where 1.0 is the frame's
/// own pixels. See [`fit_scale`].
pub fn render_to_pixmap(scene: &crate::scene::Scene, scale: f32) -> Result<Pixmap, AllocError> {
    let mut renderer = PixmapRenderer::new(scale, scene.width(), scene.height())?;
    renderer.render(scene)?;
    Ok(renderer.into_pixmap())
}

/// The uniform scale that fits a `width × height` frame inside `target`
/// pixels. It can exceed 1.0. The caller caps it.
pub fn fit_scale(width: f32, height: f32, target: (u32, u32)) -> f32 {
    let (tw, th) = target;
    if tw == 0 || th == 0 {
        return 1.0;
    }
    let (w, h) = frame_px(width, height);
    (tw as f32 / w as f32).min(th as f32 / h as f32)
}

/// A raster surface. It reallocates its pixmap only when the frame size
/// changes.
pub struct PixmapRenderer {
    pixmap: Pixmap,
    /// The scale as a transform, applied to every path. The caller decides
    /// with it whether a frame may grow.
    base: Transform,
    /// What each clip in effect leaves of the canvas.
    clip_stack: Vec<Clip>,
    /// Masks popped off `clip_stack`, for the next push. Every mask is
    /// canvas-sized, so any one fits.
    mask_pool: Vec<Mask>,
    /// The pixmap under each layer in effect, with the opacity of the layer.
    /// `pixmap` is the top layer.
    layer_stack: Vec<(Pixmap, f32)>,
    /// Layers drawn and popped off, for the next layer. Every layer is
    /// canvas-sized, as a mask is.
    layer_pool: Vec<Pixmap>,
    /// The opacity of each layer in effect past [`MAX_LAYER_DEPTH`], which
    /// every element takes on its own.
    fades: Vec<f32>,
    /// The builder of the next path. A finished path clears back into it, so
    /// the next path reuses its capacity.
    builder: PathBuilder,
    images: Decoded,
    /// The color of the pixmap before a frame draws on it.
    background: SkColor,
}

impl PixmapRenderer {
    /// A surface for frames at `scale`, where 1.0 is the frame's own pixels,
    /// sized first for a `width × height` frame.
    pub fn new(scale: f32, width: f32, height: f32) -> Result<Self, AllocError> {
        let base = base(scale);
        let (out_w, out_h) = out_size(width, height, base.sx);
        Ok(Self {
            pixmap: new_pixmap(out_w, out_h)?,
            base,
            clip_stack: Vec::new(),
            mask_pool: Vec::new(),
            layer_stack: Vec::new(),
            layer_pool: Vec::new(),
            fades: Vec::new(),
            builder: PathBuilder::new(),
            images: Decoded::default(),
            background: SkColor::TRANSPARENT,
        })
    }

    /// Render the next frames at `scale`. The surface grows or shrinks at
    /// the next render.
    pub fn set_scale(&mut self, scale: f32) {
        self.base = base(scale);
    }

    /// Start the next frames from `color`, where the default is
    /// transparent. A display that cannot show transparency passes an
    /// opaque color, and every pixel comes out opaque.
    pub fn set_background(&mut self, color: Rgba) {
        self.background = sk_color(color);
    }

    /// Hand out the pixmap of the last render, and render the next frames
    /// into `pixmap`. A render resizes `pixmap` when its size is wrong.
    pub fn replace_pixmap(&mut self, pixmap: Pixmap) -> Pixmap {
        // A pooled mask has the size of the pixmap that goes out, and the
        // next render sees only the size of `pixmap`.
        let size = |p: &Pixmap| (p.width(), p.height());
        if size(&pixmap) != size(&self.pixmap) {
            self.mask_pool.clear();
            self.layer_pool.clear();
        }
        std::mem::replace(&mut self.pixmap, pixmap)
    }

    /// The pixmap of the last render.
    pub fn into_pixmap(self) -> Pixmap {
        self.pixmap
    }

    /// The opacity that each element takes from the layers past
    /// [`MAX_LAYER_DEPTH`].
    fn fade(&self) -> f32 {
        self.fades.iter().product()
    }
}

impl Default for PixmapRenderer {
    /// A surface at scale 1.0 that takes its size at the first render.
    fn default() -> Self {
        Self::new(1.0, 1.0, 1.0).expect("a 1x1 pixmap always allocates")
    }
}

/// The images that the bitmaps drew, decoded, so the next frames draw them
/// without a decode. Past [`MAX_LIVE_PIXELS`], the images that a bitmap
/// drew longest ago go first, but not one that the frame drew, so a frame
/// of more images than fit keeps the ones that fit and decodes the others
/// at each draw, once for the draws of one image in a row. An image that
/// does not decode keeps `None`, so it draws the marker of a missing image
/// without a second decode.
#[derive(Default)]
struct Decoded {
    images: HashMap<Image, Use>,
    /// The pixels of `images`, as their headers give them.
    pixels: u64,
    /// How many times a bitmap drew.
    draws: u64,
    /// The value of `draws` when the frame began.
    frame_start: u64,
    /// The last image that did not fit, decoded, so the bitmaps that draw
    /// it in a row decode it once.
    spill: Option<(Image, Option<Pixmap>)>,
}

struct Use {
    pixmap: Option<Pixmap>,
    /// The value of `draws` when a bitmap last drew the image.
    last: u64,
}

impl Decoded {
    fn next_frame(&mut self) {
        self.frame_start = self.draws;
    }

    /// The pixels of `image`, decoded now if no bitmap drew it lately.
    fn get(&mut self, image: &Image) -> Option<&Pixmap> {
        self.draws += 1;
        if !self.images.contains_key(image) {
            let need = image.pixels();
            let mut may_go: Vec<(u64, Image)> = self
                .images
                .iter()
                .filter(|(_, u)| u.last <= self.frame_start)
                .map(|(i, u)| (u.last, i.clone()))
                .collect();
            let room: u64 = may_go.iter().map(|(_, i)| i.pixels()).sum();
            let spilled = self.spill.take_if(|(spilled, _)| spilled == image);
            let pixmap = match spilled {
                Some((_, pixmap)) => pixmap,
                None => crate::asset::decode(image.file(), MAX_IMAGE_PIXELS).ok(),
            };
            if self.pixels - room + need > MAX_LIVE_PIXELS {
                let (_, pixmap) = self.spill.insert((image.clone(), pixmap));
                return pixmap.as_ref();
            }
            may_go.sort_unstable_by_key(|(last, _)| *last);
            for (_, old) in may_go {
                if self.pixels + need <= MAX_LIVE_PIXELS {
                    break;
                }
                self.images.remove(&old);
                self.pixels -= old.pixels();
            }
            self.pixels += need;
            self.images.insert(image.clone(), Use { pixmap, last: 0 });
        }
        let used = self.images.get_mut(image).expect("the image is in");
        used.last = self.draws;
        used.pixmap.as_ref()
    }
}

impl Canvas<AllocError> for PixmapRenderer {
    /// Reallocates the surface when the scaled size changed, then fills it
    /// with the background.
    fn ensure_size(&mut self, width: f32, height: f32) -> Result<(), AllocError> {
        let (out_w, out_h) = out_size(width, height, self.base.sx);
        // A frame ends with an empty clip stack, and its masks serve the next
        // frame.
        self.mask_pool
            .extend(self.clip_stack.drain(..).filter_map(Clip::into_mask));
        if (out_w, out_h) != (self.pixmap.width(), self.pixmap.height()) {
            // Masks and layers are canvas-sized, so a resize invalidates every
            // pooled one.
            self.mask_pool.clear();
            self.layer_pool.clear();
            self.pixmap = new_pixmap(out_w, out_h)?;
        }
        self.pixmap.fill(self.background);
        self.images.next_frame();
        Ok(())
    }

    fn draw_path(&mut self, path: &Path) {
        let style = &path.style;
        let do_fill = style.draws_fill();
        let do_stroke = style.draws_stroke();
        if !do_fill && !do_stroke {
            return;
        }
        let mask = match in_effect(&self.clip_stack) {
            InEffect::Nothing => return,
            InEffect::Everything => None,
            InEffect::Through(mask) => Some(mask),
        };
        let mut builder = std::mem::take(&mut self.builder);
        path.outline(&mut builder);
        // A path with no segments builds nothing. The pdf and the svg test
        // the segments before they write, and here the builder answers.
        let Some(sk_path) = builder.finish() else {
            return;
        };
        if do_fill && within_reach(&sk_path, self.base, 0.0) {
            let paint = sk_paint(paint_to_shader(&style.fill, self.fade()));
            self.pixmap.fill_path(
                &sk_path,
                &paint,
                sk_fill_rule(style.fill_rule),
                self.base,
                mask,
            );
        }
        if do_stroke {
            let paint = sk_paint(paint_to_shader(&style.stroke, self.fade()));
            // A Dash is even and not empty, holds no negative length, sums
            // above zero and is finite, which is all StrokeDash asks for, so
            // it never refuses. One that did would draw the stroke solid.
            let dash = style
                .dash
                .as_ref()
                .and_then(|d| StrokeDash::new(d.array().to_vec(), d.offset()));
            let stroke = Stroke {
                width: style.stroke_width,
                line_cap: sk_line_cap(style.line_cap),
                line_join: sk_line_join(style.line_join),
                miter_limit: style.miter_limit,
                dash,
            };
            if stroke_within_reach(&sk_path, &stroke, self.base) {
                self.pixmap
                    .stroke_path(&sk_path, &paint, &stroke, self.base, mask);
            }
        }
        self.builder = sk_path.clear();
    }

    fn draw_text(&mut self, node: &Text) {
        if !node.draws_fill() && !node.draws_stroke() {
            return;
        }
        let mask = match in_effect(&self.clip_stack) {
            InEffect::Nothing => return,
            InEffect::Everything => None,
            InEffect::Through(mask) => Some(mask),
        };
        let fade = self.fade();
        render_text(
            node,
            &mut self.pixmap,
            mask,
            self.base,
            fade,
            &mut self.builder,
        );
    }

    /// Draw the image of `bitmap`. An image that does not decode draws a
    /// gray box with a red cross in its place, so a missing image shows.
    fn draw_bitmap(&mut self, bitmap: &Bitmap) {
        let fade = self.fade();
        let mask = match in_effect(&self.clip_stack) {
            InEffect::Nothing => return,
            InEffect::Everything => None,
            InEffect::Through(mask) => Some(mask),
        };
        let [a, b, c, d, e, f] = bitmap.transform;
        let square = Transform::from_row(a, b, c, d, e, f).post_concat(self.base);
        let Some(image) = self.images.get(&bitmap.image) else {
            draw_missing(
                &mut self.pixmap,
                &mut self.builder,
                square,
                self.base.sx,
                fade,
                mask,
            );
            return;
        };
        let (w, h) = (image.width() as f32, image.height() as f32);
        // The pixmap starts at its top left corner, and `square` takes the
        // unit square centred on the origin.
        let transform = Transform::from_translate(-w / 2.0, -h / 2.0)
            .post_scale(1.0 / w, 1.0 / h)
            .post_concat(square);
        let reach = SkRect::from_xywh(0.0, 0.0, w, h)
            .is_some_and(|rect| rect_within_reach(rect, transform));
        if reach {
            let paint = PixmapPaint {
                quality: match bitmap.sampling {
                    Sampling::Smooth => FilterQuality::Bilinear,
                    Sampling::Nearest => FilterQuality::Nearest,
                },
                opacity: fade,
                ..PixmapPaint::default()
            };
            self.pixmap
                .draw_pixmap(0, 0, image.as_ref(), &paint, transform, mask);
        }
    }

    fn with_clip<T>(&mut self, clip: &ClipPath, inside: impl FnOnce(&mut Self) -> T) -> T {
        let clip = self.clip_mask(clip);
        self.clip_stack.push(clip);
        let guard = RestoreOnDrop {
            canvas: self,
            restore: |c: &mut Self| {
                if let Some(Clip::Mask(mask)) = c.clip_stack.pop() {
                    c.mask_pool.push(mask);
                }
            },
        };
        inside(&mut *guard.canvas)
    }

    /// Past [`MAX_LAYER_DEPTH`] layers, a layer draws into the one below it
    /// and each of its elements takes the opacity, so the memory stays
    /// bounded. The elements differ from a group only where they overlap.
    fn with_layer<T>(&mut self, opacity: f32, inside: impl FnOnce(&mut Self) -> T) -> T {
        if matches!(self.clip_stack.last(), Some(Clip::Hidden)) {
            return inside(self);
        }
        if self.layer_stack.len() >= MAX_LAYER_DEPTH {
            self.fades.push(opacity);
            let guard = RestoreOnDrop {
                canvas: self,
                restore: |c: &mut Self| {
                    c.fades.pop();
                },
            };
            return inside(&mut *guard.canvas);
        }
        let layer = self.take_layer();
        let under = std::mem::replace(&mut self.pixmap, layer);
        self.layer_stack.push((under, opacity));
        let guard = RestoreOnDrop {
            canvas: self,
            restore: |c: &mut Self| {
                let Some((under, opacity)) = c.layer_stack.pop() else {
                    return;
                };
                let layer = std::mem::replace(&mut c.pixmap, under);
                // The clips in effect already masked what went into the
                // layer, and the layer is canvas-sized, so it draws as it is.
                let paint = PixmapPaint {
                    opacity,
                    ..PixmapPaint::default()
                };
                c.pixmap
                    .draw_pixmap(0, 0, layer.as_ref(), &paint, Transform::identity(), None);
                c.layer_pool.push(layer);
            },
        };
        inside(&mut *guard.canvas)
    }
}

impl Renderer for PixmapRenderer {
    type Error = AllocError;

    type Output<'a> = &'a Pixmap;

    fn output(&self) -> &Pixmap {
        &self.pixmap
    }
}

impl PixmapRenderer {
    /// The coverage of `clip` inside the clip in effect. A clip that gets no
    /// mask hides what it holds, so nothing paints outside it.
    fn clip_mask(&mut self, clip: &ClipPath) -> Clip {
        if matches!(self.clip_stack.last(), Some(Clip::Hidden)) {
            return Clip::Hidden;
        }
        let mut builder = std::mem::take(&mut self.builder);
        clip.segments().outline(&mut builder);
        // A sub-path of a clip is closed, as in an SVG clipPath. tiny_skia
        // accepts a close on a closed contour.
        builder.close();
        // An empty path covers nothing.
        let Some(path) = builder.finish() else {
            return Clip::Hidden;
        };
        // A clip that reaches too far hides what it holds.
        let reach = within_reach(&path, self.base, 0.0);
        let mask = reach.then(|| self.take_mask()).flatten().map(|mut mask| {
            mask.fill_path(&path, sk_fill_rule(clip.fill_rule), true, self.base);
            if let Some(Clip::Mask(parent)) = self.clip_stack.last() {
                // Mask::intersect_path would rasterize into a second
                // canvas-sized mask.
                for (a, b) in mask.data_mut().iter_mut().zip(parent.data()) {
                    *a = mask_mul(*a, *b);
                }
            }
            mask
        });
        self.builder = path.clear();
        mask.map_or(Clip::Hidden, Clip::Mask)
    }

    /// A transparent canvas-sized layer, from the pool when one is there.
    fn take_layer(&mut self) -> Pixmap {
        match self.layer_pool.pop() {
            Some(mut layer) => {
                layer.fill(SkColor::TRANSPARENT);
                layer
            }
            None => new_pixmap(self.pixmap.width(), self.pixmap.height())
                .expect("a layer has the size of the pixmap, which allocated"),
        }
    }

    /// A cleared canvas-sized mask, from the pool when one is there.
    /// `fill_path` adds to the coverage a mask holds, so a reused one has to
    /// be cleared.
    fn take_mask(&mut self) -> Option<Mask> {
        match self.mask_pool.pop() {
            Some(mut m) => {
                m.clear();
                Some(m)
            }
            None => Mask::new(self.pixmap.width(), self.pixmap.height()),
        }
    }
}

/// What one clip leaves of the canvas.
enum Clip {
    /// The coverage that everything inside the clip paints through.
    Mask(Mask),
    /// Nothing paints, because the clip has no area, a clip around it hides
    /// it, or its mask could not be allocated.
    Hidden,
}

impl Clip {
    /// The mask, for the pool.
    fn into_mask(self) -> Option<Mask> {
        match self {
            Clip::Mask(mask) => Some(mask),
            Clip::Hidden => None,
        }
    }
}

/// What the clips in effect leave for the next draw.
enum InEffect<'a> {
    Everything,
    Through(&'a Mask),
    Nothing,
}

/// Reads the stack alone, and not `&self`, so the caller still holds the
/// pixmap and the builder while it paints through the mask.
fn in_effect(clip_stack: &[Clip]) -> InEffect<'_> {
    match clip_stack.last() {
        None => InEffect::Everything,
        Some(Clip::Mask(mask)) => InEffect::Through(mask),
        Some(Clip::Hidden) => InEffect::Nothing,
    }
}

/// `a * b / 255`, rounded as tiny-skia does, so the result matches
/// `Mask::intersect_path`.
fn mask_mul(a: u8, b: u8) -> u8 {
    let prod = u32::from(a) * u32::from(b) + 128;
    ((prod + (prod >> 8)) >> 8) as u8
}

/// A frame's size in whole output pixels. The window asks for it too, to
/// size itself for a scene.
pub(crate) fn frame_px(width: f32, height: f32) -> (u32, u32) {
    (
        frame_side(width.ceil()) as u32,
        frame_side(height.ceil()) as u32,
    )
}

/// The transform of `scale`. A zero or negative scale would allocate nothing
/// to draw into.
fn base(scale: f32) -> Transform {
    let scale = scale.max(1e-3);
    Transform::from_scale(scale, scale)
}

/// The most pixels of a frame, 8192 by 8192. The pixmap and every clip
/// mask have the size of the frame, and a failed allocation aborts the
/// process, so a larger frame is an [`AllocError`].
pub const MAX_FRAME_PIXELS: u64 = 1 << 26;

/// The most layers in effect at once. Each one holds a pixmap of the size of
/// the frame.
pub const MAX_LAYER_DEPTH: usize = 4;

/// A transparent pixmap of `width` by `height`, or an error for one of more
/// than [`MAX_FRAME_PIXELS`].
fn new_pixmap(width: u32, height: u32) -> Result<Pixmap, AllocError> {
    let error = AllocError { width, height };
    if u64::from(width) * u64::from(height) > MAX_FRAME_PIXELS {
        return Err(error);
    }
    Pixmap::new(width, height).ok_or(error)
}

/// The size in output pixels of a frame at `scale`.
fn out_size(width: f32, height: f32, scale: f32) -> (u32, u32) {
    let (w, h) = frame_px(width, height);
    let out_w = frame_side(((w as f32) * scale).ceil()) as u32;
    let out_h = frame_side(((h as f32) * scale).ceil()) as u32;
    (out_w, out_h)
}

fn sk_fill_rule(r: FillRule) -> SkFillRule {
    match r {
        FillRule::EvenOdd => SkFillRule::EvenOdd,
        FillRule::NonZero => SkFillRule::Winding,
    }
}

fn sk_line_cap(c: LineCap) -> SkLineCap {
    match c {
        LineCap::Round => SkLineCap::Round,
        LineCap::Square => SkLineCap::Square,
        LineCap::Butt => SkLineCap::Butt,
    }
}

fn sk_line_join(j: LineJoin) -> SkLineJoin {
    match j {
        LineJoin::Round => SkLineJoin::Round,
        LineJoin::Bevel => SkLineJoin::Bevel,
        LineJoin::Miter => SkLineJoin::Miter,
    }
}

/// An anti-aliased paint with `shader`.
fn sk_paint(shader: SkShader<'static>) -> SkPaint<'static> {
    SkPaint {
        shader,
        anti_alias: true,
        ..SkPaint::default()
    }
}

/// A gradient that tiny-skia refuses falls back to the primary color, the
/// first stop. Nothing that tiny-skia refuses arrives here today, because the
/// scene removes each such case. The fallback keeps the path drawn if a later
/// change lets one through.
fn paint_to_shader(p: &Paint, fade: f32) -> SkShader<'static> {
    let g = match p {
        Paint::Solid(c) => return SkShader::SolidColor(sk_color(faded(*c, fade))),
        Paint::Gradient(g) => g,
    };
    let stops = sk_stops(g.stops(), fade);
    let spread = sk_spread(g.spread());
    match g.geom() {
        GradientGeom::Linear { x0, y0, x1, y1 } => tiny_skia::LinearGradient::new(
            SkPoint::from_xy(x0, y0),
            SkPoint::from_xy(x1, y1),
            stops,
            spread,
            Transform::identity(),
        ),
        GradientGeom::Radial { cx, cy, radius } => {
            let center = SkPoint::from_xy(cx, cy);
            tiny_skia::RadialGradient::new(
                center,
                0.0,
                center,
                radius,
                stops,
                spread,
                Transform::identity(),
            )
        }
    }
    .unwrap_or_else(|| SkShader::SolidColor(sk_color(faded(p.primary_color(), fade))))
}

/// `c` with its alpha times `fade`.
fn faded(c: Rgba, fade: f32) -> Rgba {
    Rgba { a: c.a * fade, ..c }
}

fn sk_color(c: Rgba) -> SkColor {
    SkColor::from_rgba8(c.r, c.g, c.b, (c.a * 255.0).round().clamp(0.0, 255.0) as u8)
}

fn sk_stops(stops: &[Stop], fade: f32) -> Vec<SkStop> {
    stops
        .iter()
        .map(|s| SkStop::new(s.offset, sk_color(faded(s.color, fade))))
        .collect()
}

fn sk_spread(s: SpreadMode) -> SkSpread {
    match s {
        SpreadMode::Pad => SkSpread::Pad,
        SpreadMode::Reflect => SkSpread::Reflect,
        SpreadMode::Repeat => SkSpread::Repeat,
    }
}

fn render_text(
    node: &Text,
    pixmap: &mut Pixmap,
    mask: Option<&Mask>,
    base: Transform,
    fade: f32,
    builder: &mut PathBuilder,
) {
    let Some(layout) = TextLayout::new(&node.spec) else {
        return;
    };

    // The transform is in the `cm` convention, which is the order of
    // Transform::from_row.
    let [a, b, c, d, e, f] = node.transform;
    let local = Transform::from_row(a, b, c, d, e, f);
    // The text transform applies first, then the scale.
    let transform = local.post_concat(base);
    let paints = TextPaints {
        fill: node
            .draws_fill()
            .then(|| text_paint(&node.fill, local, fade))
            .flatten(),
        stroke: node
            .draws_stroke()
            .then(|| text_paint(&node.stroke, local, fade))
            .flatten(),
        width: node.stroke_width,
    };

    paint_text_path(&paints, pixmap, mask, transform, builder, |out| {
        layout.outline(out)
    });
    // The underline paints on its own. In one path, a glyph that winds the
    // other way from the rectangle would cancel it where the two cross.
    if node.underline {
        paint_text_path(&paints, pixmap, mask, transform, builder, |out| {
            layout.outline_underline(out)
        });
    }
}

/// The paints of a text, `None` for a side that draws nothing.
struct TextPaints {
    fill: Option<SkPaint<'static>>,
    stroke: Option<SkPaint<'static>>,
    width: f32,
}

/// The paint of one side of a text. tiny-skia moves a shader with the path,
/// so a gradient takes the inverse of `local` to stay in canvas space.
/// Returns `None` for a gradient under a `local` with no inverse, which
/// squashes the text to a line.
fn text_paint(paint: &Paint, local: Transform, fade: f32) -> Option<SkPaint<'static>> {
    let mut shader = paint_to_shader(paint, fade);
    if let Paint::Gradient(_) = paint {
        shader.transform(local.invert()?);
    }
    Some(sk_paint(shader))
}

/// Builds a path of the text with `outline`, then fills and strokes it. The
/// finished path clears back into `builder`.
fn paint_text_path(
    paints: &TextPaints,
    pixmap: &mut Pixmap,
    mask: Option<&Mask>,
    transform: Transform,
    builder: &mut PathBuilder,
    outline: impl FnOnce(&mut PathBuilder),
) {
    let mut b = std::mem::take(builder);
    outline(&mut b);
    let Some(path) = b.finish() else {
        return;
    };
    if let Some(paint) = &paints.fill
        && within_reach(&path, transform, 0.0)
    {
        // A TrueType glyph fills with non-zero winding.
        pixmap.fill_path(&path, paint, SkFillRule::Winding, transform, mask);
    }
    if let Some(paint) = &paints.stroke {
        let stroke = Stroke {
            width: paints.width,
            miter_limit: TEXT_MITER_LIMIT,
            dash: None,
            ..Stroke::default()
        };
        if stroke_within_reach(&path, &stroke, transform) {
            pixmap.stroke_path(&path, paint, &stroke, transform, mask);
        }
    }
    *builder = path.clear();
}

/// Draw the marker of a bitmap with no image over the unit square that
/// `transform` places. The outline and the cross are `width` pixels wide
/// whatever the transform, so they are drawn in output pixels.
fn draw_missing(
    pixmap: &mut Pixmap,
    builder: &mut PathBuilder,
    transform: Transform,
    width: f32,
    fade: f32,
    mask: Option<&Mask>,
) {
    let unit = SkRect::from_xywh(-0.5, -0.5, 1.0, 1.0).expect("the unit square is a rect");
    if !rect_within_reach(unit, transform) {
        return;
    }
    let t = [
        transform.sx,
        transform.ky,
        transform.kx,
        transform.sy,
        transform.tx,
        transform.ty,
    ];
    let mut b = std::mem::take(builder);
    missing_box(t, &mut b);
    let Some(outline) = b.finish() else {
        return;
    };
    let gray = sk_paint(SkShader::SolidColor(sk_color(faded(MISSING_FILL, fade))));
    let red = sk_paint(SkShader::SolidColor(sk_color(faded(MISSING_STROKE, fade))));
    let stroke = Stroke {
        width,
        ..Stroke::default()
    };
    let identity = Transform::identity();
    pixmap.fill_path(&outline, &gray, SkFillRule::Winding, identity, mask);
    pixmap.stroke_path(&outline, &red, &stroke, identity, mask);
    let mut b = outline.clear();
    missing_cross(t, &mut b);
    let Some(cross) = b.finish() else {
        return;
    };
    pixmap.stroke_path(&cross, &red, &stroke, identity, mask);
    *builder = cross.clear();
}

/// The farthest a path may reach from the origin, in output pixels.
/// tiny-skia overflows an `i32` on a path that reaches 2^29 pixels above the
/// canvas and aborts (linebender/tiny-skia#180), so a path that reaches
/// farther than this draws nothing.
const MAX_REACH: f32 = (1u32 << 28) as f32;

/// Returns `true` if `path` under `transform`, grown by `pad` on each side,
/// stays within [`MAX_REACH`], `false` otherwise.
fn within_reach(path: &SkPath, transform: Transform, pad: f32) -> bool {
    path.bounds()
        .outset(pad, pad)
        .is_some_and(|b| rect_within_reach(b, transform))
}

/// Returns `true` if `rect` under `transform` stays within [`MAX_REACH`],
/// `false` otherwise.
fn rect_within_reach(rect: SkRect, transform: Transform) -> bool {
    rect.transform(transform).is_some_and(|b| {
        b.left() >= -MAX_REACH
            && b.top() >= -MAX_REACH
            && b.right() <= MAX_REACH
            && b.bottom() <= MAX_REACH
    })
}

/// Returns `true` if `stroke` of `path` under `transform` stays within
/// [`MAX_REACH`], `false` otherwise. A join or a cap reaches at most
/// `miter_limit` widths past the path, and the limit is never below 1, so
/// that bound settles most strokes, and only a stroke past it is outlined to
/// find where it reaches.
fn stroke_within_reach(path: &SkPath, stroke: &Stroke, transform: Transform) -> bool {
    within_reach(path, transform, stroke.width * stroke.miter_limit)
        || path
            .stroke(stroke, PathStroker::compute_resolution_scale(&transform))
            .is_some_and(|outline| within_reach(&outline, transform, 0.0))
}

/// The inherent methods of [`PathBuilder`], so paths, clips and glyphs build
/// through [`crate::scene::Segments::outline`] and
/// [`crate::text::TextLayout::outline`].
impl PathSink for PathBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        PathBuilder::move_to(self, x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        PathBuilder::line_to(self, x, y);
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        PathBuilder::quad_to(self, cx, cy, x, y);
    }
    fn cubic_to(&mut self, cx1: f32, cy1: f32, cx2: f32, cy2: f32, x: f32, y: f32) {
        PathBuilder::cubic_to(self, cx1, cy1, cx2, cy2, x, y);
    }
    fn close(&mut self) {
        PathBuilder::close(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::tests::rect;
    use crate::scene::{Dash, PathStyle, Rgba, RotatedRect, Scene, Stop, Text, TextSpec};

    fn pixel_rgba(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let p = pixmap.pixel(x, y).expect("pixel in range");
        let p = p.demultiply();
        (p.red(), p.green(), p.blue(), p.alpha())
    }

    fn solid(r: u8, g: u8, b: u8) -> PathStyle {
        PathStyle {
            fill: Paint::rgba(r, g, b, 1.0),
            ..PathStyle::default()
        }
    }

    fn square_path(style: PathStyle, side: f32) -> Path {
        Path::builder(style, 0.0, 0.0)
            .line_to(side, 0.0)
            .line_to(side, side)
            .line_to(0.0, side)
            .build()
    }

    fn square_clip(x: f32, y: f32, side: f32) -> ClipPath {
        ClipPath::builder(FillRule::NonZero, x, y)
            .line_to(x + side, y)
            .line_to(x + side, y + side)
            .line_to(x, y + side)
            .build()
    }

    fn assert_same_pixels(expected: &Pixmap, got: &Pixmap) {
        let size = |pm: &Pixmap| (pm.width(), pm.height());
        assert_eq!(size(expected), size(got));
        for y in 0..expected.height() {
            for x in 0..expected.width() {
                assert_eq!(
                    pixel_rgba(expected, x, y),
                    pixel_rgba(got, x, y),
                    "mismatch at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn a_gradient_on_a_text_stays_in_canvas_space() {
        // Red at x 0 to blue at x 100, on the fill of one text and on the
        // stroke of another, both turned by 90 degrees and drawn at scale 2.
        let ramp = || {
            let stop = |offset, r, b| Stop {
                offset,
                color: Rgba { r, g: 0, b, a: 1.0 },
            };
            Paint::linear(
                0.0,
                0.0,
                100.0,
                0.0,
                vec![stop(0.0, 255, 0), stop(1.0, 0, 255)],
            )
        };
        let upright = |cx| {
            let spec = TextSpec {
                size: 30.0,
                text: "HHHH".into(),
                ..TextSpec::default()
            };
            spec.fit(RotatedRect {
                cx,
                cy: 50.0,
                w: 80.0,
                h: 20.0,
                angle_deg: 90.0,
            })
            .expect("text draws")
        };
        let mut scene = Scene::new(100.0, 100.0);
        scene.add_text(Text {
            fill: ramp(),
            ..upright(25.0)
        });
        scene.add_text(Text {
            stroke: ramp(),
            stroke_width: 4.0,
            ..upright(75.0)
        });
        let pixmap = render_to_pixmap(&scene, 2.0).expect("pixmap");
        // The text runs down, so every opaque pixel of a column has the
        // color of the ramp at its x.
        for x in (0..200).step_by(5) {
            let expected = x as f32 / 200.0 * 255.0;
            for y in 0..200 {
                let (r, _, b, a) = pixel_rgba(&pixmap, x, y);
                if a == 255 {
                    assert!((f32::from(b) - expected).abs() < 4.0, "({x}, {y}): {r} {b}");
                    assert!(
                        (f32::from(r) - (255.0 - expected)).abs() < 4.0,
                        "({x}, {y}): {r} {b}"
                    );
                }
            }
        }
    }

    #[test]
    fn underline_stays_whole_across_a_glyph_that_winds_the_other_way() {
        // URW Gothic is a CFF face, whose contours wind the other way from
        // the TrueType faces that the crate embeds. A machine without it has
        // nothing to check.
        let family = "URW Gothic";
        let installed =
            crate::text::measure(family, 400, crate::scene::FontStyle::Normal, 96.0, "gyp")
                .is_some_and(|m| m.family() == family);
        if !installed {
            return;
        }
        let node = Text {
            fill: Paint::Solid(crate::scene::Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 1.0,
            }),
            transform: [1.0, 0.0, 0.0, 1.0, 160.0, 60.0],
            spec: TextSpec {
                size: 96.0,
                family: family.into(),
                text: "gyp".into(),
                ..TextSpec::default()
            },
            underline: true,
            ..Text::default()
        };
        let layout = crate::text::TextLayout::new(&node.spec).expect("node draws");
        let u = layout.underline_rect();
        let y = (60.0 + (u.y_top + u.y_bot) / 2.0) as u32;
        let x_l = (160.0 + u.x_l).ceil() as u32 + 1;
        let x_r = (160.0 + u.x_r).floor() as u32 - 1;
        let mut scene = Scene::new(320.0, 120.0);
        scene.add_text(node);
        let pixmap = render_to_pixmap(&scene, 1.0).expect("pixmap");
        for x in x_l..x_r {
            assert_eq!(pixel_rgba(&pixmap, x, y).3, 255, "a hole at x {x}");
        }
    }

    #[test]
    fn fit_scale_is_uncapped_geometry() {
        // Capping growth is the policy of the terminal.
        assert_eq!(fit_scale(200.0, 100.0, (50, 50)), 0.25);
        assert_eq!(fit_scale(10.0, 10.0, (1000, 1000)), 100.0);
        // A degenerate target leaves the frame at its own size.
        assert_eq!(fit_scale(10.0, 10.0, (0, 10)), 1.0);
    }

    #[test]
    fn draw_path_paints_rectangle() {
        let mut r = PixmapRenderer::new(1.0, 20.0, 20.0).expect("alloc");
        r.draw_path(&square_path(solid(0, 255, 0), 20.0));
        let pm = r.into_pixmap();
        assert_eq!(pixel_rgba(&pm, 10, 10), (0, 255, 0, 255));
    }

    #[test]
    fn a_bitmap_stretches_its_image_over_the_rect() {
        let pm = draw_on_canvas(Bitmap::fit(red_blue_png(), WHOLE_CANVAS));
        assert_eq!(pixel_rgba(&pm, 2, 5), (255, 0, 0, 255));
        assert_eq!(pixel_rgba(&pm, 17, 5), (0, 0, 255, 255));
    }

    #[test]
    fn a_nearest_bitmap_keeps_the_edge_between_its_pixels_hard() {
        let smooth = draw_on_canvas(Bitmap::fit(red_blue_png(), WHOLE_CANVAS));
        let nearest = draw_on_canvas(Bitmap {
            sampling: Sampling::Nearest,
            ..Bitmap::fit(red_blue_png(), WHOLE_CANVAS)
        });
        // The pixels next to the middle blend red and blue unless the
        // sampling is nearest.
        for x in [9, 10] {
            let (r, _, b, _) = pixel_rgba(&smooth, x, 5);
            assert!(r > 0 && b > 0, "{x}: {r} {b}");
        }
        assert_eq!(pixel_rgba(&nearest, 9, 5), (255, 0, 0, 255));
        assert_eq!(pixel_rgba(&nearest, 10, 5), (0, 0, 255, 255));
    }

    #[test]
    fn a_bitmap_whose_image_does_not_decode_draws_a_gray_box_with_a_red_cross() {
        let mut r = PixmapRenderer::new(1.0, 20.0, 10.0).expect("alloc");
        r.draw_bitmap(&Bitmap::fit(crate::asset::png_image(2, 1), WHOLE_CANVAS));
        let pm = r.into_pixmap();
        assert_eq!(pixel_rgba(&pm, 3, 5), (200, 200, 200, 255));
        // The cross goes through the center, antialiased.
        let (r, g, _, a) = pixel_rgba(&pm, 10, 5);
        assert!(r == 200 && g < 100 && a == 255, "{r} {g} {a}");
    }

    #[test]
    fn the_decoded_images_past_the_limit_drop_the_one_drawn_longest_ago() {
        // Eight of the largest images fill the limit.
        let image = |k: u32| crate::asset::png_image(2048, 2048 - k);
        let mut decoded = Decoded::default();
        for k in 0..8 {
            decoded.get(&image(k));
        }
        decoded.next_frame();
        decoded.get(&image(0));
        decoded.get(&image(8));
        assert!(decoded.images.contains_key(&image(0)));
        assert!(!decoded.images.contains_key(&image(1)));
        assert_eq!(decoded.images.len(), 8);
        let pixels: u64 = decoded.images.keys().map(Image::pixels).sum();
        assert_eq!(decoded.pixels, pixels);
    }

    #[test]
    fn a_frame_past_the_limit_keeps_the_images_that_fit() {
        let image = |k: u32| crate::asset::png_image(2048, 2048 - k);
        let mut decoded = Decoded::default();
        for _ in 0..2 {
            decoded.next_frame();
            for k in 0..10 {
                decoded.get(&image(k));
            }
        }
        for k in 0..8 {
            assert!(decoded.images.contains_key(&image(k)), "{k}");
        }
        assert_eq!(decoded.images.len(), 8);
    }

    #[test]
    fn an_image_past_the_limit_drawn_in_a_row_decodes_once() {
        let image = |k: u32| crate::asset::png_image(2048, 2048 - k);
        let mut decoded = Decoded::default();
        decoded.next_frame();
        for k in 0..9 {
            decoded.get(&image(k));
        }
        assert!(decoded.spill.as_ref().is_some_and(|(i, _)| *i == image(8)));
        // A head alone does not decode, so a second decode would give `None`.
        decoded.spill = Some((image(8), Pixmap::new(1, 1)));
        assert!(decoded.get(&image(8)).is_some());
        assert!(decoded.get(&image(9)).is_none());
        // In the next frame the spilled image fits, with the same pixels.
        decoded.spill = Some((image(8), Pixmap::new(1, 1)));
        decoded.next_frame();
        assert!(decoded.get(&image(8)).is_some());
        assert!(decoded.images.contains_key(&image(8)));
    }

    /// The whole canvas of [`draw_on_canvas`].
    const WHOLE_CANVAS: RotatedRect = RotatedRect {
        cx: 10.0,
        cy: 5.0,
        w: 20.0,
        h: 10.0,
        angle_deg: 0.0,
    };

    /// A PNG of two pixels, red on the left and blue on the right.
    fn red_blue_png() -> Image {
        let mut png = Pixmap::new(2, 1).expect("alloc");
        png.pixels_mut().copy_from_slice(&[
            tiny_skia::ColorU8::from_rgba(255, 0, 0, 255).premultiply(),
            tiny_skia::ColorU8::from_rgba(0, 0, 255, 255).premultiply(),
        ]);
        Image::new(png.encode_png().expect("encode")).expect("a PNG")
    }

    /// Draw `bitmap` over a canvas of 20x10.
    fn draw_on_canvas(bitmap: Bitmap) -> Pixmap {
        let mut r = PixmapRenderer::new(1.0, 20.0, 10.0).expect("alloc");
        r.draw_bitmap(&bitmap);
        r.into_pixmap()
    }

    #[test]
    fn with_clip_excludes_outside() {
        let mut r = PixmapRenderer::new(1.0, 20.0, 20.0).expect("alloc");
        r.with_clip(&square_clip(0.0, 0.0, 10.0), |c| {
            c.draw_path(&square_path(solid(0, 0, 255), 20.0));
        });
        let pm = r.into_pixmap();
        assert_eq!(pixel_rgba(&pm, 5, 5), (0, 0, 255, 255));
        assert_eq!(pixel_rgba(&pm, 15, 15).3, 0);
    }

    #[test]
    fn two_squares_in_a_layer_do_not_darken_where_they_overlap() {
        let mut scene = Scene::new(30.0, 20.0);
        scene.layer(0.5, |layer| {
            layer.add_path(rect(solid(0, 0, 255), 0.0, 0.0, 20.0, 20.0));
            layer.add_path(rect(solid(0, 0, 255), 10.0, 0.0, 20.0, 20.0));
        });
        let pm = render_to_pixmap(&scene, 1.0).expect("pixmap");
        assert_eq!(pixel_rgba(&pm, 5, 10), pixel_rgba(&pm, 15, 10));
        assert_eq!(pixel_rgba(&pm, 15, 10).3, 128);
    }

    #[test]
    fn a_layer_inside_a_clip_draws_only_inside_the_clip() {
        let mut scene = Scene::new(20.0, 20.0);
        scene.clip(square_clip(0.0, 0.0, 10.0), |clip| {
            clip.layer(0.5, |layer| {
                layer.add_path(rect(solid(0, 0, 255), 0.0, 0.0, 20.0, 20.0));
            });
        });
        let pm = render_to_pixmap(&scene, 1.0).expect("pixmap");
        assert_eq!(pixel_rgba(&pm, 5, 5).3, 128);
        assert_eq!(pixel_rgba(&pm, 15, 15).3, 0);
    }

    #[test]
    fn a_layer_past_the_most_layers_fades_its_elements() {
        fn nest(scene: &mut Scene, depth: usize) {
            if depth == 0 {
                scene.add_path(rect(solid(0, 0, 255), 0.0, 0.0, 10.0, 10.0));
            } else {
                scene.layer(0.5, |layer| nest(layer, depth - 1));
            }
        }
        let alpha = |depth| {
            let mut scene = Scene::new(10.0, 10.0);
            nest(&mut scene, depth);
            pixel_rgba(&render_to_pixmap(&scene, 1.0).expect("pixmap"), 5, 5).3
        };
        for depth in 1..MAX_LAYER_DEPTH + 3 {
            assert!(alpha(depth) < alpha(depth - 1), "{depth}");
        }
        // Six halves of 255.
        assert_eq!(alpha(MAX_LAYER_DEPTH + 2), 4);
    }

    #[test]
    fn a_frame_of_too_many_pixels_is_an_alloc_error() {
        let huge = Scene::new(1e6, 1e6);
        let error = AllocError {
            width: 1_000_000,
            height: 1_000_000,
        };
        assert_eq!(render_to_pixmap(&huge, 1.0).err(), Some(error));
        let mut r = PixmapRenderer::default();
        assert_eq!(r.render(&huge).err(), Some(error));
        // The last pixels of the limit still allocate.
        let side = (MAX_FRAME_PIXELS as f32).sqrt();
        assert!(render_to_pixmap(&Scene::new(side, side), 1.0).is_ok());
        assert!(render_to_pixmap(&Scene::new(side + 1.0, side), 1.0).is_err());
    }

    #[test]
    fn set_scale_resizes_the_next_render() {
        let mut scene = Scene::new(10.0, 10.0);
        scene.add_path(rect(solid(255, 0, 0), 0.0, 0.0, 5.0, 5.0));
        let mut r = PixmapRenderer::new(1.0, scene.width(), scene.height()).expect("alloc");
        r.render(&scene).expect("render");
        r.set_scale(2.0);
        let pm = r.render(&scene).expect("render");
        assert_eq!((pm.width(), pm.height()), (20, 20));
        assert_eq!(pixel_rgba(pm, 9, 9), (255, 0, 0, 255));
        assert_eq!(pixel_rgba(pm, 11, 11).3, 0);
    }

    #[test]
    fn a_replaced_pixmap_holds_the_last_render_and_the_next_one_resizes() {
        let mut scene = Scene::new(10.0, 10.0);
        scene.add_path(rect(solid(255, 0, 0), 0.0, 0.0, 5.0, 5.0));
        let mut r = PixmapRenderer::new(1.0, scene.width(), scene.height()).expect("alloc");
        r.render(&scene).expect("render");
        let last = r.replace_pixmap(Pixmap::new(1, 1).expect("alloc"));
        assert_eq!(pixel_rgba(&last, 2, 2), (255, 0, 0, 255));
        let pm = r.render(&scene).expect("render");
        assert_eq!((pm.width(), pm.height()), (10, 10));
        assert_eq!(pixel_rgba(pm, 2, 2), (255, 0, 0, 255));
    }

    #[test]
    fn a_clip_after_two_replaced_pixmaps_of_other_sizes_covers_the_frame() {
        let clipped = |side: f32| {
            let mut scene = Scene::new(side, side);
            scene.clip(square_clip(0.0, 0.0, side), |clip| {
                clip.add_path(rect(solid(0, 0, 255), 0.0, 0.0, side, side));
            });
            scene
        };
        let mut r = PixmapRenderer::default();
        r.render(&clipped(20.0)).expect("render");
        let large = r.replace_pixmap(Pixmap::new(1, 1).expect("alloc"));
        r.render(&clipped(10.0)).expect("render");
        r.replace_pixmap(large);
        let pm = r.render(&clipped(20.0)).expect("render");
        assert_eq!(pixel_rgba(pm, 15, 15), (0, 0, 255, 255));
    }

    #[test]
    fn a_recycled_mask_does_not_leak_the_previous_clip() {
        // Sibling clips reuse one mask buffer.
        let mut r = PixmapRenderer::new(1.0, 20.0, 20.0).expect("alloc");
        let cover = |c: &mut PixmapRenderer| c.draw_path(&square_path(solid(0, 0, 255), 20.0));
        r.with_clip(&square_clip(0.0, 0.0, 8.0), cover);
        r.with_clip(&square_clip(10.0, 0.0, 8.0), cover);

        let pm = r.into_pixmap();
        // Each clip painted its own box, and neither painted the gap.
        assert_eq!(pixel_rgba(&pm, 4, 4), (0, 0, 255, 255));
        assert_eq!(pixel_rgba(&pm, 14, 4), (0, 0, 255, 255));
        assert_eq!(pixel_rgba(&pm, 9, 4).3, 0);
    }

    #[test]
    fn an_empty_clip_hides_what_it_holds() {
        // A clip inside it hides what it holds too.
        let mut scene = Scene::new(20.0, 20.0);
        scene.clip(ClipPath::default(), |empty| {
            empty.add_path(rect(solid(0, 255, 0), 0.0, 0.0, 20.0, 20.0));
            empty.clip(square_clip(0.0, 0.0, 20.0), |inner| {
                inner.add_path(rect(solid(0, 0, 255), 0.0, 0.0, 20.0, 20.0));
            });
        });
        let pm = render_to_pixmap(&scene, 1.0).expect("pixmap");
        assert_eq!(pixel_rgba(&pm, 10, 10).3, 0);
    }

    #[test]
    fn a_closed_path_joins_the_first_corner_of_every_sub_path() {
        // Two squares, each stroked 6 wide from its top left corner. Only a
        // miter join covers the pixel outside that corner.
        let style = PathStyle {
            stroke: Paint::rgba(0, 0, 255, 1.0),
            stroke_width: 6.0,
            closed: true,
            ..PathStyle::default()
        };
        let mut scene = Scene::new(80.0, 40.0);
        scene.add_path(
            Path::builder(style, 10.0, 10.0)
                .line_to(30.0, 10.0)
                .line_to(30.0, 30.0)
                .line_to(10.0, 30.0)
                .move_to(50.0, 10.0)
                .line_to(70.0, 10.0)
                .line_to(70.0, 30.0)
                .line_to(50.0, 30.0)
                .build(),
        );
        let pm = render_to_pixmap(&scene, 1.0).expect("pixmap");
        assert_eq!(pixel_rgba(&pm, 8, 8), (0, 0, 255, 255));
        assert_eq!(pixel_rgba(&pm, 48, 8), (0, 0, 255, 255));
    }

    #[test]
    fn an_odd_dash_draws_like_its_array_repeated() {
        let line = |array: Vec<f32>| {
            let mut scene = Scene::new(40.0, 10.0);
            let style = PathStyle {
                stroke: Paint::rgba(0, 0, 0, 1.0),
                stroke_width: 2.0,
                dash: Dash::new(array, 0.0).map(Box::new),
                ..PathStyle::default()
            };
            scene.path(style, 0.0, 5.0).line_to(40.0, 5.0);
            render_to_pixmap(&scene, 1.0).expect("pixmap")
        };
        let odd = line(vec![5.0]);
        assert_eq!(pixel_rgba(&odd, 7, 5).3, 0, "no gap");
        assert_same_pixels(&line(vec![5.0, 5.0]), &odd);
    }

    /// A scene with a triangle whose apex is `apex` pixels above the canvas,
    /// drawn as a path, or as a clip around a filled canvas.
    fn triangle_above(apex: f32, as_clip: bool) -> Pixmap {
        let mut scene = Scene::new(100.0, 100.0);
        if as_clip {
            let clip = ClipPath::builder(FillRule::NonZero, 10.0, 10.0)
                .line_to(50.0, -apex)
                .line_to(90.0, 90.0)
                .build();
            scene.clip(clip, |inner| {
                inner.add_path(rect(solid(0, 0, 255), 0.0, 0.0, 100.0, 100.0));
            });
        } else {
            scene
                .path(solid(0, 0, 255), 10.0, 10.0)
                .line_to(50.0, -apex)
                .line_to(90.0, 90.0);
        }
        render_to_pixmap(&scene, 1.0).expect("pixmap")
    }

    #[test]
    fn a_path_that_reaches_too_far_draws_nothing() {
        assert_eq!(pixel_rgba(&triangle_above(1e8, false), 50, 20).3, 255);
        assert_eq!(pixel_rgba(&triangle_above(1e9, false), 50, 20).3, 0);
    }

    #[test]
    fn a_clip_that_reaches_too_far_hides_what_it_holds() {
        assert_eq!(pixel_rgba(&triangle_above(1e8, true), 50, 20).3, 255);
        assert_eq!(pixel_rgba(&triangle_above(1e9, true), 50, 20).3, 0);
    }

    #[test]
    fn a_stroke_that_reaches_too_far_draws_nothing() {
        let line = |width: f32| {
            let mut scene = Scene::new(100.0, 100.0);
            let style = PathStyle {
                stroke: Paint::rgba(0, 0, 255, 1.0),
                stroke_width: width,
                ..PathStyle::default()
            };
            scene.path(style, 10.0, 50.0).line_to(90.0, 50.0);
            render_to_pixmap(&scene, 1.0).expect("pixmap")
        };
        assert_eq!(pixel_rgba(&line(1e8), 50, 20).3, 255);
        assert_eq!(pixel_rgba(&line(1e10), 50, 20).3, 0);
    }

    #[test]
    fn a_text_that_reaches_too_far_draws_nothing() {
        // A glyph 20 units tall at a scale of 1e9 reaches past the canvas.
        let mut scene = Scene::new(100.0, 100.0);
        scene.add_text(Text {
            fill: Paint::Solid(Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 1.0,
            }),
            transform: [1e9, 0.0, 0.0, 1e9, 0.0, 50.0],
            spec: TextSpec {
                size: 20.0,
                text: "H".into(),
                ..TextSpec::default()
            },
            ..Text::default()
        });
        let pm = render_to_pixmap(&scene, 1.0).expect("pixmap");
        assert_eq!(pixel_rgba(&pm, 50, 20).3, 0);
    }

    #[test]
    fn a_radial_gradient_of_no_radius_paints_its_last_stop() {
        let stop = |offset, b| Stop {
            offset,
            color: Rgba {
                r: 0,
                g: 0,
                b,
                a: 1.0,
            },
        };
        let stops = vec![stop(0.0, 10), stop(1.0, 200)];
        let style = PathStyle {
            fill: Paint::radial(10.0, 10.0, 0.0, stops),
            ..PathStyle::default()
        };
        let mut scene = Scene::new(20.0, 20.0);
        scene.add_path(rect(style, 0.0, 0.0, 20.0, 20.0));
        let pm = render_to_pixmap(&scene, 1.0).expect("pixmap");
        assert_eq!(pixel_rgba(&pm, 10, 10), (0, 0, 200, 255));
    }

    #[test]
    fn a_gradient_axis_too_long_to_measure_paints_its_ramp() {
        let stop = |offset, b| Stop {
            offset,
            color: Rgba {
                r: 0,
                g: 0,
                b,
                a: 1.0,
            },
        };
        let style = PathStyle {
            fill: Paint::linear(-3e38, 0.0, 3e38, 0.0, vec![stop(0.0, 0), stop(1.0, 200)]),
            ..PathStyle::default()
        };
        let mut scene = Scene::new(20.0, 20.0);
        scene.add_path(rect(style, 0.0, 0.0, 20.0, 20.0));
        let pm = render_to_pixmap(&scene, 1.0).expect("pixmap");
        // The square sits at the middle of the axis, so it takes the middle
        // of the ramp, which is where the svg and the pdf put it too.
        assert_eq!(pixel_rgba(&pm, 10, 10), (0, 0, 100, 255));
    }

    fn rasterize(scene: &Scene) -> Pixmap {
        render_to_pixmap(scene, 1.0).expect("pixmap")
    }

    fn text_node(cx: f32, cy: f32, bw: f32, bh: f32, size: f32, text: &str) -> Text {
        Text {
            fill: Paint::Solid(Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 1.0,
            }),
            ..TextSpec {
                size,
                text: text.to_owned(),
                ..TextSpec::default()
            }
            .fit(RotatedRect {
                cx,
                cy,
                w: bw,
                h: bh,
                angle_deg: 0.0,
            })
            .expect("text fits")
        }
    }

    #[test]
    fn rasterize_filled_circle_center_is_red() {
        let mut scene = Scene::new(40.0, 40.0);
        {
            let mut p = scene.path(solid(255, 0, 0), 40.0, 20.0);
            p.arc_to(20.0, 20.0, 0.0, false, true, 0.0, 20.0);
            p.arc_to(20.0, 20.0, 0.0, false, true, 40.0, 20.0);
        }
        let pm = rasterize(&scene);
        let (r, g, b, _) = pixel_rgba(&pm, 20, 20);
        assert_eq!((r, g, b), (255, 0, 0));
    }

    #[test]
    fn a_background_fills_every_frame_of_any_size() {
        let mut renderer = PixmapRenderer::default();
        renderer.set_background(Rgba {
            r: 255,
            g: 255,
            b: 255,
            a: 1.0,
        });
        let mut dot = Scene::new(4.0, 4.0);
        {
            let mut p = dot.path(solid(255, 0, 0), 4.0, 2.0);
            p.arc_to(2.0, 2.0, 0.0, false, true, 0.0, 2.0);
            p.arc_to(2.0, 2.0, 0.0, false, true, 4.0, 2.0);
        }
        let pm = renderer.render(&dot).unwrap();
        assert_eq!(pixel_rgba(pm, 2, 2), (255, 0, 0, 255));
        // The next frame of the same size starts from the background too.
        let pm = renderer.render(&Scene::new(4.0, 4.0)).unwrap();
        assert_eq!(pixel_rgba(pm, 2, 2), (255, 255, 255, 255));
        let pm = renderer.render(&Scene::new(6.0, 3.0)).unwrap();
        assert_eq!(pixel_rgba(pm, 5, 2), (255, 255, 255, 255));
    }

    #[test]
    fn rasterize_default_background_is_transparent() {
        // An empty scene leaves the pixmap transparent.
        let scene = Scene::new(5.0, 5.0);
        let pm = rasterize(&scene);
        assert_eq!(pixel_rgba(&pm, 2, 2).3, 0);
    }

    fn count_opaque_pixels(pm: &Pixmap) -> u32 {
        let mut count: u32 = 0;
        for y in 0..pm.height() {
            for x in 0..pm.width() {
                if pixel_rgba(pm, x, y).3 > 0 {
                    count += 1;
                }
            }
        }
        count
    }

    #[test]
    fn rasterize_text_draws_some_pixels() {
        let mut scene = Scene::new(100.0, 40.0);
        scene.add_text(text_node(50.0, 20.0, 100.0, 40.0, 24.0, "Hi"));
        let pm = rasterize(&scene);
        assert!(count_opaque_pixels(&pm) > 50, "expected text pixels");
    }

    #[test]
    fn rasterize_text_corners_remain_transparent() {
        let mut scene = Scene::new(200.0, 60.0);
        scene.add_text(text_node(100.0, 30.0, 200.0, 60.0, 24.0, "Hi"));
        let pm = rasterize(&scene);
        assert_eq!(pixel_rgba(&pm, 0, 0).3, 0);
        assert_eq!(pixel_rgba(&pm, 199, 59).3, 0);
    }

    #[test]
    fn rasterize_text_handles_multibyte_utf8() {
        // A multi-byte UTF-8 string.
        let mut scene = Scene::new(100.0, 40.0);
        scene.add_text(text_node(50.0, 20.0, 100.0, 40.0, 24.0, "Olá"));
        let pm = rasterize(&scene);
        assert!(count_opaque_pixels(&pm) > 30, "expected text pixels");
    }

    #[test]
    fn rasterize_text_underline_adds_pixels() {
        // The same text with and without underline.
        let mut without = Scene::new(100.0, 40.0);
        without.add_text(text_node(50.0, 20.0, 100.0, 40.0, 24.0, "Hi"));
        let mut with = Scene::new(100.0, 40.0);
        let mut node = text_node(50.0, 20.0, 100.0, 40.0, 24.0, "Hi");
        node.underline = true;
        with.add_text(node);

        let n = count_opaque_pixels(&rasterize(&without));
        let u = count_opaque_pixels(&rasterize(&with));
        assert!(u > n, "underline should add pixels: {} -> {}", n, u);
    }

    #[test]
    fn rasterize_text_empty_renders_nothing() {
        // TextSpec::fit refuses an empty text, but the wire can still carry
        // one.
        let mut scene = Scene::new(10.0, 10.0);
        scene.add_text(Text {
            fill: Paint::Solid(Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 1.0,
            }),
            transform: [1.0, 0.0, 0.0, 1.0, 5.0, 5.0],
            spec: TextSpec {
                size: 24.0,
                ..TextSpec::default()
            },
            ..Text::default()
        });
        let pm = rasterize(&scene);
        assert_eq!(count_opaque_pixels(&pm), 0);
    }

    #[test]
    fn rasterize_linear_gradient_left_to_right() {
        // Black at x=0 to white at x=40. The left pixel is near black, the
        // right near white, and the middle a gray between them.
        let mut scene = Scene::new(40.0, 10.0);
        let style = PathStyle {
            fill: Paint::linear(
                0.0,
                0.0,
                40.0,
                0.0,
                vec![
                    Stop {
                        offset: 0.0,
                        color: Rgba {
                            r: 0,
                            g: 0,
                            b: 0,
                            a: 1.0,
                        },
                    },
                    Stop {
                        offset: 1.0,
                        color: Rgba {
                            r: 255,
                            g: 255,
                            b: 255,
                            a: 1.0,
                        },
                    },
                ],
            ),
            ..PathStyle::default()
        };
        scene.add_path(rect(style, 0.0, 0.0, 40.0, 10.0));
        let pm = rasterize(&scene);
        let left = pixel_rgba(&pm, 1, 5).0;
        let mid = pixel_rgba(&pm, 20, 5).0;
        let right = pixel_rgba(&pm, 38, 5).0;
        assert!(left < 32, "left pixel too bright: {left}");
        assert!(right > 223, "right pixel too dark: {right}");
        assert!(
            mid > left + 64 && mid + 64 < right,
            "mid pixel not between: left={left} mid={mid} right={right}"
        );
    }

    #[test]
    fn rasterize_radial_gradient_center_bright_edge_dark() {
        // White at the center, transparent at the edge.
        let mut scene = Scene::new(40.0, 40.0);
        let style = PathStyle {
            fill: Paint::radial(
                20.0,
                20.0,
                20.0,
                vec![
                    Stop {
                        offset: 0.0,
                        color: Rgba {
                            r: 255,
                            g: 255,
                            b: 255,
                            a: 1.0,
                        },
                    },
                    Stop {
                        offset: 1.0,
                        color: Rgba {
                            r: 0,
                            g: 0,
                            b: 0,
                            a: 0.0,
                        },
                    },
                ],
            ),
            ..PathStyle::default()
        };
        scene.add_path(rect(style, 0.0, 0.0, 40.0, 40.0));
        let pm = rasterize(&scene);
        let center_a = pixel_rgba(&pm, 20, 20).3;
        let edge_a = pixel_rgba(&pm, 0, 20).3;
        assert!(center_a > 200, "center too dim: {center_a}");
        assert!(edge_a < 40, "edge too opaque: {edge_a}");
    }

    #[test]
    fn rasterize_linear_gradient_reflect_mirrors_past_axis() {
        // The axis goes from x=0 to x=20. Reflect mirrors the gradient with
        // period 2, so x=10 (t=0.5) and x=30 (t=1.5) are both gray, where Pad
        // would clamp x=30 to white.
        let mut scene = Scene::new(80.0, 10.0);
        let style = PathStyle {
            fill: Paint::linear(
                0.0,
                0.0,
                20.0,
                0.0,
                vec![
                    Stop {
                        offset: 0.0,
                        color: Rgba {
                            r: 0,
                            g: 0,
                            b: 0,
                            a: 1.0,
                        },
                    },
                    Stop {
                        offset: 1.0,
                        color: Rgba {
                            r: 255,
                            g: 255,
                            b: 255,
                            a: 1.0,
                        },
                    },
                ],
            )
            .with_spread(crate::scene::SpreadMode::Reflect),
            ..PathStyle::default()
        };
        scene.add_path(rect(style, 0.0, 0.0, 80.0, 10.0));
        let pm = rasterize(&scene);
        let mid_axis = pixel_rgba(&pm, 10, 5).0; // t = 0.5
        let pad_zone = pixel_rgba(&pm, 30, 5).0; // t = 1.5
        let pad_far = pixel_rgba(&pm, 50, 5).0; // t = 2.5
        assert!(
            (mid_axis as i32 - pad_zone as i32).abs() < 30,
            "expected mirror near t=1.5; got mid={mid_axis} reflected={pad_zone}"
        );
        assert!(
            (mid_axis as i32 - pad_far as i32).abs() < 30,
            "expected period 2; got mid={mid_axis} two-periods-out={pad_far}"
        );
    }

    #[test]
    fn rasterize_dash_stroke_has_gaps() {
        // A [10, 10] dash on a stroke that starts at x=5. x=10 falls in an on
        // segment and x=20 in an off segment.
        let mut scene = Scene::new(100.0, 20.0);
        {
            let mut p = scene.path(
                PathStyle {
                    stroke: Paint::rgba(255, 0, 0, 1.0),
                    stroke_width: 3.0,
                    dash: Dash::new(vec![10.0, 10.0], 0.0).map(Box::new),
                    ..PathStyle::default()
                },
                5.0,
                10.0,
            );
            p.line_to(95.0, 10.0);
        }
        let pm = rasterize(&scene);
        let on = pixel_rgba(&pm, 10, 10).3;
        let off = pixel_rgba(&pm, 20, 10).3;
        assert!(on > 200, "on-segment expected opaque: {on}");
        assert!(off < 40, "off-segment expected transparent: {off}");
    }
}
