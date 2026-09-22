//! Terminal display of a [`crate::scene::Scene`] through the Kitty graphics
//! protocol, DEC Sixel or truecolor half-blocks. [`Terminal`] is a session
//! in the alt screen, where each frame replaces the previous one at (0, 0),
//! and a [`Printer`] prints images inline.
//!
//! The tty belongs to the process, so one `Terminal` exists at a time, and
//! a [`Printer`] prints nothing while it does. A terminal without the
//! keyboard protocol of Kitty or the win32-input-mode of Windows Terminal
//! does not tell a key down from a key up, so each key event there is a
//! press. With stdin redirected the display reads no keys and no mouse.

use std::fmt;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use crossterm::style::Print;
use crossterm::{cursor, execute, queue, terminal};
use tiny_skia::Pixmap;

use super::driver::{NoGraphics, OpenError, PresentError, period_from_hz, sealed};
use super::inbox::{Inbox, Next, Sender};
use super::sixel;
use crate::event::{Event, InputEvent, Interrupt, MouseEvent};
use crate::renderer::pixmap::{Assets, PixmapRenderer};
use crate::renderer::{AllocError, Renderer};
use crate::scene::Scene;
use crate::wire::ReadError;

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
    /// so an engine stops here the code that never calls `wait_event`. With
    /// stdin redirected no reader takes the keys, and Ctrl-C raises SIGINT
    /// instead.
    pub on_interrupt: Option<Box<dyn FnMut() + Send>>,
    /// Print the last frame on the main screen at close, where it stays
    /// with the rest of the output of the program. Without it the frame
    /// goes away with the alt screen.
    pub keep_last_frame: bool,
}

/// A [`super::Display`] over the alt screen of the terminal, in raw mode.
/// Ctrl-C arrives as [`Interrupt::Close`]. The size of the terminal
/// arrives as an [`InputEvent::Resize`] ahead of the first Vsync, and again
/// after each change. A mouse event gives the center of its cell.
pub struct Terminal {
    inbox: Inbox,
    canvas: Canvas,
    /// `None` after [`super::Display::close`].
    live: Option<Live>,
    /// The size in pixels of the frame on screen, or `None` before the
    /// first one. Kitty keeps a frame after the session.
    frame_size: Option<(u32, u32)>,
    /// The scene of the last present, drawn again after a resize.
    last: Option<Scene>,
    /// How the reader maps a cell to the scene on screen.
    cells: Arc<Mutex<CellMap>>,
    /// [`TerminalOptions::keep_last_frame`].
    keep_last_frame: bool,
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
        // The probe reads the replies from stdin, so it runs under the
        // claim, where no reader thread takes them.
        let backend = pick_backend().ok_or(NoGraphics)?;
        claim.show_through(backend);
        let stdin_tty = io::IsTerminal::is_terminal(&io::stdin());
        enter_raw_mode(stdin_tty).map_err(OpenError::Io)?;
        install_panic_hook();
        let keys = execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)
            .and_then(|()| KeyInput::start(stdin_tty));
        let mut inbox = Inbox::new(Some(Self::VSYNC_PERIOD));
        let cell = cell_pixels();
        if let Some((width, height)) = terminal::size().ok().and_then(|s| scene_size(s, cell)) {
            inbox.send_first(InputEvent::Resize { width, height });
        }
        let cells = Arc::new(Mutex::new(CellMap::before_frames(backend, cell)));
        let reader = keys.and_then(|keys| {
            let cells = Arc::clone(&cells);
            Reader::spawn(inbox.sender(), cells, keys, options.on_interrupt)
        });
        let reader = match reader {
            Ok(reader) => reader,
            Err(e) => {
                leave(backend, false);
                return Err(OpenError::Io(e));
            }
        };
        Ok(Self {
            inbox,
            canvas: Canvas::new(backend),
            live: Some(Live { reader, claim }),
            frame_size: None,
            last: None,
            cells,
            keep_last_frame: options.keep_last_frame,
        })
    }

    /// Rasterize `scene` and write the frame. `frame_size` takes the size
    /// once the frame is on screen, so a write that stops part way leaves
    /// it empty and the next frame clears and writes every cell.
    fn draw(&mut self, scene: &Scene) -> Result<(), PresentError> {
        let Canvas {
            backend,
            renderer,
            buffers,
        } = &mut self.canvas;
        let scale = scale_for_backend(*backend, scene.width(), scene.height());
        renderer.set_scale(scale);
        let pixmap = renderer.render(scene)?;
        *self.cells.lock().unwrap_or_else(PoisonError::into_inner) =
            CellMap::new(*backend, cell_pixels(), scale);
        let mut stdout = io::stdout().lock();
        // A smaller frame leaves the edges of the one before. The same Kitty
        // id replaces the whole image in place, and a clear, or a delete
        // before the transmit, shows the cleared cells for one refresh.
        let size = (pixmap.width(), pixmap.height());
        if self.frame_size.take() != Some(size) && *backend != Backend::Kitty {
            queue!(stdout, terminal::Clear(terminal::ClearType::All))?;
            buffers.blocks.forget();
        }
        queue!(stdout, cursor::MoveTo(0, 0))?;
        write_image(&mut stdout, *backend, pixmap, buffers, Placement::Frame)?;
        stdout.flush()?;
        self.frame_size = Some(size);
        Ok(())
    }

    /// Draw the last scene again, after a resize. The terminal may have
    /// moved or wrapped the cells of the old frame, so the screen clears.
    /// A failure reaches the caller at its next `present`, which draws the
    /// same scene.
    fn redraw(&mut self) {
        if let Some(scene) = self.last.take() {
            self.frame_size = None;
            let _ = self.draw(&scene);
            self.last = Some(scene);
        }
    }
}

