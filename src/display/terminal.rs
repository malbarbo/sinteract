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
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use crossterm::style::Print;
use crossterm::{cursor, execute, queue, terminal};
use tiny_skia::{Pixmap, PremultipliedColorU8};

use super::driver::{NoGraphics, OpenError, PresentError, sealed};
use super::inbox::{Inbox, Next, Sender};
use super::sixel;
use super::tick_clock::TickClock;
use crate::event::{Event, Interrupt, MouseEvent};
use crate::renderer::pixmap::PixmapRenderer;
use crate::renderer::{AllocError, Renderer};
use crate::scene::{Rgba, Scene};

const KITTY_ANIMATION_ID: u32 = 1042;

// Fallback when the terminal does not answer the `CSI 16 t` probe.
const CELL_W_DEFAULT: u32 = 8;
const CELL_H_DEFAULT: u32 = 16;

/// Sixel has no transparency that keeps the previous frame, so a frame
/// starts from this.
const SIXEL_BACKGROUND: Rgba = Rgba {
    r: 255,
    g: 255,
    b: 255,
    a: 255,
};

/// What an engine adds to [`Terminal::open_with`].
#[derive(Clone, Copy, Debug, Default)]
pub struct TerminalOptions {
    /// Runs on the reader thread when the user presses Ctrl-C, after the
    /// Close goes into the queue. Raw mode turns off the signal of Ctrl-C,
    /// so an engine stops here the code that never calls `wait_event`. With
    /// stdin redirected no reader takes the keys, and Ctrl-C raises SIGINT
    /// instead.
    pub on_interrupt: Option<fn()>,
    /// Print the last frame on the main screen at close, where it stays
    /// with the rest of the output of the program. Without it the frame
    /// goes away with the alt screen.
    pub keep_last_frame: bool,
    /// The rows of text under the frame, which [`Terminal::set_footer`]
    /// fills. The frame shrinks to leave them free.
    pub footer_rows: u16,
    /// The graphics of the frames. `None` asks the terminal which ones it
    /// shows.
    pub graphics: Option<TerminalGraphics>,
}

/// A [`super::Display`] over the alt screen of the terminal, in raw mode.
/// Ctrl-C arrives as [`Interrupt::Close`]. The size of the terminal
/// arrives as an [`InputEvent::Resize`](crate::event::InputEvent::Resize)
/// ahead of the first tick, and again after each change. A mouse event
/// gives the center of its cell.
///
/// A thread of the session writes each frame while the next one renders.
/// So a write that fails reports at the next
/// [`present`](super::Display::present), and the failure of the last frame
/// does not report. The thread takes the lock of stdout for each frame, so
/// a caller that holds the lock across a `present` waits forever.
pub struct Terminal {
    inbox: Inbox,
    clock: TickClock,
    /// The pixmap of the next frame, the clip masks and the images of the
    /// bitmaps.
    renderer: PixmapRenderer,
    graphics: TerminalGraphics,
    /// `None` after [`super::Display::close`].
    active: Option<Active>,
    /// The scene of the last present, drawn again after a resize.
    last: Option<Scene>,
    /// How the reader maps a cell to the scene on screen.
    cells: Arc<Mutex<CellMap>>,
    /// [`TerminalOptions::keep_last_frame`].
    keep_last_frame: bool,
    /// The text under the frame, which the writer of the frames also draws.
    footer: Arc<Mutex<Footer>>,
}

/// What a session holds until it closes.
struct Active {
    reader: Reader,
    writer: FrameWriter<FrameState>,
    claim: Claim,
}

impl Terminal {
    /// There is no hardware refresh in a terminal. 60 Hz is smooth for
    /// half-block animation and does not flood the pty with escape codes.
    const TICK_RATE: NonZeroU32 = NonZeroU32::new(60_000).expect("60 Hz is not zero");

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
        let graphics = options.graphics.or_else(pick_graphics).ok_or(NoGraphics)?;
        claim.show_through(graphics);
        let stdin_tty = io::IsTerminal::is_terminal(&io::stdin());
        enter_raw_mode(stdin_tty).map_err(OpenError::Io)?;
        install_panic_hook();
        let keys = execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)
            .and_then(|()| KeyInput::start(stdin_tty));
        let inbox = Inbox::new(None);
        let cell = cell_pixels();
        let footer_rows = options.footer_rows;
        let size = terminal::size().ok();
        if let Some((width, height)) = size.and_then(|s| scene_size(s, cell, footer_rows)) {
            let _ = inbox.sender().send_resize(width, height);
        }
        let cells = Arc::new(Mutex::new(CellMap::before_frames(graphics, cell)));
        let reader = keys.and_then(|keys| {
            let cells = Arc::clone(&cells);
            Reader::spawn(
                inbox.sender(),
                cells,
                keys,
                footer_rows,
                options.on_interrupt,
            )
        });
        let reader = match reader {
            Ok(reader) => reader,
            Err(e) => {
                leave(graphics, false);
                return Err(OpenError::Io(e));
            }
        };
        let Printer {
            renderer,
            bytes,
            painter,
        } = Printer::with_painter(Painter::for_stdout(graphics));
        let footer = Arc::new(Mutex::new(Footer::new(footer_rows)));
        let state = FrameState {
            painter,
            bytes,
            frame_size: None,
            footer: Arc::clone(&footer),
        };
        let writer = match FrameWriter::spawn(state, FrameState::write_frame) {
            Ok(writer) => writer,
            Err(e) => {
                reader.stop();
                leave(graphics, false);
                return Err(OpenError::Io(e));
            }
        };
        Ok(Self {
            inbox,
            clock: TickClock::from_millihertz(Self::TICK_RATE),
            renderer,
            graphics,
            active: Some(Active {
                reader,
                writer,
                claim,
            }),
            last: None,
            cells,
            keep_last_frame: options.keep_last_frame,
            footer,
        })
    }

    /// Show `lines` in the rows of [`TerminalOptions::footer_rows`] under
    /// the frame. Only the last lines fit, a line wider than the terminal
    /// loses its end, and a control character shows as a space. The text
    /// waits for the first frame, and goes away with the alt screen at
    /// close.
    pub fn set_footer<S: AsRef<str>>(&mut self, lines: &[S]) -> Result<(), PresentError> {
        if self.active.is_none() {
            return Err(PresentError::Closed);
        }
        // The writer of the frames takes the two locks in the same order.
        let mut stdout = io::stdout().lock();
        let mut footer = self.footer.lock().unwrap_or_else(PoisonError::into_inner);
        footer.set(lines);
        if TTY.load(Ordering::Acquire) == RESTORED {
            return Ok(());
        }
        let Some(top) = footer.top else {
            return Ok(());
        };
        footer.write(&mut stdout, top, screen_cols())?;
        stdout.flush()?;
        Ok(())
    }

    /// Rasterize `scene` and hand the frame to the writer, which clears the
    /// screen first when `after_resize` holds. Returns the result of the
    /// write of the frame before.
    fn draw(&mut self, scene: &Scene, after_resize: bool) -> Result<(), PresentError> {
        let Some(active) = self.active.as_mut() else {
            return Err(PresentError::Closed);
        };
        let footer_rows = self
            .footer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .rows;
        let scale = scale_for_graphics(self.graphics, scene.width(), scene.height(), footer_rows);
        self.renderer.set_scale(scale);
        self.renderer.render(scene)?;
        *self.cells.lock().unwrap_or_else(PoisonError::into_inner) =
            CellMap::new(self.graphics, cell_pixels(), scale);
        active.writer.write(&mut self.renderer, after_resize)?;
        Ok(())
    }

    /// Draw the last scene again, after a resize. The terminal may have
    /// moved or wrapped the cells of the old frame, so the screen clears.
    /// The writer keeps a failed write for the next `present`, which also
    /// renders again after a failed render.
    fn redraw(&mut self) {
        if let Some(scene) = self.last.take() {
            let _ = self.draw(&scene, true);
            self.last = Some(scene);
        }
    }
}

impl super::Display for Terminal {
    fn present(&mut self, scene: Scene) -> Result<(), PresentError> {
        if self.active.is_none() {
            return Err(PresentError::Closed);
        }
        // A resize that waits for its redraw still clears the screen.
        let after_resize = self.inbox.take_redraw();
        let drawn = self.draw(&scene, after_resize);
        self.last = Some(scene);
        drawn
    }

    fn wait_event(&mut self, deadline: Option<Instant>) -> Result<Event, Interrupt> {
        loop {
            match self.inbox.next(&mut self.clock, deadline) {
                Next::Ready(ready) => return ready.map(Event::from),
                Next::Redraw => self.redraw(),
                Next::Block(timeout) => self.inbox.wait_for_message(timeout),
            }
        }
    }

    fn sender(&self) -> Sender {
        self.inbox.sender()
    }

    fn tick_rate(&self) -> NonZeroU32 {
        self.clock.millihertz()
    }

    fn footer_rows(&self) -> u16 {
        self.footer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .rows
    }

    fn set_footer(&mut self, lines: &[String]) -> Result<(), PresentError> {
        Terminal::set_footer(self, lines)
    }

