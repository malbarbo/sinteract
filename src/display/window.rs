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

use std::cell::RefCell;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use softbuffer::{Context, Surface};
use tiny_skia::Pixmap;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::error::EventLoopError;
use winit::event::{ElementState, KeyEvent, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey, PhysicalKey};
use winit::platform::pump_events::{EventLoopExtPumpEvents, PumpStatus};
use winit::window::{Window as WinitWindow, WindowAttributes, WindowId};

use super::driver::{OpenError, PresentError, sealed};
use super::inbox::{Inbox, Next, Sender};
use super::tick_clock::TickClock;
use crate::event::{
    Event, Interrupt, KeyKind, Modifiers, MouseAction, MouseButton, MouseButtons, MouseEvent, key,
};
use crate::renderer::Renderer;
use crate::renderer::pixmap::{PixmapRenderer, fit_scale, frame_px};
use crate::scene::Scene;

/// A [`super::Display`] over a winit window. Closing the window arrives
/// as [`Interrupt::Close`], and the window stays until
/// [`super::Display::close`]. The size of the window arrives as an
/// [`InputEvent::Resize`](crate::event::InputEvent::Resize) ahead of the
/// first tick, and again after each change.
pub struct Window {
    inbox: Inbox,
    clock: TickClock,
    /// `None` after [`super::Display::close`].
    active: Option<Active>,
}

struct Active {
    lent: Lent,
    app: App,
    window: Rc<WinitWindow>,
    surface: Surface<Rc<WinitWindow>, Rc<WinitWindow>>,
    /// Kept across frames, so a frame reuses the pixmap and the clip masks.
    renderer: PixmapRenderer,
    /// The scene of the last present, drawn again when the platform asks.
    last: Option<Last>,
    #[cfg(target_os = "macos")]
    display_link: super::display_link::DisplayLink,
}

impl Window {
    /// The rate of the ticks on a monitor that gives no refresh rate.
    const TICK_RATE: NonZeroU32 = NonZeroU32::new(60_000).expect("60 Hz is not zero");

    /// How long [`Window::open`] waits for the platform to create the window.
    const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

    /// Open a window of `width` by `height` logical pixels. Fails with
    /// [`OpenError::Busy`] while another `Window` exists, and with
    /// [`OpenError::Platform`] when the platform has no window for us, or on
    /// a thread other than the one of the first window.
    pub fn open(title: &str, width: f32, height: f32) -> Result<Self, OpenError> {
        let mut lent = Lent::take()?;
        let proxy = lent.proxy.clone();
        let inbox = Inbox::new(Some(Arc::new(move || {
            let _ = proxy.send_event(());
        })));
        let (w, h) = frame_px(width, height);
        let attrs = WindowAttributes::default()
            .with_title(title)
            .with_inner_size(LogicalSize::new(w as f64, h as f64));
        let (window, mut app) =
            lent.create_window(inbox.sender_without_waker(), attrs, Self::OPEN_TIMEOUT)?;
        let surface = match new_surface(&window) {
            Ok(surface) => surface,
            Err(e) => {
                drop(window);
                lent.destroy_dropped(&mut app);
                return Err(e);
            }
        };
        #[cfg(target_os = "macos")]
        let display_link = super::display_link::DisplayLink::start(&window, lent.proxy.clone());
        Ok(Self {
            inbox,
            clock: TickClock::from_millihertz(Self::TICK_RATE),
            active: Some(Active {
                lent,
                app,
                window,
                surface,
                renderer: PixmapRenderer::default(),
                last: None,
                #[cfg(target_os = "macos")]
                display_link,
            }),
        })
    }

    /// Run the loop of the window for at most `timeout`, pace the ticks
    /// at the refresh rate of the monitor that shows the window, align the
    /// ticks to a frame callback of Wayland or a frame of the display link
    /// of macOS, and draw a scene that waited for the callback.
    fn pump(&mut self, timeout: Duration) {
        let Some(s) = self.active.as_mut() else {
            return;
        };
        if !s.lent.pump(&mut s.app, timeout) {
            let _ = s.app.tx.send_close();
        }
        // Wayland puts the window on a monitor only after its first frame,
        // so the rate stays to read until a monitor shows the window.
        if s.app.refresh_rate_stale
            && let Some(monitor) = s.window.current_monitor()
        {
            s.app.refresh_rate_stale = false;
            let rate = monitor.refresh_rate_millihertz().and_then(NonZeroU32::new);
            self.clock.set_millihertz(rate.unwrap_or(Self::TICK_RATE));
        }
        let shown = s.app.frame_callback.as_mut().and_then(|f| f.shown.take());
        #[cfg(target_os = "macos")]
        let shown = shown.or_else(|| s.display_link.take());
        if let Some(at) = shown {
            self.clock.align(at);
        }
        if s.last.as_ref().is_some_and(|l| l.unshown) && !s.app.frame_pending() {
            // A failure here fails the next present the same way.
            let _ = s.show();
        }
    }
}

