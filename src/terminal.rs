//! Terminal renderer for `World` and inline images using the Kitty graphics
//! protocol.
//!
//! On Kitty-compatible terminals (Kitty, Ghostty, WezTerm, modern Konsole),
//! `show_image` rasterizes a [`crate::scene::Scene`] directly with
//! `tiny-skia` and transmits it as an RGBA payload. Animations (`World.run`)
//! drive into alt-screen + raw mode via `enter_animation` / `exit_animation`,
//! and each frame replaces the previous image at (0, 0). Keyboard events are
//! polled non-blocking via `crossterm`.
//!
//! On terminals without Kitty graphics support, `show_svg` is invoked instead
//! and prints the SVG source (preserving the prior native behavior).
//!
//! Limitations:
//! - Terminals do not distinguish keydown from keyup, so all key events are
//!   reported as KEYPRESS (event_type = 0). `on_key_down` / `on_key_up`
//!   handlers therefore behave like `on_key_press`.
//! - Text resolves through [`crate::text::resolve`] — Liberation Sans /
//!   Serif / Mono are embedded in all four variants; unknown families fall
//!   back to a system font (via fontdb) or to Liberation Sans.
//! - Bitmap nodes are not rendered (the renderer logs a warning and skips
//!   them); SVG output remains the canonical form.

use std::io::{self, Write};
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{cursor, event, execute, queue, terminal};
use tiny_skia::{
    Color as SkColor, FillRule as SkFillRule, GradientStop as SkStop, LineCap as SkLineCap,
    LineJoin as SkLineJoin, Mask, Paint as SkPaint, PathBuilder, Pixmap, Point as SkPoint,
    Shader as SkShader, SpreadMode as SkSpread, Stroke, StrokeDash, Transform,
};

use crate::renderer::{Renderer, sealed::Paint};
use crate::scene::{
    Bitmap, ClipPath, FillRule, LineCap, LineJoin, Paint as IrPaint, Path, Rgba, Scene, Segment,
    TextNode,
};
use crate::sixel;

const KITTY_ANIMATION_ID: u32 = 1042;
const KITTY_ONESHOT_ID_BASE: u32 = 2000;

pub(crate) const KEYPRESS: i32 = 0;
pub(crate) const KEYDOWN: i32 = 1;
pub(crate) const KEYUP: i32 = 2;

// Fallback cell size when the terminal didn't reply to the `CSI 16 t` probe.
// Real cells vary (most are 8×16 to 10×20 depending on font); we ask the
// terminal for the actual pixel size in `cell_pixels()` and fall back to
// these defaults only when the query returns nothing.
const CELL_W_DEFAULT: u32 = 8;
const CELL_H_DEFAULT: u32 = 16;

/// Pixel size of one terminal cell — queried via `CSI 16 t` and cached by
/// [`crate::term_query`]. Falls back to `(8, 16)` when the terminal didn't
/// reply (multiplexers, ancient terminals, non-tty stdout).
fn cell_pixels() -> (u32, u32) {
    crate::term_query::graphics_caps()
        .cell_px
        .unwrap_or((CELL_W_DEFAULT, CELL_H_DEFAULT))
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum Backend {
    Kitty,
    Sixel,
    TextBlocks,
}

struct State {
    in_animation: bool,
    raw_enabled: bool,
    image_displayed: bool,
    next_oneshot_id: u32,
    warned_bitmap: bool,
    text_blocks_lines: u16,
}

static STATE: Mutex<State> = Mutex::new(State {
    in_animation: false,
    raw_enabled: false,
    image_displayed: false,
    next_oneshot_id: KITTY_ONESHOT_ID_BASE,
    warned_bitmap: false,
    text_blocks_lines: 0,
});

/// Best-effort detection of terminals that report 24-bit truecolor support
/// — the prerequisite for the half-blocks (`▀`) ANSI fallback used when
/// neither Kitty nor Sixel is available.
pub fn text_blocks_supported() -> bool {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return false;
    }
    if let Ok(ct) = std::env::var("COLORTERM") {
        let lc = ct.to_ascii_lowercase();
        if lc == "truecolor" || lc == "24bit" {
            return true;
        }
    }
    if let Ok(t) = std::env::var("TERM")
        && (t.ends_with("-direct") || t == "xterm-direct")
    {
        return true;
    }
    // Common modern terminals advertise truecolor implicitly. These envs are
    // strong signals; conservative-but-useful for v1.
    if std::env::var_os("KITTY_WINDOW_ID").is_some() {
        return true;
    }
    if std::env::var_os("WT_SESSION").is_some() {
        return true;
    }
    if let Ok(prog) = std::env::var("TERM_PROGRAM") {
        let lc = prog.to_ascii_lowercase();
        if matches!(
            lc.as_str(),
            "ghostty" | "wezterm" | "konsole" | "vscode" | "iterm.app" | "apple_terminal"
        ) {
            return true;
        }
    }
    false
}

