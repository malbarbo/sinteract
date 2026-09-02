//! Reusable tiny-skia raster surface: renders a [`crate::scene::Scene`] (or a
//! wire stream) into an owned [`Pixmap`]. Pure computation — no tty or window —
//! so it builds on wasm too; the terminal and window backends drive it and then
//! present the pixels their own way.

use std::io;

use tiny_skia::{
    Color as SkColor, FillRule as SkFillRule, GradientStop as SkStop, LineCap as SkLineCap,
    LineJoin as SkLineJoin, Mask, Paint as SkPaint, PathBuilder, Pixmap, Point as SkPoint,
    Shader as SkShader, SpreadMode as SkSpread, Stroke, StrokeDash, Transform,
};

use crate::renderer::{Renderer, sealed::Paint};
use crate::scene::{
    ClipPath, FillRule, GradientGeom, LineCap, LineJoin, Paint as IrPaint, Path, Rgba, Scene,
    Segment, Segments, TextNode,
};

/// A reusable raster surface. Owns its [`Pixmap`]; [`Renderer::render`] clears
/// and redraws into it, reallocating only when the frame size changes, so a
/// redraw loop reuses one allocation.
struct PixmapRenderer {
    /// Output pixmap, sized in [`Self::ensure_size`]. Never absent while the
    /// renderer is live — allocation failure is surfaced as an error there,
    /// so no draw op has to reason about a missing surface.
    pixmap: Pixmap,
    /// `input → output` scale folded into a transform applied to every path.
    base: Transform,
    clip_stack: Vec<Mask>,
    out_w: u32,
    out_h: u32,
    /// Target output box in pixels (`None` = render at native size).
    target_px: Option<(u32, u32)>,
    /// Upper bound on the rasterizer's uniform scale factor — depends on
    /// the active backend (1.0 for Kitty/Sixel, smaller for half-blocks).
    max_scale: f32,
}

/// Scaled output dimensions and the input→output transform for a `width ×
/// height` frame under `target`/`max_scale`.
fn fit(
    width: f32,
    height: f32,
    target: Option<(u32, u32)>,
    max_scale: f32,
) -> (u32, u32, Transform) {
    let w = width.ceil().max(1.0) as u32;
    let h = height.ceil().max(1.0) as u32;
    let s = compute_scale(w, h, target, max_scale);
    let out_w = ((w as f32) * s).ceil().max(1.0) as u32;
    let out_h = ((h as f32) * s).ceil().max(1.0) as u32;
    (out_w, out_h, Transform::from_scale(s, s))
}

/// Allocate an `out_w × out_h` pixmap with a transparent background (so the
/// backend's own background shows through). `None` on allocation failure.
fn new_pixmap(out_w: u32, out_h: u32) -> Option<Pixmap> {
    Pixmap::new(out_w, out_h).map(|mut pm| {
        pm.fill(tiny_skia::Color::TRANSPARENT);
        pm
    })
}

