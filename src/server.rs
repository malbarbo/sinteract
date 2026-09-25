//! [`ServerCore`] holds the rules of a room, with no I/O and no clock. The
//! server and the page that plays the part of the server keep the sockets,
//! the pipes and the timer, and call the core for each thing that happens.
//! The core writes the messages for the engine into a buffer, and the host
//! takes them with [`ServerCore::take_engine_output`] and writes them to the
//! engine. The order of the messages is the order of the calls, as long as
//! one task of the host takes the output.

use std::collections::BTreeMap;
use std::num::NonZeroU32;

use crate::event::InputEvent;
use crate::wire;
use crate::wire::to_engine::{self, Member, Roster};
use crate::wire::to_server;

/// The rules of a room, from the players and the timer of the host to the
/// messages for the engine.
///
/// The room waits in the lobby until [`ServerCore::start`], then plays until
/// [`ServerCore::close`]. Only a playing room writes to the engine, apart
/// from the close.
#[derive(Debug)]
pub struct ServerCore {
    /// A number never returns, so a late message for a player who left
    /// never goes to a new one.
    next_player: NonZeroU32,
    seats: BTreeMap<NonZeroU32, Seat>,
    phase: Phase,
    /// The messages for the engine that the host has not taken yet.
    to_engine: Vec<u8>,
}

/// The connection of a view to the seat of a player.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Conn(NonZeroU32);

impl Conn {
    /// The player of the seat.
    pub fn player(self) -> NonZeroU32 {
        self.0
    }
}

/// Why [`ServerCore::from_view`] dropped a message of a view.
#[derive(Debug)]
pub enum ViewError {
    /// The message has more bytes than [`MAX_VIEW_BYTES`].
    TooLong(usize),
    /// The message does not decode.
    Payload(wire::Error),
}

impl std::fmt::Display for ViewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ViewError::TooLong(len) => {
                write!(f, "message of {len} bytes exceeds cap {MAX_VIEW_BYTES}")
            }
            ViewError::Payload(e) => write!(f, "message does not decode: {e}"),
        }
    }
}

impl std::error::Error for ViewError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ViewError::Payload(e) => Some(e),
            ViewError::TooLong(_) => None,
        }
    }
}

/// The cap on a message of a view. Input is small, and the cap keeps what
/// the core writes for the engine under the cap of the framing.
pub const MAX_VIEW_BYTES: usize = 64 * 1024;

/// The cap on a nickname, in bytes of UTF-8.
const MAX_NICKNAME_BYTES: usize = 64;

/// A message for the engine is far below the cap of the framing, since the
/// nicknames and the messages of the views have a cap.
const UNDER_THE_CAP: &str = "a message for the engine is under the cap of the framing";

impl ServerCore {
    pub fn new() -> Self {
        ServerCore {
            next_player: NonZeroU32::MIN,
            seats: BTreeMap::new(),
            phase: Phase::Lobby,
            to_engine: Vec::new(),
        }
    }

    /// Seat a new player as `nickname`, and return the connection of its
    /// view. The nickname loses its control characters, so it cannot move
    /// the cursor of a terminal that prints it, and is cut to 64 bytes. In
    /// the lobby the player waits for the start, and in the game the engine
    /// gets a join.
    pub fn join(&mut self, nickname: &str) -> Conn {
        let player = self.next_player;
        self.next_player = player
            .checked_add(1)
            .expect("a room has fewer than 2^32 joins");
        let nickname = clean_nickname(nickname);
        if self.phase == Phase::Playing {
            to_engine::write_join(&mut self.to_engine, player, &nickname).expect(UNDER_THE_CAP);
        }
        self.seats.insert(
            player,
            Seat {
                nickname,
                lobby_size: None,
            },
        );
        Conn(player)
    }

    /// Free the seat of `conn`. In the game the engine gets a leave. The
    /// close of the view and the drop of its socket may both call it, so a
    /// second call does nothing.
    pub fn leave(&mut self, conn: Conn) {
        if self.seats.remove(&conn.0).is_some() && self.phase == Phase::Playing {
            to_engine::write_leave(&mut self.to_engine, conn.0).expect(UNDER_THE_CAP);
        }
    }

