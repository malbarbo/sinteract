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
use tiny_skia::Pixmap;

use crate::pixmap::rasterize_scene;
use crate::sixel;

const KITTY_ANIMATION_ID: u32 = 1042;
const KITTY_ONESHOT_ID_BASE: u32 = 2000;

pub(crate) const KEYPRESS: i32 = 0;
pub(crate) const KEYDOWN: i32 = 1;
pub(crate) const KEYUP: i32 = 2;

// Fallback when the `CSI 16 t` probe returns nothing; see `cell_pixels`.
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
    text_blocks_lines: u16,
    /// Ctrl-C during animation; surfaced as [`InputEvent::Close`], not a process kill.
    closed: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    in_animation: false,
    raw_enabled: false,
    image_displayed: false,
    next_oneshot_id: KITTY_ONESHOT_ID_BASE,
    text_blocks_lines: 0,
    closed: false,
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
    // Clear before any early-return, else a prior session's Ctrl-C closes this one.
    state.closed = false;
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

    // Flag closed, not `process::exit` — that would kill a server hosting
    // other sessions. `wait_event` turns the flag into `InputEvent::Close`.
    if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
        STATE.lock().unwrap().closed = true;
        return None;
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

/// Ctrl-C since [`enter_animation`]? `TerminalFrontend` polls it to emit
/// [`crate::event::InputEvent::Close`].
pub fn closed() -> bool {
    STATE.lock().unwrap().closed
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
    use crate::scene::{FillRule, Paint as IrPaint, PathStyle, Rgba, Scene, TextNode};

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

    use crate::scene::{Gradient, Stop};

    #[test]
    fn rasterize_linear_gradient_left_to_right() {
        // 40×10 rect, linear gradient from black (x=0) to white (x=40). The
        // leftmost pixel should be ≈ black, the rightmost ≈ white, and the
        // middle a clearly-different gray in between.
        let mut scene = Scene::new(40.0, 10.0);
        let style = PathStyle {
            fill: IrPaint::gradient(Gradient::linear(
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
            )),
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
            fill: IrPaint::gradient(Gradient::radial(
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
            )),
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
            fill: IrPaint::gradient(
                crate::scene::Gradient::linear(
                    0.0,
                    0.0,
                    20.0,
                    0.0,
                    vec![
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
                )
                .with_spread(crate::scene::SpreadMode::Reflect),
            ),
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
}