impl super::Display for Window {
    fn present(&mut self, scene: Scene) -> Result<(), PresentError> {
        let Some(active) = self.active.as_mut() else {
            return Err(PresentError::Closed);
        };
        self.inbox.take_redraw();
        active.last = Some(Last {
            scene,
            unshown: false,
        });
        active.show()
    }

    /// Block in the event loop of the window, which the [`Sender`]s wake.
    fn wait_event(&mut self, deadline: Option<Instant>) -> Result<Event, Interrupt> {
        // A frame that draws for longer than the period finds the next tick
        // due, so the wait returns it without a pump, and the platform takes
        // a window that never answers for hung. So every call pumps once.
        self.pump(Duration::ZERO);
        loop {
            // A closed session has a closed inbox, which returns Close
            // before it redraws or blocks.
            match self.inbox.next(&mut self.clock, deadline) {
                Next::Ready(ready) => return ready,
                Next::Redraw => {
                    // A failure here fails the next present the same way.
                    if let Some(s) = self.active.as_mut() {
                        let _ = s.show();
                    }
                }
                Next::Block(timeout) => self.pump(timeout),
            }
        }
    }

    fn sender(&self) -> Sender {
        self.inbox.sender()
    }

    /// Destroy the window and give the event loop back.
    fn close(&mut self) {
        let Some(Active {
            mut lent,
            mut app,
            window,
            surface,
            #[cfg(target_os = "macos")]
            display_link,
            ..
        }) = self.active.take()
        else {
            return;
        };
        self.inbox.close();
        #[cfg(target_os = "macos")]
        drop(display_link);
        drop(surface);
        drop(window);
        lent.destroy_dropped(&mut app);
    }
}

impl sealed::Sealed for Window {}

impl Drop for Window {
    fn drop(&mut self) {
        super::Display::close(self);
    }
}

impl Active {
    /// Draw `last` now, or when the frame callback of Wayland for the
    /// frame before arrives. A hidden window gets no callback, so it draws
    /// nothing, and the ticks keep their rate.
    fn show(&mut self) -> Result<(), PresentError> {
        let Some(mut last) = self.last.take() else {
            return Ok(());
        };
        // A second frame would block in `buffer_mut` until the compositor
        // releases a buffer.
        last.unshown = self.app.frame_pending();
        let drawn = if last.unshown {
            Ok(())
        } else {
            self.draw(&last.scene)
        };
        self.last = Some(last);
        drawn
    }

    /// Rasterize `scene` at the size of the surface and present it.
    fn draw(&mut self, scene: &Scene) -> Result<(), PresentError> {
        // The size from the last Resized. X11 answers inner_size with a
        // round trip to the server. A newer size comes with a redraw.
        let inner = self.app.size;
        // A window of no pixels is minimized, and the platform asks for a
        // redraw when it comes back.
        let (Some(w), Some(h)) = (NonZeroU32::new(inner.width), NonZeroU32::new(inner.height))
        else {
            return Ok(());
        };
        self.surface.resize(w, h).map_err(surface_error)?;
        let target_px = (w.get(), h.get());
        // The scene fills the window, so there is no cap on the scale.
        let scale = fit_scale(scene.width(), scene.height(), target_px);
        self.renderer.set_scale(scale);
        let pixmap = self.renderer.render(scene)?;
        let mut buffer = self.surface.buffer_mut().map_err(surface_error)?;
        let viewport = Viewport::centered(scale, pixmap, target_px);
        blit_pixmap(pixmap, &mut buffer, (w, h), viewport.offset);
        self.app.viewport = viewport;
        self.window.pre_present_notify();
        buffer.present().map_err(surface_error)?;
        if let Some(callback) = &mut self.app.frame_callback {
            // The RedrawRequested of the callback clears it.
            self.window.request_redraw();
            callback.pending = true;
        }
        Ok(())
    }
}

