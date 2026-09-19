//! Window display of a [`crate::scene::Scene`] through winit and softbuffer.
//!
//! The window opens with the logical size of the scene, and every present
//! rasterizes at the physical size of the surface, letterboxed, so HiDPI and
//! a resize keep the aspect ratio.
//!
//! winit builds one event loop per process, and the loop cannot move to
//! another thread. So the loop outlives the [`Window`]. A window borrows it
//! at open and gives it back at close, and every window of the process opens
//! on the thread of the first one. Open windows from a thread that lives as
//! long as the process, such as the main thread.

use std::cell::{Cell, RefCell};
use std::mem;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use softbuffer::{Context, Surface};
use tiny_skia::Pixmap;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::pump_events::{EventLoopExtPumpEvents, PumpStatus};
use winit::window::{Window as WinitWindow, WindowAttributes, WindowId};

use super::driver::{OpenError, period_from_hz, sealed, warn_bitmaps_once};
use super::inbox::{Inbox, Sender, Wait};
use crate::event::{Event, InputEvent, KeyKind, key};
use crate::renderer::Renderer;
use crate::renderer::pixmap::{PixmapRenderer, fit_scale, frame_px};
use crate::scene::Scene;

/// A [`super::Display`] over a winit window. Closing the window arrives
/// as [`InputEvent::Close`], and the window stays until
/// [`super::Display::close`].
pub struct Window {
    inbox: Inbox,
    /// `None` after [`super::Display::close`].
    session: Option<Session>,
    warned_bitmaps: bool,
}

struct Session {
    lent: Lent,
    app: App,
    window: Rc<WinitWindow>,
    surface: Surface<Rc<WinitWindow>, Rc<WinitWindow>>,
    /// Kept across frames, so a frame reuses the pixmap and the clip masks.
    renderer: PixmapRenderer,
    /// The scene of the last present, drawn again when the platform asks.
    last: Option<Scene>,
}

impl Window {
    /// The window paints through softbuffer without a swap chain, so its
    /// cadence is software-timed too.
    const VSYNC_PERIOD: Duration = period_from_hz(60);

    /// How long [`Window::open`] waits for the platform to create the window.
    const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

    /// Open a window of `width` by `height` logical pixels. Fails with
    /// [`OpenError::Busy`] while another `Window` exists, and with
    /// [`OpenError::Platform`] when the platform has no window for us, or on
    /// a thread other than the one of the first window.
    pub fn open(title: &str, width: f32, height: f32) -> Result<Self, OpenError> {
        let mut lent = Lent::take()?;
        let proxy = lent.event_loop().create_proxy();
        let inbox = Inbox::with_waker(
            Some(Self::VSYNC_PERIOD),
            Some(Arc::new(move || {
                let _ = proxy.send_event(());
            })),
        );
        let (w, h) = frame_px(width, height);
        let attrs = WindowAttributes::default()
            .with_title(title)
            .with_inner_size(LogicalSize::new(w as f64, h as f64));
        let mut app = App::new(inbox.sender(), attrs);
        let window = lent.create_window(&mut app, Self::OPEN_TIMEOUT)?;
        let surface = match new_surface(&window) {
            Ok(surface) => surface,
            Err(e) => {
                drop(window);
                lent.pump(&mut app, Some(Duration::ZERO));
                return Err(e);
            }
        };
        Ok(Self {
            inbox,
            session: Some(Session {
                lent,
                app,
                window,
                surface,
                renderer: PixmapRenderer::default(),
                last: None,
            }),
            warned_bitmaps: false,
        })
    }
}