impl super::Display for Terminal {
    fn present(&mut self, scene: Scene) -> Result<(), PresentError> {
        if self.live.is_none() {
            return Err(PresentError::Closed);
        }
        let drawn = self.draw(&scene);
        self.last = Some(scene);
        drawn
    }

    fn wait_event(&mut self, deadline: Option<Instant>) -> Result<Event, Interrupt> {
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

    /// Decode the asset as a PNG, whatever `mime` says.
    fn push_asset(
        &mut self,
        id: u32,
        blob: &[u8],
        _mime: Option<&str>,
    ) -> Result<(), PresentError> {
        if self.live.is_none() {
            return Err(PresentError::Closed);
        }
        self.canvas.renderer.assets_mut().insert_png(id, blob)?;
        Ok(())
    }

    /// Stop the reader thread, and leave the alt screen and raw mode.
    fn close(&mut self) {
        let Some(live) = self.live.take() else {
            return;
        };
        self.inbox.close();
        live.reader.stop();
        if !live.claim.restored() {
            leave(self.canvas.backend, self.frame_size.is_some());
            // The session is over, and close has no way to report that
            // the frame did not print.
            if self.keep_last_frame
                && let Some(scene) = &self.last
            {
                let _ = self.canvas.print(scene);
            }
        }
    }
}

impl sealed::Sealed for Terminal {}

impl Drop for Terminal {
    fn drop(&mut self) {
        super::Display::close(self);
    }
}

/// The bytes a frame needs.
#[derive(Default)]
struct ImageBuffers {
    /// The PNG that the Kitty protocol carries.
    image: Vec<u8>,
    /// The Sixel encoder, which keeps the pixels and the text of a frame.
    sixel: sixel::Encoder,
    /// The escapes that go to the terminal.
    escapes: Vec<u8>,
    /// The cells that a half-block frame compares against and replaces. A
    /// print writes every cell and leaves this alone.
    blocks: BlockScreen,
}

/// Why a print did not reach the terminal. A failure comes with the error
/// that caused it, since the library writes no message of its own.
#[derive(Debug)]
pub enum PrintError {
    /// A [`Terminal`] session holds the tty.
    Busy,
    /// Rasterizing the scene failed.
    Alloc(AllocError),
    /// The write stopped part way, which may leave part of the image on
    /// screen.
    Io(io::Error),
}

impl fmt::Display for PrintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrintError::Busy => f.write_str("a terminal session holds the tty"),
            PrintError::Alloc(e) => write!(f, "cannot draw the scene: {e}"),
            PrintError::Io(e) => write!(f, "cannot show the image: {e}"),
        }
    }
}

