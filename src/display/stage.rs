//! [`Stage`], where an engine runs its game, on a display of this process
//! or in a session with a server, with the same loop.

use std::cell::Cell;
use std::fmt;
use std::fs::File;
use std::io::{self, BufWriter, Read};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::Instant;

use super::{Display, PresentError, TerminalError, WindowError};
use crate::event::{Event, InputEvent};
use crate::scene::Scene;
use crate::session::PlayerRange;
use crate::session::{
    FrameError, FrameRate, Player, Session, SessionError, SessionEvent, StartError, Target,
};

/// The game of an engine, in a session with a server when the server runs
/// the engine, and on a window or the terminal otherwise, where the user is
/// [`Player`] 1, or in a window for each player, where the user plays every
/// player. The loop of the game is the same in all of them:
///
/// ```no_run
/// # use sinteract::display::{Stage, StageEvent, StageMode, TerminalOptions};
/// # use sinteract::scene::Scene;
/// # use sinteract::session::Target;
/// # use sinteract::session::PlayerRange;
/// # fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let players = PlayerRange::new(1, 1).expect("1 to 1 is a range");
/// let mode = StageMode::auto(None, TerminalOptions::default());
/// let (mut stage, _players) = Stage::open("My game", players, mode)?;
/// loop {
///     match stage.wait(None)? {
///         StageEvent::Frame => stage.present(Target::All, Scene::new(400.0, 300.0))?,
///         StageEvent::Input { player, event } => {}
///         StageEvent::Error(e) => eprintln!("{e}"),
///         StageEvent::Timeout => {}
///         StageEvent::Close => break,
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
    Frame,
    /// The input of `player`.
    Input { player: Player, event: InputEvent },
    /// The session dropped a message that broke a rule, and goes on. Only
    /// a session sends it.
    Error(SessionError),
    /// The deadline passed with nothing to deliver.
    Timeout,
    /// The user or the platform ended the input, or the server ended the
    /// session. Every wait from now on returns it.
    Close,
}

/// What an engine adds to a terminal display.
#[derive(Clone, Copy, Debug, Default)]
pub struct TerminalOptions {
    /// Runs on the reader thread when the user presses Ctrl-C, after the
    /// Close goes into the queue. Raw mode turns off the signal of Ctrl-C,
    /// so an engine stops here the code that never calls `wait`. With
    /// stdin redirected no reader takes the keys, and Ctrl-C raises SIGINT
    /// instead.
    pub on_interrupt: Option<fn()>,
    /// Print the last frame on the main screen at close, where it stays
    /// with the rest of the output of the program. Without it the frame
    /// goes away with the alt screen.
    pub keep_last_frame: bool,
    /// The rows of text under the frame, which [`Stage::set_footer`]
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

/// Where [`Stage::open`] shows a game, on a display of this process or in a
/// session with a server.
#[derive(Clone, Copy, Debug)]
pub enum StageMode {
    /// A window for each player.
    Window {
        /// How many players play, each in a window of its own, or the
        /// fewest that the game takes when `None`.
        players: Option<NonZeroU32>,
    },
    /// The terminal, which shows one player.
    Terminal {
        options: TerminalOptions,
        /// The graphics of the frames. `None` asks the terminal which ones
        /// it shows.
        graphics: Option<TerminalGraphics>,
    },
    /// No display. The stage starts a frame at 60 Hz and draws nothing, so a game
    /// runs without a screen, as in a test.
    Headless {
        /// How many players play, or the fewest that the game takes when
        /// `None`.
        players: Option<NonZeroU32>,
    },
    /// A session with the server that runs this process, on fd 3 and fd 4.
    /// The server gives the players.
    Session,
}

impl StageMode {
    /// A window for `players` when the process has a window system, and
    /// the terminal with `options` otherwise, as over ssh. More than one
    /// player always gets windows, since the terminal shows one. Outside
    /// macOS the check builds the event loop of the windows, so it runs on
    /// the thread that opens the stage.
    pub fn auto(players: Option<NonZeroU32>, options: TerminalOptions) -> StageMode {
        StageMode::choose(players, options, has_window_system)
    }

