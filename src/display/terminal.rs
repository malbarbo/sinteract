//! Terminal display of a [`crate::scene::Scene`] through the Kitty graphics
//! protocol, DEC Sixel or truecolor half-blocks. [`Terminal`] is a session
//! in the alt screen, where each frame replaces the previous one at (0, 0),
//! and [`show_image`] prints one image inline.
//!
//! The tty belongs to the process, so one `Terminal` exists at a time, and
//! [`show_image`] prints nothing while it does. A Unix terminal does not
//! tell a key down from a key up, so every key event is a press.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use crossterm::event::{
    self as ct_event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::{cursor, execute, queue, terminal};
use tiny_skia::Pixmap;

use super::driver::{OpenError, period_from_hz, sealed, warn_bitmaps_once};
use super::inbox::{Inbox, Sender, Wait};
use super::sixel;
use crate::event::{
    Event, InputEvent, KeyKind, Modifiers, MouseAction, MouseButton, MouseButtons, MouseEvent, key,
};
use crate::renderer::Renderer;
use crate::renderer::pixmap::PixmapRenderer;
use crate::scene::Scene;

const KITTY_ANIMATION_ID: u32 = 1042;

// Fallback when the terminal does not answer the `CSI 16 t` probe.
const CELL_W_DEFAULT: u32 = 8;
const CELL_H_DEFAULT: u32 = 16;

/// Sixel has no transparency that keeps the previous frame, so a frame
/// paints over this.
const SIXEL_BACKGROUND: (u8, u8, u8) = (255, 255, 255);

/// What an engine adds to [`Terminal::open_with`].
#[derive(Default)]
pub struct TerminalOptions {
    /// Runs on the reader thread when the user presses Ctrl-C, after the
    /// Close goes into the queue. Raw mode turns off the signal of Ctrl-C,
    /// so an engine stops here the code that never calls `wait_event`.
    pub on_interrupt: Option<Box<dyn FnMut() + Send>>,
}

/// A [`super::Display`] over the alt screen of the terminal, in raw mode.
/// Ctrl-C arrives as [`InputEvent::Close`]. The size of the terminal
/// arrives as an [`InputEvent::Resize`] ahead of the first Vsync, and again
/// after each change. A mouse event gives the center of its cell.
pub struct Terminal {
    inbox: Inbox,
    backend: Backend,
    /// `None` after [`super::Display::close`].
    live: Option<Live>,
    /// The size in pixels of the frame on screen, or `None` before the
    /// first one. Kitty keeps a frame after the session.
    frame_size: Option<(u32, u32)>,
    /// Kept across frames, so a frame reuses the pixmap and the clip masks.
    renderer: PixmapRenderer,
    /// The scene of the last present, drawn again after a resize.
    last: Option<Scene>,
    /// How the reader maps a cell to the scene on screen.
    cells: Arc<Mutex<CellMap>>,
    warned_bitmaps: bool,
}

/// What a session holds until it closes.
struct Live {
    reader: Reader,
    claim: Claim,
}

impl Terminal {
    /// There is no hardware refresh in a terminal. 60 Hz is smooth for
    /// half-block animation and does not flood the pty with escape codes.
    const VSYNC_PERIOD: Duration = period_from_hz(60);

    /// Enter the alt screen and raw mode, with [`TerminalOptions::default`].
    pub fn open() -> Result<Self, OpenError> {
        Self::open_with(TerminalOptions::default())
    }

    /// Enter the alt screen and raw mode. Fails with [`OpenError::Busy`]
    /// while another `Terminal` exists, and with [`OpenError::NoGraphics`]
    /// when the terminal shows neither Kitty, Sixel nor truecolor.
    pub fn open_with(options: TerminalOptions) -> Result<Self, OpenError> {
        let claim = Claim::take()?;
        // The probe reads the replies from the tty, so it runs under the
        // claim, where no reader thread takes them.
        let backend = pick_backend().ok_or(OpenError::NoGraphics)?;
        claim.show_through(backend);
        terminal::enable_raw_mode().map_err(OpenError::Io)?;
        install_panic_hook();
        let entered = execute!(
            io::stdout(),
            terminal::EnterAlternateScreen,
            cursor::Hide,
            ct_event::EnableMouseCapture
        );
        let mut inbox = Inbox::new(Some(Self::VSYNC_PERIOD));
        let cell = cell_pixels();
        if let Some((width, height)) = terminal::size().ok().and_then(|s| scene_size(s, cell)) {
            inbox.send_first(InputEvent::Resize { width, height });
        }
        let cells = Arc::new(Mutex::new(CellMap::before_frames(backend, cell)));
        let reader = entered
            .and_then(|()| Reader::spawn(inbox.sender(), Arc::clone(&cells), options.on_interrupt));
        let reader = match reader {
            Ok(reader) => reader,
            Err(e) => {
                leave(backend, false);
                return Err(OpenError::Io(e));
            }
        };
        Ok(Self {
            inbox,
            backend,
            live: Some(Live { reader, claim }),
            frame_size: None,
            renderer: PixmapRenderer::default(),
            last: None,
            cells,
            warned_bitmaps: false,
        })
    }

