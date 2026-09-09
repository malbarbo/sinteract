//! Rasterize a [`crate::scene::Scene`] into a tiny-skia [`Pixmap`]. It needs
//! no tty or window, so it builds on wasm. The terminal and the window
//! present the pixels.

use tiny_skia::{
    Color as SkColor, FillRule as SkFillRule, GradientStop as SkStop, LineCap as SkLineCap,
    LineJoin as SkLineJoin, Mask, Paint as SkPaint, PathBuilder, Pixmap, Point as SkPoint,
    Shader as SkShader, SpreadMode as SkSpread, Stroke, StrokeDash, Transform,
};

use crate::renderer::{Renderer, sealed::Paint};
use crate::scene::{
    ClipPath, FillRule, GradientGeom, LineCap, LineJoin, Paint as IrPaint, Path, Rgba, Segment,
    Segments, TextNode,
};

/// A raster surface. It reallocates its pixmap only when the frame size
/// changes.
struct PixmapRenderer {
    pixmap: Pixmap,
    /// The scale as a transform, applied to every path.
    base: Transform,
    clip_stack: Vec<Mask>,
    /// Masks popped off `clip_stack`, for the next push. Every mask is
    /// canvas-sized, so any one fits.
    mask_pool: Vec<Mask>,
    out_w: u32,
    out_h: u32,
    /// The scale from the caller, who decides whether a frame may grow.
    scale: f32,
}

/// A frame's size in whole output pixels.
fn frame_px(width: f32, height: f32) -> (u32, u32) {
    (width.ceil().max(1.0) as u32, height.ceil().max(1.0) as u32)
}

/// The uniform scale that fits a `width × height` frame inside `target`
/// pixels. It can exceed 1.0. The caller caps it.
pub(crate) fn fit_scale(width: f32, height: f32, target: (u32, u32)) -> f32 {
    let (tw, th) = target;
    if tw == 0 || th == 0 {
        return 1.0;
    }
    let (w, h) = frame_px(width, height);
    (tw as f32 / w as f32).min(th as f32 / h as f32)
}

/// The output size and the transform of a frame at `scale`.
fn fit(width: f32, height: f32, scale: f32) -> (u32, u32, Transform) {
    let (w, h) = frame_px(width, height);
    // A zero or negative scale would allocate nothing to draw into.
    let s = scale.max(1e-3);
    let out_w = ((w as f32) * s).ceil().max(1.0) as u32;
    let out_h = ((h as f32) * s).ceil().max(1.0) as u32;
    (out_w, out_h, Transform::from_scale(s, s))
}

/// A transparent pixmap, so the background of the backend shows through.
/// `None` when the allocation fails.
fn new_pixmap(out_w: u32, out_h: u32) -> Option<Pixmap> {
    Pixmap::new(out_w, out_h).map(|mut pm| {
        pm.fill(tiny_skia::Color::TRANSPARENT);
        pm
    })
}

/// Returns `true` if any segment was appended, a lone move included, `false`
/// otherwise.
fn append_segments(builder: &mut PathBuilder, segments: Segments<'_>) -> bool {
    let mut any = false;
    for seg in segments {
        match seg {
            Segment::Move { x, y } => builder.move_to(x, y),
            Segment::Line { x, y } => builder.line_to(x, y),
            Segment::Quad { cx, cy, x, y } => builder.quad_to(cx, cy, x, y),
            Segment::Cubic {
                c1x,
                c1y,
                c2x,
                c2y,
                x,
                y,
            } => builder.cubic_to(c1x, c1y, c2x, c2y, x, y),
        }
        any = true;
    }
    any
}

impl PixmapRenderer {
    /// `None` when the surface cannot be allocated.
    fn new(scale: f32, width: f32, height: f32) -> Option<Self> {
        let (out_w, out_h, base) = fit(width, height, scale);
        Some(Self {
            pixmap: new_pixmap(out_w, out_h)?,
            base,
            clip_stack: Vec::new(),
            mask_pool: Vec::new(),
            out_w,
            out_h,
            scale,
        })
    }

    fn into_pixmap(self) -> Pixmap {
        self.pixmap
    }

    /// Returns `true` if a mask was pushed, `false` otherwise. An empty clip
    /// pushes nothing, and the guard in [`Paint::with_clip`] pops only what
    /// was pushed.
    fn push_clip(&mut self, clip: &ClipPath) -> bool {
        let mut builder = PathBuilder::new();
        append_segments(&mut builder, clip.segments());
        // A sub-path of a clip is closed, as in an SVG clipPath. tiny_skia
        // accepts a close on a closed contour.
        builder.close();
        let Some(path) = builder.finish() else {
            return false;
        };
        let Some(mut mask) = self.take_mask() else {
            return false;
        };
        mask.fill_path(&path, sk_fill_rule(clip.fill_rule), true, self.base);
        if let Some(parent) = self.clip_stack.last() {
            // Mask::intersect_path would rasterize into a second canvas-sized
            // mask.
            for (a, b) in mask.data_mut().iter_mut().zip(parent.data()) {
                *a = mask_mul(*a, *b);
            }
        }
        self.clip_stack.push(mask);
        true
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
            None => Mask::new(self.out_w, self.out_h),
        }
    }
}

