//! Window display backend — paints [`crate::scene::Scene`]s into a native
//! OS window via `winit` + `softbuffer`. Peer of [`crate::terminal`] for
//! environments where the terminal is not graphics-capable (or when the
//! user prefers a real window).
//!
//! ## Lifecycle
//!
//! Mirrors [`crate::terminal`]:
//!
//! - [`enter_animation`] — initialize the window state. Idempotent.
//! - [`show_image`] — rasterize a draw list and present it to the window.
//! - [`poll_key_event`] — non-blocking poll, returns the next queued
//!   keyboard event in the same shape as [`crate::terminal::poll_key_event`]:
//!   `(event_type, key, [alt, ctrl, shift, meta, repeat])`.
//! - [`exit_animation`] — destroy the window.
//!
//! ## Threading
//!
//! `winit` requires the event loop to live on the main thread (especially on
//! macOS). All state is held in a `thread_local!` and the API panics if the
//! caller drives the window from a non-main thread. Hosts that embed an
//! interpreter (spython, sgleam) already keep the script on the main thread,
//! so this restriction is met by construction.
//!
//! ## DPI and resize
//!
//! The window is created with a logical size matching the [`crate::scene::Scene`]
//! dimensions. On every present we rasterize the draw list to the *physical*
//! surface size, so HiDPI scaling and user resizes are handled by re-rendering
//! at the surface resolution while preserving aspect ratio (letterboxed).
//!
//! Unlike [`crate::terminal`], there is no `kitty_supported`/`sixel_supported`
//! capability dance — opening a window either succeeds or fails with a clear
//! error. Hosts that want a fallback (e.g. SVG print) should pick the backend
//! at the host level.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::Duration;

use softbuffer::{Context, Surface};
use tiny_skia::Pixmap;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::pump_events::EventLoopExtPumpEvents;
use winit::window::{Window, WindowAttributes, WindowId};

use crate::terminal::{KEYDOWN, KEYPRESS, KEYUP};

type KeyEventTuple = (i32, String, [bool; 5]);

struct App {
    title: String,
    /// Size requested for the next window-creation. Set on each
    /// `show_image` from the draw-list dimensions; the `resumed` callback
    /// reads it. `None` until the first `show_image`.
    pending_size: Option<(u32, u32)>,
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    pending: VecDeque<KeyEventTuple>,
    modifiers: ModifiersState,
    closed: bool,
}

impl App {
    fn new(title: String) -> Self {
        Self {
            title,
            pending_size: None,
            window: None,
            surface: None,
            pending: VecDeque::new(),
            modifiers: ModifiersState::empty(),
            closed: false,
        }
    }

    fn ensure_window(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let Some((w, h)) = self.pending_size else {
            // No size requested yet — wait for the first show.
            return;
        };
        let attrs = WindowAttributes::default()
            .with_title(self.title.clone())
            .with_inner_size(LogicalSize::new(w as f64, h as f64));
        let Ok(window) = event_loop.create_window(attrs) else {
            self.closed = true;
            return;
        };
        let window = Rc::new(window);
        let context = match Context::new(window.clone()) {
            Ok(c) => c,
            Err(_) => {
                self.closed = true;
                return;
            }
        };
        let surface = match Surface::new(&context, window.clone()) {
            Ok(s) => s,
            Err(_) => {
                self.closed = true;
                return;
            }
        };
        self.window = Some(window);
        self.surface = Some(surface);
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.ensure_window(event_loop);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                // Flag closed and let the event loop wind down; `closed()`
                // reports it and `WindowFrontend::wait_event` turns it into
                // `InputEvent::Close`, so the host unwinds and tears the
                // window down through `exit()`. No `process::exit` — it would
                // kill a server hosting other sessions. Mirrors `terminal`'s
                // Ctrl-C handling.
                self.closed = true;
                event_loop.exit();
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                push_key_events(&mut self.pending, &event, self.modifiers);
            }
            _ => {}
        }
    }
}

/// Translate a `winit` key event into one or two Python events.
///
/// On the first press (no repeat) we emit **both** `KEYDOWN` and `KEYPRESS`,
/// so handlers attached only to `on_key_press` still see a tap, while
/// handlers on `on_key_down` see a clean once-per-press signal. Auto-repeat
/// emits `KEYPRESS` only; release emits `KEYUP`.
fn push_key_events(out: &mut VecDeque<KeyEventTuple>, ev: &KeyEvent, mods: ModifiersState) {
    let Some(key) = winit_key_to_string(&ev.logical_key) else {
        return;
    };
    let m = [
        mods.alt_key(),
        mods.control_key(),
        mods.shift_key(),
        mods.super_key(),
        ev.repeat,
    ];
    match ev.state {
        ElementState::Pressed if ev.repeat => {
            out.push_back((KEYPRESS, key, m));
        }
        ElementState::Pressed => {
            out.push_back((KEYDOWN, key.clone(), m));
            out.push_back((KEYPRESS, key, m));
        }
        ElementState::Released => {
            out.push_back((KEYUP, key, m));
        }
    }
}