    fn draw(&mut self, scene: &Scene) {
        let scale = scale_for_backend(self.backend, scene.width(), scene.height());
        let Some(pixmap) = rasterize(&mut self.renderer, scale, scene) else {
            return;
        };
        *self.cells.lock().unwrap_or_else(PoisonError::into_inner) =
            CellMap::new(self.backend, cell_pixels(), scale);
        let mut stdout = io::stdout().lock();
        // A smaller frame leaves the edges of the one before. The same Kitty
        // id replaces the whole image in place, and a clear, or a delete
        // before the transmit, shows the cleared cells for one refresh.
        let size = (pixmap.width(), pixmap.height());
        if self.frame_size.replace(size) != Some(size) && self.backend != Backend::Kitty {
            let _ = queue!(stdout, terminal::Clear(terminal::ClearType::All));
        }
        let _ = queue!(stdout, cursor::MoveTo(0, 0));
        let _ = match self.backend {
            Backend::Kitty => emit_kitty(&mut stdout, pixmap, Some(KITTY_ANIMATION_ID)),
            Backend::Sixel => {
                sixel::encode(pixmap, SIXEL_BACKGROUND).and_then(|b| stdout.write_all(&b))
            }
            Backend::TextBlocks => render_text_blocks(&mut stdout, pixmap),
        };
        let _ = stdout.flush();
    }

    /// Draw the last scene again, after a resize. The terminal may have
    /// moved or wrapped the cells of the old frame, so the screen clears.
    fn redraw(&mut self) {
        if let Some(scene) = self.last.take() {
            self.frame_size = None;
            self.draw(&scene);
            self.last = Some(scene);
        }
    }
}

impl super::Display for Terminal {
    fn present(&mut self, scene: &Scene) {
        if self.live.is_none() {
            return;
        }
        warn_bitmaps_once(&mut self.warned_bitmaps, scene, "terminal");
        self.draw(scene);
        self.last = Some(scene.clone());
    }

    fn wait_event(&mut self, deadline: Option<Instant>) -> Event {
        loop {
            match self.inbox.wait_with(deadline, Inbox::receive) {
                Wait::Event(event) => return event,
                Wait::Redraw => self.redraw(),
            }
        }
    }

    fn sender(&self) -> Sender {
        self.inbox.sender()
    }

    /// The terminal draws without bitmaps, so it drops the upload.
    fn push_asset(&mut self, _id: u32, _blob: &[u8], _mime: Option<&str>) {}

    /// Stop the reader thread, and leave the alt screen and raw mode.
    fn close(&mut self) {
        let Some(live) = self.live.take() else {
            return;
        };
        self.inbox.close();
        live.reader.stop();
        // Keys typed after the reader stopped would go to the shell.
        drain_input();
        if !live.claim.restored() {
            leave(self.backend, self.frame_size.is_some());
        }
    }
}

impl sealed::Sealed for Terminal {}

impl Drop for Terminal {
    fn drop(&mut self) {
        super::Display::close(self);
    }
}

/// Print `scene` at the cursor, through Kitty when the terminal supports
/// it, else Sixel, else half-blocks. Prints nothing while a [`Terminal`]
/// holds the tty, or when the terminal has no graphics.
pub fn show_image(scene: &Scene) {
    if TTY.load(Ordering::Acquire) != FREE {
        eprintln!("[sinteract] a terminal session is open; not printing the image");
        return;
    }
    let Some(backend) = pick_backend() else {
        return;
    };
    let mut renderer = PixmapRenderer::default();
    let scale = scale_for_backend(backend, scene.width(), scene.height());
    let Some(pixmap) = rasterize(&mut renderer, scale, scene) else {
        return;
    };
    let mut stdout = io::stdout().lock();
    let _ = match backend {
        Backend::Kitty => emit_kitty(&mut stdout, pixmap, None).and_then(|()| writeln!(stdout)),
        Backend::Sixel => sixel::encode(pixmap, SIXEL_BACKGROUND)
            .and_then(|b| stdout.write_all(&b))
            .and_then(|()| writeln!(stdout)),
        Backend::TextBlocks => render_text_blocks(&mut stdout, pixmap),
    };
    let _ = stdout.flush();
}

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
    super::term_query::graphics_caps().kitty
}

