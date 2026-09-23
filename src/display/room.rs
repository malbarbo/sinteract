//! [`Room`] is the engine side of a session with players. A game server
//! runs the engine as a subprocess, writes the input of every view to its
//! stdin with the player of the view, and reads from its stdout the frames
//! for each player.
//!
//! The room reads with [`crate::wire::to_engine`] and writes with
//! [`crate::wire::to_view`], as [`super::Stdio`] does. It differs from
//! `Stdio` in three ways. Every event carries its player, a frame goes to
//! one player or to all of them, and a clock of the room makes the Vsync,
//! since each view has its own pace. The server keeps only the newest frame
//! of a player whose connection is busy.

use std::io::{self, BufRead, Write};
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use super::driver::{OpenError, PresentError};
use super::inbox::{Queued, Sender, input_supersedes};
use super::link::{ClaimedStdin, Link};
use crate::event::{InputEvent, Interrupt};
use crate::scene::Scene;
use crate::wire::framing::UNROUTED;
use crate::wire::to_engine::{self, Message, Roster};
use crate::wire::{self, ReadError, to_view};

/// The engine side of a session with players. It shows nothing, and only
/// carries the protocol.
///
/// A thread reads the input and feeds the queue, so a [`Sender`] wakes
/// [`Room::wait_event`] and the deadline holds.
pub struct Room {
    link: Link<RoomEvent>,
}

/// What [`Room::wait_event`] delivers.
#[derive(Clone, Debug)]
pub enum RoomEvent {
    /// Time to draw a frame for each player.
    Vsync,
    /// The input of `player`. The room makes the Vsync, so `event` is never
    /// [`InputEvent::Vsync`].
    Input {
        player: NonZeroU32,
        event: InputEvent,
    },
    /// `player` joined the session.
    Join {
        player: NonZeroU32,
        nickname: String,
    },
    /// `player` left the session. A frame for `player` goes nowhere from
    /// now on.
    Leave { player: NonZeroU32 },
}

impl Room {
    /// Talk over the stdin and the stdout of the process, with a Vsync every
    /// `vsync_period`. Blocks until the server starts the session, and
    /// returns the room with the players of the start. The framing is
    /// binary, so the engine must not write text to stdout.
    ///
    /// Fails with [`OpenError::Busy`] while a [`super::Stdio`] or a `Room`
    /// reads stdin, and
    /// with [`OpenError::Io`] if the stream fails or ends before the start,
    /// if a message other than the start comes first, or if the reader
    /// thread does not start.
    pub fn open(vsync_period: Duration) -> Result<(Self, Roster), OpenError> {
        Self::with_streams(ClaimedStdin::claim()?, io::stdout(), vsync_period)
            .map_err(OpenError::Io)
    }

    /// Talk over `reader` and `writer`, as a test does. Blocks until the
    /// start, as [`Room::open`] does.
    pub fn with_streams<R, W>(
        mut reader: R,
        writer: W,
        vsync_period: Duration,
    ) -> io::Result<(Self, Roster)>
    where
        R: BufRead + Send + 'static,
        W: Write + Send + 'static,
    {
        let roster = read_start(&mut reader)?;
        let link = Link::new(reader, writer, "sinteract-room", Some(vsync_period), route)?;
        Ok((Self { link }, roster))
    }

    /// Send `scene` to `player`, and flush.
    pub fn present_to(&mut self, player: NonZeroU32, scene: &Scene) -> Result<(), PresentError> {
        self.link
            .send(|w| to_view::write_frame(w, player.get(), scene))
    }

    /// Send `scene` to every player, and flush.
    pub fn present_all(&mut self, scene: &Scene) -> Result<(), PresentError> {
        self.link.send(|w| to_view::write_frame(w, UNROUTED, scene))
    }

    /// The events of the server and of the [`Sender`]s, in the order of
    /// arrival, as [`super::Display::wait_event`] delivers them.
    pub fn wait_event(&mut self, deadline: Option<Instant>) -> Result<RoomEvent, Interrupt> {
        self.link.wait(deadline)
    }

    /// A handle that pushes into this queue from any thread.
    pub fn sender(&self) -> Sender<RoomEvent> {
        self.link.sender()
    }

    /// Send the asset to every player. The server keeps it for a player
    /// who joins later.
    pub fn push_asset(
        &mut self,
        id: u32,
        blob: &[u8],
        mime: Option<&str>,
    ) -> Result<(), PresentError> {
        self.link
            .send(|w| to_view::write_asset(w, UNROUTED, id, blob, mime))
    }

