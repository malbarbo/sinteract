//! Terminal display of a [`crate::scene::Scene`] through the Kitty graphics
//! protocol, DEC Sixel or truecolor half-blocks, and the animation lifecycle
//! of alt screen and raw mode with key polling. An animation frame replaces
//! the previous one at (0, 0).
//!
//! A terminal does not tell a key down from a key up, so every key event is
//! a press.

use std::io::{self, Write};
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{cursor, event, execute, queue, terminal};
use tiny_skia::Pixmap;

use crate::renderer::pixmap::rasterize_scene;
use crate::sixel;

const KITTY_ANIMATION_ID: u32 = 1042;
const KITTY_ONESHOT_ID_BASE: u32 = 2000;

pub(crate) const KEYPRESS: i32 = 0;
pub(crate) const KEYDOWN: i32 = 1;
pub(crate) const KEYUP: i32 = 2;

// Fallback when the terminal does not answer the `CSI 16 t` probe.
const CELL_W_DEFAULT: u32 = 8;
const CELL_H_DEFAULT: u32 = 16;

/// Pixel size of one terminal cell, from the cached probe in
/// [`crate::term_query`]. A terminal under a multiplexer or without a tty
/// does not answer, and gets 8 by 16.
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
    /// Set by Ctrl-C during an animation. The frontend reports it as a close.
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

/// Returns `true` if the terminal reports 24-bit color, `false` otherwise.
/// The half-blocks fallback needs it.
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
    // These terminals support truecolor without setting COLORTERM.
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

/// Returns `true` if the terminal speaks the Kitty graphics protocol,
/// `false` otherwise. The answer comes from a query to the terminal, because
/// the environment variables are wrong over ssh and under a multiplexer.
/// The probe runs at most once per process.
pub fn kitty_supported() -> bool {
    crate::term_query::graphics_caps().kitty
}

// -----------------------------------------------------------------------------
// Terminal-aware sizing + half-blocks renderer
// -----------------------------------------------------------------------------

/// Pixel box available for the image, from the terminal size. `None` when
/// crossterm cannot read the size, as when stdout is a file.
fn target_pixels_for_backend(backend: Backend) -> Option<(u32, u32)> {
    let (cols, rows) = terminal::size().ok()?;
    if cols == 0 || rows == 0 {
        return None;
    }
    // One row stays free for the prompt that follows the image.
    let rows_avail = rows.saturating_sub(1).max(1);
    let (cw, ch) = cell_pixels();
    Some(match backend {
        Backend::Kitty | Backend::Sixel => (cols as u32 * cw, rows_avail as u32 * ch),
        // Half-blocks pack two image-pixel rows into one cell row, and one
        // image-pixel column into one cell column.
        Backend::TextBlocks => (cols as u32, rows_avail as u32 * 2),
    })
}

/// Upper bound on the scale of the rasterizer for `backend`. In Kitty and
/// Sixel a pixmap pixel is a screen pixel, so the cap of 1.0 keeps the image
/// at native size or smaller. In half-blocks a pixmap pixel covers a cell
/// width by half a cell height, so the cap is the inverse of the larger of
/// the two, and a logical pixel never grows past a screen pixel.
fn max_scale_for_backend(backend: Backend) -> f32 {
    match backend {
        Backend::Kitty | Backend::Sixel => 1.0,
        Backend::TextBlocks => {
            let (cw, ch) = cell_pixels();
            1.0 / (cw as f32).max(ch as f32 / 2.0)
        }
    }
}

/// Scale for a `width` by `height` frame on `backend`. The frame shrinks to
/// fit the cell grid when the grid size is known, and the cap of the backend
/// applies either way, because half-blocks pack two rows per cell even
/// without a known grid.
fn scale_for_backend(backend: Backend, width: f32, height: f32) -> f32 {
    let cap = max_scale_for_backend(backend);
    match target_pixels_for_backend(backend) {
        Some(target) => crate::renderer::pixmap::fit_scale(width, height, target).min(cap),
        None => cap,
    }
}