/// Returns `true` if the terminal supports DEC Sixel, `false` otherwise. The
/// answer comes from the same probe as [`kitty_supported`].
pub fn sixel_supported() -> bool {
    super::term_query::graphics_caps().sixel
}

/// The discriminants are the values of [`TTY`] while a [`Terminal`] shows
/// through the backend.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(u8)]
enum Backend {
    Kitty = 2,
    Sixel = 3,
    TextBlocks = 4,
}

fn pick_backend() -> Option<Backend> {
    if kitty_supported() {
        Some(Backend::Kitty)
    } else if sixel_supported() {
        Some(Backend::Sixel)
    } else if text_blocks_supported() {
        Some(Backend::TextBlocks)
    } else {
        None
    }
}

fn rasterize<'r>(
    renderer: &'r mut PixmapRenderer,
    scale: f32,
    scene: &Scene,
) -> Option<&'r Pixmap> {
    renderer.set_scale(scale);
    let pixmap = renderer.render(scene).ok();
    if pixmap.is_none() {
        eprintln!("[sinteract] failed to rasterize draw list");
    }
    pixmap
}

// -----------------------------------------------------------------------------
// The tty of the process
// -----------------------------------------------------------------------------

/// Who holds the tty: [`FREE`], a [`Terminal`] that still probes it
/// ([`PROBING`]), one that shows through a [`Backend`], by its
/// discriminant, or one whose tty the panic hook put back ([`RESTORED`]).
/// The panic hook changes it, so it is an atomic and not a lock.
static TTY: AtomicU8 = AtomicU8::new(FREE);
const FREE: u8 = 0;
const PROBING: u8 = 1;
const RESTORED: u8 = 5;

/// The hold of a [`Terminal`] on the tty, released on drop.
struct Claim;

impl Claim {
    fn take() -> Result<Self, OpenError> {
        match TTY.compare_exchange(FREE, PROBING, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => Ok(Claim),
            Err(_) => Err(OpenError::Busy),
        }
    }

    /// Record the backend that the probe picked, for the panic hook.
    fn show_through(&self, backend: Backend) {
        TTY.store(backend as u8, Ordering::Release);
    }

    /// Returns `true` if the panic hook put the tty back, `false`
    /// otherwise. A second restore would print after the panic message,
    /// and some terminals move the cursor back up on leaving the alt screen
    /// twice.
    fn restored(&self) -> bool {
        TTY.load(Ordering::Acquire) == RESTORED
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        TTY.store(FREE, Ordering::Release);
    }
}

/// Mark the tty as put back and return the backend of the [`Terminal`]
/// that holds it. `None` when nobody holds it or the tty is already back,
/// and half-blocks while the probe runs, since only Kitty needs more than
/// leaving the alt screen.
fn restore_held() -> Option<Backend> {
    let tag = TTY
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |tag| {
            (tag != FREE && tag != RESTORED).then_some(RESTORED)
        })
        .ok()?;
    Some(
        [Backend::Kitty, Backend::Sixel, Backend::TextBlocks]
            .into_iter()
            .find(|&b| b as u8 == tag)
            .unwrap_or(Backend::TextBlocks),
    )
}

/// Leave the alt screen and raw mode. Kitty keeps an image across the flip
/// of the alt screen, so a frame it shows goes by id. Sixel and half-blocks
/// output lives in the alt screen and goes with it.
fn leave(backend: Backend, frame_shown: bool) {
    let mut stdout = io::stdout().lock();
    if frame_shown && backend == Backend::Kitty {
        let _ = delete_kitty_image(&mut stdout, KITTY_ANIMATION_ID);
    }
    let _ = execute!(
        stdout,
        ct_event::DisableMouseCapture,
        cursor::Show,
        terminal::LeaveAlternateScreen
    );
    drop(stdout);
    let _ = terminal::disable_raw_mode();
}

