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

use crate::outline::PathSink;
use crate::renderer::{
    AllocError, AssetError, Renderer, RestoreOnDrop, TEXT_MITER_LIMIT, frame_side, sealed::Canvas,
};
use crate::scene::{
    Bitmap, ClipPath, FillRule, GradientGeom, LineCap, LineJoin, Paint, Path, Rgba, SpreadMode,
    Stop, Text,
};
use crate::text::TextLayout;

/// Rasterize a [`crate::scene::Scene`] at `scale`, where 1.0 is the frame's
/// own pixels. See [`fit_scale`].
pub fn render_to_pixmap(scene: &crate::scene::Scene, scale: f32) -> Option<Pixmap> {
    let mut renderer = PixmapRenderer::new(scale, scene.width(), scene.height())?;
    renderer.render(scene).ok()?;
    Some(renderer.into_pixmap())
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
    /// The builder of the next path. A finished path clears back into it, so
    /// the next path reuses its capacity.
    builder: PathBuilder,
    assets: Assets,
    /// The color of the pixmap before a frame draws on it.
    background: SkColor,
}

impl PixmapRenderer {
    /// A surface for frames at `scale`, where 1.0 is the frame's own pixels,
    /// sized first for a `width × height` frame. `None` when the surface
    /// cannot be allocated.
    pub fn new(scale: f32, width: f32, height: f32) -> Option<Self> {
        let base = base(scale);
        let (out_w, out_h) = out_size(width, height, base.sx);
        Some(Self {
            pixmap: Pixmap::new(out_w, out_h)?,
            base,
            clip_stack: Vec::new(),
            mask_pool: Vec::new(),
            builder: PathBuilder::new(),
            assets: Assets::default(),
            background: SkColor::TRANSPARENT,
        })
    }

    /// The images that a [`Bitmap`] of the next frames names.
    pub fn assets_mut(&mut self) -> &mut Assets {
        &mut self.assets
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
        }
        std::mem::replace(&mut self.pixmap, pixmap)
    }

    /// The pixmap of the last render.
    pub fn into_pixmap(self) -> Pixmap {
        self.pixmap
    }
}

impl Default for PixmapRenderer {
    /// A surface at scale 1.0 that takes its size at the first render.
    fn default() -> Self {
        Self::new(1.0, 1.0, 1.0).expect("a 1x1 pixmap always allocates")
    }
}

/// The images that the bitmaps of a scene name by id, decoded.
#[derive(Clone, Debug, Default)]
pub struct Assets {
    images: HashMap<u32, Pixmap>,
}

impl Assets {
    /// Decode the PNG in `blob` and keep it for the bitmaps of `id`, in place
    /// of the image that `id` named before.
    pub fn insert_png(&mut self, id: u32, blob: &[u8]) -> Result<(), AssetError> {
        let image = Pixmap::decode_png(blob).map_err(|e| AssetError(Box::new(e)))?;
        self.images.insert(id, image);
        Ok(())
    }

    /// Drop the image of `id`, if there is one.
    pub fn remove(&mut self, id: u32) {
        self.images.remove(&id);
    }
}