/// Write `pixmap` as truecolor half-blocks (`▀`). Each pair of rows becomes
/// one cell row, with the upper pixel in the foreground and the lower one in
/// the background, both composited over black.
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
            // One SGR for both colors is shorter and leaves no partial state
            // if the write stops.
            write!(
                out,
                "\x1b[38;2;{};{};{};48;2;{};{};{}m▀",
                tr, tg, tb, br, bg, bb
            )?;
        }
        // In raw mode a bare LF does not return the cursor to column 0.
        out.write_all(b"\x1b[0m\r\n")?;
        lines = lines.saturating_add(1);
        y += 2;
    }
    Ok(lines)
}

fn blend_on_black(p: tiny_skia::PremultipliedColorU8) -> (u8, u8, u8) {
    // The pixel is premultiplied, so compositing over black changes nothing.
    (p.red(), p.green(), p.blue())
}

// -----------------------------------------------------------------------------
// Kitty graphics protocol I/O
// -----------------------------------------------------------------------------

/// Write the Kitty escape sequences that show `pixmap` at the cursor, as raw
/// RGBA in chunks.
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

/// Show `scene` in the terminal, through Kitty when the terminal supports
/// it, else Sixel, else half-blocks.
pub fn show_image(scene: &crate::scene::Scene) {
    let Some(backend) = pick_backend() else {
        return;
    };
    let scale = scale_for_backend(backend, scene.width, scene.height);
    let Some(pixmap) = rasterize_scene(scene, scale) else {
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
        None
    }
}

