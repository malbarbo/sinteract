//! [`Frontend`], the enum a host drives, with [`TerminalFrontend`] and
//! [`WindowFrontend`]. The module is private, and [`super`] re-exports the
//! three names.

use std::time::{Duration, Instant};

use crate::event::{
    InputEvent, KeyEvent, KeyKind, MOD_ALT, MOD_CTRL, MOD_META, MOD_REPEAT, MOD_SHIFT,
};
use crate::scene::Scene;

use super::stdio::StdioFrontend;

const fn period_from_hz(hz: u32) -> Duration {
    Duration::from_nanos(1_000_000_000 / hz as u64)
}

/// The driver a host (spython, sgleam) talks to. The terminal, the window
/// and stdio share one method surface. Construct one with
/// [`Frontend::terminal`], [`Frontend::window`] or [`Frontend::stdio`], and
/// drive it for the whole session:
///
/// ```ignore
/// let mut fr = Frontend::pick_native("My game");
/// fr.enter();
/// while let Some(ev) = fr.wait_event(None) {
///     match ev {
///         InputEvent::Vsync => { /* simulate + repaint */ },
///         InputEvent::Key(k) => { /* dispatch */ },
///         InputEvent::Close => break,
///     }
///     fr.present(&scene);
/// }
/// fr.exit();
/// ```
///
/// It is an enum and not a `Box<dyn Frontend>`, so that the call in the main
/// loop of the host dispatches without a vtable.
pub enum Frontend {
    Terminal(TerminalFrontend),
    Window(WindowFrontend),
    Stdio(StdioFrontend),
}

impl Frontend {
    /// Prefer the terminal when stdout is a tty with graphics, and the window
    /// otherwise. `title` only matters for a window. A terminal keeps the
    /// title of the shell.
    pub fn pick_native(title: &str) -> Self {
        if super::terminal::kitty_supported()
            || super::sixel::sixel_supported()
            || super::terminal::text_blocks_supported()
        {
            Frontend::terminal()
        } else {
            Frontend::window(title)
        }
    }

    pub fn terminal() -> Self {
        Frontend::Terminal(TerminalFrontend::new())
    }

    pub fn window(title: &str) -> Self {
        Frontend::Window(WindowFrontend::new(title))
    }

    pub fn stdio() -> Self {
        Frontend::Stdio(StdioFrontend::new())
    }

    pub fn enter(&mut self) {
        match self {
            Frontend::Terminal(f) => f.enter(),
            Frontend::Window(f) => f.enter(),
            Frontend::Stdio(f) => f.enter(),
        }
    }

    pub fn exit(&mut self) {
        match self {
            Frontend::Terminal(f) => f.exit(),
            Frontend::Window(f) => f.exit(),
            Frontend::Stdio(f) => f.exit(),
        }
    }

    /// Block until the next input event or until `deadline`, or forever when
    /// it is `None`. Returns `None` only when the frontend has shut down, on
    /// a closed window or on EOF at stdin.
    pub fn wait_event(&mut self, deadline: Option<Instant>) -> Option<InputEvent> {
        match self {
            Frontend::Terminal(f) => f.wait_event(deadline),
            Frontend::Window(f) => f.wait_event(deadline),
            Frontend::Stdio(f) => f.wait_event(deadline),
        }
    }

    /// Render `scene` to the active output.
    pub fn present(&mut self, scene: &Scene) {
        match self {
            Frontend::Terminal(f) => f.present(scene),
            Frontend::Window(f) => f.present(scene),
            Frontend::Stdio(f) => f.present(scene),
        }
    }

    /// Upload a bitmap for `Bitmap.id`. The terminal and the window do not
    /// support bitmaps and ignore it.
    pub fn push_asset(&mut self, id: u32, blob: &[u8], mime: Option<&str>) {
        match self {
            Frontend::Terminal(_) | Frontend::Window(_) => {}
            Frontend::Stdio(f) => f.push_asset(id, blob, mime),
        }
    }
}

/// Say once per frontend that this backend drops the bitmaps of the frame. A
/// process-global flag would stay silent for every session after the first,
/// and a server hosts many sessions.
fn warn_bitmaps_once(warned: &mut bool, scene: &Scene, backend: &str) {
    if !*warned && scene.has_bitmaps() {
        *warned = true;
        eprintln!(
            "[sinteract] the {backend} renderer does not support bitmaps; drawing without them."
        );
    }
}

// ---------------------------------------------------------------------------
// Common vsync-scheduling helper
// ---------------------------------------------------------------------------

/// Software-timed vsync. Each backend constructs one with its own period. A
/// backend with a platform vsync emits the event from the platform callback
/// and does not use this.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VsyncClock {
    period: Duration,
    /// `None` until the first vsync, so the first `wait_event` fires `Vsync`
    /// at once.
    last_vsync: Option<Instant>,
}

impl VsyncClock {
    pub(crate) fn new(period: Duration) -> Self {
        Self {
            period,
            last_vsync: None,
        }
    }