struct Last {
    scene: Scene,
    /// `scene` waits for the frame callback of Wayland.
    unshown: bool,
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

fn surface_error(e: impl std::fmt::Display) -> PresentError {
    PresentError::Platform(e.to_string())
}

// -----------------------------------------------------------------------------
// The event loop of the process
// -----------------------------------------------------------------------------

thread_local! {
    /// The event loop of the process, on the thread that built it. winit
    /// refuses a second build, so the loop stays for every later window.
    static LOOP: RefCell<Loop> = const { RefCell::new(Loop::Unbuilt) };
}

enum Loop {
    Unbuilt,
    /// The build failed, and winit refuses a second one.
    Failed(String),
    /// The loop is leaked, because some platforms tear down under a dropped
    /// loop.
    Ready {
        event_loop: &'static mut EventLoop<()>,
        /// A [`Lent`] holds the loop.
        held: bool,
    },
    /// The platform ended the loop, or a pump unwound, and no window opens
    /// again.
    Dead,
}

/// The event loop on loan to a [`Window`]. Drop gives it back.
struct Lent {
    proxy: EventLoopProxy<()>,
}

impl Lent {
    fn take() -> Result<Self, OpenError> {
        LOOP.with_borrow_mut(|state| match state {
            Loop::Unbuilt => match build_loop() {
                Ok(event_loop) => {
                    let proxy = event_loop.create_proxy();
                    *state = Loop::Ready {
                        event_loop: Box::leak(Box::new(event_loop)),
                        held: true,
                    };
                    Ok(Self { proxy })
                }
                Err(OpenError::Platform(e)) => {
                    *state = Loop::Failed(e.clone());
                    Err(OpenError::Platform(e))
                }
                Err(e) => Err(e),
            },
            Loop::Failed(e) => Err(OpenError::Platform(e.clone())),
            Loop::Ready { held: true, .. } => Err(OpenError::Busy),
            Loop::Ready { event_loop, held } => {
                *held = true;
                Ok(Self {
                    proxy: event_loop.create_proxy(),
                })
            }
            Loop::Dead => Err(OpenError::LoopEnded),
        })
    }

    /// Run the loop until an event or `timeout`. Returns `true` if the loop
    /// still runs, `false` if the platform ended it.
    ///
    /// The loop never calls `ActiveEventLoop::exit`, because `pump` does
    /// not clear the flag and the next window would find it set.
    fn pump(&mut self, handler: &mut impl ApplicationHandler, timeout: Duration) -> bool {
        LOOP.with_borrow_mut(|state| {
            let Loop::Ready { event_loop, .. } = state else {
                return false;
            };
            if let PumpStatus::Exit(_) = event_loop.pump_app_events(Some(timeout), handler) {
                *state = Loop::Dead;
                return false;
            }
            true
        })
    }

    /// Run the loop until the platform destroys a dropped window. Wayland,
    /// X11 and Windows destroy it as the loop runs. Wayland queues the
    /// destroy in one pump and sends it in the flush that starts the next
    /// one, so it takes two.
    fn destroy_dropped(&mut self, app: &mut App) {
        for _ in 0..2 {
            self.pump(app, Duration::ZERO);
        }
    }