/// Append `segments` to a tiny-skia path builder. Returns whether any segment
/// was emitted (a lone move counts). Shared by path fill/stroke and clip-mask
/// construction so the segment→builder mapping lives in one place.
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
    /// Allocate a renderer whose surface fits `width × height` (scaled per
    /// `target_px`/`max_scale`). `None` if the surface cannot be allocated.
    fn new(target_px: Option<(u32, u32)>, max_scale: f32, width: f32, height: f32) -> Option<Self> {
        let (out_w, out_h, base) = fit(width, height, target_px, max_scale);
        Some(Self {
            pixmap: new_pixmap(out_w, out_h)?,
            base,
            clip_stack: Vec::new(),
            out_w,
            out_h,
            target_px,
            max_scale,
        })
    }

    /// Prepare the surface for a `width × height` frame: reallocate if the
    /// scaled size changed, otherwise clear the existing buffer in place —
    /// reusing the allocation across same-size frames. The clip stack is
    /// always reset.
    fn ensure_size(&mut self, width: f32, height: f32) -> Result<(), crate::wire::Error> {
        let (out_w, out_h, base) = fit(width, height, self.target_px, self.max_scale);
        self.base = base;
        self.clip_stack.clear();
        if (out_w, out_h) == (self.out_w, self.out_h) {
            self.pixmap.fill(tiny_skia::Color::TRANSPARENT);
        } else {
            self.pixmap = new_pixmap(out_w, out_h).ok_or(crate::wire::Error::Alloc {
                width: out_w,
                height: out_h,
            })?;
            self.out_w = out_w;
            self.out_h = out_h;
        }
        Ok(())
    }

    /// Consume the renderer and hand back the owned pixmap (for one-shot
    /// callers that want to move the result out rather than borrow it).
    fn into_pixmap(self) -> Pixmap {
        self.pixmap
    }

    /// Build a clip mask from `clip`, intersect it with the current one, and
    /// push it. Returns whether a mask was actually pushed — an empty or
    /// unbuildable clip pushes nothing, and [`Paint::with_clip`]'s guard pops
    /// only what was pushed (so the stack stays balanced regardless).
    fn push_clip(&mut self, clip: &ClipPath) -> bool {
        let parent = self.clip_stack.last();
        let mut builder = PathBuilder::new();
        append_segments(&mut builder, clip.segments());
        // SVG `<clipPath>` semantics: sub-paths are filled, so close before
        // intersecting. tiny_skia tolerates an explicit close on an already-
        // closed contour.
        builder.close();
        let Some(path) = builder.finish() else {
            return false;
        };
        let Some(mut mask) = (match parent {
            Some(m) => Some(m.clone()),
            None => Mask::new(self.out_w, self.out_h).map(|mut m| {
                if let Some(rect) =
                    tiny_skia::Rect::from_xywh(0.0, 0.0, self.out_w as f32, self.out_h as f32)
                {
                    m.fill_path(
                        &PathBuilder::from_rect(rect),
                        SkFillRule::Winding,
                        true,
                        Transform::identity(),
                    );
                }
                m
            }),
        }) else {
            return false;
        };
        mask.intersect_path(&path, sk_fill_rule(clip.fill_rule), true, self.base);
        self.clip_stack.push(mask);
        true
    }
}

/// Pops the clip pushed by [`Paint::with_clip`] on scope exit — including on
/// unwind — so the clip stack cannot leak if the `inside` body panics.
struct ClipGuard<'a> {
    canvas: &'a mut PixmapRenderer,
    pushed: bool,
}

impl Drop for ClipGuard<'_> {
    fn drop(&mut self) {
        if self.pushed {
            self.canvas.clip_stack.pop();
        }
    }
}

impl Paint for PixmapRenderer {
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

    fn render(&mut self, scene: &Scene) -> Result<&Pixmap, crate::wire::Error> {
        self.ensure_size(scene.width, scene.height)?;
        self.paint_elements(&scene.elements);
        Ok(&self.pixmap)
    }