impl std::error::Error for PrintError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PrintError::Alloc(e) => Some(e),
            PrintError::Io(e) => Some(e),
            PrintError::Busy => None,
        }
    }
}

/// Prints scenes at the cursor, outside a session, through Kitty when the
/// terminal supports it, else Sixel, else half-blocks. It keeps the images
/// of the bitmaps and the buffers from one print to the next. A program that
/// draws one frame over another opens a [`Terminal`] instead.
pub struct Printer {
    canvas: Canvas,
}

impl Printer {
    /// Fails with [`NoGraphics`] when the terminal shows neither Kitty,
    /// Sixel nor truecolor graphics, or stdout is not a terminal. A REPL
    /// prints its values as text then. The probe runs once per process.
    pub fn new() -> Result<Self, NoGraphics> {
        let backend = pick_backend().ok_or(NoGraphics)?;
        Ok(Self {
            canvas: Canvas::new(backend),
        })
    }

    /// The images that a [`crate::scene::Bitmap`] of the next prints names.
    pub fn assets_mut(&mut self) -> &mut Assets {
        self.canvas.renderer.assets_mut()
    }

    /// Print `scene` at the cursor, and leave the cursor on the line below
    /// it.
    pub fn print(&mut self, scene: &Scene) -> Result<(), PrintError> {
        if TTY.load(Ordering::Acquire) != FREE {
            return Err(PrintError::Busy);
        }
        self.canvas.print(scene)
    }
}

/// What draws a scene into the terminal, for the frames of a [`Terminal`]
/// and the prints of a [`Printer`].
struct Canvas {
    backend: Backend,
    /// Kept across frames, so a frame reuses the pixmap and the clip masks.
    renderer: PixmapRenderer,
    /// Kept across frames, so a frame reuses the allocations.
    buffers: ImageBuffers,
}

impl Canvas {
    fn new(backend: Backend) -> Self {
        Self {
            backend,
            renderer: PixmapRenderer::default(),
            buffers: ImageBuffers::default(),
        }
    }

    /// Print `scene` at the cursor, with every cell of the image, and leave
    /// the cursor on the line below it.
    fn print(&mut self, scene: &Scene) -> Result<(), PrintError> {
        let scale = scale_for_backend(self.backend, scene.width(), scene.height());
        self.renderer.set_scale(scale);
        let pixmap = self.renderer.render(scene).map_err(PrintError::Alloc)?;
        let mut stdout = io::stdout().lock();
        write_image(
            &mut stdout,
            self.backend,
            pixmap,
            &mut self.buffers,
            Placement::Still,
        )
        .and_then(|()| stdout.flush())
        .map_err(PrintError::Io)
    }
}

/// Where an image goes.
#[derive(Clone, Copy, Debug)]
enum Placement {
    /// A frame of the session, which replaces the frame on screen.
    Frame,
    /// An image at the cursor, which stays until the terminal scrolls it
    /// away, with the cursor on the line below it.
    Still,
}

