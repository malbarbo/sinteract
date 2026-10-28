//! [`Stage`], where an engine runs its game, on a display of this process
//! or in a session with a server, with the same loop.

use std::cell::Cell;
use std::fmt;
use std::fs::File;
use std::io::{self, BufWriter, Read};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::Instant;

use super::{Display, OpenError, PresentError};
use crate::event::{Event, InputEvent, Interrupt};
use crate::scene::Scene;
use crate::session::{FrameError, Player, Session, SessionError, SessionEvent, StartError, Target};
use crate::session::{PlayerRange, SESSION_VAR};

/// The game of an engine, in a session with a server when the server runs
/// the engine, and on a window or the terminal otherwise, where the user is
/// [`Player`] 1, or in a window for each player, where the user plays every
/// player. The loop of the game is the same in all of them:
///
/// ```no_run
/// # use sinteract::display::{Stage, StageEvent, StageOptions};
/// # use sinteract::event::Interrupt;
/// # use sinteract::scene::Scene;
/// # use sinteract::session::Target;
/// # use sinteract::session::PlayerRange;
/// # fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let players = PlayerRange::new(1, 1).expect("1 to 1 is a range");
/// let options = StageOptions::default();
/// let (mut stage, _players) = Stage::open("My game", players, options)?;
/// loop {
///     match stage.wait(None) {
///         Ok(StageEvent::Tick) => stage.present(Target::All, Scene::new(400.0, 300.0))?,
///         Ok(StageEvent::Input { player, event }) => {}
///         Ok(StageEvent::Error(e)) => eprintln!("{e}"),
///         Err(Interrupt::Read(e)) => eprintln!("{e}"),
///         Err(Interrupt::Close) => break,
///         Err(Interrupt::Wake | Interrupt::Timeout) => {}
///     }
/// }
/// stage.close();
/// # Ok(())
/// # }
/// ```
pub struct Stage {
    inner: Inner,
    /// The players are 1 to this one, here and in a session.
    players: NonZeroU32,
}

/// What [`Stage::wait`] delivers.
#[derive(Debug)]
pub enum StageEvent {
    /// Time to draw the next frames, for every player.
    Tick,
    /// The input of `player`.
    Input { player: Player, event: InputEvent },
    /// The session dropped a message that broke a rule, and goes on. Only
    /// a session sends it.
    Error(SessionError),
}

/// Where [`Stage::open`] shows a game that no server runs.
#[derive(Clone, Copy, Debug, Default)]
pub struct StageOptions {
    pub display: Native,
    /// How many players play on a display of this process, each in a
    /// window of its own, or the fewest that the game takes when `None`.
    /// More than one needs a window, so [`Native::Auto`] does not fall
    /// back to the terminal. A session takes its players from the server.
    pub players: Option<NonZeroU32>,
    /// What the terminal adds, when the game goes there.
    pub terminal: TerminalOptions,
}

/// What an engine adds to a terminal display.
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
    /// The rows of text under the frame, which `Terminal::set_footer`
    /// fills. The frame shrinks to leave them free.
    pub footer_rows: u16,
}

/// How a terminal display or a `Printer` draws a scene.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum TerminalGraphics {
    /// The graphics protocol of Kitty.
    Kitty,
    /// DEC Sixel.
    Sixel,
    /// Two pixels per cell, as the foreground and background of `▀`. It
    /// needs truecolor.
    HalfBlocks,
}

/// The display of this process that shows a game.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Native {
    /// A window, and the terminal when no window opens, as over ssh.
    #[default]
    Auto,
    /// Only a window.
    Window,
    /// Only the terminal.
    Terminal {
        /// The graphics of the frames. `None` asks the terminal which ones
        /// it shows.
        graphics: Option<TerminalGraphics>,
    },
    /// No display. Outside a session, the stage ticks at 60 Hz and draws
    /// nothing, so a game runs without a screen, as in a test. A session
    /// still opens, since the server asks for it.
    Headless,
}

/// The most players on a display of this process, each in a window of its
/// own.
pub const MAX_LOCAL_PLAYERS: u32 = 8;

/// Whether this build shows a game on a display of its own. A build
/// without one, such as an engine on wasm32, shows a game only in a
/// session.
const HAS_NATIVE: bool = cfg!(all(feature = "terminal", feature = "window"));