    /// Returns `true` if the room goes from the lobby to the game, `false`
    /// otherwise, as for a second click on start. The engine gets a start
    /// with the players of the seats, then the last resize of each view in
    /// the lobby.
    pub fn start(&mut self) -> bool {
        match self.phase {
            Phase::Lobby => {}
            Phase::Playing | Phase::Closing => return false,
        }
        let members = self
            .seats
            .iter()
            .map(|(&player, seat)| Member {
                player,
                nickname: seat.nickname.clone(),
            })
            .collect();
        let roster = Roster::new(members).expect("a map has each player once");
        to_engine::write_start(&mut self.to_engine, &roster).expect(UNDER_THE_CAP);
        for (&player, seat) in &mut self.seats {
            if let Some((width, height)) = seat.lobby_size.take() {
                let event = InputEvent::Resize { width, height };
                to_engine::write_input(&mut self.to_engine, player, &event).expect(UNDER_THE_CAP);
            }
        }
        self.phase = Phase::Playing;
        true
    }

    /// Tell the engine to draw the next frames, in the game.
    pub fn tick(&mut self) {
        if self.phase == Phase::Playing {
            to_engine::write_tick(&mut self.to_engine).expect(UNDER_THE_CAP);
        }
    }

    /// Tell the engine to end, once. The engine may still send its last
    /// frames.
    pub fn close(&mut self) {
        match self.phase {
            Phase::Lobby | Phase::Playing => {
                to_engine::write_close(&mut self.to_engine).expect(UNDER_THE_CAP);
                self.phase = Phase::Closing;
            }
            Phase::Closing => {}
        }
    }

    /// Take a message that the view of `conn` sent with no envelope, such as
    /// one that came over a WebSocket. An event goes to [`Self::input`],
    /// and the close of the view to [`Self::leave`]. A message of an arm
    /// from a newer schema is dropped.
    pub fn from_view(&mut self, conn: Conn, payload: &[u8]) -> Result<(), ViewError> {
        if payload.len() > MAX_VIEW_BYTES {
            return Err(ViewError::TooLong(payload.len()));
        }
        match to_server::decode(payload).map_err(ViewError::Payload)? {
            Some(to_server::Message::Input(event)) => self.input(conn, &event),
            Some(to_server::Message::Close) => self.leave(conn),
            None => {}
        }
        Ok(())
    }

    /// Pass `event` of the view of `conn` to the engine, in the game. The
    /// tick paces the engine, so a Vsync of the view is dropped. In the
    /// lobby the engine has not started, and the room keeps only the size
    /// of the last resize, for the start. The input of a player who left is
    /// dropped.
    pub fn input(&mut self, conn: Conn, event: &InputEvent) {
        let Some(seat) = self.seats.get_mut(&conn.0) else {
            return;
        };
        if matches!(event, InputEvent::Vsync) {
            return;
        }
        match self.phase {
            Phase::Lobby => {
                if let InputEvent::Resize { width, height } = *event {
                    seat.lobby_size = Some((width, height));
                }
            }
            Phase::Playing => {
                to_engine::write_input(&mut self.to_engine, conn.0, event).expect(UNDER_THE_CAP);
            }
            Phase::Closing => {}
        }
    }

    /// Move the messages for the engine to the end of `buf`. A host that
    /// cannot write all of them keeps the rest in `buf` for the next write.
    pub fn take_engine_output(&mut self, buf: &mut Vec<u8>) {
        buf.append(&mut self.to_engine);
    }
}

impl Default for ServerCore {
    fn default() -> Self {
        Self::new()
    }
}

/// The seat of a player who is in the room.
#[derive(Debug)]
struct Seat {
    nickname: String,
    /// The size of the last resize in the lobby, for the start.
    lobby_size: Option<(f32, f32)>,
}