    pub(crate) fn next_vsync_at(&self) -> Instant {
        match self.last_vsync {
            None => Instant::now(),
            Some(t) => t + self.period,
        }
    }

    /// Record a vsync now. Returns the [`InputEvent::Vsync`] to deliver.
    pub(crate) fn fire(&mut self) -> InputEvent {
        self.last_vsync = Some(Instant::now());
        InputEvent::Vsync
    }

    /// Returns `true` if a Vsync is due at `now`, `false` otherwise. Takes
    /// `now` so the caller does not read the clock twice.
    pub(crate) fn is_due(&self, now: Instant) -> bool {
        now >= self.next_vsync_at()
    }
}

/// The shortest wait among the deadline of the caller and the next vsync.
pub(crate) fn poll_timeout(deadline: Option<Instant>, next_vsync: Instant) -> Duration {
    let now = Instant::now();
    let mut out = next_vsync.saturating_duration_since(now);
    if let Some(d) = deadline {
        out = out.min(d.saturating_duration_since(now));
    }
    out
}

/// Build an [`InputEvent`] from the tuple that `poll_key_event` returns in
/// `terminal` and `window`. `flags` is `[alt, ctrl, shift, meta, repeat]`.
pub(crate) fn key_event_from_legacy(event_type: i32, key: String, flags: [bool; 5]) -> InputEvent {
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

pub struct TerminalFrontend {
    clock: VsyncClock,
    entered: bool,
    warned_bitmaps: bool,
}

impl TerminalFrontend {
    /// There is no hardware refresh in a terminal. 60 Hz is smooth for
    /// half-block animation and does not flood the pty with escape codes.
    const VSYNC_PERIOD: Duration = period_from_hz(60);

    pub fn new() -> Self {
        Self {
            clock: VsyncClock::new(Self::VSYNC_PERIOD),
            entered: false,
            warned_bitmaps: false,
        }
    }

    pub fn enter(&mut self) {
        if !self.entered {
            super::terminal::enter_animation();
            self.entered = true;
        }
    }

    pub fn exit(&mut self) {
        if self.entered {
            super::terminal::exit_animation();
            self.entered = false;
        }
    }

    pub fn present(&mut self, scene: &Scene) {
        warn_bitmaps_once(&mut self.warned_bitmaps, scene, "terminal");
        super::terminal::show_image(scene);
    }

    pub fn wait_event(&mut self, deadline: Option<Instant>) -> Option<InputEvent> {
        if self.clock.is_due(Instant::now()) {
            return Some(self.clock.fire());
        }
        loop {
            // The loop sleeps in short steps instead of blocking on
            // crossterm. A terminal emits few events per second, so the
            // poll costs nothing that matters.
            let now = Instant::now();
            if let Some(d) = deadline
                && now >= d
            {
                return None;
            }
            if self.clock.is_due(now) {
                return Some(self.clock.fire());
            }
            if super::terminal::closed() {
                return Some(InputEvent::Close);
            }
            if let Some(legacy) = super::terminal::poll_key_event() {
                let (et, key, flags) = legacy;
                return Some(key_event_from_legacy(et, key, flags));
            }
            let next_vsync = self.clock.next_vsync_at();
            let timeout = poll_timeout(deadline, next_vsync);
            // Sleep at most 8 ms, so a key or a close is seen soon even when
            // the deadline is far.
            std::thread::sleep(timeout.min(Duration::from_millis(8)));
        }
    }
}

impl Default for TerminalFrontend {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// WindowFrontend
// ---------------------------------------------------------------------------

pub struct WindowFrontend {
    title: String,
    clock: VsyncClock,
    entered: bool,
    warned_bitmaps: bool,
}

impl WindowFrontend {
    /// The window paints through softbuffer without a swap chain, so its
    /// cadence is software-timed too.
    const VSYNC_PERIOD: Duration = period_from_hz(60);

    pub fn new(title: &str) -> Self {
        Self {
            title: title.to_owned(),
            clock: VsyncClock::new(Self::VSYNC_PERIOD),
            entered: false,
            warned_bitmaps: false,
        }
    }

    pub fn enter(&mut self) {
        if !self.entered {
            super::window::enter_animation(&self.title);
            self.entered = true;
        }
    }

    pub fn exit(&mut self) {
        if self.entered {
            super::window::exit_animation();
            self.entered = false;
        }
    }

    pub fn present(&mut self, scene: &Scene) {
        warn_bitmaps_once(&mut self.warned_bitmaps, scene, "window");
        super::window::show_image(scene);
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
            if super::window::closed() {
                return Some(InputEvent::Close);
            }
            if let Some(legacy) = super::window::poll_key_event() {
                let (et, key, flags) = legacy;
                return Some(key_event_from_legacy(et, key, flags));
            }
            let next_vsync = self.clock.next_vsync_at();
            let timeout = poll_timeout(deadline, next_vsync);
            std::thread::sleep(timeout.min(Duration::from_millis(8)));
        }
    }
}
