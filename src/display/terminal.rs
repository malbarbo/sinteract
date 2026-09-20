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
use super::inbox::{Inbox, Next, Sender};
use super::sixel;
use crate::event::{
    Event, InputEvent, KeyKind, Modifiers, MouseAction, MouseButton, MouseButtons, MouseEvent,
    NoEvent, key,
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
/// Ctrl-C arrives as [`NoEvent::Close`]. The size of the terminal
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
    /// Kept across frames, so a frame reuses the allocations.
    buffers: ImageBuffers,
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
            buffers: ImageBuffers::default(),
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
            self.buffers.blocks.forget();
        }
        let _ = queue!(stdout, cursor::MoveTo(0, 0));
        let _ = match self.backend {
            Backend::Kitty => emit_kitty(
                &mut stdout,
                pixmap,
                Some(KITTY_ANIMATION_ID),
                &mut self.buffers.image,
                &mut self.buffers.escapes,
            ),
            Backend::Sixel => {
                sixel::encode(pixmap, SIXEL_BACKGROUND).and_then(|b| stdout.write_all(&b))
            }
            Backend::TextBlocks => update_text_blocks(
                &mut stdout,
                pixmap,
                &mut self.buffers.escapes,
                &mut self.buffers.blocks,
            ),
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

    fn wait_event(&mut self, deadline: Option<Instant>) -> Result<Event, NoEvent> {
        loop {
            match self.inbox.wait_with(deadline, Inbox::receive) {
                Next::Ready(ready) => return ready,
                Next::Redraw => self.redraw(),
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

/// The bytes a frame needs. Sixel builds its own bytes and leaves all
/// three empty.
#[derive(Default)]
struct ImageBuffers {
    /// The PNG that the Kitty protocol carries.
    image: Vec<u8>,
    /// The escapes that go to the terminal.
    escapes: Vec<u8>,
    /// The cells that a half-block frame compares against and replaces.
    /// [`show_image`] writes every cell and leaves this empty.
    blocks: BlockScreen,
}

/// Print `scene` at the cursor, through Kitty when the terminal supports
/// it, else Sixel, else half-blocks. Prints nothing while a [`Terminal`]
/// holds the tty, or when the terminal has no graphics. A program that
/// draws one frame after another opens a [`Terminal`], which keeps the
/// buffers and writes only what changed.
pub fn show_image(scene: &Scene) {
    if TTY.load(Ordering::Acquire) != FREE {
        eprintln!("[sinteract] a terminal session is open; not printing the image");
        return;
    }
    let Some(backend) = pick_backend() else {
        return;
    };
    let scale = scale_for_backend(backend, scene.width(), scene.height());
    let mut renderer = PixmapRenderer::default();
    let Some(pixmap) = rasterize(&mut renderer, scale, scene) else {
        return;
    };
    let mut buffers = ImageBuffers::default();
    let mut stdout = io::stdout().lock();
    let _ = match backend {
        Backend::Kitty => emit_kitty(
            &mut stdout,
            pixmap,
            None,
            &mut buffers.image,
            &mut buffers.escapes,
        )
        .and_then(|()| writeln!(stdout)),
        Backend::Sixel => sixel::encode(pixmap, SIXEL_BACKGROUND)
            .and_then(|b| stdout.write_all(&b))
            .and_then(|()| writeln!(stdout)),
        Backend::TextBlocks => render_text_blocks(&mut stdout, pixmap, &mut buffers.escapes),
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
            let _ = tx.send_close();
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
        let _ = self.0.send_close();
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
        KeyCode::F(n) => key::FUNCTION_KEYS
            .get(usize::from(n).checked_sub(1)?)
            .copied()?
            .into(),
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
/// the background, both composited over black. The frame goes out in one
/// write, because `out` is a `LineWriter` that would otherwise make a
/// syscall per cell row. `buf` holds the frame, and a caller that draws
/// again passes the same one.
fn render_text_blocks<W: Write>(out: &mut W, pixmap: &Pixmap, buf: &mut Vec<u8>) -> io::Result<()> {
    buf.clear();
    // A cell takes about 12 bytes once the repeated colors are left out, and
    // a cell row covers two pixel rows.
    buf.reserve(pixmap.width() as usize * pixmap.height() as usize * 6);
    // `chunks` panics on a size of 0, and a pixmap is never 0 pixels wide.
    let mut rows = pixmap.pixels().chunks(pixmap.width() as usize);
    while let Some(top) = rows.next() {
        let bottom = rows.next().unwrap_or_default();
        // The reset at the end of a row leaves the terminal with the default
        // colors, which no cell carries, so the first cell sets both.
        let mut shown = None;
        for (x, &t) in top.iter().enumerate() {
            let cell = Cell::new(t, bottom.get(x));
            push_cell(buf, cell, shown);
            shown = Some(cell);
        }
        // In raw mode a bare LF does not return the cursor to column 0.
        buf.extend_from_slice(b"\x1b[0m\r\n");
    }
    out.write_all(buf)
}

/// The half-block cells of the frame on screen, and of the one being
/// written.
#[derive(Default)]
struct BlockScreen {
    /// The cells that the terminal shows, row by row. Empty before the
    /// first frame and after the screen clears.
    shown: Vec<Cell>,
    /// The cells of one row of `shown`.
    cols: usize,
    /// The cells of the frame being written.
    next: Vec<Cell>,
}

impl BlockScreen {
    /// Forget what the terminal shows, so the next frame writes every cell.
    /// A clear, or anything else that paints over the frame, happens behind
    /// the screen.
    fn forget(&mut self) {
        self.shown.clear();
        self.cols = 0;
    }
}

/// Write the cells where `pixmap` differs from the frame that `screen`
/// holds, and keep `pixmap` as that frame. The cells start at (0, 0), so
/// only a caller that owns the whole screen may call this. `buf` holds the
/// escapes, and a caller that draws again passes the same one.
///
/// A frame of an animation changes a few percent of the cells, and the
/// cursor jumps over the rest.
fn update_text_blocks<W: Write>(
    out: &mut W,
    pixmap: &Pixmap,
    buf: &mut Vec<u8>,
    screen: &mut BlockScreen,
) -> io::Result<()> {
    let cols = pixmap.width() as usize;
    block_cells(pixmap, &mut screen.next);
    // A frame of another shape shares no cell with the one on screen.
    let shown = if screen.cols == cols && screen.shown.len() == screen.next.len() {
        screen.shown.as_slice()
    } else {
        &[]
    };
    buf.clear();
    // The reset at the end of a frame leaves the terminal with the default
    // colors, which no cell carries, so the first cell sets both.
    let mut state = None;
    // Where the cursor sits after the cell written last.
    let mut at = None;
    for (r, row) in screen.next.chunks(cols).enumerate() {
        let old = shown.get(r * cols..(r + 1) * cols).unwrap_or_default();
        for (x, &cell) in row.iter().enumerate() {
            if old.get(x) == Some(&cell) {
                continue;
            }
            if at != Some((r, x)) {
                write!(buf, "\x1b[{};{}H", r + 1, x + 1)?;
            }
            push_cell(buf, cell, state);
            state = Some(cell);
            at = Some((r, x + 1));
        }
    }
    buf.extend_from_slice(b"\x1b[0m");
    if let Err(e) = out.write_all(buf) {
        // Part of the frame reached the terminal, and which part is unknown.
        screen.forget();
        return Err(e);
    }
    std::mem::swap(&mut screen.shown, &mut screen.next);
    screen.cols = cols;
    Ok(())
}

/// The half-block cells of `pixmap`, row by row, in `out`.
fn block_cells(pixmap: &Pixmap, out: &mut Vec<Cell>) {
    out.clear();
    out.reserve(pixmap.width() as usize * pixmap.height().div_ceil(2) as usize);
    // `chunks` panics on a size of 0, and a pixmap is never 0 pixels wide.
    let mut rows = pixmap.pixels().chunks(pixmap.width() as usize);
    while let Some(top) = rows.next() {
        let bottom = rows.next().unwrap_or_default();
        out.extend(
            top.iter()
                .enumerate()
                .map(|(x, &t)| Cell::new(t, bottom.get(x))),
        );
    }
}

/// The two colors of a half-block cell. The upper pixel of the pair is the
/// foreground and the lower one is the background.
#[derive(Clone, Copy, PartialEq)]
struct Cell {
    fg: [u8; 3],
    bg: [u8; 3],
}

impl Cell {
    /// The cell of the pixel `top` above the pixel `bottom`, both
    /// composited over black. An odd height leaves the last cell row with
    /// nothing below, which is black.
    fn new(
        top: tiny_skia::PremultipliedColorU8,
        bottom: Option<&tiny_skia::PremultipliedColorU8>,
    ) -> Self {
        Self {
            fg: blend_on_black(top),
            bg: bottom.map_or(BLACK, |&b| blend_on_black(b)),
        }
    }
}

const BLACK: [u8; 3] = [0, 0, 0];

/// Write `cell`, with an SGR only for a color that `shown` does not already
/// hold. A drawing repeats a color across neighboring cells, and a cell that
/// changes neither color costs three bytes instead of thirty-six.
fn push_cell(buf: &mut Vec<u8>, cell: Cell, shown: Option<Cell>) {
    let (fg, bg) = match shown {
        Some(shown) => (shown.fg != cell.fg, shown.bg != cell.bg),
        None => (true, true),
    };
    // One SGR for both colors is shorter and leaves no partial state if the
    // write stops.
    if fg {
        buf.extend_from_slice(b"\x1b[38;2;");
        push_channels(buf, cell.fg);
        if bg {
            buf.extend_from_slice(b";48;2;");
            push_channels(buf, cell.bg);
        }
        buf.push(b'm');
    } else if bg {
        buf.extend_from_slice(b"\x1b[48;2;");
        push_channels(buf, cell.bg);
        buf.push(b'm');
    }
    buf.extend_from_slice("▀".as_bytes());
}

/// The three channels of an SGR color, separated by `;`. A `write!` here
/// costs ten times what the digits cost, because it formats at run time.
fn push_channels(buf: &mut Vec<u8>, color: [u8; 3]) {
    for (i, v) in color.into_iter().enumerate() {
        if i > 0 {
            buf.push(b';');
        }
        if v >= 100 {
            buf.push(b'0' + v / 100);
        }
        if v >= 10 {
            buf.push(b'0' + (v / 10) % 10);
        }
        buf.push(b'0' + v % 10);
    }
}

fn blend_on_black(p: tiny_skia::PremultipliedColorU8) -> [u8; 3] {
    // The pixel is premultiplied, so compositing over black changes nothing.
    [p.red(), p.green(), p.blue()]
}

// -----------------------------------------------------------------------------
// Kitty graphics protocol I/O
// -----------------------------------------------------------------------------

/// The pixels of one conversion, and of one chunk of the escapes. Their
/// bytes are a multiple of 3, so base64 turns them into the 4096 bytes that
/// the protocol allows, with no padding inside.
const CHUNK_PIXELS: usize = 768;
const CHUNK_BYTES: usize = CHUNK_PIXELS * 4;

/// Write the Kitty escape sequences that show `pixmap` at the cursor, as a
/// PNG in chunks. An image with an `id` replaces the image of that id, and
/// one without stays until the terminal scrolls it away. `image` takes the
/// PNG and `buf` the escapes, and a caller that draws again passes the same
/// two.
fn emit_kitty<W: Write>(
    w: &mut W,
    pixmap: &Pixmap,
    id: Option<u32>,
    image: &mut Vec<u8>,
    buf: &mut Vec<u8>,
) -> io::Result<()> {
    encode_png(pixmap, image)?;
    buf.clear();
    buf.reserve(image.len() * 4 / 3 + image.len() / CHUNK_BYTES * 16 + 64);
    let mut encoded = [0u8; CHUNK_BYTES / 3 * 4];
    let total_chunks = image.len().div_ceil(CHUNK_BYTES).max(1);
    for (idx, chunk) in image.chunks(CHUNK_BYTES).enumerate() {
        let more: u8 = if idx + 1 < total_chunks { 1 } else { 0 };
        if idx == 0 {
            // The PNG header carries the size, so s and v say nothing here.
            write!(buf, "\x1b_Ga=T,f=100,q=2,m={}", more)?;
            if let Some(id) = id {
                write!(buf, ",i={id}")?;
            }
            buf.push(b';');
        } else {
            write!(buf, "\x1b_Gm={},q=2;", more)?;
        }
        let n = B64
            .encode_slice(chunk, &mut encoded)
            .expect("a chunk encodes into four bytes per three");
        buf.extend_from_slice(encoded.get(..n).expect("encode_slice fills a prefix"));
        buf.extend_from_slice(b"\x1b\\");
    }
    w.write_all(buf)
}

/// Encode `pixmap` into `out` as a PNG with straight alpha.
///
/// The terminal reads the raw pixels of a frame of 960 by 540 in 95 ms,
/// because they travel in base64, and the same frame as a PNG in 1 ms. The
/// adaptive filter is what earns it, since it turns a flat area into a row
/// of zeros. `Balanced` shrinks the frame four times more and costs ten
/// times the encoding, which the write does not give back.
fn encode_png(pixmap: &Pixmap, out: &mut Vec<u8>) -> io::Result<()> {
    out.clear();
    let mut encoder = png::Encoder::new(&mut *out, pixmap.width(), pixmap.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    encoder.set_filter(png::Filter::Adaptive);
    let mut writer = encoder.write_header().map_err(io::Error::other)?;
    let mut rows = writer.stream_writer().map_err(io::Error::other)?;
    let mut straight = [0u8; CHUNK_BYTES];
    for chunk in pixmap.pixels().chunks(CHUNK_PIXELS) {
        rows.write_all(straight_alpha(chunk, &mut straight))?;
    }
    rows.finish().map_err(io::Error::other)?;
    writer.finish().map_err(io::Error::other)
}

/// The pixels of `chunk` in `out`, with the premultiplication undone. PNG
/// holds straight alpha, so a premultiplied pixel would take its alpha a
/// second time and come out dark.
fn straight_alpha<'a>(chunk: &[tiny_skia::PremultipliedColorU8], out: &'a mut [u8]) -> &'a [u8] {
    let (groups, _) = out.as_chunks_mut::<4>();
    for (px, p) in groups.iter_mut().zip(chunk) {
        let c = p.demultiply();
        *px = [c.red(), c.green(), c.blue(), c.alpha()];
    }
    out.get(..chunk.len() * 4)
        .expect("a chunk holds at most the pixels of the buffer")
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
        render_text_blocks(&mut buf, &pm, &mut Vec::new()).expect("write ok");
        assert_eq!(rows(&buf), 2);
        // U+2580 in UTF-8.
        assert!(buf.windows(3).any(|w| w == [0xE2, 0x96, 0x80]));
    }

    #[test]
    fn text_blocks_uses_truecolor_codes() {
        let pm = solid(2, 2, 0, 0, 255);
        let mut buf: Vec<u8> = Vec::new();
        render_text_blocks(&mut buf, &pm, &mut Vec::new()).expect("write ok");
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("\x1b[38;2;"), "missing 24-bit fg SGR: {s:?}");
        assert!(s.contains(";48;2;"), "missing 24-bit bg SGR: {s:?}");
        assert!(s.contains("\x1b[0m"), "missing reset: {s:?}");
    }

    #[test]
    fn text_blocks_sets_a_color_once_for_a_run_of_cells() {
        let pm = solid(8, 2, 0, 0, 255);
        let mut buf: Vec<u8> = Vec::new();
        render_text_blocks(&mut buf, &pm, &mut Vec::new()).expect("write ok");
        let s = String::from_utf8_lossy(&buf);
        assert_eq!(s.matches("\x1b[38;2;").count(), 1, "one fg SGR: {s:?}");
        assert_eq!(s.matches("48;2;").count(), 1, "one bg SGR: {s:?}");
        assert_eq!(s.matches('▀').count(), 8);
    }

    #[test]
    fn text_blocks_handles_odd_height() {
        // The last cell row has no bottom pixel and takes black.
        let pm = solid(3, 3, 255, 255, 255);
        let mut buf: Vec<u8> = Vec::new();
        render_text_blocks(&mut buf, &pm, &mut Vec::new()).expect("write ok");
        // ceil(3 / 2) rows.
        assert_eq!(rows(&buf), 2);
    }

    #[test]
    fn text_blocks_writes_one_row_for_one_pixel() {
        let pm = Pixmap::new(1, 1).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        render_text_blocks(&mut buf, &pm, &mut Vec::new()).expect("write ok");
        assert_eq!(rows(&buf), 1);
    }

    fn rows(buf: &[u8]) -> usize {
        buf.windows(2).filter(|w| w == b"\r\n").count()
    }

    /// The frame that `update_text_blocks` writes for `pixmap`, and the
    /// cells it holds.
    fn update(screen: &mut BlockScreen, pixmap: &Pixmap) -> (Vec<u8>, usize) {
        let mut out: Vec<u8> = Vec::new();
        update_text_blocks(&mut out, pixmap, &mut Vec::new(), screen).expect("write ok");
        let cells = String::from_utf8_lossy(&out).matches('▀').count();
        (out, cells)
    }

    #[test]
    fn a_repeated_half_block_frame_writes_no_cell() {
        let pm = solid(4, 2, 0, 0, 255);
        let mut screen = BlockScreen::default();
        assert_eq!(update(&mut screen, &pm).1, 4);
        assert_eq!(update(&mut screen, &pm).0, b"\x1b[0m");
    }

    #[test]
    fn a_half_block_frame_writes_the_cell_that_changed() {
        let mut screen = BlockScreen::default();
        assert_eq!(update(&mut screen, &solid(4, 2, 0, 0, 255)).1, 4);
        let mut pm = solid(4, 2, 0, 0, 255);
        pm.pixels_mut()[1] = tiny_skia::ColorU8::from_rgba(255, 0, 0, 255).premultiply();
        let (out, cells) = update(&mut screen, &pm);
        assert_eq!(cells, 1);
        // Row 1, column 2 of the terminal.
        assert!(
            String::from_utf8_lossy(&out).contains("\x1b[1;2H"),
            "missing the jump: {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn a_forgotten_half_block_screen_writes_every_cell() {
        let pm = solid(4, 2, 0, 0, 255);
        let mut screen = BlockScreen::default();
        assert_eq!(update(&mut screen, &pm).1, 4);
        screen.forget();
        assert_eq!(update(&mut screen, &pm).1, 4);
    }

    #[test]
    fn a_half_block_frame_of_another_shape_writes_every_cell() {
        let mut screen = BlockScreen::default();
        // Both hold four cells, in one row of four and in two rows of two.
        assert_eq!(update(&mut screen, &solid(4, 2, 0, 0, 255)).1, 4);
        assert_eq!(update(&mut screen, &solid(2, 4, 0, 0, 255)).1, 4);
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

    /// The pixels of the image that `escapes` carries.
    fn decode_kitty(escapes: Vec<u8>, header: &str) -> Vec<u8> {
        let escapes = String::from_utf8(escapes).unwrap();
        let payload = escapes
            .strip_prefix(header)
            .expect("the frame starts with the header")
            .trim_end_matches("\x1b\\");
        let png = B64.decode(payload).unwrap();
        let mut reader = png::Decoder::new(io::Cursor::new(png)).read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        pixels.truncate(info.buffer_size());
        pixels
    }

    #[test]
    fn kitty_names_the_frame_and_not_an_inline_image() {
        let pm = Pixmap::new(1, 1).unwrap();
        let mut framed = Vec::new();
        emit_kitty(
            &mut framed,
            &pm,
            Some(KITTY_ANIMATION_ID),
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            decode_kitty(framed, "\x1b_Ga=T,f=100,q=2,m=0,i=1042;"),
            [0, 0, 0, 0]
        );
        let mut inline = Vec::new();
        emit_kitty(&mut inline, &pm, None, &mut Vec::new(), &mut Vec::new()).unwrap();
        assert_eq!(
            decode_kitty(inline, "\x1b_Ga=T,f=100,q=2,m=0;"),
            [0, 0, 0, 0]
        );
    }

    #[test]
    fn a_kitty_pixel_goes_out_with_straight_alpha() {
        let mut pm = Pixmap::new(1, 1).unwrap();
        let half_red = tiny_skia::ColorU8::from_rgba(255, 0, 0, 128);
        pm.pixels_mut().fill(half_red.premultiply());
        let mut out = Vec::new();
        emit_kitty(&mut out, &pm, None, &mut Vec::new(), &mut Vec::new()).unwrap();
        assert_eq!(
            decode_kitty(out, "\x1b_Ga=T,f=100,q=2,m=0;"),
            [255, 0, 0, 128]
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