/// Write `pixmap` through `backend` at the cursor.
fn write_image<W: Write>(
    out: &mut W,
    backend: Backend,
    pixmap: &Pixmap,
    buffers: &mut ImageBuffers,
    placement: Placement,
) -> io::Result<()> {
    let kitty_id = match placement {
        Placement::Frame => Some(KITTY_ANIMATION_ID),
        Placement::Still => None,
    };
    match (backend, placement) {
        (Backend::Kitty, _) => emit_kitty(
            out,
            pixmap,
            kitty_id,
            &mut buffers.image,
            &mut buffers.escapes,
        )?,
        (Backend::Sixel, _) => {
            out.write_all(buffers.sixel.encode(pixmap, SIXEL_BACKGROUND)?)?;
        }
        (Backend::TextBlocks, Placement::Frame) => {
            return update_text_blocks(out, pixmap, &mut buffers.escapes, &mut buffers.blocks);
        }
        // Each row of the cells ends in a newline.
        (Backend::TextBlocks, Placement::Still) => {
            return render_text_blocks(out, pixmap, &mut buffers.escapes);
        }
    }
    match placement {
        Placement::Frame => Ok(()),
        Placement::Still => writeln!(out),
    }
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
            "ghostty" | "wezterm" | "konsole" | "vscode" | "iterm.app"
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

/// Enter raw mode. With stdin redirected nobody reads the keys, and Ctrl-C
/// has to raise SIGINT.
#[cfg(unix)]
fn enter_raw_mode(stdin_tty: bool) -> io::Result<()> {
    terminal::enable_raw_mode()?;
    if !stdin_tty && let Err(e) = super::term_query::signal_on_ctrl_c() {
        let _ = terminal::disable_raw_mode();
        return Err(e);
    }
    Ok(())
}

/// Enter raw mode. On Windows raw mode changes only the input of the
/// console, so with stdin redirected the console stays as it is, and Ctrl-C
/// keeps its signal.
#[cfg(windows)]
fn enter_raw_mode(stdin_tty: bool) -> io::Result<()> {
    if stdin_tty {
        terminal::enable_raw_mode()?;
    }
    Ok(())
}

/// Leave the alt screen and raw mode. Kitty keeps an image across the flip
/// of the alt screen, so a frame it shows goes by id. Sixel and half-blocks
/// output lives in the alt screen and goes with it.
fn leave(backend: Backend, frame_shown: bool) {
    let mut stdout = io::stdout().lock();
    if frame_shown && backend == Backend::Kitty {
        let _ = delete_kitty_image(&mut stdout, KITTY_ANIMATION_ID);
    }
    // A terminal ignores the pop of a protocol that it does not speak and
    // the reset of a mode that it does not know, so these go out whether or
    // not the session turned them on. Kitty keeps a stack of the keyboard
    // flags per screen, so the pop goes before the alt screen leaves. On
    // Windows, crossterm turns on the escapes of the console at the first
    // command that writes one, which Print does not, so Print goes second.
    let _ = execute!(
        stdout,
        cursor::Show,
        Print(INPUT_OFF),
        terminal::LeaveAlternateScreen
    );
    drop(stdout);
    // The input that nobody read would go to the shell, and so would a key
    // up or a mouse report that the terminal sent before it took the
    // escapes above. A redirected stdin belongs to the shell.
    if io::IsTerminal::is_terminal(&io::stdin()) {
        super::term_query::discard_input();
    }
    #[cfg(windows)]
    let _ = super::term_query::set_vt_input(false);
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

/// Report the buttons and every motion of the mouse, in the SGR form, or in
/// the rxvt form for a terminal without it.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h";

/// Report when the terminal loses the focus.
const FOCUS_ON: &str = "\x1b[?1004h";

/// Pop the keyboard protocol of Kitty, and turn off win32-input-mode and
/// the reports of the focus and of the mouse.
const INPUT_OFF: &str =
    "\x1b[<u\x1b[?9001l\x1b[?1004l\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

/// Who reads the keys of the terminal.
#[derive(Clone, Copy)]
enum KeyInput {
    /// [`super::vt_input`] reads the bytes of stdin, and `kitty` says that
    /// the terminal is under the keyboard protocol of Kitty.
    Bytes { kitty: bool },
    /// Nobody, because stdin is not a terminal. Ctrl-C raises SIGINT.
    Off,
}

impl KeyInput {
    /// Pick who reads the keys, and turn on the mouse for them. Push the
    /// keyboard protocol of Kitty when the terminal speaks it, and on
    /// Windows turn on win32-input-mode. Both report when a key comes up,
    /// so the terminal also reports the focus, whose loss brings up every
    /// key that is down.
    fn start(stdin_tty: bool) -> io::Result<Self> {
        if !stdin_tty {
            return Ok(KeyInput::Off);
        }
        #[cfg(unix)]
        let (kitty, keys_on) = if super::term_query::graphics_caps().kitty_keyboard {
            let push = format!("\x1b[>{}u{FOCUS_ON}", super::vt_input::FLAGS);
            (true, push)
        } else {
            (false, String::new())
        };
        #[cfg(windows)]
        let (kitty, keys_on) = {
            super::term_query::set_vt_input(true)?;
            (false, format!("\x1b[?9001h{FOCUS_ON}"))
        };
        let mut stdout = io::stdout();
        write!(stdout, "{MOUSE_ON}{keys_on}")?;
        stdout.flush()?;
        Ok(KeyInput::Bytes { kitty })
    }
}

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
        keys: KeyInput,
        on_interrupt: Option<Box<dyn FnMut() + Send>>,
    ) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("sinteract-terminal".into())
            .spawn(move || match keys {
                KeyInput::Bytes { kitty } => read_bytes(&tx, &cells, &flag, kitty, on_interrupt),
                KeyInput::Off => watch_size(&tx, &flag),
            })?;
        Ok(Self { stop, thread })
    }

    fn stop(self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.thread.join();
    }
}