/// `a * b / 255`, rounded as tiny-skia does, so the result matches
/// `Mask::intersect_path`.
fn mask_mul(a: u8, b: u8) -> u8 {
    let prod = u32::from(a) * u32::from(b) + 128;
    ((prod + (prod >> 8)) >> 8) as u8
}

/// Pops the clip of [`Paint::with_clip`] when dropped, so the stack stays
/// balanced when the body panics.
struct ClipGuard<'a> {
    canvas: &'a mut PixmapRenderer,
    pushed: bool,
}

impl Drop for ClipGuard<'_> {
    fn drop(&mut self) {
        if self.pushed
            && let Some(mask) = self.canvas.clip_stack.pop()
        {
            self.canvas.mask_pool.push(mask);
        }
    }
}

impl Paint for PixmapRenderer {
    /// Clears the surface, and reallocates it when the scaled size changed.
    fn ensure_size(&mut self, width: f32, height: f32) -> Result<(), crate::renderer::AllocError> {
        let (out_w, out_h, base) = fit(width, height, self.scale);
        self.base = base;
        // A frame ends with an empty clip stack, and its masks serve the next
        // frame.
        self.mask_pool.append(&mut self.clip_stack);
        if (out_w, out_h) == (self.out_w, self.out_h) {
            self.pixmap.fill(tiny_skia::Color::TRANSPARENT);
        } else {
            // Masks are canvas-sized, so a resize invalidates every pooled one.
            self.mask_pool.clear();
            self.pixmap = new_pixmap(out_w, out_h).ok_or(crate::renderer::AllocError {
                width: out_w,
                height: out_h,
            })?;
            self.out_w = out_w;
            self.out_h = out_h;
        }
        Ok(())
    }

    fn draw_path(&mut self, path: &Path) {
        let style = &path.style;
        let mut builder = PathBuilder::new();
        if !append_segments(&mut builder, path.segments()) {
            return;
        }
        if style.closed {
            builder.close();
        }
        let Some(sk_path) = builder.finish() else {
            return;
        };
        let mask = self.clip_stack.last();

        if style.draws_fill() {
            let paint = SkPaint {
                shader: paint_to_shader(&style.fill),
                anti_alias: true,
                ..SkPaint::default()
            };
            self.pixmap.fill_path(
                &sk_path,
                &paint,
                sk_fill_rule(style.fill_rule),
                self.base,
                mask,
            );
        }
        if style.draws_stroke() {
            let paint = SkPaint {
                shader: paint_to_shader(&style.stroke),
                anti_alias: true,
                ..SkPaint::default()
            };
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
            self.pixmap
                .stroke_path(&sk_path, &paint, &stroke, self.base, mask);
        }
    }

    fn draw_text(&mut self, node: &TextNode) {
        render_text(node, &mut self.pixmap, self.clip_stack.last(), self.base);
    }