fn paint_pixmap(backend: Backend, pixmap: Pixmap) {
    let mut state = STATE.lock().unwrap();
    let mut stdout = io::stdout().lock();

    match backend {
        Backend::Kitty => {
            if state.in_animation {
                // The same image id replaces the frame in place. A delete
                // followed by a transmit shows the cleared cells for one
                // refresh and flickers.
                let _ = queue!(stdout, cursor::MoveTo(0, 0));
                let _ = emit_kitty(&mut stdout, &pixmap, KITTY_ANIMATION_ID);
                state.image_displayed = true;
            } else {
                // A new id per image, so successive images do not replace
                // each other.
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
            // Sixel has no image id, so a frame paints over an opaque
            // background, or its transparent pixels would show the previous
            // frame.
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
                // Repaint from the top, so a frame overwrites the previous one.
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

/// Print the SVG source. A host uses it when the terminal has no graphics.
pub fn show_svg(svg: &str) {
    println!("{svg}");
}

pub fn enter_animation() {
    let mut state = STATE.lock().unwrap();
    if state.in_animation {
        return;
    }
    // Reset before any early return, or the Ctrl-C of a previous session
    // closes this one.
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
    // Kitty keeps the image across the alt screen flip, so delete it by id.
    // Sixel and half-blocks output lives in the alt screen and goes with it.
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

/// Map a crossterm key code to the key name of the W3C UI Events spec.
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

/// Return the next key event without blocking, as
/// `(kind, key, [alt, ctrl, shift, meta, repeat])`.
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

    // process::exit would kill a server that hosts other sessions, so the
    // frontend reports a close instead.
    if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
        STATE.lock().unwrap().closed = true;
        return None;
    }

    // A terminal reports a release only with the kitty keyboard protocol,
    // which is off, and reports a repeat as a press.
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

/// Returns `true` if the user pressed Ctrl-C since [`enter_animation`],
/// `false` otherwise.
pub fn closed() -> bool {
    STATE.lock().unwrap().closed
}

/// Install a panic hook that takes the terminal out of raw mode after a
/// crash. A second call chains the hooks.
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
        rasterize_scene(scene, 1.0).expect("pixmap")
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
        // The rectangle is larger than the clip box.
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
        let mut scene = Scene::new(10.0, 10.0);
        scene.text(text_node(5.0, 5.0, 10.0, 10.0, 24.0, ""));
        let pm = rasterize(&scene);
        assert_eq!(count_opaque_pixels(&pm), 0);
    }

    /// Fit into `target`, then apply `cap`, as `scale_for_backend` does when
    /// the grid is known.
    fn fit_capped(scene: &Scene, target: (u32, u32), cap: f32) -> f32 {
        crate::renderer::pixmap::fit_scale(scene.width, scene.height, target).min(cap)
    }

    #[test]
    fn scale_to_fit_preserves_aspect() {
        // 200×100 into 50×50 fits the width, scale 0.25, output 50×25.
        let mut scene = Scene::new(200.0, 100.0);
        rect_path(&mut scene, solid(0, 0, 255), 0.0, 0.0, 200.0, 100.0);
        let pm = rasterize_scene(&scene, fit_capped(&scene, (50, 50), 1.0)).expect("pixmap");
        assert_eq!(pm.width(), 50);
        assert_eq!(pm.height(), 25);
        assert_eq!(pixel_rgba(&pm, 25, 12), (0, 0, 255, 255));
    }

    #[test]
    fn scale_to_fit_does_not_upscale() {
        // A 10×10 image keeps its size in a 1000×1000 target.
        let mut scene = Scene::new(10.0, 10.0);
        rect_path(&mut scene, solid(0, 255, 0), 0.0, 0.0, 10.0, 10.0);
        let pm = rasterize_scene(&scene, fit_capped(&scene, (1000, 1000), 1.0)).expect("pixmap");
        assert_eq!(pm.width(), 10);
        assert_eq!(pm.height(), 10);
    }

    #[test]
    fn scale_to_fit_height_constrained() {
        // 100×200 into 200×50 fits the height, scale 0.25, output 25×50.
        let mut scene = Scene::new(100.0, 200.0);
        rect_path(&mut scene, solid(255, 0, 0), 0.0, 0.0, 100.0, 200.0);
        let pm = rasterize_scene(&scene, fit_capped(&scene, (200, 50), 1.0)).expect("pixmap");
        assert_eq!(pm.width(), 25);
        assert_eq!(pm.height(), 50);
    }

    #[test]
    fn text_blocks_max_scale_caps_below_native() {
        // A 100×100 image for half-blocks with 8×16 cells shrinks to about
        // 100 / 8 pixels wide.
        let mut scene = Scene::new(100.0, 100.0);
        rect_path(&mut scene, solid(0, 0, 255), 0.0, 0.0, 100.0, 100.0);
        // The target is the half-blocks box of an 80×24 terminal.
        let pm = rasterize_scene(&scene, fit_capped(&scene, (80, 48), 1.0 / 8.0)).expect("pixmap");
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
        // U+2580 in UTF-8.
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
        // The last cell row has no bottom pixel and takes black.
        let mut scene = Scene::new(3.0, 3.0);
        rect_path(&mut scene, solid(255, 255, 255), 0.0, 0.0, 3.0, 3.0);
        let pm = rasterize(&scene);
        let mut buf: Vec<u8> = Vec::new();
        let lines = render_text_blocks(&mut buf, &pm).expect("write ok");
        // ceil(3 / 2) rows.
        assert_eq!(lines, 2);
    }

    #[test]
    fn text_blocks_empty_pixmap_is_noop() {
        // A 1×1 pixmap gives one row.
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
        // Black at x=0 to white at x=40. The left pixel is near black, the
        // right near white, and the middle a gray between them.
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
        // White at the center, transparent at the edge.
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
        // The axis goes from x=0 to x=20. Reflect mirrors the gradient with
        // period 2, so x=10 (t=0.5) and x=30 (t=1.5) are both gray, where Pad
        // would clamp x=30 to white.
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
            let mut p = scene.path(PathStyle {
                stroke: IrPaint::rgba(255, 0, 0, 1.0),
                stroke_width: 3.0,
                dash: crate::scene::Dash::new(vec![10.0, 10.0], 0.0).map(Box::new),
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