    /// Pump until the window of `attrs` exists, with the [`App`] of its
    /// events. The window appears in the callbacks, so a failure reaches the
    /// caller as an error and not as a late Close.
    fn create_window(
        &mut self,
        tx: Sender,
        attrs: WindowAttributes,
        timeout: Duration,
    ) -> Result<(Rc<WinitWindow>, App), OpenError> {
        let mut opener = Opener {
            tx,
            attrs,
            opened: None,
        };
        let deadline = Instant::now() + timeout;
        loop {
            if !self.pump(&mut opener, Duration::from_millis(16)) {
                return Err(OpenError::LoopEnded);
            }
            match opener.opened.take() {
                Some(Ok(opened)) => return Ok(opened),
                Some(Err(e)) => return Err(OpenError::Platform(e)),
                None if Instant::now() >= deadline => return Err(OpenError::Timeout),
                None => {}
            }
        }
    }
}

impl Drop for Lent {
    /// A loop that unwound in a pump is in no state to run again.
    fn drop(&mut self) {
        LOOP.with_borrow_mut(|state| {
            if thread::panicking() {
                *state = Loop::Dead;
            } else if let Loop::Ready { held, .. } = state {
                *held = false;
            }
        });
    }
}

/// Returns `true` if `event_loop` runs on Wayland, `false` otherwise.
fn is_wayland(event_loop: &ActiveEventLoop) -> bool {
    #[cfg(all(unix, not(target_vendor = "apple"), not(target_os = "android")))]
    {
        use winit::platform::wayland::ActiveEventLoopExtWayland;
        event_loop.is_wayland()
    }
    #[cfg(not(all(unix, not(target_vendor = "apple"), not(target_os = "android"))))]
    {
        let _ = event_loop;
        false
    }
}

/// Build the loop of the process on this thread. winit panics on a thread
/// other than the main one unless told otherwise, so Linux and Windows allow
/// any thread, and macOS, which cannot, gets an error. A second build means
/// that another thread built the loop.
fn build_loop() -> Result<EventLoop<()>, OpenError> {
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
            return Err(OpenError::Platform(
                "on macOS a window opens only on the main thread".into(),
            ));
        }
        // The default menu quits the process on Cmd+Q, and a quit is the
        // decision of the engine.
        builder.with_default_menu(false);
    }
    let event_loop = builder.build().map_err(|e| match e {
        EventLoopError::RecreationAttempt => OpenError::WrongThread,
        e => OpenError::Platform(format!("the window event loop did not build: {e}")),
    })?;
    event_loop.set_control_flow(ControlFlow::Wait);
    Ok(event_loop)
}

// -----------------------------------------------------------------------------
// Callbacks of the event loop
// -----------------------------------------------------------------------------

/// Creates a window in the first callback that can, and passes the events
/// that follow in the same pump to the [`App`] of the window.
struct Opener {
    tx: Sender,
    attrs: WindowAttributes,
    opened: Option<Result<(Rc<WinitWindow>, App), String>>,
}

impl Opener {
    fn open(&mut self, event_loop: &ActiveEventLoop) {
        if self.opened.is_some() {
            return;
        }
        self.opened = Some(match event_loop.create_window(self.attrs.clone()) {
            Ok(window) => {
                let app = App::new(self.tx.clone(), &window, is_wayland(event_loop));
                Ok((Rc::new(window), app))
            }
            Err(e) => Err(e.to_string()),
        });
    }
}

impl ApplicationHandler for Opener {
    /// Only the first pump of the loop resumes.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.open(event_loop);
    }

    /// Every pump ends here, on every platform, so the window of a later
    /// session appears here.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.open(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let Some(Ok((_, app))) = &mut self.opened {
            app.window_event(event_loop, id, event);
        }
    }
}

struct App {
    tx: Sender,
    /// The window of this session. An event of a window of an earlier
    /// session can still be in the loop.
    id: WindowId,
    modifiers: ModifiersState,
    /// Where the last frame sits in the window, to map the pointer.
    viewport: Viewport,
    /// The last position of the pointer, in the pixels of the window.
    cursor: PhysicalPosition<f64>,
    buttons: MouseButtons,
    /// Device pixels per logical pixel.
    scale_factor: f64,
    size: PhysicalSize<u32>,
    /// The size in the last Resize, in logical pixels.
    reported_size: (f32, f32),
    held: HeldKeys,
    /// The window may be on another monitor, whose refresh rate is still
    /// to read.
    refresh_rate_stale: bool,
    /// Wayland paces the frames by its frame callback. Other platforms
    /// have none.
    frame_callback: Option<FrameCallback>,
}

#[derive(Default)]
struct FrameCallback {
    /// The callback of the last frame did not arrive yet.
    pending: bool,
    /// When the last callback arrived, just after the screen showed the
    /// frame, for Window::pump to align the tick clock.
    shown: Option<Instant>,
}

impl App {
    /// Send the first event of `window`, a Resize, ahead of any from the
    /// pumps that create it.
    fn new(tx: Sender, window: &WinitWindow, frame_callbacks: bool) -> Self {
        let scale_factor = window.scale_factor();
        let size = window.inner_size();
        let reported_size = logical_size(size, scale_factor);
        let _ = tx.send_resize(reported_size.0, reported_size.1);
        Self {
            tx,
            id: window.id(),
            modifiers: ModifiersState::empty(),
            // Until the first frame, the pointer maps to logical pixels.
            viewport: Viewport {
                scale: scale_factor as f32,
                offset: (0, 0),
            },
            cursor: PhysicalPosition::default(),
            buttons: MouseButtons::default(),
            scale_factor,
            size,
            reported_size,
            held: HeldKeys::default(),
            refresh_rate_stale: true,
            frame_callback: frame_callbacks.then(FrameCallback::default),
        }
    }