    /// The display of [`StageMode::auto`], with `has_window_system` for the
    /// check, which runs only for one player.
    fn choose(
        players: Option<NonZeroU32>,
        options: TerminalOptions,
        has_window_system: impl FnOnce() -> bool,
    ) -> StageMode {
        if players.is_some_and(|players| players.get() > 1) || has_window_system() {
            StageMode::Window { players }
        } else {
            StageMode::Terminal {
                options,
                graphics: None,
            }
        }
    }
}

/// The most players on a display of this process, each in a window of its
/// own.
pub const MAX_LOCAL_PLAYERS: u32 = 8;

/// Why a frame of [`Stage::present`] did not go out.
#[derive(Debug)]
pub enum StagePresentError {
    /// The display failed to show the frame.
    Display(PresentError),
    /// The session did not write the frame.
    Session(FrameError),
    /// The frame goes to a player that the stage does not have, such as a
    /// player of an earlier stage, and nothing went out.
    NoPlayer(Player),
}

impl fmt::Display for StagePresentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StagePresentError::Display(e) => e.fmt(f),
            StagePresentError::Session(e) => e.fmt(f),
            StagePresentError::NoPlayer(player) => {
                write!(f, "the stage has no player {}", player.number())
            }
        }
    }
}

impl std::error::Error for StagePresentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StagePresentError::Display(e) => Some(e),
            StagePresentError::Session(e) => Some(e),
            StagePresentError::NoPlayer(_) => None,
        }
    }
}

/// Why [`Stage::open`] did not open a stage.
#[derive(Debug)]
pub enum StageError {
    /// A window did not open.
    Window(WindowError),
    /// The terminal did not open.
    Terminal(TerminalError),
    /// Another stage of this process holds fd 3 and fd 4, or the
    /// [`Worlds`](super::Worlds) already has an open stage.
    Busy,
    /// The session did not start.
    Start(StartError),
    /// The game takes `range`, and `players` play on a display of this
    /// process, which is outside the range or above
    /// [`MAX_LOCAL_PLAYERS`].
    Players {
        range: PlayerRange,
        players: NonZeroU32,
    },
    /// The game takes `range`, at least two players, and the display is
    /// the terminal, which shows one.
    TerminalPlayers { range: PlayerRange },
    /// Several players play on a display of this process, and the session
    /// has no graphical display for their windows.
    NoWindowPlayers,
    /// The mode is [`StageMode::Session`], and fd 3 or fd 4 is not open, or
    /// the platform has no such descriptors.
    NoSession,
    /// `fd`, which is fd 3 or fd 4, is open and is not a pipe or a socket,
    /// so no server opened it for the session, as with a `3>file` in a
    /// shell.
    NotSessionFd { fd: i32 },
    /// The mode asks for a window or the terminal, and the build leaves
    /// out its feature. [`StageMode::Headless`] needs either one.
    NoDisplay,
}

impl Stage {
    /// Open the stage of a game that takes `players`, with each player and
    /// its nickname. With [`StageMode::Session`] it starts a session that
    /// reads the server from fd 3 and writes to it on fd 4, and blocks
    /// until the start. Otherwise it opens a window with `title`, or the
    /// terminal, as `mode` asks, with player 1 and an empty nickname. With several
    /// players in `mode`, it opens a window for each one, with the
    /// number of the player in the title and an empty nickname, and closing
    /// any of them ends the stage. A window stays hidden until its first
    /// present, which gives it the size of the scene. A second stage in a
    /// session fails with [`StageError::Busy`] while the first one owns fd 3
    /// and fd 4.
    pub fn open(
        title: &str,
        players: PlayerRange,
        mode: StageMode,
    ) -> Result<(Stage, Vec<(Player, String)>), StageError> {
        match mode {
            StageMode::Session => Stage::open_session(players),
            StageMode::Window { players: local } => {
                let local = Stage::local_players(players, local)?;
                if local.get() > 1 {
                    return open_players(title, local);
                }
                Stage::open_local(local, open_window(title)?)
            }
            StageMode::Terminal { options, graphics } => {
                if players.min().get() > 1 {
                    return Err(StageError::TerminalPlayers { range: players });
                }
                Stage::open_local(NonZeroU32::MIN, open_terminal(options, graphics)?)
            }
            StageMode::Headless { players: local } => {
                let local = Stage::local_players(players, local)?;
                Stage::open_local(local, open_headless()?)
            }
        }
    }