    /// Tell the server that the session ended, unless the server ended it.
    /// A second call does nothing, and drop calls it.
    pub fn close(&mut self) {
        self.link.close();
    }
}

impl Queued for RoomEvent {
    fn vsync() -> Self {
        RoomEvent::Vsync
    }

    fn is_vsync(&self) -> bool {
        matches!(self, RoomEvent::Vsync)
    }

    /// A move or a resize replaces only one of the same player.
    fn supersedes(&self, old: &Self) -> bool {
        match (self, old) {
            (
                RoomEvent::Input { player, event },
                RoomEvent::Input {
                    player: old_player,
                    event: old,
                },
            ) => player == old_player && input_supersedes(event, old),
            (
                RoomEvent::Vsync
                | RoomEvent::Input { .. }
                | RoomEvent::Join { .. }
                | RoomEvent::Leave { .. },
                _,
            ) => false,
        }
    }

    /// The inputs of two players keep no order between them. A join or a
    /// leave keeps its place among the inputs, so no input moves across a
    /// change of the players.
    fn independent(&self, other: &Self) -> bool {
        match (self, other) {
            (RoomEvent::Input { player, .. }, RoomEvent::Input { player: other, .. }) => {
                player != other
            }
            (
                RoomEvent::Vsync
                | RoomEvent::Input { .. }
                | RoomEvent::Join { .. }
                | RoomEvent::Leave { .. },
                _,
            ) => false,
        }
    }
}

/// Read the first message, which has to be the start.
fn read_start(reader: &mut impl BufRead) -> io::Result<Roster> {
    let invalid = |what: &str| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the server sent {what} before the start"),
        )
    };
    match to_engine::read(reader) {
        Ok(Some(Message::Start(roster))) => Ok(roster),
        Ok(Some(Message::Input { .. })) => Err(invalid("an event")),
        Ok(Some(Message::Join { .. })) => Err(invalid("a join")),
        Ok(Some(Message::Leave { .. })) => Err(invalid("a leave")),
        Ok(Some(Message::Close)) => Err(invalid("a close")),
        Ok(None) => Err(io::ErrorKind::UnexpectedEof.into()),
        Err(ReadError::Broken(e)) => Err(e),
        Err(ReadError::Payload(e)) => Err(io::Error::new(io::ErrorKind::InvalidData, e)),
    }
}