    /// Returns `true` if the frame callback of the last frame did not arrive
    /// yet, `false` otherwise.
    fn frame_pending(&self) -> bool {
        self.frame_callback.as_ref().is_some_and(|f| f.pending)
    }

    /// Send a Resize when the logical size changed since the last one.
    fn report_size(&mut self) {
        let (width, height) = logical_size(self.size, self.scale_factor);
        if (width, height) != self.reported_size {
            self.reported_size = (width, height);
            let _ = self.tx.send_resize(width, height);
        }
    }

    /// Send the events of a winit key event. A press gives `Down` and
    /// `Press`, the first one and each repeat alike, as a browser does, and
    /// the release of a held key gives `Up`, with the name from its `Down`.
    fn report_key_events(&mut self, ev: &KeyEvent) {
        match ev.state {
            ElementState::Pressed => {
                let Some(key) = winit_key_to_string(&ev.logical_key) else {
                    return;
                };
                self.held.press(ev.physical_key, key.clone());
                self.report_key(KeyKind::Down, key.clone());
                self.report_key(KeyKind::Press, key);
            }
            ElementState::Released => {
                if let Some(key) = self.held.release(ev.physical_key) {
                    self.report_key(KeyKind::Up, key);
                }
            }
        }
    }

    fn report_key(&self, kind: KeyKind, key: String) {
        let _ = self.tx.send_key(crate::event::KeyEvent {
            kind,
            key,
            modifiers: modifiers(self.modifiers),
        });
    }

    fn report_mouse(&self, action: MouseAction) {
        let (x, y) = self.viewport.to_scene(self.cursor);
        let _ = self.tx.send_mouse(MouseEvent {
            action,
            x,
            y,
            modifiers: modifiers(self.modifiers),
            buttons: self.buttons,
        });
    }
}

impl ApplicationHandler for App {
    /// Only the first pump of the loop resumes, and the [`Opener`] takes it.
    fn resumed(&mut self, _: &ActiveEventLoop) {}

    fn window_event(&mut self, _: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if self.id != id {
            return;
        }
        match event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                let _ = self.tx.send_close();
            }
            WindowEvent::Moved(_) => self.refresh_rate_stale = true,
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor;
                self.refresh_rate_stale = true;
                self.report_size();
                let _ = self.tx.request_redraw();
            }
            WindowEvent::Resized(size) => {
                self.size = size;
                self.report_size();
                let _ = self.tx.request_redraw();
            }
            // The frame callback of Wayland arrives as a RedrawRequested,
            // and Window::pump aligns the tick clock to it and draws a
            // scene that waits for it. A redraw
            // there would draw again and ask for the next callback forever.
            // winit gives no time of the callback, so the stamp is the time
            // of the call, late after a frame that drew past the vblank.
            WindowEvent::RedrawRequested => match &mut self.frame_callback {
                Some(callback) if callback.pending => {
                    callback.pending = false;
                    callback.shown = Some(Instant::now());
                }
                _ => {
                    let _ = self.tx.request_redraw();
                }
            },
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = position;
                self.report_mouse(MouseAction::Move);
            }
            WindowEvent::CursorLeft { .. } => self.report_mouse(MouseAction::Leave),
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(button) = mouse_button(button) else {
                    return;
                };
                let action = match state {
                    ElementState::Pressed => {
                        self.buttons = self.buttons.with(button);
                        MouseAction::Down(button)
                    }
                    // The loss of focus released the button already.
                    ElementState::Released if !self.buttons.contains(button) => return,
                    ElementState::Released => {
                        self.buttons = self.buttons.without(button);
                        MouseAction::Up(button)
                    }
                };
                self.report_mouse(action);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = wheel_notches(delta, self.scale_factor);
                self.report_mouse(MouseAction::Wheel { dx, dy });
            }
            // On X11 and Windows, winit makes up a press for each key held
            // when the window gains focus. The user did not press it here.
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } if !(is_synthetic && event.state == ElementState::Pressed) => {
                self.report_key_events(&event);
            }
            // Wayland sends no release for the keys held when the window
            // loses focus, and takes them as released. A grab of the
            // pointer, as by the window switcher, can take the release of a
            // button too.
            WindowEvent::Focused(false) => {
                for key in self.held.release_all() {
                    self.report_key(KeyKind::Up, key);
                }
                for button in MouseButton::ALL {
                    if self.buttons.contains(button) {
                        self.buttons = self.buttons.without(button);
                        self.report_mouse(MouseAction::Up(button));
                    }
                }
            }
            _ => {}
        }
    }
}

