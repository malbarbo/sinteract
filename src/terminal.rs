//! Terminal renderer for `World` and inline images using the Kitty graphics
//! protocol.
//!
//! On Kitty-compatible terminals (Kitty, Ghostty, WezTerm, modern Konsole),
//! `show_image_dl` rasterizes a [`crate::ir::DrawList`] directly with
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
    FillRule as SkFillRule, LineCap as SkLineCap, LineJoin as SkLineJoin, Mask, Paint, PathBuilder,
    Pixmap, Stroke, Transform,
};

use crate::ir::{BitmapNode, ClipBox, FillRule, LineCap, LineJoin, PathStyle, Rgba, TextNode};
use crate::sink::DrawSink;
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
// PixmapSink — DrawSink → tiny-skia raster
// -----------------------------------------------------------------------------

struct PixmapSink {
    /// Target output box in pixels (`None` = render at native size).
    target_px: Option<(u32, u32)>,
    /// Upper bound on the rasterizer's uniform scale factor — depends on
    /// the active backend (1.0 for Kitty/Sixel, smaller for half-blocks).
    max_scale: f32,
    /// Output pixmap, allocated when `begin` fires.
    pixmap: Option<Pixmap>,
    /// `input → output` scale folded into a transform applied to every path.
    base: Transform,
    clip_stack: Vec<Mask>,
    out_w: u32,
    out_h: u32,
    pending: Option<PendingPath>,
}

struct PendingPath {
    builder: PathBuilder,
    style: PathStyle,
    has_points: bool,
}

impl PixmapSink {
    fn new(target_px: Option<(u32, u32)>, max_scale: f32) -> Self {
        Self {
            target_px,
            max_scale,
            pixmap: None,
            base: Transform::identity(),
            clip_stack: Vec::new(),
            out_w: 0,
            out_h: 0,
            pending: None,
        }
    }

    fn flush_path(&mut self) {
        let Some(p) = self.pending.take() else {
            return;
        };
        let Some(pixmap) = self.pixmap.as_mut() else {
            return;
        };
        if !p.has_points {
            return;
        }
        let mut builder = p.builder;
        if p.style.closed {
            builder.close();
        }
        let Some(path) = builder.finish() else {
            return;
        };
        let mask = self.clip_stack.last();

        if p.style.fill.a > 0.0 {
            let mut paint = Paint::default();
            paint.set_color_rgba8(
                p.style.fill.r,
                p.style.fill.g,
                p.style.fill.b,
                (p.style.fill.a * 255.0).round().clamp(0.0, 255.0) as u8,
            );
            paint.anti_alias = true;
            pixmap.fill_path(
                &path,
                &paint,
                sk_fill_rule(p.style.fill_rule),
                self.base,
                mask,
            );
        }
        if p.style.stroke.a > 0.0 && p.style.stroke_width > 0.0 {
            let mut paint = Paint::default();
            paint.set_color_rgba8(
                p.style.stroke.r,
                p.style.stroke.g,
                p.style.stroke.b,
                (p.style.stroke.a * 255.0).round().clamp(0.0, 255.0) as u8,
            );
            paint.anti_alias = true;
            let stroke = Stroke {
                width: p.style.stroke_width,
                line_cap: sk_line_cap(p.style.line_cap),
                line_join: sk_line_join(p.style.line_join),
                miter_limit: 10.0,
                dash: None,
            };
            pixmap.stroke_path(&path, &paint, &stroke, self.base, mask);
        }
    }
}

impl DrawSink for PixmapSink {
    fn begin(&mut self, width: f32, height: f32) {
        let w = width.ceil().max(1.0) as u32;
        let h = height.ceil().max(1.0) as u32;
        let s = compute_scale(w, h, self.target_px, self.max_scale);
        self.out_w = ((w as f32) * s).ceil().max(1.0) as u32;
        self.out_h = ((h as f32) * s).ceil().max(1.0) as u32;
        self.base = Transform::from_scale(s, s);
        self.pixmap = Pixmap::new(self.out_w, self.out_h).map(|mut pm| {
            // Transparent background: the terminal background shows through.
            pm.fill(tiny_skia::Color::TRANSPARENT);
            pm
        });
    }

    fn path_begin(&mut self, style: &PathStyle) {
        self.flush_path();
        self.pending = Some(PendingPath {
            builder: PathBuilder::new(),
            style: *style,
            has_points: false,
        });
    }