/// A hook puts the tty back before the panic message prints, from
/// whichever thread panics. Otherwise the message goes to the alt screen and
/// disappears with it. With `panic = "abort"` no drop runs, so the hook is
/// the only restore. With unwinding the drop of the `Terminal` finds the tty
/// back and leaves it alone, and a panic that the program catches leaves
/// the `Terminal` outside the alt screen. The only lock that the hook takes is
/// the one of crossterm around the saved mode, which no code holds across a
/// panic.
fn install_panic_hook() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Some(backend) = restore_held() {
                leave(backend, true);
            }
            prev(info);
        }));
    });
}

// -----------------------------------------------------------------------------
// Input
// -----------------------------------------------------------------------------

/// The thread that moves the input of the terminal into the queue.
struct Reader {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

/// How long the reader blocks before it checks `stop`, which bounds how
/// long [`Reader::stop`] waits.
const READ_POLL: Duration = Duration::from_millis(50);

impl Reader {
    fn spawn(
        tx: Sender,
        cells: Arc<Mutex<CellMap>>,
        on_interrupt: Option<Box<dyn FnMut() + Send>>,
    ) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("sinteract-terminal".into())
            .spawn(move || read_input(&tx, &cells, &flag, on_interrupt))?;
        Ok(Self { stop, thread })
    }

    fn stop(self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.thread.join();
    }
}

/// Send the input until `stop`, Ctrl-C or a read error. A Close goes into
/// the queue on every way out, so a reader that dies does not leave the
/// engine waiting in raw mode.
fn read_input(
    tx: &Sender,
    cells: &Mutex<CellMap>,
    stop: &AtomicBool,
    mut on_interrupt: Option<Box<dyn FnMut() + Send>>,
) {
    let _close = CloseOnExit(tx);
    let mut buttons = MouseButtons::default();
    while !stop.load(Ordering::Acquire) {
        let ev = match ct_event::poll(READ_POLL) {
            Ok(false) => continue,
            Ok(true) => ct_event::read(),
            Err(e) => Err(e),
        };
        let ev = match ev {
            Ok(ev) => ev,
            Err(e) => {
                eprintln!("[sinteract] terminal read error: {e}");
                return;
            }
        };
        let key = match ev {
            ct_event::Event::Key(key) => key,
            ct_event::Event::Mouse(m) => {
                let cells = *cells.lock().unwrap_or_else(PoisonError::into_inner);
                if tx
                    .send_input(InputEvent::Mouse(mouse_event(m, &mut buttons, cells)))
                    .is_err()
                {
                    return;
                }
                continue;
            }
            ct_event::Event::Resize(cols, rows) => {
                if let Some((width, height)) = scene_size((cols, rows), cell_pixels()) {
                    let _ = tx.send_input(InputEvent::Resize { width, height });
                }
                if tx.request_redraw().is_err() {
                    return;
                }
                continue;
            }
            ct_event::Event::FocusGained
            | ct_event::Event::FocusLost
            | ct_event::Event::Paste(_) => continue,
        };
        if is_ctrl_c(&key) {
            let _ = tx.send_input(InputEvent::Close);
            if let Some(f) = on_interrupt.as_mut() {
                f();
            }
            return;
        }
        if let Some(k) = key_event(key)
            && tx.send_input(InputEvent::Key(k)).is_err()
        {
            return;
        }
    }
}

struct CloseOnExit<'a>(&'a Sender);

impl Drop for CloseOnExit<'_> {
    fn drop(&mut self) {
        let _ = self.0.send_input(InputEvent::Close);
    }
}

fn is_ctrl_c(key: &KeyEvent) -> bool {
    key.kind != KeyEventKind::Release
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c'))
}

/// Drop the events that crossterm read and nobody took.
fn drain_input() {
    while ct_event::poll(Duration::ZERO).unwrap_or(false) {
        if ct_event::read().is_err() {
            break;
        }
    }
}

/// The key event of a crossterm key, or `None` for a release or a key with no
/// name. Windows reports every release, and a Unix terminal reports one only
/// with the kitty keyboard protocol, which is off. A release is dropped, so a
/// terminal sends a press alone on every platform. A repeat arrives as a
/// press.
fn key_event(ev: KeyEvent) -> Option<crate::event::KeyEvent> {
    if ev.kind == KeyEventKind::Release {
        return None;
    }
    let key = key_code_to_string(ev.code)?;
    Some(crate::event::KeyEvent {
        kind: KeyKind::Press,
        key,
        modifiers: modifiers(ev.modifiers),
        repeat: ev.kind == KeyEventKind::Repeat,
    })
}

