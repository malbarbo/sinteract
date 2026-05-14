//! `Frontend` — the unified driver type that hosts (spython, sgleam, future
//! sgleam wasm, etc.) talk to.
//!
//! All concrete frontends — terminal, window, stdio — share the same
//! method surface. Hosts construct one [`Frontend`] up front and drive it:
//!
//! ```ignore
//! let mut fr = Frontend::pick_native("My game");
//! fr.enter();
//! while let Some(ev) = fr.wait_event(None) {
//!     match ev {
//!         InputEvent::Vsync => { /* simulate + repaint */ },
//!         InputEvent::Key(k) => { /* dispatch */ },
//!         InputEvent::Close => break,
//!     }
//!     fr.present(&dl);
//! }
//! fr.exit();
//! ```
//!
//! The variants are an enum (not `Box<dyn Frontend>`) on purpose: dispatch
//! is monomorphic, no vtable, and the compiler can specialize each call
//! site. The host's main loop is hot, so the indirection cost matters.

use std::time::{Duration, Instant};

use crate::event::{InputEvent, KeyEvent, KeyKind, MOD_ALT, MOD_CTRL, MOD_META, MOD_REPEAT, MOD_SHIFT};
use crate::ir::DrawList;

#[cfg(not(target_arch = "wasm32"))]
use crate::stdio::StdioFrontend;

/// Convenience: build a [`Duration`] period from a frequency in Hz. Each
/// backend uses this to express its own software-timed vsync cadence — the
/// value is not shared across backends.
const fn period_from_hz(hz: u32) -> Duration {
    Duration::from_nanos(1_000_000_000 / hz as u64)
}

/// Public driver. Construct one with [`Frontend::terminal`],
/// [`Frontend::window`], or [`Frontend::stdio`]; the host then drives it
/// for the whole session.
pub enum Frontend {
    #[cfg(not(target_arch = "wasm32"))]
    Terminal(TerminalFrontend),
    #[cfg(not(target_arch = "wasm32"))]
    Window(WindowFrontend),
    #[cfg(not(target_arch = "wasm32"))]
    Stdio(StdioFrontend),
}

impl Frontend {
    /// Pick the right native frontend automatically. Today: prefer terminal
    /// when stdout is a tty with graphics support, otherwise window. Hosts
    /// that need explicit control should call the per-variant constructors.
    /// `title` is used only when the chosen backend is a window — terminals
    /// inherit their title from the shell.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn pick_native(title: &str) -> Self {
        // Defer to the existing terminal capability probe — same heuristic
        // spython has been using.
        if crate::terminal::kitty_supported()
            || crate::sixel::sixel_supported()
            || crate::terminal::text_blocks_supported()
        {
            Frontend::terminal()
        } else {
            Frontend::window(title)
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn terminal() -> Self {
        Frontend::Terminal(TerminalFrontend::new())
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn window(title: &str) -> Self {
        Frontend::Window(WindowFrontend::new(title))
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn stdio() -> Self {
        Frontend::Stdio(StdioFrontend::new())
    }

    pub fn enter(&mut self) {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Frontend::Terminal(f) => f.enter(),
            #[cfg(not(target_arch = "wasm32"))]
            Frontend::Window(f) => f.enter(),
            #[cfg(not(target_arch = "wasm32"))]
            Frontend::Stdio(f) => f.enter(),
        }
    }

    pub fn exit(&mut self) {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Frontend::Terminal(f) => f.exit(),
            #[cfg(not(target_arch = "wasm32"))]
            Frontend::Window(f) => f.exit(),
            #[cfg(not(target_arch = "wasm32"))]
            Frontend::Stdio(f) => f.exit(),
        }
    }

    /// Block until the next input event or `deadline` elapses. `None`
    /// means "wait forever". Returns `None` only if the frontend has shut
    /// down (window closed, stdin EOF) — callers treat that as terminal.
    pub fn wait_event(&mut self, deadline: Option<Instant>) -> Option<InputEvent> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Frontend::Terminal(f) => f.wait_event(deadline),
            #[cfg(not(target_arch = "wasm32"))]
            Frontend::Window(f) => f.wait_event(deadline),
            #[cfg(not(target_arch = "wasm32"))]
            Frontend::Stdio(f) => f.wait_event(deadline),
        }
    }

    /// Render `dl` to the active output.
    pub fn present(&mut self, dl: &DrawList) {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Frontend::Terminal(f) => f.present(dl),
            #[cfg(not(target_arch = "wasm32"))]
            Frontend::Window(f) => f.present(dl),
            #[cfg(not(target_arch = "wasm32"))]
            Frontend::Stdio(f) => f.present(dl),
        }
    }

    /// Upload a bitmap to be referenced by `BitmapNode.id`. Frontends that
    /// do not support bitmaps (terminal, pdf, current window) ignore this.
    pub fn push_asset(&mut self, id: u32, blob: &[u8], mime: Option<&str>) {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Frontend::Terminal(_) | Frontend::Window(_) => {}
            #[cfg(not(target_arch = "wasm32"))]
            Frontend::Stdio(f) => f.push_asset(id, blob, mime),
        }
    }
}