/// Send the input of the terminal until `stop`, Ctrl-C or a read error,
/// with the keyboard protocol of Kitty when `kitty` holds. A Close goes
/// into the queue on every way out, so a reader that dies does not leave
/// the engine waiting in raw mode. A resize sends nothing through the tty,
/// so the reader compares the size at each turn.
fn read_bytes(
    tx: &Sender,
    cells: &Mutex<CellMap>,
    stop: &AtomicBool,
    kitty: bool,
    mut on_interrupt: Option<Box<dyn FnMut() + Send>>,
) {
    use super::vt_input::{Input, VtInput};

    let _close = CloseOnExit(tx);
    let mut tty = match super::term_query::TtyInput::open() {
        Ok(tty) => tty,
        Err(e) => {
            let _ = tx.send_read_error(ReadError::Broken(e));
            return;
        }
    };
    let mut keys = VtInput::new(kitty);
    let mut size = SizeWatch::default();
    let mut bytes = Vec::new();
    let mut inputs = Vec::new();
    while !stop.load(Ordering::Acquire) {
        if size.check(tx).is_err() {
            return;
        }
        if let Err(e) = tty.read(READ_POLL, &mut bytes) {
            let _ = tx.send_read_error(ReadError::Broken(e));
            return;
        }
        if bytes.is_empty() {
            keys.flush(&mut inputs);
        } else {
            keys.feed(&bytes, &mut inputs);
            bytes.clear();
        }
        for input in inputs.drain(..) {
            let sent = match input {
                Input::Key(k) => tx.send_input(InputEvent::Key(k)),
                Input::Mouse(m) => {
                    let cells = *cells.lock().unwrap_or_else(PoisonError::into_inner);
                    let (x, y) = cells.to_scene(m.column, m.row);
                    tx.send_input(InputEvent::Mouse(MouseEvent {
                        action: m.action,
                        x,
                        y,
                        modifiers: m.modifiers,
                        buttons: m.buttons,
                    }))
                }
                Input::Interrupt => {
                    let _ = tx.send_close();
                    if let Some(f) = on_interrupt.as_mut() {
                        f();
                    }
                    return;
                }
            };
            if sent.is_err() {
                return;
            }
        }
    }
}

/// Send the resizes until `stop`, for a terminal that has no reader of the
/// keys.
fn watch_size(tx: &Sender, stop: &AtomicBool) {
    let _close = CloseOnExit(tx);
    let mut size = SizeWatch::default();
    while !stop.load(Ordering::Acquire) {
        if size.check(tx).is_err() {
            return;
        }
        thread::sleep(READ_POLL);
    }
}