impl Canvas for PixmapRenderer {
    /// Reallocates the surface when the scaled size changed, then fills it
    /// with the background.
    fn ensure_size(&mut self, width: f32, height: f32) -> Result<(), AllocError> {
        let (out_w, out_h) = out_size(width, height, self.base.sx);
        // A frame ends with an empty clip stack, and its masks serve the next
        // frame.
        self.mask_pool
            .extend(self.clip_stack.drain(..).filter_map(Clip::into_mask));
        if (out_w, out_h) != (self.pixmap.width(), self.pixmap.height()) {
            // Masks are canvas-sized, so a resize invalidates every pooled one.
            self.mask_pool.clear();
            self.pixmap = Pixmap::new(out_w, out_h).ok_or(AllocError {
                width: out_w,
                height: out_h,
            })?;
        }
        self.pixmap.fill(self.background);
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
        path.segments().outline(&mut builder);
        if style.closed {
            builder.close();
        }
        // A path with no segments builds nothing. The pdf and the svg test
        // the segments before they write, and here the builder answers.
        let Some(sk_path) = builder.finish() else {
            return;
        };
        if do_fill && within_reach(&sk_path, self.base, 0.0) {
            let paint = sk_paint(paint_to_shader(&style.fill));
            self.pixmap.fill_path(
                &sk_path,
                &paint,
                sk_fill_rule(style.fill_rule),
                self.base,
                mask,
            );
        }
        if do_stroke {
            let paint = sk_paint(paint_to_shader(&style.stroke));
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
        render_text(node, &mut self.pixmap, mask, self.base, &mut self.builder);
    }

    /// Draw the image of `bitmap.id`. An id with no image draws nothing.
    fn draw_bitmap(&mut self, bitmap: &Bitmap) {
        let Some(image) = self.assets.images.get(&bitmap.id) else {
            return;
        };
        let mask = match in_effect(&self.clip_stack) {
            InEffect::Nothing => return,
            InEffect::Everything => None,
            InEffect::Through(mask) => Some(mask),
        };
        let (w, h) = (image.width() as f32, image.height() as f32);
        let [a, b, c, d, e, f] = bitmap.transform;
        // The transform puts the center of the image at the origin, and the
        // pixmap starts at its top left corner.
        let transform = Transform::from_translate(-w / 2.0, -h / 2.0)
            .post_concat(Transform::from_row(a, b, c, d, e, f))
            .post_concat(self.base);
        let reach = SkRect::from_xywh(0.0, 0.0, w, h)
            .is_some_and(|rect| rect_within_reach(rect, transform));
        if reach {
            let paint = PixmapPaint {
                quality: FilterQuality::Bilinear,
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
}

impl Renderer for PixmapRenderer {
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

/// A gradient tiny-skia turns down falls back to the primary color, the first
/// stop, so the path still draws. Nothing arrives here that it turns down: a
/// gradient with no stops does not draw at all, one with no extent is a solid
/// color before it gets here, a line too long to measure is shortened by the
/// scene, and the transform is the identity. The fallback stands for a rule a
/// later version adds.
fn paint_to_shader(p: &Paint) -> SkShader<'static> {
    let g = match p {
        Paint::Solid(c) => return SkShader::SolidColor(sk_color(*c)),
        Paint::Gradient(g) => g,
    };
    let stops = sk_stops(g.stops());
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
    .unwrap_or_else(|| SkShader::SolidColor(sk_color(p.primary_color())))
}

fn sk_color(c: Rgba) -> SkColor {
    SkColor::from_rgba8(c.r, c.g, c.b, (c.a * 255.0).round().clamp(0.0, 255.0) as u8)
}

fn sk_stops(stops: &[Stop]) -> Vec<SkStop> {
    stops
        .iter()
        .map(|s| SkStop::new(s.offset, sk_color(s.color)))
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

    paint_text_path(node, pixmap, mask, transform, builder, |out| {
        layout.outline(out)
    });
    // The underline paints on its own. In one path, a glyph that winds the
    // other way from the rectangle would cancel it where the two cross.
    if node.underline {
        paint_text_path(node, pixmap, mask, transform, builder, |out| {
            layout.outline_underline(out)
        });
    }
}

/// Builds a path of `node` with `outline`, then fills and strokes it. The
/// finished path clears back into `builder`.
fn paint_text_path(
    node: &Text,
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
    if node.draws_fill() && within_reach(&path, transform, 0.0) {
        let paint = sk_paint(SkShader::SolidColor(sk_color(node.fill)));
        // A TrueType glyph fills with non-zero winding.
        pixmap.fill_path(&path, &paint, SkFillRule::Winding, transform, mask);
    }
    if node.draws_stroke() {
        let paint = sk_paint(SkShader::SolidColor(sk_color(node.stroke)));
        let stroke = Stroke {
            width: node.stroke_width,
            miter_limit: TEXT_MITER_LIMIT,
            dash: None,
            ..Stroke::default()
        };
        if stroke_within_reach(&path, &stroke, transform) {
            pixmap.stroke_path(&path, &paint, &stroke, transform, mask);
        }
    }
    *builder = path.clear();
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
/// through [`crate::scene::Segments::outline`] and [`crate::text::TextLayout::outline`].
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
            fill: crate::scene::Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 1.0,
            },
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
        scene.text(node);
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
        let pm = draw_red_blue_png(1);
        assert_eq!(pixel_rgba(&pm, 2, 5), (255, 0, 0, 255));
        assert_eq!(pixel_rgba(&pm, 17, 5), (0, 0, 255, 255));
    }

    #[test]
    fn a_bitmap_with_no_image_draws_nothing() {
        let pm = draw_red_blue_png(2);
        assert!(pm.pixels().iter().all(|p| p.alpha() == 0));
    }

    #[test]
    fn insert_png_refuses_what_is_not_a_png() {
        let mut assets = Assets::default();
        assert!(assets.insert_png(1, b"GIF89a").is_err());
    }

    #[test]
    fn remove_drops_the_image_of_an_id() {
        let mut assets = Assets::default();
        let png = Pixmap::new(1, 1)
            .expect("alloc")
            .encode_png()
            .expect("encode");
        assets.insert_png(1, &png).expect("a PNG decodes");
        assets.remove(2);
        assert!(assets.images.contains_key(&1));
        assets.remove(1);
        assert!(assets.images.is_empty());
    }

    /// Keep a PNG of two pixels, red on the left and blue on the right, as
    /// id 1, and draw the bitmap of `id` over a canvas of 20x10.
    fn draw_red_blue_png(id: u32) -> Pixmap {
        let mut png = Pixmap::new(2, 1).expect("alloc");
        png.pixels_mut().copy_from_slice(&[
            tiny_skia::ColorU8::from_rgba(255, 0, 0, 255).premultiply(),
            tiny_skia::ColorU8::from_rgba(0, 0, 255, 255).premultiply(),
        ]);
        let mut r = PixmapRenderer::new(1.0, 20.0, 10.0).expect("alloc");
        r.assets_mut()
            .insert_png(1, &png.encode_png().expect("encode"))
            .expect("a PNG decodes");
        let rect = RotatedRect {
            cx: 10.0,
            cy: 5.0,
            w: 20.0,
            h: 10.0,
            angle_deg: 0.0,
        };
        r.draw_bitmap(&Bitmap::fit(id, 2, 1, rect));
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
    fn render_stream_matches_render_for_flat_path() {
        let mut scene = Scene::new(10.0, 10.0);
        scene.add_path(rect(solid(255, 0, 0), 0.0, 0.0, 10.0, 10.0));
        let bytes = crate::wire::scene::encode(&scene);

        let mut r_atomic = PixmapRenderer::new(1.0, scene.width(), scene.height()).expect("alloc");
        let pm_atomic = r_atomic.render(&scene).expect("render");

        // A different size, so render_stream has to resize.
        let mut r_stream = PixmapRenderer::new(1.0, 1.0, 1.0).expect("alloc");
        let pm_streamed = r_stream.render_stream(&bytes[..]).expect("decode + render");

        assert_same_pixels(pm_atomic, pm_streamed);
        assert_eq!(pixel_rgba(pm_streamed, 5, 5), (255, 0, 0, 255));
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
            let mut clip_scope = scene.clip(square_clip(0.0, 0.0, side));
            clip_scope.add_path(rect(solid(0, 0, 255), 0.0, 0.0, side, side));
            drop(clip_scope);
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
    fn streamed_paths_do_not_inherit_the_scratch() {
        // Every path of a frame decodes into one reused Path. A stale segment
        // would show after a long path followed by a short one.
        let mut scene = Scene::new(20.0, 20.0);
        {
            let mut p = scene.path(solid(0, 0, 255), 0.0, 0.0);
            p.line_to(20.0, 0.0);
            p.line_to(20.0, 8.0);
            p.cubic_to(14.0, 10.0, 6.0, 10.0, 0.0, 8.0);
        }
        {
            let mut p = scene.path(solid(255, 0, 0), 0.0, 12.0);
            p.line_to(20.0, 20.0);
        }
        let bytes = crate::wire::scene::encode(&scene);

        let mut direct = PixmapRenderer::new(1.0, scene.width(), scene.height()).expect("alloc");
        let expected = direct.render(&scene).expect("render").clone();

        let mut streamed = PixmapRenderer::new(1.0, scene.width(), scene.height()).expect("alloc");
        let got = streamed.render_stream(&bytes[..]).expect("decode + render");

        assert_same_pixels(&expected, got);
    }

    #[test]
    fn render_stream_handles_nested_clip() {
        let mut scene = Scene::new(20.0, 20.0);
        {
            let mut clip_scope = scene.clip(square_clip(0.0, 0.0, 10.0));
            clip_scope.add_path(rect(solid(0, 0, 255), 0.0, 0.0, 20.0, 20.0));
        }
        let bytes = crate::wire::scene::encode(&scene);

        let mut r = PixmapRenderer::new(1.0, 1.0, 1.0).expect("alloc");
        let pm = r.render_stream(&bytes[..]).expect("decode + render");
        assert_eq!(pixel_rgba(pm, 5, 5), (0, 0, 255, 255));
        assert_eq!(pixel_rgba(pm, 15, 15).3, 0);
    }

    #[test]
    fn render_stream_skips_an_element_of_an_unknown_arm() {
        // A red path that becomes an arm of a newer schema, under a blue one.
        let mut scene = Scene::new(10.0, 10.0);
        scene.add_path(rect(solid(255, 0, 0), 0.0, 0.0, 10.0, 10.0));
        scene.add_path(rect(solid(0, 0, 255), 0.0, 0.0, 5.0, 5.0));
        let bytes =
            crate::wire::with_unknown_scene_value(&crate::wire::scene::encode(&scene), |m| {
                crate::wire::tag_of(m.get_elements().unwrap().get(0))
            });

        let mut r = PixmapRenderer::new(1.0, 1.0, 1.0).expect("alloc");
        let pm = r.render_stream(&bytes[..]).expect("decode + render");
        assert_eq!(pixel_rgba(pm, 2, 2), (0, 0, 255, 255));
        assert_eq!(pixel_rgba(pm, 7, 7).3, 0);
    }

    #[test]
    fn render_stream_skips_an_element_that_holds_an_unknown_value() {
        // A red path with a paint arm of a newer schema, a clip with verbs of
        // a newer schema around another red path, and a blue path.
        use crate::scene_capnp::element::Which;
        use crate::wire::scene::encode;
        use crate::wire::{tag_of, with_unknown_scene_value};
        let mut scene = Scene::new(10.0, 10.0);
        scene.add_path(rect(solid(255, 0, 0), 0.0, 0.0, 10.0, 10.0));
        {
            let mut clip_scope = scene.clip(square_clip(0.0, 0.0, 10.0));
            clip_scope.add_path(rect(solid(255, 0, 0), 0.0, 0.0, 10.0, 10.0));
        }
        scene.add_path(rect(solid(0, 0, 255), 0.0, 0.0, 5.0, 5.0));
        let bytes = with_unknown_scene_value(&encode(&scene), |m| {
            let Ok(Which::Path(p)) = m.get_elements().unwrap().get(0).which() else {
                panic!("expected Path");
            };
            tag_of(p.unwrap().get_style().unwrap().get_fill().unwrap())
        });
        let bytes = with_unknown_scene_value(&bytes, |m| {
            let Ok(Which::Clipped(c)) = m.get_elements().unwrap().get(1).which() else {
                panic!("expected Clipped");
            };
            c.unwrap().get_clip().unwrap().get_verbs().unwrap().as_ptr()
        });

        let mut r = PixmapRenderer::new(1.0, 1.0, 1.0).expect("alloc");
        let pm = r.render_stream(&bytes[..]).expect("decode + render");
        assert_eq!(pixel_rgba(pm, 2, 2), (0, 0, 255, 255));
        assert_eq!(pixel_rgba(pm, 7, 7).3, 0);
    }

    #[test]
    fn an_empty_clip_hides_what_it_holds() {
        // A clip inside it hides what it holds too.
        let mut scene = Scene::new(20.0, 20.0);
        {
            let mut empty = scene.clip(ClipPath::default());
            empty.add_path(rect(solid(0, 255, 0), 0.0, 0.0, 20.0, 20.0));
            let mut inner = empty.clip(square_clip(0.0, 0.0, 20.0));
            inner.add_path(rect(solid(0, 0, 255), 0.0, 0.0, 20.0, 20.0));
        }
        let pm = render_to_pixmap(&scene, 1.0).expect("pixmap");
        assert_eq!(pixel_rgba(&pm, 10, 10).3, 0);
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
            let mut inner = scene.clip(clip);
            inner.add_path(rect(solid(0, 0, 255), 0.0, 0.0, 100.0, 100.0));
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
        scene.text(Text {
            fill: Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 1.0,
            },
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
    fn render_stream_skips_an_element_that_holds_a_non_finite_float() {
        // A red path with a miter limit and a clip with a coordinate that
        // become NaN on the wire, the clip around another red path, and a
        // blue path.
        let mark = 777.0;
        let mut scene = Scene::new(10.0, 10.0);
        let red = PathStyle {
            miter_limit: mark,
            ..solid(255, 0, 0)
        };
        scene.add_path(rect(red, 0.0, 0.0, 10.0, 10.0));
        {
            let mut clip_scope = scene.clip(square_clip(0.0, 0.0, mark));
            clip_scope.add_path(rect(solid(255, 0, 0), 0.0, 0.0, 10.0, 10.0));
        }
        scene.add_path(rect(solid(0, 0, 255), 0.0, 0.0, 5.0, 5.0));
        let bytes = crate::wire::with_float(&crate::wire::scene::encode(&scene), mark, f32::NAN);

        let mut r = PixmapRenderer::new(1.0, 1.0, 1.0).expect("alloc");
        let pm = r.render_stream(&bytes[..]).expect("decode + render");
        assert_eq!(pixel_rgba(pm, 2, 2), (0, 0, 255, 255));
        assert_eq!(pixel_rgba(pm, 7, 7).3, 0);
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
            fill: Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 1.0,
            },
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
        scene.text(text_node(50.0, 20.0, 100.0, 40.0, 24.0, "Hi"));
        let pm = rasterize(&scene);
        assert!(count_opaque_pixels(&pm) > 50, "expected text pixels");
    }

    #[test]
    fn rasterize_text_corners_remain_transparent() {
        let mut scene = Scene::new(200.0, 60.0);
        scene.text(text_node(100.0, 30.0, 200.0, 60.0, 24.0, "Hi"));
        let pm = rasterize(&scene);
        assert_eq!(pixel_rgba(&pm, 0, 0).3, 0);
        assert_eq!(pixel_rgba(&pm, 199, 59).3, 0);
    }

    #[test]
    fn rasterize_text_handles_multibyte_utf8() {
        // A multi-byte UTF-8 string.
        let mut scene = Scene::new(100.0, 40.0);
        scene.text(text_node(50.0, 20.0, 100.0, 40.0, 24.0, "Olá"));
        let pm = rasterize(&scene);
        assert!(count_opaque_pixels(&pm) > 30, "expected text pixels");
    }

    #[test]
    fn rasterize_text_underline_adds_pixels() {
        // The same text with and without underline.
        let mut without = Scene::new(100.0, 40.0);
        without.text(text_node(50.0, 20.0, 100.0, 40.0, 24.0, "Hi"));
        let mut with = Scene::new(100.0, 40.0);
        let mut node = text_node(50.0, 20.0, 100.0, 40.0, 24.0, "Hi");
        node.underline = true;
        with.text(node);

        let n = count_opaque_pixels(&rasterize(&without));
        let u = count_opaque_pixels(&rasterize(&with));
        assert!(u > n, "underline should add pixels: {} -> {}", n, u);
    }

    #[test]
    fn rasterize_text_empty_renders_nothing() {
        // TextSpec::fit refuses an empty text, but the wire can still carry
        // one.
        let mut scene = Scene::new(10.0, 10.0);
        scene.text(Text {
            fill: Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 1.0,
            },
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