fn logical_size(size: PhysicalSize<u32>, scale_factor: f64) -> (f32, f32) {
    let logical = size.to_logical::<f32>(scale_factor);
    (logical.width, logical.height)
}

/// The keys down in the window, each with the name from its `Down`. The
/// name of a key can change while it is down, as when Shift goes down, so
/// a release finds its key by the physical key.
#[derive(Default)]
struct HeldKeys(Vec<(PhysicalKey, String)>);

impl HeldKeys {
    fn press(&mut self, code: PhysicalKey, key: String) {
        if !self.0.iter().any(|(c, _)| *c == code) {
            self.0.push((code, key));
        }
    }

    /// The name of `code` if it was down, which drops a release that has no
    /// press, such as the late one that macOS sends after a focus loss.
    fn release(&mut self, code: PhysicalKey) -> Option<String> {
        let i = self.0.iter().position(|(c, _)| *c == code)?;
        Some(self.0.remove(i).1)
    }

    fn release_all(&mut self) -> Vec<String> {
        self.0.drain(..).map(|(_, key)| key).collect()
    }
}

fn modifiers(mods: ModifiersState) -> Modifiers {
    Modifiers {
        alt: mods.alt_key(),
        ctrl: mods.control_key(),
        shift: mods.shift_key(),
        meta: mods.super_key(),
    }
}

/// `None` for a button that has no W3C number.
fn mouse_button(button: winit::event::MouseButton) -> Option<MouseButton> {
    use winit::event::MouseButton as B;
    Some(match button {
        B::Left => MouseButton::Left,
        B::Middle => MouseButton::Middle,
        B::Right => MouseButton::Right,
        B::Back => MouseButton::Back,
        B::Forward => MouseButton::Forward,
        B::Other(_) => return None,
    })
}

/// A browser scrolls about this many logical pixels per notch of the
/// wheel, so a touchpad that reports pixels gives notches at that rate.
const PIXELS_PER_NOTCH: f64 = 100.0;

/// The wheel in notches, with the W3C sign. winit gives the opposite sign,
/// the way that the content moves. `0.0 - v` turns a still axis into 0.0,
/// where `-v` would give -0.0.
fn wheel_notches(delta: MouseScrollDelta, scale_factor: f64) -> (f32, f32) {
    let (x, y) = match delta {
        MouseScrollDelta::LineDelta(x, y) => (x, y),
        MouseScrollDelta::PixelDelta(p) => {
            let per_notch = PIXELS_PER_NOTCH * scale_factor;
            ((p.x / per_notch) as f32, (p.y / per_notch) as f32)
        }
    };
    (0.0 - x, 0.0 - y)
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

/// Where a frame sits in the window and at what scale, which maps a point
/// of the window to the scene.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Viewport {
    /// Device pixels per unit of the scene.
    scale: f32,
    /// The top-left corner of the frame, in device pixels.
    offset: (u32, u32),
}

impl Viewport {
    /// A frame of `pixmap` at `scale`, centered in `(bw, bh)`. The fit keeps
    /// the aspect ratio, so one axis may leave a band.
    fn centered(scale: f32, pixmap: &Pixmap, (bw, bh): (u32, u32)) -> Self {
        Self {
            scale,
            offset: (
                bw.saturating_sub(pixmap.width()) / 2,
                bh.saturating_sub(pixmap.height()) / 2,
            ),
        }
    }

    /// The point of the scene under `p`, a point of the window. A point on
    /// a band falls outside the scene.
    fn to_scene(self, p: PhysicalPosition<f64>) -> (f32, f32) {
        let scale = f64::from(self.scale);
        (
            ((p.x - f64::from(self.offset.0)) / scale) as f32,
            ((p.y - f64::from(self.offset.1)) / scale) as f32,
        )
    }
}