fn modifiers(m: KeyModifiers) -> Modifiers {
    Modifiers {
        alt: m.contains(KeyModifiers::ALT),
        ctrl: m.contains(KeyModifiers::CONTROL),
        shift: m.contains(KeyModifiers::SHIFT),
        meta: m.contains(KeyModifiers::SUPER),
    }
}

/// The mouse event of a crossterm one, at the center of its cell. `buttons`
/// holds the buttons down across events, since crossterm reports only the
/// button of each event.
fn mouse_event(m: ct_event::MouseEvent, buttons: &mut MouseButtons, cells: CellMap) -> MouseEvent {
    let action = match m.kind {
        MouseEventKind::Down(b) => {
            *buttons = buttons.with(mouse_button(b));
            MouseAction::Down(mouse_button(b))
        }
        MouseEventKind::Up(b) => {
            *buttons = buttons.without(mouse_button(b));
            MouseAction::Up(mouse_button(b))
        }
        MouseEventKind::Drag(b) => {
            *buttons = buttons.with(mouse_button(b));
            MouseAction::Move
        }
        MouseEventKind::Moved => MouseAction::Move,
        MouseEventKind::ScrollDown => MouseAction::Wheel { dx: 0.0, dy: 1.0 },
        MouseEventKind::ScrollUp => MouseAction::Wheel { dx: 0.0, dy: -1.0 },
        MouseEventKind::ScrollRight => MouseAction::Wheel { dx: 1.0, dy: 0.0 },
        MouseEventKind::ScrollLeft => MouseAction::Wheel { dx: -1.0, dy: 0.0 },
    };
    let (x, y) = cells.to_scene(m.column, m.row);
    MouseEvent {
        action,
        x,
        y,
        modifiers: modifiers(m.modifiers),
        buttons: *buttons,
    }
}

fn mouse_button(b: ct_event::MouseButton) -> MouseButton {
    match b {
        ct_event::MouseButton::Left => MouseButton::Left,
        ct_event::MouseButton::Middle => MouseButton::Middle,
        ct_event::MouseButton::Right => MouseButton::Right,
    }
}

/// Map a crossterm key code to its name in [`crate::event::key`], or to the
/// text it types. A function key above F12 has no name, as in the window.
fn key_code_to_string(code: KeyCode) -> Option<String> {
    Some(match code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Backspace => key::BACKSPACE.into(),
        KeyCode::Enter => key::ENTER.into(),
        KeyCode::Left => key::ARROW_LEFT.into(),
        KeyCode::Right => key::ARROW_RIGHT.into(),
        KeyCode::Up => key::ARROW_UP.into(),
        KeyCode::Down => key::ARROW_DOWN.into(),
        KeyCode::Home => key::HOME.into(),
        KeyCode::End => key::END.into(),
        KeyCode::PageUp => key::PAGE_UP.into(),
        KeyCode::PageDown => key::PAGE_DOWN.into(),
        KeyCode::Tab | KeyCode::BackTab => key::TAB.into(),
        KeyCode::Delete => key::DELETE.into(),
        KeyCode::Insert => key::INSERT.into(),
        KeyCode::Esc => key::ESCAPE.into(),
        KeyCode::F(n @ 1..=12) => key::FUNCTION_KEYS[usize::from(n - 1)].into(),
        _ => return None,
    })
}

/// Pixel size of one terminal cell, from the cached probe in
/// [`super::term_query`]. A terminal under a multiplexer or without a tty
/// does not answer, and gets 8 by 16.
fn cell_pixels() -> (u32, u32) {
    super::term_query::graphics_caps()
        .cell_px
        .unwrap_or((CELL_W_DEFAULT, CELL_H_DEFAULT))
}

// -----------------------------------------------------------------------------
// Terminal-aware sizing + half-blocks renderer
// -----------------------------------------------------------------------------

/// Pixel box available for the image, from the terminal size. `None` when
/// crossterm cannot read the size, as when stdout is a file.
fn target_pixels_for_backend(backend: Backend, cell: (u32, u32)) -> Option<(u32, u32)> {
    let (cols, rows_avail) = image_cells(terminal::size().ok()?)?;
    let (cw, ch) = cell;
    Some(match backend {
        Backend::Kitty | Backend::Sixel => (cols as u32 * cw, rows_avail as u32 * ch),
        // Half-blocks pack two image-pixel rows into one cell row, and one
        // image-pixel column into one cell column.
        Backend::TextBlocks => (cols as u32, rows_avail as u32 * 2),
    })
}