    /// The stage of `players` on `display`, which shows them all.
    fn open_local(
        players: NonZeroU32,
        display: Box<dyn Display>,
    ) -> Result<(Stage, Vec<(Player, String)>), StageError> {
        let stage = Stage {
            inner: Inner::Local(display),
            players,
        };
        Ok((stage, numbered(players)))
    }

    /// The players of a display of this process for a game that takes
    /// `players`, from `local` or the fewest of the game. Fails, before any
    /// display opens, as [`Stage::open`] would for these players.
    fn local_players(
        players: PlayerRange,
        local: Option<NonZeroU32>,
    ) -> Result<NonZeroU32, StageError> {
        let local = local.unwrap_or(players.min());
        if !players.contains(local.get() as usize) || local.get() > MAX_LOCAL_PLAYERS {
            return Err(StageError::Players {
                range: players,
                players: local,
            });
        }
        Ok(local)
    }

    fn open_session(players: PlayerRange) -> Result<(Stage, Vec<(Player, String)>), StageError> {
        let (from_server, to_server, taken) = session_fds::take()?;
        // No deadline until the start, since a read that times out there
        // loses the hello.
        let deadline = Rc::new(Deadline::default());
        let reader = DeadlineReader {
            file: from_server,
            deadline: Rc::clone(&deadline),
        };
        let (session, players) = Session::start(players, reader, BufWriter::new(to_server))
            .map_err(StageError::Start)?;
        let count = session.player_count();
        let remote = Remote {
            session,
            deadline,
            ended: false,
            _taken: taken,
        };
        let stage = Stage {
            inner: Inner::Remote(Box::new(remote)),
            players: count,
        };
        Ok((stage, players))
    }

    /// Block until the next event, or until `deadline` and then return
    /// [`StageEvent::Timeout`], as [`Display::wait`] does. The end of the
    /// session is [`StageEvent::Close`]. A read of the input that fails, a
    /// stream of the server that breaks or a write to the server that
    /// fails returns its error, and every wait after it returns Close.
    pub fn wait(&mut self, deadline: Option<Instant>) -> io::Result<StageEvent> {
        match &mut self.inner {
            Inner::Local(display) => loop {
                return Ok(match display.wait(deadline)? {
                    Event::Frame => StageEvent::Frame,
                    Event::Input(event) => StageEvent::Input {
                        player: Player::LOCAL,
                        event,
                    },
                    // No Sender of the display leaves the stage.
                    Event::Wake => continue,
                    Event::Timeout => StageEvent::Timeout,
                    Event::Close => StageEvent::Close,
                });
            },
            #[cfg(feature = "window")]
            Inner::Players(players) => players.wait(deadline),
            Inner::Remote(remote) => remote.wait(deadline),
        }
    }