impl super::Display for Window {
    fn present(&mut self, scene: &Scene) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        warn_bitmaps_once(&mut self.warned_bitmaps, scene, "window");
        session.draw(scene);
        session.last = Some(scene.clone());
    }

    /// Block in the event loop of the window, which the [`Sender`]s wake.
    fn wait_event(&mut self, deadline: Option<Instant>) -> Event {
        loop {
            let mut session = self.session.as_mut();
            let wait = self.inbox.wait_with(deadline, |_, timeout| {
                // A closed session has a closed inbox, which returns Close
                // before it blocks.
                let Some(s) = session.as_mut() else {
                    return;
                };
                if !s.lent.pump(&mut s.app, timeout) {
                    let _ = s.app.tx.send_input(InputEvent::Close);
                }
            });
            match wait {
                Wait::Event(event) => return event,
                Wait::Redraw => {
                    if let Some(s) = self.session.as_mut() {
                        s.redraw();
                    }
                }
            }
        }
    }

    fn sender(&self) -> Sender {
        self.inbox.sender()
    }

    /// The window draws without bitmaps, so it drops the upload.
    fn push_asset(&mut self, _id: u32, _blob: &[u8], _mime: Option<&str>) {}

    /// Destroy the window and give the event loop back.
    fn close(&mut self) {
        let Some(Session {
            mut lent,
            mut app,
            window,
            surface,
            ..
        }) = self.session.take()
        else {
            return;
        };
        self.inbox.close();
        drop(surface);
        drop(window);
        // Wayland, X11 and Windows destroy a window as the loop runs.
        lent.pump(&mut app, Some(Duration::ZERO));
    }
}

impl sealed::Sealed for Window {}

impl Drop for Window {
    fn drop(&mut self) {
        super::Display::close(self);
    }
}

impl Session {
    /// Rasterize `scene` at the size of the surface and present it.
    fn draw(&mut self, scene: &Scene) {
        let inner = self.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(inner.width), NonZeroU32::new(inner.height))
        else {
            return;
        };
        if self.surface.resize(w, h).is_err() {
            return;
        }
        let target_px = (w.get(), h.get());
        // The scene fills the window, so there is no cap on the scale.
        self.renderer
            .set_scale(fit_scale(scene.width(), scene.height(), target_px));
        let Ok(pixmap) = self.renderer.render(scene) else {
            return;
        };
        let Ok(mut buffer) = self.surface.buffer_mut() else {
            return;
        };
        blit_pixmap(pixmap, &mut buffer, target_px);
        let _ = buffer.present();
    }

    /// Draw the last scene again, at the current size of the window.
    fn redraw(&mut self) {
        if let Some(scene) = self.last.take() {
            self.draw(&scene);
            self.last = Some(scene);
        }
    }
}

fn new_surface(
    window: &Rc<WinitWindow>,
) -> Result<Surface<Rc<WinitWindow>, Rc<WinitWindow>>, OpenError> {
    let context = Context::new(window.clone()).map_err(platform_error)?;
    Surface::new(&context, window.clone()).map_err(platform_error)
}

fn platform_error(e: impl std::fmt::Display) -> OpenError {
    OpenError::Platform(e.to_string())
}

// -----------------------------------------------------------------------------
// The event loop of the process
// -----------------------------------------------------------------------------

/// The thread that built the event loop, or why the build failed. winit
/// refuses a second build in the process, so the answer holds for good.
static LOOP_BUILT: OnceLock<Result<ThreadId, String>> = OnceLock::new();

thread_local! {
    /// The event loop while no window holds it.
    static PARKED: RefCell<Parked> = const { RefCell::new(Parked(None)) };
    /// The platform ended the loop, or a pump unwound, and no window opens
    /// again.
    static LOOP_DEAD: Cell<bool> = const { Cell::new(false) };
}

struct Parked(Option<EventLoop<()>>);

impl Drop for Parked {
    /// The thread ends. Some platforms tear down under a dropped loop at
    /// that point, and the process ends soon anyway.
    fn drop(&mut self) {
        if let Some(event_loop) = self.0.take() {
            mem::forget(event_loop);
        }
    }
}

/// The event loop on loan to a [`Window`]. Drop gives it back, unless it
/// died, in which case it is forgotten.
struct Lent {
    event_loop: Option<EventLoop<()>>,
    dead: bool,
}

