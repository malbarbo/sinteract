//! [`ServerCore`] holds the rules of a room, with no I/O and no clock. The
//! server and the page that plays the part of the server keep the sockets,
//! the pipes and the timer, and call the core for each thing that happens.
//! The core writes the messages for the engine into a buffer, and the host
//! takes them with [`ServerCore::take_engine_output`] and writes them to the
//! engine. The order of the messages is the order of the calls, as long as
//! one task of the host takes the output.

use std::collections::BTreeMap;
use std::num::NonZeroU32;

use crate::wire::to_engine::{self, Member, Roster};

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

/// The cap on a nickname, in bytes of UTF-8.
const MAX_NICKNAME_BYTES: usize = 64;

/// A message for the engine is far below the cap of the framing, since the
/// nicknames have a cap.
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
        self.seats.insert(player, Seat { nickname });
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
    /// with the players of the seats.
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
    use crate::session::{Session, SessionEvent};

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
                    SessionEvent::Input { player, event } => format!("{player} {event:?}"),
                    SessionEvent::Join { player, nickname } => format!("join {player} {nickname}"),
                    SessionEvent::Leave { player } => format!("leave {player}"),
                    SessionEvent::Error(e) => format!("error {e}"),
                    SessionEvent::End(None) => "close".into(),
                    SessionEvent::End(Some(e)) => format!("broken {e}"),
                })
                .collect()
        }
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
}