/// The size of the terminal at the last check.
struct SizeWatch(Option<(u16, u16)>);

impl Default for SizeWatch {
    fn default() -> Self {
        Self(terminal::size().ok())
    }
}

impl SizeWatch {
    /// Send a resize when the size changed since the last check.
    fn check(&mut self, tx: &Sender) -> Result<(), super::Closed> {
        let now = terminal::size().ok();
        if now == self.0 {
            return Ok(());
        }
        self.0 = now;
        now.map_or(Ok(()), |now| resized(tx, now))
    }
}

/// Send the size of the scene for a terminal of `(cols, rows)`, and draw
/// the last frame again.
fn resized(tx: &Sender, size: (u16, u16)) -> Result<(), super::Closed> {
    if let Some((width, height)) = scene_size(size, cell_pixels()) {
        let _ = tx.send_input(InputEvent::Resize { width, height });
    }
    tx.request_redraw()
}

struct CloseOnExit<'a>(&'a Sender);

impl Drop for CloseOnExit<'_> {
    fn drop(&mut self) {
        let _ = self.0.send_close();
    }
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
    // A cell whose colors repeat takes the three bytes of `▀`, which is the
    // common case in a drawing, and a cell row covers two pixel rows. A
    // frame of many colors grows the buffer once and keeps the room.
    buf.reserve(pixmap.width() as usize * pixmap.height() as usize * 2);
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
    fn a_printer_prints_nothing_while_a_session_holds_the_tty() {
        let mut printer = Printer {
            canvas: Canvas::new(Backend::TextBlocks),
        };
        let claim = Claim::take().expect("no session runs in a test");
        assert!(matches!(
            printer.print(&Scene::new(4.0, 4.0)),
            Err(PrintError::Busy)
        ));
        drop(claim);
    }

    /// The mark of each backend in what it writes, which the other two
    /// never write. A Kitty image is a graphics escape, a Sixel image a
    /// device control string, and half-blocks are cells of one character.
    const MARKS: [(Backend, &[u8]); 3] = [
        (Backend::Kitty, b"\x1b_Ga=T"),
        (Backend::Sixel, b"\x1bP"),
        (Backend::TextBlocks, "▀".as_bytes()),
    ];

    /// What `write_image` writes for a small blue image.
    fn dispatch(backend: Backend, placement: Placement) -> Vec<u8> {
        let pixmap = solid(4, 4, 0, 0, 255);
        let mut out: Vec<u8> = Vec::new();
        let mut buffers = ImageBuffers::default();
        write_image(&mut out, backend, &pixmap, &mut buffers, placement).expect("write ok");
        out
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn a_backend_writes_its_own_protocol_and_no_other() {
        for placement in [Placement::Frame, Placement::Still] {
            for (backend, _) in MARKS {
                let out = dispatch(backend, placement);
                for (other, mark) in MARKS {
                    assert_eq!(
                        contains(&out, mark),
                        other == backend,
                        "{backend:?} {placement:?} wrote the mark of {other:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_still_image_ends_in_one_newline_and_a_frame_ends_in_none() {
        for (backend, _) in MARKS {
            let still = dispatch(backend, Placement::Still);
            assert!(still.ends_with(b"\n"), "{backend:?} still");
            assert!(!still.ends_with(b"\n\n"), "{backend:?} still");
            let frame = dispatch(backend, Placement::Frame);
            assert!(!frame.ends_with(b"\n"), "{backend:?} frame");
        }
    }

    #[test]
    fn the_kitty_frame_carries_the_animation_id_and_the_still_image_none() {
        let id = format!(",i={KITTY_ANIMATION_ID};");
        assert!(contains(
            &dispatch(Backend::Kitty, Placement::Frame),
            id.as_bytes()
        ));
        assert!(!contains(
            &dispatch(Backend::Kitty, Placement::Still),
            b",i="
        ));
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
}