impl Lent {
    fn take() -> Result<Self, OpenError> {
        let mut built = None;
        let owner = LOOP_BUILT.get_or_init(|| match build_loop() {
            Ok(event_loop) => {
                built = Some(event_loop);
                Ok(thread::current().id())
            }
            Err(e) => Err(e),
        });
        if let Some(event_loop) = built {
            return Ok(Self::of(event_loop));
        }
        match owner {
            Err(e) => Err(OpenError::Platform(e.clone())),
            Ok(id) if *id != thread::current().id() => Err(OpenError::Platform(
                "a window opens only on the thread of the first window".into(),
            )),
            Ok(_) if LOOP_DEAD.get() => Err(OpenError::Platform(
                "the window event loop ended, and it cannot start again".into(),
            )),
            Ok(_) => PARKED
                .with_borrow_mut(|p| p.0.take())
                .map(Self::of)
                .ok_or(OpenError::Busy),
        }
    }

    fn of(event_loop: EventLoop<()>) -> Self {
        Self {
            event_loop: Some(event_loop),
            dead: false,
        }
    }

    fn event_loop(&self) -> &EventLoop<()> {
        self.event_loop
            .as_ref()
            .expect("the loop leaves only on drop")
    }

    /// Run the loop until an event or `timeout`. Returns `true` if the loop
    /// still runs, `false` if the platform ended it.
    ///
    /// The loop never calls `ActiveEventLoop::exit`, because `pump` does
    /// not clear the flag and the next window would find it set.
    fn pump(&mut self, app: &mut App, timeout: Option<Duration>) -> bool {
        if self.dead {
            return false;
        }
        let event_loop = self
            .event_loop
            .as_mut()
            .expect("the loop leaves only on drop");
        if let PumpStatus::Exit(_) = event_loop.pump_app_events(timeout, app) {
            self.dead = true;
        }
        !self.dead
    }

    /// Pump until `app` has its window. The window appears in the callbacks,
    /// so a failure reaches the caller as an error and not as a late Close.
    fn create_window(
        &mut self,
        app: &mut App,
        timeout: Duration,
    ) -> Result<Rc<WinitWindow>, OpenError> {
        let deadline = Instant::now() + timeout;
        loop {
            if !self.pump(app, Some(Duration::from_millis(16))) {
                return Err(OpenError::Platform(
                    "the window event loop ended before the window opened".into(),
                ));
            }
            match app.created.take() {
                Some(Ok(window)) => return Ok(window),
                Some(Err(e)) => return Err(OpenError::Platform(e)),
                None if Instant::now() >= deadline => {
                    return Err(OpenError::Platform(
                        "the window did not open in time".into(),
                    ));
                }
                None => {}
            }
        }
    }
}

impl Drop for Lent {
    /// A loop that died, or that unwound in a pump, is in no state to run
    /// again.
    fn drop(&mut self) {
        let Some(event_loop) = self.event_loop.take() else {
            return;
        };
        if self.dead || thread::panicking() {
            mem::forget(event_loop);
            LOOP_DEAD.set(true);
        } else {
            PARKED.with_borrow_mut(|p| p.0 = Some(event_loop));
        }
    }
}

/// Build the loop of the process on this thread. winit panics on a thread
/// other than the main one unless told otherwise, so Linux and Windows allow
/// any thread, and macOS, which cannot, gets an error.
fn build_loop() -> Result<EventLoop<()>, String> {
    let mut builder = EventLoop::<()>::with_user_event();
    #[cfg(all(unix, not(target_vendor = "apple"), not(target_os = "android")))]
    {
        use winit::platform::x11::EventLoopBuilderExtX11;
        builder.with_any_thread(true);
    }
    #[cfg(windows)]
    {
        use winit::platform::windows::EventLoopBuilderExtWindows;
        builder.with_any_thread(true);
    }
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::EventLoopBuilderExtMacOS;
        if unsafe { libc::pthread_main_np() } != 1 {
            return Err("on macOS a window opens only on the main thread".into());
        }
        // The default menu quits the process on Cmd+Q, and a quit is the
        // decision of the engine.
        builder.with_default_menu(false);
    }
    let event_loop = builder
        .build()
        .map_err(|e| format!("the window event loop did not build: {e}"))?;
    event_loop.set_control_flow(ControlFlow::Wait);
    Ok(event_loop)
}