    fn with_clip<T>(&mut self, clip: &ClipPath, inside: impl FnOnce(&mut Self) -> T) -> T {
        let pushed = self.push_clip(clip);
        let guard = ClipGuard {
            canvas: self,
            pushed,
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

fn sk_color(c: Rgba) -> SkColor {
    SkColor::from_rgba8(c.r, c.g, c.b, (c.a * 255.0).round().clamp(0.0, 255.0) as u8)
}

fn sk_stops(stops: &[crate::scene::Stop]) -> Vec<SkStop> {
    stops
        .iter()
        .map(|s| SkStop::new(s.offset, sk_color(s.color)))
        .collect()
}

fn sk_spread(s: crate::scene::SpreadMode) -> SkSpread {
    match s {
        crate::scene::SpreadMode::Pad => SkSpread::Pad,
        crate::scene::SpreadMode::Reflect => SkSpread::Reflect,
        crate::scene::SpreadMode::Repeat => SkSpread::Repeat,
    }
}

/// A gradient that tiny-skia rejects, for a degenerate line or no stops,
/// falls back to the primary color, so the path still draws.
fn paint_to_shader(p: &IrPaint) -> SkShader<'static> {
    let g = match p {
        IrPaint::Solid(c) => return SkShader::SolidColor(sk_color(*c)),
        IrPaint::Gradient(g) => g,
    };
    let stops = sk_stops(&g.stops);
    let spread = sk_spread(g.spread);
    match g.geom {
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

/// Rasterize a [`crate::scene::Scene`] at `scale`, where 1.0 is the frame's
/// own pixels. See [`fit_scale`].
pub(crate) fn rasterize_scene(scene: &crate::scene::Scene, scale: f32) -> Option<Pixmap> {
    let mut renderer = PixmapRenderer::new(scale, scene.width, scene.height)?;
    renderer.render(scene).ok()?;
    Some(renderer.into_pixmap())
}

// -----------------------------------------------------------------------------
// Text
// -----------------------------------------------------------------------------

fn render_text(node: &TextNode, pixmap: &mut Pixmap, mask: Option<&Mask>, base: Transform) {
    let Some(layout) = crate::text::layout_text(node) else {
        return;
    };

    let mut builder = PathBuilder::new();
    let mut adapter = SkiaOutline { b: &mut builder };
    crate::text::outline_layout(&layout, &node.text, &mut adapter);

    if node.underline {
        crate::text::outline_underline(&layout, &mut adapter);
    }

    let Some(path) = builder.finish() else {
        return;
    };

    // The transform is in the `cm` convention, which is the order of
    // Transform::from_row.
    let local = Transform::from_row(
        node.transform[0],
        node.transform[1],
        node.transform[2],
        node.transform[3],
        node.transform[4],
        node.transform[5],
    );
    // The text transform applies first, then the scale.
    let transform = local.post_concat(base);

    if node.fill.a > 0.0 {
        let mut paint = SkPaint::default();
        paint.set_color(sk_color(node.fill));
        paint.anti_alias = true;
        // A TrueType glyph fills with non-zero winding.
        pixmap.fill_path(&path, &paint, SkFillRule::Winding, transform, mask);
    }
    if node.stroke.a > 0.0 && node.stroke_width > 0.0 {
        let mut paint = SkPaint::default();
        paint.set_color(sk_color(node.stroke));
        paint.anti_alias = true;
        // A glyph is a closed smooth contour, so the cap and the join do not
        // show.
        let stroke = Stroke {
            width: node.stroke_width,
            miter_limit: 10.0,
            dash: None,
            ..Stroke::default()
        };
        pixmap.stroke_path(&path, &paint, &stroke, transform, mask);
    }
}

struct SkiaOutline<'a> {
    b: &'a mut PathBuilder,
}

impl<'a> crate::text::OutlineBuilder for SkiaOutline<'a> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.b.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.b.line_to(x, y);
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.b.quad_to(cx, cy, x, y);
    }
    fn cubic_to(&mut self, cx1: f32, cy1: f32, cx2: f32, cy2: f32, x: f32, y: f32) {
        self.b.cubic_to(cx1, cy1, cx2, cy2, x, y);
    }
    fn close(&mut self) {
        self.b.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{PathStyle, Scene};

    fn pixel_rgba(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let p = pixmap.pixel(x, y).expect("pixel in range");
        let p = p.demultiply();
        (p.red(), p.green(), p.blue(), p.alpha())
    }

    fn solid(r: u8, g: u8, b: u8) -> PathStyle {
        PathStyle {
            fill: IrPaint::rgba(r, g, b, 1.0),
            ..PathStyle::default()
        }
    }

    fn rect_path(scene: &mut Scene, style: PathStyle, x: f32, y: f32, w: f32, h: f32) {
        let mut p = scene.path(style);
        p.move_to(x, y);
        p.line_to(x + w, y);
        p.line_to(x + w, y + h);
        p.line_to(x, y + h);
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
        let path = Path::builder(solid(0, 255, 0))
            .move_to(0.0, 0.0)
            .line_to(20.0, 0.0)
            .line_to(20.0, 20.0)
            .line_to(0.0, 20.0)
            .build();
        r.draw_path(&path);
        let pm = r.into_pixmap();
        assert_eq!(pixel_rgba(&pm, 10, 10), (0, 255, 0, 255));
    }

    #[test]
    fn with_clip_excludes_outside() {
        let mut r = PixmapRenderer::new(1.0, 20.0, 20.0).expect("alloc");
        let clip = ClipPath::builder(FillRule::NonZero)
            .move_to(0.0, 0.0)
            .line_to(10.0, 0.0)
            .line_to(10.0, 10.0)
            .line_to(0.0, 10.0)
            .build();
        r.with_clip(&clip, |c| {
            let path = Path::builder(solid(0, 0, 255))
                .move_to(0.0, 0.0)
                .line_to(20.0, 0.0)
                .line_to(20.0, 20.0)
                .line_to(0.0, 20.0)
                .build();
            c.draw_path(&path);
        });
        let pm = r.into_pixmap();
        assert_eq!(pixel_rgba(&pm, 5, 5), (0, 0, 255, 255));
        assert_eq!(pixel_rgba(&pm, 15, 15).3, 0);
    }

    #[test]
    fn render_stream_matches_render_for_flat_path() {
        let mut scene = Scene::new(10.0, 10.0);
        rect_path(&mut scene, solid(255, 0, 0), 0.0, 0.0, 10.0, 10.0);
        let bytes = crate::wire::encode_frame(&scene);

        let mut r_atomic = PixmapRenderer::new(1.0, scene.width, scene.height).expect("alloc");
        let pm_atomic = r_atomic.render(&scene).expect("render");

        // A different size, so render_stream has to resize.
        let mut r_stream = PixmapRenderer::new(1.0, 1.0, 1.0).expect("alloc");
        let pm_streamed = r_stream.render_stream(&bytes[..]).expect("decode + render");

        for y in 0..10 {
            for x in 0..10 {
                assert_eq!(
                    pixel_rgba(pm_atomic, x, y),
                    pixel_rgba(pm_streamed, x, y),
                    "mismatch at ({x}, {y})"
                );
            }
        }
        assert_eq!(pixel_rgba(pm_streamed, 5, 5), (255, 0, 0, 255));
    }

    #[test]
    fn a_recycled_mask_does_not_leak_the_previous_clip() {
        // Sibling clips reuse one mask buffer.
        let mut r = PixmapRenderer::new(1.0, 20.0, 20.0).expect("alloc");
        let cover = |c: &mut PixmapRenderer| {
            let path = Path::builder(solid(0, 0, 255))
                .move_to(0.0, 0.0)
                .line_to(20.0, 0.0)
                .line_to(20.0, 20.0)
                .line_to(0.0, 20.0)
                .build();
            c.draw_path(&path);
        };
        let box_at = |x: f32| {
            ClipPath::builder(FillRule::NonZero)
                .move_to(x, 0.0)
                .line_to(x + 8.0, 0.0)
                .line_to(x + 8.0, 8.0)
                .line_to(x, 8.0)
                .build()
        };
        r.with_clip(&box_at(0.0), cover);
        r.with_clip(&box_at(10.0), cover);

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
            let mut p = scene.path(solid(0, 0, 255));
            p.move_to(0.0, 0.0);
            p.line_to(20.0, 0.0);
            p.line_to(20.0, 8.0);
            p.cubic_to(14.0, 10.0, 6.0, 10.0, 0.0, 8.0);
        }
        {
            let mut p = scene.path(solid(255, 0, 0));
            p.move_to(0.0, 12.0);
            p.line_to(20.0, 20.0);
        }
        let bytes = crate::wire::encode_frame(&scene);

        let mut direct = PixmapRenderer::new(1.0, scene.width, scene.height).expect("alloc");
        let expected = direct.render(&scene).expect("render").clone();

        let mut streamed = PixmapRenderer::new(1.0, scene.width, scene.height).expect("alloc");
        let got = streamed.render_stream(&bytes[..]).expect("decode + render");

        for y in 0..20 {
            for x in 0..20 {
                assert_eq!(
                    pixel_rgba(&expected, x, y),
                    pixel_rgba(got, x, y),
                    "mismatch at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn render_stream_handles_nested_clip() {
        let mut scene = Scene::new(20.0, 20.0);
        {
            let clip = ClipPath::builder(FillRule::NonZero)
                .move_to(0.0, 0.0)
                .line_to(10.0, 0.0)
                .line_to(10.0, 10.0)
                .line_to(0.0, 10.0)
                .build();
            let mut clip_scope = scene.clip(clip);
            rect_path(&mut clip_scope, solid(0, 0, 255), 0.0, 0.0, 20.0, 20.0);
        }
        let bytes = crate::wire::encode_frame(&scene);

        let mut r = PixmapRenderer::new(1.0, 1.0, 1.0).expect("alloc");
        let pm = r.render_stream(&bytes[..]).expect("decode + render");
        assert_eq!(pixel_rgba(pm, 5, 5), (0, 0, 255, 255));
        assert_eq!(pixel_rgba(pm, 15, 15).3, 0);
    }

    #[test]
    fn render_stream_rejects_non_frame_message() {
        let bytes = crate::wire::encode_close();
        let mut r = PixmapRenderer::new(1.0, 1.0, 1.0).expect("alloc");
        let err = r.render_stream(&bytes[..]).expect_err("not a frame");
        assert!(matches!(err, crate::wire::Error::WrongMessageKind));
    }
}