/// Whether the room waits for its start, plays, or told the engine to end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Lobby,
    Playing,
    Closing,
}

/// `nickname` without its control characters, cut at a character to
/// [`MAX_NICKNAME_BYTES`].
fn clean_nickname(nickname: &str) -> String {
    let mut clean = String::new();
    for c in nickname.chars().filter(|c| !c.is_control()) {
        if clean.len() + c.len_utf8() > MAX_NICKNAME_BYTES {
            break;
        }
        clean.push(c);
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{KeyEvent, KeyKind, Modifiers};
    use crate::session::{Session, SessionEvent};
    use crate::wire::framing::HEADER_BYTES;

    /// A core with a session that reads what the core writes, which also
    /// checks that the core writes nothing that a session rejects.
    struct Room {
        core: ServerCore,
        engine: Session,
    }

    impl Room {
        fn new() -> Self {
            Room {
                core: ServerCore::new(),
                engine: Session::new(),
            }
        }

        /// The events that the engine got since the last call, in a short
        /// form.
        fn events(&mut self) -> Vec<String> {
            let mut buf = Vec::new();
            self.core.take_engine_output(&mut buf);
            self.engine.feed(&buf);
            std::iter::from_fn(|| self.engine.next_event())
                .map(|e| match e {
                    SessionEvent::Start(roster) => {
                        let members: Vec<_> = roster
                            .members()
                            .iter()
                            .map(|m| format!("{} {}", m.player, m.nickname))
                            .collect();
                        format!("start {}", members.join(", "))
                    }
                    SessionEvent::Vsync => "tick".into(),
                    SessionEvent::Input { player, event } => match event {
                        InputEvent::Key(k) => format!("{player} key {}", k.key),
                        InputEvent::Resize { width, height } => {
                            format!("{player} resize {width}x{height}")
                        }
                        InputEvent::Mouse(_) | InputEvent::Vsync | InputEvent::Pad(_) => {
                            format!("{player} {event:?}")
                        }
                    },
                    SessionEvent::Join { player, nickname } => format!("join {player} {nickname}"),
                    SessionEvent::Leave { player } => format!("leave {player}"),
                    SessionEvent::Error(e) => format!("error {e}"),
                    SessionEvent::End(None) => "close".into(),
                    SessionEvent::End(Some(e)) => format!("broken {e}"),
                })
                .collect()
        }
    }

    fn key(name: &str) -> InputEvent {
        InputEvent::Key(KeyEvent {
            kind: KeyKind::Press,
            key: name.into(),
            modifiers: Modifiers::default(),
            repeat: false,
        })
    }

    fn resize(width: f32, height: f32) -> InputEvent {
        InputEvent::Resize { width, height }
    }

    /// `event` as a view sends it over a WebSocket.
    fn from_view(event: &InputEvent) -> Vec<u8> {
        let mut out = Vec::new();
        to_server::write_input(&mut out, event).unwrap();
        out.split_off(HEADER_BYTES)
    }

    #[test]
    fn the_start_has_the_players_of_the_lobby() {
        let mut room = Room::new();
        room.core.join("Ana");
        let beto = room.core.join("Beto");
        room.core.join("Caio");
        room.core.leave(beto);
        room.core.tick();
        assert!(room.events().is_empty());
        assert!(room.core.start());
        assert_eq!(room.events(), ["start 1 Ana, 3 Caio"]);
        assert!(!room.core.start());
        assert!(room.events().is_empty());
    }

    #[test]
    fn the_engine_gets_a_join_and_a_leave_in_the_game() {
        let mut room = Room::new();
        room.core.start();
        let ana = room.core.join("Ana");
        room.core.tick();
        room.core.leave(ana);
        room.core.leave(ana);
        assert_eq!(room.events(), ["start ", "join 1 Ana", "tick", "leave 1"]);
    }

    #[test]
    fn a_number_never_returns() {
        let mut core = ServerCore::new();
        let ana = core.join("Ana");
        core.leave(ana);
        assert_eq!(core.join("Beto").player().get(), 2);
    }

    #[test]
    fn the_close_goes_once_and_nothing_follows() {
        let mut room = Room::new();
        let ana = room.core.join("Ana");
        room.core.start();
        room.core.close();
        room.core.close();
        room.core.tick();
        room.core.join("Beto");
        room.core.leave(ana);
        assert!(!room.core.start());
        assert_eq!(room.events(), ["start 1 Ana", "close"]);
    }

    #[test]
    fn a_close_in_the_lobby_ends_the_engine() {
        let mut room = Room::new();
        room.core.close();
        assert!(!room.core.start());
        assert_eq!(room.events(), ["close"]);
    }

    #[test]
    fn a_nickname_loses_its_control_characters_and_is_cut_at_a_character() {
        assert_eq!(clean_nickname("A\u{1b}[2Jna\n"), "A[2Jna");
        let long = "é".repeat(40);
        assert_eq!(clean_nickname(&long), "é".repeat(32));
    }

    #[test]
    fn the_output_goes_after_what_the_buffer_holds() {
        let mut core = ServerCore::new();
        core.close();
        let mut buf = b"rest".to_vec();
        core.take_engine_output(&mut buf);
        assert_eq!(&buf[..4], b"rest");
        assert!(buf.len() > 4);
        let mut again = Vec::new();
        core.take_engine_output(&mut again);
        assert!(again.is_empty());
    }

    #[test]
    fn the_input_of_a_view_goes_with_its_player() {
        let mut room = Room::new();
        let ana = room.core.join("Ana");
        let beto = room.core.join("Beto");
        room.core.start();
        room.core.from_view(beto, &from_view(&key("b"))).unwrap();
        room.core.input(ana, &key("a"));
        room.core.input(ana, &InputEvent::Vsync);
        assert_eq!(room.events(), ["start 1 Ana, 2 Beto", "2 key b", "1 key a"]);
    }

    #[test]
    fn the_lobby_keeps_the_last_resize_for_the_start() {
        let mut room = Room::new();
        let ana = room.core.join("Ana");
        let beto = room.core.join("Beto");
        room.core.input(ana, &resize(10.0, 10.0));
        room.core.input(ana, &key("a"));
        room.core.input(ana, &resize(20.0, 30.0));
        room.core.input(beto, &key("b"));
        room.core.start();
        assert_eq!(room.events(), ["start 1 Ana, 2 Beto", "1 resize 20x30"]);
        room.core.input(ana, &resize(40.0, 50.0));
        assert_eq!(room.events(), ["1 resize 40x50"]);
    }

    #[test]
    fn the_close_of_a_view_is_a_leave() {
        let mut room = Room::new();
        let ana = room.core.join("Ana");
        room.core.start();
        room.core
            .from_view(ana, &to_server::encode_close())
            .unwrap();
        room.core.input(ana, &key("a"));
        room.core.close();
        room.core.input(ana, &key("a"));
        assert_eq!(room.events(), ["start 1 Ana", "leave 1", "close"]);
    }

    #[test]
    fn a_message_of_a_view_that_is_too_long_or_does_not_decode_is_an_error() {
        let mut core = ServerCore::new();
        let ana = core.join("Ana");
        let long = vec![0; MAX_VIEW_BYTES + 1];
        assert!(matches!(
            core.from_view(ana, &long),
            Err(ViewError::TooLong(_))
        ));
        assert!(matches!(
            core.from_view(ana, b"junk"),
            Err(ViewError::Payload(_))
        ));
    }

    #[test]
    fn a_message_of_an_unknown_arm_is_dropped() {
        let mut room = Room::new();
        let ana = room.core.join("Ana");
        room.core.start();
        let unknown = wire::with_unknown_view_value(&from_view(&key("a")), |m| wire::tag_of(m));
        room.core.from_view(ana, &unknown).unwrap();
        assert_eq!(room.events(), ["start 1 Ana"]);
    }
}