    fn render_stream(&mut self, reader: impl io::Read) -> Result<&Pixmap, crate::wire::Error> {
        crate::wire::stream_frame(self, reader, |s, w, h| s.ensure_size(w, h))?;
        Ok(&self.pixmap)
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

/// Convert an IR [`IrPaint`] to a tiny-skia [`SkShader`]. Gradients that fail
/// to construct (e.g. degenerate line, missing stops) collapse to the paint's
/// primary color so the draw still produces output.
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

/// Rasterize a [`crate::scene::Scene`], optionally fitting the output to
/// `target_px` (in pixels). When a target is given, the output is uniformly
/// scaled so it fits inside the target box while preserving aspect ratio.
/// Scaling is **shrink-only**: an image smaller than the target stays at its
/// native dimensions (the user picked those numbers; respect them).
pub(crate) fn rasterize_scene(
    scene: &crate::scene::Scene,
    target_px: Option<(u32, u32)>,
    max_scale: f32,
) -> Option<Pixmap> {
    let mut renderer = PixmapRenderer::new(target_px, max_scale, scene.width, scene.height)?;
    renderer.render(scene).ok()?;
    Some(renderer.into_pixmap())
}

/// Uniform scale factor to fit `(w, h)` inside `target` (both in pixels),
/// capped at `max_scale` so we never upscale beyond the per-backend limit.
fn compute_scale(w: u32, h: u32, target: Option<(u32, u32)>, max_scale: f32) -> f32 {
    match target {
        Some((tw, th)) if w > 0 && h > 0 && tw > 0 && th > 0 => {
            let sw = tw as f32 / w as f32;
            let sh = th as f32 / h as f32;
            sw.min(sh).clamp(1e-3, max_scale)
        }
        _ => max_scale,
    }
}

// -----------------------------------------------------------------------------
// Text rendering (T tag → text node)
// -----------------------------------------------------------------------------

fn render_text(node: &TextNode, pixmap: &mut Pixmap, mask: Option<&Mask>, base: Transform) {
    let Some(layout) = crate::text::layout_text(node) else {
        return;
    };

    let mut builder = PathBuilder::new();
    let mut adapter = SkiaOutline { b: &mut builder };
    crate::text::outline_with(layout.face, &node.text, layout.size_i, &mut adapter);

    if node.underline {
        crate::text::outline_underline(&layout, &mut adapter);
    }

    let Some(path) = builder.finish() else {
        return;
    };

    // `node.transform` follows the PDF `cm` / SVG `matrix(...)` convention,
    // which is exactly tiny_skia's `Transform::from_row` row order.
    let local = Transform::from_row(
        node.transform[0],
        node.transform[1],
        node.transform[2],
        node.transform[3],
        node.transform[4],
        node.transform[5],
    );
    // `post_concat(base)` => final = base * local: apply the local text
    // transform first, then the global pixmap-scale.
    let transform = local.post_concat(base);

    if node.fill.a > 0.0 {
        let mut paint = SkPaint::default();
        paint.set_color(sk_color(node.fill));
        paint.anti_alias = true;
        // Text glyphs are TrueType; non-zero winding is the standard fill rule.
        pixmap.fill_path(&path, &paint, SkFillRule::Winding, transform, mask);
    }
    if node.stroke.a > 0.0 && node.stroke_width > 0.0 {
        let mut paint = SkPaint::default();
        paint.set_color(sk_color(node.stroke));
        paint.anti_alias = true;
        // Text outlines are closed contours on smooth curves — cap/join
        // tweaks are imperceptible, so we don't carry them through the
        // wire. tiny_skia's defaults (butt cap, miter join) are fine.
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
    use crate::scene::PathStyle;

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
    fn draw_path_paints_rectangle() {
        // Drive the paint primitives directly — no Scene materialization.
        // `into_pixmap` moves the owned buffer out.
        let mut r = PixmapRenderer::new(None, 1.0, 20.0, 20.0).expect("alloc");
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
        // The `inside` body runs with the clip active; the clip pops when the
        // body returns.
        let mut r = PixmapRenderer::new(None, 1.0, 20.0, 20.0).expect("alloc");
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
        // Inside the clip box: blue. Outside (e.g. 15,15): transparent.
        assert_eq!(pixel_rgba(&pm, 5, 5), (0, 0, 255, 255));
        assert_eq!(pixel_rgba(&pm, 15, 15).3, 0);
    }

    #[test]
    fn render_stream_matches_render_for_flat_path() {
        // Build a scene with a single red square, encode it as a capnp Frame,
        // feed the bytes to render_stream — pixel result must match the
        // in-memory render path.
        let mut scene = Scene::new(10.0, 10.0);
        rect_path(&mut scene, solid(255, 0, 0), 0.0, 0.0, 10.0, 10.0);
        let bytes = crate::wire::encode_frame(&scene);

        let mut r_atomic =
            PixmapRenderer::new(None, 1.0, scene.width, scene.height).expect("alloc");
        let pm_atomic = r_atomic.render(&scene).expect("render");

        // Construct at a different size to exercise the resize-on-render path.
        let mut r_stream = PixmapRenderer::new(None, 1.0, 1.0, 1.0).expect("alloc");
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
    fn render_stream_handles_nested_clip() {
        // Capnp Frame with a Clipped subtree: with_clip runs the nested walk
        // with the clip active, then pops it.
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

        let mut r = PixmapRenderer::new(None, 1.0, 1.0, 1.0).expect("alloc");
        let pm = r.render_stream(&bytes[..]).expect("decode + render");
        assert_eq!(pixel_rgba(pm, 5, 5), (0, 0, 255, 255));
        assert_eq!(pixel_rgba(pm, 15, 15).3, 0);
    }

    #[test]
    fn render_stream_rejects_non_frame_message() {
        let bytes = crate::wire::encode_close();
        let mut r = PixmapRenderer::new(None, 1.0, 1.0, 1.0).expect("alloc");
        let err = r.render_stream(&bytes[..]).expect_err("not a frame");
        assert!(matches!(err, crate::wire::Error::WrongMessageKind));
    }
}