// ---------------------------------------------------------------------------
// Common vsync-scheduling helper
// ---------------------------------------------------------------------------

/// Software-timed vsync emitter. Each backend constructs one with its own
/// period (terminal vs window vs others). Real-vsync integration (winit
/// swap chain, browser rAF) bypasses this helper and emits Vsync events
/// directly from the platform callback.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VsyncClock {
    period: Duration,
    /// `None` until the first vsync has been emitted; lets the very first
    /// `wait_event` fire `Vsync` immediately.
    last_vsync: Option<Instant>,
}

impl VsyncClock {
    pub(crate) fn new(period: Duration) -> Self {
        Self {
            period,
            last_vsync: None,
        }
    }

    /// Time at which the next Vsync should fire.
    pub(crate) fn next_vsync_at(&self) -> Instant {
        match self.last_vsync {
            None => Instant::now(),
            Some(t) => t + self.period,
        }
    }

    /// Mark a vsync as fired now. Returns the [`InputEvent::Vsync`] for
    /// convenience.
    pub(crate) fn fire(&mut self) -> InputEvent {
        self.last_vsync = Some(Instant::now());
        InputEvent::Vsync
    }

    /// `true` if a Vsync is due (ignoring overshoot). Saves an `Instant::now`
    /// when the caller already knows the time.
    pub(crate) fn is_due(&self, now: Instant) -> bool {
        now >= self.next_vsync_at()
    }
}

/// Combine the caller's deadline with the next vsync deadline, returning the
/// tightest `Duration` we can wait on the OS poll.
pub(crate) fn poll_timeout(deadline: Option<Instant>, next_vsync: Instant) -> Duration {
    let now = Instant::now();
    let mut out = next_vsync.saturating_duration_since(now);
    if let Some(d) = deadline {
        out = out.min(d.saturating_duration_since(now));
    }
    out
}

/// Translate the `[alt, ctrl, shift, meta, repeat]` tuple historic spython
/// uses into an [`InputEvent`] modifier bitmask.
pub(crate) fn key_event_from_legacy(
    event_type: i32,
    key: String,
    flags: [bool; 5],
) -> InputEvent {
    let kind = match event_type {
        1 => KeyKind::Down,
        2 => KeyKind::Up,
        _ => KeyKind::Press,
    };
    let mut m = 0u8;
    if flags[0] {
        m |= MOD_ALT;
    }
    if flags[1] {
        m |= MOD_CTRL;
    }
    if flags[2] {
        m |= MOD_SHIFT;
    }
    if flags[3] {
        m |= MOD_META;
    }
    if flags[4] {
        m |= MOD_REPEAT;
    }
    InputEvent::Key(KeyEvent {
        kind,
        key,
        modifiers: m,
    })
}