    /// Stop the reader thread, wait for the frame that the writer holds,
    /// and leave the alt screen and raw mode.
    fn close(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        self.inbox.close();
        active.reader.stop();
        // A writer that panicked hands back nothing, and the panic hook
        // already put the tty back.
        let Some(state) = active.writer.finish() else {
            return;
        };
        if !active.claim.restored() {
            leave(self.graphics, state.frame_size.is_some());
            // The session is over, and close has no way to report that
            // the frame did not print.
            if self.keep_last_frame
                && let Some(scene) = &self.last
            {
                let footer_rows = self.footer_rows();
                let mut printer = Printer {
                    renderer: std::mem::take(&mut self.renderer),
                    bytes: state.bytes,
                    painter: state.painter,
                };
                let _ = printer.print_unchecked(scene, footer_rows);
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

/// Writes the frames of a [`Terminal`] from a thread of its own, so the
/// next frame renders while this one goes to the terminal. There are two
/// pixmaps, one in the renderer and one with the thread, so at most one
/// frame waits for its write. `S` is the state of the write, and the thread
/// owns it.
struct FrameWriter<S> {
    frames: mpsc::Sender<Frame>,
    written: mpsc::Receiver<Written>,
    /// `None` after a panic of the thread went on in the caller.
    thread: Option<JoinHandle<S>>,
    /// The failed write of the frame before a redraw, which has no caller
    /// to report to.
    unreported: Option<io::Error>,
}

/// A frame for the thread of a [`FrameWriter`].
struct Frame {
    pixmap: Pixmap,
    after_resize: bool,
}

/// A frame back from the thread, with the result of its write.
struct Written {
    pixmap: Pixmap,
    result: io::Result<()>,
}

impl<S: Send + 'static> FrameWriter<S> {
    /// Start the thread, which calls `write` with each frame.
    fn spawn(mut state: S, write: fn(&mut S, &Pixmap, bool) -> io::Result<()>) -> io::Result<Self> {
        let (frames, to_write) = mpsc::channel::<Frame>();
        let (done, written) = mpsc::channel();
        // The first write gives this pixmap to the renderer, and the next
        // render resizes it.
        done.send(Written {
            pixmap: Pixmap::new(1, 1).expect("a 1x1 pixmap always allocates"),
            result: Ok(()),
        })
        .expect("the receiver is in scope");
        let thread = thread::Builder::new()
            .name("sinteract-frames".into())
            .spawn(move || {
                for frame in to_write {
                    let result = write(&mut state, &frame.pixmap, frame.after_resize);
                    let back = Written {
                        pixmap: frame.pixmap,
                        result,
                    };
                    // A FrameWriter that drops without finish takes no
                    // pixmap back.
                    let _ = done.send(back);
                }
                state
            })?;
        Ok(Self {
            frames,
            written,
            thread: Some(thread),
            unreported: None,
        })
    }

    /// Wait for the write of the frame before, hand the last render of
    /// `renderer` to the thread, and give the pixmap of the frame before to
    /// the renderer. The thread clears the screen first when `after_resize`
    /// holds. Returns the result of the write of the frame before, except
    /// after a resize, where the error waits for the next call.
    fn write(&mut self, renderer: &mut PixmapRenderer, after_resize: bool) -> io::Result<()> {
        let Ok(Written { pixmap, result }) = self.written.recv() else {
            self.resume_panic();
        };
        let frame = Frame {
            pixmap: renderer.replace_pixmap(pixmap),
            after_resize,
        };
        if self.frames.send(frame).is_err() {
            self.resume_panic();
        }
        let result = match self.unreported.take() {
            Some(e) => Err(e),
            None => result,
        };
        if after_resize {
            self.unreported = result.err();
            return Ok(());
        }
        result
    }

    /// Go on with the panic of the thread, the only way that the thread
    /// ends before finish.
    fn resume_panic(&mut self) -> ! {
        let thread = self.thread.take().expect("the thread panics once");
        let payload = thread
            .join()
            .map(|_| ())
            .expect_err("the thread of the frames ends before finish only by a panic");
        std::panic::resume_unwind(payload)
    }

    /// Wait for the thread to write the frame it holds, and take back `S`.
    /// `None` when the thread panicked.
    fn finish(self) -> Option<S> {
        drop(self.frames);
        self.thread?.join().ok()
    }
}

/// What the writer of a [`Terminal`] keeps from one frame to the next.
struct FrameState {
    painter: Painter,
    /// Where the painter builds the escapes of a frame before the write.
    bytes: Vec<u8>,
    /// The size in pixels of the frame on screen, or `None` before the
    /// first one. Kitty keeps a frame after the session.
    frame_size: Option<(u32, u32)>,
    /// The text under the frame, drawn again after each frame.
    footer: Arc<Mutex<Footer>>,
}

impl FrameState {
    /// Write `pixmap` to stdout, unless the panic hook put the tty back,
    /// since the frame would then print over the shell. The check holds the
    /// lock of stdout, so a panic hook that puts the tty back waits for
    /// this frame or makes it skip.
    fn write_frame(&mut self, pixmap: &Pixmap, after_resize: bool) -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        if TTY.load(Ordering::Acquire) == RESTORED {
            return Ok(());
        }
        self.write_to(&mut stdout, pixmap, after_resize)
    }

    /// Write `pixmap` over the frame on screen. `frame_size` takes the size
    /// once the frame is on screen, so a write that stops part way leaves
    /// it empty and the next frame clears and writes every cell.
    fn write_to<W: Write>(
        &mut self,
        out: &mut W,
        pixmap: &Pixmap,
        after_resize: bool,
    ) -> io::Result<()> {
        if after_resize {
            self.frame_size = None;
        }
        // A smaller frame leaves the edges of the one before.
        let size = (pixmap.width(), pixmap.height());
        let mut footer = self.footer.lock().unwrap_or_else(PoisonError::into_inner);
        if self.frame_size.take() != Some(size) && self.painter.clear_old_frame(out)? {
            footer.top = None;
        }
        queue!(out, cursor::MoveTo(0, 0))?;
        self.painter
            .write_image(out, &mut self.bytes, pixmap, Placement::Frame)?;
        let top = frame_rows(self.painter.graphics(), pixmap.height(), cell_pixels().1);
        footer.write(out, top, screen_cols())?;
        out.flush()?;
        self.frame_size = Some(size);
        Ok(())
    }
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
///
/// Every field lives from one print to the next, so a print reuses the
/// allocations. A [`Terminal`] takes the fields for its frames and puts
/// them back into a printer to print its last frame at close.
pub struct Printer {
    /// The pixmap, the clip masks and the images of the bitmaps.
    renderer: PixmapRenderer,
    /// Where the painter builds the escapes of an image before the write.
    bytes: Vec<u8>,
    painter: Painter,
}

impl Printer {
    /// Fails with [`NoGraphics`] when the terminal shows neither Kitty,
    /// Sixel nor truecolor graphics, or stdout is not a terminal. A REPL
    /// prints its values as text then. The probe runs once per process.
    pub fn new() -> Result<Self, NoGraphics> {
        let graphics = pick_graphics().ok_or(NoGraphics)?;
        Ok(Self::with_graphics(graphics))
    }

    /// Prints through `graphics`, whatever the terminal shows.
    pub fn with_graphics(graphics: TerminalGraphics) -> Self {
        Self::with_painter(Painter::for_stdout(graphics))
    }

    /// Print `scene` at the cursor, and leave the cursor on the line below
    /// it.
    pub fn print(&mut self, scene: &Scene) -> Result<(), PrintError> {
        if TTY.load(Ordering::Acquire) != FREE {
            return Err(PrintError::Busy);
        }
        self.print_unchecked(scene, 0)
    }

    fn with_painter(painter: Painter) -> Self {
        let mut renderer = PixmapRenderer::default();
        match painter.graphics() {
            TerminalGraphics::Sixel => renderer.set_background(SIXEL_BACKGROUND),
            // Kitty shows transparency, and a half-block cell reads a
            // premultiplied pixel as the pixel over black.
            TerminalGraphics::Kitty | TerminalGraphics::HalfBlocks => {}
        }
        Self {
            renderer,
            bytes: Vec::new(),
            painter,
        }
    }

    /// Print `scene` at the cursor, with every cell of the image, and leave
    /// the cursor on the line below it, even while a session holds the tty.
    /// A [`Terminal`] prints its last frame at close with it, while it still
    /// holds the tty, and passes its `footer_rows` so the frame keeps the
    /// size it had on the alt screen.
    fn print_unchecked(&mut self, scene: &Scene, footer_rows: u16) -> Result<(), PrintError> {
        let scale = scale_for_graphics(
            self.painter.graphics(),
            scene.width(),
            scene.height(),
            footer_rows,
        );
        self.renderer.set_scale(scale);
        let pixmap = self.renderer.render(scene).map_err(PrintError::Alloc)?;
        let mut stdout = io::stdout().lock();
        self.painter
            .write_image(&mut stdout, &mut self.bytes, pixmap, Placement::Still)
            .and_then(|()| stdout.flush())
            .map_err(PrintError::Io)
    }
}

/// The painter of a [`Printer`] and of the frames of a [`Terminal`], with
/// what it keeps from one image to the next.
#[expect(
    clippy::large_enum_variant,
    reason = "a Printer holds one Painter, so a smaller enum saves nothing"
)]
enum Painter {
    Kitty(KittyMedium),
    Sixel(sixel::Encoder),
    /// The cells on screen, which a frame compares with its own and
    /// replaces. A print writes every cell and leaves them alone.
    HalfBlocks(HalfBlockScreen),
}

impl Painter {
    /// The painter of `graphics`, which sends a Kitty image as a PNG.
    fn new(graphics: TerminalGraphics) -> Self {
        match graphics {
            TerminalGraphics::Kitty => Painter::Kitty(KittyMedium::DirectPng),
            TerminalGraphics::Sixel => Painter::Sixel(sixel::Encoder::new()),
            TerminalGraphics::HalfBlocks => Painter::HalfBlocks(HalfBlockScreen::default()),
        }
    }