/// Copy `pixmap` into a softbuffer `0RGB` buffer of `bw` by `bh` pixels,
/// with its top-left corner at `(off_x, off_y)`. The band around it turns
/// black. A scene that fills the window leaves no band, and then nothing
/// is cleared.
fn blit_pixmap(
    pixmap: &Pixmap,
    buffer: &mut [u32],
    (bw, bh): (NonZeroU32, NonZeroU32),
    (off_x, off_y): (u32, u32),
) {
    assert_eq!(
        buffer.len(),
        bw.get() as usize * bh.get() as usize,
        "softbuffer gives a buffer of the size of the surface"
    );
    let bw = bw.get() as usize;
    let off_x = off_x as usize;
    let mut rows = buffer.chunks_mut(bw);
    for row in rows.by_ref().take(off_y as usize) {
        row.fill(0);
    }
    // A pixmap can come out a rounding pixel wider than the surface, so the
    // copy clips at the edge.
    for (src, dst) in pixmap
        .pixels()
        .chunks(pixmap.width() as usize)
        .zip(rows.by_ref())
    {
        let (left, rest) = dst.split_at_mut(off_x.min(dst.len()));
        let (mid, right) = rest.split_at_mut(src.len().min(rest.len()));
        left.fill(0);
        right.fill(0);
        for (d, p) in mid.iter_mut().zip(src) {
            // softbuffer takes 0RGB. A premultiplied pixel is already the
            // pixel over black, the color of the band.
            *d = (u32::from(p.red()) << 16) | (u32::from(p.green()) << 8) | u32::from(p.blue());
        }
    }
    for row in rows {
        row.fill(0);
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
    fn a_held_key_is_released_once_by_its_physical_key() {
        use winit::keyboard::KeyCode;
        let a = PhysicalKey::Code(KeyCode::KeyA);
        let b = PhysicalKey::Code(KeyCode::KeyB);
        let mut held = HeldKeys::default();
        held.press(a, "a".into());
        // A repeat keeps the first name.
        held.press(a, "A".into());
        held.press(b, "b".into());
        assert_eq!(held.release(a), Some("a".into()));
        assert_eq!(held.release(a), None);
        assert_eq!(held.release_all(), ["b"]);
        assert_eq!(held.release(b), None);
    }

    #[test]
    fn blit_centers_the_pixmap_over_black() {
        let mut pixmap = Pixmap::new(2, 1).unwrap();
        let opaque = tiny_skia::ColorU8::from_rgba(255, 0, 0, 255).premultiply();
        let half = tiny_skia::ColorU8::from_rgba(0, 255, 0, 128).premultiply();
        pixmap.pixels_mut().copy_from_slice(&[opaque, half]);
        let mut buffer = [0xFFFF_FFFF; 4 * 3];
        let viewport = Viewport::centered(1.0, &pixmap, (4, 3));
        blit_pixmap(
            &pixmap,
            &mut buffer,
            (NonZeroU32::new(4).unwrap(), NonZeroU32::new(3).unwrap()),
            viewport.offset,
        );
        #[rustfmt::skip]
        let expected = [
            0, 0, 0, 0,
            0, 0xFF_0000, 0x00_8000, 0,
            0, 0, 0, 0,
        ];
        assert_eq!(buffer, expected);
    }

    #[test]
    fn a_point_of_the_window_maps_to_the_scene() {
        // A 100 by 50 scene at scale 4, centered in a 400 by 400 window.
        let pixmap = Pixmap::new(400, 200).unwrap();
        let viewport = Viewport::centered(4.0, &pixmap, (400, 400));
        assert_eq!(viewport.offset, (0, 100));
        let at = |x, y| viewport.to_scene(PhysicalPosition::new(x, y));
        assert_eq!(at(200.0, 150.0), (50.0, 12.5));
        // The band above the frame.
        assert_eq!(at(0.0, 50.0), (0.0, -12.5));
    }

    #[test]
    fn the_wheel_counts_notches_with_the_w3c_sign() {
        let down = wheel_notches(MouseScrollDelta::LineDelta(0.0, -1.0), 2.0);
        assert_eq!(down, (0.0, 1.0));
        assert!(down.0.is_sign_positive());
        let pixels = MouseScrollDelta::PixelDelta(PhysicalPosition::new(-400.0, 100.0));
        assert_eq!(wheel_notches(pixels, 2.0), (2.0, -0.5));
    }
}