/// Why [`Stage::open`] did not open a stage.
#[derive(Debug)]
pub enum StageError {
    /// Neither a window nor the terminal opened.
    Open(OpenError),
    /// The session did not start.
    Start(StartError),
    /// The game takes `range`, and `players` play on a display of this
    /// process, which is outside the range or above
    /// [`MAX_LOCAL_PLAYERS`].
    Players {
        range: PlayerRange,
        players: NonZeroU32,
    },
    /// Several players play on a display of this process, and the options
    /// ask for the terminal, which shows one.
    TerminalPlayers,
    /// Several players play on a display of this process, and the session
    /// has no graphical display for their windows.
    NoWindowPlayers,
    /// `SINTERACT_SESSION` asks for a session, or the build has no display
    /// of its own, and fd 3 or fd 4 is not open, or the platform has no such
    /// descriptors.
    NoSession,
}

impl Stage {
    /// Returns `true` if [`Stage::open`] starts a session, `false` if it
    /// opens a display of this process.
    pub fn opens_session() -> bool {
        !HAS_NATIVE || std::env::var_os(SESSION_VAR).is_some()
    }

    /// Open the stage of a game that takes `players`, with each player and
    /// its nickname. With `SINTERACT_SESSION` in the environment, or in a
    /// build with no display of its own, it starts a session that reads
    /// the server from fd 3 and writes to it on fd 4, and blocks until the
    /// start. Otherwise it opens a window with `title`, or the terminal, as
    /// `options` asks, with player 1 and an empty nickname. With several
    /// players in `options`, it opens a window for each one, with the
    /// number of the player in the title and an empty nickname, and closing
    /// any of them ends the stage. A window stays hidden until its first
    /// present, which gives it the size of the scene. A second stage in a
    /// session fails with [`OpenError::Busy`] while the first one owns fd 3
    /// and fd 4.
    pub fn open(
        title: &str,
        players: PlayerRange,
        options: StageOptions,
    ) -> Result<(Stage, Vec<(Player, String)>), StageError> {
        if Stage::opens_session() {
            return Stage::open_session(players);
        }
        let local = Stage::local_players(players, &options)?;
        if local.get() > 1 && options.display != Native::Headless {
            return open_players(title, local);
        }
        let display = open_local(title, options).map_err(StageError::Open)?;
        let stage = Stage {
            inner: Inner::Local(display),
            players: local,
        };
        Ok((stage, numbered(local)))
    }

    /// The players on a display of this process for a game that takes
    /// `players`, from `options` or the fewest of the game. Fails, before
    /// any display opens, as [`Stage::open`] would for these players.
    fn local_players(
        players: PlayerRange,
        options: &StageOptions,
    ) -> Result<NonZeroU32, StageError> {
        let local = options.players.unwrap_or(players.min());
        if !players.contains(local.get() as usize) || local.get() > MAX_LOCAL_PLAYERS {
            return Err(StageError::Players {
                range: players,
                players: local,
            });
        }
        match options.display {
            Native::Terminal { .. } if local.get() > 1 => Err(StageError::TerminalPlayers),
            _ => Ok(local),
        }
    }

    fn open_session(players: PlayerRange) -> Result<(Stage, Vec<(Player, String)>), StageError> {
        let (from_server, to_server) = take_session_fds()?;
        // No deadline until the start, since a read that times out there
        // loses the hello.
        let deadline = Rc::new(Cell::new(None));
        let reader = DeadlineReader {
            file: from_server,
            deadline: Rc::clone(&deadline),
        };
        // A start that fails drops both files, which is a close of the
        // stage too: without the release, a wasi instance could open no
        // stage again.
        let (session, players) = Session::start(players, reader, BufWriter::new(to_server))
            .map_err(|e| {
                release_session_fds();
                StageError::Start(e)
            })?;
        let count = session.players();
        let remote = Remote {
            tick_rate: session.tick_rate(),
            session: Some(session),
            deadline,
            ended: false,
        };
        let stage = Stage {
            inner: Inner::Remote(Box::new(remote)),
            players: count,
        };
        Ok((stage, players))
    }