    /// The painter of `graphics` for the terminal on stdout. It sends a
    /// Kitty image through shared memory when the probe found that the
    /// terminal reads it.
    fn for_stdout(graphics: TerminalGraphics) -> Self {
        match graphics {
            TerminalGraphics::Kitty => Painter::Kitty(KittyMedium::for_stdout()),
            TerminalGraphics::Sixel | TerminalGraphics::HalfBlocks => Painter::new(graphics),
        }
    }

    fn graphics(&self) -> TerminalGraphics {
        match self {
            Painter::Kitty(_) => TerminalGraphics::Kitty,
            Painter::Sixel(_) => TerminalGraphics::Sixel,
            Painter::HalfBlocks(_) => TerminalGraphics::HalfBlocks,
        }
    }

    /// Write `pixmap`. A still image goes at the cursor. A frame goes at
    /// (0, 0), so the cursor has to be there. A caller that draws again
    /// passes the same `bytes`.
    fn write_image<W: Write>(
        &mut self,
        out: &mut W,
        bytes: &mut Vec<u8>,
        pixmap: &Pixmap,
        placement: Placement,
    ) -> io::Result<()> {
        match self {
            Painter::Kitty(medium) => {
                let id = match placement {
                    Placement::Frame => Some(KITTY_ANIMATION_ID),
                    Placement::Still => None,
                };
                match medium {
                    KittyMedium::DirectPng => emit_kitty(out, pixmap, id, bytes)?,
                    #[cfg(unix)]
                    KittyMedium::SharedMemory { next } => emit_kitty_shared(out, pixmap, id, next)?,
                }
                placement.end(out)
            }
            Painter::Sixel(encoder) => {
                bytes.clear();
                encoder.encode(pixmap, bytes)?;
                out.write_all(bytes)?;
                placement.end(out)
            }
            Painter::HalfBlocks(screen) => match placement {
                Placement::Frame => update_half_blocks(out, pixmap, bytes, screen),
                // Each row of the cells ends in a newline, so the cursor
                // already sits on the line below the image.
                Placement::Still => render_half_blocks(out, pixmap, bytes),
            },
        }
    }

