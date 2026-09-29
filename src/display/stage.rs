//! [`Stage`], where an engine runs its game, on a display of this process
//! or in a session with a server, with the same loop.

use std::fmt;
use std::fs::File;
use std::io::BufWriter;
use std::time::Instant;

use super::{Display, OpenError, PresentError, TerminalOptions, open_native};
use crate::event::{Event, InputEvent, Interrupt};
use crate::scene::Scene;
use crate::session::PlayerRange;
use crate::session::{FrameError, Player, Session, SessionError, SessionEvent, StartError, Target};

/// The game of an engine, in a session with a server when the server runs
/// the engine, and on a window or the terminal otherwise, where the user is
/// [`Player`] 1. The loop of the game is the same in both:
///
/// ```no_run
/// # use sinteract::display::{Stage, StageEvent, TerminalOptions};
/// # use sinteract::event::Interrupt;
/// # use sinteract::scene::Scene;
/// # use sinteract::session::Target;
/// # use sinteract::session::PlayerRange;
/// # fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let players = PlayerRange::new(1, 1).expect("1 to 1 is a range");
/// let options = TerminalOptions::default();
/// let (mut stage, _players) = Stage::open("My game", 400.0, 300.0, players, options)?;
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

/// Why [`Stage::open`] did not open a stage.
#[derive(Debug)]
pub enum StageError {
    /// Neither a window nor the terminal opened.
    Open(OpenError),
    /// The session did not start.
    Start(StartError),
    /// A game on a display of this process has one player, and this game
    /// takes more.
    Players(PlayerRange),
    /// `SINTERACT_SESSION` asks for a session, and fd 3 or fd 4 is not open,
    /// or the platform has no such descriptors.
    NoSession,
}

/// The variable of the environment that the server sets when it runs the
/// engine with the session on fd 3 and fd 4.
const SESSION_VAR: &str = "SINTERACT_SESSION";

impl Stage {
    /// Open the stage of a game that takes `players`, with each player and
    /// its nickname. With `SINTERACT_SESSION` in the environment, it starts
    /// a session that reads the server from fd 3 and writes to it on fd 4,
    /// and blocks until the start. Otherwise it opens a window of `width`
    /// by `height` logical pixels, or the terminal, as [`open_native`]
    /// does with `title` and `options`, with player 1 and an empty
    /// nickname. A second stage in a session fails with
    /// [`OpenError::Busy`], since the first one owns fd 3 and fd 4.
    pub fn open(
        title: &str,
        width: f32,
        height: f32,
        players: PlayerRange,
        options: TerminalOptions,
    ) -> Result<(Stage, Vec<(Player, String)>), StageError> {
        if std::env::var_os(SESSION_VAR).is_some() {
            let (from_server, to_server) = take_session_fds()?;
            let (session, players) =
                Session::start(players, from_server, BufWriter::new(to_server))
                    .map_err(StageError::Start)?;
            let remote = Remote {
                session: Some(session),
                ended: false,
            };
            return Ok((
                Stage {
                    inner: Inner::Remote(remote),
                },
                players,
            ));
        }
        if players.min().get() > 1 {
            return Err(StageError::Players(players));
        }
        let display = open_native(title, width, height, options).map_err(StageError::Open)?;
        let stage = Stage {
            inner: Inner::Local(display),
        };
        Ok((stage, vec![(Player::LOCAL, String::new())]))
    }

    /// Block until the next event, as [`Display::wait_event`] does. In a
    /// session the tick of the server bounds the wait, so the session does
    /// not look at `deadline`, and never returns [`Interrupt::Timeout`] or
    /// [`Interrupt::Wake`]. The end of the session is
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
            Inner::Remote(remote) => remote.wait(),
        }
    }

    /// Show `scene` to `to`. A display of this process shows every frame,
    /// since its one player is player 1. A session writes the frame, with
    /// each image that the server does not have, and still writes after
    /// the end of the stream of the server, so the views get the last
    /// frames.
    pub fn present(&mut self, to: Target, scene: Scene) -> Result<(), PresentError> {
        match &mut self.inner {
            Inner::Local(display) => display.present(scene),
            Inner::Remote(remote) => {
                let session = remote.session.as_mut().ok_or(PresentError::Closed)?;
                session.write_frame(to, &scene).map_err(|e| match e {
                    FrameError::Full(e) => PresentError::Full(e),
                    FrameError::Io(e) => PresentError::Io(e),
                })
            }
        }
    }

    /// End the stage. A session closes fd 4, which ends the room for the
    /// server. A second call does nothing, and drop calls it.
    pub fn close(&mut self) {
        match &mut self.inner {
            Inner::Local(display) => display.close(),
            Inner::Remote(remote) => remote.session = None,
        }
    }
}

impl fmt::Display for StageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StageError::Open(e) => e.fmt(f),
            StageError::Start(e) => e.fmt(f),
            StageError::Players(range) => write!(
                f,
                "the game takes at least {} players, and only one plays here",
                range.min()
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
            StageError::Players(_) | StageError::NoSession => None,
        }
    }
}

impl fmt::Debug for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.inner {
            Inner::Local(_) => "Local",
            Inner::Remote(_) => "Remote",
        };
        f.debug_tuple("Stage").field(&kind).finish()
    }
}

enum Inner {
    Local(Box<dyn Display>),
    Remote(Remote),
}

/// A session with a server, on fd 3 and fd 4.
struct Remote {
    /// `None` after [`Stage::close`].
    session: Option<Session<File, BufWriter<File>>>,
    /// The stream of the server ended or failed, and every wait returns
    /// Close.
    ended: bool,
}

impl Remote {
    fn wait(&mut self) -> Result<StageEvent, Interrupt> {
        let Some(session) = self.session.as_mut().filter(|_| !self.ended) else {
            return Err(Interrupt::Close);
        };
        match session.wait() {
            Ok(SessionEvent::Tick) => Ok(StageEvent::Tick),
            Ok(SessionEvent::Input { player, event }) => Ok(StageEvent::Input { player, event }),
            Ok(SessionEvent::Error(e)) => Ok(StageEvent::Error(e)),
            Ok(SessionEvent::End(None)) => {
                self.ended = true;
                Err(Interrupt::Close)
            }
            // A read of a file that fails does not come back.
            Ok(SessionEvent::End(Some(e))) | Err(e) => {
                self.ended = true;
                Err(Interrupt::Read(e))
            }
        }
    }
}

/// Fd 3 and fd 4, which the server opened for the session.
#[cfg(unix)]
fn take_session_fds() -> Result<(File, File), StageError> {
    use std::os::fd::FromRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Set once a stage takes fd 3 and fd 4, since a second owner of the
    /// same descriptors would close them twice.
    static TAKEN: AtomicBool = AtomicBool::new(false);

    // SAFETY: F_GETFD only reads the flags of a descriptor, and fails on
    // one that is not open.
    let open = |fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1;
    if !open(3) || !open(4) {
        return Err(StageError::NoSession);
    }
    if TAKEN.swap(true, Ordering::SeqCst) {
        return Err(StageError::Open(OpenError::Busy));
    }
    // SAFETY: both are open, SINTERACT_SESSION says that the server opened
    // them for the session, and TAKEN gives them to one stage.
    Ok(unsafe { (File::from_raw_fd(3), File::from_raw_fd(4)) })
}

#[cfg(not(unix))]
fn take_session_fds() -> Result<(File, File), StageError> {
    Err(StageError::NoSession)
}