    /// Block until the next event, or until `deadline` and then return
    /// [`Interrupt::Timeout`], as [`Display::wait_event`] does. A session
    /// never returns [`Interrupt::Wake`]. The end of the session is
    /// [`Interrupt::Close`], and a stream that fails or breaks is
    /// [`Interrupt::Read`], with a Close right after.
    pub fn wait(&mut self, deadline: Option<Instant>) -> Result<StageEvent, Interrupt> {
        match &mut self.inner {
            Inner::Local(display) => match display.wait_event(deadline)? {
                Event::Tick => Ok(StageEvent::Tick),
                Event::Input(event) => Ok(StageEvent::Input {
                    player: Player::LOCAL,
                    event,
                }),
            },
            #[cfg(feature = "window")]
            Inner::Players(players) => players.wait(deadline),
            Inner::Remote(remote) => remote.wait(deadline),
        }
    }

    /// Show `scene` to `to`. A player that the stage does not have fails
    /// with [`PresentError::NoPlayer`], here and in a session. A window of
    /// each player shows the last frame to all or to that player at the
    /// next wait. A draw of a window that fails after a present, at the
    /// wait, at a frame callback or at a redraw, fails the next present,
    /// which still keeps its scene. A session writes the frame, with each
    /// image that the server does not have, and still writes after the
    /// end of the stream of the server, so the views get the last frames.
    pub fn present(&mut self, to: Target, scene: Scene) -> Result<(), PresentError> {
        let has_player = || match to {
            Target::Player(player) if player.number() > self.players => {
                Err(PresentError::NoPlayer(player))
            }
            _ => Ok(()),
        };
        match &mut self.inner {
            Inner::Local(display) => {
                has_player()?;
                display.present(scene)
            }
            #[cfg(feature = "window")]
            Inner::Players(players) => players.present(to, scene),
            Inner::Remote(remote) => {
                has_player()?;
                let session = remote.session.as_mut().ok_or(PresentError::Closed)?;
                session.write_frame(to, &scene).map_err(|e| match e {
                    FrameError::Full(e) => PresentError::Full(e),
                    FrameError::Io(e) => PresentError::Io(e),
                })
            }
        }
    }

    /// The rows of text under the frame, as [`Display::footer_rows`]. A
    /// session has none.
    pub fn footer_rows(&self) -> u16 {
        match &self.inner {
            Inner::Local(display) => display.footer_rows(),
            #[cfg(feature = "window")]
            Inner::Players(..) => 0,
            Inner::Remote(_) => 0,
        }
    }

    /// Show the last [`Stage::footer_rows`] of `lines` under the frame.
    pub fn set_footer(&mut self, lines: &[String]) -> Result<(), PresentError> {
        match &mut self.inner {
            Inner::Local(display) => display.set_footer(lines),
            #[cfg(feature = "window")]
            Inner::Players(..) => Ok(()),
            Inner::Remote(_) => Ok(()),
        }
    }

    /// The rate of the ticks in thousandths of a hertz, 60000 for 60 Hz. In
    /// a session the rate is the one of the server, the same until the
    /// end. A server with a timer ticks at this rate, so a game may move by
    /// a step of it at each tick. A page that ticks at each refresh of the
    /// screen gives a nominal rate, and a game that keeps time measures it
    /// with its own clock. On a display of this process it is
    /// [`Display::tick_rate`], which changes with the monitor, so a game
    /// reads it again at each tick.
    pub fn tick_rate(&self) -> NonZeroU32 {
        match &self.inner {
            Inner::Local(display) => display.tick_rate(),
            #[cfg(feature = "window")]
            Inner::Players(players) => players.window.tick_rate(),
            Inner::Remote(remote) => remote.tick_rate,
        }
    }

    /// End the stage. A session closes fd 3 and fd 4, which ends the room
    /// for the server. A second call does nothing, and drop calls it.
    pub fn close(&mut self) {
        match &mut self.inner {
            Inner::Local(display) => display.close(),
            #[cfg(feature = "window")]
            Inner::Players(players) => players.window.close(),
            Inner::Remote(remote) => remote.close(),
        }
    }
}