    /// Remove the frame on screen, unless the next frame replaces it whole.
    /// Returns `true` if the screen cleared, `false` otherwise.
    fn clear_old_frame<W: Write>(&mut self, out: &mut W) -> io::Result<bool> {
        match self {
            // The same id replaces the whole image in place, and a clear, or
            // a delete before the transmit, shows the cleared cells for one
            // refresh.
            Painter::Kitty(_) => Ok(false),
            Painter::Sixel(_) => {
                queue!(out, terminal::Clear(terminal::ClearType::All))?;
                Ok(true)
            }
            Painter::HalfBlocks(screen) => {
                queue!(out, terminal::Clear(terminal::ClearType::All))?;
                screen.forget();
                Ok(true)
            }
        }
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

impl Placement {
    /// Leave the cursor on the line below a still image.
    fn end<W: Write>(self, out: &mut W) -> io::Result<()> {
        match self {
            Placement::Frame => Ok(()),
            Placement::Still => writeln!(out),
        }
    }
}

/// How a Kitty image goes to the terminal.
enum KittyMedium {
    /// A PNG in the escapes (`t=d`), which reaches a terminal on another
    /// machine.
    DirectPng,
    /// A shared memory object per image (`t=s`), which the terminal reads
    /// without a decode. `next` numbers the object of the next image.
    #[cfg(unix)]
    SharedMemory { next: u64 },
}

impl KittyMedium {
    fn for_stdout() -> Self {
        #[cfg(unix)]
        if super::term_query::caps().kitty == Some(super::term_query::KittyTransfer::SharedMemory) {
            return KittyMedium::SharedMemory { next: 0 };
        }
        KittyMedium::DirectPng
    }
}

/// Returns `true` if the terminal reports 24-bit color, `false` otherwise.
/// The half-blocks fallback needs it.
pub(crate) fn half_blocks_supported() -> bool {
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
pub(crate) fn kitty_supported() -> bool {
    super::term_query::caps().kitty.is_some()
}

/// Returns `true` if the terminal supports DEC Sixel, `false` otherwise. The
/// answer comes from the same probe as [`kitty_supported`].
pub(crate) fn sixel_supported() -> bool {
    super::term_query::caps().sixel
}

/// How a [`Terminal`] or a [`Printer`] draws a scene.
// The discriminants are the values of `TTY` while a `Terminal` shows through
// the graphics.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(u8)]
pub enum TerminalGraphics {
    /// The graphics protocol of Kitty.
    Kitty = 2,
    /// DEC Sixel.
    Sixel = 3,
    /// Two pixels per cell, as the foreground and background of `▀`. It
    /// needs truecolor.
    HalfBlocks = 4,
}

impl TerminalGraphics {
    /// Returns `true` if the terminal on stdout shows these graphics,
    /// `false` otherwise.
    pub fn is_supported(self) -> bool {
        match self {
            TerminalGraphics::Kitty => kitty_supported(),
            TerminalGraphics::Sixel => sixel_supported(),
            TerminalGraphics::HalfBlocks => half_blocks_supported(),
        }
    }
}

fn pick_graphics() -> Option<TerminalGraphics> {
    if kitty_supported() {
        Some(TerminalGraphics::Kitty)
    } else if sixel_supported() {
        Some(TerminalGraphics::Sixel)
    } else if half_blocks_supported() {
        Some(TerminalGraphics::HalfBlocks)
    } else {
        None
    }
}

// -----------------------------------------------------------------------------
// The tty of the process
// -----------------------------------------------------------------------------

/// Who holds the tty: [`FREE`], a [`Terminal`] that still probes it
/// ([`PROBING`]), one that shows through a [`TerminalGraphics`], by its
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

    /// Record the graphics that the probe picked, for the panic hook.
    fn show_through(&self, graphics: TerminalGraphics) {
        TTY.store(graphics as u8, Ordering::Release);
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

/// Mark the tty as put back and return the graphics of the [`Terminal`]
/// that holds it. `None` when nobody holds it or the tty is already back,
/// and half-blocks while the probe runs, since only Kitty needs more than
/// leaving the alt screen.
fn restore_held() -> Option<TerminalGraphics> {
    let tag = TTY
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |tag| {
            (tag != FREE && tag != RESTORED).then_some(RESTORED)
        })
        .ok()?;
    Some(
        [
            TerminalGraphics::Kitty,
            TerminalGraphics::Sixel,
            TerminalGraphics::HalfBlocks,
        ]
        .into_iter()
        .find(|&b| b as u8 == tag)
        .unwrap_or(TerminalGraphics::HalfBlocks),
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
fn leave(graphics: TerminalGraphics, frame_shown: bool) {
    let mut stdout = io::stdout().lock();
    if frame_shown && graphics == TerminalGraphics::Kitty {
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
            if let Some(graphics) = restore_held() {
                leave(graphics, true);
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
    /// [`super::term_input`] reads the bytes of stdin, and `kitty` says that
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
        let (kitty, keys_on) = if super::term_query::caps().kitty_keyboard {
            let push = format!(
                "\x1b[>{}u{FOCUS_ON}",
                super::term_input::KITTY_KEYBOARD_FLAGS
            );
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
        footer_rows: u16,
        on_interrupt: Option<fn()>,
    ) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("sinteract-terminal".into())
            .spawn(move || match keys {
                KeyInput::Bytes { kitty } => {
                    let size = SizeWatch::new(footer_rows);
                    read_bytes(&tx, &cells, &flag, kitty, size, on_interrupt)
                }
                KeyInput::Off => watch_size(&tx, &flag, SizeWatch::new(footer_rows)),
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
    mut size: SizeWatch,
    on_interrupt: Option<fn()>,
) {
    use super::term_input::{Decoder, Input};

    let _close = CloseOnExit(tx);
    let mut tty = match super::term_query::TtyInput::open() {
        Ok(tty) => tty,
        Err(e) => {
            let _ = tx.send_read_error(e);
            return;
        }
    };
    let mut keys = Decoder::new(kitty);
    let mut bytes = Vec::new();
    let mut inputs = Vec::new();
    while !stop.load(Ordering::Acquire) {
        if size.check(tx).is_err() {
            return;
        }
        if let Err(e) = tty.read(READ_POLL, &mut bytes) {
            let _ = tx.send_read_error(e);
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
                Input::Key(k) => tx.send_key(k),
                Input::Mouse(m) => {
                    let cells = *cells.lock().unwrap_or_else(PoisonError::into_inner);
                    let (x, y) = cells.to_scene(m.column, m.row);
                    tx.send_mouse(MouseEvent {
                        action: m.action,
                        x,
                        y,
                        modifiers: m.modifiers,
                        buttons: m.buttons,
                    })
                }
                Input::Interrupt => {
                    let _ = tx.send_close();
                    if let Some(f) = on_interrupt {
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
fn watch_size(tx: &Sender, stop: &AtomicBool, mut size: SizeWatch) {
    let _close = CloseOnExit(tx);
    while !stop.load(Ordering::Acquire) {
        if size.check(tx).is_err() {
            return;
        }
        thread::sleep(READ_POLL);
    }
}

/// The size of the terminal at the last check.
struct SizeWatch {
    size: Option<(u16, u16)>,
    /// [`TerminalOptions::footer_rows`].
    footer_rows: u16,
}

impl SizeWatch {
    fn new(footer_rows: u16) -> Self {
        Self {
            size: terminal::size().ok(),
            footer_rows,
        }
    }

    /// Send a resize when the size changed since the last check.
    fn check(&mut self, tx: &Sender) -> Result<(), super::Closed> {
        let now = terminal::size().ok();
        if now == self.size {
            return Ok(());
        }
        self.size = now;
        now.map_or(Ok(()), |now| resized(tx, now, self.footer_rows))
    }
}

/// Send the size of the scene for a terminal of `(cols, rows)` with
/// `footer_rows` under the frame, and draw the last frame again.
fn resized(tx: &Sender, size: (u16, u16), footer_rows: u16) -> Result<(), super::Closed> {
    if let Some((width, height)) = scene_size(size, cell_pixels(), footer_rows) {
        let _ = tx.send_resize(width, height);
    }
    tx.request_redraw()
}

struct CloseOnExit<'a>(&'a Sender);

impl Drop for CloseOnExit<'_> {
    fn drop(&mut self) {
        let _ = self.0.send_close();
    }
}

/// The lines of text under the frame of a [`Terminal`]. The writer of the
/// frames draws them after each frame, and [`Terminal::set_footer`] draws
/// them between two frames.
struct Footer {
    /// [`TerminalOptions::footer_rows`].
    rows: u16,
    /// At most `rows` lines, with no control characters.
    lines: Vec<String>,
    /// The first row of the footer on screen, or `None` while no footer is
    /// on screen.
    top: Option<u16>,
}

impl Footer {
    fn new(rows: u16) -> Self {
        Self {
            rows,
            lines: Vec::new(),
            top: None,
        }
    }

    /// Keep the last `rows` of `lines`.
    fn set<S: AsRef<str>>(&mut self, lines: &[S]) {
        let skip = lines.len().saturating_sub(self.rows.into());
        self.lines = lines
            .iter()
            .skip(skip)
            .map(|line| {
                let line = line.as_ref();
                line.chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect()
            })
            .collect();
    }

    /// Draw the footer from row `top`, with at most `cols` characters a
    /// line. A footer on screen at another row goes blank first, since
    /// Kitty draws a frame of another height without a clear.
    fn write<W: Write>(&mut self, out: &mut W, top: u16, cols: u16) -> io::Result<()> {
        if self.rows == 0 {
            return Ok(());
        }
        if let Some(old) = self.top.take().filter(|&old| old != top) {
            for row in old..old.saturating_add(self.rows) {
                queue!(
                    out,
                    cursor::MoveTo(0, row),
                    terminal::Clear(terminal::ClearType::UntilNewLine)
                )?;
            }
        }
        // A wide character takes two columns, so a line of `cols`
        // characters can still be too wide. Without the wrap, its end stays
        // in the last column instead of moving the rows below.
        queue!(out, terminal::DisableLineWrap)?;
        for (i, row) in (top..top.saturating_add(self.rows)).enumerate() {
            queue!(
                out,
                cursor::MoveTo(0, row),
                terminal::Clear(terminal::ClearType::UntilNewLine)
            )?;
            if let Some(line) = self.lines.get(i) {
                let end = line
                    .char_indices()
                    .nth(cols.into())
                    .map_or(line.len(), |(i, _)| i);
                out.write_all(line.get(..end).unwrap_or(line).as_bytes())?;
            }
        }
        queue!(out, terminal::EnableLineWrap)?;
        self.top = Some(top);
        Ok(())
    }
}

/// The rows of cells that a frame `height` pixels high takes on `graphics`,
/// with cells `cell_height` pixels high.
fn frame_rows(graphics: TerminalGraphics, height: u32, cell_height: u32) -> u16 {
    let rows = match graphics {
        TerminalGraphics::HalfBlocks => height.div_ceil(2),
        TerminalGraphics::Kitty => height.div_ceil(cell_height),
        // Sixel draws six rows of pixels at a time.
        TerminalGraphics::Sixel => height.next_multiple_of(6).div_ceil(cell_height),
    };
    u16::try_from(rows).unwrap_or(u16::MAX)
}

/// The columns of the terminal, or as many as a line may have when the
/// size is unknown.
fn screen_cols() -> u16 {
    terminal::size().map_or(u16::MAX, |(cols, _)| cols)
}

/// Pixel size of one terminal cell, from the cached probe in
/// [`super::term_query`]. A terminal under a multiplexer or without a tty
/// does not answer, and gets 8 by 16.
fn cell_pixels() -> (u32, u32) {
    super::term_query::caps()
        .cell_px
        .unwrap_or((CELL_W_DEFAULT, CELL_H_DEFAULT))
}

// -----------------------------------------------------------------------------
// Terminal-aware sizing + half-blocks renderer
// -----------------------------------------------------------------------------

/// Pixel box available for the image, from the terminal size and the
/// `footer_rows` under it. `None` when crossterm cannot read the size, as
/// when stdout is a file.
fn target_pixels_for_graphics(
    graphics: TerminalGraphics,
    cell: (u32, u32),
    footer_rows: u16,
) -> Option<(u32, u32)> {
    let (cols, rows) = image_cells(terminal::size().ok()?, footer_rows)?;
    let (pw, ph) = pixmap_per_cell(graphics, cell);
    Some((u32::from(cols) * pw, u32::from(rows) * ph))
}

/// The pixmap pixels in a cell, across and down. In Kitty and Sixel a
/// pixmap pixel is a screen pixel, and in half-blocks a cell holds one
/// pixmap pixel across and two down.
fn pixmap_per_cell(graphics: TerminalGraphics, cell: (u32, u32)) -> (u32, u32) {
    match graphics {
        TerminalGraphics::Kitty | TerminalGraphics::Sixel => cell,
        TerminalGraphics::HalfBlocks => (1, 2),
    }
}

/// The columns and the rows that the image may take in a terminal of
/// `(cols, rows)` with `footer_rows` under it, or `None` for a terminal of
/// no cells. The image keeps at least one row.
fn image_cells((cols, rows): (u16, u16), footer_rows: u16) -> Option<(u16, u16)> {
    if cols == 0 || rows == 0 {
        return None;
    }
    // One row stays free. Inline, it holds the prompt that follows the
    // image. In the alt screen, it keeps a Sixel in the last row from
    // scrolling the screen.
    Some((
        cols,
        rows.saturating_sub(1).saturating_sub(footer_rows).max(1),
    ))
}

/// The largest scene that a terminal of `(cols, rows)` with `footer_rows`
/// under the frame shows at scale 1, in the pixels of the screen.
/// Half-blocks show it smaller.
fn scene_size(size: (u16, u16), (cw, ch): (u32, u32), footer_rows: u16) -> Option<(f32, f32)> {
    let (cols, rows) = image_cells(size, footer_rows)?;
    Some(((u32::from(cols) * cw) as f32, (u32::from(rows) * ch) as f32))
}

/// How a cell of the terminal maps to the scene of the frame at (0, 0).
#[derive(Clone, Copy, Debug, PartialEq)]
struct CellMap {
    /// The units of the scene per cell, across and down.
    per_cell: (f32, f32),
}

impl CellMap {
    /// A frame at `scale`.
    fn new(graphics: TerminalGraphics, cell: (u32, u32), scale: f32) -> Self {
        let (pw, ph) = pixmap_per_cell(graphics, cell);
        Self {
            per_cell: (pw as f32 / scale, ph as f32 / scale),
        }
    }

    /// Before the first frame, the largest scale of the graphics.
    fn before_frames(graphics: TerminalGraphics, cell: (u32, u32)) -> Self {
        Self::new(graphics, cell, max_scale_for_graphics(graphics, cell))
    }

    fn to_scene(self, column: u16, row: u16) -> (f32, f32) {
        (
            (f32::from(column) + 0.5) * self.per_cell.0,
            (f32::from(row) + 0.5) * self.per_cell.1,
        )
    }
}

/// Upper bound on the scale of the rasterizer for `graphics`, so that a
/// logical pixel never grows past [`pixel_density`] screen pixels. A
/// pixmap pixel covers `cw / pw` by `ch / ph` screen pixels, so the cap
/// divides by the larger of the two.
fn max_scale_for_graphics(graphics: TerminalGraphics, (cw, ch): (u32, u32)) -> f32 {
    let (pw, ph) = pixmap_per_cell(graphics, (cw, ch));
    pixel_density(ch) / (cw as f32 / pw as f32).max(ch as f32 / ph as f32)
}

/// The height in pixels of a cell of Monospace 11 at a desktop scale of 1,
/// as foot draws it.
const REFERENCE_CELL_H: f32 = 19.0;

/// How many screen pixels a logical pixel covers, from a cell `ch` pixels
/// high. A terminal does not tell its HiDPI factor, and the cell grows with
/// that factor and with the font, so the ratio to [`REFERENCE_CELL_H`]
/// stands for it. The scene then keeps its size next to the text, as it
/// does in a window. A small font does not shrink it below 1.
fn pixel_density(ch: u32) -> f32 {
    (ch as f32 / REFERENCE_CELL_H).max(1.0)
}

/// Scale for a `width` by `height` frame on `graphics`, with `footer_rows`
/// under it. The frame shrinks to fit the cell grid when the grid size is
/// known, and the cap of the graphics applies either way, because
/// half-blocks pack two rows per cell even without a known grid.
fn scale_for_graphics(
    graphics: TerminalGraphics,
    width: f32,
    height: f32,
    footer_rows: u16,
) -> f32 {
    let cell = cell_pixels();
    let target = target_pixels_for_graphics(graphics, cell, footer_rows);
    capped_scale(
        width,
        height,
        target,
        max_scale_for_graphics(graphics, cell),
    )
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
fn render_half_blocks<W: Write>(out: &mut W, pixmap: &Pixmap, buf: &mut Vec<u8>) -> io::Result<()> {
    buf.clear();
    // A cell whose colors repeat takes the three bytes of `▀`, which is the
    // common case in a drawing, and a cell row covers two pixel rows. A
    // frame of many colors grows the buffer once and keeps the room.
    buf.reserve(pixmap.width() as usize * pixmap.height() as usize * 2);
    for (top, bottom) in pixel_pairs(pixmap) {
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

/// The half-block cells of the frame on screen.
#[derive(Default)]
struct HalfBlockScreen {
    /// The cells that the terminal shows, row by row. Empty before the
    /// first frame and after the screen clears.
    shown: Vec<Cell>,
    /// The cells of one row of `shown`.
    cols: usize,
}

impl HalfBlockScreen {
    /// Forget what the terminal shows, so the next frame writes every cell.
    /// A clear, or anything else that paints over the frame, happens behind
    /// the screen.
    fn forget(&mut self) {
        self.shown.clear();
        self.cols = 0;
    }
}

/// Write the cells where `pixmap` differs from the frame that `screen`
/// holds, and keep `pixmap` as that frame. A write that fails empties
/// `screen`, so the next frame writes every cell. The cells start at
/// (0, 0), so only a caller that owns the whole screen may call this. `buf`
/// holds the escapes, and a caller that draws again passes the same one.
///
/// A frame of an animation changes a few percent of the cells, and the
/// cursor jumps over the rest.
fn update_half_blocks<W: Write>(
    out: &mut W,
    pixmap: &Pixmap,
    buf: &mut Vec<u8>,
    screen: &mut HalfBlockScreen,
) -> io::Result<()> {
    let cols = pixmap.width() as usize;
    let len = cols * pixmap.height().div_ceil(2) as usize;
    // A frame of another shape shares no cell with the one on screen, so
    // the loop writes every cell and never compares with the fill of the
    // resize.
    let all = screen.cols != cols || screen.shown.len() != len;
    screen.shown.resize(
        len,
        Cell {
            fg: BLACK,
            bg: BLACK,
        },
    );
    screen.cols = cols;
    buf.clear();
    // The reset at the end of a frame leaves the terminal with the default
    // colors, which no cell carries, so the first cell sets both.
    let mut state = None;
    // Where the cursor sits after the cell written last.
    let mut at = None;
    let rows = screen.shown.chunks_mut(cols).zip(pixel_pairs(pixmap));
    for (r, (shown, (top, bottom))) in rows.enumerate() {
        for (x, (slot, &t)) in shown.iter_mut().zip(top).enumerate() {
            let cell = Cell::new(t, bottom.get(x));
            if !all && *slot == cell {
                continue;
            }
            *slot = cell;
            if at != Some((r, x)) {
                write!(buf, "\x1b[{};{}H", r + 1, x + 1).expect("a Vec takes every write");
            }
            push_cell(buf, cell, state);
            state = Some(cell);
            at = Some((r, x + 1));
        }
    }
    buf.extend_from_slice(b"\x1b[0m");
    out.write_all(buf).inspect_err(|_| {
        // Part of the frame reached the terminal, and which part is unknown.
        screen.forget();
    })
}

/// The rows of pixels of `pixmap` in pairs, one for each row of cells. The
/// last row of an odd height has an empty row below it.
fn pixel_pairs(
    pixmap: &Pixmap,
) -> impl Iterator<Item = (&[PremultipliedColorU8], &[PremultipliedColorU8])> {
    let cols = pixmap.width() as usize;
    // `chunks` panics on a size of 0, and a pixmap is never 0 pixels wide.
    pixmap
        .pixels()
        .chunks(2 * cols)
        .map(move |pair| pair.split_at_checked(cols).unwrap_or((pair, &[])))
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
/// one without stays until the terminal scrolls it away. `buf` takes the
/// escapes, and a caller that draws again passes the same one.
fn emit_kitty<W: Write>(
    w: &mut W,
    pixmap: &Pixmap,
    id: Option<u32>,
    buf: &mut Vec<u8>,
) -> io::Result<()> {
    let mut chunks = KittyChunks::new(buf, id);
    encode_png(pixmap, &mut chunks)?;
    chunks.finish();
    w.write_all(buf)
}

/// Write the Kitty escape that shows `pixmap` at the cursor from a shared
/// memory object, as [`emit_kitty`] does with a PNG, and flush it. The
/// terminal reads the pixels in straight alpha and removes the object.
/// `next` numbers the object, and a number that another object holds, such
/// as one that a terminal has not read yet, gives way to the one after it.
#[cfg(unix)]
fn emit_kitty_shared<W: Write>(
    w: &mut W,
    pixmap: &Pixmap,
    id: Option<u32>,
    next: &mut u64,
) -> io::Result<()> {
    let len = pixmap.data().len();
    let name = loop {
        let name = std::ffi::CString::new(format!("/sinteract-{:x}-{next:x}", std::process::id()))
            .expect("the name has no NUL");
        *next += 1;
        match super::shm::create(&name, len, |bytes| {
            straight_alpha(pixmap.pixels(), bytes);
        }) {
            Ok(()) => break name,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    };
    let written = write!(
        w,
        "\x1b_Ga=T,f=32,t=s,s={},v={},S={len},q=2",
        pixmap.width(),
        pixmap.height()
    )
    .and_then(|()| match id {
        Some(id) => write!(w, ",i={id}"),
        None => Ok(()),
    })
    .and_then(|()| write!(w, ";{}\x1b\\", B64.encode(name.as_bytes())))
    .and_then(|()| w.flush());
    // A terminal that never got the whole escape never reads the object.
    if written.is_err() {
        super::shm::unlink(&name);
    }
    written
}

/// Cuts a PNG into the chunks of the Kitty protocol as the encoder writes
/// it, and appends each chunk to `buf` in base64 with its escape. Only the
/// last chunk says `m=0`, so a full chunk waits for the next byte.
struct KittyChunks<'a> {
    /// Empty until the first chunk, which carries the header.
    buf: &'a mut Vec<u8>,
    id: Option<u32>,
    /// At most [`CHUNK_BYTES`].
    pending: Vec<u8>,
}

impl<'a> KittyChunks<'a> {
    fn new(buf: &'a mut Vec<u8>, id: Option<u32>) -> Self {
        buf.clear();
        Self {
            buf,
            id,
            pending: Vec::with_capacity(CHUNK_BYTES),
        }
    }

    /// Append the last chunk.
    fn finish(mut self) {
        self.emit(false);
    }

    fn emit(&mut self, more: bool) {
        let m = u8::from(more);
        if self.buf.is_empty() {
            // The PNG header carries the size, so s and v say nothing here.
            write!(self.buf, "\x1b_Ga=T,f=100,q=2,m={m}").expect("a Vec takes every write");
            if let Some(id) = self.id {
                write!(self.buf, ",i={id}").expect("a Vec takes every write");
            }
            self.buf.push(b';');
        } else {
            write!(self.buf, "\x1b_Gm={m},q=2;").expect("a Vec takes every write");
        }
        let mut encoded = [0u8; CHUNK_BYTES / 3 * 4];
        let n = B64
            .encode_slice(&self.pending, &mut encoded)
            .expect("a chunk encodes into four bytes per three");
        self.buf
            .extend_from_slice(encoded.get(..n).expect("encode_slice fills a prefix"));
        self.buf.extend_from_slice(b"\x1b\\");
        self.pending.clear();
    }
}

impl Write for KittyChunks<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        // An empty write says nothing about whether more bytes follow.
        if data.is_empty() {
            return Ok(0);
        }
        if self.pending.len() == CHUNK_BYTES {
            self.emit(true);
        }
        let before = self.pending.len();
        let room = CHUNK_BYTES - before;
        self.pending.extend(data.iter().take(room));
        Ok(self.pending.len() - before)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Write `pixmap` into `out` as a PNG with straight alpha.
///
/// The terminal reads the raw pixels of a frame of 960 by 540 in 95 ms,
/// because they travel in base64, and the same frame as a PNG in 1 ms. The
/// adaptive filter is what earns it, since it turns a flat area into a row
/// of zeros. `Balanced` shrinks the frame four times more and costs ten
/// times the encoding, which the write does not give back.
fn encode_png<W: Write>(pixmap: &Pixmap, out: W) -> io::Result<()> {
    let mut encoder = png::Encoder::new(out, pixmap.width(), pixmap.height());
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
        let kitty = CellMap::new(TerminalGraphics::Kitty, (10, 20), 0.5);
        assert_eq!(kitty.to_scene(0, 0), (10.0, 20.0));
        assert_eq!(kitty.to_scene(3, 1), (70.0, 60.0));
        // Half-blocks at scale 0.25: one pixmap pixel across and two down.
        let blocks = CellMap::new(TerminalGraphics::HalfBlocks, (10, 20), 0.25);
        assert_eq!(blocks.to_scene(0, 0), (2.0, 4.0));
    }

    #[test]
    fn the_scene_size_leaves_the_last_row_free() {
        assert_eq!(scene_size((80, 25), (8, 16), 0), Some((640.0, 384.0)));
        assert_eq!(scene_size((80, 1), (8, 16), 0), Some((640.0, 16.0)));
        assert_eq!(scene_size((0, 25), (8, 16), 0), None);
    }

    #[test]
    fn the_footer_rows_come_out_of_the_scene_size() {
        assert_eq!(scene_size((80, 25), (8, 16), 5), Some((640.0, 304.0)));
        // The frame keeps one row in a terminal too small for the footer.
        assert_eq!(scene_size((80, 3), (8, 16), 5), Some((640.0, 16.0)));
    }

    #[test]
    fn a_frame_takes_the_rows_of_its_graphics() {
        assert_eq!(frame_rows(TerminalGraphics::HalfBlocks, 5, 16), 3);
        assert_eq!(frame_rows(TerminalGraphics::Kitty, 33, 16), 3);
        // 32 pixels make six bands of six, which reach 36.
        assert_eq!(frame_rows(TerminalGraphics::Sixel, 32, 16), 3);
    }

    #[test]
    fn a_footer_keeps_its_last_lines_without_control_characters() {
        let mut footer = Footer::new(2);
        footer.set(&["one", "two", "a\tb\x1b[2J"]);
        assert_eq!(footer.lines, ["two", "a b [2J"]);
    }

    /// What `footer` writes from row `top` with `cols` columns.
    fn footer_bytes(footer: &mut Footer, top: u16, cols: u16) -> String {
        let mut out = Vec::new();
        footer.write(&mut out, top, cols).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_footer_cuts_a_line_at_the_width() {
        let mut footer = Footer::new(1);
        footer.set(&["abcdef"]);
        let out = footer_bytes(&mut footer, 0, 4);
        assert!(out.contains("abcd") && !out.contains("abcde"), "{out:?}");
    }

    #[test]
    fn a_footer_at_another_row_blanks_the_old_one() {
        let mut footer = Footer::new(1);
        footer.set(&["x"]);
        let first = footer_bytes(&mut footer, 3, 80);
        assert!(first.starts_with("\x1b[?7l\x1b[4;1H"), "{first:?}");
        let same = footer_bytes(&mut footer, 3, 80);
        assert_eq!(same, first, "the same row");
        let moved = footer_bytes(&mut footer, 1, 80);
        assert!(moved.starts_with("\x1b[4;1H\x1b[K"), "{moved:?}");
        assert!(moved.contains("\x1b[2;1H\x1b[Kx"), "{moved:?}");
    }

    #[test]
    fn a_frame_draws_the_footer_under_it() {
        let mut state = sixel_state();
        state.footer = Arc::new(Mutex::new(Footer::new(1)));
        state.footer.lock().unwrap().set(&["log"]);
        let mut out = Vec::new();
        state
            .write_to(&mut out, &solid(4, 32, 0, 0, 255), false)
            .unwrap();
        assert_eq!(
            state.footer.lock().unwrap().top,
            Some(frame_rows(TerminalGraphics::Sixel, 32, cell_pixels().1))
        );
        assert!(String::from_utf8_lossy(&out).ends_with("log\x1b[?7h"));
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
            max_scale_for_graphics(TerminalGraphics::HalfBlocks, (8, 16)),
            1.0 / 8.0
        );
        assert_eq!(
            max_scale_for_graphics(TerminalGraphics::Kitty, (8, 16)),
            1.0
        );
    }

    #[test]
    fn a_taller_cell_than_the_reference_raises_the_cap() {
        // A cell of 38 pixels is twice the reference, as at a HiDPI of 2.
        assert_eq!(
            max_scale_for_graphics(TerminalGraphics::Kitty, (18, 38)),
            2.0
        );
        assert_eq!(
            max_scale_for_graphics(TerminalGraphics::Sixel, (18, 38)),
            2.0
        );
        assert_eq!(
            max_scale_for_graphics(TerminalGraphics::HalfBlocks, (18, 38)),
            2.0 / 19.0
        );
        assert_eq!(
            max_scale_for_graphics(TerminalGraphics::Kitty, (10, 19)),
            1.0
        );
    }

    #[test]
    fn half_blocks_renders_some_pixels() {
        let pm = solid(4, 4, 255, 0, 0);
        let mut buf: Vec<u8> = Vec::new();
        render_half_blocks(&mut buf, &pm, &mut Vec::new()).expect("write ok");
        assert_eq!(rows(&buf), 2);
        // U+2580 in UTF-8.
        assert!(buf.windows(3).any(|w| w == [0xE2, 0x96, 0x80]));
    }

    #[test]
    fn half_blocks_uses_truecolor_codes() {
        let pm = solid(2, 2, 0, 0, 255);
        let mut buf: Vec<u8> = Vec::new();
        render_half_blocks(&mut buf, &pm, &mut Vec::new()).expect("write ok");
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("\x1b[38;2;"), "missing 24-bit fg SGR: {s:?}");
        assert!(s.contains(";48;2;"), "missing 24-bit bg SGR: {s:?}");
        assert!(s.contains("\x1b[0m"), "missing reset: {s:?}");
    }

    #[test]
    fn half_blocks_sets_a_color_once_for_a_run_of_cells() {
        let pm = solid(8, 2, 0, 0, 255);
        let mut buf: Vec<u8> = Vec::new();
        render_half_blocks(&mut buf, &pm, &mut Vec::new()).expect("write ok");
        let s = String::from_utf8_lossy(&buf);
        assert_eq!(s.matches("\x1b[38;2;").count(), 1, "one fg SGR: {s:?}");
        assert_eq!(s.matches("48;2;").count(), 1, "one bg SGR: {s:?}");
        assert_eq!(s.matches('▀').count(), 8);
    }

    #[test]
    fn half_blocks_handles_odd_height() {
        // The last cell row has no bottom pixel and takes black.
        let pm = solid(3, 3, 255, 255, 255);
        let mut buf: Vec<u8> = Vec::new();
        render_half_blocks(&mut buf, &pm, &mut Vec::new()).expect("write ok");
        // ceil(3 / 2) rows.
        assert_eq!(rows(&buf), 2);
    }

    #[test]
    fn half_blocks_writes_one_row_for_one_pixel() {
        let pm = Pixmap::new(1, 1).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        render_half_blocks(&mut buf, &pm, &mut Vec::new()).expect("write ok");
        assert_eq!(rows(&buf), 1);
    }

    fn rows(buf: &[u8]) -> usize {
        buf.windows(2).filter(|w| w == b"\r\n").count()
    }

    /// The frame that `update_half_blocks` writes for `pixmap`, and the
    /// cells it holds.
    fn update(screen: &mut HalfBlockScreen, pixmap: &Pixmap) -> (Vec<u8>, usize) {
        let mut out: Vec<u8> = Vec::new();
        update_half_blocks(&mut out, pixmap, &mut Vec::new(), screen).expect("write ok");
        let cells = String::from_utf8_lossy(&out).matches('▀').count();
        (out, cells)
    }

    /// A renderer whose last render is a `w` by `h` frame of `color`.
    fn rendered(renderer: &mut PixmapRenderer, w: f32, h: f32, color: (u8, u8, u8)) {
        let (r, g, b) = color;
        renderer.set_background(Rgba { r, g, b, a: 255 });
        renderer.render(&Scene::new(w, h)).unwrap();
    }

    /// The color of the top left pixel.
    fn first_rgb(pixmap: &Pixmap) -> (u8, u8, u8) {
        let p = pixmap.pixels()[0];
        (p.red(), p.green(), p.blue())
    }

    /// The color of the top left pixel, the size and the resize flag of a
    /// frame.
    type Seen = ((u8, u8, u8), (u32, u32), bool);

    /// The frames that the writer saw, and whether the write of each fails.
    #[derive(Default)]
    struct Log {
        frames: Vec<Seen>,
        fail: Vec<bool>,
    }

    fn log_frame(log: &mut Log, pixmap: &Pixmap, after_resize: bool) -> io::Result<()> {
        let size = (pixmap.width(), pixmap.height());
        log.frames.push((first_rgb(pixmap), size, after_resize));
        match log.fail.get(log.frames.len() - 1) {
            Some(true) => Err(io::Error::other("the write failed")),
            Some(false) | None => Ok(()),
        }
    }

    #[test]
    fn the_writer_writes_each_frame_in_order() {
        let mut writer = FrameWriter::spawn(Log::default(), log_frame).unwrap();
        let mut renderer = PixmapRenderer::default();
        rendered(&mut renderer, 4.0, 2.0, (255, 0, 0));
        writer.write(&mut renderer, false).unwrap();
        rendered(&mut renderer, 3.0, 3.0, (0, 0, 255));
        writer.write(&mut renderer, true).unwrap();
        let log = writer.finish().unwrap();
        assert_eq!(
            log.frames,
            [((255, 0, 0), (4, 2), false), ((0, 0, 255), (3, 3), true)]
        );
    }

    #[test]
    fn the_renderer_gets_back_the_pixmap_of_the_frame_before() {
        let mut writer = FrameWriter::spawn(Log::default(), log_frame).unwrap();
        let mut renderer = PixmapRenderer::default();
        rendered(&mut renderer, 4.0, 2.0, (255, 0, 0));
        writer.write(&mut renderer, false).unwrap();
        rendered(&mut renderer, 4.0, 2.0, (0, 0, 255));
        writer.write(&mut renderer, false).unwrap();
        let back = renderer.output();
        assert_eq!((first_rgb(back), back.width()), ((255, 0, 0), 4));
        writer.finish().unwrap();
    }

    #[test]
    fn a_failed_write_reports_at_the_next_frame() {
        let log = Log {
            fail: vec![false, true, false],
            ..Log::default()
        };
        let mut writer = FrameWriter::spawn(log, log_frame).unwrap();
        let mut renderer = PixmapRenderer::default();
        let mut results = Vec::new();
        for _ in 0..4 {
            rendered(&mut renderer, 2.0, 2.0, (0, 255, 0));
            results.push(writer.write(&mut renderer, false).is_ok());
        }
        assert_eq!(results, [true, true, false, true]);
        assert_eq!(writer.finish().unwrap().frames.len(), 4);
    }

    #[test]
    fn a_failed_write_before_a_redraw_reports_at_the_frame_after() {
        let log = Log {
            fail: vec![false, true, false, false],
            ..Log::default()
        };
        let mut writer = FrameWriter::spawn(log, log_frame).unwrap();
        let mut renderer = PixmapRenderer::default();
        let mut results = Vec::new();
        for after_resize in [false, false, true, false, false] {
            rendered(&mut renderer, 2.0, 2.0, (0, 255, 0));
            results.push(writer.write(&mut renderer, after_resize).is_ok());
        }
        assert_eq!(results, [true, true, true, false, true]);
        writer.finish().unwrap();
    }

    fn panic_at_the_write(_: &mut (), _: &Pixmap, _: bool) -> io::Result<()> {
        panic!("the write broke");
    }

    #[test]
    fn a_panic_of_the_writer_goes_on_in_the_caller() {
        let mut writer = FrameWriter::spawn((), panic_at_the_write).unwrap();
        let mut renderer = PixmapRenderer::default();
        rendered(&mut renderer, 2.0, 2.0, (0, 255, 0));
        writer.write(&mut renderer, false).unwrap();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            writer.write(&mut renderer, false)
        }));
        let payload = caught.unwrap_err();
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"the write broke"));
        assert!(writer.finish().is_none());
    }

    /// The frame state of Sixel, whose painter clears the whole screen for
    /// a frame that does not replace the one before.
    fn sixel_state() -> FrameState {
        FrameState {
            painter: Painter::new(TerminalGraphics::Sixel),
            bytes: Vec::new(),
            frame_size: None,
            footer: Arc::new(Mutex::new(Footer::new(0))),
        }
    }

    /// Returns `true` if a write of `pixmap` clears the screen first,
    /// `false` otherwise.
    fn clears(state: &mut FrameState, pixmap: &Pixmap, after_resize: bool) -> bool {
        let mut out = Vec::new();
        state.write_to(&mut out, pixmap, after_resize).unwrap();
        out.starts_with(b"\x1b[2J")
    }

    #[test]
    fn a_frame_clears_the_screen_when_it_does_not_cover_the_one_before() {
        let mut state = sixel_state();
        let (small, large) = (solid(2, 2, 0, 0, 255), solid(4, 4, 0, 0, 255));
        assert!(clears(&mut state, &large, false), "the first frame");
        assert!(!clears(&mut state, &large, false), "the same size");
        assert!(clears(&mut state, &small, false), "a smaller frame");
        assert!(clears(&mut state, &small, true), "after a resize");
        assert!(!clears(&mut state, &small, false), "the same size again");
    }

    /// A write that fails after `room` bytes.
    struct Short {
        room: usize,
    }

    impl Write for Short {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.room == 0 {
                return Err(io::Error::other("the write failed"));
            }
            let n = buf.len().min(self.room);
            self.room -= n;
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_frame_after_a_failed_write_clears_the_screen() {
        let mut state = sixel_state();
        let pixmap = solid(4, 4, 0, 0, 255);
        assert!(clears(&mut state, &pixmap, false));
        let failed = state.write_to(&mut Short { room: 8 }, &pixmap, false);
        assert!(failed.is_err());
        assert_eq!(state.frame_size, None);
        assert!(clears(&mut state, &pixmap, false));
    }

    #[test]
    fn a_printer_prints_nothing_while_a_session_holds_the_tty() {
        let mut printer = Printer::with_painter(Painter::new(TerminalGraphics::HalfBlocks));
        let claim = Claim::take().expect("no session runs in a test");
        assert!(matches!(
            printer.print(&Scene::new(4.0, 4.0)),
            Err(PrintError::Busy)
        ));
        drop(claim);
    }

    /// The mark of each protocol in what it writes, which the other two
    /// never write. A Kitty image is a graphics escape, a Sixel image a
    /// device control string, and half-blocks are cells of one character.
    const MARKS: [(TerminalGraphics, &[u8]); 3] = [
        (TerminalGraphics::Kitty, b"\x1b_Ga=T"),
        (TerminalGraphics::Sixel, b"\x1bP"),
        (TerminalGraphics::HalfBlocks, "▀".as_bytes()),
    ];

    /// What [`Painter::write_image`] writes for a small blue image.
    fn dispatch(graphics: TerminalGraphics, placement: Placement) -> Vec<u8> {
        let pixmap = solid(4, 4, 0, 0, 255);
        let mut out: Vec<u8> = Vec::new();
        Painter::new(graphics)
            .write_image(&mut out, &mut Vec::new(), &pixmap, placement)
            .expect("write ok");
        out
    }

    #[test]
    fn only_a_sixel_printer_draws_over_white() {
        for (graphics, _) in MARKS {
            let mut printer = Printer::with_painter(Painter::new(graphics));
            let pixmap = printer.renderer.render(&Scene::new(4.0, 4.0)).unwrap();
            let p = pixmap.pixels()[0];
            let expected = match graphics {
                TerminalGraphics::Sixel => [255, 255, 255, 255],
                TerminalGraphics::Kitty | TerminalGraphics::HalfBlocks => [0, 0, 0, 0],
            };
            assert_eq!(
                [p.red(), p.green(), p.blue(), p.alpha()],
                expected,
                "{graphics:?}"
            );
        }
    }

    /// Linux lists the shared memory objects under `/dev/shm`, where a
    /// test reads them back.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_shared_kitty_image_holds_the_straight_pixels() {
        let mut pixmap = solid(2, 1, 0, 0, 255);
        pixmap.pixels_mut()[1] = tiny_skia::ColorU8::from_rgba(200, 100, 0, 128).premultiply();
        let straight: Vec<u8> = pixmap
            .pixels()
            .iter()
            .flat_map(|p| {
                let c = p.demultiply();
                [c.red(), c.green(), c.blue(), c.alpha()]
            })
            .collect();
        let mut painter = Painter::Kitty(KittyMedium::SharedMemory { next: 0 });
        let mut out = Vec::new();
        for _ in 0..2 {
            painter
                .write_image(&mut out, &mut Vec::new(), &pixmap, Placement::Frame)
                .unwrap();
        }
        let head = format!("\x1b_Ga=T,f=32,t=s,s=2,v=1,S=8,q=2,i={KITTY_ANIMATION_ID};");
        let names: Vec<String> = out
            .split(|&b| b == 0x1b)
            .filter_map(|escape| escape.strip_prefix(&head.as_bytes()[1..]))
            .map(|payload| String::from_utf8(B64.decode(payload).unwrap()).unwrap())
            .collect();
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1]);
        for name in names {
            let bytes = std::fs::read(format!("/dev/shm{name}")).unwrap();
            super::super::shm::unlink(&std::ffi::CString::new(name).unwrap());
            assert_eq!(bytes, straight);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_shared_kitty_image_skips_a_name_in_use() {
        // Far from the numbers of the other test, which runs at the same
        // time in the same process.
        let first = 1 << 40;
        let name = |n: u64| {
            std::ffi::CString::new(format!("/sinteract-{:x}-{n:x}", std::process::id())).unwrap()
        };
        super::super::shm::create(&name(first), 1, |_| {}).unwrap();
        let mut next = first;
        let mut out = Vec::new();
        emit_kitty_shared(&mut out, &solid(1, 1, 0, 0, 255), None, &mut next).unwrap();
        super::super::shm::unlink(&name(first));
        super::super::shm::unlink(&name(first + 1));
        assert_eq!(next, first + 2);
        let payload = B64.encode(name(first + 1).as_bytes());
        assert!(out.ends_with(format!(";{payload}\x1b\\").as_bytes()));
    }

    #[test]
    fn a_painter_reports_its_graphics() {
        for (graphics, _) in MARKS {
            assert_eq!(Painter::new(graphics).graphics(), graphics);
        }
    }

    #[test]
    fn an_old_frame_clears_the_screen_except_under_kitty() {
        for (graphics, _) in MARKS {
            let mut out: Vec<u8> = Vec::new();
            Painter::new(graphics)
                .clear_old_frame(&mut out)
                .expect("write ok");
            assert_eq!(
                contains(&out, b"\x1b[2J"),
                graphics != TerminalGraphics::Kitty,
                "{graphics:?}"
            );
        }
    }

    #[test]
    fn a_cleared_half_block_frame_writes_every_cell_again() {
        let pixmap = solid(4, 2, 0, 0, 255);
        let mut painter = Painter::new(TerminalGraphics::HalfBlocks);
        let cells = |painter: &mut Painter| {
            let mut out: Vec<u8> = Vec::new();
            painter
                .write_image(&mut out, &mut Vec::new(), &pixmap, Placement::Frame)
                .expect("write ok");
            String::from_utf8_lossy(&out).matches('▀').count()
        };
        assert_eq!(cells(&mut painter), 4);
        assert_eq!(cells(&mut painter), 0);
        painter.clear_old_frame(&mut Vec::new()).expect("write ok");
        assert_eq!(cells(&mut painter), 4);
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn each_graphics_writes_its_own_protocol_and_no_other() {
        for placement in [Placement::Frame, Placement::Still] {
            for (graphics, _) in MARKS {
                let out = dispatch(graphics, placement);
                for (other, mark) in MARKS {
                    assert_eq!(
                        contains(&out, mark),
                        other == graphics,
                        "{graphics:?} {placement:?} wrote the mark of {other:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_image_leaves_nothing_of_the_one_before_in_the_bytes() {
        let pixmap = solid(4, 4, 0, 0, 255);
        let mut bytes = Vec::new();
        for placement in [Placement::Frame, Placement::Still] {
            for (graphics, _) in MARKS {
                let mut painter = Painter::new(graphics);
                let mut write = |painter: &mut Painter| {
                    let mut out: Vec<u8> = Vec::new();
                    painter
                        .write_image(&mut out, &mut bytes, &pixmap, placement)
                        .expect("write ok");
                    out
                };
                let first = write(&mut painter);
                // A half-block frame writes only the cells that changed, so
                // the second one starts from a cleared screen.
                painter.clear_old_frame(&mut Vec::new()).expect("write ok");
                assert_eq!(write(&mut painter), first, "{graphics:?} {placement:?}");
            }
        }
    }

    #[test]
    fn a_still_image_ends_in_one_newline_and_a_frame_ends_in_none() {
        for (graphics, _) in MARKS {
            let still = dispatch(graphics, Placement::Still);
            assert!(still.ends_with(b"\n"), "{graphics:?} still");
            assert!(!still.ends_with(b"\n\n"), "{graphics:?} still");
            let frame = dispatch(graphics, Placement::Frame);
            assert!(!frame.ends_with(b"\n"), "{graphics:?} frame");
        }
    }

    #[test]
    fn the_kitty_frame_carries_the_animation_id_and_the_still_image_none() {
        let id = format!(",i={KITTY_ANIMATION_ID};");
        assert!(contains(
            &dispatch(TerminalGraphics::Kitty, Placement::Frame),
            id.as_bytes()
        ));
        assert!(!contains(
            &dispatch(TerminalGraphics::Kitty, Placement::Still),
            b",i="
        ));
    }

    #[test]
    fn a_repeated_half_block_frame_writes_no_cell() {
        let pm = solid(4, 2, 0, 0, 255);
        let mut screen = HalfBlockScreen::default();
        assert_eq!(update(&mut screen, &pm).1, 4);
        assert_eq!(update(&mut screen, &pm).0, b"\x1b[0m");
    }

    /// A blue pixmap of `w` by `h` with the top pixels of the columns in
    /// `red` turned red.
    fn blue_with_red_tops(w: u32, h: u32, red: &[usize]) -> Pixmap {
        let mut pm = solid(w, h, 0, 0, 255);
        for &x in red {
            pm.pixels_mut()[x] = tiny_skia::ColorU8::from_rgba(255, 0, 0, 255).premultiply();
        }
        pm
    }

    #[test]
    fn a_half_block_frame_sets_both_colors_once_for_a_run_of_cells() {
        let mut screen = HalfBlockScreen::default();
        let (out, _) = update(&mut screen, &blue_with_red_tops(2, 2, &[0, 1]));
        assert_eq!(
            String::from_utf8_lossy(&out),
            "\x1b[1;1H\x1b[38;2;255;0;0;48;2;0;0;255m▀▀\x1b[0m"
        );
    }

    #[test]
    fn a_half_block_frame_jumps_over_a_cell_that_stays() {
        let mut screen = HalfBlockScreen::default();
        update(&mut screen, &solid(4, 2, 0, 0, 255));
        let (out, cells) = update(&mut screen, &blue_with_red_tops(4, 2, &[0, 2]));
        assert_eq!(cells, 2);
        let out = String::from_utf8_lossy(&out);
        assert!(
            out.contains("\x1b[1;1H") && out.contains("\x1b[1;3H"),
            "{out:?}"
        );
    }

    #[test]
    fn a_half_block_frame_does_not_jump_to_the_next_cell() {
        let mut screen = HalfBlockScreen::default();
        update(&mut screen, &solid(4, 2, 0, 0, 255));
        let (out, cells) = update(&mut screen, &blue_with_red_tops(4, 2, &[1, 2]));
        assert_eq!(cells, 2);
        assert_eq!(String::from_utf8_lossy(&out).matches("H").count(), 1);
    }

    #[test]
    fn a_half_block_frame_writes_the_cell_that_changed() {
        let mut screen = HalfBlockScreen::default();
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
        let mut screen = HalfBlockScreen::default();
        assert_eq!(update(&mut screen, &pm).1, 4);
        screen.forget();
        assert_eq!(update(&mut screen, &pm).1, 4);
    }

    #[test]
    fn a_failed_half_block_frame_writes_every_cell_next_time() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut screen = HalfBlockScreen::default();
        assert_eq!(update(&mut screen, &solid(4, 2, 0, 0, 255)).1, 4);
        let red = solid(4, 2, 255, 0, 0);
        assert!(update_half_blocks(&mut Broken, &red, &mut Vec::new(), &mut screen).is_err());
        assert_eq!(update(&mut screen, &red).1, 4);
    }

    #[test]
    fn a_half_block_frame_of_odd_height_has_a_last_row_of_cells() {
        let pm = solid(2, 3, 0, 0, 255);
        let mut screen = HalfBlockScreen::default();
        assert_eq!(update(&mut screen, &pm).1, 4);
        assert_eq!(update(&mut screen, &pm).1, 0);
    }

    #[test]
    fn a_taller_half_block_frame_writes_its_new_rows() {
        // The new rows are black, the color of the fill that the screen
        // grows with.
        let mut screen = HalfBlockScreen::default();
        assert_eq!(update(&mut screen, &solid(2, 2, 0, 0, 0)).1, 2);
        assert_eq!(update(&mut screen, &solid(2, 4, 0, 0, 0)).1, 4);
    }

    #[test]
    fn a_half_block_frame_of_another_shape_writes_every_cell() {
        let mut screen = HalfBlockScreen::default();
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
        claim.show_through(TerminalGraphics::Kitty);
        assert!(!claim.restored());
        assert_eq!(restore_held(), Some(TerminalGraphics::Kitty));
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
        decode_png(B64.decode(payload).unwrap())
    }

    fn decode_png(png: Vec<u8>) -> Vec<u8> {
        let mut reader = png::Decoder::new(io::Cursor::new(png)).read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        pixels.truncate(info.buffer_size());
        pixels
    }

    /// The headers of the chunks that `writes` make, one write per slice.
    fn kitty_headers(writes: &[&[u8]]) -> Vec<String> {
        let mut buf = Vec::new();
        let mut chunks = KittyChunks::new(&mut buf, None);
        for data in writes {
            assert_eq!(chunks.write(data).unwrap(), data.len());
        }
        chunks.finish();
        String::from_utf8(buf)
            .unwrap()
            .split_terminator("\x1b\\")
            .map(|chunk| chunk.split_once(';').unwrap().0.to_owned())
            .collect()
    }

    #[test]
    fn a_full_kitty_chunk_waits_for_the_next_byte() {
        let full = [0; CHUNK_BYTES];
        assert_eq!(
            kitty_headers(&[&full, &full]),
            ["\x1b_Ga=T,f=100,q=2,m=1", "\x1b_Gm=0,q=2"]
        );
        // An empty write says nothing about more bytes, so the chunk is
        // still the last.
        assert_eq!(kitty_headers(&[&full, &[]]), ["\x1b_Ga=T,f=100,q=2,m=0"]);
    }

    #[test]
    fn a_kitty_image_of_many_chunks_marks_all_but_the_last() {
        // Noise does not compress, so the PNG takes several chunks.
        let mut pm = Pixmap::new(64, 64).unwrap();
        let mut expected = Vec::new();
        let mut seed = 1u32;
        for p in pm.pixels_mut() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let [r, g, b, _] = seed.to_be_bytes();
            *p = tiny_skia::ColorU8::from_rgba(r, g, b, 255).premultiply();
            expected.extend([r, g, b, 255]);
        }
        let mut out = Vec::new();
        emit_kitty(&mut out, &pm, None, &mut Vec::new()).unwrap();
        let out = String::from_utf8(out).unwrap();
        let chunks: Vec<&str> = out.split_terminator("\x1b\\").collect();
        assert!(chunks.len() > 2, "{} chunks", chunks.len());
        let mut payload = String::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let (header, data) = chunk.split_once(';').unwrap();
            let last = i + 1 == chunks.len();
            let expected = match (i, last) {
                (0, _) => "\x1b_Ga=T,f=100,q=2,m=1",
                (_, false) => "\x1b_Gm=1,q=2",
                (_, true) => "\x1b_Gm=0,q=2",
            };
            assert_eq!(header, expected, "chunk {i}");
            if !last {
                assert_eq!(data.len(), 4096, "chunk {i}");
            }
            payload.push_str(data);
        }
        assert_eq!(decode_png(B64.decode(payload).unwrap()), expected);
    }

    #[test]
    fn kitty_names_the_frame_and_not_an_inline_image() {
        let pm = Pixmap::new(1, 1).unwrap();
        let mut framed = Vec::new();
        emit_kitty(&mut framed, &pm, Some(KITTY_ANIMATION_ID), &mut Vec::new()).unwrap();
        assert_eq!(
            decode_kitty(framed, "\x1b_Ga=T,f=100,q=2,m=0,i=1042;"),
            [0, 0, 0, 0]
        );
        let mut inline = Vec::new();
        emit_kitty(&mut inline, &pm, None, &mut Vec::new()).unwrap();
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
        emit_kitty(&mut out, &pm, None, &mut Vec::new()).unwrap();
        assert_eq!(
            decode_kitty(out, "\x1b_Ga=T,f=100,q=2,m=0;"),
            [255, 0, 0, 128]
        );
    }
}