// -----------------------------------------------------------------------------
// Callbacks of the event loop
// -----------------------------------------------------------------------------

struct App {
    tx: Sender,
    /// What the next callback creates, until it does.
    to_create: Option<WindowAttributes>,
    created: Option<Result<Rc<WinitWindow>, String>>,
    /// The window of this session. An event of a window of an earlier
    /// session can still be in the loop.
    id: Option<WindowId>,
    modifiers: ModifiersState,
}

impl App {
    fn new(tx: Sender, attrs: WindowAttributes) -> Self {
        Self {
            tx,
            to_create: Some(attrs),
            created: None,
            id: None,
            modifiers: ModifiersState::empty(),
        }
    }

    fn create(&mut self, event_loop: &ActiveEventLoop) {
        let Some(attrs) = self.to_create.take() else {
            return;
        };
        let created = match event_loop.create_window(attrs) {
            Ok(window) => {
                self.id = Some(window.id());
                Ok(Rc::new(window))
            }
            Err(e) => Err(e.to_string()),
        };
        self.created = Some(created);
    }
}

impl ApplicationHandler for App {
    /// Only the first pump of the loop resumes.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.create(event_loop);
    }

    /// Every pump ends here, on every platform, so a window of a later
    /// session appears here.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.create(event_loop);
    }

    fn window_event(&mut self, _: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if self.id != Some(id) {
            return;
        }
        match event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                let _ = self.tx.send_input(InputEvent::Close);
            }
            WindowEvent::Resized(_)
            | WindowEvent::ScaleFactorChanged { .. }
            | WindowEvent::RedrawRequested => {
                let _ = self.tx.request_redraw();
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                send_key_events(&self.tx, &event, self.modifiers);
            }
            _ => {}
        }
    }
}

/// Send the events of a winit key event. A press gives `Down` and `Press`,
/// the first one and each repeat alike, as a browser does, and a release
/// gives `Up`. `repeat` tells a repeat from the first press.
fn send_key_events(tx: &Sender, ev: &KeyEvent, mods: ModifiersState) {
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
    let send = |kind, key| {
        let _ = tx.send_input(InputEvent::Key(event(kind, key)));
    };
    match ev.state {
        ElementState::Pressed => {
            send(KeyKind::Down, key.clone());
            send(KeyKind::Press, key);
        }
        ElementState::Released => send(KeyKind::Up, key),
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
            // softbuffer takes 0RGB. A premultiplied pixel is already the
            // pixel over black, the color of the band.
            let p = src[src_row_start + x];
            buffer[dst_row_start + x] =
                (u32::from(p.red()) << 16) | (u32::from(p.green()) << 8) | u32::from(p.blue());
        }
    }
}

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

    #[test]
    fn blit_centers_the_pixmap_over_black() {
        let mut pixmap = Pixmap::new(2, 1).unwrap();
        let opaque = tiny_skia::ColorU8::from_rgba(255, 0, 0, 255).premultiply();
        let half = tiny_skia::ColorU8::from_rgba(0, 255, 0, 128).premultiply();
        pixmap.pixels_mut().copy_from_slice(&[opaque, half]);
        let mut buffer = [0xFFFF_FFFF; 4 * 3];
        blit_pixmap(&pixmap, &mut buffer, (4, 3));
        #[rustfmt::skip]
        let expected = [
            0, 0, 0, 0,
            0, 0xFF_0000, 0x00_8000, 0,
            0, 0, 0, 0,
        ];
        assert_eq!(buffer, expected);
    }
}
