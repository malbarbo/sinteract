//! [`Frames`], the stage of a world of a teaching language, as spython and
//! sgleam run it. The world keeps its own clock, shows a scene when its
//! state changes, and reads only the keys, the pads and the close.

use std::collections::{HashMap, VecDeque};
use std::thread;
use std::time::Instant;

use super::{PresentError, Stage, StageError, StageEvent, StageMode, StagePresentError};
use crate::event::{InputEvent, KeyEvent, KeyKind, PadEvent};
use crate::scene::Scene;
use crate::session::FrameError;
use crate::session::{Player, PlayerRange, Target};

/// The frames of the worlds of a program, one world after another. A world
/// of one player lets the first [`Frames::show`] open the stage. A world of
/// several players opens the stage and takes its players with
/// [`Frames::open`], and shows each one its scene with [`Frames::show_to`].
/// A window takes the size of the scene of its first show.
/// [`Frames::close`] ends the stage.
///
/// A show keeps the scene and draws nothing. The stage draws the scene of
/// the last show of each player at its next tick, so a world that changes faster than the
/// screen draws only what the screen can show, and a session sends one
/// frame per tick of the server.
pub struct Frames {
    title: String,
    mode: Box<dyn FnMut() -> StageMode>,
    state: State,
    /// The lines of [`Frames::print_line`] while a stage keeps rows for
    /// them, at most as many as the rows.
    footer: Vec<String>,
}

/// What [`Frames::wait_event`] returns.
#[derive(Debug, PartialEq)]
pub enum FrameEvent {
    Key(Player, KeyEvent),
    Pad(Player, PadEvent),
    /// The user closed the display, or the server ended the session. Every
    /// wait after it returns Close too, until the next world.
    Close,
    /// The stage ticked, and the scenes of the shows before it went out. A
    /// scene that the world shows now goes out at the next tick.
    Tick,
    /// The deadline passed first.
    Timeout,
}

/// Why [`Frames::show`] showed nothing.
#[derive(Debug)]
pub enum ShowError {
    /// The stage of the world did not open, and the world goes on without
    /// one.
    Open(StageError),
    /// The stage did not open for an earlier show of the same world.
    Unavailable,
}

enum State {
    /// No world showed a scene yet, or the last world closed.
    Closed,
    Open(Open),
    /// The stage did not open, so the world does not try again at each
    /// show.
    Unavailable,
}

struct Open {
    stage: Stage,
    players: Vec<Player>,
    /// The scene of the last show of each target, until a tick draws it.
    pending: Vec<(Target, Scene)>,
    /// The events that a wait took and did not return yet.
    events: VecDeque<FrameEvent>,
    /// The players whose last key was a down.
    after_down: HashMap<Player, bool>,
    /// The stage closed or failed.
    closed: bool,
}

/// A world has one player, the user or player 1 of the server.
const PLAYERS: PlayerRange = PlayerRange::new(1, 1).expect("1 to 1 is a range");

impl Frames {
    /// The frames of a program, with `title` on the window, and the
    /// mode of each stage from `mode`, with the local players. The
    /// call comes when a world opens its stage, so a `mode` that calls
    /// [`StageMode::auto`] checks for a window only in a program that shows
    /// one.
    pub fn new(title: impl Into<String>, mode: impl FnMut() -> StageMode + 'static) -> Self {
        Frames {
            title: title.into(),
            mode: Box::new(mode),
            state: State::Closed,
            footer: Vec::new(),
        }
    }

    /// Returns `true` if a world has a stage open, `false` otherwise.
    pub fn is_open(&self) -> bool {
        matches!(self.state, State::Open(_))
    }

    /// The players of the world, from the start of its stage. With no stage
    /// open, there are none.
    pub fn players(&self) -> &[Player] {
        match &self.state {
            State::Open(open) => &open.players,
            State::Closed | State::Unavailable => &[],
        }
    }

