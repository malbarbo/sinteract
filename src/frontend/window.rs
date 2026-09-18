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
use std::time::{Duration, Instant};

use softbuffer::{Context, Surface};
use tiny_skia::Pixmap;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::pump_events::EventLoopExtPumpEvents;
use winit::window::{Window as WinitWindow, WindowAttributes, WindowId};

use super::driver::{period_from_hz, sealed, warn_bitmaps_once};
use super::inbox::{Inbox, Sender};
use crate::event::{Event, InputEvent, KeyKind, key};
use crate::scene::Scene;

/// A [`super::Frontend`] over a winit window. Closing the window arrives
/// as [`InputEvent::Close`].
pub struct Window {
    inbox: Inbox,
    tx: Sender,
    close_sent: bool,
    closed: bool,
    warned_bitmaps: bool,
}

impl Window {
    /// The window paints through softbuffer without a swap chain, so its
    /// cadence is software-timed too.
    const VSYNC_PERIOD: Duration = period_from_hz(60);

    /// Set the window up. It opens on the first `present`, with the size of
    /// the scene.
    pub fn open(title: &str) -> Self {
        enter_animation(title);
        let inbox = Inbox::new(Some(Self::VSYNC_PERIOD));
        let tx = inbox.sender();
        Self {
            inbox,
            tx,
            close_sent: false,
            closed: false,
            warned_bitmaps: false,
        }
    }
}

impl super::Frontend for Window {
    fn present(&mut self, scene: &Scene) {
        if self.closed {
            return;
        }
        warn_bitmaps_once(&mut self.warned_bitmaps, scene, "window");
        show_image(scene);
    }

    fn wait_event(&mut self, deadline: Option<Instant>) -> Event {
        let Self {
            inbox,
            tx,
            close_sent,
            ..
        } = self;
        poll_input(inbox, deadline, || forward_input(tx, close_sent))
    }

    fn sender(&self) -> Sender {
        self.inbox.sender()
    }

    /// The window draws without bitmaps, so it drops the upload.
    fn push_asset(&mut self, _id: u32, _blob: &[u8], _mime: Option<&str>) {}

    /// Destroy the window.
    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.inbox.close();
        exit_animation();
    }
}

impl sealed::Sealed for Window {}

impl Drop for Window {
    fn drop(&mut self) {
        super::Frontend::close(self);
    }
}

/// Move the keys and a closed window into the queue. `close_sent` keeps a
/// second Close out.
fn forward_input(tx: &Sender, close_sent: &mut bool) {
    while let Some(key) = poll_key_event() {
        let _ = tx.send_input(InputEvent::Key(key));
    }
    if !*close_sent && closed() {
        *close_sent = true;
        let _ = tx.send_input(InputEvent::Close);
    }
}

/// How often the window looks for a key. Its input does not wake the
/// queue, so [`poll_input`] checks this often.
const INPUT_POLL: Duration = Duration::from_millis(8);

/// Wait on `inbox` in steps of [`INPUT_POLL`], and call `forward` before
/// each step so it moves the input of the platform into the queue.
fn poll_input(inbox: &mut Inbox, deadline: Option<Instant>, mut forward: impl FnMut()) -> Event {
    loop {
        forward();
        let step = Instant::now() + INPUT_POLL;
        let until = deadline.map_or(step, |d| d.min(step));
        match inbox.wait(Some(until)) {
            Event::Timeout if deadline.is_none_or(|d| Instant::now() < d) => {}
            event => return event,
        }
    }
}