// ---------------------------------------------------------------------------
// TerminalFrontend
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
pub struct TerminalFrontend {
    clock: VsyncClock,
    entered: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl TerminalFrontend {
    /// Vsync cadence in the terminal. There is no hardware refresh to sync
    /// against; 60 Hz is enough for smooth half-block animation without
    /// flooding the pty with escape codes.
    const VSYNC_PERIOD: Duration = period_from_hz(60);

    pub fn new() -> Self {
        Self {
            clock: VsyncClock::new(Self::VSYNC_PERIOD),
            entered: false,
        }
    }

    pub fn enter(&mut self) {
        if !self.entered {
            crate::terminal::enter_animation();
            self.entered = true;
        }
    }

    pub fn exit(&mut self) {
        if self.entered {
            crate::terminal::exit_animation();
            self.entered = false;
        }
    }

    pub fn present(&mut self, dl: &DrawList) {
        crate::terminal::show_image_dl(dl);
    }

    pub fn wait_event(&mut self, deadline: Option<Instant>) -> Option<InputEvent> {
        // Fast path: vsync due before we even poll.
        if self.clock.is_due(Instant::now()) {
            return Some(self.clock.fire());
        }
        loop {
            // crossterm's event::poll has its own non-blocking shape, so we
            // implement a busy-ish loop with bounded sleeps. That is fine
            // here because terminals do not emit thousands of events per
            // second; the bottleneck is IO, not the poll period.
            let now = Instant::now();
            if let Some(d) = deadline
                && now >= d
            {
                return None;
            }
            if self.clock.is_due(now) {
                return Some(self.clock.fire());
            }
            if let Some(legacy) = crate::terminal::poll_key_event() {
                let (et, key, flags) = legacy;
                return Some(key_event_from_legacy(et, key, flags));
            }
            let next_vsync = self.clock.next_vsync_at();
            let timeout = poll_timeout(deadline, next_vsync);
            // Hard floor so we don't burn CPU when the next vsync is microseconds away.
            std::thread::sleep(timeout.min(Duration::from_millis(8)));
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for TerminalFrontend {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// WindowFrontend
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
pub struct WindowFrontend {
    title: String,
    clock: VsyncClock,
    entered: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl WindowFrontend {
    /// Software-timed vsync cadence used by the current window stub. Once
    /// the window switches to a real swap chain (winit + wgpu present), the
    /// clock will be replaced by platform vsync callbacks and this constant
    /// becomes irrelevant.
    const VSYNC_PERIOD: Duration = period_from_hz(60);

    pub fn new(title: &str) -> Self {
        Self {
            title: title.to_owned(),
            clock: VsyncClock::new(Self::VSYNC_PERIOD),
            entered: false,
        }
    }

    pub fn enter(&mut self) {
        if !self.entered {
            crate::window::enter_animation(&self.title);
            self.entered = true;
        }
    }

    pub fn exit(&mut self) {
        if self.entered {
            crate::window::exit_animation();
            self.entered = false;
        }
    }

    pub fn present(&mut self, dl: &DrawList) {
        crate::window::show_image_dl(dl);
    }

    pub fn wait_event(&mut self, deadline: Option<Instant>) -> Option<InputEvent> {
        if self.clock.is_due(Instant::now()) {
            return Some(self.clock.fire());
        }
        loop {
            let now = Instant::now();
            if let Some(d) = deadline
                && now >= d
            {
                return None;
            }
            if self.clock.is_due(now) {
                return Some(self.clock.fire());
            }
            if crate::window::closed() {
                return Some(InputEvent::Close);
            }
            if let Some(legacy) = crate::window::poll_key_event() {
                let (et, key, flags) = legacy;
                return Some(key_event_from_legacy(et, key, flags));
            }
            let next_vsync = self.clock.next_vsync_at();
            let timeout = poll_timeout(deadline, next_vsync);
            std::thread::sleep(timeout.min(Duration::from_millis(8)));
        }
    }
}