/// The display that `options` asks for.
#[cfg(all(feature = "terminal", feature = "window"))]
fn open_local(title: &str, options: StageOptions) -> Result<Box<dyn Display>, OpenError> {
    use super::{Terminal, Window};
    let window = || Window::open_seats(&[title.to_owned()]);
    match options.display {
        Native::Auto => super::driver::window_or_terminal(window, options.terminal),
        Native::Window => Ok(Box::new(window()?)),
        Native::Terminal { graphics } => Ok(Box::new(Terminal::open(options.terminal, graphics)?)),
        Native::Headless => Ok(Box::new(super::headless::Headless::new())),
    }
}

/// The players 1 to `players`, each with an empty nickname.
fn numbered(players: NonZeroU32) -> Vec<(Player, String)> {
    (1..=players.get())
        .filter_map(NonZeroU32::new)
        .map(|number| (Player(number), String::new()))
        .collect()
}

/// A window for each of `players`.
#[cfg(feature = "window")]
fn open_players(
    title: &str,
    players: NonZeroU32,
) -> Result<(Stage, Vec<(Player, String)>), StageError> {
    let all = numbered(players);
    let titles: Vec<_> = all
        .iter()
        .map(|(player, _)| format!("{title} (player {})", player.number()))
        .collect();
    let window = super::Window::open_seats(&titles).map_err(|e| match e {
        OpenError::NoWindow => StageError::NoWindowPlayers,
        e => StageError::Open(e),
    })?;
    let stage = Stage {
        inner: Inner::Players(Box::new(Players {
            window,
            pending: Pending::new(players),
        })),
        players,
    };
    Ok((stage, all))
}

#[cfg(not(feature = "window"))]
fn open_players(
    _title: &str,
    _players: NonZeroU32,
) -> Result<(Stage, Vec<(Player, String)>), StageError> {
    Err(StageError::NoWindowPlayers)
}

#[cfg(not(all(feature = "terminal", feature = "window")))]
fn open_local(_title: &str, _options: StageOptions) -> Result<Box<dyn Display>, OpenError> {
    Err(OpenError::Platform("this build has no display".into()))
}

impl fmt::Display for StageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StageError::Open(e) => e.fmt(f),
            StageError::Start(e) => e.fmt(f),
            StageError::Players { players, .. } if players.get() > MAX_LOCAL_PLAYERS => write!(
                f,
                "{players} players do not fit on this machine, which takes at most \
                 {MAX_LOCAL_PLAYERS}"
            ),
            StageError::Players { range, players } => write!(
                f,
                "the game takes from {} to {} players, and {players} play here",
                range.min(),
                range.max()
            ),
            StageError::TerminalPlayers => {
                f.write_str("the terminal shows one player, and several play here")
            }
            StageError::NoWindowPlayers => f.write_str(
                "several players need a window each, and this session has no graphical display",
            ),
            StageError::NoSession => {
                write!(f, "{SESSION_VAR} is set, and fd 3 or fd 4 is not open")
            }
        }
    }
}

impl std::error::Error for StageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StageError::Open(e) => Some(e),
            StageError::Start(e) => Some(e),
            StageError::Players { .. }
            | StageError::TerminalPlayers
            | StageError::NoWindowPlayers
            | StageError::NoSession => None,
        }
    }
}

impl fmt::Debug for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.inner {
            Inner::Local(_) => "Local",
            #[cfg(feature = "window")]
            Inner::Players(..) => "Players",
            Inner::Remote(_) => "Remote",
        };
        f.debug_tuple("Stage").field(&kind).finish()
    }
}

enum Inner {
    Local(Box<dyn Display>),
    #[cfg(feature = "window")]
    Players(Box<Players>),
    // Boxed, since a session holds its buffers inline.
    Remote(Box<Remote>),
}

/// A window for each of the players.
#[cfg(feature = "window")]
struct Players {
    window: super::Window,
    pending: Pending,
}

#[cfg(feature = "window")]
impl Players {
    /// Keep `scene` for the seats of `to` until the next wait. A draw
    /// that failed since the last present fails this one, which still
    /// keeps `scene`.
    fn present(&mut self, to: Target, scene: Scene) -> Result<(), PresentError> {
        if !self.window.is_open() {
            return Err(PresentError::Closed);
        }
        self.pending.keep(to, scene)?;
        self.window.take_failed()
    }