    /// Open the stage of a world that takes `players`, with the players of
    /// the display of this process, and return each player
    /// with its nickname. When the stage does not open, the
    /// world goes on without one, and each show returns
    /// [`ShowError::Unavailable`]. While the stage of the last world is
    /// open, it fails with [`StageError::Busy`] and leaves that stage as
    /// it is.
    pub fn open(&mut self, players: PlayerRange) -> Result<Vec<(Player, String)>, StageError> {
        if self.is_open() {
            return Err(StageError::Busy);
        }
        match Stage::open(&self.title, players, (self.mode)()) {
            Ok((stage, players)) => {
                self.state = State::Open(Open {
                    stage,
                    players: players.iter().map(|(player, _)| *player).collect(),
                    pending: Vec::new(),
                    events: VecDeque::new(),
                    after_down: HashMap::new(),
                    closed: false,
                });
                Ok(players)
            }
            Err(e) => {
                self.state = State::Unavailable;
                Err(e)
            }
        }
    }

    /// Keep `scene` for every player until the next tick, and open the
    /// stage of one player first if the world has none. Options with more
    /// than one player fail with [`StageError::Players`].
    pub fn show(&mut self, scene: Scene) -> Result<(), ShowError> {
        if let State::Closed = self.state {
            self.open(PLAYERS).map_err(ShowError::Open)?;
        }
        self.keep(Target::All, scene)
    }

    /// Keep `scene` for `player` until the next tick. The world took its
    /// players from [`Frames::open`].
    pub fn show_to(&mut self, player: Player, scene: Scene) -> Result<(), ShowError> {
        self.keep(Target::Player(player), scene)
    }

    fn keep(&mut self, target: Target, scene: Scene) -> Result<(), ShowError> {
        let State::Open(open) = &mut self.state else {
            return Err(ShowError::Unavailable);
        };
        open.pending.retain(|(t, _)| *t != target);
        open.pending.push((target, scene));
        Ok(())
    }

    /// Wait until a key, a pad, a tick, the close or `deadline`. The scene
    /// of the last show goes out at the tick. The mouse and a resize
    /// never come out. A terminal that sends a press with no down before
    /// it gives a down and then the press, since a world reads a down as
    /// the start of a key. With no stage open, it sleeps until
    /// `deadline` and returns [`FrameEvent::Timeout`].
    ///
    /// A message that the session drops goes to `on_error`, and the wait
    /// goes on.
    pub fn wait_event(
        &mut self,
        deadline: Instant,
        mut on_error: impl FnMut(&dyn std::error::Error),
    ) -> FrameEvent {
        let State::Open(open) = &mut self.state else {
            thread::sleep(deadline.saturating_duration_since(Instant::now()));
            return FrameEvent::Timeout;
        };
        loop {
            if let Some(event) = open.events.pop_front() {
                return event;
            }
            if open.closed {
                return FrameEvent::Close;
            }
            match open.stage.wait(Some(deadline)) {
                Ok(StageEvent::Tick) => {
                    open.present(&mut on_error);
                    return FrameEvent::Tick;
                }
                Ok(StageEvent::Input {
                    player,
                    event: InputEvent::Key(key),
                }) => open.take_key(player, key),
                Ok(StageEvent::Input {
                    player,
                    event: InputEvent::Pad(pad),
                }) => open.events.push_back(FrameEvent::Pad(player, pad)),
                Ok(StageEvent::Input { .. }) => {}
                Ok(StageEvent::Error(e)) => on_error(&e),
                Ok(StageEvent::Timeout) => return FrameEvent::Timeout,
                Ok(StageEvent::Close) => open.closed = true,
                Err(e) => {
                    on_error(&e);
                    open.closed = true;
                }
            }
        }
    }

    /// Show `line` under the frame. Returns `true` if the stage keeps rows
    /// for it, `false` if the caller prints it.
    pub fn print_line(&mut self, line: &str) -> bool {
        let State::Open(open) = &mut self.state else {
            return false;
        };
        let rows = usize::from(open.stage.footer_rows());
        if rows == 0 {
            return false;
        }
        self.footer.extend(line.split('\n').map(String::from));
        let excess = self.footer.len().saturating_sub(rows);
        self.footer.drain(..excess);
        open.stage.set_footer(&self.footer).is_ok()
    }

    /// Draw the scenes of the last shows, which no tick drew yet, and end
    /// the stage of the world. Returns the lines under the frame, which
    /// the caller prints, since the frame stays on screen above them. The
    /// next show opens a new stage.
    pub fn close(&mut self) -> Vec<String> {
        if let State::Open(mut open) = std::mem::replace(&mut self.state, State::Closed) {
            // The world is over, and nothing reads the error.
            open.present(&mut |_| {});
            open.stage.close();
        }
        std::mem::take(&mut self.footer)
    }
}

