//! Window display of a [`crate::scene::Scene`] through winit and softbuffer,
//! with the lifecycle of [`super::terminal`]. [`enter_animation`] sets the
//! state up, [`show_image`] rasterizes a scene and presents it,
//! [`poll_key_event`] returns the next key event, and [`exit_animation`]
//! destroys the window.
//!
//! winit needs the event loop on the main thread, so the state lives in a
//! `thread_local!`. The window opens with the logical size of the scene, and
//! every present rasterizes at the physical size of the surface, letterboxed,
//! so HiDPI and a resize keep the aspect ratio. Opening a window succeeds or
//! fails, so there is no capability query. A host that wants a fallback picks
//! the backend itself.

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

use crate::event::KeyKind;

struct App {
    title: String,
    /// Size of the window to create, from the first `show_image`. `resumed`
    /// reads it.
    pending_size: Option<(u32, u32)>,
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    pending: VecDeque<crate::event::KeyEvent>,
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
            // The size comes with the first show.
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
                // process::exit would kill a server that hosts other sessions,
                // so the frontend reports a close instead.
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

/// Push the events of a winit key event. A first press gives `Down` and
/// `Press`, so a handler on either sees the tap, an auto-repeat gives
/// `Press` only, and a release gives `Up`.
fn push_key_events(
    out: &mut VecDeque<crate::event::KeyEvent>,
    ev: &KeyEvent,
    mods: ModifiersState,
) {
    let Some(key) = winit_key_to_string(&ev.logical_key) else {
        return;
    };
    let modifiers = crate::event::modifiers(
        mods.alt_key(),
        mods.control_key(),
        mods.shift_key(),
        mods.super_key(),
        ev.repeat,
    );
    let event = |kind, key| crate::event::KeyEvent {
        kind,
        key,
        modifiers,
    };
    match ev.state {
        ElementState::Pressed if ev.repeat => {
            out.push_back(event(KeyKind::Press, key));
        }
        ElementState::Pressed => {
            out.push_back(event(KeyKind::Down, key.clone()));
            out.push_back(event(KeyKind::Press, key));
        }
        ElementState::Released => {
            out.push_back(event(KeyKind::Up, key));
        }
    }
}

/// Map a winit key to the key name of the W3C UI Events spec, the same table
/// as [`super::terminal::poll_key_event`].
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

/// Set the window state up. A second call without [`exit_animation`] does
/// nothing. The window itself opens on the first [`show_image`], with the
/// size of the scene, so the host does not need the size before its first
/// frame.
pub fn enter_animation(title: &str) {
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_some() {
            return;
        }
        let event_loop = match EventLoop::<()>::with_user_event().build() {
            Ok(el) => el,
            Err(e) => {
                eprintln!("[sinteract] failed to create window event loop: {e}");
                return;
            }
        };
        event_loop.set_control_flow(ControlFlow::Wait);
        let app = App::new(title.to_string());
        *slot = Some(State { event_loop, app });
    });
}

/// Destroy the window. A second call does nothing.
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

/// Return the next queued key event, or `None`. Pumps the event loop first,
/// so an event that just arrived is in the queue.
pub fn poll_key_event() -> Option<crate::event::KeyEvent> {
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

/// Returns `true` if the user closed the window since [`enter_animation`],
/// `false` otherwise.
pub fn closed() -> bool {
    STATE.with(|cell| cell.borrow().as_ref().map(|s| s.app.closed).unwrap_or(true))
}

/// Rasterize `scene` and present it. Pumps the event loop first, so a resize
/// or a DPI change applies to this frame, and opens the window on the first
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
    // `resumed` reads the size on this pump.
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
        // The scene fills the window, so there is no cap on the scale.
        let scale = crate::renderer::pixmap::fit_scale(scene.width, scene.height, target_px);
        let pixmap = match crate::renderer::pixmap::rasterize_scene(scene, scale) {
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

/// Copy `pixmap` into a softbuffer `0RGB` buffer, centered. The fit keeps the
/// aspect ratio, so one axis may leave a band, and the band is black.
fn blit_pixmap(pixmap: &Pixmap, buffer: &mut [u32], (bw, bh): (u32, u32)) {
    let pw = pixmap.width();
    let ph = pixmap.height();

    let off_x = (bw.saturating_sub(pw)) / 2;
    let off_y = (bh.saturating_sub(ph)) / 2;

    buffer.fill(0);

    let src = pixmap.pixels();
    for y in 0..ph.min(bh - off_y.min(bh)) {
        let dst_row_start = ((off_y + y) as usize) * (bw as usize) + off_x as usize;
        let src_row_start = (y as usize) * (pw as usize);
        let row_w = pw.min(bw - off_x.min(bw)) as usize;
        for x in 0..row_w {
            // softbuffer takes 0RGB, and the window has no transparency.
            let (r, g, b) = super::pixel::unpremultiply(src[src_row_start + x]);
            buffer[dst_row_start + x] = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
        }
    }
}

/// Does nothing. A window has no terminal state to restore after a panic,
/// and the OS reclaims the window. Exists so a host installs the hook of
/// either backend the same way.
pub fn install_panic_hook() {}