    /// Show `scene` to `to`. A player that the stage does not have fails
    /// with [`StagePresentError::NoPlayer`], here and in a session. A window of
    /// each player shows the last frame to all or to that player at the
    /// next wait. A draw of a window that fails after a present, at the
    /// wait, at a frame callback or at a redraw, fails the next present,
    /// which still keeps its scene. A session writes the frame, with each
    /// image that the server does not have, and still writes after the
    /// end of the stream of the server, so the views get the last frames.
    pub fn present(&mut self, to: Target, scene: Scene) -> Result<(), StagePresentError> {
        if let Target::Player(player) = to
            && player.number() > self.players
        {
            return Err(StagePresentError::NoPlayer(player));
        }
        match &mut self.inner {
            Inner::Local(display) => display.present(scene).map_err(StagePresentError::Display),
            #[cfg(feature = "window")]
            Inner::Players(players) => players.present(to, scene),
            Inner::Remote(remote) => (remote.session)
                .write_frame(to, &scene)
                .map_err(StagePresentError::Session),
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

    /// The rate of the frames. In a session the rate is the one of the
    /// server, the same until the
    /// end. A server with a timer ticks at this rate, so a game may move by
    /// a step of it at each tick. A page that ticks at each refresh of the
    /// screen gives a nominal rate, and a game that keeps time measures it
    /// with its own clock. On a display of this process it is
    /// [`Display::frame_rate`], which changes with the monitor, so a game
    /// reads it again at each frame.
    pub fn frame_rate(&self) -> FrameRate {
        match &self.inner {
            Inner::Local(display) => display.frame_rate(),
            #[cfg(feature = "window")]
            Inner::Players(players) => players.window.frame_rate(),
            Inner::Remote(remote) => remote.session.frame_rate(),
        }
    }

    /// End the stage, as its drop does. A session closes fd 3 and fd 4,
    /// which ends the room for the server.
    pub fn close(self) {
        drop(self);
    }
}

/// A window for one player, with `title`.
#[cfg(feature = "window")]
fn open_window(title: &str) -> Result<Box<dyn Display>, StageError> {
    let window = super::Window::open_seats(&[title.to_owned()]).map_err(StageError::Window)?;
    Ok(Box::new(window))
}

#[cfg(not(feature = "window"))]
fn open_window(_title: &str) -> Result<Box<dyn Display>, StageError> {
    Err(StageError::NoDisplay)
}

#[cfg(feature = "terminal")]
fn open_terminal(
    options: TerminalOptions,
    graphics: Option<TerminalGraphics>,
) -> Result<Box<dyn Display>, StageError> {
    let terminal = super::Terminal::open(options, graphics).map_err(StageError::Terminal)?;
    Ok(Box::new(terminal))
}

#[cfg(not(feature = "terminal"))]
fn open_terminal(
    _options: TerminalOptions,
    _graphics: Option<TerminalGraphics>,
) -> Result<Box<dyn Display>, StageError> {
    Err(StageError::NoDisplay)
}

#[cfg(any(feature = "terminal", feature = "window"))]
fn open_headless() -> Result<Box<dyn Display>, StageError> {
    Ok(Box::new(super::headless::Headless::new()))
}

#[cfg(not(any(feature = "terminal", feature = "window")))]
fn open_headless() -> Result<Box<dyn Display>, StageError> {
    Err(StageError::NoDisplay)
}

#[cfg(feature = "window")]
fn has_window_system() -> bool {
    super::window::has_window_system()
}

/// A build without the window opens none, so [`StageMode::auto`] picks
/// the terminal.
#[cfg(not(feature = "window"))]
fn has_window_system() -> bool {
    false
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
        WindowError::NoWindowSystem => StageError::NoWindowPlayers,
        e => StageError::Window(e),
    })?;
    let stage = Stage {
        inner: Inner::Players(Box::new(Players { window })),
        players,
    };
    Ok((stage, all))
}

#[cfg(not(feature = "window"))]
fn open_players(
    _title: &str,
    _players: NonZeroU32,
) -> Result<(Stage, Vec<(Player, String)>), StageError> {
    Err(StageError::NoDisplay)
}

impl fmt::Display for StageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StageError::Window(e) => e.fmt(f),
            StageError::Terminal(e) => e.fmt(f),
            StageError::Busy => f.write_str("another stage of this process is open"),
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
            StageError::TerminalPlayers { range } => write!(
                f,
                "the game takes at least {} players, and the terminal shows one, so \
                 they need a window each",
                range.min()
            ),
            StageError::NoWindowPlayers => f.write_str(
                "several players need a window each, and this session has no graphical display",
            ),
            StageError::NoSession => {
                f.write_str("no server ran this engine, since fd 3 or fd 4 is not open")
            }
            StageError::NotSessionFd { fd } => {
                write!(
                    f,
                    "fd {fd} is not a pipe or a socket, so no server opened it"
                )
            }
            StageError::NoDisplay => f.write_str("this build has no display for the mode"),
        }
    }
}

impl std::error::Error for StageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StageError::Window(e) => Some(e),
            StageError::Terminal(e) => Some(e),
            StageError::Start(e) => Some(e),
            StageError::Busy
            | StageError::Players { .. }
            | StageError::TerminalPlayers { .. }
            | StageError::NoWindowPlayers
            | StageError::NoSession
            | StageError::NotSessionFd { .. }
            | StageError::NoDisplay => None,
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
}