/// The columns and the rows that the image may take in a terminal of
/// `(cols, rows)`, or `None` for a terminal of no cells.
fn image_cells((cols, rows): (u16, u16)) -> Option<(u16, u16)> {
    if cols == 0 || rows == 0 {
        return None;
    }
    // One row stays free. Inline, it holds the prompt that follows the
    // image. In the alt screen, it keeps a Sixel in the last row from
    // scrolling the screen.
    Some((cols, rows.saturating_sub(1).max(1)))
}

/// The largest scene that a terminal of `(cols, rows)` shows at scale 1,
/// in the pixels of the screen. Half-blocks show it smaller.
fn scene_size(size: (u16, u16), (cw, ch): (u32, u32)) -> Option<(f32, f32)> {
    let (cols, rows) = image_cells(size)?;
    Some(((u32::from(cols) * cw) as f32, (u32::from(rows) * ch) as f32))
}

/// How a cell of the terminal maps to the scene of the frame at (0, 0).
#[derive(Clone, Copy, Debug, PartialEq)]
struct CellMap {
    /// The units of the scene per cell, across and down.
    per_cell: (f32, f32),
}

impl CellMap {
    /// A frame at `scale`. In Kitty and Sixel a pixmap pixel is a screen
    /// pixel, and in half-blocks a cell holds one pixmap pixel across and
    /// two down.
    fn new(backend: Backend, (cw, ch): (u32, u32), scale: f32) -> Self {
        let pixels = match backend {
            Backend::Kitty | Backend::Sixel => (cw as f32, ch as f32),
            Backend::TextBlocks => (1.0, 2.0),
        };
        Self {
            per_cell: (pixels.0 / scale, pixels.1 / scale),
        }
    }

    /// Before the first frame, the largest scale of the backend.
    fn before_frames(backend: Backend, cell: (u32, u32)) -> Self {
        Self::new(backend, cell, max_scale_for_backend(backend, cell))
    }

    fn to_scene(self, column: u16, row: u16) -> (f32, f32) {
        (
            (f32::from(column) + 0.5) * self.per_cell.0,
            (f32::from(row) + 0.5) * self.per_cell.1,
        )
    }
}

/// Upper bound on the scale of the rasterizer for `backend`. In Kitty and
/// Sixel a pixmap pixel is a screen pixel, so the cap of 1.0 keeps the image
/// at native size or smaller. In half-blocks a pixmap pixel covers a cell
/// width by half a cell height, so the cap is the inverse of the larger of
/// the two, and a logical pixel never grows past a screen pixel.
fn max_scale_for_backend(backend: Backend, (cw, ch): (u32, u32)) -> f32 {
    match backend {
        Backend::Kitty | Backend::Sixel => 1.0,
        Backend::TextBlocks => 1.0 / (cw as f32).max(ch as f32 / 2.0),
    }
}

/// Scale for a `width` by `height` frame on `backend`. The frame shrinks to
/// fit the cell grid when the grid size is known, and the cap of the backend
/// applies either way, because half-blocks pack two rows per cell even
/// without a known grid.
fn scale_for_backend(backend: Backend, width: f32, height: f32) -> f32 {
    let cell = cell_pixels();
    let target = target_pixels_for_backend(backend, cell);
    capped_scale(width, height, target, max_scale_for_backend(backend, cell))
}

/// The scale that fits a `width` by `height` frame into `target`, and never
/// more than `cap`.
fn capped_scale(width: f32, height: f32, target: Option<(u32, u32)>, cap: f32) -> f32 {
    match target {
        Some(target) => crate::renderer::pixmap::fit_scale(width, height, target).min(cap),
        None => cap,
    }
}

/// Write `pixmap` as truecolor half-blocks (`▀`). Each pair of rows becomes
/// one cell row, with the upper pixel in the foreground and the lower one in
/// the background, both composited over black.
fn render_text_blocks<W: Write>(out: &mut W, pixmap: &Pixmap) -> io::Result<()> {
    let w = pixmap.width() as usize;
    let h = pixmap.height() as usize;
    let pixels = pixmap.pixels();
    let mut y = 0;
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
        y += 2;
    }
    Ok(())
}