/// The event of a message of the server. An event of player 0 is an error.
/// The room drops the Vsync of a view, since its clock makes every Vsync,
/// and a start after the first one changes nothing.
fn route(message: Message) -> Result<Option<RoomEvent>, ReadError> {
    match message {
        Message::Input { player, event } => {
            let player =
                NonZeroU32::new(player).ok_or(ReadError::Payload(wire::Error::NoPlayer))?;
            Ok(match event {
                InputEvent::Vsync => None,
                InputEvent::Key(_)
                | InputEvent::Mouse(_)
                | InputEvent::Resize { .. }
                | InputEvent::Pad(_) => Some(RoomEvent::Input { player, event }),
            })
        }
        Message::Join { player, nickname } => Ok(Some(RoomEvent::Join { player, nickname })),
        Message::Leave { player } => Ok(Some(RoomEvent::Leave { player })),
        Message::Start(_) => Ok(None),
        Message::Close => unreachable!("the link ends the session at a close"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::inbox::Inbox;
    use crate::display::link::SharedWriter;
    use crate::event::{KeyEvent, KeyKind, Modifiers, MouseAction, MouseButtons, MouseEvent};
    use crate::wire::to_engine::Member;
    use std::io::{BufReader, Cursor, PipeWriter};

    const HOUR: Duration = Duration::from_secs(3600);

    fn player(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

    fn key(name: &str) -> InputEvent {
        InputEvent::Key(KeyEvent {
            kind: KeyKind::Press,
            key: name.into(),
            modifiers: Modifiers::default(),
            repeat: false,
        })
    }

    fn at(x: f32) -> InputEvent {
        InputEvent::Mouse(MouseEvent {
            action: MouseAction::Move,
            x,
            y: 0.0,
            modifiers: Modifiers::default(),
            buttons: MouseButtons::default(),
        })
    }

    /// A start with Ana as player 1 and Beto as player 2.
    fn start() -> Vec<u8> {
        let roster = Roster::new(vec![
            Member {
                player: player(1),
                nickname: "Ana".into(),
            },
            Member {
                player: player(2),
                nickname: "Beto".into(),
            },
        ])
        .unwrap();
        let mut out = Vec::new();
        to_engine::write_start(&mut out, &roster).unwrap();
        out
    }

    /// A room over `input`, which then ends, with a clock that fires once.
    fn reading(input: Vec<u8>) -> Room {
        Room::with_streams(Cursor::new(input), Vec::<u8>::new(), HOUR)
            .unwrap()
            .0
    }

    /// A room whose input stays open while the returned writer lives.
    fn open_session() -> (Room, PipeWriter, SharedWriter) {
        let (r, mut w) = io::pipe().unwrap();
        w.write_all(&start()).unwrap();
        let written = SharedWriter::default();
        let (room, _) = Room::with_streams(BufReader::new(r), written.clone(), HOUR).unwrap();
        (room, w, written)
    }

    /// The next event of `room` that is not the Vsync of the clock.
    fn next(room: &mut Room) -> Result<RoomEvent, Interrupt> {
        loop {
            match room.wait_event(None) {
                Ok(RoomEvent::Vsync) => continue,
                other => return other,
            }
        }
    }

    #[test]
    fn open_returns_the_players_of_the_start() {
        let (_, roster) = Room::with_streams(Cursor::new(start()), Vec::<u8>::new(), HOUR).unwrap();
        let names: Vec<_> = roster
            .members()
            .iter()
            .map(|m| (m.player.get(), m.nickname.as_str()))
            .collect();
        assert_eq!(names, [(1, "Ana"), (2, "Beto")]);
    }

    #[test]
    fn open_fails_on_a_message_before_the_start() {
        let mut input = Vec::new();
        to_engine::write_input(&mut input, 1, &key("a")).unwrap();
        input.extend_from_slice(&start());
        let e = Room::with_streams(Cursor::new(input), Vec::<u8>::new(), HOUR)
            .err()
            .unwrap();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn open_fails_when_the_stream_ends_before_the_start() {
        let e = Room::with_streams(Cursor::new(Vec::new()), Vec::<u8>::new(), HOUR)
            .err()
            .unwrap();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn the_events_carry_their_player() {
        let mut input = start();
        to_engine::write_input(&mut input, 2, &key("a")).unwrap();
        to_engine::write_join(&mut input, player(3), "Caio").unwrap();
        to_engine::write_leave(&mut input, player(1)).unwrap();
        let mut room = reading(input);
        match next(&mut room) {
            Ok(RoomEvent::Input {
                player: p,
                event: InputEvent::Key(k),
            }) => assert_eq!((p, k.key.as_str()), (player(2), "a")),
            other => panic!("got {other:?}"),
        }
        match next(&mut room) {
            Ok(RoomEvent::Join {
                player: p,
                nickname,
            }) => {
                assert_eq!((p, nickname.as_str()), (player(3), "Caio"));
            }
            other => panic!("got {other:?}"),
        }
        assert!(matches!(
            next(&mut room),
            Ok(RoomEvent::Leave { player: p }) if p == player(1)
        ));
        assert!(matches!(next(&mut room), Err(Interrupt::Close)));
    }

    #[test]
    fn the_room_drops_the_vsync_of_a_view() {
        let vsync = Message::Input {
            player: 1,
            event: InputEvent::Vsync,
        };
        assert!(matches!(route(vsync), Ok(None)));
    }

    #[test]
    fn an_event_of_player_0_is_a_read_error_and_the_session_goes_on() {
        let mut input = start();
        to_engine::write_input(&mut input, UNROUTED, &InputEvent::Vsync).unwrap();
        to_engine::write_input(&mut input, 1, &key("b")).unwrap();
        let mut room = reading(input);
        assert!(matches!(
            next(&mut room),
            Err(Interrupt::Read(ReadError::Payload(wire::Error::NoPlayer)))
        ));
        assert!(matches!(
            next(&mut room),
            Ok(RoomEvent::Input { player: p, .. }) if p == player(1)
        ));
    }

    #[test]
    fn a_move_replaces_only_a_move_of_the_same_player() {
        let input = |p, event| RoomEvent::Input {
            player: player(p),
            event,
        };
        assert!(input(1, at(2.0)).supersedes(&input(1, at(1.0))));
        assert!(!input(2, at(2.0)).supersedes(&input(1, at(1.0))));
        assert!(!input(1, key("a")).supersedes(&input(1, key("a"))));
    }

    #[test]
    fn a_move_replaces_one_of_its_player_past_the_input_of_another() {
        let mut inbox: Inbox<RoomEvent> = Inbox::new(None);
        let tx = inbox.sender();
        let input = |p, event| RoomEvent::Input {
            player: player(p),
            event,
        };
        for ev in [
            input(1, at(1.0)),
            input(2, at(1.0)),
            input(1, at(2.0)),
            input(2, key("a")),
            input(1, at(3.0)),
        ] {
            tx.send_event(ev).unwrap();
        }
        let mut next = || match inbox.wait(None) {
            Ok(RoomEvent::Input { player, event }) => (player.get(), event),
            other => panic!("got {other:?}"),
        };
        assert!(matches!(next(), (1, InputEvent::Mouse(m)) if m.x == 3.0));
        assert!(matches!(next(), (2, InputEvent::Mouse(m)) if m.x == 1.0));
        assert!(matches!(next(), (2, InputEvent::Key(_))));
    }

    #[test]
    fn a_move_keeps_its_order_with_the_input_of_its_player_and_a_wake() {
        let mut inbox: Inbox<RoomEvent> = Inbox::new(None);
        let tx = inbox.sender();
        let input = |p, event| RoomEvent::Input {
            player: player(p),
            event,
        };
        tx.send_event(input(1, at(1.0))).unwrap();
        tx.send_event(input(1, key("a"))).unwrap();
        tx.send_event(input(2, key("b"))).unwrap();
        tx.send_event(input(1, at(2.0))).unwrap();
        tx.wake().unwrap();
        tx.send_event(input(1, at(3.0))).unwrap();
        let mut next = || match inbox.wait(None) {
            Ok(RoomEvent::Input { player, event }) => Some((player.get(), event)),
            Err(Interrupt::Wake) => None,
            other => panic!("got {other:?}"),
        };
        assert!(matches!(next(), Some((1, InputEvent::Mouse(m))) if m.x == 1.0));
        assert!(matches!(next(), Some((1, InputEvent::Key(_)))));
        assert!(matches!(next(), Some((2, InputEvent::Key(_)))));
        assert!(matches!(next(), Some((1, InputEvent::Mouse(m))) if m.x == 2.0));
        assert!(next().is_none());
        assert!(matches!(next(), Some((1, InputEvent::Mouse(m))) if m.x == 3.0));
    }

    #[test]
    fn a_move_does_not_pass_a_leave() {
        let mut inbox: Inbox<RoomEvent> = Inbox::new(None);
        let tx = inbox.sender();
        let input = |p, event| RoomEvent::Input {
            player: player(p),
            event,
        };
        tx.send_event(input(1, at(1.0))).unwrap();
        tx.send_event(RoomEvent::Leave { player: player(2) })
            .unwrap();
        tx.send_event(input(1, at(2.0))).unwrap();
        assert!(matches!(inbox.wait(None), Ok(RoomEvent::Input { .. })));
        assert!(matches!(inbox.wait(None), Ok(RoomEvent::Leave { .. })));
        assert!(matches!(inbox.wait(None), Ok(RoomEvent::Input { .. })));
    }

    #[test]
    fn a_frame_goes_to_its_player_and_an_asset_to_all() {
        let (mut room, _input, written) = open_session();
        let scene = Scene::new(10.0, 10.0);
        room.present_to(player(2), &scene).unwrap();
        room.present_all(&scene).unwrap();
        room.push_asset(7, &[1, 2, 3], None).unwrap();
        room.close();
        let bytes = written.bytes();
        let mut r = &bytes[..];
        let mut next = || to_view::read(&mut r).unwrap().expect("a message");
        assert!(matches!(next(), (2, to_view::Message::Frame(_))));
        assert!(matches!(next(), (UNROUTED, to_view::Message::Frame(_))));
        assert!(matches!(
            next(),
            (UNROUTED, to_view::Message::Asset { id: 7, .. })
        ));
        assert!(matches!(next(), (UNROUTED, to_view::Message::Close)));
    }

    #[test]
    fn present_fails_after_close() {
        let (mut room, _input, _) = open_session();
        room.close();
        assert!(matches!(
            room.present_all(&Scene::new(1.0, 1.0)),
            Err(PresentError::Closed)
        ));
        assert!(matches!(room.wait_event(None), Err(Interrupt::Close)));
    }
}
