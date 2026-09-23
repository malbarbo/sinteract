//! The messages from the server to the engine, in the `ServerMessage`
//! union.
//!
//! The server starts the session with the players, passes on the input of
//! each view with its player, and says when a player joins or leaves.
//! The server ends the session with a close. A view that talks to the
//! engine with no server between them takes the role of the server, and
//! sends its input with [`UNROUTED`].

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::num::NonZeroU32;

use capnp::Word;
use capnp::message::{Builder as MessageBuilder, HeapAllocator};

use crate::event::InputEvent;
use crate::protocol_capnp::server_message;

use super::Error;
use super::event::{read_input_event, write_input_event};
use super::framing::{Player, Side, UNROUTED, write_framed};
use super::protocol::{ReadError, decode_root, read_next};

/// One message of the server, one variant per arm of `ServerMessage`. The
/// arm `event` is `Input` here, so it does not clash with
/// [`crate::event::Event`]. The player comes from the header, and `Close`
/// and `Start` are about the whole session. Player 0 is the server itself
/// or the only view, so only `Input` has a player 0.
#[derive(Clone, Debug)]
pub enum Message {
    Input {
        player: Player,
        event: InputEvent,
    },
    Close,
    Start(Roster),
    Join {
        player: NonZeroU32,
        nickname: String,
    },
    Leave {
        player: NonZeroU32,
    },
}

/// A player of the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// The number of the player in the session.
    pub player: NonZeroU32,
    pub nickname: String,
}

/// The players at the start of the session, each player once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Roster(Vec<Member>);

/// Two members of a [`Roster`] have the same player.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DuplicatePlayer(pub NonZeroU32);

impl Roster {
    /// The roster of `members`, or the first player that repeats.
    pub fn new(members: Vec<Member>) -> Result<Roster, DuplicatePlayer> {
        let mut seen = HashSet::with_capacity(members.len());
        match members.iter().find(|m| !seen.insert(m.player)) {
            Some(m) => Err(DuplicatePlayer(m.player)),
            None => Ok(Roster(members)),
        }
    }

    pub fn members(&self) -> &[Member] {
        &self.0
    }
}

impl std::fmt::Display for DuplicatePlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "player {} is in the roster twice", self.0)
    }
}

impl std::error::Error for DuplicatePlayer {}

impl From<DuplicatePlayer> for Error {
    fn from(e: DuplicatePlayer) -> Self {
        Error::DuplicatePlayer(e)
    }
}

/// Read the next message of the server. Returns `None` at the end of the
/// stream. A message or an event of an arm from a newer schema is skipped,
/// and the next one comes out.
pub fn read(r: &mut impl Read) -> Result<Option<Message>, ReadError> {
    read_next(r, Side::Server, decode)
}

/// Write the input `ev` of `player`.
pub fn write_input(w: &mut impl Write, player: Player, ev: &InputEvent) -> io::Result<()> {
    write_framed(w, Side::Server, player, &input_message(ev))
}

/// Write the close of the session.
pub fn write_close(w: &mut impl Write) -> io::Result<()> {
    write_framed(w, Side::Server, UNROUTED, &close_message())
}

/// Write the start of the session with `roster`.
pub fn write_start(w: &mut impl Write, roster: &Roster) -> io::Result<()> {
    write_framed(w, Side::Server, UNROUTED, &start_message(roster.members()))
}

/// Write that `player` joined the session as `nickname`.
pub fn write_join(w: &mut impl Write, player: NonZeroU32, nickname: &str) -> io::Result<()> {
    write_framed(w, Side::Server, player.get(), &join_message(nickname))
}

/// Write that `player` left the session.
pub fn write_leave(w: &mut impl Write, player: NonZeroU32) -> io::Result<()> {
    write_framed(w, Side::Server, player.get(), &leave_message())
}

/// Decode the payload in `words` in place, for `player` of the header.
/// `None` for a message or an event of an arm from a newer schema. A join,
/// a leave or a member of player 0, and a roster that repeats a player, are
/// errors.
pub(super) fn decode(player: Player, words: &[Word]) -> Result<Option<Message>, Error> {
    decode_root::<server_message::Owned, _>(words, |msg| decode_message(player, msg))
}

fn decode_message(
    player: Player,
    msg: server_message::Reader<'_>,
) -> Result<Option<Message>, Error> {
    let Ok(which) = msg.which() else {
        return Ok(None);
    };
    match which {
        server_message::Event(e) => {
            Ok(read_input_event(e?)?.map(|event| Message::Input { player, event }))
        }
        server_message::Close(_) => Ok(Some(Message::Close)),
        server_message::Start(s) => {
            let members = s?
                .get_members()?
                .iter()
                .map(|m| {
                    Ok(Member {
                        player: nonzero_player(m.get_player())?,
                        nickname: m.get_nickname()?.to_str()?.to_owned(),
                    })
                })
                .collect::<Result<_, Error>>()?;
            Ok(Some(Message::Start(Roster::new(members)?)))
        }
        server_message::Join(j) => Ok(Some(Message::Join {
            player: nonzero_player(player)?,
            nickname: j?.get_nickname()?.to_str()?.to_owned(),
        })),
        server_message::Leave(_) => Ok(Some(Message::Leave {
            player: nonzero_player(player)?,
        })),
    }
}

/// `player`, or [`Error::NoPlayer`] if it is 0.
fn nonzero_player(player: Player) -> Result<NonZeroU32, Error> {
    NonZeroU32::new(player).ok_or(Error::NoPlayer)
}

fn input_message(ev: &InputEvent) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    write_input_event(
        builder.init_root::<server_message::Builder>().init_event(),
        ev,
    );
    builder
}

fn close_message() -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    builder.init_root::<server_message::Builder>().init_close();
    builder
}

fn start_message(members: &[Member]) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let start = builder.init_root::<server_message::Builder>().init_start();
    let len = u32::try_from(members.len()).expect("fewer than 2^32 players");
    let mut list = start.init_members(len);
    for (i, member) in (0..len).zip(members) {
        let mut m = list.reborrow().get(i);
        m.set_player(member.player.get());
        m.set_nickname(&*member.nickname);
    }
    builder
}

fn join_message(nickname: &str) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    builder
        .init_root::<server_message::Builder>()
        .init_join()
        .set_nickname(nickname);
    builder
}

fn leave_message() -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    builder.init_root::<server_message::Builder>().init_leave();
    builder
}

/// Encode the input `ev`, with no envelope.
#[cfg(test)]
pub(crate) fn encode_input(ev: &InputEvent) -> Vec<u8> {
    super::finish(input_message(ev))
}

/// Encode a close, with no envelope.
#[cfg(test)]
pub(crate) fn encode_close() -> Vec<u8> {
    super::finish(close_message())
}