fn blend_on_black(p: tiny_skia::PremultipliedColorU8) -> (u8, u8, u8) {
    // The pixel is premultiplied, so compositing over black changes nothing.
    (p.red(), p.green(), p.blue())
}

// -----------------------------------------------------------------------------
// Kitty graphics protocol I/O
// -----------------------------------------------------------------------------

/// Write the Kitty escape sequences that show `pixmap` at the cursor, as raw
/// RGBA in chunks. An image with an `id` replaces the image of that id, and
/// one without stays until the terminal scrolls it away.
fn emit_kitty<W: Write>(w: &mut W, pixmap: &Pixmap, id: Option<u32>) -> io::Result<()> {
    let encoded = B64.encode(pixmap.data());
    let bytes = encoded.as_bytes();
    let chunk_size = 4096;
    let total_chunks = bytes.len().div_ceil(chunk_size).max(1);
    for (idx, chunk) in bytes.chunks(chunk_size).enumerate() {
        let more: u8 = if idx + 1 < total_chunks { 1 } else { 0 };
        if idx == 0 {
            write!(
                w,
                "\x1b_Ga=T,f=32,s={},v={},q=2,m={}",
                pixmap.width(),
                pixmap.height(),
                more,
            )?;
            if let Some(id) = id {
                write!(w, ",i={id}")?;
            }
            w.write_all(b";")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, r: u8, g: u8, b: u8) -> Pixmap {
        let mut pm = Pixmap::new(w, h).unwrap();
        let color = tiny_skia::ColorU8::from_rgba(r, g, b, 255).premultiply();
        pm.pixels_mut().fill(color);
        pm
    }

    #[test]
    fn a_cell_maps_to_the_center_of_its_part_of_the_scene() {
        // Kitty at scale 0.5 with 10 by 20 cells: a cell covers 20 by 40 units.
        let kitty = CellMap::new(Backend::Kitty, (10, 20), 0.5);
        assert_eq!(kitty.to_scene(0, 0), (10.0, 20.0));
        assert_eq!(kitty.to_scene(3, 1), (70.0, 60.0));
        // Half-blocks at scale 0.25: one pixmap pixel across and two down.
        let blocks = CellMap::new(Backend::TextBlocks, (10, 20), 0.25);
        assert_eq!(blocks.to_scene(0, 0), (2.0, 4.0));
    }

    #[test]
    fn the_scene_size_leaves_the_last_row_free() {
        assert_eq!(scene_size((80, 25), (8, 16)), Some((640.0, 384.0)));
        assert_eq!(scene_size((80, 1), (8, 16)), Some((640.0, 16.0)));
        assert_eq!(scene_size((0, 25), (8, 16)), None);
    }

    #[test]
    fn a_drag_holds_its_button_until_the_up() {
        let cells = CellMap::new(Backend::Kitty, (1, 1), 1.0);
        let mut buttons = MouseButtons::default();
        let at = |kind| ct_event::MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::SHIFT,
        };
        let left = ct_event::MouseButton::Left;
        let down = mouse_event(at(MouseEventKind::Down(left)), &mut buttons, cells);
        assert_eq!(down.action, MouseAction::Down(MouseButton::Left));
        assert!(down.modifiers.shift);
        let drag = mouse_event(at(MouseEventKind::Drag(left)), &mut buttons, cells);
        assert_eq!(drag.action, MouseAction::Move);
        assert!(drag.buttons.contains(MouseButton::Left));
        let up = mouse_event(at(MouseEventKind::Up(left)), &mut buttons, cells);
        assert_eq!(up.buttons, MouseButtons::default());
        let wheel = mouse_event(at(MouseEventKind::ScrollUp), &mut buttons, cells);
        assert_eq!(wheel.action, MouseAction::Wheel { dx: 0.0, dy: -1.0 });
    }

    #[test]
    fn capped_scale_fits_the_width_or_the_height() {
        assert_eq!(capped_scale(200.0, 100.0, Some((50, 50)), 1.0), 0.25);
        assert_eq!(capped_scale(100.0, 200.0, Some((200, 50)), 1.0), 0.25);
    }

    #[test]
    fn capped_scale_stops_at_the_cap() {
        assert_eq!(capped_scale(10.0, 10.0, Some((1000, 1000)), 1.0), 1.0);
        assert_eq!(capped_scale(10.0, 10.0, None, 0.5), 0.5);
    }

    #[test]
    fn half_blocks_cap_a_logical_pixel_at_a_screen_pixel() {
        // An 8 by 16 cell holds one pixmap pixel of 8 by 8 screen pixels.
        assert_eq!(
            max_scale_for_backend(Backend::TextBlocks, (8, 16)),
            1.0 / 8.0
        );
        assert_eq!(max_scale_for_backend(Backend::Kitty, (8, 16)), 1.0);
    }

    #[test]
    fn text_blocks_renders_some_pixels() {
        let pm = solid(4, 4, 255, 0, 0);
        let mut buf: Vec<u8> = Vec::new();
        render_text_blocks(&mut buf, &pm).expect("write ok");
        assert_eq!(rows(&buf), 2);
        // U+2580 in UTF-8.
        assert!(buf.windows(3).any(|w| w == [0xE2, 0x96, 0x80]));
    }

    #[test]
    fn text_blocks_uses_truecolor_codes() {
        let pm = solid(2, 2, 0, 0, 255);
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
        let pm = solid(3, 3, 255, 255, 255);
        let mut buf: Vec<u8> = Vec::new();
        render_text_blocks(&mut buf, &pm).expect("write ok");
        // ceil(3 / 2) rows.
        assert_eq!(rows(&buf), 2);
    }

    #[test]
    fn text_blocks_writes_one_row_for_one_pixel() {
        let pm = Pixmap::new(1, 1).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        render_text_blocks(&mut buf, &pm).expect("write ok");
        assert_eq!(rows(&buf), 1);
    }

    fn rows(buf: &[u8]) -> usize {
        buf.windows(2).filter(|w| w == b"\r\n").count()
    }

    #[test]
    fn a_key_release_sends_nothing() {
        let press = KeyEvent::new_with_kind(KeyCode::Up, KeyModifiers::NONE, KeyEventKind::Press);
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..press
        };
        assert_eq!(key_event(press).map(|k| k.kind), Some(KeyKind::Press));
        assert!(key_event(release).is_none());
    }

    #[test]
    fn every_key_name_of_the_terminal_is_in_key_all() {
        let codes = [
            KeyCode::Backspace,
            KeyCode::Enter,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Delete,
            KeyCode::Insert,
            KeyCode::Esc,
        ];
        for code in codes.into_iter().chain((1..=12).map(KeyCode::F)) {
            let name = key_code_to_string(code).expect("named");
            assert!(key::ALL.contains(&name.as_str()), "{name}");
        }
    }

    #[test]
    fn a_function_key_above_f12_sends_nothing() {
        assert_eq!(key_code_to_string(KeyCode::F(12)).as_deref(), Some("F12"));
        assert!(key_code_to_string(KeyCode::F(13)).is_none());
    }

    /// One test, since [`TTY`] is global.
    #[test]
    fn a_claim_holds_the_tty_and_the_panic_hook_restores_it_once() {
        assert_eq!(restore_held(), None);
        let claim = Claim::take().expect("free");
        assert!(matches!(Claim::take(), Err(OpenError::Busy)));
        claim.show_through(Backend::Kitty);
        assert!(!claim.restored());
        assert_eq!(restore_held(), Some(Backend::Kitty));
        assert!(claim.restored());
        assert_eq!(restore_held(), None);
        assert!(matches!(Claim::take(), Err(OpenError::Busy)));
        drop(claim);
        assert_eq!(restore_held(), None);
        assert!(Claim::take().is_ok());
    }

    #[test]
    fn kitty_names_the_frame_and_not_an_inline_image() {
        let pm = Pixmap::new(1, 1).unwrap();
        let mut framed = Vec::new();
        emit_kitty(&mut framed, &pm, Some(KITTY_ANIMATION_ID)).unwrap();
        assert!(
            String::from_utf8(framed)
                .unwrap()
                .starts_with("\x1b_Ga=T,f=32,s=1,v=1,q=2,m=0,i=1042;")
        );
        let mut inline = Vec::new();
        emit_kitty(&mut inline, &pm, None).unwrap();
        assert!(
            String::from_utf8(inline)
                .unwrap()
                .starts_with("\x1b_Ga=T,f=32,s=1,v=1,q=2,m=0;")
        );
    }

    #[test]
    fn ctrl_c_is_a_press_of_c_with_control() {
        let press = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(is_ctrl_c(&press));
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Release,
        );
        assert!(!is_ctrl_c(&release));
        assert!(!is_ctrl_c(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        )));
    }
}