#[cfg(feature = "window")]
impl Players {
    /// Keep `scene` for the seats of `to` until the next wait. A draw
    /// that failed since the last present fails this one, which still
    /// keeps `scene`.
    fn present(&mut self, to: Target, scene: Scene) -> Result<(), StagePresentError> {
        self.window.present_to(to, scene);
        self.window
            .take_failed()
            .map_err(StagePresentError::Display)
    }

    /// Draw the scenes that wait, and block until the next event.
    fn wait(&mut self, deadline: Option<Instant>) -> io::Result<StageEvent> {
        use super::inbox::Seated;
        loop {
            return Ok(match self.window.wait_seated(deadline)? {
                Seated::Frame => StageEvent::Frame,
                Seated::Input(seat, event) => StageEvent::Input {
                    player: Player(NonZeroU32::MIN.saturating_add(seat)),
                    event,
                },
                // No Sender of the window leaves the stage.
                Seated::Wake => continue,
                Seated::Timeout => StageEvent::Timeout,
                Seated::Close => StageEvent::Close,
            });
        }
    }
}

/// A session with a server, on fd 3 and fd 4.
struct Remote {
    session: Session<DeadlineReader, BufWriter<File>>,
    /// The deadline of the reader of the session, which has no other way
    /// to it.
    deadline: Rc<Deadline>,
    /// The stream of the server ended or failed, and every wait returns
    /// Close.
    ended: bool,
    /// The last field, so fd 3 and fd 4 close before the next stage can
    /// take them.
    _taken: session_fds::Taken,
}

impl Remote {
    fn wait(&mut self, deadline: Option<Instant>) -> io::Result<StageEvent> {
        if self.ended {
            return Ok(StageEvent::Close);
        }
        self.deadline.at.set(deadline);
        let event = self.session.wait();
        self.deadline.at.set(None);
        let passed = self.deadline.passed.take();
        match event {
            Ok(SessionEvent::Frame) => Ok(StageEvent::Frame),
            Ok(SessionEvent::Input { player, event }) => Ok(StageEvent::Input { player, event }),
            Ok(SessionEvent::Error(e)) => Ok(StageEvent::Error(e)),
            Ok(SessionEvent::End) => {
                self.ended = true;
                Ok(StageEvent::Close)
            }
            // The session keeps its bytes over an error of the reader, so
            // the next wait goes on from there.
            Err(_) if passed => Ok(StageEvent::Timeout),
            // A read of a file or a write that fails does not come back.
            Err(e) => {
                self.ended = true;
                Err(e)
            }
        }
    }
}

/// The deadline of a wait of a session, and whether the reader saw it pass.
/// A `TimedOut` from elsewhere, such as from fd 4, is a failure.
#[derive(Default)]
struct Deadline {
    at: Cell<Option<Instant>>,
    passed: Cell<bool>,
}

/// Fd 3, which a read waits on until a deadline. The read fails with
/// [`io::ErrorKind::TimedOut`] and marks the deadline passed when the
/// deadline passes first.
struct DeadlineReader {
    file: File,
    deadline: Rc<Deadline>,
}

impl Read for DeadlineReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(at) = self.deadline.at.get()
            && !wait_readable(&self.file, at)?
        {
            self.deadline.passed.set(true);
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.file.read(buf)
    }
}

