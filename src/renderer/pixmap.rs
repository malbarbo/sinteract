//! Rasterize a [`crate::scene::Scene`] into a tiny-skia [`Pixmap`]. It needs
//! no tty or window, so it builds on wasm. The terminal and the window
//! present the pixels.

use tiny_skia::{
    Color as SkColor, FillRule as SkFillRule, GradientStop as SkStop, LineCap as SkLineCap,
    LineJoin as SkLineJoin, Mask, Paint as SkPaint, PathBuilder, Pixmap, Point as SkPoint,
    Shader as SkShader, SpreadMode as SkSpread, Stroke, StrokeDash, Transform,
};

use crate::renderer::{Renderer, RestoreOnDrop, outline_segments, sealed::Canvas};
use crate::scene::{ClipPath, FillRule, GradientGeom, LineCap, LineJoin, Paint, Path, Rgba, Text};
use crate::text::OutlineBuilder;

/// Rasterize a [`crate::scene::Scene`] at `scale`, where 1.0 is the frame's
/// own pixels. See [`fit_scale`].
pub fn render_to_pixmap(scene: &crate::scene::Scene, scale: f32) -> Option<Pixmap> {
    let mut renderer = PixmapRenderer::new(scale, scene.width, scene.height)?;
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
    /// The scale as a transform, applied to every path. The caller sets the
    /// scale once, and decides with it whether a frame may grow.
    base: Transform,
    /// The mask of each clip in effect. `None` is a clip that hides what it
    /// holds, because its path is empty, a clip around it hides what it
    /// holds, or its mask could not be allocated.
    clip_stack: Vec<Option<Mask>>,
    /// Masks popped off `clip_stack`, for the next push. Every mask is
    /// canvas-sized, so any one fits.
    mask_pool: Vec<Mask>,
    /// The builder of the next path. A finished path clears back into it, so
    /// the next path reuses its capacity.
    builder: PathBuilder,
}

impl PixmapRenderer {
    /// A surface for frames at `scale`, where 1.0 is the frame's own pixels,
    /// sized first for a `width × height` frame. `None` when the surface
    /// cannot be allocated.
    pub fn new(scale: f32, width: f32, height: f32) -> Option<Self> {
        // A zero or negative scale would allocate nothing to draw into.
        let scale = scale.max(1e-3);
        let (out_w, out_h) = out_size(width, height, scale);
        Some(Self {
            pixmap: Pixmap::new(out_w, out_h)?,
            base: Transform::from_scale(scale, scale),
            clip_stack: Vec::new(),
            mask_pool: Vec::new(),
            builder: PathBuilder::new(),
        })
    }

    /// The pixmap of the last render.
    pub fn into_pixmap(self) -> Pixmap {
        self.pixmap
    }
}

impl Canvas for PixmapRenderer {
    /// Clears the surface, and reallocates it when the scaled size changed.
    fn ensure_size(&mut self, width: f32, height: f32) -> Result<(), crate::renderer::AllocError> {
        let (out_w, out_h) = out_size(width, height, self.base.sx);
        // A frame ends with an empty clip stack, and its masks serve the next
        // frame.
        self.mask_pool.extend(self.clip_stack.drain(..).flatten());
        if (out_w, out_h) == (self.pixmap.width(), self.pixmap.height()) {
            self.pixmap.fill(tiny_skia::Color::TRANSPARENT);
        } else {
            // Masks are canvas-sized, so a resize invalidates every pooled one.
            self.mask_pool.clear();
            // A new pixmap is transparent, so the background of the backend
            // shows through.
            self.pixmap = Pixmap::new(out_w, out_h).ok_or(crate::renderer::AllocError {
                width: out_w,
                height: out_h,
            })?;
        }
        Ok(())
    }