impl Open {
    fn present(&mut self, on_error: &mut impl FnMut(&dyn std::error::Error)) {
        for (target, scene) in std::mem::take(&mut self.pending) {
            if let Err(e) = self.stage.present(target, scene) {
                on_error(&e);
                if let StagePresentError::Display(PresentError::Io(_))
                | StagePresentError::Session(FrameError::Io(_)) = e
                {
                    self.closed = true;
                    break;
                }
            }
        }
    }

    fn take_key(&mut self, player: Player, key: KeyEvent) {
        let after_down = self.after_down.entry(player).or_default();
        if key.kind == KeyKind::Press && !*after_down {
            let down = KeyEvent {
                kind: KeyKind::Down,
                ..key.clone()
            };
            self.events.push_back(FrameEvent::Key(player, down));
        }
        *after_down = key.kind == KeyKind::Down;
        self.events.push_back(FrameEvent::Key(player, key));
    }
}

impl std::fmt::Display for ShowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShowError::Open(e) => e.fmt(f),
            ShowError::Unavailable => f.write_str("the display did not open"),
        }
    }
}

impl std::error::Error for ShowError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ShowError::Open(e) => Some(e),
            ShowError::Unavailable => None,
        }
    }
}

impl std::fmt::Debug for Frames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frames")
            .field("title", &self.title)
            .field("open", &self.is_open())
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::num::NonZeroU32;
    use std::time::Duration;

    use super::*;
    use crate::display::stage::tests::{Server, session};
    use crate::event::{Modifiers, PadButton};
    use crate::wire::testing::Message;

    /// A display that never opens a screen, so no test opens the tty or
    /// connects to a window system.
    fn headless() -> StageMode {
        StageMode::Headless { players: None }
    }

    /// Frames with the stage of a session open, as after a first show.
    fn frames() -> (Frames, Server) {
        let (stage, server) = session();
        let mut frames = Frames::new("test", headless);
        frames.state = State::Open(Open {
            stage,
            players: vec![Player(NonZeroU32::MIN)],
            pending: Vec::new(),
            events: VecDeque::new(),
            after_down: HashMap::new(),
            closed: false,
        });
        (frames, server)
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_millis(20)
    }

    fn wait(frames: &mut Frames) -> FrameEvent {
        frames.wait_event(soon(), |e| panic!("{e}"))
    }

    fn key(kind: KeyKind, name: &str) -> KeyEvent {
        KeyEvent {
            kind,
            key: name.into(),
            modifiers: Modifiers::default(),
        }
    }

    fn widths(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .map(|m| match m {
                Message::Frame { scene, .. } => format!("frame {}", scene.width()),
                Message::TickTaken => "taken".into(),
                Message::Asset { id, .. } => format!("asset {id}"),
            })
            .collect()
    }

    #[test]
    fn a_show_goes_out_at_the_next_tick_and_only_the_last_one() {
        let (mut frames, mut server) = frames();
        frames.show(Scene::new(10.0, 10.0)).unwrap();
        frames.show(Scene::new(20.0, 10.0)).unwrap();
        assert_eq!(wait(&mut frames), FrameEvent::Timeout);
        assert!(server.read().is_empty());
        server.tick();
        assert_eq!(wait(&mut frames), FrameEvent::Tick);
        assert_eq!(widths(&server.read()), ["taken", "frame 20"]);
        server.tick();
        assert_eq!(wait(&mut frames), FrameEvent::Tick);
        assert_eq!(widths(&server.read()), ["taken"]);
    }

    #[test]
    fn a_show_to_a_player_goes_out_with_the_player() {
        let (mut frames, mut server) = frames();
        let ana = Player(NonZeroU32::MIN);
        frames.show_to(ana, Scene::new(10.0, 10.0)).unwrap();
        frames.show_to(ana, Scene::new(20.0, 10.0)).unwrap();
        server.tick();
        assert_eq!(wait(&mut frames), FrameEvent::Tick);
        let players: Vec<_> = server
            .read()
            .into_iter()
            .filter_map(|m| match m {
                Message::Frame { player, scene } => Some((player, scene.width())),
                _ => None,
            })
            .collect();
        assert_eq!(players, [(Some(NonZeroU32::MIN), 20.0)]);
    }

    #[cfg(all(feature = "terminal", feature = "window"))]
    #[test]
    fn a_show_opens_one_player() {
        let mut frames = Frames::new("test", headless);
        frames.show(Scene::new(10.0, 10.0)).unwrap();
        assert_eq!(frames.players(), [Player(NonZeroU32::MIN)]);
        assert_eq!(wait(&mut frames), FrameEvent::Tick);
    }

    #[cfg(all(feature = "terminal", feature = "window"))]
    #[test]
    fn a_show_with_more_players_in_the_options_fails() {
        let mut frames = Frames::new("test", || StageMode::Headless {
            players: NonZeroU32::new(2),
        });
        assert!(matches!(
            frames.show(Scene::new(10.0, 10.0)),
            Err(ShowError::Open(StageError::Players { .. }))
        ));
    }

    #[cfg(all(feature = "terminal", feature = "window"))]
    #[test]
    fn an_open_that_fails_leaves_the_world_with_no_stage() {
        let mut frames = Frames::new("test", || StageMode::Terminal {
            options: crate::display::TerminalOptions::default(),
            graphics: None,
        });
        let range = PlayerRange::new(2, 4).expect("2 to 4 is a range");
        assert!(matches!(
            frames.open(range),
            Err(StageError::TerminalPlayers { .. })
        ));
        assert!(!frames.is_open());
        assert!(frames.players().is_empty());
        let ana = Player(NonZeroU32::MIN);
        assert!(matches!(
            frames.show_to(ana, Scene::new(10.0, 10.0)),
            Err(ShowError::Unavailable)
        ));
    }

    #[test]
    fn a_second_open_is_busy_and_keeps_the_stage() {
        let (mut frames, mut server) = frames();
        assert!(matches!(frames.open(PLAYERS), Err(StageError::Busy)));
        assert!(frames.is_open());
        server.tick();
        assert_eq!(wait(&mut frames), FrameEvent::Tick);
    }

    #[test]
    fn a_show_to_with_no_stage_is_unavailable() {
        let mut frames = Frames::new("test", headless);
        assert!(frames.players().is_empty());
        let ana = Player(NonZeroU32::MIN);
        assert!(matches!(
            frames.show_to(ana, Scene::new(10.0, 10.0)),
            Err(ShowError::Unavailable)
        ));
    }

    #[test]
    fn a_press_with_no_down_before_it_gives_a_down_first() {
        let (mut frames, mut server) = frames();
        server.key(key(KeyKind::Press, "a"));
        server.key(key(KeyKind::Down, "b"));
        server.key(key(KeyKind::Press, "b"));
        server.key(key(KeyKind::Up, "b"));
        let kinds: Vec<_> = (0..5)
            .map(|_| match wait(&mut frames) {
                FrameEvent::Key(_, k) => format!("{:?} {}", k.kind, k.key),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(kinds, ["Down a", "Press a", "Down b", "Press b", "Up b"]);
    }

    #[test]
    fn a_pad_comes_out_with_its_player() {
        let (mut frames, mut server) = frames();
        server.pad(PadEvent::Down(PadButton::A));
        assert_eq!(
            wait(&mut frames),
            FrameEvent::Pad(Player(NonZeroU32::MIN), PadEvent::Down(PadButton::A))
        );
    }

    #[test]
    fn the_end_of_the_session_closes_and_close_draws_the_last_show() {
        let (mut frames, mut server) = frames();
        frames.show(Scene::new(30.0, 10.0)).unwrap();
        server.end();
        assert_eq!(wait(&mut frames), FrameEvent::Close);
        assert_eq!(wait(&mut frames), FrameEvent::Close);
        assert!(frames.close().is_empty());
        assert!(!frames.is_open());
        assert!(frames.players().is_empty());
        assert_eq!(widths(&server.read()), ["frame 30"]);
    }

    #[test]
    fn a_wait_with_no_stage_sleeps_until_the_deadline() {
        let mut frames = Frames::new("test", headless);
        let deadline = soon();
        assert_eq!(
            frames.wait_event(deadline, |e| panic!("{e}")),
            FrameEvent::Timeout
        );
        assert!(Instant::now() >= deadline);
    }
}