    /// Draw the scenes that wait, and block until the next event.
    fn wait(&mut self, deadline: Option<Instant>) -> Result<StageEvent, Interrupt> {
        let pending = &mut self.pending;
        let seated = self.window.wait_seated(deadline, |window| {
            pending.draw(|player, scene| {
                // Pending has the players of the window, and a closed window
                // ends the wait with Close.
                let _ = window.present_to(Target::Player(player), scene);
            });
        });
        match seated? {
            super::inbox::Seated::Tick => Ok(StageEvent::Tick),
            super::inbox::Seated::Input(seat, event) => Ok(StageEvent::Input {
                player: Player(NonZeroU32::MIN.saturating_add(seat)),
                event,
            }),
        }
    }
}

/// The last scene of each player since the last draw. A player that gets
/// a scene for every player and then one of its own before the draw
/// draws only its own.
#[cfg(feature = "window")]
struct Pending {
    /// The scene of player k at index k - 1.
    scenes: Vec<Option<Rc<Scene>>>,
}

#[cfg(feature = "window")]
impl Pending {
    /// No scenes for the players 1 to `players`.
    fn new(players: NonZeroU32) -> Self {
        Self {
            scenes: vec![None; players.get() as usize],
        }
    }

    /// Keep `scene` for the players of `to`, in place of the scene that
    /// each one had.
    fn keep(&mut self, to: Target, scene: Scene) -> Result<(), PresentError> {
        let scene = Rc::new(scene);
        match to {
            Target::All => self.scenes.fill(Some(scene)),
            Target::Player(player) => {
                let seat = player.number().get() as usize - 1;
                let Some(pending) = self.scenes.get_mut(seat) else {
                    return Err(PresentError::NoPlayer(player));
                };
                *pending = Some(scene);
            }
        }
        Ok(())
    }

    /// Hand each scene that waits to `draw`, with its player.
    fn draw(&mut self, mut draw: impl FnMut(Player, Rc<Scene>)) {
        let players = (1..).map_while(NonZeroU32::new).map(Player);
        for (player, pending) in players.zip(&mut self.scenes) {
            if let Some(scene) = pending.take() {
                draw(player, scene);
            }
        }
    }
}

/// A session with a server, on fd 3 and fd 4.
struct Remote {
    /// The rate of the ticks, from the start of the session.
    tick_rate: NonZeroU32,
    /// `None` after [`Stage::close`].
    session: Option<Session<DeadlineReader, BufWriter<File>>>,
    /// The deadline of the reader of the session, which has no other way
    /// to it.
    deadline: Rc<Cell<Option<Instant>>>,
    /// The stream of the server ended or failed, and every wait returns
    /// Close.
    ended: bool,
}

impl Remote {
    fn wait(&mut self, deadline: Option<Instant>) -> Result<StageEvent, Interrupt> {
        let Some(session) = self.session.as_mut().filter(|_| !self.ended) else {
            return Err(Interrupt::Close);
        };
        self.deadline.set(deadline);
        let event = session.wait();
        self.deadline.set(None);
        match event {
            Ok(SessionEvent::Tick) => Ok(StageEvent::Tick),
            Ok(SessionEvent::Input { player, event }) => Ok(StageEvent::Input { player, event }),
            Ok(SessionEvent::Error(e)) => Ok(StageEvent::Error(e)),
            Ok(SessionEvent::End) => {
                self.ended = true;
                Err(Interrupt::Close)
            }
            // The session keeps its bytes over an error of the reader, so
            // the next wait goes on from there.
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Err(Interrupt::Timeout),
            // A read of a file that fails does not come back.
            Err(e) => {
                self.ended = true;
                Err(Interrupt::Read(e))
            }
        }
    }

    fn close(&mut self) {
        if self.session.take().is_some() {
            release_session_fds();
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.close();
    }
}

/// Fd 3, which a read waits on until a deadline. The read fails with
/// [`io::ErrorKind::TimedOut`] when the deadline passes first.
struct DeadlineReader {
    file: File,
    deadline: Rc<Cell<Option<Instant>>>,
}

impl Read for DeadlineReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(deadline) = self.deadline.get() {
            wait_readable(&self.file, deadline)?;
        }
        self.file.read(buf)
    }
}