/// Whether the terminal speaks the Kitty graphics protocol. Asks the
/// terminal directly via a synchronous capability query (see `term_query`)
/// — env-based heuristics lie (SSH strips them, multiplexers don't, custom
/// shells override them). The probe is cached, so the I/O cost is paid at
/// most once per process.
pub fn kitty_supported() -> bool {
    crate::term_query::graphics_caps().kitty
}

// -----------------------------------------------------------------------------
// PixmapRenderer — reusable tiny-skia raster surface
// -----------------------------------------------------------------------------

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

/// Allocate an `out_w × out_h` pixmap with a transparent background (the
/// terminal background shows through). `None` on allocation failure.
fn new_pixmap(out_w: u32, out_h: u32) -> Option<Pixmap> {
    Pixmap::new(out_w, out_h).map(|mut pm| {
        pm.fill(tiny_skia::Color::TRANSPARENT);
        pm
    })
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
        for seg in clip.segments() {
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
        }
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
        let mut has_points = false;
        for seg in path.segments() {
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
            has_points = true;
        }
        if !has_points {
            return;
        }
        if style.closed {
            builder.close();
        }
        let Some(sk_path) = builder.finish() else {
            return;
        };
        let mask = self.clip_stack.last();

        if style.fill.is_visible() {
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
        if style.stroke.is_visible() && style.stroke_width > 0.0 {
            let paint = SkPaint {
                shader: paint_to_shader(&style.stroke),
                anti_alias: true,
                ..SkPaint::default()
            };
            let dash = if style.dash_array.is_empty() {
                None
            } else {
                StrokeDash::new(style.dash_array.clone(), style.dash_offset)
            };
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

    fn draw_bitmap(&mut self, _node: &Bitmap) {
        let mut s = STATE.lock().unwrap();
        if !s.warned_bitmap {
            eprintln!(
                "[spython] terminal renderer does not yet support bitmaps; \
                 this image is being shown without bitmaps."
            );
            s.warned_bitmap = true;
        }
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
    match p {
        IrPaint::Solid(c) => SkShader::SolidColor(sk_color(*c)),
        IrPaint::Linear(g) => {
            let stops = sk_stops(&g.stops);
            tiny_skia::LinearGradient::new(
                SkPoint::from_xy(g.x0, g.y0),
                SkPoint::from_xy(g.x1, g.y1),
                stops,
                sk_spread(g.spread),
                Transform::identity(),
            )
            .unwrap_or_else(|| SkShader::SolidColor(sk_color(p.primary_color())))
        }
        IrPaint::Radial(g) => {
            let center = SkPoint::from_xy(g.cx, g.cy);
            let stops = sk_stops(&g.stops);
            tiny_skia::RadialGradient::new(
                center,
                0.0,
                center,
                g.radius,
                stops,
                sk_spread(g.spread),
                Transform::identity(),
            )
            .unwrap_or_else(|| SkShader::SolidColor(sk_color(p.primary_color())))
        }
    }
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
    // Match Python: `int(f.size)` is used for measurement.
    let size_i = node.size as i32;
    if size_i <= 0 || node.text.is_empty() {
        return;
    }

    // Pick the actual variant requested. `node.family` falls back to
    // Liberation Sans when empty (default), so the historic test fixtures
    // keep rendering with the same face.
    let font = crate::text::resolve(&node.family, node.weight, node.style);
    let face = font.face();

    // Measure the rendered width — only used for the underline rect now;
    // the rest of the placement lives in `node.transform`. Vertical extent
    // is read from face metrics directly when the underline runs.
    let original_w = crate::text::measure_width_with(face, &node.text, size_i) as f32;
    if original_w <= 0.0 {
        return;
    }
    let baseline_y = crate::text::measure_y_offset_with(face, &node.text, size_i) as f32;

    let mut builder = PathBuilder::new();
    let mut adapter = SkiaOutline { b: &mut builder };
    crate::text::outline_with(face, &node.text, size_i, &mut adapter);

    if node.underline {
        // Use the face's own metrics so non-Sans fonts (Serif / Mono /
        // system) get a position that matches their design — hardcoded
        // Liberation Sans values would look misplaced under e.g. a Serif.
        let face_units = face.units_per_em() as f32;
        let scale = node.size / face_units;
        let metrics = face.underline_metrics();
        let pos_units = metrics.map(|m| m.position as f32).unwrap_or(-217.0);
        let thickness_units = metrics.map(|m| m.thickness as f32).unwrap_or(150.0);
        let underline_pos = -pos_units * scale; // font y-up → flips to +y in box-local
        let thickness = (thickness_units * scale).max(1.0);
        let y_top = baseline_y + underline_pos - thickness / 2.0;
        let y_bot = y_top + thickness;
        let x_l = crate::text::measure_x_offset_with(face, &node.text, size_i) as f32;
        let x_r = x_l + original_w;
        builder.move_to(x_l, y_top);
        builder.line_to(x_r, y_top);
        builder.line_to(x_r, y_bot);
        builder.line_to(x_l, y_bot);
        builder.close();
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

    let Rgba {
        r: fr,
        g: fg,
        b: fb,
        a: fa,
    } = node.fill;
    if fa > 0.0 {
        let mut paint = SkPaint::default();
        paint.set_color_rgba8(fr, fg, fb, (fa * 255.0).round().clamp(0.0, 255.0) as u8);
        paint.anti_alias = true;
        // Text glyphs are TrueType; non-zero winding is the standard fill rule.
        pixmap.fill_path(&path, &paint, SkFillRule::Winding, transform, mask);
    }
    let Rgba {
        r: sr,
        g: sg,
        b: sb,
        a: sa,
    } = node.stroke;
    if sa > 0.0 && node.stroke_width > 0.0 {
        let mut paint = SkPaint::default();
        paint.set_color_rgba8(sr, sg, sb, (sa * 255.0).round().clamp(0.0, 255.0) as u8);
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

// -----------------------------------------------------------------------------
// Terminal-aware sizing + half-blocks renderer
// -----------------------------------------------------------------------------

/// Target pixel box for the active backend, derived from `terminal::size()`.
/// `None` means "no constraint" (used when crossterm fails or stdout isn't a
/// terminal, e.g. in tests piped to a file).
fn target_pixels_for_backend(backend: Backend) -> Option<(u32, u32)> {
    let (cols, rows) = terminal::size().ok()?;
    if cols == 0 || rows == 0 {
        return None;
    }
    // Reserve one row so the prompt that follows the image (or the prompt
    // sitting above an alt-screen animation) doesn't push the last row off.
    let rows_avail = rows.saturating_sub(1).max(1);
    let (cw, ch) = cell_pixels();
    Some(match backend {
        Backend::Kitty | Backend::Sixel => (cols as u32 * cw, rows_avail as u32 * ch),
        // Half-blocks pack two image-pixel rows into one cell row, and one
        // image-pixel column into one cell column.
        Backend::TextBlocks => (cols as u32, rows_avail as u32 * 2),
    })
}

/// Per-backend upper bound on the rasterizer's uniform scale factor.
///
/// For Kitty/Sixel each pixmap pixel is one screen pixel, so capping at
/// `1.0` keeps the image at native size or smaller. For half-blocks each
/// pixmap pixel covers `cell_w × cell_h/2` screen pixels — uncapped, a 100×100
/// logical image would stretch to ~`100·cell_w` screen pixels wide. Cap at
/// `1 / max(cell_w, cell_h/2)` so a logical pixel never expands past one
/// screen pixel.
fn max_scale_for_backend(backend: Backend) -> f32 {
    match backend {
        Backend::Kitty | Backend::Sixel => 1.0,
        Backend::TextBlocks => {
            let (cw, ch) = cell_pixels();
            1.0 / (cw as f32).max(ch as f32 / 2.0)
        }
    }
}

/// Render a pixmap to truecolor ANSI half-blocks (`▀`). Each pair of pixmap
/// rows becomes one terminal cell row; the foreground holds the upper pixel
/// and the background holds the lower one. Alpha is composited over black.
fn render_text_blocks<W: Write>(out: &mut W, pixmap: &Pixmap) -> io::Result<u16> {
    let w = pixmap.width() as usize;
    let h = pixmap.height() as usize;
    if w == 0 || h == 0 {
        return Ok(0);
    }
    let pixels = pixmap.pixels();
    let mut y = 0;
    let mut lines: u16 = 0;
    while y < h {
        let top = &pixels[y * w..(y + 1) * w];
        let bot: Option<&[_]> = if y + 1 < h {
            Some(&pixels[(y + 1) * w..(y + 2) * w])
        } else {
            None
        };
        for x in 0..w {
            let (tr, tg, tb) = blend_on_black(top[x]);
            let (br, bg, bb) = match bot {
                Some(b) => blend_on_black(b[x]),
                None => (0, 0, 0),
            };
            // Combined SGR is shorter on the wire and avoids partial state
            // if the write is interrupted.
            write!(
                out,
                "\x1b[38;2;{};{};{};48;2;{};{};{}m▀",
                tr, tg, tb, br, bg, bb
            )?;
        }
        // CRLF — in animation mode the tty is in raw mode and a bare LF
        // wouldn't return the cursor to column 0, so each line would start
        // wherever the previous one ended.
        out.write_all(b"\x1b[0m\r\n")?;
        lines = lines.saturating_add(1);
        y += 2;
    }
    Ok(lines)
}

fn blend_on_black(p: tiny_skia::PremultipliedColorU8) -> (u8, u8, u8) {
    // tiny-skia stores RGB premultiplied with alpha. Composite-over-black
    // collapses to `pre.rgb` since black contributes nothing.
    (p.red(), p.green(), p.blue())
}

// -----------------------------------------------------------------------------
// Kitty graphics protocol I/O
// -----------------------------------------------------------------------------

/// Emit the Kitty graphics protocol escape sequences to display `pixmap` at
/// the current cursor position. Uses RGBA raw transmission, chunked.
fn emit_kitty<W: Write>(w: &mut W, pixmap: &Pixmap, id: u32) -> io::Result<()> {
    let encoded = B64.encode(pixmap.data());
    let bytes = encoded.as_bytes();
    let chunk_size = 4096;
    let total_chunks = bytes.len().div_ceil(chunk_size).max(1);
    for (idx, chunk) in bytes.chunks(chunk_size).enumerate() {
        let more: u8 = if idx + 1 < total_chunks { 1 } else { 0 };
        if idx == 0 {
            write!(
                w,
                "\x1b_Ga=T,f=32,s={},v={},i={},q=2,m={};",
                pixmap.width(),
                pixmap.height(),
                id,
                more,
            )?;
        } else {
            write!(w, "\x1b_Gm={},q=2;", more)?;
        }
        w.write_all(chunk)?;
        write!(w, "\x1b\\")?;
    }
    Ok(())
}

fn delete_kitty_image<W: Write>(w: &mut W, id: u32) -> io::Result<()> {
    write!(w, "\x1b_Ga=d,d=I,i={},q=2;\x1b\\", id)
}

// -----------------------------------------------------------------------------
// Public entry points
// -----------------------------------------------------------------------------

/// `show_image` handler installed into the engine on native targets.
/// Receives a pre-built [`crate::scene::Scene`] and dispatches to Kitty when
/// supported, otherwise to Sixel, otherwise to half-blocks ANSI.
pub fn show_image(scene: &crate::scene::Scene) {
    let Some(backend) = pick_backend() else {
        return;
    };
    let target = target_pixels_for_backend(backend);
    let max_scale = max_scale_for_backend(backend);
    let Some(pixmap) = rasterize_scene(scene, target, max_scale) else {
        eprintln!("[spython] failed to rasterize draw list");
        return;
    };
    paint_pixmap(backend, pixmap);
}

fn pick_backend() -> Option<Backend> {
    if kitty_supported() {
        Some(Backend::Kitty)
    } else if sixel::sixel_supported() {
        Some(Backend::Sixel)
    } else if text_blocks_supported() {
        Some(Backend::TextBlocks)
    } else {
        // Python only calls show_image when at least one path is supported,
        // but be defensive.
        None
    }
}

fn paint_pixmap(backend: Backend, pixmap: Pixmap) {
    let mut state = STATE.lock().unwrap();
    let mut stdout = io::stdout().lock();

    match backend {
        Backend::Kitty => {
            if state.in_animation {
                // Re-transmit with the same image ID at (0, 0). Kitty replaces
                // the previous frame in place; an explicit delete-then-transmit
                // cycle shows the cleared cell for one terminal refresh and
                // causes flicker.
                let _ = queue!(stdout, cursor::MoveTo(0, 0));
                let _ = emit_kitty(&mut stdout, &pixmap, KITTY_ANIMATION_ID);
                state.image_displayed = true;
            } else {
                // One-shot inline display (e.g. REPL displayhook): rotate the
                // image id so successive renders do not collide on the same
                // Kitty placement.
                let id = state.next_oneshot_id;
                state.next_oneshot_id = state
                    .next_oneshot_id
                    .checked_add(1)
                    .unwrap_or(KITTY_ONESHOT_ID_BASE);
                let _ = emit_kitty(&mut stdout, &pixmap, id);
                let _ = writeln!(stdout);
            }
        }
        Backend::Sixel => {
            // Sixel: no image-id replacement, so animations must paint over an
            // opaque background or each frame would leave a trail of stale pixels
            // wherever the new frame is transparent.
            let bytes = sixel::encode(&pixmap, (255, 255, 255));
            if state.in_animation {
                let _ = queue!(stdout, cursor::MoveTo(0, 0));
                let _ = stdout.write_all(&bytes);
                state.image_displayed = true;
            } else {
                let _ = stdout.write_all(&bytes);
                let _ = writeln!(stdout);
            }
        }
        Backend::TextBlocks => {
            if state.in_animation {
                // Alt screen + raw mode is already active. Repaint from the
                // top so each frame fully overwrites the previous one.
                let _ = queue!(stdout, cursor::MoveTo(0, 0));
                let lines =
                    render_text_blocks(&mut stdout, &pixmap).unwrap_or(state.text_blocks_lines);
                state.text_blocks_lines = lines;
                state.image_displayed = true;
            } else {
                let _ = render_text_blocks(&mut stdout, &pixmap);
            }
        }
    }
    let _ = stdout.flush();
}

/// `show_svg` handler installed into the engine on native targets. Falls back
/// to printing the SVG source — used on terminals without Kitty graphics
/// support, since the Python side prefers `show_image` when Kitty is available.
pub fn show_svg(svg: &str) {
    println!("{svg}");
}

pub fn enter_animation() {
    let mut state = STATE.lock().unwrap();
    if state.in_animation {
        return;
    }
    if !kitty_supported() && !sixel::sixel_supported() && !text_blocks_supported() {
        eprintln!(
            "[spython] terminal does not advertise graphics support; \
             World output will fall back to printing SVG. \
             Try Kitty, Ghostty, WezTerm, Konsole, a Sixel-capable \
             terminal (Windows Terminal ≥ 1.22, mlterm, foot, mintty), \
             or a truecolor terminal (set COLORTERM=truecolor)."
        );
        return;
    }
    let mut stdout = io::stdout().lock();
    if terminal::enable_raw_mode().is_err() {
        return;
    }
    let _ = execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide);
    state.in_animation = true;
    state.raw_enabled = true;
    state.image_displayed = false;
    state.text_blocks_lines = 0;
}

pub fn exit_animation() {
    let mut state = STATE.lock().unwrap();
    if !state.in_animation {
        return;
    }
    let mut stdout = io::stdout().lock();
    // Kitty: image storage persists across the alt screen flip — delete by id.
    // Sixel + TextBlocks: output lives in the alt-screen grid and disappears
    // when we leave it, no extra cleanup needed.
    if state.image_displayed && kitty_supported() {
        let _ = delete_kitty_image(&mut stdout, KITTY_ANIMATION_ID);
    }
    let _ = execute!(stdout, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = stdout.flush();
    drop(stdout);
    if state.raw_enabled {
        let _ = terminal::disable_raw_mode();
    }
    state.in_animation = false;
    state.raw_enabled = false;
    state.image_displayed = false;
    state.text_blocks_lines = 0;
}

/// Map a `crossterm` `KeyCode` to the string the WASM frontend produces.
fn key_code_to_string(code: KeyCode) -> Option<String> {
    Some(match code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Left => "ArrowLeft".into(),
        KeyCode::Right => "ArrowRight".into(),
        KeyCode::Up => "ArrowUp".into(),
        KeyCode::Down => "ArrowDown".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        KeyCode::Tab | KeyCode::BackTab => "Tab".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::Insert => "Insert".into(),
        KeyCode::Esc => "Escape".into(),
        KeyCode::F(n) => format!("F{n}"),
        _ => return None,
    })
}

/// `poll_key_event` handler installed into the engine on native targets.
pub fn poll_key_event() -> Option<(i32, String, [bool; 5])> {
    {
        let state = STATE.lock().unwrap();
        if !state.raw_enabled {
            return None;
        }
    }

    if !event::poll(Duration::ZERO).ok()? {
        return None;
    }
    let evt = event::read().ok()?;
    let Event::Key(KeyEvent {
        code,
        modifiers,
        kind,
        ..
    }) = evt
    else {
        return None;
    };

    // Ctrl-C in the animation should exit cleanly.
    //
    // TODO(fase 5): once hosts drive the terminal through
    // `crate::frontend::Frontend`, surface Ctrl-C as `InputEvent::Close`
    // instead of killing the process. The current behavior pre-dates
    // `Frontend` and matches `simage::window`'s `CloseRequested` handler;
    // both should change together.
    if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
        exit_animation();
        std::process::exit(130);
    }

    // Most terminals only emit Press; Release requires the kitty keyboard
    // protocol which we do not enable. Repeat is reported as Press too.
    let event_type = match kind {
        KeyEventKind::Release => KEYUP,
        _ => KEYPRESS,
    };

    let key = key_code_to_string(code)?;
    let alt = modifiers.contains(KeyModifiers::ALT);
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let shift = modifiers.contains(KeyModifiers::SHIFT);
    let meta = modifiers.contains(KeyModifiers::SUPER);
    let repeat = matches!(kind, KeyEventKind::Repeat);
    Some((event_type, key, [alt, ctrl, shift, meta, repeat]))
}

/// Install a panic hook so a crashed animation does not leave the terminal
/// in raw mode. Idempotent — calling more than once chains the hooks.
///
/// Note: This is purely a safety net. Hosts (spython, sgleam) typically
/// also wire `enter_animation` / `exit_animation` into their own scripted
/// lifecycle.
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        exit_animation();
        prev(info);
    }));
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

    fn text_node(cx: f32, cy: f32, bw: f32, bh: f32, size: f32, text: &str) -> TextNode {
        TextNode {
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
                size,
                text,
                cx,
                cy,
                bw,
                bh,
                0.0,
            ),
            size,
            text: text.to_owned(),
            ..TextNode::default()
        }
    }

    fn rasterize(scene: &Scene) -> Pixmap {
        rasterize_scene(scene, None, 1.0).expect("pixmap")
    }

    fn rect_path(scene: &mut Scene, style: PathStyle, x: f32, y: f32, w: f32, h: f32) {
        let mut p = scene.path(style);
        p.move_to(x, y);
        p.line_to(x + w, y);
        p.line_to(x + w, y + h);
        p.line_to(x, y + h);
    }

    #[test]
    fn rasterize_filled_rectangle() {
        let mut scene = Scene::new(40.0, 30.0);
        rect_path(&mut scene, solid(0, 0, 255), 0.0, 0.0, 40.0, 30.0);
        let pm = rasterize(&scene);
        assert_eq!(pm.width(), 40);
        assert_eq!(pm.height(), 30);
        assert_eq!(pixel_rgba(&pm, 20, 15), (0, 0, 255, 255));
    }

    #[test]
    fn rasterize_filled_circle_center_is_red() {
        let mut scene = Scene::new(40.0, 40.0);
        {
            let mut p = scene.path(solid(255, 0, 0));
            p.move_to(40.0, 20.0);
            p.arc_to(20.0, 20.0, 0.0, false, true, 0.0, 20.0);
            p.arc_to(20.0, 20.0, 0.0, false, true, 40.0, 20.0);
        }
        let pm = rasterize(&scene);
        let (r, g, b, _) = pixel_rgba(&pm, 20, 20);
        assert_eq!((r, g, b), (255, 0, 0));
    }

    #[test]
    fn rasterize_clip_excludes_outside() {
        // Blue rectangle clipped to a 20×20 box centered at (10, 10) — pixel
        // (35, 25) would lie outside the clip if the full rect made it through.
        let mut scene = Scene::new(20.0, 20.0);
        {
            let mut clip = scene.clip_rect(10.0, 10.0, 20.0, 20.0, 0.0, FillRule::NonZero);
            rect_path(&mut clip, solid(0, 0, 255), -5.0, -5.0, 40.0, 30.0);
        }
        let pm = rasterize(&scene);
        assert_eq!(
            (
                pixel_rgba(&pm, 10, 10).0,
                pixel_rgba(&pm, 10, 10).1,
                pixel_rgba(&pm, 10, 10).2
            ),
            (0, 0, 255)
        );
    }

    #[test]
    fn rasterize_default_background_is_transparent() {
        // Empty image (no commands) should leave the pixmap fully transparent.
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
        // Portuguese "Olá" — multi-byte UTF-8. Render must not panic and must
        // paint pixels.
        let mut scene = Scene::new(100.0, 40.0);
        scene.text(text_node(50.0, 20.0, 100.0, 40.0, 24.0, "Olá"));
        let pm = rasterize(&scene);
        assert!(count_opaque_pixels(&pm) > 30, "expected text pixels");
    }

    #[test]
    fn rasterize_text_underline_adds_pixels() {
        // Same text twice — once with underline and once without. Underline
        // should produce strictly more painted pixels.
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
        // Empty string + valid box should leave the canvas transparent.
        let mut scene = Scene::new(10.0, 10.0);
        scene.text(text_node(5.0, 5.0, 10.0, 10.0, 24.0, ""));
        let pm = rasterize(&scene);
        assert_eq!(count_opaque_pixels(&pm), 0);
    }

    #[test]
    fn scale_to_fit_preserves_aspect() {
        // 200×100 input + 50×50 target → fit width: scale=0.25 → 50×25 output.
        let mut scene = Scene::new(200.0, 100.0);
        rect_path(&mut scene, solid(0, 0, 255), 0.0, 0.0, 200.0, 100.0);
        let pm = rasterize_scene(&scene, Some((50, 50)), 1.0).expect("pixmap");
        assert_eq!(pm.width(), 50);
        assert_eq!(pm.height(), 25);
        assert_eq!(pixel_rgba(&pm, 25, 12), (0, 0, 255, 255));
    }

    #[test]
    fn scale_to_fit_does_not_upscale() {
        // Tiny 10×10 image + huge 1000×1000 target should keep native dims.
        let mut scene = Scene::new(10.0, 10.0);
        rect_path(&mut scene, solid(0, 255, 0), 0.0, 0.0, 10.0, 10.0);
        let pm = rasterize_scene(&scene, Some((1000, 1000)), 1.0).expect("pixmap");
        assert_eq!(pm.width(), 10);
        assert_eq!(pm.height(), 10);
    }

    #[test]
    fn scale_to_fit_height_constrained() {
        // 100×200 input + 200×50 target → fit height: scale=0.25 → 25×50 output.
        let mut scene = Scene::new(100.0, 200.0);
        rect_path(&mut scene, solid(255, 0, 0), 0.0, 0.0, 100.0, 200.0);
        let pm = rasterize_scene(&scene, Some((200, 50)), 1.0).expect("pixmap");
        assert_eq!(pm.width(), 25);
        assert_eq!(pm.height(), 50);
    }

    #[test]
    fn text_blocks_max_scale_caps_below_native() {
        // A 100×100 image rendered for half-blocks (cell 8×16) must shrink to
        // ~native screen pixels: 100 px → P_w = 100/8 ≈ 12 image px. With the
        // old cap of 1.0 the pixmap was 100×100, which painted 100 cols × 50
        // cell rows on screen — way bigger than the native logical size.
        let mut scene = Scene::new(100.0, 100.0);
        rect_path(&mut scene, solid(0, 0, 255), 0.0, 0.0, 100.0, 100.0);
        // target is the half-blocks bounding box for an 80×24 terminal.
        let pm = rasterize_scene(&scene, Some((80, 48)), 1.0 / 8.0).expect("pixmap");
        assert!(pm.width() <= 13, "got width {}", pm.width());
        assert!(pm.height() <= 13, "got height {}", pm.height());
    }

    #[test]
    fn text_blocks_renders_some_pixels() {
        let mut scene = Scene::new(4.0, 4.0);
        rect_path(&mut scene, solid(255, 0, 0), 0.0, 0.0, 4.0, 4.0);
        let pm = rasterize(&scene);
        let mut buf: Vec<u8> = Vec::new();
        let lines = render_text_blocks(&mut buf, &pm).expect("write ok");
        assert_eq!(lines, 2);
        // Half-block character is U+2580 (UTF-8: E2 96 80).
        assert!(buf.windows(3).any(|w| w == [0xE2, 0x96, 0x80]));
    }

    #[test]
    fn text_blocks_uses_truecolor_codes() {
        let mut scene = Scene::new(2.0, 2.0);
        rect_path(&mut scene, solid(0, 0, 255), 0.0, 0.0, 2.0, 2.0);
        let pm = rasterize(&scene);
        let mut buf: Vec<u8> = Vec::new();
        render_text_blocks(&mut buf, &pm).expect("write ok");
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("\x1b[38;2;"), "missing 24-bit fg SGR: {s:?}");
        assert!(s.contains(";48;2;"), "missing 24-bit bg SGR: {s:?}");
        assert!(s.contains("\x1b[0m"), "missing reset: {s:?}");
    }

    #[test]
    fn text_blocks_handles_odd_height() {
        // 3×3: last cell row has no bottom pixel and must default to black.
        let mut scene = Scene::new(3.0, 3.0);
        rect_path(&mut scene, solid(255, 255, 255), 0.0, 0.0, 3.0, 3.0);
        let pm = rasterize(&scene);
        let mut buf: Vec<u8> = Vec::new();
        let lines = render_text_blocks(&mut buf, &pm).expect("write ok");
        // ceil(3/2) = 2 cell rows.
        assert_eq!(lines, 2);
    }

    #[test]
    fn text_blocks_empty_pixmap_is_noop() {
        // Defensive: 1×1 pixmap should still produce one row, not panic.
        let pm = Pixmap::new(1, 1).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        let lines = render_text_blocks(&mut buf, &pm).expect("write ok");
        assert_eq!(lines, 1);
    }

    // -----------------------------------------------------------------------
    // Gradient + dash rendering
    // -----------------------------------------------------------------------

    use crate::scene::{LinearGradient, RadialGradient, Stop};

    #[test]
    fn rasterize_linear_gradient_left_to_right() {
        // 40×10 rect, linear gradient from black (x=0) to white (x=40). The
        // leftmost pixel should be ≈ black, the rightmost ≈ white, and the
        // middle a clearly-different gray in between.
        let mut scene = Scene::new(40.0, 10.0);
        let style = PathStyle {
            fill: IrPaint::Linear(LinearGradient {
                x0: 0.0,
                y0: 0.0,
                x1: 40.0,
                y1: 0.0,
                stops: vec![
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
                ..LinearGradient::default()
            }),
            ..PathStyle::default()
        };
        rect_path(&mut scene, style, 0.0, 0.0, 40.0, 10.0);
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
        // 40×40, radial gradient centered at (20, 20) radius 20: white at
        // center, transparent at the edge.
        let mut scene = Scene::new(40.0, 40.0);
        let style = PathStyle {
            fill: IrPaint::Radial(RadialGradient {
                cx: 20.0,
                cy: 20.0,
                radius: 20.0,
                stops: vec![
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
                ..RadialGradient::default()
            }),
            ..PathStyle::default()
        };
        rect_path(&mut scene, style, 0.0, 0.0, 40.0, 40.0);
        let pm = rasterize(&scene);
        let center_a = pixel_rgba(&pm, 20, 20).3;
        let edge_a = pixel_rgba(&pm, 0, 20).3;
        assert!(center_a > 200, "center too dim: {center_a}");
        assert!(edge_a < 40, "edge too opaque: {edge_a}");
    }

    #[test]
    fn rasterize_linear_gradient_reflect_mirrors_past_axis() {
        // 80×10 rect, gradient axis (0,0)→(20,0): with Reflect, the gradient
        // tiles like an even mirror over t periods of length 2. Pixel at
        // x=10 sits at t=0.5 (mid-axis, gray). Pixel at x=30 sits at t=1.5,
        // which Pad clamps to white but Reflect folds back to t=0.5 (gray).
        let mut scene = Scene::new(80.0, 10.0);
        let style = PathStyle {
            fill: IrPaint::Linear(crate::scene::LinearGradient {
                x0: 0.0,
                y0: 0.0,
                x1: 20.0,
                y1: 0.0,
                spread: crate::scene::SpreadMode::Reflect,
                stops: vec![
                    crate::scene::Stop {
                        offset: 0.0,
                        color: Rgba {
                            r: 0,
                            g: 0,
                            b: 0,
                            a: 1.0,
                        },
                    },
                    crate::scene::Stop {
                        offset: 1.0,
                        color: Rgba {
                            r: 255,
                            g: 255,
                            b: 255,
                            a: 1.0,
                        },
                    },
                ],
            }),
            ..PathStyle::default()
        };
        rect_path(&mut scene, style, 0.0, 0.0, 80.0, 10.0);
        let pm = rasterize(&scene);
        let mid_axis = pixel_rgba(&pm, 10, 5).0; // t = 0.5 → ~gray
        let pad_zone = pixel_rgba(&pm, 30, 5).0; // t = 1.5 → reflect → ~gray
        let pad_far = pixel_rgba(&pm, 50, 5).0; // t = 2.5 → reflect → mid again
        // With Pad these would all clamp to white past x=20; with Reflect they
        // should mirror back into the gradient.
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
        // Horizontal stroke from (5,10) to (95,10) with a [10, 10] dash.
        // Sample on the line: x=10 sits inside an "on" segment (opaque); x=20
        // sits inside an "off" segment (transparent).
        let mut scene = Scene::new(100.0, 20.0);
        {
            let mut p = scene.path(PathStyle {
                stroke: IrPaint::rgba(255, 0, 0, 1.0),
                stroke_width: 3.0,
                dash_array: vec![10.0, 10.0],
                ..PathStyle::default()
            });
            p.move_to(5.0, 10.0);
            p.line_to(95.0, 10.0);
        }
        let pm = rasterize(&scene);
        let on = pixel_rgba(&pm, 10, 10).3;
        let off = pixel_rgba(&pm, 20, 10).3;
        assert!(on > 200, "on-segment expected opaque: {on}");
        assert!(off < 40, "off-segment expected transparent: {off}");
    }

    // -----------------------------------------------------------------------
    // Paint primitives + streaming entry points
    // -----------------------------------------------------------------------

    use crate::scene::ClipPath;

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