    fn draw_path(&mut self, path: &Path) {
        let Some(mask) = mask_in_effect(&self.clip_stack) else {
            return;
        };
        let style = &path.style;
        let mut builder = std::mem::take(&mut self.builder);
        outline_segments(path.segments(), &mut builder);
        if style.closed {
            builder.close();
        }
        let Some(sk_path) = builder.finish() else {
            return;
        };
        if style.draws_fill() {
            let paint = sk_paint(paint_to_shader(&style.fill));
            self.pixmap.fill_path(
                &sk_path,
                &paint,
                sk_fill_rule(style.fill_rule),
                self.base,
                mask,
            );
        }
        if style.draws_stroke() {
            let paint = sk_paint(paint_to_shader(&style.stroke));
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
        self.builder = sk_path.clear();
    }

    fn draw_text(&mut self, node: &Text) {
        if let Some(mask) = mask_in_effect(&self.clip_stack) {
            render_text(node, &mut self.pixmap, mask, self.base, &mut self.builder);
        }
    }

    fn with_clip<T>(&mut self, clip: &ClipPath, inside: impl FnOnce(&mut Self) -> T) -> T {
        let mask = self.clip_mask(clip);
        self.clip_stack.push(mask);
        let guard = RestoreOnDrop {
            canvas: self,
            restore: |c: &mut Self| {
                if let Some(Some(mask)) = c.clip_stack.pop() {
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
    /// The coverage of `clip` inside the clip in effect, or `None` when the
    /// clip hides what it holds. A clip that gets no mask hides what it
    /// holds, so nothing paints outside it.
    fn clip_mask(&mut self, clip: &ClipPath) -> Option<Mask> {
        if matches!(self.clip_stack.last(), Some(None)) {
            return None;
        }
        let mut builder = std::mem::take(&mut self.builder);
        outline_segments(clip.segments(), &mut builder);
        // A sub-path of a clip is closed, as in an SVG clipPath. tiny_skia
        // accepts a close on a closed contour.
        builder.close();
        // An empty path covers nothing.
        let path = builder.finish()?;
        let mask = self.take_mask().map(|mut mask| {
            mask.fill_path(&path, sk_fill_rule(clip.fill_rule), true, self.base);
            if let Some(Some(parent)) = self.clip_stack.last() {
                // Mask::intersect_path would rasterize into a second
                // canvas-sized mask.
                for (a, b) in mask.data_mut().iter_mut().zip(parent.data()) {
                    *a = mask_mul(*a, *b);
                }
            }
            mask
        });
        self.builder = path.clear();
        mask
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

/// The mask to paint through, `Some(None)` outside any clip, or `None`
/// inside a clip that hides what it holds.
fn mask_in_effect(clip_stack: &[Option<Mask>]) -> Option<Option<&Mask>> {
    match clip_stack.last() {
        None => Some(None),
        Some(mask) => mask.as_ref().map(Some),
    }
}

/// `a * b / 255`, rounded as tiny-skia does, so the result matches
/// `Mask::intersect_path`.
fn mask_mul(a: u8, b: u8) -> u8 {
    let prod = u32::from(a) * u32::from(b) + 128;
    ((prod + (prod >> 8)) >> 8) as u8
}

/// A frame's size in whole output pixels.
fn frame_px(width: f32, height: f32) -> (u32, u32) {
    (width.ceil().max(1.0) as u32, height.ceil().max(1.0) as u32)
}

/// The size in output pixels of a frame at `scale`.
fn out_size(width: f32, height: f32, scale: f32) -> (u32, u32) {
    let (w, h) = frame_px(width, height);
    let out_w = ((w as f32) * scale).ceil().max(1.0) as u32;
    let out_h = ((h as f32) * scale).ceil().max(1.0) as u32;
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

/// A gradient that tiny-skia rejects, for a degenerate line or no stops,
/// falls back to the primary color, so the path still draws.
fn paint_to_shader(p: &Paint) -> SkShader<'static> {
    let g = match p {
        Paint::Solid(c) => return SkShader::SolidColor(sk_color(*c)),
        Paint::Gradient(g) => g,
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

// -----------------------------------------------------------------------------
// Text
// -----------------------------------------------------------------------------

fn render_text(
    node: &Text,
    pixmap: &mut Pixmap,
    mask: Option<&Mask>,
    base: Transform,
    builder: &mut PathBuilder,
) {
    let Some(layout) = crate::text::TextLayout::new(&node.spec) else {
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
    if node.draws_fill() {
        let paint = sk_paint(SkShader::SolidColor(sk_color(node.fill)));
        // A TrueType glyph fills with non-zero winding.
        pixmap.fill_path(&path, &paint, SkFillRule::Winding, transform, mask);
    }
    if node.draws_stroke() {
        let paint = sk_paint(SkShader::SolidColor(sk_color(node.stroke)));
        let stroke = Stroke {
            width: node.stroke_width,
            miter_limit: crate::renderer::TEXT_MITER_LIMIT,
            dash: None,
            ..Stroke::default()
        };
        pixmap.stroke_path(&path, &paint, &stroke, transform, mask);
    }
    *builder = path.clear();
}

/// The inherent methods of [`PathBuilder`], so paths, clips and glyphs build
/// through [`outline_segments`] and [`crate::text::TextLayout::outline`].
impl OutlineBuilder for PathBuilder {
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
    use crate::scene::{PathStyle, Scene, TextSpec};

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
        rect(&mut scene, solid(255, 0, 0), 0.0, 0.0, 10.0, 10.0);
        let bytes = crate::wire::encode_frame(&scene);

        let mut r_atomic = PixmapRenderer::new(1.0, scene.width, scene.height).expect("alloc");
        let pm_atomic = r_atomic.render(&scene).expect("render");

        // A different size, so render_stream has to resize.
        let mut r_stream = PixmapRenderer::new(1.0, 1.0, 1.0).expect("alloc");
        let pm_streamed = r_stream.render_stream(&bytes[..]).expect("decode + render");

        assert_same_pixels(pm_atomic, pm_streamed);
        assert_eq!(pixel_rgba(pm_streamed, 5, 5), (255, 0, 0, 255));
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
        let bytes = crate::wire::encode_frame(&scene);

        let mut direct = PixmapRenderer::new(1.0, scene.width, scene.height).expect("alloc");
        let expected = direct.render(&scene).expect("render").clone();

        let mut streamed = PixmapRenderer::new(1.0, scene.width, scene.height).expect("alloc");
        let got = streamed.render_stream(&bytes[..]).expect("decode + render");

        assert_same_pixels(&expected, got);
    }

    #[test]
    fn render_stream_handles_nested_clip() {
        let mut scene = Scene::new(20.0, 20.0);
        {
            let mut clip_scope = scene.clip(square_clip(0.0, 0.0, 10.0));
            rect(&mut clip_scope, solid(0, 0, 255), 0.0, 0.0, 20.0, 20.0);
        }
        let bytes = crate::wire::encode_frame(&scene);

        let mut r = PixmapRenderer::new(1.0, 1.0, 1.0).expect("alloc");
        let pm = r.render_stream(&bytes[..]).expect("decode + render");
        assert_eq!(pixel_rgba(pm, 5, 5), (0, 0, 255, 255));
        assert_eq!(pixel_rgba(pm, 15, 15).3, 0);
    }

    #[test]
    fn render_stream_skips_an_element_of_an_unknown_arm() {
        // A red path that becomes an arm of a newer schema, under a blue one.
        let mut scene = Scene::new(10.0, 10.0);
        rect(&mut scene, solid(255, 0, 0), 0.0, 0.0, 10.0, 10.0);
        rect(&mut scene, solid(0, 0, 255), 0.0, 0.0, 5.0, 5.0);
        let bytes = crate::wire::with_unknown_value(&crate::wire::encode_frame(&scene), |m| {
            crate::wire::tag_of(crate::wire::frame_of(m).get_elements().unwrap().get(0))
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
        use crate::wire::{encode_frame, frame_of, tag_of, with_unknown_value};
        let mut scene = Scene::new(10.0, 10.0);
        rect(&mut scene, solid(255, 0, 0), 0.0, 0.0, 10.0, 10.0);
        {
            let mut clip_scope = scene.clip(square_clip(0.0, 0.0, 10.0));
            rect(&mut clip_scope, solid(255, 0, 0), 0.0, 0.0, 10.0, 10.0);
        }
        rect(&mut scene, solid(0, 0, 255), 0.0, 0.0, 5.0, 5.0);
        let bytes = with_unknown_value(&encode_frame(&scene), |m| {
            let Ok(Which::Path(p)) = frame_of(m).get_elements().unwrap().get(0).which() else {
                panic!("expected Path");
            };
            tag_of(p.unwrap().get_style().unwrap().get_fill().unwrap())
        });
        let bytes = with_unknown_value(&bytes, |m| {
            let Ok(Which::Clipped(c)) = frame_of(m).get_elements().unwrap().get(1).which() else {
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
    fn render_stream_rejects_non_frame_message() {
        let bytes = crate::wire::encode_close();
        let mut r = PixmapRenderer::new(1.0, 1.0, 1.0).expect("alloc");
        let err = r.render_stream(&bytes[..]).expect_err("not a frame");
        assert!(matches!(err, crate::wire::StreamError::WrongMessageKind));
    }

    #[test]
    fn render_stream_rejects_a_message_of_an_unknown_arm() {
        let bytes = crate::wire::with_unknown_value(&crate::wire::encode_close(), |m| {
            crate::wire::tag_of(m)
        });
        let mut r = PixmapRenderer::new(1.0, 1.0, 1.0).expect("alloc");
        let err = r.render_stream(&bytes[..]).expect_err("not a frame");
        assert!(matches!(err, crate::wire::StreamError::WrongMessageKind));
    }

    #[test]
    fn an_empty_clip_hides_what_it_holds() {
        // A clip inside it hides what it holds too.
        let mut scene = Scene::new(20.0, 20.0);
        {
            let mut empty = scene.clip(ClipPath::default());
            rect(&mut empty, solid(0, 255, 0), 0.0, 0.0, 20.0, 20.0);
            let mut inner = empty.clip(square_clip(0.0, 0.0, 20.0));
            rect(&mut inner, solid(0, 0, 255), 0.0, 0.0, 20.0, 20.0);
        }
        let pm = render_to_pixmap(&scene, 1.0).expect("pixmap");
        assert_eq!(pixel_rgba(&pm, 10, 10).3, 0);
    }
}