/// Wait until `file` has bytes, its end or an error to read, or fail with
/// [`io::ErrorKind::TimedOut`] at `deadline`.
#[cfg(any(unix, target_os = "wasi"))]
fn wait_readable(file: &File, deadline: Instant) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        // Rounded up, so the wait does not end a little before the
        // deadline and spin.
        let ms = left.as_micros().div_ceil(1000);
        let timeout = i32::try_from(ms).unwrap_or(i32::MAX);
        let mut fd = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `fd` is one valid pollfd, and the call reads one.
        match unsafe { libc::poll(&mut fd, 1, timeout) } {
            0 if Instant::now() >= deadline => return Err(io::ErrorKind::TimedOut.into()),
            0 => {}
            n if n > 0 => return Ok(()),
            _ => {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }
    }
}

/// A platform with no poll reads with no deadline.
#[cfg(not(any(unix, target_os = "wasi")))]
fn wait_readable(_file: &File, _deadline: Instant) -> io::Result<()> {
    Ok(())
}

/// Set while a stage owns fd 3 and fd 4, since a second owner of the
/// same descriptors would close them twice.
#[cfg(any(unix, target_os = "wasi"))]
static TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Fd 3 and fd 4, which the server opened for the session.
#[cfg(any(unix, target_os = "wasi"))]
fn take_session_fds() -> Result<(File, File), StageError> {
    use std::os::fd::FromRawFd;
    use std::sync::atomic::Ordering;

    use crate::session::{ENGINE_TO_SERVER_FD, SERVER_TO_ENGINE_FD};

    // SAFETY: F_GETFD only reads the flags of a descriptor, and fails on
    // one that is not open.
    let open = |fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1;
    if !open(SERVER_TO_ENGINE_FD) || !open(ENGINE_TO_SERVER_FD) {
        return Err(StageError::NoSession);
    }
    if TAKEN.swap(true, Ordering::SeqCst) {
        return Err(StageError::Open(OpenError::Busy));
    }
    // SAFETY: both are open, SINTERACT_SESSION or a build with no display
    // of its own says that the server opened them for the session, and
    // TAKEN gives them to one stage.
    Ok(unsafe {
        (
            File::from_raw_fd(SERVER_TO_ENGINE_FD),
            File::from_raw_fd(ENGINE_TO_SERVER_FD),
        )
    })
}