/// Map a `winit` logical key to the W3C UI Events string the Python side
/// expects (matches the table in [`crate::terminal::poll_key_event`]).
fn winit_key_to_string(key: &Key) -> Option<String> {
    Some(match key {
        Key::Character(s) => s.to_string(),
        Key::Named(named) => match named {
            NamedKey::Backspace => "Backspace".into(),
            NamedKey::Enter => "Enter".into(),
            NamedKey::Tab => "Tab".into(),
            NamedKey::Space => " ".into(),
            NamedKey::ArrowLeft => "ArrowLeft".into(),
            NamedKey::ArrowRight => "ArrowRight".into(),
            NamedKey::ArrowUp => "ArrowUp".into(),
            NamedKey::ArrowDown => "ArrowDown".into(),
            NamedKey::Home => "Home".into(),
            NamedKey::End => "End".into(),
            NamedKey::PageUp => "PageUp".into(),
            NamedKey::PageDown => "PageDown".into(),
            NamedKey::Delete => "Delete".into(),
            NamedKey::Insert => "Insert".into(),
            NamedKey::Escape => "Escape".into(),
            NamedKey::F1 => "F1".into(),
            NamedKey::F2 => "F2".into(),
            NamedKey::F3 => "F3".into(),
            NamedKey::F4 => "F4".into(),
            NamedKey::F5 => "F5".into(),
            NamedKey::F6 => "F6".into(),
            NamedKey::F7 => "F7".into(),
            NamedKey::F8 => "F8".into(),
            NamedKey::F9 => "F9".into(),
            NamedKey::F10 => "F10".into(),
            NamedKey::F11 => "F11".into(),
            NamedKey::F12 => "F12".into(),
            _ => return None,
        },
        _ => return None,
    })
}

struct State {
    event_loop: EventLoop<()>,
    app: App,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Initialize the window backend. Idempotent — calling twice without a
/// matching [`exit_animation`] is a no-op.
///
/// The OS window itself is created lazily on the first [`show_image`],
/// using the draw-list size as the initial logical dimensions. This way
/// the host doesn't need to predict a size before its first frame.
pub fn enter_animation(title: &str) {
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_some() {
            return;
        }
        let event_loop = match EventLoop::<()>::with_user_event().build() {
            Ok(el) => el,
            Err(e) => {
                eprintln!("[simage] failed to create window event loop: {e}");
                return;
            }
        };
        event_loop.set_control_flow(ControlFlow::Wait);
        let app = App::new(title.to_string());
        *slot = Some(State { event_loop, app });
    });
}

/// Tear down the window. Idempotent.
pub fn exit_animation() {
    STATE.with(|cell| {
        cell.borrow_mut().take();
    });
}

fn pump_for(timeout: Duration) {
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return;
        };
        let _ = state
            .event_loop
            .pump_app_events(Some(timeout), &mut state.app);
    });
}

/// Return the next queued keyboard event, or `None` if the queue is empty.
/// Drives the event loop with a zero-timeout pump first, so freshly-arrived
/// events are visible.
pub fn poll_key_event() -> Option<KeyEventTuple> {
    pump_for(Duration::ZERO);
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let state = slot.as_mut()?;
        if state.app.closed {
            return None;
        }
        state.app.pending.pop_front()
    })
}

/// Whether the user closed the window since [`enter_animation`].
pub fn closed() -> bool {
    STATE.with(|cell| cell.borrow().as_ref().map(|s| s.app.closed).unwrap_or(true))
}

/// Rasterize `scene` and present it in the window. Pumps the event loop first
/// so window resize / DPI changes take effect on the same frame, and
/// lazily creates the window using `scene.width` / `scene.height` on the first
/// call.
pub fn show_image(scene: &crate::scene::Scene) {
    let dl_size = (
        scene.width.ceil().max(1.0) as u32,
        scene.height.ceil().max(1.0) as u32,
    );
    STATE.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut()
            && state.app.pending_size.is_none()
        {
            state.app.pending_size = Some(dl_size);
        }
    });
    // Pump after setting the size so `resumed` can pick it up on the
    // first iteration.
    pump_for(Duration::ZERO);
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return;
        };
        let Some(window) = state.app.window.as_ref() else {
            return;
        };
        let surface = match state.app.surface.as_mut() {
            Some(s) => s,
            None => return,
        };

        let inner = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(inner.width), NonZeroU32::new(inner.height))
        else {
            return;
        };

        if surface.resize(w, h).is_err() {
            return;
        }

        let target_px = (w.get(), h.get());
        let pixmap = match crate::terminal::rasterize_scene(scene, Some(target_px), 32.0) {
            Some(p) => p,
            None => return,
        };

        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };

        blit_pixmap(&pixmap, &mut buffer, target_px);
        let _ = buffer.present();
    });
}

/// Copy a [`tiny_skia::Pixmap`] into a softbuffer `0RGB` u32 buffer.
///
/// The pixmap is centered + letterboxed if smaller than the buffer (which
/// happens because `rasterize_scene` is shrink-only — when the
/// window is bigger than the draw list, the pixmap stays at native size
/// and we paint the surrounding area black).
fn blit_pixmap(pixmap: &Pixmap, buffer: &mut [u32], (bw, bh): (u32, u32)) {
    let pw = pixmap.width();
    let ph = pixmap.height();

    // Center.
    let off_x = (bw.saturating_sub(pw)) / 2;
    let off_y = (bh.saturating_sub(ph)) / 2;

    // Background.
    buffer.fill(0);

    let src = pixmap.pixels();
    for y in 0..ph.min(bh - off_y.min(bh)) {
        let dst_row_start = ((off_y + y) as usize) * (bw as usize) + off_x as usize;
        let src_row_start = (y as usize) * (pw as usize);
        let row_w = pw.min(bw - off_x.min(bw)) as usize;
        for x in 0..row_w {
            // softbuffer wants straight-alpha 0RGB (0x00_RR_GG_BB); alpha is
            // dropped since the window has no transparency.
            let (r, g, b) = crate::pixel::unpremultiply(src[src_row_start + x]);
            buffer[dst_row_start + x] = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
        }
    }
}

/// Mirrors [`crate::terminal::install_panic_hook`] but currently a no-op:
/// a window-backed animation has no terminal state to restore, and the OS
/// will reclaim the window when the process aborts. Provided so hosts can
/// install hooks symmetrically.
pub fn install_panic_hook() {}