struct App {
    title: String,
    /// Size of the window to create, from the first `show_image`. `resumed`
    /// reads it.
    pending_size: Option<(u32, u32)>,
    window: Option<Rc<WinitWindow>>,
    surface: Option<Surface<Rc<WinitWindow>, Rc<WinitWindow>>>,
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

/// Push the events of a winit key event. A press gives `Down` and `Press`,
/// the first one and each repeat alike, as a browser does, and a release
/// gives `Up`. `repeat` tells a repeat from the first press.
fn push_key_events(
    out: &mut VecDeque<crate::event::KeyEvent>,
    ev: &KeyEvent,
    mods: ModifiersState,
) {
    let Some(key) = winit_key_to_string(&ev.logical_key) else {
        return;
    };
    let modifiers = crate::event::Modifiers {
        alt: mods.alt_key(),
        ctrl: mods.control_key(),
        shift: mods.shift_key(),
        meta: mods.super_key(),
    };
    let event = |kind, key| crate::event::KeyEvent {
        kind,
        key,
        modifiers,
        repeat: ev.repeat,
    };
    match ev.state {
        ElementState::Pressed => {
            out.push_back(event(KeyKind::Down, key.clone()));
            out.push_back(event(KeyKind::Press, key));
        }
        ElementState::Released => {
            out.push_back(event(KeyKind::Up, key));
        }
    }
}

/// Map a winit key to its name in [`crate::event::key`], or to the text it
/// types.
fn winit_key_to_string(key: &Key) -> Option<String> {
    Some(match key {
        Key::Character(s) => s.to_string(),
        Key::Named(named) => match named {
            NamedKey::Backspace => key::BACKSPACE.into(),
            NamedKey::Enter => key::ENTER.into(),
            NamedKey::Tab => key::TAB.into(),
            NamedKey::Space => " ".into(),
            NamedKey::ArrowLeft => key::ARROW_LEFT.into(),
            NamedKey::ArrowRight => key::ARROW_RIGHT.into(),
            NamedKey::ArrowUp => key::ARROW_UP.into(),
            NamedKey::ArrowDown => key::ARROW_DOWN.into(),
            NamedKey::Home => key::HOME.into(),
            NamedKey::End => key::END.into(),
            NamedKey::PageUp => key::PAGE_UP.into(),
            NamedKey::PageDown => key::PAGE_DOWN.into(),
            NamedKey::Delete => key::DELETE.into(),
            NamedKey::Insert => key::INSERT.into(),
            NamedKey::Escape => key::ESCAPE.into(),
            NamedKey::F1 => key::F1.into(),
            NamedKey::F2 => key::F2.into(),
            NamedKey::F3 => key::F3.into(),
            NamedKey::F4 => key::F4.into(),
            NamedKey::F5 => key::F5.into(),
            NamedKey::F6 => key::F6.into(),
            NamedKey::F7 => key::F7.into(),
            NamedKey::F8 => key::F8.into(),
            NamedKey::F9 => key::F9.into(),
            NamedKey::F10 => key::F10.into(),
            NamedKey::F11 => key::F11.into(),
            NamedKey::F12 => key::F12.into(),
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
    let dl_size = crate::renderer::pixmap::frame_px(scene.width(), scene.height());
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
        let scale = crate::renderer::pixmap::fit_scale(scene.width(), scene.height(), target_px);
        let pixmap = match crate::renderer::pixmap::render_to_pixmap(scene, scale) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_name_of_the_window_is_in_key_all() {
        let named = [
            NamedKey::Backspace,
            NamedKey::Enter,
            NamedKey::Tab,
            NamedKey::ArrowLeft,
            NamedKey::ArrowRight,
            NamedKey::ArrowUp,
            NamedKey::ArrowDown,
            NamedKey::Home,
            NamedKey::End,
            NamedKey::PageUp,
            NamedKey::PageDown,
            NamedKey::Delete,
            NamedKey::Insert,
            NamedKey::Escape,
            NamedKey::F1,
            NamedKey::F2,
            NamedKey::F3,
            NamedKey::F4,
            NamedKey::F5,
            NamedKey::F6,
            NamedKey::F7,
            NamedKey::F8,
            NamedKey::F9,
            NamedKey::F10,
            NamedKey::F11,
            NamedKey::F12,
        ];
        for k in named {
            let name = winit_key_to_string(&Key::Named(k)).expect("named");
            assert!(key::ALL.contains(&name.as_str()), "{name}");
        }
    }
}