/// Let the next stage take fd 3 and fd 4. Only a wasi host keeps the
/// descriptors open after a close, for the next session of the instance.
/// Elsewhere a closed descriptor may come back as a file of another
/// owner, so the session of a process stays its only one.
#[cfg(any(unix, target_os = "wasi"))]
fn release_session_fds() {
    if cfg!(target_os = "wasi") {
        TAKEN.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(not(any(unix, target_os = "wasi")))]
fn take_session_fds() -> Result<(File, File), StageError> {
    Err(StageError::NoSession)
}

#[cfg(not(any(unix, target_os = "wasi")))]
fn release_session_fds() {}

#[cfg(all(test, unix))]
pub(super) mod tests {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::time::Duration;

    use super::*;
    use crate::wire::framing::{self, Side};
    use crate::wire::{server_to_engine, testing};

    const RATE: NonZeroU32 = NonZeroU32::new(60_000).expect("60 Hz is not zero");

    /// The two ends of a pipe, to read and to write.
    fn pipe() -> (File, File) {
        let mut fds = [0; 2];
        // SAFETY: `fds` holds the two descriptors that pipe writes.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: pipe opened both, and nothing else owns them.
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    }

    /// The server side of a session on two pipes.
    pub(in crate::display) struct Server {
        /// What the server writes to the engine, until the end.
        to_engine: Option<File>,
        /// What the engine wrote, which a read takes without blocking.
        from_engine: File,
    }

    impl Server {
        fn writer(&mut self) -> &mut File {
            self.to_engine.as_mut().expect("the server did not end")
        }

        pub(in crate::display) fn tick(&mut self) {
            server_to_engine::write_tick(self.writer()).unwrap();
        }

        /// Close the stream to the engine.
        pub(in crate::display) fn end(&mut self) {
            self.to_engine = None;
        }

        pub(in crate::display) fn key(&mut self, key: crate::event::KeyEvent) {
            self.input(InputEvent::Key(key));
        }

        pub(in crate::display) fn pad(&mut self, pad: crate::event::PadEvent) {
            self.input(InputEvent::Pad(pad));
        }

        fn input(&mut self, event: InputEvent) {
            let player = NonZeroU32::new(1).unwrap();
            server_to_engine::write_input(self.writer(), player, &event).unwrap();
        }

        /// The messages that the engine wrote since the last call.
        pub(in crate::display) fn read(&mut self) -> Vec<testing::Message> {
            let mut bytes = Vec::new();
            let mut buf = [0; 4096];
            loop {
                match self.from_engine.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => bytes.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => panic!("{e}"),
                }
            }
            let mut rest = &bytes[..];
            let mut messages = Vec::new();
            while let Some((payload, after)) = framing::split_message(rest, Side::Engine).unwrap() {
                messages.extend(testing::decode(payload).unwrap());
                rest = after;
            }
            assert!(rest.is_empty(), "the engine wrote part of a message");
            messages
        }
    }

    /// A stage in a session on two pipes, after a start of one player, and
    /// the server, which already read the hello.
    pub(in crate::display) fn session() -> (Stage, Server) {
        let (from_server, mut to_engine) = pipe();
        let (from_engine, to_server) = pipe();
        // SAFETY: F_SETFL only sets the flags of an open descriptor.
        unsafe { libc::fcntl(from_engine.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
        server_to_engine::write_start(&mut to_engine, &["Ana"], RATE).unwrap();
        let deadline = Rc::new(Cell::new(None));
        let reader = DeadlineReader {
            file: from_server,
            deadline: Rc::clone(&deadline),
        };
        let players = PlayerRange::new(1, 1).unwrap();
        let (session, _) = Session::start(players, reader, BufWriter::new(to_server)).unwrap();
        let count = session.players();
        let remote = Remote {
            tick_rate: session.tick_rate(),
            session: Some(session),
            deadline,
            ended: false,
        };
        let stage = Stage {
            inner: Inner::Remote(Box::new(remote)),
            players: count,
        };
        let mut server = Server {
            to_engine: Some(to_engine),
            from_engine,
        };
        // The hello, which is no message of `testing`.
        let mut hello = [0; 4096];
        let _ = server.from_engine.read(&mut hello);
        (stage, server)
    }

    #[test]
    fn a_wait_past_its_deadline_times_out_and_the_session_goes_on() {
        let (mut stage, mut server) = session();
        let deadline = Instant::now() + Duration::from_millis(20);
        assert!(matches!(
            stage.wait(Some(deadline)),
            Err(Interrupt::Timeout)
        ));
        assert!(Instant::now() >= deadline);
        server.tick();
        assert!(matches!(stage.wait(Some(deadline)), Ok(StageEvent::Tick)));
        assert!(matches!(server.read()[..], [testing::Message::TickTaken]));
    }

    #[test]
    fn a_frame_to_a_player_the_stage_lacks_fails_and_writes_nothing() {
        let (mut stage, mut server) = session();
        let other = Player(NonZeroU32::new(2).unwrap());
        assert!(matches!(
            stage.present(Target::Player(other), Scene::new(10.0, 10.0)),
            Err(PresentError::NoPlayer(p)) if p == other
        ));
        assert!(server.read().is_empty());
    }

    #[test]
    fn a_wait_with_no_deadline_blocks_until_the_server_writes() {
        let (mut stage, mut server) = session();
        let mut to_engine = server.to_engine.take().unwrap();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            server_to_engine::write_tick(&mut to_engine).unwrap();
        });
        assert!(matches!(stage.wait(None), Ok(StageEvent::Tick)));
        writer.join().unwrap();
        assert!(matches!(stage.wait(None), Err(Interrupt::Close)));
    }

    #[cfg(all(feature = "terminal", feature = "window"))]
    #[test]
    fn a_stage_with_native_none_ticks_for_its_players() {
        let range = PlayerRange::new(1, 4).expect("1 to 4 is a range");
        let options = StageOptions {
            display: Native::Headless,
            players: NonZeroU32::new(2),
            ..StageOptions::default()
        };
        let (mut stage, players) = Stage::open("test", range, options).unwrap();
        let numbers: Vec<_> = players.iter().map(|(p, _)| p.number().get()).collect();
        assert_eq!(numbers, [1, 2]);
        assert!(matches!(stage.wait(None), Ok(StageEvent::Tick)));
        let ana = Player(NonZeroU32::MIN);
        assert!(
            stage
                .present(Target::Player(ana), Scene::new(10.0, 10.0))
                .is_ok()
        );
        stage.close();
        assert!(matches!(stage.wait(None), Err(Interrupt::Close)));
    }

    #[cfg(all(feature = "terminal", feature = "window"))]
    fn open_with(players: u32, local: Option<u32>, display: Native) -> Result<Stage, StageError> {
        let range = PlayerRange::new(1, players).expect("a range");
        let options = StageOptions {
            display,
            players: local.and_then(NonZeroU32::new),
            ..StageOptions::default()
        };
        Stage::open("test", range, options).map(|(stage, _)| stage)
    }

    #[cfg(all(feature = "terminal", feature = "window"))]
    #[test]
    fn local_players_outside_the_range_of_the_game_fail() {
        for (max, local) in [(1, 2), (16, 9)] {
            let result = open_with(max, Some(local), Native::Headless);
            assert!(
                matches!(result, Err(StageError::Players { players, .. }) if players.get() == local)
            );
        }
    }

    #[cfg(all(feature = "terminal", feature = "window"))]
    #[test]
    fn several_local_players_need_a_window() {
        let result = open_with(2, Some(2), Native::Terminal { graphics: None });
        assert!(matches!(result, Err(StageError::TerminalPlayers)));
    }

    #[cfg(all(feature = "terminal", feature = "window"))]
    #[test]
    fn no_local_players_means_the_fewest_of_the_game() {
        let range = PlayerRange::new(2, 4).expect("2 to 4 is a range");
        let options = StageOptions {
            display: Native::Terminal { graphics: None },
            ..StageOptions::default()
        };
        let result = Stage::open("test", range, options);
        assert!(matches!(result, Err(StageError::TerminalPlayers)));
    }

    /// Two players with nothing to draw.
    #[cfg(feature = "window")]
    fn two_pending() -> Pending {
        Pending::new(NonZeroU32::new(2).expect("2 is not zero"))
    }

    /// Each scene that `pending` draws, as its player and its width.
    #[cfg(feature = "window")]
    fn drawn(pending: &mut Pending) -> Vec<(u32, f32)> {
        let mut drawn = Vec::new();
        pending.draw(|player, scene| drawn.push((player.number().get(), scene.width())));
        drawn
    }

    #[cfg(feature = "window")]
    fn second() -> Target {
        Target::Player(Player(NonZeroU32::new(2).expect("2 is not zero")))
    }

    #[cfg(feature = "window")]
    #[test]
    fn a_scene_to_a_player_after_one_to_all_replaces_only_its_own() {
        let mut pending = two_pending();
        pending.keep(Target::All, Scene::new(1.0, 1.0)).unwrap();
        pending.keep(second(), Scene::new(2.0, 1.0)).unwrap();
        assert_eq!(drawn(&mut pending), [(1, 1.0), (2, 2.0)]);
        assert_eq!(drawn(&mut pending), []);
    }

    #[cfg(feature = "window")]
    #[test]
    fn a_scene_to_all_after_one_to_a_player_replaces_it() {
        let mut pending = two_pending();
        pending.keep(second(), Scene::new(2.0, 1.0)).unwrap();
        pending.keep(Target::All, Scene::new(1.0, 1.0)).unwrap();
        assert_eq!(drawn(&mut pending), [(1, 1.0), (2, 1.0)]);
    }

    #[cfg(feature = "window")]
    #[test]
    fn a_scene_to_a_player_the_stage_lacks_fails_and_keeps_nothing() {
        let mut pending = two_pending();
        let third = Player(NonZeroU32::new(3).expect("3 is not zero"));
        assert!(matches!(
            pending.keep(Target::Player(third), Scene::new(3.0, 1.0)),
            Err(PresentError::NoPlayer(p)) if p == third
        ));
        assert_eq!(drawn(&mut pending), []);
    }
}