    fn move_to(&mut self, x: f32, y: f32) {
        if let Some(p) = self.pending.as_mut() {
            p.builder.move_to(x, y);
            p.has_points = true;
        }
    }

    fn line_to(&mut self, x: f32, y: f32) {
        if let Some(p) = self.pending.as_mut() {
            p.builder.line_to(x, y);
            p.has_points = true;
        }
    }

    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        if let Some(p) = self.pending.as_mut() {
            p.builder.quad_to(cx, cy, x, y);
            p.has_points = true;
        }
    }

    fn cubic_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        if let Some(p) = self.pending.as_mut() {
            p.builder.cubic_to(c1x, c1y, c2x, c2y, x, y);
            p.has_points = true;
        }
    }

    fn path_end(&mut self) {
        self.flush_path();
    }

    fn clip_push(&mut self, clip: &ClipBox) {
        self.flush_path();
        let parent = self.clip_stack.last();
        let mut builder = PathBuilder::new();
        let hw = clip.w / 2.0;
        let hh = clip.h / 2.0;
        let corners = [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)];
        let cos = (clip.angle * std::f32::consts::PI / 180.0).cos();
        let sin = (clip.angle * std::f32::consts::PI / 180.0).sin();
        for (i, (x, y)) in corners.iter().enumerate() {
            let rx = clip.cx + x * cos - y * sin;
            let ry = clip.cy + x * sin + y * cos;
            if i == 0 {
                builder.move_to(rx, ry);
            } else {
                builder.line_to(rx, ry);
            }
        }
        builder.close();
        let Some(path) = builder.finish() else { return };
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
            return;
        };
        mask.intersect_path(&path, SkFillRule::Winding, true, self.base);
        self.clip_stack.push(mask);
    }

    fn clip_pop(&mut self) {
        self.flush_path();
        self.clip_stack.pop();
    }

    fn text(&mut self, node: &TextNode) {
        self.flush_path();
        let Some(pixmap) = self.pixmap.as_mut() else {
            return;
        };
        render_text(node, pixmap, self.clip_stack.last(), self.base);
    }

    fn bitmap(&mut self, _node: &BitmapNode) {
        self.flush_path();
        let mut s = STATE.lock().unwrap();
        if !s.warned_bitmap {
            eprintln!(
                "[spython] terminal renderer does not yet support bitmaps; \
                 this image is being shown without bitmaps."
            );
            s.warned_bitmap = true;
        }
    }

    fn end(&mut self) {
        self.flush_path();
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

/// Rasterize a [`crate::ir::DrawList`], optionally fitting the output to
/// `target_px` (in pixels). When a target is given, the output is uniformly
/// scaled so it fits inside the target box while preserving aspect ratio.
/// Scaling is **shrink-only**: an image smaller than the target stays at its
/// native dimensions (the user picked those numbers; respect them).
pub(crate) fn rasterize_draw_list_dl(
    dl: &crate::ir::DrawList,
    target_px: Option<(u32, u32)>,
    max_scale: f32,
) -> Option<Pixmap> {
    let mut sink = PixmapSink::new(target_px, max_scale);
    dl.play_into(&mut sink);
    sink.pixmap
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

    let original_w = crate::text::measure_width_with(face, &node.text, size_i) as f32;
    let original_h = crate::text::measure_height_with(face, &node.text, size_i) as f32;
    if original_w <= 0.0 || original_h <= 0.0 {
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

    let scale_x = node.bw / original_w * if node.flip_h { -1.0 } else { 1.0 };
    let scale_y = node.bh / original_h * if node.flip_v { -1.0 } else { 1.0 };
    // Compose the local text transform first, then the global pixmap-scale
    // (`base`). `post_concat` means: apply `local` to the point, then `base`
    // — i.e. final = base * local * p.
    let transform = Transform::from_translate(node.cx, node.cy)
        .pre_rotate(node.angle)
        .pre_scale(scale_x, scale_y)
        .post_concat(base);

    let Rgba {
        r: fr,
        g: fg,
        b: fb,
        a: fa,
    } = node.fill;
    if fa > 0.0 {
        let mut paint = Paint::default();
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
        let mut paint = Paint::default();
        paint.set_color_rgba8(sr, sg, sb, (sa * 255.0).round().clamp(0.0, 255.0) as u8);
        paint.anti_alias = true;
        let stroke = Stroke {
            width: node.stroke_width,
            line_cap: sk_line_cap(node.line_cap),
            line_join: sk_line_join(node.line_join),
            miter_limit: 10.0,
            dash: None,
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

/// `show_image_dl` handler installed into the engine on native targets.
/// Receives a pre-built [`crate::ir::DrawList`] and dispatches to Kitty when
/// supported, otherwise to Sixel, otherwise to half-blocks ANSI.
pub fn show_image_dl(dl: &crate::ir::DrawList) {
    let Some(backend) = pick_backend() else {
        return;
    };
    let target = target_pixels_for_backend(backend);
    let max_scale = max_scale_for_backend(backend);
    let Some(pixmap) = rasterize_draw_list_dl(dl, target, max_scale) else {
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
    use crate::ir::DrawList;

    fn pixel_rgba(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let p = pixmap.pixel(x, y).expect("pixel in range");
        let p = p.demultiply();
        (p.red(), p.green(), p.blue(), p.alpha())
    }

    fn solid(r: u8, g: u8, b: u8) -> PathStyle {
        PathStyle {
            fill: Rgba { r, g, b, a: 1.0 },
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
            cx,
            cy,
            bw,
            bh,
            size,
            text: text.to_owned(),
            ..TextNode::default()
        }
    }

    fn rasterize(dl: &DrawList) -> Pixmap {
        rasterize_draw_list_dl(dl, None, 1.0).expect("pixmap")
    }

    fn rect_path(dl: &mut DrawList, style: PathStyle, x: f32, y: f32, w: f32, h: f32) {
        dl.path_begin(style);
        dl.move_to(x, y);
        dl.line_to(x + w, y);
        dl.line_to(x + w, y + h);
        dl.line_to(x, y + h);
    }

    #[test]
    fn rasterize_filled_rectangle() {
        let mut dl = DrawList::new(40.0, 30.0);
        rect_path(&mut dl, solid(0, 0, 255), 0.0, 0.0, 40.0, 30.0);
        let pm = rasterize(&dl);
        assert_eq!(pm.width(), 40);
        assert_eq!(pm.height(), 30);
        assert_eq!(pixel_rgba(&pm, 20, 15), (0, 0, 255, 255));
    }

    #[test]
    fn rasterize_filled_circle_center_is_red() {
        let mut dl = DrawList::new(40.0, 40.0);
        dl.path_begin(solid(255, 0, 0));
        dl.move_to(40.0, 20.0);
        dl.arc_to(20.0, 20.0, 0.0, false, true, 0.0, 20.0);
        dl.arc_to(20.0, 20.0, 0.0, false, true, 40.0, 20.0);
        let pm = rasterize(&dl);
        let (r, g, b, _) = pixel_rgba(&pm, 20, 20);
        assert_eq!((r, g, b), (255, 0, 0));
    }

    #[test]
    fn rasterize_clip_excludes_outside() {
        // Blue rectangle clipped to a 20×20 box centered at (10, 10) — pixel
        // (35, 25) would lie outside the clip if the full rect made it through.
        let mut dl = DrawList::new(20.0, 20.0);
        dl.clip_push(ClipBox {
            cx: 10.0,
            cy: 10.0,
            w: 20.0,
            h: 20.0,
            angle: 0.0,
        });
        rect_path(&mut dl, solid(0, 0, 255), -5.0, -5.0, 40.0, 30.0);
        dl.clip_pop();
        let pm = rasterize(&dl);
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
        let dl = DrawList::new(5.0, 5.0);
        let pm = rasterize(&dl);
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
        let mut dl = DrawList::new(100.0, 40.0);
        dl.text(text_node(50.0, 20.0, 100.0, 40.0, 24.0, "Hi"));
        let pm = rasterize(&dl);
        assert!(count_opaque_pixels(&pm) > 50, "expected text pixels");
    }

    #[test]
    fn rasterize_text_corners_remain_transparent() {
        let mut dl = DrawList::new(200.0, 60.0);
        dl.text(text_node(100.0, 30.0, 200.0, 60.0, 24.0, "Hi"));
        let pm = rasterize(&dl);
        assert_eq!(pixel_rgba(&pm, 0, 0).3, 0);
        assert_eq!(pixel_rgba(&pm, 199, 59).3, 0);
    }

    #[test]
    fn rasterize_text_handles_multibyte_utf8() {
        // Portuguese "Olá" — multi-byte UTF-8. Render must not panic and must
        // paint pixels.
        let mut dl = DrawList::new(100.0, 40.0);
        dl.text(text_node(50.0, 20.0, 100.0, 40.0, 24.0, "Olá"));
        let pm = rasterize(&dl);
        assert!(count_opaque_pixels(&pm) > 30, "expected text pixels");
    }

    #[test]
    fn rasterize_text_underline_adds_pixels() {
        // Same text twice — once with underline and once without. Underline
        // should produce strictly more painted pixels.
        let mut without = DrawList::new(100.0, 40.0);
        without.text(text_node(50.0, 20.0, 100.0, 40.0, 24.0, "Hi"));
        let mut with = DrawList::new(100.0, 40.0);
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
        let mut dl = DrawList::new(10.0, 10.0);
        dl.text(text_node(5.0, 5.0, 10.0, 10.0, 24.0, ""));
        let pm = rasterize(&dl);
        assert_eq!(count_opaque_pixels(&pm), 0);
    }

    #[test]
    fn scale_to_fit_preserves_aspect() {
        // 200×100 input + 50×50 target → fit width: scale=0.25 → 50×25 output.
        let mut dl = DrawList::new(200.0, 100.0);
        rect_path(&mut dl, solid(0, 0, 255), 0.0, 0.0, 200.0, 100.0);
        let pm = rasterize_draw_list_dl(&dl, Some((50, 50)), 1.0).expect("pixmap");
        assert_eq!(pm.width(), 50);
        assert_eq!(pm.height(), 25);
        assert_eq!(pixel_rgba(&pm, 25, 12), (0, 0, 255, 255));
    }

    #[test]
    fn scale_to_fit_does_not_upscale() {
        // Tiny 10×10 image + huge 1000×1000 target should keep native dims.
        let mut dl = DrawList::new(10.0, 10.0);
        rect_path(&mut dl, solid(0, 255, 0), 0.0, 0.0, 10.0, 10.0);
        let pm = rasterize_draw_list_dl(&dl, Some((1000, 1000)), 1.0).expect("pixmap");
        assert_eq!(pm.width(), 10);
        assert_eq!(pm.height(), 10);
    }

    #[test]
    fn scale_to_fit_height_constrained() {
        // 100×200 input + 200×50 target → fit height: scale=0.25 → 25×50 output.
        let mut dl = DrawList::new(100.0, 200.0);
        rect_path(&mut dl, solid(255, 0, 0), 0.0, 0.0, 100.0, 200.0);
        let pm = rasterize_draw_list_dl(&dl, Some((200, 50)), 1.0).expect("pixmap");
        assert_eq!(pm.width(), 25);
        assert_eq!(pm.height(), 50);
    }

    #[test]
    fn text_blocks_max_scale_caps_below_native() {
        // A 100×100 image rendered for half-blocks (cell 8×16) must shrink to
        // ~native screen pixels: 100 px → P_w = 100/8 ≈ 12 image px. With the
        // old cap of 1.0 the pixmap was 100×100, which painted 100 cols × 50
        // cell rows on screen — way bigger than the native logical size.
        let mut dl = DrawList::new(100.0, 100.0);
        rect_path(&mut dl, solid(0, 0, 255), 0.0, 0.0, 100.0, 100.0);
        // target is the half-blocks bounding box for an 80×24 terminal.
        let pm = rasterize_draw_list_dl(&dl, Some((80, 48)), 1.0 / 8.0).expect("pixmap");
        assert!(pm.width() <= 13, "got width {}", pm.width());
        assert!(pm.height() <= 13, "got height {}", pm.height());
    }

    #[test]
    fn text_blocks_renders_some_pixels() {
        let mut dl = DrawList::new(4.0, 4.0);
        rect_path(&mut dl, solid(255, 0, 0), 0.0, 0.0, 4.0, 4.0);
        let pm = rasterize(&dl);
        let mut buf: Vec<u8> = Vec::new();
        let lines = render_text_blocks(&mut buf, &pm).expect("write ok");
        assert_eq!(lines, 2);
        // Half-block character is U+2580 (UTF-8: E2 96 80).
        assert!(buf.windows(3).any(|w| w == [0xE2, 0x96, 0x80]));
    }

    #[test]
    fn text_blocks_uses_truecolor_codes() {
        let mut dl = DrawList::new(2.0, 2.0);
        rect_path(&mut dl, solid(0, 0, 255), 0.0, 0.0, 2.0, 2.0);
        let pm = rasterize(&dl);
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
        let mut dl = DrawList::new(3.0, 3.0);
        rect_path(&mut dl, solid(255, 255, 255), 0.0, 0.0, 3.0, 3.0);
        let pm = rasterize(&dl);
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
}