/// Wait until `file` has bytes, its end or an error to read. Returns
/// `true` if it has, `false` if `deadline` passes first.
#[cfg(any(unix, target_os = "wasi"))]
fn wait_readable(file: &File, deadline: Instant) -> io::Result<bool> {
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
            0 if Instant::now() >= deadline => return Ok(false),
            0 => {}
            n if n > 0 => return Ok(true),
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
fn wait_readable(_file: &File, _deadline: Instant) -> io::Result<bool> {
    Ok(true)
}

/// Fd 3 and fd 4, in a module of their own, so that only [`session_fds::take`]
/// builds a [`session_fds::Taken`].
mod session_fds {
    use std::fs::File;

    use super::StageError;

    /// Set while a stage owns fd 3 and fd 4, since a second owner of the
    /// same descriptors would close them twice.
    #[cfg(any(unix, target_os = "wasi"))]
    static TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// The hold of a stage on fd 3 and fd 4, which its drop lets go. Only
    /// [`take`] builds one, after it sets [`TAKEN`].
    pub(super) struct Taken(());

    impl Taken {
        /// A hold on no descriptors, for a test that runs a session on pipes.
        /// The drop of a hold does nothing on unix.
        #[cfg(all(test, unix))]
        pub(super) fn for_test() -> Self {
            Taken(())
        }
    }

    /// Let the next stage take fd 3 and fd 4. Only a wasi host keeps the
    /// descriptors open after a close, for the next session of the instance.
    /// Elsewhere a closed descriptor may come back as a file of another
    /// owner, so the session of a process stays its only one.
    #[cfg(any(unix, target_os = "wasi"))]
    impl Drop for Taken {
        fn drop(&mut self) {
            if cfg!(target_os = "wasi") {
                TAKEN.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    /// Fd 3 and fd 4, which the server opened for the session, and the hold
    /// on them.
    #[cfg(any(unix, target_os = "wasi"))]
    pub(super) fn take() -> Result<(File, File, Taken), StageError> {
        use std::os::fd::FromRawFd;
        use std::sync::atomic::Ordering;

        use crate::session::{ENGINE_TO_SERVER_FD, SERVER_TO_ENGINE_FD};

        check_session_fd(SERVER_TO_ENGINE_FD)?;
        check_session_fd(ENGINE_TO_SERVER_FD)?;
        if TAKEN.swap(true, Ordering::SeqCst) {
            return Err(StageError::Busy);
        }
        // SAFETY: both are open, StageMode::Session says that a server opened
        // them, and on unix both are a pipe or a socket, which a server gives
        // and a shell does not. TAKEN gives them to one stage.
        Ok(unsafe {
            (
                File::from_raw_fd(SERVER_TO_ENGINE_FD),
                File::from_raw_fd(ENGINE_TO_SERVER_FD),
                Taken(()),
            )
        })
    }

    /// Fails unless `fd` is a pipe or a socket, which only a server gives a
    /// process. A closed `fd` gives [`StageError::NoSession`].
    #[cfg(unix)]
    pub(super) fn check_session_fd(fd: i32) -> Result<(), StageError> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fstat writes one stat, and fails on a descriptor that is not
        // open.
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            return Err(StageError::NoSession);
        }
        // SAFETY: fstat succeeded, so it wrote the stat.
        let kind = unsafe { stat.assume_init() }.st_mode & libc::S_IFMT;
        if kind == libc::S_IFIFO || kind == libc::S_IFSOCK {
            Ok(())
        } else {
            Err(StageError::NotSessionFd { fd })
        }
    }

    /// Fails unless `fd` is open. The hosts of wasi have no fstat, so the type
    /// of `fd` goes unchecked.
    #[cfg(target_os = "wasi")]
    pub(super) fn check_session_fd(fd: i32) -> Result<(), StageError> {
        // SAFETY: F_GETFD only reads the flags of a descriptor, and fails on
        // one that is not open.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
            return Err(StageError::NoSession);
        }
        Ok(())
    }

    #[cfg(not(any(unix, target_os = "wasi")))]
    pub(super) fn take() -> Result<(File, File, Taken), StageError> {
        Err(StageError::NoSession)
    }
}

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

    #[test]
    fn a_pipe_or_a_socket_can_carry_a_session() {
        let (read, write) = pipe();
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        for fd in [
            read.as_raw_fd(),
            write.as_raw_fd(),
            a.as_raw_fd(),
            b.as_raw_fd(),
        ] {
            assert!(session_fds::check_session_fd(fd).is_ok());
        }
    }

    #[test]
    fn a_file_cannot_carry_a_session() {
        let file = File::open(file!()).unwrap();
        let fd = file.as_raw_fd();
        let Err(StageError::NotSessionFd { fd: got }) = session_fds::check_session_fd(fd) else {
            panic!("a file carries a session");
        };
        assert_eq!(got, fd);
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
        let deadline = Rc::new(Deadline::default());
        let reader = DeadlineReader {
            file: from_server,
            deadline: Rc::clone(&deadline),
        };
        let players = PlayerRange::new(1, 1).unwrap();
        let (session, _) = Session::start(players, reader, BufWriter::new(to_server)).unwrap();
        let count = session.player_count();
        let remote = Remote {
            session,
            deadline,
            ended: false,
            _taken: session_fds::Taken::for_test(),
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
            Ok(StageEvent::Timeout)
        ));
        assert!(Instant::now() >= deadline);
        server.tick();
        assert!(matches!(stage.wait(Some(deadline)), Ok(StageEvent::Frame)));
        assert!(matches!(server.read()[..], [testing::Message::TickTaken]));
    }

    #[test]
    fn a_frame_to_a_player_the_stage_lacks_fails_and_writes_nothing() {
        let (mut stage, mut server) = session();
        let other = Player(NonZeroU32::new(2).unwrap());
        assert!(matches!(
            stage.present(Target::Player(other), Scene::new(10.0, 10.0)),
            Err(StagePresentError::NoPlayer(p)) if p == other
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
        assert!(matches!(stage.wait(None), Ok(StageEvent::Frame)));
        writer.join().unwrap();
        assert!(matches!(stage.wait(None), Ok(StageEvent::Close)));
    }

    #[cfg(all(feature = "terminal", feature = "window"))]
    #[test]
    fn a_stage_with_native_none_ticks_for_its_players() {
        let range = PlayerRange::new(1, 4).expect("1 to 4 is a range");
        let mode = StageMode::Headless {
            players: NonZeroU32::new(2),
        };
        let (mut stage, players) = Stage::open("test", range, mode).unwrap();
        let numbers: Vec<_> = players.iter().map(|(p, _)| p.number().get()).collect();
        assert_eq!(numbers, [1, 2]);
        assert!(matches!(stage.wait(None), Ok(StageEvent::Frame)));
        let ana = Player(NonZeroU32::MIN);
        assert!(
            stage
                .present(Target::Player(ana), Scene::new(10.0, 10.0))
                .is_ok()
        );
        stage.close();
    }

    #[cfg(all(feature = "terminal", feature = "window"))]
    fn open_with(players: u32, mode: StageMode) -> Result<Stage, StageError> {
        let range = PlayerRange::new(1, players).expect("a range");
        Stage::open("test", range, mode).map(|(stage, _)| stage)
    }

    #[cfg(all(feature = "terminal", feature = "window"))]
    #[test]
    fn local_players_outside_the_range_of_the_game_fail() {
        for (max, local) in [(1, 2), (16, 9)] {
            let mode = StageMode::Headless {
                players: NonZeroU32::new(local),
            };
            let result = open_with(max, mode);
            assert!(
                matches!(result, Err(StageError::Players { players, .. }) if players.get() == local)
            );
        }
    }

    #[cfg(all(feature = "terminal", feature = "window"))]
    #[test]
    fn no_local_players_means_the_fewest_of_the_game() {
        let range = PlayerRange::new(2, 4).expect("2 to 4 is a range");
        let mode = StageMode::Terminal {
            options: TerminalOptions::default(),
            graphics: None,
        };
        let Err(e) = Stage::open("test", range, mode) else {
            panic!("a terminal opened for two players");
        };
        assert!(matches!(e, StageError::TerminalPlayers { range: r } if r == range));
        assert_eq!(
            e.to_string(),
            "the game takes at least 2 players, and the terminal shows one, so they need a \
             window each"
        );
    }

    #[cfg(not(feature = "window"))]
    #[test]
    fn a_build_without_the_window_opens_no_window() {
        let range = PlayerRange::new(1, 1).expect("1 to 1 is a range");
        let mode = StageMode::Window { players: None };
        assert!(matches!(
            Stage::open("test", range, mode),
            Err(StageError::NoDisplay)
        ));
    }

    #[test]
    fn auto_gives_one_player_a_window_when_there_is_a_window_system() {
        let options = TerminalOptions::default();
        assert!(matches!(
            StageMode::choose(None, options, || true),
            StageMode::Window { players: None }
        ));
        assert!(matches!(
            StageMode::choose(NonZeroU32::new(1), options, || false),
            StageMode::Terminal { graphics: None, .. }
        ));
    }

    #[test]
    fn auto_gives_several_players_windows_without_a_check() {
        let two = NonZeroU32::new(2);
        let mode = StageMode::choose(two, TerminalOptions::default(), || panic!("checked"));
        assert!(matches!(mode, StageMode::Window { players } if players == two));
    }
}
